//! Visual geolocation of untagged photos. Proposes a pin; writes GPS only after a human accepts.

pub mod apply;
pub mod cluster;
pub mod config;
pub mod exif;
pub mod geocode;
pub mod ingest;
pub mod media;
#[cfg(target_os = "macos")]
pub mod media_macos;
pub mod review;
pub mod status;
pub mod store;
pub mod types;
pub mod undo;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
