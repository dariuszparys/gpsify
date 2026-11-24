//! Folder ingest: walk, classify, parallel SHA-256 hash, batched EXIF, cluster, upsert.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde_json::json;
use thiserror::Error;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::cluster::{plan_clusters, ClusterMember};
use crate::config::expand_user_path;
use crate::exif::{
    classify_media, exiftool_available, has_unsafe_path_chars, is_photos_library,
    is_photos_library_name, is_processable, read_exif_batch, ExifRead, MediaKind, XMP_EXTS,
};
use crate::media::hash_file;
use crate::store::{options_hash, JobIndex, JobRepo, NewPhoto, PhotoPatch, PhotoRow, StoreError};
use crate::types::PhotoStatus;

const IGNORE_NAMES: &[&str] = &[".DS_Store", "Thumbs.db"];

#[derive(Debug, Error)]
#[error("{0}")]
pub struct IngestError(pub String);

impl From<StoreError> for IngestError {
    fn from(err: StoreError) -> Self {
        Self(err.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestSummary {
    pub job_id: String,
    pub total: i64,
    pub tagged: i64,
    pub untagged: i64,
    pub skipped: i64,
    pub missing: i64,
    pub clusters: usize,
    pub other: Vec<(String, i64)>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct DiskFile {
    path: PathBuf,
    path_str: String,
    relpath: String,
    kind: MediaKind,
    sha256: String,
    size: i64,
    mtime_ns: i64,
    unreadable: bool,
}

#[derive(Debug, Clone)]
pub struct IngestOptions<'a> {
    pub hint: Option<&'a str>,
    pub recursive: bool,
    pub relocate: Option<&'a str>,
    pub data_dir: &'a Path,
}

impl<'a> IngestOptions<'a> {
    pub fn new(data_dir: &'a Path) -> Self {
        Self {
            hint: None,
            recursive: false,
            relocate: None,
            data_dir,
        }
    }
}

pub fn run_ingest(folder: &Path, opts: IngestOptions<'_>) -> Result<IngestSummary, IngestError> {
    let expanded = expand_user_path(folder);
    let root = std::fs::canonicalize(&expanded)
        .map_err(|_| IngestError(format!("folder not found: {}", folder.display())))?;
    if !root.is_dir() {
        return Err(IngestError(format!(
            "not a directory: {}",
            folder.display()
        )));
    }
    if is_photos_library(&root) {
        return Err(IngestError(
            "refusing to ingest a Photos.app library bundle (*.photoslibrary). Export originals first."
                .to_string(),
        ));
    }

    let root_canonical = root.to_string_lossy().into_owned();
    let opt_hash = options_hash(opts.recursive);
    let mut warnings = Vec::new();

    let disk_files = walk_and_hash(&root, opts.recursive);
    if !exiftool_available()
        && disk_files
            .iter()
            .any(|item| matches!(item.kind, MediaKind::Heic | MediaKind::Raw | MediaKind::Png))
    {
        return Err(IngestError(
            "exiftool is required to read HEIC, RAW, and PNG metadata safely. Install exiftool, then re-run ingest. Run gpsify doctor for diagnostics."
                .to_string(),
        ));
    }

    let index = JobIndex::open(opts.data_dir)?;
    let (repo, job_id) = if let Some(relocate_id) = opts.relocate {
        relocate_job(
            &index,
            opts.data_dir,
            relocate_id,
            &root,
            &root_canonical,
            opts.recursive,
            opts.hint,
        )?
    } else {
        open_or_create_job(
            &index,
            opts.data_dir,
            &root,
            &root_canonical,
            opts.recursive,
            &opt_hash,
            opts.hint,
        )?
    };

    if disk_files.is_empty() && !opts.recursive {
        let subdirs = count_walkable_subdirs(&root);
        if subdirs > 0 {
            let noun = if subdirs == 1 {
                "subdirectory"
            } else {
                "subdirectories"
            };
            warnings.push(format!(
                "no files at top level; {subdirs} {noun} found — did you mean --recursive?"
            ));
        }
    }

    let processable_paths: Vec<PathBuf> = disk_files
        .iter()
        .filter(|item| is_processable(item.kind) && !item.unreadable)
        .map(|item| item.path.clone())
        .collect();
    let exif_result = read_exif_batch(&processable_paths);
    warnings.extend(exif_result.warnings);

    upsert_photos(&repo, &disk_files, &exif_result.by_path)?;
    let cluster_count = rebuild_clusters(&repo)?;
    index.touch_ingest(&job_id, None)?;

    let counts = repo.count_photos_by_status()?;
    let total: i64 = counts.values().sum();
    let tagged = counts
        .get(PhotoStatus::SkippedHasGps.as_str())
        .copied()
        .unwrap_or(0);
    let untagged = counts
        .get(PhotoStatus::Pending.as_str())
        .copied()
        .unwrap_or(0);
    let skipped = counts
        .get(PhotoStatus::SkippedUnsupported.as_str())
        .copied()
        .unwrap_or(0)
        + counts
            .get(PhotoStatus::SkippedUnreadable.as_str())
            .copied()
            .unwrap_or(0)
        + counts
            .get(PhotoStatus::SkippedNoPreview.as_str())
            .copied()
            .unwrap_or(0);
    let missing = counts
        .get(PhotoStatus::Missing.as_str())
        .copied()
        .unwrap_or(0);

    Ok(IngestSummary {
        job_id,
        total,
        tagged,
        untagged,
        skipped,
        missing,
        clusters: cluster_count,
        other: other_counts(&counts),
        warnings,
    })
}

fn open_or_create_job(
    index: &JobIndex,
    data_dir: &Path,
    root: &Path,
    root_canonical: &str,
    recursive: bool,
    opt_hash: &str,
    hint: Option<&str>,
) -> Result<(JobRepo, String), IngestError> {
    let candidate_id = Uuid::new_v4().to_string();
    let row = index.register(&candidate_id, root_canonical, opt_hash, None)?;
    let job_id = row.job_id;
    if job_id == candidate_id {
        let config_json = json!({ "recursive": recursive }).to_string();
        let repo = JobRepo::create(
            data_dir,
            &job_id,
            &root.to_string_lossy(),
            root_canonical,
            opt_hash,
            hint,
            &config_json,
            None,
        )?;
        return Ok((repo, job_id));
    }
    let repo = JobRepo::open(data_dir, &job_id)?;
    if let Some(h) = hint {
        repo.set_hint(Some(h))?;
    }
    Ok((repo, job_id))
}

fn relocate_job(
    index: &JobIndex,
    data_dir: &Path,
    relocate_id: &str,
    root: &Path,
    root_canonical: &str,
    recursive: bool,
    hint: Option<&str>,
) -> Result<(JobRepo, String), IngestError> {
    let Some(existing) = index.get(relocate_id)? else {
        return Err(IngestError(format!("unknown job_id {relocate_id:?}")));
    };
    let expected_true = options_hash(true);
    let expected_false = options_hash(false);
    let stored_recursive = if existing.options_hash == expected_true {
        true
    } else if existing.options_hash == expected_false {
        false
    } else {
        recursive
    };
    if stored_recursive != recursive {
        return Err(IngestError(format!(
            "relocate keeps the original --recursive flag; this job was created with recursive={stored_recursive}"
        )));
    }
    index.relocate(relocate_id, root_canonical, None)?;
    let repo = JobRepo::open(data_dir, relocate_id)?;
    repo.set_root(&root.to_string_lossy(), root_canonical)?;
    if let Some(h) = hint {
        repo.set_hint(Some(h))?;
    }
    Ok((repo, relocate_id.to_string()))
}

fn iter_files(root: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if recursive {
        let iter = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                if entry.file_type().is_symlink() {
                    return false;
                }
                if entry.file_type().is_dir() {
                    let name = entry.file_name().to_string_lossy();
                    if name == ".gpsify" || is_photos_library_name(&name) {
                        return false;
                    }
                }
                true
            });
        for entry in iter.flatten() {
            let p = entry.path();
            if entry.file_type().is_file() && !entry.file_type().is_symlink() && !ignored(p, root) {
                found.push(p.to_path_buf());
            }
        }
    } else if let Ok(rd) = std::fs::read_dir(root) {
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if !meta.file_type().is_symlink() && meta.is_file() && !ignored(&p, root) {
                found.push(p);
            }
        }
    }
    found.sort();
    found
}

fn count_walkable_subdirs(root: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut count = 0;
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".gpsify" || is_photos_library_name(&name) {
            continue;
        }
        count += 1;
    }
    count
}

fn ignored(path: &Path, root: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.starts_with("._") || IGNORE_NAMES.contains(&name) {
        return true;
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let dot_ext = format!(".{}", ext.to_ascii_lowercase());
        if XMP_EXTS.contains(&dot_ext.as_str()) {
            return true;
        }
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .any(|c| matches!(c, Component::Normal(s) if s == ".gpsify"))
}

fn posix_relpath(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn walk_and_hash(root: &Path, recursive: bool) -> Vec<DiskFile> {
    let paths = iter_files(root, recursive);
    paths
        .par_iter()
        .map(|path| {
            let kind = classify_media(path);
            let relpath = posix_relpath(path, root);
            let path_raw = path.to_string_lossy().into_owned();
            if has_unsafe_path_chars(&path_raw) || has_unsafe_path_chars(&relpath) {
                return DiskFile {
                    path: path.clone(),
                    path_str: path_raw,
                    relpath,
                    kind,
                    sha256: String::new(),
                    size: 0,
                    mtime_ns: 0,
                    unreadable: true,
                };
            }
            let unreadable_stub = || DiskFile {
                path: path.clone(),
                path_str: path.to_string_lossy().into_owned(),
                relpath: relpath.clone(),
                kind,
                sha256: String::new(),
                size: 0,
                mtime_ns: 0,
                unreadable: true,
            };

            let Ok(resolved) = std::fs::canonicalize(path) else {
                return unreadable_stub();
            };
            if !resolved.starts_with(root) || is_photos_library(&resolved) {
                return unreadable_stub();
            }
            let Ok(meta) = std::fs::metadata(&resolved) else {
                return unreadable_stub();
            };
            let Ok(digest) = hash_file(&resolved) else {
                return unreadable_stub();
            };
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);

            DiskFile {
                path_str: resolved.to_string_lossy().into_owned(),
                path: resolved,
                relpath,
                kind,
                sha256: digest,
                size: meta.len() as i64,
                mtime_ns,
                unreadable: false,
            }
        })
        .collect()
}

fn upsert_photos(
    repo: &JobRepo,
    disk_files: &[DiskFile],
    exif_by_path: &BTreeMap<String, ExifRead>,
) -> Result<(), IngestError> {
    let existing_rows = repo.list_photos()?;
    let mut by_path: BTreeMap<String, PhotoRow> = existing_rows
        .iter()
        .map(|row| (row.path.clone(), row.clone()))
        .collect();
    let mut claimed_ids = BTreeSet::new();

    for item in disk_files {
        let exif = exif_by_path
            .get(&item.path_str)
            .cloned()
            .unwrap_or_default();
        let existing = match by_path.get(&item.path_str).cloned() {
            Some(row) => Some(row),
            None => {
                if let Some(matched) =
                    match_by_sha256(&existing_rows, &item.sha256, &claimed_ids, &item.relpath)
                {
                    repo.update_photo(
                        &matched.id,
                        PhotoPatch {
                            path: Some(item.path_str.clone()),
                            relpath: Some(item.relpath.clone()),
                            ..Default::default()
                        },
                    )?;
                    let refreshed = repo.get_photo(&matched.id)?;
                    if let Some(ref r) = refreshed {
                        by_path.insert(item.path_str.clone(), r.clone());
                    }
                    refreshed
                } else {
                    None
                }
            }
        };

        let Some(row) = existing else {
            insert_disk_file(repo, item, &exif)?;
            continue;
        };

        claimed_ids.insert(row.id.clone());
        refresh_existing(repo, &row, item, &exif)?;
    }

    let seen_paths: BTreeSet<&str> = disk_files.iter().map(|i| i.path_str.as_str()).collect();
    for row in &existing_rows {
        if claimed_ids.contains(&row.id) || seen_paths.contains(row.path.as_str()) {
            continue;
        }
        if row.status == PhotoStatus::Missing.as_str() {
            continue;
        }
        repo.set_photo_status(&row.id, PhotoStatus::Missing, None, None)?;
    }
    Ok(())
}

fn insert_disk_file(repo: &JobRepo, item: &DiskFile, exif: &ExifRead) -> Result<(), IngestError> {
    let (status, error, has_gps, lat, lng) = classify_status(item, exif);
    repo.insert_photo(NewPhoto {
        photo_id: None,
        path: item.path_str.clone(),
        relpath: item.relpath.clone(),
        sha256: item.sha256.clone(),
        size: item.size,
        mtime_ns: item.mtime_ns,
        status,
        error,
        datetime_original: exif.capture_time().map(str::to_string),
        make: exif.make.clone(),
        model: exif.model.clone(),
        has_gps,
        existing_lat: lat,
        existing_lng: lng,
        cluster_id: None,
        updated_at: None,
    })?;
    Ok(())
}

fn refresh_existing(
    repo: &JobRepo,
    row: &PhotoRow,
    item: &DiskFile,
    exif: &ExifRead,
) -> Result<(), IngestError> {
    if item.sha256 == row.sha256 && !item.unreadable {
        repo.update_photo(
            &row.id,
            PhotoPatch {
                size: Some(item.size),
                mtime_ns: Some(item.mtime_ns),
                path: Some(item.path_str.clone()),
                relpath: Some(item.relpath.clone()),
                ..Default::default()
            },
        )?;
        return Ok(());
    }
    repo.clear_current_proposal(&row.id)?;
    repo.clear_current_decision(&row.id)?;
    let (status, error, has_gps, lat, lng) = classify_status(item, exif);
    repo.update_photo(
        &row.id,
        PhotoPatch {
            path: Some(item.path_str.clone()),
            relpath: Some(item.relpath.clone()),
            sha256: Some(item.sha256.clone()),
            size: Some(item.size),
            mtime_ns: Some(item.mtime_ns),
            status: Some(status),
            error: Some(error),
            datetime_original: Some(exif.capture_time().map(str::to_string)),
            make: Some(exif.make.clone()),
            model: Some(exif.model.clone()),
            has_gps: Some(has_gps),
            existing_lat: Some(lat),
            existing_lng: Some(lng),
            ..Default::default()
        },
    )?;
    Ok(())
}

fn match_by_sha256<'a>(
    existing_rows: &'a [PhotoRow],
    sha256: &str,
    claimed_ids: &BTreeSet<String>,
    relpath: &str,
) -> Option<&'a PhotoRow> {
    if sha256.is_empty() {
        return None;
    }
    let matches: Vec<&PhotoRow> = existing_rows
        .iter()
        .filter(|row| row.sha256 == sha256 && !claimed_ids.contains(&row.id))
        .collect();
    if matches.is_empty() {
        return None;
    }
    for row in &matches {
        if row.relpath == relpath {
            return Some(*row);
        }
    }
    Some(matches[0])
}

fn classify_status(
    item: &DiskFile,
    exif: &ExifRead,
) -> (PhotoStatus, Option<String>, bool, Option<f64>, Option<f64>) {
    if item.unreadable {
        return (
            PhotoStatus::SkippedUnreadable,
            Some("unreadable".to_string()),
            false,
            None,
            None,
        );
    }
    match item.kind {
        MediaKind::Unsupported => (PhotoStatus::SkippedUnsupported, None, false, None, None),
        MediaKind::Jpeg | MediaKind::Png | MediaKind::Heic | MediaKind::Raw => {
            if exif.has_gps() {
                (
                    PhotoStatus::SkippedHasGps,
                    None,
                    true,
                    exif.gps_lat,
                    exif.gps_lng,
                )
            } else {
                (PhotoStatus::Pending, None, false, None, None)
            }
        }
    }
}

fn other_counts(counts: &BTreeMap<String, i64>) -> Vec<(String, i64)> {
    const BUCKETS: &[PhotoStatus] = &[
        PhotoStatus::SkippedHasGps,
        PhotoStatus::Pending,
        PhotoStatus::SkippedUnsupported,
        PhotoStatus::SkippedUnreadable,
        PhotoStatus::SkippedNoPreview,
        PhotoStatus::Missing,
    ];
    let bucket_names: BTreeSet<&str> = BUCKETS.iter().map(|s| s.as_str()).collect();
    let known_names: BTreeSet<&str> = PhotoStatus::ALL.iter().map(|s| s.as_str()).collect();

    let mut out = Vec::new();
    for &status in PhotoStatus::ALL {
        let name = status.as_str();
        if !bucket_names.contains(name) {
            if let Some(&n) = counts.get(name) {
                if n != 0 {
                    out.push((name.to_string(), n));
                }
            }
        }
    }
    for (name, &n) in counts {
        if n != 0 && !known_names.contains(name.as_str()) {
            out.push((name.clone(), n));
        }
    }
    out
}

fn rebuild_clusters(repo: &JobRepo) -> Result<usize, IngestError> {
    let photos = repo.list_photos()?;
    let mut members = Vec::new();
    for row in photos {
        if matches!(
            row.status.as_str(),
            "missing" | "skipped_unsupported" | "skipped_unreadable"
        ) {
            continue;
        }
        members.push(ClusterMember {
            photo_id: row.id,
            relpath: row.relpath,
            datetime_original: row.datetime_original,
        });
    }
    let plans = plan_clusters(&members);
    repo.delete_clusters()?;
    for plan in &plans {
        repo.insert_cluster(
            Some(&plan.cluster_id),
            &plan.parent,
            plan.first_ts.as_deref(),
            plan.n(),
            plan.undated,
        )?;
        for member in &plan.members {
            repo.update_photo(
                &member.photo_id,
                PhotoPatch {
                    cluster_id: Some(Some(plan.cluster_id.clone())),
                    ..Default::default()
                },
            )?;
        }
    }
    Ok(plans.len())
}
