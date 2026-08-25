"""Folder ingest: walk, classify, hash, batched EXIF, cluster, upsert (PR 3).

Creates or resumes a job via the global index. Re-ingest follows the design
table (same sha256 keeps history; different sha256 resets current proposal
and decision; missing paths are marked ``missing``). ``--relocate`` rebinds
``root_canonical`` and matches existing rows by sha256.

HEIC decode and RAW preview extract live in ``media.py`` (PR 4) and are not
run here — ingest never writes ``previews/`` or ``thumbs/``. This module never
writes GPS.

The summary accounts for every photo row: ``total`` covers the whole job and
any status outside the tagged / untagged / skipped / missing buckets is
reported in ``other``, so re-ingesting an already-proposed job does not print
four zeros.
"""

from __future__ import annotations

import hashlib
import json
import os
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Never

from gpsify.cluster import ClusterMember, plan_clusters
from gpsify.exif import (
    ExifRead,
    MediaKind,
    XMP_EXTS,
    classify_media,
    is_processable,
    read_exif_batch,
)
from gpsify.store import JobConflictError, JobIndex, JobRepo, StoreError, options_hash
from gpsify.types import PhotoStatus

IGNORE_NAMES = frozenset({".DS_Store", "Thumbs.db"})
HASH_CHUNK_SIZE = 1024 * 1024


class IngestError(Exception):
    """User-facing ingest failure (CLI exit 2)."""


SUMMARY_BUCKETS: frozenset[PhotoStatus] = frozenset(
    {
        PhotoStatus.skipped_has_gps,
        PhotoStatus.pending,
        PhotoStatus.skipped_unsupported,
        PhotoStatus.skipped_unreadable,
        PhotoStatus.skipped_no_preview,
        PhotoStatus.missing,
    }
)


@dataclass(frozen=True)
class IngestSummary:
    job_id: str
    total: int
    tagged: int
    untagged: int
    skipped: int
    missing: int
    clusters: int
    other: tuple[tuple[str, int], ...]
    warnings: tuple[str, ...]


@dataclass
class _DiskFile:
    path: Path
    path_str: str
    relpath: str
    kind: MediaKind
    sha256: str
    size: int
    mtime_ns: int
    unreadable: bool = False


def is_photos_library(path: Path) -> bool:
    """True if ``path`` is or sits inside a ``*.photoslibrary`` bundle (K14)."""
    resolved = path.resolve()
    candidates = (resolved, *resolved.parents)
    return any(_is_photos_library_name(part.name) for part in candidates)


async def run_ingest(
    folder: Path,
    *,
    hint: str | None = None,
    recursive: bool = False,
    relocate: str | None = None,
    data_dir: Path,
) -> IngestSummary:
    root = folder.expanduser()
    try:
        root = root.resolve(strict=True)
    except FileNotFoundError as exc:
        raise IngestError(f"folder not found: {folder}") from exc
    if not root.is_dir():
        raise IngestError(f"not a directory: {folder}")
    if is_photos_library(root):
        raise IngestError(
            "refusing to ingest a Photos.app library bundle (*.photoslibrary). "
            "Export originals first."
        )

    root_canonical = str(root)
    opt_hash = options_hash(recursive=recursive)
    warnings: list[str] = []

    index = await JobIndex.open(data_dir)
    repo: JobRepo | None = None
    try:
        if relocate:
            repo, job_id = await _relocate_job(
                index,
                data_dir,
                relocate_id=relocate,
                root=root,
                root_canonical=root_canonical,
                recursive=recursive,
                hint=hint,
            )
        else:
            repo, job_id = await _open_or_create_job(
                index,
                data_dir,
                root=root,
                root_canonical=root_canonical,
                recursive=recursive,
                opt_hash=opt_hash,
                hint=hint,
            )

        disk_files = _walk_and_hash(root, recursive=recursive)
        processable_paths = [
            item.path for item in disk_files if is_processable(item.kind) and not item.unreadable
        ]
        exif_result = read_exif_batch(processable_paths)
        warnings.extend(exif_result.warnings)

        await _upsert_photos(repo, disk_files, exif_result.by_path)
        cluster_count = await _rebuild_clusters(repo)
        await index.touch_ingest(job_id)
        counts = await repo.count_photos_by_status()
        return IngestSummary(
            job_id=job_id,
            total=sum(counts.values()),
            tagged=counts.get(PhotoStatus.skipped_has_gps.value, 0),
            untagged=counts.get(PhotoStatus.pending.value, 0),
            skipped=(
                counts.get(PhotoStatus.skipped_unsupported.value, 0)
                + counts.get(PhotoStatus.skipped_unreadable.value, 0)
                + counts.get(PhotoStatus.skipped_no_preview.value, 0)
            ),
            missing=counts.get(PhotoStatus.missing.value, 0),
            clusters=cluster_count,
            other=_other_counts(counts),
            warnings=tuple(warnings),
        )
    except JobConflictError as exc:
        raise IngestError(str(exc)) from exc
    except StoreError as exc:
        raise IngestError(str(exc)) from exc
    finally:
        if repo is not None:
            await repo.close()
        await index.close()


async def _open_or_create_job(
    index: JobIndex,
    data_dir: Path,
    *,
    root: Path,
    root_canonical: str,
    recursive: bool,
    opt_hash: str,
    hint: str | None,
) -> tuple[JobRepo, str]:
    candidate_id = str(uuid.uuid4())
    row = await index.register(candidate_id, root_canonical, opt_hash)
    job_id = str(row["job_id"])
    if job_id == candidate_id:
        repo = await JobRepo.create(
            data_dir,
            job_id=job_id,
            root_path=str(root),
            root_canonical=root_canonical,
            options_hash=opt_hash,
            hint=hint,
            config_json=json.dumps(
                {"recursive": recursive}, separators=(",", ":"), sort_keys=True
            ),
        )
        return repo, job_id
    repo = await JobRepo.open(data_dir, job_id)
    if hint is not None:
        await repo.set_hint(hint)
    return repo, job_id


async def _relocate_job(
    index: JobIndex,
    data_dir: Path,
    *,
    relocate_id: str,
    root: Path,
    root_canonical: str,
    recursive: bool,
    hint: str | None,
) -> tuple[JobRepo, str]:
    existing = await index.get(relocate_id)
    if existing is None:
        raise IngestError(f"unknown job_id {relocate_id!r}")
    stored_hash = str(existing["options_hash"])
    expected_true = options_hash(recursive=True)
    expected_false = options_hash(recursive=False)
    if stored_hash == expected_true:
        stored_recursive = True
    elif stored_hash == expected_false:
        stored_recursive = False
    else:
        stored_recursive = recursive
    if stored_recursive != recursive:
        raise IngestError(
            "relocate keeps the original --recursive flag; "
            f"this job was created with recursive={stored_recursive}"
        )
    await index.relocate(relocate_id, root_canonical)
    repo = await JobRepo.open(data_dir, relocate_id)
    await repo.set_root(str(root), root_canonical)
    if hint is not None:
        await repo.set_hint(hint)
    return repo, relocate_id


def _walk_and_hash(root: Path, *, recursive: bool) -> list[_DiskFile]:
    files: list[_DiskFile] = []
    for path in _iter_files(root, recursive=recursive):
        kind = classify_media(path)
        relpath = path.relative_to(root).as_posix()
        try:
            stat = path.stat()
            digest = _sha256_file(path)
        except OSError:
            files.append(
                _DiskFile(
                    path=path,
                    path_str=str(path),
                    relpath=relpath,
                    kind=kind,
                    sha256="",
                    size=0,
                    mtime_ns=0,
                    unreadable=True,
                )
            )
            continue
        files.append(
            _DiskFile(
                path=path,
                path_str=str(path),
                relpath=relpath,
                kind=kind,
                sha256=digest,
                size=int(stat.st_size),
                mtime_ns=int(stat.st_mtime_ns),
            )
        )
    return files


def _iter_files(root: Path, *, recursive: bool) -> list[Path]:
    found: list[Path] = []
    if recursive:
        for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
            dirnames[:] = [
                name
                for name in dirnames
                if name != ".gpsify" and not _is_photos_library_name(name)
            ]
            for name in filenames:
                path = Path(dirpath) / name
                if _ignored(path, root):
                    continue
                if path.is_file():
                    found.append(path)
    else:
        for path in root.iterdir():
            if path.is_file() and not _ignored(path, root):
                found.append(path)
    return sorted(found)


def _ignored(path: Path, root: Path) -> bool:
    name = path.name
    if name.startswith("._"):
        return True
    if name in IGNORE_NAMES:
        return True
    if path.suffix.lower() in XMP_EXTS:
        return True
    try:
        parts = path.relative_to(root).parts
    except ValueError:
        parts = path.parts
    return ".gpsify" in parts


def _is_photos_library_name(name: str) -> bool:
    return name.endswith(".photoslibrary")


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(HASH_CHUNK_SIZE)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


async def _upsert_photos(
    repo: JobRepo,
    disk_files: list[_DiskFile],
    exif_by_path: dict[str, ExifRead],
) -> None:
    existing_rows = await repo.list_photos()
    by_path = {str(row["path"]): row for row in existing_rows}
    claimed_ids: set[str] = set()

    for item in disk_files:
        row = by_path.get(item.path_str)
        if row is None:
            row = _match_by_sha256(existing_rows, item.sha256, claimed_ids, item.relpath)
            if row is not None:
                await repo.update_photo(
                    row["id"],
                    path=item.path_str,
                    relpath=item.relpath,
                )
                row = await repo.get_photo(row["id"])
                if row is not None:
                    by_path[item.path_str] = row
        if row is None:
            await _insert_disk_file(repo, item, exif_by_path.get(item.path_str, ExifRead()))
            continue
        claimed_ids.add(str(row["id"]))
        await _refresh_existing(repo, row, item, exif_by_path.get(item.path_str, ExifRead()))

    seen_paths = {item.path_str for item in disk_files}
    for row in existing_rows:
        if row["id"] in claimed_ids:
            continue
        if row["path"] in seen_paths:
            continue
        if row["status"] == PhotoStatus.missing.value:
            continue
        await repo.set_photo_status(row["id"], PhotoStatus.missing)


async def _insert_disk_file(repo: JobRepo, item: _DiskFile, exif: ExifRead) -> None:
    status, error, has_gps, lat, lng = _classify_status(item, exif)
    await repo.insert_photo(
        path=item.path_str,
        relpath=item.relpath,
        sha256=item.sha256,
        size=item.size,
        mtime_ns=item.mtime_ns,
        status=status,
        error=error,
        datetime_original=exif.capture_time,
        make=exif.make,
        model=exif.model,
        has_gps=has_gps,
        existing_lat=lat,
        existing_lng=lng,
    )


async def _refresh_existing(
    repo: JobRepo,
    row: dict[str, object],
    item: _DiskFile,
    exif: ExifRead,
) -> None:
    photo_id = str(row["id"])
    if item.sha256 == row["sha256"] and not item.unreadable:
        await repo.update_photo(
            photo_id,
            size=item.size,
            mtime_ns=item.mtime_ns,
            path=item.path_str,
            relpath=item.relpath,
        )
        return
    await repo.clear_current_proposal(photo_id)
    await repo.clear_current_decision(photo_id)
    status, error, has_gps, lat, lng = _classify_status(item, exif)
    await repo.update_photo(
        photo_id,
        path=item.path_str,
        relpath=item.relpath,
        sha256=item.sha256,
        size=item.size,
        mtime_ns=item.mtime_ns,
        status=status,
        error=error,
        datetime_original=exif.capture_time,
        make=exif.make,
        model=exif.model,
        has_gps=has_gps,
        existing_lat=lat,
        existing_lng=lng,
    )


def _match_by_sha256(
    existing_rows: list[dict[str, object]],
    sha256: str,
    claimed_ids: set[str],
    relpath: str,
) -> dict[str, object] | None:
    if not sha256:
        return None
    matches = [
        row
        for row in existing_rows
        if row["sha256"] == sha256 and str(row["id"]) not in claimed_ids
    ]
    if not matches:
        return None
    for row in matches:
        if row["relpath"] == relpath:
            return row
    return matches[0]


def _classify_status(
    item: _DiskFile, exif: ExifRead
) -> tuple[PhotoStatus, str | None, bool, float | None, float | None]:
    if item.unreadable:
        return PhotoStatus.skipped_unreadable, "unreadable", False, None, None
    match item.kind:
        case MediaKind.unsupported:
            return PhotoStatus.skipped_unsupported, None, False, None, None
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic | MediaKind.raw:
            if exif.has_gps:
                return (
                    PhotoStatus.skipped_has_gps,
                    None,
                    True,
                    exif.gps_lat,
                    exif.gps_lng,
                )
            return PhotoStatus.pending, None, False, None, None
        case _:
            _unreachable(item.kind)


def _other_counts(counts: dict[str, int]) -> tuple[tuple[str, int], ...]:
    """Non-zero statuses no summary bucket covers, in ``PhotoStatus`` order.

    A resumed job carries photos that later stages already moved past
    ``pending`` (``proposed``, ``written``, ...). Without this the four buckets
    report an all-zero re-ingest of work that is plainly there.
    """
    bucketed = {status.value for status in SUMMARY_BUCKETS}
    all_statuses = {status.value for status in PhotoStatus}
    known = tuple(
        (status.value, counts[status.value])
        for status in PhotoStatus
        if status.value not in bucketed and counts.get(status.value)
    )
    unknown = tuple(
        (name, count)
        for name, count in sorted(counts.items())
        if count and name not in all_statuses
    )
    return known + unknown


async def _rebuild_clusters(repo: JobRepo) -> int:
    photos = await repo.list_photos()
    members: list[ClusterMember] = []
    for row in photos:
        status = PhotoStatus(str(row["status"]))
        if status in {
            PhotoStatus.missing,
            PhotoStatus.skipped_unsupported,
            PhotoStatus.skipped_unreadable,
        }:
            continue
        members.append(
            ClusterMember(
                photo_id=str(row["id"]),
                relpath=str(row["relpath"]),
                datetime_original=(
                    str(row["datetime_original"])
                    if row["datetime_original"] is not None
                    else None
                ),
            )
        )
    plans = plan_clusters(members)
    await repo.delete_clusters()
    for plan in plans:
        await repo.insert_cluster(
            cluster_id=plan.cluster_id,
            parent=plan.parent,
            first_ts=plan.first_ts,
            n=plan.n,
            undated=plan.undated,
        )
        for member in plan.members:
            await repo.update_photo(member.photo_id, cluster_id=plan.cluster_id)
    return len(plans)


def _unreachable(value: Never) -> Never:
    raise IngestError(f"unhandled media kind: {value!r}")
