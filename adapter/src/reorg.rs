//! The reorg feature: bring an old, messy collection into a clean layout —
//! real file/folder moves, duplicate removal, and LLM-assisted catalog
//! fixes — while keeping every track's id (→ record slug → brain
//! embedding) intact (the fingerprint ID registry + mosaic ADR 0161).
//!
//!   * plan:    which files move where, why (per layout style)
//!   * apply:   the moves (fs rename), then the standard rescan+reindex
//!   * dedupe:  content-fingerprint groups, keep one, remove the twins
//!   * suggest: the ambiguous tracks → the brain's `reorg-suggest`
//!     workflow (OpenRouter) → normalized artist/album/title
//!   * tags:    apply the accepted suggestions as real ID3/lofty tags
//!
//! Scope rules: LAYOUT MOVES apply to music collections only (sermon /
//! audiobook structure is meaningful and kept); dedupe + LLM catalog fixes
//! apply to all kinds. Destructive actions (apply, dedupe, tags) are
//! admin-only — the shared tenant library is not a user's to restructure.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use aria_audio::CollectionKind;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// styles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `Artist/Album/NN - title.ext` (missing artist/album levels dropped).
    ArtistAlbum,
    /// `Artist/NN - title.ext` (no album grouping).
    ArtistFlat,
    /// Everything flat in the collection root: `NN - title.ext`.
    Flat,
    /// No moves (report only).
    Keep,
}

impl Style {
    pub fn as_str(self) -> &'static str {
        match self {
            Style::ArtistAlbum => "artist-album",
            Style::ArtistFlat => "artist-flat",
            Style::Flat => "flat",
            Style::Keep => "keep",
        }
    }
}

pub fn parse_style(s: &str) -> Option<Style> {
    match s.trim().to_ascii_lowercase().as_str() {
        "artist-album" | "artist_album" | "standard" => Some(Style::ArtistAlbum),
        "artist-flat" | "artist_flat" => Some(Style::ArtistFlat),
        "flat" => Some(Style::Flat),
        "keep" | "none" => Some(Style::Keep),
        _ => None,
    }
}

/// Folder-name safety: strip the Windows/POSIX offenders, collapse spaces.
fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            _ => c,
        })
        .collect();
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "01 - Bohemian Rhapsody" → "Bohemian Rhapsody" (file name only — the
/// display title keeps the tag value).
fn title_clean(t: &str) -> &str {
    if let Some(pos) = t.find(['-', '.']) {
        let prefix = &t[..pos];
        if !prefix.trim().is_empty() && prefix.trim().chars().all(|c| c.is_ascii_digit()) {
            return t[pos + 1..].trim_start();
        }
    }
    t
}

/// Date prefix ("2019-05-12 - ") off sermon file names.
fn title_nodate(t: &str) -> &str {
    let mut rest = t;
    for _ in 0..2 {
        if let Some(pos) = rest.find(['-', ' ']) {
            let prefix = rest[..pos].trim();
            if prefix.len() >= 8 && prefix.chars().filter(|c| c.is_ascii_digit()).count() >= 8 {
                rest = rest[pos + 1..].trim_start();
                continue;
            }
        }
        break;
    }
    rest
}

/// The target (folder segments relative to the collection root + file
/// name) for a track under a style. Empty name = Keep (no move).
/// The track number: the tag when present, else the filename's leading
/// number ("01 - ", "12." — the classic ripped-CD naming), else 00.
fn nn_of(t: &aria_audio::Track) -> String {
    if let Some(n) = t.track_no {
        return format!("{n:02}");
    }
    let stem = Path::new(&t.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if let Some(pos) = stem.find(['-', '.']) {
        let prefix = stem[..pos].trim();
        if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = prefix.parse::<u32>() {
                return format!("{n:02}");
            }
        }
    }
    "00".to_string()
}

fn target_of(style: Style, t: &aria_audio::Track, ext: &str) -> (Vec<String>, String) {
    let nn = nn_of(t);
    let base = match t.kind {
        CollectionKind::Music => title_clean(&t.title),
        CollectionKind::Sermon => title_nodate(&t.title),
        CollectionKind::Audiobook => t.title.as_str(),
    };
    let name = format!("{nn} - {base}.{ext}");
    let artist = sanitize(&t.artist);
    let album = sanitize(&t.album);
    match style {
        Style::ArtistAlbum => {
            let mut segs = Vec::new();
            if !artist.is_empty() {
                segs.push(artist);
            }
            if !album.is_empty() {
                segs.push(album);
            }
            (segs, name)
        }
        Style::ArtistFlat => {
            let mut segs = Vec::new();
            if !artist.is_empty() {
                segs.push(artist);
            }
            (segs, name)
        }
        Style::Flat => (Vec::new(), name),
        Style::Keep => (Vec::new(), String::new()),
    }
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
pub struct Move {
    pub track_id: u64,
    pub title: String,
    pub artist: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct DupTrack {
    pub track_id: u64,
    pub title: String,
    pub artist: String,
    pub path: String,
    pub duration_ms: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct DupGroup {
    pub fingerprint: u64,
    pub tracks: Vec<DupTrack>,
}

#[derive(Serialize, Debug)]
pub struct ReorgPlan {
    pub style: String,
    pub collection: String,
    pub kind: String,
    pub moves: Vec<Move>,
    pub duplicates: Vec<DupGroup>,
    /// Music tracks with missing/messy metadata — the LLM-suggest
    /// candidates (the `index` field is the array position).
    pub ambiguous: Vec<Value>,
    pub move_bytes: u64,
}

fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Build the plan for one collection. `root` = the collection directory.
pub fn plan(lib: &aria_audio::Library, root: &Path, collection: &str, style: Style) -> ReorgPlan {
    let tracks: Vec<&aria_audio::Track> =
        lib.iter().filter(|t| t.collection == collection).collect();
    let kind = match tracks.first().map(|t| t.kind).unwrap_or_default() {
        CollectionKind::Music => "music",
        CollectionKind::Audiobook => "audiobook",
        CollectionKind::Sermon => "sermon",
    };
    // Layout moves are music-only (sermon/audiobook structure is kept);
    // dedupe + LLM apply to everything.
    let move_style = if kind == "music" { style } else { Style::Keep };

    // Current relative paths (collision detection): path → track id.
    let mut taken: BTreeMap<String, u64> = tracks
        .iter()
        .filter_map(|t| {
            Path::new(&t.path)
                .strip_prefix(root)
                .ok()
                .map(|p| (p.to_string_lossy().to_string(), t.id))
        })
        .collect();

    let mut moves = Vec::new();
    let mut move_bytes = 0u64;
    let mut ambiguous = Vec::new();

    for t in &tracks {
        let Ok(rel) = Path::new(&t.path).strip_prefix(root) else {
            continue;
        };
        let rel_s = rel.to_string_lossy().to_string();
        let ext = ext_of(&t.path);
        let (segs, name) = target_of(move_style, t, &ext);
        let needs_catalog_fix =
            kind == "music" && (needs_work(&t.artist) || needs_work(&t.album) || needs_work(&t.title));
        if name.is_empty() {
            if needs_catalog_fix {
                push_ambiguous(&mut ambiguous, t);
            }
            continue;
        }
        let mut target_s = {
            let mut v = segs.clone();
            v.push(name.clone());
            v.join("/")
        };
        if target_s != rel_s {
            // Collision with a file that is NOT this track → deterministic
            // " (2)", " (3)" … suffix on the file name.
            let ext_s = format!(".{ext}");
            if taken.get(&target_s).is_some_and(|&id| id != t.id) {
                let cur = target_s
                    .rsplit('/')
                    .next()
                    .map(str::to_string)
                    .unwrap_or_else(|| name.clone())
                    .strip_suffix(&ext_s)
                    .map(str::to_string)
                    .unwrap_or_else(|| name.clone());
                let mut i = 2u32;
                loop {
                    let cand = format!("{cur} ({i}){ext_s}");
                    let mut v = segs.clone();
                    v.push(cand);
                    target_s = v.join("/");
                    if !taken.get(&target_s).is_some_and(|&id| id != t.id) {
                        break;
                    }
                    i += 1;
                }
            }
        }
        if target_s != rel_s {
            let size = std::fs::metadata(&t.path).map(|m| m.len()).unwrap_or(0);
            move_bytes += size;
            taken.insert(target_s.clone(), t.id);
            taken.remove(&rel_s);
            let reason = if segs.is_empty() {
                "flat layout"
            } else if segs.len() == 1 {
                "artist grouping"
            } else {
                "artist/album grouping"
            };
            moves.push(Move {
                track_id: t.id,
                title: t.title.clone(),
                artist: t.artist.clone(),
                from: rel_s,
                to: target_s,
                reason: reason.to_string(),
            });
        }
        // Independent of the move: a messy CATALOG needs LLM work even when
        // the file is already where the layout wants it (and vice versa).
        if needs_catalog_fix {
            push_ambiguous(&mut ambiguous, t);
        }
    }
    moves.sort_by(|a, b| a.from.cmp(&b.from));

    // Duplicates: identical content (the fingerprint), 2+ members.
    let mut by_fp: BTreeMap<u64, Vec<&aria_audio::Track>> = BTreeMap::new();
    for t in &tracks {
        if t.fingerprint != 0 {
            by_fp.entry(t.fingerprint).or_default().push(t);
        }
    }
    let duplicates: Vec<DupGroup> = by_fp
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(fp, v)| {
            let mut v = v;
            v.sort_by(|a, b| a.path.cmp(&b.path));
            DupGroup {
                fingerprint: fp,
                tracks: v
                    .into_iter()
                    .map(|t| DupTrack {
                        track_id: t.id,
                        title: t.title.clone(),
                        artist: t.artist.clone(),
                        path: t.path.clone(),
                        duration_ms: t.duration_ms,
                    })
                    .collect(),
            }
        })
        .collect();

    ReorgPlan {
        style: move_style.as_str().to_string(),
        collection: collection.to_string(),
        kind: kind.to_string(),
        moves,
        duplicates,
        ambiguous,
        move_bytes,
    }
}

/// Empty OR placeholder-junk ("~1", "??", "Kein Künstler" — no real letters
/// / a known placeholder) = the field needs catalog work.
fn needs_work(s: &str) -> bool {
    let n = s.trim();
    if n.is_empty() {
        return true;
    }
    let has_letter = n
        .chars()
        .any(|c| c.is_ascii_alphabetic() || ('À'..='ß').contains(&c) || ('à'..='ÿ').contains(&c));
    if !has_letter {
        return true;
    }
    let low = n.to_lowercase();
    matches!(
        low.as_str(),
        "kein kuenstler"
            | "kein künstler"
            | "keine kuensterin"
            | "keine künstlerin"
            | "unbekannt"
            | "unbekannter kuenstler"
            | "unbekannter künstler"
            | "unknown"
            | "unknown artist"
            | "n/a"
            | "none"
            | "various artists"
            | "diverse"
            | "artist"
            | "album"
    )
}

fn push_ambiguous(v: &mut Vec<Value>, t: &aria_audio::Track) {
    v.push(serde_json::json!({
        "index": v.len(),
        "track_id": t.id,
        "title": t.title,
        "artist": t.artist,
        "album": t.album,
        "collection": t.collection,
        "kind": match t.kind {
            CollectionKind::Music => "music",
            CollectionKind::Audiobook => "audiobook",
            CollectionKind::Sermon => "sermon",
        },
        "file": t.path.rsplit('/').next().unwrap_or(""),
        "path": t.path,
        "duration_ms": t.duration_ms,
    }));
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct ApplyReport {
    pub moved: usize,
    pub failed: Vec<Value>,
    pub bytes: u64,
}

/// Execute the moves (directories created as needed; rename with a
/// copy+delete fallback). The ids do NOT change — the caller rescans
/// afterwards (rescan_cycle); the registry keeps every id stable, so the
/// brain only updates the path metadata (ADR 0161 keeps the embeddings).
pub fn apply_moves(root: &Path, moves: &[Move]) -> ApplyReport {
    let mut moved = 0usize;
    let mut bytes = 0u64;
    let mut failed = Vec::new();
    for m in moves {
        let from = root.join(&m.from);
        let to = root.join(&m.to);
        if !from.exists() {
            failed.push(serde_json::json!({"title": m.title, "error": "source missing"}));
            continue;
        }
        if let Some(parent) = to.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                failed.push(serde_json::json!({"title": m.title, "error": format!("mkdir: {e}")}));
                continue;
            }
        }
        let res = std::fs::rename(&from, &to).or_else(|_| {
            // Cross-device fallback (should not happen on one volume).
            std::fs::copy(&from, &to)
                .and_then(|_| std::fs::remove_file(&from))
                .map(drop)
        });
        match res {
            Ok(()) => {
                moved += 1;
                bytes += std::fs::metadata(&to).map(|m| m.len()).unwrap_or(0);
            }
            Err(e) => failed.push(serde_json::json!({"title": m.title, "error": e.to_string()})),
        }
    }
    ApplyReport {
        moved,
        failed,
        bytes,
    }
}

/// Remove duplicate files (safety: every removed file must share the
/// keeper's fingerprint). Returns the removed paths.
pub fn remove_duplicates(
    lib: &aria_audio::Library,
    keep_id: u64,
    remove_ids: &[u64],
) -> Result<Vec<String>, String> {
    let Some(keep) = lib.get(keep_id) else {
        return Err("keeper track not found".into());
    };
    let mut removed = Vec::new();
    for &id in remove_ids {
        let Some(t) = lib.get(id) else {
            continue;
        };
        if t.fingerprint == 0 || t.fingerprint != keep.fingerprint {
            return Err(format!(
                "refusing to remove `{}` — its content does not match the keeper",
                t.title
            ));
        }
        std::fs::remove_file(&t.path).map_err(|e| format!("remove {}: {e}", t.path))?;
        removed.push(t.path.clone());
    }
    Ok(removed)
}

/// Write the accepted catalog suggestions as real tags (lofty). Only the
/// provided (non-empty) fields are touched. Returns the tagged count.
/// One accepted catalog suggestion: `track_id` + the optional new
/// `title` / `artist` / `album` (empty = leave that field as is).
pub type TagEdit = (u64, Option<String>, Option<String>, Option<String>);

pub fn apply_tags(lib: &aria_audio::Library, items: &[TagEdit]) -> Result<usize, String> {
    let mut n = 0;
    for (id, title, artist, album) in items {
        let Some(t) = lib.get(*id) else {
            continue;
        };
        aria_audio::set_tags(
            Path::new(&t.path),
            title.as_deref().filter(|s| !s.trim().is_empty()),
            artist.as_deref().filter(|s| !s.trim().is_empty()),
            album.as_deref().filter(|s| !s.trim().is_empty()),
        )
        .map_err(|e| format!("{}: {e}", t.title))?;
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

fn admin_only(u: &crate::web::WebUser) -> Option<Response> {
    if u.role == "admin" {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "ok": false,
                    "error": "reorg changes touch the shared library — admin only"
                })),
            )
                .into_response(),
        )
    }
}

#[derive(Deserialize)]
struct PlanQuery {
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    collection: Option<String>,
}

/// GET /dl/reorg/plan?style=artist-album&collection=musik — any app user
/// (read-only: it plans, it does not touch the files).
async fn handle_plan(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<PlanQuery>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    let style = q
        .style
        .as_deref()
        .and_then(parse_style)
        .unwrap_or(Style::ArtistAlbum);
    let items = match state.cfg.collection_items() {
        Ok(i) => i,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": e})),
            )
                .into_response();
        }
    };
    let lib = state.library.read().unwrap();
    let wanted = q.collection.as_deref();
    let plans: Vec<Value> = items
        .iter()
        .filter(|i| wanted.map(|w| i.name == w).unwrap_or(true))
        .map(|i| plan(&lib, &i.path, &i.name, style))
        .map(|p| serde_json::to_value(&p).unwrap_or_default())
        .collect();
    drop(lib);
    Json(serde_json::json!({"ok": true, "plans": plans})).into_response()
}

#[derive(Deserialize)]
struct ApplyBody {
    style: String,
    collection: String,
}

/// POST /dl/reorg/apply {style, collection} — admin only.
async fn handle_apply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ApplyBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if let Some(r) = admin_only(&u) {
        return r;
    }
    let Some(style) = parse_style(&body.style) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "unknown style"})),
        )
            .into_response();
    };
    let items = match state.cfg.collection_items() {
        Ok(i) => i,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": e})),
            )
                .into_response();
        }
    };
    let Some(item) = items.into_iter().find(|i| i.name == body.collection) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "unknown collection"})),
        )
            .into_response();
    };
    let p = {
        let lib = state.library.read().unwrap();
        plan(&lib, &item.path, &item.name, style)
    };
    if p.moves.is_empty() {
        return Json(serde_json::json!({"ok": true, "moved": 0, "note": "nothing to move"}))
            .into_response();
    }
    // The moves are fs renames (fast); run them off the thread pool so a
    // huge collection never blocks a request.
    let moves = p.moves.clone();
    let root = item.path.clone();
    let report = tokio::task::spawn_blocking(move || apply_moves(&root, &moves))
        .await
        .unwrap_or_else(|e| ApplyReport {
            moved: 0,
            failed: vec![serde_json::json!({"error": format!("task: {e}")})],
            bytes: 0,
        });
    let reindex = crate::web::rescan_cycle(&state).await.is_some();
    Json(serde_json::json!({
        "ok": true,
        "moved": report.moved,
        "failed": report.failed,
        "bytes": report.bytes,
        "reindex": reindex,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DedupeBody {
    keep_id: u64,
    remove_ids: Vec<u64>,
}

/// POST /dl/reorg/dedupe {keep_id, remove_ids} — admin only.
async fn handle_dedupe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DedupeBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if let Some(r) = admin_only(&u) {
        return r;
    }
    let removed = {
        let lib = state.library.read().unwrap();
        remove_duplicates(&lib, body.keep_id, &body.remove_ids)
    };
    let Ok(removed) = removed else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"ok": false, "error": "safety check failed"})),
        )
            .into_response();
    };
    // Drop the removed tracks from every user's playlists.
    state.playlists.forget_track_ids(&body.remove_ids);
    let reindex = crate::web::rescan_cycle(&state).await.is_some();
    Json(serde_json::json!({
        "ok": true,
        "removed": removed.len(),
        "reindex": reindex,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SuggestBody {
    /// The ambiguous entries straight from the plan (or a subset of them).
    tracks: Vec<Value>,
}

/// POST /dl/reorg/suggest {tracks} → the brain's reorg-suggest workflow
/// (OpenRouter). Returns {suggestions: [...]} — parsed from whatever shape
/// the workflow output takes (array, or a JSON string of one).
async fn handle_suggest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<SuggestBody>,
) -> Response {
    if crate::web::authorized(&state.cfg, &headers).is_none() {
        return crate::web::denied();
    }
    if body.tracks.is_empty() {
        return Json(serde_json::json!({"ok": true, "suggestions": []})).into_response();
    }
    let tracks_json = serde_json::to_string(&body.tracks).unwrap_or_else(|_| "[]".to_string());
    match state
        .agent
        .reorg_suggest(&tracks_json, std::time::Duration::from_secs(60))
        .await
    {
        Ok(raw) => {
            let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Array(vec![]));
            let arr = v.get("suggestions").cloned().unwrap_or(v);
            let arr = arr.as_array().cloned().unwrap_or_default();
            Json(serde_json::json!({"ok": true, "suggestions": arr})).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "error": format!("suggest: {e}")})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct TagsBody {
    /// Rows: [track_id, title?, artist?, album?]
    items: Vec<Value>,
}

/// POST /dl/reorg/apply-tags {items} — admin only: write the accepted
/// suggestions as real tags, then rescan+reindex.
async fn handle_apply_tags(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<TagsBody>,
) -> Response {
    let Some(u) = crate::web::authorized(&state.cfg, &headers) else {
        return crate::web::denied();
    };
    if let Some(r) = admin_only(&u) {
        return r;
    }
    let parsed: Vec<TagEdit> = body
        .items
        .iter()
        .filter_map(|v| {
            let id = v.get(0)?.as_u64()?;
            let s = |i: usize| v.get(i).and_then(|x| x.as_str()).map(str::to_string);
            Some((id, s(1), s(2), s(3)))
        })
        .collect();
    let n = {
        let lib = state.library.read().unwrap();
        apply_tags(&lib, &parsed)
    };
    match n {
        Ok(n) => {
            let reindex = crate::web::rescan_cycle(&state).await.is_some();
            Json(serde_json::json!({"ok": true, "tagged": n, "reindex": reindex})).into_response()
        }
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------


pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/dl/reorg/plan", get(handle_plan))
        .route("/dl/reorg/apply", post(handle_apply))
        .route("/dl/reorg/dedupe", post(handle_dedupe))
        .route("/dl/reorg/suggest", post(handle_suggest))
        .route("/dl/reorg/apply-tags", post(handle_apply_tags))
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reorg dance on a messy on-disk collection: the plan finds
    /// the moves, apply_moves executes them, and the fingerprint registry
    /// keeps the ids stable across the move (the embedding-safe property).
    #[test]
    fn plan_and_apply_move_files_and_keep_ids() {
        let dir = std::env::temp_dir().join(format!(
            "aria-reorg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("musik");
        std::fs::create_dir_all(&root).unwrap();
        // Messy: files flat in the root with numberless names, no tags
        // (artist/album come from the FILE NAME conventions only when the
        // tags are absent — here they are empty, so the plan must still
        // produce a clean flat layout at minimum).
        std::fs::write(root.join("01 - Bohemian.mp3"), b"fake-aaa-1111").unwrap();
        std::fs::write(root.join("02 - Radio.mp3"), b"fake-bbb-2222").unwrap();

        let items = [aria_audio::CollectionItem {
            name: "musik".into(),
            kind: CollectionKind::Music,
            path: root.clone(),
        }];
        // One registry across ALL scans (the deployment's mode) — this is
        // what makes the ids (and thus the record slugs / embeddings)
        // stable across the move.
        let mut reg = aria_audio::IdRegistry::default();
        let lib = aria_audio::Library::scan_collections_with(&items, &mut reg).unwrap();
        assert_eq!(lib.len(), 2);
        let id_of = |lib: &aria_audio::Library, suffix: &str| {
            lib.iter()
                .find(|t| t.path.ends_with(suffix))
                .map(|t| t.id)
                .expect("track present")
        };
        let bohem_before = id_of(&lib, "Bohemian.mp3");
        let radio_before = id_of(&lib, "Radio.mp3");
        // Track the bohemian file across its rename by CONTENT (that's the
        // registry's key) — its filename changes twice in this test.
        let bohem_fp = lib
            .iter()
            .find(|t| t.id == bohem_before)
            .unwrap()
            .fingerprint;

        // flat style: the titles already are "NN - x" → no moves needed.
        let plan_flat = plan(&lib, &root, "musik", Style::Flat);
        assert!(
            plan_flat.moves.is_empty(),
            "flat layout is already met: {:?}",
            plan_flat.moves
        );

        // artist-album with empty tags: artist/album are empty → the
        // target is flat anyway → still no moves (nothing to group by).
        let plan_aa = plan(&lib, &root, "musik", Style::ArtistAlbum);
        assert!(plan_aa.moves.is_empty());

        // Now give the tracks tags (simulating the LLM apply-tags step) via
        // a second scan of a tagged tree — instead, verify the MOVE path
        // directly: hand the plan a target layout the files do NOT meet by
        // using artist-flat with a fake artist assigned through the
        // library. The simplest real move: rename a file to something the
        // layout does not match.
        std::fs::rename(root.join("01 - Bohemian.mp3"), root.join("random_name.mp3")).unwrap();
        let lib2 = aria_audio::Library::scan_collections_with(&items, &mut reg).unwrap();
        // The PLAN must now want to move random_name.mp3 back to the flat
        // layout.
        let plan2 = plan(&lib2, &root, "musik", Style::Flat);
        assert!(
            plan2
                .moves
                .iter()
                .any(|m| m.from == "random_name.mp3" && m.to.starts_with("0")),
            "the unformatted file is planned for a move: {:?}",
            plan2.moves
        );
        let report = apply_moves(&root, &plan2.moves);
        assert_eq!(report.moved, plan2.moves.len(), "{:?}", report.failed);
        assert!(!root.join("random_name.mp3").exists());

        // The whole point: after the move, a rescan with the SAME registry
        // hands both files the ids they had before — the record slugs (and
        // the brain's embeddings) are untouched. The bohemian file is found
        // by its content fingerprint (its name changed twice).
        let lib3 = aria_audio::Library::scan_collections_with(&items, &mut reg).unwrap();
        let bohem_after = lib3
            .iter()
            .find(|t| t.fingerprint == bohem_fp)
            .map(|t| t.id)
            .expect("moved file still present");
        assert_eq!(bohem_after, bohem_before, "the moved file kept its id");
        assert_eq!(
            id_of(&lib3, "Radio.mp3"),
            radio_before,
            "untouched file kept its id"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Duplicate detection: two files with identical content share the
    /// fingerprint and form one group.
    #[test]
    fn plan_groups_duplicates_by_fingerprint() {
        let dir = std::env::temp_dir().join(format!(
            "aria-reorg-dupe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("musik");
        std::fs::create_dir_all(&root).unwrap();
        let content = b"identical-content-0000000000000000";
        std::fs::write(root.join("a.mp3"), content).unwrap();
        std::fs::write(root.join("b.mp3"), content).unwrap();
        let items = [aria_audio::CollectionItem {
            name: "musik".into(),
            kind: CollectionKind::Music,
            path: root.clone(),
        }];
        let lib = aria_audio::Library::scan_collections(&items).unwrap();
        let plan = plan(&lib, &root, "musik", Style::Keep);
        assert_eq!(plan.duplicates.len(), 1, "one duplicate group");
        assert_eq!(plan.duplicates[0].tracks.len(), 2);

        // remove_duplicates keeps the keeper, removes the twin.
        let keeper = plan.duplicates[0].tracks[0].track_id;
        let twin = plan.duplicates[0].tracks[1].track_id;
        let removed = remove_duplicates(&lib, keeper, &[twin]).unwrap();
        assert_eq!(removed.len(), 1);
        // Safety: a NON-matching file is refused.
        let other = lib.iter().find(|t| t.id != keeper && t.id != twin);
        if let Some(o) = other {
            let err = remove_duplicates(&lib, keeper, &[o.id]).unwrap_err();
            assert!(err.contains("does not match"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}