from __future__ import annotations

import asyncio
import os
import re
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar

import pytest
from typer.testing import CliRunner

from gpsify.cli import app
from gpsify.cluster import CLUSTER_GAP, parse_capture_time
from gpsify.ingest import IngestError, is_photos_library, run_ingest
from gpsify.store import JobIndex, JobRepo, options_hash
from gpsify.types import PhotoStatus, PinSource
from jpeg_factory import PORTO_LAT, PORTO_LNG, write_test_jpeg

T = TypeVar("T")
runner = CliRunner()
_ANSI_SGR = re.compile(r"\x1b\[[0-9;]*m")
FIXTURES = Path(__file__).parent / "fixtures"


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


def _plain(text: str) -> str:
    return _ANSI_SGR.sub("", text)


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


def _copy_fixture(name: str, dest: Path) -> Path:
    dest.write_bytes((FIXTURES / name).read_bytes())
    return dest


def test_photos_library_detection(tmp_path: Path) -> None:
    bundle = tmp_path / "Travel.photoslibrary"
    originals = bundle / "originals"
    originals.mkdir(parents=True)
    (originals / "IMG_1.jpg").write_bytes(b"x")
    assert is_photos_library(bundle) is True
    assert is_photos_library(originals) is True
    assert is_photos_library(tmp_path) is False


def test_ingest_refuses_photos_library(tmp_path: Path, data_dir: Path) -> None:
    bundle = tmp_path / "Album.photoslibrary"
    bundle.mkdir()
    with pytest.raises(IngestError, match="Photos.app"):
        run(run_ingest(bundle, data_dir=data_dir))


def test_ingest_classifies_gps_and_plain_jpegs(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "trip"
    folder.mkdir()
    _copy_fixture("jpeg_with_gps.jpg", folder / "tagged.jpg")
    _copy_fixture("jpeg_no_gps.jpg", folder / "plain.jpg")
    (folder / "notes.txt").write_text("ignore me", encoding="utf-8")
    (folder / "clip.mov").write_bytes(b"ftyp")
    (folder / "._AppleDouble.jpg").write_bytes(b"x")
    (folder / ".DS_Store").write_bytes(b"x")
    (folder / "Thumbs.db").write_bytes(b"x")
    hidden = folder / ".gpsify" / "cache.jpg"
    hidden.parent.mkdir()
    write_test_jpeg(hidden)

    summary = run(run_ingest(folder, hint="weekend in Porto", data_dir=data_dir))
    assert summary.total == 4
    assert summary.other == ()
    assert summary.tagged == 1
    assert summary.untagged == 1
    assert summary.skipped == 2  # txt + mov
    assert summary.missing == 0
    assert summary.clusters >= 1

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, summary.job_id) as repo:
            photos = {row["relpath"]: row for row in await repo.list_photos()}
            assert set(photos) == {"tagged.jpg", "plain.jpg", "notes.txt", "clip.mov"}
            assert photos["tagged.jpg"]["status"] == PhotoStatus.skipped_has_gps.value
            assert photos["tagged.jpg"]["has_gps"] == 1
            assert photos["tagged.jpg"]["existing_lat"] == pytest.approx(PORTO_LAT, abs=1e-4)
            assert photos["tagged.jpg"]["existing_lng"] == pytest.approx(PORTO_LNG, abs=1e-4)
            assert photos["plain.jpg"]["status"] == PhotoStatus.pending.value
            assert photos["notes.txt"]["status"] == PhotoStatus.skipped_unsupported.value
            job = await repo.get_job()
            assert job is not None
            assert job["hint"] == "weekend in Porto"

    run(inspect())


def test_ingest_resumes_same_key_and_hint_does_not_fork(
    tmp_path: Path, data_dir: Path
) -> None:
    folder = tmp_path / "album"
    folder.mkdir()
    write_test_jpeg(folder / "a.jpg", datetime_original="2019:06:14 18:00:00")
    first = run(run_ingest(folder, hint="first", data_dir=data_dir))
    second = run(run_ingest(folder, hint="second", data_dir=data_dir))
    assert first.job_id == second.job_id
    recursive = run(run_ingest(folder, recursive=True, data_dir=data_dir))
    assert recursive.job_id != first.job_id

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, first.job_id) as repo:
            job = await repo.get_job()
            assert job is not None
            assert job["hint"] == "second"
            assert job["options_hash"] == options_hash(recursive=False)

    run(inspect())


def test_reingest_reports_statuses_past_pending(
    tmp_path: Path, data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    folder = tmp_path / "album"
    folder.mkdir()
    write_test_jpeg(folder / "a.jpg", datetime_original="2019:06:14 18:00:00")
    write_test_jpeg(folder / "b.jpg", datetime_original="2019:06:14 18:02:00")
    first = run(run_ingest(folder, data_dir=data_dir))
    assert first.total == 2
    assert first.untagged == 2
    assert first.other == ()

    async def advance() -> None:
        async with await JobRepo.open(data_dir, first.job_id) as repo:
            photos = sorted(await repo.list_photos(), key=lambda row: row["relpath"])
            await repo.set_photo_status(photos[0]["id"], PhotoStatus.proposed)
            await repo.set_photo_status(photos[1]["id"], PhotoStatus.refused)

    run(advance())

    second = run(run_ingest(folder, data_dir=data_dir))
    assert second.job_id == first.job_id
    assert second.total == 2
    assert second.untagged == 0
    assert second.tagged == 0
    assert second.skipped == 0
    assert second.missing == 0
    assert second.other == (("proposed", 1), ("refused", 1))

    result = runner.invoke(app, ["ingest", str(folder)])
    assert result.exit_code == 0, result.output
    stdout = _plain(result.stdout)
    assert "total: 2" in stdout
    assert "untagged: 0" in stdout
    assert "other: proposed=1  refused=1" in stdout


def test_reingest_marks_missing_and_sha256_reset(
    tmp_path: Path, data_dir: Path
) -> None:
    folder = tmp_path / "album"
    folder.mkdir()
    kept = write_test_jpeg(folder / "kept.jpg", datetime_original="2019:06:14 18:00:00")
    gone = write_test_jpeg(folder / "gone.jpg", datetime_original="2019:06:14 18:01:00")
    changed = write_test_jpeg(
        folder / "changed.jpg", datetime_original="2019:06:14 18:02:00"
    )
    first = run(run_ingest(folder, data_dir=data_dir))

    async def seed_proposal() -> str:
        async with await JobRepo.open(data_dir, first.job_id) as repo:
            photos = {row["relpath"]: row for row in await repo.list_photos()}
            photo_id = str(photos["changed.jpg"]["id"])
            await repo.insert_proposal(
                photo_id=photo_id,
                provider="xai",
                model="grok-4.6",
                prompt_version="PROMPT_V1",
                raw_json="{}",
                pin_source=PinSource.geocoder,
                pin_lat=41.0,
                pin_lng=-8.0,
                place_name="Porto",
            )
            await repo.add_decision(photo_id, "accept")
            return photo_id

    photo_id = run(seed_proposal())
    gone.unlink()
    write_test_jpeg(
        changed,
        datetime_original="2019:06:14 20:00:00",
        color=(9, 9, 9),
    )
    os.utime(kept, ns=(kept.stat().st_atime_ns, kept.stat().st_mtime_ns + 1_000_000))
    second = run(run_ingest(folder, data_dir=data_dir))
    assert second.job_id == first.job_id
    assert second.missing == 1

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, first.job_id) as repo:
            photos = {row["relpath"]: row for row in await repo.list_photos()}
            assert photos["gone.jpg"]["status"] == PhotoStatus.missing.value
            changed_row = photos["changed.jpg"]
            assert changed_row["id"] == photo_id
            assert changed_row["status"] == PhotoStatus.pending.value
            current_proposal = await repo.get_current_proposal(photo_id)
            current_decision = await repo.get_current_decision(photo_id)
            assert current_proposal is None
            assert current_decision is None
            # History rows remain.
            cursor = await repo._db.execute(
                "SELECT COUNT(*) AS n FROM proposal WHERE photo_id = ?",
                (photo_id,),
            )
            row = await cursor.fetchone()
            assert row is not None
            assert int(row["n"]) == 1
            kept_row = photos["kept.jpg"]
            assert kept_row["status"] == PhotoStatus.pending.value

    run(inspect())


def test_relocate_matches_by_sha256(tmp_path: Path, data_dir: Path) -> None:
    original = tmp_path / "old"
    original.mkdir()
    write_test_jpeg(original / "shot.jpg", datetime_original="2019:06:14 18:00:00")
    first = run(run_ingest(original, hint="trip", data_dir=data_dir))
    moved = tmp_path / "new-place"
    moved.mkdir()
    (moved / "shot.jpg").write_bytes((original / "shot.jpg").read_bytes())
    relocated = run(
        run_ingest(moved, relocate=first.job_id, data_dir=data_dir)
    )
    assert relocated.job_id == first.job_id
    assert relocated.untagged == 1
    assert relocated.missing == 0

    async def inspect() -> None:
        async with await JobIndex.open(data_dir) as index:
            row = await index.get(first.job_id)
            assert row is not None
            assert row["root_canonical"] == str(moved.resolve())
        async with await JobRepo.open(data_dir, first.job_id) as repo:
            photos = await repo.list_photos()
            assert len(photos) == 1
            assert photos[0]["path"] == str((moved / "shot.jpg").resolve())
            assert photos[0]["relpath"] == "shot.jpg"
            job = await repo.get_job()
            assert job is not None
            assert job["root_canonical"] == str(moved.resolve())

    run(inspect())


def test_recursive_walk_and_undated_vs_dated_clusters(
    tmp_path: Path, data_dir: Path
) -> None:
    root = tmp_path / "tree"
    day1 = root / "day1"
    day1.mkdir(parents=True)
    write_test_jpeg(day1 / "a.jpg", datetime_original="2019:06:14 18:00:00")
    write_test_jpeg(day1 / "b.jpg", datetime_original="2019:06:14 18:10:00")
    gap_over = parse_capture_time("2019:06:14 18:00:00")
    assert gap_over is not None
    later = (gap_over + CLUSTER_GAP).strftime("%Y:%m:%d %H:%M:%S")
    # exactly 45 minutes stays in the same cluster; ingest uses capture time, not mtime
    write_test_jpeg(day1 / "c.jpg", datetime_original=later)
    write_test_jpeg(day1 / "undated.jpg")
    write_test_jpeg(root / "top.jpg", datetime_original="2019:06:14 12:00:00")
    nested = run(run_ingest(root, recursive=True, data_dir=data_dir))
    shallow = run(run_ingest(root, recursive=False, data_dir=data_dir))
    assert nested.job_id != shallow.job_id
    assert shallow.untagged == 1  # only top.jpg

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, nested.job_id) as repo:
            clusters = await repo.list_clusters()
            photos = await repo.list_photos()
            assert len(photos) == 5
            undated = [row for row in clusters if int(row["undated"]) == 1]
            dated = [row for row in clusters if int(row["undated"]) == 0]
            assert len(undated) == 1
            assert undated[0]["parent"] == "day1"
            # a+b+c same dated cluster (45 min inclusive); top.jpg is another
            assert len(dated) == 2

    run(inspect())


def test_png_heic_raw_classification(tmp_path: Path, data_dir: Path) -> None:
    folder = tmp_path / "mix"
    folder.mkdir()
    (folder / "pic.png").write_bytes(b"not-a-real-png")
    (folder / "live.heic").write_bytes(b"ftypheic")
    (folder / "body.cr2").write_bytes(b"raw-bytes")
    # A well-formed XMP packet, matching what gpsify's own sidecar writer emits.
    # Bare <exif:GPS*> fragments read on the Pillow/Piexif fallback but are not
    # valid XMP, so exiftool rejects them and the result flips on exiftool.
    (folder / "body.xmp").write_text(
        '<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>\n'
        '<x:xmpmeta xmlns:x="adobe:ns:meta/">\n'
        ' <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">\n'
        '  <rdf:Description rdf:about=""\n'
        '   xmlns:exif="http://ns.adobe.com/exif/1.0/"\n'
        '   exif:GPSLatitude="41.1456"\n'
        '   exif:GPSLongitude="-8.6143"/>\n'
        ' </rdf:RDF>\n'
        '</x:xmpmeta>\n'
        '<?xpacket end="w"?>\n',
        encoding="utf-8",
    )
    summary = run(run_ingest(folder, data_dir=data_dir))
    assert summary.tagged == 1  # RAW + sidecar GPS
    assert summary.untagged == 2  # png + heic stubs carry no GPS either way

    async def inspect() -> None:
        async with await JobRepo.open(data_dir, summary.job_id) as repo:
            photos = {row["relpath"]: row for row in await repo.list_photos()}
            assert "body.xmp" not in photos
            assert photos["body.cr2"]["status"] == PhotoStatus.skipped_has_gps.value
            assert photos["pic.png"]["status"] == PhotoStatus.pending.value
            assert photos["live.heic"]["status"] == PhotoStatus.pending.value

    run(inspect())


def test_cli_ingest_prints_job_id(
    tmp_path: Path, data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    folder = tmp_path / "cli-album"
    folder.mkdir()
    write_test_jpeg(folder / "a.jpg", datetime_original="2019:06:14 18:00:00")
    result = runner.invoke(app, ["ingest", str(folder), "--hint", "Porto"])
    assert result.exit_code == 0, result.output
    stdout = _plain(result.stdout)
    lines = [line for line in stdout.splitlines() if line.strip()]
    job_id = lines[0].strip()
    assert len(job_id) == 36
    assert "total: 1" in stdout
    assert "tagged:" in stdout
    assert "untagged: 1" in stdout
    assert "other:" not in stdout
    assert "clusters:" in stdout


def test_cli_ingest_photos_library_exits_2(
    tmp_path: Path, data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    bundle = tmp_path / "X.photoslibrary"
    bundle.mkdir()
    result = runner.invoke(app, ["ingest", str(bundle)])
    assert result.exit_code == 2
    combined = _plain(f"{result.stdout}{result.stderr}")
    assert "photoslibrary" in combined.lower() or "Photos.app" in combined
