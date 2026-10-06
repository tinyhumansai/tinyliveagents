//! Unit tests for the shared configuration checks.

use super::*;
use crate::types::{AudioFormat, ToolDeclaration};
use serde_json::json;

const CAPS: Capabilities = Capabilities {
    native_audio: true,
    tools: true,
    server_vad: true,
    manual_activity: true,
    text_input: true,
    interruptions: true,
    resumption: false,
    input_sample_rates: &[16_000],
};

#[test]
fn accepts_a_default_config() {
    assert!(validate_common(&LiveConfig::new(), &CAPS).is_ok());
}

#[test]
fn rejects_an_unsupported_rate() {
    let mut config = LiveConfig::new();
    config.input_format = AudioFormat::pcm16(44_100);
    assert!(validate_common(&config, &CAPS).is_err());
}

#[test]
fn rejects_unnamed_tools() {
    let config = LiveConfig::new().with_tool(ToolDeclaration::new(" ", "d", json!({})));
    assert!(validate_common(&config, &CAPS).is_err());
}

#[test]
fn rejects_tools_on_a_toolless_provider() {
    let caps = Capabilities {
        tools: false,
        ..CAPS
    };
    let config = LiveConfig::new().with_tool(ToolDeclaration::new("t", "d", json!({})));
    assert!(validate_common(&config, &caps).is_err());
}
