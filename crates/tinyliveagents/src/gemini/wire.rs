//! The Gemini Live wire codec.
//!
//! Client frames: `setup` (direct only), `realtimeInput` (audio, activity
//! markers, end of stream), `clientContent` (typed turns) and `toolResponse`.
//! Server frames: `setupComplete`, `serverContent` (audio parts, transcription
//! fragments, `interrupted`, `turnComplete`), `toolCall`,
//! `toolCallCancellation`, `usageMetadata`, `sessionResumptionUpdate` and
//! `goAway`. Server frames may arrive as text or binary; both hold JSON.
//!
//! Gemini streams transcriptions as fragments. The codec accumulates them so
//! every [`LiveEvent::InputTranscript`] / [`LiveEvent::OutputTranscript`]
//! carries the utterance *so far*, and closes each utterance with an
//! `is_final` event: the user's when the model starts answering, the agent's
//! when its turn completes or is interrupted.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};
use crate::transport::{Decoded, WireCodec, common_close_error, json_frame};
use crate::types::{
    AudioFormat, ClientCommand, LiveEvent, SessionInfo, ToolCall, ToolResult, TurnDetection, Usage,
};

/// Gemini Live answers in 24 kHz PCM16.
pub const OUTPUT_SAMPLE_RATE: u32 = 24_000;

/// Whether the codec sends the setup itself or a relay already did.
#[derive(Debug, Clone)]
pub(crate) enum Mode {
    /// Send this `setup` frame on open and become ready on `setupComplete`.
    Direct(Value),
    /// The relay fixed the setup at ticket time; ready as soon as the socket
    /// opens.
    Relay,
}

/// Translates between the standard vocabulary and Gemini Live frames.
#[derive(Debug)]
pub(crate) struct GeminiCodec {
    mode: Mode,
    provider: &'static str,
    model: Option<String>,
    session_id: Option<String>,
    input_format: AudioFormat,
    turn_detection: TurnDetection,
    ready: bool,
    input_text: String,
    output_text: String,
    pending_usage: Option<Usage>,
    next_call: u64,
}

impl GeminiCodec {
    pub(crate) fn new(
        mode: Mode,
        provider: &'static str,
        model: Option<String>,
        session_id: Option<String>,
        input_format: AudioFormat,
        turn_detection: TurnDetection,
    ) -> Self {
        Self {
            mode,
            provider,
            model,
            session_id,
            input_format,
            turn_detection,
            ready: false,
            input_text: String::new(),
            output_text: String::new(),
            pending_usage: None,
            next_call: 0,
        }
    }

    fn ready_event(&mut self) -> LiveEvent {
        self.ready = true;
        LiveEvent::Ready(SessionInfo {
            provider: self.provider.to_string(),
            session_id: self.session_id.clone(),
            model: self.model.clone(),
            input_format: self.input_format,
            output_format: AudioFormat::pcm16(OUTPUT_SAMPLE_RATE),
        })
    }

    fn finish_input(&mut self, out: &mut Vec<Decoded>) {
        if !self.input_text.is_empty() {
            let text = std::mem::take(&mut self.input_text);
            out.push(Decoded::Event(LiveEvent::InputTranscript {
                text,
                is_final: true,
            }));
        }
    }

    fn finish_output(&mut self, out: &mut Vec<Decoded>) {
        if !self.output_text.is_empty() {
            let text = std::mem::take(&mut self.output_text);
            out.push(Decoded::Event(LiveEvent::OutputTranscript {
                text,
                is_final: true,
            }));
        }
    }

    fn decode_server_content(&mut self, content: &Value, out: &mut Vec<Decoded>) -> Result<()> {
        if let Some(fragment) = transcription_text(content.get("inputTranscription")) {
            self.input_text.push_str(fragment);
            out.push(Decoded::Event(LiveEvent::InputTranscript {
                text: self.input_text.clone(),
                is_final: false,
            }));
        }
        if let Some(parts) = content
            .pointer("/modelTurn/parts")
            .and_then(Value::as_array)
        {
            self.finish_input(out);
            for part in parts {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if let Some(data) = part.pointer("/inlineData/data").and_then(Value::as_str) {
                    let audio = B64
                        .decode(data)
                        .map_err(|_| Error::Protocol("audio part is not base64".into()))?;
                    out.push(Decoded::Event(LiveEvent::Audio(Bytes::from(audio))));
                } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                    self.output_text.push_str(text);
                    out.push(Decoded::Event(LiveEvent::OutputTranscript {
                        text: self.output_text.clone(),
                        is_final: false,
                    }));
                }
            }
        }
        if let Some(fragment) = transcription_text(content.get("outputTranscription")) {
            self.finish_input(out);
            self.output_text.push_str(fragment);
            out.push(Decoded::Event(LiveEvent::OutputTranscript {
                text: self.output_text.clone(),
                is_final: false,
            }));
        }
        if content.get("interrupted").and_then(Value::as_bool) == Some(true) {
            self.finish_output(out);
            out.push(Decoded::Event(LiveEvent::Interrupted));
        }
        if content.get("turnComplete").and_then(Value::as_bool) == Some(true) {
            self.finish_input(out);
            self.finish_output(out);
            out.push(Decoded::Event(LiveEvent::TurnComplete {
                usage: self.pending_usage.take(),
            }));
        }
        Ok(())
    }

    fn decode_tool_calls(&mut self, tool_call: &Value, out: &mut Vec<Decoded>) {
        let calls = tool_call
            .get("functionCalls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !calls.is_empty() {
            self.finish_input(out);
        }
        for call in calls {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| self.mint_call_id(), str::to_string);
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            out.push(Decoded::Event(LiveEvent::ToolCall(ToolCall {
                call_id,
                name,
                args,
            })));
        }
    }

    fn mint_call_id(&mut self) -> String {
        self.next_call += 1;
        format!("call-{}", self.next_call)
    }
}

fn transcription_text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(|v| v.get("text"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Parses Gemini's protobuf-JSON duration (`"12.5s"`) into milliseconds.
pub(crate) fn duration_ms(text: &str) -> Option<u64> {
    let seconds: f64 = text.strip_suffix('s')?.parse().ok()?;
    if seconds.is_sign_negative() || !seconds.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((seconds * 1000.0).round() as u64)
}

fn usage_from(metadata: &Value) -> Usage {
    Usage {
        input_tokens: metadata.get("promptTokenCount").and_then(Value::as_u64),
        output_tokens: metadata
            .get("responseTokenCount")
            .or_else(|| metadata.get("candidatesTokenCount"))
            .and_then(Value::as_u64),
        total_tokens: metadata.get("totalTokenCount").and_then(Value::as_u64),
        audio_seconds: None,
    }
}

/// The `functionResponses[].response` object for a result. Gemini requires an
/// object, so scalars are wrapped as `{"result": ...}` or `{"error": ...}`.
pub(crate) fn response_object(result: &ToolResult) -> Value {
    match (&result.output, result.is_error) {
        (Value::Object(_), false) => result.output.clone(),
        (output, true) => json!({ "error": output }),
        (output, false) => json!({ "result": output }),
    }
}

impl WireCodec for GeminiCodec {
    fn on_open(&mut self) -> (Vec<Message>, Vec<LiveEvent>) {
        match self.mode.clone() {
            Mode::Direct(setup) => (vec![json_frame(&setup)], Vec::new()),
            Mode::Relay => {
                let ready = self.ready_event();
                (Vec::new(), vec![ready])
            }
        }
    }

    fn encode(&mut self, command: ClientCommand) -> Vec<Message> {
        let frame = match command {
            ClientCommand::Audio(pcm) => json!({
                "realtimeInput": {
                    "audio": {
                        "data": B64.encode(&pcm),
                        "mimeType": self.input_format.mime_type(),
                    }
                }
            }),
            ClientCommand::Text(text) => json!({
                "clientContent": {
                    "turns": [{ "role": "user", "parts": [{ "text": text }] }],
                    "turnComplete": true,
                }
            }),
            ClientCommand::ToolResult(result) => json!({
                "toolResponse": {
                    "functionResponses": [{
                        "id": result.call_id,
                        "name": result.name,
                        "response": response_object(&result),
                    }]
                }
            }),
            ClientCommand::ActivityStart => json!({ "realtimeInput": { "activityStart": {} } }),
            ClientCommand::ActivityEnd => match self.turn_detection {
                TurnDetection::Manual => json!({ "realtimeInput": { "activityEnd": {} } }),
                TurnDetection::Server => json!({ "realtimeInput": { "audioStreamEnd": true } }),
            },
            // Gemini has no explicit cancel; barge-in is detected from audio.
            ClientCommand::Interrupt | ClientCommand::Close => return Vec::new(),
        };
        vec![json_frame(&frame)]
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Vec<Decoded>> {
        let value: Value = serde_json::from_slice(frame)
            .map_err(|_| Error::Protocol("gemini frame is not json".into()))?;
        let mut out = Vec::new();
        if value.get("setupComplete").is_some() && !self.ready {
            out.push(Decoded::Event(self.ready_event()));
        }
        if let Some(metadata) = value.get("usageMetadata") {
            self.pending_usage = Some(usage_from(metadata));
        }
        if let Some(content) = value.get("serverContent") {
            self.decode_server_content(content, &mut out)?;
        }
        if let Some(tool_call) = value.get("toolCall") {
            self.decode_tool_calls(tool_call, &mut out);
        }
        if let Some(cancel) = value.get("toolCallCancellation") {
            let call_ids = cancel
                .get("ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            out.push(Decoded::Event(LiveEvent::ToolCallCancelled { call_ids }));
        }
        if let Some(update) = value.get("sessionResumptionUpdate") {
            let resumable = update
                .get("resumable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let (true, Some(handle)) =
                (resumable, update.get("newHandle").and_then(Value::as_str))
            {
                out.push(Decoded::Event(LiveEvent::ResumptionHandle {
                    handle: handle.to_string(),
                }));
            }
        }
        if let Some(go_away) = value.get("goAway") {
            let time_left_ms = go_away
                .get("timeLeft")
                .and_then(Value::as_str)
                .and_then(duration_ms);
            out.push(Decoded::Event(LiveEvent::GoAway { time_left_ms }));
        }
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            out.push(Decoded::Event(LiveEvent::Error {
                error: Error::Provider(message),
                fatal: false,
            }));
        }
        Ok(out)
    }

    fn close_error(&self, code: u16, reason: &str) -> Option<Error> {
        let lowered = reason.to_ascii_lowercase();
        match code {
            1007 => Some(Error::InvalidConfig(reason.to_string())),
            1008 if lowered.contains("api key") || lowered.contains("permission") => {
                Some(Error::Unauthorized)
            }
            _ => common_close_error(code, reason),
        }
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
