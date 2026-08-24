from __future__ import annotations

import asyncio
from collections.abc import Coroutine
from pathlib import Path
from typing import Any, TypeVar
from urllib.parse import parse_qs, urlparse

import httpx
import pytest

from gpsify.config import DEFAULT_GEOCODER_URL, load_settings
from gpsify.geocode import (
    DEFAULT_UNCACHED_MAX,
    OSM_ATTRIBUTION,
    FakeGeocoder,
    GeocodeCache,
    GeocodeConfigError,
    GeocodeError,
    GeocodeService,
    NominatimGeocoder,
    cache_key,
    looks_like_house_address,
    nominatim_user_agent,
    normalize_place,
    pick_best_hit,
    require_geocoder_ready,
)
from gpsify.geocode.nominatim import AsyncRateLimiter, parse_nominatim_hits
from gpsify.reconcile import (
    geocode_and_reconcile,
    haversine_m,
    reconcile_hit,
)
from gpsify.types import GeoHit, Granularity, PhotoStatus, PinSource, RefusalReason, VisionGuess

T = TypeVar("T")

PORTO = "Clérigos Church, Porto, Portugal"
PORTO_LAT = 41.1456
PORTO_LNG = -8.6143


def run(coro: Coroutine[Any, Any, T]) -> T:
    return asyncio.run(coro)


def _guess(**overrides: Any) -> VisionGuess:
    payload: dict[str, Any] = {
        "refusal": RefusalReason.none,
        "primary_place_name": PORTO,
        "country_code": "PT",
        "granularity": Granularity.exact,
        "model_confidence": 0.8,
        "clues": [{"kind": "architecture", "text": "baroque tower", "weight": 0.7}],
        "candidates": [
            {
                "place_name": "Lisbon, Portugal",
                "country_code": "PT",
                "confidence": 0.2,
                "why": "decoy",
            }
        ],
        "used_album_hint": False,
        "used_filename_hint": False,
    }
    payload.update(overrides)
    return VisionGuess.model_validate(payload)


def _hit(query: str = PORTO, **overrides: Any) -> GeoHit:
    payload: dict[str, Any] = {
        "query": query,
        "display_name": "Igreja dos Clérigos, Cedofeita, Porto, Portugal",
        "lat": PORTO_LAT,
        "lng": PORTO_LNG,
        "osm_type": "way",
        "osm_id": "1",
        "provider": "fake",
    }
    payload.update(overrides)
    return GeoHit.model_validate(payload)


async def _reconcile(
    tmp_path: Path,
    guess: VisionGuess,
    geocoder: FakeGeocoder,
    **service_kw: Any,
):
    async with GeocodeCache(tmp_path / "geocode.sqlite") as cache:
        service = GeocodeService(geocoder, cache, **service_kw)
        result = await geocode_and_reconcile(guess, service)
        return result, service


def test_osm_attribution_exported_for_ui() -> None:
    assert "OpenStreetMap" in OSM_ATTRIBUTION
    assert OSM_ATTRIBUTION.startswith("©")


def test_normalize_and_cache_key() -> None:
    assert normalize_place("  Clerigos Church, Porto!!! ") == "clerigos church, porto"
    assert normalize_place("ﬁord") == "fiord"
    key = cache_key("Porto, Portugal", "PT", "EN")
    assert key == "porto, portugal|pt|en"


def test_house_address_heuristic() -> None:
    assert looks_like_house_address("12 Oak Street, Portland")
    assert looks_like_house_address("1600 Pennsylvania Avenue")
    assert not looks_like_house_address(PORTO)
    assert not looks_like_house_address("Times Square, New York")


def test_default_uncached_cap_is_50() -> None:
    assert DEFAULT_UNCACHED_MAX == 50


def test_contact_required_for_public_nominatim(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("NOMINATIM_CONTACT", raising=False)
    monkeypatch.setenv("GPSIFY_DATA_DIR", str(tmp_path))
    settings = load_settings(_env_file=None, geocoder_url=DEFAULT_GEOCODER_URL)
    with pytest.raises(GeocodeConfigError, match="nominatim_contact"):
        require_geocoder_ready(settings)

    settings = load_settings(
        _env_file=None,
        geocoder_url="http://127.0.0.1:8080/search",
    )
    require_geocoder_ready(settings)

    settings = load_settings(
        _env_file=None,
        geocoder="mapbox",
        geocoder_url=DEFAULT_GEOCODER_URL,
    )
    require_geocoder_ready(settings)

    settings = load_settings(
        _env_file=None,
        geocoder="google",
        geocoder_url=DEFAULT_GEOCODER_URL,
    )
    require_geocoder_ready(settings)


def test_nominatim_user_agent_is_identifiable() -> None:
    ua = nominatim_user_agent("dev@example.com")
    assert ua == "gpsify/0.1 (personal; dev@example.com)"
    assert "python-httpx" not in ua
    assert "python-requests" not in ua


def test_public_nominatim_refuses_missing_contact() -> None:
    with pytest.raises(GeocodeConfigError, match="nominatim_contact"):
        NominatimGeocoder(DEFAULT_GEOCODER_URL, contact=None)


def test_dry_run_blocks_live_nominatim_http() -> None:
    geo = NominatimGeocoder("http://127.0.0.1:8080/search", contact=None)
    with pytest.raises(GeocodeError, match="GPSIFY_DRY_RUN"):
        run(geo.search("Porto"))


def test_nominatim_mock_transport_parses_json_and_sets_ua() -> None:
    seen: dict[str, str] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["url"] = str(request.url)
        seen["ua"] = request.headers["user-agent"]
        return httpx.Response(
            200,
            json=[
                {
                    "lat": "41.1456",
                    "lon": "-8.6143",
                    "display_name": "Igreja dos Clérigos, Porto, Portugal",
                    "osm_type": "way",
                    "osm_id": 42,
                    "boundingbox": ["41.14", "41.15", "-8.62", "-8.61"],
                    "importance": 0.7,
                }
            ],
        )

    async def _go() -> list[GeoHit]:
        client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        geo = NominatimGeocoder(
            "https://example.test/search",
            contact="dev@example.com",
            client=client,
            rate_limiter=AsyncRateLimiter(0.0),
        )
        try:
            return await geo.search("Clérigos Church, Porto", country_code="PT", lang="en")
        finally:
            await client.aclose()

    hits = run(_go())
    assert len(hits) == 1
    assert hits[0].lat == pytest.approx(41.1456)
    assert hits[0].lng == pytest.approx(-8.6143)
    assert hits[0].boundingbox == (41.14, 41.15, -8.62, -8.61)
    assert hits[0].osm_id == "42"
    assert seen["ua"] == "gpsify/0.1 (personal; dev@example.com)"
    parsed = urlparse(seen["url"])
    assert parsed.path.endswith("/search")
    qs = parse_qs(parsed.query)
    assert qs["q"] == ["Clérigos Church, Porto"]
    assert qs["format"] == ["jsonv2"]
    assert qs["countrycodes"] == ["pt"]


def test_parse_nominatim_skips_broken_rows() -> None:
    hits = parse_nominatim_hits(
        [{"lat": "x", "lon": "y"}, {"lat": "1", "lon": "2", "display_name": "Ok"}],
        query="q",
        provider="nominatim",
    )
    assert len(hits) == 1
    assert hits[0].display_name == "Ok"


def test_rate_limiter_enforces_one_request_per_second() -> None:
    class FakeClock:
        def __init__(self) -> None:
            self.now = 100.0
            self.sleeps: list[float] = []

        def monotonic(self) -> float:
            return self.now

        async def sleep(self, seconds: float) -> None:
            self.sleeps.append(seconds)
            self.now += seconds

    clock = FakeClock()
    limiter = AsyncRateLimiter(1.0, monotonic=clock.monotonic, sleep=clock.sleep)

    async def _go() -> None:
        await limiter.acquire()
        await limiter.acquire()

    run(_go())
    assert clock.sleeps == [1.0]


def test_cache_hit_skips_geocoder(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _hit())
    guess = _guess()

    async def _go() -> tuple[int, int]:
        async with GeocodeCache(tmp_path / "c.sqlite") as cache:
            service = GeocodeService(geocoder, cache)
            await geocode_and_reconcile(guess, service)
            await geocode_and_reconcile(guess, service)
            return service.uncached_lookups, service.cache_hits

    uncached, cached = run(_go())
    assert geocoder.queries == [PORTO]
    assert uncached == 1
    assert cached == 1


def test_agree_within_exact_threshold(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _hit())
    guess = _guess(lat_hint=PORTO_LAT + 0.001, lng_hint=PORTO_LNG)
    result, _service = run(_reconcile(tmp_path, guess, geocoder))
    assert result.photo_status is PhotoStatus.proposed
    assert result.pin_source is PinSource.geocoder
    assert result.pin_lat == pytest.approx(PORTO_LAT)
    assert result.pin_lng == pytest.approx(PORTO_LNG)
    assert result.model_agree is True
    assert result.model_lat is None
    assert result.geocode is not None
    assert haversine_m(PORTO_LAT, PORTO_LNG, guess.lat_hint or 0, guess.lng_hint or 0) < 500


def test_disagree_keeps_geocoder_pin(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _hit())
    guess = _guess(lat_hint=41.5, lng_hint=PORTO_LNG)
    result, _service = run(_reconcile(tmp_path, guess, geocoder))
    assert result.pin_source is PinSource.geocoder
    assert result.pin_lat == pytest.approx(PORTO_LAT)
    assert result.model_agree is False
    assert result.model_lat == pytest.approx(41.5)
    assert result.model_lng == pytest.approx(PORTO_LNG)
    assert haversine_m(PORTO_LAT, PORTO_LNG, 41.5, PORTO_LNG) > 500


def test_geocode_miss_is_pin_none(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    result, service = run(_reconcile(tmp_path, _guess(), geocoder))
    assert result.pin_source is PinSource.none
    assert result.photo_status is PhotoStatus.proposed
    assert result.skipped_geocode == "miss"
    assert service.uncached_lookups == 1
    assert geocoder.queries == [PORTO]


def test_refusal_never_geocodes(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _hit())
    for reason in (
        RefusalReason.privacy_sensitive,
        RefusalReason.policy_refusal,
        RefusalReason.indoor_generic,
        RefusalReason.insufficient_evidence,
        RefusalReason.not_a_photograph,
    ):
        geocoder.queries.clear()
        guess = _guess(refusal=reason)
        result, service = run(_reconcile(tmp_path, guess, geocoder))
        assert result.photo_status is PhotoStatus.refused
        assert result.pin_source is PinSource.none
        assert result.skipped_geocode == "refusal"
        assert geocoder.queries == []
        assert service.uncached_lookups == 0


def test_empty_primary_is_refused(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    result, _service = run(
        _reconcile(tmp_path, _guess(primary_place_name=None), geocoder)
    )
    assert result.photo_status is PhotoStatus.refused
    assert result.skipped_geocode == "empty_primary"
    assert geocoder.queries == []


def test_house_address_is_not_queried(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    geocoder.add("12 Oak Street, Portland", _hit(query="12 Oak Street, Portland"))
    guess = _guess(primary_place_name="12 Oak Street, Portland", country_code="US")
    result, service = run(_reconcile(tmp_path, guess, geocoder))
    assert result.pin_source is PinSource.none
    assert result.photo_status is PhotoStatus.proposed
    assert result.skipped_geocode == "house_address"
    assert geocoder.queries == []
    assert service.uncached_lookups == 0


def test_uncached_cap_stops_further_lookups(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    places = ["Porto, Portugal", "Lisbon, Portugal", "Faro, Portugal"]
    for place in places:
        geocoder.add(place, _hit(query=place, display_name=place))

    async def _go() -> list[str]:
        async with GeocodeCache(tmp_path / "c.sqlite") as cache:
            service = GeocodeService(geocoder, cache, uncached_max=2, cap_uncached=True)
            reasons: list[str] = []
            for place in places:
                result = await geocode_and_reconcile(
                    _guess(primary_place_name=place, country_code=None),
                    service,
                )
                reasons.append(result.skipped_geocode or "ok")
            assert service.uncached_lookups == 2
            assert service.cap_stopped is True
            return reasons

    reasons = run(_go())
    assert reasons == ["ok", "ok", "uncached_cap"]
    assert geocoder.queries == places[:2]


def test_primary_only_does_not_geocode_candidates(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    geocoder.add(PORTO, _hit())
    geocoder.add("Lisbon, Portugal", _hit(query="Lisbon, Portugal", lat=38.72, lng=-9.14))
    result, _service = run(_reconcile(tmp_path, _guess(), geocoder))
    assert result.pin_lat == pytest.approx(PORTO_LAT)
    assert geocoder.queries == [PORTO]


def test_pick_best_hit_prefers_country_and_osm_type() -> None:
    us = _hit(
        display_name="Porto, NY, United States",
        osm_type="node",
        lat=43.0,
        lng=-78.0,
    )
    pt_node = _hit(display_name="Porto, Portugal", osm_type="node", osm_id="2")
    pt_rel = _hit(display_name="Porto, Portugal", osm_type="relation", osm_id="3")
    chosen = pick_best_hit(
        [us, pt_node, pt_rel],
        country_code="PT",
        granularity=Granularity.city,
    )
    assert chosen is not None
    assert chosen.osm_type == "relation"
    assert chosen.osm_id == "3"

    poi = pick_best_hit(
        [pt_rel, pt_node],
        country_code="PT",
        granularity=Granularity.exact,
    )
    assert poi is not None
    assert poi.osm_type == "node"


def test_no_model_hint_agrees_with_geocoder() -> None:
    hit = _hit()
    result = reconcile_hit(_guess(), hit)
    assert result.model_agree is True
    assert result.model_lat is None


def test_haversine_porto_lisbon_order_of_magnitude() -> None:
    meters = haversine_m(41.1496, -8.6109, 38.7223, -9.1393)
    assert 270_000 < meters < 290_000


def test_geocoder_error_is_a_miss_not_cached(tmp_path: Path) -> None:
    geocoder = FakeGeocoder(error_on={PORTO})
    guess = _guess()

    async def _go() -> None:
        async with GeocodeCache(tmp_path / "c.sqlite") as cache:
            service = GeocodeService(geocoder, cache)
            first = await geocode_and_reconcile(guess, service)
            assert first.skipped_geocode == "miss"
            geocoder.error_on.clear()
            geocoder.add(PORTO, _hit())
            second = await geocode_and_reconcile(guess, service)
            assert second.pin_source is PinSource.geocoder
            assert service.uncached_lookups == 2

    run(_go())
    assert geocoder.queries == [PORTO, PORTO]


def test_cached_miss_does_not_requery(tmp_path: Path) -> None:
    geocoder = FakeGeocoder()
    guess = _guess()

    async def _go() -> int:
        async with GeocodeCache(tmp_path / "c.sqlite") as cache:
            service = GeocodeService(geocoder, cache)
            await geocode_and_reconcile(guess, service)
            await geocode_and_reconcile(guess, service)
            return service.cache_hits

    hits =     run(_go())
    assert geocoder.queries == [PORTO]
    assert hits == 1
