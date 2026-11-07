//! The music-manager API behind the web app: library browsing + instant
//! search, in-browser streaming, and persistent playlists (create,
//! organize, reorder, play-all, download-all).
//!
//! All routes live under /dl (gated by the same identity check as the
//! download manager) and are served by the adapter — the in-memory
//! library is right here.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use aria_audio::{CollectionKind, StreamSource};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// library browse + search
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TrackView {
    id: u64,
    title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    artist: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    album: String,
    collection: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_no: Option<u32>,
    duration_ms: u64,
    /// Media-root-relative path (the offline catalog's key — the SPA
    /// saves for offline by this path).
    rel_path: String,
    /// True when the track (or its album, via the scan's per-album cover
    /// dedupe) carries embedded art — the SPA then requests /art/<id> for
    /// the card/hero image.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    has_art: bool,
}

fn kind_str(k: CollectionKind) -> &'static str {
    match k {
        CollectionKind::Music => "music",
        CollectionKind::Audiobook => "audiobook",
        CollectionKind::Sermon => "sermon",
    }
}

fn track_view(t: &aria_audio::Track) -> TrackView {
    TrackView {
        id: t.id,
        title: t.title.clone(),
        artist: t.artist.clone(),
        album: t.album.clone(),
        collection: t.collection.clone(),
        kind: kind_str(t.kind).to_string(),
        track_no: t.track_no,
        duration_ms: t.duration_ms,
        rel_path: t
            .path
            .strip_prefix("/media/")
            .map(str::to_string)
            .unwrap_or_else(|| t.path.rsplit('/').next().unwrap_or("").to_string()),
        has_art: t.cover.is_some(),
    }
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // "2026-07-10 08:00" UTC (cosmetic; the same civil algorithm as web.rs)
    let (days, rem) = (secs / 86400, secs % 86400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
}

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
    ((if m <= 2 { y + 1 } else { y }), m, d)
}

#[derive(Serialize)]
struct AlbumView {
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    year: String,
    /// A track id with embedded art (the SPA renders /art/<id> as the
    /// album card image); None = no art (generated tile).
    #[serde(skip_serializing_if = "Option::is_none")]
    art_id: Option<u64>,
    tracks: Vec<TrackView>,
}

#[derive(Serialize)]
struct ArtistView {
    name: String,
    /// The first available album art across the artist's albums.
    #[serde(skip_serializing_if = "Option::is_none")]
    art_id: Option<u64>,
    albums: Vec<AlbumView>,
}

#[derive(Serialize)]
struct CollectionView {
    name: String,
    kind: String,
    count: usize,
    artists: Vec<ArtistView>,
}

fn sort_tracks<'a>(tracks: impl Iterator<Item = &'a aria_audio::Track>) -> Vec<TrackView> {
    let mut v: Vec<TrackView> = tracks.map(track_view).collect();
    v.sort_by(|a, b| {
        a.collection
            .cmp(&b.collection)
            .then(a.artist.cmp(&b.artist))
            .then(a.album.cmp(&b.album))
            .then(a.track_no.cmp(&b.track_no))
            .then(a.title.cmp(&b.title))
    });
    v
}

/// The browse hierarchy: collection → artist → album → tracks (a track
/// without album/artist lands in the collection's "Unbekannt" bucket).
async fn handle_library(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let lib = state.library.read().unwrap();
    let mut by_collection: std::collections::BTreeMap<
        String,
        (CollectionKind, Vec<&aria_audio::Track>),
    > = std::collections::BTreeMap::new();
    for t in lib.iter() {
        by_collection
            .entry(t.collection.clone())
            .or_insert_with(|| (t.kind, Vec::new()))
            .1
            .push(t);
    }
    let mut collections = Vec::new();
    for (name, (kind, tracks)) in &by_collection {
        let mut by_artist: std::collections::BTreeMap<String, Vec<&aria_audio::Track>> =
            std::collections::BTreeMap::new();
        for t in tracks {
            let artist = if t.artist.trim().is_empty() {
                "Unbekannt".to_string()
            } else {
                t.artist.trim().to_string()
            };
            by_artist.entry(artist).or_default().push(t);
        }
        let mut artists = Vec::new();
        for (a_name, a_tracks) in &by_artist {
            let mut by_album: std::collections::BTreeMap<String, Vec<&aria_audio::Track>> =
                std::collections::BTreeMap::new();
            for t in a_tracks {
                let album = if t.album.trim().is_empty() {
                    "Verschiedenes".to_string()
                } else {
                    t.album.trim().to_string()
                };
                by_album.entry(album).or_default().push(t);
            }
            let mut albums = Vec::new();
            for (al_name, al_tracks) in &by_album {
                let year = al_tracks
                    .iter()
                    .find(|t| !t.year.trim().is_empty())
                    .map(|t| t.year.trim().to_string())
                    .unwrap_or_default();
                let mut list = sort_tracks(al_tracks.iter().copied());
                list.sort_by(|a, b| a.track_no.cmp(&b.track_no).then(a.title.cmp(&b.title)));
                let art_id = al_tracks
                    .iter()
                    .find(|t| t.cover.is_some())
                    .map(|t| t.id);
                albums.push(AlbumView {
                    name: al_name.clone(),
                    year,
                    art_id,
                    tracks: list,
                });
            }
            let art_id = albums.iter().find_map(|al| al.art_id);
            artists.push(ArtistView {
                name: a_name.clone(),
                art_id,
                albums,
            });
        }
        collections.push(CollectionView {
            name: name.clone(),
            kind: kind_str(*kind).to_string(),
            count: tracks.len(),
            artists,
        });
    }
    Json(serde_json::json!({
        "ok": true,
        "total": lib.len(),
        "collections": collections,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    /// Optional collection filter (name, case-insensitive).
    #[serde(default)]
    collection: Option<String>,
}

/// Instant, deterministic library search (title/artist/album substring,
/// no LLM): score = title hits > artist hits > album hits; top 50.
async fn handle_library_search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<SearchQuery>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let needle = q.q.trim().to_lowercase();
    if needle.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "empty query"})),
        )
            .into_response();
    }
    let col = q.collection.as_deref().map(|s| s.trim().to_lowercase());
    let lib = state.library.read().unwrap();
    let mut scored: Vec<(u8, TrackView)> = Vec::new();
    for t in lib.iter() {
        if !t.playable {
            continue;
        }
        if let Some(c) = &col {
            if !t.collection.to_lowercase().contains(c) {
                continue;
            }
        }
        let tl = t.title.to_lowercase();
        let al = t.artist.to_lowercase();
        let bl = t.album.to_lowercase();
        let score = if tl.contains(&needle) {
            3
        } else if al.contains(&needle) {
            2
        } else if bl.contains(&needle) {
            1
        } else {
            continue;
        };
        scored.push((score, track_view(t)));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.title.cmp(&a.1.title)));
    let tracks: Vec<TrackView> = scored.into_iter().take(50).map(|(_, v)| v).collect();
    Json(serde_json::json!({"ok": true, "tracks": tracks})).into_response()
}

// ---------------------------------------------------------------------------
// streaming (in-browser)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PlayBody {
    id: u64,
    #[serde(default)]
    offset_ms: u64,
}

/// Issue a stream token for a library track (device "web" — the per-device
/// player state of the Echo is untouched). The URL is the public
/// /stream/{token} endpoint (token = capability, 48 hex, 12 h TTL).
async fn handle_play(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PlayBody>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let (src, title, artist) = {
        let lib = state.library.read().unwrap();
        let Some(track) = lib.get(body.id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"ok": false, "error": "track not found"})),
            )
                .into_response();
        };
        if !track.playable {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("format {} is not streamable (MP3/AAC only)", track.format_str())
                })),
            )
                .into_response();
        }
        (
            StreamSource::File {
                track_id: track.id,
                path: track.path.clone(),
                format: track.format,
                offset_ms: body.offset_ms,
                avg_bitrate_kbps: track.avg_bitrate_kbps,
                title: track.title.clone(),
                artist: track.artist.clone(),
            },
            track.title.clone(),
            track.artist.clone(),
        )
    };
    let token = state.registry.issue("web", src);
    let url = format!("{}/stream/{}", state.cfg.base_url(), token);
    let art = state.art_url(body.id);
    // The user-scope recent list (per the caller's verified realm claim).
    if let Some(u) = crate::web::authorized(&state.cfg, &headers) {
        state.recent.record(
            &u.user_key(),
            crate::web::RecentItem {
                title: title.clone(),
                artist: artist.clone(),
                kind: "play".into(),
                track_id: Some(body.id),
                path: None,
                at: crate::web::fmt_time(std::time::SystemTime::now()),
            },
        );
    }
    Json(serde_json::json!({
        "ok": true,
        "url": url,
        "title": title,
        "artist": artist,
        "art": art,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// playlists
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct PlItem {
    /// "track" (a library id) or "youtube" (a video URL).
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artist: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub album: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    /// The personal-scope owner (the user realm key). Pre-multi-user
    /// playlists default to "admin" (the sole user at the time).
    #[serde(default = "default_owner")]
    pub owner: String,
    /// Shared with the group (tenant) — an ACTIVE action of the owner:
    /// group members can see + play + download it, only the owner (or
    /// admin) can edit it. Private by default.
    #[serde(default)]
    pub shared: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub items: Vec<PlItem>,
    pub created_at: String,
    pub updated_at: String,
}

fn default_owner() -> String {
    "admin".to_string()
}

impl Default for Playlist {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            owner: default_owner(),
            shared: false,
            items: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}

pub struct PlaylistStore {
    list: Mutex<Vec<Playlist>>,
    file: std::path::PathBuf,
}

impl PlaylistStore {
    pub fn load(cfg: &crate::config::Config) -> Arc<Self> {
        let file = cfg.data_dir().join("playlists.json");
        let list = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<Playlist>>(&raw).ok())
            .unwrap_or_default();
        Arc::new(Self {
            list: Mutex::new(list),
            file,
        })
    }

    fn save(&self, list: &[Playlist]) {
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(raw) = serde_json::to_string_pretty(list) {
            let _ = std::fs::write(&self.file, raw);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Playlist>> {
        self.list.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Drop items referencing the given track ids (every user's playlists)
    /// — after a dedupe removed the underlying files.
    pub fn forget_track_ids(&self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        let mut list = self.lock();
        let mut changed = false;
        for p in list.iter_mut() {
            let before = p.items.len();
            p.items
                .retain(|it| it.track_id.map(|id| !ids.contains(&id)).unwrap_or(true));
            if p.items.len() != before {
                changed = true;
                p.updated_at = now_iso();
            }
        }
        if changed {
            self.save(&list);
        }
    }

    /// The playlists the caller sees: their own + the group's shared ones
    /// (admin sees all — the owner field travels).
    pub fn summary(&self, user_key: &str, is_admin: bool) -> Vec<Value> {
        let list = self.lock();
        list.iter()
            .filter(|p| is_admin || p.owner == user_key || p.shared)
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "owner": p.owner,
                    "shared": p.shared,
                    "is_mine": p.owner == user_key,
                    "description": p.description,
                    "count": p.items.len(),
                    "updated_at": p.updated_at,
                })
            })
            .collect()
    }

    /// The caller may SEE it: owner, group-shared, or admin.
    fn find_visible(&self, id: &str, user_key: &str, is_admin: bool) -> bool {
        self.lock()
            .iter()
            .any(|p| p.id == id && (is_admin || p.owner == user_key || p.shared))
    }

    /// The caller may EDIT it: owner or admin (group members can play a
    /// shared playlist but never change it).
    fn find_editable(&self, id: &str, user_key: &str, is_admin: bool) -> bool {
        self.lock()
            .iter()
            .any(|p| p.id == id && (is_admin || p.owner == user_key))
    }
}

fn new_id() -> String {
    format!(
        "{}{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id()
    )
}

fn playlist_json(p: &Playlist, state: &AppState, user_key: &str) -> Value {
    // Resolve library items against the CURRENT library (a file moved or
    // deleted since the item was added still shows its saved metadata).
    let lib = state.library.read().unwrap();
    let items: Vec<Value> = p
        .items
        .iter()
        .enumerate()
        .map(|(pos, it)| {
            let mut v = serde_json::json!({
                "pos": pos,
                "kind": it.kind,
                "track_id": it.track_id,
                "url": it.url,
                "title": it.title,
                "artist": it.artist,
                "album": it.album,
                "duration_ms": it.duration_ms,
            });
            if let Some(tid) = it.track_id {
                match lib.get(tid) {
                    Some(t) => {
                        v["playable"] = Value::Bool(t.playable);
                        v["duration_ms"] = Value::from(t.duration_ms);
                    }
                    None => {
                        v["missing"] = Value::Bool(true);
                    }
                }
            }
            v
        })
        .collect();
    drop(lib);
    serde_json::json!({
        "id": p.id,
        "name": p.name,
        "owner": p.owner,
        "shared": p.shared,
        "is_mine": p.owner == user_key,
        "description": p.description,
        "items": items,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

fn store(state: &AppState) -> &PlaylistStore {
    &state.playlists
}

async fn handle_playlists_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    let uk = u.user_key();
    Json(
        serde_json::json!({"ok": true, "playlists": store(&state).summary(&uk, u.role == "admin")}),
    )
    .into_response()
}

#[derive(Deserialize)]
struct PlaylistCreateBody {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

async fn handle_playlists_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PlaylistCreateBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "name required"})),
        )
            .into_response();
    }
    let now = now_iso();
    let p = Playlist {
        id: new_id(),
        name: name.clone(),
        description: body.description.unwrap_or_default(),
        owner: u.user_key(),
        shared: false,
        items: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
    };
    let id = p.id.clone();
    {
        let mut list = store(&state).lock();
        list.push(p);
        store(&state).save(&list);
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"ok": true, "id": id})),
    )
        .into_response()
}

async fn handle_playlist_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if !store(&state).find_visible(&id, &u.user_key(), u.role == "admin") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    let list = store(&state).lock();
    match list.iter().find(|p| p.id == id) {
        Some(p) => {
            let uk = u.user_key();
            Json(serde_json::json!({"ok": true, "playlist": playlist_json(p, &state, &uk)}))
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct PlaylistUpdateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

async fn handle_playlist_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<PlaylistUpdateBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if !store(&state).find_editable(&id, &u.user_key(), u.role == "admin") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    let mut list = store(&state).lock();
    let Some(p) = list.iter_mut().find(|p| p.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    };
    if let Some(n) = body.name.filter(|n| !n.trim().is_empty()) {
        p.name = n.trim().to_string();
    }
    if let Some(d) = body.description {
        p.description = d;
    }
    p.updated_at = now_iso();
    store(&state).save(&list);
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn handle_playlist_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if !store(&state).find_editable(&id, &u.user_key(), u.role == "admin") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    let mut list = store(&state).lock();
    let before = list.len();
    list.retain(|p| p.id != id);
    if list.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    store(&state).save(&list);
    Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct ItemsAddBody {
    items: Vec<ItemAdd>,
    /// Insert at this position (default: append).
    #[serde(default)]
    position: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ItemAdd {
    /// Library track (kind "track" is implied).
    track_id: Option<u64>,
    /// YouTube video (kind "youtube" is implied) — URL or bare id.
    url: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    duration_ms: Option<u64>,
}

/// Build + validate new playlist items (shared by add and replace).
fn build_items(state: &AppState, items: &[ItemAdd]) -> (Vec<PlItem>, Vec<String>) {
    let lib = state.library.read().unwrap();
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, it) in items.iter().enumerate() {
        match (it.track_id, it.url.as_deref()) {
            (Some(tid), _) => {
                let Some(t) = lib.get(tid) else {
                    errors.push(format!("item {}: unknown track id {tid}", i + 1));
                    continue;
                };
                out.push(PlItem {
                    kind: "track".into(),
                    track_id: Some(tid),
                    url: None,
                    title: if it.title.is_empty() {
                        t.title.clone()
                    } else {
                        it.title.clone()
                    },
                    artist: if it.artist.is_empty() {
                        t.artist.clone()
                    } else {
                        it.artist.clone()
                    },
                    album: if it.album.is_empty() {
                        t.album.clone()
                    } else {
                        it.album.clone()
                    },
                    duration_ms: Some(t.duration_ms),
                });
            }
            (None, Some(u)) => {
                let url = if u.starts_with("http") {
                    u.to_string()
                } else if u.chars().count() <= 11 && u.chars().all(|c| c.is_ascii_alphanumeric()) {
                    format!("https://www.youtube.com/watch?v={u}")
                } else {
                    errors.push(format!("item {}: no usable url or track_id", i + 1));
                    continue;
                };
                out.push(PlItem {
                    kind: "youtube".into(),
                    track_id: None,
                    url: Some(url),
                    title: it.title.clone(),
                    artist: it.artist.clone(),
                    album: it.album.clone(),
                    duration_ms: it.duration_ms,
                });
            }
            (None, None) => {
                errors.push(format!("item {}: needs track_id or url", i + 1));
            }
        }
    }
    (out, errors)
}

async fn handle_playlist_items_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ItemsAddBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if !store(&state).find_editable(&id, &u.user_key(), u.role == "admin") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    let (new_items, errors) = build_items(&state, &body.items);
    if new_items.is_empty() && !errors.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"ok": false, "errors": errors})),
        )
            .into_response();
    }
    let mut list = store(&state).lock();
    let Some(p) = list.iter_mut().find(|p| p.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    };
    let at = body.position.unwrap_or(p.items.len()).min(p.items.len());
    for (offset, item) in new_items.into_iter().enumerate() {
        p.items.insert(at + offset, item);
    }
    p.updated_at = now_iso();
    store(&state).save(&list);
    Json(serde_json::json!({"ok": true, "at": at, "errors": errors})).into_response()
}

/// Replace the whole item list (the client sends the full order it rendered
/// — the simplest correct reorder semantics).
async fn handle_playlist_items_replace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ItemsAddBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if !store(&state).find_editable(&id, &u.user_key(), u.role == "admin") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    let (new_items, errors) = build_items(&state, &body.items);
    let mut list = store(&state).lock();
    let Some(p) = list.iter_mut().find(|p| p.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    };
    p.items = new_items;
    p.updated_at = now_iso();
    store(&state).save(&list);
    Json(serde_json::json!({"ok": true, "errors": errors})).into_response()
}

#[derive(Deserialize)]
struct ItemPosQuery {
    pos: usize,
}

async fn handle_playlist_item_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<ItemPosQuery>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if !store(&state).find_editable(&id, &u.user_key(), u.role == "admin") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    let mut list = store(&state).lock();
    let Some(p) = list.iter_mut().find(|p| p.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    };
    if q.pos >= p.items.len() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "position out of range"})),
        )
            .into_response();
    }
    p.items.remove(q.pos);
    p.updated_at = now_iso();
    store(&state).save(&list);
    Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct ShareBody {
    shared: bool,
}

/// Toggle group-sharing — an ACTIVE owner action (or admin). Group members
/// may then see + play + download the playlist, but only the owner/admin
/// may edit it (see `find_editable`).
async fn handle_playlist_share(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ShareBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if !store(&state).find_editable(&id, &u.user_key(), u.role == "admin") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    }
    let mut list = store(&state).lock();
    let Some(p) = list.iter_mut().find(|p| p.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "playlist not found"})),
        )
            .into_response();
    };
    p.shared = body.shared;
    p.updated_at = now_iso();
    let shared = p.shared;
    store(&state).save(&list);
    Json(serde_json::json!({"ok": true, "shared": shared})).into_response()
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/dl/library", get(handle_library))
        .route("/dl/library/search", get(handle_library_search))
        .route("/dl/play", post(handle_play))
        .route(
            "/dl/playlists",
            get(handle_playlists_get).post(handle_playlists_create),
        )
        .route(
            "/dl/playlists/{id}",
            get(handle_playlist_get)
                .put(handle_playlist_update)
                .delete(handle_playlist_delete),
        )
        .route(
            "/dl/playlists/{id}/items",
            post(handle_playlist_items_add)
                .put(handle_playlist_items_replace)
                .delete(handle_playlist_item_remove),
        )
        .route("/dl/playlists/{id}/share", post(handle_playlist_share))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_in(dir: &std::path::Path) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.server.data_dir = Some(dir.to_path_buf());
        c
    }

    fn pl(id: &str, owner: &str, shared: bool) -> Playlist {
        Playlist {
            id: id.into(),
            name: id.into(),
            owner: owner.into(),
            shared,
            description: String::new(),
            items: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn sharing_semantics() {
        let dir = std::env::temp_dir().join(format!("aria-pl-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join("playlists.json"));
        let store = PlaylistStore::load(&cfg_in(&dir));
        {
            let mut list = store.lock();
            list.push(pl("mine", "admin", false));
            list.push(pl("shared-by-anna", "anna", true));
            list.push(pl("private-by-anna", "anna", false));
            store.save(&list);
        }
        // admin (owner of "mine", and admin) sees all three.
        let all = store.summary("admin", true);
        assert_eq!(all.len(), 3);
        // anna (not admin) sees her own (both) + nothing of admin's private.
        let anna = store.summary("anna", false);
        let anna_ids: Vec<&str> = anna.iter().map(|v| v["id"].as_str().unwrap()).collect();
        assert_eq!(anna_ids, vec!["shared-by-anna", "private-by-anna"]);
        // A different group member (carl) sees ONLY the shared one.
        let carl = store.summary("carl", false);
        assert_eq!(carl.len(), 1);
        assert_eq!(carl[0]["id"], "shared-by-anna");
        assert_eq!(carl[0]["is_mine"], false);
        assert_eq!(carl[0]["shared"], true);
        // Visibility: group member may SEE the shared playlist…
        assert!(store.find_visible("shared-by-anna", "carl", false));
        // …but may not EDIT it; only the owner (anna) or an admin can.
        assert!(!store.find_editable("shared-by-anna", "carl", false));
        assert!(store.find_editable("shared-by-anna", "anna", false));
        assert!(store.find_editable("shared-by-anna", "carl", true));
        // A private playlist is invisible + uneditable to group members.
        assert!(!store.find_visible("private-by-anna", "carl", false));
        assert!(!store.find_editable("private-by-anna", "carl", false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
