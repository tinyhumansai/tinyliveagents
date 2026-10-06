//! Talks to the Sarvam cascade: asks for the time out loud, answers the
//! `get_time` tool call, and saves the spoken reply.
//!
//! ```sh
//! SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example sarvam_tool_call -- [in.wav] [out.wav]
//! ```
//!
//! Without `in.wav` (16 kHz PCM16 mono) the question is synthesized with
//! Sarvam's REST TTS.

use std::path::PathBuf;
use std::time::Duration;

use tinyliveagents::sarvam::SarvamCascade;
use tinyliveagents::{LiveConfig, LiveProvider};
use tinyliveagents_examples::{
    BoxError, converse, env, get_time_tool, read_wav_16k, sarvam_speech, wav_bytes,
};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let key = env("SARVAM_API_KEY").ok_or("set SARVAM_API_KEY")?;
    let mut args = std::env::args().skip(1);
    let input = args.next().filter(|a| !a.is_empty()).map(PathBuf::from);
    let output = PathBuf::from(args.next().unwrap_or_else(|| "sarvam-reply.wav".into()));

    let utterance = match input {
        Some(path) => read_wav_16k(&path).await?,
        None => sarvam_speech(&key, "What time is it in UTC right now?", "en-IN").await?,
    };
    let config = LiveConfig::new()
        .with_language("en-IN")
        .with_system_instruction("You are a concise voice assistant. Use tools to answer questions about the time. Reply in one short sentence.")
        .with_tool(get_time_tool());
    let mut session = SarvamCascade::new(key).connect(config).await?;
    let recording = converse(&mut session, &utterance, true, Duration::from_secs(60)).await?;

    println!("heard: {:?}", recording.heard);
    println!("said: {:?}", recording.said);
    println!("tool calls: {}", recording.tool_calls.len());
    println!(
        "first audio after utterance: {:?} ms",
        recording.first_audio_ms
    );
    println!("errors: {:?}", recording.errors);
    tokio::fs::write(&output, wav_bytes(recording.output_rate, &recording.audio)?).await?;
    println!(
        "wrote {} ({} bytes of audio)",
        output.display(),
        recording.audio.len()
    );
    Ok(())
}
