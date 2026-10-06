//! Gemini Live: native-audio conversation with function calling.
//!
//! Two ways in, one wire protocol:
//!
//! - [`GeminiLive`] connects straight to Google with an API key and sends the
//!   session `setup` itself.
//! - [`GeminiRelay`] connects to a relay URL minted elsewhere (for example the
//!   TinyHumans backend's metered relay). The relay fixed the setup when it
//!   minted the ticket, so the host builds that request with
//!   [`ticket_request`] from the same [`LiveConfig`] and this crate never
//!   touches the relay's HTTP API or its credential.
//!
//! Audio in is PCM16 at 16 kHz; audio out is PCM16 at 24 kHz
//! ([`OUTPUT_SAMPLE_RATE`]).

mod schema;
mod setup;
mod wire;

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::provider::{LiveProvider, validate_common};
use crate::session::{LiveSession, READY_TIMEOUT, await_ready, session_pair};
use crate::transport::{connect, drive};
use crate::types::{Capabilities, LiveConfig};

pub use schema::clean_schema;
pub use setup::{DEFAULT_MODEL, setup_message, ticket_request};
pub use wire::OUTPUT_SAMPLE_RATE;

use wire::{GeminiCodec, Mode};

/// Google's Live endpoint.
pub const DEFAULT_ENDPOINT: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

const CAPABILITIES: Capabilities = Capabilities {
    native_audio: true,
    tools: true,
    server_vad: true,
    manual_activity: true,
    text_input: true,
    interruptions: true,
    resumption: true,
    input_sample_rates: &[16_000],
};

/// Gemini Live over a direct connection with a Google API key.
#[derive(Clone)]
pub struct GeminiLive {
    api_key: String,
    endpoint: String,
}

impl std::fmt::Debug for GeminiLive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiLive")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl GeminiLive {
    /// A provider using `api_key` against Google's endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }

    /// Overrides the endpoint (a proxy, or a mock in tests).
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

#[async_trait]
impl LiveProvider for GeminiLive {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn connect(&self, config: LiveConfig) -> Result<LiveSession> {
        if self.api_key.trim().is_empty() {
            return Err(Error::InvalidConfig("gemini api key is empty".into()));
        }
        validate_common(&config, &CAPABILITIES)?;
        let socket = connect(&self.endpoint, &[("x-goog-api-key", self.api_key.clone())]).await?;
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let codec = GeminiCodec::new(
            Mode::Direct(setup_message(&config)),
            self.id(),
            Some(model),
            None,
            config.input_format,
            config.vad.turn_detection,
        );
        tracing::debug!(provider = "gemini", "tinyliveagents: connected");
        let (session, channels) = session_pair();
        await_ready(
            session.with_task(tokio::spawn(drive(
                Box::new(socket),
                Box::new(codec),
                channels,
            ))),
            READY_TIMEOUT,
        )
        .await
    }
}

/// Gemini Live through a relay ticket URL minted by the host.
#[derive(Clone)]
pub struct GeminiRelay {
    ws_url: String,
    session_id: Option<String>,
    model: Option<String>,
}

impl std::fmt::Debug for GeminiRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL carries a single-use ticket; never print it.
        f.debug_struct("GeminiRelay")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl GeminiRelay {
    /// A provider for one relay URL. The URL is single use: connect once.
    pub fn connect_url(ws_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            session_id: None,
            model: None,
        }
    }

    /// Records the relay's session id and model so they appear in
    /// [`crate::SessionInfo`].
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>, model: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self.model = Some(model.into());
        self
    }
}

#[async_trait]
impl LiveProvider for GeminiRelay {
    fn id(&self) -> &'static str {
        "gemini-relay"
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    /// Connects to the relay. The setup was fixed at ticket time, so only the
    /// input format and turn detection of `config` are used here.
    async fn connect(&self, config: LiveConfig) -> Result<LiveSession> {
        let mut checked = config.clone();
        // Tools were declared at ticket time; don't re-validate them here.
        checked.tools.clear();
        validate_common(&checked, &CAPABILITIES)?;
        let socket = connect(&self.ws_url, &[]).await?;
        let codec = GeminiCodec::new(
            Mode::Relay,
            self.id(),
            self.model.clone().or(config.model),
            self.session_id.clone(),
            config.input_format,
            config.vad.turn_detection,
        );
        tracing::debug!(provider = "gemini-relay", "tinyliveagents: connected");
        let (session, channels) = session_pair();
        await_ready(
            session.with_task(tokio::spawn(drive(
                Box::new(socket),
                Box::new(codec),
                channels,
            ))),
            READY_TIMEOUT,
        )
        .await
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
