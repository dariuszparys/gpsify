//! Vision + geocode propose loop and geocode-only catch-up pass.
//!
//! Does not write GPS to photos. On start: crash-repairs `proposing` -> `pending`
//! and acquires `jobs/<id>/propose.lock`. `job.stats_json` is merged cumulatively
//! so later runs never erase prior spend; each run alone is saved under `last_run`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};

use crate::config::Settings;
use crate::exif::{classify_media, exiftool_available, is_processable, MediaKind};
use crate::geocode::{
    cache_key, geocode_cache_path, is_public_nominatim_url, require_geocoder_ready, GeocodeCache,
    GeocodeService, Geocoder, NominatimGeocoder, DEFAULT_UNCACHED_MAX,
    PUBLIC_NOMINATIM_CAP_WARNING,
};
use crate::media::ensure_preview;
use crate::providers::{
    GeminiProvider, VisionError, VisionProvider, XaiGrokProvider, PROMPT_VERSION,
};
use crate::reconcile::{
    geocode_and_reconcile, result_from_lookup, should_geocode, ReconcileResult,
};
use crate::store::{
    job_dir, JobExclusiveLock, JobRepo, JobRow, NewProposal, PhotoRow, ProposalRow, StoreError,
};
use crate::types::{
    Granularity, PhotoStatus, PinSource, PromptContext, ProposedSibling, RefusalReason,
    TaggedSibling, Usage, VisionCallOptions, VisionGuess, ALLOWED_VISION_PROVIDERS,
};

pub const RETRY_BACKOFF_S: &[f64] = &[2.0, 8.0, 30.0];
pub const DEFAULT_CONCURRENCY: usize = 3;
pub const UI_RANK_REFUSE: f64 = 0.25;
pub const USD_PER_MILLION_INPUT: f64 = 2.0;
pub const USD_PER_MILLION_OUTPUT: f64 = 6.0;
pub const LOCK_FILENAME: &str = "propose.lock";

pub const GEMINI_TIER_WARNING: &str = "gemini has no per-request store=False; free-tier keys may be used to improve Google products. Use a billing-enabled key for real photos.";

pub type SleepFn = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
pub type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ProposeError(pub String);

impl From<StoreError> for ProposeError {
    fn from(err: StoreError) -> Self {
        Self(err.to_string())
    }
}

pub fn default_sleep_fn() -> SleepFn {
    Arc::new(|d| Box::pin(tokio::time::sleep(d)))
}

pub fn instant_sleep_fn() -> SleepFn {
    Arc::new(|_| Box::pin(std::future::ready(())))
}

fn promo_until() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 12, 31).expect("valid promo date")
}

fn usd_rates_for_model(model: &str, on: NaiveDate) -> (f64, f64) {
    let key = model.trim();
    let after_promo = on > promo_until();
    match key {
        "grok-4.6" => (2.0, 6.0),
        "gemini-2.5-pro" => (1.25, 10.0),
        "gemini-2.5-flash" => (0.30, 2.50),
        "gemini-2.5-flash-lite" => (0.10, 0.40),
        "gemini-3.5-flash" => (1.50, 9.00),
        "gemini-3.5-flash-lite" => (0.30, 2.50),
        "gemini-3.6-flash" | "gemini-3.7-flash" => {
            if after_promo {
                (1.50, 7.50)
            } else {
                (0.75, 3.75)
            }
        }
        _ => (USD_PER_MILLION_INPUT, USD_PER_MILLION_OUTPUT),
    }
}

/// Order-of-magnitude cost estimate from published rates; reasoning bills as output.
pub fn estimate_usd(usage: &Usage, model: Option<&str>, on: Option<NaiveDate>) -> f64 {
    let today = on.unwrap_or_else(|| Utc::now().date_naive());
    let (rate_in, rate_out) = usd_rates_for_model(model.unwrap_or(""), today);
    let output = usage.output_tokens + usage.reasoning_tokens;
    (usage.input_tokens as f64 * rate_in + output as f64 * rate_out) / 1_000_000.0
}

pub fn clip01(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

pub fn granularity_score(granularity: Granularity) -> f64 {
    match granularity {
        Granularity::Exact => 1.0,
        Granularity::Street => 0.85,
        Granularity::Neighborhood => 0.7,
        Granularity::City => 0.55,
        Granularity::Region => 0.3,
        Granularity::Country => 0.15,
        Granularity::Unknown => 0.0,
    }
}

pub fn clue_score(guess: &VisionGuess) -> f64 {
    let n_clues = guess.clues.len() as f64;
    let bonus = if guess
        .clues
        .iter()
        .any(|c| matches!(c.kind.as_str(), "sign" | "text"))
    {
        0.15
    } else {
        0.0
    };
    clip01((0.25 * n_clues + bonus).min(1.0))
}

pub fn geocode_score(result: &ReconcileResult) -> f64 {
    match result.pin_source {
        PinSource::Geocoder => {
            if result.model_agree == Some(false) {
                0.6
            } else {
                1.0
            }
        }
        PinSource::User => 1.0,
        PinSource::None => 0.2,
    }
}

fn is_hint_stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "album"
            | "an"
            | "and"
            | "at"
            | "from"
            | "in"
            | "of"
            | "or"
            | "photos"
            | "the"
            | "to"
            | "trip"
            | "vacation"
            | "weekend"
            | "year"
    )
}

fn split_hint_tokens(hint: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in hint.chars() {
        if ch.is_alphabetic() || ch == '-' || ch == '\'' {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// True when the album hint looks like it names a city rather than just `"weekend trip"`.
pub fn hint_names_city(hint: Option<&str>) -> bool {
    let Some(raw) = hint else {
        return false;
    };
    if raw.trim().is_empty() {
        return false;
    }
    for token in split_hint_tokens(raw) {
        let lowered = token.to_lowercase();
        if is_hint_stopword(&lowered) {
            continue;
        }
        let first_upper = token.chars().next().is_some_and(char::is_uppercase);
        if first_upper && token.chars().count() >= 3 {
            return true;
        }
        if lowered.chars().count() >= 4 {
            return true;
        }
    }
    false
}

pub fn cluster_score(
    guess: &VisionGuess,
    proposed_siblings: &[ProposedSibling],
    gps_siblings: &[TaggedSibling],
) -> f64 {
    if proposed_siblings.is_empty() && gps_siblings.is_empty() {
        return 0.0;
    }
    let total = proposed_siblings.len() + gps_siblings.len();
    if total == 0 {
        return 0.0;
    }
    let guess_cc = guess
        .country_code
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    let guess_place = guess
        .primary_place_name
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let mut matches = 0usize;
    for sibling in proposed_siblings {
        let sib_cc = sibling
            .country_code
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_ascii_uppercase();
        if !guess_cc.is_empty() && !sib_cc.is_empty() && guess_cc == sib_cc {
            matches += 1;
            continue;
        }
        if !guess_place.is_empty() && sibling.place_name.to_lowercase().contains(&guess_place) {
            matches += 1;
        }
    }
    matches as f64 / total as f64
}

pub fn ui_rank(
    guess: &VisionGuess,
    result: &ReconcileResult,
    proposed_siblings: &[ProposedSibling],
    gps_siblings: &[TaggedSibling],
) -> f64 {
    clip01(
        0.45 * clip01(guess.model_confidence)
            + 0.20 * clip01(granularity_score(guess.granularity))
            + 0.15 * clip01(clue_score(guess))
            + 0.10 * clip01(geocode_score(result))
            + 0.10 * clip01(cluster_score(guess, proposed_siblings, gps_siblings)),
    )
}

pub fn refuse_guess_reason(
    guess: &VisionGuess,
    album_hint: Option<&str>,
    rank: f64,
) -> Option<&'static str> {
    if guess.refusal != RefusalReason::None {
        return Some("refusal");
    }
    let has_sign_text = guess
        .clues
        .iter()
        .any(|c| matches!(c.kind.as_str(), "sign" | "text"));
    let weak_granularity = matches!(
        guess.granularity,
        Granularity::Unknown | Granularity::Country
    );
    if weak_granularity && !has_sign_text && !hint_names_city(album_hint) {
        return Some("weak_granularity");
    }
    if rank < UI_RANK_REFUSE {
        return Some("low_ui_rank");
    }
    None
}

fn blocked_guess(detail: &str) -> VisionGuess {
    VisionGuess {
        refusal: RefusalReason::PolicyRefusal,
        refusal_detail: Some(detail.to_string()),
        primary_place_name: None,
        country_code: None,
        admin1: None,
        granularity: Granularity::Unknown,
        lat_hint: None,
        lng_hint: None,
        model_confidence: 0.0,
        clues: Vec::new(),
        candidates: Vec::new(),
        used_album_hint: false,
        used_filename_hint: false,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProposeSummary {
    pub job_id: String,
    pub planned: usize,
    pub proposed: usize,
    pub refused: usize,
    pub error: usize,
    pub skipped_existing: usize,
    pub skipped_no_preview: usize,
    pub dry_run: bool,
    pub cache_hits: usize,
    pub uncached_lookups: usize,
    pub est_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub warnings: Vec<String>,
    pub planned_relpaths: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeocodeSummary {
    pub job_id: String,
    pub planned: usize,
    pub pinned: usize,
    pub still_unpinned: usize,
    pub unrefused: usize,
    pub dry_run: bool,
    pub cache_hits: usize,
    pub uncached_lookups: usize,
    pub distinct_places: usize,
    pub cold_places: usize,
    pub warnings: Vec<String>,
    pub planned_relpaths: Vec<String>,
}

#[derive(Clone)]
pub struct ProposeOptions {
    pub settings: Settings,
    pub limit: Option<usize>,
    pub concurrency: usize,
    pub max_cost: Option<f64>,
    pub dry_run: bool,
    pub force: bool,
    pub no_context: bool,
    pub provider_name: Option<String>,
    pub geocode_uncached_max: usize,
    pub vision: Option<Arc<dyn VisionProvider>>,
    pub geocoder: Option<Arc<dyn Geocoder>>,
    pub sleep: Option<SleepFn>,
    pub progress: Option<ProgressFn>,
}

impl ProposeOptions {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            limit: None,
            concurrency: DEFAULT_CONCURRENCY,
            max_cost: None,
            dry_run: false,
            force: false,
            no_context: false,
            provider_name: None,
            geocode_uncached_max: DEFAULT_UNCACHED_MAX,
            vision: None,
            geocoder: None,
            sleep: None,
            progress: None,
        }
    }
}

#[derive(Clone)]
pub struct GeocodeOptions {
    pub settings: Settings,
    pub limit: Option<usize>,
    pub dry_run: bool,
    pub geocode_uncached_max: usize,
    pub geocoder: Option<Arc<dyn Geocoder>>,
    pub progress: Option<ProgressFn>,
}

impl GeocodeOptions {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            limit: None,
            dry_run: false,
            geocode_uncached_max: DEFAULT_UNCACHED_MAX,
            geocoder: None,
            progress: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ProposalSummaryMemo {
    place_name: Option<String>,
    granularity: Option<String>,
    model_confidence: Option<f64>,
    country_code: Option<String>,
    pin_source: Option<String>,
    provider: String,
    model: String,
    prompt_version: String,
    raw_json: String,
}

impl ProposalSummaryMemo {
    fn from_row(row: &ProposalRow) -> Self {
        let country_code = serde_json::from_str::<VisionGuess>(&row.raw_json)
            .ok()
            .and_then(|g| g.country_code);
        Self {
            place_name: row.place_name.clone(),
            granularity: row.granularity.clone(),
            model_confidence: row.model_confidence,
            country_code,
            pin_source: row.pin_source.clone(),
            provider: row.provider.clone(),
            model: row.model.clone(),
            prompt_version: row.prompt_version.clone(),
            raw_json: row.raw_json.clone(),
        }
    }
}

fn provider_key_env(provider_key: &str) -> &'static str {
    match provider_key {
        "gemini" => "GEMINI_API_KEY",
        _ => "XAI_API_KEY",
    }
}

fn settings_api_key<'a>(settings: &'a Settings, provider_key: &str) -> Option<&'a str> {
    let key = match provider_key {
        "xai" => settings.xai_api_key.as_deref(),
        "gemini" => settings.gemini_api_key.as_deref(),
        _ => None,
    }?;
    if key.trim().is_empty() {
        None
    } else {
        Some(key)
    }
}

fn resolve_vision(
    settings: &Settings,
    vision: Option<Arc<dyn VisionProvider>>,
    provider_key: &str,
) -> Result<(Arc<dyn VisionProvider>, bool), ProposeError> {
    if let Some(v) = vision {
        return Ok((v, false));
    }
    let key = settings_api_key(settings, provider_key).ok_or_else(|| {
        ProposeError(format!(
            "{} is required for propose --provider {provider_key}",
            provider_key_env(provider_key)
        ))
    })?;
    let provider: Arc<dyn VisionProvider> = match provider_key {
        "gemini" => Arc::new(
            GeminiProvider::new(key, &settings.gemini_model)
                .map_err(|e| ProposeError(e.to_string()))?,
        ),
        _ => Arc::new(
            XaiGrokProvider::new(key, &settings.model).map_err(|e| ProposeError(e.to_string()))?,
        ),
    };
    Ok((provider, true))
}

fn resolve_geocoder(
    settings: &Settings,
    geocoder: Option<Arc<dyn Geocoder>>,
) -> Result<(Arc<dyn Geocoder>, bool), ProposeError> {
    if let Some(g) = geocoder {
        let is_fake = g.name() == "fake";
        return Ok((g, !is_fake));
    }
    let client = NominatimGeocoder::new(
        &settings.geocoder_url,
        settings.nominatim_contact.as_deref(),
        &settings.geocoder,
    )
    .map_err(|e| ProposeError(e.to_string()))?;
    Ok((Arc::new(client), true))
}

fn emit(progress: Option<&ProgressFn>, msg: &str) {
    if let Some(cb) = progress {
        cb(msg);
    }
}

fn log_event(log_file: &StdMutex<Option<File>>, event: Value) {
    let Ok(mut guard) = log_file.lock() else {
        return;
    };
    let Some(handle) = guard.as_mut() else {
        return;
    };
    let mut map = match event {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    map.insert(
        "ts".to_string(),
        Value::String(Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    if let Ok(line) = serde_json::to_string(&Value::Object(map)) {
        let _ = writeln!(handle, "{line}");
        let _ = handle.flush();
    }
}

fn prior_stats(job: Option<&JobRow>) -> (Map<String, Value>, Map<String, Value>) {
    let mut existing = Map::new();
    if let Some(j) = job {
        if !j.stats_json.trim().is_empty() {
            if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&j.stats_json) {
                existing = parsed;
            }
        }
    }
    let prior = existing
        .get("propose")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    (existing, prior)
}

fn prior_number(prior: &Map<String, Value>, key: &str) -> f64 {
    prior.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn prior_optional_number(prior: &Map<String, Value>, key: &str) -> Option<f64> {
    prior.get(key).and_then(Value::as_f64)
}

fn prior_map(prior: &Map<String, Value>, key: &str) -> Map<String, Value> {
    prior
        .get(key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn latency_percentiles(latencies: &[f64]) -> (Option<f64>, Option<f64>) {
    if latencies.is_empty() {
        return (None, None);
    }
    let mut ordered = latencies.to_vec();
    ordered.sort_by(f64::total_cmp);
    let n = ordered.len();
    let p50 = if n % 2 == 1 {
        ordered[n / 2]
    } else {
        (ordered[n / 2 - 1] + ordered[n / 2]) / 2.0
    };
    let idx = (n - 1).min(((n - 1) as f64 * 0.95).round() as usize);
    let p95 = ordered[idx];
    (Some(p50), Some(p95))
}

fn hit_rate(cache_hits: usize, uncached: usize) -> Option<f64> {
    let lookups = cache_hits + uncached;
    if lookups == 0 {
        None
    } else {
        Some(cache_hits as f64 / lookups as f64)
    }
}

fn round_to_6(val: f64) -> f64 {
    (val * 1_000_000.0).round() / 1_000_000.0
}

fn round_to_4(val: f64) -> f64 {
    (val * 10_000.0).round() / 10_000.0
}

fn write_propose_stats(
    repo: &JobRepo,
    summary: &ProposeSummary,
    latencies: &[f64],
    refusal_hist: &BTreeMap<String, usize>,
    cache_hits: usize,
    uncached: usize,
    baseline: &Map<String, Value>,
) -> Result<(), StoreError> {
    let job = repo.get_job()?;
    let (mut existing, _) = prior_stats(job.as_ref());
    let (p50, p95) = latency_percentiles(latencies);

    let run_counts = [
        ("planned", summary.planned),
        ("proposed", summary.proposed),
        ("refused", summary.refused),
        ("error", summary.error),
        ("skipped_no_preview", summary.skipped_no_preview),
    ];

    let prior_counts = prior_map(baseline, "counts");
    let mut total_counts = Map::new();
    for (k, v) in run_counts {
        let prev = prior_number(&prior_counts, k) as u64;
        total_counts.insert(k.to_string(), json!(prev + v as u64));
    }

    let prior_hist = prior_map(baseline, "refusal_histogram");
    let mut all_hist_keys = BTreeSet::new();
    for k in prior_hist.keys() {
        all_hist_keys.insert(k.clone());
    }
    for k in refusal_hist.keys() {
        all_hist_keys.insert(k.clone());
    }
    let mut total_hist = Map::new();
    for k in all_hist_keys {
        let prev = prior_number(&prior_hist, &k) as u64;
        let cur = refusal_hist.get(&k).copied().unwrap_or(0) as u64;
        total_hist.insert(k, json!(prev + cur));
    }

    let total_cache_hits = prior_number(baseline, "geocode_cache_hits") as usize + cache_hits;
    let total_uncached = prior_number(baseline, "geocode_uncached") as usize + uncached;

    let mut run_counts_map = Map::new();
    for (k, v) in run_counts {
        run_counts_map.insert(k.to_string(), json!(v));
    }

    let eff_p50 = p50.or_else(|| prior_optional_number(baseline, "latency_ms_p50"));
    let eff_p95 = p95.or_else(|| prior_optional_number(baseline, "latency_ms_p95"));

    let last_run = json!({
        "counts": Value::Object(run_counts_map),
        "latency_ms_p50": p50,
        "latency_ms_p95": p95,
        "est_usd": round_to_6(summary.est_usd),
        "geocode_cache_hits": cache_hits,
        "geocode_uncached": uncached,
        "geocode_cache_hit_rate": hit_rate(cache_hits, uncached),
        "refusal_histogram": refusal_hist,
        "input_tokens": summary.input_tokens,
        "output_tokens": summary.output_tokens,
        "reasoning_tokens": summary.reasoning_tokens,
    });

    existing.insert(
        "propose".to_string(),
        json!({
            "counts": Value::Object(total_counts),
            "latency_ms_p50": eff_p50,
            "latency_ms_p95": eff_p95,
            "est_usd": round_to_6(prior_number(baseline, "est_usd") + summary.est_usd),
            "geocode_cache_hits": total_cache_hits,
            "geocode_uncached": total_uncached,
            "geocode_cache_hit_rate": hit_rate(total_cache_hits, total_uncached),
            "refusal_histogram": Value::Object(total_hist),
            "input_tokens": prior_number(baseline, "input_tokens") as u64 + summary.input_tokens,
            "output_tokens": prior_number(baseline, "output_tokens") as u64 + summary.output_tokens,
            "reasoning_tokens": prior_number(baseline, "reasoning_tokens") as u64 + summary.reasoning_tokens,
            "runs": prior_number(baseline, "runs") as u64 + 1,
            "last_run": last_run,
        }),
    );
    repo.set_stats_json(&Value::Object(existing).to_string())
}

fn write_geocode_stats(repo: &JobRepo, summary: &GeocodeSummary) -> Result<(), StoreError> {
    let job = repo.get_job()?;
    let (mut existing, _) = prior_stats(job.as_ref());
    let prior = prior_map(&existing, "geocode");
    let run_items = [
        ("planned", summary.planned),
        ("pinned", summary.pinned),
        ("still_unpinned", summary.still_unpinned),
        ("unrefused", summary.unrefused),
        ("geocode_cache_hits", summary.cache_hits),
        ("geocode_uncached", summary.uncached_lookups),
    ];
    let mut run_map = Map::new();
    let mut geocode_map = Map::new();
    for (k, v) in run_items {
        run_map.insert(k.to_string(), json!(v));
        let prev = prior_number(&prior, k) as usize;
        geocode_map.insert(k.to_string(), json!(prev + v));
    }
    geocode_map.insert(
        "runs".to_string(),
        json!(prior_number(&prior, "runs") as usize + 1),
    );
    geocode_map.insert("last_run".to_string(), Value::Object(run_map));
    existing.insert("geocode".to_string(), Value::Object(geocode_map));
    repo.set_stats_json(&Value::Object(existing).to_string())
}

fn is_eligible_status(status: &str, force: bool) -> bool {
    match status {
        "pending" | "error" => true,
        "proposed" | "refused" => force,
        _ => false,
    }
}

fn select_photos(
    photos: &[PhotoRow],
    current_by_photo: &HashMap<String, ProposalSummaryMemo>,
    force: bool,
    limit: Option<usize>,
) -> Vec<PhotoRow> {
    let mut selected: Vec<PhotoRow> = photos
        .iter()
        .filter(|p| {
            is_eligible_status(&p.status, force)
                && is_processable(classify_media(Path::new(&p.path)))
                && (force || !current_by_photo.contains_key(&p.id))
        })
        .cloned()
        .collect();
    selected.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    if let Some(lim) = limit {
        selected.truncate(lim);
    }
    selected
}

fn cluster_context(
    photo: &PhotoRow,
    photos: &[PhotoRow],
    current_by_photo: &HashMap<String, ProposalSummaryMemo>,
) -> (Vec<ProposedSibling>, Vec<TaggedSibling>) {
    let Some(cluster_id) = photo.cluster_id.as_deref() else {
        return (Vec::new(), Vec::new());
    };
    let mut members: Vec<&PhotoRow> = photos
        .iter()
        .filter(|row| row.cluster_id.as_deref() == Some(cluster_id))
        .collect();
    members.sort_by(|a, b| {
        let ats = a.datetime_original.as_deref().unwrap_or("");
        let bts = b.datetime_original.as_deref().unwrap_or("");
        ats.cmp(bts).then_with(|| a.relpath.cmp(&b.relpath))
    });

    let mut proposed = Vec::new();
    let mut gps = Vec::new();
    for member in members {
        if member.id == photo.id {
            continue;
        }
        let filename = Path::new(&member.relpath)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&member.relpath)
            .to_string();
        if member.has_gps != 0 {
            if let (Some(lat), Some(lng)) = (member.existing_lat, member.existing_lng) {
                if gps.len() < 4 {
                    gps.push(TaggedSibling { filename, lat, lng });
                }
            }
            continue;
        }
        if proposed.len() >= 8 {
            continue;
        }
        let Some(current) = current_by_photo.get(&member.id) else {
            continue;
        };
        let gran = current
            .granularity
            .as_deref()
            .and_then(|s| s.parse::<Granularity>().ok())
            .unwrap_or(Granularity::Unknown);
        proposed.push(ProposedSibling {
            filename,
            captured_at: member.datetime_original.clone(),
            place_name: current.place_name.clone().unwrap_or_default(),
            country_code: current.country_code.clone(),
            granularity: gran,
            model_confidence: current.model_confidence.unwrap_or(0.0),
        });
    }
    (proposed, gps)
}

async fn guess_with_retries(
    vision: &dyn VisionProvider,
    image_jpeg: &[u8],
    context: &PromptContext,
    options: &VisionCallOptions,
    sleep: &SleepFn,
) -> Result<(VisionGuess, Usage), VisionError> {
    let mut delays = vec![0.0_f64];
    delays.extend_from_slice(RETRY_BACKOFF_S);
    let mut last_err: Option<VisionError> = None;
    for delay in delays {
        if delay > 0.0 {
            sleep(Duration::from_secs_f64(delay)).await;
        }
        match vision.guess(image_jpeg, context, options).await {
            Ok(ok) => return Ok(ok),
            Err(err) if err.is_retryable() => {
                last_err = Some(err);
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or_else(|| VisionError::Retryable("retry exhausted".to_string())))
}

struct SharedProposeState {
    repo: JobRepo,
    photos: Vec<PhotoRow>,
    current_by_photo: HashMap<String, ProposalSummaryMemo>,
    summary: ProposeSummary,
    latencies: Vec<f64>,
    refusal_hist: BTreeMap<String, usize>,
    baseline: Map<String, Value>,
}

impl SharedProposeState {
    fn checkpoint(&self, service: &GeocodeService) -> Result<(), StoreError> {
        write_propose_stats(
            &self.repo,
            &self.summary,
            &self.latencies,
            &self.refusal_hist,
            service.cache_hits(),
            service.uncached_lookups(),
            &self.baseline,
        )
    }
}

fn serialize_geocode_json(result: &ReconcileResult) -> Result<Option<String>, ProposeError> {
    if let Some(hit) = &result.geocode {
        let mut val = serde_json::to_value(hit).map_err(|e| ProposeError(e.to_string()))?;
        if let Some(obj) = val.as_object_mut() {
            obj.insert("skipped_geocode".to_string(), json!(result.skipped_geocode));
            obj.insert("model_lat".to_string(), json!(result.model_lat));
            obj.insert("model_lng".to_string(), json!(result.model_lng));
        }
        return Ok(Some(val.to_string()));
    }
    if let Some(skip) = &result.skipped_geocode {
        return Ok(Some(json!({ "skipped_geocode": skip }).to_string()));
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn process_one(
    state: Arc<Mutex<SharedProposeState>>,
    photo: PhotoRow,
    settings: Settings,
    vision: Arc<dyn VisionProvider>,
    service: Arc<GeocodeService>,
    job_root: PathBuf,
    hint: Option<String>,
    folder_basename: String,
    no_context: bool,
    sleep: SleepFn,
    progress: Option<ProgressFn>,
    log_file: Arc<StdMutex<Option<File>>>,
    max_cost: Option<f64>,
) {
    let photo_id = photo.id.clone();
    let routing_name = Path::new(&photo.relpath)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&photo.relpath)
        .to_string();

    let (proposed_siblings, gps_siblings) = {
        let mut guard = state.lock().await;
        if let Some(cap) = max_cost {
            if guard.summary.est_usd >= cap {
                let msg = "estimated cost budget reached; remaining photos can be resumed";
                if !guard.summary.warnings.iter().any(|w| w == msg) {
                    guard.summary.warnings.push(msg.to_string());
                }
                return;
            }
        }
        if let Err(err) = guard
            .repo
            .set_photo_status(&photo_id, PhotoStatus::Proposing, None, None)
        {
            guard.summary.error += 1;
            log_event(
                &log_file,
                json!({
                    "level": "error",
                    "photo_id": photo_id,
                    "event": "status_error",
                    "error": err.to_string(),
                }),
            );
            return;
        }
        cluster_context(&photo, &guard.photos, &guard.current_by_photo)
    };

    let context = if no_context {
        PromptContext {
            hint: None,
            folder_basename: String::new(),
            filename: routing_name.clone(),
            datetime_original: None,
            offset: None,
            make: None,
            model: None,
            proposed_siblings: Vec::new(),
            gps_siblings: Vec::new(),
            no_context: true,
        }
    } else {
        PromptContext {
            hint: hint.clone(),
            folder_basename,
            filename: routing_name.clone(),
            datetime_original: photo.datetime_original.clone(),
            offset: None,
            make: photo.make.clone(),
            model: photo.model.clone(),
            proposed_siblings: proposed_siblings.clone(),
            gps_siblings: gps_siblings.clone(),
            no_context: false,
        }
    };

    let source_path = PathBuf::from(&photo.path);
    let expected_sha = photo.sha256.clone();
    let jr = job_root.clone();
    let pid = photo_id.clone();
    let preview_res =
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>, (PhotoStatus, String)> {
            let kind = classify_media(&source_path);
            let preview_path = ensure_preview(&jr, &pid, &source_path, &expected_sha, Some(kind))
                .map_err(|e| (e.status, e.message))?;
            std::fs::read(&preview_path).map_err(|e| (PhotoStatus::Error, e.to_string()))
        })
        .await;

    let image_jpeg = match preview_res {
        Ok(Ok(bytes)) => bytes,
        Ok(Err((err_status, err_msg))) => {
            let mut guard = state.lock().await;
            let _ = guard
                .repo
                .set_photo_status(&photo_id, err_status, Some(&err_msg), None);
            if err_status == PhotoStatus::SkippedNoPreview {
                guard.summary.skipped_no_preview += 1;
            } else {
                guard.summary.error += 1;
            }
            emit(
                progress.as_ref(),
                &format!("{routing_name}: {}", err_status.as_str()),
            );
            let _ = guard.checkpoint(&service);
            return;
        }
        Err(join_err) => {
            let mut guard = state.lock().await;
            let _ = guard.repo.set_photo_status(
                &photo_id,
                PhotoStatus::Error,
                Some(&join_err.to_string()),
                None,
            );
            guard.summary.error += 1;
            emit(progress.as_ref(), &format!("{routing_name}: error"));
            let _ = guard.checkpoint(&service);
            return;
        }
    };

    let options = match VisionCallOptions::new(&settings.reasoning_effort, settings.timeout_s) {
        Ok(opts) => opts,
        Err(err) => {
            let mut guard = state.lock().await;
            let _ = guard.repo.set_photo_status(
                &photo_id,
                PhotoStatus::Error,
                Some(&err.to_string()),
                None,
            );
            guard.summary.error += 1;
            emit(progress.as_ref(), &format!("{routing_name}: error"));
            let _ = guard.checkpoint(&service);
            return;
        }
    };

    let started = Instant::now();
    let guess_res =
        guess_with_retries(vision.as_ref(), &image_jpeg, &context, &options, &sleep).await;
    let latency_ms = started.elapsed().as_millis() as i64;

    let (guess, usage) = match guess_res {
        Ok(ok) => ok,
        Err(err) if err.is_blocked() => {
            let blocked = blocked_guess(&err.to_string());
            let raw_json = serde_json::to_string(&blocked).unwrap_or_else(|_| "{}".to_string());
            let mut guard = state.lock().await;
            guard.latencies.push(latency_ms as f64);
            let _ = guard.repo.insert_proposal(NewProposal {
                photo_id: photo_id.clone(),
                provider: vision.name().to_string(),
                model: vision.model().to_string(),
                prompt_version: PROMPT_VERSION.to_string(),
                raw_json: raw_json.clone(),
                place_name: None,
                granularity: Some(blocked.granularity.as_str().to_string()),
                model_confidence: Some(blocked.model_confidence),
                ui_rank: Some(0.0),
                refusal: Some(blocked.refusal.as_str().to_string()),
                pin_lat: None,
                pin_lng: None,
                pin_source: Some(PinSource::None),
                model_agree: None,
                geocode_json: Some(json!({"skipped_geocode": "refusal"}).to_string()),
                input_tokens: Some(0),
                output_tokens: Some(0),
                reasoning_tokens: Some(0),
                latency_ms: Some(latency_ms),
                current: true,
                proposal_id: None,
                created_at: None,
            });
            let _ = guard.repo.set_photo_status(
                &photo_id,
                PhotoStatus::Refused,
                Some(&err.to_string()),
                None,
            );
            guard.current_by_photo.insert(
                photo_id.clone(),
                ProposalSummaryMemo {
                    place_name: None,
                    granularity: Some(blocked.granularity.as_str().to_string()),
                    model_confidence: Some(blocked.model_confidence),
                    country_code: None,
                    pin_source: Some(PinSource::None.as_str().to_string()),
                    provider: vision.name().to_string(),
                    model: vision.model().to_string(),
                    prompt_version: PROMPT_VERSION.to_string(),
                    raw_json,
                },
            );
            if let Some(p) = guard.photos.iter_mut().find(|r| r.id == photo_id) {
                p.status = PhotoStatus::Refused.as_str().to_string();
            }
            guard.summary.refused += 1;
            *guard
                .refusal_hist
                .entry(blocked.refusal.as_str().to_string())
                .or_insert(0) += 1;
            log_event(
                &log_file,
                json!({
                    "level": "warning",
                    "photo_id": photo_id,
                    "event": "vision_blocked",
                    "ms": latency_ms,
                    "error": err.to_string(),
                    "status": PhotoStatus::Refused.as_str(),
                }),
            );
            emit(
                progress.as_ref(),
                &format!(
                    "{}/{} proposed, {} refused, {} error, ~${:.2} est (incl. reasoning_tokens)",
                    guard.summary.proposed,
                    guard.summary.planned,
                    guard.summary.refused,
                    guard.summary.error,
                    guard.summary.est_usd
                ),
            );
            let _ = guard.checkpoint(&service);
            return;
        }
        Err(err) => {
            let mut guard = state.lock().await;
            let _ = guard.repo.set_photo_status(
                &photo_id,
                PhotoStatus::Error,
                Some(&err.to_string()),
                None,
            );
            guard.summary.error += 1;
            guard.latencies.push(latency_ms as f64);
            log_event(
                &log_file,
                json!({
                    "level": "error",
                    "photo_id": photo_id,
                    "event": "vision_error",
                    "ms": latency_ms,
                    "error": err.to_string(),
                }),
            );
            emit(progress.as_ref(), &format!("{routing_name}: error"));
            let _ = guard.checkpoint(&service);
            return;
        }
    };

    let usd = estimate_usd(&usage, Some(vision.model()), None);
    {
        let mut guard = state.lock().await;
        guard.latencies.push(latency_ms as f64);
        guard.summary.est_usd += usd;
        guard.summary.input_tokens += usage.input_tokens;
        guard.summary.output_tokens += usage.output_tokens;
        guard.summary.reasoning_tokens += usage.reasoning_tokens;
        let _ = guard.checkpoint(&service);
    }

    let result = geocode_and_reconcile(&guess, &service).await;
    let rank = ui_rank(&guess, &result, &proposed_siblings, &gps_siblings);
    let effective_hint = if no_context { None } else { hint.as_deref() };
    let refuse = refuse_guess_reason(&guess, effective_hint, rank);

    let mut photo_status = result.photo_status;
    let mut pin_source = result.pin_source;
    let mut pin_lat = result.pin_lat;
    let mut pin_lng = result.pin_lng;
    let mut model_agree = result.model_agree;
    if let Some(reason) = refuse {
        photo_status = PhotoStatus::Refused;
        if reason == "refusal" {
            pin_source = PinSource::None;
            pin_lat = None;
            pin_lng = None;
            model_agree = None;
        }
    }

    let geocode_json = serialize_geocode_json(&result).ok().flatten();
    let agree_int = model_agree.map(i64::from);
    let raw_json = serde_json::to_string(&guess).unwrap_or_else(|_| "{}".to_string());

    let mut guard = state.lock().await;
    let _ = guard.repo.insert_proposal(NewProposal {
        photo_id: photo_id.clone(),
        provider: vision.name().to_string(),
        model: vision.model().to_string(),
        prompt_version: PROMPT_VERSION.to_string(),
        raw_json: raw_json.clone(),
        place_name: guess.primary_place_name.clone(),
        granularity: Some(guess.granularity.as_str().to_string()),
        model_confidence: Some(guess.model_confidence),
        ui_rank: Some(rank),
        refusal: Some(guess.refusal.as_str().to_string()),
        pin_lat,
        pin_lng,
        pin_source: Some(pin_source),
        model_agree: agree_int,
        geocode_json,
        input_tokens: Some(usage.input_tokens as i64),
        output_tokens: Some(usage.output_tokens as i64),
        reasoning_tokens: Some(usage.reasoning_tokens as i64),
        latency_ms: Some(latency_ms),
        current: true,
        proposal_id: None,
        created_at: None,
    });
    let _ = guard
        .repo
        .set_photo_status(&photo_id, photo_status, None, None);

    guard.current_by_photo.insert(
        photo_id.clone(),
        ProposalSummaryMemo {
            place_name: guess.primary_place_name.clone(),
            granularity: Some(guess.granularity.as_str().to_string()),
            model_confidence: Some(guess.model_confidence),
            country_code: guess.country_code.clone(),
            pin_source: Some(pin_source.as_str().to_string()),
            provider: vision.name().to_string(),
            model: vision.model().to_string(),
            prompt_version: PROMPT_VERSION.to_string(),
            raw_json,
        },
    );
    if let Some(p) = guard.photos.iter_mut().find(|r| r.id == photo_id) {
        p.status = photo_status.as_str().to_string();
    }

    match photo_status {
        PhotoStatus::Proposed => guard.summary.proposed += 1,
        PhotoStatus::Refused => {
            guard.summary.refused += 1;
            *guard
                .refusal_hist
                .entry(guess.refusal.as_str().to_string())
                .or_insert(0) += 1;
        }
        _ => guard.summary.error += 1,
    }

    log_event(
        &log_file,
        json!({
            "level": "info",
            "photo_id": photo_id,
            "event": "vision_ok",
            "model": vision.model(),
            "ms": latency_ms,
            "place": guess.primary_place_name,
            "ui_rank": round_to_4(rank),
            "in": usage.input_tokens,
            "out": usage.output_tokens,
            "reason": usage.reasoning_tokens,
            "est_usd": round_to_6(usd),
            "status": photo_status.as_str(),
        }),
    );
    emit(
        progress.as_ref(),
        &format!(
            "{}/{} proposed, {} refused, {} error, ~${:.2} est (incl. reasoning_tokens)",
            guard.summary.proposed,
            guard.summary.planned,
            guard.summary.refused,
            guard.summary.error,
            guard.summary.est_usd
        ),
    );
    let _ = guard.checkpoint(&service);
}

/// Run vision + geocode propose loop on `job_id`. Does not write GPS.
pub async fn run_propose(
    job_id: &str,
    mut opts: ProposeOptions,
) -> Result<ProposeSummary, ProposeError> {
    if let Some(max_cost) = opts.max_cost {
        if !max_cost.is_finite() || max_cost <= 0.0 {
            return Err(ProposeError(
                "--max-cost must be finite and > 0".to_string(),
            ));
        }
        opts.concurrency = 1;
    }
    if opts.concurrency < 1 {
        return Err(ProposeError("--concurrency must be >= 1".to_string()));
    }
    if let Some(limit) = opts.limit {
        if limit < 1 {
            return Err(ProposeError("--limit must be >= 1".to_string()));
        }
    }

    let provider_key = opts
        .provider_name
        .as_deref()
        .unwrap_or(&opts.settings.vision_provider)
        .trim()
        .to_ascii_lowercase();
    if opts.vision.is_none() && !ALLOWED_VISION_PROVIDERS.contains(&provider_key.as_str()) {
        return Err(ProposeError(format!(
            "--provider must be one of gemini, xai; got {provider_key:?}"
        )));
    }
    if !opts.dry_run
        && opts.vision.is_none()
        && settings_api_key(&opts.settings, &provider_key).is_none()
    {
        return Err(ProposeError(format!(
            "{} is required for propose --provider {provider_key}",
            provider_key_env(&provider_key)
        )));
    }
    require_geocoder_ready(&opts.settings).map_err(|e| ProposeError(e.to_string()))?;

    let data_dir = &opts.settings.data_dir;
    let root = job_dir(data_dir, job_id);
    let mut lock = JobExclusiveLock::new(root.join(LOCK_FILENAME));
    lock.acquire().map_err(|e| ProposeError(e.to_string()))?;

    let repo = JobRepo::open(data_dir, job_id)?;
    repo.reset_stale_proposing()?;
    let job = repo
        .get_job()?
        .ok_or_else(|| ProposeError(format!("job row missing for {job_id}")))?;
    let photos = repo.list_photos()?;
    let current_rows = repo.list_current_proposals()?;
    let mut current_by_photo = HashMap::with_capacity(current_rows.len());
    for row in &current_rows {
        current_by_photo.insert(row.photo_id.clone(), ProposalSummaryMemo::from_row(row));
    }

    let selected = select_photos(&photos, &current_by_photo, opts.force, opts.limit);
    if !exiftool_available()
        && selected.iter().any(|p| {
            matches!(
                classify_media(Path::new(&p.path)),
                MediaKind::Heic | MediaKind::Raw | MediaKind::Png
            )
        })
    {
        return Err(ProposeError(
            "exiftool is required for HEIC, RAW, and PNG; install it and re-run ingest before propose"
                .to_string(),
        ));
    }

    let planned_relpaths: Vec<String> = selected.iter().map(|r| r.relpath.clone()).collect();
    let mut summary = ProposeSummary {
        job_id: job_id.to_string(),
        planned: selected.len(),
        dry_run: opts.dry_run,
        planned_relpaths,
        ..Default::default()
    };

    if opts.dry_run {
        let _cache = GeocodeCache::open(geocode_cache_path(data_dir))?;
        emit(
            opts.progress.as_ref(),
            &format!(
                "dry-run: {} vision calls planned, 0 geocode cache hits (no {provider_key}, no public Nominatim)",
                summary.planned
            ),
        );
        return Ok(summary);
    }

    let cache = GeocodeCache::open(geocode_cache_path(data_dir))?;
    let (resolved_geocoder, not_fake_geocoder) =
        resolve_geocoder(&opts.settings, opts.geocoder.clone())?;
    let cap_uncached = is_public_nominatim_url(&opts.settings.geocoder_url) && not_fake_geocoder;
    let service = Arc::new(GeocodeService::new(
        resolved_geocoder,
        cache,
        &opts.settings.geocode_lang,
        opts.geocode_uncached_max,
        cap_uncached,
    ));

    let (resolved_vision, owned_vision) =
        resolve_vision(&opts.settings, opts.vision.clone(), &provider_key)?;
    if owned_vision && resolved_vision.name() == "gemini" {
        summary.warnings.push(GEMINI_TIER_WARNING.to_string());
        emit(
            opts.progress.as_ref(),
            &format!("warning: {GEMINI_TIER_WARNING}"),
        );
    }

    let log_dir = root.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| ProposeError(e.to_string()))?;
    let opened_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("propose.log"))
        .map_err(|e| ProposeError(e.to_string()))?;
    let log_file = Arc::new(StdMutex::new(Some(opened_log)));

    let hint = job.hint.clone();
    let folder_basename = Path::new(&job.root_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let (_, baseline) = prior_stats(Some(&job));
    let sleep = opts.sleep.unwrap_or_else(default_sleep_fn);

    let shared = Arc::new(Mutex::new(SharedProposeState {
        repo,
        photos,
        current_by_photo,
        summary,
        latencies: Vec::new(),
        refusal_hist: BTreeMap::new(),
        baseline,
    }));

    let sem = Arc::new(Semaphore::new(opts.concurrency));
    let mut handles = Vec::with_capacity(selected.len());
    for photo in selected {
        let permit = Arc::clone(&sem)
            .acquire_owned()
            .await
            .map_err(|e| ProposeError(e.to_string()))?;
        let state = Arc::clone(&shared);
        let settings = opts.settings.clone();
        let vision = Arc::clone(&resolved_vision);
        let svc = Arc::clone(&service);
        let jr = root.clone();
        let h = hint.clone();
        let fb = folder_basename.clone();
        let sl = Arc::clone(&sleep);
        let prog = opts.progress.clone();
        let lf = Arc::clone(&log_file);
        let max_cost = opts.max_cost;
        let no_ctx = opts.no_context;
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            process_one(
                state, photo, settings, vision, svc, jr, h, fb, no_ctx, sl, prog, lf, max_cost,
            )
            .await;
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let mut final_guard = shared.lock().await;
    if service.cap_stopped() {
        final_guard
            .summary
            .warnings
            .push(PUBLIC_NOMINATIM_CAP_WARNING.to_string());
        emit(
            opts.progress.as_ref(),
            &format!("warning: {PUBLIC_NOMINATIM_CAP_WARNING}"),
        );
    }
    final_guard.summary.cache_hits = service.cache_hits();
    final_guard.summary.uncached_lookups = service.uncached_lookups();
    final_guard.checkpoint(&service)?;

    let summary_out = final_guard.summary.clone();
    emit(
        opts.progress.as_ref(),
        &format!(
            "{}/{} proposed, {} refused, {} error, ~${:.2} est (incl. reasoning_tokens)",
            summary_out.proposed,
            summary_out.planned,
            summary_out.refused,
            summary_out.error,
            summary_out.est_usd
        ),
    );
    Ok(summary_out)
}

fn select_unpinned(
    photos: &[PhotoRow],
    current_by_photo: &HashMap<String, ProposalSummaryMemo>,
    limit: Option<usize>,
) -> Vec<(PhotoRow, ProposalSummaryMemo, VisionGuess)> {
    let mut selected = Vec::new();
    for photo in photos {
        if !matches!(photo.status.as_str(), "proposed" | "refused") {
            continue;
        }
        let Some(prop) = current_by_photo.get(&photo.id) else {
            continue;
        };
        if prop.pin_source.as_deref() != Some("none") {
            continue;
        }
        let Ok(guess) = VisionGuess::from_json_str(&prop.raw_json) else {
            continue;
        };
        if should_geocode(&guess).is_some() {
            continue;
        }
        selected.push((photo.clone(), prop.clone(), guess));
    }
    selected.sort_by(|a, b| a.0.relpath.cmp(&b.0.relpath));
    if let Some(lim) = limit {
        selected.truncate(lim);
    }
    selected
}

/// How many current proposals `run_geocode` would try to pin.
pub fn count_unpinned_proposals(repo: &JobRepo) -> Result<usize, StoreError> {
    let photos = repo.list_photos()?;
    let rows = repo.list_current_proposals()?;
    let mut map = HashMap::with_capacity(rows.len());
    for row in &rows {
        map.insert(row.photo_id.clone(), ProposalSummaryMemo::from_row(row));
    }
    Ok(select_unpinned(&photos, &map, None).len())
}

/// Geocode proposals left at `pin_source=none` using stored `VisionGuess` (zero vision cost).
pub async fn run_geocode(
    job_id: &str,
    opts: GeocodeOptions,
) -> Result<GeocodeSummary, ProposeError> {
    if let Some(limit) = opts.limit {
        if limit < 1 {
            return Err(ProposeError("--limit must be >= 1".to_string()));
        }
    }
    require_geocoder_ready(&opts.settings).map_err(|e| ProposeError(e.to_string()))?;

    let data_dir = &opts.settings.data_dir;
    let root = job_dir(data_dir, job_id);
    let mut lock = JobExclusiveLock::new(root.join(LOCK_FILENAME));
    lock.acquire().map_err(|e| ProposeError(e.to_string()))?;

    let repo = JobRepo::open(data_dir, job_id)?;
    let job = repo
        .get_job()?
        .ok_or_else(|| ProposeError(format!("job row missing for {job_id}")))?;
    let mut photos = repo.list_photos()?;
    let rows = repo.list_current_proposals()?;
    let mut current_by_photo = HashMap::with_capacity(rows.len());
    for row in &rows {
        current_by_photo.insert(row.photo_id.clone(), ProposalSummaryMemo::from_row(row));
    }

    let selected = select_unpinned(&photos, &current_by_photo, opts.limit);
    let planned_relpaths: Vec<String> =
        selected.iter().map(|(p, _, _)| p.relpath.clone()).collect();
    let mut summary = GeocodeSummary {
        job_id: job_id.to_string(),
        planned: selected.len(),
        dry_run: opts.dry_run,
        planned_relpaths,
        ..Default::default()
    };

    let cache = GeocodeCache::open(geocode_cache_path(data_dir))?;
    if opts.dry_run {
        let mut keys = BTreeSet::new();
        for (_, _, guess) in &selected {
            let place = guess.primary_place_name.as_deref().unwrap_or("").trim();
            keys.insert(cache_key(
                place,
                guess.country_code.as_deref(),
                &opts.settings.geocode_lang,
            ));
        }
        let mut cold = 0usize;
        for key in &keys {
            if !cache.get(key)?.found {
                cold += 1;
            }
        }
        summary.distinct_places = keys.len();
        summary.cold_places = cold;
        emit(
            opts.progress.as_ref(),
            &format!(
                "dry-run: {} unpinned proposals, {} distinct place names, {} need an uncached lookup (no geocoder HTTP)",
                summary.planned, summary.distinct_places, summary.cold_places
            ),
        );
        return Ok(summary);
    }

    let (resolved_geocoder, not_fake_geocoder) = resolve_geocoder(&opts.settings, opts.geocoder)?;
    let cap_uncached = is_public_nominatim_url(&opts.settings.geocoder_url) && not_fake_geocoder;
    let service = GeocodeService::new(
        resolved_geocoder,
        cache,
        &opts.settings.geocode_lang,
        opts.geocode_uncached_max,
        cap_uncached,
    );

    let log_dir = root.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| ProposeError(e.to_string()))?;
    let opened_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("propose.log"))
        .map_err(|e| ProposeError(e.to_string()))?;
    let log_file = Arc::new(StdMutex::new(Some(opened_log)));

    let hint = job.hint.as_deref();
    for (photo, proposal, guess) in selected {
        let photo_id = photo.id.clone();
        let started = Instant::now();
        let primary = guess.primary_place_name.as_deref().unwrap_or("").trim();
        let lookup = service
            .lookup_primary(primary, guess.country_code.as_deref(), guess.granularity)
            .await;
        let result = result_from_lookup(&guess, lookup);
        let latency_ms = started.elapsed().as_millis() as i64;
        if result.pin_source == PinSource::None {
            summary.still_unpinned += 1;
            continue;
        }

        let (proposed_siblings, gps_siblings) = cluster_context(&photo, &photos, &current_by_photo);
        let rank = ui_rank(&guess, &result, &proposed_siblings, &gps_siblings);
        let refuse = refuse_guess_reason(&guess, hint, rank);
        let prior_status = photo.status.as_str();
        let mut photo_status = if refuse.is_some() {
            PhotoStatus::Refused
        } else {
            PhotoStatus::Proposed
        };
        if photo_status == PhotoStatus::Refused && prior_status == "proposed" {
            // Never demote an already-proposed photo when re-geocoding.
            photo_status = PhotoStatus::Proposed;
        }

        let geocode_json = serialize_geocode_json(&result)?;
        repo.insert_proposal(NewProposal {
            photo_id: photo_id.clone(),
            provider: proposal.provider.clone(),
            model: proposal.model.clone(),
            prompt_version: proposal.prompt_version.clone(),
            raw_json: proposal.raw_json.clone(),
            place_name: guess.primary_place_name.clone(),
            granularity: Some(guess.granularity.as_str().to_string()),
            model_confidence: Some(guess.model_confidence),
            ui_rank: Some(rank),
            refusal: Some(guess.refusal.as_str().to_string()),
            pin_lat: result.pin_lat,
            pin_lng: result.pin_lng,
            pin_source: Some(result.pin_source),
            model_agree: result.model_agree.map(i64::from),
            geocode_json,
            input_tokens: Some(0),
            output_tokens: Some(0),
            reasoning_tokens: Some(0),
            latency_ms: Some(latency_ms),
            current: true,
            proposal_id: None,
            created_at: None,
        })?;
        repo.set_photo_status(&photo_id, photo_status, None, None)?;

        current_by_photo.insert(
            photo_id.clone(),
            ProposalSummaryMemo {
                place_name: guess.primary_place_name.clone(),
                granularity: Some(guess.granularity.as_str().to_string()),
                model_confidence: Some(guess.model_confidence),
                country_code: guess.country_code.clone(),
                pin_source: Some(result.pin_source.as_str().to_string()),
                provider: proposal.provider,
                model: proposal.model,
                prompt_version: proposal.prompt_version,
                raw_json: proposal.raw_json,
            },
        );
        if let Some(p) = photos.iter_mut().find(|r| r.id == photo_id) {
            p.status = photo_status.as_str().to_string();
        }

        summary.pinned += 1;
        if prior_status == "refused" && photo_status == PhotoStatus::Proposed {
            summary.unrefused += 1;
        }
        log_event(
            &log_file,
            json!({
                "level": "info",
                "photo_id": photo_id,
                "event": "geocode_fill",
                "ms": latency_ms,
                "place": guess.primary_place_name,
                "ui_rank": round_to_4(rank),
                "pin_source": result.pin_source.as_str(),
                "status": photo_status.as_str(),
            }),
        );
        emit(
            opts.progress.as_ref(),
            &format!(
                "{}/{} pinned, {} still unpinned, {} un-refused",
                summary.pinned, summary.planned, summary.still_unpinned, summary.unrefused
            ),
        );
    }

    if service.cap_stopped() {
        summary
            .warnings
            .push(PUBLIC_NOMINATIM_CAP_WARNING.to_string());
        emit(
            opts.progress.as_ref(),
            &format!("warning: {PUBLIC_NOMINATIM_CAP_WARNING}"),
        );
    }
    summary.cache_hits = service.cache_hits();
    summary.uncached_lookups = service.uncached_lookups();
    write_geocode_stats(&repo, &summary)?;
    emit(
        opts.progress.as_ref(),
        &format!(
            "{}/{} pinned, {} still unpinned, {} un-refused",
            summary.pinned, summary.planned, summary.still_unpinned, summary.unrefused
        ),
    );
    Ok(summary)
}
