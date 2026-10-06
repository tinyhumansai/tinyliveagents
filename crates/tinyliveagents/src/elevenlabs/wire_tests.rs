//! Frame-level tests for the ElevenLabs codec.

use super::*;
use crate::types::ToolResult;

fn text_of(message: &Message) -> Value {
    match message {
        Message::Text(text) => serde_json::from_str(text).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn decode(codec: &mut ElevenLabsCodec, value: &Value) -> Vec<Decoded> {
    codec.decode(value.to_string().as_bytes()).unwrap()
}

fn event(codec: &mut ElevenLabsCodec, value: &Value) -> LiveEvent {
    match decode(codec, value).pop() {
        Some(Decoded::Event(event)) => event,
        other => panic!("expected an event, got {other:?}"),
    }
}

#[test]
fn parses_agent_formats() {
    assert_eq!(parse_format("pcm_16000"), Some(AgentFormat::Pcm(16_000)));
    assert_eq!(parse_format("pcm_44100"), Some(AgentFormat::Pcm(44_100)));
    assert_eq!(parse_format("ulaw_8000"), Some(AgentFormat::Ulaw8k));
    assert_eq!(AgentFormat::Ulaw8k.rate(), 8_000);
    assert_eq!(parse_format("pcm_x"), None);
    assert_eq!(parse_format("pcm_1"), None);
    assert_eq!(parse_format("mp3_44100"), None);
}

#[test]
fn initiation_sends_only_what_is_set() {
    let bare = initiation_message(&LiveConfig::new());
    assert_eq!(bare, json!({"type": "conversation_initiation_client_data"}));

    let mut config = LiveConfig::new()
        .with_voice("voice-1")
        .with_system_instruction("p")
        .with_language("en")
        .with_provider_options(json!({
            "user_id": "u1",
            "custom_llm_extra_body": {"user": "u1"},
            "dynamic_variables": {"name": "Sam"},
            "ignored": 1
        }));
    config.first_message = Some("Hi".into());
    let full = initiation_message(&config);
    assert_eq!(
        full,
        json!({
            "type": "conversation_initiation_client_data",
            "conversation_config_override": {
                "agent": {"prompt": {"prompt": "p"}, "first_message": "Hi", "language": "en"},
                "tts": {"voice_id": "voice-1"}
            },
            "user_id": "u1",
            "custom_llm_extra_body": {"user": "u1"},
            "dynamic_variables": {"name": "Sam"}
        })
    );
}

#[test]
fn opens_with_the_initiation_frame() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new().with_voice("v"));
    let (frames, events) = codec.on_open();
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(
        text_of(&frames[0])["conversation_config_override"]["tts"]["voice_id"],
        "v"
    );
}

#[test]
fn becomes_ready_with_the_agent_formats_and_resamples_input() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    let ready = event(
        &mut codec,
        &json!({
            "type": "conversation_initiation_metadata",
            "conversation_initiation_metadata_event": {
                "conversation_id": "conv-1",
                "agent_output_audio_format": "pcm_24000",
                "user_input_audio_format": "pcm_8000"
            }
        }),
    );
    let LiveEvent::Ready(info) = ready else {
        panic!("expected ready")
    };
    assert_eq!(info.session_id.as_deref(), Some("conv-1"));
    assert_eq!(info.output_format.sample_rate, 24_000);
    assert_eq!(info.input_format.sample_rate, 16_000);

    // Four 16 kHz samples become two 8 kHz samples (four bytes).
    let frame = codec.encode(ClientCommand::Audio(Bytes::from_static(&[0; 8])));
    let chunk = text_of(&frame[0])["user_audio_chunk"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(B64.decode(chunk).unwrap().len(), 4);
}

#[test]
fn ready_defaults_when_metadata_is_missing() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    let LiveEvent::Ready(info) = event(
        &mut codec,
        &json!({"type": "conversation_initiation_metadata"}),
    ) else {
        panic!("expected ready")
    };
    assert_eq!(info.output_format.sample_rate, 16_000);
    assert_eq!(info.session_id, None);
}

#[test]
fn encodes_commands() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    assert_eq!(
        text_of(&codec.encode(ClientCommand::Audio(Bytes::from_static(&[1, 2])))[0]),
        json!({"user_audio_chunk": "AQI="})
    );
    assert_eq!(
        text_of(&codec.encode(ClientCommand::Text("hi".into()))[0]),
        json!({"type": "user_message", "text": "hi"})
    );
    let result = ToolResult {
        call_id: "t1".into(),
        name: "f".into(),
        output: json!({"a": 1}),
        is_error: false,
    };
    assert_eq!(
        text_of(&codec.encode(ClientCommand::ToolResult(result))[0]),
        json!({"type": "client_tool_result", "tool_call_id": "t1", "result": "{\"a\":1}", "is_error": false})
    );
    assert_eq!(
        text_of(&codec.encode(ClientCommand::ActivityStart)[0]),
        json!({"type": "user_activity"})
    );
    assert!(
        codec.encode(ClientCommand::ActivityEnd).is_empty(),
        "expected nothing"
    );
    assert!(
        codec.encode(ClientCommand::Interrupt).is_empty(),
        "expected nothing"
    );
    assert!(
        codec.encode(ClientCommand::Close).is_empty(),
        "expected nothing"
    );
}

#[test]
fn decodes_server_events() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    assert_eq!(
        event(
            &mut codec,
            &json!({"type": "audio", "audio_event": {"audio_base_64": "AQI=", "event_id": 1}})
        ),
        LiveEvent::Audio(Bytes::from_static(&[1, 2]))
    );
    assert_eq!(
        event(
            &mut codec,
            &json!({"type": "tentative_user_transcript", "tentative_user_transcription_event": {"user_transcript": "wha"}})
        ),
        LiveEvent::InputTranscript {
            text: "wha".into(),
            is_final: false
        }
    );
    assert_eq!(
        event(
            &mut codec,
            &json!({"type": "user_transcript", "user_transcription_event": {"user_transcript": "what time"}})
        ),
        LiveEvent::InputTranscript {
            text: "what time".into(),
            is_final: true
        }
    );
    assert_eq!(
        event(
            &mut codec,
            &json!({"type": "agent_response", "agent_response_event": {"agent_response": "Noon."}})
        ),
        LiveEvent::OutputTranscript {
            text: "Noon.".into(),
            is_final: false
        }
    );
    assert_eq!(
        event(
            &mut codec,
            &json!({"type": "agent_response_correction", "agent_response_correction_event": {"corrected_agent_response": "No"}})
        ),
        LiveEvent::OutputTranscript {
            text: "No".into(),
            is_final: true
        }
    );
    assert_eq!(
        event(
            &mut codec,
            &json!({"type": "interruption", "interruption_event": {"event_id": 2}})
        ),
        LiveEvent::Interrupted
    );
    assert_eq!(
        event(
            &mut codec,
            &json!({"type": "client_tool_call", "client_tool_call": {"tool_name": "get_time", "tool_call_id": "t1", "parameters": {"tz": "UTC"}}})
        ),
        LiveEvent::ToolCall(ToolCall {
            call_id: "t1".into(),
            name: "get_time".into(),
            args: json!({"tz": "UTC"})
        })
    );
    assert_eq!(
        event(&mut codec, &json!({"type": "error", "message": "bad"})),
        LiveEvent::Error {
            error: Error::Provider("bad".into()),
            fatal: false
        }
    );
    assert_eq!(
        event(&mut codec, &json!({"type": "error"})),
        LiveEvent::Error {
            error: Error::Provider("unknown error".into()),
            fatal: false
        }
    );
    assert!(
        decode(&mut codec, &json!({"type": "vad_score"})).is_empty(),
        "expected nothing"
    );
}

#[test]
fn answers_pings_with_pongs() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    let reply = decode(
        &mut codec,
        &json!({"type": "ping", "ping_event": {"event_id": 9, "ping_ms": 50}}),
    );
    match &reply[0] {
        Decoded::Reply(frame) => assert_eq!(text_of(frame), json!({"type": "pong", "event_id": 9})),
        Decoded::Event(event) => panic!("expected a reply, got {event:?}"),
    }
}

#[test]
fn rejects_non_json_and_bad_audio() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    assert!(matches!(codec.decode(b"nope"), Err(Error::Protocol(_))));
    let bad = json!({"type": "audio", "audio_event": {"audio_base_64": "!!"}});
    assert!(matches!(
        codec.decode(bad.to_string().as_bytes()),
        Err(Error::Protocol(_))
    ));
}

#[test]
fn ulaw_agents_are_transcoded_both_ways() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    let LiveEvent::Ready(info) = event(
        &mut codec,
        &json!({
            "type": "conversation_initiation_metadata",
            "conversation_initiation_metadata_event": {
                "agent_output_audio_format": "ulaw_8000",
                "user_input_audio_format": "ulaw_8000"
            }
        }),
    ) else {
        panic!("expected ready")
    };
    assert_eq!(info.output_format.sample_rate, 8_000);

    // Two μ-law bytes (silence) become two PCM16 samples.
    let audio = event(
        &mut codec,
        &json!({"type": "audio", "audio_event": {"audio_base_64": B64.encode([0xFF_u8, 0xFF])}}),
    );
    assert_eq!(audio, LiveEvent::Audio(Bytes::from_static(&[0, 0, 0, 0])));

    // Four 16 kHz PCM samples become two 8 kHz μ-law bytes.
    let frame = codec.encode(ClientCommand::Audio(Bytes::from_static(&[0; 8])));
    let chunk = text_of(&frame[0])["user_audio_chunk"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(B64.decode(chunk).unwrap(), vec![0xFF, 0xFF]);
}

#[test]
fn unsupported_agent_formats_fail_the_session() {
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());
    let failed = event(
        &mut codec,
        &json!({
            "type": "conversation_initiation_metadata",
            "conversation_initiation_metadata_event": {"agent_output_audio_format": "mp3_44100"}
        }),
    );
    assert_eq!(
        failed,
        LiveEvent::Error {
            error: Error::InvalidConfig("unsupported elevenlabs audio format mp3_44100".into()),
            fatal: true
        }
    );
}

#[test]
fn audio_from_an_unsupported_host_rate_is_dropped() {
    let mut config = LiveConfig::new();
    config.input_format = AudioFormat::pcm16(1_000);
    let mut codec = ElevenLabsCodec::new(&config);
    assert!(
        codec
            .encode(ClientCommand::Audio(Bytes::from_static(&[0; 8])))
            .is_empty(),
        "expected nothing"
    );
}

fn events(codec: &mut ElevenLabsCodec, value: &Value) -> Vec<LiveEvent> {
    decode(codec, value)
        .into_iter()
        .map(|d| match d {
            Decoded::Event(e) => e,
            Decoded::Reply(r) => panic!("unexpected reply {r:?}"),
        })
        .collect()
}

#[test]
fn a_reply_is_final_once_it_can_no_longer_change() {
    let reply = |text: &str| json!({"type": "agent_response", "agent_response_event": {"agent_response": text}});
    let heard = |text: &str| json!({"type": "user_transcript", "user_transcription_event": {"user_transcript": text}});
    let partial = |text: &str| LiveEvent::OutputTranscript {
        text: text.into(),
        is_final: false,
    };
    let fin = |text: &str| LiveEvent::OutputTranscript {
        text: text.into(),
        is_final: true,
    };
    let mut codec = ElevenLabsCodec::new(&LiveConfig::new());

    // A reply that is corrected after barge-in: only the correction is final.
    assert_eq!(
        events(&mut codec, &reply("It is two o'clock and")),
        vec![partial("It is two o'clock and")]
    );
    assert_eq!(
        events(
            &mut codec,
            &json!({"type": "agent_response_correction", "agent_response_correction_event": {"corrected_agent_response": "It is two"}})
        ),
        vec![fin("It is two")]
    );
    // The next user utterance has nothing left to settle.
    assert_eq!(
        events(&mut codec, &heard("thanks")),
        vec![LiveEvent::InputTranscript {
            text: "thanks".into(),
            is_final: true
        }]
    );

    // An uncorrected reply settles when the user speaks next...
    events(&mut codec, &reply("You're welcome."));
    assert_eq!(
        events(&mut codec, &heard("bye")),
        vec![
            fin("You're welcome."),
            LiveEvent::InputTranscript {
                text: "bye".into(),
                is_final: true
            }
        ]
    );
    // ...or when another reply starts.
    events(&mut codec, &reply("One."));
    assert_eq!(
        events(&mut codec, &reply("Two.")),
        vec![fin("One."), partial("Two.")]
    );
}
