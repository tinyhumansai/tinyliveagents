//! Building Gemini Live session setup from a [`LiveConfig`].
//!
//! Two shapes come out of one configuration:
//!
//! - [`setup_message`]: the `setup` frame a direct connection sends first.
//! - [`ticket_request`]: the body of a relay's "create live session" call,
//!   where the setup is fixed when the ticket is minted (the TinyHumans backend
//!   relay works this way). Its fields are the setup's fields at the top level,
//!   plus `mode: "conversation"`.
//!
//! Recognised `provider_options` keys:
//!
//! - `google_search` (bool): add Google Search grounding.
//! - `resumption_handle` (string): resume an earlier session.
//! - `context_window_compression` (object): passed through verbatim.
//! - `max_minutes` (integer): relay session cap (ticket only).
//! - `setup` (object): merged last over the generated setup, for anything this
//!   crate does not model.

use serde_json::{Map, Value, json};

use super::schema::clean_schema;
use crate::types::{LiveConfig, TurnDetection};

/// The default Live model when the configuration names none.
pub const DEFAULT_MODEL: &str = "gemini-3.8-live";

/// The setup fields shared by the direct frame and the relay ticket, keyed in
/// Gemini's camelCase.
fn setup_fields(config: &LiveConfig) -> Map<String, Value> {
    let mut setup = Map::new();
    setup.insert("responseModalities".into(), json!(["AUDIO"]));

    let mut speech = Map::new();
    if let Some(voice) = &config.voice {
        speech.insert(
            "voiceConfig".into(),
            json!({ "prebuiltVoiceConfig": { "voiceName": voice } }),
        );
    }
    if let Some(language) = &config.language {
        speech.insert("languageCode".into(), json!(language));
    }
    if !speech.is_empty() {
        setup.insert("speechConfig".into(), Value::Object(speech));
    }

    if let Some(text) = &config.system_instruction {
        setup.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": text }] }),
        );
    }

    let mut tools = Vec::new();
    if !config.tools.is_empty() {
        let declarations: Vec<Value> = config
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": clean_schema(&tool.parameters),
                })
            })
            .collect();
        tools.push(json!({ "functionDeclarations": declarations }));
    }
    if config
        .provider_options
        .get("google_search")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        tools.push(json!({ "googleSearch": {} }));
    }
    if !tools.is_empty() {
        setup.insert("tools".into(), Value::Array(tools));
    }

    if config.input_transcription {
        setup.insert("inputAudioTranscription".into(), json!({}));
    }
    if config.output_transcription {
        setup.insert("outputAudioTranscription".into(), json!({}));
    }

    let mut detection = Map::new();
    match config.vad.turn_detection {
        TurnDetection::Manual => {
            detection.insert("disabled".into(), json!(true));
        }
        TurnDetection::Server => {
            if let Some(ms) = config.vad.silence_ms {
                detection.insert("silenceDurationMs".into(), json!(ms));
            }
        }
    }
    if !detection.is_empty() {
        setup.insert(
            "realtimeInputConfig".into(),
            json!({ "automaticActivityDetection": detection }),
        );
    }

    if let Some(handle) = config.option_str("resumption_handle") {
        setup.insert("sessionResumption".into(), json!({ "handle": handle }));
    }
    if let Some(compression) = config.provider_options.get("context_window_compression") {
        setup.insert("contextWindowCompression".into(), compression.clone());
    }
    setup
}

fn merge_overrides(target: &mut Map<String, Value>, config: &LiveConfig) {
    if let Some(Value::Object(extra)) = config.provider_options.get("setup") {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// The model id with Gemini's `models/` prefix.
fn qualified_model(config: &LiveConfig) -> String {
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    if model.starts_with("models/") {
        model.to_string()
    } else {
        format!("models/{model}")
    }
}

/// The `setup` frame for a direct connection.
#[must_use]
pub fn setup_message(config: &LiveConfig) -> Value {
    let mut fields = setup_fields(config);
    // On the direct wire, modalities and speech live under generationConfig.
    let mut generation = Map::new();
    if let Some(modalities) = fields.remove("responseModalities") {
        generation.insert("responseModalities".into(), modalities);
    }
    if let Some(speech) = fields.remove("speechConfig") {
        generation.insert("speechConfig".into(), speech);
    }
    fields.insert("generationConfig".into(), Value::Object(generation));
    fields.insert("model".into(), json!(qualified_model(config)));
    merge_overrides(&mut fields, config);
    json!({ "setup": fields })
}

/// The body of a relay's create-live-session call (`mode: "conversation"`).
#[must_use]
pub fn ticket_request(config: &LiveConfig) -> Value {
    let mut fields = setup_fields(config);
    fields.insert("mode".into(), json!("conversation"));
    fields.insert(
        "model".into(),
        json!(config.model.as_deref().unwrap_or(DEFAULT_MODEL)),
    );
    if let Some(minutes) = config
        .provider_options
        .get("max_minutes")
        .and_then(Value::as_u64)
    {
        fields.insert("maxMinutes".into(), json!(minutes));
    }
    merge_overrides(&mut fields, config);
    Value::Object(fields)
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
