//! Sarvam streaming text-to-speech (`bulbul`).
//!
//! One socket carries many utterances: the client sends a `config` frame, then
//! `text` frames as the reply streams in, then `flush` to have everything
//! buffered spoken. The server sends `audio` frames (base64, in the requested
//! codec) and, with `send_completion_event=true`, an `event` frame with
//! `event_type: "final"` once a flush has been fully synthesized.
//!
//! The cascade asks for `linear16` so audio needs no decoding; if the server
//! wraps a chunk in a WAV header anyway, the header is stripped.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};
use crate::transport::{WsStream, connect, json_frame};
use crate::types::LiveConfig;

/// Sarvam's streaming TTS endpoint.
pub const DEFAULT_TTS_ENDPOINT: &str = "wss://api.sarvam.ai/text-to-speech/ws";
/// The default TTS model.
pub const DEFAULT_TTS_MODEL: &str = "bulbul:v3";
/// The default `bulbul:v3` speaker.
pub const DEFAULT_SPEAKER: &str = "shubh";
/// The default output rate.
pub const DEFAULT_OUTPUT_RATE: u32 = 24_000;

/// What one TTS frame means.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TtsEvent {
    /// A chunk of PCM16 audio.
    Audio(Bytes),
    /// Everything flushed so far has been synthesized.
    Final,
    /// The server reported an error.
    Error(String),
    /// A frame with no meaning for the cascade.
    Ignored,
}

/// The output rate for `config`.
pub(crate) fn output_rate(config: &LiveConfig) -> u32 {
    config
        .provider_options
        .get("output_sample_rate")
        .and_then(Value::as_u64)
        .and_then(|rate| u32::try_from(rate).ok())
        .filter(|rate| [8_000, 16_000, 22_050, 24_000].contains(rate))
        .unwrap_or(DEFAULT_OUTPUT_RATE)
}

/// The `config` frame for a TTS socket.
pub(crate) fn config_frame(config: &LiveConfig, language: &str) -> Value {
    let mut data = Map::new();
    data.insert("language_code".into(), json!(language));
    data.insert(
        "speaker".into(),
        json!(config.voice.as_deref().unwrap_or(DEFAULT_SPEAKER)),
    );
    data.insert(
        "model".into(),
        json!(config.option_str("tts_model").unwrap_or(DEFAULT_TTS_MODEL)),
    );
    data.insert("output_audio_codec".into(), json!("linear16"));
    data.insert(
        "speech_sample_rate".into(),
        json!(output_rate(config).to_string()),
    );
    if let Some(pace) = config.provider_options.get("pace").and_then(Value::as_f64) {
        data.insert("pace".into(), json!(pace));
    }
    json!({ "type": "config", "data": data })
}

/// Strips a RIFF/WAV header if `audio` has one, returning the PCM payload.
pub(crate) fn strip_wav_header(audio: Vec<u8>) -> Vec<u8> {
    if audio.len() < 12 || &audio[0..4] != b"RIFF" || &audio[8..12] != b"WAVE" {
        return audio;
    }
    let mut offset = 12;
    while offset + 8 <= audio.len() {
        let id = &audio[offset..offset + 4];
        let size = u32::from_le_bytes([
            audio[offset + 4],
            audio[offset + 5],
            audio[offset + 6],
            audio[offset + 7],
        ]) as usize;
        let body = offset + 8;
        if id == b"data" {
            let end = body.saturating_add(size).min(audio.len());
            return audio[body..end].to_vec();
        }
        offset = body.saturating_add(size + (size & 1));
    }
    Vec::new()
}

/// Decodes one TTS frame.
///
/// # Errors
///
/// [`Error::Protocol`] when the frame is not JSON or its audio is not base64.
pub(crate) fn decode(frame: &[u8]) -> Result<TtsEvent> {
    let value: Value = serde_json::from_slice(frame)
        .map_err(|_| Error::Protocol("sarvam tts frame is not json".into()))?;
    Ok(
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "audio" => {
                let data = value
                    .pointer("/data/audio")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let audio = B64
                    .decode(data)
                    .map_err(|_| Error::Protocol("tts audio is not base64".into()))?;
                TtsEvent::Audio(Bytes::from(strip_wav_header(audio)))
            }
            "event" => {
                if value.pointer("/data/event_type").and_then(Value::as_str) == Some("final") {
                    TtsEvent::Final
                } else {
                    TtsEvent::Ignored
                }
            }
            "error" => TtsEvent::Error(
                value
                    .pointer("/data/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            ),
            _ => TtsEvent::Ignored,
        },
    )
}

/// One open TTS socket.
#[derive(Debug)]
pub(crate) struct TtsStream {
    ws: WsStream,
    /// Text sent since the last completed flush.
    pending: bool,
    /// Flushes sent whose `final` has not arrived.
    awaiting_final: usize,
}

impl TtsStream {
    /// Opens a socket and sends its configuration.
    ///
    /// # Errors
    ///
    /// The connection errors of [`connect`], or [`Error::Connect`] when the
    /// configuration cannot be sent.
    pub(crate) async fn open(
        endpoint: &str,
        api_key: &str,
        config: &LiveConfig,
        language: &str,
    ) -> Result<Self> {
        let mut url = url::Url::parse(endpoint)
            .map_err(|_| Error::InvalidConfig("sarvam tts endpoint is invalid".into()))?;
        url.query_pairs_mut()
            .append_pair(
                "model",
                config.option_str("tts_model").unwrap_or(DEFAULT_TTS_MODEL),
            )
            .append_pair("send_completion_event", "true");
        let mut ws = connect(
            url.as_str(),
            &[("api-subscription-key", api_key.to_string())],
        )
        .await?;
        ws.send(json_frame(&config_frame(config, language)))
            .await
            .map_err(|_| Error::Connect("could not configure sarvam tts".into()))?;
        Ok(Self {
            ws,
            pending: false,
            awaiting_final: 0,
        })
    }

    /// Queues text for synthesis.
    ///
    /// # Errors
    ///
    /// [`Error::Connect`] when the socket is gone.
    pub(crate) async fn speak(&mut self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.pending = true;
        self.send(json!({ "type": "text", "data": { "text": text } }))
            .await
    }

    /// Asks for everything queued to be spoken. A no-op when nothing is queued.
    ///
    /// # Errors
    ///
    /// [`Error::Connect`] when the socket is gone.
    pub(crate) async fn flush(&mut self) -> Result<()> {
        if !self.pending {
            return Ok(());
        }
        self.pending = false;
        self.awaiting_final += 1;
        self.send(json!({ "type": "flush" })).await
    }

    /// Whether flushed text is still being synthesized.
    pub(crate) fn is_busy(&self) -> bool {
        self.awaiting_final > 0
    }

    async fn send(&mut self, value: Value) -> Result<()> {
        self.ws
            .send(Message::Text(value.to_string().into()))
            .await
            .map_err(|_| Error::Connect("sarvam tts socket closed".into()))
    }

    /// The next meaningful event. `None` when the socket closed.
    ///
    /// # Errors
    ///
    /// [`Error::Provider`] for a server error and [`Error::Protocol`] for an
    /// undecodable frame.
    pub(crate) async fn next(&mut self) -> Option<Result<TtsEvent>> {
        loop {
            let frame = match self.ws.next().await? {
                Ok(Message::Text(text)) => text.as_bytes().to_vec(),
                Ok(Message::Binary(bytes)) => bytes.to_vec(),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => continue,
            };
            return match decode(&frame) {
                Ok(TtsEvent::Ignored) => continue,
                Ok(TtsEvent::Final) => {
                    self.awaiting_final = self.awaiting_final.saturating_sub(1);
                    Some(Ok(TtsEvent::Final))
                }
                Ok(TtsEvent::Error(message)) => Some(Err(Error::Provider(message))),
                other => Some(other),
            };
        }
    }

    /// Closes the socket.
    pub(crate) async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

#[cfg(test)]
#[path = "tts_tests.rs"]
mod tests;
