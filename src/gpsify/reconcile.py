"""Geocode the primary place name and reconcile model hints with the geocoder pin (PR 5).

Pin shown to the user is always the geocoder lat/lng. Model ``lat_hint``/``lng_hint``
are compared via haversine; disagreement is recorded, never averaged. Candidates
are not geocoded in this slice.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Never

from gpsify.geocode.base import GeocodeService, LookupResult, looks_like_house_address
from gpsify.types import GeoHit, Granularity, PhotoStatus, PinSource, RefusalReason, VisionGuess

EARTH_RADIUS_M = 6_371_000.0


class ReconcileError(Exception):
    """Unexpected reconcile input (programming error)."""


def _unreachable(value: Never) -> Never:
    raise ReconcileError(f"unhandled value: {value!r}")


@dataclass(frozen=True)
class ReconcileResult:
    """Proposal pin + status after geocoding the primary place (or skipping)."""

    pin_source: PinSource
    pin_lat: float | None
    pin_lng: float | None
    model_agree: bool | None
    geocode: GeoHit | None
    photo_status: PhotoStatus
    skipped_geocode: str | None
    model_lat: float | None = None
    model_lng: float | None = None


def haversine_m(lat1: float, lng1: float, lat2: float, lng2: float) -> float:
    phi1, phi2 = math.radians(lat1), math.radians(lat2)
    dphi = math.radians(lat2 - lat1)
    dlmb = math.radians(lng2 - lng1)
    chord = (
        math.sin(dphi / 2) ** 2
        + math.cos(phi1) * math.cos(phi2) * math.sin(dlmb / 2) ** 2
    )
    return 2 * EARTH_RADIUS_M * math.asin(min(1.0, math.sqrt(chord)))


def agreement_threshold_m(granularity: Granularity) -> float | None:
    """Meters. ``None`` means model hints cannot agree (unknown granularity)."""
    match granularity:
        case Granularity.exact | Granularity.street:
            return 500.0
        case Granularity.neighborhood:
            return 2_000.0
        case Granularity.city:
            return 15_000.0
        case Granularity.region:
            return 100_000.0
        case Granularity.country:
            return 500_000.0
        case Granularity.unknown:
            return None
        case _:
            _unreachable(granularity)


def should_geocode(guess: VisionGuess) -> str | None:
    """Return a skip reason, or None if the primary place may be queried."""
    match guess.refusal:
        case RefusalReason.none:
            pass
        case (
            RefusalReason.insufficient_evidence
            | RefusalReason.indoor_generic
            | RefusalReason.privacy_sensitive
            | RefusalReason.policy_refusal
            | RefusalReason.not_a_photograph
        ):
            return "refusal"
        case _:
            _unreachable(guess.refusal)
    primary = (guess.primary_place_name or "").strip()
    if not primary:
        return "empty_primary"
    if looks_like_house_address(primary):
        return "house_address"
    return None


def _refused(reason: str) -> ReconcileResult:
    return ReconcileResult(
        pin_source=PinSource.none,
        pin_lat=None,
        pin_lng=None,
        model_agree=None,
        geocode=None,
        photo_status=PhotoStatus.refused,
        skipped_geocode=reason,
    )


def _no_pin(reason: str) -> ReconcileResult:
    return ReconcileResult(
        pin_source=PinSource.none,
        pin_lat=None,
        pin_lng=None,
        model_agree=None,
        geocode=None,
        photo_status=PhotoStatus.proposed,
        skipped_geocode=reason,
    )


def reconcile_hit(guess: VisionGuess, hit: GeoHit) -> ReconcileResult:
    """Pin = geocoder point. Record model agreement; never average two points."""
    model_lat = guess.lat_hint
    model_lng = guess.lng_hint
    if (
        model_lat is None
        or model_lng is None
        or not math.isfinite(model_lat)
        or not math.isfinite(model_lng)
    ):
        agree: bool | None = True
        ghost_lat = None
        ghost_lng = None
    else:
        threshold = agreement_threshold_m(guess.granularity)
        distance = haversine_m(hit.lat, hit.lng, model_lat, model_lng)
        agree = threshold is not None and distance <= threshold
        ghost_lat = model_lat
        ghost_lng = model_lng
        if agree:
            ghost_lat = None
            ghost_lng = None
    return ReconcileResult(
        pin_source=PinSource.geocoder,
        pin_lat=hit.lat,
        pin_lng=hit.lng,
        model_agree=agree,
        geocode=hit,
        photo_status=PhotoStatus.proposed,
        skipped_geocode=None,
        model_lat=ghost_lat,
        model_lng=ghost_lng,
    )


def result_from_lookup(guess: VisionGuess, lookup: LookupResult) -> ReconcileResult:
    if lookup.skipped == "house_address":
        return _no_pin("house_address")
    if lookup.capped:
        return _no_pin("uncached_cap")
    if lookup.hit is None:
        return _no_pin("miss")
    return reconcile_hit(guess, lookup.hit)


async def geocode_and_reconcile(
    guess: VisionGuess,
    service: GeocodeService,
) -> ReconcileResult:
    """Primary-only geocode + pin reconcile. Candidates are never queried."""
    skip = should_geocode(guess)
    match skip:
        case None:
            pass
        case "refusal" | "empty_primary":
            return _refused(skip)
        case "house_address":
            return _no_pin(skip)
        case _:
            raise ReconcileError(f"unhandled skip reason: {skip!r}")

    primary = (guess.primary_place_name or "").strip()
    lookup = await service.lookup_primary(
        primary,
        guess.country_code,
        guess.granularity,
    )
    return result_from_lookup(guess, lookup)
