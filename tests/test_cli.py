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


def test_stub_command_exits_2() -> None:
    result = runner.invoke(app, ["status"])
    assert result.exit_code == 2
    combined = _plain(f"{result.stdout}{result.stderr}")
    assert "not implemented" in combined.lower()


def test_apply_rejects_out_and_in_place_together() -> None:
    result = runner.invoke(app, ["apply", "job-id", "--out", "/tmp/out", "--in-place"])
    assert result.exit_code == 2
    combined = _plain(f"{result.stdout}{result.stderr}")
    assert "--out" in combined and "--in-place" in combined
    assert "not implemented" not in combined.lower()
