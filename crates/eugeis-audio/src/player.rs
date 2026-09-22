//! Per-device player state: queue, position, history, shuffle/repeat.

use serde::{Deserialize, Serialize};

use crate::library::shuffle_in_place;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Repeat {
    #[default]
    Off,
    One,
    All,
}

/// Serializable per-device playback state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerState {
    /// Ordered track ids to play; `pos` indexes the current track.
    pub queue: Vec<u64>,
    pub pos: usize,
    /// Recently played (most recent last); backs "previous".
    pub history: Vec<u64>,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Token currently bound to a stream (see [`crate::registry::PlaybackRegistry`]).
    pub current_token: Option<String>,
    /// Byte-offset start of the current stream, in ms.
    pub current_offset_ms: u64,
}

impl PlayerState {
    pub fn current(&self) -> Option<u64> {
        self.queue.get(self.pos).copied()
    }

    /// Replace the queue and start at its first entry (or stay idle if empty).
    pub fn start_queue(&mut self, queue: Vec<u64>) {
        self.queue = queue;
        self.pos = 0;
        self.current_token = None;
        self.current_offset_ms = 0;
    }

    pub fn is_idle(&self) -> bool {
        self.queue.is_empty() || self.pos >= self.queue.len()
    }

    /// Move forward respecting repeat mode. Returns the next track id.
    pub fn advance(&mut self) -> Option<u64> {
        if self.queue.is_empty() {
            return None;
        }
        match self.repeat {
            Repeat::One => Some(self.queue[self.pos.min(self.queue.len() - 1)]),
            Repeat::All => {
                self.pos = (self.pos + 1) % self.queue.len();
                Some(self.queue[self.pos])
            }
            Repeat::Off => {
                if self.pos + 1 < self.queue.len() {
                    self.pos += 1;
                    Some(self.queue[self.pos])
                } else {
                    // End of queue: park at the last position, mark idle.
                    None
                }
            }
        }
    }

    /// Go back one track (from history); falls back to restarting current.
    pub fn go_back(&mut self) -> Option<u64> {
        // The current track may already be the last history entry (it was
        // `note_finished()` before the player moved on); skip past it.
        if let Some(cur) = self.current() {
            if self.history.last() == Some(&cur) {
                self.history.pop();
            }
        }
        if let Some(prev) = self.history.pop() {
            if let Some(pos) = self.queue.iter().position(|t| *t == prev) {
                self.pos = pos;
            }
            self.current_token = None;
            self.current_offset_ms = 0;
            Some(prev)
        } else if let Some(cur) = self.current() {
            self.current_token = None;
            self.current_offset_ms = 0;
            Some(cur)
        } else {
            None
        }
    }

    /// Record `id` as the currently playing track.
    pub fn set_current(&mut self, id: u64, token: String, offset_ms: u64) {
        if !self.queue.contains(&id) {
            self.queue.push(id);
            self.pos = self.queue.len() - 1;
        } else {
            self.pos = self.queue.iter().position(|t| *t == id).unwrap_or(0);
        }
        self.current_token = Some(token);
        self.current_offset_ms = offset_ms;
    }

    /// Mark the current track as finished (for history bookkeeping).
    pub fn note_finished(&mut self) {
        if let Some(cur) = self.current() {
            if self.history.last() != Some(&cur) {
                self.history.push(cur);
                if self.history.len() > 64 {
                    self.history.remove(0);
                }
            }
        }
    }

    pub fn stop(&mut self) {
        self.current_token = None;
        self.current_offset_ms = 0;
    }

    pub fn note_offset(&mut self, offset_ms: i64) {
        if offset_ms >= 0 {
            self.current_offset_ms = offset_ms as u64;
        }
    }

    /// Shuffle the remaining queue (current position kept).
    pub fn shuffle_remaining(&mut self) {
        if self.shuffle && self.queue.len() > 1 {
            let mut rng = rand::rng();
            let mut rest: Vec<u64> = self.queue.drain(self.pos + 1..).collect();
            shuffle_in_place(&mut rest, &mut rng);
            self.queue.extend(rest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PlayerState {
        let mut s = PlayerState::default();
        s.start_queue(vec![1, 2, 3]);
        s
    }

    #[test]
    fn advance_walks_queue_and_stops_at_end() {
        let mut s = state();
        assert_eq!(s.advance(), Some(2));
        assert_eq!(s.advance(), Some(3));
        assert_eq!(s.advance(), None);
    }

    #[test]
    fn repeat_all_wraps() {
        let mut s = state();
        s.repeat = Repeat::All;
        assert_eq!(s.advance(), Some(2));
        assert_eq!(s.advance(), Some(3));
        assert_eq!(s.advance(), Some(1));
    }

    #[test]
    fn repeat_one_replays() {
        let mut s = state();
        s.repeat = Repeat::One;
        assert_eq!(s.advance(), Some(1));
        assert_eq!(s.advance(), Some(1));
    }

    #[test]
    fn go_back_uses_history() {
        let mut s = state();
        s.set_current(1, "t1".into(), 0);
        s.note_finished();
        s.advance();
        s.set_current(2, "t2".into(), 0);
        s.note_finished();
        assert_eq!(s.go_back(), Some(1));
        assert_eq!(s.current(), Some(1));
    }

    #[test]
    fn set_current_inserts_untracked_track() {
        let mut s = state();
        s.set_current(99, "t".into(), 0);
        assert_eq!(s.current(), Some(99));
        assert!(s.queue.contains(&99));
    }
}
