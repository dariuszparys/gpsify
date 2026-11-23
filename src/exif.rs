//! Batched EXIF / XMP read and GPS write via `exiftool` (K7).
//!
//! Read: one `exiftool -j -fast -n` process per 200 files. If `exiftool` is
//! missing, fall back to pure-Rust `kamadak-exif` for JPEG read-only and skip
//! HEIC GPS detection with a warning. RAW sibling `.xmp` GPS is merged onto
//! the RAW row when the sidecar exists.
//!
//! Write: one `exiftool -@ argfile` of per-dest tag sets. JPEG/HEIC get
//! EXIF+XMP; PNG gets XMP in-file; RAW gets a sibling `.xmp` only. `exiftool`
//! is required for all writes (K7); `GPSIFY_DRY_RUN=1` blocks writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::config::live_http_blocked;

pub const EXIFTOOL_CHUNK_SIZE: usize = 200;
pub const EXIFTOOL_TIMEOUT_S: u64 = 120;
pub const EXIFTOOL_WRITE_TIMEOUT_S: u64 = 600;
pub const GPS_MATCH_DEG: f64 = 1e-5;

pub const EXIFTOOL_READ_ARGS: &[&str] = &[
    "-j",
    "-fast",
    "-n",
    "-GPSLatitude",
    "-GPSLongitude",
    "-GPSLatitudeRef",
    "-GPSLongitudeRef",
    "-DateTimeOriginal",
    "-CreateDate",
    "-OffsetTimeOriginal",
    "-Make",
    "-Model",
    "-Orientation",
    "-GPSImgDirection",
    "-GPSAltitude",
];

pub const JPEG_EXTS: &[&str] = &[".jpg", ".jpeg"];
pub const PNG_EXTS: &[&str] = &[".png"];
pub const HEIC_EXTS: &[&str] = &[".heic", ".heif"];
pub const RAW_EXTS: &[&str] = &[
    ".cr2", ".cr3", ".nef", ".arw", ".dng", ".raf", ".orf", ".rw2",
];
pub const XMP_EXTS: &[&str] = &[".xmp"];

pub const MISSING_EXIFTOOL_WARNING: &str =
    "exiftool not found; using pure-Rust JPEG EXIF only. HEIC GPS detection skipped.";
pub const WRITE_REQUIRES_EXIFTOOL: &str = "GPS write requires exiftool";
pub const HEIC_WRITE_REQUIRES_EXIFTOOL: &str = "HEIC write requires exiftool";
pub const DRY_RUN_BLOCKS_WRITES: &str = "GPSIFY_DRY_RUN=1 blocks exiftool writes";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ExifWriteError(pub String);

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

#[derive(Debug, Clone, PartialEq)]
pub struct GpsWrite {
    pub dest: PathBuf,
    pub lat: f64,
    pub lng: f64,
    pub kind: MediaKind,
    pub job_id: String,
    pub decision_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExifRead {
    pub gps_lat: Option<f64>,
    pub gps_lng: Option<f64>,
    pub datetime_original: Option<String>,
    pub create_date: Option<String>,
    pub offset_time_original: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub orientation: Option<i64>,
    pub gps_img_direction: Option<f64>,
    pub gps_altitude: Option<f64>,
    pub source: String,
}

impl Default for ExifRead {
    fn default() -> Self {
        Self {
            gps_lat: None,
            gps_lng: None,
            datetime_original: None,
            create_date: None,
            offset_time_original: None,
            make: None,
            model: None,
            orientation: None,
            gps_img_direction: None,
            gps_altitude: None,
            source: "none".to_string(),
        }
    }
}

impl ExifRead {
    pub fn has_gps(&self) -> bool {
        matches!((self.gps_lat, self.gps_lng), (Some(a), Some(b)) if a.is_finite() && b.is_finite())
    }

    /// `DateTimeOriginal`, falling back to `CreateDate`. Never file mtime.
    pub fn capture_time(&self) -> Option<&str> {
        self.datetime_original
            .as_deref()
            .or(self.create_date.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExifBatchResult {
    pub by_path: BTreeMap<String, ExifRead>,
    pub warnings: Vec<String>,
    pub used_exiftool: bool,
}

pub fn has_unsafe_path_chars(value: &str) -> bool {
    value.contains('\n') || value.contains('\r') || value.contains('\0')
}

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

pub fn is_photos_library_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".photoslibrary")
}

/// True if `path` or its resolved target sits inside a `*.photoslibrary` bundle (K14).
pub fn is_photos_library(path: &Path) -> bool {
    for anc in path.ancestors() {
        if let Some(name) = anc.file_name().and_then(|n| n.to_str()) {
            if is_photos_library_name(name) {
                return true;
            }
        }
    }
    if let Ok(resolved) = std::fs::canonicalize(path) {
        for anc in resolved.ancestors() {
            if let Some(name) = anc.file_name().and_then(|n| n.to_str()) {
                if is_photos_library_name(name) {
                    return true;
                }
            }
        }
    }
    false
}

pub fn sidecar_xmp_path(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let stem = path.file_stem()?.to_str()?;
    let canon_self = std::fs::canonicalize(path).ok();
    for ext in ["xmp", "XMP"] {
        let candidate = parent.join(format!("{stem}.{ext}"));
        let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        if let (Some(c_self), Ok(c_cand)) = (&canon_self, std::fs::canonicalize(&candidate)) {
            if &c_cand == c_self {
                continue;
            }
        }
        return Some(candidate);
    }
    None
}

pub fn exiftool_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("exiftool");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn exiftool_available() -> bool {
    exiftool_path().is_some()
}

pub fn writes_blocked() -> bool {
    live_http_blocked()
}

pub fn read_exif_batch(paths: &[PathBuf]) -> ExifBatchResult {
    read_exif_batch_with(paths, exiftool_available())
}

pub fn read_exif_batch_with(paths: &[PathBuf], use_exiftool: bool) -> ExifBatchResult {
    let resolved: Vec<PathBuf> = paths
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let mut query: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut sidecar_for: BTreeMap<String, PathBuf> = BTreeMap::new();

    for path in &resolved {
        let key = path.to_string_lossy().into_owned();
        if seen.insert(key.clone()) {
            query.push(path.clone());
        }
        if classify_media(path) == MediaKind::Raw {
            if let Some(sidecar) = sidecar_xmp_path(path) {
                let sidecar_res = std::fs::canonicalize(&sidecar).unwrap_or(sidecar);
                sidecar_for.insert(key, sidecar_res.clone());
                let skey = sidecar_res.to_string_lossy().into_owned();
                if seen.insert(skey) {
                    query.push(sidecar_res);
                }
            }
        }
    }

    let mut warnings = Vec::new();
    let raw_map = if use_exiftool {
        read_with_exiftool(&query, &mut warnings)
    } else {
        warnings.push(MISSING_EXIFTOOL_WARNING.to_string());
        read_fallback(&query)
    };

    let mut by_path = BTreeMap::new();
    for path in &resolved {
        let key = path.to_string_lossy().into_owned();
        let mut tags = raw_map.get(&key).cloned().unwrap_or_default();
        if let Some(sidecar) = sidecar_for.get(&key) {
            let skey = sidecar.to_string_lossy().into_owned();
            if let Some(stags) = raw_map.get(&skey) {
                tags = merge_sidecar(tags, stags);
            }
        }
        by_path.insert(key, tags);
    }

    ExifBatchResult {
        by_path,
        warnings,
        used_exiftool: use_exiftool,
    }
}

pub fn run_command_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdout_handle = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });
    let mut stderr_handle = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        })
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("process timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e.to_string()),
        }
    };

    let stdout = stdout_handle
        .take()
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .take()
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn read_with_exiftool(paths: &[PathBuf], warnings: &mut Vec<String>) -> BTreeMap<String, ExifRead> {
    let mut result = BTreeMap::new();
    for chunk in paths.chunks(EXIFTOOL_CHUNK_SIZE) {
        let mut cmd = Command::new("exiftool");
        cmd.args(EXIFTOOL_READ_ARGS);
        cmd.arg("--");
        for path in chunk {
            cmd.arg(path);
        }
        let completed =
            match run_command_with_timeout(&mut cmd, Duration::from_secs(EXIFTOOL_TIMEOUT_S)) {
                Ok(out) => out,
                Err(exc) => {
                    warnings.push(format!(
                        "exiftool failed ({exc}); falling back for {} file(s)",
                        chunk.len()
                    ));
                    result.extend(read_fallback(chunk));
                    continue;
                }
            };
        let code = completed.status.code().unwrap_or(-1);
        let stdout_str = String::from_utf8_lossy(&completed.stdout);
        if (code != 0 && code != 1) || stdout_str.trim().is_empty() {
            warnings.push(format!(
                "exiftool exit {code}; falling back for {} file(s)",
                chunk.len()
            ));
            result.extend(read_fallback(chunk));
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&stdout_str) else {
            warnings.push("exiftool returned non-JSON output; falling back".to_string());
            result.extend(read_fallback(chunk));
            continue;
        };
        let Some(items) = payload.as_array() else {
            warnings.push("exiftool JSON was not a list; falling back".to_string());
            result.extend(read_fallback(chunk));
            continue;
        };

        let mut parsed_files = BTreeSet::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(source) = obj.get("SourceFile").and_then(|v| v.as_str()) else {
                continue;
            };
            let key = Path::new(source).to_string_lossy().into_owned();
            result.insert(key.clone(), exif_from_exiftool_item(obj));
            parsed_files.insert(key);
        }
        for path in chunk {
            let key = path.to_string_lossy().into_owned();
            if !parsed_files.contains(&key) {
                let fb = read_fallback(std::slice::from_ref(path));
                result.insert(key.clone(), fb.get(&key).cloned().unwrap_or_default());
            }
        }
    }
    result
}

fn read_fallback(paths: &[PathBuf]) -> BTreeMap<String, ExifRead> {
    let mut result = BTreeMap::new();
    for path in paths {
        let key = path.to_string_lossy().into_owned();
        let entry = match classify_media(path) {
            MediaKind::Jpeg => read_jpeg_kamadak(path),
            MediaKind::Png => read_png_xmp_fallback(path),
            MediaKind::Heic => ExifRead {
                source: "skipped_heic".to_string(),
                ..Default::default()
            },
            MediaKind::Raw => ExifRead::default(),
            MediaKind::Unsupported => {
                let is_xmp = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| format!(".{}", e.to_ascii_lowercase()))
                    .map(|e| XMP_EXTS.contains(&e.as_str()))
                    .unwrap_or(false);
                if is_xmp {
                    read_xmp_sidecar(path)
                } else {
                    ExifRead::default()
                }
            }
        };
        result.insert(key, entry);
    }
    result
}

fn exif_from_exiftool_item(item: &serde_json::Map<String, Value>) -> ExifRead {
    let mut lat = as_float(item.get("GPSLatitude"));
    let mut lng = as_float(item.get("GPSLongitude"));
    lat = apply_ref(lat, item.get("GPSLatitudeRef"), 'S');
    lng = apply_ref(lng, item.get("GPSLongitudeRef"), 'W');
    let orientation = as_float(item.get("Orientation")).map(|v| v as i64);
    ExifRead {
        gps_lat: lat,
        gps_lng: lng,
        datetime_original: as_str(item.get("DateTimeOriginal")),
        create_date: as_str(item.get("CreateDate")),
        offset_time_original: as_str(item.get("OffsetTimeOriginal")),
        make: as_str(item.get("Make")),
        model: as_str(item.get("Model")),
        orientation,
        gps_img_direction: as_float(item.get("GPSImgDirection")),
        gps_altitude: as_float(item.get("GPSAltitude")),
        source: "exiftool".to_string(),
    }
}

fn read_jpeg_kamadak(path: &Path) -> ExifRead {
    let Ok(file) = File::open(path) else {
        return ExifRead::default();
    };
    let mut reader = BufReader::new(file);
    let Ok(exif_data) = exif::Reader::new()
        .continue_on_error(true)
        .read_from_container(&mut reader)
        .or_else(|e| e.distill_partial_result(|_| {}))
    else {
        return ExifRead::default();
    };

    let mut out = ExifRead {
        source: "rust_exif".to_string(),
        ..Default::default()
    };

    let get_ascii = |tag: exif::Tag| -> Option<String> {
        let field = exif_data.get_field(tag, exif::In::PRIMARY)?;
        if let exif::Value::Ascii(ref vecs) = field.value {
            let bytes = vecs.first()?;
            let s = String::from_utf8_lossy(bytes)
                .trim_matches('\0')
                .trim()
                .to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        } else {
            let s = field.display_value().to_string();
            let trimmed = s.trim_matches('"').trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    };

    let get_dms = |tag: exif::Tag| -> Option<f64> {
        let field = exif_data.get_field(tag, exif::In::PRIMARY)?;
        if let exif::Value::Rational(ref rats) = field.value {
            if rats.len() < 3 {
                return None;
            }
            let r = |idx: usize| -> Option<f64> {
                let rat = rats.get(idx)?;
                if rat.denom == 0 {
                    None
                } else {
                    Some(rat.num as f64 / rat.denom as f64)
                }
            };
            let val = r(0)? + r(1)? / 60.0 + r(2)? / 3600.0;
            if val.is_finite() {
                Some(val)
            } else {
                None
            }
        } else {
            None
        }
    };

    if let (Some(mut lat), Some(mut lng)) = (
        get_dms(exif::Tag::GPSLatitude),
        get_dms(exif::Tag::GPSLongitude),
    ) {
        if let Some(r) = get_ascii(exif::Tag::GPSLatitudeRef) {
            if r.to_ascii_uppercase().starts_with('S') {
                lat = -lat.abs();
            }
        }
        if let Some(r) = get_ascii(exif::Tag::GPSLongitudeRef) {
            if r.to_ascii_uppercase().starts_with('W') {
                lng = -lng.abs();
            }
        }
        if lat.is_finite() && lng.is_finite() {
            out.gps_lat = Some(lat);
            out.gps_lng = Some(lng);
        }
    }

    out.datetime_original = get_ascii(exif::Tag::DateTimeOriginal);
    out.create_date = get_ascii(exif::Tag::DateTimeDigitized);
    out.offset_time_original = get_ascii(exif::Tag::OffsetTimeOriginal);
    out.make = get_ascii(exif::Tag::Make);
    out.model = get_ascii(exif::Tag::Model);
    if let Some(field) = exif_data.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
        if let Some(v) = field.value.get_uint(0) {
            out.orientation = Some(v as i64);
        }
    }
    out
}

fn xmp_lat_elem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)<exif:GPSLatitude>\s*(-?[0-9]+(?:\.[0-9]+)?)\s*</exif:GPSLatitude>")
            .unwrap()
    })
}

fn xmp_lng_elem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)<exif:GPSLongitude>\s*(-?[0-9]+(?:\.[0-9]+)?)\s*</exif:GPSLongitude>")
            .unwrap()
    })
}

fn xmp_lat_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)exif:GPSLatitude\s*=\s*['"](-?[0-9]+(?:\.[0-9]+)?)['"]"#).unwrap()
    })
}

fn xmp_lng_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)exif:GPSLongitude\s*=\s*['"](-?[0-9]+(?:\.[0-9]+)?)['"]"#).unwrap()
    })
}

fn match_float(text: &str, p1: &Regex, p2: &Regex) -> Option<f64> {
    for pat in [p1, p2] {
        if let Some(caps) = pat.captures(text) {
            if let Some(m) = caps.get(1) {
                if let Ok(v) = m.as_str().trim().parse::<f64>() {
                    if v.is_finite() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

fn read_xmp_sidecar(path: &Path) -> ExifRead {
    let Ok(bytes) = std::fs::read(path) else {
        return ExifRead::default();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lat = match_float(&text, xmp_lat_elem_re(), xmp_lat_attr_re());
    let lng = match_float(&text, xmp_lng_elem_re(), xmp_lng_attr_re());
    ExifRead {
        gps_lat: lat,
        gps_lng: lng,
        source: "xmp".to_string(),
        ..Default::default()
    }
}

fn read_png_xmp_fallback(path: &Path) -> ExifRead {
    let Ok(bytes) = std::fs::read(path) else {
        return ExifRead::default();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lat = match_float(&text, xmp_lat_elem_re(), xmp_lat_attr_re());
    let lng = match_float(&text, xmp_lng_elem_re(), xmp_lng_attr_re());
    if lat.is_none() && lng.is_none() {
        ExifRead::default()
    } else {
        ExifRead {
            gps_lat: lat,
            gps_lng: lng,
            source: "xmp".to_string(),
            ..Default::default()
        }
    }
}

fn merge_sidecar(primary: ExifRead, sidecar: &ExifRead) -> ExifRead {
    let gps_lat = match primary.gps_lat {
        Some(v) if v.is_finite() => Some(v),
        _ => sidecar.gps_lat,
    };
    let gps_lng = match primary.gps_lng {
        Some(v) if v.is_finite() => Some(v),
        _ => sidecar.gps_lng,
    };
    let source = if primary.source != "none" {
        primary.source
    } else {
        sidecar.source.clone()
    };
    ExifRead {
        gps_lat,
        gps_lng,
        datetime_original: primary
            .datetime_original
            .or_else(|| sidecar.datetime_original.clone()),
        create_date: primary.create_date.or_else(|| sidecar.create_date.clone()),
        offset_time_original: primary
            .offset_time_original
            .or_else(|| sidecar.offset_time_original.clone()),
        make: primary.make.or_else(|| sidecar.make.clone()),
        model: primary.model.or_else(|| sidecar.model.clone()),
        orientation: primary.orientation,
        gps_img_direction: primary.gps_img_direction,
        gps_altitude: primary.gps_altitude,
        source,
    }
}

fn apply_ref(coord: Option<f64>, ref_val: Option<&Value>, negative: char) -> Option<f64> {
    let c = coord?;
    let Some(text) = as_str(ref_val) else {
        return Some(c);
    };
    if text.to_ascii_uppercase().starts_with(negative) && c > 0.0 {
        Some(-c)
    } else {
        Some(c)
    }
}

fn as_float(val: Option<&Value>) -> Option<f64> {
    let v = val?;
    match v {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn as_str(val: Option<&Value>) -> Option<String> {
    let v = val?;
    let raw = match v {
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => return None,
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
    };
    let trimmed = raw.trim().trim_matches('\0').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn angular_distance_deg(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlmb = (lng2 - lng1).to_radians();
    let chord = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlmb / 2.0).sin().powi(2);
    (2.0 * chord.sqrt().min(1.0).asin()).to_degrees()
}

pub fn gps_matches(lat1: Option<f64>, lng1: Option<f64>, lat2: f64, lng2: f64) -> bool {
    match (lat1, lng1) {
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() => {
            angular_distance_deg(a, b, lat2, lng2) < GPS_MATCH_DEG
        }
        _ => false,
    }
}

pub fn read_dest_gps(path: &Path) -> (Option<f64>, Option<f64>) {
    if !path.is_file() {
        return (None, None);
    }
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = resolved.to_string_lossy().into_owned();
    let result = read_exif_batch(&[resolved]);
    let tags = result.by_path.get(&key).cloned().unwrap_or_default();
    (tags.gps_lat, tags.gps_lng)
}

pub fn dest_gps_matches(path: &Path, lat: f64, lng: f64) -> bool {
    let (got_lat, got_lng) = read_dest_gps(path);
    gps_matches(got_lat, got_lng, lat, lng)
}

pub fn gps_instructions(job_id: &str, decision_id: &str) -> String {
    format!("Geotagged by gpsify job {job_id} decision {decision_id}")
}

fn fmt_coord(value: f64) -> String {
    let formatted = format!("{value:.8}");
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "-0" || trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn lat_ref(lat: f64) -> &'static str {
    if lat >= 0.0 {
        "N"
    } else {
        "S"
    }
}

fn lng_ref(lng: f64) -> &'static str {
    if lng >= 0.0 {
        "E"
    } else {
        "W"
    }
}

fn argfile_lines(item: &GpsWrite) -> Result<Vec<String>, ExifWriteError> {
    let dest_raw = item.dest.to_string_lossy();
    if dest_raw.is_empty()
        || has_unsafe_path_chars(&dest_raw)
        || dest_raw.as_ref() != dest_raw.trim()
        || has_unsafe_path_chars(&item.job_id)
        || has_unsafe_path_chars(&item.decision_id)
    {
        return Err(ExifWriteError(
            "unsafe path or identifier for exiftool argfile".to_string(),
        ));
    }
    let dest_arg = if item.dest.is_absolute() {
        dest_raw.into_owned()
    } else {
        format!("./{dest_raw}")
    };
    let lat_s = fmt_coord(item.lat);
    let lng_s = fmt_coord(item.lng);
    let instructions = gps_instructions(&item.job_id, &item.decision_id);

    let mut lines = vec![
        "-charset".to_string(),
        "filename=utf8".to_string(),
        "-n".to_string(),
    ];
    match item.kind {
        MediaKind::Jpeg | MediaKind::Heic => {
            lines.extend([
                format!("-GPSLatitude={lat_s}"),
                format!("-GPSLatitudeRef={}", lat_ref(item.lat)),
                format!("-GPSLongitude={lng_s}"),
                format!("-GPSLongitudeRef={}", lng_ref(item.lng)),
                "-GPSMapDatum=WGS-84".to_string(),
                "-GPSVersionID=2 3 0 0".to_string(),
                format!("-XMP:GPSLatitude={lat_s}"),
                format!("-XMP:GPSLongitude={lng_s}"),
                "-XMP-dc:Source=gpsify".to_string(),
                format!("-XMP-photoshop:Instructions={instructions}"),
                "-overwrite_original".to_string(),
                dest_arg,
            ]);
        }
        MediaKind::Png | MediaKind::Raw => {
            lines.extend([
                format!("-XMP:GPSLatitude={lat_s}"),
                format!("-XMP:GPSLongitude={lng_s}"),
                "-XMP-dc:Source=gpsify".to_string(),
                format!("-XMP-photoshop:Instructions={instructions}"),
                "-overwrite_original".to_string(),
                dest_arg,
            ]);
        }
        MediaKind::Unsupported => {
            return Err(ExifWriteError(format!(
                "cannot write GPS for {}",
                item.dest.display()
            )));
        }
    }
    Ok(lines)
}

pub fn render_exiftool_argfile(items: &[GpsWrite]) -> Result<String, ExifWriteError> {
    if items.is_empty() {
        return Ok(String::new());
    }
    let mut blocks = Vec::with_capacity(items.len());
    for item in items {
        blocks.push(argfile_lines(item)?.join("\n"));
    }
    Ok(format!("{}\n", blocks.join("\n-execute\n")))
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn xmp_packet(lat: f64, lng: f64, instructions: &str) -> String {
    let lat_s = fmt_coord(lat);
    let lng_s = fmt_coord(lng);
    let inst = xml_escape(instructions);
    format!(
        concat!(
            "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <rdf:Description rdf:about=\"\"\n",
            "   xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n",
            "   xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n",
            "   xmlns:photoshop=\"http://ns.adobe.com/photoshop/1.0/\"\n",
            "   exif:GPSLatitude=\"{}\"\n",
            "   exif:GPSLongitude=\"{}\"\n",
            "   dc:Source=\"gpsify\"\n",
            "   photoshop:Instructions=\"{}\"/>\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
            "<?xpacket end=\"w\"?>\n"
        ),
        lat_s, lng_s, inst
    )
}

fn seed_missing_xmp_sidecar(item: &GpsWrite) -> Result<(), ExifWriteError> {
    if item.dest.is_file() {
        return Ok(());
    }
    if let Some(parent) = item.dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ExifWriteError(e.to_string()))?;
    }
    let packet = xmp_packet(
        item.lat,
        item.lng,
        &gps_instructions(&item.job_id, &item.decision_id),
    );
    let parent = item.dest.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| ExifWriteError(e.to_string()))?;
    tmp.write_all(packet.as_bytes())
        .map_err(|e| ExifWriteError(e.to_string()))?;
    tmp.persist(&item.dest)
        .map_err(|e| ExifWriteError(e.error.to_string()))?;
    Ok(())
}

pub fn write_gps_batch(
    items: &[GpsWrite],
) -> Result<BTreeMap<String, Option<String>>, ExifWriteError> {
    write_gps_batch_with(items, !writes_blocked(), exiftool_available())
}

pub fn write_gps_batch_with(
    items: &[GpsWrite],
    allow_writes: bool,
    has_exiftool: bool,
) -> Result<BTreeMap<String, Option<String>>, ExifWriteError> {
    if !allow_writes {
        return Err(ExifWriteError(DRY_RUN_BLOCKS_WRITES.to_string()));
    }
    if items.is_empty() {
        return Ok(BTreeMap::new());
    }
    if !has_exiftool {
        return Err(ExifWriteError(WRITE_REQUIRES_EXIFTOOL.to_string()));
    }

    for item in items {
        if item.kind == MediaKind::Raw && !item.dest.is_file() {
            seed_missing_xmp_sidecar(item)?;
        }
    }

    let body = render_exiftool_argfile(items)?;
    let mut tmp = NamedTempFile::with_suffix(".args").map_err(|e| ExifWriteError(e.to_string()))?;
    tmp.write_all(body.as_bytes())
        .map_err(|e| ExifWriteError(e.to_string()))?;
    tmp.flush().map_err(|e| ExifWriteError(e.to_string()))?;
    let args_path = tmp.path().to_path_buf();

    let mut cmd = Command::new("exiftool");
    cmd.arg("-@").arg(&args_path);
    let completed =
        run_command_with_timeout(&mut cmd, Duration::from_secs(EXIFTOOL_WRITE_TIMEOUT_S))
            .map_err(|e| ExifWriteError(format!("exiftool write failed: {e}")))?;

    let code = completed.status.code().unwrap_or(-1);
    if code != 0 && code != 1 {
        let stderr = String::from_utf8_lossy(&completed.stderr)
            .trim()
            .to_string();
        return Err(ExifWriteError(format!(
            "exiftool write exit {code}: {stderr}"
        )));
    }

    let mut errors = BTreeMap::new();
    for item in items {
        let key = item.dest.to_string_lossy().into_owned();
        if dest_gps_matches(&item.dest, item.lat, item.lng) {
            errors.insert(key, None);
        } else {
            errors.insert(key, Some("gps_verify_failed".to_string()));
        }
    }
    Ok(errors)
}
