use gpsify::types::{
    Granularity, ReasoningEffort, RefusalReason, Usage, VisionCallOptions, VisionGuess,
};

#[test]
fn vision_call_options_refuses_store_true() {
    let err = VisionCallOptions::with_store(true, "low", 600.0).unwrap_err();
    assert!(err.to_string().contains("store must be False"));
}

#[test]
fn vision_call_options_rejects_xhigh() {
    let err = VisionCallOptions::new("xhigh", 600.0).unwrap_err();
    assert!(err.to_string().contains("xhigh"));
}

#[test]
fn vision_call_options_rejects_short_timeout() {
    let err = VisionCallOptions::new("low", 30.0).unwrap_err();
    assert!(err.to_string().contains("120"));
}

#[test]
fn vision_call_options_normalizes_effort() {
    let opts = VisionCallOptions::new(" LOW ", 120.0).expect("valid options");
    assert!(!opts.store());
    assert_eq!(opts.reasoning_effort, ReasoningEffort::Low);
    opts.assert_safe().expect("safe options");
}

#[test]
fn vision_call_options_deserialize_rejects_store_true() {
    let json = r#"{"store":true,"reasoning_effort":"low","timeout_s":600}"#;
    let err = serde_json::from_str::<VisionCallOptions>(json).unwrap_err();
    assert!(err.to_string().contains("store must be False"));
}

#[test]
fn vision_guess_round_trip() {
    let json = r#"{
        "refusal": "none",
        "primary_place_name": "Clérigos Church, Porto, Portugal",
        "country_code": "PT",
        "granularity": "exact",
        "model_confidence": 0.8,
        "clues": [{"kind": "architecture", "text": "baroque tower", "weight": 0.7}],
        "candidates": [
            {
                "place_name": "Clérigos Church, Porto, Portugal",
                "country_code": "PT",
                "confidence": 0.8,
                "why": "tower silhouette"
            }
        ],
        "used_album_hint": true,
        "used_filename_hint": false
    }"#;
    let guess = VisionGuess::from_json_str(json).expect("valid VisionGuess");
    assert_eq!(guess.refusal, RefusalReason::None);
    assert_eq!(guess.granularity, Granularity::Exact);
    assert_eq!(
        guess.primary_place_name.as_deref(),
        Some("Clérigos Church, Porto, Portugal")
    );
    assert!(guess.lat_hint.is_none());
}

#[test]
fn vision_guess_rejects_out_of_range_confidence() {
    let json = r#"{
        "refusal": "none",
        "granularity": "city",
        "model_confidence": 1.5,
        "clues": [],
        "candidates": [],
        "used_album_hint": false,
        "used_filename_hint": false
    }"#;
    let err = VisionGuess::from_json_str(json).unwrap_err();
    assert!(err.to_string().contains("model_confidence"));
}

#[test]
fn usage_defaults_are_zero() {
    let usage = Usage::default();
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
    assert_eq!(usage.reasoning_tokens, 0);
}
