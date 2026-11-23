//! Shared desktop application state across Tauri IPC commands and media protocol.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use gpsify::config::{live_http_blocked, load_settings, Settings};
use gpsify::exif::exiftool_available;
use gpsify::geocode::require_geocoder_ready;
use gpsify::media::heic_available;
use gpsify::propose::count_unpinned_proposals;
use gpsify::review::routes::{job_payload, list_low_rank_photos};
use gpsify::review::{build_review_state_for_desktop, ReviewState};
use gpsify::status::job_status;
use gpsify::store::resolve_job_handle;

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSnapshot {
    pub exiftool: bool,
    pub heic: bool,
    pub vision_provider: String,
    pub model: String,
    pub gemini_model: String,
    pub reasoning_effort: String,
    pub api_key_configured: bool,
    pub xai_key_configured: bool,
    pub gemini_key_configured: bool,
    pub geocoder_ok: bool,
    pub geocoder_error: Option<String>,
    pub geocoder_url: String,
    pub data_dir: String,
    pub dry_run: bool,
}

pub struct BusyGuard {
    slot: Arc<StdMutex<Option<String>>>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.slot.lock() {
            *guard = None;
        }
    }
}

pub struct DesktopState {
    settings: StdMutex<Settings>,
    pub active_review: Mutex<Option<ReviewState>>,
    busy: Arc<StdMutex<Option<String>>>,
    pub initial_handle: Option<String>,
    reload_from_env: bool,
}

impl DesktopState {
    pub fn from_env(initial_handle: Option<String>) -> Result<Self, String> {
        let settings = load_settings().map_err(|e| e.to_string())?;
        Ok(Self {
            settings: StdMutex::new(settings),
            active_review: Mutex::new(None),
            busy: Arc::new(StdMutex::new(None)),
            initial_handle,
            reload_from_env: true,
        })
    }

    pub fn with_settings(settings: Settings, initial_handle: Option<String>) -> Self {
        Self {
            settings: StdMutex::new(settings),
            active_review: Mutex::new(None),
            busy: Arc::new(StdMutex::new(None)),
            initial_handle,
            reload_from_env: false,
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().expect("settings lock").clone()
    }

    pub fn reload_settings(&self) -> Result<Settings, String> {
        if self.reload_from_env {
            let fresh = load_settings().map_err(|e| e.to_string())?;
            let mut guard = self.settings.lock().expect("settings lock");
            *guard = fresh.clone();
            Ok(fresh)
        } else {
            Ok(self.settings())
        }
    }

    pub fn acquire_busy(&self, stage: &str) -> Result<BusyGuard, String> {
        let mut guard = self.busy.lock().expect("busy lock");
        if let Some(current) = guard.as_deref() {
            return Err(format!("another operation is already running ({current})"));
        }
        *guard = Some(stage.to_string());
        Ok(BusyGuard {
            slot: Arc::clone(&self.busy),
        })
    }

    pub fn media_base_url() -> &'static str {
        #[cfg(target_os = "windows")]
        {
            "http://gpsify.localhost"
        }
        #[cfg(not(target_os = "windows"))]
        {
            "gpsify://localhost"
        }
    }

    pub fn doctor_snapshot(&self) -> DoctorSnapshot {
        let s = self.settings.lock().expect("settings lock").clone();
        let xai_key_configured = s
            .xai_api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        let gemini_key_configured = s
            .gemini_api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        let api_key_configured = if s.vision_provider == "gemini" {
            gemini_key_configured
        } else {
            xai_key_configured
        };
        let (geocoder_ok, geocoder_error) = match require_geocoder_ready(&s) {
            Ok(()) => (true, None),
            Err(err) => (false, Some(err.to_string())),
        };
        DoctorSnapshot {
            exiftool: exiftool_available(),
            heic: heic_available(),
            vision_provider: s.vision_provider,
            model: s.model,
            gemini_model: s.gemini_model,
            reasoning_effort: s.reasoning_effort,
            api_key_configured,
            xai_key_configured,
            gemini_key_configured,
            geocoder_ok,
            geocoder_error,
            geocoder_url: s.geocoder_url,
            data_dir: PathBuf::from(&s.data_dir).to_string_lossy().into_owned(),
            dry_run: live_http_blocked(),
        }
    }

    pub async fn active_state(&self) -> Result<ReviewState, String> {
        self.active_review
            .lock()
            .await
            .clone()
            .ok_or_else(|| "no active album opened".to_string())
    }

    pub async fn open_job_state(&self, handle: &str) -> Result<Value, String> {
        let settings = self.settings();
        let resolved = resolve_job_handle(&settings.data_dir, handle).map_err(|e| e.to_string())?;
        let review_state = build_review_state_for_desktop(settings.clone(), &resolved.job_id)
            .map_err(|e| e.to_string())?;

        let job = job_payload(&review_state).await.map_err(|(_, msg)| msg)?;

        let data_dir = settings.data_dir.clone();
        let jid = resolved.job_id.clone();
        let repo = review_state.repo.clone();
        let (rollup, unpinned, low_rank_count) = tokio::task::spawn_blocking(move || {
            let status = job_status(&data_dir, &jid).map_err(|e| e.to_string())?;
            let unpinned = count_unpinned_proposals(&repo).map_err(|e| e.to_string())?;
            let low_rank = list_low_rank_photos(&repo)
                .map_err(|e| e.to_string())?
                .len();
            Ok::<_, String>((status, unpinned, low_rank))
        })
        .await
        .map_err(|e| e.to_string())??;

        {
            let mut active = self.active_review.lock().await;
            *active = Some(review_state);
        }

        Ok(json!({
            "job": job,
            "status": rollup,
            "unpinned": unpinned,
            "low_rank_count": low_rank_count,
            "note": resolved.note,
            "media_base": Self::media_base_url(),
        }))
    }
}
