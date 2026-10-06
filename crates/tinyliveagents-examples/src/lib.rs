//! Shared helpers for the tinyliveagents examples and live tests.
//!
//! - WAV reading and writing (PCM16 mono only, no dependency).
//! - [`sarvam_speech`]: synthesizes a spoken prompt with Sarvam's REST TTS, so
//!   live tests can talk to a provider without a microphone or a fixture file.
//! - [`converse`]: plays an utterance into a session in real time, answers the
//!   demo `get_time` tool, and records what came back.
//!
//! Nothing here is used by the library itself.

#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde_json::{Value, json};
use tinyliveagents::{LiveEvent, LiveSession, ToolCall, ToolDeclaration, ToolResult};

/// A boxed error for example code.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Reads the value of `name`, or `None` when it is unset or empty.
#[must_use]
pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Extracts PCM16 mono samples and the sample rate from a WAV file's bytes.
pub fn parse_wav(bytes: &[u8]) -> Result<(u32, Vec<u8>), BoxError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a wav file".into());
    }
    let mut offset = 12;
    let mut rate = 0;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        let body = offset + 8;
        if id == b"fmt " {
            let channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into()?);
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into()?);
            let bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into()?);
            if channels != 1 || bits != 16 {
                return Err("only 16-bit mono wav is supported".into());
            }
        } else if id == b"data" {
            let end = (body + size).min(bytes.len());
            return Ok((rate, bytes[body..end].to_vec()));
        }
        offset = body + size + (size & 1);
    }
    Err("wav has no data chunk".into())
}

/// Reads a PCM16 mono WAV file.
pub async fn read_wav(path: &Path) -> Result<(u32, Vec<u8>), BoxError> {
    parse_wav(&tokio::fs::read(path).await?)
}

/// Encodes PCM16 mono audio as a WAV file.
#[must_use]
pub fn wav_bytes(rate: u32, pcm: &[u8]) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(pcm.len() + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// Synthesizes `text` as 16 kHz PCM16 with Sarvam's REST TTS.
pub async fn sarvam_speech(api_key: &str, text: &str, language: &str) -> Result<Vec<u8>, BoxError> {
    let response: Value = reqwest::Client::new()
        .post("https://api.sarvam.ai/text-to-speech")
        .header("api-subscription-key", api_key)
        .json(&json!({
            "text": text,
            "target_language_code": language,
            "model": "bulbul:v3",
            "speaker": "priya",
            "speech_sample_rate": 16000,
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let audio = response
        .pointer("/audios/0")
        .and_then(Value::as_str)
        .ok_or("tts response has no audio")?;
    let (rate, pcm) = parse_wav(&B64.decode(audio)?)?;
    if rate != 16_000 {
        return Err(format!("expected 16 kHz speech, got {rate}").into());
    }
    Ok(pcm)
}

/// The demo tool every example declares.
#[must_use]
pub fn get_time_tool() -> ToolDeclaration {
    ToolDeclaration::new(
        "get_time",
        "Returns the current time in a timezone. Always call this to answer questions about the time.",
        json!({
            "type": "object",
            "properties": { "timezone": { "type": "string", "description": "IANA timezone, e.g. UTC" } },
            "required": ["timezone"]
        }),
    )
}

/// The demo tool's fixed answer.
#[must_use]
pub fn answer_get_time(call: &ToolCall) -> ToolResult {
    ToolResult::ok(
        call,
        json!({ "time": "14:05", "timezone": call.args["timezone"] }),
    )
}

/// What came back from a conversation.
#[derive(Default)]
pub struct Recording {
    /// Final user transcripts.
    pub heard: Vec<String>,
    /// Final agent transcripts.
    pub said: Vec<String>,
    /// Tool calls the agent made.
    pub tool_calls: Vec<ToolCall>,
    /// The agent's audio, concatenated.
    pub audio: Vec<u8>,
    /// The agent's output rate.
    pub output_rate: u32,
    /// Turns completed.
    pub turns: usize,
    /// Errors reported along the way.
    pub errors: Vec<String>,
    /// Milliseconds from the end of the utterance to the first agent audio.
    pub first_audio_ms: Option<u128>,
}

impl std::fmt::Debug for Recording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recording")
            .field("heard", &self.heard)
            .field("said", &self.said)
            .field("tool_calls", &self.tool_calls)
            .field("audio_bytes", &self.audio.len())
            .field("output_rate", &self.output_rate)
            .field("turns", &self.turns)
            .field("errors", &self.errors)
            .field("first_audio_ms", &self.first_audio_ms)
            .finish()
    }
}

/// Plays `utterance` (16 kHz PCM16) into `session` in 100 ms frames at real
/// time, then silence, answering `get_time` calls, until a turn completes after
/// a tool call (or any turn when `expect_tool` is false) or `timeout` passes.
pub async fn converse(
    session: &mut LiveSession,
    utterance: &[u8],
    expect_tool: bool,
    timeout: Duration,
) -> Result<Recording, BoxError> {
    let sender = session.sender();
    let mut recording = Recording::default();

    match session.recv().await {
        Some(LiveEvent::Ready(info)) => recording.output_rate = info.output_format.sample_rate,
        other => return Err(format!("expected ready, got {other:?}").into()),
    }

    // Stream the utterance, then two seconds of silence so server VAD ends the
    // turn, then keep a trickle of silence going like a live microphone.
    let frame = 3_200;
    let mut frames: Vec<Bytes> = utterance
        .chunks(frame)
        .map(|c| Bytes::copy_from_slice(c))
        .collect();
    frames.extend(std::iter::repeat_n(Bytes::from(vec![0_u8; frame]), 20));
    let pump_sender = sender.clone();
    let pump = tokio::spawn(async move {
        for chunk in frames {
            if pump_sender.send_audio(chunk).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        loop {
            if pump_sender.send_audio(vec![0_u8; frame]).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    #[allow(clippy::cast_possible_truncation)]
    let utterance_ms = (utterance.len() / 32) as u64;
    let utterance_end = Instant::now() + Duration::from_millis(utterance_ms);

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let Ok(event) = tokio::time::timeout_at(deadline, session.recv()).await else {
            recording.errors.push("timed out".into());
            break;
        };
        let Some(event) = event else { break };
        match event {
            LiveEvent::Audio(audio) => {
                if recording.first_audio_ms.is_none() {
                    recording.first_audio_ms = Some(
                        Instant::now()
                            .saturating_duration_since(utterance_end)
                            .as_millis(),
                    );
                }
                recording.audio.extend_from_slice(&audio);
            }
            LiveEvent::InputTranscript {
                text,
                is_final: true,
            } => recording.heard.push(text),
            LiveEvent::OutputTranscript {
                text,
                is_final: true,
            } => recording.said.push(text),
            LiveEvent::ToolCall(call) => {
                println!("tool call: {} {}", call.name, call.args);
                let result = answer_get_time(&call);
                recording.tool_calls.push(call);
                sender.send_tool_result(result).await?;
            }
            LiveEvent::TurnComplete { .. } => {
                recording.turns += 1;
                if !expect_tool || !recording.tool_calls.is_empty() {
                    break;
                }
            }
            LiveEvent::Error { error, .. } => recording.errors.push(error.to_string()),
            LiveEvent::Closed(reason) => {
                recording.errors.push(format!("closed: {reason:?}"));
                break;
            }
            _ => {}
        }
    }
    pump.abort();
    let _ = sender.close().await;
    Ok(recording)
}

/// The spoken question the examples and live tests ask: `LIVE_TEST_WAV` (a
/// 16 kHz PCM16 mono file) when set, otherwise synthesized with Sarvam TTS.
pub async fn question_audio(text: &str) -> Result<Vec<u8>, BoxError> {
    if let Some(path) = env("LIVE_TEST_WAV") {
        let (rate, pcm) = read_wav(Path::new(&path)).await?;
        if rate != 16_000 {
            return Err("LIVE_TEST_WAV must be 16 kHz".into());
        }
        return Ok(pcm);
    }
    let key = env("SARVAM_API_KEY").ok_or("set LIVE_TEST_WAV or SARVAM_API_KEY")?;
    sarvam_speech(&key, text, "en-IN").await
}

/// Calls a TinyHumans backend route with `TINYHUMANS_API_KEY` and returns the
/// response's `data` (or the whole body when unwrapped).
pub async fn tinyhumans(
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, BoxError> {
    let base = env("TINYHUMANS_BASE_URL").unwrap_or_else(|| "https://api.tinyhumans.ai".into());
    let key = env("TINYHUMANS_API_KEY").ok_or("set TINYHUMANS_API_KEY")?;
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", base.trim_end_matches('/')))
        .header("x-api-key", key);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response: Value = request.send().await?.error_for_status()?.json().await?;
    Ok(response.get("data").cloned().unwrap_or(response))
}

/// Mints a Gemini Live relay ticket for `config` and returns `(ws_url,
/// session_id, model)`.
pub async fn mint_gemini_ticket(
    config: &tinyliveagents::LiveConfig,
) -> Result<(String, String, String), BoxError> {
    let body = tinyliveagents::gemini::ticket_request(config);
    let ticket = tinyhumans(
        reqwest::Method::POST,
        "/agent-integrations/gemini/live/sessions",
        Some(body),
    )
    .await?;
    let field = |name: &str| {
        ticket
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("ticket has no {name}"))
    };
    Ok((field("wsUrl")?, field("sessionId")?, field("model")?))
}
