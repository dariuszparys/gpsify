//! Required global job index at `~/.gpsify/gpsify.sqlite` (K11).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::expand_user_path;
use crate::store::db::{index_db_path, job_db_path, open_index_db, utc_now_iso, StoreError};

pub const LAST_HANDLE: &str = "last";

/// SHA-256 of canonical JSON `{"recursive": bool}` (no spaces, sorted keys).
pub fn options_hash(recursive: bool) -> String {
    let payload = if recursive {
        br#"{"recursive":true}"#.as_slice()
    } else {
        br#"{"recursive":false}"#.as_slice()
    };
    let digest = Sha256::digest(payload);
    hex::encode(digest)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRow {
    pub job_id: String,
    pub root_canonical: String,
    pub options_hash: String,
    pub created_at: String,
    pub last_ingest_at: String,
}

fn row_to_index(row: &Row<'_>) -> rusqlite::Result<IndexRow> {
    Ok(IndexRow {
        job_id: row.get("job_id")?,
        root_canonical: row.get("root_canonical")?,
        options_hash: row.get("options_hash")?,
        created_at: row.get("created_at")?,
        last_ingest_at: row.get("last_ingest_at")?,
    })
}

fn is_constraint_err(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(ffi, _) if ffi.code == ErrorCode::ConstraintViolation
    )
}

#[derive(Clone)]
pub struct JobIndex {
    conn: Arc<Mutex<Connection>>,
    pub path: PathBuf,
}

impl JobIndex {
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        let path = index_db_path(data_dir);
        let conn = open_index_db(&path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    pub fn with_conn<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError>,
    {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("index mutex poisoned".into()))?;
        f(&guard)
    }

    pub fn lookup(
        &self,
        root_canonical: &str,
        options_hash_value: &str,
    ) -> Result<Option<IndexRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("index mutex poisoned".into()))?;
        lookup_on(&guard, root_canonical, options_hash_value)
    }

    pub fn get(&self, job_id: &str) -> Result<Option<IndexRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("index mutex poisoned".into()))?;
        get_on(&guard, job_id)
    }

    /// Newest ingest first (`last_ingest_at DESC, rowid DESC`).
    pub fn list_jobs(&self) -> Result<Vec<IndexRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("index mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT job_id, root_canonical, options_hash, created_at, last_ingest_at
             FROM job_index
             ORDER BY last_ingest_at DESC, rowid DESC",
        )?;
        let rows = stmt
            .query_map([], row_to_index)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn register(
        &self,
        job_id: &str,
        root_canonical: &str,
        options_hash_value: &str,
        now: Option<&str>,
    ) -> Result<IndexRow, StoreError> {
        let stamp = now.map(str::to_string).unwrap_or_else(utc_now_iso);
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("index mutex poisoned".into()))?;

        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let insert_res = tx.execute(
            "INSERT INTO job_index (
                job_id, root_canonical, options_hash, created_at, last_ingest_at
             ) VALUES (?, ?, ?, ?, ?)",
            params![job_id, root_canonical, options_hash_value, stamp, stamp],
        );

        match insert_res {
            Ok(_) => {
                tx.commit()?;
            }
            Err(err) if is_constraint_err(&err) => {
                drop(tx);
                if let Some(existing) = lookup_on(&guard, root_canonical, options_hash_value)? {
                    return Ok(existing);
                }
                return Err(StoreError::Conflict(format!(
                    "job_id {job_id:?} already exists with a different resume key"
                )));
            }
            Err(err) => return Err(StoreError::Sqlite(err)),
        }

        get_on(&guard, job_id)?
            .ok_or_else(|| StoreError::Other(format!("failed to register job_id {job_id:?}")))
    }

    pub fn touch_ingest(&self, job_id: &str, now: Option<&str>) -> Result<(), StoreError> {
        let stamp = now.map(str::to_string).unwrap_or_else(utc_now_iso);
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("index mutex poisoned".into()))?;
        let changed = guard.execute(
            "UPDATE job_index SET last_ingest_at = ? WHERE job_id = ?",
            params![stamp, job_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Other(format!("unknown job_id {job_id:?}")));
        }
        Ok(())
    }

    pub fn relocate(
        &self,
        job_id: &str,
        new_root_canonical: &str,
        now: Option<&str>,
    ) -> Result<IndexRow, StoreError> {
        let stamp = now.map(str::to_string).unwrap_or_else(utc_now_iso);
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("index mutex poisoned".into()))?;
        if get_on(&guard, job_id)?.is_none() {
            return Err(StoreError::Other(format!("unknown job_id {job_id:?}")));
        }
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let res = tx.execute(
            "UPDATE job_index SET root_canonical = ?, last_ingest_at = ? WHERE job_id = ?",
            params![new_root_canonical, stamp, job_id],
        );
        match res {
            Ok(_) => tx.commit()?,
            Err(err) if is_constraint_err(&err) => {
                return Err(StoreError::Conflict(
                    "relocate would collide with an existing resume key".to_string(),
                ));
            }
            Err(err) => return Err(StoreError::Sqlite(err)),
        }
        get_on(&guard, job_id)?
            .ok_or_else(|| StoreError::Other(format!("failed to relocate job_id {job_id:?}")))
    }
}

fn lookup_on(
    conn: &Connection,
    root_canonical: &str,
    options_hash_value: &str,
) -> Result<Option<IndexRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT job_id, root_canonical, options_hash, created_at, last_ingest_at
             FROM job_index
             WHERE root_canonical = ? AND options_hash = ?",
            params![root_canonical, options_hash_value],
            row_to_index,
        )
        .optional()?;
    Ok(row)
}

fn get_on(conn: &Connection, job_id: &str) -> Result<Option<IndexRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT job_id, root_canonical, options_hash, created_at, last_ingest_at
             FROM job_index
             WHERE job_id = ?",
            params![job_id],
            row_to_index,
        )
        .optional()?;
    Ok(row)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedJob {
    pub job_id: String,
    pub root_canonical: String,
    pub note: Option<String>,
}

pub fn looks_like_uuid(handle: &str) -> bool {
    Uuid::parse_str(handle).is_ok()
}

pub fn canonical_root(folder: &Path) -> Result<String, StoreError> {
    let expanded = expand_user_path(folder);
    let canonical = std::fs::canonicalize(&expanded)?;
    Ok(canonical.to_string_lossy().into_owned())
}

pub fn resolve_job_handle(data_dir: &Path, handle: &str) -> Result<ResolvedJob, StoreError> {
    let index = JobIndex::open(data_dir)?;
    if looks_like_uuid(handle) {
        if let Some(row) = index.get(handle)? {
            return Ok(ResolvedJob {
                job_id: row.job_id,
                root_canonical: row.root_canonical,
                note: None,
            });
        }
        if job_db_path(data_dir, handle).exists() {
            return Ok(ResolvedJob {
                job_id: handle.to_string(),
                root_canonical: String::new(),
                note: None,
            });
        }
        return Err(StoreError::Other(format!(
            "no job with id {handle:?}. Run 'gpsify status' to list jobs."
        )));
    }

    let jobs = index.list_jobs()?;
    if handle == LAST_HANDLE {
        let Some(newest) = jobs.into_iter().next() else {
            return Err(StoreError::Other(
                "no jobs yet. Run 'gpsify ingest <folder>' first.".to_string(),
            ));
        };
        let note = format!("using job {} ({})", newest.job_id, newest.root_canonical);
        return Ok(ResolvedJob {
            job_id: newest.job_id,
            root_canonical: newest.root_canonical,
            note: Some(note),
        });
    }

    let root = canonical_root(Path::new(handle)).map_err(|_| {
        StoreError::Other(format!(
            "not a job id, 'last', or an existing folder: {handle:?}"
        ))
    })?;

    let matches: Vec<IndexRow> = jobs
        .into_iter()
        .filter(|row| row.root_canonical == root)
        .collect();
    if matches.is_empty() {
        return Err(StoreError::Other(format!(
            "no job for folder {root}. Run 'gpsify ingest {handle}' first, or 'gpsify status' to list jobs."
        )));
    }
    if matches.len() == 1 {
        return Ok(ResolvedJob {
            job_id: matches[0].job_id.clone(),
            root_canonical: root,
            note: None,
        });
    }

    let winner = &matches[0];
    let scope = if winner.options_hash == options_hash(true) {
        "recursive"
    } else {
        "non-recursive"
    };
    let note = format!(
        "{} jobs share this folder; using the most recent: {} ({scope})",
        matches.len(),
        winner.job_id
    );
    Ok(ResolvedJob {
        job_id: winner.job_id.clone(),
        root_canonical: root,
        note: Some(note),
    })
}
