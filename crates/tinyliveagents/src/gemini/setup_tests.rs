//! Tests for Gemini setup and relay ticket construction.

use super::*;
use crate::types::{ToolDeclaration, TurnDetection};
use serde_json::json;

fn full_config() -> LiveConfig {
    let mut config = LiveConfig::new()
        .with_model("gemini-x")
        .with_system_instruction("be brief")
        .with_voice("Puck")
        .with_language("en-US")
        .with_tool(ToolDeclaration::new(
            "get_time",
            "Current time",
            json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        ))
        .with_provider_options(json!({
            "google_search": true,
            "resumption_handle": "h1",
            "context_window_compression": { "slidingWindow": {} },
            "max_minutes": 5,
            "setup": { "extra": 1 }
        }));
    config.vad.silence_ms = Some(700);
    config
}

#[test]
fn direct_setup_nests_generation_config() {
    let setup = setup_message(&full_config());
    let s = &setup["setup"];
    assert_eq!(s["model"], "models/gemini-x");
    assert_eq!(
        s["generationConfig"]["responseModalities"],
        json!(["AUDIO"])
    );
    assert_eq!(
        s["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
        "Puck"
    );
    assert_eq!(
        s["generationConfig"]["speechConfig"]["languageCode"],
        "en-US"
    );
    assert_eq!(s["systemInstruction"]["parts"][0]["text"], "be brief");
    assert_eq!(
        s["tools"][0]["functionDeclarations"][0]["parameters"],
        json!({ "type": "object", "properties": {} })
    );
    assert_eq!(s["tools"][1], json!({ "googleSearch": {} }));
    assert_eq!(s["inputAudioTranscription"], json!({}));
    assert_eq!(s["outputAudioTranscription"], json!({}));
    assert_eq!(
        s["realtimeInputConfig"]["automaticActivityDetection"]["silenceDurationMs"],
        700
    );
    assert_eq!(s["sessionResumption"]["handle"], "h1");
    assert_eq!(
        s["contextWindowCompression"],
        json!({ "slidingWindow": {} })
    );
    assert_eq!(s["extra"], 1);
    assert!(s.get("maxMinutes").is_none());
    assert!(s.get("responseModalities").is_none());
}

#[test]
fn minimal_setup_uses_defaults_and_keeps_a_prefixed_model() {
    let mut config = LiveConfig::new().with_model("models/already");
    config.input_transcription = false;
    config.output_transcription = false;
    let setup = setup_message(&config);
    let s = &setup["setup"];
    assert_eq!(s["model"], "models/already");
    assert!(s.get("tools").is_none());
    assert!(s.get("speechConfig").is_none());
    assert!(s["generationConfig"].get("speechConfig").is_none());
    assert!(s.get("inputAudioTranscription").is_none());
    assert!(s.get("realtimeInputConfig").is_none());

    let default_model = setup_message(&LiveConfig::new());
    assert_eq!(
        default_model["setup"]["model"],
        format!("models/{DEFAULT_MODEL}")
    );
}

#[test]
fn manual_turn_detection_disables_server_vad() {
    let mut config = LiveConfig::new();
    config.vad.turn_detection = TurnDetection::Manual;
    let setup = setup_message(&config);
    assert_eq!(
        setup["setup"]["realtimeInputConfig"]["automaticActivityDetection"]["disabled"],
        true
    );
}

#[test]
fn ticket_request_is_flat_with_mode_and_minutes() {
    let ticket = ticket_request(&full_config());
    assert_eq!(ticket["mode"], "conversation");
    assert_eq!(ticket["model"], "gemini-x");
    assert_eq!(ticket["maxMinutes"], 5);
    assert_eq!(ticket["responseModalities"], json!(["AUDIO"]));
    assert_eq!(
        ticket["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
        "Puck"
    );
    assert_eq!(
        ticket["tools"][0]["functionDeclarations"][0]["name"],
        "get_time"
    );
    assert!(ticket.get("generationConfig").is_none());

    let default_ticket = ticket_request(&LiveConfig::new());
    assert_eq!(default_ticket["model"], DEFAULT_MODEL);
    assert!(default_ticket.get("maxMinutes").is_none());
}
