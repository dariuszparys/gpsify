"""Job SQLite schema v1.

Propose is asyncio: a blocking ``sqlite3`` call on the event loop would stall
vision. This package uses **aiosqlite** (the design also allows
``asyncio.to_thread`` around stdlib sqlite3). WAL mode, one writer connection
per job, readers allowed.

``PRAGMA user_version=1`` ships the **complete** v1 schema. Later PRs do not
add columns. Additive migrations only after v1.

Photo status strings are ``gpsify.types.PhotoStatus`` (PR 1 types module).
This package persists those values; it does not redefine the enum.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

import aiosqlite

SCHEMA_VERSION = 1
INDEX_DB_NAME = "gpsify.sqlite"
JOB_DB_NAME = "job.sqlite"


class StoreError(Exception):
    """Base error for the job store and global index."""


class SchemaError(StoreError):
    """Database ``user_version`` is not the v1 schema this code understands."""


def job_dir(data_dir: Path, job_id: str) -> Path:
    return data_dir / "jobs" / job_id


def job_db_path(data_dir: Path, job_id: str) -> Path:
    return job_dir(data_dir, job_id) / JOB_DB_NAME


def index_db_path(data_dir: Path) -> Path:
    return data_dir / INDEX_DB_NAME


def utc_now_iso() -> str:
    """UTC timestamp stored as TEXT (second precision, ``Z`` suffix)."""
    return (
        datetime.now(timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z")
    )


# Complete v1 schema from docs/design.md. Later PRs must not add columns.
JOB_SCHEMA = """
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
"""

INDEX_SCHEMA = """
CREATE TABLE job_index (
  job_id TEXT PRIMARY KEY,
  root_canonical TEXT NOT NULL,
  options_hash TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_ingest_at TEXT NOT NULL,
  UNIQUE(root_canonical, options_hash)
);
"""


async def configure_connection(db: aiosqlite.Connection) -> None:
    """Per-connection pragmas. ``foreign_keys`` is not persistent."""
    db.row_factory = aiosqlite.Row
    await db.execute("PRAGMA foreign_keys = ON")
    await db.execute("PRAGMA journal_mode = WAL")
    await db.execute("PRAGMA busy_timeout = 5000")


async def connect(path: Path) -> aiosqlite.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    # isolation_level must be set on connect: assigning it later touches the
    # sqlite3 connection from the caller thread and raises ProgrammingError.
    db = await aiosqlite.connect(path, isolation_level=None)
    await configure_connection(db)
    return db


async def get_user_version(db: aiosqlite.Connection) -> int:
    cursor = await db.execute("PRAGMA user_version")
    row = await cursor.fetchone()
    if row is None:
        return 0
    return int(row[0])


async def set_user_version(db: aiosqlite.Connection, version: int) -> None:
    await db.execute(f"PRAGMA user_version = {int(version)}")


async def init_schema(
    db: aiosqlite.Connection,
    schema_sql: str,
    *,
    label: str,
) -> None:
    version = await get_user_version(db)
    if version == 0:
        await db.executescript(schema_sql)
        await set_user_version(db, SCHEMA_VERSION)
        return
    if version != SCHEMA_VERSION:
        raise SchemaError(
            f"unsupported {label} schema user_version={version}; "
            f"expected {SCHEMA_VERSION}"
        )


async def open_job_db(path: Path) -> aiosqlite.Connection:
    db = await connect(path)
    try:
        await init_schema(db, JOB_SCHEMA, label="job")
    except Exception:
        await db.close()
        raise
    return db


async def open_index_db(path: Path) -> aiosqlite.Connection:
    db = await connect(path)
    try:
        await init_schema(db, INDEX_SCHEMA, label="index")
    except Exception:
        await db.close()
        raise
    return db
