//! Library scan -> metadata records -> deterministic media.jsonl.gz.
//!
//! The adapter is the ONLY component that reads the audio files (tags via
//! lofty, streaming for the Echo). It publishes a flat JSONL of metadata
//! records — one per track / album / artist / radio station / audiobook /
//! chapter / sermon recording — which the brain ingests as the `media`
//! knowledge collection (`kind: entries`: `text` = retrieval text, `slug` =
//! stable id, everything else = `meta`, searchable + meta-filterable).
//!
//! Determinism: records are sorted by slug, keys sorted, gzip with mtime 0
//! (ADR 0156 form) — an unchanged library yields a byte-identical file, so
//! the brain's delta manifest (ADR 0156/0019) skips unchanged scans.

use std::collections::BTreeMap;

use aria_audio::{CollectionKind, Library, slugify};

use crate::config::Config;

fn j<T: serde::Serialize + ?Sized>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// One record: `slug` + `text` (the retrieval text) + meta fields.
fn record(
    slug: String,
    text: &str,
    mut meta: BTreeMap<String, serde_json::Value>,
) -> serde_json::Value {
    meta.insert("slug".into(), j(&slug));
    meta.insert("text".into(), j(&text));
    serde_json::Value::Object(meta.into_iter().collect())
}

/// Build the full metadata catalog from a scanned library + config.
pub fn build_records(library: &Library, cfg: &Config) -> Vec<serde_json::Value> {
    let mut recs: Vec<serde_json::Value> = Vec::new();

    // Radio presets (declared in config, not scanned).
    for (name, url) in &cfg.radio.stations {
        let mut m = BTreeMap::new();
        m.insert("entry".into(), j(&name));
        m.insert("kind_media".into(), j("radio"));
        m.insert("name".into(), j(&name));
        m.insert("url".into(), j(&url));
        recs.push(record(
            format!("rad:{}", slugify(name)),
            &format!("{name} — Internetradio-Station"),
            m,
        ));
    }

    // Per-file records + umbrella aggregation.
    // albums: (collection|artist|album) -> track count
    let mut albums: BTreeMap<String, (String, String, String, usize)> = BTreeMap::new();
    // artists: (collection|artist) -> track count
    let mut artists: BTreeMap<String, usize> = BTreeMap::new();
    // books: name -> (narrator, chapter count, max chapter no)
    let mut books: BTreeMap<String, (String, usize, usize)> = BTreeMap::new();

    for t in library.iter() {
        let (slug, kind_media, text): (String, &str, String) = match t.kind {
            CollectionKind::Music => {
                let mut parts: Vec<String> = Vec::new();
                if !t.album.is_empty() {
                    parts.push(format!(
                        "{}{}",
                        t.album,
                        if t.year.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", t.year)
                        }
                    ));
                }
                if !t.genre.is_empty() {
                    parts.push(t.genre.clone());
                }
                let text = if parts.is_empty() {
                    format!("{} — {}", t.title, t.artist)
                } else {
                    format!("{} — {}, {}", t.title, t.artist, parts.join(", "))
                };
                (format!("trk:{:04}", t.id), "track", text)
            }
            CollectionKind::Audiobook => (
                format!(
                    "abk:{}:ch:{:02}",
                    slugify(&t.book),
                    t.chapter_no.unwrap_or(0)
                ),
                "chapter",
                format!(
                    "{} — {} ({}: Kapitel {})",
                    t.title,
                    t.book,
                    t.book,
                    t.chapter_no.unwrap_or(0)
                ),
            ),
            CollectionKind::Sermon => (
                format!("serm:{:04}", t.id),
                "sermon",
                format!("{} — Predigtaufnahme", t.title),
            ),
        };

        let mut m = BTreeMap::new();
        m.insert("entry".into(), j(&t.title));
        m.insert("kind_media".into(), j(kind_media));
        m.insert("collection".into(), j(&t.collection));
        if !t.artist.is_empty() {
            m.insert("artist".into(), j(&t.artist));
        }
        if !t.album.is_empty() {
            m.insert("album".into(), j(&t.album));
        }
        if !t.year.is_empty() {
            m.insert("year".into(), j(&t.year));
        }
        if !t.genre.is_empty() {
            m.insert("genre".into(), j(&t.genre));
        }
        if let Some(n) = t.track_no {
            m.insert("track_no".into(), j(&n));
        }
        if t.kind == CollectionKind::Audiobook {
            m.insert("book".into(), j(&t.book));
            m.insert("chapter".into(), j(&t.chapter_no.unwrap_or(0)));
            m.insert("chapter_title".into(), j(&t.title));
            if !t.artist.is_empty() {
                m.insert("narrator".into(), j(&t.artist));
            }
        }
        if t.playable {
            m.insert("file".into(), j(&t.path));
            m.insert("duration_ms".into(), j(&t.duration_ms));
            m.insert("format".into(), j(&t.format_str()));
        }
        recs.push(record(slug, &text, m));

        // Umbrella aggregation.
        match t.kind {
            CollectionKind::Music => {
                if !t.album.is_empty() {
                    let key = format!("{}|{}|{}", t.collection, t.artist, t.album);
                    let e = albums.entry(key).or_insert_with(|| {
                        (t.collection.clone(), t.artist.clone(), t.album.clone(), 0)
                    });
                    e.3 += 1;
                }
                if !t.artist.is_empty() {
                    let key = format!("{}|{}", t.collection, t.artist);
                    *artists.entry(key).or_insert(0) += 1;
                }
            }
            CollectionKind::Audiobook => {
                let key = t.book.clone();
                let e = books.entry(key).or_insert_with(|| (t.artist.clone(), 0, 0));
                e.1 += 1;
                e.2 = e.2.max(t.chapter_no.unwrap_or(0) as usize);
            }
            CollectionKind::Sermon => {}
        }
    }

    for (collection, artist, album, n) in albums.values() {
        let mut m = BTreeMap::new();
        m.insert("entry".into(), j(&album));
        m.insert("kind_media".into(), j("album"));
        m.insert("collection".into(), j(&collection));
        m.insert("artist".into(), j(&artist));
        m.insert("album".into(), j(&album));
        m.insert("tracks".into(), j(&n));
        // The collection word is part of the record text so list questions
        // ("welche sammlungen habe ich") retrieve factual context for the
        // LLM — it must read "Sammlung musik", not guess from names.
        let text = if artist.is_empty() {
            format!("{album} — Album, {n} Titel, Sammlung {collection}")
        } else {
            format!("{album} — Album von {artist}, {n} Titel, Sammlung {collection}")
        };
        recs.push(record(format!("alb:{}", slugify(album)), &text, m));
    }
    // Distinct album counts per (collection|artist) from the albums map.
    let mut album_counts: BTreeMap<String, usize> = BTreeMap::new();
    for key in albums.keys() {
        if let Some(coll_artist) = key.rsplit_once('|').map(|(p, _)| p) {
            *album_counts.entry(coll_artist.to_string()).or_insert(0) += 1;
        }
    }
    for (key, tracks_n) in &artists {
        let artist_name = artists_key_name(key);
        let collection = key.split('|').next().unwrap_or(key).to_string();
        let albums_n = album_counts.get(key).copied().unwrap_or(0);
        let mut m = BTreeMap::new();
        m.insert("entry".into(), j(&artist_name));
        m.insert("kind_media".into(), j("artist"));
        m.insert("collection".into(), j(&collection));
        m.insert("artist".into(), j(&artist_name));
        m.insert("albums".into(), j(&albums_n));
        m.insert("tracks".into(), j(&tracks_n));
        recs.push(record(
            format!("art:{}", slugify(&artist_name)),
            &format!("{artist_name} — Künstler, Sammlung {collection}, {albums_n} Alben, {tracks_n} Titel"),
            m,
        ));
    }
    for (book, (narrator, chapters, _last)) in &books {
        let mut m = BTreeMap::new();
        m.insert("entry".into(), j(&book));
        m.insert("kind_media".into(), j("audiobook"));
        m.insert("book".into(), j(&book));
        if !narrator.is_empty() {
            m.insert("narrator".into(), j(&narrator));
        }
        m.insert("chapters".into(), j(&chapters));
        let text = if narrator.is_empty() {
            format!("{book} — Hörbuch, {chapters} Kapitel")
        } else {
            format!("{book} — Hörbuch, {chapters} Kapitel, gelesen von {narrator}")
        };
        recs.push(record(format!("abk:{}", slugify(book)), &text, m));
    }

    // Deterministic order: by slug.
    recs.sort_by(|a, b| {
        a.get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
    });
    recs
}

/// The artist name embedded in an artists-map key (`collection|artist`).
fn artists_key_name(key: &str) -> String {
    key.split('|').nth(1).unwrap_or(key).to_string()
}

/// Deterministic gzip (mtime 0, no filename) of the input bytes.
pub fn gzip_deterministic(input: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    // Deflate (zlib-wrapped, header + adler32 stripped below).
    let mut deflater = ZlibEncoder::new(Vec::new(), Compression::best());
    deflater.write_all(input).expect("deflate write");
    let zlib = deflater.finish().expect("deflate finish");
    let deflate = &zlib[2..zlib.len() - 4];

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(input);
    let crc = hasher.finalize();
    let mut out = Vec::with_capacity(10 + deflate.len() + 8);
    // GZIP header: magic, method 8 (deflate), flags 0, mtime 0, xfl 2 (best
    // compression), os 255 (unknown).
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff]);
    out.extend_from_slice(deflate);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(input.len() as u32).to_le_bytes());
    out
}

/// Serialize records to JSONL (one per line; record keys are sorted).
pub fn records_to_jsonl(records: &[serde_json::Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        let line = serde_json::to_string(r).expect("record serializes");
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Write the scan output atomically (tmp + rename) when changed.
/// Returns true when the file was rewritten (the brain should reindex).
///
/// Guard: an EMPTY scan (the media dir is empty or briefly unmounted) must
/// never wipe a previously published catalog — that would erase the
/// library metadata (the seeded sample corpus, or a real library during a
/// transient mount failure) from the brain's knowledge base.
pub fn publish(cfg: &Config, records: &[serde_json::Value]) -> std::io::Result<bool> {
    let out = cfg.scan_out_path();
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let new = gzip_deterministic(&records_to_jsonl(records));
    if std::fs::read(&out).map(|old| old == new).unwrap_or(false) {
        return Ok(false);
    }
    if records.is_empty() && existing_has_records(&out) {
        return Ok(false);
    }
    let tmp = out.with_extension("jsonl.tmp");
    std::fs::write(&tmp, &new)?;
    std::fs::rename(&tmp, &out)?;
    Ok(true)
}

/// True when the published file at `out` decodes to at least one record
/// (one JSON line each). A missing or undecodable file counts as empty.
fn existing_has_records(out: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(old) = std::fs::read(out) else {
        return false;
    };
    let mut dec = flate2::read::GzDecoder::new(&old[..]);
    let mut buf = Vec::new();
    if dec.read_to_end(&mut buf).is_err() {
        return false;
    }
    buf.contains(&b'\n')
}

/// Publish the catalog ONLY when the scan saw an actual library.
///
/// An empty scan (media dir missing or briefly unmounted, collections not
/// created yet) must never publish — even the config's radio presets alone
/// would form a non-empty record set and wipe the previously published
/// corpus out of the brain's knowledge base. Returns `Some(changed)` when
/// a publish happened, `None` when the empty scan was skipped.
pub fn publish_if_nonempty(
    cfg: &Config,
    library: &Library,
    records: &[serde_json::Value],
) -> std::io::Result<Option<bool>> {
    if library.is_empty() {
        return Ok(None);
    }
    Ok(Some(publish(cfg, records)?))
}

/// Ask the brain to reindex its knowledge (the adapter's service token).
pub async fn reindex(cfg: &Config) -> Result<(), String> {
    let Some(token) = cfg.brain.token.clone() else {
        return Err("brain.token is not set; cannot reindex".into());
    };
    let base = cfg.brain.base_url.trim_end_matches('/');
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/ops/reindex"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let s = resp.status();
        let b = resp.text().await.unwrap_or_default();
        return Err(format!("reindex failed: {s}: {b}"));
    }
    Ok(())
}

/// Run one full scan: walk the collections, index the library, build the
/// metadata records.
///
/// Ids are minted through the persistent ID registry (content
/// fingerprint → id), so a track keeps its id — and the brain keeps its
/// embedding (ADR 0161) — across moves; only genuinely new files get new
/// ids. A first-ever scan reproduces the legacy positional ids.
pub fn scan_library(cfg: &Config) -> Result<(Library, Vec<serde_json::Value>), String> {
    let items = cfg.collection_items()?;
    let reg_path = cfg.data_dir().join("id-registry.json");
    let mut registry = aria_audio::IdRegistry::load(&reg_path);
    let known = registry.map.len();
    let library = Library::scan_collections_with(&items, &mut registry)
        .map_err(|e| e.to_string())?;
    registry.save(&reg_path);
    let fresh = registry.map.len() - known;
    if fresh > 0 {
        eprintln!("id-registry: {fresh} new id(s) minted ({known} known)");
    }
    let records = build_records(&library, cfg);
    Ok((library, records))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_is_deterministic_and_valid() {
        let input = b"line1\nline2\nline3\n";
        let a = gzip_deterministic(input);
        let b = gzip_deterministic(input);
        assert_eq!(a, b, "same input => same bytes");
        assert_eq!(&a[4..8], &[0, 0, 0, 0], "mtime must be zero");
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut dec = GzDecoder::new(&a[..]);
        let mut back = Vec::new();
        dec.read_to_end(&mut back).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn jsonl_is_sorted_and_stable() {
        let mut recs = vec![
            serde_json::json!({"text": "B", "slug": "b"}),
            serde_json::json!({"text": "A", "slug": "a"}),
        ];
        recs.sort_by(|a, b| a["slug"].as_str().unwrap().cmp(b["slug"].as_str().unwrap()));
        let a = records_to_jsonl(&recs);
        let b = records_to_jsonl(&recs);
        assert_eq!(a, b);
        assert_eq!(std::str::from_utf8(&a).unwrap().lines().count(), 2);
    }

    #[test]
    fn record_carries_slug_and_text() {
        let mut m = BTreeMap::new();
        m.insert("artist".to_string(), j(&"Queen"));
        let r = record("trk:0001".into(), "Bohemian Rhapsody — Queen", m);
        assert_eq!(r["slug"], "trk:0001");
        assert_eq!(r["text"], "Bohemian Rhapsody — Queen");
        assert_eq!(r["artist"], "Queen");
    }

    #[test]
    fn slugify_is_stable() {
        assert_eq!(slugify("A Night at the Opera"), "a-night-at-the-opera");
        assert_eq!(slugify("Psalmen"), "psalmen");
        assert_eq!(slugify("---x---"), "x");
    }

    fn cfg_with_out(out: &std::path::Path) -> Config {
        toml::from_str(&format!(
            r#"
            [server]
            bind = "127.0.0.1:0"
            [scan]
            out_path = "{}"
            [radio.stations]
            "rock fm" = "https://r.example/rock.mp3"
            "#,
            out.display().to_string().replace('"', "\\\"")
        ))
        .expect("test config parses")
    }

    #[test]
    fn publish_creates_and_updates() {
        let dir = std::env::temp_dir().join(format!("aria-scan-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("media.jsonl.gz");
        let cfg = cfg_with_out(&out);
        assert!(
            publish(&cfg, &[]).unwrap(),
            "first publish (no file yet) creates it"
        );
        assert!(out.exists());
        // A non-empty catalog publishes; identical content does not republish.
        let one = vec![serde_json::json!({"slug": "trk:0001", "text": "t", "kind_media": "track"})];
        assert!(publish(&cfg, &one).unwrap(), "changed content publishes");
        assert!(
            !publish(&cfg, &one).unwrap(),
            "identical content does not republish"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_library_publishes_nothing() {
        let dir = std::env::temp_dir().join(format!("aria-scan-test-lib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("media.jsonl.gz");
        let cfg = cfg_with_out(&out);
        let lib = Library::empty();
        // Config radio presets alone must NOT form a publishable catalog.
        let records = build_records(&lib, &cfg);
        assert!(
            matches!(publish_if_nonempty(&cfg, &lib, &records), Ok(None)),
            "an empty library never publishes (even with radio presets present)"
        );
        assert!(!out.exists(), "nothing was written");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_scan_never_wipes_a_populated_catalog() {
        let dir = std::env::temp_dir().join(format!("aria-scan-test-wipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("media.jsonl.gz");
        let cfg = cfg_with_out(&out);
        let recs = vec![
            serde_json::json!({"slug": "trk:0001", "text": "a", "kind_media": "track"}),
            serde_json::json!({"slug": "alb:x", "text": "b", "kind_media": "album"}),
        ];
        assert!(publish(&cfg, &recs).unwrap());
        // The media dir goes away (or is empty) -> the scan yields nothing.
        // The published catalog must survive.
        assert!(
            !publish(&cfg, &[]).unwrap(),
            "an empty scan must not overwrite a populated catalog"
        );
        let old = std::fs::read(&out).unwrap();
        assert_eq!(old, gzip_deterministic(&records_to_jsonl(&recs)));
        // ...and an empty scan over an empty (or missing) file is allowed.
        std::fs::remove_file(&out).unwrap();
        assert!(publish(&cfg, &[]).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
