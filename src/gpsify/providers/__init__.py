"""Vision providers. v1 ships xAI grok-4.6 only; Gemini is post-v1 (PR 10)."""

from gpsify.providers.base import (
    FakeVisionProvider,
    VisionContractError,
    VisionError,
    VisionParseError,
    VisionProvider,
    VisionRetryableError,
    assert_safe_vision_options,
)
from gpsify.providers.xai_grok import (
    PROMPT_VERSION,
    PROMPT_V1,
    XAI_BASE_URL,
    XaiGrokProvider,
    build_xai_request,
    format_user_message,
)

__all__ = [
    "PROMPT_V1",
    "PROMPT_VERSION",
    "XAI_BASE_URL",
    "FakeVisionProvider",
    "VisionContractError",
    "VisionError",
    "VisionParseError",
    "VisionProvider",
    "VisionRetryableError",
    "XaiGrokProvider",
    "assert_safe_vision_options",
    "build_xai_request",
    "format_user_message",
]
