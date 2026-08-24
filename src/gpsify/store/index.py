"""Required global job index at ``~/.gpsify/gpsify.sqlite`` (K11).

Resume key = canonical root path + ``options_hash`` (SHA-256 of canonical JSON
``{"recursive": bool}`` only). ``--hint`` is not part of the key.

Two simultaneous inserts of the same key: the ``UNIQUE`` constraint wins and
the second caller opens the existing ``job_id``.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any, Self

import aiosqlite

from gpsify.store.db import StoreError, index_db_path, open_index_db, utc_now_iso


class JobConflictError(StoreError):
    """Index unique key or primary key conflict that is not a resume hit."""


def options_hash(*, recursive: bool) -> str:
    """SHA-256 of canonical JSON ``{"recursive": bool}`` (sorted keys, no spaces)."""
    payload = json.dumps({"recursive": recursive}, separators=(",", ":"), sort_keys=True)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def _row(row: aiosqlite.Row | None) -> dict[str, Any] | None:
    if row is None:
        return None
    return dict(row)


class JobIndex:
    """One writer connection to the required global ``gpsify.sqlite`` index."""

    def __init__(self, db: aiosqlite.Connection, path: Path) -> None:
        self._db = db
        self.path = path

    @classmethod
    async def open(cls, data_dir: Path) -> Self:
        path = index_db_path(data_dir)
        db = await open_index_db(path)
        return cls(db, path)

    async def close(self) -> None:
        if self._db is not None:
            await self._db.close()
            self._db = None  # type: ignore[assignment]

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    async def lookup(
        self, root_canonical: str, options_hash_value: str
    ) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            """
            SELECT job_id, root_canonical, options_hash, created_at, last_ingest_at
            FROM job_index
            WHERE root_canonical = ? AND options_hash = ?
            """,
            (root_canonical, options_hash_value),
        )
        return _row(await cursor.fetchone())

    async def get(self, job_id: str) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            """
            SELECT job_id, root_canonical, options_hash, created_at, last_ingest_at
            FROM job_index
            WHERE job_id = ?
            """,
            (job_id,),
        )
        return _row(await cursor.fetchone())

    async def register(
        self,
        job_id: str,
        root_canonical: str,
        options_hash_value: str,
        *,
        now: str | None = None,
    ) -> dict[str, Any]:
        """Insert a job, or return the existing row when the resume key matches.

        A colliding ``job_id`` that is not the same resume key is an error.
        """
        stamp = now or utc_now_iso()
        try:
            await self._db.execute("BEGIN IMMEDIATE")
            await self._db.execute(
                """
                INSERT INTO job_index (
                    job_id, root_canonical, options_hash, created_at, last_ingest_at
                ) VALUES (?, ?, ?, ?, ?)
                """,
                (job_id, root_canonical, options_hash_value, stamp, stamp),
            )
            await self._db.commit()
        except aiosqlite.IntegrityError:
            await self._db.rollback()
            existing = await self.lookup(root_canonical, options_hash_value)
            if existing is not None:
                return existing
            raise JobConflictError(
                f"job_id {job_id!r} already exists with a different resume key"
            ) from None

        row = await self.get(job_id)
        if row is None:
            raise StoreError(f"failed to register job_id {job_id!r}")
        return row

    async def touch_ingest(self, job_id: str, *, now: str | None = None) -> None:
        stamp = now or utc_now_iso()
        cursor = await self._db.execute(
            "UPDATE job_index SET last_ingest_at = ? WHERE job_id = ?",
            (stamp, job_id),
        )
        if cursor.rowcount == 0:
            raise StoreError(f"unknown job_id {job_id!r}")

    async def relocate(
        self,
        job_id: str,
        new_root_canonical: str,
        *,
        now: str | None = None,
    ) -> dict[str, Any]:
        """Rebind ``root_canonical`` after a folder move. ``options_hash`` is unchanged."""
        current = await self.get(job_id)
        if current is None:
            raise StoreError(f"unknown job_id {job_id!r}")
        stamp = now or utc_now_iso()
        try:
            await self._db.execute("BEGIN IMMEDIATE")
            await self._db.execute(
                """
                UPDATE job_index
                SET root_canonical = ?, last_ingest_at = ?
                WHERE job_id = ?
                """,
                (new_root_canonical, stamp, job_id),
            )
            await self._db.commit()
        except aiosqlite.IntegrityError as exc:
            await self._db.rollback()
            raise JobConflictError(
                "relocate would collide with an existing resume key"
            ) from exc
        row = await self.get(job_id)
        if row is None:
            raise StoreError(f"failed to relocate job_id {job_id!r}")
        return row
