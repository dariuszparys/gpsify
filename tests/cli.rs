use std::path::Path;
use std::process::Command;

use gpsify::config::Settings;
use gpsify::exif::{exiftool_available, read_exif_batch};
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::store::{JobRepo, NewProposal};
use gpsify::types::{Granularity, PhotoStatus, PinSource, RefusalReason, VisionGuess};
use image::{ImageFormat, RgbImage};
use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_gpsify")
}

fn write_test_jpeg(path: &Path) {
    let img = RgbImage::new(64, 48);
    img.save_with_format(path, ImageFormat::Jpeg).unwrap();
}

#[test]
fn cli_help_and_version_and_doctor() {
    let ver = Command::new(bin()).arg("--version").output().unwrap();
    assert!(ver.status.success());
    let stdout = String::from_utf8_lossy(&ver.stdout);
    assert!(stdout.contains("gpsify"));

    let help = Command::new(bin()).arg("--help").output().unwrap();
    assert!(help.status.success());
    let h = String::from_utf8_lossy(&help.stdout);
    for cmd in [
        "doctor", "run", "ingest", "propose", "geocode", "review", "decide", "apply", "undo",
        "skip", "status", "clean",
    ] {
        assert!(h.contains(cmd), "missing subcommand {cmd} in --help");
    }

    let tmp = TempDir::new().unwrap();
    let doc = Command::new(bin())
        .arg("doctor")
        .env("GPSIFY_DATA_DIR", tmp.path())
        .env("NOMINATIM_CONTACT", "doc@example.com")
        .env("XAI_API_KEY", "xai-secret-should-not-leak")
        .env("GEMINI_API_KEY", "gem-secret-should-not-leak")
        .output()
        .unwrap();
    let d_out = String::from_utf8_lossy(&doc.stdout);
    assert!(d_out.contains("exiftool:"));
    assert!(d_out.contains("geocoder: configured"));
    assert!(!d_out.contains("xai-secret-should-not-leak"));
    assert!(!d_out.contains("gem-secret-should-not-leak"));
}

#[test]
fn cli_ingest_propose_geocode_run_dry_run_status_and_clean() {
    let tmp = TempDir::new().unwrap();
    let album = tmp.path().join("porto");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&album).unwrap();
    write_test_jpeg(&album.join("img1.jpg"));

    let base = || {
        let mut c = Command::new(bin());
        c.env("GPSIFY_DATA_DIR", &data_dir)
            .env("NOMINATIM_CONTACT", "cli@example.com")
            .env("XAI_API_KEY", "dummy-key")
            .env("GPSIFY_DRY_RUN", "1");
        c
    };

    // ingest
    let ingest_out = base()
        .args(["ingest", album.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(ingest_out.status.success(), "{:?}", ingest_out);
    let ingest_stdout = String::from_utf8_lossy(&ingest_out.stdout);
    let job_id = ingest_stdout.lines().next().unwrap().trim().to_string();
    assert!(ingest_stdout.contains("untagged: 1"));

    // propose --dry-run via handle `last`
    let prop_dry = base()
        .args(["propose", "last", "--dry-run"])
        .output()
        .unwrap();
    assert!(prop_dry.status.success());
    assert!(String::from_utf8_lossy(&prop_dry.stdout).contains("dry-run: 1 vision calls planned"));

    // geocode --dry-run via folder path handle
    let geo_dry = base()
        .args(["geocode", album.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(geo_dry.status.success());
    assert!(String::from_utf8_lossy(&geo_dry.stdout).contains("dry-run: 0 unpinned proposals"));

    // run --dry-run on a fresh folder
    let album2 = tmp.path().join("lisbon");
    std::fs::create_dir_all(&album2).unwrap();
    write_test_jpeg(&album2.join("img2.jpg"));
    let run_dry = base()
        .args(["run", album2.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(run_dry.status.success());
    assert!(String::from_utf8_lossy(&run_dry.stdout).contains("dry-run: nothing spent"));

    // status --json on `last` and listing
    let st_json = base().args(["status", &job_id, "--json"]).output().unwrap();
    assert!(st_json.status.success());
    let parsed: Value = serde_json::from_slice(&st_json.stdout).unwrap();
    assert_eq!(parsed["job_id"], job_id);

    let st_list = base().args(["status", "--json"]).output().unwrap();
    assert!(st_list.status.success());
    let list_json: Value = serde_json::from_slice(&st_list.stdout).unwrap();
    assert_eq!(list_json["jobs"].as_array().unwrap().len(), 2);

    // apply with 0 accepted -> exit 3
    let app_empty = base().args(["apply", &job_id]).output().unwrap();
    assert_eq!(app_empty.status.code(), Some(3));

    // apply rejects --out + --in-place -> exit 2
    let app_conflict = base()
        .args([
            "apply",
            &job_id,
            "--out",
            tmp.path().to_str().unwrap(),
            "--in-place",
        ])
        .output()
        .unwrap();
    assert_eq!(app_conflict.status.code(), Some(2));

    // apply --in-place without TTY or --i-mean-it -> exit 2
    let app_notty = base()
        .args(["apply", &job_id, "--in-place"])
        .output()
        .unwrap();
    assert_eq!(app_notty.status.code(), Some(2));

    // clean
    let clean = base().args(["clean", &job_id]).output().unwrap();
    assert!(clean.status.success());
    assert!(String::from_utf8_lossy(&clean.stdout).contains("kept sqlite and backups"));
}

#[test]
fn cli_refuses_photos_library_and_tests_decide_skip_apply_undo() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let lib_dir = tmp.path().join("Pictures.photoslibrary");
    std::fs::create_dir_all(&lib_dir).unwrap();

    let lib_out = Command::new(bin())
        .args(["run", lib_dir.to_str().unwrap(), "--dry-run"])
        .env("GPSIFY_DATA_DIR", &data_dir)
        .output()
        .unwrap();
    assert_eq!(lib_out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&lib_out.stderr).contains("photoslibrary"));

    // Setup album with 2 photos, manually insert low-rank + high-rank proposals
    let album = tmp.path().join("trip");
    std::fs::create_dir_all(&album).unwrap();
    write_test_jpeg(&album.join("low.jpg"));
    write_test_jpeg(&album.join("high.jpg"));

    let _ = Settings::default();
    let ingested = run_ingest(&album, IngestOptions::new(&data_dir)).unwrap();
    let job_id = ingested.job_id;
    let repo = JobRepo::open(&data_dir, &job_id).unwrap();
    let photos = repo.list_photos().unwrap();
    let low = photos.iter().find(|p| p.relpath == "low.jpg").unwrap();
    let high = photos.iter().find(|p| p.relpath == "high.jpg").unwrap();

    let guess = VisionGuess {
        refusal: RefusalReason::None,
        refusal_detail: None,
        primary_place_name: Some("Porto".to_string()),
        country_code: Some("PT".to_string()),
        admin1: None,
        granularity: Granularity::City,
        lat_hint: None,
        lng_hint: None,
        model_confidence: 0.7,
        clues: Vec::new(),
        candidates: Vec::new(),
        used_album_hint: false,
        used_filename_hint: false,
    };
    let raw_json = serde_json::to_string(&guess).unwrap();

    repo.insert_proposal(NewProposal {
        photo_id: low.id.clone(),
        provider: "fake".to_string(),
        model: "fake".to_string(),
        prompt_version: "PROMPT_V1".to_string(),
        raw_json: raw_json.clone(),
        place_name: Some("Porto".to_string()),
        granularity: Some("city".to_string()),
        model_confidence: Some(0.4),
        ui_rank: Some(0.35),
        refusal: Some("none".to_string()),
        pin_lat: Some(41.15),
        pin_lng: Some(-8.61),
        pin_source: Some(PinSource::Geocoder),
        current: true,
        ..Default::default()
    })
    .unwrap();
    repo.set_photo_status(&low.id, PhotoStatus::Proposed, None, None)
        .unwrap();

    repo.insert_proposal(NewProposal {
        photo_id: high.id.clone(),
        provider: "fake".to_string(),
        model: "fake".to_string(),
        prompt_version: "PROMPT_V1".to_string(),
        raw_json,
        place_name: Some("Porto".to_string()),
        granularity: Some("city".to_string()),
        model_confidence: Some(0.9),
        ui_rank: Some(0.85),
        refusal: Some("none".to_string()),
        pin_lat: Some(41.1456),
        pin_lng: Some(-8.6139),
        pin_source: Some(PinSource::Geocoder),
        current: true,
        ..Default::default()
    })
    .unwrap();
    repo.set_photo_status(&high.id, PhotoStatus::Proposed, None, None)
        .unwrap();
    drop(repo);

    // skip --all-low --yes
    let skip_out = Command::new(bin())
        .args(["skip", &job_id, "--all-low", "--yes"])
        .env("GPSIFY_DATA_DIR", &data_dir)
        .output()
        .unwrap();
    assert!(skip_out.status.success(), "{:?}", skip_out);
    assert!(String::from_utf8_lossy(&skip_out.stdout).contains("skipped 1 photos"));

    // decide accept on high.jpg
    let dec_out = Command::new(bin())
        .args(["decide", &job_id, "--photo", &high.id, "--action", "accept"])
        .env("GPSIFY_DATA_DIR", &data_dir)
        .output()
        .unwrap();
    assert!(dec_out.status.success());

    // apply --dry-run
    let app_dry = Command::new(bin())
        .args(["apply", &job_id, "--dry-run"])
        .env("GPSIFY_DATA_DIR", &data_dir)
        .output()
        .unwrap();
    assert!(app_dry.status.success());

    // Real in-place apply + undo if exiftool is available (note: do NOT set GPSIFY_DRY_RUN=1)
    if exiftool_available() {
        let app_in = Command::new(bin())
            .args(["apply", &job_id, "--in-place", "--i-mean-it"])
            .env("GPSIFY_DATA_DIR", &data_dir)
            .env_remove("GPSIFY_DRY_RUN")
            .output()
            .unwrap();
        assert!(app_in.status.success(), "{:?}", app_in);

        let batch = read_exif_batch(&[album.join("high.jpg")]);
        let canon = std::fs::canonicalize(album.join("high.jpg")).unwrap();
        let tags = batch
            .by_path
            .get(&canon.to_string_lossy().to_string())
            .unwrap();
        assert!(tags.gps_lat.is_some());

        let undo_out = Command::new(bin())
            .args(["undo", &job_id])
            .env("GPSIFY_DATA_DIR", &data_dir)
            .output()
            .unwrap();
        assert!(undo_out.status.success(), "{:?}", undo_out);

        let batch_after = read_exif_batch(&[album.join("high.jpg")]);
        let tags_after = batch_after
            .by_path
            .get(&canon.to_string_lossy().to_string())
            .unwrap();
        assert_eq!(tags_after.gps_lat, None);
    }
}
