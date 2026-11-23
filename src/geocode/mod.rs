//! Geocoder trait, fake provider, OSM attribution, and lookup orchestration (K10).

pub mod cache;
pub mod nominatim;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use regex::Regex;
use thiserror::Error;
use url::Url;

use crate::config::Settings;
use crate::types::{GeoHit, Granularity};

pub use cache::{cache_key, geocode_cache_path, normalize_place, CacheLookup, GeocodeCache};
pub use nominatim::{
    nominatim_user_agent, parse_nominatim_hits, AsyncRateLimiter, NominatimGeocoder,
};

pub const DEFAULT_UNCACHED_MAX: usize = 50;
pub const OSM_ATTRIBUTION: &str = "© OpenStreetMap contributors";
pub const PUBLIC_NOMINATIM_HOSTS: &[&str] = &["nominatim.openstreetmap.org", "nominatim.osm.org"];
pub const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost", "::1", "[::1]"];
pub const KEYED_GEOCODERS: &[&str] = &["mapbox", "google"];

pub const PUBLIC_NOMINATIM_CAP_WARNING: &str =
    "Public Nominatim uncached lookup cap reached; remaining pins left at pin_source=none. Set geocoder_url to Photon / self-hosted Nominatim / Mapbox or re-run with --geocode-uncached-max N.";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct GeocodeError(pub String);

#[derive(Debug, Error)]
#[error("{0}")]
pub struct GeocodeConfigError(pub String);

#[async_trait]
pub trait Geocoder: Send + Sync {
    fn name(&self) -> &str;

    async fn search(
        &self,
        query: &str,
        country_code: Option<&str>,
        lang: &str,
    ) -> Result<Vec<GeoHit>, GeocodeError>;
}

fn house_address_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\d+\s+\S.*\b(?:street|st\.?|avenue|ave\.?|road|rd\.?|boulevard|blvd\.?|lane|ln\.?|drive|dr\.?|court|ct\.?|place|pl\.?|way|terrace|circle|close|row|highway|hwy\.?)\b",
        )
        .expect("valid house address regex")
    })
}

fn iso_english_name(code: &str) -> Option<&'static str> {
    match code {
        "AT" => Some("austria"),
        "AU" => Some("australia"),
        "BE" => Some("belgium"),
        "BR" => Some("brazil"),
        "CA" => Some("canada"),
        "CH" => Some("switzerland"),
        "CZ" => Some("czechia"),
        "DE" => Some("germany"),
        "DK" => Some("denmark"),
        "ES" => Some("spain"),
        "FI" => Some("finland"),
        "FR" => Some("france"),
        "GB" => Some("united kingdom"),
        "GR" => Some("greece"),
        "IE" => Some("ireland"),
        "IN" => Some("india"),
        "IT" => Some("italy"),
        "JP" => Some("japan"),
        "MX" => Some("mexico"),
        "NL" => Some("netherlands"),
        "NO" => Some("norway"),
        "NZ" => Some("new zealand"),
        "PL" => Some("poland"),
        "PT" => Some("portugal"),
        "SE" => Some("sweden"),
        "US" => Some("united states"),
        _ => None,
    }
}

pub fn url_hostname(raw_url: &str) -> String {
    Url::parse(raw_url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_ascii_lowercase()))
        .unwrap_or_default()
}

pub fn is_public_nominatim_url(url: &str) -> bool {
    let host = url_hostname(url);
    PUBLIC_NOMINATIM_HOSTS.contains(&host.as_str())
}

pub fn is_loopback_url(url: &str) -> bool {
    let host = url_hostname(url);
    LOOPBACK_HOSTS.contains(&host.as_str())
}

pub fn public_nominatim_requires_contact(settings: &Settings) -> bool {
    if KEYED_GEOCODERS.contains(&settings.geocoder.as_str()) {
        return false;
    }
    is_public_nominatim_url(&settings.geocoder_url)
}

pub fn require_geocoder_ready(settings: &Settings) -> Result<(), GeocodeConfigError> {
    if !public_nominatim_requires_contact(settings) {
        return Ok(());
    }
    if settings
        .nominatim_contact
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(GeocodeConfigError(
            "nominatim_contact is required for public Nominatim (set NOMINATIM_CONTACT or nominatim_contact in config.toml)"
                .to_string(),
        ));
    }
    Ok(())
}

/// True for strings that look like a house number + street (never send to Nominatim).
pub fn looks_like_house_address(place: &str) -> bool {
    let stripped = place.trim();
    if stripped.is_empty() {
        return false;
    }
    house_address_re().is_match(stripped)
}

fn country_matches(hit: &GeoHit, country_code: &str) -> bool {
    let code = country_code.trim().to_ascii_lowercase();
    if code.is_empty() {
        return false;
    }
    let name = hit.display_name.to_ascii_lowercase();
    let pat = format!(r"\b{}\b", regex::escape(&code));
    if let Ok(re) = Regex::new(&pat) {
        if re.is_match(&name) {
            return true;
        }
    }
    if let Some(english) = iso_english_name(&code.to_ascii_uppercase()) {
        return name.contains(english);
    }
    false
}

fn osm_type_rank(osm_type: Option<&str>, granularity: Granularity) -> usize {
    let kind = osm_type.unwrap_or("").to_ascii_lowercase();
    let order: [&str; 3] = match granularity {
        Granularity::Exact => ["node", "way", "relation"],
        Granularity::Street
        | Granularity::Neighborhood
        | Granularity::City
        | Granularity::Region
        | Granularity::Country
        | Granularity::Unknown => ["relation", "way", "node"],
    };
    order.iter().position(|&x| x == kind).unwrap_or(order.len())
}

pub fn pick_best_hit(
    hits: &[GeoHit],
    country_code: Option<&str>,
    granularity: Granularity,
) -> Option<GeoHit> {
    if hits.is_empty() {
        return None;
    }
    let mut indexed: Vec<(usize, &GeoHit)> = hits.iter().enumerate().collect();
    if let Some(cc) = country_code {
        if !cc.trim().is_empty() {
            let matched: Vec<(usize, &GeoHit)> = indexed
                .iter()
                .copied()
                .filter(|&(_, h)| country_matches(h, cc))
                .collect();
            if !matched.is_empty() {
                indexed = matched;
            }
        }
    }
    indexed.sort_by_key(|&(idx, hit)| (osm_type_rank(hit.osm_type.as_deref(), granularity), idx));
    Some(indexed[0].1.clone())
}

#[derive(Default)]
struct FakeState {
    hits: BTreeMap<String, Vec<GeoHit>>,
    queries: Vec<String>,
    error_on: BTreeSet<String>,
}

/// In-memory geocoder for offline tests. Never opens a socket.
#[derive(Clone, Default)]
pub struct FakeGeocoder {
    inner: Arc<Mutex<FakeState>>,
}

impl FakeGeocoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, query: &str, hits: Vec<GeoHit>) {
        self.inner
            .lock()
            .unwrap()
            .hits
            .insert(query.to_string(), hits);
    }

    pub fn set_error_on(&self, query: &str) {
        self.inner
            .lock()
            .unwrap()
            .error_on
            .insert(query.to_string());
    }

    pub fn queries(&self) -> Vec<String> {
        self.inner.lock().unwrap().queries.clone()
    }
}

#[async_trait]
impl Geocoder for FakeGeocoder {
    fn name(&self) -> &str {
        "fake"
    }

    async fn search(
        &self,
        query: &str,
        _country_code: Option<&str>,
        _lang: &str,
    ) -> Result<Vec<GeoHit>, GeocodeError> {
        let mut guard = self.inner.lock().unwrap();
        guard.queries.push(query.to_string());
        if guard.error_on.contains(query) {
            return Err(GeocodeError(format!("fake geocoder error for {query:?}")));
        }
        Ok(guard.hits.get(query).cloned().unwrap_or_default())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LookupResult {
    pub hit: Option<GeoHit>,
    pub cache_hit: bool,
    pub capped: bool,
    pub skipped: Option<String>,
}

struct ServiceCounters {
    uncached_lookups: usize,
    cache_hits: usize,
    cap_stopped: bool,
}

#[derive(Clone)]
pub struct GeocodeService {
    geocoder: Arc<dyn Geocoder>,
    cache: GeocodeCache,
    lang: String,
    uncached_max: usize,
    cap_uncached: bool,
    counters: Arc<Mutex<ServiceCounters>>,
}

impl GeocodeService {
    pub fn new(
        geocoder: Arc<dyn Geocoder>,
        cache: GeocodeCache,
        lang: impl Into<String>,
        uncached_max: usize,
        cap_uncached: bool,
    ) -> Self {
        Self {
            geocoder,
            cache,
            lang: lang.into(),
            uncached_max,
            cap_uncached,
            counters: Arc::new(Mutex::new(ServiceCounters {
                uncached_lookups: 0,
                cache_hits: 0,
                cap_stopped: false,
            })),
        }
    }

    pub fn uncached_lookups(&self) -> usize {
        self.counters.lock().unwrap().uncached_lookups
    }

    pub fn cache_hits(&self) -> usize {
        self.counters.lock().unwrap().cache_hits
    }

    pub fn cap_stopped(&self) -> bool {
        self.counters.lock().unwrap().cap_stopped
    }

    pub fn cache(&self) -> &GeocodeCache {
        &self.cache
    }

    pub fn lang(&self) -> &str {
        &self.lang
    }

    pub async fn lookup_primary(
        &self,
        place: &str,
        country_code: Option<&str>,
        granularity: Granularity,
    ) -> LookupResult {
        if looks_like_house_address(place) {
            return LookupResult {
                hit: None,
                cache_hit: false,
                capped: false,
                skipped: Some("house_address".to_string()),
            };
        }

        let key = cache_key(place, country_code, &self.lang);
        if let Ok(cached) = self.cache.get(&key) {
            if cached.found {
                self.counters.lock().unwrap().cache_hits += 1;
                return LookupResult {
                    hit: cached.hit,
                    cache_hit: true,
                    capped: false,
                    skipped: None,
                };
            }
        }

        {
            let mut guard = self.counters.lock().unwrap();
            if self.cap_uncached && guard.uncached_lookups >= self.uncached_max {
                guard.cap_stopped = true;
                return LookupResult {
                    hit: None,
                    cache_hit: false,
                    capped: true,
                    skipped: None,
                };
            }
            guard.uncached_lookups += 1;
        }

        let hits = match self.geocoder.search(place, country_code, &self.lang).await {
            Ok(h) => h,
            Err(_) => {
                return LookupResult {
                    hit: None,
                    cache_hit: false,
                    capped: false,
                    skipped: None,
                };
            }
        };

        let hit = pick_best_hit(&hits, country_code, granularity);
        let _ = self.cache.put(&key, hit.as_ref(), place);
        LookupResult {
            hit,
            cache_hit: false,
            capped: false,
            skipped: None,
        }
    }
}
