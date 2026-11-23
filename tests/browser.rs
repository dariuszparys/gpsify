use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
};
use chromiumoxide::layout::Point;
use futures::StreamExt;
use gpsify::config::{load_settings_from_sources, SettingsOverrides};
use gpsify::geocode::{FakeGeocoder, GeocodeCache, GeocodeService};
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::review::{bind_review_listener, create_router, ReviewState};
use gpsify::store::{job_dir, JobRepo, NewProposal};
use gpsify::types::{PhotoStatus, PinSource};
use image::{Rgb, RgbImage};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::{Mutex, Semaphore};

fn find_chrome_executable() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("CHROME_PATH") {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ];
    for c in candidates {
        let p = Path::new(c);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}

async fn setup_review_server(tmp: &Path) -> (String, tokio::task::JoinHandle<()>) {
    let album = tmp.join("album");
    std::fs::create_dir_all(&album).unwrap();
    for i in 0..3u8 {
        RgbImage::from_pixel(80, 80, Rgb([i * 70, 90, 150]))
            .save(album.join(format!("{i}.jpg")))
            .unwrap();
    }
    let data = tmp.join("data");
    let ingested = run_ingest(&album, IngestOptions::new(&data)).unwrap();
    let repo = JobRepo::open(&data, &ingested.job_id).unwrap();
    let mut photos = repo.list_photos().unwrap();
    photos.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    for (i, photo) in photos.iter().enumerate() {
        repo.insert_proposal(NewProposal {
            photo_id: photo.id.clone(),
            provider: "fake".to_string(),
            model: "fake".to_string(),
            prompt_version: "test".to_string(),
            raw_json: "{}".to_string(),
            place_name: Some(format!("Place {i}")),
            pin_lat: Some(40.0 + i as f64),
            pin_lng: Some(10.0 + i as f64),
            pin_source: Some(PinSource::Geocoder),
            ui_rank: Some(0.9),
            current: true,
            ..Default::default()
        })
        .unwrap();
        repo.set_photo_status(&photo.id, PhotoStatus::Proposed, None, None)
            .unwrap();
    }

    let settings = load_settings_from_sources(
        SettingsOverrides {
            data_dir: Some(data.clone()),
            geocoder_url: Some("http://127.0.0.1:1/search".to_string()),
            map_tile_url: Some("/tiles/{z}/{x}/{y}.png".to_string()),
            ..Default::default()
        },
        &Default::default(),
        &Default::default(),
    )
    .unwrap();

    let cache = GeocodeCache::open(data.join("cache").join("geocode.sqlite")).unwrap();
    let service = GeocodeService::new(Arc::new(FakeGeocoder::new()), cache, "en", 50, false);
    let token = "browser-test-token-xyz".to_string();
    let state = ReviewState {
        settings,
        job_id: ingested.job_id.clone(),
        repo,
        service,
        job_root: job_dir(&data, &ingested.job_id),
        auth_token: token.clone(),
        media_slots: Arc::new(Semaphore::new(2)),
        mutation_lock: Arc::new(Mutex::new(())),
    };

    let listener = bind_review_listener(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = create_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    (format!("http://127.0.0.1:{port}/?token={token}"), handle)
}

async fn wait_for_tiles(page: &chromiumoxide::Page, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let count = page.find_elements(".tile").await.unwrap_or_default().len();
        if count == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} tiles (got {count})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_selected(page: &chromiumoxide::Page, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count = page
            .find_elements(".tile.selected")
            .await
            .unwrap_or_default()
            .len();
        if count == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} selected tiles (got {count})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn shift_click_last_tile(page: &chromiumoxide::Page) {
    let tiles = page.find_elements(".tile").await.unwrap();
    let last = tiles.last().unwrap();
    let pt = last
        .scroll_into_view()
        .await
        .unwrap()
        .clickable_point()
        .await
        .unwrap();
    let pressed = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(pt.x)
        .y(pt.y)
        .button(MouseButton::Left)
        .modifiers(8) // Shift
        .click_count(1)
        .build()
        .unwrap();
    let released = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(pt.x)
        .y(pt.y)
        .button(MouseButton::Left)
        .modifiers(8)
        .click_count(1)
        .build()
        .unwrap();
    page.execute(pressed).await.unwrap();
    page.execute(released).await.unwrap();
}

async fn accepted_records(page: &chromiumoxide::Page) -> Vec<Value> {
    let val: Value = page
        .evaluate_function(
            "async () => { const r = await fetch('/api/photos?status=accepted'); return await r.json(); }",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    val["photos"].as_array().unwrap().clone()
}

async fn launch_browser(profile_dir: &Path) -> (Browser, tokio::task::JoinHandle<()>) {
    let mut builder = BrowserConfig::builder()
        .user_data_dir(profile_dir)
        .window_size(1440, 1000)
        .no_sandbox()
        .new_headless_mode();
    if let Some(exe) = find_chrome_executable() {
        builder = builder.chrome_executable(exe);
    }
    let config = builder.build().unwrap();
    let (browser, mut handler) = Browser::launch(config).await.unwrap();
    let task = tokio::spawn(async move {
        while let Some(ev) = handler.next().await {
            if let Err(err) = ev {
                if matches!(err, chromiumoxide::error::CdpError::Serde(_)) {
                    continue;
                }
                eprintln!("chromiumoxide handler error: {err:?}");
                break;
            }
        }
    });
    (browser, task)
}

#[tokio::test]
async fn headless_chrome_review_workflows() {
    // 1. Bulk accept preserves each photo's own proposal pin
    {
        let tmp = TempDir::new().unwrap();
        let profile = tmp.path().join("chrome-profile");
        let (url, server_task) = setup_review_server(tmp.path()).await;
        let (mut browser, handler_task) = launch_browser(&profile).await;
        let page = browser.new_page(&url).await.unwrap();
        wait_for_tiles(&page, 3).await;

        page.find_element(".tile")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        shift_click_last_tile(&page).await;
        wait_for_selected(&page, 3).await;

        page.find_element("#btn-accept")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        wait_for_tiles(&page, 0).await;

        page.find_element(r#"[data-filter="accepted"]"#)
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        wait_for_tiles(&page, 3).await;

        let records = accepted_records(&page).await;
        let lats: Vec<f64> = records
            .iter()
            .map(|p| p["decision"]["lat"].as_f64().unwrap())
            .collect();
        assert_eq!(lats, vec![40.0, 41.0, 42.0]);

        browser.close().await.unwrap();
        handler_task.abort();
        server_task.abort();
    }

    // 2. Edit pin persists accept_edited decision
    {
        let tmp = TempDir::new().unwrap();
        let profile = tmp.path().join("chrome-profile");
        let (url, server_task) = setup_review_server(tmp.path()).await;
        let (mut browser, handler_task) = launch_browser(&profile).await;
        let page = browser.new_page(&url).await.unwrap();
        wait_for_tiles(&page, 3).await;

        page.find_element(".tile")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        page.find_element("#btn-edit")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();

        let map_pt: Value = page
            .evaluate_function(
                "() => { const r = document.querySelector('#map').getBoundingClientRect(); return { x: r.left + 160, y: r.top + 140 }; }",
            )
            .await
            .unwrap()
            .into_value()
            .unwrap();
        page.click(Point {
            x: map_pt["x"].as_f64().unwrap(),
            y: map_pt["y"].as_f64().unwrap(),
        })
        .await
        .unwrap();

        page.find_element("#btn-edit")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        wait_for_tiles(&page, 2).await;

        let records = accepted_records(&page).await;
        assert_eq!(records[0]["decision"]["action"], "accept_edited");
        assert_ne!(records[0]["decision"]["lat"].as_f64().unwrap(), 40.0);

        browser.close().await.unwrap();
        handler_task.abort();
        server_task.abort();
    }

    // 3. Undated selection pin copy requires exact confirm_n
    {
        let tmp = TempDir::new().unwrap();
        let profile = tmp.path().join("chrome-profile");
        let (url, server_task) = setup_review_server(tmp.path()).await;
        let (mut browser, handler_task) = launch_browser(&profile).await;
        let page = browser.new_page(&url).await.unwrap();
        wait_for_tiles(&page, 3).await;

        page.find_element(".tile")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        shift_click_last_tile(&page).await;
        page.find_element("#btn-apply-pin")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();

        let confirm_input = page.find_element("#apply-confirm").await.unwrap();
        confirm_input
            .click()
            .await
            .unwrap()
            .type_str("1")
            .await
            .unwrap();
        page.find_element("#apply-ok")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let txt = page
                .find_element("#status-msg")
                .await
                .unwrap()
                .inner_text()
                .await
                .unwrap()
                .unwrap_or_default();
            if txt.contains("requires confirm_n") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "got status: {txt:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(accepted_records(&page).await.len(), 1);

        page.find_element(r#"[data-filter="all"]"#)
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        wait_for_tiles(&page, 3).await;
        page.find_element(".tile")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        shift_click_last_tile(&page).await;
        page.find_element("#btn-apply-pin")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();

        page.evaluate_function("() => { document.querySelector('#apply-confirm').value = ''; }")
            .await
            .unwrap();
        let confirm_input = page.find_element("#apply-confirm").await.unwrap();
        confirm_input
            .click()
            .await
            .unwrap()
            .type_str("2")
            .await
            .unwrap();
        page.find_element("#apply-ok")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let txt = page
                .find_element("#status-msg")
                .await
                .unwrap()
                .inner_text()
                .await
                .unwrap()
                .unwrap_or_default();
            if txt.contains("Pin applied to 2 photos") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "got status: {txt:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let records = accepted_records(&page).await;
        assert_eq!(records.len(), 3);
        let lats: std::collections::BTreeSet<i64> = records
            .iter()
            .map(|p| p["decision"]["lat"].as_f64().unwrap() as i64)
            .collect();
        assert_eq!(lats.into_iter().collect::<Vec<_>>(), vec![42]);

        browser.close().await.unwrap();
        handler_task.abort();
        server_task.abort();
    }
}
