//! aria-audio — self-hosted music engine (no ffmpeg, no external daemons).

pub mod library;
pub mod player;
pub mod registry;
pub mod search;

pub use library::{
    CollectionItem, CollectionKind, Cover, Format, IdRegistry, Library, TagEdit, Track,
    fingerprint_file, set_tags, slugify,
};
pub use player::{PlayerState, Repeat};
pub use registry::{PlaybackRegistry, ResolvedStream, StreamSource};
pub use search::search;
