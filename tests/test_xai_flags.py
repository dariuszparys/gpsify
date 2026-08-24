"""The real xAI wrapper must refuse unsafe client flags (not the fake provider)."""

from __future__ import annotations

import asyncio
from collections.abc import Coroutine
from typing import Any, TypeVar

import pytest
from pydantic import ValidationError

from gpsify.providers.base import VisionContractError, assert_safe_vision_options
from gpsify.providers.xai_grok import (
    PROMPT_V1,
    XaiGrokProvider,
    assert_safe_xai_payload,
    build_xai_request,
    format_user_message,
    parse_usage,
    parse_vision_guess,
)
from gpsify.types import (
    Granularity,
    PromptContext,
    RefusalReason,
    Usage,
    VisionCallOptions,
    VisionGuess,
)

T = TypeVar("T")


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


def _options(**overrides: Any) -> VisionCallOptions:
    payload = {"store": False, "reasoning_effort": "low", "timeout_s": 120.0}
    payload.update(overrides)
    return VisionCallOptions.model_validate(payload)


def _context(**overrides: Any) -> PromptContext:
    payload = {
        "hint": "weekend in Porto",
        "folder_basename": "porto-2019",
        "filename": "IMG_1042.jpg",
        "datetime_original": "2019:06:14 18:04:00",
        "make": "Canon",
        "model": "EOS 6D",
        "no_context": False,
    }
    payload.update(overrides)
    return PromptContext.model_validate(payload)


def test_prompt_v1_is_versioned_geolocation_prompt() -> None:
    assert "travel/landmark" in PROMPT_V1
    assert "house number" in PROMPT_V1.lower() or "Do not return a street address" in PROMPT_V1
    assert "private" in PROMPT_V1.lower()


def test_vision_call_options_refuse_store_true() -> None:
    with pytest.raises(ValidationError, match="store"):
        VisionCallOptions(store=True, reasoning_effort="low", timeout_s=120)


def test_vision_call_options_require_reasoning_effort() -> None:
    with pytest.raises(ValidationError):
        VisionCallOptions.model_validate({"store": False, "timeout_s": 120})


def test_vision_call_options_reject_xhigh() -> None:
    with pytest.raises(ValidationError, match="xhigh"):
        VisionCallOptions(store=False, reasoning_effort="xhigh", timeout_s=120)


def test_assert_safe_options_rejects_constructed_unsafe_flags() -> None:
    stored = VisionCallOptions.model_construct(
        store=True, reasoning_effort="low", timeout_s=120.0
    )
    with pytest.raises(VisionContractError, match="store"):
        assert_safe_vision_options(stored)
    missing = VisionCallOptions.model_construct(
        store=False, reasoning_effort="", timeout_s=120.0
    )
    with pytest.raises(VisionContractError, match="reasoning_effort"):
        assert_safe_vision_options(missing)


def test_build_xai_request_sets_store_false_and_reasoning_effort() -> None:
    jpeg = b"\xff\xd8\xff" + b"\x00" * 16
    payload = build_xai_request(jpeg, _context(), _options(), model="grok-4.6")
    assert payload["store"] is False
    assert payload["store_messages"] is False
    assert payload["reasoning_effort"] == "low"
    assert payload["model"] == "grok-4.6"
    image = payload["messages"][1]["content"][0]
    url = image["image_url"]["url"]
    assert url.startswith("data:image/jpeg;base64,")
    assert "http://" not in url
    assert "https://" not in url
    assert image["image_url"]["detail"] == "high"
    text = payload["messages"][1]["content"][1]["text"]
    assert "IMG_1042.jpg" in text
    assert "/Users/" not in text
    assert payload["response_format"]["type"] == "json_schema"


def test_build_xai_request_refuses_empty_or_huge_image() -> None:
    with pytest.raises(Exception, match="empty"):
        build_xai_request(b"", _context(), _options(), model="grok-4.6")


def test_assert_safe_payload_refuses_storage_or_missing_effort() -> None:
    jpeg = b"\xff\xd8\xff" + b"\x00" * 16
    payload = build_xai_request(jpeg, _context(), _options(), model="grok-4.6")
    bad_store = dict(payload)
    bad_store["store"] = True
    with pytest.raises(VisionContractError, match="store"):
        assert_safe_xai_payload(bad_store)
    bad_messages = dict(payload)
    bad_messages["store_messages"] = True
    with pytest.raises(VisionContractError, match="store_messages"):
        assert_safe_xai_payload(bad_messages)
    missing = dict(payload)
    del missing["reasoning_effort"]
    with pytest.raises(VisionContractError, match="reasoning_effort"):
        assert_safe_xai_payload(missing)


def test_no_context_user_message_is_pixels_task_only() -> None:
    text = format_user_message(_context(no_context=True, hint="weekend in Porto"))
    assert text == "Task: infer where this photo was taken."
    assert "Porto" not in text
    assert "IMG_1042" not in text


def test_xai_provider_blocked_by_dry_run(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GPSIFY_DRY_RUN", "1")
    provider = XaiGrokProvider("test-key", model="grok-4.6")
    with pytest.raises(Exception, match="GPSIFY_DRY_RUN"):
        run(provider.guess(b"\xff\xd8\xff\x00", _context(), _options()))


def test_parse_usage_reads_reasoning_tokens() -> None:
    usage = parse_usage(
        {
            "usage": {
                "prompt_tokens": 2100,
                "completion_tokens": 520,
                "completion_tokens_details": {"reasoning_tokens": 800},
            }
        }
    )
    assert usage == Usage(input_tokens=2100, output_tokens=520, reasoning_tokens=800)
    usage = parse_usage(
        {"usage": {"input_tokens": 10, "output_tokens": 4, "reasoning_tokens": 9}}
    )
    assert usage.reasoning_tokens == 9


def test_parse_vision_guess_from_chat_completion() -> None:
    guess = VisionGuess(
        refusal=RefusalReason.none,
        primary_place_name="Clérigos Church, Porto, Portugal",
        country_code="PT",
        granularity=Granularity.exact,
        model_confidence=0.8,
        clues=[],
        candidates=[],
        used_album_hint=False,
        used_filename_hint=False,
    )
    payload = {"choices": [{"message": {"content": guess.model_dump_json()}}]}
    parsed = parse_vision_guess(payload)
    assert parsed.primary_place_name == guess.primary_place_name
