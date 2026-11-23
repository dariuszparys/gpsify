//! Lazy JPEG previews and thumbs.
//!
//! Ingest never writes `previews/` or `thumbs/`. Propose and the review UI
//! call this module just-in-time. Pipeline: decode (HEIC primary still, RAW
//! embedded JPEG via `exiftool -b`) -> EXIF orientation transpose -> long-edge
//! resize -> JPEG quality 85. Originals are never mutated. RAW containers are
//! never opened as images and never sent. This module never writes GPS.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::exif::{
    classify_media, exiftool_available, is_photos_library, run_command_with_timeout, MediaKind,
    EXIFTOOL_TIMEOUT_S,
};
use crate::types::PhotoStatus;

pub const PREVIEW_LONG_EDGE: u32 = 1280;
pub const THUMB_LONG_EDGE: u32 = 256;
pub const JPEG_QUALITY: u8 = 85;
pub const MIN_RAW_PREVIEW_EDGE: u32 = 256;
pub const HASH_CHUNK_SIZE: usize = 1024 * 1024;
pub const RAW_PREVIEW_TAGS: &[&str] = &["JpgFromRaw", "PreviewImage", "OtherImage", "ThumbnailImage"];

#[derive(Debug, Error)]
#[error("{message}")]
pub struct MediaError {
    pub message: String,
    pub status: PhotoStatus,
}

impl MediaError {
    pub fn new(message: impl Into<String>, status: PhotoStatus) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }
}

pub fn heic_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        cfg!(feature = "heic")
    }
}

pub fn hash_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn preview_path(job_root: &Path, photo_id: &str) -> PathBuf {
    job_root.join("previews").join(format!("{photo_id}.jpg"))
}

pub fn thumb_path(job_root: &Path, photo_id: &str) -> PathBuf {
    job_root.join("thumbs").join(format!("{photo_id}.jpg"))
}

pub fn render_preview(
    path: &Path,
    kind: Option<MediaKind>,
    expected_sha256: &str,
) -> Result<Vec<u8>, MediaError> {
    render_jpeg(path, kind, expected_sha256, PREVIEW_LONG_EDGE)
}

pub fn render_thumb(
    path: &Path,
    kind: Option<MediaKind>,
    expected_sha256: &str,
) -> Result<Vec<u8>, MediaError> {
    render_jpeg(path, kind, expected_sha256, THUMB_LONG_EDGE)
}

pub fn render_jpeg(
    path: &Path,
    kind: Option<MediaKind>,
    expected_sha256: &str,
    long_edge: u32,
) -> Result<Vec<u8>, MediaError> {
    let is_symlink = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink || is_photos_library(path) {
        return Err(MediaError::new(
            "refusing symlink or Photos library source",
            PhotoStatus::Error,
        ));
    }
    let resolved_kind = kind.unwrap_or_else(|| classify_media(path));

    let jpeg = match resolved_kind {
        MediaKind::Unsupported => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            return Err(MediaError::new(
                format!("unsupported media for preview: {ext}"),
                PhotoStatus::SkippedUnsupported,
            ));
        }
        MediaKind::Raw => {
            let embedded = extract_raw_preview_jpeg(path)?;
            render_from_bytes(&embedded, MediaKind::Jpeg, long_edge)?
        }
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic => {
            let bytes = std::fs::read(path).map_err(|e| {
                MediaError::new(format!("unreadable: {e}"), PhotoStatus::SkippedUnreadable)
            })?;
            render_from_bytes(&bytes, resolved_kind, long_edge)?
        }
    };

    let digest = hash_file(path).map_err(|e| {
        MediaError::new(format!("failed to hash source: {e}"), PhotoStatus::Error)
    })?;
    if digest != expected_sha256 {
        return Err(MediaError::new(
            "source sha256 changed; refusing to send preview",
            PhotoStatus::Error,
        ));
    }

    Ok(jpeg)
}

pub fn ensure_preview(
    job_root: &Path,
    photo_id: &str,
    source: &Path,
    expected_sha256: &str,
    kind: Option<MediaKind>,
) -> Result<PathBuf, MediaError> {
    ensure_derived(
        &preview_path(job_root, photo_id),
        source,
        expected_sha256,
        kind,
        PREVIEW_LONG_EDGE,
    )
}

pub fn ensure_thumb(
    job_root: &Path,
    photo_id: &str,
    source: &Path,
    expected_sha256: &str,
    kind: Option<MediaKind>,
) -> Result<PathBuf, MediaError> {
    ensure_derived(
        &thumb_path(job_root, photo_id),
        source,
        expected_sha256,
        kind,
        THUMB_LONG_EDGE,
    )
}

fn ensure_derived(
    dest: &Path,
    source: &Path,
    expected_sha256: &str,
    kind: Option<MediaKind>,
    long_edge: u32,
) -> Result<PathBuf, MediaError> {
    let is_symlink = std::fs::symlink_metadata(source)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink || is_photos_library(source) {
        return Err(MediaError::new(
            "refusing symlink or Photos library source",
            PhotoStatus::Error,
        ));
    }
    if dest.is_file() {
        let digest = hash_file(source).map_err(|e| {
            MediaError::new(format!("failed to hash source: {e}"), PhotoStatus::Error)
        })?;
        if digest != expected_sha256 {
            return Err(MediaError::new(
                "source sha256 changed; refusing to send preview",
                PhotoStatus::Error,
            ));
        }
        return Ok(dest.to_path_buf());
    }

    let jpeg = render_jpeg(source, kind, expected_sha256, long_edge)?;
    let parent = dest
        .parent()
        .ok_or_else(|| MediaError::new("invalid derived dest", PhotoStatus::Error))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| MediaError::new(format!("mkdir failed: {e}"), PhotoStatus::Error))?;

    let mut tmp = NamedTempFile::new_in(parent)
        .map_err(|e| MediaError::new(format!("tempfile failed: {e}"), PhotoStatus::Error))?;
    tmp.write_all(&jpeg)
        .map_err(|e| MediaError::new(format!("write failed: {e}"), PhotoStatus::Error))?;
    tmp.persist(dest)
        .map_err(|e| MediaError::new(format!("persist failed: {}", e.error), PhotoStatus::Error))?;

    Ok(dest.to_path_buf())
}

pub fn extract_raw_preview_jpeg(path: &Path) -> Result<Vec<u8>, MediaError> {
    extract_raw_preview_jpeg_with(path, exiftool_available(), |p, tag| {
        exiftool_binary_tag(p, tag)
    })
}

pub fn extract_raw_preview_jpeg_with<F>(
    path: &Path,
    has_exiftool: bool,
    mut fetch_tag: F,
) -> Result<Vec<u8>, MediaError>
where
    F: FnMut(&Path, &str) -> Vec<u8>,
{
    if !has_exiftool {
        return Err(MediaError::new(
            "RAW preview extract needs exiftool; no usable embedded preview",
            PhotoStatus::SkippedNoPreview,
        ));
    }
    for &tag in RAW_PREVIEW_TAGS {
        let blob = fetch_tag(path, tag);
        if blob.is_empty() {
            continue;
        }
        let Some(long_edge) = jpeg_long_edge(&blob) else {
            continue;
        };
        if long_edge >= MIN_RAW_PREVIEW_EDGE {
            return Ok(blob);
        }
    }
    Err(MediaError::new(
        "no usable RAW embedded preview (need long edge >= 256 px)",
        PhotoStatus::SkippedNoPreview,
    ))
}

fn exiftool_binary_tag(path: &Path, tag: &str) -> Vec<u8> {
    let mut cmd = Command::new("exiftool");
    cmd.arg("-b")
        .arg(format!("-{tag}"))
        .arg("--")
        .arg(path);
    match run_command_with_timeout(&mut cmd, Duration::from_secs(EXIFTOOL_TIMEOUT_S)) {
        Ok(out) => out.stdout,
        Err(_) => Vec::new(),
    }
}

fn jpeg_long_edge(blob: &[u8]) -> Option<u32> {
    let cursor = std::io::Cursor::new(blob);
    let reader = image::ImageReader::new(BufReader::new(cursor))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    Some(w.max(h))
}

#[cfg(target_os = "macos")]
fn render_from_bytes(bytes: &[u8], _kind: MediaKind, long_edge: u32) -> Result<Vec<u8>, MediaError> {
    crate::media_macos::render_jpeg_from_bytes(
        bytes,
        long_edge,
        f64::from(JPEG_QUALITY) / 100.0,
    )
}

#[cfg(not(target_os = "macos"))]
fn render_from_bytes(bytes: &[u8], kind: MediaKind, long_edge: u32) -> Result<Vec<u8>, MediaError> {
    use fast_image_resize as fr;
    use image::codecs::jpeg::JpegEncoder;
    use image::{DynamicImage, ImageReader};

    let mut img = if kind == MediaKind::Heic {
        #[cfg(feature = "heic")]
        {
            decode_heic_libheif(bytes)?
        }
        #[cfg(not(feature = "heic"))]
        {
            return Err(MediaError::new(
                "HEIC requires macOS ImageIO or --features heic",
                PhotoStatus::SkippedUnsupported,
            ));
        }
    } else {
        ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| MediaError::new(format!("unreadable: {e}"), PhotoStatus::SkippedUnreadable))?
            .decode()
            .map_err(|e| MediaError::new(format!("unreadable: {e}"), PhotoStatus::SkippedUnreadable))?
    };

    if let Ok(exif_data) = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(bytes))
    {
        if let Some(field) = exif_data.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
            if let Some(ori) = field.value.get_uint(0) {
                if let Some(orient) = exif_orientation_from_u32(ori) {
                    img.apply_orientation(orient);
                }
            }
        }
    }

    let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
    let (w, h) = (rgb.width(), rgb.height());
    let current = w.max(h);
    let resized = if current > 0 && current > long_edge {
        let scale = f64::from(long_edge) / f64::from(current);
        let new_w = ((f64::from(w) * scale).round() as u32).max(1);
        let new_h = ((f64::from(h) * scale).round() as u32).max(1);
        let mut dst = fr::images::Image::new(new_w, new_h, fr::PixelType::U8x3);
        let mut resizer = fr::Resizer::new();
        resizer
            .resize(
                &rgb,
                &mut dst,
                &fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3)),
            )
            .map_err(|e| MediaError::new(format!("resize failed: {e}"), PhotoStatus::Error))?;
        image::RgbImage::from_raw(new_w, new_h, dst.into_vec())
            .ok_or_else(|| MediaError::new("resize buffer mismatch", PhotoStatus::Error))?
    } else {
        rgb.into_rgb8()
    };

    let mut out = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    enc.encode_image(&resized)
        .map_err(|e| MediaError::new(format!("jpeg encode failed: {e}"), PhotoStatus::Error))?;
    Ok(out)
}

#[cfg(all(not(target_os = "macos"), feature = "heic"))]
fn decode_heic_libheif(bytes: &[u8]) -> Result<image::DynamicImage, MediaError> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let lib = LibHeif::new();
    let ctx = HeifContext::read_from_bytes(bytes).map_err(|e| {
        MediaError::new(format!("unreadable HEIC: {e}"), PhotoStatus::SkippedUnreadable)
    })?;
    let handle = ctx.primary_image_handle().map_err(|e| {
        MediaError::new(format!("unreadable HEIC: {e}"), PhotoStatus::SkippedUnreadable)
    })?;
    let decoded = lib
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
        .map_err(|e| {
            MediaError::new(format!("unreadable HEIC: {e}"), PhotoStatus::SkippedUnreadable)
        })?;
    let w = decoded.width();
    let h = decoded.height();
    let plane = decoded.planes().interleaved.ok_or_else(|| {
        MediaError::new("unreadable HEIC: no interleaved RGB plane", PhotoStatus::SkippedUnreadable)
    })?;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h as usize {
        let start = y * plane.stride;
        rgb.extend_from_slice(&plane.data[start..start + (w as usize * 3)]);
    }
    let img = image::RgbImage::from_raw(w, h, rgb)
        .ok_or_else(|| MediaError::new("unreadable HEIC buffer", PhotoStatus::SkippedUnreadable))?;
    Ok(image::DynamicImage::ImageRgb8(img))
}

#[cfg(not(target_os = "macos"))]
fn exif_orientation_from_u32(v: u32) -> Option<image::metadata::Orientation> {
    use image::metadata::Orientation;
    match v {
        1 => Some(Orientation::NoTransforms),
        2 => Some(Orientation::FlipHorizontal),
        3 => Some(Orientation::Rotate180),
        4 => Some(Orientation::FlipVertical),
        5 => Some(Orientation::Rotate90FlipH),
        6 => Some(Orientation::Rotate90),
        7 => Some(Orientation::Rotate270FlipH),
        8 => Some(Orientation::Rotate270),
        _ => None,
    }
}
