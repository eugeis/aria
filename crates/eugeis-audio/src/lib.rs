//! eugeis-audio — self-hosted music engine (no ffmpeg, no external daemons).

pub mod library;
pub mod player;
pub mod registry;
pub mod search;

pub use library::{Cover, Format, Library, Track};
pub use player::{PlayerState, Repeat};
pub use registry::{PlaybackRegistry, ResolvedStream, StreamSource};
pub use search::search;
