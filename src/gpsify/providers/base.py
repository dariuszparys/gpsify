"""VisionProvider protocol, call-option contract, and fake provider."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Protocol

from gpsify.types import PromptContext, Usage, VisionCallOptions, VisionGuess


class VisionError(Exception):
    """Vision call failed. Callers map this to photo status ``error`` unless noted."""


class VisionContractError(VisionError):
    """Unsafe client flags. The wrapper must refuse to send."""


class VisionParseError(VisionError):
    """HTTP succeeded but the body was empty or unparsable."""


class VisionBlockedError(VisionParseError):
    """The provider's safety filter blocked this image. Terminal, not an error.

    Re-sending the same pixels is billed and blocked again, so ``propose`` maps
    this to ``refused`` / ``policy_refusal`` rather than ``error``, keeping
    the photo out of the retry-eligible set.
    """


class VisionRetryableError(VisionError):
    """429 / 5xx / timeout. Propose retries then marks the photo ``error``."""


def assert_safe_vision_options(options: VisionCallOptions) -> None:
    """Refuse if ``store`` is not False or ``reasoning_effort`` is missing.

    ``VisionCallOptions`` already validates on construction; this is the
    last-chance check the wrapper must run before any HTTP send.
    """
    if options.store is not False:
        raise VisionContractError("store must be False on every vision call")
    effort = getattr(options, "reasoning_effort", None)
    if effort is None or str(effort).strip() == "":
        raise VisionContractError("reasoning_effort is required on every vision call")


class VisionProvider(Protocol):
    name: str
    model: str

    async def guess(
        self,
        image_jpeg: bytes,
        context: PromptContext,
        options: VisionCallOptions,
    ) -> tuple[VisionGuess, Usage]:
        """``image_jpeg`` is already transposed+resized.

        Implementations MUST refuse if ``options.store`` is not False or
        ``options.reasoning_effort`` is missing.
        """
        ...


GuessFactory = Callable[[bytes, PromptContext, VisionCallOptions], VisionGuess]


@dataclass
class FakeVisionProvider:
    """In-memory vision provider for CI. Never opens a socket."""

    name: str = "fake"
    model: str = "fake-vision"
    guess_result: VisionGuess | None = None
    by_filename: dict[str, VisionGuess] = field(default_factory=dict)
    factory: GuessFactory | None = None
    calls: list[tuple[bytes, PromptContext, VisionCallOptions]] = field(
        default_factory=list
    )
    error_on: set[str] = field(default_factory=set)
    blocked_on: set[str] = field(default_factory=set)
    retryable_failures_remaining: int = 0
    usage: Usage = field(default_factory=lambda: Usage(input_tokens=100, output_tokens=50, reasoning_tokens=10))

    async def guess(
        self,
        image_jpeg: bytes,
        context: PromptContext,
        options: VisionCallOptions,
    ) -> tuple[VisionGuess, Usage]:
        assert_safe_vision_options(options)
        self.calls.append((image_jpeg, context, options))
        if self.retryable_failures_remaining > 0:
            self.retryable_failures_remaining -= 1
            raise VisionRetryableError("fake 503")
        if context.filename in self.blocked_on:
            raise VisionBlockedError("content policy: FAKE_BLOCK")
        if context.filename in self.error_on:
            raise VisionError(f"fake vision error for {context.filename!r}")
        if self.factory is not None:
            return self.factory(image_jpeg, context, options), self.usage
        if context.filename in self.by_filename:
            return self.by_filename[context.filename], self.usage
        if self.guess_result is not None:
            return self.guess_result, self.usage
        raise VisionError(f"no fake guess configured for {context.filename!r}")
