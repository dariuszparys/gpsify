from __future__ import annotations

import io
from collections.abc import Callable
from pathlib import Path
from typing import Any

from PIL import Image
import pytest

from gpsify.exif import MediaKind, exiftool_available
from gpsify.media import (
    MIN_RAW_PREVIEW_EDGE,
    RAW_PREVIEW_TAGS,
    MediaError,
    extract_raw_preview_jpeg,
    hash_file,
    render_preview,
)
from gpsify.types import PhotoStatus
from jpeg_factory import write_test_jpeg


def _jpeg_bytes(size: tuple[int, int], color: tuple[int, int, int] = (9, 10, 11)) -> bytes:
    buffer = io.BytesIO()
    Image.new("RGB", size, color).save(buffer, format="JPEG", quality=85)
    return buffer.getvalue()


def test_raw_preview_tag_order(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    raw = tmp_path / "IMG_0001.CR2"
    raw.write_bytes(b"SENSOR-DUMP-NOT-A-JPEG")
    calls: list[str] = []
    blobs = {
        "JpgFromRaw": b"",
        "PreviewImage": _jpeg_bytes((80, 60)),  # too small
        "OtherImage": _jpeg_bytes((400, 300), (200, 10, 10)),
        "ThumbnailImage": _jpeg_bytes((512, 512), (10, 200, 10)),
    }

    def fake_extract(_path: Path, tag: str) -> bytes:
        calls.append(tag)
        return blobs[tag]

    monkeypatch.setattr("gpsify.media.exiftool_available", lambda: True)
    monkeypatch.setattr("gpsify.media._exiftool_binary_tag", fake_extract)

    chosen = extract_raw_preview_jpeg(raw)
    assert calls == ["JpgFromRaw", "PreviewImage", "OtherImage"]
    assert chosen == blobs["OtherImage"]
    assert tuple(RAW_PREVIEW_TAGS) == (
        "JpgFromRaw",
        "PreviewImage",
        "OtherImage",
        "ThumbnailImage",
    )


def test_raw_thumbnail_used_only_as_last_resort(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    raw = tmp_path / "body.nef"
    raw.write_bytes(b"SENSOR-DUMP")
    thumb = _jpeg_bytes((320, 240), (11, 22, 33))

    def fake_extract(_path: Path, tag: str) -> bytes:
        if tag == "ThumbnailImage":
            return thumb
        return b""

    monkeypatch.setattr("gpsify.media.exiftool_available", lambda: True)
    monkeypatch.setattr("gpsify.media._exiftool_binary_tag", fake_extract)
    assert extract_raw_preview_jpeg(raw) == thumb


def test_raw_too_small_is_skipped_no_preview(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    raw = tmp_path / "tiny.arw"
    raw.write_bytes(b"SENSOR-DUMP")
    small = _jpeg_bytes((64, 48))

    monkeypatch.setattr("gpsify.media.exiftool_available", lambda: True)
    monkeypatch.setattr(
        "gpsify.media._exiftool_binary_tag",
        lambda _path, _tag: small,
    )
    with pytest.raises(MediaError) as exc:
        extract_raw_preview_jpeg(raw)
    assert exc.value.status is PhotoStatus.skipped_no_preview
    assert str(MIN_RAW_PREVIEW_EDGE) in str(exc.value)


def test_raw_without_exiftool_is_skipped_no_preview(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    raw = tmp_path / "body.dng"
    raw.write_bytes(b"SENSOR-DUMP")
    monkeypatch.setattr("gpsify.media.exiftool_available", lambda: False)
    with pytest.raises(MediaError) as exc:
        render_preview(raw, MediaKind.raw, expected_sha256=hash_file(raw))
    assert exc.value.status is PhotoStatus.skipped_no_preview


def test_raw_never_opens_container(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    raw = tmp_path / "body.cr2"
    raw.write_bytes(b"SENSOR-DUMP-NOT-A-JPEG")
    embedded = _jpeg_bytes((400, 300), (40, 50, 200))
    monkeypatch.setattr("gpsify.media.exiftool_available", lambda: True)
    monkeypatch.setattr(
        "gpsify.media._exiftool_binary_tag",
        lambda _path, tag: embedded if tag == "JpgFromRaw" else b"",
    )

    opened_paths: list[object] = []
    real_open: Callable[..., Image.Image] = Image.open

    def tracking_open(fp: object, *args: Any, **kwargs: Any) -> Image.Image:
        opened_paths.append(fp)
        return real_open(fp, *args, **kwargs)

    monkeypatch.setattr("gpsify.media.Image.open", tracking_open)
    jpeg = render_preview(raw, MediaKind.raw, expected_sha256=hash_file(raw))
    assert jpeg[:2] == b"\xff\xd8"
    assert raw.read_bytes() == b"SENSOR-DUMP-NOT-A-JPEG"
    for fp in opened_paths:
        if isinstance(fp, (str, Path)):
            assert Path(fp).resolve() != raw.resolve()
            assert Path(fp).suffix.lower() != ".cr2"


@pytest.mark.skipif(not exiftool_available(), reason="exiftool not installed")
def test_exiftool_b_on_jpeg_yields_no_raw_preview(tmp_path: Path) -> None:
    """Live exiftool: a JPEG named like RAW has no embedded RAW preview tags."""
    rawish = tmp_path / "not-really.cr2"
    write_test_jpeg(rawish, size=(32, 32))
    with pytest.raises(MediaError) as exc:
        extract_raw_preview_jpeg(rawish)
    assert exc.value.status is PhotoStatus.skipped_no_preview
