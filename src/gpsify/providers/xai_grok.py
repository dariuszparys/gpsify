"""xAI grok-4.6 vision wrapper (PR 6).

Builds an OpenAI-compatible Chat Completions request at ``https://api.x.ai/v1``.
Every image call MUST set ``store=False`` / ``store_messages=False`` and an
explicit ``reasoning_effort``. The wrapper refuses to send otherwise.
``GPSIFY_DRY_RUN=1`` blocks live HTTP (same guard as the geocoder).
"""

from __future__ import annotations

import base64
import json
from collections.abc import Mapping
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
from gpsify.types import PromptContext, Usage, VisionCallOptions, VisionGuess

PROMPT_VERSION = "PROMPT_V1"
XAI_BASE_URL = "https://api.x.ai/v1"
XAI_CHAT_COMPLETIONS_PATH = "/chat/completions"
MAX_IMAGE_BYTES = 20 * 1024 * 1024

PROMPT_V1 = """You are geolocating a travel/landmark photograph for the owner of the photo.

Look at visible evidence only: signs, language/script, architecture, vegetation, road markings, vehicles, skyline, weather/season consistency with the stated capture time.

Return a specific named place you would type into a map search box (e.g. "Clérigos Church, Porto, Portugal"), not a prose paragraph. Prefer a landmark or city name. Do not return a street address with a house number.

Give 1–3 candidates when uncertain. Prefer a correct city over a wrong street.

If this appears to be a private home, hotel room, generic indoor party, or a close-up with no geographic signal, set refusal accordingly. Do not invent a neighborhood.

Do not identify private individuals or infer a home address from a window, doorbell, or street number on a house.

lat_hint/lng_hint are optional approximations; the place name is the source of truth.

If an album hint or filename is provided, treat it as a prior, not as proof. Say whether you used it.
"""


def format_user_message(context: PromptContext) -> str:
    """Assemble the user text. Never includes a full home path."""
    if context.no_context:
        return "Task: infer where this photo was taken."
    hint = context.hint or ""
    captured = " ".join(
        part for part in (context.datetime_original or "", context.offset or "") if part
    )
    camera = " ".join(part for part in (context.make or "", context.model or "") if part)
    lines = [
        f"Album hint (user-provided, optional): {hint}",
        f"Folder name: {context.folder_basename}",
        f"Filename: {context.filename}",
        f"Capture time (EXIF, may be wrong TZ): {captured}",
        f"Camera: {camera}",
        "Cluster siblings already proposed (max 8):",
    ]
    if context.proposed_siblings:
        for sibling in context.proposed_siblings[:8]:
            ts = sibling.captured_at or ""
            cc = sibling.country_code or ""
            lines.append(
                f"  - {ts} {sibling.filename} → {sibling.place_name}, {cc} "
                f"({sibling.granularity.value}, {sibling.model_confidence})"
            )
    else:
        lines.append("  (none)")
    lines.append("Already-tagged siblings in this cluster (max 4):")
    if context.gps_siblings:
        for sibling in context.gps_siblings[:4]:
            lines.append(
                f"  - {sibling.filename} @ {sibling.lat:.4f}, {sibling.lng:.4f}"
            )
    else:
        lines.append("  (none)")
    lines.append("Task: infer where this photo was taken.")
    return "\n".join(lines)


def jpeg_data_url(image_jpeg: bytes) -> str:
    encoded = base64.standard_b64encode(image_jpeg).decode("ascii")
    return f"data:image/jpeg;base64,{encoded}"


def vision_guess_json_schema() -> dict[str, Any]:
    return VisionGuess.model_json_schema(mode="serialization")


def build_xai_request(
    image_jpeg: bytes,
    context: PromptContext,
    options: VisionCallOptions,
    *,
    model: str,
) -> dict[str, Any]:
    """Build the Chat Completions JSON body. Does not send.

    Tests inspect this dict: ``store`` / ``store_messages`` must be False and
    ``reasoning_effort`` must be present. The image is a data URL, never a
    public URL of the user's photo.
    """
    assert_safe_vision_options(options)
    if not image_jpeg:
        raise VisionError("refusing to send an empty image")
    if len(image_jpeg) > MAX_IMAGE_BYTES:
        raise VisionError("image exceeds 20 MiB; refusing to send")
    payload: dict[str, Any] = {
        "model": model,
        "store": False,
        "store_messages": False,
        "reasoning_effort": options.reasoning_effort,
        "messages": [
            {"role": "system", "content": PROMPT_V1},
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": jpeg_data_url(image_jpeg),
                            "detail": "high",
                        },
                    },
                    {"type": "text", "text": format_user_message(context)},
                ],
            },
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "VisionGuess",
                "strict": True,
                "schema": vision_guess_json_schema(),
            },
        },
    }
    assert_safe_xai_payload(payload)
    return payload


def assert_safe_xai_payload(payload: Mapping[str, Any]) -> None:
    """Last-chance refuse: do not send if storage is on or reasoning is omitted."""
    if payload.get("store") is not False:
        raise VisionContractError("refusing to send: store must be False")
    if payload.get("store_messages") is not False:
        raise VisionContractError("refusing to send: store_messages must be False")
    effort = payload.get("reasoning_effort")
    if effort is None or str(effort).strip() == "":
        raise VisionContractError("refusing to send: reasoning_effort is required")


def parse_usage(payload: Mapping[str, Any]) -> Usage:
    usage = payload.get("usage")
    if not isinstance(usage, Mapping):
        return Usage()
    input_tokens = _as_int(usage.get("input_tokens"), usage.get("prompt_tokens"))
    output_tokens = _as_int(usage.get("output_tokens"), usage.get("completion_tokens"))
    details = usage.get("output_tokens_details") or usage.get("completion_tokens_details")
    reasoning = 0
    if isinstance(details, Mapping):
        reasoning = _as_int(details.get("reasoning_tokens"))
    if reasoning == 0:
        reasoning = _as_int(usage.get("reasoning_tokens"))
    return Usage(
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        reasoning_tokens=reasoning,
    )


def parse_vision_guess(payload: Mapping[str, Any]) -> VisionGuess:
    choices = payload.get("choices")
    if not isinstance(choices, list) or not choices:
        raise VisionParseError("empty parse: no choices")
    message = choices[0].get("message") if isinstance(choices[0], Mapping) else None
    if not isinstance(message, Mapping):
        raise VisionParseError("empty parse: no message")
    refusal = message.get("refusal")
    if refusal:
        raise VisionParseError(f"content policy: {refusal}")
    content = message.get("content")
    if not isinstance(content, str) or not content.strip():
        raise VisionParseError("empty parse: no content")
    try:
        return VisionGuess.model_validate_json(content)
    except Exception:
        try:
            return VisionGuess.model_validate(json.loads(content))
        except Exception as exc:
            raise VisionParseError("empty parse: content is not VisionGuess JSON") from exc


def _as_int(*values: Any) -> int:
    for value in values:
        if value is None:
            continue
        try:
            return int(value)
        except (TypeError, ValueError):
            continue
    return 0


class XaiGrokProvider:
    """Official v1 vision implementation. Fresh chat per photo; no response-id reuse."""

    name = "xai"

    def __init__(
        self,
        api_key: str,
        *,
        model: str,
        client: httpx.AsyncClient | None = None,
        base_url: str = XAI_BASE_URL,
    ) -> None:
        if not api_key.strip():
            raise VisionError("XAI_API_KEY is missing")
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
        payload = build_xai_request(
            image_jpeg, context, options, model=self.model
        )
        if live_http_blocked():
            raise VisionError("GPSIFY_DRY_RUN=1 blocks live xAI HTTP")
        client = await self._ensure_client(options.timeout_s)
        url = f"{self._base_url}{XAI_CHAT_COMPLETIONS_PATH}"
        try:
            response = await client.post(
                url,
                json=payload,
                headers={
                    "Authorization": f"Bearer {self._api_key}",
                    "Content-Type": "application/json",
                },
                timeout=options.timeout_s,
            )
        except httpx.TimeoutException as exc:
            raise VisionRetryableError("xAI timeout") from exc
        except httpx.HTTPError as exc:
            raise VisionError(f"xAI HTTP error: {exc}") from exc
        if response.status_code == 429 or response.status_code >= 500:
            raise VisionRetryableError(f"xAI HTTP {response.status_code}")
        if response.status_code != 200:
            raise VisionParseError(f"xAI HTTP {response.status_code}")
        try:
            body = response.json()
        except ValueError as exc:
            raise VisionParseError("xAI returned non-JSON") from exc
        if not isinstance(body, dict):
            raise VisionParseError("xAI returned non-object JSON")
        return parse_vision_guess(body), parse_usage(body)

    async def _ensure_client(self, timeout_s: float) -> httpx.AsyncClient:
        if self._client is not None:
            return self._client
        if live_http_blocked():
            raise VisionError("GPSIFY_DRY_RUN=1 blocks live xAI HTTP")
        self._client = httpx.AsyncClient(
            timeout=timeout_s,
            follow_redirects=True,
        )
        return self._client
