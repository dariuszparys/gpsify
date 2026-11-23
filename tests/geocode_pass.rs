use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gpsify::config::Settings;
use gpsify::geocode::{GeocodeError, Geocoder};
use gpsify::ingest::{run_ingest, IngestOptions};
use gpsify::propose::{
    count_unpinned_proposals, instant_sleep_fn, run_geocode, run_propose, GeocodeOptions,
    ProposeOptions,
};
use gpsify::providers::FakeVisionProvider;
use gpsify::store::JobRepo;
use gpsify::types::{GeoHit, Granularity, RefusalReason, Usage, VisionGuess};
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
    s
}

/// Non-"fake" Geocoder double so `GeocodeService` enforces `geocode_uncached_max`.
#[derive(Default)]
struct CappableGeocoder {
    hits: Mutex<BTreeMap<String, Vec<GeoHit>>>,
    queries: Mutex<Vec<String>>,
}

impl CappableGeocoder {
    fn add(&self, query: &str, hit: GeoHit) {
        self.hits
            .lock()
            .unwrap()
            .entry(query.to_string())
            .or_default()
            .push(hit);
    }

    fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}

#[async_trait]
impl Geocoder for CappableGeocoder {
    fn name(&self) -> &str {
        "cappable"
    }

    async fn search(
        &self,
        query: &str,
        _country_code: Option<&str>,
        _lang: &str,
    ) -> Result<Vec<GeoHit>, GeocodeError> {
        self.queries.lock().unwrap().push(query.to_string());
        Ok(self
            .hits
            .lock()
            .unwrap()
            .get(query)
            .cloned()
            .unwrap_or_default())
    }
}

#[tokio::test]
async fn run_geocode_fills_pinless_proposals_promotes_and_zeroes_tokens() {
    let tmp = TempDir::new().unwrap();
    let album = tmp.path().join("album");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&album).unwrap();
    write_test_jpeg(&album.join("borderline.jpg"));
    write_test_jpeg(&album.join("indoor.jpg"));

    let job_id = run_ingest(&album, IngestOptions::new(&data_dir))
        .unwrap()
        .job_id;
    let settings = test_settings(&data_dir);

    let borderline_low = VisionGuess {
        refusal: RefusalReason::None,
        refusal_detail: None,
        primary_place_name: Some("Ribeira, Porto, Portugal".to_string()),
        country_code: Some("PT".to_string()),
        admin1: Some("Porto".to_string()),
        granularity: Granularity::Neighborhood,
        lat_hint: None,
        lng_hint: None,
        // rank without pin: 0.45*0.18 + 0.20*0.7 + 0.10*0.2 = 0.241 < 0.25 -> refused initially
        // rank with pin:    0.45*0.18 + 0.20*0.7 + 0.10*1.0 = 0.321 >= 0.25 -> promoted!
        model_confidence: 0.18,
        clues: Vec::new(),
        candidates: Vec::new(),
        used_album_hint: false,
        used_filename_hint: false,
    };

    let indoor = VisionGuess {
        refusal: RefusalReason::IndoorGeneric,
        refusal_detail: Some("Living room".to_string()),
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
    };

    let usage = Usage {
        input_tokens: 900,
        output_tokens: 150,
        reasoning_tokens: 50,
    };
    let vis = Arc::new(FakeVisionProvider::sequence(
        vec![Ok((borderline_low, usage)), Ok((indoor.clone(), usage))],
        (indoor, usage),
    ));

    // Uncached cap = 0 leaves borderline at pin_source=none without caching a miss.
    let capped_geo = Arc::new(CappableGeocoder::default());
    let mut prop_opts = ProposeOptions::new(settings.clone());
    prop_opts.geocode_uncached_max = 0;
    prop_opts.concurrency = 1;
    prop_opts.sleep = Some(instant_sleep_fn());
    prop_opts.vision = Some(vis);
    prop_opts.geocoder = Some(capped_geo.clone());
    let prop_summary = run_propose(&job_id, prop_opts).await.unwrap();
    assert_eq!(prop_summary.refused, 2);
    assert!(capped_geo.queries().is_empty());

    let repo = JobRepo::open(&data_dir, &job_id).unwrap();
    assert_eq!(count_unpinned_proposals(&repo).unwrap(), 1);

    // Dry run geocode counts 1 distinct/cold place and sends zero queries.
    let working_geo = Arc::new(CappableGeocoder::default());
    working_geo.add(
        "Ribeira, Porto, Portugal",
        GeoHit {
            query: "Ribeira, Porto, Portugal".to_string(),
            display_name: "Ribeira, Porto, Portugal".to_string(),
            lat: 41.1406,
            lng: -8.6130,
            boundingbox: None,
            osm_type: Some("node".to_string()),
            osm_id: Some("999".to_string()),
            importance: Some(0.7),
            provider: "cappable".to_string(),
        },
    );

    let mut dry_geo = GeocodeOptions::new(settings.clone());
    dry_geo.dry_run = true;
    dry_geo.geocoder = Some(working_geo.clone());
    let dry_res = run_geocode(&job_id, dry_geo).await.unwrap();
    assert_eq!(dry_res.planned, 1);
    assert_eq!(dry_res.distinct_places, 1);
    assert_eq!(dry_res.cold_places, 1);
    assert!(working_geo.queries().is_empty());

    // Live geocode pass fills pin, un-refuses borderline, ignores indoor refusal, zeroes token cols.
    let mut live_geo = GeocodeOptions::new(settings);
    live_geo.geocoder = Some(working_geo.clone());
    let res = run_geocode(&job_id, live_geo).await.unwrap();
    assert_eq!(res.planned, 1);
    assert_eq!(res.pinned, 1);
    assert_eq!(res.unrefused, 1);
    assert_eq!(res.still_unpinned, 0);
    assert_eq!(working_geo.queries(), vec!["Ribeira, Porto, Portugal"]);

    assert_eq!(count_unpinned_proposals(&repo).unwrap(), 0);
    let current = repo.list_current_proposals().unwrap();
    let filled = current
        .iter()
        .find(|p| p.place_name.as_deref() == Some("Ribeira, Porto, Portugal"))
        .unwrap();
    assert_eq!(filled.input_tokens, Some(0));
    assert_eq!(filled.output_tokens, Some(0));
    assert_eq!(filled.reasoning_tokens, Some(0));
    assert_eq!(filled.pin_source.as_deref(), Some("geocoder"));

    let stats: Value = serde_json::from_str(&repo.get_job().unwrap().unwrap().stats_json).unwrap();
    assert_eq!(stats["geocode"]["pinned"], 1);
    assert_eq!(stats["geocode"]["unrefused"], 1);
    assert_eq!(stats["geocode"]["runs"], 1);
}
