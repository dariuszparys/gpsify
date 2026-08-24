"""Application settings from the environment and ``~/.gpsify/config.toml``."""

from __future__ import annotations

import os
import re
import tomllib
from collections.abc import Mapping
from pathlib import Path
from typing import Any

from pydantic import AliasChoices, Field, field_validator
from pydantic.fields import FieldInfo
from pydantic_settings import (
    BaseSettings,
    PydanticBaseSettingsSource,
    SettingsConfigDict,
)

from gpsify.types import ALLOWED_REASONING_EFFORT, MIN_VISION_TIMEOUT_S

DEFAULT_MODEL = "grok-4.6"
DEFAULT_REASONING = "low"
DEFAULT_TIMEOUT_S = 600.0
DEFAULT_VISION_PROVIDER = "xai"
DEFAULT_GEOCODER_URL = "https://nominatim.openstreetmap.org/search"
DEFAULT_MAP_TILE_URL = "https://tile.openstreetmap.org/{z}/{x}/{y}.png"
DEFAULT_GEOCODE_LANG = "en"
DEFAULT_GEOCODER = "nominatim"

_EMAIL_SHAPED = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")

_VISION_TOML_KEYS = {
    "provider": "vision_provider",
    "model": "model",
    "reasoning_effort": "reasoning_effort",
    "timeout_s": "timeout_s",
}


def default_data_dir() -> Path:
    override = os.environ.get("GPSIFY_DATA_DIR")
    if override:
        return Path(override).expanduser()
    return Path.home() / ".gpsify"


def config_toml_path(data_dir: Path | None = None) -> Path:
    return (data_dir or default_data_dir()) / "config.toml"


def flatten_config_toml(raw: Mapping[str, Any]) -> dict[str, Any]:
    """Lift ``[vision]`` keys to the flat Settings field names."""
    flat = {k: v for k, v in raw.items() if k != "vision"}
    vision = raw.get("vision") or {}
    if not isinstance(vision, Mapping):
        raise ValueError("config.toml [vision] must be a table")
    for src, dest in _VISION_TOML_KEYS.items():
        if src in vision:
            flat[dest] = vision[src]
    return flat


class TomlSettingsSource(PydanticBaseSettingsSource):
    """Read ``config.toml`` from the data dir (``GPSIFY_DATA_DIR`` or ``~/.gpsify``)."""

    def __init__(self, settings_cls: type[BaseSettings], toml_path: Path) -> None:
        super().__init__(settings_cls)
        self._values = self._load(toml_path)

    @staticmethod
    def _load(toml_path: Path) -> dict[str, Any]:
        if not toml_path.is_file():
            return {}
        with toml_path.open("rb") as fh:
            raw = tomllib.load(fh)
        return flatten_config_toml(raw)

    def get_field_value(self, field: FieldInfo, field_name: str) -> tuple[Any, str, bool]:
        if field_name in self._values:
            return self._values[field_name], field_name, False
        return None, field_name, False

    def prepare_field_value(
        self,
        field_name: str,
        field: FieldInfo,
        value: Any,
        value_is_complex: bool,
    ) -> Any:
        return value

    def __call__(self) -> dict[str, Any]:
        data: dict[str, Any] = {}
        for field_name, field in self.settings_cls.model_fields.items():
            value, _key, is_complex = self.get_field_value(field, field_name)
            if value is not None:
                data[field_name] = self.prepare_field_value(
                    field_name, field, value, is_complex
                )
        return data


class Settings(BaseSettings):
    """Runtime settings. Environment wins over ``config.toml`` over defaults."""

    model_config = SettingsConfigDict(
        extra="ignore",
        populate_by_name=True,
        env_file=".env",
        env_file_encoding="utf-8",
        env_ignore_empty=True,
    )

    xai_api_key: str | None = Field(
        default=None,
        validation_alias=AliasChoices("XAI_API_KEY", "xai_api_key"),
    )
    vision_provider: str = Field(
        default=DEFAULT_VISION_PROVIDER,
        validation_alias=AliasChoices("GPSIFY_VISION_PROVIDER", "vision_provider"),
    )
    model: str = Field(
        default=DEFAULT_MODEL,
        validation_alias=AliasChoices("GPSIFY_MODEL", "model"),
    )
    reasoning_effort: str = Field(
        default=DEFAULT_REASONING,
        validation_alias=AliasChoices("GPSIFY_REASONING", "reasoning_effort"),
    )
    timeout_s: float = Field(
        default=DEFAULT_TIMEOUT_S,
        validation_alias=AliasChoices("GPSIFY_TIMEOUT_S", "timeout_s"),
    )
    nominatim_contact: str | None = Field(
        default=None,
        validation_alias=AliasChoices("NOMINATIM_CONTACT", "nominatim_contact"),
    )
    geocoder_url: str = Field(
        default=DEFAULT_GEOCODER_URL,
        validation_alias=AliasChoices("GEOCODER_URL", "geocoder_url"),
    )
    geocoder: str = Field(
        default=DEFAULT_GEOCODER,
        validation_alias=AliasChoices("GPSIFY_GEOCODER", "geocoder"),
    )
    map_tile_url: str = Field(
        default=DEFAULT_MAP_TILE_URL,
        validation_alias=AliasChoices("MAP_TILE_URL", "map_tile_url"),
    )
    geocode_lang: str = Field(
        default=DEFAULT_GEOCODE_LANG,
        validation_alias=AliasChoices("GEOCODE_LANG", "geocode_lang"),
    )
    data_dir: Path = Field(
        default_factory=default_data_dir,
        validation_alias=AliasChoices("GPSIFY_DATA_DIR", "data_dir"),
    )

    @field_validator("reasoning_effort")
    @classmethod
    def _reasoning_effort(cls, value: str) -> str:
        effort = value.strip().lower()
        if effort not in ALLOWED_REASONING_EFFORT:
            allowed = ", ".join(sorted(ALLOWED_REASONING_EFFORT))
            raise ValueError(
                f"reasoning_effort must be one of {allowed}; "
                f"got {value!r} (xhigh is out of v1)"
            )
        return effort

    @field_validator("timeout_s")
    @classmethod
    def _timeout_s(cls, value: float) -> float:
        if value < MIN_VISION_TIMEOUT_S:
            raise ValueError(f"timeout_s must be >= {MIN_VISION_TIMEOUT_S}")
        return value

    @field_validator("nominatim_contact")
    @classmethod
    def _nominatim_contact(cls, value: str | None) -> str | None:
        if value is None:
            return None
        stripped = value.strip()
        if stripped == "":
            return None
        if _EMAIL_SHAPED.match(stripped) is None:
            raise ValueError("nominatim_contact must be an email-shaped string")
        return stripped

    @field_validator("data_dir", mode="before")
    @classmethod
    def _data_dir(cls, value: Path | str) -> Path:
        return Path(value).expanduser()

    @field_validator("vision_provider", "geocoder", "geocode_lang")
    @classmethod
    def _normalize_token(cls, value: str) -> str:
        return value.strip().lower()

    @field_validator("model")
    @classmethod
    def _strip_model(cls, value: str) -> str:
        return value.strip()

    @classmethod
    def settings_customise_sources(
        cls,
        settings_cls: type[BaseSettings],
        init_settings: PydanticBaseSettingsSource,
        env_settings: PydanticBaseSettingsSource,
        dotenv_settings: PydanticBaseSettingsSource,
        file_secret_settings: PydanticBaseSettingsSource,
    ) -> tuple[PydanticBaseSettingsSource, ...]:
        return (
            init_settings,
            env_settings,
            dotenv_settings,
            TomlSettingsSource(settings_cls, config_toml_path()),
            file_secret_settings,
        )


def load_settings(**overrides: Any) -> Settings:
    """Build settings. Pass ``_env_file=None`` from tests to skip a local ``.env``."""
    return Settings(**overrides)
