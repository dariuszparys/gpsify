"""Batched EXIF / XMP read and GPS write.

Read (PR 3): one ``exiftool -j -fast -n`` process per 200 files. If
``exiftool`` is missing, fall back to Pillow / Piexif **per file** (JPEG only)
and skip HEIC GPS detection with a warning. RAW sibling ``.xmp`` GPS is merged
onto the RAW row when the sidecar exists.

Write (PR 8): one ``exiftool -@ argfile`` of per-dest tag sets. JPEG/HEIC get
EXIF+XMP; PNG gets XMP in-file; RAW gets a sibling ``.xmp`` only. Piexif is
the JPEG-only test fallback; PNG/RAW XMP is written directly when exiftool is
missing. ``GPSIFY_DRY_RUN=1`` blocks writes.
"""

from __future__ import annotations

import json
import math
import os
import re
import shutil
import subprocess
import tempfile
from collections.abc import Sequence
from dataclasses import dataclass, replace
from enum import Enum
from pathlib import Path
from typing import Any, Never
from xml.sax.saxutils import escape as xml_escape

import piexif
from PIL import Image, PngImagePlugin

EXIFTOOL_CHUNK_SIZE = 200
EXIFTOOL_TIMEOUT_S = 120
EXIFTOOL_WRITE_TIMEOUT_S = 600
GPS_MATCH_DEG = 1e-5

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
HEIC_WRITE_REQUIRES_EXIFTOOL = "HEIC write requires exiftool"
DRY_RUN_BLOCKS_WRITES = "GPSIFY_DRY_RUN=1 blocks exiftool writes"


class MediaKind(str, Enum):
    jpeg = "jpeg"
    png = "png"
    heic = "heic"
    raw = "raw"
    unsupported = "unsupported"


class ExifWriteError(RuntimeError):
    """GPS write refused or failed before any dest was mutated."""


@dataclass(frozen=True)
class GpsWrite:
    dest: Path
    lat: float
    lng: float
    kind: MediaKind
    job_id: str
    decision_id: str


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
                result[str(path)] = _read_png_xmp(path)
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


def writes_blocked() -> bool:
    return os.environ.get("GPSIFY_DRY_RUN", "") == "1"


def angular_distance_deg(lat1: float, lng1: float, lat2: float, lng2: float) -> float:
    """Haversine central angle in degrees."""
    phi1, phi2 = math.radians(lat1), math.radians(lat2)
    dphi = math.radians(lat2 - lat1)
    dlmb = math.radians(lng2 - lng1)
    chord = (
        math.sin(dphi / 2) ** 2
        + math.cos(phi1) * math.cos(phi2) * math.sin(dlmb / 2) ** 2
    )
    return math.degrees(2 * math.asin(min(1.0, math.sqrt(chord))))


def gps_matches(
    lat1: float | None,
    lng1: float | None,
    lat2: float,
    lng2: float,
    *,
    max_deg: float = GPS_MATCH_DEG,
) -> bool:
    if not _finite(lat1) or not _finite(lng1):
        return False
    assert lat1 is not None and lng1 is not None
    return angular_distance_deg(lat1, lng1, lat2, lng2) < max_deg


def read_dest_gps(path: Path) -> tuple[float | None, float | None]:
    """Read GPS from an image dest or an XMP sidecar."""
    if not path.is_file():
        return None, None
    result = read_exif_batch([path])
    tags = result.by_path.get(str(path.resolve()), ExifRead())
    return tags.gps_lat, tags.gps_lng


def dest_gps_matches(path: Path, lat: float, lng: float) -> bool:
    got_lat, got_lng = read_dest_gps(path)
    return gps_matches(got_lat, got_lng, lat, lng)


def gps_instructions(job_id: str, decision_id: str) -> str:
    return f"Geotagged by gpsify job {job_id} decision {decision_id}"


def render_exiftool_argfile(items: Sequence[GpsWrite]) -> str:
    """Would-be ``exiftool -@`` body. One ``-execute`` block per dest."""
    blocks: list[str] = []
    for item in items:
        blocks.append("\n".join(_argfile_lines(item)))
    return "\n-execute\n".join(blocks) + ("\n" if blocks else "")


def write_gps_batch(items: Sequence[GpsWrite]) -> dict[str, str | None]:
    """Write GPS tags. Returns dest path string → error (``None`` on GPS verify)."""
    if writes_blocked():
        raise ExifWriteError(DRY_RUN_BLOCKS_WRITES)
    if not items:
        return {}
    if exiftool_available():
        _write_with_exiftool(items)
    else:
        _write_fallback(items)
    errors: dict[str, str | None] = {}
    for item in items:
        key = str(item.dest)
        if dest_gps_matches(item.dest, item.lat, item.lng):
            errors[key] = None
        else:
            errors[key] = "gps_verify_failed"
    return errors


def _argfile_lines(item: GpsWrite) -> list[str]:
    lat_s = _fmt_coord(item.lat)
    lng_s = _fmt_coord(item.lng)
    instructions = gps_instructions(item.job_id, item.decision_id)
    lines = ["-n"]
    match item.kind:
        case MediaKind.jpeg | MediaKind.heic:
            lines.extend(
                [
                    f"-GPSLatitude={lat_s}",
                    f"-GPSLatitudeRef={_lat_ref(item.lat)}",
                    f"-GPSLongitude={lng_s}",
                    f"-GPSLongitudeRef={_lng_ref(item.lng)}",
                    "-GPSMapDatum=WGS-84",
                    "-GPSVersionID=2 3 0 0",
                    f"-XMP:GPSLatitude={lat_s}",
                    f"-XMP:GPSLongitude={lng_s}",
                    "-XMP-dc:Source=gpsify",
                    f"-XMP-photoshop:Instructions={instructions}",
                    "-overwrite_original",
                    str(item.dest),
                ]
            )
        case MediaKind.png:
            lines.extend(
                [
                    f"-XMP:GPSLatitude={lat_s}",
                    f"-XMP:GPSLongitude={lng_s}",
                    "-XMP-dc:Source=gpsify",
                    f"-XMP-photoshop:Instructions={instructions}",
                    "-overwrite_original",
                    str(item.dest),
                ]
            )
        case MediaKind.raw:
            lines.extend(
                [
                    f"-XMP:GPSLatitude={lat_s}",
                    f"-XMP:GPSLongitude={lng_s}",
                    "-XMP-dc:Source=gpsify",
                    f"-XMP-photoshop:Instructions={instructions}",
                    "-overwrite_original",
                    str(item.dest),
                ]
            )
        case MediaKind.unsupported:
            raise ExifWriteError(f"cannot write GPS for {item.dest}")
        case _:
            _unreachable(item.kind)
    return lines


def _write_with_exiftool(items: Sequence[GpsWrite]) -> None:
    for item in items:
        if item.kind is MediaKind.raw and not item.dest.is_file():
            _write_xmp_sidecar(
                item.dest, item.lat, item.lng, item.job_id, item.decision_id
            )
    body = render_exiftool_argfile(items)
    handle = tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        suffix=".args",
        delete=False,
    )
    try:
        handle.write(body)
        handle.close()
        completed = subprocess.run(
            ["exiftool", "-@", handle.name],
            check=False,
            capture_output=True,
            text=True,
            timeout=EXIFTOOL_WRITE_TIMEOUT_S,
        )
        if completed.returncode not in (0, 1):
            raise ExifWriteError(
                f"exiftool write exit {completed.returncode}: {completed.stderr.strip()}"
            )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise ExifWriteError(f"exiftool write failed: {exc}") from exc
    finally:
        Path(handle.name).unlink(missing_ok=True)


def _write_fallback(items: Sequence[GpsWrite]) -> None:
    for item in items:
        try:
            match item.kind:
                case MediaKind.jpeg:
                    _write_jpeg_piexif(item.dest, item.lat, item.lng)
                case MediaKind.png:
                    _write_png_xmp(item.dest, item.lat, item.lng, item.job_id, item.decision_id)
                case MediaKind.raw:
                    _write_xmp_sidecar(item.dest, item.lat, item.lng, item.job_id, item.decision_id)
                case MediaKind.heic:
                    continue
                case MediaKind.unsupported:
                    continue
                case _:
                    _unreachable(item.kind)
        except Exception:
            continue


def _write_jpeg_piexif(path: Path, lat: float, lng: float) -> None:
    try:
        exif_dict = piexif.load(str(path))
    except Exception:
        exif_dict = {"0th": {}, "Exif": {}, "GPS": {}, "1st": {}, "thumbnail": None}
    gps_ifd = dict(exif_dict.get("GPS") or {})
    gps_ifd[piexif.GPSIFD.GPSVersionID] = (2, 3, 0, 0)
    gps_ifd[piexif.GPSIFD.GPSLatitudeRef] = _lat_ref(lat)
    gps_ifd[piexif.GPSIFD.GPSLatitude] = _deg_to_dms(abs(lat))
    gps_ifd[piexif.GPSIFD.GPSLongitudeRef] = _lng_ref(lng)
    gps_ifd[piexif.GPSIFD.GPSLongitude] = _deg_to_dms(abs(lng))
    gps_ifd[piexif.GPSIFD.GPSMapDatum] = "WGS-84"
    exif_dict["GPS"] = gps_ifd
    piexif.insert(piexif.dump(exif_dict), str(path))


def _write_png_xmp(path: Path, lat: float, lng: float, job_id: str, decision_id: str) -> None:
    packet = _xmp_packet(lat, lng, gps_instructions(job_id, decision_id))
    with Image.open(path) as image:
        converted = image.convert("RGB")
        info = PngImagePlugin.PngInfo()
        for key, value in converted.info.items():
            if isinstance(value, str) and key != "XML:com.adobe.xmp":
                info.add_text(key, value)
        info.add_text("XML:com.adobe.xmp", packet)
        converted.save(path, "PNG", pnginfo=info)


def _write_xmp_sidecar(
    path: Path, lat: float, lng: float, job_id: str, decision_id: str
) -> None:
    instructions = gps_instructions(job_id, decision_id)
    if path.is_file():
        text = path.read_text(encoding="utf-8", errors="replace")
        merged = _merge_xmp_gps(text, lat, lng, instructions)
    else:
        merged = _xmp_packet(lat, lng, instructions)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".gpsify-tmp")
    tmp.write_text(merged, encoding="utf-8")
    tmp.replace(path)


def _read_png_xmp(path: Path) -> ExifRead:
    try:
        with Image.open(path) as image:
            xmp = image.info.get("XML:com.adobe.xmp") or image.info.get("xmp")
        if isinstance(xmp, bytes):
            xmp = xmp.decode("utf-8", errors="replace")
        if isinstance(xmp, str) and xmp.strip():
            lat = _match_float(xmp, _XMP_LAT_ELEM, _XMP_LAT_ATTR)
            lng = _match_float(xmp, _XMP_LNG_ELEM, _XMP_LNG_ATTR)
            return ExifRead(gps_lat=lat, gps_lng=lng, source="xmp")
    except Exception:
        pass
    try:
        text = path.read_bytes().decode("latin-1", errors="replace")
    except OSError:
        return ExifRead(source="none")
    lat = _match_float(text, _XMP_LAT_ELEM, _XMP_LAT_ATTR)
    lng = _match_float(text, _XMP_LNG_ELEM, _XMP_LNG_ATTR)
    if lat is None and lng is None:
        return ExifRead(source="none")
    return ExifRead(gps_lat=lat, gps_lng=lng, source="xmp")


def _xmp_packet(lat: float, lng: float, instructions: str) -> str:
    lat_s = _fmt_coord(lat)
    lng_s = _fmt_coord(lng)
    inst = xml_escape(instructions, entities={'"': "&quot;"})
    return (
        '<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>\n'
        '<x:xmpmeta xmlns:x="adobe:ns:meta/">\n'
        ' <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">\n'
        '  <rdf:Description rdf:about=""\n'
        '   xmlns:exif="http://ns.adobe.com/exif/1.0/"\n'
        '   xmlns:dc="http://purl.org/dc/elements/1.1/"\n'
        '   xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"\n'
        f'   exif:GPSLatitude="{lat_s}"\n'
        f'   exif:GPSLongitude="{lng_s}"\n'
        '   dc:Source="gpsify"\n'
        f'   photoshop:Instructions="{inst}"/>\n'
        " </rdf:RDF>\n"
        "</x:xmpmeta>\n"
        '<?xpacket end="w"?>\n'
    )


def _merge_xmp_gps(text: str, lat: float, lng: float, instructions: str) -> str:
    if not text.strip():
        return _xmp_packet(lat, lng, instructions)
    lat_s = _fmt_coord(lat)
    lng_s = _fmt_coord(lng)
    inst = xml_escape(instructions, entities={'"': "&quot;"})
    updated, nlat = _XMP_LAT_ELEM.subn(
        f"<exif:GPSLatitude>{lat_s}</exif:GPSLatitude>", text, count=1
    )
    if nlat == 0:
        updated, nlat = _XMP_LAT_ATTR.subn(f'exif:GPSLatitude="{lat_s}"', updated, count=1)
    updated, nlng = _XMP_LNG_ELEM.subn(
        f"<exif:GPSLongitude>{lng_s}</exif:GPSLongitude>", updated, count=1
    )
    if nlng == 0:
        updated, nlng = _XMP_LNG_ATTR.subn(f'exif:GPSLongitude="{lng_s}"', updated, count=1)
    if nlat == 0 or nlng == 0:
        insertion = (
            f' exif:GPSLatitude="{lat_s}" exif:GPSLongitude="{lng_s}" '
            f'dc:Source="gpsify" photoshop:Instructions="{inst}"'
        )
        if "rdf:Description" in updated:
            updated = updated.replace("<rdf:Description", f"<rdf:Description{insertion}", 1)
        else:
            return _xmp_packet(lat, lng, instructions)
    return updated


def _fmt_coord(value: float) -> str:
    return f"{value:.8f}".rstrip("0").rstrip(".")


def _lat_ref(lat: float) -> str:
    return "N" if lat >= 0 else "S"


def _lng_ref(lng: float) -> str:
    return "E" if lng >= 0 else "W"


def _deg_to_dms(deg: float) -> tuple[tuple[int, int], tuple[int, int], tuple[int, int]]:
    degrees = int(deg)
    minutes_float = (deg - degrees) * 60
    minutes = int(minutes_float)
    seconds = (minutes_float - minutes) * 60
    return (degrees, 1), (minutes, 1), (int(round(seconds * 10000)), 10000)
