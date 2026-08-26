"""Vision providers. The default is xAI grok-4.6; Gemini is the alternative."""

from gpsify.providers.base import (
    FakeVisionProvider,
    VisionBlockedError,
    VisionContractError,
    VisionError,
    VisionParseError,
    VisionProvider,
    VisionRetryableError,
    assert_safe_vision_options,
)
from gpsify.providers.gemini import (
    GEMINI_BASE_URL,
    GEMINI_DEFAULT_MODEL,
    GeminiProvider,
    assert_safe_gemini_payload,
    build_gemini_request,
    to_gemini_schema,
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
    "GEMINI_BASE_URL",
    "GEMINI_DEFAULT_MODEL",
    "PROMPT_V1",
    "PROMPT_VERSION",
    "XAI_BASE_URL",
    "GeminiProvider",
    "FakeVisionProvider",
    "VisionBlockedError",
    "VisionContractError",
    "VisionError",
    "VisionParseError",
    "VisionProvider",
    "VisionRetryableError",
    "XaiGrokProvider",
    "assert_safe_gemini_payload",
    "assert_safe_vision_options",
    "build_gemini_request",
    "build_xai_request",
    "format_user_message",
    "to_gemini_schema",
]
