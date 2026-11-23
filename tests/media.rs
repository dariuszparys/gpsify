use std::io::Cursor;

use gpsify::exif::MediaKind;
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::media::{
    ensure_preview, ensure_thumb, extract_raw_preview_jpeg, extract_raw_preview_jpeg_with,
    hash_file, heic_available, preview_path, render_preview, render_thumb, thumb_path,
    MIN_RAW_PREVIEW_EDGE, PREVIEW_LONG_EDGE, THUMB_LONG_EDGE,
};
use gpsify::store::job_dir;
use gpsify::types::PhotoStatus;
use image::{ImageReader, Rgb, RgbImage};
use tempfile::TempDir;

fn encode_rgb_jpeg(w: u32, h: u32, color: [u8; 3], orientation: Option<u16>) -> Vec<u8> {
    let img = RgbImage::from_pixel(w, h, Rgb(color));
    let mut raw = Vec::new();
    img.write_to(&mut Cursor::new(&mut raw), image::ImageFormat::Jpeg)
        .unwrap();
    let Some(ori) = orientation else {
        return raw;
    };
    // Minimal little-endian TIFF APP1 segment with Orientation tag (0x0112).
    let mut tiff: Vec<u8> = Vec::new();
    tiff.extend_from_slice(b"Exif\0\0");
    tiff.extend_from_slice(b"II"); // little-endian
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
    tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 tag
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&ori.to_le_bytes());
    tiff.extend_from_slice(&0u16.to_le_bytes()); // pad to 4 bytes
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0

    let seg_len = (tiff.len() + 2) as u16;
    let mut out = Vec::with_capacity(raw.len() + tiff.len() + 4);
    out.extend_from_slice(&raw[..2]); // SOI FF D8
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&seg_len.to_be_bytes());
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&raw[2..]);
    out
}

fn jpeg_dimensions(blob: &[u8]) -> (u32, u32) {
    ImageReader::new(Cursor::new(blob))
        .with_guessed_format()
        .unwrap()
        .into_dimensions()
        .unwrap()
}

#[test]
fn exif_transpose_orientation_6() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sideways.jpg");
    let bytes = encode_rgb_jpeg(4, 8, [200, 10, 10], Some(6));
    std::fs::write(&path, &bytes).unwrap();
    let digest = hash_file(&path).unwrap();
    let jpeg = render_preview(&path, Some(MediaKind::Jpeg), &digest).unwrap();
    let (w, h) = jpeg_dimensions(&jpeg);
    assert_eq!((w, h), (8, 4));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(hash_file(&path).unwrap(), digest);
}

#[test]
fn preview_and_thumb_downscale_without_upscaling() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wide.jpg");
    std::fs::write(&path, encode_rgb_jpeg(1600, 800, [20, 80, 160], None)).unwrap();
    let digest = hash_file(&path).unwrap();

    let preview = render_preview(&path, None, &digest).unwrap();
    assert_eq!(jpeg_dimensions(&preview), (PREVIEW_LONG_EDGE, 640));

    let thumb = render_thumb(&path, None, &digest).unwrap();
    assert_eq!(jpeg_dimensions(&thumb), (THUMB_LONG_EDGE, 128));

    let small = tmp.path().join("small.jpg");
    std::fs::write(&small, encode_rgb_jpeg(40, 20, [10, 20, 30], None)).unwrap();
    let small_prev = render_preview(&small, None, &hash_file(&small).unwrap()).unwrap();
    assert_eq!(jpeg_dimensions(&small_prev), (40, 20));
}

#[test]
fn png_preview_does_not_mutate_source() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("plain.png");
    let img = RgbImage::from_pixel(32, 16, Rgb([8, 9, 10]));
    img.save(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    let digest = hash_file(&path).unwrap();
    let jpeg = render_preview(&path, Some(MediaKind::Png), &digest).unwrap();
    assert_eq!(&jpeg[..2], b"\xff\xd8");
    assert_eq!(jpeg_dimensions(&jpeg), (32, 16));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn source_sha256_mismatch_and_unreadable_refused() {
    let tmp = TempDir::new().unwrap();
    let ok = tmp.path().join("a.jpg");
    std::fs::write(&ok, encode_rgb_jpeg(16, 16, [1, 2, 3], None)).unwrap();
    let err = render_preview(&ok, None, &"0".repeat(64)).unwrap_err();
    assert_eq!(err.status, PhotoStatus::Error);
    assert!(err.message.to_ascii_lowercase().contains("sha256"));

    let bad = tmp.path().join("bad.jpg");
    std::fs::write(&bad, b"not-a-jpeg").unwrap();
    let err = render_preview(&bad, Some(MediaKind::Jpeg), &hash_file(&bad).unwrap()).unwrap_err();
    assert_eq!(err.status, PhotoStatus::SkippedUnreadable);
}

#[test]
fn symlink_and_photos_library_refused() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target.jpg");
    std::fs::write(&target, encode_rgb_jpeg(16, 16, [1, 2, 3], None)).unwrap();
    let link = tmp.path().join("link.jpg");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let err =
        render_preview(&link, Some(MediaKind::Jpeg), &hash_file(&target).unwrap()).unwrap_err();
    assert_eq!(err.status, PhotoStatus::Error);

    let bundle = tmp.path().join("Lib.photoslibrary").join("IMG.jpg");
    std::fs::create_dir_all(bundle.parent().unwrap()).unwrap();
    std::fs::copy(&target, &bundle).unwrap();
    let err =
        render_preview(&bundle, Some(MediaKind::Jpeg), &hash_file(&bundle).unwrap()).unwrap_err();
    assert_eq!(err.status, PhotoStatus::Error);
}

#[test]
fn ingest_does_not_write_previews_or_thumbs_and_ensure_is_lazy() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("gpsify-data");
    let album = tmp.path().join("album");
    std::fs::create_dir_all(&album).unwrap();
    let source = album.join("a.jpg");
    std::fs::write(&source, encode_rgb_jpeg(400, 200, [50, 100, 150], None)).unwrap();
    let before = std::fs::read(&source).unwrap();
    let digest = hash_file(&source).unwrap();

    let summary = run_ingest(&album, IngestOptions::new(&data_dir)).unwrap();
    let root = job_dir(&data_dir, &summary.job_id);
    assert!(!root.join("previews").exists());
    assert!(!root.join("thumbs").exists());

    let p = ensure_preview(&root, "photo-lazy", &source, &digest, Some(MediaKind::Jpeg)).unwrap();
    assert_eq!(p, preview_path(&root, "photo-lazy"));
    assert!(p.is_file());
    assert!(!thumb_path(&root, "photo-lazy").exists());

    let t = ensure_thumb(&root, "photo-lazy", &source, &digest, Some(MediaKind::Jpeg)).unwrap();
    assert_eq!(t, thumb_path(&root, "photo-lazy"));
    assert!(t.is_file());
    assert_eq!(std::fs::read(&source).unwrap(), before);
}

#[test]
fn raw_preview_extract_rules() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("body.cr2");
    std::fs::write(&raw, b"SENSOR-DUMP-NOT-A-JPEG").unwrap();

    let err = extract_raw_preview_jpeg_with(&raw, false, |_, _| Vec::new()).unwrap_err();
    assert_eq!(err.status, PhotoStatus::SkippedNoPreview);

    let tiny = encode_rgb_jpeg(160, 120, [10, 20, 30], None);
    let err = extract_raw_preview_jpeg_with(&raw, true, |_, _| tiny.clone()).unwrap_err();
    assert_eq!(err.status, PhotoStatus::SkippedNoPreview);
    assert!(err.message.contains(&MIN_RAW_PREVIEW_EDGE.to_string()));

    let good = encode_rgb_jpeg(400, 300, [40, 50, 200], None);
    let picked = extract_raw_preview_jpeg_with(&raw, true, |_, tag| {
        if tag == "PreviewImage" {
            good.clone()
        } else {
            Vec::new()
        }
    })
    .unwrap();
    assert_eq!(picked, good);

    // Real exiftool -b on fake RAW yields SkippedNoPreview
    if gpsify::exif::exiftool_available() {
        let rawish = tmp.path().join("not-really.cr2");
        std::fs::write(&rawish, encode_rgb_jpeg(32, 32, [1, 2, 3], None)).unwrap();
        let err = extract_raw_preview_jpeg(&rawish).unwrap_err();
        assert_eq!(err.status, PhotoStatus::SkippedNoPreview);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_imageio_decodes_real_heic() {
    assert!(heic_available());
    let tmp = TempDir::new().unwrap();
    let heic = tmp.path().join("sample.heic");
    let src_jpeg = encode_rgb_jpeg(64, 32, [12, 90, 180], None);
    let heic_bytes = gpsify::media_macos::encode_heic_from_bytes(&src_jpeg)
        .expect("ImageIO should encode HEIC in memory on macOS");
    std::fs::write(&heic, &heic_bytes).unwrap();
    assert!(heic.is_file());

    let digest = hash_file(&heic).unwrap();
    let jpeg = render_preview(&heic, Some(MediaKind::Heic), &digest).unwrap();
    assert_eq!(&jpeg[..2], b"\xff\xd8");
    let (w, h) = jpeg_dimensions(&jpeg);
    assert_eq!((w, h), (64, 32));
}
