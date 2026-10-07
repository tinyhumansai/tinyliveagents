//! Live tests against real providers. Every test is `#[ignore]`d and skips
//! (passing) when its credential is unset, so they run only on request:
//!
//! ```sh
//! SARVAM_API_KEY=... cargo test -p tinyliveagents-examples --test live -- --ignored --nocapture
//! ```

use std::time::Duration;

use tinyliveagents::gemini::{GeminiLive, GeminiRelay};
use tinyliveagents::sarvam::SarvamCascade;
use tinyliveagents::{LiveConfig, LiveProvider};
use tinyliveagents_examples::{
    BoxError, Recording, audio_source_available, converse, env, get_time_tool, mint_gemini_ticket,
    question_audio,
};

const QUESTION: &str = "What time is it in UTC right now?";
const PROMPT: &str = "You are a concise voice assistant. Use tools to answer questions about the time. Reply in one short sentence.";

fn assert_answered_with_the_tool(recording: &Recording) {
    println!("{recording:?}");
    assert!(
        recording.errors.is_empty(),
        "errors: {:?}",
        recording.errors
    );
    assert!(!recording.tool_calls.is_empty(), "expected a get_time call");
    assert!(recording.tool_calls.iter().all(|c| c.name == "get_time"));
    assert!(!recording.audio.is_empty(), "expected spoken audio");
    // The wording is the model's choice; what matters is that the tool was
    // called, its result went back, and the agent spoke a reply.
    assert!(
        recording.said.iter().any(|s| !s.trim().is_empty()),
        "expected a spoken answer: {:?}",
        recording.said
    );
}

#[tokio::test]
#[ignore = "talks to Sarvam; needs SARVAM_API_KEY"]
async fn live_sarvam_answers_a_spoken_question_with_a_tool() {
    let Some(key) = env("SARVAM_API_KEY") else {
        eprintln!("SARVAM_API_KEY unset; skipping");
        return;
    };
    let audio = question_audio(QUESTION).await.unwrap();
    let config = LiveConfig::new()
        .with_language("en-IN")
        .with_system_instruction(PROMPT)
        .with_tool(get_time_tool());
    let mut session = SarvamCascade::new(key).connect(config).await.unwrap();
    let recording = converse(&mut session, &audio, true, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        recording
            .heard
            .iter()
            .any(|h| h.to_lowercase().contains("time"))
    );
    assert_answered_with_the_tool(&recording);
}

#[tokio::test]
#[ignore = "talks to Sarvam; needs SARVAM_API_KEY"]
async fn live_sarvam_rejects_a_bad_key() {
    if env("SARVAM_API_KEY").is_none() {
        eprintln!("SARVAM_API_KEY unset; skipping");
        return;
    }
    let result = SarvamCascade::new("sk_invalid")
        .connect(LiveConfig::new())
        .await;
    match result {
        Err(error) => println!("refused at connect: {error}"),
        Ok(mut session) => {
            // Some refusals arrive as a close right after the upgrade.
            let mut closed = false;
            while let Some(event) = session.recv().await {
                if let tinyliveagents::LiveEvent::Closed(reason) = event {
                    println!("closed: {reason:?}");
                    assert_eq!(
                        reason,
                        tinyliveagents::CloseReason::Error(tinyliveagents::Error::Unauthorized)
                    );
                    closed = true;
                    break;
                }
            }
            assert!(closed);
        }
    }
}

#[tokio::test]
#[ignore = "talks to Google; needs GEMINI_API_KEY"]
async fn live_gemini_direct_answers_a_spoken_question_with_a_tool() {
    let Some(key) = env("GEMINI_API_KEY") else {
        eprintln!("GEMINI_API_KEY unset; skipping");
        return;
    };
    if !audio_source_available() {
        eprintln!("no LIVE_TEST_WAV or SARVAM_API_KEY to speak the question; skipping");
        return;
    }
    let audio = question_audio(QUESTION).await.unwrap();
    let mut config = LiveConfig::new()
        .with_system_instruction(PROMPT)
        .with_tool(get_time_tool());
    if let Some(model) = env("GEMINI_LIVE_MODEL") {
        config = config.with_model(model);
    }
    let mut session = GeminiLive::new(key).connect(config).await.unwrap();
    let recording = converse(&mut session, &audio, true, Duration::from_secs(60))
        .await
        .unwrap();
    assert_answered_with_the_tool(&recording);
}

#[tokio::test]
#[ignore = "talks to the TinyHumans backend; needs TINYHUMANS_API_KEY"]
async fn live_gemini_relay_answers_a_spoken_question_with_a_tool() {
    if env("TINYHUMANS_API_KEY").is_none() {
        eprintln!("TINYHUMANS_API_KEY unset; skipping");
        return;
    }
    if !audio_source_available() {
        eprintln!("no LIVE_TEST_WAV or SARVAM_API_KEY to speak the question; skipping");
        return;
    }
    let audio = question_audio(QUESTION).await.unwrap();
    let config = LiveConfig::new()
        .with_system_instruction(PROMPT)
        .with_tool(get_time_tool())
        .with_provider_options(serde_json::json!({ "max_minutes": 2 }));
    let (url, session_id, model) = mint_gemini_ticket(&config).await.unwrap();
    let provider = GeminiRelay::connect_url(url).with_session(session_id, model);
    let mut session = provider.connect(config).await.unwrap();
    let recording = converse(&mut session, &audio, true, Duration::from_secs(60))
        .await
        .unwrap();
    assert_answered_with_the_tool(&recording);
}

#[tokio::test]
#[ignore = "talks to Sarvam; needs SARVAM_API_KEY"]
async fn live_sarvam_accepts_automatic_language_detection() -> Result<(), BoxError> {
    let Some(key) = env("SARVAM_API_KEY") else {
        eprintln!("SARVAM_API_KEY unset; skipping");
        return Ok(());
    };
    let audio = question_audio(QUESTION).await?;
    let config = LiveConfig::new()
        .with_system_instruction(PROMPT)
        .with_tool(get_time_tool())
        .with_provider_options(serde_json::json!({ "auto_language": true }));
    let mut session = SarvamCascade::new(key).connect(config).await?;
    let recording = converse(&mut session, &audio, true, Duration::from_secs(60)).await?;
    // `language_code=auto` was accepted: speech was recognised and answered.
    assert!(
        recording
            .heard
            .iter()
            .any(|h| h.to_lowercase().contains("time")),
        "{recording:?}"
    );
    assert_answered_with_the_tool(&recording);
    Ok(())
}
