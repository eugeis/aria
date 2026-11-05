//! aria-dl — the library downloader (house pattern: single-file, std-only,
//! `rustc -O tools/aria-dl.rs`).
//!
//! Self-updating backend: yt-dlp is a single static binary that we fetch
//! from its GitHub releases when missing or broken (YouTube breaks old
//! yt-dlp versions regularly — a one-shot re-fetch is the fix, no apt).
//! ffmpeg is required for audio extraction/tagging.
//!
//! What it does:
//!   aria-dl ensure                          check/refresh yt-dlp + ffmpeg
//!   aria-dl search <QUERY> [-n N]           list candidates (TSV: id \t duration \t title)
//!   aria-dl dl <URL|yt-dlp-ID>              download audio → MP3 into the library
//!   aria-dl dl --search <QUERY>             search + take the first hit
//!       [--artist A] [--album B] [--collection musik] [--track N]
//!
//! The MP3 lands in <library-root>/<collection>/<Artist>/<Album>/<NN - Title>.mp3
//! with ID3 tags (artist/album/title/track), the same shape the scan
//! expects — the adapter picks it up on its next rescan, no restart.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp";
const YTDLP_SYSTEM: &str = "/usr/local/bin/yt-dlp";
const DEFAULT_LIBRARY: &str = "/home/ee/audio";
const DEFAULT_COLLECTION: &str = "musik";
const FALLBACK_ARTIST: &str = "Downloads";
const FALLBACK_ALBUM: &str = "Verschiedenes";

struct Args {
    cmd: String,
    query: String,
    n: usize,
    artist: String,
    album: String,
    collection: String,
    track: Option<u32>,
    library: PathBuf,
}

fn fail(msg: &str) -> ! {
    eprintln!("aria-dl: {msg}");
    std::process::exit(2);
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd).args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Where to install/use yt-dlp: prefer the per-user bin (writable without
/// root); fall back to the system location when that is usable.
fn ytdlp_path() -> String {
    if let Ok(home) = std::env::var("HOME") {
        let dir = format!("{home}/.local/bin");
        if fs::create_dir_all(&dir).is_ok() {
            // Writability probe via a UNIQUE name — never touch the real
            // binary (an earlier version deleted it during the probe).
            let probe = format!("{dir}/.yt-dlp-writetest-{}", std::process::id());
            if fs::File::create(&probe).is_ok() {
                let _ = fs::remove_file(&probe);
                return format!("{dir}/yt-dlp");
            }
        }
    }
    YTDLP_SYSTEM.to_string()
}

fn find_ytdlp() -> Option<String> {
    for candidate in [ytdlp_path(), YTDLP_SYSTEM.to_string(), "yt-dlp".to_string()] {
        if run(&candidate, &["--version"]).is_ok() {
            return Some(candidate);
        }
    }
    None
}

fn ytdlp_version() -> Option<String> {
    find_ytdlp()
        .and_then(|p| run(&p, &["--version"]).ok())
        .map(|v| v.trim().to_string())
}

/// Self-update: missing or non-runnable yt-dlp → fetch the latest single
/// binary from the GitHub release (no apt, no pip). Returns the binary path.
fn ensure_ytdlp() -> String {
    if let Some(p) = find_ytdlp() {
        return p;
    }
    let dest = ytdlp_path();
    println!(
        "aria-dl: yt-dlp missing/broken — fetching latest release binary to {dest} …"
    );
    let tmp = format!("{dest}.new");
    let status = match Command::new("curl")
        .arg("-fsSL")
        .arg("-o")
        .arg(&tmp)
        .arg(YTDLP_URL)
        .status()
    {
        Ok(s) => s,
        Err(e) => fail(&format!("curl: {e}")),
    };
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        fail(&format!(
            "yt-dlp download failed (curl exit {status}) — {dest} not writable?"
        ));
    }
    let _ = fs::remove_file(&dest);
    if fs::rename(&tmp, &dest).is_err() {
        fail(&format!("cannot install to {dest} (need write access)"));
    }
    let _ = Command::new("chmod").arg("755").arg(&dest).status();
    match ytdlp_version() {
        Some(v) => {
            println!("aria-dl: yt-dlp {v} installed");
        }
        None => fail("yt-dlp installed but still not runnable (binary incompatible?)"),
    }
    dest
}

fn ensure_ffmpeg() {
    let ok = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        fail("ffmpeg not found on PATH — apt-get install ffmpeg (or a static build)");
    }
}

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
        "Unbenannt".to_string()
    } else {
        t.to_string()
    }
}

fn fmt_dur(secs: f64) -> String {
    let m = (secs / 60.0) as u32;
    let s = (secs % 60.0) as u32;
    format!("{m}:{s:02}")
}

fn main() {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        cmd: String::new(),
        query: String::new(),
        n: 10,
        artist: String::new(),
        album: String::new(),
        collection: DEFAULT_COLLECTION.to_string(),
        track: None,
        library: PathBuf::from(DEFAULT_LIBRARY),
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "ensure" | "search" | "dl" => a.cmd = arg.to_string(),
            "--search" => a.query = it.next().unwrap_or_else(|| fail("--search needs a query")),
            "--artist" => a.artist = it.next().unwrap_or_else(|| fail("--artist needs a value")),
            "--album" => a.album = it.next().unwrap_or_else(|| fail("--album needs a value")),
            "--collection" => {
                a.collection = it.next().unwrap_or_else(|| fail("--collection needs a value"))
            }
            "--track" => a.track = it.next().and_then(|v| v.parse().ok()),
            "--library" => a.library = PathBuf::from(it.next().unwrap_or_else(|| fail("--library needs a path"))),
            "-n" => a.n = it.next().and_then(|v| v.parse().ok()).unwrap_or(10),
            s if s.starts_with("--") => fail(&format!("unknown flag {s}")),
            s => a.query = s.to_string(),
        }
    }
    if a.cmd.is_empty() {
        fail("usage: aria-dl ensure | search <q> [-n N] | dl <URL|ID> | dl --search <q> [--artist A --album B --collection C --track N --library DIR]");
    }
    if a.cmd == "ensure" {
        ensure_ffmpeg();
        let v = ensure_ytdlp();
        println!("aria-dl: ok (yt-dlp {v})");
        return;
    }

    let bin = ensure_ytdlp();
    ensure_ffmpeg();
    println!("aria-dl: yt-dlp {bin}");

    match a.cmd.as_str() {
        "search" => {
            if a.query.is_empty() {
                fail("search needs a query");
            }
            let q = format!("ytsearch{}:{}", a.n, a.query);
            let out = run(
                &bin,
                &[
                    "--flat-playlist",
                    "--no-warnings",
                    "-q",
                    "--print",
                    "%(id)s\t%(duration)s\t%(title)s",
                    "--",
                    q.as_str(),
                ],
            )
            .unwrap_or_else(|e| fail(&e));
            let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                println!("aria-dl: no results for '{}'", a.query);
                return;
            }
            for (i, l) in lines.iter().enumerate() {
                let mut parts = l.split('\t');
                let id = parts.next().unwrap_or_default();
                let dur = parts
                    .next()
                    .and_then(|d| d.parse::<f64>().ok())
                    .map(fmt_dur)
                    .unwrap_or_else(|| "?".into());
                let title = parts.next().unwrap_or_default();
                println!("{:>2}  {dur}  {id}  {title}", i + 1);
            }
        }
        "dl" => {
            if a.query.is_empty() {
                fail("dl needs a URL, an ID, or --search <query>");
            }
            let target = if a.query.starts_with("http") || a.query.starts_with("yt") || a.query.len() <= 11 {
                a.query.clone()
            } else {
                // A plain phrase → take the first search hit.
                format!("ytsearch1:{}", a.query)
            };
            // Force the requested artist/album into the ID3 tags (yt-dlp's
            // extracted metadata is often empty or wrong for choral content).
            let artist_dir = slug_dir(if a.artist.is_empty() { "" } else { &a.artist });
            let album_dir = slug_dir(if a.album.is_empty() { "" } else { &a.album });
            let col_dir = slug_dir(&a.collection);
            let out_dir = a.library.join(&col_dir).join(if artist_dir.is_empty() { FALLBACK_ARTIST } else { &artist_dir }).join(if album_dir.is_empty() { FALLBACK_ALBUM } else { &album_dir });
            if fs::create_dir_all(&out_dir).is_err() {
                fail(&format!("cannot create {:?}", out_dir));
            }
            let track_prefix = a.track.map(|t| format!("{t:02} - ")).unwrap_or_default();
            // %(title)s.%(ext)s → we tag + rename after the fact (cleaner
            // than fighting yt-dlp's output template with forced fields).
            let out_tpl = out_dir.join(format!("__dl_{track_prefix}%(title)s.%(ext)s"));
            let mut yt_args: Vec<String> = vec![
                "--no-warnings".into(),
                "-f".into(),
                "bestaudio/best".into(),
                "-x".into(),
                "--audio-format".into(),
                "mp3".into(),
                "--audio-quality".into(),
                "192K".into(),
                "--embed-metadata".into(),
                "-o".into(),
                out_tpl.to_string_lossy().into_owned(),
                "--".into(),
                target.clone(),
            ];
            // No --parse-metadata: its value must be a named-group regex and
            // literal values abort the download ("bad character in group
            // name"). fix_tags() below sets the authoritative ID3 tags.
            print!("aria-dl: downloading '{target}' … ");
            let res = run(&bin, &yt_args.iter().map(|s| s.as_str()).collect::<Vec<_>>()[..]);
            match res {
                Ok(_) => {
                    // Rename __dl_* → final name, then guarantee the ID3
                    // tags via ffmpeg (yt-dlp's extracted metadata is
                    // often empty or wrong for choral content).
                    let files: Vec<PathBuf> = fs::read_dir(&out_dir)
                        .into_iter()
                        .flatten()
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.to_string_lossy().contains("__dl_") && p.extension().map(|x| x == "mp3").unwrap_or(false))
                        .collect();
                    let some = files.first().cloned();
                    match some {
                        Some(f) => {
                            let base = f
                                .file_name()
                                .map(|n| {
                                    let n = n.to_string_lossy();
                                    n.replacen("__dl_", "", 1)
                                })
                                .unwrap_or_else(|| "download.mp3".into());
                            let dst = out_dir.join(&base);
                            let _ = fs::rename(&f, &dst);
                            let _ = fix_tags(&dst, &a.artist, &a.album, a.track, &base);
                            println!("done");
                            println!("aria-dl: {}", dst.to_string_lossy());
                        }
                        None => println!("finished (no output file found — check the yt-dlp output above)"),
                    }
                }
                Err(e) => fail(&e),
            }
        }
        _ => unreachable!(),
    }
}

/// Guarantee the ID3 tags via ffmpeg (mp3v2 + title/artist/album/track).
fn fix_tags(
    file: &Path,
    artist: &str,
    album: &str,
    track: Option<u32>,
    filename: &str,
) -> Result<(), String> {
    let title = filename
        .rsplit_once(".mp3")
        .map(|(t, _)| t.to_string())
        .unwrap_or_else(|| filename.to_string());
    let title = title
        .split(" - ")
        .last()
        .map(|s| s.to_string())
        .unwrap_or(title);
    let meta: Vec<String> = vec![
        format!("title={title}"),
        if artist.is_empty() {
            String::new()
        } else {
            format!("artist={artist}")
        },
        if album.is_empty() {
            String::new()
        } else {
            format!("album={album}")
        },
        track.map(|t| format!("track={t}")).unwrap_or_default(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        file.to_string_lossy().into_owned(),
        "-map_metadata".into(),
        "0".into(),
        "-metadata".into(),
    ];
    args.extend(meta);
    args.push(file.to_string_lossy().into_owned());
    run("ffmpeg", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()[..]).map(|_| ())
}
