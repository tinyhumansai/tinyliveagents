//! The provider trait every live voice backend implements.

use async_trait::async_trait;

use crate::error::Result;
use crate::session::LiveSession;
use crate::types::{Capabilities, LiveConfig};

/// A live voice provider: something that turns a [`LiveConfig`] into a running
/// [`LiveSession`].
///
/// Implementations hold their credential or endpoint; connecting never needs
/// more than the configuration. A provider that needs a per-session URL minted
/// elsewhere (a relay ticket, a signed URL) exposes a `connect_url`
/// constructor instead and implements this trait over the URL it was given.
#[async_trait]
pub trait LiveProvider: Send + Sync + std::fmt::Debug {
    /// A stable, lowercase id (`gemini`, `gemini-relay`, `elevenlabs`,
    /// `sarvam`).
    fn id(&self) -> &'static str;

    /// What this provider supports.
    fn capabilities(&self) -> Capabilities;

    /// Opens a session. The first event on it is [`crate::LiveEvent::Ready`].
    ///
    /// # Errors
    ///
    /// [`crate::Error::InvalidConfig`] for a configuration the provider cannot
    /// serve, and the connection errors of [`crate::Error`] when the provider
    /// cannot be reached or refuses the credential.
    async fn connect(&self, config: LiveConfig) -> Result<LiveSession>;
}

/// Checks the parts of `config` every provider validates the same way.
///
/// # Errors
///
/// [`crate::Error::InvalidConfig`] when the input rate is not one the provider
/// accepts or a tool declaration has an empty name.
pub(crate) fn validate_common(config: &LiveConfig, capabilities: &Capabilities) -> Result<()> {
    if !capabilities.accepts_input_rate(config.input_format.sample_rate) {
        return Err(crate::Error::InvalidConfig(format!(
            "input sample rate {} is not supported",
            config.input_format.sample_rate
        )));
    }
    if config.tools.iter().any(|tool| tool.name.trim().is_empty()) {
        return Err(crate::Error::InvalidConfig(
            "tool declarations need a name".into(),
        ));
    }
    if !config.tools.is_empty() && !capabilities.tools {
        return Err(crate::Error::InvalidConfig(
            "this provider does not support tools".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
