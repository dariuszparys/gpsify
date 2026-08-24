from __future__ import annotations

import asyncio
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar

import pytest
from PIL import Image
from typer.testing import CliRunner

from gpsify.apply import (
    DEST_DIVERGED,
    SOURCE_CHANGED,
    ApplyError,
    copies_root,
    run_apply,
)
from gpsify.cli import app
from gpsify.exif import (
    DRY_RUN_BLOCKS_WRITES,
    ExifWriteError,
    GpsWrite,
    MediaKind,
    dest_gps_matches,
    write_gps_batch,
)
from gpsify.media import hash_file
from gpsify.store import DecisionError, JobRepo, job_dir, options_hash
from gpsify.types import DecisionAction, PhotoStatus, PinSource, WriteReceiptStatus
from gpsify.undo import run_undo
from jpeg_factory import PORTO_LAT, PORTO_LNG, write_test_jpeg

T = TypeVar("T")
runner = CliRunner()
JOB_ID = "88888888-8888-4888-8888-888888888888"
PORTO = "Clérigos Church, Porto"


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


def write_test_png(path: Path, color: tuple[int, int, int] = (40, 120, 80)) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    Image.new("RGB", (8, 8), color).save(path, "PNG")
    return path


async def _create_repo(data_dir: Path, root: Path) -> JobRepo:
    return await JobRepo.create(
        data_dir,
        job_id=JOB_ID,
        root_path=str(root),
        root_canonical=str(root.resolve()),
        options_hash=options_hash(recursive=False),
        hint="weekend in Porto",
        config_json="{}",
    )


async def _add_photo(
    repo: JobRepo,
    path: Path,
    *,
    relpath: str | None = None,
    status: PhotoStatus = PhotoStatus.proposed,
) -> str:
    return await repo.insert_photo(
        path=str(path),
        relpath=relpath or path.name,
        sha256=hash_file(path),
        size=path.stat().st_size,
        mtime_ns=1,
        status=status,
        datetime_original="2019:06:14 18:04:00",
    )


async def _proposal(
    repo: JobRepo,
    photo_id: str,
    *,
    pin_source: PinSource = PinSource.geocoder,
    pin_lat: float | None = PORTO_LAT,
    pin_lng: float | None = PORTO_LNG,
) -> str:
    return await repo.insert_proposal(
        photo_id=photo_id,
        provider="xai",
        model="grok-4.6",
        prompt_version="PROMPT_V1",
        raw_json="{}",
        place_name=PORTO,
        granularity="exact",
        model_confidence=0.8,
        ui_rank=0.83,
        refusal="none",
        pin_lat=pin_lat,
        pin_lng=pin_lng,
        pin_source=pin_source,
    )


def _accepted_jpeg(data_dir: Path, tmp_path: Path) -> tuple[Path, str]:
    root = tmp_path / "photos"
    root.mkdir()
    jpeg = write_test_jpeg(root / "IMG_a.jpg", datetime_original="2019:06:14 18:04:00")

    async def body() -> str:
        repo = await _create_repo(data_dir, root)
        async with repo:
            photo_id = await _add_photo(repo, jpeg)
            await _proposal(repo, photo_id)
            await repo.add_decision(photo_id, DecisionAction.accept)
            return photo_id

    return jpeg, run(body())


@pytest.mark.writes_exif
def test_happy_copy_to_default_copies(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    ingest_hash = hash_file(jpeg)
    summary = run(run_apply(JOB_ID, data_dir=data_dir, i_mean_it=True))
    assert summary.wrote == 1
    assert summary.failed == 0
    dest = copies_root(data_dir, JOB_ID) / "IMG_a.jpg"
    assert dest.is_file()
    assert dest_gps_matches(dest, PORTO_LAT, PORTO_LNG)
    assert hash_file(jpeg) == ingest_hash
    assert hash_file(dest) != ingest_hash

    async def check() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            photo = await repo.get_photo(photo_id)
            receipt = await repo.get_write_receipt(photo_id, str(dest))
            assert photo is not None and receipt is not None
            assert photo["status"] == PhotoStatus.written.value
            assert photo["sha256"] == ingest_hash
            assert receipt["status"] == WriteReceiptStatus.ok.value
            assert receipt["written_sha256"] == hash_file(dest)
            assert receipt["in_place"] == 0

    run(check())


@pytest.mark.writes_exif
def test_copy_mode_source_sha256_mismatch_aborts(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    jpeg.write_bytes(jpeg.read_bytes() + b"mutated")
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.wrote == 0
    assert summary.failed == 1
    dest = copies_root(data_dir, JOB_ID) / "IMG_a.jpg"
    assert not dest.exists()

    async def check() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            photo = await repo.get_photo(photo_id)
            receipt = await repo.get_write_receipt(photo_id, str(dest))
            assert photo is not None and receipt is not None
            assert photo["status"] == PhotoStatus.write_failed.value
            assert receipt["error"] == SOURCE_CHANGED

    run(check())


@pytest.mark.writes_exif
def test_pending_retry_dest_missing_copies_and_writes(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    dest = copies_root(data_dir, JOB_ID) / "IMG_a.jpg"

    async def seed_pending() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            decision = await repo.get_current_decision(photo_id)
            assert decision is not None
            await repo.upsert_write_receipt(
                photo_id=photo_id,
                decision_id=str(decision["id"]),
                dest_path=str(dest),
                in_place=False,
                status=WriteReceiptStatus.pending,
            )

    run(seed_pending())
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.wrote == 1
    assert dest.is_file()
    assert dest_gps_matches(dest, PORTO_LAT, PORTO_LNG)


@pytest.mark.writes_exif
def test_pending_retry_dest_gps_already_matching(
    data_dir: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    dest = copies_root(data_dir, JOB_ID) / "IMG_a.jpg"
    dest.parent.mkdir(parents=True)
    dest.write_bytes(jpeg.read_bytes())
    write_gps_batch(
        [
            GpsWrite(
                dest=dest,
                lat=PORTO_LAT,
                lng=PORTO_LNG,
                kind=MediaKind.jpeg,
                job_id=JOB_ID,
                decision_id="seed",
            )
        ]
    )
    dest_hash = hash_file(dest)

    async def seed_pending() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            decision = await repo.get_current_decision(photo_id)
            assert decision is not None
            await repo.upsert_write_receipt(
                photo_id=photo_id,
                decision_id=str(decision["id"]),
                dest_path=str(dest),
                in_place=False,
                status=WriteReceiptStatus.pending,
            )

    run(seed_pending())
    calls: list[int] = []
    original = write_gps_batch

    def spy(items: Any) -> Any:
        calls.append(len(items))
        return original(items)

    monkeypatch.setattr("gpsify.apply.write_gps_batch", spy)
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.wrote == 1
    assert calls == []

    async def check() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            receipt = await repo.get_write_receipt(photo_id, str(dest))
            assert receipt is not None
            assert receipt["status"] == WriteReceiptStatus.ok.value
            assert receipt["written_sha256"] == dest_hash

    run(check())


@pytest.mark.writes_exif
def test_pending_retry_half_written_restores_backup(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    dest = copies_root(data_dir, JOB_ID) / "IMG_a.jpg"
    dest.parent.mkdir(parents=True)
    dest.write_bytes(jpeg.read_bytes())
    write_gps_batch(
        [
            GpsWrite(
                dest=dest,
                lat=40.0,
                lng=-8.0,
                kind=MediaKind.jpeg,
                job_id=JOB_ID,
                decision_id="wrong",
            )
        ]
    )
    backup = job_dir(data_dir, JOB_ID) / "backups" / f"{photo_id}.bin"
    backup.parent.mkdir(parents=True)
    backup.write_bytes(jpeg.read_bytes())

    async def seed_pending() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            decision = await repo.get_current_decision(photo_id)
            assert decision is not None
            await repo.upsert_write_receipt(
                photo_id=photo_id,
                decision_id=str(decision["id"]),
                dest_path=str(dest),
                in_place=False,
                status=WriteReceiptStatus.pending,
                backup_path=str(backup),
            )

    run(seed_pending())
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.wrote == 1
    assert dest_gps_matches(dest, PORTO_LAT, PORTO_LNG)


@pytest.mark.writes_exif
def test_in_place_pending_retry_gps_match_ignores_ingest_hash(
    data_dir: Path, tmp_path: Path
) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    ingest_hash = hash_file(jpeg)
    write_gps_batch(
        [
            GpsWrite(
                dest=jpeg,
                lat=PORTO_LAT,
                lng=PORTO_LNG,
                kind=MediaKind.jpeg,
                job_id=JOB_ID,
                decision_id="seed",
            )
        ]
    )
    assert hash_file(jpeg) != ingest_hash

    async def seed_pending() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            decision = await repo.get_current_decision(photo_id)
            assert decision is not None
            await repo.upsert_write_receipt(
                photo_id=photo_id,
                decision_id=str(decision["id"]),
                dest_path=str(jpeg),
                in_place=True,
                status=WriteReceiptStatus.pending,
            )

    run(seed_pending())
    summary = run(run_apply(JOB_ID, data_dir=data_dir, in_place=True, i_mean_it=True))
    assert summary.wrote == 1

    async def check() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            photo = await repo.get_photo(photo_id)
            receipt = await repo.get_write_receipt(photo_id, str(jpeg))
            assert photo is not None and receipt is not None
            assert photo["sha256"] == ingest_hash
            assert photo["status"] == PhotoStatus.written.value
            assert receipt["status"] == WriteReceiptStatus.ok.value
            assert receipt["error"] is None

    run(check())


@pytest.mark.writes_exif
def test_in_place_idempotent_reapply_of_ok_receipt(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    first = run(run_apply(JOB_ID, data_dir=data_dir, in_place=True, i_mean_it=True))
    assert first.wrote == 1
    tagged_hash = hash_file(jpeg)
    second = run(run_apply(JOB_ID, data_dir=data_dir, in_place=True, i_mean_it=True))
    assert second.wrote == 1
    assert hash_file(jpeg) == tagged_hash

    async def check() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            receipts = await repo.list_write_receipts()
            assert len(receipts) == 1
            assert receipts[0]["status"] == WriteReceiptStatus.ok.value

    run(check())


@pytest.mark.writes_exif
def test_skip_after_accept_is_not_written(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)

    async def skip() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            await repo.add_decision(photo_id, DecisionAction.skip)

    run(skip())
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.wrote == 0
    assert summary.eligible == 0
    assert not (copies_root(data_dir, JOB_ID) / "IMG_a.jpg").exists()


@pytest.mark.writes_exif
def test_png_xmp_only(data_dir: Path, tmp_path: Path) -> None:
    root = tmp_path / "photos"
    png = write_test_png(root / "IMG_p.png")

    async def body() -> str:
        repo = await _create_repo(data_dir, root)
        async with repo:
            photo_id = await _add_photo(repo, png)
            await _proposal(repo, photo_id)
            await repo.add_decision(photo_id, DecisionAction.accept)
            return photo_id

    run(body())
    ingest = hash_file(png)
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.wrote == 1
    assert "png_xmp_only" in summary.warnings
    dest = copies_root(data_dir, JOB_ID) / "IMG_p.png"
    assert dest_gps_matches(dest, PORTO_LAT, PORTO_LNG)
    assert hash_file(png) == ingest


@pytest.mark.writes_exif
def test_raw_copy_writes_sidecar_does_not_change_raw_hash(data_dir: Path, tmp_path: Path) -> None:
    root = tmp_path / "photos"
    raw = root / "IMG_r.CR2"
    raw.parent.mkdir(parents=True)
    raw.write_bytes(b"not-a-real-raw-payload")
    ingest_hash = hash_file(raw)

    async def body() -> str:
        repo = await _create_repo(data_dir, root)
        async with repo:
            photo_id = await _add_photo(repo, raw)
            await _proposal(repo, photo_id)
            await repo.add_decision(photo_id, DecisionAction.accept)
            return photo_id

    photo_id = run(body())
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.wrote == 1
    copied = copies_root(data_dir, JOB_ID) / "IMG_r.CR2"
    sidecar = copied.with_suffix(".xmp")
    assert copied.is_file()
    assert sidecar.is_file()
    assert hash_file(raw) == ingest_hash
    assert hash_file(copied) == ingest_hash
    assert dest_gps_matches(sidecar, PORTO_LAT, PORTO_LNG)

    async def check() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            receipt = await repo.get_write_receipt(photo_id, str(sidecar))
            photo = await repo.get_photo(photo_id)
            assert receipt is not None and photo is not None
            assert receipt["dest_path"] == str(sidecar)
            assert photo["sha256"] == ingest_hash

    run(check())


@pytest.mark.writes_exif
def test_raw_in_place_writes_only_sidecar(data_dir: Path, tmp_path: Path) -> None:
    root = tmp_path / "photos"
    raw = root / "IMG_r.NEF"
    raw.parent.mkdir(parents=True)
    raw.write_bytes(b"raw-bytes")
    ingest_hash = hash_file(raw)

    async def body() -> str:
        repo = await _create_repo(data_dir, root)
        async with repo:
            photo_id = await _add_photo(repo, raw)
            await _proposal(repo, photo_id)
            await repo.add_decision(photo_id, DecisionAction.accept)
            return photo_id

    run(body())
    summary = run(run_apply(JOB_ID, data_dir=data_dir, in_place=True, i_mean_it=True))
    assert summary.wrote == 1
    sidecar = raw.with_suffix(".xmp")
    assert sidecar.is_file()
    assert hash_file(raw) == ingest_hash
    assert dest_gps_matches(sidecar, PORTO_LAT, PORTO_LNG)


@pytest.mark.writes_exif
def test_raw_undo_deletes_created_sidecar_and_restores_previous(
    data_dir: Path, tmp_path: Path
) -> None:
    root = tmp_path / "photos"
    raw = root / "IMG_r.ARW"
    raw.parent.mkdir(parents=True)
    raw.write_bytes(b"raw-bytes")
    existing = raw.with_suffix(".xmp")
    existing.write_text(
        """<?xml version="1.0"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/"
      exif:GPSLatitude="1.0"
      exif:GPSLongitude="2.0"
      xml:other="keep-me"/>
  </rdf:RDF>
</x:xmpmeta>
""",
        encoding="utf-8",
    )

    async def body() -> str:
        repo = await _create_repo(data_dir, root)
        async with repo:
            photo_id = await _add_photo(repo, raw)
            await _proposal(repo, photo_id)
            await repo.add_decision(photo_id, DecisionAction.accept)
            return photo_id

    run(body())
    summary = run(run_apply(JOB_ID, data_dir=data_dir, in_place=True, i_mean_it=True))
    assert summary.wrote == 1
    assert dest_gps_matches(existing, PORTO_LAT, PORTO_LNG)
    undone = run(run_undo(JOB_ID, data_dir=data_dir))
    assert undone.undone == 1
    assert dest_gps_matches(existing, 1.0, 2.0)

    created = root / "IMG_new.CR2"
    created.write_bytes(b"other-raw")

    async def second() -> str:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            pid = await _add_photo(repo, created)
            await _proposal(repo, pid)
            await repo.add_decision(pid, DecisionAction.accept)
            return pid

    run(second())
    run(run_apply(JOB_ID, data_dir=data_dir, in_place=True, i_mean_it=True))
    sidecar = created.with_suffix(".xmp")
    assert sidecar.is_file()
    run(run_undo(JOB_ID, data_dir=data_dir, photo=created))
    assert not sidecar.exists()


@pytest.mark.writes_exif
def test_raw_skipped_no_preview_is_not_written(data_dir: Path, tmp_path: Path) -> None:
    root = tmp_path / "photos"
    raw = root / "IMG_noprev.CR2"
    raw.parent.mkdir(parents=True)
    raw.write_bytes(b"raw")

    async def body() -> None:
        repo = await _create_repo(data_dir, root)
        async with repo:
            await _add_photo(repo, raw, status=PhotoStatus.skipped_no_preview)

    run(body())
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.eligible == 0
    assert summary.wrote == 0
    assert not (copies_root(data_dir, JOB_ID) / "IMG_noprev.xmp").exists()


@pytest.mark.writes_exif
def test_undo_sets_undone_then_reapply_upserts(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    dest = copies_root(data_dir, JOB_ID) / "IMG_a.jpg"
    run(run_apply(JOB_ID, data_dir=data_dir))
    assert dest.is_file()
    undone = run(run_undo(JOB_ID, data_dir=data_dir))
    assert undone.undone == 1
    assert not dest.exists()

    async def check_undone() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            photo = await repo.get_photo(photo_id)
            receipt = await repo.get_write_receipt(photo_id, str(dest))
            assert photo is not None and receipt is not None
            assert photo["status"] == PhotoStatus.accepted.value
            assert receipt["status"] == WriteReceiptStatus.undone.value
            rows = await repo.list_write_receipts()
            assert len(rows) == 1

    run(check_undone())
    again = run(run_apply(JOB_ID, data_dir=data_dir))
    assert again.wrote == 1
    assert dest.is_file()

    async def check_ok() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            receipt = await repo.get_write_receipt(photo_id, str(dest))
            photo = await repo.get_photo(photo_id)
            assert receipt is not None and photo is not None
            assert receipt["status"] == WriteReceiptStatus.ok.value
            assert photo["status"] == PhotoStatus.written.value
            assert len(await repo.list_write_receipts()) == 1

    run(check_ok())


def test_add_decision_accept_copies_geocoder_pin_and_rejects_none(
    data_dir: Path, tmp_path: Path
) -> None:
    root = tmp_path / "photos"
    jpeg = write_test_jpeg(root / "IMG_a.jpg")

    async def body() -> None:
        repo = await _create_repo(data_dir, root)
        async with repo:
            photo_id = await _add_photo(repo, jpeg)
            await _proposal(repo, photo_id, pin_source=PinSource.geocoder)
            decision_id = await repo.add_decision(photo_id, "accept")
            decision = await repo.get_decision(decision_id)
            assert decision is not None
            assert decision["lat"] == pytest.approx(PORTO_LAT)
            assert decision["lng"] == pytest.approx(PORTO_LNG)
            other = await _add_photo(repo, write_test_jpeg(root / "IMG_b.jpg"))
            await _proposal(repo, other, pin_source=PinSource.none, pin_lat=None, pin_lng=None)
            with pytest.raises(DecisionError, match="pin_source"):
                await repo.add_decision(other, DecisionAction.accept)

    run(body())


def test_apply_dry_run_prints_argfile_and_writes_nothing(
    data_dir: Path, tmp_path: Path
) -> None:
    jpeg, _photo_id = _accepted_jpeg(data_dir, tmp_path)
    ingest_hash = hash_file(jpeg)
    summary = run(run_apply(JOB_ID, data_dir=data_dir, dry_run=True))
    assert summary.dry_run is True
    assert "GPSLatitude" in summary.argfile
    assert str(copies_root(data_dir, JOB_ID) / "IMG_a.jpg") in summary.argfile
    assert not (copies_root(data_dir, JOB_ID) / "IMG_a.jpg").exists()
    assert hash_file(jpeg) == ingest_hash


def test_gpsify_dry_run_blocks_writes(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("GPSIFY_DRY_RUN", "1")
    jpeg = write_test_jpeg(tmp_path / "a.jpg")
    with pytest.raises(ExifWriteError, match="GPSIFY_DRY_RUN"):
        write_gps_batch(
            [
                GpsWrite(
                    dest=jpeg,
                    lat=PORTO_LAT,
                    lng=PORTO_LNG,
                    kind=MediaKind.jpeg,
                    job_id="j",
                    decision_id="d",
                )
            ]
        )
    assert DRY_RUN_BLOCKS_WRITES


def test_apply_requires_i_mean_it_for_in_place(data_dir: Path, tmp_path: Path) -> None:
    _accepted_jpeg(data_dir, tmp_path)
    with pytest.raises(ApplyError, match="i-mean-it"):
        run(run_apply(JOB_ID, data_dir=data_dir, in_place=True, i_mean_it=False))


def test_apply_nothing_accepted_exit_3(data_dir: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root = tmp_path / "photos"
    jpeg = write_test_jpeg(root / "IMG_a.jpg")

    async def body() -> None:
        repo = await _create_repo(data_dir, root)
        async with repo:
            await _add_photo(repo, jpeg)

    run(body())
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["apply", JOB_ID])
    assert result.exit_code == 3


def test_cli_apply_dry_run(data_dir: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _accepted_jpeg(data_dir, tmp_path)
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["apply", JOB_ID, "--dry-run"])
    assert result.exit_code == 0
    assert "GPSLatitude" in result.stdout
    assert not (copies_root(data_dir, JOB_ID) / "IMG_a.jpg").exists()


@pytest.mark.writes_exif
def test_apply_out_dir(data_dir: Path, tmp_path: Path) -> None:
    jpeg, _photo_id = _accepted_jpeg(data_dir, tmp_path)
    out = tmp_path / "tagged"
    summary = run(run_apply(JOB_ID, data_dir=data_dir, out=out))
    assert summary.wrote == 1
    dest = out / "IMG_a.jpg"
    assert dest.is_file()
    assert dest_gps_matches(dest, PORTO_LAT, PORTO_LNG)
    assert hash_file(jpeg) != hash_file(dest)


@pytest.mark.writes_exif
def test_copy_mode_dest_diverged_when_copy_edited(data_dir: Path, tmp_path: Path) -> None:
    jpeg, photo_id = _accepted_jpeg(data_dir, tmp_path)
    dest = copies_root(data_dir, JOB_ID) / "IMG_a.jpg"
    dest.parent.mkdir(parents=True)
    dest.write_bytes(jpeg.read_bytes() + b"user-edit")
    summary = run(run_apply(JOB_ID, data_dir=data_dir))
    assert summary.failed == 1

    async def check() -> None:
        async with await JobRepo.open(data_dir, JOB_ID) as repo:
            receipt = await repo.get_write_receipt(photo_id, str(dest))
            assert receipt is not None
            assert receipt["error"] == DEST_DIVERGED

    run(check())
