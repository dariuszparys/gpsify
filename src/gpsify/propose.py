"""Vision + geocode propose loop (PR 6). Does not write GPS.

On start: crash-repair ``proposing`` → ``pending``, take ``jobs/<id>/propose.lock``.
Lazy previews, xAI (or a fake provider in tests), primary-only geocode, UI rank,
proposal rows. ``--dry-run`` is network-free (planned calls; cache reads allowed).

``job.stats_json`` is merged, never replaced: spend (counts, tokens, USD,
geocode counters, refusal histogram) is cumulative for the life of the job, so a
run that plans nothing cannot zero what an earlier run really spent. This run
alone is kept under ``propose.last_run``.
"""

from __future__ import annotations

import asyncio
import json
import os
import statistics
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path
from typing import Any, Never, TextIO

from gpsify.config import Settings
from gpsify.exif import classify_media, is_processable
from gpsify.geocode import (
    DEFAULT_UNCACHED_MAX,
    PUBLIC_NOMINATIM_CAP_WARNING,
    FakeGeocoder,
    GeocodeCache,
    GeocodeConfigError,
    GeocodeService,
    Geocoder,
    geocode_cache_path,
    is_public_nominatim_url,
    require_geocoder_ready,
)
from gpsify.geocode.nominatim import NominatimGeocoder
from gpsify.media import MediaError, ensure_preview
from gpsify.providers import (
    GeminiProvider,
    VisionError,
    VisionProvider,
    VisionRetryableError,
    XaiGrokProvider,
)
from gpsify.providers.xai_grok import PROMPT_VERSION
from gpsify.reconcile import ReconcileResult, geocode_and_reconcile
from gpsify.store import JobRepo, StoreError, job_dir
from gpsify.types import (
    ALLOWED_VISION_PROVIDERS,
    Granularity,
    PhotoStatus,
    PinSource,
    PromptContext,
    ProposedSibling,
    RefusalReason,
    TaggedSibling,
    Usage,
    VisionCallOptions,
    VisionGuess,
)

RETRY_BACKOFF_S = (2.0, 8.0, 30.0)
DEFAULT_CONCURRENCY = 3
UI_RANK_REFUSE = 0.25
SIGN_TEXT_KINDS = frozenset({"sign", "text"})
# grok-4.6 published rates below 200k prompt (docs.x.ai). Reasoning billed as output.
USD_PER_MILLION_INPUT = 2.0
USD_PER_MILLION_OUTPUT = 6.0
# Per-model (input, output) USD per 1M tokens. Order-of-magnitude priors for the
# running estimate, never a quote. Thinking/reasoning tokens bill as output on
# both providers. Unknown models fall back to the grok-4.6 rates above, which
# over- rather than under-state every Gemini model here.
# Gemini rates verified against ai.google.dev/gemini-api/docs/pricing, paid tier,
# 2026-08-25. grok-4.6 from docs.x.ai (<200k prompt).
USD_PER_MILLION: dict[str, tuple[float, float]] = {
    "grok-4.6": (2.0, 6.0),
    "gemini-2.5-pro": (1.25, 10.0),
    "gemini-2.5-flash": (0.30, 2.50),
    "gemini-2.5-flash-lite": (0.10, 0.40),
    "gemini-3.5-flash": (1.50, 9.00),
    "gemini-3.5-flash-lite": (0.30, 2.50),
    "gemini-3.6-flash": (0.75, 3.75),
    "gemini-3.7-flash": (0.75, 3.75),
}
# Google's 3.6/3.7 Flash launch pricing is promotional and doubles on this date.
# Without this, every estimate silently halves itself from New Year's Day.
PROMO_UNTIL = date(2026, 12, 31)
USD_PER_MILLION_AFTER_PROMO: dict[str, tuple[float, float]] = {
    "gemini-3.6-flash": (1.50, 7.50),
    "gemini-3.7-flash": (1.50, 7.50),
}
LOCK_FILENAME = "propose.lock"
PROVIDER_KEY_ENV = {"xai": "XAI_API_KEY", "gemini": "GEMINI_API_KEY"}
# Gemini has no per-request store=False. Retention is a property of the key's
# tier, and gpsify cannot detect the tier from the API, so it says so out loud.
GEMINI_TIER_WARNING = (
    "gemini has no per-request store=False; free-tier keys may be used to "
    "improve Google products. Use a billing-enabled key for real photos."
)

_HINT_STOPWORDS = frozenset(
    {
        "a",
        "album",
        "an",
        "and",
        "at",
        "from",
        "in",
        "of",
        "or",
        "photos",
        "the",
        "to",
        "trip",
        "vacation",
        "weekend",
        "year",
    }
)

SleepFn = Callable[[float], Awaitable[None]]
ProgressFn = Callable[[str], None]


class ProposeError(Exception):
    """User-facing propose failure (CLI exit 2)."""


class ProposePartialError(Exception):
    """Some photos ended in ``error`` (CLI exit 1)."""

    def __init__(self, summary: ProposeSummary) -> None:
        super().__init__("propose finished with errors")
        self.summary = summary


def _unreachable(value: Never) -> Never:
    raise ProposeError(f"unhandled value: {value!r}")


def clip01(value: float) -> float:
    return max(0.0, min(1.0, value))


def estimate_usd(usage: Usage, model: str | None = None, on: date | None = None) -> float:
    """Order-of-magnitude prior from published list rates, not a quote.

    Reasoning/thinking tokens bill as output on both providers. ``on`` overrides
    today's date so the promotional-rate rollover is testable.
    """
    key = (model or "").strip()
    rate_in, rate_out = USD_PER_MILLION.get(
        key, (USD_PER_MILLION_INPUT, USD_PER_MILLION_OUTPUT)
    )
    if (on or date.today()) > PROMO_UNTIL and key in USD_PER_MILLION_AFTER_PROMO:
        rate_in, rate_out = USD_PER_MILLION_AFTER_PROMO[key]
    output = usage.output_tokens + usage.reasoning_tokens
    return (usage.input_tokens * rate_in + output * rate_out) / 1_000_000.0


def granularity_score(granularity: Granularity) -> float:
    match granularity:
        case Granularity.exact:
            return 1.0
        case Granularity.street:
            return 0.85
        case Granularity.neighborhood:
            return 0.7
        case Granularity.city:
            return 0.55
        case Granularity.region:
            return 0.3
        case Granularity.country:
            return 0.15
        case Granularity.unknown:
            return 0.0
        case _:
            _unreachable(granularity)


def clue_score(guess: VisionGuess) -> float:
    n_clues = len(guess.clues)
    bonus = 0.15 if any(clue.kind in SIGN_TEXT_KINDS for clue in guess.clues) else 0.0
    return clip01(min(1.0, 0.25 * n_clues + bonus))


def geocode_score(result: ReconcileResult) -> float:
    match result.pin_source:
        case PinSource.geocoder:
            if result.model_agree is False:
                return 0.6
            return 1.0
        case PinSource.user:
            return 1.0
        case PinSource.none:
            return 0.2
        case _:
            _unreachable(result.pin_source)


def hint_names_city(hint: str | None) -> bool:
    """True when the album hint looks like it names a city (not just 'weekend trip')."""
    if not hint or not hint.strip():
        return False
    tokens = [token for token in _split_hint_tokens(hint) if token]
    for token in tokens:
        lowered = token.lower()
        if lowered in _HINT_STOPWORDS:
            continue
        if token[0].isupper() and len(token) >= 3:
            return True
        if len(lowered) >= 4:
            return True
    return False


def _split_hint_tokens(hint: str) -> list[str]:
    buf: list[str] = []
    current: list[str] = []
    for char in hint:
        if char.isalpha() or char in "-'":
            current.append(char)
        else:
            if current:
                buf.append("".join(current))
                current = []
    if current:
        buf.append("".join(current))
    return buf


def cluster_score(
    guess: VisionGuess,
    proposed_siblings: Sequence[ProposedSibling],
    gps_siblings: Sequence[TaggedSibling],
) -> float:
    if not proposed_siblings and not gps_siblings:
        return 0.0
    total = len(proposed_siblings) + len(gps_siblings)
    if total == 0:
        return 0.0
    guess_cc = (guess.country_code or "").strip().upper()
    guess_place = (guess.primary_place_name or "").strip().lower()
    matches = 0
    for sibling in proposed_siblings:
        sib_cc = (sibling.country_code or "").strip().upper()
        if guess_cc and sib_cc and guess_cc == sib_cc:
            matches += 1
            continue
        if guess_place and guess_place in sibling.place_name.lower():
            matches += 1
    return matches / total


def ui_rank(
    guess: VisionGuess,
    result: ReconcileResult,
    proposed_siblings: Sequence[ProposedSibling],
    gps_siblings: Sequence[TaggedSibling],
) -> float:
    return clip01(
        0.45 * clip01(guess.model_confidence)
        + 0.20 * clip01(granularity_score(guess.granularity))
        + 0.15 * clip01(clue_score(guess))
        + 0.10 * clip01(geocode_score(result))
        + 0.10 * clip01(cluster_score(guess, proposed_siblings, gps_siblings))
    )


def refuse_guess_reason(
    guess: VisionGuess,
    *,
    album_hint: str | None,
    rank: float,
) -> str | None:
    if guess.refusal is not RefusalReason.none:
        return "refusal"
    has_sign_text = any(clue.kind in SIGN_TEXT_KINDS for clue in guess.clues)
    weak_granularity = guess.granularity in {Granularity.unknown, Granularity.country}
    if weak_granularity and not has_sign_text and not hint_names_city(album_hint):
        return "weak_granularity"
    if rank < UI_RANK_REFUSE:
        return "low_ui_rank"
    return None


def _pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


class JobExclusiveLock:
    """Exclusive pid lock at ``jobs/<id>/propose.lock`` (shared later by apply)."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self._held = False

    def acquire(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        if self.path.is_file():
            existing = _read_pid(self.path)
            if existing is not None and _pid_alive(existing):
                raise ProposeError(
                    f"another propose/apply holds the job lock (pid {existing})"
                )
            try:
                self.path.unlink()
            except FileNotFoundError:
                pass
        try:
            fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
        except FileExistsError as exc:
            raise ProposeError("another propose/apply holds the job lock") from exc
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(str(os.getpid()))
        self._held = True

    def release(self) -> None:
        if not self._held:
            return
        self._held = False
        try:
            if _read_pid(self.path) == os.getpid():
                self.path.unlink()
        except FileNotFoundError:
            pass

    def __enter__(self) -> JobExclusiveLock:
        self.acquire()
        return self

    def __exit__(self, *exc: object) -> None:
        self.release()


def _read_pid(path: Path) -> int | None:
    try:
        raw = path.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    if not raw:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


@dataclass
class ProposeSummary:
    job_id: str
    planned: int = 0
    proposed: int = 0
    refused: int = 0
    error: int = 0
    skipped_existing: int = 0
    skipped_no_preview: int = 0
    dry_run: bool = False
    cache_hits: int = 0
    uncached_lookups: int = 0
    est_usd: float = 0.0
    input_tokens: int = 0
    output_tokens: int = 0
    reasoning_tokens: int = 0
    warnings: list[str] = field(default_factory=list)
    planned_relpaths: list[str] = field(default_factory=list)


def _eligible_statuses(*, force: bool) -> frozenset[PhotoStatus]:
    base = {PhotoStatus.pending, PhotoStatus.error}
    if force:
        base.update({PhotoStatus.proposed, PhotoStatus.refused})
    return frozenset(base)


async def run_propose(
    job_id: str,
    *,
    settings: Settings,
    limit: int | None = None,
    concurrency: int = DEFAULT_CONCURRENCY,
    dry_run: bool = False,
    force: bool = False,
    no_context: bool = False,
    provider_name: str | None = None,
    geocode_uncached_max: int = DEFAULT_UNCACHED_MAX,
    vision: VisionProvider | None = None,
    geocoder: Geocoder | None = None,
    sleep: SleepFn = asyncio.sleep,
    progress: ProgressFn | None = None,
) -> ProposeSummary:
    """Run propose. Does not write GPS. Raises ``ProposeError`` on usage/config."""
    if concurrency < 1:
        raise ProposeError("--concurrency must be >= 1")
    if limit is not None and limit < 1:
        raise ProposeError("--limit must be >= 1")
    provider_key = (provider_name or settings.vision_provider).strip().lower()
    if vision is None and provider_key not in ALLOWED_VISION_PROVIDERS:
        allowed = ", ".join(sorted(ALLOWED_VISION_PROVIDERS))
        raise ProposeError(f"--provider must be one of {allowed}; got {provider_key!r}")
    if not dry_run and vision is None and not _api_key(settings, provider_key):
        raise ProposeError(
            f"{PROVIDER_KEY_ENV[provider_key]} is required for propose "
            f"--provider {provider_key}"
        )
    try:
        require_geocoder_ready(settings)
    except GeocodeConfigError as exc:
        raise ProposeError(str(exc)) from exc

    data_dir = settings.data_dir
    root = job_dir(data_dir, job_id)
    lock = JobExclusiveLock(root / LOCK_FILENAME)
    lock.acquire()
    repo: JobRepo | None = None
    cache: GeocodeCache | None = None
    owned_vision: XaiGrokProvider | GeminiProvider | None = None
    nominatim: NominatimGeocoder | None = None
    log_handle: TextIO | None = None
    try:
        try:
            repo = await JobRepo.open(data_dir, job_id)
        except StoreError as exc:
            raise ProposeError(str(exc)) from exc
        await repo.reset_stale_proposing()
        job = await repo.get_job()
        if job is None:
            raise ProposeError(f"job row missing for {job_id}")
        photos = await repo.list_photos()
        current_by_photo = {
            row["photo_id"]: row for row in await repo.list_current_proposals()
        }
        selected = _select_photos(photos, current_by_photo, force=force, limit=limit)
        summary = ProposeSummary(
            job_id=job_id,
            planned=len(selected),
            dry_run=dry_run,
            planned_relpaths=[str(row["relpath"]) for row in selected],
        )
        if dry_run:
            cache = await GeocodeCache(geocode_cache_path(data_dir)).open()
            # Cache reads are allowed; we cannot know place names without vision.
            summary.cache_hits = 0
            _emit(
                progress,
                f"dry-run: {summary.planned} vision calls planned, "
                f"{summary.cache_hits} geocode cache hits "
                f"(no {provider_key}, no public Nominatim)",
            )
            return summary

        cache = await GeocodeCache(geocode_cache_path(data_dir)).open()
        resolved_geocoder, nominatim = _resolve_geocoder(settings, geocoder)
        cap_uncached = is_public_nominatim_url(settings.geocoder_url) and not isinstance(
            resolved_geocoder, FakeGeocoder
        )
        service = GeocodeService(
            resolved_geocoder,
            cache,
            lang=settings.geocode_lang,
            uncached_max=geocode_uncached_max,
            cap_uncached=cap_uncached,
        )
        resolved_vision, owned_vision = _resolve_vision(settings, vision, provider_key)
        if owned_vision is not None and owned_vision.name == "gemini":
            summary.warnings.append(GEMINI_TIER_WARNING)
            _emit(progress, f"warning: {GEMINI_TIER_WARNING}")
        log_dir = root / "logs"
        log_dir.mkdir(parents=True, exist_ok=True)
        log_handle = (log_dir / "propose.log").open("a", encoding="utf-8")
        write_lock = asyncio.Lock()
        latencies: list[float] = []
        refusal_hist: dict[str, int] = {}
        hint = job["hint"]
        folder_basename = Path(str(job["root_path"])).name
        sem = asyncio.Semaphore(concurrency)

        async def process(photo: dict[str, Any]) -> None:
            async with sem:
                await _process_one(
                    repo,
                    photo,
                    photos=photos,
                    current_by_photo=current_by_photo,
                    write_lock=write_lock,
                    summary=summary,
                    settings=settings,
                    vision=resolved_vision,
                    service=service,
                    job_root=root,
                    hint=hint,
                    folder_basename=folder_basename,
                    no_context=no_context,
                    sleep=sleep,
                    progress=progress,
                    log_handle=log_handle,
                    latencies=latencies,
                    refusal_hist=refusal_hist,
                )

        await asyncio.gather(*(process(photo) for photo in selected))
        if service.cap_stopped:
            warning = PUBLIC_NOMINATIM_CAP_WARNING
            summary.warnings.append(warning)
            _emit(progress, f"warning: {warning}")
        summary.cache_hits = service.cache_hits
        summary.uncached_lookups = service.uncached_lookups
        await _write_stats(
            repo,
            summary=summary,
            latencies=latencies,
            refusal_hist=refusal_hist,
            cache_hits=service.cache_hits,
            uncached=service.uncached_lookups,
        )
        _emit(
            progress,
            f"{summary.proposed}/{summary.planned} proposed, "
            f"{summary.refused} refused, {summary.error} error, "
            f"~${summary.est_usd:.2f} est (incl. reasoning_tokens)",
        )
        return summary
    finally:
        if log_handle is not None:
            log_handle.close()
        if cache is not None:
            await cache.close()
        if nominatim is not None:
            await nominatim.aclose()
        if owned_vision is not None:
            await owned_vision.aclose()
        if repo is not None:
            await repo.close()
        lock.release()


def _resolve_geocoder(
    settings: Settings, geocoder: Geocoder | None
) -> tuple[Geocoder, NominatimGeocoder | None]:
    if geocoder is not None:
        return geocoder, None
    client = NominatimGeocoder(
        settings.geocoder_url,
        contact=settings.nominatim_contact,
        provider=settings.geocoder,
    )
    return client, client


def _api_key(settings: Settings, provider_key: str) -> str | None:
    match provider_key:
        case "xai":
            return settings.xai_api_key
        case "gemini":
            return settings.gemini_api_key
        case _:
            return None


def _resolve_vision(
    settings: Settings, vision: VisionProvider | None, provider_key: str
) -> tuple[VisionProvider, XaiGrokProvider | GeminiProvider | None]:
    if vision is not None:
        return vision, None
    api_key = _api_key(settings, provider_key)
    assert api_key is not None
    client: XaiGrokProvider | GeminiProvider
    match provider_key:
        case "gemini":
            client = GeminiProvider(api_key, model=settings.gemini_model)
        case _:
            client = XaiGrokProvider(api_key, model=settings.model)
    return client, client


def _select_photos(
    photos: Sequence[dict[str, Any]],
    current_by_photo: dict[str, dict[str, Any]],
    *,
    force: bool,
    limit: int | None,
) -> list[dict[str, Any]]:
    allowed = {status.value for status in _eligible_statuses(force=force)}
    selected: list[dict[str, Any]] = []
    for photo in photos:
        if photo["status"] not in allowed:
            continue
        if not is_processable(classify_media(Path(str(photo["path"])))):
            continue
        if not force and photo["id"] in current_by_photo:
            continue
        selected.append(photo)
    selected.sort(key=lambda row: str(row["relpath"]))
    if limit is not None:
        selected = selected[:limit]
    return selected


async def _process_one(
    repo: JobRepo,
    photo: dict[str, Any],
    *,
    photos: Sequence[dict[str, Any]],
    current_by_photo: dict[str, dict[str, Any]],
    write_lock: asyncio.Lock,
    summary: ProposeSummary,
    settings: Settings,
    vision: VisionProvider,
    service: GeocodeService,
    job_root: Path,
    hint: str | None,
    folder_basename: str,
    no_context: bool,
    sleep: SleepFn,
    progress: ProgressFn | None,
    log_handle: TextIO | None,
    latencies: list[float],
    refusal_hist: dict[str, int],
) -> None:
    photo_id = str(photo["id"])
    source = Path(str(photo["path"]))
    async with write_lock:
        await repo.set_photo_status(photo_id, PhotoStatus.proposing, error=None)
    proposed_siblings, gps_siblings = _cluster_context(
        photo, photos, current_by_photo
    )
    routing_name = Path(str(photo["relpath"])).name
    context = PromptContext(
        hint=None if no_context else hint,
        folder_basename="" if no_context else folder_basename,
        filename=routing_name,
        datetime_original=None if no_context else photo["datetime_original"],
        offset=None,
        make=None if no_context else photo["make"],
        model=None if no_context else photo["model"],
        proposed_siblings=[] if no_context else proposed_siblings,
        gps_siblings=[] if no_context else gps_siblings,
        no_context=no_context,
    )

    try:
        preview = ensure_preview(
            job_root,
            photo_id,
            source,
            expected_sha256=str(photo["sha256"]),
            kind=classify_media(source),
        )
        image_jpeg = preview.read_bytes()
    except MediaError as exc:
        async with write_lock:
            await repo.set_photo_status(photo_id, exc.status, error=str(exc))
        if exc.status is PhotoStatus.skipped_no_preview:
            summary.skipped_no_preview += 1
        else:
            summary.error += 1
        _emit(progress, f"{routing_name}: {exc.status.value}")
        return

    options = VisionCallOptions(
        store=False,
        reasoning_effort=settings.reasoning_effort,
        timeout_s=settings.timeout_s,
    )
    started = time.perf_counter()
    try:
        guess, usage = await _guess_with_retries(
            vision, image_jpeg, context, options, sleep=sleep
        )
    except VisionError as exc:
        latency_ms = int((time.perf_counter() - started) * 1000)
        async with write_lock:
            await repo.set_photo_status(photo_id, PhotoStatus.error, error=str(exc))
        summary.error += 1
        latencies.append(float(latency_ms))
        _log_event(
            log_handle,
            {
                "level": "error",
                "photo_id": photo_id,
                "event": "vision_error",
                "ms": latency_ms,
                "error": str(exc),
            },
        )
        _emit(progress, f"{routing_name}: error")
        return

    latency_ms = int((time.perf_counter() - started) * 1000)
    latencies.append(float(latency_ms))
    result = await geocode_and_reconcile(guess, service)
    rank = ui_rank(guess, result, proposed_siblings, gps_siblings)
    refuse = refuse_guess_reason(
        guess,
        album_hint=None if no_context else hint,
        rank=rank,
    )
    photo_status = result.photo_status
    pin_source = result.pin_source
    pin_lat = result.pin_lat
    pin_lng = result.pin_lng
    model_agree = result.model_agree
    if refuse is not None:
        photo_status = PhotoStatus.refused
        if refuse == "refusal":
            pin_source = PinSource.none
            pin_lat = None
            pin_lng = None
            model_agree = None

    geocode_json = None
    if result.geocode is not None:
        dump = result.geocode.model_dump()
        dump["skipped_geocode"] = result.skipped_geocode
        dump["model_lat"] = result.model_lat
        dump["model_lng"] = result.model_lng
        geocode_json = json.dumps(dump)
    elif result.skipped_geocode:
        geocode_json = json.dumps({"skipped_geocode": result.skipped_geocode})

    agree_int: int | None
    if model_agree is None:
        agree_int = None
    else:
        agree_int = int(model_agree)

    async with write_lock:
        await repo.insert_proposal(
            photo_id=photo_id,
            provider=vision.name,
            model=vision.model,
            prompt_version=PROMPT_VERSION,
            raw_json=guess.model_dump_json(),
            place_name=guess.primary_place_name,
            granularity=guess.granularity.value,
            model_confidence=guess.model_confidence,
            ui_rank=rank,
            refusal=guess.refusal.value,
            pin_lat=pin_lat,
            pin_lng=pin_lng,
            pin_source=pin_source,
            model_agree=agree_int,
            geocode_json=geocode_json,
            input_tokens=usage.input_tokens,
            output_tokens=usage.output_tokens,
            reasoning_tokens=usage.reasoning_tokens,
            latency_ms=latency_ms,
            current=True,
        )
        await repo.set_photo_status(photo_id, photo_status, error=None)
        current_by_photo[photo_id] = {
            "photo_id": photo_id,
            "place_name": guess.primary_place_name,
            "granularity": guess.granularity.value,
            "model_confidence": guess.model_confidence,
            "country_code": guess.country_code,
        }
        # Keep cluster context fresh for later photos.
        photo["status"] = photo_status.value

    usd = estimate_usd(usage, vision.model)
    summary.est_usd += usd
    summary.input_tokens += usage.input_tokens
    summary.output_tokens += usage.output_tokens
    summary.reasoning_tokens += usage.reasoning_tokens
    if photo_status is PhotoStatus.proposed:
        summary.proposed += 1
    elif photo_status is PhotoStatus.refused:
        summary.refused += 1
        refusal_hist[guess.refusal.value] = refusal_hist.get(guess.refusal.value, 0) + 1
    else:
        summary.error += 1

    _log_event(
        log_handle,
        {
            "level": "info",
            "photo_id": photo_id,
            "event": "vision_ok",
            "model": vision.model,
            "ms": latency_ms,
            "place": guess.primary_place_name,
            "ui_rank": round(rank, 4),
            "in": usage.input_tokens,
            "out": usage.output_tokens,
            "reason": usage.reasoning_tokens,
            "est_usd": round(usd, 6),
            "status": photo_status.value,
        },
    )
    _emit(
        progress,
        f"{summary.proposed}/{summary.planned} proposed, "
        f"{summary.refused} refused, {summary.error} error, "
        f"~${summary.est_usd:.2f} est (incl. reasoning_tokens)",
    )


async def _guess_with_retries(
    vision: VisionProvider,
    image_jpeg: bytes,
    context: PromptContext,
    options: VisionCallOptions,
    *,
    sleep: SleepFn,
) -> tuple[VisionGuess, Usage]:
    last: VisionError | None = None
    delays = (0.0, *RETRY_BACKOFF_S)
    for delay in delays:
        if delay:
            await sleep(delay)
        try:
            return await vision.guess(image_jpeg, context, options)
        except VisionRetryableError as exc:
            last = exc
            continue
        except VisionError:
            raise
    assert last is not None
    raise last


def _cluster_context(
    photo: dict[str, Any],
    photos: Sequence[dict[str, Any]],
    current_by_photo: dict[str, dict[str, Any]],
) -> tuple[list[ProposedSibling], list[TaggedSibling]]:
    cluster_id = photo.get("cluster_id")
    if not cluster_id:
        return [], []
    members = [row for row in photos if row.get("cluster_id") == cluster_id]
    members.sort(key=lambda row: (row["datetime_original"] or "", row["relpath"]))
    proposed: list[ProposedSibling] = []
    gps: list[TaggedSibling] = []
    for member in members:
        if member["id"] == photo["id"]:
            continue
        filename = Path(str(member["relpath"])).name
        if member["has_gps"] and member["existing_lat"] is not None:
            if member["existing_lng"] is not None and len(gps) < 4:
                gps.append(
                    TaggedSibling(
                        filename=filename,
                        lat=float(member["existing_lat"]),
                        lng=float(member["existing_lng"]),
                    )
                )
            continue
        current = current_by_photo.get(str(member["id"]))
        if current is None or len(proposed) >= 8:
            continue
        place = current.get("place_name") or ""
        granularity_raw = current.get("granularity") or Granularity.unknown.value
        try:
            granularity = Granularity(granularity_raw)
        except ValueError:
            granularity = Granularity.unknown
        confidence = current.get("model_confidence")
        proposed.append(
            ProposedSibling(
                filename=filename,
                captured_at=member["datetime_original"],
                place_name=place,
                country_code=current.get("country_code"),
                granularity=granularity,
                model_confidence=float(confidence) if confidence is not None else 0.0,
            )
        )
    return proposed[:8], gps[:4]


def _prior_stats(job: Any) -> tuple[dict[str, Any], dict[str, Any]]:
    """Return ``(whole stats dict, prior propose block)`` from ``job.stats_json``."""
    existing: dict[str, Any] = {}
    if job is not None and job["stats_json"]:
        try:
            loaded = json.loads(job["stats_json"])
            if isinstance(loaded, dict):
                existing = loaded
        except json.JSONDecodeError:
            existing = {}
    prior = existing.get("propose")
    return existing, prior if isinstance(prior, dict) else {}


def _prior_number(prior: Mapping[str, Any], key: str) -> float:
    """Previous total, or 0.0 when absent, null, or not a number."""
    value = prior.get(key)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0.0
    return float(value)


def _prior_optional_number(prior: Mapping[str, Any], key: str) -> float | None:
    """Previous latency percentile, or None when absent, null, or not a number."""
    value = prior.get(key)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _prior_mapping(prior: Mapping[str, Any], key: str) -> dict[str, Any]:
    value = prior.get(key)
    return dict(value) if isinstance(value, dict) else {}


def _latency_percentiles(
    latencies: Sequence[float],
) -> tuple[float | None, float | None]:
    if not latencies:
        return None, None
    ordered = sorted(latencies)
    idx = min(len(ordered) - 1, int(round(0.95 * (len(ordered) - 1))))
    return statistics.median(latencies), ordered[idx]


def _hit_rate(cache_hits: float, uncached: float) -> float | None:
    lookups = cache_hits + uncached
    return (cache_hits / lookups) if lookups else None


async def _write_stats(
    repo: JobRepo,
    *,
    summary: ProposeSummary,
    latencies: Sequence[float],
    refusal_hist: dict[str, int],
    cache_hits: int,
    uncached: int,
) -> None:
    """Merge this run into ``job.stats_json``.

    Spend is cumulative for the life of the job: a later run (including one
    that plans nothing) must never zero the tokens and USD an earlier run
    really spent. ``last_run`` carries this invocation on its own.
    """
    existing, prior = _prior_stats(await repo.get_job())
    p50, p95 = _latency_percentiles(latencies)
    run_counts = {
        "planned": summary.planned,
        "proposed": summary.proposed,
        "refused": summary.refused,
        "error": summary.error,
        "skipped_no_preview": summary.skipped_no_preview,
    }
    prior_counts = _prior_mapping(prior, "counts")
    prior_hist = _prior_mapping(prior, "refusal_histogram")
    total_cache_hits = int(_prior_number(prior, "geocode_cache_hits")) + cache_hits
    total_uncached = int(_prior_number(prior, "geocode_uncached")) + uncached
    total_hist = {
        key: int(_prior_number(prior_hist, key)) + refusal_hist.get(key, 0)
        for key in (*prior_hist, *refusal_hist)
    }
    existing["propose"] = {
        "counts": {
            key: int(_prior_number(prior_counts, key)) + value
            for key, value in run_counts.items()
        },
        "latency_ms_p50": (
            p50 if p50 is not None else _prior_optional_number(prior, "latency_ms_p50")
        ),
        "latency_ms_p95": (
            p95 if p95 is not None else _prior_optional_number(prior, "latency_ms_p95")
        ),
        "est_usd": round(_prior_number(prior, "est_usd") + summary.est_usd, 6),
        "geocode_cache_hits": total_cache_hits,
        "geocode_uncached": total_uncached,
        "geocode_cache_hit_rate": _hit_rate(total_cache_hits, total_uncached),
        "refusal_histogram": total_hist,
        "input_tokens": int(_prior_number(prior, "input_tokens")) + summary.input_tokens,
        "output_tokens": int(_prior_number(prior, "output_tokens")) + summary.output_tokens,
        "reasoning_tokens": (
            int(_prior_number(prior, "reasoning_tokens")) + summary.reasoning_tokens
        ),
        "runs": int(_prior_number(prior, "runs")) + 1,
        "last_run": {
            "counts": run_counts,
            "latency_ms_p50": p50,
            "latency_ms_p95": p95,
            "est_usd": round(summary.est_usd, 6),
            "geocode_cache_hits": cache_hits,
            "geocode_uncached": uncached,
            "geocode_cache_hit_rate": _hit_rate(cache_hits, uncached),
            "refusal_histogram": dict(refusal_hist),
            "input_tokens": summary.input_tokens,
            "output_tokens": summary.output_tokens,
            "reasoning_tokens": summary.reasoning_tokens,
        },
    }
    await repo.set_stats_json(json.dumps(existing))


def _log_event(handle: TextIO | None, event: dict[str, Any]) -> None:
    if handle is None:
        return
    payload = {"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), **event}
    handle.write(json.dumps(payload, ensure_ascii=True) + "\n")
    handle.flush()


def _emit(progress: ProgressFn | None, message: str) -> None:
    if progress is not None:
        progress(message)
