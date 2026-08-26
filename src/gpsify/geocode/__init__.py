"""Switchable geocoder, unique-string cache, and OSM attribution."""

from gpsify.geocode.base import (
    DEFAULT_UNCACHED_MAX,
    OSM_ATTRIBUTION,
    PUBLIC_NOMINATIM_CAP_WARNING,
    FakeGeocoder,
    GeocodeConfigError,
    GeocodeError,
    GeocodeService,
    Geocoder,
    LookupResult,
    is_loopback_url,
    is_public_nominatim_url,
    looks_like_house_address,
    pick_best_hit,
    public_nominatim_requires_contact,
    require_geocoder_ready,
)
from gpsify.geocode.cache import (
    CACHE_DB_NAME,
    GeocodeCache,
    cache_key,
    geocode_cache_path,
    normalize_place,
)
from gpsify.geocode.nominatim import NominatimGeocoder, nominatim_user_agent

__all__ = [
    "CACHE_DB_NAME",
    "DEFAULT_UNCACHED_MAX",
    "OSM_ATTRIBUTION",
    "PUBLIC_NOMINATIM_CAP_WARNING",
    "FakeGeocoder",
    "GeocodeCache",
    "GeocodeConfigError",
    "GeocodeError",
    "GeocodeService",
    "Geocoder",
    "LookupResult",
    "NominatimGeocoder",
    "cache_key",
    "geocode_cache_path",
    "is_loopback_url",
    "is_public_nominatim_url",
    "looks_like_house_address",
    "nominatim_user_agent",
    "normalize_place",
    "pick_best_hit",
    "public_nominatim_requires_contact",
    "require_geocoder_ready",
]
