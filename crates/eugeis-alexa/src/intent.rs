//! Classification of an ASK request into a concrete intent the router handles.

use crate::request::Envelope;

/// Everything the eugeis router can do, derived from the raw ASK request.
#[derive(Debug, Clone, PartialEq)]
pub enum IntentKind {
    /// "Alexa, open eugeis"
    Launch,
    /// User said stop/cancel or the session ended.
    SessionEnded,
    SystemRequest,
    Help,
    Cancel,

    /// Anything not matched by a skill intent; `utterance` is the transcript
    /// when ASK provides it (slot "utterance"), else `None`.
    Fallback {
        utterance: Option<String>,
    },
    /// Explicit "ask eugeis <question>" style request.
    Ask {
        utterance: String,
    },
    /// "play <query>" — song, artist, album, genre, or radio station.
    PlayMusic {
        query: Option<String>,
    },
    /// Bare "play" / "resume" (PlaybackController).
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Resume with offset reported by the device.
    Resume {
        offset_ms: i64,
    },

    /// Device-originated player reports.
    PlaybackStarted,
    PlaybackStopped,
    PlaybackNearlyFinished,
    PlaybackFailed,
    PlaybackFinished,

    ToggleShuffle,
    RepeatOne,
    RepeatAll,
    RepeatOff,
    Seek {
        offset_ms: Option<i64>,
    },
    /// Recognized intent name the router does not model.
    Unknown {
        name: String,
    },
}

impl IntentKind {
    pub fn from_envelope(env: &Envelope) -> Self {
        let rtype = env.request.request_type.as_str();
        match rtype {
            "LaunchRequest" => return IntentKind::Launch,
            "SessionEndedRequest" => return IntentKind::SessionEnded,
            "SystemRequest" => return IntentKind::SystemRequest,
            "IntentRequest" => {}
            other => {
                return IntentKind::Unknown {
                    name: other.to_string(),
                };
            }
        }

        let name = env
            .intent_name()
            .map(str::to_string)
            .unwrap_or_else(|| "Unknown".to_string());
        match name.as_str() {
            // PlaybackController intents (no utterances; recognized by name).
            "PlayIntent" => IntentKind::Play,
            "PauseIntent" => IntentKind::Pause,
            "StopIntent" => IntentKind::Stop,
            "NextIntent" => IntentKind::Next,
            "PreviousIntent" => IntentKind::Previous,
            "ResumeIntent" => IntentKind::Resume {
                offset_ms: env
                    .audio_player()
                    .and_then(|a| a.offset_in_milliseconds)
                    .unwrap_or(0),
            },
            // PlaybackNotifier requests (device -> skill).
            "PlaybackStartedRequest" => IntentKind::PlaybackStarted,
            "PlaybackStoppedRequest" => IntentKind::PlaybackStopped,
            "PlaybackNearlyFinishedRequest" => IntentKind::PlaybackNearlyFinished,
            "PlaybackFailedRequest" => IntentKind::PlaybackFailed,
            "PlaybackFinishedRequest" => IntentKind::PlaybackFinished,
            // AMAZON built-ins.
            "AMAZON.HelpIntent" => IntentKind::Help,
            "AMAZON.CancelIntent" => IntentKind::Cancel,
            "AMAZON.StopIntent" => IntentKind::Stop,
            "AMAZON.NextIntent" => IntentKind::Next,
            "AMAZON.PreviousIntent" => IntentKind::Previous,
            "AMAZON.FallbackIntent" => IntentKind::Fallback {
                utterance: env.slot("utterance").map(str::to_string),
            },
            // eugeis custom intents.
            "PlayMusicIntent" => IntentKind::PlayMusic {
                query: env.slot("query").map(str::to_string),
            },
            "AskIntent" => match env.slot("utterance") {
                Some(u) => IntentKind::Ask {
                    utterance: u.to_string(),
                },
                None => IntentKind::Fallback { utterance: None },
            },
            "ToggleShuffleIntent" => IntentKind::ToggleShuffle,
            "RepeatOneIntent" => IntentKind::RepeatOne,
            "RepeatAllIntent" => IntentKind::RepeatAll,
            "RepeatOffIntent" => IntentKind::RepeatOff,
            "SeekIntent" => IntentKind::Seek {
                offset_ms: env.slot("time").and_then(parse_time_slot).or_else(|| {
                    env.slot_values("time")
                        .iter()
                        .find_map(|v| parse_time_slot(v))
                }),
            },
            other => IntentKind::Unknown {
                name: other.to_string(),
            },
        }
    }

    /// Transcript of a user question addressed to the agent, if any.
    pub fn agent_utterance(&self) -> Option<&str> {
        match self {
            IntentKind::Ask { utterance } => Some(utterance),
            IntentKind::Fallback { utterance } => utterance.as_deref(),
            _ => None,
        }
    }
}

/// Parse an AMAZON.TIME slot value into milliseconds.
///
/// Handles ISO-8601 durations ("PT2M30S", "PT30S", "PT1H2M"), "MM:SS",
/// "H:MM:SS", and bare minutes ("3").
pub fn parse_time_slot(value: &str) -> Option<i64> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    // ISO 8601 duration: PT[nH][nM][nS], left to right.
    let rest = v.strip_prefix('P').and_then(|r| r.strip_prefix('T'));
    if let Some(rest) = rest {
        let mut secs: i64 = 0;
        let mut num = String::new();
        for c in rest.chars() {
            if c.is_ascii_digit() {
                num.push(c);
            } else {
                let mult = match c {
                    'H' => 3600,
                    'M' => 60,
                    'S' => 1,
                    _ => return None,
                };
                if let Ok(n) = num.parse::<i64>() {
                    secs += n * mult;
                }
                num.clear();
            }
        }
        if !num.is_empty() {
            return None;
        }
        return (secs > 0).then_some(secs * 1000);
    }
    // Clock-like: MM:SS or H:MM:SS.
    if v.contains(':') {
        let parts: Vec<&str> = v.split(':').collect();
        let nums: Vec<i64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        return match nums.as_slice() {
            [m, s] => Some(*m * 60_000 + *s * 1000),
            [h, m, s] => Some(*h * 3_600_000 + *m * 60_000 + *s * 1000),
            [m] => Some(*m * 60_000),
            _ => None,
        };
    }
    // Bare minutes.
    v.parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
        .map(|n| n * 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::from_str;

    fn envelope(json: &str) -> Envelope {
        from_str(json).expect("envelope parses")
    }

    const BASE: &str = r#"{"version":"1.0","session":{"new":false,"sessionId":"s1","application":{"applicationId":"amzn1.ask.skill.test"}},"context":{"System":{"device":{"deviceId":"d1","supportedInterfaces":{"AudioPlayer":{}}}},"AudioPlayer":{"playerActivity":"PLAYING","token":"tok","offsetInMilliseconds":1234}},"request":{"type":"IntentRequest","requestId":"r1","timestamp":"2026-01-01T00:00:00Z","locale":"en-US","dialogState":"SUCCEEDED","intent":{"name":"PlayIntent"}}}"#;

    #[test]
    fn parses_full_envelope() {
        let env = envelope(BASE);
        assert_eq!(env.device_id(), Some("d1"));
        assert_eq!(env.application_id(), Some("amzn1.ask.skill.test"));
        let ap = env.audio_player().unwrap();
        assert_eq!(
            ap.player_activity,
            Some(crate::request::PlayerActivity::Playing)
        );
        assert_eq!(ap.offset_in_milliseconds, Some(1234));
    }

    #[test]
    fn classifies_playback_controller() {
        for (intent, want) in [
            ("PlayIntent", IntentKind::Play),
            ("PauseIntent", IntentKind::Pause),
            ("StopIntent", IntentKind::Stop),
            ("NextIntent", IntentKind::Next),
            ("PreviousIntent", IntentKind::Previous),
        ] {
            let json = BASE.replace(
                "\"intent\":{\"name\":\"PlayIntent\"}",
                &format!("\"intent\":{{\"name\":\"{intent}\"}}"),
            );
            let env = envelope(&json);
            assert_eq!(IntentKind::from_envelope(&env), want, "{intent}");
        }
    }

    #[test]
    fn classifies_resume_with_offset() {
        let json = BASE.replace(
            "\"intent\":{\"name\":\"PlayIntent\"}",
            "\"intent\":{\"name\":\"ResumeIntent\"}",
        );
        let env = envelope(&json);
        assert_eq!(
            IntentKind::from_envelope(&env),
            IntentKind::Resume { offset_ms: 1234 }
        );
    }

    #[test]
    fn classifies_play_music_query() {
        let json = BASE.replace(
            "\"intent\":{\"name\":\"PlayIntent\"}",
            "\"intent\":{\"name\":\"PlayMusicIntent\",\"slots\":{\"query\":{\"name\":\"query\",\"value\":\"bohemian rhapsody by queen\"}}}",
        );
        let env = envelope(&json);
        assert_eq!(
            IntentKind::from_envelope(&env),
            IntentKind::PlayMusic {
                query: Some("bohemian rhapsody by queen".into())
            }
        );
    }

    #[test]
    fn fallback_with_utterance_slot() {
        let json = r#"{"version":"1.0","session":null,"context":null,"request":{"type":"IntentRequest","requestId":"r","intent":{"name":"AMAZON.FallbackIntent","slots":{"utterance":{"name":"utterance","value":"what is the capital of France"}}}}}"#;
        let env = envelope(json);
        assert_eq!(
            IntentKind::from_envelope(&env),
            IntentKind::Fallback {
                utterance: Some("what is the capital of France".into())
            }
        );
    }

    #[test]
    fn parse_time_variants() {
        assert_eq!(parse_time_slot("PT2M30S"), Some(150_000));
        assert_eq!(parse_time_slot("PT30S"), Some(30_000));
        assert_eq!(parse_time_slot("PT1H2M3S"), Some(3_723_000));
        assert_eq!(parse_time_slot("2:30"), Some(150_000));
        assert_eq!(parse_time_slot("1:02:03"), Some(3_723_000));
        assert_eq!(parse_time_slot("5"), Some(300_000));
        assert_eq!(parse_time_slot(""), None);
        assert_eq!(parse_time_slot("garbage"), None);
    }
}
