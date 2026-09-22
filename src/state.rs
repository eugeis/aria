//! Shared application state: library, per-device players, playback tokens,
//! pending approvals, and cached late agent replies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use aria_audio::{Library, PlaybackRegistry, PlayerState};
use aria_zeroclaw::ApprovalInfo;
use serde::{Deserialize, Serialize};

use crate::agent::AgentBackend;
use crate::config::Config;

/// A tool approval we should ask the user about on the next turn.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub info: ApprovalInfo,
    pub asked_at: Instant,
}

/// An agent reply that completed after the user's request had already been
/// answered (post-approval turn or timeout); spoken on the next turn.
#[derive(Debug, Clone)]
pub struct CachedReply {
    pub text: String,
}

pub struct AppState {
    pub cfg: Config,
    /// Swapped atomically on rescan.
    pub library: Arc<RwLock<Library>>,
    /// device id -> player state.
    pub players: Mutex<HashMap<String, PlayerState>>,
    pub registry: PlaybackRegistry,
    pub agent: Arc<dyn AgentBackend>,
    /// device id -> pending tool approval awaiting a yes/no.
    pub approvals: Mutex<HashMap<String, PendingApproval>>,
    /// device id -> late reply to speak on the next turn.
    pub cached_reply: Mutex<HashMap<String, CachedReply>>,
    pub started: Instant,
}

impl AppState {
    pub fn new(cfg: Config, library: Library, agent: Arc<dyn AgentBackend>) -> Self {
        let players = load_state(&cfg.state_path()).players;
        Self {
            cfg,
            library: Arc::new(RwLock::new(library)),
            players: Mutex::new(players),
            registry: PlaybackRegistry::new(),
            agent,
            approvals: Mutex::new(HashMap::new()),
            cached_reply: Mutex::new(HashMap::new()),
            started: Instant::now(),
        }
    }

    pub fn player(&self) -> std::sync::MutexGuard<'_, HashMap<String, PlayerState>> {
        match self.players.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Pending approval for `device`, if one is still within the gateway's
    /// approval timeout (stale ones are dropped; the gateway auto-denied them).
    pub fn pending_approval(&self, device: &str) -> Option<PendingApproval> {
        let mut map = self.approvals.lock().unwrap();
        let p = map.get(device)?;
        if p.asked_at.elapsed() >= std::time::Duration::from_secs(p.info.timeout_secs.max(1)) {
            map.remove(device);
            return None;
        }
        Some(p.clone())
    }

    pub fn set_pending_approval(&self, device: &str, p: Option<PendingApproval>) {
        let mut m = self.approvals.lock().unwrap();
        match p {
            Some(v) => {
                m.insert(device.to_string(), v);
            }
            None => {
                m.remove(device);
            }
        }
    }

    pub fn take_cached_reply(&self, device: &str) -> Option<String> {
        self.cached_reply
            .lock()
            .unwrap()
            .remove(device)
            .map(|c| c.text)
    }

    pub fn set_cached_reply(&self, device: &str, text: String) {
        if !text.trim().is_empty() {
            self.cached_reply
                .lock()
                .unwrap()
                .insert(device.to_string(), CachedReply { text });
        }
    }

    /// Persist player states (debounced by callers; also on shutdown).
    pub fn save_state(&self) {
        let players = self.player().clone();
        let state = PersistedState {
            players,
            saved_at: now_iso(),
        };
        let path = self.cfg.state_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(raw) = serde_json::to_string_pretty(&state) {
            if let Err(e) = std::fs::write(&path, raw) {
                tracing::warn!(path = %path.display(), %e, "failed to save state");
            }
        }
    }

    /// Cover art URL for a track (empty string when unavailable).
    pub fn art_url(&self, track_id: u64) -> String {
        let lib = self.library.read().unwrap();
        if lib.cover(track_id).is_some() {
            format!("{}/art/{track_id}", self.cfg.base_url())
        } else {
            String::new()
        }
    }
}

fn now_iso() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    s.to_string()
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    players: HashMap<String, PlayerState>,
    saved_at: String,
}

fn load_state(path: &PathBuf) -> PersistedState {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => PersistedState::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::MockAgent;

    fn test_state(dir: &std::path::Path) -> AppState {
        let mut cfg = Config::default();
        cfg.server.data_dir = Some(dir.to_path_buf());
        AppState::new(cfg, Library::empty(), Arc::new(MockAgent::new()))
    }

    #[test]
    fn state_roundtrips() {
        let dir = std::env::temp_dir().join(format!("aria-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = test_state(&dir);
        {
            let mut p = st.player();
            let mut ps = PlayerState::default();
            ps.start_queue(vec![1, 2]);
            ps.shuffle = true;
            p.insert("dev1".into(), ps);
        }
        st.save_state();
        let st2 = test_state(&dir);
        let p = st2.player();
        assert!(p.get("dev1").is_some());
        assert!(p["dev1"].shuffle);
        assert_eq!(p["dev1"].queue, vec![1, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
