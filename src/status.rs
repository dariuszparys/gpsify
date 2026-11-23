//! Status rollup and preview/thumb cleanup.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::exif::{classify_media, MediaKind};
use crate::store::{copies_root, job_dir, JobIndex, JobRepo, StoreError};
use crate::types::PhotoStatus;

pub const PNG_XMP_ONLY_WARNING: &str = "png_xmp_only";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct StatusError(pub String);

impl From<StoreError> for StatusError {
    fn from(err: StoreError) -> Self {
        Self(err.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusRollup {
    pub job_id: String,
    pub root_path: String,
    pub root_canonical: String,
    pub hint: Option<String>,
    pub created_at: String,
    pub dest_root: String,
    pub counts: BTreeMap<String, i64>,
    pub stats: serde_json::Map<String, serde_json::Value>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobListEntry {
    pub job_id: String,
    pub root_canonical: String,
    pub options_hash: String,
    pub created_at: String,
    pub last_ingest_at: String,
    pub dest_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanSummary {
    pub job_id: String,
    pub deleted_previews: bool,
    pub deleted_thumbs: bool,
    pub kept_sqlite: bool,
    pub kept_backups: bool,
}

fn parse_stats(raw: &str) -> serde_json::Map<String, serde_json::Value> {
    if raw.trim().is_empty() {
        return serde_json::Map::new();
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

fn counts_with_zeros(live: &BTreeMap<String, i64>) -> BTreeMap<String, i64> {
    let mut counts = BTreeMap::new();
    for status in PhotoStatus::ALL {
        let key = status.as_str().to_string();
        let val = live.get(&key).copied().unwrap_or(0);
        counts.insert(key, val);
    }
    for (k, &v) in live {
        counts.entry(k.clone()).or_insert(v);
    }
    counts
}

fn is_png(relpath: &str) -> bool {
    match classify_media(Path::new(relpath)) {
        MediaKind::Png => true,
        MediaKind::Jpeg | MediaKind::Heic | MediaKind::Raw | MediaKind::Unsupported => false,
    }
}

pub fn list_job_entries(data_dir: &Path) -> Result<Vec<JobListEntry>, StatusError> {
    let index = JobIndex::open(data_dir)?;
    let rows = index.list_jobs()?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let dest_root = copies_root(data_dir, &row.job_id)
                .to_string_lossy()
                .into_owned();
            JobListEntry {
                job_id: row.job_id,
                root_canonical: row.root_canonical,
                options_hash: row.options_hash,
                created_at: row.created_at,
                last_ingest_at: row.last_ingest_at,
                dest_root,
            }
        })
        .collect())
}

pub fn job_status(data_dir: &Path, job_id: &str) -> Result<StatusRollup, StatusError> {
    let repo = JobRepo::open(data_dir, job_id)?;
    let job = repo
        .get_job()?
        .ok_or_else(|| StatusError(format!("job row missing for {job_id}")))?;
    let live = repo.count_photos_by_status()?;
    let photos = repo.list_photos()?;
    let mut warnings = Vec::new();
    if photos.iter().any(|p| is_png(&p.relpath)) {
        warnings.push(PNG_XMP_ONLY_WARNING.to_string());
    }
    Ok(StatusRollup {
        job_id: job.id,
        root_path: job.root_path,
        root_canonical: job.root_canonical,
        hint: job.hint,
        created_at: job.created_at,
        dest_root: copies_root(data_dir, job_id).to_string_lossy().into_owned(),
        counts: counts_with_zeros(&live),
        stats: parse_stats(&job.stats_json),
        warnings,
    })
}

pub fn format_job_list(entries: &[JobListEntry]) -> String {
    if entries.is_empty() {
        return "no jobs".to_string();
    }
    let mut lines = vec!["job_id  last_ingest_at  dest_root  root".to_string()];
    for entry in entries {
        lines.push(format!(
            "{}  {}  {}  {}",
            entry.job_id, entry.last_ingest_at, entry.dest_root, entry.root_canonical
        ));
    }
    lines.join("\n")
}

pub fn format_status(rollup: &StatusRollup) -> String {
    let mut lines = vec![
        format!("job_id: {}", rollup.job_id),
        format!("root: {}", rollup.root_path),
        format!("dest: {}", rollup.dest_root),
    ];
    if let Some(hint) = &rollup.hint {
        if !hint.is_empty() {
            lines.push(format!("hint: {hint}"));
        }
    }
    let mut nonzero = Vec::new();
    for status in PhotoStatus::ALL {
        let key = status.as_str();
        if let Some(&n) = rollup.counts.get(key) {
            if n != 0 {
                nonzero.push(format!("{key}={n}"));
            }
        }
    }
    for (k, &n) in &rollup.counts {
        if n != 0 && !PhotoStatus::ALL.iter().any(|s| s.as_str() == k) {
            nonzero.push(format!("{k}={n}"));
        }
    }
    if nonzero.is_empty() {
        lines.push("counts: (empty)".to_string());
    } else {
        lines.push(format!("counts: {}", nonzero.join("  ")));
    }

    if let Some(serde_json::Value::Object(propose)) = rollup.stats.get("propose") {
        if let Some(est_usd) = propose.get("est_usd") {
            lines.push(format!("est_usd: {est_usd}"));
        }
        for key in ["input_tokens", "output_tokens", "reasoning_tokens"] {
            if let Some(val) = propose.get(key) {
                lines.push(format!("{key}: {val}"));
            }
        }
    }
    if let Some(serde_json::Value::Object(apply)) = rollup.stats.get("apply") {
        if apply.contains_key("wrote") {
            let wrote = apply.get("wrote").and_then(|v| v.as_i64()).unwrap_or(0);
            let failed = apply.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
            let skipped = apply.get("skipped").and_then(|v| v.as_i64()).unwrap_or(0);
            lines.push(format!(
                "apply: wrote={wrote}  failed={failed}  skipped={skipped}"
            ));
        }
    }
    lines.join("\n")
}

pub fn run_clean(data_dir: &Path, job_id: &str) -> Result<CleanSummary, StatusError> {
    let _repo = JobRepo::open(data_dir, job_id)?;
    let root = job_dir(data_dir, job_id);
    let previews = root.join("previews");
    let thumbs = root.join("thumbs");
    let sqlite_path = root.join("job.sqlite");
    let backups = root.join("backups");

    let deleted_previews = previews.exists();
    let deleted_thumbs = thumbs.exists();
    if deleted_previews {
        std::fs::remove_dir_all(&previews).map_err(|e| StatusError(e.to_string()))?;
    }
    if deleted_thumbs {
        std::fs::remove_dir_all(&thumbs).map_err(|e| StatusError(e.to_string()))?;
    }
    Ok(CleanSummary {
        job_id: job_id.to_string(),
        deleted_previews,
        deleted_thumbs,
        kept_sqlite: sqlite_path.is_file(),
        kept_backups: backups.exists(),
    })
}
