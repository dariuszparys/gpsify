"""Status rollup and preview/thumb cleanup (PR 9).

``gpsify status --json`` reads ``job.stats_json`` plus live photo counts.
``gpsify clean`` deletes ``previews/`` and ``thumbs/`` only; sqlite and
backups stay. Cost counters are written by propose/apply; this module
exposes them.
"""

from __future__ import annotations

import json
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Never

from gpsify.apply import PNG_XMP_ONLY_WARNING, copies_root
from gpsify.exif import MediaKind, classify_media
from gpsify.store import JobIndex, JobRepo, StoreError, job_dir
from gpsify.types import PhotoStatus


class StatusError(Exception):
    """Missing job / usage failure (CLI exit 2)."""


@dataclass
class StatusRollup:
    job_id: str
    root_path: str
    root_canonical: str
    hint: str | None
    created_at: str
    dest_root: str
    counts: dict[str, int]
    stats: dict[str, Any]
    warnings: list[str] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {
            "job_id": self.job_id,
            "root_path": self.root_path,
            "root_canonical": self.root_canonical,
            "hint": self.hint,
            "created_at": self.created_at,
            "dest_root": self.dest_root,
            "counts": self.counts,
            "stats": self.stats,
            "warnings": list(self.warnings),
        }


@dataclass
class JobListEntry:
    job_id: str
    root_canonical: str
    options_hash: str
    created_at: str
    last_ingest_at: str
    dest_root: str

    def to_json(self) -> dict[str, Any]:
        return {
            "job_id": self.job_id,
            "root_canonical": self.root_canonical,
            "options_hash": self.options_hash,
            "created_at": self.created_at,
            "last_ingest_at": self.last_ingest_at,
            "dest_root": self.dest_root,
        }


@dataclass
class CleanSummary:
    job_id: str
    deleted_previews: bool
    deleted_thumbs: bool
    kept_sqlite: bool
    kept_backups: bool


def _unreachable(value: Never) -> Never:
    raise StatusError(f"unhandled value: {value!r}")


def _parse_stats(raw: str | None) -> dict[str, Any]:
    if not raw:
        return {}
    try:
        loaded = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    if isinstance(loaded, dict):
        return loaded
    return {}


def _counts_with_zeros(live: dict[str, int]) -> dict[str, int]:
    counts = {status.value: int(live.get(status.value, 0)) for status in PhotoStatus}
    for key, value in live.items():
        counts.setdefault(str(key), int(value))
    return counts


def _is_png(relpath: str) -> bool:
    kind = classify_media(Path(relpath))
    match kind:
        case MediaKind.png:
            return True
        case MediaKind.jpeg | MediaKind.heic | MediaKind.raw | MediaKind.unsupported:
            return False
        case _:
            _unreachable(kind)


async def _png_in_job(repo: JobRepo) -> bool:
    for photo in await repo.list_photos():
        if _is_png(str(photo["relpath"])):
            return True
    return False


async def list_job_entries(data_dir: Path) -> list[JobListEntry]:
    async with await JobIndex.open(data_dir) as index:
        rows = await index.list_jobs()
    return [
        JobListEntry(
            job_id=str(row["job_id"]),
            root_canonical=str(row["root_canonical"]),
            options_hash=str(row["options_hash"]),
            created_at=str(row["created_at"]),
            last_ingest_at=str(row["last_ingest_at"]),
            dest_root=str(copies_root(data_dir, str(row["job_id"]))),
        )
        for row in rows
    ]


async def job_status(data_dir: Path, job_id: str) -> StatusRollup:
    try:
        repo = await JobRepo.open(data_dir, job_id)
    except StoreError as exc:
        raise StatusError(str(exc)) from exc
    try:
        job = await repo.get_job()
        if job is None:
            raise StatusError(f"job row missing for {job_id}")
        live = await repo.count_photos_by_status()
        warnings: list[str] = []
        if await _png_in_job(repo):
            warnings.append(PNG_XMP_ONLY_WARNING)
        return StatusRollup(
            job_id=str(job["id"]),
            root_path=str(job["root_path"]),
            root_canonical=str(job["root_canonical"]),
            hint=job["hint"],
            created_at=str(job["created_at"]),
            dest_root=str(copies_root(data_dir, job_id)),
            counts=_counts_with_zeros(live),
            stats=_parse_stats(job["stats_json"]),
            warnings=warnings,
        )
    finally:
        await repo.close()


def format_job_list(entries: list[JobListEntry]) -> str:
    if not entries:
        return "no jobs"
    lines = ["job_id  last_ingest_at  dest_root  root"]
    for entry in entries:
        lines.append(
            f"{entry.job_id}  {entry.last_ingest_at}  {entry.dest_root}  {entry.root_canonical}"
        )
    return "\n".join(lines)


def format_status(rollup: StatusRollup) -> str:
    lines = [
        f"job_id: {rollup.job_id}",
        f"root: {rollup.root_path}",
        f"dest: {rollup.dest_root}",
    ]
    if rollup.hint:
        lines.append(f"hint: {rollup.hint}")
    nonzero = [f"{name}={n}" for name, n in rollup.counts.items() if n]
    if nonzero:
        lines.append("counts: " + "  ".join(nonzero))
    else:
        lines.append("counts: (empty)")
    propose = rollup.stats.get("propose")
    if isinstance(propose, dict):
        if "est_usd" in propose:
            lines.append(f"est_usd: {propose['est_usd']}")
        for key in ("input_tokens", "output_tokens", "reasoning_tokens"):
            if key in propose:
                lines.append(f"{key}: {propose[key]}")
    apply_stats = rollup.stats.get("apply")
    if isinstance(apply_stats, dict) and "wrote" in apply_stats:
        lines.append(
            "apply: "
            f"wrote={apply_stats.get('wrote', 0)}  "
            f"failed={apply_stats.get('failed', 0)}  "
            f"skipped={apply_stats.get('skipped', 0)}"
        )
    return "\n".join(lines)


async def run_clean(data_dir: Path, job_id: str) -> CleanSummary:
    try:
        repo = await JobRepo.open(data_dir, job_id)
    except StoreError as exc:
        raise StatusError(str(exc)) from exc
    await repo.close()

    root = job_dir(data_dir, job_id)
    previews = root / "previews"
    thumbs = root / "thumbs"
    sqlite_path = root / "job.sqlite"
    backups = root / "backups"
    deleted_previews = previews.exists()
    deleted_thumbs = thumbs.exists()
    if deleted_previews:
        shutil.rmtree(previews)
    if deleted_thumbs:
        shutil.rmtree(thumbs)
    return CleanSummary(
        job_id=job_id,
        deleted_previews=deleted_previews,
        deleted_thumbs=deleted_thumbs,
        kept_sqlite=sqlite_path.is_file(),
        kept_backups=backups.exists(),
    )
