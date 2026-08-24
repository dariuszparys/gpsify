"""Shared Pydantic contracts for vision, geocoding, jobs, and decisions."""

from __future__ import annotations

from enum import Enum
from typing import Self

from pydantic import BaseModel, Field, field_validator, model_validator

ALLOWED_REASONING_EFFORT = frozenset({"low", "medium", "high"})
MIN_VISION_TIMEOUT_S = 120.0


class Granularity(str, Enum):
    exact = "exact"
    street = "street"
    neighborhood = "neighborhood"
    city = "city"
    region = "region"
    country = "country"
    unknown = "unknown"


class RefusalReason(str, Enum):
    none = "none"
    insufficient_evidence = "insufficient_evidence"
    indoor_generic = "indoor_generic"
    privacy_sensitive = "privacy_sensitive"
    policy_refusal = "policy_refusal"
    not_a_photograph = "not_a_photograph"


class PinSource(str, Enum):
    geocoder = "geocoder"
    user = "user"
    none = "none"


class DecisionAction(str, Enum):
    accept = "accept"
    accept_edited = "accept_edited"
    skip = "skip"
    cluster_apply = "cluster_apply"


class WriteReceiptStatus(str, Enum):
    pending = "pending"
    ok = "ok"
    failed = "failed"
    undone = "undone"


class PhotoStatus(str, Enum):
    discovered = "discovered"
    skipped_has_gps = "skipped_has_gps"
    skipped_unsupported = "skipped_unsupported"
    skipped_unreadable = "skipped_unreadable"
    skipped_no_preview = "skipped_no_preview"
    pending = "pending"
    proposing = "proposing"
    proposed = "proposed"
    refused = "refused"
    error = "error"
    accepted = "accepted"
    accepted_edited = "accepted_edited"
    skipped_by_user = "skipped_by_user"
    written = "written"
    write_failed = "write_failed"
    undone = "undone"
    missing = "missing"


class GeocoderName(str, Enum):
    nominatim = "nominatim"
    photon = "photon"
    mapbox = "mapbox"
    google = "google"


class Clue(BaseModel):
    kind: str = Field(description="sign|language|architecture|vegetation|road|vehicle|text|skyline|other")
    text: str
    weight: float = Field(ge=0, le=1)


class Candidate(BaseModel):
    place_name: str
    country_code: str | None = Field(default=None, description="ISO 3166-1 alpha-2")
    admin1: str | None = None
    lat_hint: float | None = None
    lng_hint: float | None = None
    confidence: float = Field(ge=0, le=1)
    why: str


class VisionGuess(BaseModel):
    refusal: RefusalReason
    refusal_detail: str | None = None
    primary_place_name: str | None = None
    country_code: str | None = None
    admin1: str | None = None
    granularity: Granularity
    lat_hint: float | None = None
    lng_hint: float | None = None
    model_confidence: float = Field(ge=0, le=1)
    clues: list[Clue]
    candidates: list[Candidate]
    used_album_hint: bool
    used_filename_hint: bool


class GeoHit(BaseModel):
    query: str
    display_name: str
    lat: float
    lng: float
    boundingbox: tuple[float, float, float, float] | None = None
    osm_type: str | None = None
    osm_id: str | None = None
    importance: float | None = None
    provider: str


class Usage(BaseModel):
    input_tokens: int = 0
    output_tokens: int = 0
    reasoning_tokens: int = 0


class ProposedSibling(BaseModel):
    filename: str
    captured_at: str | None = None
    place_name: str
    country_code: str | None = None
    granularity: Granularity
    model_confidence: float


class TaggedSibling(BaseModel):
    filename: str
    lat: float
    lng: float


class PromptContext(BaseModel):
    """Text assembled around one vision call. Never includes a full home path."""

    hint: str | None = None
    folder_basename: str
    filename: str
    datetime_original: str | None = None
    offset: str | None = None
    make: str | None = None
    model: str | None = None
    proposed_siblings: list[ProposedSibling] = Field(default_factory=list)
    gps_siblings: list[TaggedSibling] = Field(default_factory=list)
    no_context: bool = False


class VisionCallOptions(BaseModel):
    """Client flags for a single vision call.

    ``reasoning_effort`` has no default: omitting it silently bills ``high`` on
    grok-4.6. ``store`` must stay False so image calls are not retained.
    """

    store: bool = False
    reasoning_effort: str
    timeout_s: float

    @field_validator("reasoning_effort")
    @classmethod
    def _normalize_reasoning_effort(cls, value: str) -> str:
        effort = value.strip().lower()
        if effort not in ALLOWED_REASONING_EFFORT:
            raise ValueError(
                "reasoning_effort must be one of "
                f"{sorted(ALLOWED_REASONING_EFFORT)}; got {value!r} "
                "(xhigh is out of v1)"
            )
        return effort

    @field_validator("timeout_s")
    @classmethod
    def _require_min_timeout(cls, value: float) -> float:
        if value < MIN_VISION_TIMEOUT_S:
            raise ValueError(f"timeout_s must be >= {MIN_VISION_TIMEOUT_S}")
        return value

    @model_validator(mode="after")
    def refuse_unsafe_call(self) -> Self:
        if self.store is not False:
            raise ValueError("store must be False on every vision call")
        return self
