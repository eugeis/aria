//! Free, legally usable audio sources: search, preview, download.
//!
//! The user composes videos and needs royalty-free music/sounds: the app
//! searches a source, previews the result in place, and downloads the file
//! into the `sounds` collection. License + attribution are written INTO the
//! file's tags, so the legal info survives any later reorganization.
//!
//! Sources:
//!   * openverse  — no key needed (CC-licensed audio: Jamendo, Freesound, …)
//!   * freesound  — needs FREESOUND_API_TOKEN (free account)
//!   * pixabay    — needs PIXABAY_API_KEY (free account)
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;

use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/dl/sources", get(handle_list))
        .route("/dl/sources/search", get(handle_search))
        .route("/dl/sources/download", post(handle_download))
}

struct SourceDef {
    name: &'static str,
    about: &'static str,
    key_env: Option<&'static str>,
}

fn defs() -> Vec<SourceDef> {
    vec![
        SourceDef {
            name: "openverse",
            about: "Creative-Commons-Audio (Jamendo, Freesound, …) — ohne Key",
            key_env: None,
        },
        SourceDef {
            name: "freesound",
            about: "Freesound.org — Community-Sounds (API-Token nötig)",
            key_env: Some("FREESOUND_API_TOKEN"),
        },
        SourceDef {
            name: "pixabay",
            about: "Pixabay Music — lizenzfreie Tracks (API-Key nötig)",
            key_env: Some("PIXABAY_API_KEY"),
        },
    ]
}

fn key_of(env: Option<&str>) -> Option<String> {
    env.and_then(|e| std::env::var(e).ok())
        .filter(|v| !v.trim().is_empty())
}

/// GET /dl/sources — which sources are available (and which need a key).
async fn handle_list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let sources = defs()
        .into_iter()
        .map(|d| {
            let enabled = d
                .key_env
                .map(|env| key_of(Some(env)).is_some())
                .unwrap_or(true);
            serde_json::json!({ "name": d.name, "about": d.about, "enabled": enabled })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "ok": true, "sources": sources })).into_response()
}

#[derive(Deserialize)]
struct SearchQuery {
    source: String,
    q: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    page: Option<u32>,
}

/// GET /dl/sources/search?source=&q=&license=&page= — normalized results:
/// {title, creator, duration_sec, license, url, preview_url, landing_url}.
async fn handle_search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let def = match defs().into_iter().find(|d| d.name == query.source) {
        Some(d) => d,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": format!("unknown source `{}`", query.source)})),
            )
                .into_response();
        }
    };
    if let Some(env) = def.key_env {
        if key_of(Some(env)).is_none() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": format!("source `{}` needs a free API key ({env}) — not configured", def.name)})),
            )
                .into_response();
        }
    }
    if query.q.trim().is_empty() {
        return Json(serde_json::json!({"ok": true, "results": []})).into_response();
    }
    let page = query.page.unwrap_or(1).max(1);
    let out = match def.name {
        "openverse" => search_openverse(&query.q, query.license.as_deref(), page).await,
        "freesound" => search_freesound(&query.q, page).await,
        "pixabay" => search_pixabay(&query.q, page).await,
        _ => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "unhandled source".to_string(),
        )),
    };
    match out {
        Ok(v) => Json(v).into_response(),
        Err((code, message)) => (
            code,
            Json(serde_json::json!({ "ok": false, "error": message })),
        )
            .into_response(),
    }
}

type SearchErr = (StatusCode, String);
type SearchOutcome = Result<serde_json::Value, SearchErr>;

fn client() -> Result<reqwest::Client, SearchErr> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("http client: {e}")))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value, SearchErr> {
    let resp = client
        .get(url)
        .header("User-Agent", "aria-home/1.0 (personal library)")
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("source unreachable: {e}")))?;
    if !resp.status().is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("source answered with HTTP {}", resp.status().as_u16()),
        ));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("bad source response: {e}")))
}

async fn search_openverse(q: &str, license: Option<&str>, page: u32) -> SearchOutcome {
    let mut url = format!(
        "https://api.openverse.org/v1/audio/?q={}&page_size=20&page={page}",
        urlenc(q)
    );
    if let Some(l) = license.filter(|l| !l.is_empty() && *l != "any") {
        url.push_str(&format!("&license={l}"));
    }
    let client = client()?;
    let v = get_json(&client, &url).await?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let url = r.get("url")?.as_str()?;
                    Some(serde_json::json!({
                        "title": r.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                        "creator": r.get("creator").and_then(|t| t.as_str()).unwrap_or(""),
                        "duration_sec": r.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0),
                        "license": r.get("license").and_then(|t| t.as_str()).unwrap_or(""),
                        "url": url,
                        "preview_url": url,
                        "landing_url": r.get("foreign_landing_url").and_then(|t| t.as_str()).unwrap_or(""),
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(serde_json::json!({
        "ok": true,
        "source": "openverse",
        "total": v.get("result_count").and_then(|t| t.as_u64()).unwrap_or(0),
        "results": results,
    }))
}

async fn search_freesound(q: &str, page: u32) -> SearchOutcome {
    let token = key_of(Some("FREESOUND_API_TOKEN")).unwrap_or_default();
    let url = format!(
        "https://freesound.org/apiv2/search/text/?query={}&page={page}&token={token}",
        urlenc(q)
    );
    let client = client()?;
    let v = get_json(&client, &url).await?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let url = r.get("url").and_then(|t| t.as_str())?;
                    let id = r.get("id").and_then(|i| i.as_u64());
                    Some(serde_json::json!({
                        "title": r.get("name").and_then(|t| t.as_str()).unwrap_or(""),
                        "creator": r.get("username").and_then(|t| t.as_str()).unwrap_or(""),
                        "duration_sec": r.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0),
                        "license": r.get("license").and_then(|t| t.as_str()).unwrap_or(""),
                        "url": url,
                        "preview_url": url,
                        "landing_url": id.map(|i| format!("https://freesound.org/sounds/{i}/")).unwrap_or_default(),
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(serde_json::json!({
        "ok": true,
        "source": "freesound",
        "total": v.get("count").and_then(|t| t.as_u64()).unwrap_or(0),
        "results": results,
    }))
}

async fn search_pixabay(q: &str, page: u32) -> SearchOutcome {
    let key = key_of(Some("PIXABAY_API_KEY")).unwrap_or_default();
    let url = format!(
        "https://pixabay.com/api/?key={key}&q={}&media=music&per_page=20&page={page}",
        urlenc(q)
    );
    let client = client()?;
    let v = get_json(&client, &url).await?;
    let results = v
        .get("hits")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let url = r.get("url").and_then(|t| t.as_str())?;
                    let tags = r.get("tags").and_then(|t| t.as_str()).unwrap_or("");
                    Some(serde_json::json!({
                        "title": tags.split(',').next().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Pixabay Track"),
                        "creator": r.get("artist").and_then(|t| t.as_str()).unwrap_or(""),
                        "duration_sec": r.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0),
                        "license": "Pixabay Content License",
                        "url": url,
                        "preview_url": url,
                        "landing_url": r.get("pageURL").and_then(|t| t.as_str()).unwrap_or(""),
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(serde_json::json!({
        "ok": true,
        "source": "pixabay",
        "total": v.get("totalHits").and_then(|t| t.as_u64()).unwrap_or(0),
        "results": results,
    }))
}

/// Minimal percent-encoding for query values (no extra crate needed).
fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Deserialize)]
struct DownloadBody {
    url: String,
    title: String,
    #[serde(default)]
    creator: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    attribution: Option<String>,
}

/// POST /dl/sources/download — fetch the file into the `sounds` collection,
/// tag it (title, creator, "Freie Sounds" album, license/attribution
/// comment), then rescan so it is browsable + searchable immediately.
async fn handle_download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DownloadBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if u.role != "admin" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "downloading into the shared library — admin only"})),
        )
            .into_response();
    }
    if !body.url.starts_with("https://") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "only https download urls are allowed"})),
        )
            .into_response();
    }
    let ext = body
        .url
        .split('?')
        .next()
        .unwrap_or("")
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(
        ext.as_str(),
        "mp3" | "ogg" | "oga" | "flac" | "m4a" | "aac" | "opus" | "wav"
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": format!("unsupported audio type `.{ext}` (only mp3/ogg/flac/m4a/aac/wav)")})),
        )
            .into_response();
    }
    // Target dir: the `sounds` collection when configured, else <data>/sounds.
    let dir = state
        .cfg
        .collection_items()
        .ok()
        .and_then(|items| {
            items
                .into_iter()
                .find(|i| i.name == "sounds")
                .map(|i| i.path)
        })
        .unwrap_or_else(|| state.cfg.data_dir().join("sounds"));
    if std::fs::create_dir_all(&dir).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": "could not create the sounds dir"})),
        )
            .into_response();
    }
    let safe: String = body
        .title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            _ => c,
        })
        .collect();
    let stem = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = if stem.is_empty() {
        "sound".to_string()
    } else {
        stem
    };
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut i = 2u32;
    while path.exists() {
        path = dir.join(format!("{stem} ({i}).{ext}"));
        i += 1;
    }
    let tmp = dir.join(format!(".{stem}.aria-dl.tmp"));
    let url = body.url.clone();

    let report = tokio::task::spawn_blocking({
        let tmp = tmp.clone();
        let path = path.clone();
        move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(300))
                    .build()
                    .map_err(|e| e.to_string())?;
                let resp = client
                    .get(&url)
                    .header("User-Agent", "aria-home/1.0 (personal library)")
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("download failed: HTTP {}", resp.status().as_u16()));
                }
                use futures_util::StreamExt;
                use tokio::io::AsyncWriteExt;
                let mut out = tokio::fs::File::create(&tmp)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut stream = resp.bytes_stream();
                let mut total = 0u64;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| e.to_string())?;
                    total += chunk.len() as u64;
                    if total > 200 * 1024 * 1024 {
                        return Err("file too large (>200 MB)".to_string());
                    }
                    out.write_all(&chunk).await.map_err(|e| e.to_string())?;
                }
                out.flush().await.map_err(|e| e.to_string())?;
                drop(out);
                tokio::fs::rename(&tmp, &path)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<u64, String>(total)
            })
        }
    })
    .await;
    let size = match report {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&tmp);
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"ok": false, "error": e})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": format!("task: {e}")})),
            )
                .into_response();
        }
    };

    // The legal info goes INTO the file's comment tag — it survives any
    // later move/reorg and stays visible in the library.
    let mut comment = body
        .attribution
        .clone()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| {
            let origin = body.url.split('/').take(3).collect::<Vec<_>>().join("/");
            format!("Downloaded via aria from {origin}")
        });
    if let Some(l) = body.license.as_ref().filter(|l| !l.trim().is_empty()) {
        comment = format!("{comment} · Lizenz: {l}");
    }
    let edit = aria_audio::TagEdit {
        title: Some(body.title.clone()),
        artist: body.creator.clone().filter(|c| !c.trim().is_empty()),
        album: Some("Freie Sounds".to_string()),
        genre: None,
        language: None,
        comment: Some(comment),
    };
    if let Err(e) = aria_audio::set_tags(&path, &edit) {
        tracing::warn!(error = %e, "sources download: tagging failed (file kept)");
    }
    let reindex = crate::web::rescan_cycle(&state).await.is_some();
    Json(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "bytes": size,
        "reindex": reindex,
    }))
    .into_response()
}
