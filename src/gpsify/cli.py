"""Typer CLI. Status and clean land in this slice (PR 9)."""

from __future__ import annotations

import asyncio
import json
import sys
import webbrowser
from pathlib import Path
from typing import Never

import typer

from gpsify import __version__
from gpsify.apply import ApplyError, run_apply
from gpsify.config import load_settings
from gpsify.geocode import GeocodeConfigError, require_geocoder_ready
from gpsify.ingest import IngestError, run_ingest
from gpsify.propose import ProposeError, run_geocode, run_propose
from gpsify.review import REVIEW_HOST, ReviewError, create_app, list_low_rank_photos, serve_review
from gpsify.status import StatusError, format_job_list, format_status, job_status, list_job_entries, run_clean
from gpsify.store import DecisionError, JobRepo, StoreError
from gpsify.types import DecisionAction
from gpsify.undo import UndoError, run_undo

app = typer.Typer(
    name="gpsify",
    help=(
        "Propose GPS from photo pixels; write tags only after a human accepts. "
        "See docs/design.md."
    ),
    no_args_is_help=True,
    pretty_exceptions_enable=False,
)


def _version_callback(value: bool) -> None:
    if value:
        typer.echo(f"gpsify {__version__}")
        raise typer.Exit()


def _unreachable(value: Never) -> Never:
    raise ReviewError(f"unhandled value: {value!r}")


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
    """Ingest a folder; print job_id and counts (total / tagged / untagged / skipped)."""
    settings = load_settings()
    try:
        summary = asyncio.run(
            run_ingest(
                folder,
                hint=hint,
                recursive=recursive,
                relocate=relocate,
                data_dir=settings.data_dir,
            )
        )
    except (IngestError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    typer.echo(summary.job_id)
    typer.echo(
        f"total: {summary.total}  tagged: {summary.tagged}  "
        f"untagged: {summary.untagged}  skipped: {summary.skipped}  "
        f"missing: {summary.missing}  clusters: {summary.clusters}"
    )
    if summary.other:
        typer.echo("other: " + "  ".join(f"{name}={n}" for name, n in summary.other))
    for warning in summary.warnings:
        typer.echo(f"warning: {warning}", err=True)


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
    provider: str | None = typer.Option(
        None,
        "--provider",
        help="Vision provider: xai | gemini. Defaults to GPSIFY_VISION_PROVIDER.",
    ),
    geocode_uncached_max: int = typer.Option(
        50,
        "--geocode-uncached-max",
        help="Public Nominatim uncached lookup cap for this run.",
    ),
) -> None:
    """Call vision + geocode; write proposals. Does not write GPS."""
    settings = load_settings()
    try:
        summary = asyncio.run(
            run_propose(
                job_id,
                settings=settings,
                limit=limit,
                concurrency=concurrency,
                dry_run=dry_run,
                force=force,
                no_context=no_context,
                provider_name=provider,
                geocode_uncached_max=geocode_uncached_max,
                progress=lambda message: typer.echo(message),
            )
        )
    except (ProposeError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    for warning in summary.warnings:
        typer.echo(f"warning: {warning}", err=True)
    if summary.error:
        raise typer.Exit(code=1)


@app.command()
def geocode(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    limit: int | None = typer.Option(
        None, "--limit", help="Max unpinned proposals to geocode this run."
    ),
    dry_run: bool = typer.Option(
        False,
        "--dry-run",
        help="No network; count the place names that still need an uncached lookup.",
    ),
    geocode_uncached_max: int = typer.Option(
        50,
        "--geocode-uncached-max",
        help="Public Nominatim uncached lookup cap for this run.",
    ),
) -> None:
    """Geocode proposals left at pin_source=none. No vision call, no vision spend."""
    settings = load_settings()
    try:
        summary = asyncio.run(
            run_geocode(
                job_id,
                settings=settings,
                limit=limit,
                dry_run=dry_run,
                geocode_uncached_max=geocode_uncached_max,
                progress=lambda message: typer.echo(message),
            )
        )
    except (ProposeError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    for warning in summary.warnings:
        typer.echo(f"warning: {warning}", err=True)


@app.command()
def review(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    port: int = typer.Option(8765, "--port", help="Loopback port (binds 127.0.0.1 only)."),
) -> None:
    """Open the localhost review UI (loopback only)."""
    settings = load_settings()
    try:
        require_geocoder_ready(settings)
    except GeocodeConfigError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc

    async def _probe() -> None:
        repo = await JobRepo.open(settings.data_dir, job_id)
        await repo.close()

    try:
        asyncio.run(_probe())
    except StoreError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    starlette_app = create_app(settings=settings, job_id=job_id)
    url = f"http://{REVIEW_HOST}:{port}"
    typer.echo(f"{url}  (loopback only; no LAN bind)")
    webbrowser.open(url)
    serve_review(starlette_app, port)


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
    try:
        action_enum = DecisionAction(action)
    except ValueError:
        typer.echo("error: --action must be accept, skip, or accept_edited", err=True)
        raise typer.Exit(code=2) from None
    match action_enum:
        case DecisionAction.accept | DecisionAction.skip | DecisionAction.accept_edited:
            pass
        case DecisionAction.cluster_apply:
            typer.echo(
                "error: cluster_apply is the review API; use accept/skip/accept_edited",
                err=True,
            )
            raise typer.Exit(code=2)
        case _:
            _unreachable(action_enum)
    settings = load_settings()

    async def _run() -> str:
        repo = await JobRepo.open(settings.data_dir, job_id)
        try:
            return await repo.add_decision(photo, action_enum, lat=lat, lng=lng)
        finally:
            await repo.close()

    try:
        decision_id = asyncio.run(_run())
    except (DecisionError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    typer.echo(decision_id)
    typer.echo(action_enum.value)


@app.command()
def apply(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    out: Path | None = typer.Option(None, "--out", help="Write copies under DIR/<relpath>."),
    in_place: bool = typer.Option(False, "--in-place", help="Mutate sources (or RAW sidecars)."),
    dry_run: bool = typer.Option(False, "--dry-run", help="Print would-be writes; mutate nothing."),
    i_mean_it: bool = typer.Option(
        False,
        "--i-mean-it",
        help="Confirm --in-place without a TTY prompt.",
    ),
) -> None:
    """Write GPS from the current decision only. Default dest is the job copies/ tree."""
    if out is not None and in_place:
        typer.echo("error: --out and --in-place are mutually exclusive", err=True)
        raise typer.Exit(code=2)
    if in_place and not dry_run and not i_mean_it:
        if not sys.stdin.isatty():
            typer.echo("error: pass --i-mean-it to apply --in-place (not a TTY)", err=True)
            raise typer.Exit(code=2)
        if not typer.confirm("Mutate originals in place?"):
            raise typer.Exit(code=1)
        i_mean_it = True
    settings = load_settings()
    try:
        summary = asyncio.run(
            run_apply(
                job_id,
                data_dir=settings.data_dir,
                out=out,
                in_place=in_place,
                dry_run=dry_run,
                i_mean_it=i_mean_it or dry_run,
                progress=lambda message: typer.echo(message),
            )
        )
    except (ApplyError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    if summary.dry_run:
        return
    if summary.eligible == 0:
        typer.echo("nothing accepted to apply", err=True)
        raise typer.Exit(code=3)
    if summary.failed:
        raise typer.Exit(code=1)


@app.command()
def undo(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    photo: Path | None = typer.Option(None, "--photo", help="Limit undo to one source path."),
) -> None:
    """Restore the backup taken before exiftool (or delete/restore a RAW sidecar)."""
    settings = load_settings()
    try:
        summary = asyncio.run(
            run_undo(
                job_id,
                data_dir=settings.data_dir,
                photo=photo,
                progress=lambda message: typer.echo(message),
            )
        )
    except (UndoError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    if summary.failed:
        raise typer.Exit(code=1)


@app.command("skip")
def skip_cmd(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
    all_low: bool = typer.Option(False, "--all-low", help="Preview low UI-rank photos."),
    yes: bool = typer.Option(False, "--yes", help="Write skip decisions without a TTY confirm."),
) -> None:
    """Preview (and with --yes, write) skip decisions for low UI-rank photos."""
    if not all_low:
        typer.echo("error: --all-low is required", err=True)
        raise typer.Exit(code=2)
    settings = load_settings()

    async def _list() -> list[dict]:
        repo = await JobRepo.open(settings.data_dir, job_id)
        try:
            return await list_low_rank_photos(repo)
        finally:
            await repo.close()

    try:
        rows = asyncio.run(_list())
    except StoreError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    if not rows:
        typer.echo("no low-rank proposed photos")
        return
    typer.echo(f"low-rank photos (ui_rank < 0.55): {len(rows)}")
    for row in rows:
        place = row.get("place_name") or ""
        typer.echo(f"  {row['id']}  {row['relpath']}  {row['ui_rank']:.2f}  {place}")
    if not yes:
        if not sys.stdin.isatty():
            typer.echo("error: pass --yes to write skip decisions (not a TTY)", err=True)
            raise typer.Exit(code=2)
        if not typer.confirm(f"Write skip decisions for {len(rows)} photos?"):
            raise typer.Exit(code=1)

    async def _write() -> int:
        repo = await JobRepo.open(settings.data_dir, job_id)
        try:
            for row in rows:
                await repo.add_decision(row["id"], DecisionAction.skip)
            return len(rows)
        finally:
            await repo.close()

    try:
        written = asyncio.run(_write())
    except (DecisionError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    typer.echo(f"skipped {written} photos")


@app.command()
def status(
    job_id: str | None = typer.Argument(None, help="Job UUID. Omit to list jobs."),
    json_output: bool = typer.Option(False, "--json", help="Machine-readable rollup."),
) -> None:
    """Print job counts, dest root, and cost counters."""
    settings = load_settings()
    try:
        if job_id is None:
            entries = asyncio.run(list_job_entries(settings.data_dir))
            if json_output:
                payload = {"jobs": [entry.to_json() for entry in entries]}
                typer.echo(json.dumps(payload, sort_keys=True))
            else:
                typer.echo(format_job_list(entries))
            return
        rollup = asyncio.run(job_status(settings.data_dir, job_id))
    except (StatusError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    if json_output:
        typer.echo(json.dumps(rollup.to_json(), sort_keys=True))
    else:
        typer.echo(format_status(rollup))
    for warning in rollup.warnings:
        typer.echo(f"warning: {warning}", err=True)


@app.command()
def clean(
    job_id: str = typer.Argument(..., help="Job UUID from ingest."),
) -> None:
    """Delete previews and thumbs; keep sqlite and backups."""
    settings = load_settings()
    try:
        summary = asyncio.run(run_clean(settings.data_dir, job_id))
    except (StatusError, StoreError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    deleted: list[str] = []
    if summary.deleted_previews:
        deleted.append("previews")
    if summary.deleted_thumbs:
        deleted.append("thumbs")
    if deleted:
        typer.echo(f"deleted {' and '.join(deleted)}")
    else:
        typer.echo("nothing to clean")
    typer.echo("kept sqlite and backups")


def main() -> None:
    app()
