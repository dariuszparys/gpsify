from __future__ import annotations

import asyncio
import io
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar

from PIL import Image
import pytest

from gpsify.exif import MediaKind
from gpsify.ingest import run_ingest
from gpsify.media import (
    PREVIEW_LONG_EDGE,
    THUMB_LONG_EDGE,
    MediaError,
    ensure_preview,
    ensure_thumb,
    hash_file,
    preview_path,
    render_preview,
    render_thumb,
    thumb_path,
)
from gpsify.store.db import job_dir
from gpsify.types import PhotoStatus
from jpeg_factory import write_test_jpeg

T = TypeVar("T")


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


def _jpeg_size(blob: bytes) -> tuple[int, int]:
    with Image.open(io.BytesIO(blob)) as image:
        return image.size


def test_exif_transpose_orientation_6(tmp_path: Path) -> None:
    path = tmp_path / "sideways.jpg"
    write_test_jpeg(path, size=(4, 8), color=(200, 10, 10), orientation=6)
    digest = hash_file(path)
    before = path.read_bytes()
    jpeg = render_preview(path, MediaKind.jpeg, expected_sha256=digest)
    width, height = _jpeg_size(jpeg)
    assert (width, height) == (8, 4)
    assert path.read_bytes() == before
    assert hash_file(path) == digest


def test_preview_downscales_long_edge(tmp_path: Path) -> None:
    path = tmp_path / "wide.jpg"
    write_test_jpeg(path, size=(1600, 800), color=(20, 80, 160))
    jpeg = render_preview(path, expected_sha256=hash_file(path))
    width, height = _jpeg_size(jpeg)
    assert max(width, height) == PREVIEW_LONG_EDGE
    assert width == PREVIEW_LONG_EDGE
    assert height == 640


def test_thumb_downscales_long_edge(tmp_path: Path) -> None:
    path = tmp_path / "wide.jpg"
    write_test_jpeg(path, size=(1600, 800), color=(20, 80, 160))
    jpeg = render_thumb(path, expected_sha256=hash_file(path))
    width, height = _jpeg_size(jpeg)
    assert max(width, height) == THUMB_LONG_EDGE
    assert (width, height) == (256, 128)


def test_png_preview_does_not_mutate_source(tmp_path: Path) -> None:
    path = tmp_path / "plain.png"
    Image.new("RGB", (32, 16), (8, 9, 10)).save(path, "PNG")
    before = path.read_bytes()
    digest = hash_file(path)
    jpeg = render_preview(path, MediaKind.png, expected_sha256=digest)
    assert jpeg[:2] == b"\xff\xd8"
    assert path.read_bytes() == before
    assert hash_file(path) == digest


def test_source_sha256_mismatch_refuses(tmp_path: Path) -> None:
    path = tmp_path / "a.jpg"
    write_test_jpeg(path)
    with pytest.raises(MediaError) as exc:
        render_preview(path, expected_sha256="0" * 64)
    assert exc.value.status is PhotoStatus.error
    assert "sha256" in str(exc.value).lower()


def test_truncated_jpeg_is_unreadable(tmp_path: Path) -> None:
    path = tmp_path / "bad.jpg"
    path.write_bytes(b"not-a-jpeg")
    with pytest.raises(MediaError) as exc:
        render_preview(path, MediaKind.jpeg, expected_sha256=hash_file(path))
    assert exc.value.status is PhotoStatus.skipped_unreadable


def test_ingest_does_not_write_previews_or_thumbs(tmp_path: Path) -> None:
    data_dir = tmp_path / "gpsify-data"
    data_dir.mkdir()
    folder = tmp_path / "album"
    folder.mkdir()
    write_test_jpeg(folder / "a.jpg", datetime_original="2019:06:14 18:00:00")
    summary = run(run_ingest(folder, data_dir=data_dir))
    root = job_dir(data_dir, summary.job_id)
    assert not (root / "previews").exists()
    assert not (root / "thumbs").exists()


def test_ensure_preview_and_thumb_are_lazy(tmp_path: Path) -> None:
    data_dir = tmp_path / "gpsify-data"
    data_dir.mkdir()
    folder = tmp_path / "album"
    folder.mkdir()
    source = write_test_jpeg(
        folder / "a.jpg", size=(400, 200), datetime_original="2019:06:14 18:00:00"
    )
    before = source.read_bytes()
    digest = hash_file(source)
    summary = run(run_ingest(folder, data_dir=data_dir))
    root = job_dir(data_dir, summary.job_id)
    photo_id = "photo-lazy"

    preview = ensure_preview(
        root, photo_id, source, expected_sha256=digest, kind=MediaKind.jpeg
    )
    assert preview == preview_path(root, photo_id)
    assert preview.is_file()
    assert not thumb_path(root, photo_id).exists()
    with Image.open(preview) as image:
        assert max(image.size) <= PREVIEW_LONG_EDGE

    thumb = ensure_thumb(
        root, photo_id, source, expected_sha256=digest, kind=MediaKind.jpeg
    )
    assert thumb == thumb_path(root, photo_id)
    assert thumb.is_file()
    with Image.open(thumb) as image:
        assert max(image.size) <= THUMB_LONG_EDGE

    assert source.read_bytes() == before
    assert hash_file(source) == digest
    again = ensure_preview(
        root, photo_id, source, expected_sha256=digest, kind=MediaKind.jpeg
    )
    assert again == preview
