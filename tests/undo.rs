use gpsify::apply::{run_apply, ApplyOptions};
use gpsify::exif::exiftool_available;
use gpsify::media::hash_file;
use gpsify::store::{copies_root, options_hash, JobRepo, NewPhoto, NewProposal, PhotoRow, WriteReceiptRow};
use gpsify::types::{DecisionAction, PhotoStatus, PinSource, WriteReceiptStatus};
use gpsify::undo::{run_undo, undo_dest};
use image::{Rgb, RgbImage};
use tempfile::TempDir;

const JOB_ID: &str = "77777777-7777-4777-8777-777777777777";

fn dummy_photo(relpath: &str, path: &str, sha256: &str) -> PhotoRow {
    PhotoRow {
        id: "p1".to_string(),
        job_id: JOB_ID.to_string(),
        path: path.to_string(),
        relpath: relpath.to_string(),
        sha256: sha256.to_string(),
        size: 10,
        mtime_ns: 1,
        status: "written".to_string(),
        error: None,
        datetime_original: None,
        make: None,
        model: None,
        has_gps: 0,
        existing_lat: None,
        existing_lng: None,
        cluster_id: None,
        updated_at: "2026-08-12T00:00:00Z".to_string(),
    }
}

fn dummy_receipt(
    dest_path: &str,
    in_place: bool,
    backup_path: Option<&str>,
    written_sha256: Option<&str>,
) -> WriteReceiptRow {
    WriteReceiptRow {
        id: "r1".to_string(),
        photo_id: "p1".to_string(),
        decision_id: "d1".to_string(),
        dest_path: dest_path.to_string(),
        in_place: i64::from(in_place),
        status: "ok".to_string(),
        error: None,
        written_at: Some("2026-08-12T00:00:00Z".to_string()),
        written_sha256: written_sha256.map(str::to_string),
        backup_path: backup_path.map(str::to_string),
    }
}

#[test]
fn undo_preserves_later_edits() {
    for in_place in [true, false] {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("photo.jpg");
        let backup = tmp.path().join("backup.bin");
        std::fs::write(&backup, b"original").unwrap();
        std::fs::write(&dest, b"gpsify output").unwrap();
        let written_sha = hash_file(&dest).unwrap();
        std::fs::write(&dest, b"later edits").unwrap();

        let photo = dummy_photo("photo.jpg", &dest.to_string_lossy(), "0");
        let receipt = dummy_receipt(
            &dest.to_string_lossy(),
            in_place,
            Some(&backup.to_string_lossy()),
            Some(&written_sha),
        );
        assert_eq!(undo_dest(&photo, &receipt).unwrap(), Some("dest_diverged"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"later edits");
    }
}

#[test]
fn undo_raw_preserves_modified_copy_and_sidecar() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("photo.cr2");
    let sidecar = tmp.path().join("photo.xmp");
    std::fs::write(&raw, b"original raw").unwrap();
    let original_hash = hash_file(&raw).unwrap();
    std::fs::write(&sidecar, b"gpsify sidecar").unwrap();
    let receipt = dummy_receipt(
        &sidecar.to_string_lossy(),
        false,
        None,
        Some(&hash_file(&sidecar).unwrap()),
    );
    std::fs::write(&raw, b"later raw edits").unwrap();

    let photo = dummy_photo("photo.cr2", "/source/photo.cr2", &original_hash);
    assert_eq!(
        undo_dest(&photo, &receipt).unwrap(),
        Some("source_copy_diverged")
    );
    assert_eq!(std::fs::read(&raw).unwrap(), b"later raw edits");
    assert_eq!(std::fs::read(&sidecar).unwrap(), b"gpsify sidecar");
}

#[test]
fn undo_missing_raw_backup_and_symlink_safety() {
    let tmp = TempDir::new().unwrap();
    let sidecar = tmp.path().join("photo.xmp");
    std::fs::write(&sidecar, b"gpsify sidecar").unwrap();
    let receipt = dummy_receipt(
        &sidecar.to_string_lossy(),
        true,
        Some(&tmp.path().join("missing.xmp").to_string_lossy()),
        Some(&hash_file(&sidecar).unwrap()),
    );
    let photo = dummy_photo("photo.cr2", "/source/photo.cr2", "0");
    assert_eq!(undo_dest(&photo, &receipt).unwrap(), Some("backup_missing"));
    assert!(sidecar.is_file());

    #[cfg(unix)]
    {
        let outside = tmp.path().join("outside.jpg");
        std::fs::write(&outside, b"outside").unwrap();
        let link = tmp.path().join("link.jpg");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let backup = tmp.path().join("backup.bin");
        std::fs::write(&backup, b"restored").unwrap();
        let link_receipt = dummy_receipt(
            &link.to_string_lossy(),
            true,
            Some(&backup.to_string_lossy()),
            Some(&hash_file(&outside).unwrap()),
        );
        let link_photo = dummy_photo("link.jpg", &link.to_string_lossy(), "0");
        assert_eq!(
            undo_dest(&link_photo, &link_receipt).unwrap(),
            Some("unsafe_path")
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }
}

#[test]
fn undo_sets_undone_then_reapply_upserts() {
    if !exiftool_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let jpeg = root.join("IMG_a.jpg");
    RgbImage::from_pixel(16, 16, Rgb([30, 80, 140]))
        .save(&jpeg)
        .unwrap();

    let repo = JobRepo::create(
        &data_dir,
        JOB_ID,
        &root.to_string_lossy(),
        &std::fs::canonicalize(&root).unwrap().to_string_lossy(),
        &options_hash(false),
        None,
        "{}",
        None,
    )
    .unwrap();
    let mut np = NewPhoto::pending(
        jpeg.to_string_lossy().into_owned(),
        "IMG_a.jpg",
        hash_file(&jpeg).unwrap(),
    );
    np.status = PhotoStatus::Proposed;
    let pid = repo.insert_photo(np).unwrap();
    repo.insert_proposal(NewProposal {
        photo_id: pid.clone(),
        provider: "xai".to_string(),
        model: "grok-4.6".to_string(),
        prompt_version: "PROMPT_V1".to_string(),
        raw_json: "{}".to_string(),
        place_name: Some("Porto".to_string()),
        pin_lat: Some(41.1456),
        pin_lng: Some(-8.6143),
        pin_source: Some(PinSource::Geocoder),
        current: true,
        ..Default::default()
    })
    .unwrap();
    repo.add_decision(&pid, DecisionAction::Accept, None, None, None, None)
        .unwrap();

    let opts = ApplyOptions {
        allow_writes_override: Some(true),
        ..Default::default()
    };
    run_apply::<fn(&str)>(JOB_ID, &data_dir, opts.clone(), None).unwrap();
    let dest = copies_root(&data_dir, JOB_ID).join("IMG_a.jpg");
    assert!(dest.is_file());

    let undone = run_undo::<fn(&str)>(JOB_ID, &data_dir, None, None).unwrap();
    assert_eq!(undone.undone, 1);
    assert!(!dest.exists());
    assert_eq!(
        repo.get_photo(&pid).unwrap().unwrap().status,
        PhotoStatus::Accepted.as_str()
    );
    assert_eq!(
        repo.get_write_receipt(&pid, &dest.to_string_lossy())
            .unwrap()
            .unwrap()
            .status,
        WriteReceiptStatus::Undone.as_str()
    );
    assert_eq!(repo.list_write_receipts().unwrap().len(), 1);

    let again = run_apply::<fn(&str)>(JOB_ID, &data_dir, opts, None).unwrap();
    assert_eq!(again.wrote, 1);
    assert!(dest.is_file());
    assert_eq!(
        repo.get_photo(&pid).unwrap().unwrap().status,
        PhotoStatus::Written.as_str()
    );
    assert_eq!(repo.list_write_receipts().unwrap().len(), 1);
}
