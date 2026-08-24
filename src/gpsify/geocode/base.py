"""Geocoder protocol, fake provider, OSM attribution, and lookup orchestration (PR 5)."""

from __future__ import annotations

import os
import re
from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Never, Protocol
from urllib.parse import urlparse

from gpsify.config import Settings
from gpsify.geocode.cache import GeocodeCache, cache_key
from gpsify.types import GeoHit, Granularity

DEFAULT_UNCACHED_MAX = 50
OSM_ATTRIBUTION = "© OpenStreetMap contributors"
PUBLIC_NOMINATIM_HOSTS = frozenset(
    {"nominatim.openstreetmap.org", "nominatim.osm.org"}
)
LOOPBACK_HOSTS = frozenset({"127.0.0.1", "localhost", "::1"})
KEYED_GEOCODERS = frozenset({"mapbox", "google"})

# House-number at start plus a street-type token ("12 Oak Street, …").
_STREET_TOKEN = (
    r"(?:street|st\.?|avenue|ave\.?|road|rd\.?|boulevard|blvd\.?|"
    r"lane|ln\.?|drive|dr\.?|court|ct\.?|place|pl\.?|way|terrace|"
    r"circle|close|row|highway|hwy\.?)"
)
_HOUSE_ADDRESS = re.compile(
    rf"^\d+\s+\S.*\b{_STREET_TOKEN}\b",
    re.IGNORECASE,
)

# English short names for display_name country matching (travel-biased, not exhaustive).
_ISO_ENGLISH: dict[str, str] = {
    "AT": "austria",
    "AU": "australia",
    "BE": "belgium",
    "BR": "brazil",
    "CA": "canada",
    "CH": "switzerland",
    "CZ": "czechia",
    "DE": "germany",
    "DK": "denmark",
    "ES": "spain",
    "FI": "finland",
    "FR": "france",
    "GB": "united kingdom",
    "GR": "greece",
    "IE": "ireland",
    "IN": "india",
    "IT": "italy",
    "JP": "japan",
    "MX": "mexico",
    "NL": "netherlands",
    "NO": "norway",
    "NZ": "new zealand",
    "PL": "poland",
    "PT": "portugal",
    "SE": "sweden",
    "US": "united states",
}

PUBLIC_NOMINATIM_CAP_WARNING = (
    "Public Nominatim uncached lookup cap reached; remaining pins left at "
    "pin_source=none. Set geocoder_url to Photon / self-hosted Nominatim / "
    "Mapbox or re-run with --geocode-uncached-max N."
)


class GeocodeError(Exception):
    """Geocoder HTTP or policy failure. Callers treat this as a miss."""


class GeocodeConfigError(Exception):
    """Missing contact / bad geocoder settings (CLI exit 2)."""


class Geocoder(Protocol):
    """Switchable place search. Implementations must not use a stock HTTP UA."""

    name: str

    async def search(
        self,
        query: str,
        *,
        country_code: str | None = None,
        lang: str = "en",
    ) -> list[GeoHit]:
        """Return ranked hits for one query. Empty list is a miss, not an error."""
        ...


def _unreachable(value: Never) -> Never:
    raise GeocodeError(f"unhandled value: {value!r}")


def is_public_nominatim_url(url: str) -> bool:
    host = (urlparse(url).hostname or "").lower()
    return host in PUBLIC_NOMINATIM_HOSTS


def is_loopback_url(url: str) -> bool:
    host = (urlparse(url).hostname or "").lower()
    return host in LOOPBACK_HOSTS


def public_nominatim_requires_contact(settings: Settings) -> bool:
    """Contact email is required only for the donated public Nominatim instance."""
    if settings.geocoder in KEYED_GEOCODERS:
        return False
    return is_public_nominatim_url(settings.geocoder_url)


def require_geocoder_ready(settings: Settings) -> None:
    """Refuse propose/review-style starts that would hit public Nominatim without UA contact."""
    if not public_nominatim_requires_contact(settings):
        return
    if not settings.nominatim_contact:
        raise GeocodeConfigError(
            "nominatim_contact is required for public Nominatim "
            "(set NOMINATIM_CONTACT or nominatim_contact in config.toml)"
        )


def looks_like_house_address(place: str) -> bool:
    """True for strings that look like a house number + street (never send to Nominatim)."""
    stripped = place.strip()
    if not stripped:
        return False
    return _HOUSE_ADDRESS.search(stripped) is not None


def live_http_blocked() -> bool:
    return os.environ.get("GPSIFY_DRY_RUN", "") == "1"


def _country_matches(hit: GeoHit, country_code: str) -> bool:
    code = country_code.strip().lower()
    if not code:
        return False
    name = hit.display_name.lower()
    if re.search(rf"\b{re.escape(code)}\b", name) is not None:
        return True
    english = _ISO_ENGLISH.get(code.upper())
    return english is not None and english in name


def _osm_type_rank(osm_type: str | None, granularity: Granularity) -> int:
    kind = (osm_type or "").lower()
    match granularity:
        case Granularity.exact:
            order = ("node", "way", "relation")
        case (
            Granularity.street
            | Granularity.neighborhood
            | Granularity.city
            | Granularity.region
            | Granularity.country
            | Granularity.unknown
        ):
            order = ("relation", "way", "node")
        case _:
            _unreachable(granularity)
    if kind in order:
        return order.index(kind)
    return len(order)


def pick_best_hit(
    hits: Sequence[GeoHit],
    *,
    country_code: str | None,
    granularity: Granularity,
) -> GeoHit | None:
    """Prefer country_code matches; then relation/way for cities, node for exact POIs."""
    if not hits:
        return None
    indexed = list(enumerate(hits))
    if country_code:
        matched = [item for item in indexed if _country_matches(item[1], country_code)]
        if matched:
            indexed = matched
    indexed.sort(
        key=lambda item: (_osm_type_rank(item[1].osm_type, granularity), item[0])
    )
    return indexed[0][1]


@dataclass
class FakeGeocoder:
    """In-memory geocoder for CI. Never opens a socket."""

    name: str = "fake"
    hits: dict[str, list[GeoHit]] = field(default_factory=dict)
    queries: list[str] = field(default_factory=list)
    error_on: set[str] = field(default_factory=set)

    def add(self, query: str, *hits: GeoHit) -> None:
        self.hits[query] = list(hits)

    async def search(
        self,
        query: str,
        *,
        country_code: str | None = None,
        lang: str = "en",
    ) -> list[GeoHit]:
        _ = (country_code, lang)
        self.queries.append(query)
        if query in self.error_on:
            raise GeocodeError(f"fake geocoder error for {query!r}")
        return list(self.hits.get(query, []))


@dataclass
class LookupResult:
    hit: GeoHit | None
    cache_hit: bool
    capped: bool
    skipped: str | None = None


@dataclass
class GeocodeService:
    """Cache + uncached-cap wrapper. Geocodes the *primary* place name only."""

    geocoder: Geocoder
    cache: GeocodeCache
    lang: str = "en"
    uncached_max: int = DEFAULT_UNCACHED_MAX
    cap_uncached: bool = True
    uncached_lookups: int = 0
    cache_hits: int = 0
    cap_stopped: bool = False

    async def lookup_primary(
        self,
        place: str,
        country_code: str | None,
        granularity: Granularity,
    ) -> LookupResult:
        """Look up one place string. Never geocodes candidate names."""
        if looks_like_house_address(place):
            return LookupResult(
                hit=None, cache_hit=False, capped=False, skipped="house_address"
            )

        key = cache_key(place, country_code, self.lang)
        cached = await self.cache.get(key)
        if cached.found:
            self.cache_hits += 1
            return LookupResult(hit=cached.hit, cache_hit=True, capped=False)

        if self.cap_uncached and self.uncached_lookups >= self.uncached_max:
            self.cap_stopped = True
            return LookupResult(hit=None, cache_hit=False, capped=True)

        self.uncached_lookups += 1
        try:
            hits = await self.geocoder.search(
                place, country_code=country_code, lang=self.lang
            )
        except GeocodeError:
            return LookupResult(hit=None, cache_hit=False, capped=False)

        hit = pick_best_hit(hits, country_code=country_code, granularity=granularity)
        await self.cache.put(key, hit, query=place)
        return LookupResult(hit=hit, cache_hit=False, capped=False)
