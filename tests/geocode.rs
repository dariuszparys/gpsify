use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gpsify::config::{load_settings_from_sources, SettingsOverrides, DEFAULT_GEOCODER_URL};
use gpsify::geocode::{
    cache_key, looks_like_house_address, nominatim_user_agent, normalize_place,
    parse_nominatim_hits, pick_best_hit, require_geocoder_ready, AsyncRateLimiter, FakeGeocoder,
    GeocodeCache, GeocodeConfigError, GeocodeService, Geocoder, NominatimGeocoder,
    DEFAULT_UNCACHED_MAX, OSM_ATTRIBUTION,
};
use gpsify::types::{GeoHit, Granularity};
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn hit(query: &str, display_name: &str, osm_type: &str) -> GeoHit {
    GeoHit {
        query: query.to_string(),
        display_name: display_name.to_string(),
        lat: 41.1456,
        lng: -8.6143,
        boundingbox: None,
        osm_type: Some(osm_type.to_string()),
        osm_id: Some("1".to_string()),
        importance: None,
        provider: "fake".to_string(),
    }
}

#[test]
fn osm_attribution_and_default_cap() {
    assert!(OSM_ATTRIBUTION.contains("OpenStreetMap"));
    assert!(OSM_ATTRIBUTION.starts_with('©'));
    assert_eq!(DEFAULT_UNCACHED_MAX, 50);
}

#[test]
fn normalize_and_cache_key_rules() {
    assert_eq!(
        normalize_place("  Clerigos Church, Porto!!! "),
        "clerigos church, porto"
    );
    assert_eq!(normalize_place("ﬁord"), "fiord");
    assert_eq!(
        cache_key("Porto, Portugal", Some("PT"), "EN"),
        "porto, portugal|pt|en"
    );
}

#[test]
fn house_address_heuristic() {
    assert!(looks_like_house_address("12 Oak Street, Portland"));
    assert!(looks_like_house_address("1600 Pennsylvania Avenue"));
    assert!(!looks_like_house_address(
        "Clérigos Church, Porto, Portugal"
    ));
    assert!(!looks_like_house_address("Times Square, New York"));
}

#[test]
fn contact_required_for_public_nominatim_only() {
    let pub_settings = load_settings_from_sources(
        SettingsOverrides {
            geocoder_url: Some(DEFAULT_GEOCODER_URL.to_string()),
            ..Default::default()
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let err = require_geocoder_ready(&pub_settings).unwrap_err();
    assert!(err.0.contains("nominatim_contact"));

    let local = load_settings_from_sources(
        SettingsOverrides {
            geocoder_url: Some("http://127.0.0.1:8080/search".to_string()),
            ..Default::default()
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    require_geocoder_ready(&local).unwrap();

    for keyed in ["mapbox", "google"] {
        let s = load_settings_from_sources(
            SettingsOverrides {
                geocoder: Some(keyed.to_string()),
                geocoder_url: Some(DEFAULT_GEOCODER_URL.to_string()),
                ..Default::default()
            },
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        require_geocoder_ready(&s).unwrap();
    }
}

#[test]
fn nominatim_user_agent_and_public_validation() {
    let ua = nominatim_user_agent(Some("dev@example.com"));
    assert_eq!(ua, "gpsify/0.1 (personal; dev@example.com)");
    let err = NominatimGeocoder::new(DEFAULT_GEOCODER_URL, None, "nominatim")
        .err()
        .unwrap();
    assert!(matches!(err, GeocodeConfigError(_)));
}

#[tokio::test]
async fn dry_run_blocks_live_nominatim_http() {
    let geo = NominatimGeocoder::new("http://127.0.0.1:8080/search", None, "nominatim").unwrap();
    let err = {
        let _guard = ENV_LOCK.lock().await;
        let prev = std::env::var("GPSIFY_DRY_RUN").ok();
        std::env::set_var("GPSIFY_DRY_RUN", "1");
        let res = geo.search("Porto", None, "en").await;
        if let Some(p) = prev {
            std::env::set_var("GPSIFY_DRY_RUN", p);
        } else {
            std::env::remove_var("GPSIFY_DRY_RUN");
        }
        res.unwrap_err()
    };
    assert!(err.0.contains("GPSIFY_DRY_RUN"));
}

#[test]
fn parse_nominatim_hits_skips_broken_rows() {
    let payload = json!([
        {"lat": "x", "lon": "y"},
        {"lat": "1", "lon": "2", "display_name": "Ok"}
    ]);
    let hits = parse_nominatim_hits(&payload, "q", "nominatim");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].display_name, "Ok");
}

#[tokio::test]
async fn nominatim_mock_server_parses_json_and_sets_ua() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "Clérigos Church, Porto"))
        .and(query_param("format", "jsonv2"))
        .and(query_param("countrycodes", "pt"))
        .and(header(
            "user-agent",
            "gpsify/0.1 (personal; dev@example.com)",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "lat": "41.1456",
                "lon": "-8.6143",
                "display_name": "Igreja dos Clérigos, Porto, Portugal",
                "osm_type": "way",
                "osm_id": 42,
                "boundingbox": ["41.14", "41.15", "-8.62", "-8.61"],
                "importance": 0.7
            }
        ])))
        .mount(&server)
        .await;

    let geo = NominatimGeocoder::new(
        format!("{}/search", server.uri()),
        Some("dev@example.com"),
        "nominatim",
    )
    .unwrap()
    .with_rate_limiter(AsyncRateLimiter::new(Duration::ZERO))
    .with_live_override(true);

    let hits = geo
        .search("Clérigos Church, Porto", Some("PT"), "en")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!((hits[0].lat - 41.1456).abs() < 1e-6);
    assert!((hits[0].lng - (-8.6143)).abs() < 1e-6);
    assert_eq!(hits[0].boundingbox, Some([41.14, 41.15, -8.62, -8.61]));
    assert_eq!(hits[0].osm_id.as_deref(), Some("42"));
}

#[tokio::test]
async fn cache_hit_cap_and_pick_best_hit() {
    let tmp = TempDir::new().unwrap();
    let cache = GeocodeCache::open(tmp.path().join("geocode.sqlite")).unwrap();
    let fake = FakeGeocoder::new();
    fake.add(
        "Porto",
        vec![
            hit("Porto", "Porto, Spain", "relation"),
            hit("Porto", "Porto, Portugal", "node"),
            hit("Porto", "Porto City, Portugal", "relation"),
        ],
    );
    fake.add(
        "Lisbon",
        vec![hit("Lisbon", "Lisbon, Portugal", "relation")],
    );

    let service = GeocodeService::new(Arc::new(fake.clone()), cache, "en", 1, true);

    let r1 = service
        .lookup_primary("Porto", Some("PT"), Granularity::City)
        .await;
    assert!(!r1.cache_hit);
    assert_eq!(
        r1.hit.as_ref().unwrap().display_name,
        "Porto City, Portugal"
    );

    let r2 = service
        .lookup_primary("Porto", Some("PT"), Granularity::City)
        .await;
    assert!(r2.cache_hit);
    assert_eq!(service.uncached_lookups(), 1);
    assert_eq!(service.cache_hits(), 1);

    let r3 = service
        .lookup_primary("Lisbon", Some("PT"), Granularity::City)
        .await;
    assert!(r3.capped);
    assert!(service.cap_stopped());

    let exact = pick_best_hit(
        &[
            hit("x", "Place, Portugal", "relation"),
            hit("x", "Spot, Portugal", "node"),
        ],
        Some("PT"),
        Granularity::Exact,
    )
    .unwrap();
    assert_eq!(exact.osm_type.as_deref(), Some("node"));
}
