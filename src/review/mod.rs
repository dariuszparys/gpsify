//! Embedded Axum review API + static Leaflet UI (K9, K16, K18).

pub mod routes;
pub mod security;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use rand::RngCore;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Semaphore};

use crate::config::Settings;
use crate::geocode::{
    geocode_cache_path, require_geocoder_ready, GeocodeCache, GeocodeService, NominatimGeocoder,
};
use crate::store::{job_dir, JobRepo};

pub use routes::{
    batch_needs_confirm, cluster_apply_candidates, list_low_rank_photos, parse_photo_filter,
    LowRankPhoto, PhotoFilter, StaticAssets,
};

pub const REVIEW_HOST: &str = "127.0.0.1";
pub const LOW_UI_RANK: f64 = 0.55;
pub const GEOCODE_MIN_Q_LEN: usize = 3;
pub const CLUSTER_N_CAP: usize = 30;
pub const CLUSTER_UNSTAMPED_FRACTION: f64 = 0.25;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ReviewError(pub String);

#[derive(Clone)]
pub struct ReviewState {
    pub settings: Settings,
    pub job_id: String,
    pub repo: JobRepo,
    pub service: GeocodeService,
    pub job_root: PathBuf,
    pub auth_token: String,
    pub media_slots: Arc<Semaphore>,
    pub mutation_lock: Arc<Mutex<()>>,
}

pub fn generate_auth_token() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn build_review_state_for_job(
    settings: Settings,
    job_id: &str,
) -> Result<ReviewState, ReviewError> {
    require_geocoder_ready(&settings).map_err(|e| ReviewError(e.to_string()))?;
    let repo = JobRepo::open(&settings.data_dir, job_id).map_err(|e| ReviewError(e.to_string()))?;
    let cache = GeocodeCache::open(geocode_cache_path(&settings.data_dir))
        .map_err(|e| ReviewError(e.to_string()))?;
    let geocoder = NominatimGeocoder::new(
        settings.geocoder_url.clone(),
        settings.nominatim_contact.as_deref(),
        settings.geocoder.clone(),
    )
    .map_err(|e| ReviewError(e.to_string()))?;
    let service = GeocodeService::new(
        Arc::new(geocoder),
        cache,
        settings.geocode_lang.clone(),
        50,
        false,
    );
    Ok(ReviewState {
        job_root: job_dir(&settings.data_dir, job_id),
        settings,
        job_id: job_id.to_string(),
        repo,
        service,
        auth_token: generate_auth_token(),
        media_slots: Arc::new(Semaphore::new(2)),
        mutation_lock: Arc::new(Mutex::new(())),
    })
}

pub fn create_router(state: ReviewState) -> Router {
    Router::new()
        .route("/", get(routes::index_page))
        .route("/api/job", get(routes::api_job))
        .route("/api/photos", get(routes::api_photos))
        .route("/api/decisions", post(routes::api_decisions))
        .route("/api/photos/{photo_id}", get(routes::api_photo_detail))
        .route(
            "/api/photos/{photo_id}/decision",
            post(routes::api_decision),
        )
        .route("/api/thumb/{photo_id}", get(routes::api_thumb))
        .route("/api/preview/{photo_id}", get(routes::api_preview))
        .route("/api/clusters/{cluster_id}", get(routes::api_cluster))
        .route(
            "/api/clusters/{cluster_id}/apply",
            post(routes::api_cluster_apply),
        )
        .route("/api/pin/apply", post(routes::api_pin_apply))
        .route("/api/geocode", post(routes::api_geocode))
        .route("/static/{*path}", get(routes::static_asset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security::security_middleware,
        ))
        .with_state(state)
}

/// Bind strictly to `127.0.0.1:<port>` (K16). There is no LAN flag.
pub async fn bind_review_listener(port: u16) -> Result<TcpListener, std::io::Error> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    TcpListener::bind(addr).await
}

pub async fn serve_review(state: ReviewState, port: u16) -> Result<(), ReviewError> {
    let listener = bind_review_listener(port)
        .await
        .map_err(|e| ReviewError(format!("failed to bind {REVIEW_HOST}:{port}: {e}")))?;
    let router = create_router(state);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| ReviewError(e.to_string()))
}
