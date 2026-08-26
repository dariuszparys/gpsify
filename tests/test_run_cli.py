"""``gpsify run`` and job-handle resolution (PR 11).

Handles cover the copy-paste the old workflow forced: every command that took a
UUID now also takes the folder or ``last``. The resolver never mutates the
index, so these tests only ever read it.
"""

from __future__ import annotations

import asyncio
import re
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar

import pytest
from typer.testing import CliRunner

from gpsify.cli import app
from gpsify.config import Settings
from gpsify.ingest import run_ingest
from gpsify.store import StoreError, options_hash, resolve_job_handle
from gpsify.store.index import JobIndex
from jpeg_factory import write_test_jpeg

T = TypeVar("T")
runner = CliRunner()
_ANSI_SGR = re.compile(r"\x1b\[[0-9;]*m")


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


def _plain(text: str) -> str:
    return _ANSI_SGR.sub("", text)


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


def _settings(data_dir: Path, **overrides: Any) -> Settings:
    base: dict[str, Any] = {
        "data_dir": data_dir,
        "nominatim_contact": "tester@example.com",
        "xai_api_key": "test-key",
    }
    base.update(overrides)
    return Settings(**base)


def _album(root: Path, n: int = 2) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    for i in range(n):
        write_test_jpeg(root / f"img{i}.jpg", datetime_original=f"2019:07:12 14:0{i}:00")
    return root


# --- handle resolution -----------------------------------------------------


def test_resolve_by_folder_path(data_dir: Path, tmp_path: Path) -> None:
    album = _album(tmp_path / "porto")
    summary = run(run_ingest(album, data_dir=data_dir))

    resolved = run(resolve_job_handle(data_dir, str(album)))
    assert resolved.job_id == summary.job_id
    assert resolved.note is None


def test_resolve_by_uuid_and_by_last(data_dir: Path, tmp_path: Path) -> None:
    first = run(run_ingest(_album(tmp_path / "porto"), data_dir=data_dir))
    second = run(run_ingest(_album(tmp_path / "lisbon"), data_dir=data_dir))

    assert run(resolve_job_handle(data_dir, first.job_id)).job_id == first.job_id
    # last_ingest_at DESC: the newest ingest wins, and the pick is announced.
    latest = run(resolve_job_handle(data_dir, "last"))
    assert latest.job_id == second.job_id
    assert second.job_id in (latest.note or "")


def test_resolve_uuid_without_index_row_still_opens(data_dir: Path, tmp_path: Path) -> None:
    """A job database on disk stays addressable if the index row is gone."""
    summary = run(run_ingest(_album(tmp_path / "porto"), data_dir=data_dir))

    async def _drop_row() -> None:
        async with await JobIndex.open(data_dir) as index:
            await index._db.execute("DELETE FROM job_index")  # noqa: SLF001
            await index._db.commit()  # noqa: SLF001

    run(_drop_row())
    assert run(resolve_job_handle(data_dir, summary.job_id)).job_id == summary.job_id


def test_resolve_unknown_handles_are_errors(data_dir: Path, tmp_path: Path) -> None:
    with pytest.raises(StoreError, match="no jobs yet"):
        run(resolve_job_handle(data_dir, "last"))
    with pytest.raises(StoreError, match="no job with id"):
        run(resolve_job_handle(data_dir, "22222222-2222-4222-8222-222222222222"))
    with pytest.raises(StoreError, match="not a job id"):
        run(resolve_job_handle(data_dir, str(tmp_path / "nope")))

    _album(tmp_path / "unseen")
    with pytest.raises(StoreError, match="no job for folder"):
        run(resolve_job_handle(data_dir, str(tmp_path / "unseen")))


def test_two_jobs_on_one_folder_pick_newest_and_say_so(
    data_dir: Path, tmp_path: Path
) -> None:
    """--recursive changes options_hash, so one folder can hold two jobs."""
    album = _album(tmp_path / "porto")
    _album(album / "day2")
    flat = run(run_ingest(album, data_dir=data_dir))
    deep = run(run_ingest(album, recursive=True, data_dir=data_dir))
    assert flat.job_id != deep.job_id

    resolved = run(resolve_job_handle(data_dir, str(album)))
    assert resolved.job_id == deep.job_id
    assert "2 jobs share this folder" in (resolved.note or "")
    assert "recursive" in (resolved.note or "")
    assert options_hash(recursive=True) != options_hash(recursive=False)


def test_status_accepts_a_folder_path(
    data_dir: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    album = _album(tmp_path / "porto")
    summary = run(run_ingest(album, data_dir=data_dir))
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: _settings(data_dir))

    result = runner.invoke(app, ["status", str(album)])
    assert result.exit_code == 0, result.output
    assert summary.job_id in _plain(result.stdout)


# --- the run command -------------------------------------------------------


def test_run_help_documents_the_one_command_flow() -> None:
    result = runner.invoke(app, ["run", "--help"])
    assert result.exit_code == 0
    stdout = _plain(result.stdout)
    for flag in ("--hint", "--recursive", "--limit", "--dry-run", "--no-review", "--port"):
        assert flag in stdout


def test_run_dry_run_spends_nothing_and_skips_review(
    data_dir: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    album = _album(tmp_path / "porto", n=3)
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: _settings(data_dir))

    def _never(*args: Any, **kwargs: Any) -> None:
        raise AssertionError("run --dry-run must not serve the review UI")

    monkeypatch.setattr("gpsify.cli._serve_review_ui", _never)

    result = runner.invoke(app, ["run", str(album), "--dry-run"])
    assert result.exit_code == 0, result.output
    stdout = _plain(result.stdout)
    assert "scanning    3 photos" in stdout
    assert "dry-run: nothing spent" in stdout
    # Ingest is not a side effect that costs money, so the job is real and resumable.
    assert run(resolve_job_handle(data_dir, str(album))).job_id


def test_run_no_review_stops_after_propose(
    data_dir: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    album = _album(tmp_path / "porto", n=2)
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: _settings(data_dir))
    monkeypatch.setattr(
        "gpsify.cli._serve_review_ui",
        lambda *a, **k: (_ for _ in ()).throw(AssertionError("--no-review served anyway")),
    )

    result = runner.invoke(app, ["run", str(album), "--dry-run", "--no-review"])
    assert result.exit_code == 0, result.output


def test_run_refuses_a_photos_library(
    data_dir: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """K14 still holds through the composite command."""
    bundle = tmp_path / "Photos.photoslibrary"
    _album(bundle)
    monkeypatch.setattr("gpsify.cli.load_settings", lambda: _settings(data_dir))

    result = runner.invoke(app, ["run", str(bundle)])
    assert result.exit_code == 2
    assert "photoslibrary" in f"{result.stdout}{result.stderr}"
