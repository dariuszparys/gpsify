//! SQLite geocode cache keyed by `normalize(place)|country|lang`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use unicode_normalization::UnicodeNormalization;

use crate::store::{utc_now_iso, StoreError};
use crate::types::GeoHit;

pub const CACHE_DB_NAME: &str = "geocode.sqlite";

pub const CACHE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS geocode_cache (
  cache_key TEXT PRIMARY KEY,
  query TEXT NOT NULL,
  hit_json TEXT,
  created_at TEXT NOT NULL
);
"#;

fn whitespace_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").expect("valid whitespace regex"))
}

fn is_unicode_punctuation(ch: char) -> bool {
    if ch.is_ascii_punctuation() {
        return true;
    }
    matches!(
        ch as u32,
        0x00A1
            | 0x00A7
            | 0x00AB
            | 0x00B6
            | 0x00B7
            | 0x00BB
            | 0x00BF
            | 0x2010..=0x2027
            | 0x2030..=0x205E
            | 0x2E00..=0x2E7F
            | 0x3001..=0x303F
            | 0xFE10..=0xFE1F
            | 0xFE30..=0xFE6F
            | 0xFF01..=0xFF0F
            | 0xFF1A..=0xFF20
            | 0xFF3B..=0xFF40
            | 0xFF5B..=0xFF65
    )
}

pub fn geocode_cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join(CACHE_DB_NAME)
}

/// NFKC, strip, lower, punctuation -> space except comma, collapse whitespace.
pub fn normalize_place(place: &str) -> String {
    let nfkc: String = place.nfkc().collect();
    let text = nfkc.trim().to_lowercase();
    let mut chars = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == ',' {
            chars.push(ch);
        } else if is_unicode_punctuation(ch) {
            chars.push(' ');
        } else {
            chars.push(ch);
        }
    }
    whitespace_re().replace_all(&chars, " ").trim().to_string()
}

pub fn cache_key(place: &str, country_code: Option<&str>, lang: &str) -> String {
    let cc = country_code.unwrap_or("").trim().to_lowercase();
    format!(
        "{}|{}|{}",
        normalize_place(place),
        cc,
        lang.trim().to_lowercase()
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct CacheLookup {
    pub found: bool,
    pub hit: Option<GeoHit>,
}

#[derive(Clone)]
pub struct GeocodeCache {
    pub path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl GeocodeCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(CACHE_SCHEMA)?;
        Ok(Self {
            path,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn get(&self, key: &str) -> Result<CacheLookup, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("geocode cache mutex poisoned".into()))?;
        let raw: Option<Option<String>> = guard
            .query_row(
                "SELECT hit_json FROM geocode_cache WHERE cache_key = ?",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        let Some(hit_opt) = raw else {
            return Ok(CacheLookup {
                found: false,
                hit: None,
            });
        };
        let Some(hit_json) = hit_opt else {
            return Ok(CacheLookup {
                found: true,
                hit: None,
            });
        };
        let hit: GeoHit = serde_json::from_str(&hit_json)
            .map_err(|e| StoreError::Other(format!("corrupt geocode cache entry: {e}")))?;
        Ok(CacheLookup {
            found: true,
            hit: Some(hit),
        })
    }

    pub fn put(&self, key: &str, hit: Option<&GeoHit>, query: &str) -> Result<(), StoreError> {
        let payload = match hit {
            Some(h) => Some(
                serde_json::to_string(h)
                    .map_err(|e| StoreError::Other(format!("serialize GeoHit failed: {e}")))?,
            ),
            None => None,
        };
        let stamp = utc_now_iso();
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Other("geocode cache mutex poisoned".into()))?;
        guard.execute(
            "INSERT OR REPLACE INTO geocode_cache (cache_key, query, hit_json, created_at)
             VALUES (?, ?, ?, ?)",
            params![key, query, payload, stamp],
        )?;
        Ok(())
    }
}
