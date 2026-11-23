//! Visual geolocation of untagged photos. Proposes a pin; writes GPS only after a human accepts.

pub mod config;
pub mod exif;
pub mod status;
pub mod store;
pub mod types;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
