//! One standard API for live voice agent providers.
//!
//! A *live* voice agent holds a two-way audio conversation in real time: the
//! user's microphone streams in, the agent's voice streams out, either side can
//! interrupt the other, and the model can call functions mid-conversation.
//! Every provider offers this over its own WebSocket protocol. This crate puts
//! one vocabulary in front of all of them:
//!
//! - a [`LiveConfig`] describes the session (model, prompt, voice, language,
//!   tools, turn detection);
//! - a [`LiveProvider`] turns it into a [`LiveSession`];
//! - the host sends [`ClientCommand`]s and receives [`LiveEvent`]s, whichever
//!   provider is behind the session.
//!
//! # Providers
//!
//! | Provider | Type | Feature | Notes |
//! | --- | --- | --- | --- |
//! | Gemini Live, direct | [`gemini::GeminiLive`] | `gemini` | API key, native audio |
//! | Gemini Live, relayed | [`gemini::GeminiRelay`] | `gemini` | pre-minted relay ticket URL |
//! | ElevenLabs Conversational AI | [`elevenlabs::ElevenLabsConvai`] | `elevenlabs` | signed URL |
//! | Sarvam AI | [`sarvam::SarvamCascade`] | `sarvam` | chained STT, chat, TTS |
//!
//! # What this crate does not do
//!
//! It standardizes provider APIs and nothing more. It never executes a tool
//! (a [`LiveEvent::ToolCall`] is handed to the host, which answers with a
//! [`ToolResult`]), never stores or looks up a credential (providers are
//! constructed with one), never mints relay tickets or signed URLs (hosts do
//! that against their own backend), and applies no approval or product policy.
//! Orchestration — running the tools, deciding what the agent may do,
//! persisting transcripts — belongs to the host or to an agent harness built on
//! top of this crate.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(feature = "gemini")]
//! # async fn run() -> tinyliveagents::Result<()> {
//! use tinyliveagents::{LiveConfig, LiveEvent, LiveProvider, ToolResult};
//! use tinyliveagents::gemini::GeminiLive;
//!
//! let provider = GeminiLive::new("api-key");
//! let mut session = provider
//!     .connect(LiveConfig::new().with_system_instruction("Be brief."))
//!     .await?;
//! let sender = session.sender();
//! while let Some(event) = session.recv().await {
//!     match event {
//!         LiveEvent::Ready(_) => sender.send_audio(vec![0_u8; 3200]).await?,
//!         LiveEvent::ToolCall(call) => {
//!             sender.send_tool_result(ToolResult::ok(&call, "42")).await?;
//!         }
//!         LiveEvent::Closed(_) => break,
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```

pub mod audio;
mod error;
mod provider;
mod session;
#[cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
mod transport;
mod types;

#[cfg(feature = "elevenlabs")]
pub mod elevenlabs;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(feature = "sarvam")]
pub mod sarvam;

#[cfg(all(
    test,
    any(feature = "gemini", feature = "elevenlabs", feature = "sarvam")
))]
mod test_support;

pub use error::{Error, Result};
pub use provider::LiveProvider;
pub use session::{CHANNEL_CAPACITY, LiveEvents, LiveSender, LiveSession};
pub use types::{
    AudioFormat, Capabilities, ClientCommand, CloseReason, DEFAULT_INPUT_SAMPLE_RATE, LiveConfig,
    LiveEvent, SessionInfo, ToolCall, ToolDeclaration, ToolResult, TurnDetection, Usage, VadConfig,
};
