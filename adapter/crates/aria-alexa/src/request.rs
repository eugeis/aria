//! Amazon Alexa Skills Kit (ASK) v1 request envelope.
//!
//! The structs here mirror the subset of the ASK v1 API that a music +
//! conversation skill receives. Everything is tolerant: unknown fields are
//! ignored, optional sections are `Option`, so protocol additions by Amazon
//! never break deserialization.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Top-level envelope for every request Amazon sends to the skill endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    pub version: String,
    #[serde(default)]
    pub session: Option<Session>,
    #[serde(default)]
    pub context: Option<Context>,
    pub request: Request,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub new: Option<bool>,
    #[serde(default)]
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub application: Option<ApplicationRef>,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default)]
    pub attributes: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationRef {
    #[serde(default)]
    #[serde(rename = "applicationId")]
    pub application_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserRef {
    #[serde(default)]
    #[serde(rename = "userId")]
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Context {
    #[serde(default, rename = "System")]
    pub system: Option<SystemContext>,
    #[serde(default, rename = "AudioPlayer")]
    pub audio_player: Option<AudioPlayerContext>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemContext {
    #[serde(default)]
    pub device: Option<Device>,
    #[serde(default)]
    pub application: Option<ApplicationRef>,
    #[serde(default)]
    pub user: Option<UserRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    #[serde(default, rename = "deviceId")]
    pub device_id: Option<String>,
    #[serde(default, rename = "supportedInterfaces")]
    pub supported_interfaces: Option<Value>,
}

/// AudioPlayer state as reported by the device on every request.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AudioPlayerContext {
    #[serde(default)]
    #[serde(rename = "playerActivity")]
    pub player_activity: Option<PlayerActivity>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    #[serde(rename = "offsetInMilliseconds")]
    pub offset_in_milliseconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlayerActivity {
    Buffering,
    Finished,
    #[default]
    Idle,
    Paused,
    Playing,
    Stopped,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(rename = "type")]
    pub request_type: String,
    #[serde(default)]
    #[serde(rename = "requestId")]
    pub request_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    #[serde(rename = "dialogState")]
    pub dialog_state: Option<String>,
    #[serde(default)]
    pub intent: Option<Intent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Intent {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    #[serde(rename = "confirmationStatus")]
    pub confirmation_status: Option<String>,
    #[serde(default)]
    pub slots: BTreeMap<String, Slot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Slot {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub values: Option<Vec<String>>,
    /// ASK sends an object here: `{"status":"SUCCESS","value":{...}}`.
    #[serde(default, rename = "resolutionStatus")]
    pub resolution_status: Option<Value>,
    #[serde(default)]
    #[serde(rename = "confirmationStatus")]
    pub confirmation_status: Option<String>,
}

impl Envelope {
    /// Stable identifier of the Alexa device, used to key per-device state.
    pub fn device_id(&self) -> Option<&str> {
        self.context
            .as_ref()
            .and_then(|c| c.system.as_ref())
            .and_then(|s| s.device.as_ref())
            .and_then(|d| d.device_id.as_deref())
            .filter(|s| !s.is_empty())
    }

    pub fn application_id(&self) -> Option<&str> {
        self.context
            .as_ref()
            .and_then(|c| c.system.as_ref())
            .and_then(|s| s.application.as_ref())
            .and_then(|a| a.application_id.as_deref())
            .or_else(|| {
                self.session
                    .as_ref()
                    .and_then(|s| s.application.as_ref())
                    .and_then(|a| a.application_id.as_deref())
            })
    }

    pub fn audio_player(&self) -> Option<&AudioPlayerContext> {
        self.context.as_ref().and_then(|c| c.audio_player.as_ref())
    }

    /// First non-empty value of a slot, if the current request carries one.
    pub fn slot(&self, name: &str) -> Option<&str> {
        self.request
            .intent
            .as_ref()
            .and_then(|i| i.slots.get(name))
            .and_then(|s| s.value.as_deref())
            .filter(|v| !v.trim().is_empty())
    }

    /// All values of a multi-value slot.
    pub fn slot_values(&self, name: &str) -> Vec<String> {
        self.request
            .intent
            .as_ref()
            .and_then(|i| i.slots.get(name))
            .and_then(|s| s.values.clone())
            .unwrap_or_default()
    }

    pub fn intent_name(&self) -> Option<&str> {
        self.request.intent.as_ref().and_then(|i| i.name.as_deref())
    }
}
