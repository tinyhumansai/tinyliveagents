//! Shared helpers for the tinyliveagents examples and live tests.
//!
//! - WAV reading and writing (PCM16 mono only, no dependency).
//! - [`sarvam_speech`]: synthesizes a spoken prompt with Sarvam's REST TTS, so
//!   live tests can talk to a provider without a microphone or a fixture file.
//! - [`converse`]: plays an utterance into a session in real time, answers the
//!   demo `get_time` tool, and records what came back.
//!
//! Nothing here is used by the library itself.

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

/// Reads `len` bytes at `at`, or fails when they run past the buffer.
fn field<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], BoxError> {
    at.checked_add(N)
        .and_then(|end| bytes.get(at..end))
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| "wav chunk runs past the end of the file".into())
}

/// Extracts PCM16 mono samples and the sample rate from a WAV file's bytes.
///
/// A `data` chunk whose declared size is the streaming placeholder (`0` or
/// `0xFFFFFFFF`) runs to the end of the buffer; any other size must fit.
///
/// # Errors
///
/// When the bytes are not a RIFF/WAVE file, a chunk is truncated, the format
/// is not 16-bit mono integer PCM, or there is no `data` chunk.
pub fn parse_wav(bytes: &[u8]) -> Result<(u32, Vec<u8>), BoxError> {
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a wav file".into());
    }
    let mut offset = 12_usize;
    let mut rate = None;
    while offset < bytes.len() {
        let id: [u8; 4] = field(bytes, offset)?;
        let size = u32::from_le_bytes(field(bytes, offset + 4)?);
        let body = offset + 8;
        if &id == b"fmt " {
            if size < 16 {
                return Err("wav fmt chunk is too short".into());
            }
            let format = u16::from_le_bytes(field(bytes, body)?);
            let channels = u16::from_le_bytes(field(bytes, body + 2)?);
            let sample_rate = u32::from_le_bytes(field(bytes, body + 4)?);
            let bits = u16::from_le_bytes(field(bytes, body + 14)?);
            if format != 1 || channels != 1 || bits != 16 {
                return Err("only 16-bit mono integer PCM wav is supported".into());
            }
            rate = Some(sample_rate);
        } else if &id == b"data" {
            let rate = rate.ok_or("wav data chunk comes before its fmt chunk")?;
            let end = if size == 0 || size == u32::MAX {
                bytes.len()
            } else {
                body.checked_add(size as usize)
                    .filter(|end| *end <= bytes.len())
                    .ok_or("wav data chunk is truncated")?
            };
            return Ok((
                rate,
                bytes
                    .get(body..end)
                    .ok_or("wav data chunk is truncated")?
                    .to_vec(),
            ));
        }
        let padded = (size as usize).checked_add(size as usize & 1);
        offset = padded
            .and_then(|len| body.checked_add(len))
            .ok_or("wav chunk size overflows")?;
    }
    Err("wav has no data chunk".into())
}

/// Reads a PCM16 mono WAV file.
///
/// # Errors
///
/// When the file cannot be read or [`parse_wav`] rejects it.
pub async fn read_wav(path: &Path) -> Result<(u32, Vec<u8>), BoxError> {
    parse_wav(&tokio::fs::read(path).await?)
}

/// Reads a WAV file that must be 16 kHz PCM16 mono, the rate the examples
/// stream at.
///
/// # Errors
///
/// When [`read_wav`] fails or the file is not 16 kHz.
pub async fn read_wav_16k(path: &Path) -> Result<Vec<u8>, BoxError> {
    let (rate, pcm) = read_wav(path).await?;
    if rate != 16_000 {
        return Err(format!(
            "{} is {rate} Hz; 16 kHz PCM16 mono is required",
            path.display()
        )
        .into());
    }
    Ok(pcm)
}

/// Encodes PCM16 mono audio as a WAV file.
///
/// # Errors
///
/// When the audio has an odd length, or it or the byte rate does not fit a
/// WAV header's 32-bit fields.
pub fn wav_bytes(rate: u32, pcm: &[u8]) -> Result<Vec<u8>, BoxError> {
    if pcm.len() % 2 != 0 {
        return Err("PCM16 audio must have an even number of bytes".into());
    }
    let data_len = u32::try_from(pcm.len()).map_err(|_| "audio is too long for a wav file")?;
    let riff_len = data_len
        .checked_add(36)
        .ok_or("audio is too long for a wav file")?;
    let byte_rate = rate
        .checked_mul(2)
        .ok_or("sample rate is too high for a wav file")?;
    let mut out = Vec::with_capacity(pcm.len() + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    Ok(out)
}

/// Synthesizes `text` as 16 kHz PCM16 with Sarvam's REST TTS.
///
/// # Errors
///
/// When the request fails, Sarvam refuses it, or the audio is not 16 kHz.
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
        "Returns the current time in UTC (HH:MM). Always call this to answer questions about the time.",
        json!({
            "type": "object",
            "properties": { "timezone": { "type": "string", "description": "Only UTC is supported" } }
        }),
    )
}

/// The current UTC time as `HH:MM`, from the system clock.
#[must_use]
pub fn utc_hh_mm() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let minutes_today = (seconds % 86_400) / 60;
    format!("{:02}:{:02}", minutes_today / 60, minutes_today % 60)
}

/// The demo tool's answer: the real current UTC time.
#[must_use]
pub fn answer_get_time(call: &ToolCall) -> ToolResult {
    ToolResult::ok(call, json!({ "time": utc_hh_mm(), "timezone": "UTC" }))
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

/// How long to keep listening after the first reply text when the provider
/// has no turn-complete event (ElevenLabs), so the reply audio arrives.
pub const REPLY_SETTLE: Duration = Duration::from_secs(6);

/// Plays `utterance` (16 kHz PCM16) into `session` in 100 ms frames at real
/// time, then silence, answering `get_time` calls, until a turn completes after
/// a tool call (or any turn when `expect_tool` is false) or `timeout` passes.
///
/// # Errors
///
/// When the first event is not `Ready` or a tool result cannot be sent.
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
        .map(Bytes::copy_from_slice)
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

    let mut deadline = tokio::time::Instant::now() + timeout;
    // Set once reply text arrives on a turn we are not waiting a tool for:
    // providers without a turn-complete event end there.
    let mut settling = false;
    let mut last_partial: Option<String> = None;
    loop {
        let Ok(event) = tokio::time::timeout_at(deadline, session.recv()).await else {
            if !settling {
                recording.errors.push("timed out".into());
            }
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
            LiveEvent::OutputTranscript { text, is_final } => {
                if !expect_tool && !settling {
                    settling = true;
                    deadline = deadline.min(tokio::time::Instant::now() + REPLY_SETTLE);
                }
                if is_final {
                    recording.said.push(text);
                    last_partial = None;
                } else {
                    last_partial = Some(text);
                }
            }
            LiveEvent::ToolCall(call) => {
                println!("tool call: {} {}", call.name, call.args);
                let result = answer_get_time(&call);
                recording.tool_calls.push(call);
                if let Err(error) = sender.send_tool_result(result).await {
                    pump.abort();
                    return Err(error.into());
                }
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
    // A reply still open when we stopped listening counts as said.
    if let Some(text) = last_partial {
        recording.said.push(text);
    }
    pump.abort();
    let _ = sender.close().await;
    Ok(recording)
}

/// Whether [`question_audio`] has a source: `LIVE_TEST_WAV` or
/// `SARVAM_API_KEY`. Live tests skip when it has none.
#[must_use]
pub fn audio_source_available() -> bool {
    env("LIVE_TEST_WAV").is_some() || env("SARVAM_API_KEY").is_some()
}

/// The spoken question the examples and live tests ask: `LIVE_TEST_WAV` (a
/// 16 kHz PCM16 mono file) when set, otherwise synthesized with Sarvam TTS.
///
/// # Errors
///
/// When neither source is configured, the file is unusable, or synthesis
/// fails.
pub async fn question_audio(text: &str) -> Result<Vec<u8>, BoxError> {
    if let Some(path) = env("LIVE_TEST_WAV") {
        return read_wav_16k(Path::new(&path)).await;
    }
    let key = env("SARVAM_API_KEY").ok_or("set LIVE_TEST_WAV or SARVAM_API_KEY")?;
    sarvam_speech(&key, text, "en-IN").await
}

/// Calls a TinyHumans backend route with `TINYHUMANS_API_KEY` and returns the
/// response's `data` (or the whole body when unwrapped).
///
/// # Errors
///
/// When the key is unset, the request fails, or the backend refuses it.
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
///
/// # Errors
///
/// When minting fails or the ticket lacks one of those fields.
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

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
