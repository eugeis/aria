//! One-shot library transcoder: turns non-Echo-playable audio files
//! (FLAC/WMA/OGG/OPUS/WAV/AIFF) into MP3 next to the original, so the
//! whole library is streamable through the Echo (AudioPlayer supports
//! MP3/AAC only). Idempotent: an existing .mp3 that is at least as new
//! as the source is left alone. The originals are NEVER touched or
//! deleted — the MP3s are added beside them.
//!
//! Single-file, std-only (house pattern — `rustc -O tools/transcode.rs`).
//! Requires ffmpeg on PATH.
//!
//! Usage:
//!   transcode [DIR ...] [--jobs N] [--bitrate 192k]
//!
//! With no DIR, the collections from the adapter config are NOT known
//! here — pass the library roots explicitly (e.g. /home/ee/audio).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::SystemTime;

// Extensions we transcode to MP3. Everything else (mp3/aac/m4a/…) is left
// alone — the Echo streams MP3/AAC natively.
const CONVERT_EXTS: &[&str] = &["flac", "wma", "ogg", "opus", "wav", "aiff", "aif"];

struct Args {
    dirs: Vec<PathBuf>,
    jobs: usize,
    bitrate: String,
}

fn fail(msg: &str) -> ! {
    eprintln!("transcode: {msg}");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut dirs = Vec::new();
    let mut jobs = 4usize;
    let mut bitrate = "192k".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jobs" => {
                jobs = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    fail("usage: transcode [DIR ...] [--jobs N] [--bitrate 192k]")
                });
            }
            "--bitrate" => {
                bitrate = it.next().unwrap_or_else(|| {
                    fail("usage: transcode [DIR ...] [--jobs N] [--bitrate 192k]")
                });
            }
            s if s.starts_with("--") => fail(&format!("unknown flag {s}")),
            s => dirs.push(PathBuf::from(s)),
        }
    }
    if dirs.is_empty() {
        fail("no input directories — transcode [DIR ...]");
    }
    Args { dirs, jobs, bitrate }
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn mtime(p: &Path) -> SystemTime {
    fs::metadata(p).and_then(|m| m.modified()).unwrap_or_else(|_| SystemTime::UNIX_EPOCH)
}

/// Display path relative to the library root (cosmetic log help).
fn rel(p: &Path) -> &Path {
    p.strip_prefix("/home/ee/audio").unwrap_or(p)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        eprintln!("transcode: cannot read {dir:?} — skipped");
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if CONVERT_EXTS.contains(&ext_of(&p).as_str()) {
            out.push(p);
        }
    }
}

fn convert_one(src: &Path, bitrate: &str) -> Result<PathBuf, String> {
    let Some(stem) = src.file_stem() else {
        return Err("no file stem".into());
    };
    let dst = src.with_file_name(format!("{}.mp3", stem.to_string_lossy()));
    // Idempotency: existing, fresh-enough output wins.
    if dst.exists() && mtime(&dst) >= mtime(src) {
        return Err("skip-existing".into());
    }
    let out = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(src)
        .arg("-vn")
        .arg("-c:a")
        .arg("libmp3lame")
        .arg("-b:a")
        .arg(bitrate)
        .arg("-map_metadata")
        .arg("0")
        .arg(&dst)
        .output()
        .map_err(|e| format!("ffmpeg spawn: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        let _ = fs::remove_file(&dst); // never leave partial files
        return Err(format!("ffmpeg failed: {}", err.trim()));
    }
    Ok(dst)
}

fn main() {
    let args = parse_args();
    if Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        == false
    {
        fail("ffmpeg not found on PATH");
    }

    let mut sources: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for d in &args.dirs {
        if !d.is_dir() {
            fail(&format!("not a directory: {d:?}"));
        }
        walk(d, &mut sources);
    }
    sources.retain(|p| seen.insert(p.clone()));
    let to_convert: Vec<PathBuf> = sources
        .iter()
        .filter(|s| {
            let dst = s.with_extension("mp3");
            !dst.exists() || mtime(&dst) < mtime(s)
        })
        .cloned()
        .collect();
    let preexisting = sources.len() - to_convert.len();
    println!(
        "transcode: {} file(s) to convert, {} already done, {} parallel job(s), {bitrate}",
        to_convert.len(),
        preexisting,
        args.jobs,
        bitrate = args.bitrate
    );
    if to_convert.is_empty() {
        return;
    }

    let (tx, rx) = mpsc::channel::<(PathBuf, Result<PathBuf, String>)>();
    let (qtx, qrx) = mpsc::sync_channel::<PathBuf>(64);
    let n = args.jobs.max(1).min(16);
    // A sync Receiver is not Clone — the worker pool shares ONE receiver
    // behind a Mutex (recv is cheap; the ffmpeg work happens unlocked).
    let shared = std::sync::Arc::new(std::sync::Mutex::new(qrx));
    let mut handles = Vec::new();
    for _ in 0..n {
        let shared = shared.clone();
        let tx = tx.clone();
        let bitrate = args.bitrate.clone();
        handles.push(std::thread::spawn(move || loop {
            match shared.lock().unwrap().recv() {
                Ok(src) => {
                    let dst = convert_one(&src, &bitrate);
                    tx.send((src, dst)).ok();
                }
                Err(_) => break, // queue closed — all jobs dispatched
            }
        }));
    }
    for s in &to_convert {
        if qtx.send(s.clone()).is_err() {
            break;
        }
    }
    drop(qtx); // close the queue → the workers drain and exit
    drop(tx);
    for h in handles {
        h.join().ok();
    }

    let mut ok = 0usize;
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for (src, res) in rx {
        match res {
            Ok(dst) => {
                ok += 1;
                println!("  ok  {} -> {}", rel(&src).display(), dst.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default());
            }
            Err(e) if e == "skip-existing" => {}
            Err(e) => {
                failed.push((src, e));
            }
        }
    }
    println!(
        "transcode: done — {ok} converted, {preexisting} already done, {} failed",
        failed.len()
    );
    for (src, e) in &failed {
        eprintln!("  FAIL {} : {e}", rel(src).display());
    }
    if !failed.is_empty() {
        std::process::exit(1);
    }
}
