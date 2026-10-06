//! Talks to Gemini Live directly with a Google API key.
//!
//! ```sh
//! GEMINI_API_KEY=... SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example gemini_direct -- [out.wav]
//! ```
//!
//! The spoken question comes from `LIVE_TEST_WAV` or Sarvam TTS.

use std::time::Duration;

use tinyliveagents::gemini::GeminiLive;
use tinyliveagents::{LiveConfig, LiveProvider};
use tinyliveagents_examples::{BoxError, converse, env, get_time_tool, question_audio, wav_bytes};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let key = env("GEMINI_API_KEY").ok_or("set GEMINI_API_KEY")?;
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "gemini-reply.wav".into());
    let utterance = question_audio("What time is it in UTC right now?").await?;
    let mut config = LiveConfig::new()
        .with_system_instruction(
            "You are a concise voice assistant. Use tools to answer questions about the time.",
        )
        .with_tool(get_time_tool());
    if let Some(model) = env("GEMINI_LIVE_MODEL") {
        config = config.with_model(model);
    }
    let mut session = GeminiLive::new(key).connect(config).await?;
    let recording = converse(&mut session, &utterance, true, Duration::from_secs(60)).await?;
    println!(
        "{recording:#?}",
        recording = (
            &recording.heard,
            &recording.said,
            &recording.tool_calls,
            &recording.errors
        )
    );
    if !recording.errors.is_empty() {
        return Err(format!("the conversation failed: {:?}", recording.errors).into());
    }
    tokio::fs::write(&output, wav_bytes(recording.output_rate, &recording.audio)?).await?;
    println!("wrote {output}");
    Ok(())
}
