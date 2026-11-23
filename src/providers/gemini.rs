//! Google Gemini vision wrapper: alternative to xAI.
//!
//! Builds a `generateContent` request against the Gemini Developer API.
//! Maps `reasoning_effort` explicitly to `thinkingConfig.thinkingBudget` (K13)
//! and sends the API key strictly in the `x-goog-api-key` header (never URL query).

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use reqwest::Client;
use serde_json::{json, Map, Value};

use crate::config::live_http_blocked;
use crate::providers::xai::{format_user_message, PROMPT_V1};
use crate::providers::{assert_safe_vision_options, json_u64, VisionError, VisionProvider};
use crate::types::{PromptContext, Usage, VisionCallOptions, VisionGuess};

pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const GEMINI_DEFAULT_MODEL: &str = "gemini-2.5-flash";
pub const MAX_INLINE_IMAGE_BYTES: usize = 14 * 1024 * 1024;

const DEFAULT_THINKING_RANGE: (u64, u64) = (128, 24576);

const SCHEMA_KEYS: &[&str] = &[
    "type",
    "format",
    "description",
    "nullable",
    "enum",
    "items",
    "properties",
    "required",
    "propertyOrdering",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
];

const BLOCKED_FINISH_REASONS: &[&str] = &[
    "SAFETY",
    "RECITATION",
    "PROHIBITED_CONTENT",
    "BLOCKLIST",
    "SPII",
];

fn thinking_budget_range(model: &str) -> (u64, u64) {
    match model.trim() {
        "gemini-2.5-pro" => (128, 32768),
        "gemini-2.5-flash" => (128, 24576),
        "gemini-2.5-flash-lite" => (512, 24576),
        _ => DEFAULT_THINKING_RANGE,
    }
}

/// Map `reasoning_effort` (`low | medium | high`) onto a model-clamped Gemini thinking budget.
pub fn thinking_budget(reasoning_effort: &str, model: &str) -> Result<u64, VisionError> {
    let raw = match reasoning_effort.trim().to_ascii_lowercase().as_str() {
        "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        _ => {
            return Err(VisionError::Contract(format!(
                "reasoning_effort must be one of [high, low, medium]; got {reasoning_effort:?}"
            )));
        }
    };
    let (low, high) = thinking_budget_range(model);
    Ok(raw.clamp(low, high))
}

/// Convert JSON Schema (Pydantic `$defs` or `schemars 0.8` `definitions`) into Gemini's
/// OpenAPI 3.0 `responseSchema` subset.
pub fn to_gemini_schema(schema: &Value) -> Result<Value, VisionError> {
    let mut defs = Map::new();
    for key in ["$defs", "definitions"] {
        if let Some(obj) = schema.get(key).and_then(Value::as_object) {
            for (k, v) in obj {
                defs.insert(k.clone(), v.clone());
            }
        }
    }
    let converted = convert_node(schema, &defs, &[])?;
    Ok(Value::Object(converted))
}

fn convert_node(
    node: &Value,
    defs: &Map<String, Value>,
    seen: &[String],
) -> Result<Map<String, Value>, VisionError> {
    let obj = node
        .as_object()
        .ok_or_else(|| VisionError::Contract("schema node must be an object".to_string()))?;

    if let Some(ref_str) = obj.get("$ref").and_then(Value::as_str) {
        let name = ref_str.rsplit('/').next().unwrap_or("");
        if seen.iter().any(|s| s == name) {
            return Err(VisionError::Contract(format!(
                "recursive $ref in VisionGuess schema: {ref_str}"
            )));
        }
        let target = defs.get(name).ok_or_else(|| {
            VisionError::Contract(format!(
                "unresolvable $ref in VisionGuess schema: {ref_str}"
            ))
        })?;
        let mut next_seen = seen.to_vec();
        next_seen.push(name.to_string());
        let mut merged = convert_node(target, defs, &next_seen)?;
        if let Some(desc) = obj.get("description").and_then(Value::as_str) {
            merged
                .entry("description".to_string())
                .or_insert_with(|| Value::String(desc.to_string()));
        }
        if obj.get("nullable").and_then(Value::as_bool) == Some(true) {
            merged.insert("nullable".to_string(), Value::Bool(true));
        }
        return Ok(merged);
    }

    // Handle schemars `allOf` wrapper around a single `$ref` (emitted when attributes/docs accompany an enum/struct field).
    if let Some(all_of) = obj.get("allOf").and_then(Value::as_array) {
        let mut merged = Map::new();
        for sub in all_of {
            let sub_map = convert_node(sub, defs, seen)?;
            for (k, v) in sub_map {
                merged.insert(k, v);
            }
        }
        for (k, v) in obj {
            if k != "allOf" && SCHEMA_KEYS.contains(&k.as_str()) {
                merged.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        return Ok(merged);
    }

    // Collapse `anyOf` / `oneOf` null-unions into `nullable: true`.
    for union_key in ["anyOf", "oneOf"] {
        if let Some(variants) = obj.get(union_key).and_then(Value::as_array) {
            let mut non_null = Vec::new();
            let mut nullable = false;
            for variant in variants {
                if variant.get("type").and_then(Value::as_str) == Some("null") {
                    nullable = true;
                } else {
                    non_null.push(variant);
                }
            }
            let mut converted = if non_null.len() == 1 {
                convert_node(non_null[0], defs, seen)?
            } else {
                let mut list = Vec::with_capacity(non_null.len());
                for variant in non_null {
                    list.push(Value::Object(convert_node(variant, defs, seen)?));
                }
                let mut m = Map::new();
                m.insert("anyOf".to_string(), Value::Array(list));
                m
            };
            if nullable {
                converted.insert("nullable".to_string(), Value::Bool(true));
            }
            if let Some(desc) = obj.get("description").and_then(Value::as_str) {
                converted
                    .entry("description".to_string())
                    .or_insert_with(|| Value::String(desc.to_string()));
            }
            return Ok(converted);
        }
    }

    let allowed: HashSet<&str> = SCHEMA_KEYS.iter().copied().collect();
    let mut out = Map::new();

    for (key, value) in obj {
        if !allowed.contains(key.as_str()) {
            continue;
        }
        if key == "type" {
            if let Some(arr) = value.as_array() {
                let mut non_null_types = Vec::new();
                let mut has_null = false;
                for item in arr {
                    if let Some(t) = item.as_str() {
                        if t == "null" {
                            has_null = true;
                        } else {
                            non_null_types.push(t);
                        }
                    }
                }
                if let Some(first_type) = non_null_types.first() {
                    out.insert("type".to_string(), Value::String((*first_type).to_string()));
                }
                if has_null {
                    out.insert("nullable".to_string(), Value::Bool(true));
                }
            } else {
                out.insert(key.clone(), value.clone());
            }
        } else if key == "properties" {
            if let Some(props) = value.as_object() {
                let mut converted_props = Map::new();
                let mut ordering = Vec::with_capacity(props.len());
                for (name, sub) in props {
                    if sub.is_object() {
                        converted_props
                            .insert(name.clone(), Value::Object(convert_node(sub, defs, seen)?));
                        ordering.push(Value::String(name.clone()));
                    }
                }
                out.insert("properties".to_string(), Value::Object(converted_props));
                out.insert("propertyOrdering".to_string(), Value::Array(ordering));
            }
        } else if key == "items" {
            if value.is_object() {
                out.insert(
                    "items".to_string(),
                    Value::Object(convert_node(value, defs, seen)?),
                );
            } else {
                out.insert(key.clone(), value.clone());
            }
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Ok(out)
}

pub fn vision_guess_gemini_schema() -> Result<Value, VisionError> {
    let raw = serde_json::to_value(schemars::schema_for!(VisionGuess))
        .map_err(|e| VisionError::Contract(format!("failed to serialize schema: {e}")))?;
    to_gemini_schema(&raw)
}

pub fn build_gemini_request(
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
    if image_jpeg.len() > MAX_INLINE_IMAGE_BYTES {
        return Err(VisionError::Other(
            "image exceeds the Gemini inline limit; refusing to send".to_string(),
        ));
    }
    let budget = thinking_budget(options.reasoning_effort.as_str(), model)?;
    let schema = vision_guess_gemini_schema()?;
    let payload = json!({
        "systemInstruction": {"parts": [{"text": PROMPT_V1}]},
        "contents": [
            {
                "role": "user",
                "parts": [
                    {
                        "inlineData": {
                            "mimeType": "image/jpeg",
                            "data": BASE64.encode(image_jpeg),
                        }
                    },
                    {"text": format_user_message(context)}
                ]
            }
        ],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": schema,
            "thinkingConfig": {
                "thinkingBudget": budget,
                "includeThoughts": false,
            }
        }
    });
    assert_safe_gemini_payload(&payload)?;
    Ok(payload)
}

pub fn assert_safe_gemini_payload(payload: &Value) -> Result<(), VisionError> {
    let config = payload
        .get("generationConfig")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            VisionError::Contract("refusing to send: generationConfig is required".to_string())
        })?;
    let budget_ok = config
        .get("thinkingConfig")
        .and_then(Value::as_object)
        .and_then(|t| t.get("thinkingBudget"))
        .is_some_and(|v| !v.is_null());
    if !budget_ok {
        return Err(VisionError::Contract(
            "refusing to send: thinkingConfig.thinkingBudget is required (an omitted budget bills the model's own choice)"
                .to_string(),
        ));
    }
    if config.get("responseMimeType").and_then(Value::as_str) != Some("application/json") {
        return Err(VisionError::Contract(
            "refusing to send: responseMimeType must be application/json".to_string(),
        ));
    }
    let schema_present = config
        .get("responseSchema")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());
    if !schema_present {
        return Err(VisionError::Contract(
            "refusing to send: responseSchema is required".to_string(),
        ));
    }
    Ok(())
}

pub fn parse_usage(payload: &Value) -> Usage {
    let Some(meta) = payload.get("usageMetadata").and_then(Value::as_object) else {
        return Usage::default();
    };
    Usage {
        input_tokens: json_u64(meta.get("promptTokenCount")),
        output_tokens: json_u64(meta.get("candidatesTokenCount")),
        reasoning_tokens: json_u64(meta.get("thoughtsTokenCount")),
    }
}

pub fn parse_vision_guess(payload: &Value) -> Result<VisionGuess, VisionError> {
    if let Some(reason) = payload
        .get("promptFeedback")
        .and_then(Value::as_object)
        .and_then(|f| f.get("blockReason"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Err(VisionError::Blocked(format!("content policy: {reason}")));
    }

    let candidates = payload
        .get("candidates")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
        .ok_or_else(|| VisionError::Parse("empty parse: no candidates".to_string()))?;

    let candidate = candidates[0]
        .as_object()
        .ok_or_else(|| VisionError::Parse("empty parse: no candidate object".to_string()))?;

    if let Some(finish) = candidate.get("finishReason").and_then(Value::as_str) {
        if BLOCKED_FINISH_REASONS.contains(&finish) {
            return Err(VisionError::Blocked(format!("content policy: {finish}")));
        }
        if finish == "MAX_TOKENS" {
            return Err(VisionError::Parse(
                "empty parse: hit MAX_TOKENS (thinking budget may have eaten the response)"
                    .to_string(),
            ));
        }
    }

    let mut text = String::new();
    if let Some(parts) = candidate
        .get("content")
        .and_then(Value::as_object)
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            let Some(part_obj) = part.as_object() else {
                continue;
            };
            let is_thought = part_obj
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_thought {
                continue;
            }
            if let Some(chunk) = part_obj.get("text").and_then(Value::as_str) {
                text.push_str(chunk);
            }
        }
    }

    if text.trim().is_empty() {
        return Err(VisionError::Parse("empty parse: no content".to_string()));
    }

    VisionGuess::from_json_str(&text)
        .map_err(|_| VisionError::Parse("empty parse: content is not VisionGuess JSON".to_string()))
}

#[derive(Debug, Clone)]
pub struct GeminiProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: Option<Client>,
    live_override: Option<bool>,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self, VisionError> {
        let key = api_key.into();
        if key.trim().is_empty() {
            return Err(VisionError::Other("GEMINI_API_KEY is missing".to_string()));
        }
        let m = model.into();
        Ok(Self {
            api_key: key,
            model: if m.trim().is_empty() {
                GEMINI_DEFAULT_MODEL.to_string()
            } else {
                m
            },
            base_url: GEMINI_BASE_URL.to_string(),
            client: None,
            live_override: None,
        })
    }

    pub fn with_default_model(api_key: impl Into<String>) -> Result<Self, VisionError> {
        Self::new(api_key, GEMINI_DEFAULT_MODEL)
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
            .map_err(|e| VisionError::Other(format!("Gemini HTTP client error: {e}")))
    }
}

#[async_trait]
impl VisionProvider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
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
        let payload = build_gemini_request(image_jpeg, context, options, &self.model)?;
        if self.is_live_blocked() {
            return Err(VisionError::Other(
                "GPSIFY_DRY_RUN=1 blocks live Gemini HTTP".to_string(),
            ));
        }
        let client = self.http_client(options.timeout_s)?;
        // API key goes in header, never query param, so keys never land in proxy/access logs.
        let url = format!("{}/models/{}:generateContent", self.base_url, self.model);
        let response = client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs_f64(options.timeout_s))
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    VisionError::Retryable("Gemini timeout".to_string())
                } else {
                    VisionError::Other(format!("Gemini HTTP error: {err}"))
                }
            })?;

        let status = response.status().as_u16();
        if status == 429 || status >= 500 {
            return Err(VisionError::Retryable(format!("Gemini HTTP {status}")));
        }
        if status != 200 {
            return Err(VisionError::Parse(format!("Gemini HTTP {status}")));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| VisionError::Parse("Gemini returned non-JSON".to_string()))?;
        if !body.is_object() {
            return Err(VisionError::Parse(
                "Gemini returned non-object JSON".to_string(),
            ));
        }
        Ok((parse_vision_guess(&body)?, parse_usage(&body)))
    }
}
