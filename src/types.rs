//! Shared contracts for vision, geocoding, jobs, and decisions.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const ALLOWED_REASONING_EFFORT: &[&str] = &["low", "medium", "high"];
pub const ALLOWED_VISION_PROVIDERS: &[&str] = &["xai", "gemini"];
pub const MIN_VISION_TIMEOUT_S: f64 = 120.0;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct TypeError(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Exact,
    Street,
    Neighborhood,
    City,
    Region,
    Country,
    Unknown,
}

impl Granularity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Street => "street",
            Self::Neighborhood => "neighborhood",
            Self::City => "city",
            Self::Region => "region",
            Self::Country => "country",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Granularity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Granularity {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "exact" => Ok(Self::Exact),
            "street" => Ok(Self::Street),
            "neighborhood" => Ok(Self::Neighborhood),
            "city" => Ok(Self::City),
            "region" => Ok(Self::Region),
            "country" => Ok(Self::Country),
            "unknown" => Ok(Self::Unknown),
            _ => Err(TypeError(format!("unknown granularity {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    None,
    InsufficientEvidence,
    IndoorGeneric,
    PrivacySensitive,
    PolicyRefusal,
    NotAPhotograph,
}

impl RefusalReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::IndoorGeneric => "indoor_generic",
            Self::PrivacySensitive => "privacy_sensitive",
            Self::PolicyRefusal => "policy_refusal",
            Self::NotAPhotograph => "not_a_photograph",
        }
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RefusalReason {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "insufficient_evidence" => Ok(Self::InsufficientEvidence),
            "indoor_generic" => Ok(Self::IndoorGeneric),
            "privacy_sensitive" => Ok(Self::PrivacySensitive),
            "policy_refusal" => Ok(Self::PolicyRefusal),
            "not_a_photograph" => Ok(Self::NotAPhotograph),
            _ => Err(TypeError(format!("unknown refusal reason {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinSource {
    Geocoder,
    User,
    None,
}

impl PinSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Geocoder => "geocoder",
            Self::User => "user",
            Self::None => "none",
        }
    }
}

impl fmt::Display for PinSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PinSource {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "geocoder" => Ok(Self::Geocoder),
            "user" => Ok(Self::User),
            "none" => Ok(Self::None),
            _ => Err(TypeError(format!("unknown pin_source {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAction {
    Accept,
    AcceptEdited,
    Skip,
    ClusterApply,
}

impl DecisionAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::AcceptEdited => "accept_edited",
            Self::Skip => "skip",
            Self::ClusterApply => "cluster_apply",
        }
    }

    pub const fn is_writeable(self) -> bool {
        matches!(self, Self::Accept | Self::AcceptEdited | Self::ClusterApply)
    }
}

impl fmt::Display for DecisionAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DecisionAction {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "accept" => Ok(Self::Accept),
            "accept_edited" => Ok(Self::AcceptEdited),
            "skip" => Ok(Self::Skip),
            "cluster_apply" => Ok(Self::ClusterApply),
            _ => Err(TypeError(format!("unknown action {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteReceiptStatus {
    Pending,
    Ok,
    Failed,
    Undone,
}

impl WriteReceiptStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Undone => "undone",
        }
    }
}

impl fmt::Display for WriteReceiptStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WriteReceiptStatus {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "ok" => Ok(Self::Ok),
            "failed" => Ok(Self::Failed),
            "undone" => Ok(Self::Undone),
            _ => Err(TypeError(format!("unknown write_receipt status {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhotoStatus {
    Discovered,
    SkippedHasGps,
    SkippedUnsupported,
    SkippedUnreadable,
    SkippedNoPreview,
    Pending,
    Proposing,
    Proposed,
    Refused,
    Error,
    Accepted,
    AcceptedEdited,
    SkippedByUser,
    Written,
    WriteFailed,
    Undone,
    Missing,
}

impl PhotoStatus {
    pub const ALL: &'static [PhotoStatus] = &[
        Self::Discovered,
        Self::SkippedHasGps,
        Self::SkippedUnsupported,
        Self::SkippedUnreadable,
        Self::SkippedNoPreview,
        Self::Pending,
        Self::Proposing,
        Self::Proposed,
        Self::Refused,
        Self::Error,
        Self::Accepted,
        Self::AcceptedEdited,
        Self::SkippedByUser,
        Self::Written,
        Self::WriteFailed,
        Self::Undone,
        Self::Missing,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::SkippedHasGps => "skipped_has_gps",
            Self::SkippedUnsupported => "skipped_unsupported",
            Self::SkippedUnreadable => "skipped_unreadable",
            Self::SkippedNoPreview => "skipped_no_preview",
            Self::Pending => "pending",
            Self::Proposing => "proposing",
            Self::Proposed => "proposed",
            Self::Refused => "refused",
            Self::Error => "error",
            Self::Accepted => "accepted",
            Self::AcceptedEdited => "accepted_edited",
            Self::SkippedByUser => "skipped_by_user",
            Self::Written => "written",
            Self::WriteFailed => "write_failed",
            Self::Undone => "undone",
            Self::Missing => "missing",
        }
    }
}

impl fmt::Display for PhotoStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PhotoStatus {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "discovered" => Ok(Self::Discovered),
            "skipped_has_gps" => Ok(Self::SkippedHasGps),
            "skipped_unsupported" => Ok(Self::SkippedUnsupported),
            "skipped_unreadable" => Ok(Self::SkippedUnreadable),
            "skipped_no_preview" => Ok(Self::SkippedNoPreview),
            "pending" => Ok(Self::Pending),
            "proposing" => Ok(Self::Proposing),
            "proposed" => Ok(Self::Proposed),
            "refused" => Ok(Self::Refused),
            "error" => Ok(Self::Error),
            "accepted" => Ok(Self::Accepted),
            "accepted_edited" => Ok(Self::AcceptedEdited),
            "skipped_by_user" => Ok(Self::SkippedByUser),
            "written" => Ok(Self::Written),
            "write_failed" => Ok(Self::WriteFailed),
            "undone" => Ok(Self::Undone),
            "missing" => Ok(Self::Missing),
            _ => Err(TypeError(format!("unknown photo status {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeocoderName {
    Nominatim,
    Photon,
    Mapbox,
    Google,
}

impl GeocoderName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nominatim => "nominatim",
            Self::Photon => "photon",
            Self::Mapbox => "mapbox",
            Self::Google => "google",
        }
    }
}

impl fmt::Display for GeocoderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GeocoderName {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "nominatim" => Ok(Self::Nominatim),
            "photon" => Ok(Self::Photon),
            "mapbox" => Ok(Self::Mapbox),
            "google" => Ok(Self::Google),
            _ => Err(TypeError(format!("unknown geocoder {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(TypeError(format!(
                "reasoning_effort must be one of high, low, medium; got {s:?} (xhigh is out of v1)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisionProviderName {
    Xai,
    Gemini,
}

impl VisionProviderName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Xai => "xai",
            Self::Gemini => "gemini",
        }
    }
}

impl fmt::Display for VisionProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VisionProviderName {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "xai" => Ok(Self::Xai),
            "gemini" => Ok(Self::Gemini),
            _ => Err(TypeError(format!(
                "vision_provider must be one of gemini, xai; got {s:?}"
            ))),
        }
    }
}

fn validate_unit_float(value: f64, field: &str) -> Result<(), TypeError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(TypeError(format!(
            "{field} must be between 0 and 1; got {value}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Clue {
    #[schemars(
        description = "sign|language|architecture|vegetation|road|vehicle|text|skyline|other"
    )]
    pub kind: String,
    pub text: String,
    #[schemars(range(min = 0.0, max = 1.0))]
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Candidate {
    pub place_name: String,
    #[serde(default)]
    #[schemars(description = "ISO 3166-1 alpha-2")]
    pub country_code: Option<String>,
    #[serde(default)]
    pub admin1: Option<String>,
    #[serde(default)]
    pub lat_hint: Option<f64>,
    #[serde(default)]
    pub lng_hint: Option<f64>,
    #[schemars(range(min = 0.0, max = 1.0))]
    pub confidence: f64,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VisionGuess {
    pub refusal: RefusalReason,
    #[serde(default)]
    pub refusal_detail: Option<String>,
    #[serde(default)]
    pub primary_place_name: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub admin1: Option<String>,
    pub granularity: Granularity,
    #[serde(default)]
    pub lat_hint: Option<f64>,
    #[serde(default)]
    pub lng_hint: Option<f64>,
    #[schemars(range(min = 0.0, max = 1.0))]
    pub model_confidence: f64,
    pub clues: Vec<Clue>,
    pub candidates: Vec<Candidate>,
    pub used_album_hint: bool,
    pub used_filename_hint: bool,
}

impl VisionGuess {
    pub fn validate(&self) -> Result<(), TypeError> {
        validate_unit_float(self.model_confidence, "model_confidence")?;
        for (i, clue) in self.clues.iter().enumerate() {
            validate_unit_float(clue.weight, &format!("clues[{i}].weight"))?;
        }
        for (i, cand) in self.candidates.iter().enumerate() {
            validate_unit_float(cand.confidence, &format!("candidates[{i}].confidence"))?;
            if let Some(lat) = cand.lat_hint {
                if !lat.is_finite() {
                    return Err(TypeError(format!(
                        "candidates[{i}].lat_hint must be finite"
                    )));
                }
            }
            if let Some(lng) = cand.lng_hint {
                if !lng.is_finite() {
                    return Err(TypeError(format!(
                        "candidates[{i}].lng_hint must be finite"
                    )));
                }
            }
        }
        if let Some(lat) = self.lat_hint {
            if !lat.is_finite() {
                return Err(TypeError("lat_hint must be finite".into()));
            }
        }
        if let Some(lng) = self.lng_hint {
            if !lng.is_finite() {
                return Err(TypeError("lng_hint must be finite".into()));
            }
        }
        Ok(())
    }

    pub fn from_json_str(raw: &str) -> Result<Self, TypeError> {
        let guess: Self = serde_json::from_str(raw)
            .map_err(|e| TypeError(format!("invalid VisionGuess: {e}")))?;
        guess.validate()?;
        Ok(guess)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoHit {
    pub query: String,
    pub display_name: String,
    pub lat: f64,
    pub lng: f64,
    #[serde(default)]
    pub boundingbox: Option<[f64; 4]>,
    #[serde(default)]
    pub osm_type: Option<String>,
    #[serde(default)]
    pub osm_id: Option<String>,
    #[serde(default)]
    pub importance: Option<f64>,
    pub provider: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedSibling {
    pub filename: String,
    #[serde(default)]
    pub captured_at: Option<String>,
    pub place_name: String,
    #[serde(default)]
    pub country_code: Option<String>,
    pub granularity: Granularity,
    pub model_confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedSibling {
    pub filename: String,
    pub lat: f64,
    pub lng: f64,
}

/// Text assembled around one vision call. Never includes a full home path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptContext {
    #[serde(default)]
    pub hint: Option<String>,
    pub folder_basename: String,
    pub filename: String,
    #[serde(default)]
    pub datetime_original: Option<String>,
    #[serde(default)]
    pub offset: Option<String>,
    #[serde(default)]
    pub make: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub proposed_siblings: Vec<ProposedSibling>,
    #[serde(default)]
    pub gps_siblings: Vec<TaggedSibling>,
    #[serde(default)]
    pub no_context: bool,
}

/// Client flags for a single vision call (K13).
///
/// `reasoning_effort` is mandatory and `store` must stay `false` so image calls
/// are never retained.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VisionCallOptions {
    store: bool,
    pub reasoning_effort: ReasoningEffort,
    pub timeout_s: f64,
}

impl VisionCallOptions {
    pub fn new(reasoning_effort: &str, timeout_s: f64) -> Result<Self, TypeError> {
        Self::with_store(false, reasoning_effort, timeout_s)
    }

    pub fn with_store(
        store: bool,
        reasoning_effort: &str,
        timeout_s: f64,
    ) -> Result<Self, TypeError> {
        let effort = ReasoningEffort::from_str(reasoning_effort)?;
        if !timeout_s.is_finite() || timeout_s < MIN_VISION_TIMEOUT_S {
            return Err(TypeError(format!(
                "timeout_s must be >= {MIN_VISION_TIMEOUT_S}"
            )));
        }
        if store {
            return Err(TypeError(
                "store must be False on every vision call".to_string(),
            ));
        }
        Ok(Self {
            store,
            reasoning_effort: effort,
            timeout_s,
        })
    }

    pub fn store(&self) -> bool {
        self.store
    }

    pub fn assert_safe(&self) -> Result<(), TypeError> {
        if self.store {
            return Err(TypeError(
                "store must be False on every vision call".to_string(),
            ));
        }
        if !self.timeout_s.is_finite() || self.timeout_s < MIN_VISION_TIMEOUT_S {
            return Err(TypeError(format!(
                "timeout_s must be >= {MIN_VISION_TIMEOUT_S}"
            )));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct RawVisionCallOptions {
    #[serde(default)]
    store: bool,
    reasoning_effort: String,
    timeout_s: f64,
}

impl<'de> Deserialize<'de> for VisionCallOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawVisionCallOptions::deserialize(deserializer)?;
        Self::with_store(raw.store, &raw.reasoning_effort, raw.timeout_s)
            .map_err(serde::de::Error::custom)
    }
}
