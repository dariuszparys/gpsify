//! Nominatim-shaped HTTP geocoder (configurable URL, identifiable UA, 1 rps limiter).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::config::live_http_blocked;
use crate::geocode::{is_public_nominatim_url, GeocodeConfigError, GeocodeError, Geocoder};
use crate::types::GeoHit;

pub const NOMINATIM_UA_PRODUCT: &str = "gpsify/0.1";

pub fn nominatim_user_agent(contact: Option<&str>) -> String {
    match contact {
        Some(c) if !c.trim().is_empty() => {
            format!("{NOMINATIM_UA_PRODUCT} (personal; {})", c.trim())
        }
        _ => format!("{NOMINATIM_UA_PRODUCT} (personal; self-hosted)"),
    }
}

#[derive(Clone)]
pub struct AsyncRateLimiter {
    min_interval: Duration,
    next_ok_at: Arc<Mutex<Option<Instant>>>,
}

impl AsyncRateLimiter {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            next_ok_at: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn acquire(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let mut guard = self.next_ok_at.lock().await;
        let now = Instant::now();
        if let Some(next_ok) = *guard {
            if next_ok > now {
                tokio::time::sleep_until(next_ok).await;
            }
        }
        let after = Instant::now();
        *guard = Some(after + self.min_interval);
    }
}

pub fn parse_nominatim_hits(payload: &Value, query: &str, provider: &str) -> Vec<GeoHit> {
    let Some(items) = payload.as_array() else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if let Some(hit) = parse_one(obj, query, provider) {
            hits.push(hit);
        }
    }
    hits
}

fn parse_float(val: Option<&Value>) -> Option<f64> {
    let v = val?;
    match v {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn parse_one(item: &serde_json::Map<String, Value>, query: &str, provider: &str) -> Option<GeoHit> {
    let lat = parse_float(item.get("lat"))?;
    let lng = parse_float(item.get("lon"))?;

    let boundingbox = item.get("boundingbox").and_then(|bb| {
        let arr = bb.as_array()?;
        if arr.len() != 4 {
            return None;
        }
        Some([
            parse_float(arr.first())?,
            parse_float(arr.get(1))?,
            parse_float(arr.get(2))?,
            parse_float(arr.get(3))?,
        ])
    });

    let osm_type = item.get("osm_type").and_then(|v| match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    });
    let osm_id = item.get("osm_id").and_then(|v| match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    });
    let importance = parse_float(item.get("importance"));
    let display_name = item
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(GeoHit {
        query: query.to_string(),
        display_name,
        lat,
        lng,
        boundingbox,
        osm_type,
        osm_id,
        importance,
        provider: provider.to_string(),
    })
}

pub struct NominatimGeocoder {
    url: String,
    provider: String,
    user_agent: String,
    timeout: Duration,
    rate: AsyncRateLimiter,
    client: Mutex<Option<Client>>,
    allow_live_override: Option<bool>,
}

impl NominatimGeocoder {
    pub fn new(
        url: impl Into<String>,
        contact: Option<&str>,
        provider: impl Into<String>,
    ) -> Result<Self, GeocodeConfigError> {
        let url_str = url.into();
        if is_public_nominatim_url(&url_str) && contact.map(str::trim).unwrap_or("").is_empty() {
            return Err(GeocodeConfigError(
                "nominatim_contact is required for public Nominatim".to_string(),
            ));
        }
        Ok(Self {
            url: url_str,
            provider: provider.into(),
            user_agent: nominatim_user_agent(contact),
            timeout: Duration::from_secs(30),
            rate: AsyncRateLimiter::new(Duration::from_secs(1)),
            client: Mutex::new(None),
            allow_live_override: None,
        })
    }

    pub fn with_rate_limiter(mut self, rate: AsyncRateLimiter) -> Self {
        self.rate = rate;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_live_override(mut self, allow_live: bool) -> Self {
        self.allow_live_override = Some(allow_live);
        self
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    async fn get_client(&self) -> Result<Client, GeocodeError> {
        let mut guard = self.client.lock().await;
        if let Some(ref client) = *guard {
            return Ok(client.clone());
        }
        let blocked = match self.allow_live_override {
            Some(allow) => !allow,
            None => live_http_blocked(),
        };
        if blocked {
            return Err(GeocodeError(
                "GPSIFY_DRY_RUN=1 blocks live geocoder HTTP".to_string(),
            ));
        }
        let client = Client::builder()
            .timeout(self.timeout)
            .user_agent(&self.user_agent)
            .build()
            .map_err(|e| GeocodeError(format!("failed to build HTTP client: {e}")))?;
        *guard = Some(client.clone());
        Ok(client)
    }
}

#[async_trait]
impl Geocoder for NominatimGeocoder {
    fn name(&self) -> &str {
        "nominatim"
    }

    async fn search(
        &self,
        query: &str,
        country_code: Option<&str>,
        lang: &str,
    ) -> Result<Vec<GeoHit>, GeocodeError> {
        let client = self.get_client().await?;
        let mut params = vec![
            ("q", query.to_string()),
            ("format", "jsonv2".to_string()),
            ("limit", "5".to_string()),
            ("addressdetails", "1".to_string()),
            ("accept-language", lang.to_string()),
        ];
        if let Some(cc) = country_code {
            let clean = cc.trim().to_ascii_lowercase();
            if !clean.is_empty() {
                params.push(("countrycodes", clean));
            }
        }

        self.rate.acquire().await;
        let response = client
            .get(&self.url)
            .query(&params)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    GeocodeError("geocoder timeout".to_string())
                } else {
                    GeocodeError(format!("geocoder HTTP error: {e}"))
                }
            })?;

        if !response.status().is_success() {
            return Err(GeocodeError(format!(
                "geocoder HTTP {}",
                response.status().as_u16()
            )));
        }

        let payload: Value = response
            .json()
            .await
            .map_err(|_| GeocodeError("geocoder returned non-JSON".to_string()))?;

        Ok(parse_nominatim_hits(&payload, query, &self.provider))
    }
}
