from __future__ import annotations

import asyncio
import json
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar

import pytest
from typer.testing import CliRunner

from gpsify.apply import PNG_XMP_ONLY_WARNING, copies_root
from gpsify.cli import app
from gpsify.status import StatusError, job_status, run_clean
from gpsify.store import JobIndex, JobRepo, job_dir, options_hash
from gpsify.types import PhotoStatus

T = TypeVar("T")
runner = CliRunner()
JOB_ID = "99999999-9999-4999-8999-999999999999"
JOB_B = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


async def _create_job(
    data_dir: Path,
    job_id: str,
    *,
    root: str = "/photos/trip",
    hint: str | None = "weekend in Porto",
) -> JobRepo:
    async with await JobIndex.open(data_dir) as index:
        await index.register(job_id, root, options_hash(recursive=False))
    return await JobRepo.create(
        data_dir,
        job_id=job_id,
        root_path=root,
        root_canonical=root,
        options_hash=options_hash(recursive=False),
        hint=hint,
        config_json="{}",
    )


async def _add_photo(
    repo: JobRepo,
    *,
    relpath: str,
    status: PhotoStatus,
) -> str:
    return await repo.insert_photo(
        path=f"/photos/trip/{relpath}",
        relpath=relpath,
        sha256="a" * 64,
        size=1024,
        mtime_ns=1,
        status=status,
    )


def _seed_job(data_dir: Path) -> None:
    async def body() -> None:
        repo = await _create_job(data_dir, JOB_ID)
        async with repo:
            await _add_photo(repo, relpath="a.jpg", status=PhotoStatus.proposed)
            await _add_photo(repo, relpath="b.jpg", status=PhotoStatus.accepted)
            await _add_photo(repo, relpath="c.png", status=PhotoStatus.pending)
            await repo.set_stats_json(
                json.dumps(
                    {
                        "propose": {
                            "counts": {"proposed": 1, "refused": 0, "error": 0},
                            "est_usd": 0.009,
                            "input_tokens": 2100,
                            "output_tokens": 520,
                            "reasoning_tokens": 800,
                        }
                    }
                )
            )

    run(body())


def test_status_json_reads_stats_and_live_counts(data_dir: Path) -> None:
    _seed_job(data_dir)
    rollup = run(job_status(data_dir, JOB_ID))
    payload = rollup.to_json()
    assert payload["job_id"] == JOB_ID
    assert payload["dest_root"] == str(copies_root(data_dir, JOB_ID))
    assert payload["counts"][PhotoStatus.proposed.value] == 1
    assert payload["counts"][PhotoStatus.accepted.value] == 1
    assert payload["counts"][PhotoStatus.pending.value] == 1
    assert payload["counts"][PhotoStatus.written.value] == 0
    assert payload["stats"]["propose"]["est_usd"] == 0.009
    assert payload["stats"]["propose"]["reasoning_tokens"] == 800
    assert PNG_XMP_ONLY_WARNING in payload["warnings"]


def test_cli_status_json(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _seed_job(data_dir)
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["status", JOB_ID, "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["dest_root"] == str(copies_root(data_dir, JOB_ID))
    assert payload["counts"]["proposed"] == 1
    assert payload["stats"]["propose"]["reasoning_tokens"] == 800
    combined = f"{result.stdout}{result.stderr}".lower()
    assert "png_xmp_only" in combined


def test_cli_status_human_prints_dest_and_cost(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _seed_job(data_dir)
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["status", JOB_ID])
    assert result.exit_code == 0, result.output
    stdout = result.stdout
    assert JOB_ID in stdout
    assert str(copies_root(data_dir, JOB_ID)) in stdout
    assert "est_usd: 0.009" in stdout
    assert "reasoning_tokens: 800" in stdout
    assert "proposed=1" in stdout


def test_cli_status_lists_jobs(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _seed_job(data_dir)

    async def second() -> None:
        repo = await _create_job(data_dir, JOB_B, root="/photos/other")
        await repo.close()

    run(second())
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["status", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    ids = {row["job_id"] for row in payload["jobs"]}
    assert ids == {JOB_ID, JOB_B}
    dests = {row["job_id"]: row["dest_root"] for row in payload["jobs"]}
    assert dests[JOB_ID] == str(copies_root(data_dir, JOB_ID))


def test_cli_status_empty_index(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["status"])
    assert result.exit_code == 0, result.output
    assert "no jobs" in result.stdout


def test_status_unknown_job_exits_2(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["status", "missing-job"])
    assert result.exit_code == 2
    combined = f"{result.stdout}{result.stderr}".lower()
    assert "not found" in combined or "missing" in combined
    with pytest.raises(StatusError):
        run(job_status(data_dir, "missing-job"))


def test_clean_deletes_previews_and_thumbs_keeps_sqlite_backups(
    data_dir: Path,
) -> None:
    _seed_job(data_dir)
    root = job_dir(data_dir, JOB_ID)
    previews = root / "previews"
    thumbs = root / "thumbs"
    backups = root / "backups"
    copies = root / "copies"
    logs = root / "logs"
    previews.mkdir()
    thumbs.mkdir()
    backups.mkdir()
    copies.mkdir()
    logs.mkdir()
    (previews / "photo.jpg").write_bytes(b"preview")
    (thumbs / "photo.jpg").write_bytes(b"thumb")
    (backups / "photo.bin").write_bytes(b"backup")
    (copies / "photo.jpg").write_bytes(b"copy")
    (logs / "propose.log").write_text("log\n", encoding="utf-8")
    sqlite = root / "job.sqlite"
    sqlite_bytes = sqlite.read_bytes()

    summary = run(run_clean(data_dir, JOB_ID))
    assert summary.deleted_previews is True
    assert summary.deleted_thumbs is True
    assert summary.kept_sqlite is True
    assert summary.kept_backups is True
    assert not previews.exists()
    assert not thumbs.exists()
    assert sqlite.is_file()
    assert sqlite.read_bytes() == sqlite_bytes
    assert (backups / "photo.bin").read_bytes() == b"backup"
    assert (copies / "photo.jpg").read_bytes() == b"copy"
    assert (logs / "propose.log").read_text(encoding="utf-8") == "log\n"


def test_cli_clean_idempotent(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _seed_job(data_dir)
    root = job_dir(data_dir, JOB_ID)
    (root / "previews").mkdir()
    (root / "previews" / "a.jpg").write_bytes(b"x")
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    first = runner.invoke(app, ["clean", JOB_ID])
    assert first.exit_code == 0, first.output
    assert "previews" in first.stdout
    assert "kept sqlite and backups" in first.stdout
    assert not (root / "previews").exists()
    second = runner.invoke(app, ["clean", JOB_ID])
    assert second.exit_code == 0, second.output
    assert "nothing to clean" in second.stdout
    assert (root / "job.sqlite").is_file()


def test_cli_clean_unknown_job(
    data_dir: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(data_dir))
    result = runner.invoke(app, ["clean", "missing-job"])
    assert result.exit_code == 2
    assert "not implemented" not in (result.stdout + result.stderr).lower()
