"""Lazy JPEG previews and thumbs.

Ingest never writes ``previews/`` or ``thumbs/``. Propose and the review UI
call this module just-in-time. Pipeline: decode (HEIC primary still, RAW
embedded JPEG via ``exiftool -b``) → ``ImageOps.exif_transpose`` → long-edge
resize → JPEG quality 85 / 4:2:0. Originals are never mutated. RAW containers
are never opened as images and never sent. This module never writes GPS.
"""

from __future__ import annotations

import hashlib
import io
import subprocess
from pathlib import Path
from typing import Never

from PIL import Image, ImageOps, UnidentifiedImageError

from gpsify.exif import (
    EXIFTOOL_TIMEOUT_S,
    MediaKind,
    classify_media,
    exiftool_available,
)
from gpsify.types import PhotoStatus

try:
    import pillow_heif
except ImportError:
    pillow_heif = None

PREVIEW_LONG_EDGE = 1280
THUMB_LONG_EDGE = 256
JPEG_QUALITY = 85
MIN_RAW_PREVIEW_EDGE = 256
HASH_CHUNK_SIZE = 1024 * 1024
RAW_PREVIEW_TAGS = ("JpgFromRaw", "PreviewImage", "OtherImage", "ThumbnailImage")
HEIC_INSTALL_HINT = (
    "HEIC requires pillow-heif. Install with: pip install 'gpsify[heic]'"
)


class MediaError(Exception):
    """Preview/thumb failure. ``status`` is the photo status the caller should persist."""

    def __init__(self, message: str, *, status: PhotoStatus) -> None:
        super().__init__(message)
        self.status = status


def heic_plugin_available() -> bool:
    return pillow_heif is not None


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(HASH_CHUNK_SIZE)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def preview_path(job_root: Path, photo_id: str) -> Path:
    return job_root / "previews" / f"{photo_id}.jpg"


def thumb_path(job_root: Path, photo_id: str) -> Path:
    return job_root / "thumbs" / f"{photo_id}.jpg"


def render_preview(
    path: Path,
    kind: MediaKind | None = None,
    *,
    expected_sha256: str,
) -> bytes:
    """Transposed, long-edge 1280, JPEG q85. Re-hashes the original before returning."""
    return render_jpeg(
        path,
        kind,
        expected_sha256=expected_sha256,
        long_edge=PREVIEW_LONG_EDGE,
    )


def render_thumb(
    path: Path,
    kind: MediaKind | None = None,
    *,
    expected_sha256: str,
) -> bytes:
    """Transposed, long-edge 256, JPEG q85. Re-hashes the original before returning."""
    return render_jpeg(
        path,
        kind,
        expected_sha256=expected_sha256,
        long_edge=THUMB_LONG_EDGE,
    )


def render_jpeg(
    path: Path,
    kind: MediaKind | None = None,
    *,
    expected_sha256: str,
    long_edge: int,
) -> bytes:
    resolved_kind = kind if kind is not None else classify_media(path)
    image = _open_source(path, resolved_kind)
    try:
        oriented = ImageOps.exif_transpose(image)
        if oriented is None:
            oriented = image
        rgb = oriented.convert("RGB")
        resized = _resize_long_edge(rgb, long_edge)
        jpeg = _encode_jpeg(resized)
    finally:
        image.close()
    digest = hash_file(path)
    if digest != expected_sha256:
        raise MediaError(
            "source sha256 changed; refusing to send preview",
            status=PhotoStatus.error,
        )
    return jpeg


def ensure_preview(
    job_root: Path,
    photo_id: str,
    source: Path,
    *,
    expected_sha256: str,
    kind: MediaKind | None = None,
) -> Path:
    """Write ``previews/<photo_id>.jpg`` if missing. Never mutates ``source``."""
    return _ensure_derived(
        preview_path(job_root, photo_id),
        source,
        expected_sha256=expected_sha256,
        kind=kind,
        long_edge=PREVIEW_LONG_EDGE,
    )


def ensure_thumb(
    job_root: Path,
    photo_id: str,
    source: Path,
    *,
    expected_sha256: str,
    kind: MediaKind | None = None,
) -> Path:
    """Write ``thumbs/<photo_id>.jpg`` if missing. Never mutates ``source``."""
    return _ensure_derived(
        thumb_path(job_root, photo_id),
        source,
        expected_sha256=expected_sha256,
        kind=kind,
        long_edge=THUMB_LONG_EDGE,
    )


def extract_raw_preview_jpeg(path: Path) -> bytes:
    """First non-empty ``exiftool -b`` extract whose decoded long edge is ≥ 256 px.

    Tag order: JpgFromRaw, PreviewImage, OtherImage, then ThumbnailImage only
    as a last resort. Never returns RAW sensor bytes.
    """
    if not exiftool_available():
        raise MediaError(
            "RAW preview extract needs exiftool; no usable embedded preview",
            status=PhotoStatus.skipped_no_preview,
        )
    for tag in RAW_PREVIEW_TAGS:
        blob = _exiftool_binary_tag(path, tag)
        if not blob:
            continue
        long_edge = _jpeg_long_edge(blob)
        if long_edge is None:
            continue
        if long_edge >= MIN_RAW_PREVIEW_EDGE:
            return blob
    raise MediaError(
        "no usable RAW embedded preview (need long edge >= 256 px)",
        status=PhotoStatus.skipped_no_preview,
    )


def _ensure_derived(
    dest: Path,
    source: Path,
    *,
    expected_sha256: str,
    kind: MediaKind | None,
    long_edge: int,
) -> Path:
    if dest.is_file():
        digest = hash_file(source)
        if digest != expected_sha256:
            raise MediaError(
                "source sha256 changed; refusing to send preview",
                status=PhotoStatus.error,
            )
        return dest
    jpeg = render_jpeg(
        source,
        kind,
        expected_sha256=expected_sha256,
        long_edge=long_edge,
    )
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(jpeg)
    return dest


def _open_source(path: Path, kind: MediaKind) -> Image.Image:
    match kind:
        case MediaKind.jpeg | MediaKind.png:
            return _open_raster(path)
        case MediaKind.heic:
            return _open_heic(path)
        case MediaKind.raw:
            preview = extract_raw_preview_jpeg(path)
            return _open_raster_bytes(preview)
        case MediaKind.unsupported:
            raise MediaError(
                f"unsupported media for preview: {path.suffix}",
                status=PhotoStatus.skipped_unsupported,
            )
        case _:
            _unreachable(kind)


def _open_heic(path: Path) -> Image.Image:
    if pillow_heif is None:
        raise MediaError(HEIC_INSTALL_HINT, status=PhotoStatus.skipped_unsupported)
    # Primary still only (Live Photos / HEIC sequences). Do not iterate extras.
    try:
        heif_file = pillow_heif.open_heif(path)
    except Exception as exc:
        raise MediaError(
            f"unreadable HEIC: {exc}",
            status=PhotoStatus.skipped_unreadable,
        ) from exc
    to_pillow = getattr(heif_file, "to_pillow", None)
    if callable(to_pillow):
        try:
            image = to_pillow()
        except Exception as exc:
            raise MediaError(
                f"unreadable HEIC: {exc}",
                status=PhotoStatus.skipped_unreadable,
            ) from exc
        if image is not None:
            return image
    try:
        pillow_heif.register_heif_opener()
        return _open_raster(path)
    except MediaError:
        raise
    except Exception as exc:
        raise MediaError(
            f"unreadable HEIC: {exc}",
            status=PhotoStatus.skipped_unreadable,
        ) from exc


def _open_raster(path: Path) -> Image.Image:
    try:
        image = Image.open(path)
        image.load()
    except (OSError, UnidentifiedImageError, ValueError) as exc:
        raise MediaError(
            f"unreadable: {exc}",
            status=PhotoStatus.skipped_unreadable,
        ) from exc
    return image


def _open_raster_bytes(blob: bytes) -> Image.Image:
    try:
        image = Image.open(io.BytesIO(blob))
        image.load()
    except (OSError, UnidentifiedImageError, ValueError) as exc:
        raise MediaError(
            f"unreadable embedded preview: {exc}",
            status=PhotoStatus.skipped_unreadable,
        ) from exc
    return image


def _jpeg_long_edge(blob: bytes) -> int | None:
    try:
        with Image.open(io.BytesIO(blob)) as image:
            image.load()
            return max(image.size)
    except (OSError, UnidentifiedImageError, ValueError):
        return None


def _exiftool_binary_tag(path: Path, tag: str) -> bytes:
    cmd = ["exiftool", "-b", f"-{tag}", "--", str(path)]
    try:
        completed = subprocess.run(
            cmd,
            check=False,
            capture_output=True,
            timeout=EXIFTOOL_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired):
        return b""
    return completed.stdout or b""


def _resize_long_edge(image: Image.Image, long_edge: int) -> Image.Image:
    width, height = image.size
    current = max(width, height)
    if current <= 0 or current <= long_edge:
        return image
    scale = long_edge / current
    new_size = (max(1, round(width * scale)), max(1, round(height * scale)))
    return image.resize(new_size, Image.Resampling.LANCZOS)


def _encode_jpeg(image: Image.Image) -> bytes:
    rgb = image if image.mode == "RGB" else image.convert("RGB")
    buffer = io.BytesIO()
    rgb.save(
        buffer,
        format="JPEG",
        quality=JPEG_QUALITY,
        subsampling=2,
    )
    return buffer.getvalue()


def _unreachable(value: Never) -> Never:
    raise MediaError(f"unhandled media kind: {value!r}", status=PhotoStatus.error)
