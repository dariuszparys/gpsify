"""Google Gemini vision wrapper (PR 10, post-v1 second provider).

Builds a ``generateContent`` request against the Gemini Developer API. Shares
``PROMPT_V1`` and ``format_user_message`` with the xAI wrapper on purpose: a
bake-off is only meaningful if both providers see the same prompt and return
the same ``VisionGuess`` contract.

Two normative rules carry over from K13, adapted to what Gemini exposes:

* **Reasoning is always explicit.** Gemini spells it ``thinkingConfig`` rather
  than ``reasoning_effort``; the wrapper maps the settings value to a thinking
  budget and refuses to send a payload without one. Omitting it lets the model
  pick its own budget and bill for it.
* **Storage is a key-tier property, not a request flag.** Gemini has no
  per-request ``store=False``. On the free tier Google may use prompts and
  images to improve its products; paid (billing-enabled) keys are excluded.
  gpsify therefore requires a billed key for real photos -- see the README.
  ``VisionCallOptions.store`` is still asserted False so the option contract
  stays uniform across providers.

``GPSIFY_DRY_RUN=1`` blocks live HTTP (same guard as xAI and the geocoder).
"""

from __future__ import annotations

import base64
import json
from collections.abc import Mapping, Sequence
from typing import Any

import httpx

from gpsify.geocode.base import live_http_blocked
from gpsify.providers.base import (
    VisionContractError,
    VisionError,
    VisionParseError,
    VisionRetryableError,
    assert_safe_vision_options,
)
from gpsify.providers.xai_grok import PROMPT_V1, format_user_message
from gpsify.types import PromptContext, Usage, VisionCallOptions, VisionGuess

GEMINI_BASE_URL = "https://generativelanguage.googleapis.com/v1beta"
GEMINI_DEFAULT_MODEL = "gemini-2.5-flash"

# Gemini inline data rides inside the request body, and the whole request must
# stay under 20 MB. base64 inflates by ~4/3, so cap the raw JPEG well below it.
# gpsify previews are 1280 px (~200-400 KB); this is a guard, not a budget.
MAX_INLINE_IMAGE_BYTES = 14 * 1024 * 1024

# gpsify's low|medium|high mapped onto Gemini thinking budgets (tokens).
THINKING_BUDGETS: Mapping[str, int] = {
    "low": 1024,
    "medium": 8192,
    "high": 24576,
}
# Per-family clamp. 2.5 Pro cannot think for less than 128 tokens and cannot
# disable thinking at all; Flash tops out lower than Pro.
THINKING_BUDGET_RANGE: Mapping[str, tuple[int, int]] = {
    "gemini-2.5-pro": (128, 32768),
    "gemini-2.5-flash": (128, 24576),
    "gemini-2.5-flash-lite": (512, 24576),
}
DEFAULT_THINKING_RANGE = (128, 24576)

# responseSchema is an OpenAPI 3.0 subset: no $ref/$defs, and null-unions are
# expressed as `nullable`. Anything outside this set is dropped.
_SCHEMA_KEYS = frozenset(
    {
        "type",
        "format",
        "description",
        "nullable",
        "enum",
        "items",
        "properties",
        "required",
        "propertyOrdering",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
    }
)

_BLOCKED_FINISH_REASONS = frozenset(
    {"SAFETY", "RECITATION", "PROHIBITED_CONTENT", "BLOCKLIST", "SPII"}
)


def thinking_budget(reasoning_effort: str, *, model: str) -> int:
    """Map gpsify ``reasoning_effort`` onto a Gemini thinking budget."""
    effort = reasoning_effort.strip().lower()
    if effort not in THINKING_BUDGETS:
        raise VisionContractError(
            f"reasoning_effort must be one of {sorted(THINKING_BUDGETS)}; got {reasoning_effort!r}"
        )
    low, high = THINKING_BUDGET_RANGE.get(model.strip(), DEFAULT_THINKING_RANGE)
    return max(low, min(high, THINKING_BUDGETS[effort]))


def to_gemini_schema(schema: Mapping[str, Any]) -> dict[str, Any]:
    """Rewrite a Pydantic JSON Schema into Gemini's ``responseSchema`` subset.

    Inlines ``$ref``/``$defs``, collapses ``anyOf: [T, null]`` into
    ``nullable``, drops ``title``/``default``, and stamps ``propertyOrdering``
    so field order is stable across calls.
    """
    defs = {str(k): v for k, v in (schema.get("$defs") or {}).items()}
    return _convert(schema, defs, tuple())


def _convert(
    node: Mapping[str, Any], defs: Mapping[str, Any], seen: tuple[str, ...]
) -> dict[str, Any]:
    ref = node.get("$ref")
    if isinstance(ref, str):
        name = ref.rsplit("/", 1)[-1]
        if name in seen:
            raise VisionContractError(f"recursive $ref in VisionGuess schema: {ref}")
        target = defs.get(name)
        if not isinstance(target, Mapping):
            raise VisionContractError(f"unresolvable $ref in VisionGuess schema: {ref}")
        merged = _convert(target, defs, seen + (name,))
        # Keep a sibling description (Pydantic puts Field(description=...) there).
        description = node.get("description")
        if isinstance(description, str) and "description" not in merged:
            merged["description"] = description
        return merged

    variants = node.get("anyOf")
    if isinstance(variants, Sequence) and not isinstance(variants, (str, bytes)):
        non_null = [
            v for v in variants if isinstance(v, Mapping) and v.get("type") != "null"
        ]
        nullable = len(non_null) != len(list(variants))
        if len(non_null) == 1:
            converted = _convert(non_null[0], defs, seen)
        else:
            converted = {
                "anyOf": [_convert(v, defs, seen) for v in non_null if isinstance(v, Mapping)]
            }
        if nullable:
            converted["nullable"] = True
        description = node.get("description")
        if isinstance(description, str) and "description" not in converted:
            converted["description"] = description
        return converted

    out: dict[str, Any] = {}
    for key, value in node.items():
        if key not in _SCHEMA_KEYS:
            continue
        if key == "properties" and isinstance(value, Mapping):
            out["properties"] = {
                str(name): _convert(sub, defs, seen)
                for name, sub in value.items()
                if isinstance(sub, Mapping)
            }
            out["propertyOrdering"] = [str(name) for name in value]
        elif key == "items" and isinstance(value, Mapping):
            out["items"] = _convert(value, defs, seen)
        else:
            out[key] = value
    return out


def vision_guess_gemini_schema() -> dict[str, Any]:
    return to_gemini_schema(VisionGuess.model_json_schema(mode="serialization"))


def build_gemini_request(
    image_jpeg: bytes,
    context: PromptContext,
    options: VisionCallOptions,
    *,
    model: str,
) -> dict[str, Any]:
    """Build the ``generateContent`` JSON body. Does not send.

    Tests inspect this dict: ``thinkingConfig.thinkingBudget`` must be present
    and the response must be schema-constrained JSON. The image is inline
    base64, never a public URL of the user's photo.
    """
    assert_safe_vision_options(options)
    if not image_jpeg:
        raise VisionError("refusing to send an empty image")
    if len(image_jpeg) > MAX_INLINE_IMAGE_BYTES:
        raise VisionError("image exceeds the Gemini inline limit; refusing to send")
    payload: dict[str, Any] = {
        "systemInstruction": {"parts": [{"text": PROMPT_V1}]},
        "contents": [
            {
                "role": "user",
                "parts": [
                    {
                        "inlineData": {
                            "mimeType": "image/jpeg",
                            "data": base64.standard_b64encode(image_jpeg).decode("ascii"),
                        }
                    },
                    {"text": format_user_message(context)},
                ],
            }
        ],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": vision_guess_gemini_schema(),
            "thinkingConfig": {
                "thinkingBudget": thinking_budget(
                    options.reasoning_effort, model=model
                ),
                # Thought text is billed either way; gpsify only wants the JSON.
                "includeThoughts": False,
            },
        },
    }
    assert_safe_gemini_payload(payload)
    return payload


def assert_safe_gemini_payload(payload: Mapping[str, Any]) -> None:
    """Last-chance refuse: no implicit thinking budget, no unconstrained output."""
    config = payload.get("generationConfig")
    if not isinstance(config, Mapping):
        raise VisionContractError("refusing to send: generationConfig is required")
    thinking = config.get("thinkingConfig")
    if not isinstance(thinking, Mapping) or thinking.get("thinkingBudget") is None:
        raise VisionContractError(
            "refusing to send: thinkingConfig.thinkingBudget is required "
            "(an omitted budget bills the model's own choice)"
        )
    if config.get("responseMimeType") != "application/json":
        raise VisionContractError("refusing to send: responseMimeType must be application/json")
    if not config.get("responseSchema"):
        raise VisionContractError("refusing to send: responseSchema is required")


def parse_usage(payload: Mapping[str, Any]) -> Usage:
    meta = payload.get("usageMetadata")
    if not isinstance(meta, Mapping):
        return Usage()
    return Usage(
        input_tokens=_as_int(meta.get("promptTokenCount")),
        output_tokens=_as_int(meta.get("candidatesTokenCount")),
        reasoning_tokens=_as_int(meta.get("thoughtsTokenCount")),
    )


def parse_vision_guess(payload: Mapping[str, Any]) -> VisionGuess:
    feedback = payload.get("promptFeedback")
    if isinstance(feedback, Mapping) and feedback.get("blockReason"):
        raise VisionParseError(f"content policy: {feedback['blockReason']}")
    candidates = payload.get("candidates")
    if not isinstance(candidates, list) or not candidates:
        raise VisionParseError("empty parse: no candidates")
    candidate = candidates[0]
    if not isinstance(candidate, Mapping):
        raise VisionParseError("empty parse: no candidate object")
    finish = candidate.get("finishReason")
    if isinstance(finish, str) and finish in _BLOCKED_FINISH_REASONS:
        raise VisionParseError(f"content policy: {finish}")
    if finish == "MAX_TOKENS":
        raise VisionParseError(
            "empty parse: hit MAX_TOKENS (thinking budget may have eaten the response)"
        )
    content = candidate.get("content")
    parts = content.get("parts") if isinstance(content, Mapping) else None
    text = ""
    if isinstance(parts, list):
        text = "".join(
            part["text"]
            for part in parts
            # A thought part is billed but is not the answer; skip it if present.
            if isinstance(part, Mapping)
            and isinstance(part.get("text"), str)
            and not part.get("thought")
        )
    if not text.strip():
        raise VisionParseError("empty parse: no content")
    try:
        return VisionGuess.model_validate_json(text)
    except Exception:
        try:
            return VisionGuess.model_validate(json.loads(text))
        except Exception as exc:
            raise VisionParseError(
                "empty parse: content is not VisionGuess JSON"
            ) from exc


def _as_int(*values: Any) -> int:
    for value in values:
        if value is None:
            continue
        try:
            return int(value)
        except (TypeError, ValueError):
            continue
    return 0


class GeminiProvider:
    """Post-v1 second provider. Fresh request per photo; no session reuse."""

    name = "gemini"

    def __init__(
        self,
        api_key: str,
        *,
        model: str = GEMINI_DEFAULT_MODEL,
        client: httpx.AsyncClient | None = None,
        base_url: str = GEMINI_BASE_URL,
    ) -> None:
        if not api_key.strip():
            raise VisionError("GEMINI_API_KEY is missing")
        self.model = model
        self._api_key = api_key
        self._base_url = base_url.rstrip("/")
        self._client = client
        self._owns_client = client is None

    async def aclose(self) -> None:
        if self._owns_client and self._client is not None:
            await self._client.aclose()
            self._client = None

    async def guess(
        self,
        image_jpeg: bytes,
        context: PromptContext,
        options: VisionCallOptions,
    ) -> tuple[VisionGuess, Usage]:
        assert_safe_vision_options(options)
        payload = build_gemini_request(image_jpeg, context, options, model=self.model)
        if live_http_blocked():
            raise VisionError("GPSIFY_DRY_RUN=1 blocks live Gemini HTTP")
        client = await self._ensure_client(options.timeout_s)
        url = f"{self._base_url}/models/{self.model}:generateContent"
        try:
            response = await client.post(
                url,
                json=payload,
                headers={
                    # Header auth, never the ?key= query param: query strings
                    # land in proxy and server logs.
                    "x-goog-api-key": self._api_key,
                    "Content-Type": "application/json",
                },
                timeout=options.timeout_s,
            )
        except httpx.TimeoutException as exc:
            raise VisionRetryableError("Gemini timeout") from exc
        except httpx.HTTPError as exc:
            raise VisionError(f"Gemini HTTP error: {exc}") from exc
        if response.status_code == 429 or response.status_code >= 500:
            raise VisionRetryableError(f"Gemini HTTP {response.status_code}")
        if response.status_code != 200:
            raise VisionParseError(f"Gemini HTTP {response.status_code}")
        try:
            body = response.json()
        except ValueError as exc:
            raise VisionParseError("Gemini returned non-JSON") from exc
        if not isinstance(body, dict):
            raise VisionParseError("Gemini returned non-object JSON")
        return parse_vision_guess(body), parse_usage(body)

    async def _ensure_client(self, timeout_s: float) -> httpx.AsyncClient:
        if self._client is not None:
            return self._client
        if live_http_blocked():
            raise VisionError("GPSIFY_DRY_RUN=1 blocks live Gemini HTTP")
        self._client = httpx.AsyncClient(timeout=timeout_s, follow_redirects=True)
        return self._client
