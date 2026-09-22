//! eugeis-alexa — ASK v1 protocol types, intent classification, responses, SSML.

pub mod intent;
pub mod request;
pub mod response;
pub mod ssml;

pub use intent::{IntentKind, parse_time_slot};
pub use request::{Envelope, PlayerActivity};
pub use response::{AudioItem, Response, ResponseBuilder, audio_item};
