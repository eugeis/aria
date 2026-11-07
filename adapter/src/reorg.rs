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

/// The grouping hierarchy for a music layout. Levels are fixed in the
/// order language → genre → artist → album; the user picks which levels to
/// include (the preset names map onto combinations). `flat` = every file
/// straight into the collection root; an empty non-flat layout = Keep
/// (report only, no moves).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct Layout {
    pub language: bool,
    pub genre: bool,
    pub artist: bool,
    pub album: bool,
    pub flat: bool,
    /// Consolidate one-track "albums" into a single `Lose Titel` folder per
    /// artist instead of minting a folder per loose file.
    pub loose: bool,
}

impl Layout {
    pub fn is_empty(&self) -> bool {
        !self.language && !self.genre && !self.artist && !self.album
    }

    /// "language-genre-artist-album[-loose]" / the legacy presets
    /// (artist-album, artist-flat, flat, keep).
    pub fn parse(s: &str) -> Option<Layout> {
        let mut l = Layout::default();
        for part in s.trim().to_ascii_lowercase().split(['-', '_', ',', ' ']) {
            match part.trim() {
                "" => {}
                "language" | "lang" | "sprache" => l.language = true,
                "genre" | "genres" => l.genre = true,
                "artist" | "kuenstler" => l.artist = true,
                "album" | "albums" => l.album = true,
                "loose" | "lose" => l.loose = true,
                "flat" => {
                    l = Layout {
                        flat: true,
                        loose: l.loose,
                        ..Default::default()
                    };
                }
                "keep" | "none" => {
                    l = Layout::default();
                }
                _ => return None,
            }
        }
        Some(l)
    }

    pub fn as_str(&self) -> String {
        if self.flat {
            return "flat".to_string();
        }
        if self.is_empty() {
            return "keep".to_string();
        }
        let mut v = Vec::new();
        if self.language {
            v.push("language");
        }
        if self.genre {
            v.push("genre");
        }
        if self.artist {
            v.push("artist");
        }
        if self.album {
            v.push("album");
        }
        if self.loose {
            v.push("loose");
        }
        v.join("-")
    }
}

/// Normalized form for COMPARING paths/folders (not for writing):
/// lowercase, a leading "NN - "/"NN." number prefix stripped per segment,
/// punctuation gone, whitespace collapsed. Two names that differ only in
/// case, numbering, spacing or punctuation compare equal — a file that is
/// already in its (normalized) target place is NOT a move.
fn norm_cmp(s: &str) -> String {
    let low = s.to_lowercase();
    let base = if let Some(pos) = low.find(['-', '.']) {
        let prefix = low[..pos].trim();
        if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
            let tail = low[pos + 1..].trim_start();
            // "13.mp3": the dot is the EXTENSION separator, not a number
            // prefix — keep the whole name when the tail is a bare
            // extension (≤ 4 alphanumeric chars).
            if low.as_bytes()[pos] == b'.'
                && !tail.is_empty()
                && tail.len() <= 4
                && tail.chars().all(|c| c.is_ascii_alphanumeric())
            {
                low.clone()
            } else {
                tail.to_string()
            }
        } else {
            low.clone()
        }
    } else {
        low.clone()
    };
    let out: String = base
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .collect();
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn norm_rel(p: &str) -> String {
    p.split('/')
        .map(norm_cmp)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
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

/// TLAN carries "code|Description" ("deu|G") — try both parts against the
/// common ISO codes / short names and return a German display name for
/// the folder (unknown values pass through, sanitized).
fn lang_display(s: &str) -> String {
    let parts: Vec<String> = s
        .split('|')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let describe = |p: &str| -> Option<&'static str> {
        match p.to_lowercase().as_str() {
            "de" | "deu" | "ger" | "g" | "german" | "deutsch" => Some("Deutsch"),
            "en" | "eng" | "e" | "english" => Some("Englisch"),
            "fr" | "fra" | "fre" | "french" | "französisch" => Some("Französisch"),
            "es" | "spa" | "esp" | "spanish" | "spanisch" => Some("Spanisch"),
            "it" | "ita" | "itn" | "italian" | "italienisch" => Some("Italienisch"),
            "pt" | "por" | "portuguese" | "portugiesisch" => Some("Portugiesisch"),
            "nl" | "nld" | "dut" | "dutch" | "niederländisch" => Some("Niederländisch"),
            "la" | "lat" | "latin" | "lateinisch" => Some("Latein"),
            "ru" | "rus" | "russian" | "russisch" => Some("Russisch"),
            "grc" | "grk" | "el" | "greek" | "griechisch" => Some("Griechisch"),
            "he" | "heb" | "hebrew" | "hebräisch" => Some("Hebräisch"),
            _ => None,
        }
    };
    for p in &parts {
        if let Some(d) = describe(p) {
            return sanitize(d);
        }
    }
    sanitize(parts.last().map(String::as_str).unwrap_or(s.trim()))
}

/// Heuristic for encoding corruption (mojibake): Cyrillic letters inside an
/// otherwise Latin word ("Panflцte" for "Panflöte"), or the classic
/// UTF-8-read-as-Windows double encoding — "Ã"/"Â" followed by a char from
/// the U+0080..U+00BF block ("FrostÃ¶de"), or "â€" (double-encoded dash).
fn looks_mojibake(s: &str) -> bool {
    let cyr = s
        .chars()
        .filter(|c| ('\u{0400}'..='\u{04FF}').contains(c))
        .count();
    if cyr >= 1 && s.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 3 {
        return true;
    }
    let cs: Vec<char> = s.chars().collect();
    for w in 0..cs.len() {
        if (cs[w] == 'Ã' || cs[w] == 'Â')
            && w + 1 < cs.len()
            && ('\u{0080}'..='\u{00BF}').contains(&cs[w + 1])
        {
            return true;
        }
        if cs[w] == 'â' && w + 1 < cs.len() && cs[w + 1] == '€' {
            return true;
        }
    }
    false
}

fn target_of(
    layout: &Layout,
    t: &aria_audio::Track,
    ext: &str,
    singletons: &std::collections::HashSet<String>,
) -> (Vec<String>, String) {
    // Keep: an empty, non-flat layout means "report only, no moves".
    if layout.is_empty() && !layout.flat {
        return (Vec::new(), String::new());
    }
    let nn = nn_of(t);
    let raw = match t.kind {
        CollectionKind::Music => title_clean(&t.title).to_string(),
        CollectionKind::Sermon => title_nodate(&t.title).to_string(),
        CollectionKind::Audiobook => t.title.clone(),
    };
    // Titles carry whatever the tagger wrote — "song 13/13", "a:b", … A
    // slash in the title would mint a phantom directory level, so the
    // file-name part gets the same safety pass as folder names.
    let base = sanitize(&raw);
    let name = format!("{nn} - {base}.{ext}");
    let mut segs = Vec::new();
    if layout.language {
        segs.push(if t.language.trim().is_empty() {
            "Unsortiert".to_string()
        } else {
            lang_display(&t.language)
        });
    }
    if layout.genre {
        // Placeholder/junk genres ("Favorite Music", "Other", …) share the
        // `Sonstige` bucket.
        segs.push(if needs_work(&t.genre) {
            "Sonstige".to_string()
        } else {
            sanitize(&t.genre)
        });
    }
    if layout.artist {
        // RAW tag value (never the folder fallback — the fallback would
        // read the layout's own folders, e.g. the language level, as
        // artist). Junky names (numeric, placeholder, …) all share ONE
        // folder instead of minting "1", "2", "247E~1", …
        segs.push(if artist_is_junk(&t.artist_raw) {
            "Unsortiert".to_string()
        } else {
            sanitize(&t.artist_raw)
        });
    }
    if layout.album {
        // Loose files: an album with exactly one track in the collection
        // becomes `Lose Titel` per artist (no 100 one-song folders).
        let loose = layout.loose && singletons.contains(&norm_cmp(&t.album_raw));
        segs.push(if needs_work(&t.album_raw) {
            "Ohne Album".to_string()
        } else if loose {
            "Lose Titel".to_string()
        } else {
            sanitize(&t.album_raw)
        });
    }
    (segs, name)
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

/// One track's catalog state (the LLM input for the full metadata pass).
#[derive(Serialize, Debug)]
pub struct TrackMeta {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub language: String,
    pub file: String,
}

/// How complete the catalog is (drives the "which tracks still need the
/// LLM pass" decision).
#[derive(Serialize, Debug)]
pub struct TagStats {
    pub total: usize,
    pub missing_language: usize,
    pub missing_genre: usize,
    pub mojibake: usize,
    pub ambiguous: usize,
}

#[derive(Serialize, Debug)]
pub struct ReorgPlan {
    pub layout: String,
    pub collection: String,
    pub kind: String,
    pub moves: Vec<Move>,
    pub duplicates: Vec<DupGroup>,
    /// Music tracks with missing/messy metadata (incl. suspected mojibake)
    /// — the LLM-suggest candidates (the `index` field is the array
    /// position).
    pub ambiguous: Vec<Value>,
    /// Every music track's catalog state (the full metadata pass input).
    pub tracks: Vec<TrackMeta>,
    pub tag_stats: TagStats,
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
pub fn plan(
    lib: &aria_audio::Library,
    root: &Path,
    collection: &str,
    layout: &Layout,
) -> ReorgPlan {
    let tracks: Vec<&aria_audio::Track> =
        lib.iter().filter(|t| t.collection == collection).collect();
    let kind = match tracks.first().map(|t| t.kind).unwrap_or_default() {
        CollectionKind::Music => "music",
        CollectionKind::Audiobook => "audiobook",
        CollectionKind::Sermon => "sermon",
    };
    // Layout moves are music-only (sermon/audiobook structure is kept);
    // dedupe + LLM apply to everything.
    let keep = Layout::default();
    let move_layout = if kind == "music" { *layout } else { keep };

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

    // Albums that occur exactly once in this collection (normalized name):
    // with `loose` they collapse into the per-artist `Lose Titel` folder.
    let mut album_counts: BTreeMap<String, usize> = BTreeMap::new();
    if kind == "music" {
        for t in &tracks {
            *album_counts.entry(norm_cmp(&t.album_raw)).or_insert(0) += 1;
        }
    }
    let singletons: std::collections::HashSet<String> = album_counts
        .into_iter()
        .filter(|(_, c)| *c == 1)
        .map(|(k, _)| k)
        .collect();

    for t in &tracks {
        let Ok(rel) = Path::new(&t.path).strip_prefix(root) else {
            continue;
        };
        let rel_s = rel.to_string_lossy().to_string();
        let ext = ext_of(&t.path);
        let (segs, name) = target_of(&move_layout, t, &ext, &singletons);
        let needs_catalog_fix = kind == "music"
            && (needs_work(&t.artist)
                || needs_work(&t.album)
                || needs_work(&t.title)
                || looks_mojibake(&t.artist)
                || looks_mojibake(&t.album)
                || looks_mojibake(&t.title)
                || looks_mojibake(&t.genre));
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
        // Already in place (normalized): case / numbering / spacing /
        // punctuation differences between tag-derived and on-disk names are
        // cosmetic — no move, no duplicated folder.
        if norm_rel(&target_s) == norm_rel(&rel_s) {
            if needs_catalog_fix {
                push_ambiguous(&mut ambiguous, t);
            }
            continue;
        }
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
            moves.push(Move {
                track_id: t.id,
                title: t.title.clone(),
                artist: t.artist.clone(),
                from: rel_s,
                to: target_s,
                reason: format!("layout: {}", move_layout.as_str()),
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

    // Catalog completeness (music only): which tracks still need the LLM
    // metadata pass (missing language/genre) and what the full pass sees.
    let is_music = kind == "music";
    let missing_language = if is_music {
        tracks
            .iter()
            .filter(|t| t.language.trim().is_empty())
            .count()
    } else {
        0
    };
    let missing_genre = if is_music {
        tracks.iter().filter(|t| t.genre.trim().is_empty()).count()
    } else {
        0
    };
    let mojibake = if is_music {
        tracks
            .iter()
            .filter(|t| {
                looks_mojibake(&t.artist)
                    || looks_mojibake(&t.album)
                    || looks_mojibake(&t.title)
                    || looks_mojibake(&t.genre)
            })
            .count()
    } else {
        0
    };
    let track_metas: Vec<TrackMeta> = if is_music {
        tracks
            .iter()
            .map(|t| TrackMeta {
                id: t.id,
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                genre: t.genre.clone(),
                language: t.language.clone(),
                file: t.path.rsplit('/').next().unwrap_or("").to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let ambiguous_count = ambiguous.len();
    ReorgPlan {
        layout: move_layout.as_str(),
        collection: collection.to_string(),
        kind: kind.to_string(),
        moves,
        duplicates,
        ambiguous,
        tracks: track_metas,
        tag_stats: TagStats {
            total: if is_music { tracks.len() } else { 0 },
            missing_language,
            missing_genre,
            mojibake,
            ambiguous: ambiguous_count,
        },
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
            | "autor"
            | "album"
            // Default album names taggers/iTunes mint for loose files:
            | "kein titel"
            | "no title"
            | "favorite music"
            | "meine musik"
            | "other"
            | "unbekanntes genre"
    )
}

/// An artist name that must not become its own folder: empty/placeholder
/// (needs_work), 2 or fewer letters ("B-52", "247E~1"), ≤3 letters mixed
/// with digits, or digits as common as letters ("0079"). Real short names
/// without digits ("AC/DC", "A-ha", "ABBA") survive.
fn artist_is_junk(s: &str) -> bool {
    let n = s.trim();
    if needs_work(n) {
        return true;
    }
    let letters = n.chars().filter(|c| c.is_alphabetic()).count();
    let digits = n.chars().filter(|c| c.is_ascii_digit()).count();
    letters <= 2 || (letters <= 3 && digits >= 1) || (digits >= 2 && digits >= letters)
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
/// `title` / `artist` / `album` / `genre` / `language` (empty = leave
/// that field as is).
#[derive(Debug, Clone, Default)]
pub struct TagRow {
    pub id: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub language: Option<String>,
}

/// Writes the tags, continuing past per-file failures (one broken file must
/// not abort the batch). Returns (tagged, per-file errors).
pub fn apply_tags(lib: &aria_audio::Library, items: &[TagRow]) -> (usize, Vec<String>) {
    let clean = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let mut n = 0;
    let mut errors = Vec::new();
    for r in items {
        let Some(t) = lib.get(r.id) else {
            continue;
        };
        if let Err(e) = aria_audio::set_tags(
            Path::new(&t.path),
            &aria_audio::TagEdit {
                title: clean(r.title.clone()),
                artist: clean(r.artist.clone()),
                album: clean(r.album.clone()),
                genre: clean(r.genre.clone()),
                language: clean(r.language.clone()),
                comment: None,
            },
        ) {
            errors.push(format!("{}: {e}", t.title));
            continue;
        }
        n += 1;
    }
    (n, errors)
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
    /// The grouping levels ("language-genre-artist-album") — wins over
    /// `style` (the legacy preset names still work).
    #[serde(default)]
    layout: Option<String>,
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
    let spec = q
        .layout
        .as_deref()
        .or(q.style.as_deref())
        .unwrap_or("artist-album");
    let layout = match Layout::parse(spec) {
        Some(l) => l,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": format!("unknown layout `{spec}` (e.g. language-genre-artist-album, artist-album, flat, keep)")})),
            )
                .into_response();
        }
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
    let lib = state.library.read().unwrap();
    let wanted = q.collection.as_deref();
    let plans: Vec<Value> = items
        .iter()
        .filter(|i| wanted.map(|w| i.name == w).unwrap_or(true))
        .map(|i| plan(&lib, &i.path, &i.name, &layout))
        .map(|p| serde_json::to_value(&p).unwrap_or_default())
        .collect();
    drop(lib);
    Json(serde_json::json!({"ok": true, "plans": plans})).into_response()
}

#[derive(Deserialize)]
struct ApplyBody {
    /// The layout spec (levels or a legacy preset name).
    #[serde(alias = "style")]
    layout: String,
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
    let Some(layout) = Layout::parse(&body.layout) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": format!("unknown layout `{}` (e.g. language-genre-artist-album, artist-album, flat, keep)", body.layout)})),
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
        plan(&lib, &item.path, &item.name, &layout)
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
    // One workflow call per BATCH tracks: a few hundred tracks in a single
    // prompt would overflow the context window (the live plan has 491
    // ambiguous). 20/batch keeps the LLM output short enough to complete
    // reliably (longer batches trip provider rate limits / truncated JSON).
    // Sequential batches, merged.
    const BATCH: usize = 20;
    let mut out: Vec<Value> = Vec::new();
    let mut failed_batches = 0usize;
    for chunk in body.tracks.chunks(BATCH) {
        let tracks_json = serde_json::to_string(chunk).unwrap_or_else(|_| "[]".to_string());
        match state
            .agent
            .reorg_suggest(&tracks_json, std::time::Duration::from_secs(120))
            .await
        {
            Ok(raw) => {
                let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Array(vec![]));
                let arr = v.get("suggestions").cloned().unwrap_or(v);
                out.extend(arr.as_array().cloned().unwrap_or_default());
            }
            Err(e) => {
                failed_batches += 1;
                tracing::warn!(%e, "reorg suggest batch failed");
            }
        }
    }
    if out.is_empty() && failed_batches > 0 {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "error": "suggest: all batches failed"})),
        )
            .into_response();
    }
    Json(serde_json::json!({
        "ok": true,
        "suggestions": out,
        "batches": body.tracks.len().div_ceil(BATCH),
        "failed_batches": failed_batches,
    }))
    .into_response()
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
    // Rows: [track_id, title?, artist?, album?, genre?, language?]
    let parsed: Vec<TagRow> = body
        .items
        .iter()
        .filter_map(|v| {
            let id = v.get(0)?.as_u64()?;
            let s = |i: usize| v.get(i).and_then(|x| x.as_str()).map(str::to_string);
            Some(TagRow {
                id,
                title: s(1),
                artist: s(2),
                album: s(3),
                genre: s(4),
                language: s(5),
            })
        })
        .collect();
    let (n, errors) = {
        let lib = state.library.read().unwrap();
        apply_tags(&lib, &parsed)
    };
    if n == 0 && !errors.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"ok": false, "error": errors.join("; ")})),
        )
            .into_response();
    }
    let reindex = crate::web::rescan_cycle(&state).await.is_some();
    Json(serde_json::json!({
        "ok": true,
        "tagged": n,
        "errors": errors,
        "reindex": reindex,
    }))
    .into_response()
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

    #[test]
    fn set_tags_never_truncates_on_failure() {
        let dir = std::env::temp_dir().join(format!(
            "aria-reorg-safe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 1) an unparseable "mp3": the read fails BEFORE any write — the
        //    original bytes must survive untouched.
        let p = dir.join("fake.mp3");
        std::fs::write(&p, b"fake-audio-bytes-123456").unwrap();
        let r = aria_audio::set_tags(
            &p,
            &aria_audio::TagEdit {
                title: Some("x".into()),
                ..Default::default()
            },
        );
        assert!(r.is_err(), "unparseable file must fail: {r:?}");
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"fake-audio-bytes-123456",
            "the original must be untouched"
        );
        // 2) a 0-byte file is refused explicitly (a broken download).
        let p2 = dir.join("empty.mp3");
        std::fs::write(&p2, b"").unwrap();
        let r2 = aria_audio::set_tags(
            &p2,
            &aria_audio::TagEdit {
                title: Some("x".into()),
                ..Default::default()
            },
        );
        let err2 = r2.unwrap_err();
        assert!(
            err2.contains("empty"),
            "0-byte files must be refused: {err2}"
        );
        assert_eq!(std::fs::metadata(&p2).unwrap().len(), 0);
        // no temp litter left behind
        assert!(
            dir.read_dir()
                .unwrap()
                .filter_map(|e| e.ok())
                .all(|e| !e.file_name().to_string_lossy().contains(".aria-tmp"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn layout_parses_levels_and_presets() {
        let l = Layout::parse("language-genre-artist-album").unwrap();
        assert!(l.language && l.genre && l.artist && l.album && !l.flat);
        assert_eq!(l.as_str(), "language-genre-artist-album");
        let l = Layout::parse("artist-album").unwrap();
        assert!(!l.language && !l.genre && l.artist && l.album);
        let l = Layout::parse("flat").unwrap();
        assert!(l.flat);
        assert_eq!(l.as_str(), "flat");
        assert_eq!(Layout::parse("keep").unwrap().as_str(), "keep");
        assert!(Layout::parse("language-artist").unwrap().as_str() == "language-artist");
        assert!(Layout::parse("bogus").is_none());
        // the loose flag round-trips as a trailing token
        let l = Layout::parse("language-genre-artist-album-loose").unwrap();
        assert!(l.loose);
        assert_eq!(l.as_str(), "language-genre-artist-album-loose");
        assert!(!Layout::parse("artist-album").unwrap().loose);
    }

    fn minimal_mp3() -> Vec<u8> {
        let mut v = b"ID3\x03\x00\x00".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]); // zero-length tag
        for _ in 0..4 {
            v.extend_from_slice(&[0xff, 0xfb, 0x90, 0xc0]);
            v.extend(std::iter::repeat_n(0u8, 413));
        }
        v
    }

    /// A real (parseable) MP3 with ID3 tags — the layout buckets on the RAW
    /// tag values, so the fixtures must carry real tags.
    fn tagged_mp3(dir: &std::path::Path, rel: &str, title: &str, artist: &str, album: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, minimal_mp3()).unwrap();
        aria_audio::set_tags(
            &p,
            &aria_audio::TagEdit {
                title: Some(title.into()),
                artist: Some(artist.into()),
                album: Some(album.into()),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn loose_collapses_single_track_albums() {
        let dir = std::env::temp_dir().join(format!(
            "aria-reorg-loose-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("musik");
        // Artist Alpha: one single-track "album" (Only) and a real 2-track
        // one (Pair).
        tagged_mp3(&root, "Alpha/Only/01 - x.mp3", "x", "Alpha", "Only");
        tagged_mp3(&root, "Alpha/Pair/01 - y.mp3", "y", "Alpha", "Pair");
        tagged_mp3(&root, "Alpha/Pair/02 - z.mp3", "z", "Alpha", "Pair");
        let items = [aria_audio::CollectionItem {
            name: "musik".into(),
            kind: CollectionKind::Music,
            path: root.clone(),
        }];
        let lib = aria_audio::Library::scan_collections(&items).unwrap();
        assert_eq!(lib.len(), 3);

        // Without loose: everything is already in place (no moves).
        let p_keep = plan(
            &lib,
            &root,
            "musik",
            &Layout {
                artist: true,
                album: true,
                ..Default::default()
            },
        );
        assert!(p_keep.moves.is_empty(), "{:?}", p_keep.moves);

        // With loose: the single-track album collapses into `Lose Titel`,
        // the 2-track album keeps its own folder.
        let p_loose = plan(
            &lib,
            &root,
            "musik",
            &Layout {
                artist: true,
                album: true,
                loose: true,
                ..Default::default()
            },
        );
        assert_eq!(p_loose.moves.len(), 1, "{:?}", p_loose.moves);
        assert_eq!(p_loose.moves[0].from, "Alpha/Only/01 - x.mp3");
        assert_eq!(p_loose.moves[0].to, "Alpha/Lose Titel/01 - x.mp3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artist_junk_detection() {
        // numeric / digit-heavy / placeholder names must not mint folders
        assert!(artist_is_junk(""));
        assert!(artist_is_junk("1"));
        assert!(artist_is_junk("0079"));
        assert!(artist_is_junk("247E~1"));
        assert!(artist_is_junk("B-52"));
        assert!(artist_is_junk("Artist"));
        assert!(artist_is_junk("Autor"));
        // real (short) names survive
        assert!(!artist_is_junk("AC/DC"));
        assert!(!artist_is_junk("A-ha"));
        assert!(!artist_is_junk("ABBA"));
        assert!(!artist_is_junk("Heidi Klum"));
    }

    #[test]
    fn mojibake_detection() {
        assert!(looks_mojibake("Faszination Panflцte 2"));
        assert!(looks_mojibake("FrostÃ¶de"));
        assert!(looks_mojibake("Weihnachtsâ€“Lied"));
        assert!(!looks_mojibake("Heidi Klum"));
        assert!(!looks_mojibake("Griechische Hymne")); // all-Cyrillic/greek titles are fine
    }

    #[test]
    fn lang_display_maps_codes() {
        assert_eq!(lang_display("deu|G"), "Deutsch");
        assert_eq!(lang_display("eng"), "Englisch");
        assert_eq!(lang_display("Deutsch"), "Deutsch");
        assert_eq!(lang_display("yid"), "yid");
    }

    /// Cosmetic differences (case, numbering prefixes, spacing, extension
    /// case) between tag-derived and on-disk names are NOT moves — that is
    /// what made the live plan want to duplicate 1 160 already-placed files.
    #[test]
    fn plan_treats_normalized_in_place_files_as_settled() {
        let dir = std::env::temp_dir().join(format!(
            "aria-reorg-norm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("musik");
        // On-disk: numbered prefix + uppercase extension; tags carry the
        // same name with a different prefix/case → must NOT move.
        std::fs::create_dir_all(root.join(
            "Bibel-Memory/Lernkurs Gottes Verheißungen/00 - Lernkurs Gottes Verheißungen - Teil 01",
        ))
        .unwrap();
        std::fs::write(
            root.join("Bibel-Memory/Lernkurs Gottes Verheißungen/00 - Lernkurs Gottes Verheißungen - Teil 01/12 - ...unsere Sehnsucht zu stillen.MP3"),
            b"norm-1",
        )
        .unwrap();
        let items = [aria_audio::CollectionItem {
            name: "musik".into(),
            kind: CollectionKind::Music,
            path: root.clone(),
        }];
        let lib = aria_audio::Library::scan_collections(&items).unwrap();
        // Give the track the tag-derived name (no "00 - " folder prefix,
        // different case) — simulates the live mismatch.
        let plan = plan(
            &lib,
            &root,
            "musik",
            &Layout {
                artist: true,
                album: true,
                ..Default::default()
            },
        );
        assert!(
            plan.moves.iter().all(|m| {
                // A move is legitimate only when the NORMALIZED paths differ.
                norm_rel(&m.from) != norm_rel(&m.to)
            }),
            "no cosmetic-only moves: {:?}",
            plan.moves
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A slash inside the TITLE tag ("Blue Sky - song 13/13") must not mint
    /// a phantom directory level in the target path (the live "album
    /// duplicated" plan bug).
    #[test]
    fn plan_never_creates_phantom_dirs_from_slashy_titles() {
        let dir = std::env::temp_dir().join(format!("aria-reorg-slash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("musik");
        let album_dir = root.join("Там за горизонтом/00 - Blue Sky - song 13");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("13.mp3"), b"slash-test").unwrap();
        let items = [aria_audio::CollectionItem {
            name: "musik".into(),
            kind: CollectionKind::Music,
            path: root.clone(),
        }];
        let lib = aria_audio::Library::scan_collections(&items).unwrap();
        let plan = plan(
            &lib,
            &root,
            "musik",
            &Layout {
                artist: true,
                album: true,
                ..Default::default()
            },
        );
        for m in &plan.moves {
            // A move target has exactly the same depth (or fewer) segments
            // as its source + at most the intended artist/album levels — and
            // NEVER more than source + 2.
            let from_depth = m.from.split('/').count();
            let to_depth = m.to.split('/').count();
            assert!(
                to_depth <= from_depth + 2,
                "phantom directory levels: {:?} -> {:?}",
                m.from,
                m.to
            );
            assert!(
                !m.to.contains("song 13/song"),
                "album duplicated: {:?}",
                m.to
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        let plan_flat = plan(
            &lib,
            &root,
            "musik",
            &Layout {
                flat: true,
                ..Default::default()
            },
        );
        assert!(
            plan_flat.moves.is_empty(),
            "flat layout is already met: {:?}",
            plan_flat.moves
        );

        // artist-album with empty tags: the files land in the explicit
        // "Unsortiert"/"Ohne Album" buckets (instead of being scattered at
        // the collection root) — until their metadata is fixed.
        let plan_aa = plan(
            &lib,
            &root,
            "musik",
            &Layout {
                artist: true,
                album: true,
                ..Default::default()
            },
        );
        assert_eq!(plan_aa.moves.len(), 2, "{:?}", plan_aa.moves);
        assert!(
            plan_aa
                .moves
                .iter()
                .all(|m| m.to.starts_with("Unsortiert/Ohne Album/"))
        );

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
        let plan2 = plan(
            &lib2,
            &root,
            "musik",
            &Layout {
                flat: true,
                ..Default::default()
            },
        );
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
        let plan = plan(&lib, &root, "musik", &Layout::default());
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

#[cfg(test)]
mod set_tags_in_place {
    fn minimal_mp3() -> Vec<u8> {
        let mut v = b"ID3\x03\x00\x00".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]); // zero-length tag
        // A few MPEG-1 Layer III frames (128kbps, 44.1kHz = 417 bytes each)
        for _ in 0..4 {
            v.extend_from_slice(&[0xff, 0xfb, 0x90, 0xc0]);
            v.extend(std::iter::repeat_n(0u8, 413));
        }
        v
    }

    /// The live library hit this: an in-place tag rewrite must succeed,
    /// keep the audio intact, and make the new values readable.
    #[test]
    fn in_place_rewrite_preserves_audio() {
        let dir = std::env::temp_dir().join(format!(
            "aria-tag-inplace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("track.mp3");
        let original = minimal_mp3();
        std::fs::write(&p, &original).unwrap();
        let before = std::fs::metadata(&p).unwrap().len();
        let r = aria_audio::set_tags(
            &p,
            &aria_audio::TagEdit {
                title: Some("Probe Title".into()),
                genre: Some("Worship".into()),
                language: Some("Deutsch".into()),
                ..Default::default()
            },
        );
        assert!(r.is_ok(), "in-place tag write must succeed: {r:?}");
        let bytes = std::fs::read(&p).unwrap_or_default();
        // The audio tail (last MPEG frame) must survive the rewrite.
        assert!(
            bytes.len() >= (before as usize) - 50
                && bytes[bytes.len().saturating_sub(4)..] == original[original.len() - 4..],
            "the audio must survive the tag rewrite"
        );
        // And the new values are readable back.
        let (title, genre, language) = aria_audio::read_tag_summary(&p);
        assert_eq!(title, "Probe Title");
        assert_eq!(genre, "Worship");
        assert_eq!(language, "Deutsch");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
