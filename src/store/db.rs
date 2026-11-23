//! Job & global index SQLite schema v1 (PRAGMA user_version = 1).

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::Connection;
use thiserror::Error;

pub const SCHEMA_VERSION: i64 = 1;
pub const INDEX_DB_NAME: &str = "gpsify.sqlite";
pub const JOB_DB_NAME: &str = "job.sqlite";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{0}")]
    Decision(String),
    #[error("{0}")]
    Schema(String),
    #[error("{0}")]
    Conflict(String),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub fn job_dir(data_dir: &Path, job_id: &str) -> PathBuf {
    data_dir.join("jobs").join(job_id)
}

pub fn job_db_path(data_dir: &Path, job_id: &str) -> PathBuf {
    job_dir(data_dir, job_id).join(JOB_DB_NAME)
}

pub fn index_db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(INDEX_DB_NAME)
}

pub fn copies_root(data_dir: &Path, job_id: &str) -> PathBuf {
    job_dir(data_dir, job_id).join("copies")
}

pub fn backups_root(data_dir: &Path, job_id: &str) -> PathBuf {
    job_dir(data_dir, job_id).join("backups")
}

/// UTC timestamp stored as TEXT (second precision, `Z` suffix).
pub fn utc_now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Complete v1 schema; see `docs/decisions.md`. Frozen — do not add columns.
pub const JOB_SCHEMA: &str = r#"
CREATE TABLE job (
  id TEXT PRIMARY KEY,
  root_path TEXT NOT NULL,
  root_canonical TEXT NOT NULL,
  options_hash TEXT NOT NULL,
  hint TEXT,
  created_at TEXT NOT NULL,
  config_json TEXT NOT NULL,
  stats_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE cluster (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL REFERENCES job(id),
  parent TEXT NOT NULL,
  first_ts TEXT,
  n INTEGER NOT NULL,
  undated INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE photo (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL REFERENCES job(id),
  path TEXT NOT NULL,
  relpath TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  status TEXT NOT NULL,
  error TEXT,
  datetime_original TEXT,
  make TEXT,
  model TEXT,
  has_gps INTEGER NOT NULL DEFAULT 0,
  existing_lat REAL,
  existing_lng REAL,
  cluster_id TEXT REFERENCES cluster(id),
  updated_at TEXT NOT NULL,
  UNIQUE(job_id, path)
);

CREATE TABLE proposal (
  id TEXT PRIMARY KEY,
  photo_id TEXT NOT NULL REFERENCES photo(id),
  created_at TEXT NOT NULL,
  current INTEGER NOT NULL DEFAULT 0,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  raw_json TEXT NOT NULL,
  place_name TEXT,
  granularity TEXT,
  model_confidence REAL,
  ui_rank REAL,
  refusal TEXT,
  pin_lat REAL,
  pin_lng REAL,
  pin_source TEXT,
  model_agree INTEGER,
  geocode_json TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  reasoning_tokens INTEGER,
  latency_ms INTEGER
);

CREATE UNIQUE INDEX proposal_one_current
  ON proposal(photo_id) WHERE current = 1;

CREATE TABLE decision (
  id TEXT PRIMARY KEY,
  photo_id TEXT NOT NULL REFERENCES photo(id),
  proposal_id TEXT REFERENCES proposal(id),
  action TEXT NOT NULL,
  lat REAL,
  lng REAL,
  place_name TEXT,
  current INTEGER NOT NULL DEFAULT 0,
  decided_at TEXT NOT NULL
);

CREATE UNIQUE INDEX decision_one_current
  ON decision(photo_id) WHERE current = 1;

CREATE TABLE write_receipt (
  id TEXT PRIMARY KEY,
  photo_id TEXT NOT NULL REFERENCES photo(id),
  decision_id TEXT NOT NULL REFERENCES decision(id),
  dest_path TEXT NOT NULL,
  in_place INTEGER NOT NULL,
  status TEXT NOT NULL,
  error TEXT,
  written_at TEXT,
  written_sha256 TEXT,
  backup_path TEXT,
  UNIQUE(photo_id, dest_path)
);
"#;

pub const INDEX_SCHEMA: &str = r#"
CREATE TABLE job_index (
  job_id TEXT PRIMARY KEY,
  root_canonical TEXT NOT NULL,
  options_hash TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_ingest_at TEXT NOT NULL,
  UNIQUE(root_canonical, options_hash)
);
"#;

pub fn configure_connection(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

pub fn connect(path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    Ok(conn)
}

pub fn get_user_version(conn: &Connection) -> Result<i64, StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(version)
}

pub fn set_user_version(conn: &Connection, version: i64) -> Result<(), StoreError> {
    conn.execute_batch(&format!("PRAGMA user_version = {version};"))?;
    Ok(())
}

pub fn init_schema(conn: &Connection, schema_sql: &str, label: &str) -> Result<(), StoreError> {
    let version = get_user_version(conn)?;
    if version == 0 {
        conn.execute_batch(schema_sql)?;
        set_user_version(conn, SCHEMA_VERSION)?;
        return Ok(());
    }
    if version != SCHEMA_VERSION {
        return Err(StoreError::Schema(format!(
            "unsupported {label} schema user_version={version}; expected {SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

pub fn open_job_db(path: &Path) -> Result<Connection, StoreError> {
    let conn = connect(path)?;
    init_schema(&conn, JOB_SCHEMA, "job")?;
    Ok(conn)
}

pub fn open_index_db(path: &Path) -> Result<Connection, StoreError> {
    let conn = connect(path)?;
    init_schema(&conn, INDEX_SCHEMA, "index")?;
    Ok(conn)
}
