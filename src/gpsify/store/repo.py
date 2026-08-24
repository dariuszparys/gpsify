"""Job repository: schema-backed CRUD plus ``add_decision`` (K12).

``add_decision`` is the **only** mutator for decisions. It last-write-wins
``decision.current`` and mirrors the current action onto ``photo.status``.
Apply later reads the current decision row only — never ``proposal.pin_*``.
"""

from __future__ import annotations

import math
import uuid
from pathlib import Path
from typing import Any, Never, Self

import aiosqlite

from gpsify.store.db import StoreError, job_db_path, open_job_db, utc_now_iso
from gpsify.types import DecisionAction, PhotoStatus, PinSource, WriteReceiptStatus

WRITEABLE_DECISION_ACTIONS = frozenset(
    {
        DecisionAction.accept,
        DecisionAction.accept_edited,
        DecisionAction.cluster_apply,
    }
)


class DecisionError(StoreError):
    """Rejected decision (maps to HTTP 400 in the review API)."""


def _new_id() -> str:
    return str(uuid.uuid4())


def _row(row: aiosqlite.Row | None) -> dict[str, Any] | None:
    if row is None:
        return None
    return dict(row)


def _finite(value: float | None) -> bool:
    return value is not None and math.isfinite(value)


def _as_action(action: DecisionAction | str) -> DecisionAction:
    if isinstance(action, DecisionAction):
        return action
    try:
        return DecisionAction(action)
    except ValueError:
        raise DecisionError(f"unknown action {action!r}") from None


def _as_photo_status(status: PhotoStatus | str) -> PhotoStatus:
    if isinstance(status, PhotoStatus):
        return status
    try:
        return PhotoStatus(status)
    except ValueError:
        raise StoreError(f"unknown photo status {status!r}") from None


def _as_receipt_status(status: WriteReceiptStatus | str) -> WriteReceiptStatus:
    if isinstance(status, WriteReceiptStatus):
        return status
    try:
        return WriteReceiptStatus(status)
    except ValueError:
        raise StoreError(f"unknown write_receipt status {status!r}") from None


def _unreachable(value: Never) -> Never:
    raise DecisionError(f"unhandled value: {value!r}")


def _pin_allows_accept_copy(pin_source: str | None) -> bool:
    if pin_source is None:
        return False
    try:
        source = PinSource(pin_source)
    except ValueError:
        return False
    match source:
        case PinSource.geocoder | PinSource.user:
            return True
        case PinSource.none:
            return False
        case _:
            _unreachable(source)


class JobRepo:
    """One writer connection to ``jobs/<id>/job.sqlite``."""

    def __init__(self, db: aiosqlite.Connection, job_id: str, path: Path) -> None:
        self._db = db
        self.job_id = job_id
        self.path = path

    @classmethod
    async def create(
        cls,
        data_dir: Path,
        *,
        job_id: str,
        root_path: str,
        root_canonical: str,
        options_hash: str,
        hint: str | None,
        config_json: str,
        created_at: str | None = None,
    ) -> Self:
        path = job_db_path(data_dir, job_id)
        if path.exists():
            raise StoreError(f"job database already exists: {path}")
        db = await open_job_db(path)
        repo = cls(db, job_id, path)
        stamp = created_at or utc_now_iso()
        try:
            await db.execute(
                """
                INSERT INTO job (
                    id, root_path, root_canonical, options_hash, hint,
                    created_at, config_json, stats_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, '{}')
                """,
                (
                    job_id,
                    root_path,
                    root_canonical,
                    options_hash,
                    hint,
                    stamp,
                    config_json,
                ),
            )
        except Exception:
            await repo.close()
            raise
        return repo

    @classmethod
    async def open(cls, data_dir: Path, job_id: str) -> Self:
        path = job_db_path(data_dir, job_id)
        if not path.is_file():
            raise StoreError(f"job database not found: {path}")
        db = await open_job_db(path)
        repo = cls(db, job_id, path)
        job = await repo.get_job()
        if job is None:
            await repo.close()
            raise StoreError(f"job row missing in {path}")
        if job["id"] != job_id:
            await repo.close()
            raise StoreError(f"job id mismatch: db has {job['id']!r}, expected {job_id!r}")
        return repo

    async def close(self) -> None:
        if self._db is not None:
            await self._db.close()
            self._db = None  # type: ignore[assignment]

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    async def get_job(self) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            """
            SELECT id, root_path, root_canonical, options_hash, hint,
                   created_at, config_json, stats_json
            FROM job
            WHERE id = ?
            """,
            (self.job_id,),
        )
        return _row(await cursor.fetchone())

    async def set_hint(self, hint: str | None) -> None:
        await self._db.execute(
            "UPDATE job SET hint = ? WHERE id = ?",
            (hint, self.job_id),
        )

    async def set_stats_json(self, stats_json: str) -> None:
        await self._db.execute(
            "UPDATE job SET stats_json = ? WHERE id = ?",
            (stats_json, self.job_id),
        )

    async def set_root(self, root_path: str, root_canonical: str) -> None:
        await self._db.execute(
            "UPDATE job SET root_path = ?, root_canonical = ? WHERE id = ?",
            (root_path, root_canonical, self.job_id),
        )

    async def insert_cluster(
        self,
        *,
        cluster_id: str | None = None,
        parent: str,
        first_ts: str | None,
        n: int,
        undated: bool = False,
    ) -> str:
        cid = cluster_id or _new_id()
        await self._db.execute(
            """
            INSERT INTO cluster (id, job_id, parent, first_ts, n, undated)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (cid, self.job_id, parent, first_ts, n, int(undated)),
        )
        return cid

    async def get_cluster(self, cluster_id: str) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            """
            SELECT id, job_id, parent, first_ts, n, undated
            FROM cluster
            WHERE id = ?
            """,
            (cluster_id,),
        )
        return _row(await cursor.fetchone())

    async def insert_photo(
        self,
        *,
        photo_id: str | None = None,
        path: str,
        relpath: str,
        sha256: str,
        size: int,
        mtime_ns: int,
        status: PhotoStatus | str,
        error: str | None = None,
        datetime_original: str | None = None,
        make: str | None = None,
        model: str | None = None,
        has_gps: bool = False,
        existing_lat: float | None = None,
        existing_lng: float | None = None,
        cluster_id: str | None = None,
        updated_at: str | None = None,
    ) -> str:
        pid = photo_id or _new_id()
        stamp = updated_at or utc_now_iso()
        status_value = _as_photo_status(status).value
        await self._db.execute(
            """
            INSERT INTO photo (
                id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                datetime_original, make, model, has_gps, existing_lat, existing_lng,
                cluster_id, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                pid,
                self.job_id,
                path,
                relpath,
                sha256,
                size,
                mtime_ns,
                status_value,
                error,
                datetime_original,
                make,
                model,
                int(has_gps),
                existing_lat,
                existing_lng,
                cluster_id,
                stamp,
            ),
        )
        return pid

    async def get_photo(self, photo_id: str) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            """
            SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                   datetime_original, make, model, has_gps, existing_lat, existing_lng,
                   cluster_id, updated_at
            FROM photo
            WHERE id = ?
            """,
            (photo_id,),
        )
        return _row(await cursor.fetchone())

    async def get_photo_by_path(self, path: str) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            """
            SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                   datetime_original, make, model, has_gps, existing_lat, existing_lng,
                   cluster_id, updated_at
            FROM photo
            WHERE job_id = ? AND path = ?
            """,
            (self.job_id, path),
        )
        return _row(await cursor.fetchone())

    async def list_photos(self) -> list[dict[str, Any]]:
        cursor = await self._db.execute(
            """
            SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                   datetime_original, make, model, has_gps, existing_lat, existing_lng,
                   cluster_id, updated_at
            FROM photo
            WHERE job_id = ?
            ORDER BY relpath
            """,
            (self.job_id,),
        )
        return [dict(row) for row in await cursor.fetchall()]

    async def list_photos_by_sha256(self, sha256: str) -> list[dict[str, Any]]:
        cursor = await self._db.execute(
            """
            SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                   datetime_original, make, model, has_gps, existing_lat, existing_lng,
                   cluster_id, updated_at
            FROM photo
            WHERE job_id = ? AND sha256 = ?
            """,
            (self.job_id, sha256),
        )
        return [dict(row) for row in await cursor.fetchall()]

    async def count_photos_by_status(self) -> dict[str, int]:
        cursor = await self._db.execute(
            """
            SELECT status, COUNT(*) AS n
            FROM photo
            WHERE job_id = ?
            GROUP BY status
            """,
            (self.job_id,),
        )
        return {str(row["status"]): int(row["n"]) for row in await cursor.fetchall()}

    async def update_photo(self, photo_id: str, **fields: Any) -> None:
        """Patch selected photo columns. Unknown keys are rejected."""
        if not fields:
            return
        allowed = {
            "path",
            "relpath",
            "sha256",
            "size",
            "mtime_ns",
            "status",
            "error",
            "datetime_original",
            "make",
            "model",
            "has_gps",
            "existing_lat",
            "existing_lng",
            "cluster_id",
            "updated_at",
        }
        unknown = set(fields) - allowed
        if unknown:
            raise StoreError(f"cannot update photo fields {sorted(unknown)}")
        values = dict(fields)
        if "status" in values:
            values["status"] = _as_photo_status(values["status"]).value
        if "has_gps" in values:
            values["has_gps"] = int(bool(values["has_gps"]))
        if "updated_at" not in values:
            values["updated_at"] = utc_now_iso()
        assignments = ", ".join(f"{column} = ?" for column in values)
        cursor = await self._db.execute(
            f"UPDATE photo SET {assignments} WHERE id = ?",
            (*values.values(), photo_id),
        )
        if cursor.rowcount == 0:
            raise StoreError(f"unknown photo {photo_id!r}")

    async def clear_current_proposal(self, photo_id: str) -> None:
        await self._db.execute(
            "UPDATE proposal SET current = 0 WHERE photo_id = ? AND current = 1",
            (photo_id,),
        )

    async def clear_current_decision(self, photo_id: str) -> None:
        await self._db.execute(
            "UPDATE decision SET current = 0 WHERE photo_id = ? AND current = 1",
            (photo_id,),
        )

    async def list_clusters(self) -> list[dict[str, Any]]:
        cursor = await self._db.execute(
            """
            SELECT id, job_id, parent, first_ts, n, undated
            FROM cluster
            WHERE job_id = ?
            ORDER BY parent, first_ts
            """,
            (self.job_id,),
        )
        return [dict(row) for row in await cursor.fetchall()]

    async def delete_clusters(self) -> None:
        """Drop cluster rows for this job after detaching photos (FK-safe)."""
        await self._db.execute(
            "UPDATE photo SET cluster_id = NULL WHERE job_id = ?",
            (self.job_id,),
        )
        await self._db.execute("DELETE FROM cluster WHERE job_id = ?", (self.job_id,))

    async def set_photo_status(
        self,
        photo_id: str,
        status: PhotoStatus | str,
        *,
        error: str | None = None,
        updated_at: str | None = None,
    ) -> None:
        stamp = updated_at or utc_now_iso()
        cursor = await self._db.execute(
            "UPDATE photo SET status = ?, error = ?, updated_at = ? WHERE id = ?",
            (_as_photo_status(status).value, error, stamp, photo_id),
        )
        if cursor.rowcount == 0:
            raise StoreError(f"unknown photo {photo_id!r}")

    async def reset_stale_proposing(self) -> int:
        """Crash repair: ``proposing`` → ``pending``. Called at every propose start."""
        stamp = utc_now_iso()
        cursor = await self._db.execute(
            """
            UPDATE photo
            SET status = ?, updated_at = ?
            WHERE job_id = ? AND status = ?
            """,
            (
                PhotoStatus.pending.value,
                stamp,
                self.job_id,
                PhotoStatus.proposing.value,
            ),
        )
        return int(cursor.rowcount)

    async def insert_proposal(
        self,
        *,
        photo_id: str,
        provider: str,
        model: str,
        prompt_version: str,
        raw_json: str,
        place_name: str | None = None,
        granularity: str | None = None,
        model_confidence: float | None = None,
        ui_rank: float | None = None,
        refusal: str | None = None,
        pin_lat: float | None = None,
        pin_lng: float | None = None,
        pin_source: str | PinSource | None = None,
        model_agree: int | None = None,
        geocode_json: str | None = None,
        input_tokens: int | None = None,
        output_tokens: int | None = None,
        reasoning_tokens: int | None = None,
        latency_ms: int | None = None,
        current: bool = True,
        proposal_id: str | None = None,
        created_at: str | None = None,
    ) -> str:
        pid = proposal_id or _new_id()
        stamp = created_at or utc_now_iso()
        pin_source_value = pin_source.value if isinstance(pin_source, PinSource) else pin_source
        current_int = 1 if current else 0
        await self._db.execute("BEGIN IMMEDIATE")
        try:
            if current_int == 1:
                await self._db.execute(
                    "UPDATE proposal SET current = 0 WHERE photo_id = ? AND current = 1",
                    (photo_id,),
                )
            await self._db.execute(
                """
                INSERT INTO proposal (
                    id, photo_id, created_at, current, provider, model, prompt_version,
                    raw_json, place_name, granularity, model_confidence, ui_rank, refusal,
                    pin_lat, pin_lng, pin_source, model_agree, geocode_json,
                    input_tokens, output_tokens, reasoning_tokens, latency_ms
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    pid,
                    photo_id,
                    stamp,
                    current_int,
                    provider,
                    model,
                    prompt_version,
                    raw_json,
                    place_name,
                    granularity,
                    model_confidence,
                    ui_rank,
                    refusal,
                    pin_lat,
                    pin_lng,
                    pin_source_value,
                    model_agree,
                    geocode_json,
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    latency_ms,
                ),
            )
            await self._db.commit()
        except Exception:
            await self._db.rollback()
            raise
        return pid

    async def get_proposal(self, proposal_id: str) -> dict[str, Any] | None:
        cursor = await self._db.execute("SELECT * FROM proposal WHERE id = ?", (proposal_id,))
        return _row(await cursor.fetchone())

    async def get_current_proposal(self, photo_id: str) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            "SELECT * FROM proposal WHERE photo_id = ? AND current = 1",
            (photo_id,),
        )
        return _row(await cursor.fetchone())

    async def list_current_proposals(self) -> list[dict[str, Any]]:
        cursor = await self._db.execute(
            """
            SELECT proposal.*
            FROM proposal
            JOIN photo ON photo.id = proposal.photo_id
            WHERE photo.job_id = ? AND proposal.current = 1
            """,
            (self.job_id,),
        )
        return [dict(row) for row in await cursor.fetchall()]

    async def add_decision(
        self,
        photo_id: str,
        action: DecisionAction | str,
        lat: float | None = None,
        lng: float | None = None,
        proposal_id: str | None = None,
        place_name: str | None = None,
    ) -> str:
        """Last-write-wins decision mutator. See design ``add_decision`` contract."""
        action_enum = _as_action(action)
        photo = await self.get_photo(photo_id)
        if photo is None:
            raise DecisionError(f"unknown photo {photo_id!r}")
        if photo["status"] == PhotoStatus.written.value:
            raise DecisionError("photo is written; undo first")

        resolved_lat: float | None
        resolved_lng: float | None
        resolved_place = place_name
        resolved_proposal = proposal_id
        photo_status: PhotoStatus

        match action_enum:
            case DecisionAction.skip:
                if lat is not None or lng is not None:
                    raise DecisionError("skip must not include coordinates")
                resolved_lat = None
                resolved_lng = None
                photo_status = PhotoStatus.skipped_by_user
            case DecisionAction.accept:
                resolved_lat, resolved_lng, resolved_place, resolved_proposal = (
                    await self._resolve_accept(
                        photo_id,
                        lat=lat,
                        lng=lng,
                        place_name=place_name,
                        proposal_id=proposal_id,
                    )
                )
                photo_status = PhotoStatus.accepted
            case DecisionAction.accept_edited:
                if not _finite(lat) or not _finite(lng):
                    raise DecisionError("accept_edited requires finite lat/lng")
                assert lat is not None and lng is not None
                resolved_lat = lat
                resolved_lng = lng
                photo_status = PhotoStatus.accepted_edited
            case DecisionAction.cluster_apply:
                if not _finite(lat) or not _finite(lng):
                    raise DecisionError("cluster_apply requires finite lat/lng")
                assert lat is not None and lng is not None
                resolved_lat = lat
                resolved_lng = lng
                photo_status = PhotoStatus.accepted_edited
            case _:
                _unreachable(action_enum)

        if resolved_proposal is None and action_enum is not DecisionAction.skip:
            current = await self.get_current_proposal(photo_id)
            if current is not None:
                resolved_proposal = current["id"]
                if resolved_place is None:
                    resolved_place = current["place_name"]

        if action_enum in WRITEABLE_DECISION_ACTIONS:
            if not _finite(resolved_lat) or not _finite(resolved_lng):
                raise DecisionError(
                    f"{action_enum.value} must persist finite lat/lng"
                )

        decision_id = _new_id()
        stamp = utc_now_iso()
        await self._db.execute("BEGIN IMMEDIATE")
        try:
            await self._db.execute(
                "UPDATE decision SET current = 0 WHERE photo_id = ? AND current = 1",
                (photo_id,),
            )
            await self._db.execute(
                """
                INSERT INTO decision (
                    id, photo_id, proposal_id, action, lat, lng, place_name,
                    current, decided_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?)
                """,
                (
                    decision_id,
                    photo_id,
                    resolved_proposal,
                    action_enum.value,
                    resolved_lat,
                    resolved_lng,
                    resolved_place,
                    stamp,
                ),
            )
            await self._db.execute(
                "UPDATE photo SET status = ?, updated_at = ? WHERE id = ?",
                (photo_status.value, stamp, photo_id),
            )
            await self._db.commit()
        except Exception:
            await self._db.rollback()
            raise
        return decision_id

    async def _resolve_accept(
        self,
        photo_id: str,
        *,
        lat: float | None,
        lng: float | None,
        place_name: str | None,
        proposal_id: str | None,
    ) -> tuple[float, float, str | None, str | None]:
        supplied = lat is not None or lng is not None
        if supplied:
            if not _finite(lat) or not _finite(lng):
                raise DecisionError("accept lat/lng must both be finite")
            assert lat is not None and lng is not None
            return lat, lng, place_name, proposal_id

        current = await self.get_current_proposal(photo_id)
        if current is None:
            raise DecisionError("accept requires a current proposal")
        if not _pin_allows_accept_copy(current["pin_source"]):
            raise DecisionError("accept rejected: pin_source is not geocoder or user")
        pin_lat = current["pin_lat"]
        pin_lng = current["pin_lng"]
        if not _finite(pin_lat) or not _finite(pin_lng):
            raise DecisionError("accept rejected: proposal pin is missing or non-finite")
        copied_place = place_name if place_name is not None else current["place_name"]
        return pin_lat, pin_lng, copied_place, current["id"]

    async def get_decision(self, decision_id: str) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            "SELECT * FROM decision WHERE id = ?",
            (decision_id,),
        )
        return _row(await cursor.fetchone())

    async def get_current_decision(self, photo_id: str) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            "SELECT * FROM decision WHERE photo_id = ? AND current = 1",
            (photo_id,),
        )
        return _row(await cursor.fetchone())

    async def list_current_decisions(self) -> list[dict[str, Any]]:
        cursor = await self._db.execute(
            """
            SELECT decision.*
            FROM decision
            JOIN photo ON photo.id = decision.photo_id
            WHERE photo.job_id = ? AND decision.current = 1
            """,
            (self.job_id,),
        )
        return [dict(row) for row in await cursor.fetchall()]

    async def list_photos_in_cluster(self, cluster_id: str) -> list[dict[str, Any]]:
        cursor = await self._db.execute(
            """
            SELECT id, job_id, path, relpath, sha256, size, mtime_ns, status, error,
                   datetime_original, make, model, has_gps, existing_lat, existing_lng,
                   cluster_id, updated_at
            FROM photo
            WHERE job_id = ? AND cluster_id = ?
            ORDER BY relpath
            """,
            (self.job_id, cluster_id),
        )
        return [dict(row) for row in await cursor.fetchall()]

    async def upsert_write_receipt(
        self,
        *,
        photo_id: str,
        decision_id: str,
        dest_path: str,
        in_place: bool,
        status: WriteReceiptStatus | str,
        error: str | None = None,
        written_at: str | None = None,
        written_sha256: str | None = None,
        backup_path: str | None = None,
        receipt_id: str | None = None,
    ) -> str:
        status_value = _as_receipt_status(status).value
        existing = await self.get_write_receipt(photo_id, dest_path)
        rid = existing["id"] if existing is not None else (receipt_id or _new_id())
        await self._db.execute(
            """
            INSERT INTO write_receipt (
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
                backup_path = excluded.backup_path
            """,
            (
                rid,
                photo_id,
                decision_id,
                dest_path,
                int(in_place),
                status_value,
                error,
                written_at,
                written_sha256,
                backup_path,
            ),
        )
        return rid

    async def list_write_receipts(self) -> list[dict[str, Any]]:
        cursor = await self._db.execute(
            """
            SELECT write_receipt.*
            FROM write_receipt
            JOIN photo ON photo.id = write_receipt.photo_id
            WHERE photo.job_id = ?
            ORDER BY write_receipt.dest_path
            """,
            (self.job_id,),
        )
        return [dict(row) for row in await cursor.fetchall()]

    async def get_write_receipt(
        self, photo_id: str, dest_path: str
    ) -> dict[str, Any] | None:
        cursor = await self._db.execute(
            """
            SELECT * FROM write_receipt
            WHERE photo_id = ? AND dest_path = ?
            """,
            (photo_id, dest_path),
        )
        return _row(await cursor.fetchone())
