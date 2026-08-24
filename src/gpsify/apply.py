"""Write GPS from the current decision only (PR 8).

Dest inspection runs before any ingest-hash abort so ``--in-place`` JPEG retry
works after tags change the source bytes. ``photo.sha256`` stays the pre-tag
hash. Receipts upsert on ``(photo_id, dest_path)``. RAW dest is the sibling
``.xmp``; RAW bytes are never rewritten.
"""

from __future__ import annotations

import json
import math
import shutil
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Never

from gpsify.exif import (
    HEIC_WRITE_REQUIRES_EXIFTOOL,
    ExifWriteError,
    GpsWrite,
    MediaKind,
    classify_media,
    dest_gps_matches,
    exiftool_available,
    render_exiftool_argfile,
    write_gps_batch,
)
from gpsify.media import hash_file
from gpsify.propose import LOCK_FILENAME, JobExclusiveLock, ProposeError
from gpsify.store import JobRepo, StoreError, job_dir
from gpsify.store.db import utc_now_iso
from gpsify.store.repo import WRITEABLE_DECISION_ACTIONS
from gpsify.types import DecisionAction, PhotoStatus, WriteReceiptStatus

PNG_XMP_ONLY_WARNING = "png_xmp_only"
SOURCE_MISSING = "source_missing"
SOURCE_CHANGED = "source_changed"
DEST_DIVERGED = "dest_diverged"
NULL_COORDS = "decision_coords_missing"


class ApplyError(Exception):
    """Usage / lock / missing-job failure (CLI exit 2)."""


@dataclass
class ApplySummary:
    job_id: str
    dest_root: str
    in_place: bool = False
    dry_run: bool = False
    wrote: int = 0
    failed: int = 0
    skipped: int = 0
    eligible: int = 0
    argfile: str = ""
    warnings: list[str] = field(default_factory=list)


@dataclass
class _QueuedWrite:
    photo: dict[str, Any]
    decision: dict[str, Any]
    kind: MediaKind
    dest: Path
    image_dest: Path
    lat: float
    lng: float
    in_place: bool
    backup_path: str | None


def _unreachable(value: Never) -> Never:
    raise ApplyError(f"unhandled value: {value!r}")


def _finite(value: float | None) -> bool:
    return value is not None and math.isfinite(value)


def copies_root(data_dir: Path, job_id: str) -> Path:
    return job_dir(data_dir, job_id) / "copies"


def backups_root(data_dir: Path, job_id: str) -> Path:
    return job_dir(data_dir, job_id) / "backups"


def backup_path_for(data_dir: Path, job_id: str, photo_id: str, kind: MediaKind) -> Path:
    root = backups_root(data_dir, job_id)
    match kind:
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic:
            return root / f"{photo_id}.bin"
        case MediaKind.raw:
            return root / f"{photo_id}.xmp"
        case MediaKind.unsupported:
            return root / f"{photo_id}.bin"
        case _:
            _unreachable(kind)


def resolve_dests(
    photo: dict[str, Any],
    kind: MediaKind,
    *,
    dest_root: Path,
    in_place: bool,
) -> tuple[Path, Path]:
    """Return ``(receipt_dest, image_dest)``. RAW receipt dest is the sidecar."""
    source = Path(str(photo["path"]))
    rel = Path(str(photo["relpath"]))
    if rel.is_absolute():
        rel = Path(rel.name)
    match kind:
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic:
            image = source if in_place else dest_root / rel
            return image, image
        case MediaKind.raw:
            image = source if in_place else dest_root / rel
            return image.with_suffix(".xmp"), image
        case MediaKind.unsupported:
            image = source if in_place else dest_root / rel
            return image, image
        case _:
            _unreachable(kind)


def _safe_hash(path: Path) -> str | None:
    if not path.is_file():
        return None
    return hash_file(path)


def _copy_file(source: Path, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, dest)


async def run_apply(
    job_id: str,
    *,
    data_dir: Path,
    out: Path | None = None,
    in_place: bool = False,
    dry_run: bool = False,
    i_mean_it: bool = False,
    progress: Callable[[str], None] | None = None,
) -> ApplySummary:
    if out is not None and in_place:
        raise ApplyError("--out and --in-place are mutually exclusive")
    if in_place and not dry_run and not i_mean_it:
        raise ApplyError("--in-place requires --i-mean-it or an interactive confirm")

    root = job_dir(data_dir, job_id)
    dest_root = Path(out).expanduser().resolve() if out is not None else copies_root(data_dir, job_id)
    summary = ApplySummary(job_id=job_id, dest_root=str(dest_root), in_place=in_place, dry_run=dry_run)

    lock = JobExclusiveLock(root / LOCK_FILENAME)
    try:
        lock.acquire()
    except ProposeError as exc:
        raise ApplyError(str(exc)) from exc

    repo: JobRepo | None = None
    try:
        try:
            repo = await JobRepo.open(data_dir, job_id)
        except StoreError as exc:
            raise ApplyError(str(exc)) from exc
        job = await repo.get_job()
        if job is None:
            raise ApplyError(f"job row missing for {job_id}")
        if in_place:
            summary.dest_root = str(job["root_path"])

        photos = await repo.list_photos()
        decisions = {row["photo_id"]: row for row in await repo.list_current_decisions()}
        queued: list[_QueuedWrite] = []
        saw_png = False

        for photo in photos:
            kind = classify_media(Path(str(photo["relpath"])))
            decision = decisions.get(photo["id"])
            dest, image_dest = resolve_dests(photo, kind, dest_root=dest_root, in_place=in_place)
            item = await _inspect_one(
                repo,
                photo=photo,
                decision=decision,
                kind=kind,
                dest=dest,
                image_dest=image_dest,
                data_dir=data_dir,
                job_id=job_id,
                in_place=in_place,
                dry_run=dry_run,
            )
            if item is None:
                summary.skipped += 1
                continue
            if isinstance(item, str):
                summary.failed += 1
                continue
            if kind is MediaKind.png:
                saw_png = True
            if dry_run:
                queued.append(item)
                continue
            if dest_gps_matches(item.dest, item.lat, item.lng):
                await _mark_ok(repo, item)
                summary.wrote += 1
                continue
            queued.append(item)

        summary.eligible = summary.wrote + summary.failed + len(queued)
        gps_writes = [
            GpsWrite(
                dest=item.dest,
                lat=item.lat,
                lng=item.lng,
                kind=item.kind,
                job_id=job_id,
                decision_id=str(item.decision["id"]),
            )
            for item in queued
        ]
        summary.argfile = render_exiftool_argfile(gps_writes)

        if dry_run:
            if progress is not None:
                progress(summary.argfile.rstrip() or "(no writes)")
            await _write_apply_stats(repo, summary)
            return summary

        if queued:
            heic_without_tool = [
                item for item in queued if item.kind is MediaKind.heic and not exiftool_available()
            ]
            writable = [item for item in queued if item not in heic_without_tool]
            for item in heic_without_tool:
                await _mark_failed(repo, item, HEIC_WRITE_REQUIRES_EXIFTOOL)
                summary.failed += 1
            if writable:
                try:
                    errors = write_gps_batch(
                        [
                            GpsWrite(
                                dest=item.dest,
                                lat=item.lat,
                                lng=item.lng,
                                kind=item.kind,
                                job_id=job_id,
                                decision_id=str(item.decision["id"]),
                            )
                            for item in writable
                        ]
                    )
                except ExifWriteError as exc:
                    for item in writable:
                        await _mark_failed(repo, item, str(exc))
                        summary.failed += 1
                else:
                    by_dest = {str(item.dest): item for item in writable}
                    for dest_key, error in errors.items():
                        item = by_dest[dest_key]
                        if error is None:
                            await _mark_ok(repo, item)
                            summary.wrote += 1
                        else:
                            await _mark_failed(repo, item, error)
                            summary.failed += 1

        if saw_png and summary.wrote:
            summary.warnings.append(PNG_XMP_ONLY_WARNING)
        if progress is not None:
            progress(
                f"wrote: {summary.wrote}  failed: {summary.failed}  "
                f"skipped: {summary.skipped}  dest: {summary.dest_root}"
            )
            for warning in summary.warnings:
                progress(f"warning: {warning}")
        await _write_apply_stats(repo, summary)
        return summary
    finally:
        if repo is not None:
            await repo.close()
        lock.release()


async def _inspect_one(
    repo: JobRepo,
    *,
    photo: dict[str, Any],
    decision: dict[str, Any] | None,
    kind: MediaKind,
    dest: Path,
    image_dest: Path,
    data_dir: Path,
    job_id: str,
    in_place: bool,
    dry_run: bool,
) -> _QueuedWrite | str | None:
    """Prepare one photo. ``None`` = skip (not eligible). ``str`` = already failed."""
    if decision is None:
        return None
    try:
        action = DecisionAction(str(decision["action"]))
    except ValueError:
        return None
    if action not in WRITEABLE_DECISION_ACTIONS:
        return None

    lat = decision["lat"]
    lng = decision["lng"]
    if not _finite(lat) or not _finite(lng):
        if not dry_run:
            await _fail_receipt(
                repo,
                photo=photo,
                decision=decision,
                dest=dest,
                in_place=in_place,
                error=NULL_COORDS,
            )
        return NULL_COORDS
    assert lat is not None and lng is not None

    if kind is MediaKind.unsupported:
        if not dry_run:
            await _fail_receipt(
                repo,
                photo=photo,
                decision=decision,
                dest=dest,
                in_place=in_place,
                error="unsupported_media",
            )
        return "unsupported_media"

    existing = await repo.get_write_receipt(photo["id"], str(dest))
    backup_existing = existing["backup_path"] if existing is not None else None

    if dry_run:
        return _QueuedWrite(
            photo=photo,
            decision=decision,
            kind=kind,
            dest=dest,
            image_dest=image_dest,
            lat=float(lat),
            lng=float(lng),
            in_place=in_place,
            backup_path=backup_existing,
        )

    await repo.upsert_write_receipt(
        photo_id=photo["id"],
        decision_id=str(decision["id"]),
        dest_path=str(dest),
        in_place=in_place,
        status=WriteReceiptStatus.pending,
        error=None,
        written_at=None,
        written_sha256=None,
        backup_path=backup_existing,
        receipt_id=None if existing is None else str(existing["id"]),
    )

    if dest.is_file() and dest_gps_matches(dest, float(lat), float(lng)):
        return _QueuedWrite(
            photo=photo,
            decision=decision,
            kind=kind,
            dest=dest,
            image_dest=image_dest,
            lat=float(lat),
            lng=float(lng),
            in_place=in_place,
            backup_path=backup_existing,
        )

    source = Path(str(photo["path"]))
    ingest_hash = str(photo["sha256"])

    if not dest.is_file():
        error = _handle_missing_dest(
            kind=kind,
            dest=dest,
            image_dest=image_dest,
            source=source,
            ingest_hash=ingest_hash,
            in_place=in_place,
        )
        if error is not None:
            await _fail_receipt(
                repo,
                photo=photo,
                decision=decision,
                dest=dest,
                in_place=in_place,
                error=error,
                backup_path=backup_existing,
            )
            return error
    else:
        error = _handle_mismatch_dest(
            kind=kind,
            dest=dest,
            ingest_hash=ingest_hash,
            in_place=in_place,
            backup_existing=backup_existing,
        )
        if error is not None:
            await _fail_receipt(
                repo,
                photo=photo,
                decision=decision,
                dest=dest,
                in_place=in_place,
                error=error,
                backup_path=backup_existing,
            )
            return error

    backup = _backup_before_write(
        kind=kind,
        dest=dest,
        data_dir=data_dir,
        job_id=job_id,
        photo_id=str(photo["id"]),
        previous=backup_existing,
    )
    await repo.upsert_write_receipt(
        photo_id=photo["id"],
        decision_id=str(decision["id"]),
        dest_path=str(dest),
        in_place=in_place,
        status=WriteReceiptStatus.pending,
        error=None,
        backup_path=backup,
    )
    return _QueuedWrite(
        photo=photo,
        decision=decision,
        kind=kind,
        dest=dest,
        image_dest=image_dest,
        lat=float(lat),
        lng=float(lng),
        in_place=in_place,
        backup_path=backup,
    )


def _handle_missing_dest(
    *,
    kind: MediaKind,
    dest: Path,
    image_dest: Path,
    source: Path,
    ingest_hash: str,
    in_place: bool,
) -> str | None:
    match kind:
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic:
            if in_place:
                return SOURCE_MISSING
            if not source.is_file():
                return SOURCE_MISSING
            if _safe_hash(source) != ingest_hash:
                return SOURCE_CHANGED
            _copy_file(source, dest)
            return None
        case MediaKind.raw:
            if not source.is_file():
                return SOURCE_MISSING
            if _safe_hash(source) != ingest_hash:
                return SOURCE_CHANGED
            if in_place:
                return None
            if not image_dest.is_file():
                _copy_file(source, image_dest)
            elif _safe_hash(image_dest) != ingest_hash:
                return DEST_DIVERGED
            return None
        case MediaKind.unsupported:
            return "unsupported_media"
        case _:
            _unreachable(kind)


def _handle_mismatch_dest(
    *,
    kind: MediaKind,
    dest: Path,
    ingest_hash: str,
    in_place: bool,
    backup_existing: str | None,
) -> str | None:
    if backup_existing:
        backup = Path(backup_existing)
        if backup.is_file():
            _copy_file(backup, dest)
            return None
    dest_hash = _safe_hash(dest)
    match kind:
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic:
            if dest_hash == ingest_hash:
                return None
            return SOURCE_CHANGED if in_place else DEST_DIVERGED
        case MediaKind.raw:
            # Sidecar bytes never equal the RAW ingest hash. An existing
            # sibling .xmp is backed up and merged (K17), including first write
            # onto a sidecar that was already next to the RAW.
            return None
        case MediaKind.unsupported:
            return "unsupported_media"
        case _:
            _unreachable(kind)


def _backup_before_write(
    *,
    kind: MediaKind,
    dest: Path,
    data_dir: Path,
    job_id: str,
    photo_id: str,
    previous: str | None,
) -> str | None:
    match kind:
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic:
            backup = backup_path_for(data_dir, job_id, photo_id, kind)
            if dest.is_file():
                _copy_file(dest, backup)
                return str(backup)
            return previous
        case MediaKind.raw:
            if dest.is_file():
                backup = backup_path_for(data_dir, job_id, photo_id, kind)
                _copy_file(dest, backup)
                return str(backup)
            return None
        case MediaKind.unsupported:
            return previous
        case _:
            _unreachable(kind)


async def _mark_ok(repo: JobRepo, item: _QueuedWrite) -> None:
    digest = hash_file(item.dest)
    await repo.upsert_write_receipt(
        photo_id=item.photo["id"],
        decision_id=str(item.decision["id"]),
        dest_path=str(item.dest),
        in_place=item.in_place,
        status=WriteReceiptStatus.ok,
        error=None,
        written_at=utc_now_iso(),
        written_sha256=digest,
        backup_path=item.backup_path,
    )
    await repo.set_photo_status(item.photo["id"], PhotoStatus.written, error=None)


async def _mark_failed(repo: JobRepo, item: _QueuedWrite, error: str) -> None:
    await _fail_receipt(
        repo,
        photo=item.photo,
        decision=item.decision,
        dest=item.dest,
        in_place=item.in_place,
        error=error,
        backup_path=item.backup_path,
    )


async def _fail_receipt(
    repo: JobRepo,
    *,
    photo: dict[str, Any],
    decision: dict[str, Any],
    dest: Path,
    in_place: bool,
    error: str,
    backup_path: str | None = None,
) -> None:
    existing = await repo.get_write_receipt(photo["id"], str(dest))
    keep_backup = backup_path
    if keep_backup is None and existing is not None:
        keep_backup = existing.get("backup_path")
    await repo.upsert_write_receipt(
        photo_id=photo["id"],
        decision_id=str(decision["id"]),
        dest_path=str(dest),
        in_place=in_place,
        status=WriteReceiptStatus.failed,
        error=error,
        backup_path=keep_backup,
    )
    await repo.set_photo_status(photo["id"], PhotoStatus.write_failed, error=error)


async def _write_apply_stats(repo: JobRepo, summary: ApplySummary) -> None:
    job = await repo.get_job()
    existing: dict[str, Any] = {}
    if job is not None and job["stats_json"]:
        try:
            loaded = json.loads(job["stats_json"])
            if isinstance(loaded, dict):
                existing = loaded
        except json.JSONDecodeError:
            existing = {}
    existing["apply"] = {
        "wrote": summary.wrote,
        "failed": summary.failed,
        "skipped": summary.skipped,
        "dest_root": summary.dest_root,
        "in_place": summary.in_place,
        "dry_run": summary.dry_run,
    }
    await repo.set_stats_json(json.dumps(existing))
