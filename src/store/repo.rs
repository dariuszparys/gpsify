//! Job repository: schema-backed CRUD plus `add_decision` (K12).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use rusqlite::{
    params, types::Value as SqlValue, Connection, OptionalExtension, Row, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::store::db::{job_db_path, open_job_db, utc_now_iso, StoreError};
use crate::types::{DecisionAction, PhotoStatus, PinSource, WriteReceiptStatus};

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

fn is_finite_opt(v: Option<f64>) -> bool {
    matches!(v, Some(x) if x.is_finite())
}

fn pin_allows_accept_copy(pin_source: Option<&str>) -> bool {
    let Some(raw) = pin_source else {
        return false;
    };
    match PinSource::from_str(raw) {
        Ok(PinSource::Geocoder | PinSource::User) => true,
        Ok(PinSource::None) | Err(_) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRow {
    pub id: String,
    pub root_path: String,
    pub root_canonical: String,
    pub options_hash: String,
    pub hint: Option<String>,
    pub created_at: String,
    pub config_json: String,
    pub stats_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterRow {
    pub id: String,
    pub job_id: String,
    pub parent: String,
    pub first_ts: Option<String>,
    pub n: i64,
    pub undated: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotoRow {
    pub id: String,
    pub job_id: String,
    pub path: String,
    pub relpath: String,
    pub sha256: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub status: String,
    pub error: Option<String>,
    pub datetime_original: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub has_gps: i64,
    pub existing_lat: Option<f64>,
    pub existing_lng: Option<f64>,
    pub cluster_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalRow {
    pub id: String,
    pub photo_id: String,
    pub created_at: String,
    pub current: i64,
    pub provider: String,
    pub model: String,
    pub prompt_version: String,
    pub raw_json: String,
    pub place_name: Option<String>,
    pub granularity: Option<String>,
    pub model_confidence: Option<f64>,
    pub ui_rank: Option<f64>,
    pub refusal: Option<String>,
    pub pin_lat: Option<f64>,
    pub pin_lng: Option<f64>,
    pub pin_source: Option<String>,
    pub model_agree: Option<i64>,
    pub geocode_json: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRow {
    pub id: String,
    pub photo_id: String,
    pub proposal_id: Option<String>,
    pub action: String,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub place_name: Option<String>,
    pub current: i64,
    pub decided_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteReceiptRow {
    pub id: String,
    pub photo_id: String,
    pub decision_id: String,
    pub dest_path: String,
    pub in_place: i64,
    pub status: String,
    pub error: Option<String>,
    pub written_at: Option<String>,
    pub written_sha256: Option<String>,
    pub backup_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewPhoto {
    pub photo_id: Option<String>,
    pub path: String,
    pub relpath: String,
    pub sha256: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub status: PhotoStatus,
    pub error: Option<String>,
    pub datetime_original: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub has_gps: bool,
    pub existing_lat: Option<f64>,
    pub existing_lng: Option<f64>,
    pub cluster_id: Option<String>,
    pub updated_at: Option<String>,
}

impl NewPhoto {
    pub fn pending(
        path: impl Into<String>,
        relpath: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            photo_id: None,
            path: path.into(),
            relpath: relpath.into(),
            sha256: sha256.into(),
            size: 0,
            mtime_ns: 0,
            status: PhotoStatus::Pending,
            error: None,
            datetime_original: None,
            make: None,
            model: None,
            has_gps: false,
            existing_lat: None,
            existing_lng: None,
            cluster_id: None,
            updated_at: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PhotoPatch {
    pub path: Option<String>,
    pub relpath: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<i64>,
    pub mtime_ns: Option<i64>,
    pub status: Option<PhotoStatus>,
    pub error: Option<Option<String>>,
    pub datetime_original: Option<Option<String>>,
    pub make: Option<Option<String>>,
    pub model: Option<Option<String>>,
    pub has_gps: Option<bool>,
    pub existing_lat: Option<Option<f64>>,
    pub existing_lng: Option<Option<f64>>,
    pub cluster_id: Option<Option<String>>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewProposal {
    pub photo_id: String,
    pub provider: String,
    pub model: String,
    pub prompt_version: String,
    pub raw_json: String,
    pub place_name: Option<String>,
    pub granularity: Option<String>,
    pub model_confidence: Option<f64>,
    pub ui_rank: Option<f64>,
    pub refusal: Option<String>,
    pub pin_lat: Option<f64>,
    pub pin_lng: Option<f64>,
    pub pin_source: Option<PinSource>,
    pub model_agree: Option<i64>,
    pub geocode_json: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
    pub current: bool,
    pub proposal_id: Option<String>,
    pub created_at: Option<String>,
}

impl Default for NewProposal {
    fn default() -> Self {
        Self {
            photo_id: String::new(),
            provider: String::new(),
            model: String::new(),
            prompt_version: String::new(),
            raw_json: "{}".to_string(),
            place_name: None,
            granularity: None,
            model_confidence: None,
            ui_rank: None,
            refusal: None,
            pin_lat: None,
            pin_lng: None,
            pin_source: None,
            model_agree: None,
            geocode_json: None,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            latency_ms: None,
            current: true,
            proposal_id: None,
            created_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpsertWriteReceipt {
    pub photo_id: String,
    pub decision_id: String,
    pub dest_path: String,
    pub in_place: bool,
    pub status: WriteReceiptStatus,
    pub error: Option<String>,
    pub written_at: Option<String>,
    pub written_sha256: Option<String>,
    pub backup_path: Option<String>,
    pub receipt_id: Option<String>,
}

fn row_to_job(row: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: row.get("id")?,
        root_path: row.get("root_path")?,
        root_canonical: row.get("root_canonical")?,
        options_hash: row.get("options_hash")?,
        hint: row.get("hint")?,
        created_at: row.get("created_at")?,
        config_json: row.get("config_json")?,
        stats_json: row.get("stats_json")?,
    })
}

fn row_to_cluster(row: &Row<'_>) -> rusqlite::Result<ClusterRow> {
    Ok(ClusterRow {
        id: row.get("id")?,
        job_id: row.get("job_id")?,
        parent: row.get("parent")?,
        first_ts: row.get("first_ts")?,
        n: row.get("n")?,
        undated: row.get("undated")?,
    })
}

fn row_to_photo(row: &Row<'_>) -> rusqlite::Result<PhotoRow> {
    Ok(PhotoRow {
        id: row.get("id")?,
        job_id: row.get("job_id")?,
        path: row.get("path")?,
        relpath: row.get("relpath")?,
        sha256: row.get("sha256")?,
        size: row.get("size")?,
        mtime_ns: row.get("mtime_ns")?,
        status: row.get("status")?,
        error: row.get("error")?,
        datetime_original: row.get("datetime_original")?,
        make: row.get("make")?,
        model: row.get("model")?,
        has_gps: row.get("has_gps")?,
        existing_lat: row.get("existing_lat")?,
        existing_lng: row.get("existing_lng")?,
        cluster_id: row.get("cluster_id")?,
        updated_at: row.get("updated_at")?,
    })
}

fn row_to_proposal(row: &Row<'_>) -> rusqlite::Result<ProposalRow> {
    Ok(ProposalRow {
        id: row.get("id")?,
        photo_id: row.get("photo_id")?,
        created_at: row.get("created_at")?,
        current: row.get("current")?,
        provider: row.get("provider")?,
        model: row.get("model")?,
        prompt_version: row.get("prompt_version")?,
        raw_json: row.get("raw_json")?,
        place_name: row.get("place_name")?,
        granularity: row.get("granularity")?,
        model_confidence: row.get("model_confidence")?,
        ui_rank: row.get("ui_rank")?,
        refusal: row.get("refusal")?,
        pin_lat: row.get("pin_lat")?,
        pin_lng: row.get("pin_lng")?,
        pin_source: row.get("pin_source")?,
        model_agree: row.get("model_agree")?,
        geocode_json: row.get("geocode_json")?,
        input_tokens: row.get("input_tokens")?,
        output_tokens: row.get("output_tokens")?,
        reasoning_tokens: row.get("reasoning_tokens")?,
        latency_ms: row.get("latency_ms")?,
    })
}

fn row_to_decision(row: &Row<'_>) -> rusqlite::Result<DecisionRow> {
    Ok(DecisionRow {
        id: row.get("id")?,
        photo_id: row.get("photo_id")?,
        proposal_id: row.get("proposal_id")?,
        action: row.get("action")?,
        lat: row.get("lat")?,
        lng: row.get("lng")?,
        place_name: row.get("place_name")?,
        current: row.get("current")?,
        decided_at: row.get("decided_at")?,
    })
}

fn row_to_receipt(row: &Row<'_>) -> rusqlite::Result<WriteReceiptRow> {
    Ok(WriteReceiptRow {
        id: row.get("id")?,
        photo_id: row.get("photo_id")?,
        decision_id: row.get("decision_id")?,
        dest_path: row.get("dest_path")?,
        in_place: row.get("in_place")?,
        status: row.get("status")?,
        error: row.get("error")?,
        written_at: row.get("written_at")?,
        written_sha256: row.get("written_sha256")?,
        backup_path: row.get("backup_path")?,
    })
}

#[derive(Clone)]
pub struct JobRepo {
    conn: Arc<Mutex<Connection>>,
    pub job_id: String,
    pub path: PathBuf,
}

impl JobRepo {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        data_dir: &Path,
        job_id: &str,
        root_path: &str,
        root_canonical: &str,
        options_hash: &str,
        hint: Option<&str>,
        config_json: &str,
        created_at: Option<&str>,
    ) -> Result<Self, StoreError> {
        let path = job_db_path(data_dir, job_id);
        if path.exists() {
            return Err(StoreError::Other(format!(
                "job database already exists: {}",
                path.display()
            )));
        }
        let conn = open_job_db(&path)?;
        let stamp = created_at.map(str::to_string).unwrap_or_else(utc_now_iso);
        conn.execute(
            "INSERT INTO job (
                id, root_path, root_canonical, options_hash, hint,
                created_at, config_json, stats_json
             ) VALUES (?, ?, ?, ?, ?, ?, ?, '{}')",
            params![
                job_id,
                root_path,
                root_canonical,
                options_hash,
                hint,
                stamp,
                config_json,
            ],
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            job_id: job_id.to_string(),
            path,
        })
    }

    pub fn open(data_dir: &Path, job_id: &str) -> Result<Self, StoreError> {
        let path = job_db_path(data_dir, job_id);
        if !path.is_file() {
            return Err(StoreError::Other(format!(
                "job database not found: {}",
                path.display()
            )));
        }
        let conn = open_job_db(&path)?;
        let repo = Self {
            conn: Arc::new(Mutex::new(conn)),
            job_id: job_id.to_string(),
            path: path.clone(),
        };
        let Some(job) = repo.get_job()? else {
            return Err(StoreError::Other(format!(
                "job row missing in {}",
                path.display()
            )));
        };
        if job.id != job_id {
            return Err(StoreError::Other(format!(
                "job id mismatch: db has {:?}, expected {:?}",
                job.id, job_id
            )));
        }
        Ok(repo)
    }

    pub fn with_conn<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError>,
    {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        f(&guard)
    }

    pub fn get_job(&self) -> Result<Option<JobRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let row = guard
            .query_row(
                "SELECT id, root_path, root_canonical, options_hash, hint,
                        created_at, config_json, stats_json
                 FROM job
                 WHERE id = ?",
                params![self.job_id],
                row_to_job,
            )
            .optional()?;
        Ok(row)
    }

    pub fn set_hint(&self, hint: Option<&str>) -> Result<(), StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "UPDATE job SET hint = ? WHERE id = ?",
            params![hint, self.job_id],
        )?;
        Ok(())
    }

    pub fn set_stats_json(&self, stats_json: &str) -> Result<(), StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "UPDATE job SET stats_json = ? WHERE id = ?",
            params![stats_json, self.job_id],
        )?;
        Ok(())
    }

    pub fn set_root(&self, root_path: &str, root_canonical: &str) -> Result<(), StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "UPDATE job SET root_path = ?, root_canonical = ? WHERE id = ?",
            params![root_path, root_canonical, self.job_id],
        )?;
        Ok(())
    }

    pub fn insert_cluster(
        &self,
        cluster_id: Option<&str>,
        parent: &str,
        first_ts: Option<&str>,
        n: i64,
        undated: bool,
    ) -> Result<String, StoreError> {
        let cid = cluster_id.map(str::to_string).unwrap_or_else(new_id);
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "INSERT INTO cluster (id, job_id, parent, first_ts, n, undated)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![cid, self.job_id, parent, first_ts, n, i64::from(undated)],
        )?;
        Ok(cid)
    }

    pub fn get_cluster(&self, cluster_id: &str) -> Result<Option<ClusterRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let row = guard
            .query_row(
                "SELECT id, job_id, parent, first_ts, n, undated
                 FROM cluster
                 WHERE id = ?",
                params![cluster_id],
                row_to_cluster,
            )
            .optional()?;
        Ok(row)
    }

    pub fn insert_photo(&self, new: NewPhoto) -> Result<String, StoreError> {
        let pid = new.photo_id.unwrap_or_else(new_id);
        let stamp = new.updated_at.unwrap_or_else(utc_now_iso);
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "INSERT INTO photo (
                id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                datetime_original, make, model, has_gps, existing_lat, existing_lng,
                cluster_id, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                pid,
                self.job_id,
                new.path,
                new.relpath,
                new.sha256,
                new.size,
                new.mtime_ns,
                new.status.as_str(),
                new.error,
                new.datetime_original,
                new.make,
                new.model,
                i64::from(new.has_gps),
                new.existing_lat,
                new.existing_lng,
                new.cluster_id,
                stamp,
            ],
        )?;
        Ok(pid)
    }

    pub fn get_photo(&self, photo_id: &str) -> Result<Option<PhotoRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        get_photo_on(&guard, photo_id)
    }

    pub fn get_photo_by_path(&self, path: &str) -> Result<Option<PhotoRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let row = guard
            .query_row(
                "SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                        datetime_original, make, model, has_gps, existing_lat, existing_lng,
                        cluster_id, updated_at
                 FROM photo
                 WHERE job_id = ? AND path = ?",
                params![self.job_id, path],
                row_to_photo,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_photos(&self) -> Result<Vec<PhotoRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                    datetime_original, make, model, has_gps, existing_lat, existing_lng,
                    cluster_id, updated_at
             FROM photo
             WHERE job_id = ?
             ORDER BY relpath",
        )?;
        let rows = stmt
            .query_map(params![self.job_id], row_to_photo)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_photos_by_sha256(&self, sha256: &str) -> Result<Vec<PhotoRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                    datetime_original, make, model, has_gps, existing_lat, existing_lng,
                    cluster_id, updated_at
             FROM photo
             WHERE job_id = ? AND sha256 = ?",
        )?;
        let rows = stmt
            .query_map(params![self.job_id, sha256], row_to_photo)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count_photos_by_status(&self) -> Result<BTreeMap<String, i64>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT status, COUNT(*) AS n
             FROM photo
             WHERE job_id = ?
             GROUP BY status",
        )?;
        let mut map = BTreeMap::new();
        let mut rows = stmt.query(params![self.job_id])?;
        while let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            map.insert(status, count);
        }
        Ok(map)
    }

    pub fn update_photo(&self, photo_id: &str, patch: PhotoPatch) -> Result<(), StoreError> {
        let mut cols: Vec<&'static str> = Vec::new();
        let mut vals: Vec<SqlValue> = Vec::new();

        if let Some(v) = patch.path {
            cols.push("path");
            vals.push(SqlValue::Text(v));
        }
        if let Some(v) = patch.relpath {
            cols.push("relpath");
            vals.push(SqlValue::Text(v));
        }
        if let Some(v) = patch.sha256 {
            cols.push("sha256");
            vals.push(SqlValue::Text(v));
        }
        if let Some(v) = patch.size {
            cols.push("size");
            vals.push(SqlValue::Integer(v));
        }
        if let Some(v) = patch.mtime_ns {
            cols.push("mtime_ns");
            vals.push(SqlValue::Integer(v));
        }
        if let Some(v) = patch.status {
            cols.push("status");
            vals.push(SqlValue::Text(v.as_str().to_string()));
        }
        if let Some(v) = patch.error {
            cols.push("error");
            vals.push(v.map(SqlValue::Text).unwrap_or(SqlValue::Null));
        }
        if let Some(v) = patch.datetime_original {
            cols.push("datetime_original");
            vals.push(v.map(SqlValue::Text).unwrap_or(SqlValue::Null));
        }
        if let Some(v) = patch.make {
            cols.push("make");
            vals.push(v.map(SqlValue::Text).unwrap_or(SqlValue::Null));
        }
        if let Some(v) = patch.model {
            cols.push("model");
            vals.push(v.map(SqlValue::Text).unwrap_or(SqlValue::Null));
        }
        if let Some(v) = patch.has_gps {
            cols.push("has_gps");
            vals.push(SqlValue::Integer(i64::from(v)));
        }
        if let Some(v) = patch.existing_lat {
            cols.push("existing_lat");
            vals.push(v.map(SqlValue::Real).unwrap_or(SqlValue::Null));
        }
        if let Some(v) = patch.existing_lng {
            cols.push("existing_lng");
            vals.push(v.map(SqlValue::Real).unwrap_or(SqlValue::Null));
        }
        if let Some(v) = patch.cluster_id {
            cols.push("cluster_id");
            vals.push(v.map(SqlValue::Text).unwrap_or(SqlValue::Null));
        }
        if cols.is_empty() && patch.updated_at.is_none() {
            return Ok(());
        }
        let stamp = patch.updated_at.unwrap_or_else(utc_now_iso);
        cols.push("updated_at");
        vals.push(SqlValue::Text(stamp));
        vals.push(SqlValue::Text(photo_id.to_string()));

        let assignments = cols
            .iter()
            .map(|c| format!("{c} = ?"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE photo SET {assignments} WHERE id = ?");

        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let changed = guard.execute(&sql, rusqlite::params_from_iter(vals))?;
        if changed == 0 {
            return Err(StoreError::Other(format!("unknown photo {photo_id:?}")));
        }
        Ok(())
    }

    pub fn clear_current_proposal(&self, photo_id: &str) -> Result<(), StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "UPDATE proposal SET current = 0 WHERE photo_id = ? AND current = 1",
            params![photo_id],
        )?;
        Ok(())
    }

    pub fn clear_current_decision(&self, photo_id: &str) -> Result<(), StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "UPDATE decision SET current = 0 WHERE photo_id = ? AND current = 1",
            params![photo_id],
        )?;
        Ok(())
    }

    pub fn list_clusters(&self) -> Result<Vec<ClusterRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT id, job_id, parent, first_ts, n, undated
             FROM cluster
             WHERE job_id = ?
             ORDER BY parent, first_ts",
        )?;
        let rows = stmt
            .query_map(params![self.job_id], row_to_cluster)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_clusters(&self) -> Result<(), StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        guard.execute(
            "UPDATE photo SET cluster_id = NULL WHERE job_id = ?",
            params![self.job_id],
        )?;
        guard.execute("DELETE FROM cluster WHERE job_id = ?", params![self.job_id])?;
        Ok(())
    }

    pub fn set_photo_status(
        &self,
        photo_id: &str,
        status: PhotoStatus,
        error: Option<&str>,
        updated_at: Option<&str>,
    ) -> Result<(), StoreError> {
        let stamp = updated_at.map(str::to_string).unwrap_or_else(utc_now_iso);
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let changed = guard.execute(
            "UPDATE photo SET status = ?, error = ?, updated_at = ? WHERE id = ?",
            params![status.as_str(), error, stamp, photo_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Other(format!("unknown photo {photo_id:?}")));
        }
        Ok(())
    }

    /// Crash repair: `proposing` -> `pending`. Called at every propose start.
    pub fn reset_stale_proposing(&self) -> Result<usize, StoreError> {
        let stamp = utc_now_iso();
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let changed = guard.execute(
            "UPDATE photo SET status = ?, updated_at = ? WHERE job_id = ? AND status = ?",
            params![
                PhotoStatus::Pending.as_str(),
                stamp,
                self.job_id,
                PhotoStatus::Proposing.as_str(),
            ],
        )?;
        Ok(changed)
    }

    pub fn insert_proposal(&self, new: NewProposal) -> Result<String, StoreError> {
        let pid = new.proposal_id.unwrap_or_else(new_id);
        let stamp = new.created_at.unwrap_or_else(utc_now_iso);
        let pin_source_val = new.pin_source.map(|s| s.as_str().to_string());
        let current_int = i64::from(new.current);
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if current_int == 1 {
            tx.execute(
                "UPDATE proposal SET current = 0 WHERE photo_id = ? AND current = 1",
                params![new.photo_id],
            )?;
        }
        tx.execute(
            "INSERT INTO proposal (
                id, photo_id, created_at, current, provider, model, prompt_version,
                raw_json, place_name, granularity, model_confidence, ui_rank, refusal,
                pin_lat, pin_lng, pin_source, model_agree, geocode_json,
                input_tokens, output_tokens, reasoning_tokens, latency_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                pid,
                new.photo_id,
                stamp,
                current_int,
                new.provider,
                new.model,
                new.prompt_version,
                new.raw_json,
                new.place_name,
                new.granularity,
                new.model_confidence,
                new.ui_rank,
                new.refusal,
                new.pin_lat,
                new.pin_lng,
                pin_source_val,
                new.model_agree,
                new.geocode_json,
                new.input_tokens,
                new.output_tokens,
                new.reasoning_tokens,
                new.latency_ms,
            ],
        )?;
        tx.commit()?;
        Ok(pid)
    }

    pub fn get_proposal(&self, proposal_id: &str) -> Result<Option<ProposalRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let row = guard
            .query_row(
                "SELECT * FROM proposal WHERE id = ?",
                params![proposal_id],
                row_to_proposal,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_current_proposal(&self, photo_id: &str) -> Result<Option<ProposalRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        get_current_proposal_on(&guard, photo_id)
    }

    pub fn list_current_proposals(&self) -> Result<Vec<ProposalRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT proposal.*
             FROM proposal
             JOIN photo ON photo.id = proposal.photo_id
             WHERE photo.job_id = ? AND proposal.current = 1",
        )?;
        let rows = stmt
            .query_map(params![self.job_id], row_to_proposal)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Last-write-wins decision mutator (K12).
    pub fn add_decision(
        &self,
        photo_id: &str,
        action: DecisionAction,
        lat: Option<f64>,
        lng: Option<f64>,
        proposal_id: Option<&str>,
        place_name: Option<&str>,
    ) -> Result<String, StoreError> {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;

        let Some(photo) = get_photo_on(&guard, photo_id)? else {
            return Err(StoreError::Decision(format!("unknown photo {photo_id:?}")));
        };
        if photo.status == PhotoStatus::Written.as_str() {
            return Err(StoreError::Decision(
                "photo is written; undo first".to_string(),
            ));
        }

        let resolved_lat: Option<f64>;
        let resolved_lng: Option<f64>;
        let mut resolved_place = place_name.map(str::to_string);
        let mut resolved_proposal = proposal_id.map(str::to_string);
        let photo_status: PhotoStatus;

        match action {
            DecisionAction::Skip => {
                if lat.is_some() || lng.is_some() {
                    return Err(StoreError::Decision(
                        "skip must not include coordinates".to_string(),
                    ));
                }
                resolved_lat = None;
                resolved_lng = None;
                photo_status = PhotoStatus::SkippedByUser;
            }
            DecisionAction::Accept => {
                let (a_lat, a_lng, a_place, a_prop) =
                    resolve_accept_on(&guard, photo_id, lat, lng, place_name, proposal_id)?;
                resolved_lat = Some(a_lat);
                resolved_lng = Some(a_lng);
                resolved_place = a_place;
                resolved_proposal = a_prop;
                photo_status = PhotoStatus::Accepted;
            }
            DecisionAction::AcceptEdited => {
                if !is_finite_opt(lat) || !is_finite_opt(lng) {
                    return Err(StoreError::Decision(
                        "accept_edited requires finite lat/lng".to_string(),
                    ));
                }
                resolved_lat = lat;
                resolved_lng = lng;
                photo_status = PhotoStatus::AcceptedEdited;
            }
            DecisionAction::ClusterApply => {
                if !is_finite_opt(lat) || !is_finite_opt(lng) {
                    return Err(StoreError::Decision(
                        "cluster_apply requires finite lat/lng".to_string(),
                    ));
                }
                resolved_lat = lat;
                resolved_lng = lng;
                photo_status = PhotoStatus::AcceptedEdited;
            }
        }

        if resolved_proposal.is_none() && action != DecisionAction::Skip {
            if let Some(current) = get_current_proposal_on(&guard, photo_id)? {
                resolved_proposal = Some(current.id);
                if resolved_place.is_none() {
                    resolved_place = current.place_name;
                }
            }
        }

        if action.is_writeable() && (!is_finite_opt(resolved_lat) || !is_finite_opt(resolved_lng)) {
            return Err(StoreError::Decision(format!(
                "{} must persist finite lat/lng",
                action.as_str()
            )));
        }

        let decision_id = new_id();
        let stamp = utc_now_iso();
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE decision SET current = 0 WHERE photo_id = ? AND current = 1",
            params![photo_id],
        )?;
        tx.execute(
            "INSERT INTO decision (
                id, photo_id, proposal_id, action, lat, lng, place_name, current, decided_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?)",
            params![
                decision_id,
                photo_id,
                resolved_proposal,
                action.as_str(),
                resolved_lat,
                resolved_lng,
                resolved_place,
                stamp,
            ],
        )?;
        tx.execute(
            "UPDATE photo SET status = ?, updated_at = ? WHERE id = ?",
            params![photo_status.as_str(), stamp, photo_id],
        )?;
        tx.commit()?;
        Ok(decision_id)
    }

    pub fn get_decision(&self, decision_id: &str) -> Result<Option<DecisionRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let row = guard
            .query_row(
                "SELECT * FROM decision WHERE id = ?",
                params![decision_id],
                row_to_decision,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_current_decision(&self, photo_id: &str) -> Result<Option<DecisionRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let row = guard
            .query_row(
                "SELECT * FROM decision WHERE photo_id = ? AND current = 1",
                params![photo_id],
                row_to_decision,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_current_decisions(&self) -> Result<Vec<DecisionRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT decision.*
             FROM decision
             JOIN photo ON photo.id = decision.photo_id
             WHERE photo.job_id = ? AND decision.current = 1",
        )?;
        let rows = stmt
            .query_map(params![self.job_id], row_to_decision)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_photos_in_cluster(&self, cluster_id: &str) -> Result<Vec<PhotoRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                    datetime_original, make, model, has_gps, existing_lat, existing_lng,
                    cluster_id, updated_at
             FROM photo
             WHERE job_id = ? AND cluster_id = ?
             ORDER BY relpath",
        )?;
        let rows = stmt
            .query_map(params![self.job_id, cluster_id], row_to_photo)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn upsert_write_receipt(&self, up: UpsertWriteReceipt) -> Result<String, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let existing = get_write_receipt_on(&guard, &up.photo_id, &up.dest_path)?;
        let rid = existing
            .map(|r| r.id)
            .or(up.receipt_id)
            .unwrap_or_else(new_id);
        guard.execute(
            "INSERT INTO write_receipt (
                id, photo_id, decision_id, dest_path, in_place, status, error,
                written_at, written_sha256, backup_path
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(photo_id, dest_path) DO UPDATE SET
                decision_id = excluded.decision_id,
                in_place = excluded.in_place,
                status = excluded.status,
                error = excluded.error,
                written_at = excluded.written_at,
                written_sha256 = excluded.written_sha256,
                backup_path = excluded.backup_path",
            params![
                rid,
                up.photo_id,
                up.decision_id,
                up.dest_path,
                i64::from(up.in_place),
                up.status.as_str(),
                up.error,
                up.written_at,
                up.written_sha256,
                up.backup_path,
            ],
        )?;
        Ok(rid)
    }

    pub fn list_write_receipts(&self) -> Result<Vec<WriteReceiptRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        let mut stmt = guard.prepare(
            "SELECT write_receipt.*
             FROM write_receipt
             JOIN photo ON photo.id = write_receipt.photo_id
             WHERE photo.job_id = ?
             ORDER BY write_receipt.dest_path",
        )?;
        let rows = stmt
            .query_map(params![self.job_id], row_to_receipt)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_write_receipt(
        &self,
        photo_id: &str,
        dest_path: &str,
    ) -> Result<Option<WriteReceiptRow>, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("job repo mutex poisoned".into()))?;
        get_write_receipt_on(&guard, photo_id, dest_path)
    }
}

fn get_photo_on(conn: &Connection, photo_id: &str) -> Result<Option<PhotoRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                    datetime_original, make, model, has_gps, existing_lat, existing_lng,
                    cluster_id, updated_at
             FROM photo
             WHERE id = ?",
            params![photo_id],
            row_to_photo,
        )
        .optional()?;
    Ok(row)
}

fn get_current_proposal_on(
    conn: &Connection,
    photo_id: &str,
) -> Result<Option<ProposalRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT * FROM proposal WHERE photo_id = ? AND current = 1",
            params![photo_id],
            row_to_proposal,
        )
        .optional()?;
    Ok(row)
}

fn get_write_receipt_on(
    conn: &Connection,
    photo_id: &str,
    dest_path: &str,
) -> Result<Option<WriteReceiptRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT * FROM write_receipt WHERE photo_id = ? AND dest_path = ?",
            params![photo_id, dest_path],
            row_to_receipt,
        )
        .optional()?;
    Ok(row)
}

fn resolve_accept_on(
    conn: &Connection,
    photo_id: &str,
    lat: Option<f64>,
    lng: Option<f64>,
    place_name: Option<&str>,
    proposal_id: Option<&str>,
) -> Result<(f64, f64, Option<String>, Option<String>), StoreError> {
    if lat.is_some() || lng.is_some() {
        let (Some(a_lat), Some(a_lng)) = (lat, lng) else {
            return Err(StoreError::Decision(
                "accept lat/lng must both be finite".to_string(),
            ));
        };
        if !a_lat.is_finite() || !a_lng.is_finite() {
            return Err(StoreError::Decision(
                "accept lat/lng must both be finite".to_string(),
            ));
        }
        return Ok((
            a_lat,
            a_lng,
            place_name.map(str::to_string),
            proposal_id.map(str::to_string),
        ));
    }

    let Some(current) = get_current_proposal_on(conn, photo_id)? else {
        return Err(StoreError::Decision(
            "accept requires a current proposal".to_string(),
        ));
    };
    if !pin_allows_accept_copy(current.pin_source.as_deref()) {
        return Err(StoreError::Decision(
            "accept rejected: pin_source is not geocoder or user".to_string(),
        ));
    }
    let (Some(pin_lat), Some(pin_lng)) = (current.pin_lat, current.pin_lng) else {
        return Err(StoreError::Decision(
            "accept rejected: proposal pin is missing or non-finite".to_string(),
        ));
    };
    if !pin_lat.is_finite() || !pin_lng.is_finite() {
        return Err(StoreError::Decision(
            "accept rejected: proposal pin is missing or non-finite".to_string(),
        ));
    }
    let copied_place = place_name.map(str::to_string).or(current.place_name);
    Ok((pin_lat, pin_lng, copied_place, Some(current.id)))
}
