//! Application settings from overrides, environment, `.env`, and `<data_dir>/config.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{GeocoderName, ReasoningEffort, VisionProviderName, MIN_VISION_TIMEOUT_S};

pub const DEFAULT_MODEL: &str = "grok-4.6";
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
pub const DEFAULT_REASONING: &str = "low";
pub const DEFAULT_TIMEOUT_S: f64 = 600.0;
pub const DEFAULT_VISION_PROVIDER: &str = "xai";
pub const DEFAULT_GEOCODER_URL: &str = "https://nominatim.openstreetmap.org/search";
pub const DEFAULT_MAP_TILE_URL: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
pub const DEFAULT_GEOCODE_LANG: &str = "en";
pub const DEFAULT_GEOCODER: &str = "nominatim";

const VISION_TOML_KEYS: &[(&str, &str)] = &[
    ("provider", "vision_provider"),
    ("model", "model"),
    ("gemini_model", "gemini_model"),
    ("reasoning_effort", "reasoning_effort"),
    ("timeout_s", "timeout_s"),
];

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

fn email_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").expect("valid email regex"))
}

pub fn expand_user_path(raw: &Path) -> PathBuf {
    let text = raw.to_string_lossy();
    if text == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    raw.to_path_buf()
}

pub fn home_gpsify_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gpsify")
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("GPSIFY_DATA_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_user_path(Path::new(trimmed));
        }
    }
    home_gpsify_dir()
}

pub fn config_toml_path(data_dir: Option<&Path>) -> PathBuf {
    match data_dir {
        Some(dir) => dir.join("config.toml"),
        None => default_data_dir().join("config.toml"),
    }
}

pub fn flatten_config_toml(
    raw: &toml::map::Map<String, toml::Value>,
) -> Result<toml::map::Map<String, toml::Value>, ConfigError> {
    let mut flat = toml::map::Map::new();
    for (k, v) in raw {
        if k != "vision" {
            flat.insert(k.clone(), v.clone());
        }
    }
    if let Some(vision_val) = raw.get("vision") {
        let vision = vision_val
            .as_table()
            .ok_or_else(|| ConfigError("config.toml [vision] must be a table".to_string()))?;
        for &(src, dest) in VISION_TOML_KEYS {
            if let Some(val) = vision.get(src) {
                flat.insert(dest.to_string(), val.clone());
            }
        }
    }
    Ok(flat)
}

#[derive(Debug, Clone, Default)]
pub struct SettingsOverrides {
    pub xai_api_key: Option<Option<String>>,
    pub vision_provider: Option<String>,
    pub model: Option<String>,
    pub gemini_api_key: Option<Option<String>>,
    pub gemini_model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub timeout_s: Option<f64>,
    pub nominatim_contact: Option<Option<String>>,
    pub geocoder_url: Option<String>,
    pub geocoder: Option<String>,
    pub map_tile_url: Option<String>,
    pub geocode_lang: Option<String>,
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub load_dotenv: bool,
    pub overrides: SettingsOverrides,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            load_dotenv: true,
            overrides: SettingsOverrides::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub xai_api_key: Option<String>,
    pub vision_provider: String,
    pub model: String,
    pub gemini_api_key: Option<String>,
    pub gemini_model: String,
    pub reasoning_effort: String,
    pub timeout_s: f64,
    pub nominatim_contact: Option<String>,
    pub geocoder_url: String,
    pub geocoder: String,
    pub map_tile_url: String,
    pub geocode_lang: String,
    pub data_dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            xai_api_key: None,
            vision_provider: DEFAULT_VISION_PROVIDER.to_string(),
            model: DEFAULT_MODEL.to_string(),
            gemini_api_key: None,
            gemini_model: DEFAULT_GEMINI_MODEL.to_string(),
            reasoning_effort: DEFAULT_REASONING.to_string(),
            timeout_s: DEFAULT_TIMEOUT_S,
            nominatim_contact: None,
            geocoder_url: DEFAULT_GEOCODER_URL.to_string(),
            geocoder: DEFAULT_GEOCODER.to_string(),
            map_tile_url: DEFAULT_MAP_TILE_URL.to_string(),
            geocode_lang: DEFAULT_GEOCODE_LANG.to_string(),
            data_dir: home_gpsify_dir(),
        }
    }
}

impl Settings {
    pub fn provider_name(&self) -> Result<VisionProviderName, ConfigError> {
        VisionProviderName::from_str(&self.vision_provider).map_err(|e| ConfigError(e.0))
    }

    pub fn geocoder_name(&self) -> Result<GeocoderName, ConfigError> {
        GeocoderName::from_str(&self.geocoder).map_err(|e| ConfigError(e.0))
    }

    pub fn validate(mut self) -> Result<Self, ConfigError> {
        self.reasoning_effort = ReasoningEffort::from_str(&self.reasoning_effort)
            .map_err(|e| ConfigError(e.0))?
            .as_str()
            .to_string();

        if !self.timeout_s.is_finite() || self.timeout_s < MIN_VISION_TIMEOUT_S {
            return Err(ConfigError(format!(
                "timeout_s must be >= {MIN_VISION_TIMEOUT_S}"
            )));
        }

        self.nominatim_contact = match self.nominatim_contact {
            None => None,
            Some(raw) => {
                let stripped = raw.trim();
                if stripped.is_empty() {
                    None
                } else {
                    if !email_regex().is_match(stripped) {
                        return Err(ConfigError(
                            "nominatim_contact must be an email-shaped string".to_string(),
                        ));
                    }
                    Some(stripped.to_string())
                }
            }
        };

        self.data_dir = expand_user_path(&self.data_dir);
        self.vision_provider = self.vision_provider.trim().to_ascii_lowercase();
        let _ =
            VisionProviderName::from_str(&self.vision_provider).map_err(|e| ConfigError(e.0))?;
        self.geocoder = self.geocoder.trim().to_ascii_lowercase();
        self.geocode_lang = self.geocode_lang.trim().to_ascii_lowercase();
        self.model = self.model.trim().to_string();
        self.gemini_model = self.gemini_model.trim().to_string();

        Ok(self)
    }
}

fn read_dotenv_map(path: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if !path.is_file() {
        return map;
    }
    if let Ok(iter) = dotenvy::from_path_iter(path) {
        for item in iter.flatten() {
            if !item.1.is_empty() {
                map.insert(item.0, item.1);
            }
        }
    }
    map
}

fn lookup_alias(
    primary: &str,
    fallback: &str,
    env: &BTreeMap<String, String>,
    dotenv: &BTreeMap<String, String>,
) -> Option<String> {
    for source in [env, dotenv] {
        for key in [primary, fallback] {
            if let Some(v) = source.get(key) {
                if !v.is_empty() {
                    return Some(v.clone());
                }
            }
        }
    }
    None
}

fn toml_string(
    map: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<String>, ConfigError> {
    match map.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(ConfigError(format!(
            "config.toml {key} must be a string, got {}",
            other.type_str()
        ))),
    }
}

fn toml_float(
    map: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<f64>, ConfigError> {
    match map.get(key) {
        None => Ok(None),
        Some(toml::Value::Float(f)) => Ok(Some(*f)),
        Some(toml::Value::Integer(i)) => Ok(Some(*i as f64)),
        Some(other) => Err(ConfigError(format!(
            "config.toml {key} must be a number, got {}",
            other.type_str()
        ))),
    }
}

pub fn load_settings() -> Result<Settings, ConfigError> {
    load_settings_with(LoadOptions::default())
}

pub fn load_settings_with(options: LoadOptions) -> Result<Settings, ConfigError> {
    let env: BTreeMap<String, String> = std::env::vars().filter(|(_, v)| !v.is_empty()).collect();
    let dotenv = if options.load_dotenv {
        read_dotenv_map(Path::new(".env"))
    } else {
        BTreeMap::new()
    };
    load_settings_from_sources(options.overrides, &env, &dotenv)
}

pub fn load_settings_from_sources(
    overrides: SettingsOverrides,
    env: &BTreeMap<String, String>,
    dotenv: &BTreeMap<String, String>,
) -> Result<Settings, ConfigError> {
    let data_dir_for_toml = overrides
        .data_dir
        .clone()
        .or_else(|| lookup_alias("GPSIFY_DATA_DIR", "data_dir", env, dotenv).map(PathBuf::from))
        .map(|p| expand_user_path(&p))
        .unwrap_or_else(home_gpsify_dir);

    let toml_path = config_toml_path(Some(&data_dir_for_toml));
    let toml_map = if toml_path.is_file() {
        let raw_text = std::fs::read_to_string(&toml_path)
            .map_err(|e| ConfigError(format!("failed reading {}: {e}", toml_path.display())))?;
        let parsed: toml::map::Map<String, toml::Value> = toml::from_str(&raw_text)
            .map_err(|e| ConfigError(format!("invalid {}: {e}", toml_path.display())))?;
        flatten_config_toml(&parsed)?
    } else {
        toml::map::Map::new()
    };

    let mut settings = Settings::default();

    let pick_str = |primary: &str,
                    fallback: &str,
                    toml_key: &str,
                    default: &str|
     -> Result<String, ConfigError> {
        if let Some(v) = lookup_alias(primary, fallback, env, dotenv) {
            return Ok(v);
        }
        if let Some(v) = toml_string(&toml_map, toml_key)? {
            return Ok(v);
        }
        Ok(default.to_string())
    };

    let pick_opt_str =
        |primary: &str, fallback: &str, toml_key: &str| -> Result<Option<String>, ConfigError> {
            if let Some(v) = lookup_alias(primary, fallback, env, dotenv) {
                return Ok(Some(v));
            }
            toml_string(&toml_map, toml_key)
        };

    settings.xai_api_key = match overrides.xai_api_key {
        Some(v) => v,
        None => pick_opt_str("XAI_API_KEY", "xai_api_key", "xai_api_key")?,
    };
    settings.vision_provider = match overrides.vision_provider {
        Some(v) => v,
        None => pick_str(
            "GPSIFY_VISION_PROVIDER",
            "vision_provider",
            "vision_provider",
            DEFAULT_VISION_PROVIDER,
        )?,
    };
    settings.model = match overrides.model {
        Some(v) => v,
        None => pick_str("GPSIFY_MODEL", "model", "model", DEFAULT_MODEL)?,
    };
    settings.gemini_api_key = match overrides.gemini_api_key {
        Some(v) => v,
        None => pick_opt_str("GEMINI_API_KEY", "gemini_api_key", "gemini_api_key")?,
    };
    settings.gemini_model = match overrides.gemini_model {
        Some(v) => v,
        None => pick_str(
            "GPSIFY_GEMINI_MODEL",
            "gemini_model",
            "gemini_model",
            DEFAULT_GEMINI_MODEL,
        )?,
    };
    settings.reasoning_effort = match overrides.reasoning_effort {
        Some(v) => v,
        None => pick_str(
            "GPSIFY_REASONING",
            "reasoning_effort",
            "reasoning_effort",
            DEFAULT_REASONING,
        )?,
    };
    settings.timeout_s = match overrides.timeout_s {
        Some(v) => v,
        None => match lookup_alias("GPSIFY_TIMEOUT_S", "timeout_s", env, dotenv) {
            Some(raw) => raw.trim().parse::<f64>().map_err(|_| {
                ConfigError(format!("GPSIFY_TIMEOUT_S must be a number, got {raw:?}"))
            })?,
            None => toml_float(&toml_map, "timeout_s")?.unwrap_or(DEFAULT_TIMEOUT_S),
        },
    };
    settings.nominatim_contact = match overrides.nominatim_contact {
        Some(v) => v,
        None => pick_opt_str(
            "NOMINATIM_CONTACT",
            "nominatim_contact",
            "nominatim_contact",
        )?,
    };
    settings.geocoder_url = match overrides.geocoder_url {
        Some(v) => v,
        None => pick_str(
            "GEOCODER_URL",
            "geocoder_url",
            "geocoder_url",
            DEFAULT_GEOCODER_URL,
        )?,
    };
    settings.geocoder = match overrides.geocoder {
        Some(v) => v,
        None => pick_str("GPSIFY_GEOCODER", "geocoder", "geocoder", DEFAULT_GEOCODER)?,
    };
    settings.map_tile_url = match overrides.map_tile_url {
        Some(v) => v,
        None => pick_str(
            "MAP_TILE_URL",
            "map_tile_url",
            "map_tile_url",
            DEFAULT_MAP_TILE_URL,
        )?,
    };
    settings.geocode_lang = match overrides.geocode_lang {
        Some(v) => v,
        None => pick_str(
            "GEOCODE_LANG",
            "geocode_lang",
            "geocode_lang",
            DEFAULT_GEOCODE_LANG,
        )?,
    };
    settings.data_dir = match overrides.data_dir {
        Some(v) => v,
        None => {
            if let Some(v) = lookup_alias("GPSIFY_DATA_DIR", "data_dir", env, dotenv) {
                PathBuf::from(v)
            } else if let Some(v) = toml_string(&toml_map, "data_dir")? {
                PathBuf::from(v)
            } else {
                home_gpsify_dir()
            }
        }
    };

    settings.validate()
}

pub fn live_http_blocked() -> bool {
    std::env::var("GPSIFY_DRY_RUN").as_deref() == Ok("1")
}
