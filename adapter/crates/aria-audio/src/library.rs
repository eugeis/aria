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

/// The kind of collection a track was scanned from.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionKind {
    #[default]
    /// Regular music: artist/album folders, per-track metadata.
    Music,
    /// Audiobooks: one subfolder per book, its files are the chapters.
    Audiobook,
    /// Sermon / seminar recordings: playable audio of the user's scripts.
    Sermon,
}

/// One collection root folder (the user's "collection with subfolders").
#[derive(Debug, Clone)]
pub struct CollectionItem {
    /// Collection name (becomes `collection` in the track + the scan records).
    pub name: String,
    pub kind: CollectionKind,
    pub path: PathBuf,
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
    /// The collection this file was scanned from (folder name).
    #[serde(default)]
    pub collection: String,
    /// The collection kind (music / audiobook / sermon).
    #[serde(default)]
    pub kind: CollectionKind,
    /// Audiobook: the book (top-level subfolder) this chapter belongs to.
    #[serde(default)]
    pub book: String,
    /// Audiobook: 1-based chapter number within the book.
    #[serde(default)]
    pub chapter_no: Option<u32>,
    /// Release year, when tagged (music).
    #[serde(default)]
    pub year: String,
}

impl Track {
    /// The track's container format as a lowercase string.
    pub fn format_str(&self) -> &'static str {
        match self.format {
            Format::Mp3 => "mp3",
            Format::Aac => "aac",
            Format::Flac => "flac",
        }
    }
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

    /// Scan the collection roots and index their tags.
    ///
    /// - `Music`: recursive; artist falls back to the parent folder (the
    ///   `Artist/Album/nn - title.mp3` layout).
    /// - `Audiobook`: one immediate subfolder per book; the audio files
    ///   directly inside are its chapters (ordered by track number, then
    ///   file name).
    /// - `Sermon`: recursive; any audio file is one recording (the title
    ///   fallback strips a leading `YYYY-MM-DD - ` date).
    pub fn scan_collections(items: &[CollectionItem]) -> Result<Self, ScanError> {
        let mut tracks: Vec<Track> = Vec::new();
        for item in items {
            if !item.path.is_dir() {
                tracing::warn!(
                    collection = %item.name,
                    path = %item.path.display(),
                    "collection path is not a directory; skipping"
                );
                continue;
            }
            match item.kind {
                CollectionKind::Music => {
                    for path in walk(&item.path) {
                        if let Some(t) =
                            build_track(&path, &item.path, item.name.clone(), item.kind)
                        {
                            tracks.push(t);
                        }
                    }
                }
                CollectionKind::Audiobook => {
                    for book in list_books(&item.path) {
                        for path in book
                            .path
                            .read_dir()
                            .map(|rd| {
                                rd.flatten()
                                    .map(|e| e.path())
                                    .filter(|p| p.is_file())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                        {
                            let Some(format) = detect_format(&path) else {
                                continue;
                            };
                            let meta = read_meta(&path);
                            let title = if meta.title.is_empty() {
                                title_from_filename(&path)
                            } else {
                                meta.title
                            };
                            tracks.push(Track {
                                id: 0,
                                path: path.to_string_lossy().into_owned(),
                                title,
                                // The narrator, when tagged, doubles as the
                                // "artist" (spoken by …).
                                artist: meta.artist,
                                album: String::new(),
                                genre: String::new(),
                                track_no: meta.track_no,
                                duration_ms: meta.duration_ms,
                                avg_bitrate_kbps: meta.bitrate_kbps,
                                format,
                                playable: format.playable(),
                                collection: item.name.clone(),
                                kind: item.kind,
                                book: book.name.clone(),
                                chapter_no: None,
                                year: String::new(),
                            });
                        }
                    }
                    // Number the chapters per book (track number, then name).
                    for book in list_books(&item.path) {
                        let ids: Vec<usize> = tracks
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| t.book == book.name)
                            .map(|(i, _)| i)
                            .collect();
                        let mut order: Vec<&usize> = ids.iter().collect();
                        order.sort_by(|a, b| {
                            let (ta, tb) = (&tracks[**a], &tracks[**b]);
                            ta.track_no
                                .unwrap_or(u32::MAX)
                                .cmp(&tb.track_no.unwrap_or(u32::MAX))
                                .then_with(|| ta.path.cmp(&tb.path))
                        });
                        for (n, i) in order.iter().enumerate() {
                            tracks[**i].chapter_no = Some(n as u32 + 1);
                        }
                    }
                }
                CollectionKind::Sermon => {
                    for path in walk(&item.path) {
                        let Some(mut t) =
                            build_track(&path, &item.path, item.name.clone(), item.kind)
                        else {
                            continue;
                        };
                        // Title: the tag wins; else the filename minus a
                        // leading "YYYY-MM-DD - " date.
                        let meta = read_meta(&path);
                        t.title = sermon_title(&path, &meta.title);
                        tracks.push(t);
                    }
                }
            }
        }
        // Deterministic order => stable ids across identical scans.
        tracks.sort_by(|a, b| {
            (
                &a.collection,
                &a.book,
                a.chapter_no.unwrap_or(u32::MAX),
                &a.artist,
                &a.album,
                a.track_no.unwrap_or(u32::MAX),
                &a.title,
            )
                .cmp(&(
                    &b.collection,
                    &b.book,
                    b.chapter_no.unwrap_or(u32::MAX),
                    &b.artist,
                    &b.album,
                    b.track_no.unwrap_or(u32::MAX),
                    &b.title,
                ))
        });
        for (i, t) in tracks.iter_mut().enumerate() {
            t.id = i as u64 + 1;
        }
        let playable = tracks.iter().filter(|t| t.playable).count();
        tracing::info!(total = tracks.len(), playable, "collections scanned");
        Ok(Self { tracks })
    }

    /// Scan flat music roots (the v1 layout: one or more music folders).
    pub fn scan(roots: &[PathBuf]) -> Result<Self, ScanError> {
        let items = roots
            .iter()
            .map(|p| CollectionItem {
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "music".into()),
                kind: CollectionKind::Music,
                path: p.clone(),
            })
            .collect::<Vec<_>>();
        Self::scan_collections(&items)
    }

    /// Cover art for a track, if embedded.
    pub fn cover(&self, id: u64) -> Option<Cover> {
        let path = self.get(id)?.path.clone();
        read_cover(&PathBuf::from(path))
    }

    /// Distinct audiobook book names (the immediate subfolders of the
    /// audiobook collections).
    pub fn books(&self) -> Vec<String> {
        let mut books: Vec<String> = self
            .tracks
            .iter()
            .filter(|t| t.kind == CollectionKind::Audiobook && !t.book.is_empty())
            .map(|t| t.book.clone())
            .collect();
        books.sort();
        books.dedup();
        books
    }

    /// The ordered, playable chapter ids of one audiobook.
    pub fn chapters_of_book(&self, book: &str) -> Vec<u64> {
        let needle = norm(book);
        let mut chs: Vec<&Track> = self
            .tracks
            .iter()
            .filter(|t| {
                t.kind == CollectionKind::Audiobook && t.playable && norm(&t.book) == needle
            })
            .collect();
        chs.sort_by(|a, b| {
            a.chapter_no
                .unwrap_or(u32::MAX)
                .cmp(&b.chapter_no.unwrap_or(u32::MAX))
                .then_with(|| a.path.cmp(&b.path))
        });
        chs.into_iter().map(|t| t.id).collect()
    }

    /// First playable track of a book (the entry point for "play <book>").
    pub fn book_start(&self, book: &str) -> Option<u64> {
        self.chapters_of_book(book).into_iter().next()
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

/// The metadata-record slug for a name (the scan and the router must agree).
pub fn slugify(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    out.trim_matches('-').to_string()
}

struct Meta {
    title: String,
    artist: String,
    album: String,
    genre: String,
    track_no: Option<u32>,
    duration_ms: u64,
    bitrate_kbps: u32,
    year: String,
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
        year: String::new(),
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
        // Year: "Date"/"Year" tag; keep only a 4-digit year when present.
        let date = tag
            .get_string(&lofty::tag::ItemKey::RecordingDate)
            .or_else(|| tag.get_string(&lofty::tag::ItemKey::Year))
            .unwrap_or_default()
            .to_string();
        if let Ok(y) = date.chars().take(4).collect::<String>().parse::<u32>() {
            if (1900..=2100).contains(&y) {
                meta.year = date.chars().take(4).collect();
            }
        }
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

/// Build one music/sermon track from a file path (tags + fallbacks).
fn build_track(
    path: &Path,
    root: &Path,
    collection: String,
    kind: CollectionKind,
) -> Option<Track> {
    let format = detect_format(path)?;
    let meta = read_meta(path);
    let title = if meta.title.is_empty() {
        title_from_filename(path)
    } else {
        meta.title
    };
    let (folder_artist, folder_album) = folder_fallback(root, path);
    let artist = clean_artist(&if meta.artist.is_empty() {
        folder_artist
    } else {
        meta.artist
    });
    let album = if meta.album.is_empty() {
        folder_album
    } else {
        meta.album
    };
    Some(Track {
        id: 0,
        path: path.to_string_lossy().into_owned(),
        title,
        artist,
        album,
        genre: meta.genre,
        track_no: meta.track_no,
        duration_ms: meta.duration_ms,
        avg_bitrate_kbps: meta.bitrate_kbps,
        format,
        playable: format.playable(),
        collection,
        kind,
        book: String::new(),
        chapter_no: None,
        year: meta.year,
    })
}

/// File name -> title: "01 - Bohemian Rhapsody.mp3" => "Bohemian Rhapsody".
fn title_from_filename(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = stem.replace('_', " ");
    // Strip a leading track-number prefix ("01 - ", "12.", "003_").
    if let Some(pos) = stem.find(['-', '.']) {
        let prefix = &stem[..pos];
        if prefix.trim().chars().all(|c| c.is_ascii_digit()) && !prefix.trim().is_empty() {
            let rest = stem[pos + 1..].trim_start();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    stem
}

/// The spoken title of a sermon recording: the tag title when present,
/// otherwise the file stem minus a leading "YYYY-MM-DD - " date prefix.
fn sermon_title(path: &Path, tagged: &str) -> String {
    if !tagged.trim().is_empty() {
        return tagged.trim().to_string();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().replace('_', " "))
        .unwrap_or_default();
    if let Some(pos) = stem.find(" - ") {
        let prefix = &stem[..pos];
        if prefix.len() == 10 && prefix.matches('-').count() == 2 {
            return stem[pos + 3..].trim().to_string();
        }
    }
    stem
}

struct BookDir {
    name: String,
    path: PathBuf,
}

/// Immediate subdirectories of an audiobook collection root, sorted.
fn list_books(root: &Path) -> Vec<BookDir> {
    let Ok(rd) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut books: Vec<BookDir> = rd
        .flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| BookDir {
            name: e.file_name().to_string_lossy().replace('_', " "),
            path: e.path(),
        })
        .collect();
    books.sort_by(|a, b| a.name.cmp(&b.name));
    books
}

/// Folder-hierarchy fallback for missing tags: in the classic
/// `root/Artist/Album/nn - title.mp3` layout the first two segments after
/// Tagged artist placeholders that mean "no artist" — CD rippers and
/// taggers love them (the owner's choral CDs carry "Kein Künstler"). Left
/// as-is they pollute the artist records ("Kein Künstler — Künstler, …")
/// and the spoken playback lines.
fn clean_artist(a: &str) -> String {
    let n = a.trim().to_lowercase();
    if matches!(
        n.as_str(),
        "kein kuenstler" | "kein künstler" | "keine kuensterin" | "keine künstlerin"
            | "unbekannt" | "unbekannter kuenstler" | "unbekannter künstler"
            | "unknown" | "unknown artist" | "n/a" | "none" | "various artists" | "diverse"
    ) {
        String::new()
    } else {
        a.trim().to_string()
    }
}

/// the collection root are (artist, album); a file directly under the root
/// has neither.
fn folder_fallback(root: &Path, path: &Path) -> (String, String) {
    let Ok(rel) = path.strip_prefix(root) else {
        return (String::new(), String::new());
    };
    let Some(parent) = rel.parent() else {
        return (String::new(), String::new());
    };
    let segs: Vec<String> = parent
        .components()
        .map(|c| c.as_os_str().to_string_lossy().replace('_', " "))
        .collect();
    let artist = segs.first().cloned().unwrap_or_default();
    let album = segs.get(1).cloned().unwrap_or_default();
    (artist, album)
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
        let lib = Library::scan(&[PathBuf::from("/nonexistent-aria-test")]).unwrap();
        assert!(lib.is_empty());
    }

    #[test]
    fn title_prefix_stripping() {
        assert_eq!(
            title_from_filename(Path::new("music/01 - Bohemian Rhapsody.mp3")),
            "Bohemian Rhapsody"
        );
        assert_eq!(
            title_from_filename(Path::new("music/12.Heroes.mp3")),
            "Heroes"
        );
        assert_eq!(
            title_from_filename(Path::new("music/track 07.mp3")),
            "track 07"
        );
    }

    #[test]
    fn sermon_title_rules() {
        // No tag: the date prefix is stripped from the file name.
        assert_eq!(
            sermon_title(Path::new("sermons/2026-05-10 - Grace Upon Grace.mp3"), ""),
            "Grace Upon Grace"
        );
        // A tag title wins over the filename date.
        assert_eq!(
            sermon_title(Path::new("sermons/2026-05-10 - x.mp3"), "Tagged"),
            "Tagged"
        );
        // No date prefix: the (de-underlined) stem is kept.
        assert_eq!(
            sermon_title(Path::new("sermons/Sunday_service.mp3"), ""),
            "Sunday service"
        );
        // Tag wins even without a date prefix.
        assert_eq!(
            sermon_title(Path::new("sermons/Sunday service.mp3"), "Tagged"),
            "Tagged"
        );
    }

    #[test]
    fn audiobook_chapters_are_numbered_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "aria-ab-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // <root>/Psalmen/001 - a.mp3, 002 - b.mp3 (content: one zero byte would
        // trip lofty? no — lofty just fails to parse and returns no tags; the
        // duration stays 0, which is fine for the ordering test).
        std::fs::create_dir_all(dir.join("Psalmen")).unwrap();
        std::fs::write(dir.join("Psalmen/002 - b.mp3"), b"\xff\xfb").unwrap();
        std::fs::write(dir.join("Psalmen/001 - a.mp3"), b"\xff\xfb").unwrap();
        std::fs::write(dir.join("Psalmen/notes.txt"), b"not audio").unwrap();
        let lib = Library::scan_collections(&[CollectionItem {
            name: "bible-audio".into(),
            kind: CollectionKind::Audiobook,
            path: dir.clone(),
        }])
        .unwrap();
        assert_eq!(lib.len(), 2);
        let books = lib.books();
        assert_eq!(books, vec!["Psalmen".to_string()]);
        let chs = lib.chapters_of_book("Psalmen");
        assert_eq!(chs.len(), 2);
        let first = lib.get(chs[0]).unwrap();
        let second = lib.get(chs[1]).unwrap();
        assert_eq!(first.chapter_no, Some(1));
        assert_eq!(second.chapter_no, Some(2));
        assert_eq!(first.title, "a");
        assert_eq!(first.kind, CollectionKind::Audiobook);
        assert_eq!(first.collection, "bible-audio");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_artist_normalizes_placeholders() {
        // The owner's choral CDs carry artist tags like "Kein Künstler" —
        // they must normalize to empty, not pollute the artist records.
        assert_eq!(clean_artist("Kein Künstler"), "");
        assert_eq!(clean_artist("KEIN KUENSTLER"), "");
        assert_eq!(clean_artist("Unknown Artist"), "");
        assert_eq!(clean_artist("Various Artists"), "");
        assert_eq!(clean_artist("  "), "");
        // Real artists pass through untouched.
        assert_eq!(clean_artist("Michael Jackson"), "Michael Jackson");
        assert_eq!(clean_artist("  Queen "), "Queen");
    }
}
