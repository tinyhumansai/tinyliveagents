//! Talks to Gemini Live through the TinyHumans backend's metered relay.
//!
//! ```sh
//! TINYHUMANS_API_KEY=... SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example gemini_relay
//! ```
//!
//! The host (this example) mints the ticket from the same `LiveConfig` the
//! session uses; the library only speaks the relay's WebSocket.

use std::time::Duration;

use tinyliveagents::gemini::GeminiRelay;
use tinyliveagents::{LiveConfig, LiveProvider};
use tinyliveagents_examples::{
    BoxError, converse, get_time_tool, mint_gemini_ticket, question_audio,
};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let utterance = question_audio("What time is it in UTC right now?").await?;
    let config = LiveConfig::new()
        .with_system_instruction(
            "You are a concise voice assistant. Use tools to answer questions about the time.",
        )
        .with_tool(get_time_tool())
        .with_provider_options(serde_json::json!({ "max_minutes": 2 }));
    let (ws_url, session_id, model) = mint_gemini_ticket(&config).await?;
    let provider = GeminiRelay::connect_url(ws_url).with_session(session_id, model);
    let mut session = provider.connect(config).await?;
    let recording = converse(&mut session, &utterance, true, Duration::from_secs(60)).await?;
    println!(
        "heard {:?}\nsaid {:?}\ntools {:?}\nerrors {:?}",
        recording.heard, recording.said, recording.tool_calls, recording.errors
    );
    if !recording.errors.is_empty() {
        return Err(format!("the conversation failed: {:?}", recording.errors).into());
    }
    Ok(())
}
