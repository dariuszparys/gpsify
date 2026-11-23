//! Vision providers trait, error taxonomy, option safety gate, and fake provider.

pub mod gemini;
pub mod xai;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::types::{PromptContext, Usage, VisionCallOptions, VisionGuess, MIN_VISION_TIMEOUT_S};

pub use gemini::{GeminiProvider, GEMINI_BASE_URL, GEMINI_DEFAULT_MODEL};
pub use xai::{XaiGrokProvider, PROMPT_V1, PROMPT_VERSION, XAI_BASE_URL};

/// Errors returned by vision providers.
#[derive(Debug, Error)]
pub enum VisionError {
    /// Safety / content-policy block from the provider. Terminal (`refused`), never retried.
    #[error("{0}")]
    Blocked(String),

    /// Transient HTTP 429 / 5xx / timeout eligible for retry backoff.
    #[error("{0}")]
    Retryable(String),

    /// Response was non-200 (other than 429/5xx), empty, MAX_TOKENS, or invalid VisionGuess JSON.
    #[error("{0}")]
    Parse(String),

    /// Request or option violated K13 contract (`store=False`, explicit reasoning/thinking budget).
    #[error("{0}")]
    Contract(String),

    /// Configuration error, missing API key, dry-run block, oversized image, or transport error.
    #[error("{0}")]
    Other(String),
}

impl VisionError {
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked(_))
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }

    pub fn is_parse(&self) -> bool {
        matches!(self, Self::Parse(_))
    }

    pub fn is_contract(&self) -> bool {
        matches!(self, Self::Contract(_))
    }
}

/// Last-chance K13 safety check before building or sending any provider payload.
pub fn assert_safe_vision_options(options: &VisionCallOptions) -> Result<(), VisionError> {
    if options.store() {
        return Err(VisionError::Contract(
            "refusing to send: store must be False on every vision call".to_string(),
        ));
    }
    if options.reasoning_effort.as_str().trim().is_empty() {
        return Err(VisionError::Contract(
            "refusing to send: reasoning_effort is required".to_string(),
        ));
    }
    if !options.timeout_s.is_finite() || options.timeout_s < MIN_VISION_TIMEOUT_S {
        return Err(VisionError::Contract(format!(
            "refusing to send: timeout_s must be >= {MIN_VISION_TIMEOUT_S}"
        )));
    }
    Ok(())
}

#[async_trait]
pub trait VisionProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn model(&self) -> &str;

    async fn guess(
        &self,
        image_jpeg: &[u8],
        context: &PromptContext,
        options: &VisionCallOptions,
    ) -> Result<(VisionGuess, Usage), VisionError>;
}

pub(crate) fn json_u64(val: Option<&Value>) -> u64 {
    let Some(v) = val else {
        return 0;
    };
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(n) = v.as_i64() {
        return u64::try_from(n).unwrap_or(0);
    }
    if let Some(s) = v.as_str() {
        return s.trim().parse::<u64>().unwrap_or(0);
    }
    0
}

#[derive(Debug, Clone)]
pub struct RecordedVisionCall {
    pub image_jpeg: Vec<u8>,
    pub context: PromptContext,
    pub options: VisionCallOptions,
}

type FakeResponderFn = dyn Fn(&[u8], &PromptContext, &VisionCallOptions) -> Result<(VisionGuess, Usage), VisionError>
    + Send
    + Sync;

enum FakeResponder {
    Fixed(VisionGuess, Usage),
    Queue(
        Mutex<VecDeque<Result<(VisionGuess, Usage), VisionError>>>,
        (VisionGuess, Usage),
    ),
    Custom(Arc<FakeResponderFn>),
}

/// Deterministic in-memory VisionProvider for tests.
pub struct FakeVisionProvider {
    provider_name: &'static str,
    model_name: String,
    responder: FakeResponder,
    calls: Mutex<Vec<RecordedVisionCall>>,
}

impl FakeVisionProvider {
    pub fn default_guess() -> VisionGuess {
        use crate::types::{Candidate, Clue, Granularity, RefusalReason};
        VisionGuess {
            refusal: RefusalReason::None,
            refusal_detail: None,
            primary_place_name: Some("Clérigos Church, Porto, Portugal".to_string()),
            country_code: Some("PT".to_string()),
            admin1: Some("Porto".to_string()),
            granularity: Granularity::Exact,
            lat_hint: Some(41.1456),
            lng_hint: Some(-8.6139),
            model_confidence: 0.9,
            clues: vec![Clue {
                kind: "architecture".to_string(),
                text: "Baroque granite tower".to_string(),
                weight: 0.8,
            }],
            candidates: vec![Candidate {
                place_name: "Clérigos Church, Porto, Portugal".to_string(),
                country_code: Some("PT".to_string()),
                admin1: Some("Porto".to_string()),
                lat_hint: Some(41.1456),
                lng_hint: Some(-8.6139),
                confidence: 0.9,
                why: "Distinctive bell tower".to_string(),
            }],
            used_album_hint: false,
            used_filename_hint: false,
        }
    }

    pub fn fixed(guess: VisionGuess, usage: Usage) -> Self {
        Self {
            provider_name: "fake",
            model_name: "fake-vision".to_string(),
            responder: FakeResponder::Fixed(guess, usage),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn sequence(
        outcomes: Vec<Result<(VisionGuess, Usage), VisionError>>,
        fallback: (VisionGuess, Usage),
    ) -> Self {
        Self {
            provider_name: "fake",
            model_name: "fake-vision".to_string(),
            responder: FakeResponder::Queue(Mutex::new(VecDeque::from(outcomes)), fallback),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn from_fn<F>(f: F) -> Self
    where
        F: Fn(
                &[u8],
                &PromptContext,
                &VisionCallOptions,
            ) -> Result<(VisionGuess, Usage), VisionError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            provider_name: "fake",
            model_name: "fake-vision".to_string(),
            responder: FakeResponder::Custom(Arc::new(f)),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model_name = model.into();
        self
    }

    pub fn with_name(mut self, name: &'static str) -> Self {
        self.provider_name = name;
        self
    }

    pub fn calls(&self) -> Vec<RecordedVisionCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl Default for FakeVisionProvider {
    fn default() -> Self {
        Self::fixed(
            Self::default_guess(),
            Usage {
                input_tokens: 1000,
                output_tokens: 200,
                reasoning_tokens: 100,
            },
        )
    }
}

#[async_trait]
impl VisionProvider for FakeVisionProvider {
    fn name(&self) -> &'static str {
        self.provider_name
    }

    fn model(&self) -> &str {
        &self.model_name
    }

    async fn guess(
        &self,
        image_jpeg: &[u8],
        context: &PromptContext,
        options: &VisionCallOptions,
    ) -> Result<(VisionGuess, Usage), VisionError> {
        assert_safe_vision_options(options)?;
        self.calls.lock().unwrap().push(RecordedVisionCall {
            image_jpeg: image_jpeg.to_vec(),
            context: context.clone(),
            options: options.clone(),
        });
        match &self.responder {
            FakeResponder::Fixed(guess, usage) => Ok((guess.clone(), *usage)),
            FakeResponder::Queue(queue, fallback) => {
                let mut guard = queue.lock().unwrap();
                match guard.pop_front() {
                    Some(item) => item,
                    None => Ok((fallback.0.clone(), fallback.1)),
                }
            }
            FakeResponder::Custom(func) => func(image_jpeg, context, options),
        }
    }
}
