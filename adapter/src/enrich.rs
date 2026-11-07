//! Catalog enrichment (the "Katalog" tool): for albums the library has no
//! cover art (or no year) for, look the release up on MusicBrainz, fetch
//! the front cover from the Cover Art Archive, let the user preview it,
//! and — on approval — embed the image into the tracks and write the year
//! back into the tags. Everything is cached on disk (survives restarts,
//! never re-hits the network for the same album).
//!
//! Upstream etiquette: MusicBrainz requires a descriptive User-Agent and
//! asks for ≤1 request/second — a single global gate enforces the pace.
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::http::StatusCode;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MB_BASE: &str = "https://musicbrainz.org/ws/2";
const CAA_BASE: &str = "https://coverartarchive.org";
const USER_AGENT: &str =
    "aria-media/1.0 ( https://aria.eisler-systems.de ) adapter@eisler-systems.de";
const MIN_INTERVAL: Duration = Duration::from_millis(1100);

/// One cached enrichment result (persisted at {data_dir}/enrich-cache.json;
/// the image itself lives at {data_dir}/covers/<key>.<ext>).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichEntry {
    pub key: String,
    pub artist: String,
    pub album: String,
    /// MusicBrainz best match (None = searched, nothing good).
    pub found: bool,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub release_title: String,
    #[serde(default)]
    pub release_artist: String,
    #[serde(default)]
    pub year: String,
    #[serde(default)]
    pub mbid: String,
    /// The Cover Art Archive image, if one exists for the release.
    #[serde(default)]
    pub art_b64: String,
    #[serde(default)]
    pub art_mime: String,
    /// A human note when the search failed or found no art.
    #[serde(default)]
    pub note: String,
    pub updated_at: u64,
}

/// The enrichment store: in-memory cache + disk persistence + the
/// MusicBrainz pace gate. Lives inside AppState (one per process).
pub struct EnrichState {
    client: reqwest::Client,
    data_dir: std::path::PathBuf,
    cache: tokio::sync::Mutex<HashMap<String, EnrichEntry>>,
    gate: tokio::sync::Mutex<Option<Instant>>,
    /// One lookup in flight at a time (the UI paces itself, this is the
    /// backstop against two tabs racing).
    busy: AtomicBool,
}

impl EnrichState {
    pub fn new(data_dir: &std::path::Path) -> Self {
        std::fs::create_dir_all(data_dir.join("covers")).ok();
        let mut cache = HashMap::new();
        if let Ok(raw) = std::fs::read_to_string(data_dir.join("enrich-cache.json")) {
            if let Ok(entries) = serde_json::from_str::<Vec<EnrichEntry>>(&raw) {
                for e in entries {
                    cache.insert(e.key.clone(), e);
                }
            }
        }
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(USER_AGENT)
                .build()
                .expect("http client builds"),
            data_dir: data_dir.to_path_buf(),
            cache: tokio::sync::Mutex::new(cache),
            gate: tokio::sync::Mutex::new(None),
            busy: AtomicBool::new(false),
        }
    }

    /// Cache + image persistence (async — blocking_lock would panic inside
    /// the tokio runtime). The file writes are tiny; inline is fine.
    async fn persist(&self, entry: &EnrichEntry) {
        {
            let mut cache = self.cache.lock().await;
            cache.insert(entry.key.clone(), entry.clone());
            let all: Vec<EnrichEntry> = cache.values().cloned().collect();
            let file = self.data_dir.join("enrich-cache.json");
            if let Ok(raw) = serde_json::to_string(&all) {
                std::fs::write(&file, raw).ok();
            }
        }
        if !entry.art_b64.is_empty() {
            let ext = if entry.art_mime.contains("png") { "png" } else { "jpg" };
            let file = self
                .data_dir
                .join("covers")
                .join(format!("{}.{}", sanitize_name(&entry.key), ext));
            if let Ok(bytes) = base64_decode(&entry.art_b64) {
                std::fs::write(&file, bytes).ok();
            }
        }
    }

    /// The ≤1 req/s gate (shared across concurrent lookups).
    async fn pace(&self) {
        let mut gate = self.gate.lock().await;
        if let Some(last) = *gate {
            if last.elapsed() < MIN_INTERVAL {
                tokio::time::sleep(MIN_INTERVAL - last.elapsed()).await;
            }
        }
        *gate = Some(Instant::now());
    }
}

fn sanitize_name(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.chars().take(80).collect()
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Word-overlap similarity for release matching. Both directions are
/// considered (the user's album may be a SHORT form — "Oceans" — or a
/// LONG form — "Oceans (Where Feet May Fail)" — of the MusicBrainz
/// title): `max(|A∩B|/|A|, |A∩B|/|B|)` over normalized words (length ≥ 2,
/// punctuation stripped). "Whole Heart" vs "Oceans (…)" stays 0.
fn word_overlap(a: &str, b: &str) -> f64 {
    let words = |s: &str| -> std::collections::HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 2)
            .map(|w| w.to_ascii_lowercase())
            .collect()
    };
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let common = wa.iter().filter(|w| wb.contains(*w)).count();
    let d1 = common as f64 / wa.len() as f64;
    let d2 = common as f64 / wb.len() as f64;
    d1.max(d2)
}

fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

/// GET /dl/enrich/plan?collection=musik — the distinct albums of the
/// collection with what we already know (art embedded or cached).
async fn handle_plan(
    state: State<Arc<crate::state::AppState>>,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let collection = query
        .get("collection")
        .cloned()
        .unwrap_or_else(|| "musik".into());
    // The std library guard is !Send — scope it so it never lives across
    // the tokio await below.
    let by_album: std::collections::BTreeMap<(String, String), (String, usize, bool)> = {
        let lib = state.library.read().unwrap();
        let mut m = std::collections::BTreeMap::new();
        for t in lib.iter() {
            if t.collection != collection || t.kind != aria_audio::CollectionKind::Music {
                continue;
            }
            if t.artist.trim().is_empty() || t.album.trim().is_empty() {
                continue;
            }
            let e = m
                .entry((t.artist.trim().to_string(), t.album.trim().to_string()))
                .or_insert_with(|| (t.year.trim().to_string(), 0, false));
            e.1 += 1;
            e.2 = e.2 || t.cover.is_some();
        }
        m
    };
    let cache = state.enrich.cache.lock().await;
    let albums: Vec<serde_json::Value> = by_album
        .into_iter()
        .map(|((artist, album), (year, n, has_art))| {
            let key = format!("{collection}|{artist}|{album}");
            let cached_art = cache.get(&key).map(|e| !e.art_b64.is_empty()).unwrap_or(false);
            serde_json::json!({
                "key": key,
                "artist": artist,
                "album": album,
                "year": year,
                "tracks": n,
                "has_art": has_art || cached_art,
            })
        })
        .collect();
    Json(serde_json::json!({"ok": true, "albums": albums})).into_response()
}

/// POST /dl/enrich/lookup {collection, artist, album, key} — MusicBrainz
/// search + Cover Art fetch for one album (cached, paced).
#[derive(Deserialize)]
struct LookupBody {
    collection: Option<String>,
    artist: String,
    album: String,
    key: Option<String>,
}

async fn handle_lookup(
    state: State<Arc<crate::state::AppState>>,
    headers: HeaderMap,
    Json(body): Json<LookupBody>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let collection = body.collection.unwrap_or_else(|| "musik".into());
    let key = body.key.unwrap_or_else(|| {
        format!("{}|{}|{}", collection, body.artist, body.album)
    });

    // Already cached (a search that found nothing is cached too — the
    // network is the expensive part, not the answer).
    {
        let cache = state.enrich.cache.lock().await;
        if let Some(e) = cache.get(&key) {
            return Json(serde_json::json!({
                "ok": true,
                "cached": true,
                "entry": e,
            }))
            .into_response();
        }
    }

    if !state
        .enrich
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "ok": false,
                "error": "lookup already in flight — retry shortly"
            })),
        )
            .into_response();
    }
    let entry = do_lookup(&state.enrich, &key, &collection, &body.artist, &body.album).await;
    state.enrich.busy.store(false, Ordering::SeqCst);
    state.enrich.persist(&entry).await;
    Json(serde_json::json!({"ok": true, "cached": false, "entry": entry})).into_response()
}

async fn do_lookup(
    enrich: &EnrichState,
    key: &str,
    _collection: &str,
    artist: &str,
    album: &str,
) -> EnrichEntry {
    let mut entry = EnrichEntry {
        key: key.to_string(),
        artist: artist.to_string(),
        album: album.to_string(),
        found: false,
        score: 0.0,
        release_title: String::new(),
        release_artist: String::new(),
        year: String::new(),
        mbid: String::new(),
        art_b64: String::new(),
        art_mime: String::new(),
        note: String::new(),
        updated_at: now_secs(),
    };
    // 1) MusicBrainz release search.
    enrich.pace().await;
    let url = format!(
        "{MB_BASE}/release/?query={}&fmt=json&limit=5",
        urlencoded(&format!(r#"release:"{album}" AND artist:"{artist}""#))
    );
    let releases: Vec<serde_json::Value> = match enrich.client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("releases").and_then(|r| r.as_array()).cloned())
            .unwrap_or_default(),
        Ok(r) => {
            entry.note = format!("MusicBrainz HTTP {}", r.status());
            return entry;
        }
        Err(e) => {
            entry.note = format!("MusicBrainz error: {e}");
            return entry;
        }
    };

    let mut best: Option<(f64, serde_json::Value)> = None;
    for rel in &releases {
        let title = rel.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let credit: Vec<String> = rel
            .get("artist-credit")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let score = 0.6 * word_overlap(album, title) + 0.4 * word_overlap(artist, &credit.join(", "));
        if score > best.as_ref().map(|(s, _)| *s).unwrap_or(0.0) {
            best = Some((score, rel.clone()));
        }
    }

    let Some((score, rel)) = best else {
        entry.note = "kein MusicBrainz-Treffer".into();
        return entry;
    };
    if score < 0.5 {
        entry.note = format!("best match too weak (score {score:.2})");
        return entry;
    }
    entry.found = true;
    entry.score = score;
    entry.release_title = rel.get("title").and_then(|t| t.as_str()).unwrap_or("").into();
    entry.release_artist = rel
        .get("artist-credit")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    entry.mbid = rel.get("id").and_then(|i| i.as_str()).unwrap_or("").into();
    entry.year = rel
        .get("date")
        .and_then(|d| d.as_str())
        .map(|d| d.chars().take(4).collect())
        .unwrap_or_default();
    let rgid = rel
        .get("release-group")
        .and_then(|g| g.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or("");

    // 2) Cover Art Archive front image (release group first — it covers
    //    all editions — then the release itself).
    for base in [
        format!("{CAA_BASE}/release-group/{rgid}"),
        format!("{CAA_BASE}/release/{}", entry.mbid),
    ] {
        let url = format!("{base}/front-500");
        enrich.pace().await;
        match enrich.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let mime = r
                    .headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("image/jpeg")
                    .to_string();
                if let Ok(bytes) = r.bytes().await {
                    if (8 * 1024..=8 * 1024 * 1024).contains(&bytes.len()) {
                        entry.art_b64 = base64_encode(&bytes);
                        entry.art_mime = mime;
                        break;
                    }
                }
            }
            Ok(_) => {} // 404 — try the next source
            Err(e) => {
                if entry.art_b64.is_empty() {
                    entry.note = format!("Cover Art error: {e}");
                }
                break;
            }
        }
    }
    if entry.art_b64.is_empty() && entry.note.is_empty() {
        entry.note = "kein Cover beim Cover Art Archive".into();
    }
    entry
}

/// POST /dl/enrich/apply {keys, update_year} — embed the cached covers
/// into the tracks + write the year. Admin only (touches the files).
#[derive(Deserialize)]
struct ApplyBody {
    keys: Vec<String>,
    #[serde(default = "default_true")]
    update_year: bool,
}

fn default_true() -> bool {
    true
}

async fn handle_apply(
    state: State<Arc<crate::state::AppState>>,
    headers: HeaderMap,
    Json(body): Json<ApplyBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if u.role != "admin" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "ok": false,
                "error": "enrichment writes files — admin only"
            })),
        )
            .into_response();
    }
    // 1) Snapshot the requested cache entries (tokio lock, no std guard).
    let mut entries: Vec<EnrichEntry> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    {
        let cache = state.enrich.cache.lock().await;
        for key in &body.keys {
            match cache.get(key) {
                Some(e) => entries.push(e.clone()),
                None => errors.push(format!("{key}: kein Cache-Eintrag (erst lookup)")),
            }
        }
    }
    // 2) Resolve the tracks of each album (std guard, scoped — no await
    //    inside: the guard is !Send).
    let mut jobs: Vec<(EnrichEntry, Vec<(String, String)>)> = Vec::new();
    {
        let lib = state.library.read().unwrap();
        for e in entries {
            if e.art_b64.is_empty() {
                errors.push(format!("{}: kein Cover im Cache", e.album));
                continue;
            }
            let tracks: Vec<(String, String)> = lib
                .iter()
                .filter(|t| {
                    t.artist.trim() == e.artist.trim() && t.album.trim() == e.album.trim()
                })
                .map(|t| (t.path.clone(), t.title.clone()))
                .collect();
            if tracks.is_empty() {
                errors.push(format!("{}: keine Tracks gefunden", e.album));
                continue;
            }
            jobs.push((e, tracks));
        }
    }
    // 3) Write the files (no locks held).
    let mut applied = 0usize;
    let mut files = 0usize;
    for (e, tracks) in &jobs {
        let Ok(art) = base64_decode(&e.art_b64) else {
            errors.push(format!("{}: Cover dekodieren fehlgeschlagen", e.album));
            continue;
        };
        for (path, title) in tracks {
            if let Err(err) = aria_audio::set_cover(std::path::Path::new(path), &art, &e.art_mime)
            {
                errors.push(format!("{title}: {err}"));
                continue;
            }
            if body.update_year && !e.year.is_empty() {
                let _ = aria_audio::set_tags(
                    std::path::Path::new(path),
                    &aria_audio::TagEdit {
                        year: Some(e.year.clone()),
                        ..Default::default()
                    },
                );
            }
            files += 1;
        }
        applied += 1;
    }
    let reindex = crate::web::rescan_cycle(&state).await.is_some();
    Json(serde_json::json!({
        "ok": true,
        "applied": applied,
        "files": files,
        "errors": errors,
        "reindex": reindex,
    }))
    .into_response()
}

/// GET /art/cache/{key} — the cached Cover Art image (the SPA previews
/// it before the user approves the apply).
async fn handle_cached_art(
    state: State<Arc<crate::state::AppState>>,
    headers: HeaderMap,
    UrlPath(key): UrlPath<String>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let cache = state.enrich.cache.lock().await;
    match cache.get(&key) {
        Some(e) if !e.art_b64.is_empty() => {
            let bytes = base64_decode(&e.art_b64).unwrap_or_default();
            (
                StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, e.art_mime.clone()),
                    (axum::http::header::CACHE_CONTROL, "max-age=86400".to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        _ => (StatusCode::NOT_FOUND, "no cached art").into_response(),
    }
}

pub fn routes() -> Router<Arc<crate::state::AppState>> {
    Router::new()
        .route("/dl/enrich/plan", get(handle_plan))
        .route("/dl/enrich/lookup", post(handle_lookup))
        .route("/dl/enrich/apply", post(handle_apply))
        .route("/art/cache/{key}", get(handle_cached_art))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_overlap_scores_similarity() {
        // short form vs long form (either direction)
        assert!(word_overlap("Oceans", "Oceans (Where Feet May Fail)") > 0.9);
        assert!(word_overlap("Oceans (Where Feet May Fail)", "Oceans") > 0.9);
        // unrelated titles
        assert!(word_overlap("Whole Heart", "Oceans (Where Feet May Fail)") < 0.2);
        // identical
        assert_eq!(word_overlap("Faszination Panflöte 2", "Faszination Panflöte 2"), 1.0);
    }

    #[test]
    fn urlencoding_keeps_query_safe() {
        assert_eq!(urlencoded("a b\"c"), "a%20b%22c");
        assert_eq!(urlencoded("Panflöte"), "Panfl%C3%B6te");
    }
}
