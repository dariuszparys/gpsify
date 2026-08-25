"""The bake-off script's scoring math and album scan (PR 10).

``scripts/`` is not a package, so the module is loaded by path. Nothing here
touches the network: the metric helpers are pure and the scan reads EXIF only.
"""

from __future__ import annotations

import asyncio
import importlib.util
import sys
from pathlib import Path
from typing import Any

import pytest
from jpeg_factory import PORTO_LAT, PORTO_LNG, write_test_jpeg

from gpsify.geocode import FakeGeocoder, GeocodeCache, GeocodeService
from gpsify.media import render_preview
from gpsify.providers import FakeVisionProvider
from gpsify.types import (
    GeoHit,
    Granularity,
    RefusalReason,
    Usage,
    VisionCallOptions,
    VisionGuess,
)

_SPEC = importlib.util.spec_from_file_location(
    "bakeoff", Path(__file__).resolve().parent.parent / "scripts" / "bakeoff.py"
)
assert _SPEC is not None and _SPEC.loader is not None
bakeoff = importlib.util.module_from_spec(_SPEC)
sys.modules["bakeoff"] = bakeoff
_SPEC.loader.exec_module(bakeoff)


def _shot(km: float | None, **overrides: Any) -> Any:
    payload: dict[str, Any] = {
        "filename": "IMG_1042.jpg",
        "place_name": "Porto, Portugal",
        "refusal": "none",
        "km": km,
        "usage": Usage(input_tokens=1000, output_tokens=400, reasoning_tokens=600),
        "latency_ms": 1200,
    }
    payload.update(overrides)
    return bakeoff.Shot(**payload)


# --- metrics ---------------------------------------------------------------


def test_accuracy_counts_refusals_against_the_model() -> None:
    """A model that refuses half the album has not earned 100% on the rest."""
    shots = [_shot(0.4), _shot(None, refusal="indoor_generic", place_name=None)]
    assert bakeoff.accuracy_at(shots, 1.0) == 0.5
    assert bakeoff.accuracy_at(shots, 750.0) == 0.5


def test_accuracy_thresholds_are_inclusive_and_nested() -> None:
    shots = [_shot(1.0), _shot(25.0), _shot(900.0)]
    assert bakeoff.accuracy_at(shots, 1.0) == pytest.approx(1 / 3)
    assert bakeoff.accuracy_at(shots, 25.0) == pytest.approx(2 / 3)
    assert bakeoff.accuracy_at(shots, 750.0) == pytest.approx(2 / 3)
    assert bakeoff.accuracy_at([], 1.0) == 0.0


def test_median_km_ignores_photos_that_never_got_a_pin() -> None:
    shots = [_shot(2.0), _shot(4.0), _shot(None, refusal="privacy_sensitive")]
    assert bakeoff.median_km(shots) == 3.0
    assert bakeoff.median_km([_shot(None)]) is None


def test_mean_usage_excludes_errored_calls() -> None:
    shots = [
        _shot(1.0),
        _shot(None, refusal="error", error="timeout", usage=Usage()),
    ]
    # The errored call billed nothing and must not halve the observed profile.
    assert bakeoff.mean_usage(shots) == Usage(
        input_tokens=1000, output_tokens=400, reasoning_tokens=600
    )
    assert bakeoff.mean_usage([]) == Usage()


def test_summarize_reports_cost_from_the_scored_model() -> None:
    result = bakeoff.ModelResult(provider="gemini", model="gemini-2.5-flash")
    result.shots = [_shot(0.5), _shot(None, refusal="indoor_generic")]
    row = bakeoff.summarize(result)
    assert row["photos"] == 2
    assert row["refused"] == 1
    assert row["refusal_rate"] == 0.5
    assert row["accuracy"]["acc@1km"] == 0.5
    assert row["median_km"] == 0.5
    # 2 calls x (1000 in @ $0.30/M + 1000 out+think @ $2.50/M)
    assert row["usd_total"] == pytest.approx(2 * (1000 * 0.30 + 1000 * 2.50) / 1e6)
    assert row["usd_per_photo"] == pytest.approx(row["usd_total"] / 2)


def test_render_table_survives_a_model_that_pinned_nothing() -> None:
    result = bakeoff.ModelResult(provider="xai", model="grok-4.6")
    result.shots = [_shot(None, refusal="insufficient_evidence")]
    table = bakeoff.render_table([bakeoff.summarize(result)])
    assert "xai:grok-4.6" in table
    assert "@750km" in table


# --- model specs -----------------------------------------------------------


@pytest.mark.parametrize(
    "spec, expected",
    [
        ("gemini:gemini-2.5-flash", ("gemini", "gemini-2.5-flash")),
        ("  XAI : grok-4.6 ", ("xai", "grok-4.6")),
    ],
)
def test_parse_model_spec_accepts_known_providers(
    spec: str, expected: tuple[str, str]
) -> None:
    assert bakeoff.parse_model_spec(spec) == expected


@pytest.mark.parametrize("spec", ["openai:gpt-5", "gemini", "gemini:", ":grok-4.6"])
def test_parse_model_spec_rejects_the_rest(spec: str) -> None:
    with pytest.raises(SystemExit, match="--model must be"):
        bakeoff.parse_model_spec(spec)


# --- album scan ------------------------------------------------------------


def test_collect_truth_keeps_only_photos_that_carry_the_answer(tmp_path: Path) -> None:
    album = tmp_path / "porto"
    album.mkdir()
    write_test_jpeg(album / "tagged.jpg", gps=(PORTO_LAT, PORTO_LNG))
    write_test_jpeg(album / "untagged.jpg", datetime_original="2019:06:14 18:04:00")
    truths = bakeoff.collect_truth(album, recursive=False, limit=None)
    assert [t.path.name for t in truths] == ["tagged.jpg"]
    assert truths[0].lat == pytest.approx(PORTO_LAT, abs=1e-4)
    assert truths[0].lng == pytest.approx(PORTO_LNG, abs=1e-4)


def test_collect_truth_honours_limit_and_recursion(tmp_path: Path) -> None:
    album = tmp_path / "trip"
    nested = album / "day1"
    nested.mkdir(parents=True)
    for name in ("a.jpg", "b.jpg"):
        write_test_jpeg(album / name, gps=(PORTO_LAT, PORTO_LNG))
    write_test_jpeg(nested / "c.jpg", gps=(PORTO_LAT, PORTO_LNG))
    assert len(bakeoff.collect_truth(album, recursive=False, limit=None)) == 2
    assert len(bakeoff.collect_truth(album, recursive=True, limit=None)) == 3
    assert len(bakeoff.collect_truth(album, recursive=True, limit=2)) == 2


def test_collect_truth_explains_an_album_with_no_ground_truth(tmp_path: Path) -> None:
    album = tmp_path / "untagged"
    album.mkdir()
    write_test_jpeg(album / "a.jpg", datetime_original="2019:06:14 18:04:00")
    with pytest.raises(SystemExit, match="no GPS-tagged photos"):
        bakeoff.collect_truth(album, recursive=False, limit=None)


# --- scoring pipeline ------------------------------------------------------


def test_score_one_measures_the_geocoded_pin_against_the_real_gps(
    tmp_path: Path,
) -> None:
    """End to end offline: guess -> geocode -> distance from the photo's own GPS."""
    album = tmp_path / "porto"
    album.mkdir()
    photo = album / "IMG_1042.jpg"
    write_test_jpeg(photo, gps=(PORTO_LAT, PORTO_LNG))
    truth = bakeoff.collect_truth(album, recursive=False, limit=None)[0]

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
    geocoder = FakeGeocoder()
    # ~1.1 km north of the fixture's coordinates.
    geocoder.add(
        "Clérigos Church, Porto, Portugal",
        GeoHit(
            query="Clérigos Church, Porto, Portugal",
            display_name="Clérigos, Porto, Portugal",
            lat=PORTO_LAT + 0.01,
            lng=PORTO_LNG,
            provider="fake",
        ),
    )

    async def scenario() -> Any:
        cache = await GeocodeCache(tmp_path / "geocode.sqlite").open()
        service = GeocodeService(geocoder, cache)
        try:
            return await bakeoff.score_one(
                FakeVisionProvider(guess_result=guess),
                truth,
                render_preview(truth.path, truth.kind, expected_sha256=truth.sha256),
                VisionCallOptions(store=False, reasoning_effort="low", timeout_s=120.0),
                service,
                None,
            )
        finally:
            await cache.close()

    shot = asyncio.run(scenario())
    assert shot.refusal == "none"
    assert shot.place_name == "Clérigos Church, Porto, Portugal"
    assert shot.km == pytest.approx(1.11, abs=0.05)
    assert shot.error is None


def test_score_one_records_a_refusal_as_an_unpinned_photo(tmp_path: Path) -> None:
    album = tmp_path / "indoor"
    album.mkdir()
    write_test_jpeg(album / "IMG_2000.jpg", gps=(PORTO_LAT, PORTO_LNG))
    truth = bakeoff.collect_truth(album, recursive=False, limit=None)[0]
    guess = VisionGuess(
        refusal=RefusalReason.indoor_generic,
        primary_place_name=None,
        granularity=Granularity.unknown,
        model_confidence=0.0,
        clues=[],
        candidates=[],
        used_album_hint=False,
        used_filename_hint=False,
    )

    async def scenario() -> Any:
        cache = await GeocodeCache(tmp_path / "geocode.sqlite").open()
        service = GeocodeService(FakeGeocoder(), cache)
        try:
            return await bakeoff.score_one(
                FakeVisionProvider(guess_result=guess),
                truth,
                render_preview(truth.path, truth.kind, expected_sha256=truth.sha256),
                VisionCallOptions(store=False, reasoning_effort="low", timeout_s=120.0),
                service,
                None,
            )
        finally:
            await cache.close()

    shot = asyncio.run(scenario())
    assert shot.refusal == "indoor_generic"
    assert shot.km is None
    # A refusal is a scored photo, not a skipped one.
    assert bakeoff.accuracy_at([shot], 750.0) == 0.0


def test_collect_truth_refuses_a_photos_library(tmp_path: Path) -> None:
    bundle = tmp_path / "Photos Library.photoslibrary"
    bundle.mkdir()
    with pytest.raises(SystemExit, match="Photos.app library"):
        bakeoff.collect_truth(bundle, recursive=False, limit=None)
