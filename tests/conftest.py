"""Unit tests never talk to the network or write live EXIF unless marked."""

from __future__ import annotations

import os

import pytest

os.environ.setdefault("GPSIFY_DRY_RUN", "1")


@pytest.fixture(autouse=True)
def _writes_exif_unblocks_dry_run(
    request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch
) -> None:
    if request.node.get_closest_marker("writes_exif") is not None:
        monkeypatch.delenv("GPSIFY_DRY_RUN", raising=False)
