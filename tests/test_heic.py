from __future__ import annotations

import importlib.util
import io
from pathlib import Path

from PIL import Image
import pytest

from gpsify.exif import MediaKind
from gpsify.media import (
    HEIC_INSTALL_HINT,
    MediaError,
    hash_file,
    heic_plugin_available,
    render_preview,
)
from gpsify.types import PhotoStatus


def test_heic_without_plugin_is_skipped_unsupported(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("gpsify.media.pillow_heif", None)
    path = tmp_path / "live.heic"
    path.write_bytes(b"ftypheic")
    with pytest.raises(MediaError) as exc:
        render_preview(path, MediaKind.heic, expected_sha256=hash_file(path))
    assert exc.value.status is PhotoStatus.skipped_unsupported
    assert "pillow-heif" in str(exc.value)
    assert HEIC_INSTALL_HINT in str(exc.value)


def test_heic_plugin_available_matches_import() -> None:
    assert heic_plugin_available() is (importlib.util.find_spec("pillow_heif") is not None)


def test_heic_decode_primary_still_only(tmp_path: Path) -> None:
    pillow_heif = pytest.importorskip("pillow_heif")
    path = tmp_path / "primary.heic"
    image = Image.new("RGB", (40, 20), (12, 90, 180))
    try:
        pillow_heif.register_heif_opener()
        image.save(path, format="HEIF")
    except Exception as exc:
        pytest.skip(f"HEIC encoder unavailable: {exc}")
    if not path.is_file() or path.stat().st_size == 0:
        pytest.skip("HEIC encoder produced no file")

    jpeg = render_preview(path, MediaKind.heic, expected_sha256=hash_file(path))
    with Image.open(io.BytesIO(jpeg)) as preview:
        assert preview.format == "JPEG"
        assert preview.size == (40, 20)
        assert preview.getpixel((0, 0))[2] > 100


def test_heic_uses_to_pillow_primary_not_extra_images(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Live Photo / burst: only the primary still is converted."""

    class FakeHeif:
        extras_iterated = False

        def __iter__(self):
            FakeHeif.extras_iterated = True
            raise AssertionError("must not iterate HEIC extras / burst frames")

        def to_pillow(self) -> Image.Image:
            return Image.new("RGB", (24, 12), (1, 2, 3))

    class FakePillowHeif:
        @staticmethod
        def open_heif(_path: Path) -> FakeHeif:
            return FakeHeif()

    monkeypatch.setattr("gpsify.media.pillow_heif", FakePillowHeif())
    path = tmp_path / "burst.heic"
    path.write_bytes(b"ftypheic")
    jpeg = render_preview(path, MediaKind.heic, expected_sha256=hash_file(path))
    assert jpeg[:2] == b"\xff\xd8"
    assert FakeHeif.extras_iterated is False
