//! Intent router: maps classified ASK intents to audio-engine actions or
//! ZeroClaw agent turns.

use std::time::Instant;

use aria_alexa::{Envelope, IntentKind, Response, ResponseBuilder, audio_item};
use aria_audio::{Library, Repeat, StreamSource, search};
use aria_zeroclaw::{AgentError, Decision};
use std::sync::Arc;

use crate::state::{AppState, PendingApproval};

/// Entry point for every request from Amazon.
pub async fn handle_alexa(state: &Arc<AppState>, env: Envelope) -> Response {
    let kind = IntentKind::from_envelope(&env);
    let device = env.device_id().unwrap_or("unknown-device").to_string();
    tracing::debug!(device = %device, ?kind, "alexa request");

    match kind {
        IntentKind::Launch => ResponseBuilder::new()
            .say("Aria is ready. Say play some music, or ask me anything.")
            .end_session()
            .build(),
        IntentKind::SessionEnded | IntentKind::SystemRequest => {
            // Persist on the way out.
            state.save_state();
            Response::empty()
        }
        IntentKind::Help => ResponseBuilder::new()
            .say(
                "You can say: play a song, album, artist, or genre. Play radio plus a station name. \
                 Play, pause, stop, next, previous, or jump to a time. Shuffle, repeat this song, or \
                 repeat all. And you can just ask me a question — ZeroClaw will answer.",
            )
            .end_session()
            .build(),
        IntentKind::Cancel => ResponseBuilder::new().say("Okay, bye.").end_session().build(),

        IntentKind::PlayMusic { query } => handle_play_music(state, &device, query).await,
        IntentKind::Play => handle_play_noquery(state, &device, &env),
        IntentKind::Pause => {
            state.save_state();
            ResponseBuilder::new().say("Paused.").pause_audio().end_session().build()
        }
        IntentKind::Stop => {
            stop_device(state, &device);
            ResponseBuilder::new().say("Stopped.").stop_audio().end_session().build()
        }
        IntentKind::Next => handle_next(state, &device, &env).await,
        IntentKind::Previous => handle_previous(state, &device, &env),
        IntentKind::Resume { offset_ms } => handle_resume(state, &device, offset_ms),

        IntentKind::PlaybackStarted => handle_playback_started(state, &device, &env),
        IntentKind::PlaybackStopped => {
            if let Some(off) = env.audio_player().and_then(|a| a.offset_in_milliseconds) {
                note_offset(state, &device, off);
            }
            Response::empty()
        }
        IntentKind::PlaybackNearlyFinished => handle_advance(state, &device, &env, false).await,
        IntentKind::PlaybackFinished => handle_advance(state, &device, &env, true).await,
        IntentKind::PlaybackFailed => handle_advance(state, &device, &env, true).await,

        IntentKind::ToggleShuffle => handle_shuffle(state, &device),
        IntentKind::RepeatOne => set_repeat(state, &device, Repeat::One, "Repeating this song."),
        IntentKind::RepeatAll => set_repeat(state, &device, Repeat::All, "Repeating the whole queue."),
        IntentKind::RepeatOff => set_repeat(state, &device, Repeat::Off, "Repeat off."),
        IntentKind::Seek { offset_ms } => handle_seek(state, &device, offset_ms),

        IntentKind::Ask { utterance } => handle_agent_turn(state, &device, &utterance).await,
        IntentKind::Fallback { utterance } => match utterance {
            Some(u) => handle_agent_turn(state, &device, &u).await,
            None => ResponseBuilder::new()
                .say("I didn't catch that. Say ask aria plus your question, or play some music.")
                .end_session()
                .build(),
        },
        IntentKind::Unknown { .. } => ResponseBuilder::new()
            .say("I can play music or answer questions. Try play some music, or ask me something.")
            .end_session()
            .build(),
    }
}

// ── Agent turns ────────────────────────────────────────────────────────────

async fn handle_agent_turn(state: &Arc<AppState>, device: &str, utterance: &str) -> Response {
    // A late reply from the previous turn is spoken first.
    let cached = state.take_cached_reply(device);

    // Pending tool approval? Yes/no handles it; anything else reminds.
    if let Some(p) = state.pending_approval(device) {
        match parse_decision(utterance) {
            Some(d) => {
                let ok = state
                    .agent
                    .respond_approval(device, &p.info.request_id, d)
                    .is_ok();
                state.set_pending_approval(device, None);
                let msg = match (d, ok) {
                    (Decision::Deny, _) => "Denied.",
                    (_, true) => "Approved. I'll tell you when it's done.",
                    (_, false) => {
                        "I couldn't forward that approval. The agent will decide on its own timeout."
                    }
                };
                return ResponseBuilder::new().say(msg).end_session().build();
            }
            None => {
                return ResponseBuilder::new()
                    .say(format!(
                        "ZeroClaw is waiting for your approval: {}. Say yes to allow it, or no to deny it.",
                        approval_summary(&p.info),
                    ))
                    .end_session()
                    .build();
            }
        }
    }

    let timeout = state.cfg.turn_timeout();
    match state.agent.ask(device, utterance, timeout).await {
        Ok(outcome) => {
            if let Some(approval) = outcome.awaiting_approval {
                // The event collector may already have registered this; keep
                // the newest.
                state.set_pending_approval(
                    device,
                    Some(PendingApproval {
                        info: approval.clone(),
                        asked_at: Instant::now(),
                    }),
                );
                return ResponseBuilder::new()
                    .say(format!(
                        "ZeroClaw needs your approval: {}. Say yes to allow it, or no to deny it.",
                        approval_summary(&approval),
                    ))
                    .end_session()
                    .build();
            }
            let base = outcome.full.clone().unwrap_or_else(|| {
                if outcome.partial.trim().is_empty() {
                    "That's taking longer than I can say in one go. Try again in a moment."
                        .to_string()
                } else {
                    // Timeout with partial text: speak what we have.
                    outcome.partial.clone()
                }
            });
            let text = match cached {
                Some(c) if !c.trim().is_empty() => format!("{c} {base}"),
                _ => base,
            };
            ResponseBuilder::new().say(&text).end_session().build()
        }
        Err(AgentError::Busy) => ResponseBuilder::new()
            .say("I'm still working on the previous request. Give me a moment, then try again.")
            .end_session()
            .build(),
        Err(AgentError::Connect(_)) => ResponseBuilder::new()
            .say("I can't reach the ZeroClaw agent right now. Check that it is running.")
            .end_session()
            .build(),
        Err(_) => ResponseBuilder::new()
            .say("The agent hit an error. Try again in a moment.")
            .end_session()
            .build(),
    }
}

/// Classify a yes/no/always answer to an approval prompt.
pub fn parse_decision(utterance: &str) -> Option<Decision> {
    let u = utterance.trim().to_ascii_lowercase();
    let words: Vec<&str> = u.split_whitespace().collect();
    let last = words.last().copied().unwrap_or("");
    let first = words.first().copied().unwrap_or("");
    if u.contains("always") {
        return Some(Decision::Always);
    }
    let positive = [
        "yes", "yep", "yeah", "yup", "sure", "ok", "okay", "allow", "approve", "go", "ja", "bitte",
        "please",
    ];
    let negative = [
        "no", "nope", "nah", "deny", "denied", "negative", "nein", "stop",
    ];
    if positive.contains(&last) || positive.contains(&first) && words.len() <= 3 {
        return Some(Decision::Approve);
    }
    if negative.contains(&last) || negative.contains(&first) && words.len() <= 3 {
        return Some(Decision::Deny);
    }
    None
}

fn approval_summary(info: &aria_zeroclaw::ApprovalInfo) -> String {
    let base = if info.summary.trim().is_empty() {
        format!("the {} tool wants to run", info.tool)
    } else {
        format!("the {} tool: {}", info.tool, info.summary)
    };
    truncate_at_word(&base, 160)
}

fn truncate_at_word(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    match cut.rfind(' ') {
        Some(p) => cut[..p].to_string(),
        None => cut,
    }
}

// ── Music: play ────────────────────────────────────────────────────────────

async fn handle_play_music(state: &Arc<AppState>, device: &str, query: Option<String>) -> Response {
    let q = match query {
        Some(q) if !q.trim().is_empty() => q.trim().to_ascii_lowercase(),
        _ => return handle_play_noquery(state, device, &no_context_envelope()),
    };

    // 1) Radio preset?
    if let Some((name, url)) = match_radio(&state.cfg.radio, &q) {
        return start_radio(state, device, url, name);
    }

    let lib = state.library.read().unwrap();
    // 2) Album by name.
    if let Some(ids) = named_tracks(&lib, |t| !t.album.is_empty() && eq_norm(&t.album, &q)) {
        return play_queue(state, device, ids);
    }
    // 3) Artist by name.
    if let Some(ids) = named_tracks(&lib, |t| !t.artist.is_empty() && eq_norm(&t.artist, &q)) {
        return play_queue(state, device, ids);
    }
    // 4) Genre by name.
    if let Some(ids) = named_tracks(&lib, |t| !t.genre.is_empty() && eq_norm(&t.genre, &q)) {
        let ids = if shuffle_of(state, device) {
            shuffled(ids)
        } else {
            ids
        };
        return play_queue(state, device, ids);
    }
    // 5) Fuzzy search.
    let hits = search(&lib, &q, 5);
    drop(lib);
    match hits.first() {
        Some((id, _)) => start_file(state, device, *id, 0, None),
        None => ResponseBuilder::new()
            .say(format!("Sorry, I couldn't find {q} in your music library."))
            .end_session()
            .build(),
    }
}

fn no_context_envelope() -> Envelope {
    serde_json::from_str(
        r#"{"version":"1.0","request":{"type":"IntentRequest","requestId":"noop","intent":{"name":"PlayIntent"}}}"#,
    )
    .expect("static envelope")
}

fn eq_norm(a: &str, b: &str) -> bool {
    a.to_ascii_lowercase() == b.trim()
}

fn named_tracks<F: Fn(&aria_audio::Track) -> bool>(lib: &Library, pred: F) -> Option<Vec<u64>> {
    let ids: Vec<u64> = lib
        .iter()
        .filter(|t| t.playable && pred(t))
        .map(|t| t.id)
        .collect();
    (!ids.is_empty()).then_some(ids)
}

fn shuffle_of(state: &AppState, device: &str) -> bool {
    state
        .player()
        .get(device)
        .map(|p| p.shuffle)
        .unwrap_or(false)
}

fn shuffled(mut ids: Vec<u64>) -> Vec<u64> {
    if ids.len() > 1 {
        let mut rng = rand::rng();
        aria_audio::library::shuffle_in_place(&mut ids, &mut rng);
    }
    ids
}

/// Start a fresh queue and play its first entry.
fn play_queue(state: &Arc<AppState>, device: &str, ids: Vec<u64>) -> Response {
    let first = match ids.first().copied() {
        Some(id) => id,
        None => {
            return ResponseBuilder::new()
                .say("Nothing to play.")
                .end_session()
                .build();
        }
    };
    {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.start_queue(ids);
    }
    start_file(state, device, first, 0, None)
}

/// Issue a token, update player state, and build the Play response.
pub fn start_file(
    state: &Arc<AppState>,
    device: &str,
    track_id: u64,
    offset_ms: u64,
    expected: Option<(&str, u64)>,
) -> Response {
    let lib = state.library.read().unwrap();
    let Some(track) = lib.get(track_id) else {
        return ResponseBuilder::new()
            .say("I couldn't find that track.")
            .end_session()
            .build();
    };
    if !track.playable {
        return ResponseBuilder::new()
            .say("That file is not in a format Echo can stream. MP3 and AAC work.")
            .end_session()
            .build();
    }
    let (title, artist, path, format, bitrate) = (
        track.title.clone(),
        track.artist.clone(),
        track.path.clone(),
        track.format,
        track.avg_bitrate_kbps,
    );
    drop(lib);

    let token = state.registry.issue(
        device,
        StreamSource::File {
            track_id,
            path,
            format,
            offset_ms,
            avg_bitrate_kbps: bitrate,
            title: title.clone(),
            artist: artist.clone(),
        },
    );
    {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        // Remember the outgoing track so "previous" can find it.
        if p.current() != Some(track_id) {
            p.note_finished();
        }
        p.set_current(track_id, token.clone(), offset_ms);
    }
    state.save_state();

    let art = state.art_url(track_id);
    let item = audio_item(
        state.cfg.base_url(),
        &token,
        offset_ms,
        expected,
        &title,
        Some(&artist),
        Some(art),
    );
    let mut b = ResponseBuilder::new();
    b.say(format!("Playing {title} by {artist}."))
        .play_audio(item)
        .end_session();
    b.build()
}

/// Rebuild the same stream (resume/seek): same token, explicit offset.
fn resume_stream(
    state: &Arc<AppState>,
    device: &str,
    token: &str,
    offset_ms: u64,
    expected: Option<(&str, u64)>,
) -> Response {
    let Some(resolved) = state.registry.resolve(token) else {
        return ResponseBuilder::new()
            .say("That stream has expired. Say play to start again.")
            .end_session()
            .build();
    };
    let (title, artist) = match &resolved.source {
        StreamSource::File { title, artist, .. } => (title.clone(), artist.clone()),
        StreamSource::Radio { title, .. } => (title.clone(), String::new()),
    };
    let art = match &resolved.source {
        StreamSource::File { track_id, .. } => state.art_url(*track_id),
        StreamSource::Radio { .. } => String::new(),
    };
    {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.note_offset(offset_ms as i64);
        if p.current_token.as_deref() != Some(token) {
            // The state and device drifted; resync token binding.
            if let StreamSource::File { track_id, .. } = &resolved.source {
                p.set_current(*track_id, token.to_string(), offset_ms);
            }
        }
    }
    let item = audio_item(
        state.cfg.base_url(),
        token,
        offset_ms,
        expected,
        &title,
        (!artist.is_empty()).then_some(artist.as_str()),
        Some(art),
    );
    ResponseBuilder::new()
        .play_audio(item)
        .end_session()
        .build()
}

fn start_radio(state: &Arc<AppState>, device: &str, url: String, name: String) -> Response {
    let token = state.registry.issue(
        device,
        StreamSource::Radio {
            url,
            title: name.clone(),
        },
    );
    {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.start_queue(Vec::new());
        p.current_token = Some(token.clone());
        p.current_offset_ms = 0;
    }
    let item = audio_item(
        state.cfg.base_url(),
        &token,
        0,
        None,
        &name,
        Some("Radio"),
        None,
    );
    ResponseBuilder::new()
        .say(format!("Playing {name}."))
        .play_audio(item)
        .end_session()
        .build()
}

/// "Alexa, play" (no query).
fn handle_play_noquery(state: &Arc<AppState>, device: &str, env: &Envelope) -> Response {
    let ap = env.audio_player();
    // Same-token resume (device paused/stopped, user says play).
    if let Some(ap) = ap {
        if let (Some(tok), Some(off)) = (ap.token.as_deref(), ap.offset_in_milliseconds) {
            if state
                .player()
                .get(device)
                .and_then(|p| p.current_token.as_deref())
                == Some(tok)
            {
                let expected = ap_context_expected(ap);
                return resume_stream(state, device, tok, off.max(0) as u64, expected);
            }
        }
    }
    let (resume_id, resume_off) = {
        let players = state.player();
        match players.get(device) {
            Some(p) if !p.is_idle() => (p.current(), p.current_offset_ms),
            Some(p) if !p.queue.is_empty() => (Some(*p.queue.first().unwrap()), 0),
            _ => (None, 0),
        }
    };
    if let Some(id) = resume_id {
        let expected = ap.and_then(ap_context_expected);
        return start_file(state, device, id, resume_off, expected);
    }
    // Nothing queued: start the whole library.
    let shuffle = shuffle_of(state, device);
    let ids = {
        let lib = state.library.read().unwrap();
        lib.all_ids(shuffle)
    };
    match ids.first().copied() {
        Some(first) => {
            {
                let mut players = state.player();
                players.entry(device.to_string()).or_default().start_queue(ids);
            }
            start_file(state, device, first, 0, None)
        }
        None => ResponseBuilder::new()
            .say(
                "Your music library is empty. Add MP3 or AAC files to the library folders, then ask me to play.",
            )
            .end_session()
            .build(),
    }
}

/// (token, offset) from AudioPlayer context, for expected* stream fields.
fn ap_context_expected(ap: &aria_alexa::request::AudioPlayerContext) -> Option<(&str, u64)> {
    ap.token
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| (t, ap.offset_in_milliseconds.unwrap_or(0).max(0) as u64))
}

async fn handle_next(state: &Arc<AppState>, device: &str, env: &Envelope) -> Response {
    let expected = env.audio_player().and_then(ap_context_expected);
    let next = {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.note_finished();
        p.advance()
    };
    match next {
        Some(id) => start_file(state, device, id, 0, expected),
        None => ResponseBuilder::new()
            .say("That was the last one in the queue.")
            .end_session()
            .build(),
    }
}

fn handle_previous(state: &Arc<AppState>, device: &str, env: &Envelope) -> Response {
    let expected = env.audio_player().and_then(ap_context_expected);
    let prev = {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.go_back()
    };
    match prev {
        Some(id) => start_file(state, device, id, 0, expected),
        None => ResponseBuilder::new()
            .say("Nothing is playing.")
            .end_session()
            .build(),
    }
}

fn handle_resume(state: &Arc<AppState>, device: &str, offset_ms: i64) -> Response {
    let token = state
        .player()
        .get(device)
        .and_then(|p| p.current_token.clone())
        .unwrap_or_default();
    if token.is_empty() {
        return handle_play_noquery(state, device, &no_context_envelope());
    }
    resume_stream(state, device, &token, offset_ms.max(0) as u64, None)
}

fn stop_device(state: &Arc<AppState>, device: &str) {
    let mut players = state.player();
    let p = players.entry(device.to_string()).or_default();
    p.note_finished();
    p.stop();
}

fn note_offset(state: &Arc<AppState>, device: &str, offset_ms: i64) {
    let mut players = state.player();
    let p = players.entry(device.to_string()).or_default();
    p.note_offset(offset_ms);
}

fn handle_playback_started(state: &Arc<AppState>, device: &str, env: &Envelope) -> Response {
    if let Some(ap) = env.audio_player() {
        if let (Some(tok), Some(off)) = (ap.token.as_deref(), ap.offset_in_milliseconds) {
            let mut players = state.player();
            let p = players.entry(device.to_string()).or_default();
            p.current_token = Some(tok.to_string());
            p.current_offset_ms = off.max(0) as u64;
        }
    }
    state.save_state();
    Response::empty()
}

/// Device signalled the current track nearly finished / finished / failed:
/// advance the queue and hand over the next stream.
async fn handle_advance(
    state: &Arc<AppState>,
    device: &str,
    env: &Envelope,
    failed: bool,
) -> Response {
    let expected = env.audio_player().and_then(ap_context_expected);
    let next = {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.note_finished();
        p.advance()
    };
    match next {
        Some(id) => start_file(state, device, id, 0, expected),
        None => ResponseBuilder::new()
            .say(if failed {
                "Playback failed and the queue is empty."
            } else {
                "That was the last one in the queue."
            })
            .end_session()
            .build(),
    }
}

fn handle_shuffle(state: &Arc<AppState>, device: &str) -> Response {
    let (on, msg) = {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.shuffle = !p.shuffle;
        if p.shuffle {
            p.shuffle_remaining();
        }
        (
            p.shuffle,
            if p.shuffle {
                "Shuffle on."
            } else {
                "Shuffle off."
            },
        )
    };
    let _ = on;
    ResponseBuilder::new().say(msg).end_session().build()
}

fn set_repeat(state: &Arc<AppState>, device: &str, repeat: Repeat, msg: &str) -> Response {
    {
        let mut players = state.player();
        let p = players.entry(device.to_string()).or_default();
        p.repeat = repeat;
    }
    ResponseBuilder::new().say(msg).end_session().build()
}

fn handle_seek(state: &Arc<AppState>, device: &str, offset_ms: Option<i64>) -> Response {
    let Some(off) = offset_ms else {
        return ResponseBuilder::new()
            .say("I couldn't understand that time. Try jump to 2 minutes 30 seconds.")
            .end_session()
            .build();
    };
    if off < 0 {
        return ResponseBuilder::new()
            .say("I can only seek forward.")
            .end_session()
            .build();
    }
    let (track_id, duration_ms) = {
        let players = state.player();
        let Some(p) = players.get(device) else {
            return ResponseBuilder::new()
                .say("Nothing is playing.")
                .end_session()
                .build();
        };
        let Some(id) = p.current() else {
            return ResponseBuilder::new()
                .say("Nothing is playing.")
                .end_session()
                .build();
        };
        drop(players);
        let lib = state.library.read().unwrap();
        let d = lib.get(id).map(|t| t.duration_ms);
        (id, d)
    };
    let off = match duration_ms {
        Some(d) if off as u64 >= d => 0,
        _ => off,
    };
    start_file(state, device, track_id, off as u64, None)
}

// ── Radio matching ─────────────────────────────────────────────────────────

fn match_radio(radio: &crate::config::RadioConfig, q: &str) -> Option<(String, String)> {
    let q = q.trim();
    if q.len() < 3 {
        return None;
    }
    for (name, url) in &radio.stations {
        if q == name {
            return Some((name.clone(), url.clone()));
        }
    }
    for (name, url) in &radio.stations {
        let n = name.to_ascii_lowercase();
        if n.starts_with(q) || q.starts_with(&n) {
            return Some((name.clone(), url.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_alexa::intent::IntentKind;

    fn env(json: &str) -> Envelope {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn decision_phrases() {
        assert_eq!(parse_decision("yes"), Some(Decision::Approve));
        assert_eq!(parse_decision("Yes please"), Some(Decision::Approve));
        assert_eq!(parse_decision("approve"), Some(Decision::Approve));
        assert_eq!(parse_decision("no"), Some(Decision::Deny));
        assert_eq!(parse_decision("no thanks"), Some(Decision::Deny));
        assert_eq!(parse_decision("yes, but always"), Some(Decision::Always));
        assert_eq!(parse_decision("what is the weather"), None);
    }

    #[test]
    fn radio_matching() {
        let mut radio = crate::config::RadioConfig::default();
        radio
            .stations
            .insert("rock fm".into(), "https://r.example/rock.mp3".into());
        assert_eq!(
            match_radio(&radio, "rock fm").map(|(n, _)| n),
            Some("rock fm".into())
        );
        assert_eq!(
            match_radio(&radio, "rock").map(|(n, _)| n),
            Some("rock fm".into())
        );
        assert_eq!(match_radio(&radio, "play something new"), None);
        assert_eq!(match_radio(&radio, "r"), None);
    }

    #[test]
    fn intent_from_json() {
        let e = env(r#"{"version":"1.0","request":{"type":"LaunchRequest","requestId":"r"}}"#);
        assert_eq!(IntentKind::from_envelope(&e), IntentKind::Launch);
    }
}
