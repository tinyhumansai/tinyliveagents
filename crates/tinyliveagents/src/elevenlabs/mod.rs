//! ElevenLabs Conversational AI ("Agents").
//!
//! An ElevenLabs agent is configured in ElevenLabs (its LLM, voice, client
//! tools and allowed overrides); a session joins it over a WebSocket. Two ways
//! in:
//!
//! - [`ElevenLabsConvai::connect_url`] with a signed URL minted by a backend
//!   that holds the ElevenLabs key (the TinyHumans backend's
//!   `/voice-agent/get-signed-url` works this way). No credential reaches the
//!   client.
//! - [`ElevenLabsConvai::agent`] with an agent id and an ElevenLabs API key.
//!
//! Tool calls arrive as the agent's *client tools*, which are declared on the
//! agent in ElevenLabs; [`crate::LiveConfig::tools`] is ignored here. Audio
//! sent at the config's input rate is resampled to the agent's input format,
//! and the agent's output format is reported in [`crate::SessionInfo`].

mod wire;

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::provider::{LiveProvider, validate_common};
use crate::session::{LiveSession, session_pair};
use crate::transport::{connect, drive};
use crate::types::{Capabilities, LiveConfig};

pub use wire::{DEFAULT_SAMPLE_RATE, initiation_message};

use wire::ElevenLabsCodec;

/// ElevenLabs' public conversation endpoint.
pub const DEFAULT_ENDPOINT: &str = "wss://api.elevenlabs.io/v1/convai/conversation";

const CAPABILITIES: Capabilities = Capabilities {
    native_audio: false,
    tools: true,
    server_vad: true,
    manual_activity: false,
    text_input: true,
    interruptions: true,
    resumption: false,
    input_sample_rates: &[8_000, 16_000, 22_050, 24_000, 44_100, 48_000],
};

#[derive(Clone)]
enum Target {
    Signed(String),
    Agent {
        endpoint: String,
        agent_id: String,
        api_key: String,
    },
}

/// A session with an ElevenLabs agent.
#[derive(Clone)]
pub struct ElevenLabsConvai {
    target: Target,
}

impl std::fmt::Debug for ElevenLabsConvai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither the signed URL nor the API key may be printed.
        let kind = match &self.target {
            Target::Signed(_) => "signed-url",
            Target::Agent { .. } => "agent",
        };
        f.debug_struct("ElevenLabsConvai")
            .field("target", &kind)
            .finish_non_exhaustive()
    }
}

impl ElevenLabsConvai {
    /// Joins through a signed URL. Signed URLs are short-lived: connect soon.
    pub fn connect_url(signed_url: impl Into<String>) -> Self {
        Self {
            target: Target::Signed(signed_url.into()),
        }
    }

    /// Joins `agent_id` with an ElevenLabs API key.
    pub fn agent(agent_id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            target: Target::Agent {
                endpoint: DEFAULT_ENDPOINT.to_string(),
                agent_id: agent_id.into(),
                api_key: api_key.into(),
            },
        }
    }

    /// Overrides the endpoint used with [`Self::agent`] (a mock in tests).
    #[must_use]
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        if let Target::Agent { endpoint, .. } = &mut self.target {
            *endpoint = url.into();
        }
        self
    }
}

#[async_trait]
impl LiveProvider for ElevenLabsConvai {
    fn id(&self) -> &'static str {
        "elevenlabs"
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn connect(&self, config: LiveConfig) -> Result<LiveSession> {
        let mut checked = config.clone();
        checked.tools.clear();
        validate_common(&checked, &CAPABILITIES)?;
        let socket = match &self.target {
            Target::Signed(url) => connect(url, &[]).await?,
            Target::Agent {
                endpoint,
                agent_id,
                api_key,
            } => {
                if api_key.trim().is_empty() || agent_id.trim().is_empty() {
                    return Err(Error::InvalidConfig(
                        "elevenlabs needs an agent id and an api key".into(),
                    ));
                }
                let mut url = url::Url::parse(endpoint)
                    .map_err(|_| Error::InvalidConfig("elevenlabs endpoint is invalid".into()))?;
                url.query_pairs_mut().append_pair("agent_id", agent_id);
                connect(url.as_str(), &[("xi-api-key", api_key.clone())]).await?
            }
        };
        tracing::debug!(provider = "elevenlabs", "tinyliveagents: connected");
        let codec = ElevenLabsCodec::new(&config);
        let (session, channels) = session_pair();
        Ok(session.with_task(tokio::spawn(drive(socket, codec, channels))))
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
