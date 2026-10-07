//! The standard, provider-neutral vocabulary of a live voice session.
//!
//! A host builds a [`LiveConfig`], hands it to a provider, and then talks to
//! the resulting session exclusively in these types: it sends
//! [`ClientCommand`]s (audio, text, tool results, interruptions) and receives
//! [`LiveEvent`]s (audio, transcripts, tool calls, turn boundaries). Nothing in
//! here names a provider; each provider module translates between this
//! vocabulary and its own wire protocol.
//!
//! Audio is always 16-bit little-endian mono PCM. The *input* rate is chosen by
//! the host in [`LiveConfig::input_format`]; the *output* rate is the
//! provider's and is reported in [`SessionInfo::output_format`] when the
//! session becomes ready.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;

/// The sample rate every provider accepts for input, and the default.
pub const DEFAULT_INPUT_SAMPLE_RATE: u32 = 16_000;

/// A PCM16 little-endian mono audio format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    /// Samples per second.
    pub sample_rate: u32,
}

impl AudioFormat {
    /// PCM16 mono at `sample_rate` Hz.
    #[must_use]
    pub const fn pcm16(sample_rate: u32) -> Self {
        Self { sample_rate }
    }

    /// Bytes of audio per second in this format (two bytes per sample).
    #[must_use]
    pub const fn bytes_per_second(&self) -> u32 {
        self.sample_rate * 2
    }

    /// The MIME type Gemini-style protocols use for this format.
    #[must_use]
    pub fn mime_type(&self) -> String {
        format!("audio/pcm;rate={}", self.sample_rate)
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self::pcm16(DEFAULT_INPUT_SAMPLE_RATE)
    }
}

/// A function the model may call, described the way every provider accepts:
/// a name, a description, and a JSON Schema object for the arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDeclaration {
    /// The function name the model will call.
    pub name: String,
    /// What the function does, written for the model.
    pub description: String,
    /// A JSON Schema `object` describing the arguments.
    pub parameters: Value,
}

impl ToolDeclaration {
    /// A declaration from its three parts.
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// Who decides when the user has started and stopped speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnDetection {
    /// The provider runs voice activity detection on the audio stream.
    #[default]
    Server,
    /// The host marks turns with [`ClientCommand::ActivityStart`] and
    /// [`ClientCommand::ActivityEnd`].
    Manual,
}

/// Voice activity detection settings.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct VadConfig {
    /// Server-side or host-driven turn detection.
    pub turn_detection: TurnDetection,
    /// Trailing silence, in milliseconds, that ends a user turn.
    pub silence_ms: Option<u32>,
    /// Speech probability threshold in `0.0..=1.0`, where supported.
    pub threshold: Option<f32>,
}

/// Everything a provider needs to open a session.
///
/// Fields a provider does not support are ignored by it; fields it requires
/// (Sarvam's language, for example) are validated in its `connect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveConfig {
    /// The provider model id. `None` selects the provider's default.
    pub model: Option<String>,
    /// System prompt for the agent.
    pub system_instruction: Option<String>,
    /// Provider voice name or id. `None` selects the provider's default.
    pub voice: Option<String>,
    /// BCP-47 language code (`en-IN`, `hi-IN`, ...), where the provider uses one.
    pub language: Option<String>,
    /// A line the agent speaks first, where the provider supports it.
    pub first_message: Option<String>,
    /// The format of the audio the host will send.
    pub input_format: AudioFormat,
    /// Functions the model may call.
    pub tools: Vec<ToolDeclaration>,
    /// Ask for transcripts of the user's speech.
    pub input_transcription: bool,
    /// Ask for transcripts of the agent's speech.
    pub output_transcription: bool,
    /// Voice activity detection settings.
    pub vad: VadConfig,
    /// Provider-specific settings merged into the provider's own setup. Its
    /// shape is documented on each provider; `Null` means none.
    pub provider_options: Value,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            model: None,
            system_instruction: None,
            voice: None,
            language: None,
            first_message: None,
            input_format: AudioFormat::default(),
            tools: Vec::new(),
            input_transcription: true,
            output_transcription: true,
            vad: VadConfig::default(),
            provider_options: Value::Null,
        }
    }
}

impl LiveConfig {
    /// A configuration with every default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the model id.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn with_system_instruction(mut self, text: impl Into<String>) -> Self {
        self.system_instruction = Some(text.into());
        self
    }

    /// Sets the voice.
    #[must_use]
    pub fn with_voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = Some(voice.into());
        self
    }

    /// Sets the language.
    #[must_use]
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// Adds a callable function.
    #[must_use]
    pub fn with_tool(mut self, tool: ToolDeclaration) -> Self {
        self.tools.push(tool);
        self
    }

    /// Sets the provider-specific options.
    #[must_use]
    pub fn with_provider_options(mut self, options: Value) -> Self {
        self.provider_options = options;
        self
    }

    /// Reads a string field from [`Self::provider_options`].
    #[must_use]
    pub fn option_str(&self, key: &str) -> Option<&str> {
        self.provider_options.get(key).and_then(Value::as_str)
    }
}

/// A function call the model asked for. Answer it with a [`ToolResult`]
/// carrying the same `call_id` and `name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id, echoed back in the result.
    pub call_id: String,
    /// The function name.
    pub name: String,
    /// Arguments as a JSON object.
    pub args: Value,
}

/// The outcome of a [`ToolCall`], sent back to the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The id of the call this answers.
    pub call_id: String,
    /// The function name of the call this answers.
    pub name: String,
    /// The result payload. A string or any JSON value.
    pub output: Value,
    /// Whether `output` describes a failure.
    pub is_error: bool,
}

impl ToolResult {
    /// A successful result.
    pub fn ok(call: &ToolCall, output: impl Into<Value>) -> Self {
        Self {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            output: output.into(),
            is_error: false,
        }
    }

    /// A failed result with a message for the model.
    pub fn error(call: &ToolCall, message: impl Into<String>) -> Self {
        Self {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            output: Value::String(message.into()),
            is_error: true,
        }
    }

    /// The output rendered as text, for providers that only accept strings.
    #[must_use]
    pub fn output_text(&self) -> String {
        match &self.output {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

/// Something the host asks the session to do.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ClientCommand {
    /// A chunk of PCM16 audio in [`LiveConfig::input_format`].
    Audio(Bytes),
    /// A typed user message.
    Text(String),
    /// The answer to a [`LiveEvent::ToolCall`].
    ToolResult(ToolResult),
    /// The user started speaking (manual turn detection).
    ActivityStart,
    /// The user stopped speaking (manual turn detection).
    ActivityEnd,
    /// Stop the agent's current reply (barge-in from the host side).
    Interrupt,
    /// Close the session.
    Close,
}

/// Token and audio accounting for one turn, as far as the provider reports it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt / input tokens.
    pub input_tokens: Option<u64>,
    /// Response / output tokens.
    pub output_tokens: Option<u64>,
    /// Total tokens.
    pub total_tokens: Option<u64>,
    /// Seconds of audio processed, where the provider reports audio instead.
    pub audio_seconds: Option<f64>,
}

/// Facts about a session that became ready.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The provider id (see [`crate::LiveProvider::id`]).
    pub provider: String,
    /// The provider's session or conversation id, when it assigns one.
    pub session_id: Option<String>,
    /// The model serving the session, when known.
    pub model: Option<String>,
    /// The format the host sends.
    pub input_format: AudioFormat,
    /// The format of [`LiveEvent::Audio`] chunks.
    pub output_format: AudioFormat,
}

/// Why a session closed.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CloseReason {
    /// The host closed it.
    Client,
    /// The provider closed it normally.
    Remote {
        /// The WebSocket close code, if any.
        code: Option<u16>,
        /// The close reason text, if any.
        reason: String,
    },
    /// The session failed.
    Error(Error),
}

/// Something that happened in the session.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LiveEvent {
    /// The session is ready for audio. Always the first event.
    Ready(SessionInfo),
    /// A chunk of the agent's speech in [`SessionInfo::output_format`].
    Audio(Bytes),
    /// Transcript of the user's speech. Partial transcripts are replaced by
    /// later ones until one arrives with `is_final`.
    InputTranscript {
        /// The transcript text (a fragment when the provider streams words).
        text: String,
        /// Whether this closes the user's utterance.
        is_final: bool,
    },
    /// Transcript of the agent's speech.
    OutputTranscript {
        /// The transcript text (a fragment when the provider streams words).
        text: String,
        /// Whether this closes the agent's utterance.
        is_final: bool,
    },
    /// The model wants a function called.
    ToolCall(ToolCall),
    /// The model no longer wants these calls answered (usually after barge-in).
    ToolCallCancelled {
        /// Ids of the cancelled calls.
        call_ids: Vec<String>,
    },
    /// The user interrupted the agent; drop any queued playback.
    Interrupted,
    /// The agent finished its turn.
    TurnComplete {
        /// Usage for the turn, when reported.
        usage: Option<Usage>,
    },
    /// A handle the provider will accept to resume this session later.
    ResumptionHandle {
        /// The opaque handle.
        handle: String,
    },
    /// The provider will close the connection soon.
    GoAway {
        /// Milliseconds until the provider disconnects, when reported.
        time_left_ms: Option<u64>,
    },
    /// A problem the provider reported. Non-fatal errors leave the session up.
    Error {
        /// What went wrong.
        error: Error,
        /// Whether the session is ending because of it.
        fatal: bool,
    },
    /// The session closed. Always the last event.
    Closed(CloseReason),
}

/// What a provider supports, so a host can adapt its UI and turn handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    /// One model hears and speaks (as opposed to a chained STT/LLM/TTS).
    pub native_audio: bool,
    /// The provider emits [`LiveEvent::ToolCall`].
    pub tools: bool,
    /// The provider detects turns itself.
    pub server_vad: bool,
    /// The provider honours [`ClientCommand::ActivityStart`] / `ActivityEnd`.
    pub manual_activity: bool,
    /// The provider accepts [`ClientCommand::Text`].
    pub text_input: bool,
    /// The provider reports [`LiveEvent::Interrupted`].
    pub interruptions: bool,
    /// The provider issues [`LiveEvent::ResumptionHandle`].
    pub resumption: bool,
    /// Input sample rates the provider accepts.
    pub input_sample_rates: &'static [u32],
}

impl Capabilities {
    /// Whether `rate` is an accepted input sample rate.
    #[must_use]
    pub fn accepts_input_rate(&self, rate: u32) -> bool {
        self.input_sample_rates.contains(&rate)
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
