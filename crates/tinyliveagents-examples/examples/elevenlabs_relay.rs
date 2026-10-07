//! Joins the TinyHumans-hosted ElevenLabs agent through a backend-minted
//! signed URL — the flow OpenHuman's voice mode used to run in the browser.
//!
//! ```sh
//! TINYHUMANS_API_KEY=... SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example elevenlabs_relay
//! ```

use std::time::Duration;

use serde_json::{Value, json};
use tinyliveagents::elevenlabs::ElevenLabsConvai;
use tinyliveagents::{LiveConfig, LiveProvider};
use tinyliveagents_examples::{BoxError, converse, question_audio, tinyhumans};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let utterance = question_audio("Hello! Who am I talking to?").await?;
    let signed = tinyhumans(reqwest::Method::GET, "/voice-agent/get-signed-url", None).await?;
    let url = signed
        .get("signedUrl")
        .and_then(Value::as_str)
        .ok_or("no signedUrl")?;
    let token = signed
        .get("userToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let config = LiveConfig::new().with_provider_options(json!({
        "user_id": token,
        "custom_llm_extra_body": { "user": token },
    }));
    let mut session = ElevenLabsConvai::connect_url(url).connect(config).await?;
    let recording = converse(&mut session, &utterance, false, Duration::from_secs(60)).await?;
    println!(
        "heard {:?}\nsaid {:?}\nerrors {:?}",
        recording.heard, recording.said, recording.errors
    );
    if !recording.errors.is_empty() {
        return Err(format!("the conversation failed: {:?}", recording.errors).into());
    }
    Ok(())
}
