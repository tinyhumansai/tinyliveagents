//! Unit tests for the standard session vocabulary.

use super::*;
use serde_json::json;

#[test]
fn audio_format_defaults_to_16k_pcm16() {
    let format = AudioFormat::default();
    assert_eq!(format.sample_rate, 16_000);
    assert_eq!(format.bytes_per_second(), 32_000);
    assert_eq!(format.mime_type(), "audio/pcm;rate=16000");
}

#[test]
fn config_builders_set_fields() {
    let config = LiveConfig::new()
        .with_model("m")
        .with_system_instruction("be brief")
        .with_voice("Puck")
        .with_language("en-IN")
        .with_tool(ToolDeclaration::new("t", "d", json!({"type": "object"})))
        .with_provider_options(json!({"k": "v", "n": 1}));
    assert_eq!(config.model.as_deref(), Some("m"));
    assert_eq!(config.system_instruction.as_deref(), Some("be brief"));
    assert_eq!(config.voice.as_deref(), Some("Puck"));
    assert_eq!(config.language.as_deref(), Some("en-IN"));
    assert_eq!(config.tools.len(), 1);
    assert_eq!(config.option_str("k"), Some("v"));
    assert_eq!(config.option_str("n"), None);
    assert_eq!(config.option_str("missing"), None);
    assert!(config.input_transcription && config.output_transcription);
    assert_eq!(config.vad.turn_detection, TurnDetection::Server);
}

#[test]
fn config_round_trips_through_json() {
    let config = LiveConfig::new().with_model("m");
    let value = serde_json::to_value(&config).unwrap();
    assert_eq!(value["input_format"]["sample_rate"], 16_000);
    assert_eq!(value["vad"]["turn_detection"], "server");
    let back: LiveConfig = serde_json::from_value(value).unwrap();
    assert_eq!(back, config);
}

#[test]
fn tool_results_echo_the_call() {
    let call = ToolCall {
        call_id: "c1".into(),
        name: "get_time".into(),
        args: json!({}),
    };
    let ok = ToolResult::ok(&call, json!({"time": "noon"}));
    assert_eq!(ok.call_id, "c1");
    assert_eq!(ok.name, "get_time");
    assert!(!ok.is_error);
    assert_eq!(ok.output_text(), r#"{"time":"noon"}"#);

    let err = ToolResult::error(&call, "denied");
    assert!(err.is_error);
    assert_eq!(err.output_text(), "denied");
}

#[test]
fn capabilities_check_input_rates() {
    let caps = Capabilities {
        native_audio: true,
        tools: true,
        server_vad: true,
        manual_activity: false,
        text_input: true,
        interruptions: true,
        resumption: false,
        input_sample_rates: &[16_000],
    };
    assert!(caps.accepts_input_rate(16_000));
    assert!(!caps.accepts_input_rate(44_100));
}
