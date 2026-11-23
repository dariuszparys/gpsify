use std::path::{Path, PathBuf};

use gpsify::apply::{
    run_apply, ApplyError, ApplyOptions, DEST_DIVERGED, PNG_XMP_ONLY_WARNING, SOURCE_CHANGED,
    UNSAFE_PATH,
};
use gpsify::exif::{dest_gps_matches, exiftool_available, write_gps_batch_with, GpsWrite, MediaKind};
use gpsify::media::hash_file;
use gpsify::store::{copies_root, options_hash, JobRepo, NewPhoto, NewProposal, UpsertWriteReceipt};
use gpsify::types::{DecisionAction, PhotoStatus, PinSource, WriteReceiptStatus};
use gpsify::undo::run_undo;
use image::{Rgb, RgbImage};
use tempfile::TempDir;

const JOB_ID: &str = "88888888-8888-4888-8888-888888888888";
const PORTO_LAT: f64 = 41.1456;
const PORTO_LNG: f64 = -8.6143;

fn write_test_jpeg(path: &Path) -> PathBuf {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    RgbImage::from_pixel(16, 16, Rgb([30, 80, 140]))
        .save(path)
        .unwrap();
    path.to_path_buf()
}

fn write_test_png(path: &Path) -> PathBuf {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    RgbImage::from_pixel(16, 16, Rgb([40, 120, 80]))
        .save(path)
        .unwrap();
    path.to_path_buf()
}

fn create_repo(data_dir: &Path, root: &Path) -> JobRepo {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    JobRepo::create(
        data_dir,
        JOB_ID,
        &root.to_string_lossy(),
        &canon.to_string_lossy(),
        &options_hash(false),
        Some("weekend in Porto"),
        "{}",
        None,
    )
    .unwrap()
}

fn add_photo(repo: &JobRepo, path: &Path, relpath: Option<&str>, status: PhotoStatus) -> String {
    let mut np = NewPhoto::pending(
        path.to_string_lossy().into_owned(),
        relpath
            .map(str::to_string)
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned()),
        hash_file(path).unwrap(),
    );
    np.size = std::fs::metadata(path).unwrap().len() as i64;
    np.mtime_ns = 1;
    np.status = status;
    np.datetime_original = Some("2019:06:14 18:04:00".to_string());
    repo.insert_photo(np).unwrap()
}

fn add_proposal(repo: &JobRepo, photo_id: &str) {
    repo.insert_proposal(NewProposal {
        photo_id: photo_id.to_string(),
        provider: "xai".to_string(),
        model: "grok-4.6".to_string(),
        prompt_version: "PROMPT_V1".to_string(),
        raw_json: "{}".to_string(),
        place_name: Some("Clérigos Church, Porto".to_string()),
        granularity: Some("exact".to_string()),
        model_confidence: Some(0.8),
        ui_rank: Some(0.83),
        refusal: Some("none".to_string()),
        pin_lat: Some(PORTO_LAT),
        pin_lng: Some(PORTO_LNG),
        pin_source: Some(PinSource::Geocoder),
        current: true,
        ..Default::default()
    })
    .unwrap();
}

fn setup_accepted_jpeg(data_dir: &Path, tmp: &Path) -> (PathBuf, String) {
    let root = tmp.join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let jpeg = write_test_jpeg(&root.join("IMG_a.jpg"));
    let repo = create_repo(data_dir, &root);
    let pid = add_photo(&repo, &jpeg, None, PhotoStatus::Proposed);
    add_proposal(&repo, &pid);
    repo.add_decision(&pid, DecisionAction::Accept, None, None, None, None)
        .unwrap();
    (jpeg, pid)
}

fn write_opts() -> ApplyOptions {
    ApplyOptions {
        allow_writes_override: Some(true),
        ..Default::default()
    }
}

#[test]
fn apply_dry_run_and_in_place_guard() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let (jpeg, _) = setup_accepted_jpeg(&data_dir, tmp.path());
    let ingest_hash = hash_file(&jpeg).unwrap();

    let summary = run_apply::<fn(&str)>(
        JOB_ID,
        &data_dir,
        ApplyOptions {
            dry_run: true,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert!(summary.dry_run);
    assert!(summary.argfile.contains("GPSLatitude"));
    assert!(!copies_root(&data_dir, JOB_ID).join("IMG_a.jpg").exists());
    assert_eq!(hash_file(&jpeg).unwrap(), ingest_hash);

    let err = run_apply::<fn(&str)>(
        JOB_ID,
        &data_dir,
        ApplyOptions {
            in_place: true,
            i_mean_it: false,
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert!(matches!(err, ApplyError(_)));
    assert!(err.0.contains("i-mean-it"));
}

#[test]
fn happy_copy_to_default_copies_and_source_changed_aborts() {
    if !exiftool_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let (jpeg, photo_id) = setup_accepted_jpeg(&data_dir, tmp.path());
    let ingest_hash = hash_file(&jpeg).unwrap();

    let summary = run_apply::<fn(&str)>(JOB_ID, &data_dir, write_opts(), None).unwrap();
    assert_eq!(summary.wrote, 1);
    assert_eq!(summary.failed, 0);

    let dest = copies_root(&data_dir, JOB_ID).join("IMG_a.jpg");
    assert!(dest.is_file());
    assert!(dest_gps_matches(&dest, PORTO_LAT, PORTO_LNG));
    assert_eq!(hash_file(&jpeg).unwrap(), ingest_hash);
    assert_ne!(hash_file(&dest).unwrap(), ingest_hash);

    let repo = JobRepo::open(&data_dir, JOB_ID).unwrap();
    let photo = repo.get_photo(&photo_id).unwrap().unwrap();
    let receipt = repo
        .get_write_receipt(&photo_id, &dest.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(photo.status, PhotoStatus::Written.as_str());
    assert_eq!(photo.sha256, ingest_hash);
    assert_eq!(receipt.status, WriteReceiptStatus::Ok.as_str());
    assert_eq!(receipt.written_sha256, Some(hash_file(&dest).unwrap()));

    // Now test source_changed abort in a fresh job
    let tmp2 = TempDir::new().unwrap();
    let data_dir2 = tmp2.path().join("data");
    let (jpeg2, pid2) = setup_accepted_jpeg(&data_dir2, tmp2.path());
    let mut bytes = std::fs::read(&jpeg2).unwrap();
    bytes.extend_from_slice(b"mutated");
    std::fs::write(&jpeg2, bytes).unwrap();
    let res2 = run_apply::<fn(&str)>(JOB_ID, &data_dir2, write_opts(), None).unwrap();
    assert_eq!(res2.wrote, 0);
    assert_eq!(res2.failed, 1);
    let dest2 = copies_root(&data_dir2, JOB_ID).join("IMG_a.jpg");
    assert!(!dest2.exists());
    let repo2 = JobRepo::open(&data_dir2, JOB_ID).unwrap();
    assert_eq!(
        repo2.get_photo(&pid2).unwrap().unwrap().status,
        PhotoStatus::WriteFailed.as_str()
    );
    assert_eq!(
        repo2
            .get_write_receipt(&pid2, &dest2.to_string_lossy())
            .unwrap()
            .unwrap()
            .error
            .as_deref(),
        Some(SOURCE_CHANGED)
    );
}

#[test]
fn in_place_pending_retry_and_idempotent_reapply() {
    if !exiftool_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let (jpeg, photo_id) = setup_accepted_jpeg(&data_dir, tmp.path());
    let ingest_hash = hash_file(&jpeg).unwrap();

    // Seed an already-tagged file + pending receipt to simulate crash recovery on retry
    write_gps_batch_with(
        &[GpsWrite {
            dest: jpeg.clone(),
            lat: PORTO_LAT,
            lng: PORTO_LNG,
            kind: MediaKind::Jpeg,
            job_id: JOB_ID.to_string(),
            decision_id: "seed".to_string(),
        }],
        true,
        true,
    )
    .unwrap();
    assert_ne!(hash_file(&jpeg).unwrap(), ingest_hash);

    let repo = JobRepo::open(&data_dir, JOB_ID).unwrap();
    let dec = repo.get_current_decision(&photo_id).unwrap().unwrap();
    repo.upsert_write_receipt(UpsertWriteReceipt {
        photo_id: photo_id.clone(),
        decision_id: dec.id,
        dest_path: jpeg.to_string_lossy().into_owned(),
        in_place: true,
        status: WriteReceiptStatus::Pending,
        error: None,
        written_at: None,
        written_sha256: None,
        backup_path: None,
        receipt_id: None,
    })
    .unwrap();

    let summary = run_apply::<fn(&str)>(
        JOB_ID,
        &data_dir,
        ApplyOptions {
            in_place: true,
            i_mean_it: true,
            allow_writes_override: Some(true),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(summary.wrote, 1);
    assert_eq!(
        repo.get_photo(&photo_id).unwrap().unwrap().sha256,
        ingest_hash
    );
    assert_eq!(
        repo.get_photo(&photo_id).unwrap().unwrap().status,
        PhotoStatus::Written.as_str()
    );
}

#[test]
fn png_xmp_warning_and_out_dir_and_dest_diverged() {
    if !exiftool_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let root = tmp.path().join("photos");
    let png = write_test_png(&root.join("shot.png"));
    let repo = create_repo(&data_dir, &root);
    let pid = add_photo(&repo, &png, None, PhotoStatus::Proposed);
    add_proposal(&repo, &pid);
    repo.add_decision(&pid, DecisionAction::Accept, None, None, None, None)
        .unwrap();

    let out = tmp.path().join("exported");
    let summary = run_apply::<fn(&str)>(
        JOB_ID,
        &data_dir,
        ApplyOptions {
            out: Some(out.clone()),
            allow_writes_override: Some(true),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(summary.wrote, 1);
    assert!(summary.warnings.contains(&PNG_XMP_ONLY_WARNING.to_string()));
    assert!(dest_gps_matches(&out.join("shot.png"), PORTO_LAT, PORTO_LNG));

    // Diverged copy check
    let tmp2 = TempDir::new().unwrap();
    let data_dir2 = tmp2.path().join("data");
    let (jpeg2, pid2) = setup_accepted_jpeg(&data_dir2, tmp2.path());
    let dest2 = copies_root(&data_dir2, JOB_ID).join("IMG_a.jpg");
    std::fs::create_dir_all(dest2.parent().unwrap()).unwrap();
    let mut edited = std::fs::read(&jpeg2).unwrap();
    edited.extend_from_slice(b"user-edit");
    std::fs::write(&dest2, edited).unwrap();

    let res2 = run_apply::<fn(&str)>(JOB_ID, &data_dir2, write_opts(), None).unwrap();
    assert_eq!(res2.failed, 1);
    let repo2 = JobRepo::open(&data_dir2, JOB_ID).unwrap();
    assert_eq!(
        repo2
            .get_write_receipt(&pid2, &dest2.to_string_lossy())
            .unwrap()
            .unwrap()
            .error
            .as_deref(),
        Some(DEST_DIVERGED)
    );
}

#[test]
fn raw_copies_and_in_place_sidecar_only_and_undo() {
    if !exiftool_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let raw = root.join("IMG_r.CR2");
    std::fs::write(&raw, b"raw-bytes").unwrap();
    let ingest_hash = hash_file(&raw).unwrap();

    let repo = create_repo(&data_dir, &root);
    let pid = add_photo(&repo, &raw, None, PhotoStatus::Proposed);
    add_proposal(&repo, &pid);
    repo.add_decision(&pid, DecisionAction::Accept, None, None, None, None)
        .unwrap();

    let summary = run_apply::<fn(&str)>(JOB_ID, &data_dir, write_opts(), None).unwrap();
    assert_eq!(summary.wrote, 1);
    let copied = copies_root(&data_dir, JOB_ID).join("IMG_r.CR2");
    let sidecar = copied.with_extension("xmp");
    assert!(copied.is_file());
    assert!(sidecar.is_file());
    assert_eq!(hash_file(&raw).unwrap(), ingest_hash);
    assert_eq!(hash_file(&copied).unwrap(), ingest_hash);
    assert!(dest_gps_matches(&sidecar, PORTO_LAT, PORTO_LNG));

    let receipt = repo
        .get_write_receipt(&pid, &sidecar.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(receipt.dest_path, sidecar.to_string_lossy());

    // Undo copy-mode RAW deletes both copy and sidecar
    let undone = run_undo::<fn(&str)>(JOB_ID, &data_dir, None, None).unwrap();
    assert_eq!(undone.undone, 1);
    assert!(!copied.exists());
    assert!(!sidecar.exists());

    // Now test in-place RAW with pre-existing sidecar
    let existing_xmp = raw.with_extension("xmp");
    std::fs::write(
        &existing_xmp,
        r#"<?xml version="1.0"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/"
      exif:GPSLatitude="1.0"
      exif:GPSLongitude="2.0"/>
  </rdf:RDF>
</x:xmpmeta>
"#,
    )
    .unwrap();
    let in_place_res = run_apply::<fn(&str)>(
        JOB_ID,
        &data_dir,
        ApplyOptions {
            in_place: true,
            i_mean_it: true,
            allow_writes_override: Some(true),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(in_place_res.wrote, 1);
    assert_eq!(hash_file(&raw).unwrap(), ingest_hash);
    assert!(dest_gps_matches(&existing_xmp, PORTO_LAT, PORTO_LNG));

    let undone2 = run_undo::<fn(&str)>(JOB_ID, &data_dir, None, None).unwrap();
    assert_eq!(undone2.undone, 1);
    assert!(dest_gps_matches(&existing_xmp, 1.0, 2.0));
}

#[test]
fn apply_rejects_traversal_symlink_and_photoslibrary() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let jpeg = write_test_jpeg(&root.join("IMG_a.jpg"));
    let escaped = tmp.path().join("escaped.jpg");

    let repo = create_repo(&data_dir, &root);
    let pid = add_photo(&repo, &jpeg, Some("../escaped.jpg"), PhotoStatus::Proposed);
    add_proposal(&repo, &pid);
    repo.add_decision(&pid, DecisionAction::Accept, None, None, None, None)
        .unwrap();

    let out_dir = tmp.path().join("export");
    std::fs::create_dir_all(&out_dir).unwrap();
    let summary = run_apply::<fn(&str)>(
        JOB_ID,
        &data_dir,
        ApplyOptions {
            out: Some(out_dir),
            allow_writes_override: Some(true),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(summary.wrote, 0);
    assert_eq!(summary.failed, 1);
    assert!(!escaped.exists());
    assert_eq!(
        repo.get_photo(&pid).unwrap().unwrap().error.as_deref(),
        Some(UNSAFE_PATH)
    );

    let bundle_out = tmp.path().join("Evil.photoslibrary").join("originals");
    let err = run_apply::<fn(&str)>(
        JOB_ID,
        &data_dir,
        ApplyOptions {
            out: Some(bundle_out),
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert!(err.0.contains("photoslibrary"));
}
