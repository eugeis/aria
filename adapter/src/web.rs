//! The Aria web app: a browser UI + REST API for downloading YouTube audio
//! into the local library.
//!
//!   * static SPA at `/` (embedded, no separate container)
//!   * `/dl/cookies`  — the user's YouTube session (Netscape cookies.txt,
//!                      exported from their browser with a cookies extension)
//!   * `/dl/search` + `/dl/playlist` — yt-dlp flat lookups
//!   * `/dl/queue`    — a persistent download queue worked by a single
//!                      background task: yt-dlp → MP3 192k → ID3 tags →
//!                      the collection's folder layout. When the queue
//!                      drains, the library is rescanned + the brain
//!                      reindexed, so new tracks are playable immediately.
//!
//! Auth: when `server.web_token` (env ARIA_WEB_TOKEN) is set, every `/dl/*`
//! route (except login) requires the matching cookie.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::state::AppState;

const COOKIE_NAME: &str = "ariadltoken";
const YTDLP_TIMEOUT_SECS: u64 = 120;
const MAX_COOKIE_UPLOAD: usize = 1_048_576;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");
const ICON_SVG: &str = include_str!("../web/icon.svg");
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");

// ---------------------------------------------------------------------------
// static SPA
// ---------------------------------------------------------------------------

fn text(body: &'static str, mime: &'static str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime)],
        body.to_string(),
    )
        .into_response()
}

async fn handle_index() -> Response {
    text(INDEX_HTML, "text/html; charset=utf-8")
}
async fn handle_app_js() -> Response {
    text(APP_JS, "text/javascript; charset=utf-8")
}
async fn handle_style_css() -> Response {
    text(STYLE_CSS, "text/css; charset=utf-8")
}
async fn handle_icon() -> Response {
    text(ICON_SVG, "image/svg+xml")
}
async fn handle_manifest() -> Response {
    text(MANIFEST, "application/manifest+json")
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
        .next()
}

fn authorized(cfg: &Config, headers: &HeaderMap) -> bool {
    match cfg.server.web_token.as_deref() {
        None => true,
        Some(want) => cookie_value(headers, COOKIE_NAME).as_deref() == Some(want),
    }
}

async fn handle_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<Value> {
    Json(serde_json::json!({
        "ok": authorized(&state.cfg, &headers),
        "auth_required": state.cfg.server.web_token.is_some(),
    }))
}

#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

async fn handle_login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LoginBody>,
) -> Response {
    let want = match state.cfg.server.web_token.as_deref() {
        None => return Json(serde_json::json!({"ok": true, "detail": "auth not configured"})).into_response(),
        Some(w) => w.to_string(),
    };
    if body.token.trim() != want {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"ok": false}))).into_response();
    }
    let cookie = format!(
        "{COOKIE_NAME}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        want,
        30 * 24 * 3600
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

async fn handle_logout() -> Response {
    let cookie = format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

fn denied() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"ok": false, "error": "not authorized"})))
        .into_response()
}

// ---------------------------------------------------------------------------
// YouTube cookies
// ---------------------------------------------------------------------------

fn cookies_path(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("youtube-cookies.txt")
}

fn cookies_file(cfg: &Config) -> Option<PathBuf> {
    let p = cookies_path(cfg);
    p.exists().then_some(p)
}

#[derive(Serialize)]
struct CookiesStatus {
    present: bool,
    updated: Option<String>,
    youtube_lines: usize,
}

fn fmt_time(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // UTC "YYYY-MM-DD HH:MM" without pulling in a date crate.
    let (days, rem) = (secs / 86400, secs % 86400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
}

/// Howard Hinnant's days→civil-date algorithm (no chrono dependency).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i64, m, d)
}

async fn handle_cookies_get(State(state): State<Arc<AppState>>) -> Json<CookiesStatus> {
    let p = cookies_path(&state.cfg);
    if !p.exists() {
        return Json(CookiesStatus {
            present: false,
            updated: None,
            youtube_lines: 0,
        });
    }
    let (updated, youtube_lines) = match std::fs::metadata(&p).and_then(|m| m.modified()) {
        Ok(t) => (
            Some(fmt_time(t)),
            std::fs::read_to_string(&p)
                .map(|s| {
                    s.lines()
                        .filter(|l| !l.trim_start().starts_with('#') && l.contains("youtube.com"))
                        .count()
                })
                .unwrap_or(0),
        ),
        Err(_) => (None, 0),
    };
    Json(CookiesStatus {
        present: true,
        updated,
        youtube_lines,
    })
}

async fn handle_cookies_post(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_COOKIE_UPLOAD {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"ok": false, "error": "file too large (max 1 MiB)"})),
        )
            .into_response();
    }
    let s = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": "not UTF-8"})),
            )
                .into_response();
        }
    };
    if !s.lines().any(|l| l.contains("youtube.com")) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "ok": false,
                "error": "no youtube.com cookies in this file — export from a browser where you are logged in to YouTube"
            })),
        )
            .into_response();
    }
    let p = cookies_path(&state.cfg);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&p, s).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": "write failed"})),
        )
            .into_response();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Json(serde_json::json!({"ok": true, "path": p.to_string_lossy().into_owned()}))
        .into_response()
}

async fn handle_cookies_delete(State(state): State<Arc<AppState>>) -> Response {
    let p = cookies_path(&state.cfg);
    let _ = std::fs::remove_file(&p);
    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// yt-dlp lookup (search + playlist)
// ---------------------------------------------------------------------------

fn ytdlp_bin() -> Result<PathBuf, String> {
    let candidates = std::env::var("ARIA_YTDLP")
        .map(|v| vec![PathBuf::from(v)])
        .unwrap_or_default();
    let fixed = [
        PathBuf::from("/opt/ytbin/yt-dlp"),
        PathBuf::from("/usr/local/bin/yt-dlp"),
    ];
    for c in candidates.iter().chain(fixed.iter()) {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    // Fall back to PATH.
    std::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
        .map(|_| PathBuf::from("yt-dlp"))
        .map_err(|_| {
            "yt-dlp not found (looked at /opt/ytbin, /usr/local/bin, PATH) — install it on the host or set ARIA_YTDLP".into()
        })
}

#[derive(Serialize, Clone)]
struct Hit {
    id: String,
    title: String,
    duration: Option<u32>,
    channel: Option<String>,
    url: String,
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn hits_from_entries(v: &Value) -> Vec<Hit> {
    let entries = match v.get("entries").and_then(|e| e.as_array()) {
        Some(e) => e,
        None => return vec![entry_to_hit(v)],
    };
    entries.iter().filter_map(|e| entry_to_hit_opt(e)).collect()
}

fn entry_to_hit(v: &Value) -> Hit {
    entry_to_hit_opt(v).unwrap_or_else(|| Hit {
        id: String::new(),
        title: "unknown".into(),
        duration: None,
        channel: None,
        url: String::new(),
    })
}

fn entry_to_hit_opt(v: &Value) -> Option<Hit> {
    let id = str_of(v, "id")?;
    if id.is_empty() {
        return None;
    }
    let url = str_of(v, "url")
        .filter(|u| u.starts_with("http"))
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));
    let duration = v.get("duration").and_then(|d| d.as_f64()).map(|d| d as u32);
    let channel = str_of(v, "channel").or_else(|| str_of(v, "uploader"));
    Some(Hit {
        id,
        title: str_of(v, "title").unwrap_or_default(),
        duration,
        channel,
        url,
    })
}

async fn yt_flat_json(cfg: &Config, target: &str) -> Result<Value, String> {
    let bin = ytdlp_bin()?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("--no-warnings")
        .arg("--flat-playlist")
        .arg("-J")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(f) = cookies_file(cfg) {
        cmd.arg("--cookies").arg(f);
    }
    cmd.arg(target);
    let out = tokio::time::timeout(Duration::from_secs(YTDLP_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| "yt-dlp timed out (120 s)".to_string())?
        .map_err(|e| format!("cannot run yt-dlp: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: String = err.lines().take(4).collect::<Vec<_>>().join(" | ");
        return Err(format!("yt-dlp failed: {tail}"));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("bad yt-dlp JSON: {e}"))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_n")]
    n: usize,
}

fn default_n() -> usize {
    10
}

async fn handle_search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Response {
    if !authorized(&state.cfg, &headers) {
        return denied();
    }
    let query = q.q.trim().to_string();
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "empty query"})),
        )
            .into_response();
    }
    let n = q.n.clamp(1, 50);
    let target = format!("ytsearch{n}:{query}");
    match yt_flat_json(&state.cfg, &target).await {
        Ok(v) => Json(serde_json::json!({
            "ok": true,
            "title": v.get("title").and_then(|t| t.as_str()).unwrap_or(&query),
            "hits": hits_from_entries(&v),
        }))
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct PlaylistQuery {
    url: String,
}

async fn handle_playlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<PlaylistQuery>,
) -> Response {
    if !authorized(&state.cfg, &headers) {
        return denied();
    }
    let url = q.url.trim();
    if !url.starts_with("http") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "URL must start with http(s)"})),
        )
            .into_response();
    }
    match yt_flat_json(&state.cfg, url).await {
        Ok(v) => {
            let channel = v
                .get("channel")
                .or_else(|| v.get("uploader"))
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
            Json(serde_json::json!({
                "ok": true,
                "title": v.get("title").and_then(|t| t.as_str()).unwrap_or(url),
                "channel": channel,
                "hits": hits_from_entries(&v),
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// the download queue
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DlStatus {
    Queued,
    Downloading {
        progress: f32,
        detail: Option<String>,
    },
    Done {
        path: String,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct DlItem {
    pub id: String,
    pub url: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub collection: String,
    pub track: Option<u32>,
    pub status: DlStatus,
    pub added_at: String,
}

pub struct DlQueue {
    items: Mutex<Vec<DlItem>>,
    cancels: Mutex<std::collections::HashSet<String>>,
    file: PathBuf,
    changed: tokio::sync::Notify,
}

fn queue_path(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("dl-queue.json")
}

impl DlQueue {
    pub fn load(cfg: &Config) -> Arc<Self> {
        let file = queue_path(cfg);
        let mut items = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<DlItem>>(&raw).ok())
            .unwrap_or_default();
        // A restart interrupts any in-flight download: park it as failed so
        // the user can retry instead of a stuck "downloading" row.
        for it in &mut items {
            if let DlStatus::Downloading { .. } = it.status {
                it.status = DlStatus::Failed {
                    error: "interrupted by adapter restart — re-add or cancel".into(),
                };
            }
        }
        Arc::new(Self {
            items: Mutex::new(items),
            cancels: Mutex::new(Default::default()),
            file,
            changed: tokio::sync::Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Vec<DlItem>> {
        self.items.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self) {
        let items = self.lock().clone();
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(raw) = serde_json::to_string_pretty(&items) {
            let _ = std::fs::write(&self.file, raw);
        }
    }

    pub fn snapshot(&self) -> Vec<DlItem> {
        self.lock().clone()
    }

    pub fn add(&self, mut new: Vec<DlItem>) {
        {
            let mut items = self.lock();
            items.append(&mut new);
        }
        self.save();
        self.changed.notify_waiters();
    }

    pub fn update(&self, id: &str, status: DlStatus) {
        let mut items = self.lock();
        if let Some(it) = items.iter_mut().find(|i| i.id == id) {
            it.status = status;
        }
        self.save_unlocked(&items);
    }

    fn save_unlocked(&self, items: &[DlItem]) {
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(raw) = serde_json::to_string_pretty(items) {
            let _ = std::fs::write(&self.file, raw);
        }
    }

    pub fn next_queued(&self) -> Option<DlItem> {
        let mut items = self.lock();
        let pos = items.iter().position(|i| i.status == DlStatus::Queued)?;
        items[pos].status = DlStatus::Downloading {
            progress: 0.0,
            detail: Some("starting".into()),
        };
        let it = items[pos].clone();
        self.save_unlocked(&items);
        Some(it)
    }

    pub fn request_cancel(&self, id: &str) -> bool {
        let mut items = self.lock();
        let it = match items.iter_mut().find(|i| i.id == id) {
            Some(i) => i,
            None => return false,
        };
        match &it.status {
            DlStatus::Queued => {
                it.status = DlStatus::Cancelled;
                self.save_unlocked(&items);
                true
            }
            DlStatus::Downloading { .. } => {
                drop(items);
                self.cancels.lock().unwrap().insert(id.to_string());
                true
            }
            _ => false,
        }
    }

    pub fn cancel_requested(&self, id: &str) -> bool {
        self.cancels.lock().unwrap().contains(id)
    }

    pub fn clear_cancel(&self, id: &str) {
        self.cancels.lock().unwrap().remove(id);
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut items = self.lock();
        let pos = match items.iter().position(|i| i.id == id) {
            Some(p) => p,
            None => return false,
        };
        if matches!(items[pos].status, DlStatus::Downloading { .. }) {
            return false;
        }
        items.remove(pos);
        self.save_unlocked(&items);
        true
    }

    pub fn clear_finished(&self) -> usize {
        let mut items = self.lock();
        let before = items.len();
        items.retain(|i| !matches!(
            i.status,
            DlStatus::Done { .. } | DlStatus::Failed { .. } | DlStatus::Cancelled
        ));
        let n = before - items.len();
        if n > 0 {
            self.save_unlocked(&items);
        }
        n
    }
}

#[derive(Deserialize)]
struct QueueAddBody {
    items: Vec<QueueAddItem>,
}

#[derive(Deserialize)]
struct QueueAddItem {
    /// Full URL (video or playlist item).
    #[serde(default)]
    url: String,
    /// …or a bare video id.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    /// Collection name — must match a configured collection.
    collection: String,
    #[serde(default)]
    track: Option<u32>,
}

async fn handle_queue_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state.cfg, &headers) {
        return denied();
    }
    let collections: Vec<String> = state.cfg.collections.items.iter().map(|i| i.name.clone()).collect();
    Json(serde_json::json!({
        "ok": true,
        "items": state.dl_queue.snapshot(),
        "collections": collections,
    }))
    .into_response()
}

async fn handle_queue_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<QueueAddBody>,
) -> Response {
    if !authorized(&state.cfg, &headers) {
        return denied();
    }
    let known: Vec<String> = state.cfg.collections.items.iter().map(|i| i.name.clone()).collect();
    let mut out: Vec<DlItem> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (i, item) in body.items.iter().enumerate() {
        let col = match known.iter().find(|n| n.eq_ignore_ascii_case(&item.collection)) {
            Some(c) => c.clone(),
            None => {
                errors.push(format!(
                    "item {}: unknown collection '{}' (available: {})",
                    i + 1,
                    item.collection,
                    known.join(", ")
                ));
                continue;
            }
        };
        let url = if item.url.starts_with("http") {
            item.url.clone()
        } else if let Some(id) = item.id.as_deref().filter(|s| !s.is_empty()) {
            format!("https://www.youtube.com/watch?v={id}")
        } else if item.url.chars().count() <= 11
            && item.url.chars().all(|c| c.is_ascii_alphanumeric())
        {
            format!("https://www.youtube.com/watch?v={}", item.url)
        } else {
            errors.push(format!("item {}: no usable url or id", i + 1));
            continue;
        };
        out.push(DlItem {
            id: format!(
                "{}-{}",
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                i
            ),
            url,
            title: item.title.clone().unwrap_or_else(|| "unknown title".into()),
            artist: item.artist.clone().unwrap_or_default(),
            album: item.album.clone().unwrap_or_default(),
            collection: col,
            track: item.track,
            status: DlStatus::Queued,
            added_at: fmt_time(SystemTime::now()),
        });
    }
    if out.is_empty() && !errors.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"ok": false, "errors": errors})),
        )
            .into_response();
    }
    state.dl_queue.add(out);
    Json(serde_json::json!({"ok": true, "added": body.items.len().saturating_sub(errors.len()), "errors": errors}))
        .into_response()
}

fn queue_item_op(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    id: &str,
    op: &str,
) -> Response {
    if !authorized(&state.cfg, headers) {
        return denied();
    }
    let ok = match op {
        "cancel" => state.dl_queue.request_cancel(id),
        "remove" => state.dl_queue.remove(id),
        _ => false,
    };
    if ok {
        Json(serde_json::json!({"ok": true})).into_response()
    } else {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "error": "not found or currently downloading"})),
        )
            .into_response()
    }
}

async fn handle_queue_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    queue_item_op(&state, &headers, &id, "cancel")
}

async fn handle_queue_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    queue_item_op(&state, &headers, &id, "remove")
}

async fn handle_queue_clear_done(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state.cfg, &headers) {
        return denied();
    }
    let n = state.dl_queue.clear_finished();
    Json(serde_json::json!({"ok": true, "removed": n})).into_response()
}

// ---------------------------------------------------------------------------
// the worker: one download at a time, progress on stderr
// ---------------------------------------------------------------------------

fn slug_dir(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let t = cleaned.trim();
    if t.is_empty() {
        "Verschiedenes".to_string()
    } else {
        t.to_string()
    }
}

fn clean_title(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '-',
            c => c,
        })
        .collect();
    t.trim().trim_end_matches('.').to_string()
}

/// Parse `[download]  43.1% of ~ 5.2MiB at …` from a yt-dlp stderr line.
fn progress_from_line(line: &str) -> Option<(f32, String)> {
    let idx = line.find("[download]")?;
    let rest = &line[idx + "[download]".len()..];
    let pct_end = rest.find('%')?;
    let pct: f32 = rest[..pct_end].trim().parse().ok()?;
    let detail: String = rest[pct_end + 1..].trim_start().chars().take(60).collect();
    Some((pct.clamp(0.0, 100.0), detail))
}

async fn run_one(state: &AppState, item: &DlItem) {
    let q = &state.dl_queue;
    let mark = |status: DlStatus| q.update(&item.id, status);

    let bin = match ytdlp_bin() {
        Ok(b) => {
            tracing::debug!(bin = %b.display(), "yt-dlp resolved");
            b
        }
        Err(e) => {
            tracing::warn!(%e, "yt-dlp lookup failed");
            return mark(DlStatus::Failed { error: e });
        }
    };
    let col_path = match state.cfg.collection_path(&item.collection) {
        Some(p) => p,
        None => {
            return mark(DlStatus::Failed {
                error: format!("collection '{}' not in the adapter config", item.collection),
            })
        }
    };
    let artist_dir = slug_dir(&item.artist);
    let album_dir = slug_dir(&item.album);
    let out_dir = col_path.join(&artist_dir).join(&album_dir);
    if std::fs::create_dir_all(&out_dir).is_err() {
        return mark(DlStatus::Failed {
            error: format!("cannot create {}", out_dir.to_string_lossy()),
        });
    }
    let track_prefix = item.track.map(|t| format!("{t:02} - ")).unwrap_or_default();
    let out_tpl = out_dir.join(format!("__dl_{track_prefix}%(title)s.%(ext)s"));

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("--no-warnings")
        .arg("-f")
        .arg("bestaudio/best")
        .arg("-x")
        .arg("--audio-format")
        .arg("mp3")
        .arg("--audio-quality")
        .arg("192K")
        .arg("--embed-metadata")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if let Some(f) = cookies_file(&state.cfg) {
        cmd.arg("--cookies").arg(f);
    }
    // No --parse-metadata here: its syntax is NAME:<regex-with-named-group>
    // and forcing literal values through it is fiddly (and a wrong value
    // aborts the whole download). `finalize` re-tags the finished MP3 with
    // ffmpeg anyway — the authoritative tags come from the queue entry.
    cmd.arg("-o").arg(out_tpl.to_string_lossy().as_ref());
    cmd.arg("--").arg(&item.url);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return mark(DlStatus::Failed { error: format!("spawn: {e}") }),
    };
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    let stderr = child.stderr.take().expect("piped");
    let mut reader = BufReader::new(stderr);
    let mut last_err: Vec<String> = Vec::new();
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        if reader.read_line(&mut line_buf).await.map(|n| n == 0).unwrap_or(true) {
            break;
        }
        let l = line_buf.trim_end();
        if let Some((pct, detail)) = progress_from_line(l) {
            mark(DlStatus::Downloading {
                progress: pct,
                detail: Some(detail),
            });
        } else if l.contains("[ExtractAudio]") {
            mark(DlStatus::Downloading {
                progress: 100.0,
                detail: Some("extracting audio".into()),
            });
        } else if !l.trim().is_empty() && l.contains("ERROR") {
            last_err.push(l.trim().to_string());
            if last_err.len() > 5 {
                last_err.remove(0);
            }
        }
        if q.cancel_requested(&item.id) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            cleanup_partials(&out_dir);
            q.clear_cancel(&item.id);
            return mark(DlStatus::Cancelled);
        }
    }
    let status = child.wait().await;
    match status {
        Ok(s) if s.success() => {
            q.clear_cancel(&item.id);
            finalize(&out_dir, &track_prefix, &item).await;
            mark(DlStatus::Done {
                path: out_dir.join(format!("{track_prefix}{}.mp3", clean_title(&item.title))).to_string_lossy().into_owned(),
            });
        }
        Ok(s) => {
            q.clear_cancel(&item.id);
            cleanup_partials(&out_dir);
            let tail = if last_err.is_empty() {
                format!("exit {s}")
            } else {
                last_err.join(" | ")
            };
            mark(DlStatus::Failed { error: tail });
        }
        Err(e) => {
            q.clear_cancel(&item.id);
            mark(DlStatus::Failed { error: format!("wait: {e}") });
        }
    }
}

fn cleanup_partials(out_dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(out_dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("__dl_") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Rename the yt-dlp drop file to its final name and force the ID3 tags
/// (yt-dlp's extracted metadata is often empty/wrong for choral content).
async fn finalize(out_dir: &Path, track_prefix: &str, item: &DlItem) {
    let mut dropped: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(out_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("__dl_") && name.ends_with(".mp3") {
                dropped.push(e.path());
            }
        }
    }
    let Some(src) = dropped
        .into_iter()
        .max_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        })
    else {
        return;
    };
    let final_name = format!("{track_prefix}{}.mp3", clean_title(&item.title));
    let dst = out_dir.join(&final_name);
    if std::fs::rename(&src, &dst).is_err() {
        return;
    }
    // Force tags (fast: stream copy).
    let mut ff = tokio::process::Command::new("ffmpeg");
    ff.arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(&dst)
        .arg("-map_metadata")
        .arg("-1");
    if !item.artist.is_empty() {
        ff.arg("-metadata").arg(format!("artist={}", item.artist));
    }
    if !item.album.is_empty() {
        ff.arg("-metadata").arg(format!("album={}", item.album));
    }
    ff.arg("-metadata")
        .arg(format!("title={}", item.title));
    if let Some(t) = item.track {
        ff.arg("-metadata").arg(format!("track={t}"));
    }
    ff.arg("-c").arg("copy");
    let tmp = out_dir.join(format!(".{final_name}.tagged"));
    ff.arg(&tmp);
    let ok = ff.status().await.map(|s| s.success()).unwrap_or(false);
    if ok {
        let _ = std::fs::rename(&tmp, &dst);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The background worker: drains the queue one item at a time, then — when
/// new files landed — rescans the library and reindexes the brain.
pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let q = state.dl_queue.clone();
        loop {
            q.changed.notified().await;
            let mut done_in_round = 0usize;
            loop {
                let item = q.next_queued();
                let Some(item) = item else { break };
                tracing::debug!(id = %item.id, title = %item.title, "downloading");
                run_one(&state, &item).await;
                let done = q
                    .snapshot()
                    .into_iter()
                    .find(|i| i.id == item.id)
                    .is_some_and(|i| matches!(i.status, DlStatus::Done { .. }));
                if done {
                    done_in_round += 1;
                }
            }
            if done_in_round > 0 {
                tracing::info!(done_in_round, "download queue drained — rescan + reindex");
                let n = rescan_cycle(&state).await;
                if let Some(n) = n {
                    tracing::info!(items = n, "library rescanned after downloads");
                }
            }
        }
    });
}

/// One full rescan → publish → reindex cycle. Returns the item count.
pub async fn rescan_cycle(state: &AppState) -> Option<usize> {
    let scanned = {
        let cfg = state.cfg.clone();
        tokio::task::spawn_blocking(move || crate::scan::scan_library(&cfg))
            .await
            .ok()
            .and_then(|r| r.ok())?
    };
    let (lib, records) = scanned;
    let n = lib.len();
    let cfg = &state.cfg;
    match crate::scan::publish_if_nonempty(&cfg, &lib, &records) {
        Ok(Some(true)) => {
            if !cfg.brain.mock {
                match crate::scan::reindex(&cfg).await {
                    Ok(()) => tracing::info!(records = records.len(), "post-download reindex requested"),
                    Err(e) => tracing::warn!(%e, "post-download reindex failed"),
                }
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(%e, "post-download publish failed"),
    }
    *state.library.write().unwrap() = lib;
    Some(n)
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(handle_index))
        .route("/app.js", get(handle_app_js))
        .route("/style.css", get(handle_style_css))
        .route("/icon.svg", get(handle_icon))
        .route("/manifest.webmanifest", get(handle_manifest))
        .route("/dl/me", get(handle_me))
        .route("/dl/login", post(handle_login))
        .route("/dl/logout", post(handle_logout))
        .route("/dl/cookies", get(handle_cookies_get).post(handle_cookies_post).delete(handle_cookies_delete))
        .route("/dl/search", get(handle_search))
        .route("/dl/playlist", get(handle_playlist))
        .route("/dl/queue", get(handle_queue_get).post(handle_queue_add))
        .route("/dl/queue/clear-done", post(handle_queue_clear_done))
        .route("/dl/queue/{id}/cancel", post(handle_queue_cancel))
        .route("/dl/queue/{id}", delete(handle_queue_remove))
}
