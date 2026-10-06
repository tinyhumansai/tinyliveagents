//! The ElevenLabs Conversational AI wire codec.
//!
//! Client frames: `conversation_initiation_client_data` (sent on open),
//! `user_audio_chunk`, `user_message`, `user_activity`, `client_tool_result`
//! and `pong`. Server frames are `{"type": ...}` objects:
//! `conversation_initiation_metadata` (the session is ready and these are its
//! audio formats), `audio`, `user_transcript` / `tentative_user_transcript`,
//! `agent_response` / `agent_response_correction`, `interruption`, `ping`,
//! `client_tool_call` and `error`. Anything else (`vad_score`,
//! `internal_*`, ...) is ignored.
//!
//! ElevenLabs reports the agent's reply text once, when the reply starts, and
//! after a barge-in sends `agent_response_correction` with the text actually
//! spoken. So a reply is first emitted as a *partial*
//! [`LiveEvent::OutputTranscript`] and becomes final only when it can no
//! longer change: on its correction, or when the next user utterance or agent
//! reply begins. Hosts that persist final transcripts therefore keep exactly
//! one version of each reply. Agents that enable the `agent_response_complete`
//! client event also get [`LiveEvent::TurnComplete`] (and the reply settles
//! there); others have no turn boundary to report.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::audio::{
    MAX_SAMPLE_RATE, MIN_SAMPLE_RATE, pcm16_to_ulaw, resample_pcm16, ulaw_to_pcm16,
};
use crate::error::{Error, Result};
use crate::transport::{Decoded, WireCodec, json_frame};
use crate::types::{AudioFormat, ClientCommand, LiveConfig, LiveEvent, SessionInfo, ToolCall};

/// The rate ElevenLabs agents use unless configured otherwise.
pub const DEFAULT_SAMPLE_RATE: u32 = 16_000;

/// An ElevenLabs agent audio format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentFormat {
    /// PCM16 at this rate (`pcm_16000`, `pcm_24000`, ...).
    Pcm(u32),
    /// G.711 μ-law at 8 kHz (`ulaw_8000`); transcoded to and from PCM16.
    Ulaw8k,
}

impl AgentFormat {
    /// The PCM rate the host sees for this format.
    pub(crate) fn rate(self) -> u32 {
        match self {
            Self::Pcm(rate) => rate,
            Self::Ulaw8k => 8_000,
        }
    }
}

/// Parses an ElevenLabs audio format name. Formats this crate cannot carry
/// as PCM16 yield `None`.
pub(crate) fn parse_format(format: &str) -> Option<AgentFormat> {
    if format == "ulaw_8000" {
        return Some(AgentFormat::Ulaw8k);
    }
    let rate: u32 = format.strip_prefix("pcm_")?.parse().ok()?;
    (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE)
        .contains(&rate)
        .then_some(AgentFormat::Pcm(rate))
}

/// Builds the `conversation_initiation_client_data` frame from a config.
///
/// Only fields that are set are sent: an agent rejects overrides its security
/// settings do not allow, so a host should set only what its agent permits.
/// `provider_options` keys: `user_id` (string), `custom_llm_extra_body`
/// (object), `dynamic_variables` (object).
#[must_use]
pub fn initiation_message(config: &LiveConfig) -> Value {
    let mut agent = Map::new();
    if let Some(prompt) = &config.system_instruction {
        agent.insert("prompt".into(), json!({ "prompt": prompt }));
    }
    if let Some(first) = &config.first_message {
        agent.insert("first_message".into(), json!(first));
    }
    if let Some(language) = &config.language {
        agent.insert("language".into(), json!(language));
    }
    let mut overrides = Map::new();
    if !agent.is_empty() {
        overrides.insert("agent".into(), Value::Object(agent));
    }
    if let Some(voice) = &config.voice {
        overrides.insert("tts".into(), json!({ "voice_id": voice }));
    }

    let mut message = Map::new();
    message.insert("type".into(), json!("conversation_initiation_client_data"));
    if !overrides.is_empty() {
        message.insert(
            "conversation_config_override".into(),
            Value::Object(overrides),
        );
    }
    if let Some(user_id) = config.option_str("user_id") {
        message.insert("user_id".into(), json!(user_id));
    }
    for key in ["custom_llm_extra_body", "dynamic_variables"] {
        if let Some(value @ Value::Object(_)) = config.provider_options.get(key) {
            message.insert(key.into(), value.clone());
        }
    }
    Value::Object(message)
}

/// Translates between the standard vocabulary and ElevenLabs frames.
#[derive(Debug)]
pub(crate) struct ElevenLabsCodec {
    initiation: Value,
    host_input: AudioFormat,
    agent_input: AgentFormat,
    agent_output: AgentFormat,
    /// The latest agent reply, still open to an `agent_response_correction`.
    pending_reply: Option<String>,
}

impl ElevenLabsCodec {
    pub(crate) fn new(config: &LiveConfig) -> Self {
        Self {
            initiation: initiation_message(config),
            host_input: config.input_format,
            agent_input: AgentFormat::Pcm(DEFAULT_SAMPLE_RATE),
            agent_output: AgentFormat::Pcm(DEFAULT_SAMPLE_RATE),
            pending_reply: None,
        }
    }

    /// Reads one format field; a missing one keeps the default, an unknown
    /// one is an error (its audio would be mislabelled otherwise).
    fn format_field(metadata: &Value, key: &str) -> Result<AgentFormat> {
        match metadata.get(key) {
            None | Some(Value::Null) => Ok(AgentFormat::Pcm(DEFAULT_SAMPLE_RATE)),
            Some(Value::String(name)) => parse_format(name).ok_or_else(|| {
                Error::InvalidConfig(format!("unsupported elevenlabs audio format {name}"))
            }),
            Some(_) => Err(Error::Protocol(format!("{key} is not a string"))),
        }
    }

    fn ready(&mut self, metadata: &Value) -> LiveEvent {
        let formats = Self::format_field(metadata, "user_input_audio_format").and_then(|input| {
            Self::format_field(metadata, "agent_output_audio_format").map(|output| (input, output))
        });
        let (input, output) = match formats {
            Ok(pair) => pair,
            Err(error) => return LiveEvent::Error { error, fatal: true },
        };
        self.agent_input = input;
        self.agent_output = output;
        LiveEvent::Ready(SessionInfo {
            provider: "elevenlabs".into(),
            session_id: metadata
                .get("conversation_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            model: None,
            input_format: self.host_input,
            output_format: AudioFormat::pcm16(output.rate()),
        })
    }
}

impl ElevenLabsCodec {
    /// Transcript frames, with replies held open until they settle.
    fn decode_transcript(&mut self, kind: &str, value: &Value) -> Vec<Decoded> {
        let mut out = Vec::new();
        match kind {
            "agent_response" => {
                let text = str_at(value, "/agent_response_event/agent_response")
                    .unwrap_or_default()
                    .to_string();
                self.settle_reply(&mut out);
                self.pending_reply = Some(text.clone());
                out.push(Decoded::Event(LiveEvent::OutputTranscript {
                    text,
                    is_final: false,
                }));
            }
            "agent_response_correction" => {
                // The correction replaces the open reply rather than adding a
                // second one.
                self.pending_reply = None;
                let text = str_at(
                    value,
                    "/agent_response_correction_event/corrected_agent_response",
                )
                .unwrap_or_default()
                .to_string();
                out.push(Decoded::Event(LiveEvent::OutputTranscript {
                    text,
                    is_final: true,
                }));
            }
            _ => {
                let text = str_at(value, "/user_transcription_event/user_transcript")
                    .or_else(|| {
                        str_at(value, "/tentative_user_transcription_event/user_transcript")
                    })
                    .unwrap_or_default();
                let is_final = kind == "user_transcript";
                if is_final {
                    self.settle_reply(&mut out);
                }
                out.push(Decoded::Event(LiveEvent::InputTranscript {
                    text: text.to_string(),
                    is_final,
                }));
            }
        }
        out
    }

    /// Closes the open agent reply, if any, as a final transcript.
    fn settle_reply(&mut self, out: &mut Vec<Decoded>) {
        if let Some(text) = self.pending_reply.take() {
            out.push(Decoded::Event(LiveEvent::OutputTranscript {
                text,
                is_final: true,
            }));
        }
    }
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

impl WireCodec for ElevenLabsCodec {
    fn on_open(&mut self) -> (Vec<Message>, Vec<LiveEvent>) {
        (vec![json_frame(&self.initiation)], Vec::new())
    }

    fn encode(&mut self, command: ClientCommand) -> Vec<Message> {
        let frame = match command {
            ClientCommand::Audio(pcm) => {
                let pcm = match resample_pcm16(
                    &pcm,
                    self.host_input.sample_rate,
                    self.agent_input.rate(),
                ) {
                    Ok(pcm) => pcm,
                    Err(error) => {
                        tracing::debug!(%error, "tinyliveagents: dropping unresamplable audio");
                        return Vec::new();
                    }
                };
                let wire = match self.agent_input {
                    AgentFormat::Pcm(_) => pcm,
                    AgentFormat::Ulaw8k => pcm16_to_ulaw(&pcm),
                };
                json!({ "user_audio_chunk": B64.encode(&wire) })
            }
            ClientCommand::Text(text) => json!({ "type": "user_message", "text": text }),
            ClientCommand::ToolResult(result) => json!({
                "type": "client_tool_result",
                "tool_call_id": result.call_id,
                "result": result.output_text(),
                "is_error": result.is_error,
            }),
            ClientCommand::ActivityStart => json!({ "type": "user_activity" }),
            ClientCommand::ActivityEnd | ClientCommand::Interrupt | ClientCommand::Close => {
                return Vec::new();
            }
        };
        vec![json_frame(&frame)]
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Vec<Decoded>> {
        let value: Value = serde_json::from_slice(frame)
            .map_err(|_| Error::Protocol("elevenlabs frame is not json".into()))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let event = match kind {
            "conversation_initiation_metadata" => {
                let metadata = value
                    .get("conversation_initiation_metadata_event")
                    .cloned()
                    .unwrap_or(Value::Null);
                self.ready(&metadata)
            }
            "audio" => {
                let data = str_at(&value, "/audio_event/audio_base_64").unwrap_or_default();
                let audio = B64
                    .decode(data)
                    .map_err(|_| Error::Protocol("audio event is not base64".into()))?;
                LiveEvent::Audio(match self.agent_output {
                    AgentFormat::Pcm(_) => Bytes::from(audio),
                    AgentFormat::Ulaw8k => ulaw_to_pcm16(&audio),
                })
            }
            "user_transcript"
            | "tentative_user_transcript"
            | "agent_response"
            | "agent_response_correction" => return Ok(self.decode_transcript(kind, &value)),
            "interruption" => LiveEvent::Interrupted,
            // Sent (when the agent enables it) once the reply, its tools and
            // its audio are done: the reply is settled and the turn is over.
            "agent_response_complete" => {
                let mut out = Vec::new();
                self.settle_reply(&mut out);
                out.push(Decoded::Event(LiveEvent::TurnComplete { usage: None }));
                return Ok(out);
            }
            "ping" => {
                let event_id = value
                    .pointer("/ping_event/event_id")
                    .cloned()
                    .unwrap_or(Value::Null);
                return Ok(vec![Decoded::Reply(json_frame(
                    &json!({ "type": "pong", "event_id": event_id }),
                ))]);
            }
            "client_tool_call" => {
                let call = value
                    .get("client_tool_call")
                    .cloned()
                    .unwrap_or(Value::Null);
                LiveEvent::ToolCall(ToolCall {
                    call_id: str_at(&call, "/tool_call_id")
                        .unwrap_or_default()
                        .to_string(),
                    name: str_at(&call, "/tool_name").unwrap_or_default().to_string(),
                    args: call.get("parameters").cloned().unwrap_or_else(|| json!({})),
                })
            }
            "error" => LiveEvent::Error {
                error: Error::Provider(
                    str_at(&value, "/error_event/message")
                        .or_else(|| str_at(&value, "/message"))
                        .unwrap_or("unknown error")
                        .to_string(),
                ),
                fatal: false,
            },
            _ => return Ok(Vec::new()),
        };
        Ok(vec![Decoded::Event(event)])
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
