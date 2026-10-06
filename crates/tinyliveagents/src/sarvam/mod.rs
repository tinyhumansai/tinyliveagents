//! Sarvam AI, as a chained live voice agent.
//!
//! Sarvam has no single speech-to-speech endpoint. [`SarvamCascade`] composes
//! three of its APIs behind the standard session:
//!
//! 1. streaming speech-to-text (`saaras:v3-realtime`, server-side VAD),
//! 2. chat completions with tool calling (`sarvam-105b-conversations` by
//!    default; [`crate::LiveConfig::model`] overrides it),
//! 3. streaming text-to-speech (`bulbul:v3`; [`crate::LiveConfig::voice`] is
//!    the speaker).
//!
//! The cascade keeps the conversation history, so to the host it behaves like
//! a native live model: audio in, audio and transcripts out, tool calls
//! answered with tool results, barge-in reported as
//! [`crate::LiveEvent::Interrupted`].
//!
//! Input audio must be 8 or 16 kHz. [`crate::LiveConfig::language`] (BCP-47,
//! `en-IN` by default) is used for both recognition and speech; set
//! `provider_options.auto_language = true` to recognise any supported language
//! and answer in the one detected.
//!
//! `provider_options` keys: `stt_model`, `stt_prompt`, `tts_model`, `pace`,
//! `output_sample_rate` (8000, 16000, 22050 or 24000), `auto_language`, and
//! the chat parameters `temperature`, `top_p`, `max_tokens` and
//! `reasoning_effort`.

mod cascade;
mod chat;
mod chunker;
mod stt;
mod tts;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::provider::{LiveProvider, validate_common};
use crate::session::{LiveSession, session_pair};
use crate::transport::connect;
use crate::types::{AudioFormat, Capabilities, LiveConfig, SessionInfo};

pub use chat::{DEFAULT_CHAT_ENDPOINT, DEFAULT_CHAT_MODEL};
pub use stt::{DEFAULT_STT_ENDPOINT, DEFAULT_STT_MODEL};
pub use tts::{DEFAULT_OUTPUT_RATE, DEFAULT_SPEAKER, DEFAULT_TTS_ENDPOINT, DEFAULT_TTS_MODEL};

const CAPABILITIES: Capabilities = Capabilities {
    native_audio: false,
    tools: true,
    server_vad: true,
    manual_activity: false,
    text_input: true,
    interruptions: true,
    resumption: false,
    input_sample_rates: &[8_000, 16_000],
};

/// Where the cascade's three services live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SarvamEndpoints {
    /// Streaming STT WebSocket.
    pub stt: String,
    /// Chat completions URL.
    pub chat: String,
    /// Streaming TTS WebSocket.
    pub tts: String,
}

impl Default for SarvamEndpoints {
    fn default() -> Self {
        Self {
            stt: DEFAULT_STT_ENDPOINT.to_string(),
            chat: DEFAULT_CHAT_ENDPOINT.to_string(),
            tts: DEFAULT_TTS_ENDPOINT.to_string(),
        }
    }
}

/// Sarvam STT, chat and TTS chained into a live session.
#[derive(Clone)]
pub struct SarvamCascade {
    api_key: String,
    endpoints: SarvamEndpoints,
    http: reqwest::Client,
}

impl std::fmt::Debug for SarvamCascade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SarvamCascade")
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl SarvamCascade {
    /// A cascade using a Sarvam API subscription key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            endpoints: SarvamEndpoints::default(),
            http: reqwest::Client::new(),
        }
    }

    /// Overrides the service endpoints (a proxy, or mocks in tests).
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: SarvamEndpoints) -> Self {
        self.endpoints = endpoints;
        self
    }
}

#[async_trait]
impl LiveProvider for SarvamCascade {
    fn id(&self) -> &'static str {
        "sarvam"
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn connect(&self, config: LiveConfig) -> Result<LiveSession> {
        if self.api_key.trim().is_empty() {
            return Err(Error::InvalidConfig("sarvam api key is empty".into()));
        }
        validate_common(&config, &CAPABILITIES)?;
        let auto = config
            .provider_options
            .get("auto_language")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let fixed_language = if auto {
            None
        } else {
            Some(
                config
                    .language
                    .clone()
                    .unwrap_or_else(|| "en-IN".to_string()),
            )
        };
        let stt_language = fixed_language.as_deref().unwrap_or("auto");
        let url = stt::stt_url(&self.endpoints.stt, &config, stt_language)?;
        let socket = connect(&url, &[("api-subscription-key", self.api_key.clone())]).await?;

        let info = SessionInfo {
            provider: self.id().to_string(),
            session_id: None,
            model: Some(
                config
                    .model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_CHAT_MODEL.to_string()),
            ),
            input_format: config.input_format,
            output_format: AudioFormat::pcm16(tts::output_rate(&config)),
        };
        let first_message = config.first_message.clone();
        let ctx = Arc::new(cascade::TurnContext {
            http: self.http.clone(),
            chat_endpoint: self.endpoints.chat.clone(),
            tts_endpoint: self.endpoints.tts.clone(),
            api_key: self.api_key.clone(),
            config,
            language: fixed_language,
        });
        tracing::debug!(provider = "sarvam", "tinyliveagents: connected");
        let (session, channels) = session_pair();
        Ok(session.with_task(tokio::spawn(cascade::run(
            socket,
            ctx,
            channels,
            info,
            first_message,
        ))))
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
