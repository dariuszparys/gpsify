use gpsify::providers::gemini::{
    assert_safe_gemini_payload, build_gemini_request, parse_usage as gemini_parse_usage,
    parse_vision_guess as gemini_parse_vision_guess, thinking_budget, to_gemini_schema,
    vision_guess_gemini_schema, GeminiProvider, GEMINI_DEFAULT_MODEL,
};
use gpsify::providers::xai::{
    assert_safe_xai_payload, build_xai_request, format_user_message,
    parse_usage as xai_parse_usage, parse_vision_guess as xai_parse_vision_guess, XaiGrokProvider,
};
use gpsify::providers::{assert_safe_vision_options, VisionError, VisionProvider};
use gpsify::types::{
    Granularity, PromptContext, RefusalReason, Usage, VisionCallOptions, VisionGuess,
};
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sample_context() -> PromptContext {
    PromptContext {
        hint: Some("Porto".to_string()),
        folder_basename: "porto-trip".to_string(),
        filename: "IMG_1042.jpg".to_string(),
        datetime_original: Some("2025:06:15 14:22:10".to_string()),
        offset: None,
        make: Some("Apple".to_string()),
        model: Some("iPhone 15 Pro".to_string()),
        proposed_siblings: Vec::new(),
        gps_siblings: Vec::new(),
        no_context: false,
    }
}

fn sample_options() -> VisionCallOptions {
    VisionCallOptions::new("low", 120.0).expect("valid options")
}

fn sample_guess() -> VisionGuess {
    VisionGuess {
        refusal: RefusalReason::None,
        refusal_detail: None,
        primary_place_name: Some("Clérigos Church, Porto, Portugal".to_string()),
        country_code: Some("PT".to_string()),
        admin1: Some("Porto".to_string()),
        granularity: Granularity::Exact,
        lat_hint: Some(41.1456),
        lng_hint: Some(-8.6139),
        model_confidence: 0.85,
        clues: Vec::new(),
        candidates: Vec::new(),
        used_album_hint: false,
        used_filename_hint: false,
    }
}

fn has_key_recursive(val: &Value, needle: &str) -> bool {
    match val {
        Value::Object(map) => {
            if map.contains_key(needle) {
                return true;
            }
            map.values().any(|v| has_key_recursive(v, needle))
        }
        Value::Array(arr) => arr.iter().any(|v| has_key_recursive(v, needle)),
        _ => false,
    }
}

#[test]
fn vision_call_options_enforce_k13() {
    assert!(VisionCallOptions::with_store(true, "low", 120.0).is_err());
    assert!(VisionCallOptions::new("xhigh", 120.0).is_err());
    assert!(VisionCallOptions::new("low", 60.0).is_err());
    assert!(assert_safe_vision_options(&sample_options()).is_ok());
}

#[test]
fn build_xai_request_sets_store_false_and_reasoning_effort() {
    let jpeg = b"\xff\xd8\xff\x00\x01\x02";
    let payload = build_xai_request(jpeg, &sample_context(), &sample_options(), "grok-4.6")
        .expect("builds payload");
    assert_eq!(payload["store"], json!(false));
    assert_eq!(payload["store_messages"], json!(false));
    assert_eq!(payload["reasoning_effort"], json!("low"));
    assert_eq!(payload["model"], json!("grok-4.6"));

    let image = &payload["messages"][1]["content"][0];
    let url = image["image_url"]["url"].as_str().expect("url string");
    assert!(url.starts_with("data:image/jpeg;base64,"));
    assert!(!url.contains("http://"));
    assert!(!url.contains("https://"));
    assert_eq!(image["image_url"]["detail"], json!("high"));

    let text = payload["messages"][1]["content"][1]["text"]
        .as_str()
        .expect("text string");
    assert!(text.contains("IMG_1042.jpg"));
    assert!(!text.contains("/Users/"));
    assert_eq!(payload["response_format"]["type"], json!("json_schema"));
}

#[test]
fn xai_guards_reject_empty_and_unsafe_payloads() {
    assert!(build_xai_request(b"", &sample_context(), &sample_options(), "grok-4.6").is_err());

    let jpeg = b"\xff\xd8\xff\x00";
    let base = build_xai_request(jpeg, &sample_context(), &sample_options(), "grok-4.6").unwrap();

    let mut bad_store = base.clone();
    bad_store["store"] = json!(true);
    assert!(matches!(
        assert_safe_xai_payload(&bad_store),
        Err(VisionError::Contract(_))
    ));

    let mut bad_msgs = base.clone();
    bad_msgs["store_messages"] = json!(true);
    assert!(matches!(
        assert_safe_xai_payload(&bad_msgs),
        Err(VisionError::Contract(_))
    ));

    let mut missing_effort = base;
    missing_effort
        .as_object_mut()
        .unwrap()
        .remove("reasoning_effort");
    assert!(matches!(
        assert_safe_xai_payload(&missing_effort),
        Err(VisionError::Contract(_))
    ));
}

#[test]
fn no_context_user_message_strips_metadata() {
    let mut ctx = sample_context();
    ctx.no_context = true;
    let text = format_user_message(&ctx);
    assert_eq!(text, "Task: infer where this photo was taken.");
    assert!(!text.contains("Porto"));
    assert!(!text.contains("IMG_1042"));
}

#[test]
fn xai_usage_and_parse_distinguish_refusal_vs_empty() {
    let usage = xai_parse_usage(&json!({
        "usage": {
            "prompt_tokens": 2100,
            "completion_tokens": 520,
            "completion_tokens_details": {"reasoning_tokens": 800}
        }
    }));
    assert_eq!(
        usage,
        Usage {
            input_tokens: 2100,
            output_tokens: 520,
            reasoning_tokens: 800,
        }
    );

    let guess_json = serde_json::to_string(&sample_guess()).unwrap();
    let parsed = xai_parse_vision_guess(&json!({
        "choices": [{"message": {"content": guess_json}}]
    }))
    .unwrap();
    assert_eq!(
        parsed.primary_place_name.as_deref(),
        Some("Clérigos Church, Porto, Portugal")
    );

    let blocked = xai_parse_vision_guess(&json!({
        "choices": [{"message": {"refusal": "image_geolocation"}}]
    }));
    assert!(matches!(blocked, Err(VisionError::Blocked(_))));

    let empty = xai_parse_vision_guess(&json!({
        "choices": [{"message": {"content": "   "}}]
    }));
    assert!(matches!(empty, Err(VisionError::Parse(_))));
}

#[test]
fn gemini_thinking_budget_and_schema_normalization() {
    assert_eq!(thinking_budget("low", "gemini-2.5-flash").unwrap(), 1024);
    assert_eq!(thinking_budget("medium", "gemini-2.5-flash").unwrap(), 8192);
    assert_eq!(thinking_budget("high", "gemini-2.5-pro").unwrap(), 24576);
    assert!(thinking_budget("invalid", "gemini-2.5-flash").is_err());

    let gem_schema = vision_guess_gemini_schema().expect("gemini schema");
    assert!(!has_key_recursive(&gem_schema, "$ref"));
    assert!(!has_key_recursive(&gem_schema, "$defs"));
    assert!(!has_key_recursive(&gem_schema, "definitions"));
    assert!(!has_key_recursive(&gem_schema, "allOf"));
    assert!(!has_key_recursive(&gem_schema, "title"));

    let props = gem_schema["properties"].as_object().expect("properties");
    assert_eq!(
        props["primary_place_name"],
        json!({"type": "string", "nullable": true})
    );
    assert!(gem_schema["propertyOrdering"].is_array());
    assert!(props["granularity"]["enum"].is_array());
    assert!(props["refusal"]["enum"].is_array());

    let custom_defs = json!({
        "$defs": {
            "Inner": {
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }
        },
        "type": "object",
        "properties": {
            "inner": {"$ref": "#/$defs/Inner", "description": "item"}
        }
    });
    let converted = to_gemini_schema(&custom_defs).unwrap();
    assert_eq!(
        converted["properties"]["inner"]["description"],
        json!("item")
    );
    assert!(!has_key_recursive(&converted, "$defs"));

    let payload = build_gemini_request(
        b"\xff\xd8jpeg",
        &sample_context(),
        &sample_options(),
        GEMINI_DEFAULT_MODEL,
    )
    .unwrap();
    assert_eq!(
        payload["generationConfig"]["responseMimeType"],
        json!("application/json")
    );
    assert!(
        payload["generationConfig"]["thinkingConfig"]["thinkingBudget"]
            .as_u64()
            .is_some_and(|b| b > 0)
    );

    let mut bad_cfg = payload.clone();
    bad_cfg["generationConfig"] = json!({});
    assert!(matches!(
        assert_safe_gemini_payload(&bad_cfg),
        Err(VisionError::Contract(_))
    ));
}

#[test]
fn gemini_parse_handles_safety_blocks_thoughts_and_usage() {
    assert!(matches!(
        gemini_parse_vision_guess(&json!({"promptFeedback": {"blockReason": "SAFETY"}})),
        Err(VisionError::Blocked(_))
    ));
    assert!(matches!(
        gemini_parse_vision_guess(&json!({"candidates": [{"finishReason": "PROHIBITED_CONTENT"}]})),
        Err(VisionError::Blocked(_))
    ));
    assert!(matches!(
        gemini_parse_vision_guess(&json!({"candidates": [{"finishReason": "MAX_TOKENS"}]})),
        Err(VisionError::Parse(_))
    ));

    let guess_json = serde_json::to_string(&sample_guess()).unwrap();
    let ok = gemini_parse_vision_guess(&json!({
        "candidates": [{
            "finishReason": "STOP",
            "content": {
                "parts": [
                    {"thought": true, "text": "internal thought should be skipped"},
                    {"text": guess_json}
                ]
            }
        }]
    }))
    .unwrap();
    assert_eq!(ok.country_code.as_deref(), Some("PT"));

    let usage = gemini_parse_usage(&json!({
        "usageMetadata": {
            "promptTokenCount": 1032,
            "candidatesTokenCount": 480,
            "thoughtsTokenCount": 900
        }
    }));
    assert_eq!(
        usage,
        Usage {
            input_tokens: 1032,
            output_tokens: 480,
            reasoning_tokens: 900,
        }
    );
}

#[tokio::test]
async fn xai_provider_respects_dry_run_and_wiremock_roundtrip() {
    let blocked_provider = XaiGrokProvider::new("test-key", "grok-4.6")
        .unwrap()
        .with_live_override(false);
    let err = blocked_provider
        .guess(b"\xff\xd8\xff\x00", &sample_context(), &sample_options())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("GPSIFY_DRY_RUN"));

    let server = MockServer::start().await;
    let guess_json = serde_json::to_string(&sample_guess()).unwrap();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer secret-xai-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": guess_json}}],
            "usage": {
                "prompt_tokens": 500,
                "completion_tokens": 120,
                "completion_tokens_details": {"reasoning_tokens": 300}
            }
        })))
        .mount(&server)
        .await;

    let live_provider = XaiGrokProvider::new("secret-xai-key", "grok-4.6")
        .unwrap()
        .with_base_url(server.uri())
        .with_live_override(true);
    let (guess, usage) = live_provider
        .guess(b"\xff\xd8\xff\x00", &sample_context(), &sample_options())
        .await
        .expect("wiremock xAI call succeeds");
    assert_eq!(guess.country_code.as_deref(), Some("PT"));
    assert_eq!(usage.reasoning_tokens, 300);

    // 429 maps to Retryable
    let server_err = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server_err)
        .await;
    let rate_provider = XaiGrokProvider::new("secret-xai-key", "grok-4.6")
        .unwrap()
        .with_base_url(server_err.uri())
        .with_live_override(true);
    let rate_err = rate_provider
        .guess(b"\xff\xd8\xff\x00", &sample_context(), &sample_options())
        .await
        .unwrap_err();
    assert!(rate_err.is_retryable());
}

#[tokio::test]
async fn gemini_provider_sends_key_in_header_not_url_and_maps_http_errors() {
    assert!(GeminiProvider::with_default_model("   ").is_err());

    let blocked_provider = GeminiProvider::with_default_model("test-key")
        .unwrap()
        .with_live_override(false);
    let blocked_err = blocked_provider
        .guess(b"\xff\xd8\xff\x00", &sample_context(), &sample_options())
        .await
        .unwrap_err();
    assert!(blocked_err.to_string().contains("GPSIFY_DRY_RUN"));

    let server = MockServer::start().await;
    let guess_json = serde_json::to_string(&sample_guess()).unwrap();
    Mock::given(method("POST"))
        .and(path(format!("/models/{GEMINI_DEFAULT_MODEL}:generateContent")))
        .and(header("x-goog-api-key", "secret-gemini-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": guess_json}]}}],
            "usageMetadata": {"promptTokenCount": 1032, "candidatesTokenCount": 480, "thoughtsTokenCount": 900}
        })))
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_default_model("secret-gemini-key")
        .unwrap()
        .with_base_url(server.uri())
        .with_live_override(true);
    let (guess, usage) = provider
        .guess(b"\xff\xd8\xff\x00", &sample_context(), &sample_options())
        .await
        .expect("wiremock gemini roundtrip");
    assert_eq!(guess.country_code.as_deref(), Some("PT"));
    assert_eq!(usage.reasoning_tokens, 900);

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(!reqs[0].url.as_str().contains("secret-gemini-key"));

    for (code, expect_retryable) in [(429u16, true), (503u16, true), (400u16, false)] {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(code))
            .mount(&s)
            .await;
        let p = GeminiProvider::with_default_model("k")
            .unwrap()
            .with_base_url(s.uri())
            .with_live_override(true);
        let err = p
            .guess(b"\xff\xd8\xff\x00", &sample_context(), &sample_options())
            .await
            .unwrap_err();
        assert_eq!(err.is_retryable(), expect_retryable);
        assert!(err.to_string().contains(&format!("Gemini HTTP {code}")));
    }
}
