"""Nominatim-shaped HTTP geocoder (PR 5).

Speaks the public Nominatim search JSON at ``geocoder_url`` so a self-hosted
Nominatim (or any compatible endpoint) is a config change, not a code change.
Photon / Mapbox / Google adapters are not in this slice.

Every request sets an identifiable User-Agent. Stock HTTP-library UAs are
forbidden. Public Nominatim is rate-limited to 1 request/second.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable
from typing import Any

import httpx

from gpsify.geocode.base import (
    GeocodeConfigError,
    GeocodeError,
    is_public_nominatim_url,
    live_http_blocked,
)
from gpsify.types import GeoHit

# Design UA (OSMF identifiable product token). Not the setuptools 0.1.0 version.
NOMINATIM_UA_PRODUCT = "gpsify/0.1"

SleepFn = Callable[[float], Awaitable[None]]
MonotonicFn = Callable[[], float]


def nominatim_user_agent(contact: str | None) -> str:
    if contact:
        return f"{NOMINATIM_UA_PRODUCT} (personal; {contact})"
    return f"{NOMINATIM_UA_PRODUCT} (personal; self-hosted)"


class AsyncRateLimiter:
    """Serialize calls so at most one proceeds per ``min_interval_s`` (Nominatim: 1 rps)."""

    def __init__(
        self,
        min_interval_s: float = 1.0,
        *,
        monotonic: MonotonicFn = time.monotonic,
        sleep: SleepFn = asyncio.sleep,
    ) -> None:
        self._min_interval_s = min_interval_s
        self._monotonic = monotonic
        self._sleep = sleep
        self._lock = asyncio.Lock()
        self._next_ok_at = 0.0

    async def acquire(self) -> None:
        async with self._lock:
            now = self._monotonic()
            wait = self._next_ok_at - now
            if wait > 0:
                await self._sleep(wait)
                now = self._monotonic()
            self._next_ok_at = now + self._min_interval_s


def parse_nominatim_hits(
    payload: Any,
    *,
    query: str,
    provider: str,
) -> list[GeoHit]:
    if not isinstance(payload, list):
        return []
    hits: list[GeoHit] = []
    for item in payload:
        if not isinstance(item, dict):
            continue
        parsed = _parse_one(item, query=query, provider=provider)
        if parsed is not None:
            hits.append(parsed)
    return hits


def _parse_one(item: dict[str, Any], *, query: str, provider: str) -> GeoHit | None:
    try:
        lat = float(item["lat"])
        lng = float(item["lon"])
    except (KeyError, TypeError, ValueError):
        return None
    bbox_raw = item.get("boundingbox")
    boundingbox: tuple[float, float, float, float] | None = None
    if isinstance(bbox_raw, list) and len(bbox_raw) == 4:
        try:
            south, north, west, east = (float(part) for part in bbox_raw)
            boundingbox = (south, north, west, east)
        except (TypeError, ValueError):
            boundingbox = None
    osm_id = item.get("osm_id")
    importance = item.get("importance")
    try:
        importance_f = float(importance) if importance is not None else None
    except (TypeError, ValueError):
        importance_f = None
    return GeoHit(
        query=query,
        display_name=str(item.get("display_name") or ""),
        lat=lat,
        lng=lng,
        boundingbox=boundingbox,
        osm_type=str(item["osm_type"]) if item.get("osm_type") is not None else None,
        osm_id=str(osm_id) if osm_id is not None else None,
        importance=importance_f,
        provider=provider,
    )


class NominatimGeocoder:
    """HTTP client for Nominatim search JSON at a configurable URL."""

    name = "nominatim"

    def __init__(
        self,
        url: str,
        *,
        contact: str | None,
        provider: str = "nominatim",
        client: httpx.AsyncClient | None = None,
        rate_limiter: AsyncRateLimiter | None = None,
        timeout_s: float = 30.0,
    ) -> None:
        if is_public_nominatim_url(url) and not contact:
            raise GeocodeConfigError(
                "nominatim_contact is required for public Nominatim"
            )
        self._url = url
        self._provider = provider
        self._user_agent = nominatim_user_agent(contact)
        self._timeout_s = timeout_s
        self._client = client
        self._owns_client = client is None
        self._rate = rate_limiter or AsyncRateLimiter(1.0)

    @property
    def user_agent(self) -> str:
        return self._user_agent

    async def aclose(self) -> None:
        if self._owns_client and self._client is not None:
            await self._client.aclose()
            self._client = None

    async def search(
        self,
        query: str,
        *,
        country_code: str | None = None,
        lang: str = "en",
    ) -> list[GeoHit]:
        client = await self._ensure_client()
        params: dict[str, str] = {
            "q": query,
            "format": "jsonv2",
            "limit": "5",
            "addressdetails": "1",
            "accept-language": lang,
        }
        if country_code:
            params["countrycodes"] = country_code.strip().lower()
        await self._rate.acquire()
        try:
            response = await client.get(
                self._url,
                params=params,
                headers={
                    "User-Agent": self._user_agent,
                    "Accept": "application/json",
                },
            )
        except httpx.TimeoutException as exc:
            raise GeocodeError("geocoder timeout") from exc
        except httpx.HTTPError as exc:
            raise GeocodeError(f"geocoder HTTP error: {exc}") from exc
        if response.status_code != 200:
            raise GeocodeError(f"geocoder HTTP {response.status_code}")
        try:
            payload = response.json()
        except ValueError as exc:
            raise GeocodeError("geocoder returned non-JSON") from exc
        return parse_nominatim_hits(payload, query=query, provider=self._provider)

    async def _ensure_client(self) -> httpx.AsyncClient:
        if self._client is not None:
            return self._client
        if live_http_blocked():
            raise GeocodeError("GPSIFY_DRY_RUN=1 blocks live geocoder HTTP")
        self._client = httpx.AsyncClient(
            timeout=self._timeout_s,
            headers={
                "User-Agent": self._user_agent,
                "Accept": "application/json",
            },
            follow_redirects=True,
        )
        return self._client
