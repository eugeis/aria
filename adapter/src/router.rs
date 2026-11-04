//! Intent router: maps classified ASK intents to audio-engine actions or
//! brain (Aria Mosaic app) turns.

use std::sync::Arc;

use aria_alexa::{Envelope, IntentKind, Response, ResponseBuilder, audio_item};
use aria_audio::{CollectionKind, Library, Repeat, StreamSource, search, slugify};

use crate::agent::{AgentError, ResolveOutcome};
use crate::state::AppState;

/// Entry point for every request from Amazon.
pub async fn handle_alexa(state: &Arc<AppState>, env: Envelope) -> Response {
    let kind = IntentKind::from_envelope(&env);
    let device = env.device_id().unwrap_or("unknown-device").to_string();
    tracing::debug!(device = %device, ?kind, "alexa request");

    match kind {
        IntentKind::Launch => ResponseBuilder::new()
            .say("Aria ist bereit. Sag etwa: spiel Musik, oder frage mich etwas.")
            .end_session()
            .build(),
        IntentKind::SessionEnded | IntentKind::SystemRequest => {
            // Persist on the way out.
            state.save_state();
            Response::empty()
        }
        IntentKind::Help => ResponseBuilder::new()
            .say(
                "Du kannst sagen: spiel einen Song, ein Album, einen Künstler oder ein Genre. \
                 Spiel ein Hörbuch, oder sag weiter plus der Titel, um es fortzusetzen. \
                 Sag Radio plus Sendernamen. Weiter, pausiere, stoppe, nächster Titel, \
                 vorheriger Titel, oder springe zu einer Zeit. Zufall, wiederhole diesen Titel, \
                 oder wiederhole alles. Und du kannst mich einfach etwas fragen — ich antworte.",
            )
            .end_session()
            .build(),
        IntentKind::Cancel => ResponseBuilder::new()
            .say("Bis bald.")
            .end_session()
            .build(),

        IntentKind::PlayMusic { query } => handle_play_music(state, &device, query).await,
        IntentKind::Play => handle_play_noquery(state, &device, &env),
        IntentKind::Pause => {
            state.save_state();
            ResponseBuilder::new()
                .say("Pausiert.")
                .pause_audio()
                .end_session()
                .build()
        }
        IntentKind::Stop => {
            stop_device(state, &device);
            ResponseBuilder::new()
                .say("Gestoppt.")
                .stop_audio()
                .end_session()
                .build()
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
        IntentKind::RepeatOne => set_repeat(state, &device, Repeat::One, "Ich wiederhole diesen Titel."),
        IntentKind::RepeatAll => {
            set_repeat(state, &device, Repeat::All, "Ich wiederhole die ganze Warteschlange.")
        }
        IntentKind::RepeatOff => set_repeat(state, &device, Repeat::Off, "Wiederholung aus."),
        IntentKind::Seek { offset_ms } => handle_seek(state, &device, offset_ms),

        IntentKind::Ask { utterance } => handle_agent_turn(state, &device, &utterance).await,
        IntentKind::Fallback { utterance } => match utterance {
            Some(u) => {
                // Play-like fallback: "mein aria, spiel X" can arrive here
                // when the PlayMusicIntent match loses confidence to the
                // transcript. Route it to the REAL play path instead of
                // letting the RAG LLM talk about the track.
                if let Some(q) = strip_play_prefix(&u) {
                    handle_play_music(state, &device, Some(q)).await
                } else {
                    handle_agent_turn(state, &device, &u).await
                }
            }
            None => ResponseBuilder::new()
                .say("Das habe ich nicht verstanden. Sag frage plus deine Frage, oder spiel etwas.")
                .end_session()
                .build(),
        },
        IntentKind::Unknown { .. } => ResponseBuilder::new()
            .say("Ich kann Musik abspielen oder Fragen beantworten. Sag spiel Musik, oder stell mir eine Frage.")
            .end_session()
            .build(),
    }
}

// ── Brain turns ────────────────────────────────────────────────────────────

async fn handle_agent_turn(state: &Arc<AppState>, device: &str, utterance: &str) -> Response {
    let timeout = state.cfg.turn_timeout();
    match state.agent.ask(device, utterance, timeout).await {
        Ok(outcome) => {
            let text = outcome
                .text
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| {
                    "I got an empty answer. Try asking it a different way.".to_string()
                });
            ResponseBuilder::new().say(&text).end_session().build()
        }
        Err(AgentError::Connect(_)) => ResponseBuilder::new()
            .say("Ich erreiche meinen Dienst gerade nicht. Prüfe, ob Aria läuft.")
            .end_session()
            .build(),
        Err(_) => ResponseBuilder::new()
            .say("Da gab es einen Fehler. Versuch es kurz gleich noch einmal.")
            .end_session()
            .build(),
    }
}

/// Play-like prefixes a Fallback transcript can carry ("spiel X",
/// "spiele X", "mach X an", "leg X auf") — returns the remaining query.
fn strip_play_prefix(u: &str) -> Option<String> {
    let l = u.to_lowercase();
    for p in ["spiele ", "spiel ", "mach ", "leg "] {
        if l.starts_with(p) {
            let q = u[p.len()..].trim();
            if !q.is_empty() {
                return Some(q.to_string());
            }
        }
    }
    None
}

// ── Music: play ────────────────────────────────────────────────────────────

async fn handle_play_music(state: &Arc<AppState>, device: &str, query: Option<String>) -> Response {
    let q = match query {
        Some(q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => return handle_play_noquery(state, device, &no_context_envelope()),
    };
    let q = q.to_ascii_lowercase();

    // 0) "continue <audiobook>" — resume the device's position in the book.
    if let Some(book) = strip_prefixes(
        &q,
        &["continue ", "continue with ", "weiter ", "setze fort "],
    ) {
        if !book.is_empty() {
            if let Some(resp) = continue_book(state, device, &book) {
                return resp;
            }
        }
    }

    // 1) Radio preset?
    if let Some((name, url)) = match_radio(&state.cfg.radio, &q) {
        return start_radio(state, device, url, name);
    }

    // 2) Deterministic resolution over the in-memory library (sync — the
    //    read guard never crosses an await).
    match deterministic_play(state, device, &q) {
        DeterministicPlay::Queue(ids) => return play_queue(state, device, ids),
        DeterministicPlay::Track(id) => return start_file(state, device, id, 0, None),
        DeterministicPlay::Miss => {}
    }

    // 3) Brain fallback: the LLM resolves it against the metadata KB
    //    ("something for working out", "die psalmen", …).
    match state.agent.resolve_play(&q, state.cfg.turn_timeout()).await {
        Ok(outcome) if !outcome.is_none() => {
            tracing::debug!(query = %q, action = %outcome.action, slug = %outcome.slug, reason = %outcome.reason, "brain resolved play");
            match play_by_slug(state, device, &outcome) {
                Some(resp) => resp,
                None => not_found_response(&q),
            }
        }
        Ok(outcome) => {
            tracing::debug!(query = %q, reason = %outcome.reason, "brain found no match");
            not_found_response(&q)
        }
        Err(e) => {
            tracing::warn!(query = %q, %e, "brain resolve-play failed");
            not_found_response(&q)
        }
    }
}

#[derive(Debug)]
enum DeterministicPlay {
    /// A whole queue (album / artist / genre / audiobook).
    Queue(Vec<u64>),
    /// A single track (fuzzy top hit).
    Track(u64),
    Miss,
}

/// The deterministic resolver: audiobook, album, artist, genre, fuzzy — in
/// that order. Synchronous: the library read guard dies inside this fn.
fn deterministic_play(state: &AppState, device: &str, q: &str) -> DeterministicPlay {
    let lib = state.library.read().unwrap();
    // Audiobook by name (whole book, from the start).
    if let Some(book) = fuzzy_book(&lib, q) {
        let ids = lib.chapters_of_book(&book);
        if !ids.is_empty() {
            return DeterministicPlay::Queue(ids);
        }
    }
    // Album by name.
    if let Some(ids) = named_tracks(&lib, |t| !t.album.is_empty() && eq_norm(&t.album, q)) {
        return DeterministicPlay::Queue(ids);
    }
    // Artist by name.
    if let Some(ids) = named_tracks(&lib, |t| !t.artist.is_empty() && eq_norm(&t.artist, q)) {
        return DeterministicPlay::Queue(ids);
    }
    // Genre by name.
    if let Some(ids) = named_tracks(&lib, |t| !t.genre.is_empty() && eq_norm(&t.genre, q)) {
        let ids = if shuffle_of(state, device) {
            shuffled(ids)
        } else {
            ids
        };
        return DeterministicPlay::Queue(ids);
    }
    // Fuzzy search.
    let hits = search(&lib, q, 5);
    match hits.first() {
        Some((id, _)) => DeterministicPlay::Track(*id),
        None => DeterministicPlay::Miss,
    }
}

fn strip_prefixes(q: &str, prefixes: &[&str]) -> Option<String> {
    for p in prefixes {
        if let Some(rest) = q.strip_prefix(p) {
            if !rest.trim().is_empty() {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

/// Book match: exact first, then prefix either way.
fn fuzzy_book(lib: &Library, q: &str) -> Option<String> {
    let books = lib.books();
    if let Some(b) = books.iter().find(|b| eq_norm(b, q)) {
        return Some(b.clone());
    }
    books
        .iter()
        .find(|b| {
            let n = b.to_ascii_lowercase();
            n.starts_with(q) || q.starts_with(&n)
        })
        .cloned()
}

/// "Continue <book>": resume the device's current chapter+offset when it is
/// inside the book; otherwise start at chapter one.
fn continue_book(state: &Arc<AppState>, device: &str, book: &str) -> Option<Response> {
    let needle = book.to_ascii_lowercase();
    let (book_name, chs, resume_id, resume_off) = {
        let lib = state.library.read().unwrap();
        let book_name = lib.books().into_iter().find(|b| {
            let n = b.to_ascii_lowercase();
            n == needle || n.starts_with(&needle) || needle.starts_with(&n)
        })?;
        let chs = lib.chapters_of_book(&book_name);
        if chs.is_empty() {
            return None;
        }
        // Is the device's current track a chapter of this book?
        let current = state.player().get(device).and_then(|p| p.current());
        let (resume_id, resume_off) = current
            .and_then(|id| lib.get(id))
            .filter(|t| t.kind == CollectionKind::Audiobook && t.book == book_name)
            .map(|t| {
                (
                    t.id,
                    state
                        .player()
                        .get(device)
                        .map(|p| p.current_offset_ms)
                        .unwrap_or(0)
                        .min(t.duration_ms.saturating_sub(5000)),
                )
            })
            .unwrap_or((0, 0));
        (book_name, chs, resume_id, resume_off)
    };
    if resume_id != 0 {
        // Re-queue the book from the current chapter, resume the stream.
        let rest = {
            let lib = state.library.read().unwrap();
            lib.chapters_of_book(&book_name)
                .into_iter()
                .skip_while(|id| *id != resume_id)
                .collect::<Vec<_>>()
        };
        {
            let mut players = state.player();
            players
                .entry(device.to_string())
                .or_default()
                .start_queue(rest);
        }
        Some(start_file(state, device, resume_id, resume_off, None))
    } else {
        Some(play_queue(state, device, chs))
    }
}

/// Resolve the brain's slug to a playback action.
#[allow(clippy::question_mark)] // the rad: branch destructures a tuple (no ?-form)
fn play_by_slug(state: &Arc<AppState>, device: &str, outcome: &ResolveOutcome) -> Option<Response> {
    let slug = outcome.slug.trim();
    let lib = state.library.read().unwrap();
    let resp = if let Some(id) = slug
        .strip_prefix("trk:")
        .and_then(|n| n.parse::<u64>().ok())
    {
        Some(start_file(state, device, id, 0, None))
    } else if let Some(id) = slug
        .strip_prefix("serm:")
        .and_then(|n| n.parse::<u64>().ok())
    {
        Some(start_file(state, device, id, 0, None))
    } else if let Some(name) = slug.strip_prefix("alb:") {
        let ids = find_album_by_slug(&lib, name)?;
        Some(play_queue(state, device, ids))
    } else if let Some(name) = slug.strip_prefix("art:") {
        let ids = find_artist_by_slug(&lib, name)?;
        Some(play_queue(state, device, ids))
    } else if let Some(rest) = slug.strip_prefix("abk:") {
        let (name, chapter) = match rest.split_once(":ch:") {
            Some((n, c)) => (n.to_string(), c.parse::<u32>().ok()),
            None => (rest.to_string(), None),
        };
        let book = find_book_by_slug(&lib, &name)?;
        let mut ids = lib.chapters_of_book(&book);
        match chapter {
            Some(n) if (1..=ids.len() as u32).contains(&n) => {
                let rest: Vec<u64> = ids.split_off((n - 1) as usize);
                Some(play_queue(state, device, rest))
            }
            _ => Some(play_queue(state, device, ids)),
        }
    } else if let Some(name) = slug.strip_prefix("rad:") {
        let (station_name, url) = find_radio_by_slug(state, name)?;
        Some(start_radio(state, device, url, station_name))
    } else {
        return None;
    };
    drop(lib);
    resp
}

fn find_album_by_slug(lib: &Library, slug: &str) -> Option<Vec<u64>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut names: Vec<&str> = Vec::new();
    for t in lib.iter() {
        if t.album.is_empty() {
            continue;
        }
        if slugify(&t.album) == slug && seen.insert(t.album.clone()) {
            names.push(&t.album);
        }
    }
    names.first().and_then(|n| {
        let ids = lib.tracks_in_album(n);
        (!ids.is_empty()).then_some(ids)
    })
}

fn find_artist_by_slug(lib: &Library, slug: &str) -> Option<Vec<u64>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut names: Vec<&str> = Vec::new();
    for t in lib.iter() {
        if t.artist.is_empty() {
            continue;
        }
        if slugify(&t.artist) == slug && seen.insert(t.artist.clone()) {
            names.push(&t.artist);
        }
    }
    names.first().and_then(|n| {
        let ids = lib.tracks_by_artist(n);
        (!ids.is_empty()).then_some(ids)
    })
}

fn find_book_by_slug(lib: &Library, slug: &str) -> Option<String> {
    lib.books().into_iter().find(|b| slugify(b) == slug)
}

fn find_radio_by_slug(state: &AppState, slug: &str) -> Option<(String, String)> {
    for (name, url) in &state.cfg.radio.stations {
        if slugify(name) == slug {
            return Some((name.clone(), url.clone()));
        }
    }
    None
}

fn not_found_response(q: &str) -> Response {
    ResponseBuilder::new()
        .say(format!("Leider habe ich {q} nicht in deinen Sammlungen gefunden."))
        .end_session()
        .build()
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
                .say("Nichts zum Abspielen.")
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
            .say("Den Titel konnte ich nicht finden.")
            .end_session()
            .build();
    };
    if !track.playable {
        return ResponseBuilder::new()
            .say(format!(
                "Leider: {} — der Echo kann das Format {} nicht abspielen. MP3 und AAC funktionieren.",
                track.title,
                track.format_str()
            ))
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
        (!artist.trim().is_empty()).then_some(artist.as_str()),
        Some(art),
    );
    let mut b = ResponseBuilder::new();
    b.say(if artist.trim().is_empty() {
        format!("Ich spiele {title}.")
    } else {
        format!("Ich spiele {title} von {artist}.")
    })
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
            .say("Der Stream ist abgelaufen. Sag spiel, um neu zu starten.")
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
        .say(format!("Ich spiele {name}."))
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
                "Deine Musiksammlung ist leer. Lege MP3- oder AAC-Dateien in die \
                 Bibliotheksordner, und sag dann, ich soll etwas abspielen.",
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
            .say("Das war der letzte in der Warteschlange.")
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
            .say("Es spielt gerade nichts.")
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
                "Die Wiedergabe ist fehlgeschlagen und die Warteschlange ist leer."
            } else {
                "Das war der letzte in der Warteschlange."
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
                "Zufall an."
            } else {
                "Zufall aus."
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
            .say("Diese Zeit habe ich nicht verstanden. Sag zum Beispiel: springe zu 2 Minuten 30 Sekunden.")
            .end_session()
            .build();
    };
    if off < 0 {
        return ResponseBuilder::new()
            .say("Ich kann nur vorwärts spulen.")
            .end_session()
            .build();
    }
    let (track_id, duration_ms) = {
        let players = state.player();
        let Some(p) = players.get(device) else {
            return ResponseBuilder::new()
                .say("Es spielt gerade nichts.")
                .end_session()
                .build();
        };
        let Some(id) = p.current() else {
            return ResponseBuilder::new()
                .say("Es spielt gerade nichts.")
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

    /// A throwaway AppState with a scanned library.
    fn state_with(
        dir: &std::path::Path,
        items: Vec<aria_audio::CollectionItem>,
    ) -> (Arc<AppState>, std::path::PathBuf) {
        let mut cfg = crate::config::Config::default();
        let data = dir.join("state");
        std::fs::create_dir_all(&data).unwrap();
        cfg.server.data_dir = Some(data.clone());
        cfg.collections.items = items
            .iter()
            .map(|i| crate::config::CollectionEntry {
                name: i.name.clone(),
                kind: match i.kind {
                    aria_audio::CollectionKind::Music => "music".into(),
                    aria_audio::CollectionKind::Audiobook => "audiobook".into(),
                    aria_audio::CollectionKind::Sermon => "sermon".into(),
                },
                path: i.path.clone(),
            })
            .collect();
        let lib = aria_audio::Library::scan_collections(&items).unwrap();
        let st = Arc::new(AppState::new(
            cfg,
            lib,
            Arc::new(crate::agent::MockAgent::new()),
        ));
        (st, data)
    }

    fn write_music(dir: &std::path::Path) -> std::path::PathBuf {
        let root = dir.join("rock");
        std::fs::create_dir_all(root.join("Queen/News of the World")).unwrap();
        std::fs::write(
            root.join("Queen/News of the World/01 - We Will Rock You.mp3"),
            b"\xff\xfb",
        )
        .unwrap();
        std::fs::write(
            root.join("Queen/News of the World/02 - Love of My Life.mp3"),
            b"\xff\xfb",
        )
        .unwrap();
        root
    }

    #[test]
    fn deterministic_play_album_genre_fuzzy() {
        let dir = std::env::temp_dir().join(format!("aria-router-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = write_music(&dir);
        let (st, data) = state_with(
            &dir,
            vec![aria_audio::CollectionItem {
                name: "rock".into(),
                kind: aria_audio::CollectionKind::Music,
                path: root,
            }],
        );
        let q = |s: &str| s.to_ascii_lowercase();
        // Album exact.
        match deterministic_play(&st, "dev", &q("news of the world")) {
            DeterministicPlay::Queue(ids) => assert_eq!(ids.len(), 2),
            other => panic!("expected album queue, got {other:?}"),
        }
        // Artist exact.
        match deterministic_play(&st, "dev", &q("queen")) {
            DeterministicPlay::Queue(ids) => assert_eq!(ids.len(), 2),
            other => panic!("expected artist queue, got {other:?}"),
        }
        // Fuzzy single track.
        match deterministic_play(&st, "dev", &q("will rock you")) {
            DeterministicPlay::Track(_) => {}
            other => panic!("expected a track, got {other:?}"),
        }
        // Miss.
        assert!(matches!(
            deterministic_play(&st, "dev", &q("zzz nothing zzz")),
            DeterministicPlay::Miss
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn play_by_slug_track_album_audiobook() {
        let dir = std::env::temp_dir().join(format!("aria-router-slug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let music = write_music(&dir);
        let ab = dir.join("bible-audio");
        std::fs::create_dir_all(ab.join("Psalmen")).unwrap();
        std::fs::write(ab.join("Psalmen/001 - Psalm 1.mp3"), b"\xff\xfb").unwrap();
        std::fs::write(ab.join("Psalmen/002 - Psalm 2.mp3"), b"\xff\xfb").unwrap();
        let (st, data) = state_with(
            &dir,
            vec![
                aria_audio::CollectionItem {
                    name: "rock".into(),
                    kind: aria_audio::CollectionKind::Music,
                    path: music.clone(),
                },
                aria_audio::CollectionItem {
                    name: "bible-audio".into(),
                    kind: aria_audio::CollectionKind::Audiobook,
                    path: ab,
                },
            ],
        );
        let any_track_id = st
            .library
            .read()
            .unwrap()
            .iter()
            .find(|t| t.kind == aria_audio::CollectionKind::Music)
            .unwrap()
            .id;
        let book = st.library.read().unwrap().chapters_of_book("Psalmen");
        assert_eq!(book.len(), 2);
        // trk slug.
        let out = ResolveOutcome {
            action: "track".into(),
            slug: format!("trk:{:04}", any_track_id),
            reason: String::new(),
        };
        assert!(play_by_slug(&st, "dev", &out).is_some());
        // album slug.
        let out = ResolveOutcome {
            action: "album".into(),
            slug: "alb:news-of-the-world".into(),
            reason: String::new(),
        };
        assert!(play_by_slug(&st, "dev", &out).is_some());
        // whole audiobook.
        let out = ResolveOutcome {
            action: "audiobook".into(),
            slug: "abk:psalmen".into(),
            reason: String::new(),
        };
        assert!(play_by_slug(&st, "dev", &out).is_some());
        // unknown slug.
        let out = ResolveOutcome {
            action: "track".into(),
            slug: "trk:9999".into(),
            reason: String::new(),
        };
        // trk:9999 -> start_file with a missing id => a (valid) "not found" response.
        assert!(play_by_slug(&st, "dev", &out).is_some());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn continue_book_starts_from_chapter_one() {
        let dir = std::env::temp_dir().join(format!("aria-router-ab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ab = dir.join("bible-audio");
        std::fs::create_dir_all(ab.join("Psalmen")).unwrap();
        std::fs::write(ab.join("Psalmen/001 - Psalm 1.mp3"), b"\xff\xfb").unwrap();
        std::fs::write(ab.join("Psalmen/002 - Psalm 2.mp3"), b"\xff\xfb").unwrap();
        let (st, data) = state_with(
            &dir,
            vec![aria_audio::CollectionItem {
                name: "bible-audio".into(),
                kind: aria_audio::CollectionKind::Audiobook,
                path: ab,
            }],
        );
        let resp = continue_book(&st, "dev", "psalmen");
        assert!(resp.is_some(), "continue psalmen should start the book");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }
}
