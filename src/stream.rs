//! HTTP handlers: audio streams (library files + radio), cover art, health.
//!
//! Files are served as progressive MP3/AAC with HTTP Range support, so the
//! Echo can resume a paused stream by reopening the same URL with
//! `Range: bytes=N-`. Radio URLs are piped through transparently.

use core::task::ready;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use aria_audio::StreamSource;
use aria_audio::registry::looks_like_token;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncSeek, BufReader, ReadBuf, SeekFrom};

use crate::state::AppState;

pub async fn handle_alexa(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    req: Request<Body>,
) -> impl IntoResponse {
    let bytes = axum::body::to_bytes(req.into_body(), 1_048_576)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
        .unwrap_or_default();
    match serde_json::from_slice::<aria_alexa::Envelope>(&bytes) {
        Ok(env) => {
            // Client-id check (prevents other skills hitting this endpoint).
            if let (Some(want), Some(got)) =
                (state.cfg.server.client_id.as_deref(), env.application_id())
            {
                if got != want {
                    tracing::warn!(got, want, "rejected request from wrong skill");
                    let resp = aria_alexa::Response::empty();
                    return (StatusCode::OK, axum::Json(resp.to_json())).into_response();
                }
            }
            let resp = crate::router::handle_alexa(&state, env).await;
            (StatusCode::OK, axum::Json(resp.to_json())).into_response()
        }
        Err(e) => {
            tracing::debug!(%e, headers = ?headers.get(header::USER_AGENT), "unparseable /alexa request");
            (
                StatusCode::OK,
                axum::Json(aria_alexa::Response::empty().to_json()),
            )
                .into_response()
        }
    }
}

pub async fn handle_stream(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !looks_like_token(&token) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let Some(resolved) = state.registry.resolve(&token) else {
        tracing::debug!("unknown or expired stream token");
        return (StatusCode::NOT_FOUND, "expired").into_response();
    };
    match resolved.source {
        StreamSource::File {
            path,
            format,
            offset_ms,
            avg_bitrate_kbps,
            ..
        } => stream_file(&path, format, offset_ms, avg_bitrate_kbps, &headers).await,
        StreamSource::Radio { url, .. } => stream_radio(url).await,
    }
}

async fn stream_file(
    path: &str,
    format: aria_audio::Format,
    offset_ms: u64,
    avg_bitrate_kbps: u32,
    headers: &HeaderMap,
) -> Response {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path, %e, "cannot open music file");
            return (StatusCode::NOT_FOUND, "file missing").into_response();
        }
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    // Skip a leading ID3v2 tag: it is metadata, not audio, and both the
    // ms->byte seek math and the Echo's Range offsets are relative to the
    // first audio byte.
    let tag_skip = id3_tag_size(&file).await;
    // Milliseconds -> bytes at the average bitrate (MP3 is frame-aligned, so
    // a few frames of slack at the seek point are inaudible).
    let byte_start = offset_ms
        .saturating_mul(avg_bitrate_kbps as u64)
        .saturating_div(1000);
    let start_rel = parse_range_start(headers)
        .map(|n| n.max(byte_start))
        .unwrap_or(byte_start);
    let start = tag_skip.saturating_add(start_rel).min(len);
    let body = Body::from_stream(FileStream::new(
        BufReader::with_capacity(256 * 1024, file),
        start,
    ));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, format.content_type())
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

async fn stream_radio(url: String) -> Response {
    let client = reqwest::Client::new();
    let upstream = match client
        .get(&url)
        .header(header::USER_AGENT, "aria/0.1 (Alexa music bridge)")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::warn!(url, status = %r.status(), "radio upstream error");
            return (StatusCode::BAD_GATEWAY, "radio unavailable").into_response();
        }
        Err(e) => {
            tracing::warn!(url, %e, "radio upstream connect failed");
            return (StatusCode::BAD_GATEWAY, "radio unavailable").into_response();
        }
    };
    let ctype = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.starts_with("audio/")
                || v.starts_with("application/vnd.apple.mpegurl")
                || v.starts_with("application/x-mpegURL")
        })
        .unwrap_or("audio/mpeg")
        .to_string();
    let body = Body::from_stream(RadioStream {
        inner: Box::pin(upstream.bytes_stream()),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

pub async fn handle_art(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let lib = state.library.read().unwrap();
    match lib.cover(id) {
        Some(c) => {
            let mime = c.mime;
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.to_string()),
                    (header::CACHE_CONTROL, "max-age=86400".to_string()),
                ],
                c.data,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "no art").into_response(),
    }
}

pub async fn handle_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let lib = state.library.read().unwrap();
    let devices = state.player().keys().count();
    axum::Json(serde_json::json!({
        "ok": true,
        "uptime_secs": state.started.elapsed().as_secs(),
        "library_tracks": lib.len(),
        "playable_tracks": lib.playable_len(),
        "stream_tokens": state.registry.len(),
        "devices": devices,
    }))
}

/// Size in bytes of a leading ID3v2 tag, or 0 if the file has none.
async fn id3_tag_size(file: &File) -> u64 {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = match file.try_clone().await {
        Ok(f) => f,
        Err(_) => return 0,
    };
    if f.seek(SeekFrom::Start(0)).await.is_err() {
        return 0;
    }
    let mut head = [0u8; 10];
    if f.read(&mut head).await.unwrap_or(0) < 10 || &head[0..3] != b"ID3" {
        return 0;
    }
    // Synchsafe (7-bit) size at bytes 6..10; +10 for the tag header itself.
    10 + (((head[6] as u64 & 0x7f) << 21)
        | ((head[7] as u64 & 0x7f) << 14)
        | ((head[8] as u64 & 0x7f) << 7)
        | (head[9] as u64 & 0x7f))
}

fn parse_range_start(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(header::RANGE)?.to_str().ok()?;
    // "bytes=12345-"
    let v = v.strip_prefix("bytes=")?;
    let start = v.split('-').next()?;
    start.trim().parse().ok()
}

/// Chunked file reader starting at a byte offset.
struct FileStream {
    reader: BufReader<File>,
    start: u64,
    /// `start_seek` was issued; a `poll_complete` must follow before any
    /// other operation (tokio's two-phase seek).
    seeking: bool,
    initialized: bool,
    buf: Vec<u8>,
}

impl FileStream {
    fn new(reader: BufReader<File>, start: u64) -> Self {
        Self {
            reader,
            start,
            seeking: false,
            initialized: false,
            buf: vec![0u8; 256 * 1024],
        }
    }
}

impl Stream for FileStream {
    type Item = Result<bytes::Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if !this.initialized {
            // BufReader has no seek; seek the inner file (buffer is empty here).
            if !this.seeking {
                if let Err(e) =
                    Pin::new(this.reader.get_mut()).start_seek(SeekFrom::Start(this.start))
                {
                    return Poll::Ready(Some(Err(e)));
                }
                this.seeking = true;
            }
            match ready!(Pin::new(this.reader.get_mut()).poll_complete(cx)) {
                Ok(_) => {
                    this.initialized = true;
                    this.seeking = false;
                }
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
        }
        // ReadBuf::new sizes its capacity to the slice length, so the buffer
        // must keep its full length (never truncate/clear between polls).
        let mut rb = ReadBuf::new(&mut this.buf);
        let filled = match ready!(Pin::new(&mut this.reader).poll_read(cx, &mut rb)) {
            Ok(()) => rb.filled().len(),
            Err(e) => return Poll::Ready(Some(Err(e))),
        };
        if filled == 0 {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(bytes::Bytes::copy_from_slice(rb.filled()))))
        }
    }
}

/// Piped upstream radio bytes.
struct RadioStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
}

impl Stream for RadioStream {
    type Item = Result<bytes::Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use futures_util::TryStreamExt;

    #[tokio::test]
    async fn file_stream_reads_all_bytes_from_offset() {
        let dir = std::env::temp_dir().join("aria_fs_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.bin");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let file = tokio::fs::File::open(&p).await.unwrap();
        let mut s = FileStream::new(BufReader::with_capacity(64 * 1024, file), 100);
        let mut got = Vec::new();
        while let Some(chunk) = s.try_next().await.unwrap() {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got.len(), data.len() - 100, "byte count");
        assert_eq!(got, data[100..], "content");
    }

    #[tokio::test]
    async fn file_stream_zero_offset() {
        let dir = std::env::temp_dir().join("aria_fs_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t0.bin");
        let data: Vec<u8> = vec![7u8; 500_000];
        std::fs::write(&p, &data).unwrap();
        let file = tokio::fs::File::open(&p).await.unwrap();
        let mut s = FileStream::new(BufReader::with_capacity(256 * 1024, file), 0);
        let mut got = Vec::new();
        while let Some(chunk) = s.try_next().await.unwrap() {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, data);
    }
}

#[cfg(test)]
mod id3_tests {
    use super::*;

    #[tokio::test]
    async fn detects_id3v2_tag() {
        let dir = std::env::temp_dir().join("aria_fs_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tagged.mp3");
        let mut tag = b"ID3\x03\x00\x00".to_vec();
        let n: u64 = 40;
        tag.extend_from_slice(
            &[
                (n >> 21) & 0x7f,
                (n >> 14) & 0x7f,
                (n >> 7) & 0x7f,
                n & 0x7f,
            ]
            .map(|b| b as u8),
        );
        tag.extend(std::iter::repeat_n(0u8, 40));
        tag.extend_from_slice(b"\xff\xfb\x90\xc0");
        std::fs::write(&p, tag).unwrap();
        let f = tokio::fs::File::open(&p).await.unwrap();
        assert_eq!(id3_tag_size(&f).await, 50);
    }

    #[tokio::test]
    async fn no_tag_is_zero() {
        let dir = std::env::temp_dir().join("aria_fs_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plain.mp3");
        let mut raw = b"\xff\xfb\x90\xc0".to_vec();
        raw.extend(std::iter::repeat_n(0u8, 32));
        std::fs::write(&p, raw).unwrap();
        let f = tokio::fs::File::open(&p).await.unwrap();
        assert_eq!(id3_tag_size(&f).await, 0);
    }
}
