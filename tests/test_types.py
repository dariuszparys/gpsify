from __future__ import annotations

import pytest
from pydantic import ValidationError

from gpsify.types import (
    Granularity,
    RefusalReason,
    Usage,
    VisionCallOptions,
    VisionGuess,
)


def test_vision_call_options_requires_reasoning_effort() -> None:
    with pytest.raises(ValidationError):
        VisionCallOptions(timeout_s=600)  # type: ignore[call-arg]


def test_vision_call_options_refuses_store_true() -> None:
    with pytest.raises(ValidationError, match="store must be False"):
        VisionCallOptions(store=True, reasoning_effort="low", timeout_s=600)


def test_vision_call_options_rejects_xhigh() -> None:
    with pytest.raises(ValidationError, match="xhigh"):
        VisionCallOptions(reasoning_effort="xhigh", timeout_s=600)


def test_vision_call_options_rejects_short_timeout() -> None:
    with pytest.raises(ValidationError, match="120"):
        VisionCallOptions(reasoning_effort="low", timeout_s=30)


def test_vision_call_options_normalizes_effort() -> None:
    options = VisionCallOptions(reasoning_effort="LOW", timeout_s=120)
    assert options.store is False
    assert options.reasoning_effort == "low"


def test_vision_guess_round_trip() -> None:
    guess = VisionGuess(
        refusal=RefusalReason.none,
        primary_place_name="Clérigos Church, Porto, Portugal",
        country_code="PT",
        granularity=Granularity.exact,
        model_confidence=0.8,
        clues=[{"kind": "architecture", "text": "baroque tower", "weight": 0.7}],
        candidates=[
            {
                "place_name": "Clérigos Church, Porto, Portugal",
                "country_code": "PT",
                "confidence": 0.8,
                "why": "tower silhouette",
            }
        ],
        used_album_hint=True,
        used_filename_hint=False,
    )
    assert guess.primary_place_name is not None
    assert guess.lat_hint is None


def test_usage_defaults_are_zero() -> None:
    usage = Usage()
    assert usage.input_tokens == 0
    assert usage.output_tokens == 0
    assert usage.reasoning_tokens == 0
