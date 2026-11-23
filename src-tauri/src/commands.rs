//! Tauri IPC commands and core backend handlers for gpsify Desktop.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Runtime, State};
use tauri_plugin_dialog::DialogExt;

use gpsify::apply::{run_apply, ApplyOptions};
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::propose::{
    count_unpinned_proposals, run_geocode, run_propose, GeocodeOptions, ProgressFn, ProposeOptions,
    DEFAULT_CONCURRENCY,
};
use gpsify::review::routes::{
    apply_cluster, apply_pin, cluster_payload, decide_many, decide_one, geocode_place, job_payload,
    list_low_rank_photos, parse_photo_filter, photo_detail_payload, photos_payload,
    BatchDecisionBody, ClusterApplyBody, DecisionBody, GeocodeBody,
};
use gpsify::status::{job_status, list_job_entries, run_clean};
use gpsify::store::JobRepo;
use gpsify::types::DecisionAction;
use gpsify::undo::run_undo;

use crate::state::DesktopState;

#[derive(Debug, Clone, Deserialize)]
pub struct ProposeArgs {
    pub job_id: String,
    pub provider: Option<String>,
    pub limit: Option<usize>,
    pub max_cost: Option<f64>,
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub no_context: bool,
    pub geocode_uncached_max: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplyArgs {
    pub job_id: String,
    pub out: Option<String>,
    #[serde(default)]
    pub in_place: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub i_mean_it: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedJobEntry {
    pub job_id: String,
    pub root_canonical: String,
    pub options_hash: String,
    pub created_at: String,
    pub last_ingest_at: String,
    pub dest_root: String,
}

fn emit_progress<R: Runtime>(app: &AppHandle<R>, stage: &str, message: &str, done: bool) {
    let _ = app.emit(
        "gpsify://progress",
        json!({
            "stage": stage,
            "message": message,
            "done": done,
        }),
    );
}

fn progress_cb<R: Runtime>(app: AppHandle<R>, stage: &'static str) -> ProgressFn {
    Arc::new(move |msg: &str| {
        emit_progress(&app, stage, msg, false);
    })
}

pub async fn list_jobs_impl(state: &DesktopState) -> Result<Value, String> {
    let settings = state.settings();
    let entries = tokio::task::spawn_blocking(move || list_job_entries(&settings.data_dir))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let active_job_id = state
        .active_review
        .lock()
        .await
        .as_ref()
        .map(|r| r.job_id.clone());
    Ok(json!({
        "jobs": entries,
        "active_job_id": active_job_id,
    }))
}

pub async fn status_impl(state: &DesktopState, job_id: &str) -> Result<Value, String> {
    let settings = state.settings();
    let jid = job_id.to_string();
    tokio::task::spawn_blocking(move || {
        let rollup = job_status(&settings.data_dir, &jid).map_err(|e| e.to_string())?;
        let repo = JobRepo::open(&settings.data_dir, &jid).map_err(|e| e.to_string())?;
        let unpinned = count_unpinned_proposals(&repo).map_err(|e| e.to_string())?;
        let low_rank_count = list_low_rank_photos(&repo)
            .map_err(|e| e.to_string())?
            .len();
        Ok::<_, String>(json!({
            "status": rollup,
            "unpinned": unpinned,
            "low_rank_count": low_rank_count,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn bootstrap_impl(state: &DesktopState) -> Result<Value, String> {
    let _ = state.reload_settings();
    let doctor = state.doctor_snapshot();

    let mut initial_folder: Option<String> = None;
    let mut active_job: Option<Value> = None;
    if let Some(ref handle) = state.initial_handle {
        match state.open_job_state(handle).await {
            Ok(opened) => active_job = Some(opened),
            Err(_) => {
                let p = PathBuf::from(handle);
                if p.is_dir() {
                    initial_folder = Some(p.to_string_lossy().into_owned());
                }
            }
        }
    } else if let Ok(opened) = state.open_job_state("last").await {
        active_job = Some(opened);
    }

    let jobs = list_jobs_impl(state).await?;
    Ok(json!({
        "doctor": doctor,
        "jobs": jobs["jobs"],
        "active_job_id": jobs["active_job_id"],
        "active": active_job,
        "initial_folder": initial_folder,
        "media_base": DesktopState::media_base_url(),
    }))
}

pub async fn ingest_impl(
    state: &DesktopState,
    folder: String,
    hint: Option<String>,
    recursive: bool,
) -> Result<Value, String> {
    let _busy = state.acquire_busy("ingest")?;
    let settings = state.reload_settings()?;
    let folder_path = PathBuf::from(folder.trim());
    let summary = tokio::task::spawn_blocking(move || {
        let mut opts = IngestOptions::new(&settings.data_dir);
        opts.hint = hint.as_deref().map(str::trim).filter(|s| !s.is_empty());
        opts.recursive = recursive;
        run_ingest(&folder_path, opts).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    let opened = state.open_job_state(&summary.job_id).await?;
    Ok(json!({
        "summary": {
            "job_id": summary.job_id,
            "total": summary.total,
            "tagged": summary.tagged,
            "untagged": summary.untagged,
            "skipped": summary.skipped,
            "missing": summary.missing,
            "clusters": summary.clusters,
            "other": summary.other,
            "warnings": summary.warnings,
        },
        "opened": opened,
    }))
}

pub async fn propose_impl(
    state: &DesktopState,
    args: ProposeArgs,
    progress: Option<ProgressFn>,
) -> Result<Value, String> {
    let _busy = state.acquire_busy("propose")?;
    let settings = state.reload_settings()?;
    let mut opts = ProposeOptions::new(settings.clone());
    opts.provider_name = args.provider;
    opts.limit = args.limit;
    opts.max_cost = args.max_cost;
    opts.concurrency = args.concurrency.unwrap_or(DEFAULT_CONCURRENCY);
    opts.dry_run = args.dry_run;
    opts.force = args.force;
    opts.no_context = args.no_context;
    if let Some(cap) = args.geocode_uncached_max {
        opts.geocode_uncached_max = cap;
    }
    opts.progress = progress;

    let jid = args.job_id.clone();
    let summary = run_propose(&jid, opts).await.map_err(|e| e.to_string())?;
    let status = status_impl(state, &jid).await?;
    Ok(json!({
        "summary": summary,
        "status": status["status"],
        "unpinned": status["unpinned"],
        "low_rank_count": status["low_rank_count"],
    }))
}

pub async fn geocode_pass_impl(
    state: &DesktopState,
    job_id: String,
    limit: Option<usize>,
    dry_run: bool,
    geocode_uncached_max: Option<usize>,
    progress: Option<ProgressFn>,
) -> Result<Value, String> {
    let _busy = state.acquire_busy("geocode")?;
    let settings = state.reload_settings()?;
    let mut opts = GeocodeOptions::new(settings);
    opts.limit = limit;
    opts.dry_run = dry_run;
    if let Some(cap) = geocode_uncached_max {
        opts.geocode_uncached_max = cap;
    }
    opts.progress = progress;

    let summary = run_geocode(&job_id, opts)
        .await
        .map_err(|e| e.to_string())?;
    let status = status_impl(state, &job_id).await?;
    Ok(json!({
        "summary": summary,
        "status": status["status"],
        "unpinned": status["unpinned"],
        "low_rank_count": status["low_rank_count"],
    }))
}

pub async fn apply_impl(state: &DesktopState, args: ApplyArgs) -> Result<Value, String> {
    let _busy = state.acquire_busy("apply")?;
    let settings = state.reload_settings()?;
    if args.out.as_ref().is_some_and(|s| !s.trim().is_empty()) && args.in_place {
        return Err("Choose either an output folder OR in-place mutation, not both.".to_string());
    }
    if args.in_place && !args.dry_run && !args.i_mean_it {
        return Err("In-place write requires explicit confirmation.".to_string());
    }
    let data_dir = settings.data_dir.clone();
    let jid = args.job_id.clone();
    let opts = ApplyOptions {
        out: args
            .out
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
        in_place: args.in_place,
        dry_run: args.dry_run,
        i_mean_it: args.i_mean_it || args.dry_run,
        allow_writes_override: None,
    };

    let summary = tokio::task::spawn_blocking(move || {
        run_apply(&jid, &data_dir, opts, None::<fn(&str)>).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    if !summary.dry_run && summary.eligible == 0 {
        return Err("Nothing accepted to apply yet. Accept at least one photo first.".to_string());
    }
    let status = status_impl(state, &args.job_id).await?;
    Ok(json!({
        "summary": {
            "job_id": summary.job_id,
            "dest_root": summary.dest_root,
            "in_place": summary.in_place,
            "dry_run": summary.dry_run,
            "wrote": summary.wrote,
            "failed": summary.failed,
            "skipped": summary.skipped,
            "eligible": summary.eligible,
            "argfile": summary.argfile,
            "warnings": summary.warnings,
        },
        "status": status["status"],
    }))
}

pub async fn undo_impl(state: &DesktopState, job_id: String) -> Result<Value, String> {
    let _busy = state.acquire_busy("undo")?;
    let settings = state.reload_settings()?;
    let data_dir = settings.data_dir.clone();
    let jid = job_id.clone();
    let summary = tokio::task::spawn_blocking(move || {
        run_undo(&jid, &data_dir, None, None::<fn(&str)>).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    let status = status_impl(state, &job_id).await?;
    Ok(json!({
        "summary": {
            "job_id": summary.job_id,
            "undone": summary.undone,
            "failed": summary.failed,
            "skipped": summary.skipped,
            "warnings": summary.warnings,
        },
        "status": status["status"],
    }))
}

pub async fn clean_impl(state: &DesktopState, job_id: &str) -> Result<Value, String> {
    let settings = state.settings();
    let jid = job_id.to_string();
    let summary = tokio::task::spawn_blocking(move || run_clean(&settings.data_dir, &jid))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "job_id": summary.job_id,
        "deleted_previews": summary.deleted_previews,
        "deleted_thumbs": summary.deleted_thumbs,
        "kept_sqlite": summary.kept_sqlite,
        "kept_backups": summary.kept_backups,
    }))
}

pub async fn skip_low_impl(state: &DesktopState, job_id: &str) -> Result<Value, String> {
    let settings = state.settings();
    let jid = job_id.to_string();
    let skipped = tokio::task::spawn_blocking(move || {
        let repo = JobRepo::open(&settings.data_dir, &jid).map_err(|e| e.to_string())?;
        let rows = list_low_rank_photos(&repo).map_err(|e| e.to_string())?;
        for row in &rows {
            repo.add_decision(&row.id, DecisionAction::Skip, None, None, None, None)
                .map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(rows.len())
    })
    .await
    .map_err(|e| e.to_string())??;
    let status = status_impl(state, job_id).await?;
    Ok(json!({
        "skipped": skipped,
        "status": status["status"],
        "low_rank_count": status["low_rank_count"],
    }))
}

// --- Tauri IPC commands ----------------------------------------------------

#[tauri::command]
pub async fn desktop_bootstrap(state: State<'_, DesktopState>) -> Result<Value, String> {
    bootstrap_impl(&state).await
}

#[tauri::command]
pub async fn desktop_doctor(state: State<'_, DesktopState>) -> Result<Value, String> {
    let _ = state.reload_settings();
    serde_json::to_value(state.doctor_snapshot()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn desktop_list_jobs(state: State<'_, DesktopState>) -> Result<Value, String> {
    list_jobs_impl(&state).await
}

#[tauri::command]
pub async fn desktop_open_job(
    state: State<'_, DesktopState>,
    handle: String,
) -> Result<Value, String> {
    state.open_job_state(&handle).await
}

#[tauri::command]
pub async fn desktop_pick_folder<R: Runtime>(
    app: AppHandle<R>,
    title: Option<String>,
) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog().file();
    if let Some(t) = title {
        builder = builder.set_title(t);
    }
    builder.pick_folder(move |picked| {
        let path_str = picked
            .and_then(|fp| fp.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned());
        let _ = tx.send(path_str);
    });
    rx.await.map_err(|_| "folder picker cancelled".to_string())
}

#[tauri::command]
pub async fn desktop_ingest<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, DesktopState>,
    folder: String,
    hint: Option<String>,
    recursive: bool,
) -> Result<Value, String> {
    emit_progress(&app, "ingest", "Scanning folder and reading EXIF…", false);
    let res = ingest_impl(&state, folder, hint, recursive).await;
    emit_progress(&app, "ingest", "Scan complete", true);
    res
}

#[tauri::command]
pub async fn desktop_propose<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, DesktopState>,
    args: ProposeArgs,
) -> Result<Value, String> {
    emit_progress(&app, "propose", "Starting vision & geocode pass…", false);
    let cb = progress_cb(app.clone(), "propose");
    let res = propose_impl(&state, args, Some(cb)).await;
    emit_progress(&app, "propose", "Propose finished", true);
    res
}

#[tauri::command]
pub async fn desktop_geocode_pass<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, DesktopState>,
    job_id: String,
    limit: Option<usize>,
    dry_run: Option<bool>,
    geocode_uncached_max: Option<usize>,
) -> Result<Value, String> {
    emit_progress(&app, "geocode", "Geocoding unpinned proposals…", false);
    let cb = progress_cb(app.clone(), "geocode");
    let res = geocode_pass_impl(
        &state,
        job_id,
        limit,
        dry_run.unwrap_or(false),
        geocode_uncached_max,
        Some(cb),
    )
    .await;
    emit_progress(&app, "geocode", "Geocode pass finished", true);
    res
}

#[tauri::command]
pub async fn desktop_apply<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, DesktopState>,
    args: ApplyArgs,
) -> Result<Value, String> {
    emit_progress(&app, "apply", "Writing GPS tags via exiftool…", false);
    let res = apply_impl(&state, args).await;
    emit_progress(&app, "apply", "Apply finished", true);
    res
}

#[tauri::command]
pub async fn desktop_undo<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, DesktopState>,
    job_id: String,
) -> Result<Value, String> {
    emit_progress(&app, "undo", "Restoring backups…", false);
    let res = undo_impl(&state, job_id).await;
    emit_progress(&app, "undo", "Undo finished", true);
    res
}

#[tauri::command]
pub async fn desktop_status(
    state: State<'_, DesktopState>,
    job_id: String,
) -> Result<Value, String> {
    status_impl(&state, &job_id).await
}

#[tauri::command]
pub async fn desktop_clean(
    state: State<'_, DesktopState>,
    job_id: String,
) -> Result<Value, String> {
    clean_impl(&state, &job_id).await
}

#[tauri::command]
pub async fn desktop_skip_low(
    state: State<'_, DesktopState>,
    job_id: String,
) -> Result<Value, String> {
    skip_low_impl(&state, &job_id).await
}

// --- Shared Review IPC commands (zero duplicated K9/K10/K12/K18 logic) -----

#[tauri::command]
pub async fn review_job(state: State<'_, DesktopState>) -> Result<Value, String> {
    let review = state.active_state().await?;
    job_payload(&review).await.map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_photos(
    state: State<'_, DesktopState>,
    status: Option<String>,
) -> Result<Value, String> {
    let filt = parse_photo_filter(status.as_deref()).map_err(|e| e.0)?;
    let review = state.active_state().await?;
    photos_payload(&review, filt).await.map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_photo_detail(
    state: State<'_, DesktopState>,
    photo_id: String,
) -> Result<Value, String> {
    let review = state.active_state().await?;
    photo_detail_payload(&review, &photo_id)
        .await
        .map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_cluster(
    state: State<'_, DesktopState>,
    cluster_id: String,
) -> Result<Value, String> {
    let review = state.active_state().await?;
    cluster_payload(&review, &cluster_id)
        .await
        .map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_decide(
    state: State<'_, DesktopState>,
    photo_id: String,
    payload: DecisionBody,
) -> Result<Value, String> {
    let review = state.active_state().await?;
    decide_one(&review, &photo_id, payload)
        .await
        .map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_decide_many(
    state: State<'_, DesktopState>,
    payload: BatchDecisionBody,
) -> Result<Value, String> {
    let review = state.active_state().await?;
    decide_many(&review, payload).await.map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_apply_cluster(
    state: State<'_, DesktopState>,
    cluster_id: String,
    payload: ClusterApplyBody,
) -> Result<Value, String> {
    let review = state.active_state().await?;
    apply_cluster(&review, &cluster_id, payload)
        .await
        .map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_apply_pin(
    state: State<'_, DesktopState>,
    payload: ClusterApplyBody,
) -> Result<Value, String> {
    let review = state.active_state().await?;
    apply_pin(&review, payload).await.map_err(|(_, m)| m)
}

#[tauri::command]
pub async fn review_geocode(
    state: State<'_, DesktopState>,
    payload: GeocodeBody,
) -> Result<Value, String> {
    let review = state.active_state().await?;
    geocode_place(&review, payload).await.map_err(|(_, m)| m)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use gpsify::config::Settings;
    use gpsify::exif::exiftool_available;
    use gpsify::geocode::FakeGeocoder;
    use gpsify::propose::{instant_sleep_fn, run_propose, ProposeOptions};
    use gpsify::providers::FakeVisionProvider;
    use gpsify::types::GeoHit;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn test_jpeg(path: &Path) {
        let img = image::RgbImage::new(64, 48);
        img.save_with_format(path, image::ImageFormat::Jpeg)
            .unwrap();
    }

    fn settings_for(data_dir: &Path) -> Settings {
        Settings {
            data_dir: data_dir.to_path_buf(),
            nominatim_contact: Some("test@example.com".to_string()),
            xai_api_key: Some("secret-xai-key-never-leak".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn desktop_pipeline_and_review_ipc_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let album = tmp.path().join("porto");
        fs::create_dir_all(&album).unwrap();
        test_jpeg(&album.join("img1.jpg"));

        let state = DesktopState::with_settings(settings_for(&data_dir), None);

        // Doctor hides actual secret values
        let doc = state.doctor_snapshot();
        let doc_json = serde_json::to_string(&doc).unwrap();
        assert!(doc.api_key_configured);
        assert!(!doc_json.contains("secret-xai-key-never-leak"));

        // Ingest opens job in active_review
        let ing = ingest_impl(
            &state,
            album.to_string_lossy().into_owned(),
            Some("Porto".to_string()),
            false,
        )
        .await
        .unwrap();
        let job_id = ing["summary"]["job_id"].as_str().unwrap().to_string();
        assert_eq!(ing["summary"]["untagged"], 1);

        // Seed proposal offline via run_propose + FakeVisionProvider
        let fake_geo = Arc::new(FakeGeocoder::new());
        fake_geo.add(
            "Clérigos Church, Porto, Portugal",
            vec![GeoHit {
                query: "Clérigos Church, Porto, Portugal".to_string(),
                display_name: "Igreja dos Clérigos, Porto".to_string(),
                lat: 41.1456,
                lng: -8.6139,
                boundingbox: None,
                osm_type: Some("way".to_string()),
                osm_id: Some("1".to_string()),
                importance: Some(0.9),
                provider: "fake".to_string(),
            }],
        );
        let mut prop_opts = ProposeOptions::new(settings_for(&data_dir));
        prop_opts.sleep = Some(instant_sleep_fn());
        prop_opts.vision = Some(Arc::new(FakeVisionProvider::default()));
        prop_opts.geocoder = Some(fake_geo);
        run_propose(&job_id, prop_opts).await.unwrap();

        // Reload active job & inspect via shared review helpers
        let opened = state.open_job_state(&job_id).await.unwrap();
        assert_eq!(opened["job"]["id"], job_id);

        let review = state.active_state().await.unwrap();
        let photos = photos_payload(&review, gpsify::review::PhotoFilter::NeedsReview)
            .await
            .unwrap();
        let arr = photos["photos"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let pid = arr[0]["id"].as_str().unwrap().to_string();

        // Decide accept
        let dec = decide_one(
            &review,
            &pid,
            DecisionBody {
                action: DecisionAction::Accept,
                lat: None,
                lng: None,
                place_name: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(dec["photo"]["status"], "accepted");

        // Dry-run apply works even with GPSIFY_DRY_RUN=1
        let dry_app = apply_impl(
            &state,
            ApplyArgs {
                job_id: job_id.clone(),
                out: None,
                in_place: false,
                dry_run: true,
                i_mean_it: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(dry_app["summary"]["eligible"], 1);
        assert!(dry_app["summary"]["dry_run"].as_bool().unwrap());

        // Guard rejects in-place without i_mean_it
        let bad_in_place = apply_impl(
            &state,
            ApplyArgs {
                job_id: job_id.clone(),
                out: None,
                in_place: true,
                dry_run: false,
                i_mean_it: false,
            },
        )
        .await
        .unwrap_err();
        assert!(bad_in_place.contains("confirmation"));

        // Ensure clean & status work
        let st = status_impl(&state, &job_id).await.unwrap();
        assert_eq!(st["status"]["counts"]["accepted"], 1);
        let cleaned = clean_impl(&state, &job_id).await.unwrap();
        assert_eq!(cleaned["kept_sqlite"], json!(true));
        let _ = exiftool_available();
    }
}
