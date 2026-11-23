//! xAI grok-4.6 vision wrapper.
//!
//! Builds an OpenAI-compatible Chat Completions request at `https://api.x.ai/v1`.
//! Every image call MUST set `store=false` / `store_messages=false` and an
//! explicit `reasoning_effort` (K13). `GPSIFY_DRY_RUN=1` blocks live HTTP.

use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use reqwest::Client;
use serde_json::{json, Value};

use crate::config::live_http_blocked;
use crate::providers::{assert_safe_vision_options, json_u64, VisionError, VisionProvider};
use crate::types::{PromptContext, Usage, VisionCallOptions, VisionGuess};

pub const PROMPT_VERSION: &str = "PROMPT_V1";
pub const XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const XAI_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub const PROMPT_V1: &str = "\
You are geolocating a travel/landmark photograph for the owner of the photo.

Look at visible evidence only: signs, language/script, architecture, vegetation, road markings, vehicles, skyline, weather/season consistency with the stated capture time.

Return a specific named place you would type into a map search box (e.g. \"Clérigos Church, Porto, Portugal\"), not a prose paragraph. Prefer a landmark or city name. Do not return a street address with a house number.

Give 1–3 candidates when uncertain. Prefer a correct city over a wrong street.

If this appears to be a private home, hotel room, generic indoor party, or a close-up with no geographic signal, set refusal accordingly. Do not invent a neighborhood.

Do not identify private individuals or infer a home address from a window, doorbell, or street number on a house.

lat_hint/lng_hint are optional approximations; the place name is the source of truth.

If an album hint or filename is provided, treat it as a prior, not as proof. Say whether you used it.
";

/// Assemble the user message text. Never includes a full home path.
pub fn format_user_message(context: &PromptContext) -> String {
    if context.no_context {
        return "Task: infer where this photo was taken.".to_string();
    }
    let hint = context.hint.as_deref().unwrap_or("");
    let captured = [
        context.datetime_original.as_deref().unwrap_or(""),
        context.offset.as_deref().unwrap_or(""),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    let camera = [
        context.make.as_deref().unwrap_or(""),
        context.model.as_deref().unwrap_or(""),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ");

    let mut lines = vec![
        format!("Album hint (user-provided, optional): {hint}"),
        format!("Folder name: {}", context.folder_basename),
        format!("Filename: {}", context.filename),
        format!("Capture time (EXIF, may be wrong TZ): {captured}"),
        format!("Camera: {camera}"),
        "Cluster siblings already proposed (max 8):".to_string(),
    ];

    if context.proposed_siblings.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for sibling in context.proposed_siblings.iter().take(8) {
            let ts = sibling.captured_at.as_deref().unwrap_or("");
            let cc = sibling.country_code.as_deref().unwrap_or("");
            lines.push(format!(
                "  - {} {} → {}, {} ({}, {})",
                ts,
                sibling.filename,
                sibling.place_name,
                cc,
                sibling.granularity.as_str(),
                sibling.model_confidence,
            ));
        }
    }

    lines.push("Already-tagged siblings in this cluster (max 4):".to_string());
    if context.gps_siblings.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for sibling in context.gps_siblings.iter().take(4) {
            lines.push(format!(
                "  - {} @ {:.4}, {:.4}",
                sibling.filename, sibling.lat, sibling.lng
            ));
        }
    }
    lines.push("Task: infer where this photo was taken.".to_string());
    lines.join("\n")
}

pub fn jpeg_data_url(image_jpeg: &[u8]) -> String {
    let encoded = BASE64.encode(image_jpeg);
    format!("data:image/jpeg;base64,{encoded}")
}

/// JSON Schema for `VisionGuess` passed to OpenAI-compatible strict structured outputs.
pub fn vision_guess_json_schema() -> Value {
    let root = schemars::schema_for!(VisionGuess);
    serde_json::to_value(root).expect("VisionGuess JSON schema serializes")
}

/// Build the Chat Completions JSON request body without sending.
pub fn build_xai_request(
    image_jpeg: &[u8],
    context: &PromptContext,
    options: &VisionCallOptions,
    model: &str,
) -> Result<Value, VisionError> {
    assert_safe_vision_options(options)?;
    if image_jpeg.is_empty() {
        return Err(VisionError::Other(
            "refusing to send an empty image".to_string(),
        ));
    }
    if image_jpeg.len() > MAX_IMAGE_BYTES {
        return Err(VisionError::Other(
            "image exceeds 20 MiB; refusing to send".to_string(),
        ));
    }
    let payload = json!({
        "model": model,
        "store": false,
        "store_messages": false,
        "reasoning_effort": options.reasoning_effort.as_str(),
        "messages": [
            {"role": "system", "content": PROMPT_V1},
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": jpeg_data_url(image_jpeg),
                            "detail": "high",
                        }
                    },
                    {"type": "text", "text": format_user_message(context)}
                ]
            }
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "VisionGuess",
                "strict": true,
                "schema": vision_guess_json_schema(),
            }
        }
    });
    assert_safe_xai_payload(&payload)?;
    Ok(payload)
}

/// Last-chance K13 gate on an xAI Chat Completions payload.
pub fn assert_safe_xai_payload(payload: &Value) -> Result<(), VisionError> {
    if payload.get("store").and_then(Value::as_bool) != Some(false) {
        return Err(VisionError::Contract(
            "refusing to send: store must be False".to_string(),
        ));
    }
    if payload.get("store_messages").and_then(Value::as_bool) != Some(false) {
        return Err(VisionError::Contract(
            "refusing to send: store_messages must be False".to_string(),
        ));
    }
    let effort_ok = payload
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !effort_ok {
        return Err(VisionError::Contract(
            "refusing to send: reasoning_effort is required".to_string(),
        ));
    }
    Ok(())
}

pub fn parse_usage(payload: &Value) -> Usage {
    let Some(usage) = payload.get("usage").and_then(Value::as_object) else {
        return Usage::default();
    };
    let input_tokens = json_u64(
        usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens")),
    );
    let output_tokens = json_u64(
        usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens")),
    );
    let details = usage
        .get("output_tokens_details")
        .or_else(|| usage.get("completion_tokens_details"))
        .and_then(Value::as_object);
    let mut reasoning_tokens = details.map_or(0, |d| json_u64(d.get("reasoning_tokens")));
    if reasoning_tokens == 0 {
        reasoning_tokens = json_u64(usage.get("reasoning_tokens"));
    }
    Usage {
        input_tokens,
        output_tokens,
        reasoning_tokens,
    }
}

pub fn parse_vision_guess(payload: &Value) -> Result<VisionGuess, VisionError> {
    let choices = payload
        .get("choices")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
        .ok_or_else(|| VisionError::Parse("empty parse: no choices".to_string()))?;
    let message = choices[0]
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| VisionError::Parse("empty parse: no message".to_string()))?;
    if let Some(refusal) = message
        .get("refusal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Err(VisionError::Blocked(format!("content policy: {refusal}")));
    }
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| VisionError::Parse("empty parse: no content".to_string()))?;
    VisionGuess::from_json_str(content)
        .map_err(|_| VisionError::Parse("empty parse: content is not VisionGuess JSON".to_string()))
}

#[derive(Debug, Clone)]
pub struct XaiGrokProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: Option<Client>,
    live_override: Option<bool>,
}

impl XaiGrokProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self, VisionError> {
        let key = api_key.into();
        if key.trim().is_empty() {
            return Err(VisionError::Other("XAI_API_KEY is missing".to_string()));
        }
        Ok(Self {
            api_key: key,
            model: model.into(),
            base_url: XAI_BASE_URL.to_string(),
            client: None,
            live_override: None,
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    pub fn with_live_override(mut self, allow_live: bool) -> Self {
        self.live_override = Some(allow_live);
        self
    }

    fn is_live_blocked(&self) -> bool {
        match self.live_override {
            Some(allow_live) => !allow_live,
            None => live_http_blocked(),
        }
    }

    fn http_client(&self, timeout_s: f64) -> Result<Client, VisionError> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        Client::builder()
            .timeout(Duration::from_secs_f64(timeout_s))
            .build()
            .map_err(|e| VisionError::Other(format!("xAI HTTP client error: {e}")))
    }
}

#[async_trait]
impl VisionProvider for XaiGrokProvider {
    fn name(&self) -> &'static str {
        "xai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn guess(
        &self,
        image_jpeg: &[u8],
        context: &PromptContext,
        options: &VisionCallOptions,
    ) -> Result<(VisionGuess, Usage), VisionError> {
        assert_safe_vision_options(options)?;
        let payload = build_xai_request(image_jpeg, context, options, &self.model)?;
        if self.is_live_blocked() {
            return Err(VisionError::Other(
                "GPSIFY_DRY_RUN=1 blocks live xAI HTTP".to_string(),
            ));
        }
        let client = self.http_client(options.timeout_s)?;
        let url = format!("{}{}", self.base_url, XAI_CHAT_COMPLETIONS_PATH);
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs_f64(options.timeout_s))
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    VisionError::Retryable("xAI timeout".to_string())
                } else {
                    VisionError::Other(format!("xAI HTTP error: {err}"))
                }
            })?;

        let status = response.status().as_u16();
        if status == 429 || status >= 500 {
            return Err(VisionError::Retryable(format!("xAI HTTP {status}")));
        }
        if status != 200 {
            return Err(VisionError::Parse(format!("xAI HTTP {status}")));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| VisionError::Parse("xAI returned non-JSON".to_string()))?;
        if !body.is_object() {
            return Err(VisionError::Parse(
                "xAI returned non-object JSON".to_string(),
            ));
        }
        Ok((parse_vision_guess(&body)?, parse_usage(&body)))
    }
}
