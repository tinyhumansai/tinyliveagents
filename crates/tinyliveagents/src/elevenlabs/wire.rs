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
//! ElevenLabs reports the agent's reply text once, when the reply starts, so
//! [`LiveEvent::OutputTranscript`] events from this codec are always final;
//! a correction after barge-in replaces it with the text actually spoken.
//! There is no turn-complete frame, so this codec never emits
//! [`LiveEvent::TurnComplete`].

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::audio::resample_pcm16;
use crate::error::{Error, Result};
use crate::transport::{Decoded, WireCodec, json_frame};
use crate::types::{AudioFormat, ClientCommand, LiveConfig, LiveEvent, SessionInfo, ToolCall};

/// The rate ElevenLabs agents use unless configured otherwise.
pub const DEFAULT_SAMPLE_RATE: u32 = 16_000;

/// Parses an ElevenLabs audio format name (`pcm_16000`) into a sample rate.
/// Non-PCM formats (`ulaw_8000`) are not supported and yield `None`.
pub(crate) fn pcm_rate(format: &str) -> Option<u32> {
    format.strip_prefix("pcm_")?.parse().ok()
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
    agent_input_rate: u32,
}

impl ElevenLabsCodec {
    pub(crate) fn new(config: &LiveConfig) -> Self {
        Self {
            initiation: initiation_message(config),
            host_input: config.input_format,
            agent_input_rate: DEFAULT_SAMPLE_RATE,
        }
    }

    fn ready(&mut self, metadata: &Value) -> LiveEvent {
        if let Some(rate) = metadata
            .get("user_input_audio_format")
            .and_then(Value::as_str)
            .and_then(pcm_rate)
        {
            self.agent_input_rate = rate;
        }
        let output_rate = metadata
            .get("agent_output_audio_format")
            .and_then(Value::as_str)
            .and_then(pcm_rate)
            .unwrap_or(DEFAULT_SAMPLE_RATE);
        LiveEvent::Ready(SessionInfo {
            provider: "elevenlabs".into(),
            session_id: metadata
                .get("conversation_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            model: None,
            input_format: self.host_input,
            output_format: AudioFormat::pcm16(output_rate),
        })
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
                let pcm = resample_pcm16(&pcm, self.host_input.sample_rate, self.agent_input_rate);
                json!({ "user_audio_chunk": B64.encode(&pcm) })
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
                LiveEvent::Audio(Bytes::from(audio))
            }
            "user_transcript" | "tentative_user_transcript" => {
                let text = str_at(&value, "/user_transcription_event/user_transcript")
                    .or_else(|| {
                        str_at(
                            &value,
                            "/tentative_user_transcription_event/user_transcript",
                        )
                    })
                    .unwrap_or_default();
                LiveEvent::InputTranscript {
                    text: text.to_string(),
                    is_final: kind == "user_transcript",
                }
            }
            "agent_response" => LiveEvent::OutputTranscript {
                text: str_at(&value, "/agent_response_event/agent_response")
                    .unwrap_or_default()
                    .to_string(),
                is_final: true,
            },
            "agent_response_correction" => LiveEvent::OutputTranscript {
                text: str_at(
                    &value,
                    "/agent_response_correction_event/corrected_agent_response",
                )
                .unwrap_or_default()
                .to_string(),
                is_final: true,
            },
            "interruption" => LiveEvent::Interrupted,
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
