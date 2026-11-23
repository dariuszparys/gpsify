//! Vision-model bake-off on photos whose real GPS you already have.
//!
//! Nothing here writes GPS or mutates a photo. It reads EXIF, renders previews
//! in memory, calls each vision provider, and geocodes through the standard pipeline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use serde::Serialize;
use walkdir::WalkDir;

use gpsify::config::{live_http_blocked, load_settings, Settings};
use gpsify::exif::{
    classify_media, exiftool_available, is_photos_library, is_processable, read_exif_batch,
    MediaKind,
};
use gpsify::geocode::{
    geocode_cache_path, is_public_nominatim_url, require_geocoder_ready, GeocodeCache,
    GeocodeService, NominatimGeocoder, DEFAULT_UNCACHED_MAX,
};
use gpsify::media::{hash_file, render_preview};
use gpsify::propose::estimate_usd;
use gpsify::providers::{GeminiProvider, VisionProvider, XaiGrokProvider};
use gpsify::reconcile::{geocode_and_reconcile, haversine_m};
use gpsify::types::{PromptContext, Usage, VisionCallOptions, ALLOWED_VISION_PROVIDERS};

pub const ACC_THRESHOLDS_KM: &[f64] = &[1.0, 25.0, 200.0, 750.0];

#[derive(Debug, Clone, Serialize)]
pub struct Shot {
    pub filename: String,
    pub place_name: Option<String>,
    pub refusal: String,
    pub km: Option<f64>,
    pub usage: Usage,
    pub latency_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelResult {
    pub provider: String,
    pub model: String,
    pub shots: Vec<Shot>,
}

impl ModelResult {
    pub fn label(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }
}

#[derive(Debug, Clone)]
pub struct Truth {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub sha256: String,
    pub lat: f64,
    pub lng: f64,
    pub datetime_original: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelSummary {
    pub provider: String,
    pub model: String,
    pub photos: usize,
    pub errors: usize,
    pub refused: usize,
    pub refusal_rate: f64,
    pub accuracy: HashMap<String, f64>,
    pub median_km: Option<f64>,
    pub mean_usage: Usage,
    pub mean_latency_ms: u64,
    pub usd_total: f64,
    pub usd_per_photo: f64,
}

pub fn accuracy_at(shots: &[Shot], km: f64) -> f64 {
    if shots.is_empty() {
        return 0.0;
    }
    let hits = shots
        .iter()
        .filter(|s| s.km.is_some_and(|d| d <= km))
        .count();
    hits as f64 / shots.len() as f64
}

pub fn median_km(shots: &[Shot]) -> Option<f64> {
    let mut distances: Vec<f64> = shots.iter().filter_map(|s| s.km).collect();
    if distances.is_empty() {
        return None;
    }
    distances.sort_by(f64::total_cmp);
    let n = distances.len();
    if n % 2 == 1 {
        Some(distances[n / 2])
    } else {
        Some((distances[n / 2 - 1] + distances[n / 2]) / 2.0)
    }
}

pub fn mean_usage(shots: &[Shot]) -> Usage {
    let scored: Vec<&Shot> = shots.iter().filter(|s| s.error.is_none()).collect();
    if scored.is_empty() {
        return Usage::default();
    }
    let count = scored.len() as f64;
    let sum_in: u64 = scored.iter().map(|s| s.usage.input_tokens).sum();
    let sum_out: u64 = scored.iter().map(|s| s.usage.output_tokens).sum();
    let sum_reason: u64 = scored.iter().map(|s| s.usage.reasoning_tokens).sum();
    Usage {
        input_tokens: (sum_in as f64 / count).round() as u64,
        output_tokens: (sum_out as f64 / count).round() as u64,
        reasoning_tokens: (sum_reason as f64 / count).round() as u64,
    }
}

pub fn total_usd(result: &ModelResult) -> f64 {
    result
        .shots
        .iter()
        .map(|s| estimate_usd(&s.usage, Some(&result.model), None))
        .sum()
}

pub fn summarize(result: &ModelResult) -> ModelSummary {
    let shots = &result.shots;
    let scored: Vec<&Shot> = shots.iter().filter(|s| s.error.is_none()).collect();
    let refused: Vec<&&Shot> = scored.iter().filter(|s| s.refusal != "none").collect();
    let usd = total_usd(result);
    let mut accuracy = HashMap::new();
    for &km in ACC_THRESHOLDS_KM {
        let val = (accuracy_at(shots, km) * 10_000.0).round() / 10_000.0;
        accuracy.insert(format!("acc@{}km", km as u64), val);
    }
    let mean_latency_ms = if scored.is_empty() {
        0
    } else {
        let sum: u64 = scored.iter().map(|s| s.latency_ms).sum();
        (sum as f64 / scored.len() as f64).round() as u64
    };
    let refusal_rate = if scored.is_empty() {
        0.0
    } else {
        refused.len() as f64 / scored.len() as f64
    };
    let round6 = |v: f64| (v * 1_000_000.0).round() / 1_000_000.0;
    ModelSummary {
        provider: result.provider.clone(),
        model: result.model.clone(),
        photos: shots.len(),
        errors: shots.len() - scored.len(),
        refused: refused.len(),
        refusal_rate,
        accuracy,
        median_km: median_km(shots),
        mean_usage: mean_usage(shots),
        mean_latency_ms,
        usd_total: round6(usd),
        usd_per_photo: if shots.is_empty() {
            0.0
        } else {
            round6(usd / shots.len() as f64)
        },
    }
}

pub fn render_table(summaries: &[ModelSummary]) -> String {
    let acc_headers: Vec<String> = ACC_THRESHOLDS_KM
        .iter()
        .map(|&km| format!("{:>7}", format!("@{}km", km as u64)))
        .collect();
    let header = format!(
        "{:<28} {:>3} {:>4} {} {:>8} {:>6} {:>6} {:>6} {:>9} {:>8}",
        "model",
        "n",
        "ref",
        acc_headers.join(" "),
        "med km",
        "in",
        "out",
        "think",
        "$/photo",
        "$ tot"
    );
    let mut lines = vec![header.clone(), "-".repeat(header.len())];
    for row in summaries {
        let label = format!("{}:{}", row.provider, row.model);
        let label_trimmed: String = label.chars().take(28).collect();
        let acc_cols: Vec<String> = ACC_THRESHOLDS_KM
            .iter()
            .map(|&km| {
                let key = format!("acc@{}km", km as u64);
                let pct = row.accuracy.get(&key).copied().unwrap_or(0.0) * 100.0;
                format!("{pct:>6.1}%")
            })
            .collect();
        let med_str = match row.median_km {
            Some(m) => format!("{m:.1}"),
            None => "-".to_string(),
        };
        lines.push(format!(
            "{:<28} {:>3} {:>4} {} {:>8} {:>6} {:>6} {:>6} {:>9.5} {:>8.4}",
            label_trimmed,
            row.photos,
            row.refused,
            acc_cols.join(" "),
            med_str,
            row.mean_usage.input_tokens,
            row.mean_usage.output_tokens,
            row.mean_usage.reasoning_tokens,
            row.usd_per_photo,
            row.usd_total
        ));
    }
    lines.join("\n")
}

pub fn parse_model_spec(spec: &str) -> Result<(String, String), String> {
    let Some((raw_provider, raw_model)) = spec.split_once(':') else {
        let mut allowed: Vec<&str> = ALLOWED_VISION_PROVIDERS.to_vec();
        allowed.sort();
        return Err(format!(
            "--model must be '<{}>:<model-id>'; got {spec:?}",
            allowed.join("|")
        ));
    };
    let provider = raw_provider.trim().to_ascii_lowercase();
    let model = raw_model.trim().to_string();
    if !ALLOWED_VISION_PROVIDERS.contains(&provider.as_str()) || model.is_empty() {
        let mut allowed: Vec<&str> = ALLOWED_VISION_PROVIDERS.to_vec();
        allowed.sort();
        return Err(format!(
            "--model must be '<{}>:<model-id>'; got {spec:?}",
            allowed.join("|")
        ));
    }
    Ok((provider, model))
}

fn collect_truth(root: &Path, recursive: bool, limit: Option<usize>) -> Result<Vec<Truth>, String> {
    if is_photos_library(root) {
        return Err("refusing to read a Photos.app library bundle".to_string());
    }
    let mut walker = WalkDir::new(root).follow_links(false);
    if !recursive {
        walker = walker.max_depth(1);
    }
    let mut paths: Vec<PathBuf> = walker
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| is_processable(classify_media(p)))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no supported photos under {}", root.display()));
    }

    let batch = read_exif_batch(&paths);
    for warning in &batch.warnings {
        eprintln!("warning: {warning}");
    }

    let mut truths = Vec::new();
    for path in paths {
        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let key = canon.to_string_lossy().to_string();
        let Some(tags) = batch.by_path.get(&key) else {
            continue;
        };
        let (Some(lat), Some(lng)) = (tags.gps_lat, tags.gps_lng) else {
            continue;
        };
        let sha =
            hash_file(&path).map_err(|e| format!("failed to hash {}: {e}", path.display()))?;
        truths.push(Truth {
            kind: classify_media(&path),
            sha256: sha,
            lat,
            lng,
            datetime_original: tags.datetime_original.clone(),
            make: tags.make.clone(),
            model: tags.model.clone(),
            path,
        });
    }
    if truths.is_empty() {
        return Err(format!(
            "no GPS-tagged photos under {}. The bake-off scores guesses against coordinates the photos already carry.",
            root.display()
        ));
    }
    if let Some(lim) = limit {
        truths.truncate(lim);
    }
    Ok(truths)
}

fn build_provider(
    provider: &str,
    model: &str,
    settings: &Settings,
) -> Result<Arc<dyn VisionProvider>, String> {
    match provider {
        "gemini" => {
            let key = settings
                .gemini_api_key
                .as_deref()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| format!("GEMINI_API_KEY is required for {provider}:{model}"))?;
            Ok(Arc::new(
                GeminiProvider::new(key, model).map_err(|e| e.to_string())?,
            ))
        }
        _ => {
            let key = settings
                .xai_api_key
                .as_deref()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| format!("XAI_API_KEY is required for {provider}:{model}"))?;
            Ok(Arc::new(
                XaiGrokProvider::new(key, model).map_err(|e| e.to_string())?,
            ))
        }
    }
}

fn context_for(truth: &Truth, hint: Option<&str>) -> PromptContext {
    let folder_basename = truth
        .path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let filename = truth
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    PromptContext {
        hint: hint.map(str::to_string),
        folder_basename,
        filename,
        datetime_original: truth.datetime_original.clone(),
        offset: None,
        make: truth.make.clone(),
        model: truth.model.clone(),
        proposed_siblings: Vec::new(),
        gps_siblings: Vec::new(),
        no_context: false,
    }
}

async fn score_one(
    provider: &dyn VisionProvider,
    truth: &Truth,
    preview: &[u8],
    options: &VisionCallOptions,
    service: &GeocodeService,
    hint: Option<&str>,
) -> Shot {
    let filename = truth
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let started = Instant::now();
    let ctx = context_for(truth, hint);
    let (guess, usage) = match provider.guess(preview, &ctx, options).await {
        Ok(res) => res,
        Err(err) => {
            return Shot {
                filename,
                place_name: None,
                refusal: "error".to_string(),
                km: None,
                usage: Usage::default(),
                latency_ms: started.elapsed().as_millis() as u64,
                error: Some(err.to_string()),
            };
        }
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    let reconciled = geocode_and_reconcile(&guess, service).await;
    let km = match (reconciled.pin_lat, reconciled.pin_lng) {
        (Some(plat), Some(plng)) => Some(haversine_m(plat, plng, truth.lat, truth.lng) / 1000.0),
        _ => None,
    };
    Shot {
        filename,
        place_name: guess.primary_place_name,
        refusal: guess.refusal.as_str().to_string(),
        km,
        usage,
        latency_ms,
        error: None,
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "bakeoff",
    about = "Score vision models against photos whose GPS you already have. Costs real money: one paid vision call per photo per model.",
    after_help = "Scoring an album with no --limit requires --yes. GPSIFY_DRY_RUN=1 prints the plan and spends nothing."
)]
struct BakeoffArgs {
    /// Folder of already-geotagged photos.
    album: PathBuf,
    /// Repeatable, e.g. --model gemini:gemini-2.5-flash
    #[arg(long = "model", value_name = "PROVIDER:MODEL", required = true)]
    model: Vec<String>,
    /// Max photos to score. Unset scores the album.
    #[arg(long)]
    limit: Option<usize>,
    /// Required to score an album with no --limit, since that spends without a bound.
    #[arg(long)]
    yes: bool,
    /// Walk subfolders.
    #[arg(long)]
    recursive: bool,
    /// Album hint passed to every model.
    #[arg(long)]
    hint: Option<String>,
    /// Public Nominatim uncached lookup cap for this run.
    #[arg(long, default_value_t = DEFAULT_UNCACHED_MAX)]
    geocode_uncached_max: usize,
    /// Write full results as JSON here.
    #[arg(long)]
    out: Option<PathBuf>,
}

async fn run_bakeoff(args: BakeoffArgs) -> u8 {
    let settings = match load_settings() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    if let Err(e) = require_geocoder_ready(&settings) {
        eprintln!("error: {e}");
        return 2;
    }

    let mut specs = Vec::with_capacity(args.model.len());
    for raw in &args.model {
        match parse_model_spec(raw) {
            Ok(s) => specs.push(s),
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        }
    }

    let expanded = gpsify::config::expand_user_path(&args.album);
    let mut truths = match collect_truth(&expanded, args.recursive, args.limit) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };

    let plan = format!(
        "{} GPS-tagged photos x {} models = {} vision calls",
        truths.len(),
        specs.len(),
        truths.len() * specs.len()
    );
    if live_http_blocked() {
        println!("dry-run: {plan} (GPSIFY_DRY_RUN=1 blocks every call)");
        for (provider, model) in &specs {
            println!("  would call {provider}:{model}");
        }
        return 0;
    }
    if args.limit.is_none() && !args.yes {
        eprintln!(
            "refusing to spend on {plan} with no bound. Pass --limit N to cap it, --yes to score the whole album, or GPSIFY_DRY_RUN=1 to see the plan."
        );
        return 2;
    }
    if !exiftool_available()
        && truths
            .iter()
            .any(|t| matches!(t.kind, MediaKind::Heic | MediaKind::Raw | MediaKind::Png))
    {
        eprintln!("error: exiftool is required for HEIC/RAW/PNG previews");
        return 2;
    }
    println!("{plan}");

    let options = match VisionCallOptions::new(&settings.reasoning_effort, settings.timeout_s) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };

    let mut previews: HashMap<String, Vec<u8>> = HashMap::new();
    for truth in &truths {
        let name = truth
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        match render_preview(&truth.path, Some(truth.kind), &truth.sha256) {
            Ok(bytes) => {
                previews.insert(name, bytes);
            }
            Err(err) => {
                eprintln!("warning: skipping {name}: {err}");
            }
        }
    }
    truths.retain(|t| {
        t.path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| previews.contains_key(n))
    });

    let cache = match GeocodeCache::open(geocode_cache_path(&settings.data_dir)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let nominatim = match NominatimGeocoder::new(
        &settings.geocoder_url,
        settings.nominatim_contact.as_deref(),
        &settings.geocoder,
    ) {
        Ok(n) => Arc::new(n),
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let cap_uncached = is_public_nominatim_url(&settings.geocoder_url);
    let service = GeocodeService::new(
        nominatim,
        cache,
        &settings.geocode_lang,
        args.geocode_uncached_max,
        cap_uncached,
    );

    let mut results = Vec::with_capacity(specs.len());
    for (provider_key, model) in &specs {
        let client = match build_provider(provider_key, model, &settings) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        let mut result = ModelResult {
            provider: provider_key.clone(),
            model: model.clone(),
            shots: Vec::with_capacity(truths.len()),
        };
        let label = result.label();
        for (idx, truth) in truths.iter().enumerate() {
            let name = truth
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let preview = &previews[name];
            let shot = score_one(
                client.as_ref(),
                truth,
                preview,
                &options,
                &service,
                args.hint.as_deref(),
            )
            .await;
            let pin = match shot.km {
                Some(km) => format!("{km:.1} km"),
                None => shot.refusal.clone(),
            };
            println!(
                "  [{label}] {}/{} {}: {pin}",
                idx + 1,
                truths.len(),
                shot.filename
            );
            result.shots.push(shot);
        }
        results.push(result);
    }

    let summaries: Vec<ModelSummary> = results.iter().map(summarize).collect();
    println!();
    println!("{}", render_table(&summaries));
    if service.cap_stopped() {
        println!("\nwarning: hit the uncached-geocode cap; later photos scored without a pin");
    }
    if let Some(out_path) = &args.out {
        let mut shots_map = serde_json::Map::new();
        for r in &results {
            shots_map.insert(
                r.label(),
                serde_json::to_value(&r.shots).unwrap_or_default(),
            );
        }
        let payload = serde_json::json!({
            "album": expanded.to_string_lossy(),
            "photos": truths.len(),
            "reasoning_effort": settings.reasoning_effort,
            "results": summaries,
            "shots": shots_map,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => {
                if let Err(e) = std::fs::write(out_path, text) {
                    eprintln!("error: failed writing {}: {e}", out_path.display());
                    return 2;
                }
                println!("\nwrote {}", out_path.display());
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        }
    }
    0
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = BakeoffArgs::parse();
    ExitCode::from(run_bakeoff(args).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(
        filename: &str,
        km: Option<f64>,
        refusal: &str,
        err: Option<&str>,
        in_tok: u64,
        out_tok: u64,
        think_tok: u64,
    ) -> Shot {
        Shot {
            filename: filename.to_string(),
            place_name: km.map(|_| "Porto".to_string()),
            refusal: refusal.to_string(),
            km,
            usage: Usage {
                input_tokens: in_tok,
                output_tokens: out_tok,
                reasoning_tokens: think_tok,
            },
            latency_ms: 150,
            error: err.map(str::to_string),
        }
    }

    #[test]
    fn parse_model_spec_validates_provider_and_model() {
        assert_eq!(
            parse_model_spec("gemini:gemini-2.5-flash").unwrap(),
            ("gemini".to_string(), "gemini-2.5-flash".to_string())
        );
        assert_eq!(
            parse_model_spec(" XAI : grok-4.6 ").unwrap(),
            ("xai".to_string(), "grok-4.6".to_string())
        );
        assert!(parse_model_spec("openai:gpt-4").is_err());
        assert!(parse_model_spec("xai:").is_err());
        assert!(parse_model_spec("nocolon").is_err());
    }

    #[test]
    fn metrics_and_table_match_expectations() {
        let result = ModelResult {
            provider: "xai".to_string(),
            model: "grok-4.6".to_string(),
            shots: vec![
                shot("a.jpg", Some(0.4), "none", None, 1000, 200, 100),
                shot("b.jpg", Some(12.0), "none", None, 1200, 300, 200),
                shot("c.jpg", None, "indoor_generic", None, 800, 100, 0),
                shot("d.jpg", None, "error", Some("timeout"), 0, 0, 0),
            ],
        };
        assert!((accuracy_at(&result.shots, 1.0) - 0.25).abs() < 1e-9);
        assert!((accuracy_at(&result.shots, 25.0) - 0.5).abs() < 1e-9);
        assert!((median_km(&result.shots).unwrap() - 6.2).abs() < 1e-9);

        let mean = mean_usage(&result.shots);
        assert_eq!(
            mean,
            Usage {
                input_tokens: 1000,
                output_tokens: 200,
                reasoning_tokens: 100,
            }
        );

        let summary = summarize(&result);
        assert_eq!(summary.photos, 4);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.refused, 1);
        assert!(summary.usd_total > 0.0);

        let table = render_table(&[summary]);
        assert!(table.contains("xai:grok-4.6"));
        assert!(table.contains("25.0%"));
        assert!(table.contains("50.0%"));
    }
}
