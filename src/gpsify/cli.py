"""Typer CLI. Subcommands beyond ``--help`` are stubs until later design PRs land."""

from __future__ import annotations

from pathlib import Path

import typer

from gpsify import __version__

app = typer.Typer(
    name="gpsify",
    help=(
        "Propose GPS from photo pixels; write tags only after a human accepts. "
        "See docs/design.md."
    ),
    no_args_is_help=True,
    pretty_exceptions_enable=False,
)


def _not_implemented(command: str) -> None:
    typer.echo(
        f"gpsify {command} is not implemented yet. See docs/design.md (PR plan).",
        err=True,
    )
    raise typer.Exit(code=2)


def _version_callback(value: bool) -> None:
    if value:
        typer.echo(f"gpsify {__version__}")
        raise typer.Exit()


@app.callback()
def _root(
    version: bool = typer.Option(
        False,
        "--version",
        help="Print the version and exit.",
        callback=_version_callback,
        is_eager=True,
    ),
) -> None:
    """gpsify — visual geolocation of untagged photos."""


@app.command()
def ingest(
    folder: Path = typer.Argument(..., help="Photo folder to ingest."),
    hint: str | None = typer.Option(None, "--hint", help="Album / trip hint sent with vision calls."),
    recursive: bool = typer.Option(False, "--recursive", help="Walk subdirectories."),
    relocate: str | None = typer.Option(
        None,
        "--relocate",
        help="Rebind an existing job_id after a folder move.",
        metavar="JOB_ID",
    ),
) -> None:
    """Ingest a folder; print job_id and counts (tagged / untagged / skipped)."""
    _not_implemented("ingest")


@app.command()
def propose(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    limit: int | None = typer.Option(None, "--limit", help="Max photos to propose this run."),
    concurrency: int = typer.Option(3, "--concurrency", help="Parallel vision calls."),
    dry_run: bool = typer.Option(False, "--dry-run", help="No network; print planned calls and cache hits."),
    force: bool = typer.Option(False, "--force", help="Re-send photos that already have a current proposal."),
    no_context: bool = typer.Option(
        False,
        "--no-context",
        help="Pixels only: strip hint, folder, filename, and siblings.",
    ),
    provider: str = typer.Option("xai", "--provider", help="Vision provider. v1 is xai only."),
    geocode_uncached_max: int = typer.Option(
        50,
        "--geocode-uncached-max",
        help="Public Nominatim uncached lookup cap for this run.",
    ),
) -> None:
    """Call vision + geocode; write proposals. Does not write GPS."""
    _not_implemented("propose")


@app.command()
def review(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    port: int = typer.Option(8765, "--port", help="Loopback port (binds 127.0.0.1 only)."),
) -> None:
    """Open the localhost review UI (loopback only)."""
    _not_implemented("review")


@app.command()
def decide(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    photo: str = typer.Option(..., "--photo", help="Photo id to decide."),
    action: str = typer.Option(
        ...,
        "--action",
        help="accept | skip | accept_edited",
    ),
    lat: float | None = typer.Option(None, "--lat", help="Required for accept_edited."),
    lng: float | None = typer.Option(None, "--lng", help="Required for accept_edited."),
) -> None:
    """Test/script seam over add_decision. Not the review UX."""
    _not_implemented("decide")


@app.command()
def apply(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    out: Path | None = typer.Option(None, "--out", help="Write copies under DIR/<relpath>."),
    in_place: bool = typer.Option(False, "--in-place", help="Mutate sources (or RAW sidecars)."),
    dry_run: bool = typer.Option(False, "--dry-run", help="Print would-be writes; mutate nothing."),
) -> None:
    """Write GPS from the current decision only. Default dest is the job copies/ tree."""
    if out is not None and in_place:
        typer.echo("error: --out and --in-place are mutually exclusive", err=True)
        raise typer.Exit(code=2)
    _not_implemented("apply")


@app.command()
def undo(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    photo: Path | None = typer.Option(None, "--photo", help="Limit undo to one source path."),
) -> None:
    """Restore the backup taken before exiftool (or delete/restore a RAW sidecar)."""
    _not_implemented("undo")


@app.command("skip")
def skip_cmd(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    all_low: bool = typer.Option(False, "--all-low", help="Preview low UI-rank photos."),
    yes: bool = typer.Option(False, "--yes", help="Write skip decisions without a TTY confirm."),
) -> None:
    """Preview (and with --yes, write) skip decisions for low UI-rank photos."""
    _not_implemented("skip")


@app.command()
def status(
    job_id: str | None = typer.Argument(None, help="Job UUID. Omit to list jobs."),
    json_output: bool = typer.Option(False, "--json", help="Machine-readable rollup."),
) -> None:
    """Print job counts, dest root, and cost counters."""
    _not_implemented("status")


@app.command()
def clean(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
) -> None:
    """Delete previews and thumbs; keep sqlite and backups."""
    _not_implemented("clean")


def main() -> None:
    app()
