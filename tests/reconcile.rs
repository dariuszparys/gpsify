use std::sync::Arc;

use gpsify::geocode::{geocode_cache_path, FakeGeocoder, GeocodeCache, GeocodeService};
use gpsify::reconcile::{
    agreement_threshold_m, geocode_and_reconcile, haversine_m, reconcile_hit, should_geocode,
};
use gpsify::types::{
    Candidate, GeoHit, Granularity, PhotoStatus, PinSource, RefusalReason, VisionGuess,
};
use tempfile::TempDir;

fn make_guess() -> VisionGuess {
    VisionGuess {
        refusal: RefusalReason::None,
        refusal_detail: None,
        primary_place_name: Some("Clérigos Church, Porto, Portugal".to_string()),
        country_code: Some("PT".to_string()),
        admin1: Some("Porto".to_string()),
        granularity: Granularity::Exact,
        lat_hint: Some(41.1457),
        lng_hint: Some(-8.6138),
        model_confidence: 0.9,
        clues: Vec::new(),
        candidates: vec![Candidate {
            place_name: "Livraria Lello, Porto, Portugal".to_string(),
            country_code: Some("PT".to_string()),
            admin1: Some("Porto".to_string()),
            lat_hint: None,
            lng_hint: None,
            confidence: 0.2,
            why: "Nearby landmark".to_string(),
        }],
        used_album_hint: false,
        used_filename_hint: false,
    }
}

fn clerigos_hit() -> GeoHit {
    GeoHit {
        query: "Clérigos Church, Porto, Portugal".to_string(),
        display_name: "Igreja dos Clérigos, Porto, Portugal".to_string(),
        lat: 41.1456,
        lng: -8.6139,
        boundingbox: None,
        osm_type: Some("way".to_string()),
        osm_id: Some("12345".to_string()),
        importance: Some(0.8),
        provider: "nominatim".to_string(),
    }
}

#[test]
fn haversine_and_agreement_thresholds_match_spec() {
    let d = haversine_m(41.1456, -8.6139, 41.1457, -8.6138);
    assert!(d < 20.0);
    assert_eq!(agreement_threshold_m(Granularity::Exact), Some(500.0));
    assert_eq!(agreement_threshold_m(Granularity::Street), Some(500.0));
    assert_eq!(
        agreement_threshold_m(Granularity::Neighborhood),
        Some(2_000.0)
    );
    assert_eq!(agreement_threshold_m(Granularity::City), Some(15_000.0));
    assert_eq!(agreement_threshold_m(Granularity::Region), Some(100_000.0));
    assert_eq!(agreement_threshold_m(Granularity::Country), Some(500_000.0));
    assert_eq!(agreement_threshold_m(Granularity::Unknown), None);
}

#[test]
fn should_geocode_blocks_refusals_empty_primary_and_house_addresses() {
    let mut g = make_guess();
    assert_eq!(should_geocode(&g), None);

    g.refusal = RefusalReason::PrivacySensitive;
    assert_eq!(should_geocode(&g), Some("refusal"));

    g.refusal = RefusalReason::None;
    g.primary_place_name = Some("   ".to_string());
    assert_eq!(should_geocode(&g), Some("empty_primary"));

    g.primary_place_name = Some("123 Main Street, Boston".to_string());
    assert_eq!(should_geocode(&g), Some("house_address"));
}

#[test]
fn reconcile_hit_records_disagreement_without_averaging() {
    let hit = clerigos_hit();
    let agree_guess = make_guess();
    let res_ok = reconcile_hit(&agree_guess, hit.clone());
    assert_eq!(res_ok.pin_source, PinSource::Geocoder);
    assert_eq!(res_ok.pin_lat, Some(hit.lat));
    assert_eq!(res_ok.pin_lng, Some(hit.lng));
    assert_eq!(res_ok.model_agree, Some(true));
    assert_eq!(res_ok.model_lat, None);
    assert_eq!(res_ok.model_lng, None);

    let mut far_guess = make_guess();
    far_guess.lat_hint = Some(38.7223);
    far_guess.lng_hint = Some(-9.1393);
    let res_far = reconcile_hit(&far_guess, hit.clone());
    assert_eq!(res_far.pin_lat, Some(hit.lat));
    assert_eq!(res_far.pin_lng, Some(hit.lng));
    assert_eq!(res_far.model_agree, Some(false));
    assert_eq!(res_far.model_lat, Some(38.7223));
    assert_eq!(res_far.model_lng, Some(-9.1393));
}

#[tokio::test]
async fn geocode_and_reconcile_queries_primary_place_only() {
    let tmp = TempDir::new().unwrap();
    let cache = GeocodeCache::open(geocode_cache_path(tmp.path())).unwrap();
    let fake = Arc::new(FakeGeocoder::new());
    fake.add("Clérigos Church, Porto, Portugal", vec![clerigos_hit()]);
    let service = GeocodeService::new(fake.clone(), cache, "en", 50, false);

    let res = geocode_and_reconcile(&make_guess(), &service).await;
    assert_eq!(res.photo_status, PhotoStatus::Proposed);
    assert_eq!(res.pin_source, PinSource::Geocoder);
    assert_eq!(fake.queries(), vec!["Clérigos Church, Porto, Portugal"]);
}
