"""The Gemini wrapper (PR 10) must honour the same contract as the xAI one.

Every test here drives the real wrapper, not the fake provider. Live HTTP is a
``httpx.MockTransport``; no socket is ever opened.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Coroutine
from datetime import date
from pathlib import Path
from typing import Any, TypeVar

import httpx
import pytest

from gpsify.config import DEFAULT_GEMINI_MODEL, load_settings
from gpsify.ingest import run_ingest
from gpsify.propose import ProposeError, estimate_usd, run_propose
from gpsify.providers.base import VisionContractError, VisionError, VisionParseError
from gpsify.providers.gemini import (
    GEMINI_DEFAULT_MODEL,
    MAX_INLINE_IMAGE_BYTES,
    GeminiProvider,
    assert_safe_gemini_payload,
    build_gemini_request,
    parse_usage,
    parse_vision_guess,
    thinking_budget,
    to_gemini_schema,
    vision_guess_gemini_schema,
)
from gpsify.providers.xai_grok import PROMPT_V1, format_user_message
from gpsify.types import (
    Granularity,
    PromptContext,
    RefusalReason,
    Usage,
    VisionCallOptions,
    VisionGuess,
)

T = TypeVar("T")


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    path = tmp_path / "gpsify-data"
    path.mkdir()
    return path


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


def _guess_json() -> str:
    return VisionGuess(
        refusal=RefusalReason.none,
        primary_place_name="Clérigos Church, Porto, Portugal",
        country_code="PT",
        granularity=Granularity.exact,
        model_confidence=0.82,
        clues=[],
        candidates=[],
        used_album_hint=True,
        used_filename_hint=False,
    ).model_dump_json()


# --- response schema -------------------------------------------------------


def test_gemini_schema_drops_keywords_the_api_rejects() -> None:
    encoded = json.dumps(vision_guess_gemini_schema())
    for unsupported in ("$ref", "$defs", '"title"', '"default"'):
        assert unsupported not in encoded, unsupported


def test_gemini_schema_collapses_null_unions_to_nullable() -> None:
    schema = vision_guess_gemini_schema()
    optional = schema["properties"]["primary_place_name"]
    assert optional == {"type": "string", "nullable": True}
    required = schema["properties"]["used_album_hint"]
    assert "nullable" not in required


def test_gemini_schema_inlines_enums_and_orders_properties() -> None:
    schema = vision_guess_gemini_schema()
    assert schema["properties"]["refusal"]["enum"] == [r.value for r in RefusalReason]
    assert schema["properties"]["granularity"]["enum"] == [g.value for g in Granularity]
    # propertyOrdering keeps field order stable across calls.
    assert schema["propertyOrdering"][0] == "refusal"
    assert set(schema["propertyOrdering"]) == set(schema["properties"])
    nested = schema["properties"]["candidates"]["items"]
    assert nested["properties"]["place_name"] == {"type": "string"}
    assert "propertyOrdering" in nested


def test_gemini_schema_keeps_required_and_bounds() -> None:
    schema = vision_guess_gemini_schema()
    assert "refusal" in schema["required"]
    assert "refusal_detail" not in schema["required"]
    assert schema["properties"]["model_confidence"]["minimum"] == 0
    assert schema["properties"]["model_confidence"]["maximum"] == 1


def test_gemini_schema_refuses_recursive_refs() -> None:
    recursive = {
        "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}},
        "type": "object",
        "properties": {"root": {"$ref": "#/$defs/Node"}},
    }
    with pytest.raises(VisionContractError, match="recursive"):
        to_gemini_schema(recursive)


# --- request contract ------------------------------------------------------


def test_request_always_carries_an_explicit_thinking_budget() -> None:
    payload = build_gemini_request(
        b"\xff\xd8jpeg", _context(), _options(), model=GEMINI_DEFAULT_MODEL
    )
    config = payload["generationConfig"]
    assert config["thinkingConfig"]["thinkingBudget"] == thinking_budget(
        "low", model=GEMINI_DEFAULT_MODEL
    )
    assert config["thinkingConfig"]["includeThoughts"] is False
    assert config["responseMimeType"] == "application/json"
    assert config["responseSchema"]["properties"]


def test_request_shares_prompt_and_context_with_the_xai_wrapper() -> None:
    context = _context()
    payload = build_gemini_request(
        b"\xff\xd8jpeg", context, _options(), model=GEMINI_DEFAULT_MODEL
    )
    assert payload["systemInstruction"]["parts"][0]["text"] == PROMPT_V1
    parts = payload["contents"][0]["parts"]
    assert parts[1]["text"] == format_user_message(context)


def test_request_sends_inline_base64_never_a_url() -> None:
    payload = build_gemini_request(
        b"\xff\xd8jpeg", _context(), _options(), model=GEMINI_DEFAULT_MODEL
    )
    inline = payload["contents"][0]["parts"][0]["inlineData"]
    assert inline["mimeType"] == "image/jpeg"
    assert inline["data"] == "/9hqcGVn"
    assert "fileUri" not in json.dumps(payload)


def test_request_refuses_empty_and_oversize_images() -> None:
    with pytest.raises(VisionError, match="empty image"):
        build_gemini_request(b"", _context(), _options(), model=GEMINI_DEFAULT_MODEL)
    with pytest.raises(VisionError, match="inline limit"):
        build_gemini_request(
            b"x" * (MAX_INLINE_IMAGE_BYTES + 1),
            _context(),
            _options(),
            model=GEMINI_DEFAULT_MODEL,
        )


def test_request_refuses_unsafe_options() -> None:
    unsafe = _options().model_copy(update={"store": True})
    with pytest.raises(VisionContractError, match="store must be False"):
        build_gemini_request(
            b"\xff\xd8jpeg", _context(), unsafe, model=GEMINI_DEFAULT_MODEL
        )
    missing = _options().model_copy(update={"reasoning_effort": " "})
    with pytest.raises(VisionContractError, match="reasoning_effort"):
        build_gemini_request(
            b"\xff\xd8jpeg", _context(), missing, model=GEMINI_DEFAULT_MODEL
        )


@pytest.mark.parametrize(
    "mutation, message",
    [
        ({"generationConfig": {}}, "thinkingBudget"),
        (
            {"generationConfig": {"thinkingConfig": {"thinkingBudget": 1}}},
            "responseMimeType",
        ),
        (
            {
                "generationConfig": {
                    "thinkingConfig": {"thinkingBudget": 1},
                    "responseMimeType": "application/json",
                }
            },
            "responseSchema",
        ),
    ],
)
def test_last_chance_guard_refuses_incomplete_payloads(
    mutation: dict[str, Any], message: str
) -> None:
    with pytest.raises(VisionContractError, match=message):
        assert_safe_gemini_payload(mutation)


def test_thinking_budget_is_ordered_and_clamped_per_model() -> None:
    flash = [thinking_budget(e, model="gemini-2.5-flash") for e in ("low", "medium", "high")]
    assert flash == sorted(flash) and len(set(flash)) == 3
    # 2.5 Pro cannot think for fewer than 128 tokens; Flash caps below Pro.
    assert thinking_budget("high", model="gemini-2.5-flash") <= 24576
    assert thinking_budget("low", model="gemini-2.5-pro") >= 128
    with pytest.raises(VisionContractError, match="reasoning_effort"):
        thinking_budget("xhigh", model=GEMINI_DEFAULT_MODEL)


# --- response parsing ------------------------------------------------------


def test_parse_vision_guess_reads_the_answer_and_skips_thought_parts() -> None:
    body = {
        "candidates": [
            {
                "finishReason": "STOP",
                "content": {
                    "parts": [
                        {"text": "internal musing", "thought": True},
                        {"text": _guess_json()},
                    ]
                },
            }
        ]
    }
    guess = parse_vision_guess(body)
    assert guess.primary_place_name == "Clérigos Church, Porto, Portugal"
    assert guess.country_code == "PT"


@pytest.mark.parametrize(
    "body, message",
    [
        ({"promptFeedback": {"blockReason": "SAFETY"}}, "content policy"),
        ({"candidates": []}, "no candidates"),
        ({"candidates": [{"finishReason": "PROHIBITED_CONTENT"}]}, "content policy"),
        ({"candidates": [{"finishReason": "MAX_TOKENS"}]}, "MAX_TOKENS"),
        (
            {"candidates": [{"content": {"parts": [{"text": "   "}]}}]},
            "no content",
        ),
        (
            {"candidates": [{"content": {"parts": [{"text": "{\"nope\": 1}"}]}}]},
            "not VisionGuess JSON",
        ),
    ],
)
def test_parse_vision_guess_rejects_bad_bodies(body: dict[str, Any], message: str) -> None:
    with pytest.raises(VisionParseError, match=message):
        parse_vision_guess(body)


def test_parse_usage_maps_gemini_metadata_onto_usage() -> None:
    usage = parse_usage(
        {
            "usageMetadata": {
                "promptTokenCount": 1032,
                "candidatesTokenCount": 480,
                "thoughtsTokenCount": 900,
                "totalTokenCount": 2412,
            }
        }
    )
    assert usage == Usage(input_tokens=1032, output_tokens=480, reasoning_tokens=900)
    assert parse_usage({}) == Usage()


# --- provider --------------------------------------------------------------


def test_provider_refuses_a_blank_key() -> None:
    with pytest.raises(VisionError, match="GEMINI_API_KEY"):
        GeminiProvider("   ")


def test_provider_is_blocked_by_dry_run() -> None:
    provider = GeminiProvider("test-key")
    with pytest.raises(VisionError, match="GPSIFY_DRY_RUN"):
        run(provider.guess(b"\xff\xd8jpeg", _context(), _options()))


def test_provider_round_trip_over_a_mock_transport(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("GPSIFY_DRY_RUN", raising=False)
    seen: dict[str, Any] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["url"] = str(request.url)
        seen["api_key"] = request.headers.get("x-goog-api-key")
        seen["body"] = json.loads(request.content)
        return httpx.Response(
            200,
            json={
                "candidates": [
                    {"finishReason": "STOP", "content": {"parts": [{"text": _guess_json()}]}}
                ],
                "usageMetadata": {
                    "promptTokenCount": 1032,
                    "candidatesTokenCount": 480,
                    "thoughtsTokenCount": 900,
                },
            },
        )

    client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    provider = GeminiProvider("test-key", client=client)
    guess, usage = run(provider.guess(b"\xff\xd8jpeg", _context(), _options()))

    assert guess.country_code == "PT"
    assert usage.reasoning_tokens == 900
    assert seen["url"].endswith(f"/models/{GEMINI_DEFAULT_MODEL}:generateContent")
    # Key travels in a header; a ?key= query string would land in proxy logs.
    assert seen["api_key"] == "test-key"
    assert "test-key" not in seen["url"]
    assert seen["body"]["generationConfig"]["thinkingConfig"]["thinkingBudget"] > 0
    run(provider.aclose())


@pytest.mark.parametrize(
    "status, expected",
    [(429, "Gemini HTTP 429"), (503, "Gemini HTTP 503"), (400, "Gemini HTTP 400")],
)
def test_provider_maps_http_errors(
    monkeypatch: pytest.MonkeyPatch, status: int, expected: str
) -> None:
    monkeypatch.delenv("GPSIFY_DRY_RUN", raising=False)
    client = httpx.AsyncClient(
        transport=httpx.MockTransport(lambda request: httpx.Response(status, json={}))
    )
    provider = GeminiProvider("test-key", client=client)
    with pytest.raises(VisionError, match=expected):
        run(provider.guess(b"\xff\xd8jpeg", _context(), _options()))
    run(provider.aclose())


# --- wiring ----------------------------------------------------------------


def test_settings_default_to_the_researched_flash_model() -> None:
    settings = load_settings(_env_file=None)
    assert settings.gemini_model == DEFAULT_GEMINI_MODEL == GEMINI_DEFAULT_MODEL
    assert settings.vision_provider == "xai"


def test_settings_reject_an_unknown_provider() -> None:
    with pytest.raises(Exception, match="vision_provider must be one of"):
        load_settings(_env_file=None, vision_provider="openai")


def test_propose_names_the_missing_key_for_the_selected_provider(data_dir: Path) -> None:
    settings = load_settings(_env_file=None, data_dir=data_dir, gemini_api_key=None)
    with pytest.raises(ProposeError, match="GEMINI_API_KEY is required"):
        run(run_propose("no-such-job", settings=settings, provider_name="gemini"))


def test_propose_rejects_an_unknown_provider(data_dir: Path) -> None:
    settings = load_settings(_env_file=None, data_dir=data_dir)
    with pytest.raises(ProposeError, match="--provider must be one of"):
        run(run_propose("no-such-job", settings=settings, provider_name="openai"))


def test_live_gemini_run_warns_that_retention_is_a_key_property(
    tmp_path: Path, data_dir: Path
) -> None:
    """No per-request store=False exists, so propose must say so out loud."""
    folder = tmp_path / "empty-album"
    folder.mkdir()
    ingested = run(run_ingest(folder, data_dir=data_dir))
    settings = load_settings(
        _env_file=None,
        data_dir=data_dir,
        gemini_api_key="test-key",
        nominatim_contact="dev@example.com",
        geocoder_url="http://127.0.0.1:8080/search",
    )
    # No photos to send, so the run resolves the provider and stops: no HTTP.
    summary = run(
        run_propose(ingested.job_id, settings=settings, provider_name="gemini")
    )
    assert summary.planned == 0
    assert any("store=False" in warning for warning in summary.warnings)
    assert any("billing-enabled" in warning for warning in summary.warnings)


def test_estimate_usd_uses_the_selected_model_rate() -> None:
    usage = Usage(input_tokens=1_000_000, output_tokens=1_000_000, reasoning_tokens=0)
    grok = estimate_usd(usage, "grok-4.6")
    flash = estimate_usd(usage, GEMINI_DEFAULT_MODEL)
    assert grok == pytest.approx(8.0)
    assert flash == pytest.approx(2.8)
    # An unknown model must not silently price at zero.
    assert estimate_usd(usage, "made-up-model") == grok


def test_estimate_usd_follows_googles_promotional_rate_rollover() -> None:
    """3.6/3.7 Flash launch pricing doubles on 2027-01-01; the estimate must too."""
    usage = Usage(input_tokens=1_000_000, output_tokens=1_000_000, reasoning_tokens=0)
    promo = estimate_usd(usage, "gemini-3.7-flash", on=date(2026, 12, 31))
    after = estimate_usd(usage, "gemini-3.7-flash", on=date(2027, 1, 1))
    assert promo == pytest.approx(4.5)
    assert after == pytest.approx(9.0)
    # Models without a promo are unaffected by the date.
    assert estimate_usd(usage, "gemini-2.5-flash", on=date(2027, 1, 1)) == pytest.approx(2.8)


def test_gemini_37_flash_loses_its_cost_advantage_after_the_promo() -> None:
    """The reason grok stays the default: post-promo 3.7 Flash costs more."""
    # Token profile from docs/research/vision-model-selection.md for a 1280px JPEG.
    usage = Usage(input_tokens=1032, output_tokens=480, reasoning_tokens=900)
    grok = estimate_usd(usage, "grok-4.6")
    flash_25 = estimate_usd(usage, "gemini-2.5-flash")
    flash_37_after = estimate_usd(usage, "gemini-3.7-flash", on=date(2027, 1, 1))
    assert flash_25 < grok
    assert flash_37_after > grok
