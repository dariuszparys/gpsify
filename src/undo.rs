//! Undo GPS writes from backup / dest deletion (K15).
//!
//! JPEG/HEIC/PNG: restore the full-file backup over an in-place dest, or delete
//! the copy-mode dest. RAW: delete or restore the sibling XMP sidecar only; never
//! touch RAW bytes. Receipts stay upsertable (status `undone`, row not deleted).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use thiserror::Error;

use crate::config::expand_user_path;
use crate::exif::{classify_media, is_photos_library, MediaKind};
use crate::media::hash_file;
use crate::store::{
    job_dir, JobExclusiveLock, JobRepo, PhotoRow, StoreError, UpsertWriteReceipt, WriteReceiptRow,
};
use crate::types::{DecisionAction, PhotoStatus, WriteReceiptStatus};

const LOCK_FILENAME: &str = "propose.lock";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct UndoError(pub String);

impl From<StoreError> for UndoError {
    fn from(err: StoreError) -> Self {
        Self(err.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoSummary {
    pub job_id: String,
    pub undone: usize,
    pub failed: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
}

fn photo_status_after_undo(action: &str) -> PhotoStatus {
    match DecisionAction::from_str(action) {
        Ok(DecisionAction::Accept) | Err(_) => PhotoStatus::Accepted,
        Ok(DecisionAction::AcceptEdited | DecisionAction::ClusterApply) => {
            PhotoStatus::AcceptedEdited
        }
        Ok(DecisionAction::Skip) => PhotoStatus::SkippedByUser,
    }
}

fn is_symlink_path(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

pub fn run_undo<F>(
    job_id: &str,
    data_dir: &Path,
    photo_filter: Option<&Path>,
    mut progress: Option<F>,
) -> Result<UndoSummary, UndoError>
where
    F: FnMut(&str),
{
    let root = job_dir(data_dir, job_id);
    let mut lock = JobExclusiveLock::new(root.join(LOCK_FILENAME));
    lock.acquire()?;

    let repo = JobRepo::open(data_dir, job_id)?;
    let photos: BTreeMap<String, PhotoRow> = repo
        .list_photos()?
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect();
    let receipts = repo.list_write_receipts()?;

    let wanted_photo_id = match photo_filter {
        Some(needle) => Some(
            match_photo(&photos, needle)
                .ok_or_else(|| UndoError(format!("no photo matching {}", needle.display())))?,
        ),
        None => None,
    };

    let mut summary = UndoSummary {
        job_id: job_id.to_string(),
        undone: 0,
        failed: 0,
        skipped: 0,
        warnings: Vec::new(),
    };

    for receipt in &receipts {
        if let Some(ref wanted) = wanted_photo_id {
            if &receipt.photo_id != wanted {
                continue;
            }
        }
        let Ok(status_enum) = WriteReceiptStatus::from_str(&receipt.status) else {
            summary.skipped += 1;
            continue;
        };
        match status_enum {
            WriteReceiptStatus::Ok => {}
            WriteReceiptStatus::Pending
            | WriteReceiptStatus::Failed
            | WriteReceiptStatus::Undone => {
                summary.skipped += 1;
                continue;
            }
        }

        let Some(row) = photos.get(&receipt.photo_id) else {
            summary.failed += 1;
            continue;
        };

        let error: Option<String> = match undo_dest(row, receipt) {
            Ok(err) => err.map(str::to_string),
            Err(exc) => Some(format!("undo_io_error: {exc}")),
        };
        if let Some(err) = error {
            summary
                .warnings
                .push(format!("{}: {err}", receipt.photo_id));
            summary.failed += 1;
            continue;
        }

        repo.upsert_write_receipt(UpsertWriteReceipt {
            photo_id: receipt.photo_id.clone(),
            decision_id: receipt.decision_id.clone(),
            dest_path: receipt.dest_path.clone(),
            in_place: receipt.in_place != 0,
            status: WriteReceiptStatus::Undone,
            error: None,
            written_at: receipt.written_at.clone(),
            written_sha256: receipt.written_sha256.clone(),
            backup_path: receipt.backup_path.clone(),
            receipt_id: Some(receipt.id.clone()),
        })?;
        summary.undone += 1;
    }

    restore_photo_statuses(&repo, &photos)?;
    if let Some(ref mut cb) = progress {
        for w in &summary.warnings {
            cb(&format!("warning: {w}"));
        }
        cb(&format!(
            "undone: {}  failed: {}  skipped: {}",
            summary.undone, summary.failed, summary.skipped
        ));
    }
    Ok(summary)
}

fn match_photo(photos: &BTreeMap<String, PhotoRow>, needle: &Path) -> Option<String> {
    let expanded = expand_user_path(needle).to_string_lossy().into_owned();
    let needle_str = needle.to_string_lossy();
    let name = needle
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    for row in photos.values() {
        if row.path == expanded || row.relpath == expanded {
            return Some(row.id.clone());
        }
        if row.path == needle_str.as_ref() || row.relpath == needle_str.as_ref() {
            return Some(row.id.clone());
        }
        if !name.is_empty()
            && Path::new(&row.path).file_name().and_then(|n| n.to_str()) == Some(name)
        {
            let matches: Vec<&PhotoRow> = photos
                .values()
                .filter(|item| {
                    Path::new(&item.path).file_name().and_then(|n| n.to_str()) == Some(name)
                        || item.relpath == name
                })
                .collect();
            if matches.len() == 1 {
                return Some(matches[0].id.clone());
            }
        }
    }
    for row in photos.values() {
        if !name.is_empty() && row.relpath == name {
            return Some(row.id.clone());
        }
    }
    None
}

pub fn undo_dest(
    photo: &PhotoRow,
    receipt: &WriteReceiptRow,
) -> Result<Option<&'static str>, std::io::Error> {
    let dest = Path::new(&receipt.dest_path);
    let in_place = receipt.in_place != 0;
    let kind = classify_media(Path::new(&photo.relpath));
    let backup = receipt.backup_path.as_deref().map(Path::new);

    if is_symlink_path(dest) || is_photos_library(dest) {
        return Ok(Some("unsafe_path"));
    }
    if let Some(b) = backup {
        if is_symlink_path(b) || is_photos_library(b) {
            return Ok(Some("unsafe_path"));
        }
    }
    if dest.is_file() {
        let Some(ref expected) = receipt.written_sha256 else {
            return Ok(Some("dest_diverged"));
        };
        if hash_file(dest)? != *expected {
            return Ok(Some("dest_diverged"));
        }
    }
    if kind == MediaKind::Raw && !in_place {
        let ext = Path::new(&photo.path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let image_copy = if ext.is_empty() {
            dest.to_path_buf()
        } else {
            dest.with_extension(ext)
        };
        if is_symlink_path(&image_copy) || is_photos_library(&image_copy) {
            return Ok(Some("unsafe_path"));
        }
        if image_copy.is_file() && hash_file(&image_copy)? != photo.sha256 {
            return Ok(Some("source_copy_diverged"));
        }
    }
    if kind == MediaKind::Raw {
        if let Some(b) = backup {
            if !b.is_file() {
                return Ok(Some("backup_missing"));
            }
        }
    }

    match kind {
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic => {
            if in_place {
                let Some(b) = backup else {
                    return Ok(Some("backup_missing"));
                };
                if !b.is_file() {
                    return Ok(Some("backup_missing"));
                }
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(b, dest)?;
                return Ok(None);
            }
            if dest.is_file() {
                std::fs::remove_file(dest)?;
            }
            Ok(None)
        }
        MediaKind::Raw => {
            if in_place {
                match backup {
                    Some(b) if b.is_file() => {
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::copy(b, dest)?;
                    }
                    _ => {
                        if dest.is_file() {
                            std::fs::remove_file(dest)?;
                        }
                    }
                }
                return Ok(None);
            }
            if dest.is_file() {
                std::fs::remove_file(dest)?;
            }
            let ext = Path::new(&photo.path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let image_copy = if ext.is_empty() {
                dest.to_path_buf()
            } else {
                dest.with_extension(ext)
            };
            if image_copy.is_file() && image_copy != Path::new(&photo.path) {
                std::fs::remove_file(image_copy)?;
            }
            Ok(None)
        }
        MediaKind::Unsupported => Ok(Some("unsupported_media")),
    }
}

fn restore_photo_statuses(
    repo: &JobRepo,
    photos: &BTreeMap<String, PhotoRow>,
) -> Result<(), UndoError> {
    let receipts = repo.list_write_receipts()?;
    let mut ok_by_photo = BTreeSet::new();
    for r in receipts {
        if r.status == WriteReceiptStatus::Ok.as_str() {
            ok_by_photo.insert(r.photo_id);
        }
    }
    for (photo_id, photo) in photos {
        if ok_by_photo.contains(photo_id) {
            continue;
        }
        if photo.status != PhotoStatus::Written.as_str() {
            continue;
        }
        let decision = repo.get_current_decision(photo_id)?;
        let action = decision
            .as_ref()
            .map(|d| d.action.as_str())
            .unwrap_or("accept");
        repo.set_photo_status(photo_id, photo_status_after_undo(action), None, None)?;
    }
    Ok(())
}
