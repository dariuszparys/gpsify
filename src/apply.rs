//! Write GPS from the current decision only (K5, K12, K15, K17).
//!
//! Dest inspection runs before any ingest-hash abort so `--in-place` JPEG retry
//! works after tags change the source bytes. `photo.sha256` stays the pre-tag
//! hash. Receipts upsert on `(photo_id, dest_path)`. RAW dest is the sibling
//! `.xmp`; RAW bytes are never rewritten.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::config::expand_user_path;
use crate::exif::{
    classify_media, dest_gps_matches, exiftool_available, has_unsafe_path_chars,
    is_photos_library, render_exiftool_argfile, write_gps_batch_with, writes_blocked,
    ExifWriteError, GpsWrite, MediaKind, HEIC_WRITE_REQUIRES_EXIFTOOL, WRITE_REQUIRES_EXIFTOOL,
};
use crate::media::hash_file;
use crate::store::{
    backups_root, copies_root, job_dir, utc_now_iso, DecisionRow, JobExclusiveLock, JobRepo,
    PhotoRow, StoreError, UpsertWriteReceipt,
};
use crate::types::{DecisionAction, PhotoStatus, WriteReceiptStatus};

pub const PNG_XMP_ONLY_WARNING: &str = "png_xmp_only";
pub const SOURCE_MISSING: &str = "source_missing";
pub const SOURCE_CHANGED: &str = "source_changed";
pub const DEST_DIVERGED: &str = "dest_diverged";
pub const NULL_COORDS: &str = "decision_coords_missing";
pub const UNSAFE_PATH: &str = "unsafe_path";

const LOCK_FILENAME: &str = "propose.lock";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ApplyError(pub String);

impl From<StoreError> for ApplyError {
    fn from(err: StoreError) -> Self {
        Self(err.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    pub out: Option<PathBuf>,
    pub in_place: bool,
    pub dry_run: bool,
    pub i_mean_it: bool,
    /// When `Some`, overrides `GPSIFY_DRY_RUN=1` write block (for `writes_exif` tests).
    pub allow_writes_override: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplySummary {
    pub job_id: String,
    pub dest_root: String,
    pub in_place: bool,
    pub dry_run: bool,
    pub wrote: usize,
    pub failed: usize,
    pub skipped: usize,
    pub eligible: usize,
    pub argfile: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct QueuedWrite {
    photo: PhotoRow,
    decision: DecisionRow,
    kind: MediaKind,
    dest: PathBuf,
    lat: f64,
    lng: f64,
    in_place: bool,
    backup_path: Option<String>,
}

pub fn backup_path_for(data_dir: &Path, job_id: &str, photo_id: &str, kind: MediaKind) -> PathBuf {
    let root = backups_root(data_dir, job_id);
    match kind {
        MediaKind::Raw => root.join(format!("{photo_id}.xmp")),
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic | MediaKind::Unsupported => {
            root.join(format!("{photo_id}.bin"))
        }
    }
}

/// Return `(receipt_dest, image_dest)`. RAW receipt dest is the sidecar.
pub fn resolve_dests(
    photo: &PhotoRow,
    kind: MediaKind,
    dest_root: &Path,
    in_place: bool,
) -> (PathBuf, PathBuf) {
    let source = PathBuf::from(&photo.path);
    let rel = PathBuf::from(&photo.relpath);
    let image = if in_place { source } else { dest_root.join(rel) };
    match kind {
        MediaKind::Raw => (image.with_extension("xmp"), image),
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic | MediaKind::Unsupported => {
            (image.clone(), image)
        }
    }
}

/// Approximate `Path.resolve(strict=False)` by canonicalizing the deepest existing ancestor.
fn resolve_nonexistent(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(canon) = std::fs::canonicalize(cur) {
            let mut out = canon;
            for part in tail.into_iter().rev() {
                match part {
                    Component::Normal(s) => out.push(s),
                    Component::ParentDir => {
                        out.pop();
                    }
                    Component::CurDir => {}
                    Component::RootDir | Component::Prefix(_) => {}
                }
            }
            return out;
        }
        let Some(parent) = cur.parent() else {
            return path.to_path_buf();
        };
        if let Some(name) = cur.file_name() {
            tail.push(Component::Normal(name));
        }
        cur = parent;
    }
}

fn is_symlink_path(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn validate_apply_paths(
    photo: &PhotoRow,
    job_root_canonical: &Path,
    dest_root: &Path,
    dest: &Path,
    image_dest: &Path,
    in_place: bool,
) -> Option<&'static str> {
    if photo.relpath.is_empty()
        || has_unsafe_path_chars(&photo.relpath)
        || has_unsafe_path_chars(&photo.path)
    {
        return Some(UNSAFE_PATH);
    }
    let rel = Path::new(&photo.relpath);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return Some(UNSAFE_PATH);
    }
    let source = Path::new(&photo.path);
    if is_symlink_path(source)
        || is_photos_library(source)
        || is_symlink_path(dest)
        || is_symlink_path(image_dest)
        || is_photos_library(dest)
        || is_photos_library(image_dest)
    {
        return Some(UNSAFE_PATH);
    }
    let dest_str = dest.to_string_lossy();
    if dest_str.as_ref() != dest_str.trim() {
        return Some(UNSAFE_PATH);
    }
    if in_place {
        let root_resolved = resolve_nonexistent(job_root_canonical);
        if !resolve_nonexistent(source).starts_with(&root_resolved) {
            return Some(UNSAFE_PATH);
        }
        if !resolve_nonexistent(dest).starts_with(&root_resolved) {
            return Some(UNSAFE_PATH);
        }
    } else {
        let dest_root_resolved = resolve_nonexistent(dest_root);
        if !resolve_nonexistent(dest).starts_with(&dest_root_resolved) {
            return Some(UNSAFE_PATH);
        }
        if !resolve_nonexistent(image_dest).starts_with(&dest_root_resolved) {
            return Some(UNSAFE_PATH);
        }
    }
    None
}

fn safe_hash(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    hash_file(path).ok()
}

fn copy_file(source: &Path, dest: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, dest)?;
    Ok(())
}

pub fn run_apply<F>(
    job_id: &str,
    data_dir: &Path,
    opts: ApplyOptions,
    mut progress: Option<F>,
) -> Result<ApplySummary, ApplyError>
where
    F: FnMut(&str),
{
    if opts.out.is_some() && opts.in_place {
        return Err(ApplyError(
            "--out and --in-place are mutually exclusive".to_string(),
        ));
    }
    if let Some(ref out) = opts.out {
        let out_raw = out.to_string_lossy();
        if has_unsafe_path_chars(&out_raw) || out_raw.as_ref() != out_raw.trim() {
            return Err(ApplyError(
                "--out path contains unsupported control or leading/trailing whitespace characters"
                    .to_string(),
            ));
        }
    }
    if opts.in_place && !opts.dry_run && !opts.i_mean_it {
        return Err(ApplyError(
            "--in-place requires --i-mean-it or an interactive confirm".to_string(),
        ));
    }

    let root = job_dir(data_dir, job_id);
    let dest_root = match &opts.out {
        Some(out) => resolve_nonexistent(&expand_user_path(out)),
        None => copies_root(data_dir, job_id),
    };
    if opts.out.is_some() && is_photos_library(&dest_root) {
        return Err(ApplyError(
            "refusing to write into a Photos.app library bundle (*.photoslibrary)".to_string(),
        ));
    }

    let mut summary = ApplySummary {
        job_id: job_id.to_string(),
        dest_root: dest_root.to_string_lossy().into_owned(),
        in_place: opts.in_place,
        dry_run: opts.dry_run,
        wrote: 0,
        failed: 0,
        skipped: 0,
        eligible: 0,
        argfile: String::new(),
        warnings: Vec::new(),
    };

    let mut lock = JobExclusiveLock::new(root.join(LOCK_FILENAME));
    lock.acquire()?;

    let repo = JobRepo::open(data_dir, job_id)?;
    let job = repo
        .get_job()?
        .ok_or_else(|| ApplyError(format!("job row missing for {job_id}")))?;
    if opts.in_place {
        summary.dest_root = job.root_path.clone();
    }
    let job_root_canonical = PathBuf::from(&job.root_canonical);

    let photos = repo.list_photos()?;
    let decisions: BTreeMap<String, DecisionRow> = repo
        .list_current_decisions()?
        .into_iter()
        .map(|d| (d.photo_id.clone(), d))
        .collect();

    let mut queued: Vec<QueuedWrite> = Vec::new();
    let mut saw_png = false;

    for photo in &photos {
        let kind = classify_media(Path::new(&photo.relpath));
        let decision = decisions.get(&photo.id);
        let (dest, image_dest) = resolve_dests(photo, kind, &dest_root, opts.in_place);
        match inspect_one(
            &repo,
            photo,
            decision,
            kind,
            &job_root_canonical,
            &dest_root,
            &dest,
            &image_dest,
            data_dir,
            job_id,
            opts.in_place,
            opts.dry_run,
        )? {
            InspectOutcome::Skip => {
                summary.skipped += 1;
            }
            InspectOutcome::Failed => {
                summary.failed += 1;
            }
            InspectOutcome::Ready(item) => {
                if kind == MediaKind::Png {
                    saw_png = true;
                }
                if opts.dry_run {
                    queued.push(item);
                    continue;
                }
                if dest_gps_matches(&item.dest, item.lat, item.lng) {
                    mark_ok(&repo, &item)?;
                    summary.wrote += 1;
                    continue;
                }
                queued.push(item);
            }
        }
    }

    summary.eligible = summary.wrote + summary.failed + queued.len();
    let gps_writes: Vec<GpsWrite> = queued
        .iter()
        .map(|item| GpsWrite {
            dest: item.dest.clone(),
            lat: item.lat,
            lng: item.lng,
            kind: item.kind,
            job_id: job_id.to_string(),
            decision_id: item.decision.id.clone(),
        })
        .collect();
    summary.argfile = render_exiftool_argfile(&gps_writes)
        .map_err(|e| ApplyError(e.to_string()))?;

    if opts.dry_run {
        if let Some(ref mut cb) = progress {
            let trimmed = summary.argfile.trim_end();
            cb(if trimmed.is_empty() {
                "(no writes)"
            } else {
                trimmed
            });
        }
        write_apply_stats(&repo, &summary)?;
        return Ok(summary);
    }

    if !queued.is_empty() {
        let has_tool = exiftool_available();
        if !has_tool {
            for item in &queued {
                let msg = if item.kind == MediaKind::Heic {
                    HEIC_WRITE_REQUIRES_EXIFTOOL
                } else {
                    WRITE_REQUIRES_EXIFTOOL
                };
                mark_failed(&repo, item, msg)?;
                summary.failed += 1;
            }
        } else {
            let allow_writes = opts
                .allow_writes_override
                .unwrap_or_else(|| !writes_blocked());
            match write_gps_batch_with(&gps_writes, allow_writes, true) {
                Err(ExifWriteError(err)) => {
                    for item in &queued {
                        mark_failed(&repo, item, &err)?;
                        summary.failed += 1;
                    }
                }
                Ok(errors) => {
                    for item in &queued {
                        let key = item.dest.to_string_lossy().into_owned();
                        match errors.get(&key).and_then(|e| e.as_deref()) {
                            None => {
                                mark_ok(&repo, item)?;
                                summary.wrote += 1;
                            }
                            Some(err) => {
                                mark_failed(&repo, item, err)?;
                                summary.failed += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    if saw_png && summary.wrote > 0 {
        summary.warnings.push(PNG_XMP_ONLY_WARNING.to_string());
    }
    if let Some(ref mut cb) = progress {
        cb(&format!(
            "wrote: {}  failed: {}  skipped: {}  dest: {}",
            summary.wrote, summary.failed, summary.skipped, summary.dest_root
        ));
        for w in &summary.warnings {
            cb(&format!("warning: {w}"));
        }
    }
    write_apply_stats(&repo, &summary)?;
    Ok(summary)
}

enum InspectOutcome {
    Skip,
    Failed,
    Ready(QueuedWrite),
}

#[allow(clippy::too_many_arguments)]
fn inspect_one(
    repo: &JobRepo,
    photo: &PhotoRow,
    decision: Option<&DecisionRow>,
    kind: MediaKind,
    job_root_canonical: &Path,
    dest_root: &Path,
    dest: &Path,
    image_dest: &Path,
    data_dir: &Path,
    job_id: &str,
    in_place: bool,
    dry_run: bool,
) -> Result<InspectOutcome, ApplyError> {
    let Some(decision) = decision else {
        return Ok(InspectOutcome::Skip);
    };
    let Ok(action) = DecisionAction::from_str(&decision.action) else {
        return Ok(InspectOutcome::Skip);
    };
    if !action.is_writeable() {
        return Ok(InspectOutcome::Skip);
    }

    let (Some(lat), Some(lng)) = (decision.lat, decision.lng) else {
        if !dry_run {
            fail_receipt(repo, photo, decision, dest, in_place, NULL_COORDS, None)?;
        }
        return Ok(InspectOutcome::Failed);
    };
    if !lat.is_finite() || !lng.is_finite() {
        if !dry_run {
            fail_receipt(repo, photo, decision, dest, in_place, NULL_COORDS, None)?;
        }
        return Ok(InspectOutcome::Failed);
    }

    if kind == MediaKind::Unsupported {
        if !dry_run {
            fail_receipt(
                repo,
                photo,
                decision,
                dest,
                in_place,
                "unsupported_media",
                None,
            )?;
        }
        return Ok(InspectOutcome::Failed);
    }

    if let Some(path_err) =
        validate_apply_paths(photo, job_root_canonical, dest_root, dest, image_dest, in_place)
    {
        if !dry_run {
            fail_receipt(repo, photo, decision, dest, in_place, path_err, None)?;
        }
        return Ok(InspectOutcome::Failed);
    }

    let existing = repo.get_write_receipt(&photo.id, &dest.to_string_lossy())?;
    let backup_existing = existing.as_ref().and_then(|r| r.backup_path.clone());

    if dry_run {
        return Ok(InspectOutcome::Ready(QueuedWrite {
            photo: photo.clone(),
            decision: decision.clone(),
            kind,
            dest: dest.to_path_buf(),
            lat,
            lng,
            in_place,
            backup_path: backup_existing,
        }));
    }

    repo.upsert_write_receipt(UpsertWriteReceipt {
        photo_id: photo.id.clone(),
        decision_id: decision.id.clone(),
        dest_path: dest.to_string_lossy().into_owned(),
        in_place,
        status: WriteReceiptStatus::Pending,
        error: None,
        written_at: None,
        written_sha256: None,
        backup_path: backup_existing.clone(),
        receipt_id: existing.map(|r| r.id),
    })?;

    if dest.is_file() && dest_gps_matches(dest, lat, lng) {
        return Ok(InspectOutcome::Ready(QueuedWrite {
            photo: photo.clone(),
            decision: decision.clone(),
            kind,
            dest: dest.to_path_buf(),
            lat,
            lng,
            in_place,
            backup_path: backup_existing,
        }));
    }

    let source = Path::new(&photo.path);
    let ingest_hash = &photo.sha256;

    if !dest.is_file() {
        if let Some(err) =
            handle_missing_dest(kind, dest, image_dest, source, ingest_hash, in_place)?
        {
            fail_receipt(
                repo,
                photo,
                decision,
                dest,
                in_place,
                err,
                backup_existing.as_deref(),
            )?;
            return Ok(InspectOutcome::Failed);
        }
    } else if let Some(err) =
        handle_mismatch_dest(kind, dest, ingest_hash, in_place, backup_existing.as_deref())?
    {
        fail_receipt(
            repo,
            photo,
            decision,
            dest,
            in_place,
            err,
            backup_existing.as_deref(),
        )?;
        return Ok(InspectOutcome::Failed);
    }

    let backup = backup_before_write(
        kind,
        dest,
        data_dir,
        job_id,
        &photo.id,
        backup_existing.as_deref(),
    )?;
    repo.upsert_write_receipt(UpsertWriteReceipt {
        photo_id: photo.id.clone(),
        decision_id: decision.id.clone(),
        dest_path: dest.to_string_lossy().into_owned(),
        in_place,
        status: WriteReceiptStatus::Pending,
        error: None,
        written_at: None,
        written_sha256: None,
        backup_path: backup.clone(),
        receipt_id: None,
    })?;

    Ok(InspectOutcome::Ready(QueuedWrite {
        photo: photo.clone(),
        decision: decision.clone(),
        kind,
        dest: dest.to_path_buf(),
        lat,
        lng,
        in_place,
        backup_path: backup,
    }))
}

fn handle_missing_dest(
    kind: MediaKind,
    dest: &Path,
    image_dest: &Path,
    source: &Path,
    ingest_hash: &str,
    in_place: bool,
) -> Result<Option<&'static str>, ApplyError> {
    match kind {
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic => {
            if in_place || !source.is_file() {
                return Ok(Some(SOURCE_MISSING));
            }
            if safe_hash(source).as_deref() != Some(ingest_hash) {
                return Ok(Some(SOURCE_CHANGED));
            }
            copy_file(source, dest).map_err(|e| ApplyError(e.to_string()))?;
            Ok(None)
        }
        MediaKind::Raw => {
            if !source.is_file() {
                return Ok(Some(SOURCE_MISSING));
            }
            if safe_hash(source).as_deref() != Some(ingest_hash) {
                return Ok(Some(SOURCE_CHANGED));
            }
            if in_place {
                return Ok(None);
            }
            if !image_dest.is_file() {
                copy_file(source, image_dest).map_err(|e| ApplyError(e.to_string()))?;
            } else if safe_hash(image_dest).as_deref() != Some(ingest_hash) {
                return Ok(Some(DEST_DIVERGED));
            }
            Ok(None)
        }
        MediaKind::Unsupported => Ok(Some("unsupported_media")),
    }
}

fn handle_mismatch_dest(
    kind: MediaKind,
    dest: &Path,
    ingest_hash: &str,
    in_place: bool,
    backup_existing: Option<&str>,
) -> Result<Option<&'static str>, ApplyError> {
    if let Some(backup_str) = backup_existing {
        let backup = Path::new(backup_str);
        if backup.is_file() {
            copy_file(backup, dest).map_err(|e| ApplyError(e.to_string()))?;
            return Ok(None);
        }
    }
    let dest_hash = safe_hash(dest);
    match kind {
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic => {
            if dest_hash.as_deref() == Some(ingest_hash) {
                Ok(None)
            } else if in_place {
                Ok(Some(SOURCE_CHANGED))
            } else {
                Ok(Some(DEST_DIVERGED))
            }
        }
        MediaKind::Raw => Ok(None),
        MediaKind::Unsupported => Ok(Some("unsupported_media")),
    }
}

fn backup_before_write(
    kind: MediaKind,
    dest: &Path,
    data_dir: &Path,
    job_id: &str,
    photo_id: &str,
    previous: Option<&str>,
) -> Result<Option<String>, ApplyError> {
    match kind {
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic => {
            if dest.is_file() {
                let backup = backup_path_for(data_dir, job_id, photo_id, kind);
                copy_file(dest, &backup).map_err(|e| ApplyError(e.to_string()))?;
                Ok(Some(backup.to_string_lossy().into_owned()))
            } else {
                Ok(previous.map(str::to_string))
            }
        }
        MediaKind::Raw => {
            if dest.is_file() {
                let backup = backup_path_for(data_dir, job_id, photo_id, kind);
                copy_file(dest, &backup).map_err(|e| ApplyError(e.to_string()))?;
                Ok(Some(backup.to_string_lossy().into_owned()))
            } else {
                Ok(None)
            }
        }
        MediaKind::Unsupported => Ok(previous.map(str::to_string)),
    }
}

fn mark_ok(repo: &JobRepo, item: &QueuedWrite) -> Result<(), ApplyError> {
    let digest = hash_file(&item.dest).map_err(|e| ApplyError(e.to_string()))?;
    repo.upsert_write_receipt(UpsertWriteReceipt {
        photo_id: item.photo.id.clone(),
        decision_id: item.decision.id.clone(),
        dest_path: item.dest.to_string_lossy().into_owned(),
        in_place: item.in_place,
        status: WriteReceiptStatus::Ok,
        error: None,
        written_at: Some(utc_now_iso()),
        written_sha256: Some(digest),
        backup_path: item.backup_path.clone(),
        receipt_id: None,
    })?;
    repo.set_photo_status(&item.photo.id, PhotoStatus::Written, None, None)?;
    Ok(())
}

fn mark_failed(repo: &JobRepo, item: &QueuedWrite, error: &str) -> Result<(), ApplyError> {
    fail_receipt(
        repo,
        &item.photo,
        &item.decision,
        &item.dest,
        item.in_place,
        error,
        item.backup_path.as_deref(),
    )
}

fn fail_receipt(
    repo: &JobRepo,
    photo: &PhotoRow,
    decision: &DecisionRow,
    dest: &Path,
    in_place: bool,
    error: &str,
    backup_path: Option<&str>,
) -> Result<(), ApplyError> {
    let dest_str = dest.to_string_lossy().into_owned();
    let existing = repo.get_write_receipt(&photo.id, &dest_str)?;
    let keep_backup = backup_path
        .map(str::to_string)
        .or_else(|| existing.and_then(|r| r.backup_path));
    repo.upsert_write_receipt(UpsertWriteReceipt {
        photo_id: photo.id.clone(),
        decision_id: decision.id.clone(),
        dest_path: dest_str,
        in_place,
        status: WriteReceiptStatus::Failed,
        error: Some(error.to_string()),
        written_at: None,
        written_sha256: None,
        backup_path: keep_backup,
        receipt_id: None,
    })?;
    repo.set_photo_status(&photo.id, PhotoStatus::WriteFailed, Some(error), None)?;
    Ok(())
}

fn write_apply_stats(repo: &JobRepo, summary: &ApplySummary) -> Result<(), ApplyError> {
    let mut existing: Map<String, Value> = match repo.get_job()? {
        Some(job) if !job.stats_json.trim().is_empty() => {
            match serde_json::from_str::<Value>(&job.stats_json) {
                Ok(Value::Object(map)) => map,
                _ => Map::new(),
            }
        }
        _ => Map::new(),
    };
    existing.insert(
        "apply".to_string(),
        json!({
            "wrote": summary.wrote,
            "failed": summary.failed,
            "skipped": summary.skipped,
            "dest_root": summary.dest_root,
            "in_place": summary.in_place,
            "dry_run": summary.dry_run,
        }),
    );
    repo.set_stats_json(&Value::Object(existing).to_string())?;
    Ok(())
}
