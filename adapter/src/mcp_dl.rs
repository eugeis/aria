//! The adapter's MCP surface (streamable HTTP, `POST /dl/mcp`): the
//! YouTube tools for the BRAIN's assistant agent — search, playlist,
//! queue downloads, queue status. The brain's generated MCP client
//! presents the ARIA_MCP_TOKEN bearer (the same secret the adapter
//! validates); without it the endpoint answers 401.
//!
//! The endpoint lives under `/dl` because the ingress routes `/dl/*`
//! to the adapter while `/mcp` belongs to the brain.
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars::JsonSchema;
use rmcp::serde::Deserialize;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{Json, tool, tool_router};
use serde_json::{json, Value};
use tower::util::ServiceExt;
use tower_service::Service;

use crate::state::AppState;

pub struct AdapterMcp {
    state: Arc<AppState>,
}

#[derive(Deserialize, JsonSchema)]
struct YtSearchArgs {
    /// The YouTube search query (e.g. "AI generated worship music").
    query: String,
    /// Number of results (default 10, max 50).
    #[serde(default)]
    n: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct YtPlaylistArgs {
    /// The YouTube playlist URL (or a channel's /videos URL).
    url: String,
}

#[derive(Deserialize, JsonSchema)]
struct DlItemArg {
    /// The YouTube video id (11 chars) or a full watch URL.
    id: String,
    /// Title tag (defaults to a generic name — prefer the search result's title).
    #[serde(default)]
    title: Option<String>,
    /// Artist tag (use the video's channel).
    #[serde(default)]
    artist: Option<String>,
    /// Album tag (use the playlist title for playlist downloads).
    #[serde(default)]
    album: Option<String>,
    /// Target collection (default: the first configured one).
    #[serde(default)]
    collection: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct YtDownloadArgs {
    /// The videos to download (queued; the download worker picks them up
    /// and rescans the library afterwards).
    items: Vec<DlItemArg>,
}

#[tool_router(server_handler)]
impl AdapterMcp {
    #[tool(description = "Search YouTube (yt-dlp, the owner's cookies if configured). Returns the top N results: id, title, channel, duration in seconds, url. ONE call is enough — request n=30 or more and pick from those results (skip re-uploads, live streams, wrong language/kind); do NOT search repeatedly. Then queue the chosen videos with yt_download in ONE call and give the final answer.")]
    async fn yt_search(&self, Parameters(a): Parameters<YtSearchArgs>) -> Json<Value> {
        let q = a.query.trim().to_string();
        if q.is_empty() {
            return Json(json!({ "error": "empty query" }));
        }
        let n = a.n.unwrap_or(10).clamp(1, 50);
        match crate::web::yt_flat_json(&self.state.cfg, &format!("ytsearch{n}:{q}")).await {
            Ok(v) => Json(json!({
                "title": v.get("title").and_then(|t| t.as_str()).unwrap_or(&q),
                "hits": crate::web::hits_from_entries(&v),
            })),
            Err(e) => Json(json!({ "error": e })),
        }
    }

    #[tool(description = "Read a YouTube playlist (or channel) URL: title, channel, and the video list (id, title, channel, duration, url). Then queue the wanted videos with yt_download (use the playlist title as the album tag).")]
    async fn yt_playlist(&self, Parameters(a): Parameters<YtPlaylistArgs>) -> Json<Value> {
        let url = a.url.trim();
        if !url.starts_with("http") {
            return Json(json!({ "error": "url must start with http(s)" }));
        }
        match crate::web::yt_flat_json(&self.state.cfg, url).await {
            Ok(v) => Json(json!({
                "title": v.get("title").and_then(|t| t.as_str()).unwrap_or(url),
                "channel": v.get("channel").or_else(|| v.get("uploader")).and_then(|c| c.as_str()).map(String::from),
                "hits": crate::web::hits_from_entries(&v),
            })),
            Err(e) => Json(json!({ "error": e })),
        }
    }

    #[tool(description = "Queue YouTube videos for download into the library (yt-dlp → 192k mp3, tags written from the item fields, library rescanned automatically). One call per video batch; the download runs in the BACKGROUND — report what was queued and check progress with queue_status. Skip videos that are already in the queue (this call dedupes by URL).")]
    async fn yt_download(&self, Parameters(a): Parameters<YtDownloadArgs>) -> Json<Value> {
        if a.items.is_empty() {
            return Json(json!({ "error": "no items" }));
        }
        let known: Vec<String> = self
            .state
            .cfg
            .collections
            .items
            .iter()
            .map(|i| i.name.clone())
            .collect();
        let default_col = known.first().cloned();
        let mut added: Vec<Value> = Vec::new();
        let mut skipped: Vec<Value> = Vec::new();
        let mut errors: Vec<Value> = Vec::new();
        for (i, item) in a.items.iter().enumerate() {
            let col = match item.collection.as_deref().filter(|s| !s.trim().is_empty()).or(default_col.as_deref()) {
                Some(c) => known
                    .iter()
                    .find(|k| k.eq_ignore_ascii_case(c))
                    .cloned()
                    .unwrap_or_else(|| c.to_string()),
                None => {
                    errors.push(json!({ "item": i + 1, "error": "no collection configured" }));
                    continue;
                }
            };
            let url = if item.id.starts_with("http") {
                item.id.clone()
            } else if item.id.chars().count() <= 11 && item.id.chars().all(|c| c.is_ascii_alphanumeric()) {
                format!("https://www.youtube.com/watch?v={}", item.id)
            } else {
                errors.push(json!({ "item": i + 1, "error": "no usable video id or url" }));
                continue;
            };
            // Dedupe: already queued (any state) or done earlier with the
            // same URL — the agent may retry after a partial batch.
            let queued = self
                .state
                .dl_queue
                .snapshot()
                .iter()
                .any(|q| q.url == url);
            if queued {
                skipped.push(json!({ "id": item.id, "url": url }));
                continue;
            }
            self.state.dl_queue.add(vec![crate::web::DlItem {
                id: format!(
                    "{}-{}",
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                    i
                ),
                url: url.clone(),
                title: item.title.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("Video {}", i + 1)),
                artist: item.artist.clone().filter(|s| !s.trim().is_empty()).unwrap_or_default(),
                album: item.album.clone().filter(|s| !s.trim().is_empty()).unwrap_or_default(),
                collection: col.clone(),
                track: None,
                status: crate::web::DlStatus::Queued,
                added_at: crate::web::fmt_time(SystemTime::now()),
                owner: "admin".to_string(),
            }]);
            added.push(json!({ "id": item.id, "url": url, "title": item.title, "collection": col }));
        }
        Json(json!({
            "ok": true,
            "added": added,
            "skipped_already_queued": skipped,
            "errors": errors,
            "note": "downloads run in the background — use queue_status for progress",
        }))
    }

    #[tool(description = "The download queue, newest first (max 30): id, title, artist, album, collection, status (queued / downloading with progress / done with path / failed with error / cancelled). Use it to report what is done and what is still running.")]
    async fn queue_status(&self) -> Json<Value> {
        let items = self.state.dl_queue.snapshot();
        let queue: Vec<Value> = items
            .iter()
            .rev()
            .take(30)
            .map(|i| {
                json!({
                    "id": i.id,
                    "title": i.title,
                    "artist": i.artist,
                    "album": i.album,
                    "collection": i.collection,
                    "status": i.status,
                })
            })
            .collect();
        Json(json!({ "queue": queue }))
    }
}

/// The streamable-HTTP MCP service (stateless: every request initializes
/// its own session; JSON responses). Wrap it in `AuthMcp` before mounting.
pub fn mcp_service(state: Arc<AppState>) -> StreamableHttpService<AdapterMcp, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .disable_allowed_hosts()
        .disable_allowed_origins()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    StreamableHttpService::new(
        move || Ok(AdapterMcp {
            state: state.clone(),
        }),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

// ---------------------------------------------------------------------------
// bearer auth wrapper (the brain presents MOAIC_MCP_ADAPTER_AUTH; the
// adapter validates ARIA_MCP_TOKEN — the same secret on both sides).
// An empty token disables the endpoint: every request answers 401.
// ---------------------------------------------------------------------------

type InnerService = StreamableHttpService<AdapterMcp, NeverSessionManager>;

#[derive(Clone)]
pub struct AuthMcp {
    pub inner: InnerService,
    pub token: String,
}

impl Service<Request<Body>> for AuthMcp {
    type Response = <InnerService as Service<Request<Body>>>::Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        // Stateless (NeverSessionManager): always ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let expected = self.token.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let authorized = !expected.is_empty()
                && req
                    .headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .is_some_and(|t| t == expected);
            if !authorized {
                let body = Full::new(Bytes::from_static(b"{\"error\":\"unauthorized\"}"))
                    .map_err(|never: Infallible| match never {})
                    .boxed();
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap());
            }
            inner.oneshot(req).await
        })
    }
}
