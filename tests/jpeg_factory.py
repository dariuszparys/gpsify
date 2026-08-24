"""Tiny JPEG writers for tests and checked-in fixtures. Never writes GPS to originals."""

from __future__ import annotations

from pathlib import Path

import piexif
from PIL import Image

PORTO_LAT = 41.1456
PORTO_LNG = -8.6143


def _dms_rational(deg: float) -> tuple[tuple[int, int], tuple[int, int], tuple[int, int]]:
    degrees = int(deg)
    minutes_float = (deg - degrees) * 60
    minutes = int(minutes_float)
    seconds = (minutes_float - minutes) * 60
    return (degrees, 1), (minutes, 1), (int(round(seconds * 10000)), 10000)


def write_test_jpeg(
    path: Path,
    *,
    gps: tuple[float, float] | None = None,
    datetime_original: str | None = None,
    create_date: str | None = None,
    make: str = "Canon",
    model: str = "EOS 6D",
    size: tuple[int, int] = (8, 8),
    color: tuple[int, int, int] = (30, 80, 140),
    orientation: int | None = None,
) -> Path:
    """Write a tiny JPEG. ``gps`` is signed decimal (lat, lng)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    image = Image.new("RGB", size, color)
    zeroth: dict[int, object] = {
        piexif.ImageIFD.Make: make,
        piexif.ImageIFD.Model: model,
        piexif.ImageIFD.Software: "gpsify-test",
    }
    if orientation is not None:
        zeroth[piexif.ImageIFD.Orientation] = orientation
    exif_ifd: dict[int, object] = {}
    gps_ifd: dict[int, object] = {}
    if datetime_original is not None:
        exif_ifd[piexif.ExifIFD.DateTimeOriginal] = datetime_original
        zeroth[piexif.ImageIFD.DateTime] = datetime_original
    if create_date is not None:
        exif_ifd[piexif.ExifIFD.DateTimeDigitized] = create_date
    if gps is not None:
        lat, lng = gps
        gps_ifd[piexif.GPSIFD.GPSVersionID] = (2, 3, 0, 0)
        gps_ifd[piexif.GPSIFD.GPSLatitudeRef] = "N" if lat >= 0 else "S"
        gps_ifd[piexif.GPSIFD.GPSLatitude] = _dms_rational(abs(lat))
        gps_ifd[piexif.GPSIFD.GPSLongitudeRef] = "E" if lng >= 0 else "W"
        gps_ifd[piexif.GPSIFD.GPSLongitude] = _dms_rational(abs(lng))
    dumped = piexif.dump({"0th": zeroth, "Exif": exif_ifd, "GPS": gps_ifd})
    image.save(path, "JPEG", quality=85, exif=dumped)
    return path


def write_checked_in_fixtures(fixtures_dir: Path) -> None:
    write_test_jpeg(
        fixtures_dir / "jpeg_with_gps.jpg",
        gps=(PORTO_LAT, PORTO_LNG),
        datetime_original="2019:06:14 18:02:00",
        color=(180, 40, 40),
    )
    write_test_jpeg(
        fixtures_dir / "jpeg_no_gps.jpg",
        datetime_original="2019:06:14 18:04:00",
        color=(40, 120, 180),
    )
