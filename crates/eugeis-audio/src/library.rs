//! Music library scanning and metadata (via `lofty`, pure Rust).

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    Mp3,
    Aac,
    Flac,
}

impl Format {
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Mp3 => "audio/mpeg",
            Format::Aac => "audio/aac",
            Format::Flac => "audio/flac",
        }
    }

    /// Echo AudioPlayer streams MP3 and AAC natively.
    pub fn playable(self) -> bool {
        matches!(self, Format::Mp3 | Format::Aac)
    }
}

fn detect_format(path: &Path) -> Option<Format> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "mp3" => Some(Format::Mp3),
        "m4a" | "aac" | "mp4" | "m4b" | "m4p" => Some(Format::Aac),
        "flac" => Some(Format::Flac),
        _ => None,
    }
}

/// One audio file in the library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: u64,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub track_no: Option<u32>,
    pub duration_ms: u64,
    /// Average bitrate in kbps; used to translate a millisecond offset into a
    /// byte offset for MP3 stream resume.
    pub avg_bitrate_kbps: u32,
    pub format: Format,
    pub playable: bool,
}

impl Track {
    pub fn title_artist(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} by {}", self.title, self.artist)
        }
    }
}

/// Embedded cover art.
pub struct Cover {
    pub data: Vec<u8>,
    pub mime: String,
}

/// Indexed set of tracks.
#[derive(Debug, Default)]
pub struct Library {
    tracks: Vec<Track>,
}

impl Library {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter()
    }

    pub fn playable_len(&self) -> usize {
        self.tracks.iter().filter(|t| t.playable).count()
    }

    pub fn get(&self, id: u64) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// Scan `roots` recursively for audio files and index their tags.
    pub fn scan(roots: &[PathBuf]) -> Result<Self, ScanError> {
        let mut tracks: Vec<Track> = Vec::new();
        let mut next_id = 1u64;
        for root in roots {
            if !root.is_dir() {
                tracing::warn!(path = %root.display(), "library path is not a directory; skipping");
                continue;
            }
            for path in walk(root) {
                let Some(format) = detect_format(&path) else {
                    continue;
                };
                let meta = read_meta(&path);
                let mut title = meta.title;
                if title.is_empty() {
                    title = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().replace('_', " "))
                        .unwrap_or_default();
                }
                let artist = if meta.artist.is_empty() {
                    root_name(root, &path)
                } else {
                    meta.artist
                };
                tracks.push(Track {
                    id: next_id,
                    path: path.to_string_lossy().into_owned(),
                    title,
                    artist,
                    album: meta.album,
                    genre: meta.genre,
                    track_no: meta.track_no,
                    duration_ms: meta.duration_ms,
                    avg_bitrate_kbps: meta.bitrate_kbps,
                    format,
                    playable: format.playable(),
                });
                next_id += 1;
            }
        }
        tracks.sort_by(|a, b| {
            (
                &a.artist,
                &a.album,
                a.track_no.unwrap_or(u32::MAX),
                &a.title,
            )
                .cmp(&(
                    &b.artist,
                    &b.album,
                    b.track_no.unwrap_or(u32::MAX),
                    &b.title,
                ))
        });
        // Stable, compact ids after sort.
        for (i, t) in tracks.iter_mut().enumerate() {
            t.id = i as u64 + 1;
        }
        let playable = tracks.iter().filter(|t| t.playable).count();
        tracing::info!(total = tracks.len(), playable, "music library scanned");
        Ok(Self { tracks })
    }

    /// Cover art for a track, if embedded.
    pub fn cover(&self, id: u64) -> Option<Cover> {
        let path = self.get(id)?.path.clone();
        read_cover(&PathBuf::from(path))
    }

    pub fn tracks_in_album(&self, album: &str) -> Vec<u64> {
        let needle = norm(album);
        self.tracks
            .iter()
            .filter(|t| t.playable && !t.album.is_empty() && norm(&t.album) == needle)
            .map(|t| t.id)
            .collect()
    }

    pub fn tracks_by_artist(&self, artist: &str) -> Vec<u64> {
        let needle = norm(artist);
        self.tracks
            .iter()
            .filter(|t| t.playable && norm(&t.artist) == needle)
            .map(|t| t.id)
            .collect()
    }

    pub fn tracks_by_genre(&self, genre: &str) -> Vec<u64> {
        let needle = norm(genre);
        self.tracks
            .iter()
            .filter(|t| t.playable && !t.genre.is_empty() && norm(&t.genre) == needle)
            .map(|t| t.id)
            .collect()
    }

    /// All playable track ids, optionally shuffled.
    pub fn all_ids(&self, shuffled: bool) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .tracks
            .iter()
            .filter(|t| t.playable)
            .map(|t| t.id)
            .collect();
        if shuffled && ids.len() > 1 {
            let mut rng = rand::rng();
            shuffle_in_place(&mut ids, &mut rng);
        }
        ids
    }
}

pub fn shuffle_in_place<T>(v: &mut [T], rng: &mut impl rand::Rng) {
    for i in (1..v.len()).rev() {
        let j = rng.random_range(0..=i);
        v.swap(i, j);
    }
}

struct Meta {
    title: String,
    artist: String,
    album: String,
    genre: String,
    track_no: Option<u32>,
    duration_ms: u64,
    bitrate_kbps: u32,
}

fn read_meta(path: &Path) -> Meta {
    let mut meta = Meta {
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        genre: String::new(),
        track_no: None,
        duration_ms: 0,
        bitrate_kbps: 0,
    };
    let Ok(tagged) = lofty::read_from_path(path) else {
        return meta;
    };
    let props = tagged.properties();
    let duration: Duration = props.duration();
    meta.duration_ms = duration.as_millis().min(u64::MAX as u128) as u64;

    let file_len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    meta.bitrate_kbps = props
        .overall_bitrate()
        .unwrap_or(0)
        .max((file_len.saturating_mul(8) / meta.duration_ms.max(1) / 1000) as u32);

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        meta.title = tag.title().unwrap_or_default().to_string();
        meta.artist = tag.artist().unwrap_or_default().to_string();
        meta.album = tag.album().unwrap_or_default().to_string();
        meta.genre = tag.genre().unwrap_or_default().to_string();
        meta.track_no = tag
            .get_string(&lofty::tag::ItemKey::TrackNumber)
            .and_then(|v| v.parse().ok());
    }
    meta
}

fn read_cover(path: &Path) -> Option<Cover> {
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;
    let mime = pic.mime_type().map(|m| m.as_str()).unwrap_or("image/jpeg");
    Some(Cover {
        data: pic.data().to_vec(),
        mime: mime.to_string(),
    })
}

/// Parent directory name (artist folder) when the file is nested under roots.
fn root_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().replace('_', " "))
        .unwrap_or_default()
}

/// Recursive directory walk; skips hidden entries.
///
/// Collects eagerly: library scans run once at startup and on rescan, and a
/// few hundred thousand path entries is cheap.
pub fn walk(dir: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    fn inner(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if p.is_dir() {
                inner(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut collected = Vec::new();
    inner(dir, &mut collected);
    collected.into_iter()
}

fn norm(s: &str) -> String {
    s.to_ascii_lowercase().replace(&['-', '_'][..], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats() {
        assert_eq!(detect_format(Path::new("a/b.mp3")), Some(Format::Mp3));
        assert_eq!(detect_format(Path::new("a/b.M4A")), Some(Format::Aac));
        assert_eq!(detect_format(Path::new("a/b.flac")), Some(Format::Flac));
        assert_eq!(detect_format(Path::new("a/b.txt")), None);
    }

    #[test]
    fn flac_indexed_but_not_playable() {
        assert!(!Format::Flac.playable());
        assert!(Format::Mp3.playable());
        assert!(Format::Aac.playable());
    }

    #[test]
    fn scan_missing_root_is_empty() {
        let lib = Library::scan(&[PathBuf::from("/nonexistent-eugeis-test")]).unwrap();
        assert!(lib.is_empty());
    }
}
