use std::collections::BTreeMap;
use std::path::Path;

use gpsify::exif::is_photos_library;
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::store::{options_hash, JobRepo, NewProposal};
use gpsify::types::{DecisionAction, PhotoStatus, PinSource};
use image::{Rgb, RgbImage};
use tempfile::TempDir;

fn copy_fixture(name: &str, dest: &Path) {
    let src = Path::new("tests/fixtures").join(name);
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::copy(src, dest).unwrap();
}

fn write_plain_jpeg(path: &Path, color: [u8; 3]) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    RgbImage::from_pixel(12, 12, Rgb(color)).save(path).unwrap();
}

#[test]
fn photos_library_detection_and_refusal() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let bundle = tmp.path().join("Travel.photoslibrary");
    let originals = bundle.join("originals");
    std::fs::create_dir_all(&originals).unwrap();
    std::fs::write(originals.join("IMG_1.jpg"), b"x").unwrap();

    assert!(is_photos_library(&bundle));
    assert!(is_photos_library(&originals));
    assert!(!is_photos_library(tmp.path()));

    let err = run_ingest(&bundle, IngestOptions::new(&data_dir)).unwrap_err();
    assert!(err.0.contains("Photos.app"));
}

#[test]
fn ingest_classifies_gps_plain_and_ignores_sidecars() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let trip = tmp.path().join("trip");
    std::fs::create_dir_all(&trip).unwrap();

    copy_fixture("jpeg_with_gps.jpg", &trip.join("tagged.jpg"));
    copy_fixture("jpeg_no_gps.jpg", &trip.join("plain.jpg"));
    std::fs::write(trip.join("notes.txt"), b"ignore me").unwrap();
    std::fs::write(trip.join("clip.mov"), b"ftyp").unwrap();
    std::fs::write(trip.join("._AppleDouble.jpg"), b"x").unwrap();
    std::fs::write(trip.join(".DS_Store"), b"x").unwrap();
    std::fs::write(trip.join("plain.xmp"), b"<x/>").unwrap();

    let summary = run_ingest(&trip, IngestOptions::new(&data_dir)).unwrap();
    assert_eq!(summary.tagged, 1);
    assert_eq!(summary.untagged, 1);
    assert_eq!(summary.skipped, 2);
    assert_eq!(summary.missing, 0);
    assert_eq!(summary.total, 4);

    let repo = JobRepo::open(&data_dir, &summary.job_id).unwrap();
    let by_rel: BTreeMap<String, _> = repo
        .list_photos()
        .unwrap()
        .into_iter()
        .map(|p| (p.relpath.clone(), p))
        .collect();
    assert_eq!(
        by_rel["tagged.jpg"].status,
        PhotoStatus::SkippedHasGps.as_str()
    );
    assert_eq!(by_rel["plain.jpg"].status, PhotoStatus::Pending.as_str());
    assert_eq!(
        by_rel["clip.mov"].status,
        PhotoStatus::SkippedUnsupported.as_str()
    );
    assert_eq!(
        by_rel["notes.txt"].status,
        PhotoStatus::SkippedUnsupported.as_str()
    );
    assert!(!by_rel.contains_key("._AppleDouble.jpg"));
    assert!(!by_rel.contains_key(".DS_Store"));
    assert!(!by_rel.contains_key("plain.xmp"));
}

#[test]
fn non_recursive_warns_about_subdirectories() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let album = tmp.path().join("album");
    copy_fixture("jpeg_no_gps.jpg", &album.join("day1/a.jpg"));

    let flat = run_ingest(&album, IngestOptions::new(&data_dir)).unwrap();
    assert_eq!(flat.total, 0);
    assert!(flat.warnings.iter().any(|w| w.contains("--recursive")));

    let rec = run_ingest(
        &album,
        IngestOptions {
            hint: None,
            recursive: true,
            relocate: None,
            data_dir: &data_dir,
        },
    )
    .unwrap();
    assert_eq!(rec.total, 1);
    assert_ne!(rec.job_id, flat.job_id);
}

#[test]
fn reingest_reports_other_statuses_marks_missing_and_resets_changed_sha256() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let album = tmp.path().join("album");
    std::fs::create_dir_all(&album).unwrap();

    let kept = album.join("kept.jpg");
    let gone = album.join("gone.jpg");
    let changed = album.join("changed.jpg");
    write_plain_jpeg(&kept, [10, 20, 30]);
    write_plain_jpeg(&gone, [40, 50, 60]);
    write_plain_jpeg(&changed, [70, 80, 90]);

    let first = run_ingest(&album, IngestOptions::new(&data_dir)).unwrap();
    assert_eq!(first.untagged, 3);
    assert!(first.other.is_empty());

    let repo = JobRepo::open(&data_dir, &first.job_id).unwrap();
    let photos: BTreeMap<String, _> = repo
        .list_photos()
        .unwrap()
        .into_iter()
        .map(|p| (p.relpath.clone(), p))
        .collect();
    let kept_id = photos["kept.jpg"].id.clone();
    let changed_id = photos["changed.jpg"].id.clone();

    repo.set_photo_status(&kept_id, PhotoStatus::Proposed, None, None)
        .unwrap();
    repo.insert_proposal(NewProposal {
        photo_id: changed_id.clone(),
        provider: "xai".to_string(),
        model: "grok-4.6".to_string(),
        prompt_version: "PROMPT_V1".to_string(),
        raw_json: "{}".to_string(),
        pin_source: Some(PinSource::Geocoder),
        pin_lat: Some(41.0),
        pin_lng: Some(-8.0),
        place_name: Some("Porto".to_string()),
        current: true,
        ..Default::default()
    })
    .unwrap();
    repo.add_decision(&changed_id, DecisionAction::Accept, None, None, None, None)
        .unwrap();

    std::fs::remove_file(&gone).unwrap();
    write_plain_jpeg(&changed, [9, 9, 9]);

    let second = run_ingest(&album, IngestOptions::new(&data_dir)).unwrap();
    assert_eq!(second.job_id, first.job_id);
    assert_eq!(second.missing, 1);
    assert_eq!(second.untagged, 1);
    assert_eq!(second.other, vec![("proposed".to_string(), 1)]);

    let updated: BTreeMap<String, _> = repo
        .list_photos()
        .unwrap()
        .into_iter()
        .map(|p| (p.relpath.clone(), p))
        .collect();
    assert_eq!(updated["gone.jpg"].status, PhotoStatus::Missing.as_str());
    assert_eq!(updated["kept.jpg"].status, PhotoStatus::Proposed.as_str());
    assert_eq!(updated["changed.jpg"].status, PhotoStatus::Pending.as_str());
    assert!(repo.get_current_proposal(&changed_id).unwrap().is_none());
    assert!(repo.get_current_decision(&changed_id).unwrap().is_none());
}

#[test]
fn relocate_matches_by_sha256_and_checks_recursive_flag() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let old_dir = tmp.path().join("old");
    let new_dir = tmp.path().join("new-place");
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::create_dir_all(&new_dir).unwrap();

    copy_fixture("jpeg_no_gps.jpg", &old_dir.join("shot.jpg"));
    let first = run_ingest(
        &old_dir,
        IngestOptions {
            hint: Some("trip"),
            recursive: false,
            relocate: None,
            data_dir: &data_dir,
        },
    )
    .unwrap();

    std::fs::copy(old_dir.join("shot.jpg"), new_dir.join("shot.jpg")).unwrap();

    let bad_flag = run_ingest(
        &new_dir,
        IngestOptions {
            hint: None,
            recursive: true,
            relocate: Some(&first.job_id),
            data_dir: &data_dir,
        },
    )
    .unwrap_err();
    assert!(bad_flag.0.contains("recursive"));

    let relocated = run_ingest(
        &new_dir,
        IngestOptions {
            hint: None,
            recursive: false,
            relocate: Some(&first.job_id),
            data_dir: &data_dir,
        },
    )
    .unwrap();
    assert_eq!(relocated.job_id, first.job_id);
    assert_eq!(relocated.total, 1);

    let repo = JobRepo::open(&data_dir, &first.job_id).unwrap();
    assert_eq!(
        repo.get_job().unwrap().unwrap().options_hash,
        options_hash(false)
    );
}
