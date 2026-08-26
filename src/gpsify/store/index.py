"""Required global job index at ``~/.gpsify/gpsify.sqlite`` (K11).

Resume key = canonical root path + ``options_hash`` (SHA-256 of canonical JSON
``{"recursive": bool}`` only). ``--hint`` is not part of the key.

Two simultaneous inserts of the same key: the ``UNIQUE`` constraint wins and
the second caller opens the existing ``job_id``.

``resolve_job_handle`` is the CLI's read side of that index: it turns a
UUID, a folder path, or ``last`` into a ``job_id`` so no command makes the user
retype the UUID ``ingest`` printed. It never registers or mutates a row.
"""

from __future__ import annotations

import hashlib
import json
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Self

import aiosqlite

from gpsify.store.db import (
    StoreError,
    index_db_path,
    job_db_path,
    open_index_db,
    utc_now_iso,
)

LAST_HANDLE = "last"


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

    async def list_jobs(self) -> list[dict[str, Any]]:
        """Newest ingest first. Empty when the index has no rows yet.

        ``last_ingest_at`` is second-precision, so two registrations inside the
        same second tie. ``rowid DESC`` breaks that tie by insertion order —
        deterministic and still "most recent", where ordering by ``job_id``
        would have picked a random UUID.
        """
        cursor = await self._db.execute(
            """
            SELECT job_id, root_canonical, options_hash, created_at, last_ingest_at
            FROM job_index
            ORDER BY last_ingest_at DESC, rowid DESC
            """
        )
        return [dict(row) for row in await cursor.fetchall()]

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


@dataclass(frozen=True)
class ResolvedJob:
    """A ``job_id`` plus, when the handle was ambiguous, why this row won."""

    job_id: str
    root_canonical: str
    note: str | None = None


def _looks_like_uuid(handle: str) -> bool:
    try:
        uuid.UUID(handle)
    except ValueError:
        return False
    return True


def canonical_root(folder: Path | str) -> str:
    """The ``root_canonical`` ingest would store for ``folder`` (``ingest.run_ingest``).

    Kept byte-identical to the ingest side: expanduser, then ``resolve(strict=True)``.
    A path that does not exist cannot match an index row, so the caller gets the
    "no job for this folder" message rather than a stale prefix match.
    """
    return str(Path(folder).expanduser().resolve(strict=True))


async def resolve_job_handle(data_dir: Path, handle: str) -> ResolvedJob:
    """Turn a UUID, a folder path, or ``last`` into a registered ``job_id``.

    Order matters: a UUID that is actually in the index wins outright, so a job
    can always be addressed by the id ``ingest`` printed. ``last`` reads the
    newest ``last_ingest_at``. Anything else is treated as a folder path.

    One folder can hold two jobs — ``options_hash`` covers ``{"recursive": bool}``,
    so the same root ingested both ways registers twice. Rather than fail, take the
    most recently ingested and hand back a ``note`` for the caller to echo, so the
    pick is never silent.
    """
    async with await JobIndex.open(data_dir) as index:
        if _looks_like_uuid(handle):
            row = await index.get(handle)
            if row is not None:
                return ResolvedJob(row["job_id"], row["root_canonical"])
            # The index exists to resume by path; the job database is what makes
            # a job openable. A UUID whose job.sqlite is on disk stays addressable
            # even if the index row is missing, so a lost index orphans nothing.
            if job_db_path(data_dir, handle).exists():
                return ResolvedJob(handle, "")
            raise StoreError(
                f"no job with id {handle!r}. Run 'gpsify status' to list jobs."
            )

        jobs = await index.list_jobs()
        if handle == LAST_HANDLE:
            if not jobs:
                raise StoreError("no jobs yet. Run 'gpsify ingest <folder>' first.")
            newest = jobs[0]
            return ResolvedJob(
                newest["job_id"],
                newest["root_canonical"],
                note=f"using job {newest['job_id']} ({newest['root_canonical']})",
            )

        try:
            root = canonical_root(handle)
        except (OSError, RuntimeError) as exc:
            raise StoreError(
                f"not a job id, 'last', or an existing folder: {handle!r}"
            ) from exc

        matches = [row for row in jobs if row["root_canonical"] == root]
        if not matches:
            raise StoreError(
                f"no job for folder {root}. "
                f"Run 'gpsify ingest {handle}' first, or 'gpsify status' to list jobs."
            )
        if len(matches) == 1:
            return ResolvedJob(matches[0]["job_id"], root)

        # list_jobs() is ordered last_ingest_at DESC, so matches[0] is the newest.
        winner = matches[0]
        recursive_hash = options_hash(recursive=True)
        scope = "recursive" if winner["options_hash"] == recursive_hash else "non-recursive"
        return ResolvedJob(
            winner["job_id"],
            root,
            note=(
                f"{len(matches)} jobs share this folder; "
                f"using the most recent: {winner['job_id']} ({scope})"
            ),
        )
