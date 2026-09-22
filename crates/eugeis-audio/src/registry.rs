//! Playback tokens: short-lived, unguessable handles binding a stream (file or
//! radio URL) to a device, plus the byte offset the stream should start at.

use rand::Rng;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Where a playback token's bytes come from.
#[derive(Debug, Clone)]
pub enum StreamSource {
    File {
        track_id: u64,
        path: String,
        format: crate::library::Format,
        /// Starting offset in ms (for resume/seek).
        offset_ms: u64,
        /// Average bitrate in kbps (ms -> byte conversion).
        avg_bitrate_kbps: u32,
        title: String,
        artist: String,
    },
    Radio {
        url: String,
        title: String,
    },
}

pub struct ResolvedStream {
    pub source: StreamSource,
    pub device: String,
}

struct Entry {
    source: StreamSource,
    device: String,
    expires: Instant,
}

pub struct PlaybackRegistry {
    entries: Mutex<HashMap<String, Entry>>,
    /// How long a token stays valid after issuance.
    ttl: Duration,
}

impl Default for PlaybackRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(12 * 3600),
        }
    }

    /// Issue a new token for `source` on `device`.
    pub fn issue(&self, device: &str, source: StreamSource) -> String {
        let token = random_token();
        self.entries.lock().unwrap().insert(
            token.clone(),
            Entry {
                source,
                device: device.to_string(),
                expires: Instant::now() + self.ttl,
            },
        );
        token
    }

    pub fn resolve(&self, token: &str) -> Option<ResolvedStream> {
        let mut map = self.entries.lock().unwrap();
        let entry = map.get(token)?;
        if entry.expires < Instant::now() {
            map.remove(token);
            return None;
        }
        Some(ResolvedStream {
            source: entry.source.clone(),
            device: entry.device.clone(),
        })
    }

    /// Drop expired tokens; returns how many were removed.
    pub fn expire_old(&self) -> usize {
        let mut map = self.entries.lock().unwrap();
        let now = Instant::now();
        let before = map.len();
        map.retain(|_, e| e.expires >= now);
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn random_token() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 24] = rng.random();
    let mut s = String::with_capacity(48);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize].chars().next().unwrap());
        s.push(HEX[(b & 0xf) as usize].chars().next().unwrap());
    }
    s
}

const HEX: [&str; 16] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "a", "b", "c", "d", "e", "f",
];

/// True for tokens we issued (hex, 48 chars). Guards the stream endpoint.
pub fn looks_like_token(s: &str) -> bool {
    s.len() == 48 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_and_resolve() {
        let reg = PlaybackRegistry::new();
        let tok = reg.issue(
            "dev1",
            StreamSource::Radio {
                url: "https://r.example/stream.mp3".into(),
                title: "Rock FM".into(),
            },
        );
        assert!(looks_like_token(&tok));
        let r = reg.resolve(&tok).unwrap();
        assert_eq!(r.device, "dev1");
        match r.source {
            StreamSource::Radio { url, .. } => assert_eq!(url, "https://r.example/stream.mp3"),
            _ => panic!("wrong source"),
        }
        assert!(reg.resolve("not-a-token").is_none());
    }

    #[test]
    fn token_shape() {
        let t = random_token();
        assert_eq!(t.len(), 48);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
