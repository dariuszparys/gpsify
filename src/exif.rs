//! Media classification and EXIF/XMP metadata handling.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Jpeg,
    Png,
    Heic,
    Raw,
    Unsupported,
}

impl MediaKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Heic => "heic",
            Self::Raw => "raw",
            Self::Unsupported => "unsupported",
        }
    }
}

pub const JPEG_EXTS: &[&str] = &[".jpg", ".jpeg"];
pub const PNG_EXTS: &[&str] = &[".png"];
pub const HEIC_EXTS: &[&str] = &[".heic", ".heif"];
pub const RAW_EXTS: &[&str] = &[".cr2", ".cr3", ".nef", ".arw", ".dng", ".raf", ".orf", ".rw2"];
pub const XMP_EXTS: &[&str] = &[".xmp"];

pub fn classify_media(path: &Path) -> MediaKind {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => format!(".{}", e.to_ascii_lowercase()),
        None => return MediaKind::Unsupported,
    };
    let s = ext.as_str();
    if JPEG_EXTS.contains(&s) {
        MediaKind::Jpeg
    } else if PNG_EXTS.contains(&s) {
        MediaKind::Png
    } else if HEIC_EXTS.contains(&s) {
        MediaKind::Heic
    } else if RAW_EXTS.contains(&s) {
        MediaKind::Raw
    } else {
        MediaKind::Unsupported
    }
}

pub fn is_processable(kind: MediaKind) -> bool {
    matches!(
        kind,
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic | MediaKind::Raw
    )
}
