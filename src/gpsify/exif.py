"""Batched EXIF / XMP read for ingest (PR 3). Writes land in PR 8.

Preferred path is one ``exiftool -j -fast -n`` process per 200 files. If
``exiftool`` is missing, fall back to Pillow / Piexif **per file** (JPEG only)
and skip HEIC GPS detection with a warning. RAW sibling ``.xmp`` GPS is merged
onto the RAW row when the sidecar exists.
"""

from __future__ import annotations

import json
import math
import re
import shutil
import subprocess
from collections.abc import Sequence
from dataclasses import dataclass, replace
from enum import Enum
from pathlib import Path
from typing import Any, Never

EXIFTOOL_CHUNK_SIZE = 200
EXIFTOOL_TIMEOUT_S = 120

EXIFTOOL_READ_ARGS = (
    "-j",
    "-fast",
    "-n",
    "-GPSLatitude",
    "-GPSLongitude",
    "-GPSLatitudeRef",
    "-GPSLongitudeRef",
    "-DateTimeOriginal",
    "-CreateDate",
    "-OffsetTimeOriginal",
    "-Make",
    "-Model",
    "-Orientation",
    "-GPSImgDirection",
    "-GPSAltitude",
)

JPEG_EXTS = frozenset({".jpg", ".jpeg"})
PNG_EXTS = frozenset({".png"})
HEIC_EXTS = frozenset({".heic", ".heif"})
RAW_EXTS = frozenset({".cr2", ".cr3", ".nef", ".arw", ".dng", ".raf", ".orf", ".rw2"})
XMP_EXTS = frozenset({".xmp"})

_XMP_LAT_ELEM = re.compile(
    r"<exif:GPSLatitude>\s*(-?[0-9]+(?:\.[0-9]+)?)\s*</exif:GPSLatitude>",
    re.IGNORECASE,
)
_XMP_LNG_ELEM = re.compile(
    r"<exif:GPSLongitude>\s*(-?[0-9]+(?:\.[0-9]+)?)\s*</exif:GPSLongitude>",
    re.IGNORECASE,
)
_XMP_LAT_ATTR = re.compile(
    r"exif:GPSLatitude\s*=\s*['\"](-?[0-9]+(?:\.[0-9]+)?)['\"]",
    re.IGNORECASE,
)
_XMP_LNG_ATTR = re.compile(
    r"exif:GPSLongitude\s*=\s*['\"](-?[0-9]+(?:\.[0-9]+)?)['\"]",
    re.IGNORECASE,
)

MISSING_EXIFTOOL_WARNING = (
    "exiftool not found; using Pillow/Piexif for JPEG EXIF only. "
    "HEIC GPS detection skipped."
)


class MediaKind(str, Enum):
    jpeg = "jpeg"
    png = "png"
    heic = "heic"
    raw = "raw"
    unsupported = "unsupported"


@dataclass(frozen=True)
class ExifRead:
    gps_lat: float | None = None
    gps_lng: float | None = None
    datetime_original: str | None = None
    create_date: str | None = None
    offset_time_original: str | None = None
    make: str | None = None
    model: str | None = None
    orientation: int | None = None
    gps_img_direction: float | None = None
    gps_altitude: float | None = None
    source: str = "none"

    @property
    def has_gps(self) -> bool:
        return _finite(self.gps_lat) and _finite(self.gps_lng)

    @property
    def capture_time(self) -> str | None:
        """DateTimeOriginal, falling back to CreateDate. Never file mtime."""
        return self.datetime_original or self.create_date


@dataclass(frozen=True)
class ExifBatchResult:
    by_path: dict[str, ExifRead]
    warnings: tuple[str, ...]
    used_exiftool: bool


def classify_media(path: Path) -> MediaKind:
    suffix = path.suffix.lower()
    if suffix in JPEG_EXTS:
        return MediaKind.jpeg
    if suffix in PNG_EXTS:
        return MediaKind.png
    if suffix in HEIC_EXTS:
        return MediaKind.heic
    if suffix in RAW_EXTS:
        return MediaKind.raw
    return MediaKind.unsupported


def is_processable(kind: MediaKind) -> bool:
    match kind:
        case MediaKind.jpeg | MediaKind.png | MediaKind.heic | MediaKind.raw:
            return True
        case MediaKind.unsupported:
            return False
        case _:
            _unreachable(kind)


def sidecar_xmp_path(path: Path) -> Path | None:
    """Return an existing sibling ``basename.xmp`` (any case), else None."""
    parent = path.parent
    stem = path.stem
    for candidate in (parent / f"{stem}.xmp", parent / f"{stem}.XMP"):
        if candidate.is_file() and candidate.resolve() != path.resolve():
            return candidate
    return None


def exiftool_available() -> bool:
    return shutil.which("exiftool") is not None


def read_exif_batch(paths: Sequence[Path]) -> ExifBatchResult:
    """Read EXIF/XMP for ``paths``. Keys in ``by_path`` are resolved path strings."""
    resolved = [path.resolve() for path in paths]
    query: list[Path] = []
    seen: set[str] = set()
    sidecar_for: dict[str, Path] = {}

    def _add(path: Path) -> None:
        key = str(path)
        if key not in seen:
            seen.add(key)
            query.append(path)

    for path in resolved:
        _add(path)
        if classify_media(path) is MediaKind.raw:
            sidecar = sidecar_xmp_path(path)
            if sidecar is not None:
                sidecar_for[str(path)] = sidecar.resolve()
                _add(sidecar.resolve())

    warnings: list[str] = []
    used_exiftool = exiftool_available()
    if used_exiftool:
        raw_map = _read_with_exiftool(query, warnings)
    else:
        warnings.append(MISSING_EXIFTOOL_WARNING)
        raw_map = _read_fallback(query)

    by_path: dict[str, ExifRead] = {}
    for path in resolved:
        key = str(path)
        tags = raw_map.get(key, ExifRead())
        sidecar = sidecar_for.get(key)
        if sidecar is not None:
            tags = _merge_sidecar(tags, raw_map.get(str(sidecar), ExifRead()))
        by_path[key] = tags
    return ExifBatchResult(
        by_path=by_path,
        warnings=tuple(warnings),
        used_exiftool=used_exiftool,
    )


def _read_with_exiftool(paths: list[Path], warnings: list[str]) -> dict[str, ExifRead]:
    result: dict[str, ExifRead] = {}
    for chunk in _chunks(paths, EXIFTOOL_CHUNK_SIZE):
        cmd = ["exiftool", *EXIFTOOL_READ_ARGS, "--", *[str(path) for path in chunk]]
        try:
            completed = subprocess.run(
                cmd,
                check=False,
                capture_output=True,
                text=True,
                timeout=EXIFTOOL_TIMEOUT_S,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            warnings.append(f"exiftool failed ({exc}); falling back for {len(chunk)} file(s)")
            result.update(_read_fallback(chunk))
            continue
        # 0 = ok; 1 = minor errors (still emits JSON for the files that worked).
        if completed.returncode not in (0, 1) or not completed.stdout.strip():
            warnings.append(
                f"exiftool exit {completed.returncode}; falling back for {len(chunk)} file(s)"
            )
            result.update(_read_fallback(chunk))
            continue
        try:
            payload = json.loads(completed.stdout)
        except json.JSONDecodeError:
            warnings.append("exiftool returned non-JSON output; falling back")
            result.update(_read_fallback(chunk))
            continue
        if not isinstance(payload, list):
            warnings.append("exiftool JSON was not a list; falling back")
            result.update(_read_fallback(chunk))
            continue
        parsed_files: set[str] = set()
        for item in payload:
            if not isinstance(item, dict):
                continue
            source = item.get("SourceFile")
            if not isinstance(source, str):
                continue
            key = str(Path(source))
            result[key] = _exif_from_exiftool_item(item)
            parsed_files.add(key)
        for path in chunk:
            key = str(path)
            if key not in parsed_files:
                result[key] = _read_fallback([path]).get(key, ExifRead())
    return result


def _read_fallback(paths: Sequence[Path]) -> dict[str, ExifRead]:
    result: dict[str, ExifRead] = {}
    for path in paths:
        kind = classify_media(path)
        match kind:
            case MediaKind.jpeg:
                result[str(path)] = _read_jpeg_piexif(path)
            case MediaKind.png:
                result[str(path)] = ExifRead(source="none")
            case MediaKind.heic:
                result[str(path)] = ExifRead(source="skipped_heic")
            case MediaKind.raw:
                result[str(path)] = ExifRead(source="none")
            case MediaKind.unsupported:
                if path.suffix.lower() in XMP_EXTS:
                    result[str(path)] = _read_xmp_sidecar(path)
                else:
                    result[str(path)] = ExifRead(source="none")
            case _:
                _unreachable(kind)
    return result


def _exif_from_exiftool_item(item: dict[str, Any]) -> ExifRead:
    lat = _as_float(item.get("GPSLatitude"))
    lng = _as_float(item.get("GPSLongitude"))
    lat = _apply_ref(lat, item.get("GPSLatitudeRef"), negative="S")
    lng = _apply_ref(lng, item.get("GPSLongitudeRef"), negative="W")
    orientation = item.get("Orientation")
    orientation_i: int | None = None
    if isinstance(orientation, int):
        orientation_i = orientation
    elif isinstance(orientation, float) and orientation.is_integer():
        orientation_i = int(orientation)
    return ExifRead(
        gps_lat=lat,
        gps_lng=lng,
        datetime_original=_as_str(item.get("DateTimeOriginal")),
        create_date=_as_str(item.get("CreateDate")),
        offset_time_original=_as_str(item.get("OffsetTimeOriginal")),
        make=_as_str(item.get("Make")),
        model=_as_str(item.get("Model")),
        orientation=orientation_i,
        gps_img_direction=_as_float(item.get("GPSImgDirection")),
        gps_altitude=_as_float(item.get("GPSAltitude")),
        source="exiftool",
    )


def _read_jpeg_piexif(path: Path) -> ExifRead:
    try:
        import piexif
    except ImportError:
        return ExifRead(source="none")
    try:
        exif_dict = piexif.load(str(path))
    except Exception:
        return ExifRead(source="none")
    zeroth = exif_dict.get("0th") or {}
    exif_ifd = exif_dict.get("Exif") or {}
    gps_ifd = exif_dict.get("GPS") or {}
    lat, lng = _gps_from_piexif(gps_ifd)
    orientation = zeroth.get(piexif.ImageIFD.Orientation)
    orientation_i = orientation if isinstance(orientation, int) else None
    offset = None
    if hasattr(piexif.ExifIFD, "OffsetTimeOriginal"):
        offset = _as_str(exif_ifd.get(piexif.ExifIFD.OffsetTimeOriginal))
    return ExifRead(
        gps_lat=lat,
        gps_lng=lng,
        datetime_original=_as_str(exif_ifd.get(piexif.ExifIFD.DateTimeOriginal)),
        create_date=_as_str(exif_ifd.get(piexif.ExifIFD.DateTimeDigitized)),
        offset_time_original=offset,
        make=_as_str(zeroth.get(piexif.ImageIFD.Make)),
        model=_as_str(zeroth.get(piexif.ImageIFD.Model)),
        orientation=orientation_i,
        source="piexif",
    )


def _read_xmp_sidecar(path: Path) -> ExifRead:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ExifRead(source="none")
    lat = _match_float(text, _XMP_LAT_ELEM, _XMP_LAT_ATTR)
    lng = _match_float(text, _XMP_LNG_ELEM, _XMP_LNG_ATTR)
    return ExifRead(gps_lat=lat, gps_lng=lng, source="xmp")


def _merge_sidecar(primary: ExifRead, sidecar: ExifRead) -> ExifRead:
    gps_lat = primary.gps_lat if _finite(primary.gps_lat) else sidecar.gps_lat
    gps_lng = primary.gps_lng if _finite(primary.gps_lng) else sidecar.gps_lng
    return replace(
        primary,
        gps_lat=gps_lat,
        gps_lng=gps_lng,
        datetime_original=primary.datetime_original or sidecar.datetime_original,
        create_date=primary.create_date or sidecar.create_date,
        offset_time_original=primary.offset_time_original or sidecar.offset_time_original,
        make=primary.make or sidecar.make,
        model=primary.model or sidecar.model,
        source=primary.source if primary.source != "none" else sidecar.source,
    )


def _gps_from_piexif(gps_ifd: dict[int, Any]) -> tuple[float | None, float | None]:
    if not gps_ifd:
        return None, None
    try:
        import piexif
    except ImportError:
        return None, None
    lat_raw = gps_ifd.get(piexif.GPSIFD.GPSLatitude)
    lng_raw = gps_ifd.get(piexif.GPSIFD.GPSLongitude)
    if lat_raw is None or lng_raw is None:
        return None, None
    try:
        lat = _dms_to_deg(lat_raw)
        lng = _dms_to_deg(lng_raw)
    except (TypeError, ValueError, ZeroDivisionError):
        return None, None
    lat_ref = _as_str(gps_ifd.get(piexif.GPSIFD.GPSLatitudeRef)) or "N"
    lng_ref = _as_str(gps_ifd.get(piexif.GPSIFD.GPSLongitudeRef)) or "E"
    if lat_ref.upper().startswith("S"):
        lat = -abs(lat)
    if lng_ref.upper().startswith("W"):
        lng = -abs(lng)
    if not _finite(lat) or not _finite(lng):
        return None, None
    return lat, lng


def _dms_to_deg(value: Any) -> float:
    degrees, minutes, seconds = value
    return _ratio(degrees) + _ratio(minutes) / 60.0 + _ratio(seconds) / 3600.0


def _ratio(item: Any) -> float:
    if isinstance(item, (tuple, list)) and len(item) == 2:
        num, den = item
        return float(num) / float(den)
    return float(item)


def _apply_ref(coord: float | None, ref: Any, *, negative: str) -> float | None:
    if coord is None:
        return None
    text = _as_str(ref)
    if text is None:
        return coord
    # exiftool -n already signs coordinates; don't double-apply if already negative.
    if text.upper().startswith(negative) and coord > 0:
        return -coord
    return coord


def _as_float(value: Any) -> float | None:
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        number = float(value)
        return number if math.isfinite(number) else None
    if isinstance(value, str):
        try:
            number = float(value.strip())
        except ValueError:
            return None
        return number if math.isfinite(number) else None
    return None


def _as_str(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, bytes):
        text = value.decode("utf-8", errors="replace")
    else:
        text = str(value)
    text = text.strip().strip("\x00").strip()
    return text or None


def _finite(value: float | None) -> bool:
    return value is not None and math.isfinite(value)


def _match_float(text: str, *patterns: re.Pattern[str]) -> float | None:
    for pattern in patterns:
        match = pattern.search(text)
        if match is not None:
            return _as_float(match.group(1))
    return None


def _chunks(items: list[Path], size: int) -> list[list[Path]]:
    return [items[index : index + size] for index in range(0, len(items), size)]


def _unreachable(value: Never) -> Never:
    raise RuntimeError(f"unhandled media kind: {value!r}")
