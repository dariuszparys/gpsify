from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from gpsify.exif import (
    EXIFTOOL_CHUNK_SIZE,
    EXIFTOOL_READ_ARGS,
    MISSING_EXIFTOOL_WARNING,
    classify_media,
    MediaKind,
    read_exif_batch,
)
from jpeg_factory import PORTO_LAT, PORTO_LNG, write_test_jpeg

FIXTURES = Path(__file__).parent / "fixtures"


def test_classify_media_extensions() -> None:
    assert classify_media(Path("a.JPG")) is MediaKind.jpeg
    assert classify_media(Path("a.jpeg")) is MediaKind.jpeg
    assert classify_media(Path("a.png")) is MediaKind.png
    assert classify_media(Path("a.heic")) is MediaKind.heic
    assert classify_media(Path("a.HEIF")) is MediaKind.heic
    assert classify_media(Path("a.cr2")) is MediaKind.raw
    assert classify_media(Path("a.CR3")) is MediaKind.raw
    assert classify_media(Path("a.nef")) is MediaKind.raw
    assert classify_media(Path("a.arw")) is MediaKind.raw
    assert classify_media(Path("a.dng")) is MediaKind.raw
    assert classify_media(Path("a.mov")) is MediaKind.unsupported


def test_piexif_fallback_reads_checked_in_jpegs(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("gpsify.exif.shutil.which", lambda _name: None)
    result = read_exif_batch(
        [FIXTURES / "jpeg_with_gps.jpg", FIXTURES / "jpeg_no_gps.jpg"]
    )
    assert result.used_exiftool is False
    assert MISSING_EXIFTOOL_WARNING in result.warnings
    tagged = result.by_path[str((FIXTURES / "jpeg_with_gps.jpg").resolve())]
    plain = result.by_path[str((FIXTURES / "jpeg_no_gps.jpg").resolve())]
    assert tagged.has_gps is True
    assert tagged.gps_lat == pytest.approx(PORTO_LAT, abs=1e-4)
    assert tagged.gps_lng == pytest.approx(PORTO_LNG, abs=1e-4)
    assert tagged.datetime_original == "2019:06:14 18:02:00"
    assert tagged.source == "piexif"
    assert plain.has_gps is False
    assert plain.datetime_original == "2019:06:14 18:04:00"


def test_raw_sibling_xmp_gps_without_exiftool(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("gpsify.exif.shutil.which", lambda _name: None)
    raw = tmp_path / "IMG_0001.CR2"
    raw.write_bytes(b"not-a-real-raw")
    (tmp_path / "IMG_0001.xmp").write_text(
        """<?xml version="1.0"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/"
      exif:GPSLatitude="41.1456"
      exif:GPSLongitude="-8.6143"/>
  </rdf:RDF>
</x:xmpmeta>
""",
        encoding="utf-8",
    )
    result = read_exif_batch([raw])
    tags = result.by_path[str(raw.resolve())]
    assert tags.has_gps is True
    assert tags.gps_lat == pytest.approx(41.1456)
    assert tags.gps_lng == pytest.approx(-8.6143)
    assert tags.source == "xmp"


def test_exiftool_command_and_chunk_size(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[list[str]] = []

    class Completed:
        def __init__(self, files: list[str]) -> None:
            self.returncode = 0
            self.stdout = json.dumps(
                [{"SourceFile": path, "GPSLatitude": 1.0, "GPSLongitude": 2.0} for path in files]
            )
            self.stderr = ""

    def fake_run(cmd: list[str], **_kwargs: Any) -> Completed:
        calls.append(cmd)
        files = cmd[cmd.index("--") + 1 :]
        return Completed(files)

    monkeypatch.setattr("gpsify.exif.shutil.which", lambda _name: "/usr/bin/exiftool")
    monkeypatch.setattr("gpsify.exif.subprocess.run", fake_run)

    paths = []
    for index in range(EXIFTOOL_CHUNK_SIZE + 1):
        path = tmp_path / f"{index:03d}.jpg"
        write_test_jpeg(path, datetime_original="2019:06:14 18:00:00")
        paths.append(path)

    result = read_exif_batch(paths)
    assert result.used_exiftool is True
    assert len(calls) == 2
    first_files = calls[0][calls[0].index("--") + 1 :]
    second_files = calls[1][calls[1].index("--") + 1 :]
    assert len(first_files) == EXIFTOOL_CHUNK_SIZE
    assert len(second_files) == 1
    assert calls[0][:2] == ["exiftool", "-j"]
    for flag in EXIFTOOL_READ_ARGS:
        assert flag in calls[0]
    assert "-fast" in calls[0] and "-n" in calls[0]
    tagged = result.by_path[str(paths[0].resolve())]
    assert tagged.has_gps is True
    assert tagged.source == "exiftool"


def test_heic_without_exiftool_has_no_gps(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("gpsify.exif.shutil.which", lambda _name: None)
    heic = tmp_path / "IMG_1.heic"
    heic.write_bytes(b"ftypheic")
    result = read_exif_batch([heic])
    tags = result.by_path[str(heic.resolve())]
    assert tags.has_gps is False
    assert tags.source == "skipped_heic"
