from __future__ import annotations

import asyncio
import hashlib
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar

import aiosqlite
import pytest

from gpsify.store import (
    SCHEMA_VERSION,
    DecisionError,
    JobConflictError,
    JobIndex,
    JobRepo,
    SchemaError,
    options_hash,
)
from gpsify.store import db as db_module
from gpsify.store import repo as repo_module
from gpsify.store.db import (
    get_user_version,
    index_db_path,
    job_db_path,
    open_index_db,
    open_job_db,
)
from gpsify.types import (
    DecisionAction,
    PhotoStatus,
    PinSource,
    WriteReceiptStatus,
)

T = TypeVar("T")

JOB_TABLES = frozenset({"job", "cluster", "photo", "proposal", "decision", "write_receipt"})
PHOTO_COLUMNS = frozenset(
    {
        "id",
        "job_id",
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
)
PROPOSAL_COLUMNS = frozenset(
    {
        "id",
        "photo_id",
        "created_at",
        "current",
        "provider",
        "model",
        "prompt_version",
        "raw_json",
        "place_name",
        "granularity",
        "model_confidence",
        "ui_rank",
        "refusal",
        "pin_lat",
        "pin_lng",
        "pin_source",
        "model_agree",
        "geocode_json",
        "input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "latency_ms",
    }
)
RECEIPT_COLUMNS = frozenset(
    {
        "id",
        "photo_id",
        "decision_id",
        "dest_path",
        "in_place",
        "status",
        "error",
        "written_at",
        "written_sha256",
        "backup_path",
    }
)


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


async def _create_repo(data_dir: Path, job_id: str = "11111111-1111-4111-8111-111111111111") -> JobRepo:
    return await JobRepo.create(
        data_dir,
        job_id=job_id,
        root_path="/photos/trip",
        root_canonical="/photos/trip",
        options_hash=options_hash(recursive=False),
        hint="weekend in Porto",
        config_json="{}",
    )


async def _pending_photo(repo: JobRepo, suffix: str = "a") -> str:
    return await repo.insert_photo(
        path=f"/photos/trip/IMG_{suffix}.jpg",
        relpath=f"IMG_{suffix}.jpg",
        sha256="a" * 64,
        size=1024,
        mtime_ns=1,
        status=PhotoStatus.pending,
    )


async def _proposal(
    repo: JobRepo,
    photo_id: str,
    *,
    pin_source: PinSource = PinSource.geocoder,
    pin_lat: float | None = 41.1456,
    pin_lng: float | None = -8.6143,
    place_name: str | None = "Clérigos Church, Porto",
) -> str:
    return await repo.insert_proposal(
        photo_id=photo_id,
        provider="xai",
        model="grok-4.6",
        prompt_version="PROMPT_V1",
        raw_json="{}",
        place_name=place_name,
        granularity="exact",
        model_confidence=0.8,
        ui_rank=0.83,
        refusal="none",
        pin_lat=pin_lat,
        pin_lng=pin_lng,
        pin_source=pin_source,
        input_tokens=2100,
        output_tokens=520,
        reasoning_tokens=800,
        latency_ms=6120,
    )


async def _table_names(db: aiosqlite.Connection) -> set[str]:
    cursor = await db.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
    )
    rows = await cursor.fetchall()
    return {row["name"] for row in rows}


async def _columns(db: aiosqlite.Connection, table: str) -> set[str]:
    cursor = await db.execute(f"PRAGMA table_info({table})")
    rows = await cursor.fetchall()
    return {row["name"] for row in rows}


def test_photo_status_stays_in_types() -> None:
    assert not hasattr(db_module, "PhotoStatus")
    assert repo_module.PhotoStatus is PhotoStatus
    assert {member.value for member in PhotoStatus} >= {
        "pending",
        "proposed",
        "accepted",
        "accepted_edited",
        "skipped_by_user",
        "written",
        "undone",
        "missing",
    }


def test_options_hash_is_canonical_recursive_json_only() -> None:
    expected = hashlib.sha256(b'{"recursive":false}').hexdigest()
    assert options_hash(recursive=False) == expected
    assert options_hash(recursive=True) == hashlib.sha256(b'{"recursive":true}').hexdigest()
    assert options_hash(recursive=False) != options_hash(recursive=True)


def test_job_schema_v1_complete(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            db = repo._db
            assert await get_user_version(db) == SCHEMA_VERSION == 1
            mode = await (await db.execute("PRAGMA journal_mode")).fetchone()
            assert mode is not None
            assert str(mode[0]).lower() == "wal"
            fk = await (await db.execute("PRAGMA foreign_keys")).fetchone()
            assert fk is not None
            assert int(fk[0]) == 1
            assert JOB_TABLES <= await _table_names(db)
            assert PHOTO_COLUMNS <= await _columns(db, "photo")
            assert PROPOSAL_COLUMNS <= await _columns(db, "proposal")
            assert RECEIPT_COLUMNS <= await _columns(db, "write_receipt")
            assert "stats_json" in await _columns(db, "job")
            job = await repo.get_job()
            assert job is not None
            assert job["stats_json"] == "{}"
            indexes = await (
                await db.execute(
                    "SELECT name FROM sqlite_master WHERE type = 'index' AND name != ''"
                )
            ).fetchall()
            names = {row["name"] for row in indexes}
            assert "proposal_one_current" in names
            assert "decision_one_current" in names

    run(body())


def test_unsupported_user_version_is_rejected(data_dir: Path) -> None:
    async def body() -> None:
        path = job_db_path(data_dir, "schema-mismatch")
        db = await open_job_db(path)
        await db.execute("PRAGMA user_version = 99")
        await db.close()
        with pytest.raises(SchemaError, match="user_version=99"):
            await open_job_db(path)

    run(body())


def test_foreign_keys_reject_orphan_proposal(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            with pytest.raises(aiosqlite.IntegrityError):
                await repo.insert_proposal(
                    photo_id="missing-photo",
                    provider="xai",
                    model="grok-4.6",
                    prompt_version="PROMPT_V1",
                    raw_json="{}",
                )

    run(body())


def test_index_register_lookup_and_resume(data_dir: Path) -> None:
    async def body() -> None:
        digest = options_hash(recursive=False)
        async with await JobIndex.open(data_dir) as index:
            mode = await (await index._db.execute("PRAGMA journal_mode")).fetchone()
            assert mode is not None
            assert str(mode[0]).lower() == "wal"
            assert await get_user_version(index._db) == SCHEMA_VERSION
            first = await index.register(
                "job-a",
                "/photos/trip",
                digest,
            )
            resumed = await index.register(
                "job-other-uuid",
                "/photos/trip",
                digest,
            )
            assert resumed["job_id"] == first["job_id"] == "job-a"
            found = await index.lookup("/photos/trip", digest)
            assert found is not None
            assert found["job_id"] == "job-a"
            await index.touch_ingest("job-a", now="2026-08-12T00:00:00Z")
            touched = await index.get("job-a")
            assert touched is not None
            assert touched["last_ingest_at"] == "2026-08-12T00:00:00Z"
            listed = await index.list_jobs()
            assert [row["job_id"] for row in listed] == ["job-a"]
            relocated = await index.relocate("job-a", "/photos/trip-moved")
            assert relocated["root_canonical"] == "/photos/trip-moved"
            assert await index.lookup("/photos/trip", digest) is None
            assert index_db_path(data_dir).is_file()

    run(body())


def test_index_unique_job_id_conflict(data_dir: Path) -> None:
    async def body() -> None:
        async with await JobIndex.open(data_dir) as index:
            await index.register("job-a", "/a", options_hash(recursive=False))
            with pytest.raises(JobConflictError, match="already exists"):
                await index.register("job-a", "/b", options_hash(recursive=False))

    run(body())


def test_index_relocate_collision(data_dir: Path) -> None:
    async def body() -> None:
        digest = options_hash(recursive=False)
        async with await JobIndex.open(data_dir) as index:
            await index.register("job-a", "/a", digest)
            await index.register("job-b", "/b", digest)
            with pytest.raises(JobConflictError, match="collide"):
                await index.relocate("job-b", "/a")

    run(body())


def test_add_decision_skip_sets_status_and_null_coords(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            decision_id = await repo.add_decision(photo_id, DecisionAction.skip)
            photo = await repo.get_photo(photo_id)
            decision = await repo.get_decision(decision_id)
            assert photo is not None and decision is not None
            assert photo["status"] == PhotoStatus.skipped_by_user.value
            assert decision["action"] == DecisionAction.skip.value
            assert decision["lat"] is None
            assert decision["lng"] is None
            assert decision["current"] == 1
            with pytest.raises(DecisionError, match="must not include coordinates"):
                await repo.add_decision(photo_id, "skip", lat=41.0, lng=-8.0)

    run(body())


def test_add_decision_accept_copies_geocoder_pin(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            proposal_id = await _proposal(repo, photo_id, pin_source=PinSource.geocoder)
            decision_id = await repo.add_decision(photo_id, "accept")
            decision = await repo.get_decision(decision_id)
            photo = await repo.get_photo(photo_id)
            assert decision is not None and photo is not None
            assert decision["lat"] == pytest.approx(41.1456)
            assert decision["lng"] == pytest.approx(-8.6143)
            assert decision["place_name"] == "Clérigos Church, Porto"
            assert decision["proposal_id"] == proposal_id
            assert photo["status"] == PhotoStatus.accepted.value

    run(body())


def test_add_decision_accept_copies_user_pin(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            await _proposal(repo, photo_id, pin_source=PinSource.user, pin_lat=1.0, pin_lng=2.0)
            decision_id = await repo.add_decision(photo_id, DecisionAction.accept)
            decision = await repo.get_decision(decision_id)
            assert decision is not None
            assert decision["lat"] == pytest.approx(1.0)
            assert decision["lng"] == pytest.approx(2.0)

    run(body())


def test_add_decision_accept_rejects_pin_source_none(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            await _proposal(repo, photo_id, pin_source=PinSource.none, pin_lat=None, pin_lng=None)
            with pytest.raises(DecisionError, match="pin_source"):
                await repo.add_decision(photo_id, DecisionAction.accept)

    run(body())


def test_add_decision_accept_rejects_missing_proposal(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            with pytest.raises(DecisionError, match="current proposal"):
                await repo.add_decision(photo_id, DecisionAction.accept)

    run(body())


def test_add_decision_accept_rejects_non_finite_pin(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            await _proposal(repo, photo_id, pin_lat=float("nan"), pin_lng=-8.0)
            with pytest.raises(DecisionError, match="non-finite"):
                await repo.add_decision(photo_id, DecisionAction.accept)

    run(body())


def test_add_decision_accept_edited_and_cluster_apply_require_coords(
    data_dir: Path,
) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            await _proposal(repo, photo_id)
            with pytest.raises(DecisionError, match="accept_edited requires finite"):
                await repo.add_decision(photo_id, DecisionAction.accept_edited)
            with pytest.raises(DecisionError, match="cluster_apply requires finite"):
                await repo.add_decision(photo_id, DecisionAction.cluster_apply)
            edited_id = await repo.add_decision(
                photo_id,
                DecisionAction.accept_edited,
                lat=41.15,
                lng=-8.61,
                place_name="edited",
            )
            edited = await repo.get_decision(edited_id)
            photo = await repo.get_photo(photo_id)
            assert edited is not None and photo is not None
            assert edited["lat"] == pytest.approx(41.15)
            assert photo["status"] == PhotoStatus.accepted_edited.value
            cluster_id = await repo.add_decision(
                photo_id,
                DecisionAction.cluster_apply,
                lat=41.14,
                lng=-8.61,
            )
            cluster = await repo.get_decision(cluster_id)
            assert cluster is not None
            assert cluster["current"] == 1
            assert edited["id"] != cluster["id"]
            old = await repo.get_decision(edited_id)
            assert old is not None
            assert old["current"] == 0

    run(body())


def test_add_decision_last_write_wins(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            await _proposal(repo, photo_id)
            accept_id = await repo.add_decision(photo_id, DecisionAction.accept)
            skip_id = await repo.add_decision(photo_id, DecisionAction.skip)
            current = await repo.get_current_decision(photo_id)
            photo = await repo.get_photo(photo_id)
            assert current is not None and photo is not None
            assert current["id"] == skip_id
            assert current["action"] == DecisionAction.skip.value
            assert photo["status"] == PhotoStatus.skipped_by_user.value
            previous = await repo.get_decision(accept_id)
            assert previous is not None
            assert previous["current"] == 0
            cursor = await repo._db.execute(
                "SELECT COUNT(*) AS n FROM decision WHERE photo_id = ? AND current = 1",
                (photo_id,),
            )
            row = await cursor.fetchone()
            assert row is not None
            assert int(row["n"]) == 1

    run(body())


def test_add_decision_rejects_written_photo(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            await _proposal(repo, photo_id)
            await repo.add_decision(photo_id, DecisionAction.accept)
            await repo.set_photo_status(photo_id, PhotoStatus.written)
            with pytest.raises(DecisionError, match="written"):
                await repo.add_decision(photo_id, DecisionAction.skip)

    run(body())


def test_proposal_one_current_unique_index(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            first = await _proposal(repo, photo_id)
            second = await repo.insert_proposal(
                photo_id=photo_id,
                provider="xai",
                model="grok-4.6",
                prompt_version="PROMPT_V1",
                raw_json="{}",
                pin_source=PinSource.geocoder,
                pin_lat=1.0,
                pin_lng=2.0,
                current=True,
            )
            current = await repo.get_current_proposal(photo_id)
            assert current is not None
            assert current["id"] == second
            old = await repo.get_proposal(first)
            assert old is not None
            assert old["current"] == 0
            with pytest.raises(aiosqlite.IntegrityError):
                await repo._db.execute(
                    "UPDATE proposal SET current = 1 WHERE id = ?",
                    (first,),
                )

    run(body())


def test_write_receipt_upsert_includes_undone(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            photo_id = await _pending_photo(repo)
            await _proposal(repo, photo_id)
            decision_id = await repo.add_decision(photo_id, DecisionAction.accept)
            dest = "/tmp/copies/IMG_a.jpg"
            first = await repo.upsert_write_receipt(
                photo_id=photo_id,
                decision_id=decision_id,
                dest_path=dest,
                in_place=False,
                status=WriteReceiptStatus.pending,
            )
            second = await repo.upsert_write_receipt(
                photo_id=photo_id,
                decision_id=decision_id,
                dest_path=dest,
                in_place=False,
                status=WriteReceiptStatus.ok,
                written_sha256="b" * 64,
                written_at="2026-08-12T00:00:00Z",
            )
            undone = await repo.upsert_write_receipt(
                photo_id=photo_id,
                decision_id=decision_id,
                dest_path=dest,
                in_place=False,
                status="undone",
            )
            assert first == second == undone
            row = await repo.get_write_receipt(photo_id, dest)
            assert row is not None
            assert row["status"] == WriteReceiptStatus.undone.value
            cursor = await repo._db.execute(
                "SELECT COUNT(*) AS n FROM write_receipt WHERE photo_id = ?",
                (photo_id,),
            )
            count = await cursor.fetchone()
            assert count is not None
            assert int(count["n"]) == 1

    run(body())


def test_token_columns_and_cluster_round_trip(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_repo(data_dir)
        async with repo:
            cluster_id = await repo.insert_cluster(
                parent=".",
                first_ts="2019-06-14T18:02:00",
                n=2,
                undated=False,
            )
            photo_id = await repo.insert_photo(
                path="/photos/trip/IMG_x.jpg",
                relpath="IMG_x.jpg",
                sha256="c" * 64,
                size=2048,
                mtime_ns=2,
                status=PhotoStatus.proposed,
                cluster_id=cluster_id,
            )
            proposal_id = await _proposal(repo, photo_id)
            proposal = await repo.get_proposal(proposal_id)
            cluster = await repo.get_cluster(cluster_id)
            assert proposal is not None and cluster is not None
            assert proposal["input_tokens"] == 2100
            assert proposal["output_tokens"] == 520
            assert proposal["reasoning_tokens"] == 800
            assert cluster["n"] == 2
            assert cluster["undated"] == 0
            n = await repo.reset_stale_proposing()
            assert n == 0
            await repo.set_photo_status(photo_id, PhotoStatus.proposing)
            assert await repo.reset_stale_proposing() == 1
            photo = await repo.get_photo(photo_id)
            assert photo is not None
            assert photo["status"] == PhotoStatus.pending.value
            await repo.set_stats_json('{"proposed":1}')
            job = await repo.get_job()
            assert job is not None
            assert job["stats_json"] == '{"proposed":1}'
            assert job_db_path(data_dir, repo.job_id).is_file()

    run(body())


def test_index_schema_on_fresh_file(data_dir: Path) -> None:
    async def body() -> None:
        db = await open_index_db(index_db_path(data_dir))
        try:
            names = await _table_names(db)
            assert "job_index" in names
            cols = await _columns(db, "job_index")
            assert cols == {
                "job_id",
                "root_canonical",
                "options_hash",
                "created_at",
                "last_ingest_at",
            }
        finally:
            await db.close()

    run(body())
