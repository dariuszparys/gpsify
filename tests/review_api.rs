use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE};
use axum::http::{Request, StatusCode};
use gpsify::config::{load_settings_from_sources, Settings, SettingsOverrides};
use gpsify::geocode::{FakeGeocoder, GeocodeCache, GeocodeService, OSM_ATTRIBUTION};
use gpsify::media::{hash_file, thumb_path};
use gpsify::review::{
    batch_needs_confirm, create_router, list_low_rank_photos, parse_photo_filter, PhotoFilter,
    ReviewState, CLUSTER_N_CAP, GEOCODE_MIN_Q_LEN, LOW_UI_RANK, REVIEW_HOST,
};
use gpsify::store::{job_dir, options_hash, JobRepo, NewPhoto, NewProposal, PhotoRow};
use gpsify::types::{DecisionAction, GeoHit, PhotoStatus, PinSource};
use http_body_util::BodyExt;
use image::{Rgb, RgbImage};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::{Mutex, Semaphore};
use tower::ServiceExt;

const JOB_ID: &str = "22222222-2222-4222-8222-222222222222";
const PORTO: &str = "Clérigos Church, Porto, Portugal";
const PORTO_LAT: f64 = 41.1456;
const PORTO_LNG: f64 = -8.6143;

fn test_settings(data_dir: &Path) -> Settings {
    load_settings_from_sources(
        SettingsOverrides {
            data_dir: Some(data_dir.to_path_buf()),
            nominatim_contact: Some(Some("dev@example.com".to_string())),
            geocoder_url: Some("http://127.0.0.1:8080/search".to_string()),
            map_tile_url: Some("https://tile.openstreetmap.org/{z}/{x}/{y}.png".to_string()),
            ..Default::default()
        },
        &Default::default(),
        &Default::default(),
    )
    .unwrap()
}

fn porto_hit(query: &str) -> GeoHit {
    GeoHit {
        query: query.to_string(),
        display_name: "Igreja dos Clérigos, Porto, Portugal".to_string(),
        lat: PORTO_LAT,
        lng: PORTO_LNG,
        boundingbox: None,
        osm_type: Some("way".to_string()),
        osm_id: Some("1".to_string()),
        importance: None,
        provider: "fake".to_string(),
    }
}

fn create_repo(data_dir: &Path) -> JobRepo {
    JobRepo::create(
        data_dir,
        JOB_ID,
        "/photos/trip",
        "/photos/trip",
        &options_hash(false),
        Some("weekend in Porto"),
        "{}",
        None,
    )
    .unwrap()
}

fn add_photo(
    repo: &JobRepo,
    suffix: &str,
    status: PhotoStatus,
    cluster_id: Option<&str>,
    datetime_original: Option<&str>,
    path: Option<&Path>,
) -> String {
    let source = path
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/photos/trip/IMG_{suffix}.jpg")));
    let digest = if source.is_file() {
        hash_file(&source).unwrap()
    } else {
        "a".repeat(64)
    };
    let size = if source.is_file() {
        std::fs::metadata(&source).unwrap().len() as i64
    } else {
        1024
    };
    let relpath = if path.is_some() {
        source.file_name().unwrap().to_string_lossy().into_owned()
    } else {
        format!("IMG_{suffix}.jpg")
    };
    repo.insert_photo(NewPhoto {
        photo_id: None,
        path: source.to_string_lossy().into_owned(),
        relpath,
        sha256: digest,
        size,
        mtime_ns: 1,
        status,
        error: None,
        datetime_original: datetime_original.map(str::to_string),
        make: None,
        model: None,
        has_gps: false,
        existing_lat: None,
        existing_lng: None,
        cluster_id: cluster_id.map(str::to_string),
        updated_at: None,
    })
    .unwrap()
}

fn add_proposal(
    repo: &JobRepo,
    photo_id: &str,
    ui_rank: f64,
    pin_source: PinSource,
    pin_lat: Option<f64>,
    pin_lng: Option<f64>,
) {
    let guess = json!({
        "refusal": "none",
        "primary_place_name": PORTO,
        "country_code": "PT",
        "granularity": "exact",
        "lat_hint": null,
        "lng_hint": null,
        "model_confidence": 0.8,
        "clues": [{"kind": "architecture", "text": "baroque tower", "weight": 0.7}],
        "candidates": [
            {
                "place_name": "Lisbon, Portugal",
                "country_code": "PT",
                "confidence": 0.2,
                "why": "decoy"
            }
        ],
        "used_album_hint": false,
        "used_filename_hint": false
    });
    repo.insert_proposal(NewProposal {
        photo_id: photo_id.to_string(),
        provider: "xai".to_string(),
        model: "grok-4.6".to_string(),
        prompt_version: "PROMPT_V1".to_string(),
        raw_json: guess.to_string(),
        place_name: Some(PORTO.to_string()),
        granularity: Some("exact".to_string()),
        model_confidence: Some(0.8),
        ui_rank: Some(ui_rank),
        refusal: Some("none".to_string()),
        pin_lat,
        pin_lng,
        pin_source: Some(pin_source),
        model_agree: Some(1),
        current: true,
        ..Default::default()
    })
    .unwrap();
}

fn make_app(
    data_dir: &Path,
    repo: JobRepo,
    fake: FakeGeocoder,
) -> (axum::Router, String, FakeGeocoder) {
    let cache = GeocodeCache::open(data_dir.join("cache").join("geocode.sqlite")).unwrap();
    let service = GeocodeService::new(Arc::new(fake.clone()), cache, "en", 50, false);
    let token = "test-review-token-123".to_string();
    let state = ReviewState {
        settings: test_settings(data_dir),
        job_id: JOB_ID.to_string(),
        repo,
        service,
        job_root: job_dir(data_dir, JOB_ID),
        auth_token: token.clone(),
        media_slots: Arc::new(Semaphore::new(2)),
        mutation_lock: Arc::new(Mutex::new(())),
    };
    (create_router(state), token, fake)
}

async fn send_req(
    app: &axum::Router,
    req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

async fn get_authed(
    app: &axum::Router,
    token: &str,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header(HOST, "127.0.0.1")
        .header("x-gpsify-token", token)
        .body(Body::empty())
        .unwrap();
    send_req(app, req).await
}

async fn post_authed(
    app: &axum::Router,
    token: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(HOST, "127.0.0.1")
        .header("x-gpsify-token", token)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _, bytes) = send_req(app, req).await;
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[test]
fn static_search_is_enter_only() {
    let static_dir = Path::new("static");
    let mut js_files: Vec<PathBuf> = std::fs::read_dir(static_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
        .collect();
    js_files.sort();
    let names: Vec<String> = js_files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["api.js", "app.js", "desktop.js", "grid.js", "map.js"]
    );

    for path in &js_files {
        let src = std::fs::read_to_string(path).unwrap();
        assert!(!src.contains(r#"addEventListener("input""#), "{path:?}");
        assert!(!src.contains(".oninput"), "{path:?}");
        assert!(
            !src.to_ascii_lowercase().contains("autocomplete"),
            "{path:?}"
        );
    }

    let app_js = std::fs::read_to_string(static_dir.join("app.js")).unwrap();
    assert!(app_js.contains("keydown"));
    assert!(app_js.contains(r#"event.key !== "Enter""#));

    let html = std::fs::read_to_string(static_dir.join("index.html")).unwrap();
    assert!(html.contains("© OpenStreetMap contributors"));
    assert_eq!(GEOCODE_MIN_Q_LEN, 3);
}

#[test]
fn static_assets_are_local_only() {
    let static_dir = Path::new("static");
    let html = std::fs::read_to_string(static_dir.join("index.html")).unwrap();
    for attr in ["src=\"", "href=\""] {
        for chunk in html.split(attr).skip(1) {
            let url = chunk.split('"').next().unwrap_or("");
            if !url.is_empty() {
                assert!(
                    url.starts_with("/static/")
                        || url.starts_with("./")
                        || url.starts_with("data:"),
                    "unexpected remote asset reference: {url}"
                );
            }
        }
    }
    assert!(static_dir.join("vendor/leaflet.js").is_file());
    assert!(static_dir.join("vendor/leaflet.css").is_file());
    assert_eq!(REVIEW_HOST, "127.0.0.1");
}

#[tokio::test]
async fn review_api_job_filters_and_low_rank_list() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let cid = repo
        .insert_cluster(None, ".", Some("2019-06-14"), 2, false)
        .unwrap();
    let proposed = add_photo(
        &repo,
        "a",
        PhotoStatus::Proposed,
        Some(&cid),
        Some("2019:06:14 18:04:00"),
        None,
    );
    add_proposal(
        &repo,
        &proposed,
        0.83,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );

    let low = add_photo(
        &repo,
        "b",
        PhotoStatus::Proposed,
        Some(&cid),
        Some("2019:06:14 18:04:00"),
        None,
    );
    add_proposal(
        &repo,
        &low,
        0.40,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );

    let refused = add_photo(&repo, "c", PhotoStatus::Refused, None, None, None);
    add_proposal(&repo, &refused, 0.10, PinSource::None, None, None);

    let low_rows = list_low_rank_photos(&repo).unwrap();
    assert_eq!(low_rows.len(), 1);
    assert_eq!(low_rows[0].id, low);
    assert!(low_rows[0].ui_rank < LOW_UI_RANK);

    let (app, token, _) = make_app(tmp.path(), repo, FakeGeocoder::new());

    let (status, _, bytes) = get_authed(&app, &token, "/api/job").await;
    assert_eq!(status, StatusCode::OK);
    let job: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(job["id"], JOB_ID);
    assert_eq!(job["attribution"], OSM_ATTRIBUTION);
    assert_eq!(job["counts"]["proposed"], 2);

    let (status, _, bytes) = get_authed(&app, &token, "/api/photos").await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["photos"].as_array().unwrap().len(), 2);

    let (status, _, bytes) = get_authed(&app, &token, "/api/photos?status=low").await;
    assert_eq!(status, StatusCode::OK);
    let low_body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(low_body["photos"].as_array().unwrap().len(), 2);

    let (status, _, bytes) = get_authed(&app, &token, "/api/photos?status=refused").await;
    assert_eq!(status, StatusCode::OK);
    let ref_body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(ref_body["photos"][0]["id"], refused);

    let (status, _, bytes) = get_authed(&app, &token, "/api/photos?status=all").await;
    assert_eq!(status, StatusCode::OK);
    let all_body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(all_body["photos"].as_array().unwrap().len(), 3);

    let (status, _, _) = get_authed(&app, &token, "/api/photos?status=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(parse_photo_filter(None).unwrap(), PhotoFilter::NeedsReview);
}

#[tokio::test]
async fn accept_skip_batch_and_pin_source_none() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let good = add_photo(&repo, "a", PhotoStatus::Proposed, None, None, None);
    add_proposal(
        &repo,
        &good,
        0.83,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );
    let none_id = add_photo(&repo, "b", PhotoStatus::Proposed, None, None, None);
    add_proposal(&repo, &none_id, 0.83, PinSource::None, None, None);

    let (app, token, _) = make_app(tmp.path(), repo.clone(), FakeGeocoder::new());

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/photos/{good}/decision"),
        json!({"action": "accept"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dec = repo.get_current_decision(&good).unwrap().unwrap();
    assert!((dec.lat.unwrap() - PORTO_LAT).abs() < 1e-9);

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/photos/{good}/decision"),
        json!({"action": "skip"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        repo.get_photo(&good).unwrap().unwrap().status,
        PhotoStatus::SkippedByUser.as_str()
    );

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/photos/{none_id}/decision"),
        json!({"action": "accept"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/photos/{none_id}/decision"),
        json!({"action": "accept_edited"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/photos/{none_id}/decision"),
        json!({"action": "accept_edited", "lat": 41.15, "lng": -8.61}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(repo.list_write_receipts().unwrap().is_empty());

    // Batch decisions accept & partial errors
    let (status, batch_res) = post_authed(
        &app,
        &token,
        "/api/decisions",
        json!({"action": "skip", "photo_ids": [good, "missing-id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch_res["photos"].as_array().unwrap().len(), 1);
    assert_eq!(batch_res["errors"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn cluster_apply_and_pin_apply_enforce_k9_k18_caps() {
    let tmp = TempDir::new().unwrap();
    let repo = create_repo(tmp.path());
    let cid = repo.insert_cluster(None, ".", None, 31, true).unwrap();
    let src = add_photo(
        &repo,
        "src",
        PhotoStatus::Proposed,
        Some(&cid),
        Some("2019:06:14 18:04:00"),
        None,
    );
    add_proposal(
        &repo,
        &src,
        0.83,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );
    let sib = add_photo(&repo, "sib", PhotoStatus::Proposed, Some(&cid), None, None);
    add_proposal(
        &repo,
        &sib,
        0.7,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );
    let outsider = add_photo(&repo, "out", PhotoStatus::Proposed, None, None, None);
    add_proposal(
        &repo,
        &outsider,
        0.8,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );
    repo.add_decision(&src, DecisionAction::Accept, None, None, None, None)
        .unwrap();

    let (app, token, _) = make_app(tmp.path(), repo.clone(), FakeGeocoder::new());

    let (status, _, bytes) = get_authed(&app, &token, &format!("/api/clusters/{cid}")).await;
    assert_eq!(status, StatusCode::OK);
    let cl: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(cl["needs_confirm"], true);
    let cand_ids: Vec<&str> = cl["candidate_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(cand_ids.contains(&sib.as_str()));
    assert!(!cand_ids.contains(&src.as_str()));

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/clusters/{cid}/apply"),
        json!({"source_photo_id": src, "photo_ids": [sib, outsider]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/clusters/{cid}/apply"),
        json!({"source_photo_id": src, "photo_ids": [sib]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_authed(
        &app,
        &token,
        &format!("/api/clusters/{cid}/apply"),
        json!({"source_photo_id": src, "photo_ids": [sib], "confirm_n": 31}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        repo.get_current_decision(&sib).unwrap().unwrap().action,
        DecisionAction::ClusterApply.as_str()
    );

    // Selection /api/pin/apply across clusters
    let cid_b = repo
        .insert_cluster(None, "day2", Some("2019-06-15T09:00:00"), 1, false)
        .unwrap();
    let far = add_photo(
        &repo,
        "far",
        PhotoStatus::Proposed,
        Some(&cid_b),
        Some("2019:06:15 09:00:00"),
        None,
    );
    add_proposal(
        &repo,
        &far,
        0.8,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );

    let (status, res) = post_authed(
        &app,
        &token,
        "/api/pin/apply",
        json!({"source_photo_id": src, "photo_ids": [far]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["count"], 1);
    assert!(repo.list_write_receipts().unwrap().is_empty());

    // Unknown photo_id on /api/pin/apply -> 404
    let (status, _) = post_authed(
        &app,
        &token,
        "/api/pin/apply",
        json!({"source_photo_id": src, "photo_ids": ["nonexistent"]}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn batch_needs_confirm_gates() {
    let make_row = |dt: Option<&str>| PhotoRow {
        id: "id".to_string(),
        job_id: JOB_ID.to_string(),
        path: "/p".to_string(),
        relpath: "p.jpg".to_string(),
        sha256: "0".repeat(64),
        size: 1,
        mtime_ns: 1,
        status: "proposed".to_string(),
        error: None,
        datetime_original: dt.map(str::to_string),
        make: None,
        model: None,
        has_gps: 0,
        existing_lat: None,
        existing_lng: None,
        cluster_id: None,
        updated_at: "2026-08-12T00:00:00Z".to_string(),
    };
    let d = make_row(Some("2019:06:14 18:04:00"));
    let u = make_row(None);
    let dated = vec![&d, &d, &d, &d];
    let undated = vec![&u, &u, &u, &u];
    let half = vec![&d, &d, &u, &u];

    assert!(batch_needs_confirm(CLUSTER_N_CAP + 1, &dated, &dated));
    assert!(!batch_needs_confirm(4, &dated, &dated));
    assert!(batch_needs_confirm(4, &dated, &undated));
    assert!(batch_needs_confirm(4, &half, &dated));
    assert!(!batch_needs_confirm(4, &[], &[]));
}

#[tokio::test]
async fn geocode_enter_only_and_lazy_thumb_and_security_middleware() {
    let tmp = TempDir::new().unwrap();
    let img_path = tmp.path().join("plain.jpg");
    RgbImage::from_pixel(32, 16, Rgb([10, 20, 30]))
        .save(&img_path)
        .unwrap();
    let before = std::fs::read(&img_path).unwrap();

    let repo = create_repo(tmp.path());
    let pid = add_photo(
        &repo,
        "a",
        PhotoStatus::Proposed,
        None,
        None,
        Some(&img_path),
    );
    add_proposal(
        &repo,
        &pid,
        0.83,
        PinSource::Geocoder,
        Some(PORTO_LAT),
        Some(PORTO_LNG),
    );

    let fake = FakeGeocoder::new();
    fake.add("Porto", vec![porto_hit("Porto")]);
    let (app, token, fake_ref) = make_app(tmp.path(), repo.clone(), fake);

    // 1. Unauthenticated rejected (401)
    let req = Request::builder()
        .method("GET")
        .uri("/api/job")
        .header(HOST, "127.0.0.1")
        .body(Body::empty())
        .unwrap();
    let (st, _, _) = send_req(&app, req).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // 2. Wrong token rejected; valid token -> 303 + HttpOnly + SameSite=Strict cookie + no-store
    let req = Request::builder()
        .method("GET")
        .uri("/?token=wrong")
        .header(HOST, "127.0.0.1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send_req(&app, req).await.0, StatusCode::UNAUTHORIZED);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/?token={token}"))
        .header(HOST, "127.0.0.1")
        .body(Body::empty())
        .unwrap();
    let (st, headers, _) = send_req(&app, req).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(headers[LOCATION], "/");
    let cookie_str = headers[SET_COOKIE].to_str().unwrap();
    assert!(cookie_str.to_ascii_lowercase().contains("httponly"));
    assert!(cookie_str.to_ascii_lowercase().contains("samesite=strict"));
    assert_eq!(headers[CACHE_CONTROL], "no-store");

    // Extract cookie for session call
    let session_cookie = cookie_str.split(';').next().unwrap().to_string();
    let req = Request::builder()
        .method("GET")
        .uri("/")
        .header(HOST, "127.0.0.1")
        .header(COOKIE, &session_cookie)
        .body(Body::empty())
        .unwrap();
    let (st, headers, html_bytes) = send_req(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!headers.contains_key("access-control-allow-origin"));
    assert!(String::from_utf8_lossy(&html_bytes).contains("© OpenStreetMap contributors"));

    // 3. Rebound Host rejected (400)
    let req = Request::builder()
        .method("GET")
        .uri("/api/job")
        .header(HOST, "evil.example:8765")
        .header(COOKIE, &session_cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send_req(&app, req).await.0, StatusCode::BAD_REQUEST);

    // 4. Plain text POST rejected (415)
    let req = Request::builder()
        .method("POST")
        .uri("/api/geocode")
        .header(HOST, "127.0.0.1")
        .header(COOKIE, &session_cookie)
        .header(CONTENT_TYPE, "text/plain")
        .body(Body::from(r#"{"q":"Porto"}"#))
        .unwrap();
    assert_eq!(
        send_req(&app, req).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );

    // 5. Cross-origin / wrong-port Origin / Sec-Fetch-Site rejected (403)
    let req = Request::builder()
        .method("POST")
        .uri("/api/decisions")
        .header(HOST, "127.0.0.1")
        .header(COOKIE, &session_cookie)
        .header(CONTENT_TYPE, "application/json")
        .header(ORIGIN, "http://127.0.0.1:9999")
        .body(Body::from(
            json!({"action":"accept","photo_ids":[pid]}).to_string(),
        ))
        .unwrap();
    assert_eq!(send_req(&app, req).await.0, StatusCode::FORBIDDEN);

    let req = Request::builder()
        .method("POST")
        .uri("/api/decisions")
        .header(HOST, "127.0.0.1")
        .header(COOKIE, &session_cookie)
        .header(CONTENT_TYPE, "application/json")
        .header("Sec-Fetch-Site", "cross-site")
        .body(Body::from(
            json!({"action":"accept","photo_ids":[pid]}).to_string(),
        ))
        .unwrap();
    assert_eq!(send_req(&app, req).await.0, StatusCode::FORBIDDEN);
    assert!(repo.get_current_decision(&pid).unwrap().is_none());

    // 6. Geocode guards & GET 405
    let (st, _) = post_authed(&app, &token, "/api/geocode", json!({"q": "Po"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = post_authed(
        &app,
        &token,
        "/api/geocode",
        json!({"q": "12 Oak Street, Portland"}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _, _) = get_authed(&app, &token, "/api/geocode?q=Porto").await;
    assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
    let (st, hit_body) = post_authed(&app, &token, "/api/geocode", json!({"q": "Porto"})).await;
    assert_eq!(st, StatusCode::OK);
    assert!((hit_body["hit"]["lat"].as_f64().unwrap() - PORTO_LAT).abs() < 1e-6);
    assert_eq!(fake_ref.queries(), vec!["Porto"]);

    // 7. Lazy thumb & preview by UUID
    let (st, headers, thumb_bytes) = get_authed(&app, &token, &format!("/api/thumb/{pid}")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(headers[CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("image/jpeg"));
    assert_eq!(&thumb_bytes[..2], b"\xff\xd8");
    assert!(thumb_path(&job_dir(tmp.path(), JOB_ID), &pid).is_file());

    let (st, _, prev_bytes) = get_authed(&app, &token, &format!("/api/preview/{pid}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(&prev_bytes[..2], b"\xff\xd8");
    assert_eq!(std::fs::read(&img_path).unwrap(), before);
}
