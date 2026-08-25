"""Unit tests never talk to the network or write live EXIF unless marked."""

from __future__ import annotations

import os

import pytest

os.environ.setdefault("GPSIFY_DRY_RUN", "1")


@pytest.fixture(autouse=True)
def _ignore_developer_dotenv(monkeypatch: pytest.MonkeyPatch) -> None:
    """Never read the developer's ``.env``.

    ``cli.py`` calls ``load_settings()`` with no ``_env_file`` override, so a
    real ``GEMINI_API_KEY`` on the machine would leak into tests that assert a
    key is *missing* — passing in CI and failing locally, or worse. Test
    settings come from monkeypatched env vars and explicit kwargs only.
    """
    from gpsify.config import Settings

    monkeypatch.setitem(Settings.model_config, "env_file", None)


@pytest.fixture(autouse=True)
def _writes_exif_unblocks_dry_run(
    request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch
) -> None:
    if request.node.get_closest_marker("writes_exif") is not None:
        monkeypatch.delenv("GPSIFY_DRY_RUN", raising=False)
