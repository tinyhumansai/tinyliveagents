//! Frame-level tests for the Gemini codec.

use super::*;
use serde_json::json;

fn codec(mode: Mode, detection: TurnDetection) -> GeminiCodec {
    GeminiCodec::new(
        mode,
        "gemini",
        Some("m".into()),
        Some("s".into()),
        AudioFormat::pcm16(16_000),
        detection,
    )
}

fn events(decoded: Vec<Decoded>) -> Vec<LiveEvent> {
    decoded
        .into_iter()
        .map(|d| match d {
            Decoded::Event(e) => e,
            Decoded::Reply(_) => panic!("unexpected reply"),
        })
        .collect()
}

fn decode(codec: &mut GeminiCodec, value: &Value) -> Vec<LiveEvent> {
    events(codec.decode(value.to_string().as_bytes()).unwrap())
}

fn text_of(message: &Message) -> Value {
    match message {
        Message::Text(text) => serde_json::from_str(text).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn direct_sends_setup_and_becomes_ready_on_setup_complete() {
    let mut c = codec(Mode::Direct(json!({"setup": {}})), TurnDetection::Server);
    let (frames, initial) = c.on_open();
    assert_eq!(text_of(&frames[0]), json!({"setup": {}}));
    assert!(initial.is_empty(), "expected nothing");

    let got = decode(&mut c, &json!({"setupComplete": {}}));
    match &got[0] {
        LiveEvent::Ready(info) => {
            assert_eq!(info.provider, "gemini");
            assert_eq!(info.output_format.sample_rate, 24_000);
            assert_eq!(info.session_id.as_deref(), Some("s"));
        }
        other => panic!("{other:?}"),
    }
    // A repeated setupComplete is not a second Ready.
    assert!(
        decode(&mut c, &json!({"setupComplete": {}})).is_empty(),
        "expected nothing"
    );
}

#[test]
fn relay_is_ready_on_open() {
    let mut c = codec(Mode::Relay, TurnDetection::Server);
    let (frames, initial) = c.on_open();
    assert!(frames.is_empty(), "expected nothing");
    assert!(matches!(initial[0], LiveEvent::Ready(_)));
    assert!(
        decode(&mut c, &json!({"setupComplete": {}})).is_empty(),
        "expected nothing"
    );
}

#[test]
fn encodes_every_command() {
    let mut c = codec(Mode::Relay, TurnDetection::Server);
    let audio = text_of(&c.encode(ClientCommand::Audio(Bytes::from_static(&[1, 2, 3])))[0]);
    assert_eq!(audio["realtimeInput"]["audio"]["data"], "AQID");
    assert_eq!(
        audio["realtimeInput"]["audio"]["mimeType"],
        "audio/pcm;rate=16000"
    );

    let text = text_of(&c.encode(ClientCommand::Text("hi".into()))[0]);
    assert_eq!(text["clientContent"]["turns"][0]["parts"][0]["text"], "hi");
    assert_eq!(text["clientContent"]["turnComplete"], true);

    let result = ToolResult {
        call_id: "c1".into(),
        name: "f".into(),
        output: json!("ok"),
        is_error: false,
    };
    let tool = text_of(&c.encode(ClientCommand::ToolResult(result))[0]);
    assert_eq!(
        tool["toolResponse"]["functionResponses"][0],
        json!({"id": "c1", "name": "f", "response": {"result": "ok"}})
    );

    assert_eq!(
        text_of(&c.encode(ClientCommand::ActivityStart)[0]),
        json!({"realtimeInput": {"activityStart": {}}})
    );
    assert_eq!(
        text_of(&c.encode(ClientCommand::ActivityEnd)[0]),
        json!({"realtimeInput": {"audioStreamEnd": true}})
    );
    assert!(
        c.encode(ClientCommand::Interrupt).is_empty(),
        "expected nothing"
    );
    assert!(
        c.encode(ClientCommand::Close).is_empty(),
        "expected nothing"
    );

    let mut manual = codec(Mode::Relay, TurnDetection::Manual);
    assert_eq!(
        text_of(&manual.encode(ClientCommand::ActivityEnd)[0]),
        json!({"realtimeInput": {"activityEnd": {}}})
    );
}

#[test]
fn tool_responses_are_always_objects() {
    let mut result = ToolResult {
        call_id: "c".into(),
        name: "f".into(),
        output: json!({"a": 1}),
        is_error: false,
    };
    assert_eq!(response_object(&result), json!({"a": 1}));
    result.is_error = true;
    result.output = json!("denied");
    assert_eq!(response_object(&result), json!({"error": "denied"}));
    result.is_error = false;
    result.output = json!(3);
    assert_eq!(response_object(&result), json!({"result": 3}));
}

#[test]
fn accumulates_transcripts_and_closes_utterances() {
    let mut c = codec(Mode::Relay, TurnDetection::Server);
    let got = decode(
        &mut c,
        &json!({"serverContent": {"inputTranscription": {"text": "what "}}}),
    );
    assert_eq!(
        got,
        vec![LiveEvent::InputTranscript {
            text: "what ".into(),
            is_final: false
        }]
    );
    let got = decode(
        &mut c,
        &json!({"serverContent": {"inputTranscription": {"text": "time"}}}),
    );
    assert_eq!(
        got,
        vec![LiveEvent::InputTranscript {
            text: "what time".into(),
            is_final: false
        }]
    );
    // The model answering closes the user's utterance.
    let got = decode(
        &mut c,
        &json!({"serverContent": {
            "modelTurn": {"parts": [
                {"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": "AQI="}},
                {"text": "thinking", "thought": true}
            ]},
            "outputTranscription": {"text": "It is"}
        }}),
    );
    assert_eq!(
        got,
        vec![
            LiveEvent::InputTranscript {
                text: "what time".into(),
                is_final: true
            },
            LiveEvent::Audio(Bytes::from_static(&[1, 2])),
            LiveEvent::OutputTranscript {
                text: "It is".into(),
                is_final: false
            },
        ]
    );
    let got = decode(
        &mut c,
        &json!({
            "serverContent": {"outputTranscription": {"text": " noon"}, "turnComplete": true},
            "usageMetadata": {"promptTokenCount": 10, "responseTokenCount": 4, "totalTokenCount": 14}
        }),
    );
    assert_eq!(
        got,
        vec![
            LiveEvent::OutputTranscript {
                text: "It is noon".into(),
                is_final: false
            },
            LiveEvent::OutputTranscript {
                text: "It is noon".into(),
                is_final: true
            },
            LiveEvent::TurnComplete {
                usage: Some(Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(4),
                    total_tokens: Some(14),
                    audio_seconds: None
                })
            },
        ]
    );
}

#[test]
fn text_parts_count_as_agent_transcript_and_interruptions_close_it() {
    let mut c = codec(Mode::Relay, TurnDetection::Server);
    let got = decode(
        &mut c,
        &json!({"serverContent": {"modelTurn": {"parts": [{"text": "Hel"}]}}}),
    );
    assert_eq!(
        got,
        vec![LiveEvent::OutputTranscript {
            text: "Hel".into(),
            is_final: false
        }]
    );
    let got = decode(&mut c, &json!({"serverContent": {"interrupted": true}}));
    assert_eq!(
        got,
        vec![
            LiveEvent::OutputTranscript {
                text: "Hel".into(),
                is_final: true
            },
            LiveEvent::Interrupted
        ]
    );
    // A bare turnComplete with nothing pending has no usage.
    let got = decode(&mut c, &json!({"serverContent": {"turnComplete": true}}));
    assert_eq!(got, vec![LiveEvent::TurnComplete { usage: None }]);
}

#[test]
fn decodes_tool_calls_and_cancellations() {
    let mut c = codec(Mode::Relay, TurnDetection::Server);
    decode(
        &mut c,
        &json!({"serverContent": {"inputTranscription": {"text": "time?"}}}),
    );
    let got = decode(
        &mut c,
        &json!({"toolCall": {"functionCalls": [
            {"id": "c1", "name": "get_time", "args": {"tz": "UTC"}},
            {"name": "no_id"}
        ]}}),
    );
    assert_eq!(
        got,
        vec![
            LiveEvent::InputTranscript {
                text: "time?".into(),
                is_final: true
            },
            LiveEvent::ToolCall(ToolCall {
                call_id: "c1".into(),
                name: "get_time".into(),
                args: json!({"tz": "UTC"})
            }),
            LiveEvent::ToolCall(ToolCall {
                call_id: "call-1".into(),
                name: "no_id".into(),
                args: json!({})
            }),
        ]
    );
    let got = decode(&mut c, &json!({"toolCallCancellation": {"ids": ["c1", 5]}}));
    assert_eq!(
        got,
        vec![LiveEvent::ToolCallCancelled {
            call_ids: vec!["c1".into()]
        }]
    );
    assert!(
        decode(&mut c, &json!({"toolCall": {}})).is_empty(),
        "expected nothing"
    );
    assert_eq!(
        decode(&mut c, &json!({"toolCallCancellation": {}})),
        vec![LiveEvent::ToolCallCancelled { call_ids: vec![] }]
    );
}

#[test]
fn decodes_resumption_go_away_and_errors() {
    let mut c = codec(Mode::Relay, TurnDetection::Server);
    assert_eq!(
        decode(
            &mut c,
            &json!({"sessionResumptionUpdate": {"newHandle": "h", "resumable": true}})
        ),
        vec![LiveEvent::ResumptionHandle { handle: "h".into() }]
    );
    assert!(
        decode(
            &mut c,
            &json!({"sessionResumptionUpdate": {"newHandle": "h", "resumable": false}})
        )
        .is_empty(),
        "expected nothing"
    );
    assert_eq!(
        decode(&mut c, &json!({"goAway": {"timeLeft": "1.5s"}})),
        vec![LiveEvent::GoAway {
            time_left_ms: Some(1500)
        }]
    );
    assert_eq!(
        decode(&mut c, &json!({"goAway": {}})),
        vec![LiveEvent::GoAway { time_left_ms: None }]
    );
    assert_eq!(
        decode(&mut c, &json!({"error": {"message": "bad"}})),
        vec![LiveEvent::Error {
            error: Error::Provider("bad".into()),
            fatal: false
        }]
    );
    assert_eq!(
        decode(&mut c, &json!({"error": {}})),
        vec![LiveEvent::Error {
            error: Error::Provider("unknown error".into()),
            fatal: false
        }]
    );
    assert!(
        decode(&mut c, &json!({"somethingNew": 1})).is_empty(),
        "expected nothing"
    );
}

#[test]
fn rejects_non_json_and_bad_audio() {
    let mut c = codec(Mode::Relay, TurnDetection::Server);
    assert!(matches!(c.decode(b"nope"), Err(Error::Protocol(_))));
    let bad = json!({"serverContent": {"modelTurn": {"parts": [{"inlineData": {"data": "!!"}}]}}});
    assert!(matches!(
        c.decode(bad.to_string().as_bytes()),
        Err(Error::Protocol(_))
    ));
}

#[test]
fn parses_durations() {
    assert_eq!(duration_ms("10s"), Some(10_000));
    assert_eq!(duration_ms("0.25s"), Some(250));
    assert_eq!(duration_ms("10"), None);
    assert_eq!(duration_ms("-1s"), None);
    assert_eq!(duration_ms("xs"), None);
}

#[test]
fn maps_gemini_close_codes() {
    let c = codec(Mode::Relay, TurnDetection::Server);
    assert_eq!(
        c.close_error(1007, "bad setup"),
        Some(Error::InvalidConfig("bad setup".into()))
    );
    assert_eq!(
        c.close_error(1008, "API key not valid"),
        Some(Error::Unauthorized)
    );
    assert_eq!(c.close_error(4402, ""), Some(Error::InsufficientCredits));
    assert_eq!(c.close_error(1000, ""), None);
}
