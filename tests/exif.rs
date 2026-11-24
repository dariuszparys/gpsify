use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gpsify::exif::{
    classify_media, dest_gps_matches, exiftool_available, read_exif_batch, read_exif_batch_with,
    render_exiftool_argfile, write_gps_batch_with, GpsWrite, MediaKind, DRY_RUN_BLOCKS_WRITES,
    EXIFTOOL_CHUNK_SIZE, MISSING_EXIFTOOL_WARNING, WRITE_REQUIRES_EXIFTOOL,
};
use image::{Rgb, RgbImage};
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const PORTO_LAT: f64 = 41.1456;
const PORTO_LNG: f64 = -8.6143;

fn write_plain_jpeg(path: &Path) -> PathBuf {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let img = RgbImage::from_pixel(16, 16, Rgb([30, 80, 140]));
    img.save(path).unwrap();
    path.to_path_buf()
}

#[test]
fn classify_media_extensions() {
    assert_eq!(classify_media(Path::new("a.JPG")), MediaKind::Jpeg);
    assert_eq!(classify_media(Path::new("a.jpeg")), MediaKind::Jpeg);
    assert_eq!(classify_media(Path::new("a.png")), MediaKind::Png);
    assert_eq!(classify_media(Path::new("a.heic")), MediaKind::Heic);
    assert_eq!(classify_media(Path::new("a.HEIF")), MediaKind::Heic);
    assert_eq!(classify_media(Path::new("a.cr2")), MediaKind::Raw);
    assert_eq!(classify_media(Path::new("a.CR3")), MediaKind::Raw);
    assert_eq!(classify_media(Path::new("a.nef")), MediaKind::Raw);
    assert_eq!(classify_media(Path::new("a.arw")), MediaKind::Raw);
    assert_eq!(classify_media(Path::new("a.dng")), MediaKind::Raw);
    assert_eq!(classify_media(Path::new("a.mov")), MediaKind::Unsupported);
}

#[test]
fn fallback_reads_checked_in_jpegs() {
    let fixtures = Path::new("tests/fixtures");
    let with_gps = std::fs::canonicalize(fixtures.join("jpeg_with_gps.jpg")).unwrap();
    let no_gps = std::fs::canonicalize(fixtures.join("jpeg_no_gps.jpg")).unwrap();

    let result = read_exif_batch_with(&[with_gps.clone(), no_gps.clone()], false);
    assert!(!result.used_exiftool);
    assert!(result
        .warnings
        .contains(&MISSING_EXIFTOOL_WARNING.to_string()));

    let tagged = &result.by_path[&with_gps.to_string_lossy().into_owned()];
    let plain = &result.by_path[&no_gps.to_string_lossy().into_owned()];
    assert!(tagged.has_gps());
    assert!((tagged.gps_lat.unwrap() - PORTO_LAT).abs() < 1e-4);
    assert!((tagged.gps_lng.unwrap() - PORTO_LNG).abs() < 1e-4);
    assert_eq!(
        tagged.datetime_original.as_deref(),
        Some("2019:06:14 18:02:00")
    );
    assert_eq!(tagged.source, "rust_exif");

    assert!(!plain.has_gps());
    assert_eq!(
        plain.datetime_original.as_deref(),
        Some("2019:06:14 18:04:00")
    );
}

#[test]
fn raw_sibling_xmp_gps_and_heic_without_exiftool() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("IMG_0001.CR2");
    std::fs::write(&raw, b"not-a-real-raw").unwrap();
    std::fs::write(
        tmp.path().join("IMG_0001.xmp"),
        r#"<?xml version="1.0"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/"
      exif:GPSLatitude="41.1456"
      exif:GPSLongitude="-8.6143"/>
  </rdf:RDF>
</x:xmpmeta>
"#,
    )
    .unwrap();
    let heic = tmp.path().join("IMG_0002.HEIC");
    std::fs::write(&heic, b"ftypheic").unwrap();

    let result = read_exif_batch_with(&[raw.clone(), heic.clone()], false);
    let raw_key = std::fs::canonicalize(&raw)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let heic_key = std::fs::canonicalize(&heic)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let raw_tags = &result.by_path[&raw_key];
    assert!(raw_tags.has_gps());
    assert!((raw_tags.gps_lat.unwrap() - PORTO_LAT).abs() < 1e-6);
    assert!((raw_tags.gps_lng.unwrap() - PORTO_LNG).abs() < 1e-6);
    assert_eq!(raw_tags.source, "xmp");

    let heic_tags = &result.by_path[&heic_key];
    assert!(!heic_tags.has_gps());
    assert_eq!(heic_tags.source, "skipped_heic");
}

#[test]
fn argfile_quotes_leading_dash_and_hash() {
    let body = render_exiftool_argfile(&[
        GpsWrite {
            dest: PathBuf::from("-dash.jpg"),
            lat: 1.0,
            lng: 2.0,
            kind: MediaKind::Jpeg,
            job_id: "j".to_string(),
            decision_id: "d1".to_string(),
        },
        GpsWrite {
            dest: PathBuf::from("#comment.jpg"),
            lat: 3.0,
            lng: 4.0,
            kind: MediaKind::Jpeg,
            job_id: "j".to_string(),
            decision_id: "d2".to_string(),
        },
    ])
    .unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert!(lines.contains(&"./-dash.jpg"));
    assert!(lines.contains(&"./#comment.jpg"));
    assert!(lines.contains(&"-charset"));
    assert!(lines.contains(&"filename=utf8"));
    assert!(!lines.contains(&"--"));
}

#[test]
fn dry_run_and_missing_exiftool_block_writes() {
    let tmp = TempDir::new().unwrap();
    let jpeg = write_plain_jpeg(&tmp.path().join("a.jpg"));
    let items = [GpsWrite {
        dest: jpeg,
        lat: PORTO_LAT,
        lng: PORTO_LNG,
        kind: MediaKind::Jpeg,
        job_id: "j".to_string(),
        decision_id: "d".to_string(),
    }];

    {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("GPSIFY_DRY_RUN").ok();
        std::env::set_var("GPSIFY_DRY_RUN", "1");
        let err = gpsify::exif::write_gps_batch(&items).unwrap_err();
        if let Some(p) = prev {
            std::env::set_var("GPSIFY_DRY_RUN", p);
        } else {
            std::env::remove_var("GPSIFY_DRY_RUN");
        }
        assert_eq!(err.0, DRY_RUN_BLOCKS_WRITES);
    }

    let err = write_gps_batch_with(&items, true, false).unwrap_err();
    assert_eq!(err.0, WRITE_REQUIRES_EXIFTOOL);
}

#[test]
fn exiftool_batch_read_and_write_roundtrip() {
    if !exiftool_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let first = write_plain_jpeg(&tmp.path().join("-leading-dash.jpg"));
    let second = write_plain_jpeg(&tmp.path().join("#leading-hash.jpg"));
    let errors = write_gps_batch_with(
        &[
            GpsWrite {
                dest: first.clone(),
                lat: PORTO_LAT,
                lng: PORTO_LNG,
                kind: MediaKind::Jpeg,
                job_id: "job".to_string(),
                decision_id: "d1".to_string(),
            },
            GpsWrite {
                dest: second.clone(),
                lat: 48.8584,
                lng: 2.2945,
                kind: MediaKind::Jpeg,
                job_id: "job".to_string(),
                decision_id: "d2".to_string(),
            },
        ],
        true,
        true,
    )
    .unwrap();
    assert_eq!(errors[&first.to_string_lossy().into_owned()], None);
    assert_eq!(errors[&second.to_string_lossy().into_owned()], None);
    assert!(dest_gps_matches(&first, PORTO_LAT, PORTO_LNG));
    assert!(dest_gps_matches(&second, 48.8584, 2.2945));

    // Also test > EXIFTOOL_CHUNK_SIZE batching on real exiftool read
    let mut batch = Vec::new();
    for i in 0..=EXIFTOOL_CHUNK_SIZE {
        let p = tmp.path().join(format!("b_{i:03}.jpg"));
        std::fs::copy(&first, &p).unwrap();
        batch.push(p);
    }
    let res = read_exif_batch(&batch);
    assert!(res.used_exiftool);
    assert_eq!(res.by_path.len(), EXIFTOOL_CHUNK_SIZE + 1);
    let last_key = std::fs::canonicalize(batch.last().unwrap())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(res.by_path[&last_key].has_gps());
}
