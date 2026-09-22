//! HTTP handlers: audio streams (library files + radio), cover art, health.
//!
//! Files are served as progressive MP3/AAC with HTTP Range support, so the
//! Echo can resume a paused stream by reopening the same URL with
//! `Range: bytes=N-`. Radio URLs are piped through transparently.

use core::task::ready;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use eugeis_audio::StreamSource;
use eugeis_audio::registry::looks_like_token;
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
    match serde_json::from_slice::<eugeis_alexa::Envelope>(&bytes) {
        Ok(env) => {
            // Client-id check (prevents other skills hitting this endpoint).
            if let (Some(want), Some(got)) =
                (state.cfg.server.client_id.as_deref(), env.application_id())
            {
                if got != want {
                    tracing::warn!(got, want, "rejected request from wrong skill");
                    let resp = eugeis_alexa::Response::empty();
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
                axum::Json(eugeis_alexa::Response::empty().to_json()),
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
    format: eugeis_audio::Format,
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
    // Milliseconds -> bytes at the average bitrate (MP3 is frame-aligned, so
    // a few frames of slack at the seek point are inaudible).
    let byte_start = offset_ms
        .saturating_mul(avg_bitrate_kbps as u64)
        .saturating_div(1000)
        .min(len);
    let start = parse_range_start(headers)
        .map(|n| n.max(byte_start))
        .unwrap_or(byte_start)
        .min(len);
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
        .header(header::USER_AGENT, "eugeis/0.1 (Alexa music bridge)")
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
    initialized: bool,
    buf: Vec<u8>,
}

impl FileStream {
    fn new(reader: BufReader<File>, start: u64) -> Self {
        Self {
            reader,
            start,
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
            if let Err(e) = Pin::new(this.reader.get_mut()).start_seek(SeekFrom::Start(this.start))
            {
                return Poll::Ready(Some(Err(e)));
            }
            match ready!(Pin::new(this.reader.get_mut()).poll_complete(cx)) {
                Ok(_) => this.initialized = true,
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
        }
        this.buf.clear();
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
