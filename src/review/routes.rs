//! Review HTTP routes, K9/K18 pin-apply rails, and embedded static asset delivery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::exif::classify_media;
use crate::geocode::{looks_like_house_address, OSM_ATTRIBUTION};
use crate::media::{ensure_preview, ensure_thumb};
use crate::review::{
    ReviewError, ReviewState, CLUSTER_N_CAP, CLUSTER_UNSTAMPED_FRACTION, GEOCODE_MIN_Q_LEN,
    LOW_UI_RANK,
};
use crate::store::{DecisionRow, JobRepo, PhotoRow, ProposalRow, StoreError};
use crate::types::{DecisionAction, Granularity, PhotoStatus, VisionGuess};

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct StaticAssets;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoFilter {
    NeedsReview,
    Low,
    Refused,
    Accepted,
    All,
}

impl PhotoFilter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeedsReview => "needs_review",
            Self::Low => "low",
            Self::Refused => "refused",
            Self::Accepted => "accepted",
            Self::All => "all",
        }
    }
}

pub fn parse_photo_filter(raw: Option<&str>) -> Result<PhotoFilter, ReviewError> {
    let Some(s) = raw else {
        return Ok(PhotoFilter::NeedsReview);
    };
    let clean = s.trim().to_ascii_lowercase().replace(' ', "_");
    if clean.is_empty() {
        return Ok(PhotoFilter::NeedsReview);
    }
    match clean.as_str() {
        "needs_review" => Ok(PhotoFilter::NeedsReview),
        "low" => Ok(PhotoFilter::Low),
        "refused" => Ok(PhotoFilter::Refused),
        "accepted" => Ok(PhotoFilter::Accepted),
        "all" => Ok(PhotoFilter::All),
        _ => Err(ReviewError(format!("unknown status filter {s:?}"))),
    }
}

#[derive(Debug, Deserialize)]
pub struct PhotosQuery {
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DecisionBody {
    pub action: DecisionAction,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub place_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchDecisionBody {
    pub action: DecisionAction,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    #[serde(default)]
    pub place_name: Option<String>,
    pub photo_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClusterApplyBody {
    pub source_photo_id: String,
    pub photo_ids: Vec<String>,
    pub confirm_n: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct GeocodeBody {
    pub q: String,
    pub country_code: Option<String>,
}

fn api_error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn parse_json_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Box<Response>> {
    serde_json::from_slice::<T>(bytes).map_err(|e| {
        Box::new(api_error(
            StatusCode::BAD_REQUEST,
            format!("invalid body: {e}"),
        ))
    })
}

fn parse_guess(raw_json: &str) -> Option<VisionGuess> {
    if raw_json.trim().is_empty() {
        return None;
    }
    VisionGuess::from_json_str(raw_json).ok()
}

pub fn serialize_proposal(row: Option<&ProposalRow>) -> Value {
    let Some(row) = row else {
        return Value::Null;
    };
    let guess = parse_guess(&row.raw_json);
    let clues = guess.as_ref().map(|g| g.clues.clone()).unwrap_or_default();
    let candidates = guess
        .as_ref()
        .map(|g| g.candidates.clone())
        .unwrap_or_default();
    let lat_hint = guess.as_ref().and_then(|g| g.lat_hint);
    let lng_hint = guess.as_ref().and_then(|g| g.lng_hint);

    json!({
        "id": row.id,
        "place_name": row.place_name,
        "granularity": row.granularity,
        "model_confidence": row.model_confidence,
        "ui_rank": row.ui_rank,
        "refusal": row.refusal,
        "pin_lat": row.pin_lat,
        "pin_lng": row.pin_lng,
        "pin_source": row.pin_source,
        "model_agree": row.model_agree,
        "clues": clues,
        "candidates": candidates,
        "lat_hint": lat_hint,
        "lng_hint": lng_hint,
    })
}

pub fn serialize_decision(row: Option<&DecisionRow>) -> Value {
    let Some(row) = row else {
        return Value::Null;
    };
    json!({
        "id": row.id,
        "action": row.action,
        "lat": row.lat,
        "lng": row.lng,
        "place_name": row.place_name,
        "decided_at": row.decided_at,
    })
}

pub fn serialize_photo(
    photo: &PhotoRow,
    proposal: Option<&ProposalRow>,
    decision: Option<&DecisionRow>,
) -> Value {
    json!({
        "id": photo.id,
        "relpath": photo.relpath,
        "status": photo.status,
        "cluster_id": photo.cluster_id,
        "datetime_original": photo.datetime_original,
        "has_gps": photo.has_gps != 0,
        "existing_lat": photo.existing_lat,
        "existing_lng": photo.existing_lng,
        "proposal": serialize_proposal(proposal),
        "decision": serialize_decision(decision),
    })
}

fn photo_matches(photo: &PhotoRow, proposal: Option<&ProposalRow>, filt: PhotoFilter) -> bool {
    match filt {
        PhotoFilter::NeedsReview => photo.status == PhotoStatus::Proposed.as_str(),
        PhotoFilter::Low => match proposal.and_then(|p| p.ui_rank) {
            Some(rank) => rank < LOW_UI_RANK,
            None => false,
        },
        PhotoFilter::Refused => photo.status == PhotoStatus::Refused.as_str(),
        PhotoFilter::Accepted => {
            photo.status == PhotoStatus::Accepted.as_str()
                || photo.status == PhotoStatus::AcceptedEdited.as_str()
        }
        PhotoFilter::All => true,
    }
}

pub fn cluster_apply_candidates(members: &[PhotoRow]) -> Vec<&PhotoRow> {
    members
        .iter()
        .filter(|row| row.status == PhotoStatus::Proposed.as_str())
        .collect()
}

pub fn batch_needs_confirm(n: usize, candidates: &[&PhotoRow], applying: &[&PhotoRow]) -> bool {
    if n > CLUSTER_N_CAP {
        return true;
    }
    if !applying.is_empty()
        && applying
            .iter()
            .all(|row| row.datetime_original.as_deref().unwrap_or("").is_empty())
    {
        return true;
    }
    if candidates.is_empty() {
        return false;
    }
    let missing = candidates
        .iter()
        .filter(|row| row.datetime_original.as_deref().unwrap_or("").is_empty())
        .count();
    (missing as f64 / candidates.len() as f64) > CLUSTER_UNSTAMPED_FRACTION
}

#[derive(Debug, Clone, PartialEq)]
pub struct LowRankPhoto {
    pub id: String,
    pub relpath: String,
    pub ui_rank: f64,
    pub place_name: Option<String>,
}

pub fn list_low_rank_photos(repo: &JobRepo) -> Result<Vec<LowRankPhoto>, StoreError> {
    let photos = repo.list_photos()?;
    let proposals: BTreeMap<String, ProposalRow> = repo
        .list_current_proposals()?
        .into_iter()
        .map(|p| (p.photo_id.clone(), p))
        .collect();
    let mut out = Vec::new();
    for photo in photos {
        if photo.status != PhotoStatus::Proposed.as_str() {
            continue;
        }
        let Some(prop) = proposals.get(&photo.id) else {
            continue;
        };
        let Some(rank) = prop.ui_rank else {
            continue;
        };
        if rank < LOW_UI_RANK {
            out.push(LowRankPhoto {
                id: photo.id,
                relpath: photo.relpath,
                ui_rank: rank,
                place_name: prop.place_name.clone(),
            });
        }
    }
    Ok(out)
}

pub async fn index_page() -> Response {
    serve_asset("index.html")
}

pub async fn static_asset(AxumPath(path): AxumPath<String>) -> Response {
    let trimmed = path.trim_start_matches('/');
    serve_asset(trimmed)
}

fn serve_asset(path: &str) -> Response {
    let Some(file) = StaticAssets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = if path == "index.html" {
        "text/html; charset=utf-8".to_string()
    } else {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .essence_str()
            .to_string()
    };
    let mut resp = (StatusCode::OK, file.data.into_owned()).into_response();
    if let Ok(hv) = HeaderValue::from_str(&mime) {
        resp.headers_mut().insert(CONTENT_TYPE, hv);
    }
    resp
}

pub async fn api_job(State(state): State<ReviewState>) -> Response {
    let repo = state.repo.clone();
    let map_tile_url = state.settings.map_tile_url.clone();
    let res = tokio::task::spawn_blocking(move || {
        let job = repo.get_job()?;
        let counts = repo.count_photos_by_status()?;
        Ok::<_, StoreError>((job, counts))
    })
    .await;

    match res {
        Ok(Ok((Some(job), counts))) => Json(json!({
            "id": job.id,
            "root_path": job.root_path,
            "hint": job.hint,
            "counts": counts,
            "attribution": OSM_ATTRIBUTION,
            "map_tile_url": map_tile_url,
        }))
        .into_response(),
        Ok(Ok((None, _))) => api_error(StatusCode::NOT_FOUND, "job not found"),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn api_photos(
    State(state): State<ReviewState>,
    Query(query): Query<PhotosQuery>,
) -> Response {
    let filt = match parse_photo_filter(query.status.as_deref()) {
        Ok(f) => f,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e.0),
    };
    let repo = state.repo.clone();
    let res = tokio::task::spawn_blocking(move || {
        let photos = repo.list_photos()?;
        let proposals: BTreeMap<String, ProposalRow> = repo
            .list_current_proposals()?
            .into_iter()
            .map(|p| (p.photo_id.clone(), p))
            .collect();
        let decisions: BTreeMap<String, DecisionRow> = repo
            .list_current_decisions()?
            .into_iter()
            .map(|d| (d.photo_id.clone(), d))
            .collect();
        let records: Vec<Value> = photos
            .iter()
            .filter_map(|photo| {
                let prop = proposals.get(&photo.id);
                if !photo_matches(photo, prop, filt) {
                    return None;
                }
                Some(serialize_photo(photo, prop, decisions.get(&photo.id)))
            })
            .collect();
        Ok::<_, StoreError>(records)
    })
    .await;

    match res {
        Ok(Ok(records)) => Json(json!({
            "photos": records,
            "filter": filt.as_str(),
        }))
        .into_response(),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn api_photo_detail(
    State(state): State<ReviewState>,
    AxumPath(photo_id): AxumPath<String>,
) -> Response {
    let repo = state.repo.clone();
    let res = tokio::task::spawn_blocking(move || {
        let photo = repo.get_photo(&photo_id)?;
        let Some(photo) = photo else {
            return Ok::<_, StoreError>(None);
        };
        let prop = repo.get_current_proposal(&photo_id)?;
        let dec = repo.get_current_decision(&photo_id)?;
        Ok(Some(serialize_photo(&photo, prop.as_ref(), dec.as_ref())))
    })
    .await;

    match res {
        Ok(Ok(Some(val))) => Json(val).into_response(),
        Ok(Ok(None)) => api_error(StatusCode::NOT_FOUND, "photo not found"),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn serve_derived(state: ReviewState, photo_id: String, preview: bool) -> Response {
    let repo = state.repo.clone();
    let lookup = tokio::task::spawn_blocking(move || repo.get_photo(&photo_id)).await;
    let photo = match lookup {
        Ok(Ok(Some(p))) => p,
        Ok(Ok(None)) => return api_error(StatusCode::NOT_FOUND, "photo not found"),
        Ok(Err(e)) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let source = PathBuf::from(&photo.path);
    if !source.is_file() {
        return api_error(StatusCode::NOT_FOUND, "source missing");
    }
    let job_root = state.job_root.clone();
    let _permit = match state.media_slots.acquire().await {
        Ok(p) => p,
        Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "media semaphore closed"),
    };
    let derived = tokio::task::spawn_blocking(move || {
        let kind = classify_media(&source);
        let dest = if preview {
            ensure_preview(&job_root, &photo.id, &source, &photo.sha256, Some(kind))?
        } else {
            ensure_thumb(&job_root, &photo.id, &source, &photo.sha256, Some(kind))?
        };
        std::fs::read(&dest)
            .map_err(|e| crate::media::MediaError::new(e.to_string(), PhotoStatus::Error))
    })
    .await;

    match derived {
        Ok(Ok(bytes)) => {
            let mut resp = (StatusCode::OK, bytes).into_response();
            resp.headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            resp
        }
        Ok(Err(e)) => api_error(StatusCode::NOT_FOUND, e.message),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn api_thumb(
    State(state): State<ReviewState>,
    AxumPath(photo_id): AxumPath<String>,
) -> Response {
    serve_derived(state, photo_id, false).await
}

pub async fn api_preview(
    State(state): State<ReviewState>,
    AxumPath(photo_id): AxumPath<String>,
) -> Response {
    serve_derived(state, photo_id, true).await
}

pub async fn api_cluster(
    State(state): State<ReviewState>,
    AxumPath(cluster_id): AxumPath<String>,
) -> Response {
    let repo = state.repo.clone();
    let res = tokio::task::spawn_blocking(move || {
        let Some(cluster) = repo.get_cluster(&cluster_id)? else {
            return Ok::<_, StoreError>(None);
        };
        let members = repo.list_photos_in_cluster(&cluster_id)?;
        let proposals: BTreeMap<String, ProposalRow> = repo
            .list_current_proposals()?
            .into_iter()
            .map(|p| (p.photo_id.clone(), p))
            .collect();
        let payload_members: Vec<Value> = members
            .iter()
            .map(|photo| {
                let prop = proposals.get(&photo.id);
                json!({
                    "photo_id": photo.id,
                    "status": photo.status,
                    "ui_rank": prop.and_then(|p| p.ui_rank),
                    "place": prop.and_then(|p| p.place_name.clone()),
                    "has_datetime": photo.datetime_original.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
                    "relpath": photo.relpath,
                })
            })
            .collect();
        let candidates = cluster_apply_candidates(&members);
        let needs_confirm = batch_needs_confirm(cluster.n as usize, &candidates, &candidates);
        let candidate_ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        Ok(Some(json!({
            "id": cluster.id,
            "parent": cluster.parent,
            "n": cluster.n,
            "undated": cluster.undated,
            "first_ts": cluster.first_ts,
            "needs_confirm": needs_confirm,
            "candidate_ids": candidate_ids,
            "members": payload_members,
        })))
    })
    .await;

    match res {
        Ok(Ok(Some(val))) => Json(val).into_response(),
        Ok(Ok(None)) => api_error(StatusCode::NOT_FOUND, "cluster not found"),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn api_decision(
    State(state): State<ReviewState>,
    AxumPath(photo_id): AxumPath<String>,
    body_bytes: Bytes,
) -> Response {
    let body: DecisionBody = match parse_json_body(&body_bytes) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    if body.action == DecisionAction::ClusterApply {
        return api_error(
            StatusCode::BAD_REQUEST,
            "cluster_apply belongs on POST /api/clusters/{id}/apply",
        );
    }

    let _lock = state.mutation_lock.lock().await;
    let repo = state.repo.clone();
    let res = tokio::task::spawn_blocking(move || {
        let decision_id = repo.add_decision(
            &photo_id,
            body.action,
            body.lat,
            body.lng,
            None,
            body.place_name.as_deref(),
        )?;
        let decision = repo.get_current_decision(&photo_id)?;
        let photo = repo.get_photo(&photo_id)?;
        let proposal = repo.get_current_proposal(&photo_id)?;
        Ok::<_, StoreError>((decision_id, photo, proposal, decision))
    })
    .await;

    match res {
        Ok(Ok((decision_id, Some(photo), proposal, decision))) => Json(json!({
            "decision_id": decision_id,
            "photo": serialize_photo(&photo, proposal.as_ref(), decision.as_ref()),
        }))
        .into_response(),
        Ok(Ok((_, None, _, _))) => api_error(StatusCode::NOT_FOUND, "photo not found"),
        Ok(Err(StoreError::Decision(msg))) => api_error(StatusCode::BAD_REQUEST, msg),
        Ok(Err(e)) => api_error(StatusCode::NOT_FOUND, e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn dedup_preserve(ids: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for id in ids {
        if seen.insert(id.clone()) {
            out.push(id.clone());
        }
    }
    out
}

pub async fn api_decisions(State(state): State<ReviewState>, body_bytes: Bytes) -> Response {
    let body: BatchDecisionBody = match parse_json_body(&body_bytes) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    if body.photo_ids.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid body: photo_ids must not be empty",
        );
    }
    if !matches!(body.action, DecisionAction::Accept | DecisionAction::Skip) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "batch decisions support accept or skip only",
        );
    }
    if body.lat.is_some() || body.lng.is_some() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "batch decisions must not copy coordinates",
        );
    }

    let _lock = state.mutation_lock.lock().await;
    let repo = state.repo.clone();
    let ids = dedup_preserve(&body.photo_ids);
    let action = body.action;
    let res = tokio::task::spawn_blocking(move || {
        let mut photos = Vec::new();
        let mut errors = Vec::new();
        for pid in ids {
            match repo.add_decision(&pid, action, None, None, None, None) {
                Ok(_) => {
                    if let Some(photo) = repo.get_photo(&pid)? {
                        let prop = repo.get_current_proposal(&pid)?;
                        let dec = repo.get_current_decision(&pid)?;
                        photos.push(serialize_photo(&photo, prop.as_ref(), dec.as_ref()));
                    }
                }
                Err(e) => {
                    errors.push(json!({ "photo_id": pid, "error": e.to_string() }));
                }
            }
        }
        let counts = repo.count_photos_by_status()?;
        Ok::<_, StoreError>((photos, errors, counts))
    })
    .await;

    match res {
        Ok(Ok((photos, errors, counts))) => Json(json!({
            "photos": photos,
            "errors": errors,
            "counts": counts,
        }))
        .into_response(),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn apply_source_pin(
    repo: &JobRepo,
    source_photo_id: &str,
    photo_ids: &[String],
    confirm_n: Option<usize>,
    pool: &[PhotoRow],
    n: usize,
    label: &str,
) -> Result<Value, (StatusCode, String)> {
    let by_id: BTreeMap<&str, &PhotoRow> = pool.iter().map(|r| (r.id.as_str(), r)).collect();
    if !by_id.contains_key(source_photo_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("source_photo_id is not in this {label}"),
        ));
    }
    let source_decision = repo
        .get_current_decision(source_photo_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "source photo has no current decision".to_string(),
            )
        })?;

    let (Some(lat), Some(lng)) = (source_decision.lat, source_decision.lng) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "source decision has no coordinates".to_string(),
        ));
    };

    let candidates = cluster_apply_candidates(pool);
    let allowed: BTreeSet<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
    let requested = dedup_preserve(photo_ids);
    let extra: Vec<&str> = requested
        .iter()
        .map(String::as_str)
        .filter(|pid| !allowed.contains(pid))
        .collect();
    if !extra.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("photo_ids not in proposed candidate set: {extra:?}"),
        ));
    }

    let applying: Vec<&PhotoRow> = requested.iter().map(|pid| by_id[pid.as_str()]).collect();
    if batch_needs_confirm(n, &candidates, &applying) && confirm_n != Some(n) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{label} apply requires confirm_n equal to {label} size ({n})"),
        ));
    }

    let mut decision_ids = Vec::with_capacity(requested.len());
    for pid in requested {
        let did = repo
            .add_decision(
                &pid,
                DecisionAction::ClusterApply,
                Some(lat),
                Some(lng),
                source_decision.proposal_id.as_deref(),
                source_decision.place_name.as_deref(),
            )
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        decision_ids.push(did);
    }

    Ok(json!({
        "decision_ids": decision_ids,
        "count": decision_ids.len(),
    }))
}

pub async fn api_cluster_apply(
    State(state): State<ReviewState>,
    AxumPath(cluster_id): AxumPath<String>,
    body_bytes: Bytes,
) -> Response {
    let body: ClusterApplyBody = match parse_json_body(&body_bytes) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    if body.photo_ids.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid body: photo_ids must not be empty",
        );
    }

    let _lock = state.mutation_lock.lock().await;
    let repo = state.repo.clone();
    let res = tokio::task::spawn_blocking(move || {
        let Some(cluster) = repo
            .get_cluster(&cluster_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err((StatusCode::NOT_FOUND, "cluster not found".to_string()));
        };
        let members = repo
            .list_photos_in_cluster(&cluster_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        apply_source_pin(
            &repo,
            &body.source_photo_id,
            &body.photo_ids,
            body.confirm_n,
            &members,
            cluster.n as usize,
            "cluster",
        )
    })
    .await;

    match res {
        Ok(Ok(payload)) => Json(payload).into_response(),
        Ok(Err((code, msg))) => api_error(code, msg),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn api_pin_apply(State(state): State<ReviewState>, body_bytes: Bytes) -> Response {
    let body: ClusterApplyBody = match parse_json_body(&body_bytes) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    if body.photo_ids.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid body: photo_ids must not be empty",
        );
    }

    let _lock = state.mutation_lock.lock().await;
    let repo = state.repo.clone();
    let res = tokio::task::spawn_blocking(move || {
        let mut all_wanted = Vec::with_capacity(body.photo_ids.len() + 1);
        all_wanted.push(body.source_photo_id.clone());
        all_wanted.extend(body.photo_ids.iter().cloned());
        let wanted = dedup_preserve(&all_wanted);

        let mut pool = Vec::with_capacity(wanted.len());
        for pid in wanted {
            let photo = repo
                .get_photo(&pid)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .ok_or_else(|| (StatusCode::NOT_FOUND, format!("unknown photo_id: {pid}")))?;
            pool.push(photo);
        }
        let n = dedup_preserve(&body.photo_ids).len();
        apply_source_pin(
            &repo,
            &body.source_photo_id,
            &body.photo_ids,
            body.confirm_n,
            &pool,
            n,
            "selection",
        )
    })
    .await;

    match res {
        Ok(Ok(payload)) => Json(payload).into_response(),
        Ok(Err((code, msg))) => api_error(code, msg),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn api_geocode(State(state): State<ReviewState>, body_bytes: Bytes) -> Response {
    let body: GeocodeBody = match parse_json_body(&body_bytes) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let query = body.q.trim().to_string();
    if query.chars().count() < GEOCODE_MIN_Q_LEN {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("q must be at least {GEOCODE_MIN_Q_LEN} characters"),
        );
    }
    if looks_like_house_address(&query) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "refusing to geocode a house-address string",
        );
    }
    let result = state
        .service
        .lookup_primary(&query, body.country_code.as_deref(), Granularity::Unknown)
        .await;
    if result.skipped.as_deref() == Some("house_address") {
        return api_error(
            StatusCode::BAD_REQUEST,
            "refusing to geocode a house-address string",
        );
    }
    Json(json!({
        "hit": result.hit,
        "cache_hit": result.cache_hit,
        "skipped": result.skipped,
    }))
    .into_response()
}
