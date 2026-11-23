//! Geocode the primary place name and reconcile model hints with the geocoder pin.
//!
//! Pin shown to the user is always the geocoder lat/lng. Model `lat_hint`/`lng_hint`
//! are compared via haversine; disagreement is recorded, never averaged.
//! Candidates are never geocoded (K9).

use crate::geocode::{looks_like_house_address, GeocodeService, LookupResult};
use crate::types::{GeoHit, Granularity, PhotoStatus, PinSource, RefusalReason, VisionGuess};

pub const EARTH_RADIUS_M: f64 = 6_371_000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileResult {
    pub pin_source: PinSource,
    pub pin_lat: Option<f64>,
    pub pin_lng: Option<f64>,
    pub model_agree: Option<bool>,
    pub geocode: Option<GeoHit>,
    pub photo_status: PhotoStatus,
    pub skipped_geocode: Option<String>,
    pub model_lat: Option<f64>,
    pub model_lng: Option<f64>,
}

pub fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlmb = (lng2 - lng1).to_radians();
    let chord = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlmb / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * chord.sqrt().min(1.0).asin()
}

/// Distance threshold in meters; `None` means unknown granularity cannot agree.
pub fn agreement_threshold_m(granularity: Granularity) -> Option<f64> {
    match granularity {
        Granularity::Exact | Granularity::Street => Some(500.0),
        Granularity::Neighborhood => Some(2_000.0),
        Granularity::City => Some(15_000.0),
        Granularity::Region => Some(100_000.0),
        Granularity::Country => Some(500_000.0),
        Granularity::Unknown => None,
    }
}

/// Return a skip reason, or `None` if the primary place may be queried.
pub fn should_geocode(guess: &VisionGuess) -> Option<&'static str> {
    match guess.refusal {
        RefusalReason::None => {}
        RefusalReason::InsufficientEvidence
        | RefusalReason::IndoorGeneric
        | RefusalReason::PrivacySensitive
        | RefusalReason::PolicyRefusal
        | RefusalReason::NotAPhotograph => return Some("refusal"),
    }
    let primary = guess.primary_place_name.as_deref().unwrap_or("").trim();
    if primary.is_empty() {
        return Some("empty_primary");
    }
    if looks_like_house_address(primary) {
        return Some("house_address");
    }
    None
}

fn refused(reason: &str) -> ReconcileResult {
    ReconcileResult {
        pin_source: PinSource::None,
        pin_lat: None,
        pin_lng: None,
        model_agree: None,
        geocode: None,
        photo_status: PhotoStatus::Refused,
        skipped_geocode: Some(reason.to_string()),
        model_lat: None,
        model_lng: None,
    }
}

fn no_pin(reason: &str) -> ReconcileResult {
    ReconcileResult {
        pin_source: PinSource::None,
        pin_lat: None,
        pin_lng: None,
        model_agree: None,
        geocode: None,
        photo_status: PhotoStatus::Proposed,
        skipped_geocode: Some(reason.to_string()),
        model_lat: None,
        model_lng: None,
    }
}

/// Pin = geocoder point. Record model agreement; never average two points.
pub fn reconcile_hit(guess: &VisionGuess, hit: GeoHit) -> ReconcileResult {
    let (agree, ghost_lat, ghost_lng) = match (guess.lat_hint, guess.lng_hint) {
        (Some(mlat), Some(mlng)) if mlat.is_finite() && mlng.is_finite() => {
            let threshold = agreement_threshold_m(guess.granularity);
            let distance = haversine_m(hit.lat, hit.lng, mlat, mlng);
            let ok = threshold.is_some_and(|t| distance <= t);
            if ok {
                (Some(true), None, None)
            } else {
                (Some(false), Some(mlat), Some(mlng))
            }
        }
        _ => (Some(true), None, None),
    };

    ReconcileResult {
        pin_source: PinSource::Geocoder,
        pin_lat: Some(hit.lat),
        pin_lng: Some(hit.lng),
        model_agree: agree,
        geocode: Some(hit),
        photo_status: PhotoStatus::Proposed,
        skipped_geocode: None,
        model_lat: ghost_lat,
        model_lng: ghost_lng,
    }
}

pub fn result_from_lookup(guess: &VisionGuess, lookup: LookupResult) -> ReconcileResult {
    if lookup.skipped.as_deref() == Some("house_address") {
        return no_pin("house_address");
    }
    if lookup.capped {
        return no_pin("uncached_cap");
    }
    match lookup.hit {
        Some(hit) => reconcile_hit(guess, hit),
        None => no_pin("miss"),
    }
}

/// Primary-only geocode + pin reconcile. Candidates are never queried.
pub async fn geocode_and_reconcile(
    guess: &VisionGuess,
    service: &GeocodeService,
) -> ReconcileResult {
    match should_geocode(guess) {
        None => {}
        Some(reason @ ("refusal" | "empty_primary")) => return refused(reason),
        Some("house_address") => return no_pin("house_address"),
        Some(other) => return refused(other),
    }

    let primary = guess.primary_place_name.as_deref().unwrap_or("").trim();
    let lookup = service
        .lookup_primary(primary, guess.country_code.as_deref(), guess.granularity)
        .await;
    result_from_lookup(guess, lookup)
}
