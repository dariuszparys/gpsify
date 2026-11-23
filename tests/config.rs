use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use gpsify::config::{
    flatten_config_toml, load_settings_from_sources, load_settings_with, LoadOptions,
    SettingsOverrides, DEFAULT_GEOCODER_URL, DEFAULT_MAP_TILE_URL, DEFAULT_MODEL,
    DEFAULT_REASONING, DEFAULT_TIMEOUT_S, DEFAULT_VISION_PROVIDER,
};
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const ENV_KEYS: &[&str] = &[
    "XAI_API_KEY",
    "GEMINI_API_KEY",
    "GPSIFY_MODEL",
    "GPSIFY_GEMINI_MODEL",
    "GPSIFY_REASONING",
    "GPSIFY_TIMEOUT_S",
    "GPSIFY_VISION_PROVIDER",
    "GPSIFY_DATA_DIR",
    "GPSIFY_GEOCODER",
    "NOMINATIM_CONTACT",
    "GEOCODER_URL",
    "MAP_TILE_URL",
    "GEOCODE_LANG",
];

fn clear_env_vars() {
    for k in ENV_KEYS {
        std::env::remove_var(k);
    }
}

#[test]
fn defaults_match_design() {
    let tmp = TempDir::new().unwrap();
    let mut env = BTreeMap::new();
    env.insert(
        "GPSIFY_DATA_DIR".to_string(),
        tmp.path().to_string_lossy().into_owned(),
    );
    let settings =
        load_settings_from_sources(SettingsOverrides::default(), &env, &BTreeMap::new()).unwrap();
    assert_eq!(settings.xai_api_key, None);
    assert_eq!(settings.vision_provider, DEFAULT_VISION_PROVIDER);
    assert_eq!(settings.model, DEFAULT_MODEL);
    assert_eq!(settings.model, "grok-4.6");
    assert_eq!(settings.reasoning_effort, DEFAULT_REASONING);
    assert_eq!(settings.reasoning_effort, "low");
    assert_eq!(settings.timeout_s, DEFAULT_TIMEOUT_S);
    assert_eq!(settings.timeout_s, 600.0);
    assert_eq!(settings.nominatim_contact, None);
    assert_eq!(settings.geocoder_url, DEFAULT_GEOCODER_URL);
    assert_eq!(settings.geocoder, "nominatim");
    assert_eq!(settings.map_tile_url, DEFAULT_MAP_TILE_URL);
    assert_eq!(settings.geocode_lang, "en");
    assert_eq!(settings.data_dir, tmp.path());
}

#[test]
fn default_data_dir_is_home_gpsify() {
    let settings = load_settings_from_sources(
        SettingsOverrides::default(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let expected = dirs::home_dir().unwrap().join(".gpsify");
    assert_eq!(settings.data_dir, expected);
}

#[test]
fn env_overrides_via_real_process_env() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env_vars();
    let tmp = TempDir::new().unwrap();
    std::env::set_var("GPSIFY_DATA_DIR", tmp.path());
    std::env::set_var("XAI_API_KEY", "xai-test-key");
    std::env::set_var("GPSIFY_MODEL", "grok-env");
    std::env::set_var("GPSIFY_REASONING", "medium");
    std::env::set_var("GPSIFY_TIMEOUT_S", "180");
    std::env::set_var("GPSIFY_VISION_PROVIDER", "xai");
    std::env::set_var("NOMINATIM_CONTACT", "dev@example.com");
    std::env::set_var("GEOCODER_URL", "http://127.0.0.1:8080/search");

    let settings = load_settings_with(LoadOptions {
        load_dotenv: false,
        overrides: SettingsOverrides::default(),
    })
    .unwrap();
    clear_env_vars();

    assert_eq!(settings.xai_api_key.as_deref(), Some("xai-test-key"));
    assert_eq!(settings.model, "grok-env");
    assert_eq!(settings.reasoning_effort, "medium");
    assert_eq!(settings.timeout_s, 180.0);
    assert_eq!(settings.nominatim_contact.as_deref(), Some("dev@example.com"));
    assert_eq!(settings.geocoder_url, "http://127.0.0.1:8080/search");
}

#[test]
fn toml_nested_vision() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("config.toml"),
        r#"
nominatim_contact = "toml@example.com"
geocoder_url = "https://photon.komoot.io/api"
geocoder = "photon"
geocode_lang = "de"

[vision]
provider = "xai"
model = "grok-toml"
reasoning_effort = "high"
timeout_s = 240
"#,
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "GPSIFY_DATA_DIR".to_string(),
        tmp.path().to_string_lossy().into_owned(),
    );
    let settings =
        load_settings_from_sources(SettingsOverrides::default(), &env, &BTreeMap::new()).unwrap();
    assert_eq!(
        settings.nominatim_contact.as_deref(),
        Some("toml@example.com")
    );
    assert_eq!(settings.geocoder_url, "https://photon.komoot.io/api");
    assert_eq!(settings.geocoder, "photon");
    assert_eq!(settings.geocode_lang, "de");
    assert_eq!(settings.vision_provider, "xai");
    assert_eq!(settings.model, "grok-toml");
    assert_eq!(settings.reasoning_effort, "high");
    assert_eq!(settings.timeout_s, 240.0);
}

#[test]
fn env_wins_over_toml() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("config.toml"),
        r#"
[vision]
model = "grok-toml"
reasoning_effort = "medium"
"#,
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "GPSIFY_DATA_DIR".to_string(),
        tmp.path().to_string_lossy().into_owned(),
    );
    env.insert("GPSIFY_MODEL".to_string(), "grok-env".to_string());
    env.insert("GPSIFY_REASONING".to_string(), "low".to_string());

    let settings =
        load_settings_from_sources(SettingsOverrides::default(), &env, &BTreeMap::new()).unwrap();
    assert_eq!(settings.model, "grok-env");
    assert_eq!(settings.reasoning_effort, "low");
}

#[test]
fn rejects_xhigh_reasoning_from_override_and_env() {
    let err = load_settings_from_sources(
        SettingsOverrides {
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("xhigh"));

    let mut env = BTreeMap::new();
    env.insert("GPSIFY_REASONING".to_string(), "xhigh".to_string());
    let env_err =
        load_settings_from_sources(SettingsOverrides::default(), &env, &BTreeMap::new())
            .unwrap_err();
    assert!(env_err.to_string().contains("xhigh"));
}

#[test]
fn rejects_timeout_below_minimum() {
    let err = load_settings_from_sources(
        SettingsOverrides {
            timeout_s: Some(60.0),
            ..Default::default()
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("120"));
}

#[test]
fn rejects_non_email_nominatim_contact() {
    let err = load_settings_from_sources(
        SettingsOverrides {
            nominatim_contact: Some(Some("not-an-email".to_string())),
            ..Default::default()
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("email-shaped"));
}

#[test]
fn empty_nominatim_contact_is_none() {
    let settings = load_settings_from_sources(
        SettingsOverrides {
            nominatim_contact: Some(Some("  ".to_string())),
            ..Default::default()
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(settings.nominatim_contact, None);
}

#[test]
fn flatten_config_toml_lifts_vision_table() {
    let parsed: toml::map::Map<String, toml::Value> = toml::from_str(
        r#"
nominatim_contact = "a@b.co"

[vision]
model = "grok-4.6"
provider = "xai"
reasoning_effort = "low"
"#,
    )
    .unwrap();
    let flat = flatten_config_toml(&parsed).unwrap();
    assert_eq!(flat["nominatim_contact"].as_str(), Some("a@b.co"));
    assert_eq!(flat["model"].as_str(), Some("grok-4.6"));
    assert_eq!(flat["vision_provider"].as_str(), Some("xai"));
    assert!(!flat.contains_key("vision"));
}

#[test]
fn data_dir_expands_user() {
    let settings = load_settings_from_sources(
        SettingsOverrides {
            data_dir: Some(PathBuf::from("~/gpsify-data")),
            ..Default::default()
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(settings.data_dir, dirs::home_dir().unwrap().join("gpsify-data"));
}
