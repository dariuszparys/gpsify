"""Vision-model bake-off on photos whose real GPS you already have (PR 10).

Answers the question no public leaderboard can: *for your photos, which model
pins closest, and what does it actually cost?* Every number in
``docs/research/vision-model-selection.md`` comes from someone else's images.

**Ground truth is the album's own EXIF GPS.** Point this at a folder of photos
you have already geotagged; each model sees the same 1280 px preview, returns a
place name, and that name is geocoded through the normal pipeline. The distance
from the resulting pin to the photo's real coordinates is the score.

Nothing here writes GPS or mutates a photo. It reads EXIF, renders previews in
memory, calls the vision API, and geocodes -- the same reads ``propose`` does,
minus the ``proposal`` row.

    uv run python scripts/bakeoff.py ~/Pictures/porto-2019 \\
        --model xai:grok-4.6 \\
        --model gemini:gemini-2.5-flash \\
        --model gemini:gemini-3.7-flash \\
        --limit 20 --out bakeoff.json

Costs real money: one vision call per photo per model. Start with ``--limit``.
``GPSIFY_DRY_RUN=1`` prints the plan and spends nothing.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import sys
import time
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from gpsify.config import Settings, load_settings
from gpsify.exif import MediaKind, classify_media, is_processable, read_exif_batch
from gpsify.geocode import (
    GeocodeCache,
    GeocodeService,
    NominatimGeocoder,
    geocode_cache_path,
    require_geocoder_ready,
)
from gpsify.geocode.base import live_http_blocked
from gpsify.media import MediaError, hash_file, render_preview
from gpsify.propose import PROVIDER_KEY_ENV, estimate_usd
from gpsify.providers import GeminiProvider, VisionError, VisionProvider, XaiGrokProvider
from gpsify.reconcile import geocode_and_reconcile, haversine_m
from gpsify.types import ALLOWED_VISION_PROVIDERS, PromptContext, Usage, VisionCallOptions

# The thresholds the geolocation literature reports (IMAGEO-Bench, EarthWhere,
# MAPBench all use some subset), so a run here is comparable to the papers.
ACC_THRESHOLDS_KM = (1.0, 25.0, 200.0, 750.0)


@dataclass
class Shot:
    """One photo scored against one model."""

    filename: str
    place_name: str | None
    refusal: str
    km: float | None
    usage: Usage
    latency_ms: int
    error: str | None = None


@dataclass
class ModelResult:
    provider: str
    model: str
    shots: list[Shot] = field(default_factory=list)

    @property
    def label(self) -> str:
        return f"{self.provider}:{self.model}"


@dataclass
class Truth:
    """A photo with known coordinates, ready to send."""

    path: Path
    kind: MediaKind
    sha256: str
    lat: float
    lng: float
    datetime_original: str | None
    make: str | None
    model: str | None


# --- metrics (pure; unit-tested) -------------------------------------------


def accuracy_at(shots: Sequence[Shot], km: float) -> float:
    """Share of *all* scored photos pinned within ``km``.

    The denominator is every photo sent, not every photo answered -- a model
    that refuses half the album has not earned a 100% hit rate on the rest.
    """
    if not shots:
        return 0.0
    hits = sum(1 for shot in shots if shot.km is not None and shot.km <= km)
    return hits / len(shots)


def median_km(shots: Sequence[Shot]) -> float | None:
    distances = [shot.km for shot in shots if shot.km is not None]
    if not distances:
        return None
    return statistics.median(distances)


def mean_usage(shots: Sequence[Shot]) -> Usage:
    """Observed per-photo token profile -- the input to any cost coefficient."""
    scored = [shot for shot in shots if shot.error is None]
    if not scored:
        return Usage()
    count = len(scored)
    return Usage(
        input_tokens=round(sum(s.usage.input_tokens for s in scored) / count),
        output_tokens=round(sum(s.usage.output_tokens for s in scored) / count),
        reasoning_tokens=round(sum(s.usage.reasoning_tokens for s in scored) / count),
    )


def total_usd(result: ModelResult) -> float:
    return sum(estimate_usd(shot.usage, result.model) for shot in result.shots)


def summarize(result: ModelResult) -> dict[str, Any]:
    shots = result.shots
    scored = [s for s in shots if s.error is None]
    refused = [s for s in scored if s.refusal != "none"]
    usd = total_usd(result)
    return {
        "provider": result.provider,
        "model": result.model,
        "photos": len(shots),
        "errors": len(shots) - len(scored),
        "refused": len(refused),
        "refusal_rate": len(refused) / len(scored) if scored else 0.0,
        "accuracy": {
            f"acc@{int(km)}km": round(accuracy_at(shots, km), 4)
            for km in ACC_THRESHOLDS_KM
        },
        "median_km": median_km(shots),
        "mean_usage": mean_usage(shots).model_dump(),
        "mean_latency_ms": (
            round(statistics.mean(s.latency_ms for s in scored)) if scored else 0
        ),
        "usd_total": round(usd, 6),
        "usd_per_photo": round(usd / len(shots), 6) if shots else 0.0,
    }


def render_table(summaries: Sequence[dict[str, Any]]) -> str:
    header = (
        f"{'model':28} {'n':>3} {'ref':>4} "
        + " ".join(f"{'@' + str(int(km)) + 'km':>7}" for km in ACC_THRESHOLDS_KM)
        + f" {'med km':>8} {'in':>6} {'out':>6} {'think':>6} {'$/photo':>9} {'$ tot':>8}"
    )
    lines = [header, "-" * len(header)]
    for row in summaries:
        usage = row["mean_usage"]
        med = row["median_km"]
        lines.append(
            f"{row['provider'] + ':' + row['model']:28.28} "
            f"{row['photos']:>3} {row['refused']:>4} "
            + " ".join(
                f"{row['accuracy']['acc@' + str(int(km)) + 'km'] * 100:>6.1f}%"
                for km in ACC_THRESHOLDS_KM
            )
            + f" {(f'{med:.1f}' if med is not None else '-'):>8}"
            f" {usage['input_tokens']:>6} {usage['output_tokens']:>6}"
            f" {usage['reasoning_tokens']:>6}"
            f" {row['usd_per_photo']:>9.5f} {row['usd_total']:>8.4f}"
        )
    return "\n".join(lines)


# --- collection ------------------------------------------------------------


def collect_truth(root: Path, *, recursive: bool, limit: int | None) -> list[Truth]:
    """Photos in ``root`` that already carry GPS. Those are the answer key."""
    if root.name.endswith(".photoslibrary"):
        raise SystemExit("error: refusing to read a Photos.app library bundle")
    walker = root.rglob("*") if recursive else root.glob("*")
    paths = sorted(p for p in walker if p.is_file() and is_processable(classify_media(p)))
    if not paths:
        raise SystemExit(f"error: no supported photos under {root}")

    batch = read_exif_batch(paths)
    for warning in batch.warnings:
        print(f"warning: {warning}", file=sys.stderr)

    truths: list[Truth] = []
    for path in paths:
        tags = batch.by_path.get(str(path.resolve()))
        if tags is None or tags.gps_lat is None or tags.gps_lng is None:
            continue
        truths.append(
            Truth(
                path=path,
                kind=classify_media(path),
                sha256=hash_file(path),
                lat=tags.gps_lat,
                lng=tags.gps_lng,
                datetime_original=tags.datetime_original,
                make=tags.make,
                model=tags.model,
            )
        )
    if not truths:
        raise SystemExit(
            f"error: no GPS-tagged photos under {root}. The bake-off scores "
            "guesses against coordinates the photos already carry."
        )
    return truths[:limit] if limit else truths


def parse_model_spec(spec: str) -> tuple[str, str]:
    provider, _, model = spec.partition(":")
    provider = provider.strip().lower()
    model = model.strip()
    if provider not in ALLOWED_VISION_PROVIDERS or not model:
        allowed = "|".join(sorted(ALLOWED_VISION_PROVIDERS))
        raise SystemExit(f"error: --model must be '<{allowed}>:<model-id>'; got {spec!r}")
    return provider, model


def build_provider(provider: str, model: str, settings: Settings) -> VisionProvider:
    key = settings.xai_api_key if provider == "xai" else settings.gemini_api_key
    if not key:
        raise SystemExit(f"error: {PROVIDER_KEY_ENV[provider]} is required for {provider}:{model}")
    if provider == "gemini":
        return GeminiProvider(key, model=model)
    return XaiGrokProvider(key, model=model)


def context_for(truth: Truth, hint: str | None) -> PromptContext:
    return PromptContext(
        hint=hint,
        folder_basename=truth.path.parent.name,
        filename=truth.path.name,
        datetime_original=truth.datetime_original,
        make=truth.make,
        model=truth.model,
    )


async def score_one(
    provider: VisionProvider,
    truth: Truth,
    preview: bytes,
    options: VisionCallOptions,
    service: GeocodeService,
    hint: str | None,
) -> Shot:
    started = time.monotonic()
    try:
        guess, usage = await provider.guess(preview, context_for(truth, hint), options)
    except VisionError as exc:
        return Shot(
            filename=truth.path.name,
            place_name=None,
            refusal="error",
            km=None,
            usage=Usage(),
            latency_ms=int((time.monotonic() - started) * 1000),
            error=str(exc),
        )
    latency_ms = int((time.monotonic() - started) * 1000)
    reconciled = await geocode_and_reconcile(guess, service)
    km: float | None = None
    if reconciled.pin_lat is not None and reconciled.pin_lng is not None:
        km = haversine_m(
            reconciled.pin_lat, reconciled.pin_lng, truth.lat, truth.lng
        ) / 1000.0
    return Shot(
        filename=truth.path.name,
        place_name=guess.primary_place_name,
        refusal=guess.refusal.value,
        km=km,
        usage=usage,
        latency_ms=latency_ms,
    )


async def run_bakeoff(args: argparse.Namespace) -> int:
    settings = load_settings()
    require_geocoder_ready(settings)
    specs = [parse_model_spec(spec) for spec in args.model]
    truths = collect_truth(
        Path(args.album).expanduser(), recursive=args.recursive, limit=args.limit
    )

    plan = f"{len(truths)} GPS-tagged photos x {len(specs)} models = {len(truths) * len(specs)} vision calls"
    if live_http_blocked():
        print(f"dry-run: {plan} (GPSIFY_DRY_RUN=1 blocks every call)")
        for provider, model in specs:
            print(f"  would call {provider}:{model}")
        return 0
    print(plan)

    options = VisionCallOptions(
        store=False,
        reasoning_effort=settings.reasoning_effort,
        timeout_s=settings.timeout_s,
    )
    # Render each preview once and reuse it for every model: the comparison is
    # only fair if the pixels are byte-identical across providers.
    previews: dict[str, bytes] = {}
    for truth in truths:
        try:
            previews[truth.path.name] = render_preview(
                truth.path, truth.kind, expected_sha256=truth.sha256
            )
        except MediaError as exc:
            print(f"warning: skipping {truth.path.name}: {exc}", file=sys.stderr)
    truths = [t for t in truths if t.path.name in previews]

    cache = await GeocodeCache(geocode_cache_path(settings.data_dir)).open()
    nominatim = NominatimGeocoder(
        settings.geocoder_url,
        contact=settings.nominatim_contact,
        provider=settings.geocoder,
    )
    service = GeocodeService(
        nominatim,
        cache,
        lang=settings.geocode_lang,
        uncached_max=args.geocode_uncached_max,
    )
    results: list[ModelResult] = []
    try:
        for provider_key, model in specs:
            client = build_provider(provider_key, model, settings)
            result = ModelResult(provider=provider_key, model=model)
            for index, truth in enumerate(truths, start=1):
                shot = await score_one(
                    client, truth, previews[truth.path.name], options, service, args.hint
                )
                result.shots.append(shot)
                pin = f"{shot.km:.1f} km" if shot.km is not None else shot.refusal
                print(f"  [{result.label}] {index}/{len(truths)} {shot.filename}: {pin}")
            results.append(result)
            aclose = getattr(client, "aclose", None)
            if aclose is not None:
                await aclose()
    finally:
        await nominatim.aclose()
        await cache.close()

    summaries = [summarize(result) for result in results]
    print()
    print(render_table(summaries))
    if service.cap_stopped:
        print("\nwarning: hit the uncached-geocode cap; later photos scored without a pin")
    if args.out:
        Path(args.out).write_text(
            json.dumps(
                {
                    "album": str(Path(args.album).expanduser()),
                    "photos": len(truths),
                    "reasoning_effort": settings.reasoning_effort,
                    "results": summaries,
                    "shots": {
                        result.label: [vars(shot) | {"usage": shot.usage.model_dump()} for shot in result.shots]
                        for result in results
                    },
                },
                indent=2,
            ),
            encoding="utf-8",
        )
        print(f"\nwrote {args.out}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="bakeoff",
        description="Score vision models against photos whose GPS you already have.",
    )
    parser.add_argument("album", help="Folder of already-geotagged photos.")
    parser.add_argument(
        "--model",
        action="append",
        required=True,
        metavar="PROVIDER:MODEL",
        help="Repeatable, e.g. --model gemini:gemini-2.5-flash",
    )
    parser.add_argument("--limit", type=int, default=None, help="Max photos to score.")
    parser.add_argument("--recursive", action="store_true", help="Walk subfolders.")
    parser.add_argument("--hint", default=None, help="Album hint passed to every model.")
    parser.add_argument(
        "--geocode-uncached-max",
        type=int,
        default=50,
        help="Public Nominatim uncached lookup cap for this run.",
    )
    parser.add_argument("--out", default=None, help="Write full results as JSON here.")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    return asyncio.run(run_bakeoff(build_parser().parse_args(argv)))


if __name__ == "__main__":
    raise SystemExit(main())
