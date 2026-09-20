"""Undo GPS writes from backup / dest deletion.

JPEG/HEIC/PNG: restore the full-file backup over an in-place dest, or delete
the copy-mode dest. RAW: delete or restore the sibling XMP sidecar only; never
touch RAW bytes. Receipts stay upsertable (status ``undone``, row not deleted).
"""

from __future__ import annotations

import shutil
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Never

from gpsify.exif import MediaKind, classify_media
from gpsify.media import hash_file
from gpsify.propose import LOCK_FILENAME, JobExclusiveLock, ProposeError
from gpsify.store import JobRepo, StoreError, job_dir
from gpsify.types import DecisionAction, PhotoStatus, WriteReceiptStatus


class UndoError(Exception):
    """Usage / lock / missing-job failure (CLI exit 2)."""


@dataclass
class UndoSummary:
    job_id: str
    undone: int = 0
    failed: int = 0
    skipped: int = 0
    warnings: list[str] = field(default_factory=list)


def _unreachable(value: Never) -> Never:
    raise UndoError(f"unhandled value: {value!r}")


def _photo_status_after_undo(action: str) -> PhotoStatus:
    try:
        action_enum = DecisionAction(action)
    except ValueError:
        return PhotoStatus.accepted
    match action_enum:
        case DecisionAction.accept:
            return PhotoStatus.accepted
        case DecisionAction.accept_edited | DecisionAction.cluster_apply:
            return PhotoStatus.accepted_edited
        case DecisionAction.skip:
            return PhotoStatus.skipped_by_user
        case _:
            _unreachable(action_enum)


async def run_undo(
    job_id: str,
    *,
    data_dir: Path,
    photo: Path | None = None,
    progress: Callable[[str], None] | None = None,
) -> UndoSummary:
    root = job_dir(data_dir, job_id)
    lock = JobExclusiveLock(root / LOCK_FILENAME)
    try:
        lock.acquire()
    except ProposeError as exc:
        raise UndoError(str(exc)) from exc

    repo: JobRepo | None = None
    summary = UndoSummary(job_id=job_id)
    try:
        try:
            repo = await JobRepo.open(data_dir, job_id)
        except StoreError as exc:
            raise UndoError(str(exc)) from exc
        photos = {row["id"]: row for row in await repo.list_photos()}
        receipts = await repo.list_write_receipts()
        wanted_photo_id: str | None = None
        if photo is not None:
            wanted_photo_id = _match_photo(photos, photo)
            if wanted_photo_id is None:
                raise UndoError(f"no photo matching {photo}")

        for receipt in receipts:
            if wanted_photo_id is not None and receipt["photo_id"] != wanted_photo_id:
                continue
            status = str(receipt["status"])
            try:
                status_enum = WriteReceiptStatus(status)
            except ValueError:
                summary.skipped += 1
                continue
            match status_enum:
                case WriteReceiptStatus.ok:
                    pass
                case (
                    WriteReceiptStatus.pending
                    | WriteReceiptStatus.failed
                    | WriteReceiptStatus.undone
                ):
                    summary.skipped += 1
                    continue
                case _:
                    _unreachable(status_enum)

            row = photos.get(receipt["photo_id"])
            if row is None:
                summary.failed += 1
                continue
            try:
                error = _undo_dest(row, receipt)
            except OSError as exc:
                error = f"undo_io_error: {exc}"
            if error is not None:
                summary.warnings.append(f"{receipt['photo_id']}: {error}")
                summary.failed += 1
                continue
            await repo.upsert_write_receipt(
                photo_id=receipt["photo_id"],
                decision_id=str(receipt["decision_id"]),
                dest_path=str(receipt["dest_path"]),
                in_place=bool(receipt["in_place"]),
                status=WriteReceiptStatus.undone,
                error=None,
                written_at=receipt.get("written_at"),
                written_sha256=receipt.get("written_sha256"),
                backup_path=receipt.get("backup_path"),
                receipt_id=str(receipt["id"]),
            )
            summary.undone += 1

        await _restore_photo_statuses(repo, photos)
        if progress is not None:
            for warning in summary.warnings:
                progress(f"warning: {warning}")
            progress(f"undone: {summary.undone}  failed: {summary.failed}  skipped: {summary.skipped}")
        return summary
    finally:
        if repo is not None:
            await repo.close()
        lock.release()


def _match_photo(photos: dict[str, dict[str, Any]], needle: Path) -> str | None:
    resolved = str(needle.expanduser())
    name = needle.name
    for row in photos.values():
        if str(row["path"]) == resolved or str(row["relpath"]) == resolved:
            return str(row["id"])
        if str(row["path"]) == str(needle) or str(row["relpath"]) == str(needle):
            return str(row["id"])
        if Path(str(row["path"])).name == name and name:
            matches = [
                item
                for item in photos.values()
                if Path(str(item["path"])).name == name or str(item["relpath"]) == name
            ]
            if len(matches) == 1:
                return str(matches[0]["id"])
    for row in photos.values():
        if str(row["relpath"]) == name:
            return str(row["id"])
    return None


def _undo_dest(photo: dict[str, Any], receipt: dict[str, Any]) -> str | None:
    dest = Path(str(receipt["dest_path"]))
    in_place = bool(receipt["in_place"])
    kind = classify_media(Path(str(photo["relpath"])))
    backup_raw = receipt.get("backup_path")
    backup = Path(backup_raw) if backup_raw else None
    if dest.is_file() and (
        not receipt.get("written_sha256")
        or hash_file(dest) != receipt["written_sha256"]
    ):
        return "dest_diverged"
    if kind is MediaKind.raw and not in_place:
        image_copy = dest.with_suffix(Path(str(photo["path"])).suffix)
        if image_copy.is_file() and hash_file(image_copy) != photo["sha256"]:
            return "source_copy_diverged"
    if kind is MediaKind.raw and backup is not None and not backup.is_file():
        return "backup_missing"
    match kind:
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic:
            if in_place:
                if backup is None or not backup.is_file():
                    return "backup_missing"
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(backup, dest)
                return None
            if dest.is_file():
                dest.unlink()
            return None
        case MediaKind.raw:
            if in_place:
                if backup is None or not backup.is_file():
                    if dest.is_file():
                        dest.unlink()
                    return None
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(backup, dest)
                return None
            if dest.is_file():
                dest.unlink()
            image_copy = dest.with_suffix(Path(str(photo["path"])).suffix)
            if image_copy.is_file() and image_copy != Path(str(photo["path"])):
                image_copy.unlink()
            return None
        case MediaKind.unsupported:
            return "unsupported_media"
        case _:
            _unreachable(kind)


async def _restore_photo_statuses(
    repo: JobRepo, photos: dict[str, dict[str, Any]]
) -> None:
    receipts = await repo.list_write_receipts()
    ok_by_photo: set[str] = set()
    for receipt in receipts:
        if receipt["status"] == WriteReceiptStatus.ok.value:
            ok_by_photo.add(str(receipt["photo_id"]))
    for photo_id, photo in photos.items():
        if photo_id in ok_by_photo:
            continue
        if photo["status"] != PhotoStatus.written.value:
            continue
        decision = await repo.get_current_decision(photo_id)
        action = str(decision["action"]) if decision is not None else DecisionAction.accept.value
        await repo.set_photo_status(photo_id, _photo_status_after_undo(action), error=None)
