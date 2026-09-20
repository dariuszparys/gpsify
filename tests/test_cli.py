from __future__ import annotations

import re

from typer.testing import CliRunner

from gpsify import __version__
from gpsify.cli import app

runner = CliRunner()

# Rich 15 on GitHub Actions styles "--hint" as "\x1b[1m-\x1b[0m\x1b[1m-hint\x1b[0m".
_ANSI_SGR = re.compile(r"\x1b\[[0-9;]*m")


def _plain(text: str) -> str:
    return _ANSI_SGR.sub("", text)


COMMANDS = (
    "ingest",
    "propose",
    "review",
    "decide",
    "apply",
    "undo",
    "skip",
    "status",
    "clean",
)


def test_root_help_lists_subcommands() -> None:
    result = runner.invoke(app, ["--help"])
    assert result.exit_code == 0
    stdout = _plain(result.stdout)
    for name in COMMANDS:
        assert name in stdout


def test_version() -> None:
    result = runner.invoke(app, ["--version"])
    assert result.exit_code == 0
    assert __version__ in result.stdout


def test_ingest_help_documents_flags() -> None:
    result = runner.invoke(app, ["ingest", "--help"])
    assert result.exit_code == 0
    stdout = _plain(result.stdout)
    assert "--hint" in stdout
    assert "--recursive" in stdout
    assert "--relocate" in stdout


def test_status_help_documents_json() -> None:
    result = runner.invoke(app, ["status", "--help"])
    assert result.exit_code == 0
    stdout = _plain(result.stdout)
    assert "--json" in stdout


def test_clean_help() -> None:
    result = runner.invoke(app, ["clean", "--help"])
    assert result.exit_code == 0
    stdout = _plain(result.stdout)
    assert "previews" in stdout.lower() or "thumbs" in stdout.lower()


def test_apply_rejects_out_and_in_place_together() -> None:
    result = runner.invoke(app, ["apply", "job-id", "--out", "/tmp/out", "--in-place"])
    assert result.exit_code == 2
    combined = _plain(f"{result.stdout}{result.stderr}")
    assert "--out" in combined and "--in-place" in combined
    assert "not implemented" not in combined.lower()


def test_apply_help_documents_write_flags() -> None:
    result = runner.invoke(app, ["apply", "--help"])
    assert result.exit_code == 0
    stdout = _plain(result.stdout)
    assert "--out" in stdout
    assert "--in-place" in stdout
    assert "--dry-run" in stdout
    assert "--i-mean-it" in stdout


def test_apply_in_place_without_confirm_exits_2() -> None:
    result = runner.invoke(app, ["apply", "job-id", "--in-place"])
    assert result.exit_code == 2
    combined = _plain(f"{result.stdout}{result.stderr}").lower()
    assert "i-mean-it" in combined
    assert "not implemented" not in combined


def test_doctor_reports_dependencies_without_exposing_keys(monkeypatch):
    from gpsify.cli import app
    from typer.testing import CliRunner
    monkeypatch.setattr("gpsify.exif.exiftool_available", lambda: False)
    monkeypatch.setattr("gpsify.media.heic_plugin_available", lambda: False)
    monkeypatch.setenv("GPSIFY_VISION_PROVIDER", "xai")
    monkeypatch.setenv("XAI_API_KEY", "secret-do-not-print")
    monkeypatch.setenv("NOMINATIM_CONTACT", "test@example.com")
    result = CliRunner().invoke(app, ["doctor"])
    assert result.exit_code == 1
    assert "brew install exiftool" in result.output
    assert "uv sync --extra heic" in result.output
    assert "configured" in result.output
    assert "secret-do-not-print" not in result.output
