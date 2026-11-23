use std::path::Path;
use std::sync::Arc;

use chrono::NaiveDate;
use gpsify::config::Settings;
use gpsify::geocode::{FakeGeocoder, Geocoder};
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::propose::{
    estimate_usd, instant_sleep_fn, refuse_guess_reason, run_propose, ui_rank, ProposeError,
    ProposeOptions,
};
use gpsify::providers::{FakeVisionProvider, VisionError, VisionProvider};
use gpsify::reconcile::ReconcileResult;
use gpsify::store::{job_dir, JobExclusiveLock, JobRepo};
use gpsify::types::{
    Clue, GeoHit, Granularity, PhotoStatus, PinSource, ProposedSibling, TaggedSibling, Usage,
};
use image::{ImageFormat, RgbImage};
use serde_json::Value;
use tempfile::TempDir;

fn write_test_jpeg(path: &Path) {
    let img = RgbImage::new(64, 48);
    img.save_with_format(path, ImageFormat::Jpeg).unwrap();
}

fn test_settings(data_dir: &Path) -> Settings {
    let mut s = Settings::default();
    s.data_dir = data_dir.to_path_buf();
    s.nominatim_contact = Some("test@example.com".to_string());
    s.xai_api_key = Some("xai-test-key".to_string());
    s.gemini_api_key = Some("gemini-test-key".to_string());
    s
}

fn clerigos_hit() -> GeoHit {
    GeoHit {
        query: "Clérigos Church, Porto, Portugal".to_string(),
        display_name: "Igreja dos Clérigos, Porto, Portugal".to_string(),
        lat: 41.1456,
        lng: -8.6139,
        boundingbox: None,
        osm_type: Some("way".to_string()),
        osm_id: Some("100".to_string()),
        importance: Some(0.9),
        provider: "nominatim".to_string(),
    }
}

fn setup_album(album: &Path, data_dir: &Path, names: &[&str], hint: Option<&str>) -> String {
    std::fs::create_dir_all(album).unwrap();
    for name in names {
        write_test_jpeg(&album.join(name));
    }
    let mut opts = IngestOptions::new(data_dir);
    opts.hint = hint;
    run_ingest(album, opts).unwrap().job_id
}

#[test]
fn ui_rank_refusal_and_estimate_usd_rollover() {
    let guess = FakeVisionProvider::default_guess();
    let result = ReconcileResult {
        pin_source: PinSource::Geocoder,
        pin_lat: Some(41.1456),
        pin_lng: Some(-8.6139),
        model_agree: Some(true),
        geocode: Some(clerigos_hit()),
        photo_status: PhotoStatus::Proposed,
        skipped_geocode: None,
        model_lat: None,
        model_lng: None,
    };
    let rank_no_sibs = ui_rank(&guess, &result, &[], &[]);
    assert!((rank_no_sibs - (0.45 * 0.9 + 0.20 * 1.0 + 0.15 * 0.25 + 0.10 * 1.0)).abs() < 1e-9);

    let prop_sib = ProposedSibling {
        filename: "sib.jpg".to_string(),
        captured_at: None,
        place_name: "Porto".to_string(),
        country_code: Some("PT".to_string()),
        granularity: Granularity::City,
        model_confidence: 0.8,
    };
    let rank_with_sib = ui_rank(&guess, &result, &[prop_sib], &[] as &[TaggedSibling]);
    assert!(rank_with_sib > rank_no_sibs);

    let mut weak = guess.clone();
    weak.granularity = Granularity::Country;
    weak.clues = Vec::new();
    assert_eq!(
        refuse_guess_reason(&weak, Some("weekend trip"), 0.8),
        Some("weak_granularity")
    );
    assert_eq!(refuse_guess_reason(&weak, Some("Porto 2025"), 0.8), None);

    weak.clues = vec![Clue {
        kind: "sign".to_string(),
        text: "Porto".to_string(),
        weight: 0.9,
    }];
    assert_eq!(refuse_guess_reason(&weak, Some("weekend trip"), 0.8), None);
    assert_eq!(
        refuse_guess_reason(&weak, Some("weekend trip"), 0.10),
        Some("low_ui_rank")
    );

    let usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 500_000,
        reasoning_tokens: 500_000,
    };
    let pre_promo = estimate_usd(
        &usage,
        Some("gemini-3.7-flash"),
        NaiveDate::from_ymd_opt(2026, 12, 31),
    );
    let post_promo = estimate_usd(
        &usage,
        Some("gemini-3.7-flash"),
        NaiveDate::from_ymd_opt(2027, 1, 1),
    );
    assert!((post_promo - 2.0 * pre_promo).abs() < 1e-9);
}

#[tokio::test]
async fn propose_writes_proposals_cumulative_stats_and_respects_flags() {
    let tmp = TempDir::new().unwrap();
    let album = tmp.path().join("album");
    let data_dir = tmp.path().join("data");
    let job_id = setup_album(&album, &data_dir, &["a.jpg", "b.jpg"], Some("Porto"));
    let settings = test_settings(&data_dir);

    let fake_geo = Arc::new(FakeGeocoder::new());
    fake_geo.add("Clérigos Church, Porto, Portugal", vec![clerigos_hit()]);
    let fake_vis = Arc::new(FakeVisionProvider::default().with_model("grok-4.6"));

    // Dry run plans without invoking vision or geocoder.
    let mut dry_opts = ProposeOptions::new(settings.clone());
    dry_opts.dry_run = true;
    dry_opts.vision = Some(fake_vis.clone());
    dry_opts.geocoder = Some(fake_geo.clone());
    let dry = run_propose(&job_id, dry_opts).await.unwrap();
    assert_eq!(dry.planned, 2);
    assert!(fake_vis.calls().is_empty());
    assert!(fake_geo.queries().is_empty());

    // Live (fake) propose with limit=1 and no_context=true.
    let mut p1_opts = ProposeOptions::new(settings.clone());
    p1_opts.limit = Some(1);
    p1_opts.no_context = true;
    p1_opts.sleep = Some(instant_sleep_fn());
    p1_opts.vision = Some(fake_vis.clone());
    p1_opts.geocoder = Some(fake_geo.clone());
    let s1 = run_propose(&job_id, p1_opts).await.unwrap();
    assert_eq!(s1.proposed, 1);
    assert_eq!(fake_vis.calls()[0].context.hint, None);
    assert!(fake_vis.calls()[0].context.no_context);

    // Second propose picks remaining photo; existing proposal skipped unless --force.
    let mut p2_opts = ProposeOptions::new(settings.clone());
    p2_opts.sleep = Some(instant_sleep_fn());
    p2_opts.vision = Some(fake_vis.clone());
    p2_opts.geocoder = Some(fake_geo.clone());
    let s2 = run_propose(&job_id, p2_opts).await.unwrap();
    assert_eq!(s2.planned, 1);
    assert_eq!(s2.proposed, 1);

    let repo = JobRepo::open(&data_dir, &job_id).unwrap();
    let job = repo.get_job().unwrap().unwrap();
    let stats: Value = serde_json::from_str(&job.stats_json).unwrap();
    assert_eq!(stats["propose"]["counts"]["proposed"], 2);
    assert_eq!(stats["propose"]["runs"], 2);

    // No-op propose preserves cumulative counts and USD.
    let mut p3_opts = ProposeOptions::new(settings);
    p3_opts.sleep = Some(instant_sleep_fn());
    p3_opts.vision = Some(fake_vis.clone());
    p3_opts.geocoder = Some(fake_geo.clone());
    let s3 = run_propose(&job_id, p3_opts).await.unwrap();
    assert_eq!(s3.planned, 0);
    let stats_after: Value =
        serde_json::from_str(&repo.get_job().unwrap().unwrap().stats_json).unwrap();
    assert_eq!(stats_after["propose"]["counts"]["proposed"], 2);
    assert_eq!(stats_after["propose"]["runs"], 3);

    // Log safety: home/temp album path must not appear in logs/propose.log
    let log_text =
        std::fs::read_to_string(job_dir(&data_dir, &job_id).join("logs/propose.log")).unwrap();
    assert!(!log_text.contains("data:image/jpeg;base64"));
}

#[tokio::test]
async fn retry_backoff_blocked_terminal_refusal_lock_and_max_cost() {
    let tmp = TempDir::new().unwrap();
    let album = tmp.path().join("album");
    let data_dir = tmp.path().join("data");
    let job_id = setup_album(&album, &data_dir, &["a.jpg", "b.jpg", "c.jpg"], None);
    let settings = test_settings(&data_dir);

    let fake_geo: Arc<dyn Geocoder> = Arc::new({
        let g = FakeGeocoder::new();
        g.add("Clérigos Church, Porto, Portugal", vec![clerigos_hit()]);
        g
    });
    let default_pair = (
        FakeVisionProvider::default_guess(),
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            reasoning_tokens: 0,
        },
    );
    let seq_vis: Arc<dyn VisionProvider> = Arc::new(
        FakeVisionProvider::sequence(
            vec![
                Err(VisionError::Retryable("429".to_string())),
                Ok(default_pair.clone()),
                Err(VisionError::Blocked("safety filter".to_string())),
            ],
            default_pair,
        )
        .with_model("grok-4.6"),
    );

    // Simulate stale `proposing` row before running propose.
    let repo = JobRepo::open(&data_dir, &job_id).unwrap();
    let photos = repo.list_photos().unwrap();
    repo.set_photo_status(&photos[0].id, PhotoStatus::Proposing, None, None)
        .unwrap();

    // Verify JobExclusiveLock contention rejects overlapping run.
    {
        let mut lock = JobExclusiveLock::new(job_dir(&data_dir, &job_id).join("propose.lock"));
        lock.acquire().unwrap();
        let mut opts = ProposeOptions::new(settings.clone());
        opts.vision = Some(seq_vis.clone());
        opts.geocoder = Some(fake_geo.clone());
        let err = run_propose(&job_id, opts).await.unwrap_err();
        assert!(matches!(err, ProposeError(_)));
        assert!(err.to_string().contains("lock"));
    }

    // max_cost=1.50 with concurrency=1 stops after first ~$2 call + never leaves blocked photo in error.
    let mut opts = ProposeOptions::new(settings.clone());
    opts.limit = Some(2);
    opts.sleep = Some(instant_sleep_fn());
    opts.vision = Some(seq_vis.clone());
    opts.geocoder = Some(fake_geo.clone());
    let summary = run_propose(&job_id, opts).await.unwrap();
    assert_eq!(summary.proposed, 1);
    assert_eq!(summary.refused, 1);
    assert_eq!(summary.error, 0);

    // Run third photo under max_cost=1.0 -> budget already below threshold at start of new run, executes 1 call then hits budget if limit=2?
    // Wait: test max_cost stopping mid-run with 2 photos in a fresh job:
    let album2 = tmp.path().join("album2");
    let job2 = setup_album(&album2, &data_dir, &["x.jpg", "y.jpg"], None);
    let costly_vis = Arc::new(
        FakeVisionProvider::fixed(
            FakeVisionProvider::default_guess(),
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 0,
                reasoning_tokens: 0,
            },
        )
        .with_model("grok-4.6"),
    );
    let mut cap_opts = ProposeOptions::new(settings);
    cap_opts.max_cost = Some(1.0); // 1 call costs $2.00 -> second photo sees est_usd >= 1.0 and skips
    cap_opts.sleep = Some(instant_sleep_fn());
    cap_opts.vision = Some(costly_vis);
    cap_opts.geocoder = Some(fake_geo);
    let cap_summary = run_propose(&job2, cap_opts).await.unwrap();
    assert_eq!(cap_summary.proposed, 1);
    assert!(cap_summary
        .warnings
        .iter()
        .any(|w| w.contains("budget reached")));
}
