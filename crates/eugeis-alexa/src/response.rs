//! Builders for ASK v1 responses.
//!
//! Responses are camelCase JSON; the builder keeps them ergonomic and
//! serializes through `serde_json::Value` so the wire format stays explicit.

use crate::ssml;
use serde::Serialize;
use serde_json::{Value, json};

/// A complete response to send back to Amazon.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub version: String,
    pub response: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResponseBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_speech: Option<OutputSpeech>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reprompt: Option<OutputSpeech>,
    #[serde(skip_serializing_if = "is_true")]
    pub should_end_session: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub directives: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_attributes: Option<Value>,
}

fn is_true(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputSpeech {
    pub r#type: SpeechType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssml: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SpeechType {
    Ssml,
    PlainText,
}

/// AudioPlayer stream descriptor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    pub url: String,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_in_milliseconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_previous_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_offset_in_milliseconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioItem {
    pub stream: Stream,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AudioMetadata>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioMetadata {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art: Option<Art>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Art {
    pub sources: Vec<ArtSource>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtSource {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_pixels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height_pixels: Option<u32>,
}

/// Fluent builder for [`Response`].
#[derive(Debug, Clone, Default)]
pub struct ResponseBuilder {
    body: ResponseBody,
}

impl ResponseBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Speak `text` through Amazon TTS (markdown stripped, SSML-escaped).
    pub fn say(&mut self, text: impl AsRef<str>) -> &mut Self {
        let text = text.as_ref();
        if !text.trim().is_empty() {
            self.body.output_speech = Some(OutputSpeech {
                r#type: SpeechType::Ssml,
                ssml: Some(ssml::speak(text)),
                text: None,
            });
        }
        self
    }

    /// Speak raw text without SSML escaping (caller controls the format).
    pub fn say_raw_ssml(&mut self, ssml: impl AsRef<str>) -> &mut Self {
        self.body.output_speech = Some(OutputSpeech {
            r#type: SpeechType::Ssml,
            ssml: Some(ssml::wrap(ssml.as_ref())),
            text: None,
        });
        self
    }

    pub fn reprompt(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.body.reprompt = Some(OutputSpeech {
            r#type: SpeechType::PlainText,
            ssml: None,
            text: Some(text.as_ref().to_string()),
        });
        self
    }

    pub fn end_session(&mut self) -> &mut Self {
        self.body.should_end_session = true;
        self
    }

    pub fn open_session(&mut self) -> &mut Self {
        self.body.should_end_session = false;
        self
    }

    pub fn session_attributes(&mut self, attrs: Value) -> &mut Self {
        self.body.session_attributes = Some(attrs);
        self
    }

    /// AudioPlayer.Play with REPLACE_ALL.
    pub fn play_audio(&mut self, item: AudioItem) -> &mut Self {
        self.directive(json!({
            "type": "AudioPlayer.Play",
            "playBehavior": "REPLACE_ALL",
            "audioItem": item,
        }));
        self
    }

    /// AudioPlayer.Play with REPLACE_ENQUEUED (keeps what is playing queued).
    pub fn enqueue_audio(&mut self, item: AudioItem) -> &mut Self {
        self.directive(json!({
            "type": "AudioPlayer.Play",
            "playBehavior": "REPLACE_ENQUEUED",
            "audioItem": item,
        }));
        self
    }

    pub fn pause_audio(&mut self) -> &mut Self {
        self.directive(json!({"type": "AudioPlayer.Pause"}));
        self
    }

    pub fn stop_audio(&mut self) -> &mut Self {
        self.directive(json!({"type": "AudioPlayer.Stop"}));
        self
    }

    pub fn clear_queue(&mut self) -> &mut Self {
        self.directive(json!({"type": "AudioPlayer.ClearQueue", "clearBehavior": "CLEAR_ALL"}));
        self
    }

    pub fn directive(&mut self, dir: Value) -> &mut Self {
        self.body.directives.push(dir);
        self
    }

    pub fn build(&self) -> Response {
        Response {
            version: "1.0".into(),
            response: self.body.clone(),
        }
    }
}

impl Response {
    /// Empty response (required shape for session-ended / player reports that
    /// need no speech or directives).
    pub fn empty() -> Self {
        ResponseBuilder::new().end_session().build()
    }

    /// Serialize to the JSON Amazon expects.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("Response serializes")
    }
}

/// Build an AudioItem for a library/radio stream.
pub fn audio_item(
    base_url: &str,
    token: &str,
    offset_ms: u64,
    expected: Option<(&str, u64)>,
    title: &str,
    subtitle: Option<&str>,
    art_url: Option<String>,
) -> AudioItem {
    let stream = Stream {
        url: format!("{base_url}/stream/{token}"),
        token: token.to_string(),
        offset_in_milliseconds: (offset_ms > 0).then_some(offset_ms),
        expected_previous_token: expected.map(|(t, _)| t.to_string()),
        expected_offset_in_milliseconds: expected.map(|(_, o)| o),
    };
    let metadata = AudioMetadata {
        title: title.to_string(),
        subtitle: subtitle.map(str::to_string),
        art: art_url.filter(|u| !u.is_empty()).map(|u| Art {
            sources: vec![ArtSource {
                url: u,
                width_pixels: None,
                height_pixels: None,
            }],
        }),
    };
    AudioItem {
        stream,
        metadata: Some(metadata),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_play_response() {
        let mut b = ResponseBuilder::new();
        b.say("Playing test")
            .play_audio(audio_item(
                "https://example.com",
                "tok123",
                0,
                None,
                "Song",
                Some("Artist"),
                Some("https://example.com/art/tok123".into()),
            ))
            .end_session();
        let v = b.build().to_json();
        assert_eq!(v["version"], "1.0");
        assert_eq!(v["response"]["outputSpeech"]["type"], "SSML");
        assert!(
            v["response"]["outputSpeech"]["ssml"]
                .as_str()
                .unwrap()
                .starts_with("<speak>")
        );
        assert_eq!(v["response"]["shouldEndSession"], true);
        let dir = &v["response"]["directives"][0];
        assert_eq!(dir["type"], "AudioPlayer.Play");
        assert_eq!(dir["playBehavior"], "REPLACE_ALL");
        assert_eq!(
            dir["audioItem"]["stream"]["url"],
            "https://example.com/stream/tok123"
        );
        assert!(
            dir["audioItem"]["stream"]
                .get("offsetInMilliseconds")
                .is_none()
        );
        assert_eq!(dir["audioItem"]["metadata"]["title"], "Song");
    }

    #[test]
    fn offset_and_expected_serialized_when_set() {
        let item = audio_item(
            "https://e.com",
            "t",
            42_000,
            Some(("prev", 40_000)),
            "S",
            None,
            None,
        );
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["stream"]["offsetInMilliseconds"], 42_000);
        assert_eq!(v["stream"]["expectedPreviousToken"], "prev");
        assert_eq!(v["stream"]["expectedOffsetInMilliseconds"], 40_000);
    }

    #[test]
    fn empty_response_shape() {
        let v = Response::empty().to_json();
        assert_eq!(v["response"]["shouldEndSession"], true);
        assert!(v["response"].get("outputSpeech").is_none());
        assert!(v["response"].get("directives").is_none());
    }
}
