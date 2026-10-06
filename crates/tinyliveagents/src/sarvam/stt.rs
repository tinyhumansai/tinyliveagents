//! Sarvam streaming speech-to-text (`saaras:v3-realtime`).
//!
//! The client sends base64 PCM in `{"event": "audio_input", "audio": ...}`
//! frames; the server answers with `session.begin`, `transcript.partial`,
//! `transcript.final`, `vad.speech_start` / `vad.speech_end`, `error` and
//! `session.end`. Turn detection runs server-side (`endpointing=vad`), so a
//! `transcript.final` is a finished user utterance.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};
use crate::transport::json_frame;
use crate::types::LiveConfig;

/// Sarvam's realtime STT endpoint.
pub const DEFAULT_STT_ENDPOINT: &str = "wss://api.sarvam.ai/speech-to-text-realtime/ws";
/// The default realtime STT model.
pub const DEFAULT_STT_MODEL: &str = "saaras:v3-realtime";

/// What one STT frame means for the cascade.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SttEvent {
    /// The STT session is open.
    Begin,
    /// Interim text for the current utterance.
    Partial(String),
    /// The finished utterance and the language it was detected as.
    Final {
        text: String,
        language: Option<String>,
    },
    /// The user started speaking.
    SpeechStart,
    /// The user stopped speaking.
    SpeechEnd,
    /// An error; `fatal` ends the session.
    Error { message: String, fatal: bool },
    /// The STT session ended.
    End,
    /// A frame the cascade does not act on.
    Ignored,
}

/// The STT socket URL for `config`.
///
/// # Errors
///
/// [`Error::InvalidConfig`] when `endpoint` is not a URL.
pub(crate) fn stt_url(endpoint: &str, config: &LiveConfig, language: &str) -> Result<String> {
    let mut url = url::Url::parse(endpoint)
        .map_err(|_| Error::InvalidConfig("sarvam stt endpoint is invalid".into()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("language_code", language);
        query.append_pair(
            "model",
            config.option_str("stt_model").unwrap_or(DEFAULT_STT_MODEL),
        );
        query.append_pair("encoding", "linear16");
        query.append_pair("sample_rate", &config.input_format.sample_rate.to_string());
        query.append_pair("endpointing", "vad");
        if let Some(ms) = config.vad.silence_ms {
            query.append_pair("silence_duration_ms", &ms.to_string());
        }
        if let Some(threshold) = config.vad.threshold {
            query.append_pair("threshold", &threshold.to_string());
        }
        if let Some(prompt) = config.option_str("stt_prompt") {
            query.append_pair("prompt", prompt);
        }
    }
    Ok(url.into())
}

/// An audio frame.
pub(crate) fn audio_frame(pcm: &[u8]) -> Message {
    json_frame(&json!({ "event": "audio_input", "audio": B64.encode(pcm) }))
}

/// A keepalive frame; Sarvam closes idle sockets with 1008.
pub(crate) fn ping_frame() -> Message {
    json_frame(&json!({ "event": "ping" }))
}

/// The end-of-stream frame sent before closing.
pub(crate) fn end_frame() -> Message {
    json_frame(&json!({ "event": "end" }))
}

/// Decodes one STT frame.
///
/// # Errors
///
/// [`Error::Protocol`] when the frame is not JSON.
pub(crate) fn decode(frame: &[u8]) -> Result<SttEvent> {
    let value: Value = serde_json::from_slice(frame)
        .map_err(|_| Error::Protocol("sarvam stt frame is not json".into()))?;
    let text = || {
        value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    Ok(
        match value
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "session.begin" => SttEvent::Begin,
            "transcript.partial" => SttEvent::Partial(text()),
            "transcript.final" => SttEvent::Final {
                text: text(),
                language: value
                    .get("language")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "vad.speech_start" => SttEvent::SpeechStart,
            "vad.speech_end" => SttEvent::SpeechEnd,
            "error" => SttEvent::Error {
                message: value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
                fatal: value
                    .get("is_fatal")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "session.end" => SttEvent::End,
            _ => SttEvent::Ignored,
        },
    )
}

#[cfg(test)]
#[path = "stt_tests.rs"]
mod tests;
