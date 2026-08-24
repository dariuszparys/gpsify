from __future__ import annotations

from pathlib import Path

import pytest
from pydantic import ValidationError

from gpsify.config import (
    DEFAULT_GEOCODER_URL,
    DEFAULT_MAP_TILE_URL,
    DEFAULT_MODEL,
    DEFAULT_REASONING,
    DEFAULT_TIMEOUT_S,
    DEFAULT_VISION_PROVIDER,
    Settings,
    flatten_config_toml,
    load_settings,
)

ENV_KEYS = (
    "XAI_API_KEY",
    "GPSIFY_MODEL",
    "GPSIFY_REASONING",
    "GPSIFY_TIMEOUT_S",
    "GPSIFY_VISION_PROVIDER",
    "GPSIFY_DATA_DIR",
    "GPSIFY_GEOCODER",
    "NOMINATIM_CONTACT",
    "GEOCODER_URL",
    "MAP_TILE_URL",
    "GEOCODE_LANG",
)


@pytest.fixture
def isolated_env(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Path:
    for key in ENV_KEYS:
        monkeypatch.delenv(key, raising=False)
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(tmp_path))
    return tmp_path


def test_defaults_match_design(isolated_env: Path) -> None:
    settings = load_settings(_env_file=None)
    assert settings.xai_api_key is None
    assert settings.vision_provider == DEFAULT_VISION_PROVIDER
    assert settings.model == DEFAULT_MODEL == "grok-4.6"
    assert settings.reasoning_effort == DEFAULT_REASONING == "low"
    assert settings.timeout_s == DEFAULT_TIMEOUT_S == 600.0
    assert settings.nominatim_contact is None
    assert settings.geocoder_url == DEFAULT_GEOCODER_URL
    assert settings.geocoder == "nominatim"
    assert settings.map_tile_url == DEFAULT_MAP_TILE_URL
    assert settings.geocode_lang == "en"
    assert settings.data_dir == isolated_env


def test_default_data_dir_is_home_gpsify(monkeypatch: pytest.MonkeyPatch) -> None:
    for key in ENV_KEYS:
        monkeypatch.delenv(key, raising=False)
    settings = load_settings(_env_file=None)
    assert settings.data_dir == Path.home() / ".gpsify"


def test_env_overrides(isolated_env: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("XAI_API_KEY", "xai-test-key")
    monkeypatch.setenv("GPSIFY_MODEL", "grok-env")
    monkeypatch.setenv("GPSIFY_REASONING", "medium")
    monkeypatch.setenv("GPSIFY_TIMEOUT_S", "180")
    monkeypatch.setenv("GPSIFY_VISION_PROVIDER", "xai")
    monkeypatch.setenv("NOMINATIM_CONTACT", "dev@example.com")
    monkeypatch.setenv("GEOCODER_URL", "http://127.0.0.1:8080/search")
    settings = load_settings(_env_file=None)
    assert settings.xai_api_key == "xai-test-key"
    assert settings.model == "grok-env"
    assert settings.reasoning_effort == "medium"
    assert settings.timeout_s == 180.0
    assert settings.nominatim_contact == "dev@example.com"
    assert settings.geocoder_url == "http://127.0.0.1:8080/search"


def test_toml_nested_vision(isolated_env: Path) -> None:
    (isolated_env / "config.toml").write_text(
        """
nominatim_contact = "toml@example.com"
geocoder_url = "https://photon.komoot.io/api"
geocoder = "photon"
geocode_lang = "de"

[vision]
provider = "xai"
model = "grok-toml"
reasoning_effort = "high"
timeout_s = 240
""".lstrip(),
        encoding="utf-8",
    )
    settings = load_settings(_env_file=None)
    assert settings.nominatim_contact == "toml@example.com"
    assert settings.geocoder_url == "https://photon.komoot.io/api"
    assert settings.geocoder == "photon"
    assert settings.geocode_lang == "de"
    assert settings.vision_provider == "xai"
    assert settings.model == "grok-toml"
    assert settings.reasoning_effort == "high"
    assert settings.timeout_s == 240.0


def test_env_wins_over_toml(isolated_env: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    (isolated_env / "config.toml").write_text(
        """
[vision]
model = "grok-toml"
reasoning_effort = "medium"
""".lstrip(),
        encoding="utf-8",
    )
    monkeypatch.setenv("GPSIFY_MODEL", "grok-env")
    monkeypatch.setenv("GPSIFY_REASONING", "low")
    settings = load_settings(_env_file=None)
    assert settings.model == "grok-env"
    assert settings.reasoning_effort == "low"


def test_rejects_xhigh_reasoning(isolated_env: Path) -> None:
    with pytest.raises(ValidationError, match="xhigh"):
        load_settings(_env_file=None, reasoning_effort="xhigh")


def test_env_xhigh_reasoning_rejected(
    isolated_env: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GPSIFY_REASONING", "xhigh")
    with pytest.raises(ValidationError, match="xhigh"):
        load_settings(_env_file=None)


def test_rejects_timeout_below_minimum(isolated_env: Path) -> None:
    with pytest.raises(ValidationError, match="120"):
        load_settings(_env_file=None, timeout_s=60)


def test_rejects_non_email_nominatim_contact(isolated_env: Path) -> None:
    with pytest.raises(ValidationError, match="email-shaped"):
        load_settings(_env_file=None, nominatim_contact="not-an-email")


def test_empty_nominatim_contact_is_none(isolated_env: Path) -> None:
    settings = load_settings(_env_file=None, nominatim_contact="  ")
    assert settings.nominatim_contact is None


def test_flatten_config_toml_lifts_vision_table() -> None:
    flat = flatten_config_toml(
        {
            "nominatim_contact": "a@b.co",
            "vision": {"model": "grok-4.6", "provider": "xai", "reasoning_effort": "low"},
        }
    )
    assert flat["nominatim_contact"] == "a@b.co"
    assert flat["model"] == "grok-4.6"
    assert flat["vision_provider"] == "xai"
    assert "vision" not in flat


def test_data_dir_expands_user(isolated_env: Path) -> None:
    settings = Settings(_env_file=None, data_dir="~/gpsify-data")
    assert settings.data_dir == Path.home() / "gpsify-data"
