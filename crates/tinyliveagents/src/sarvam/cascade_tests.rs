//! End-to-end tests for the cascade against mock STT, chat and TTS services.

use super::*;
use crate::LiveProvider;
use crate::sarvam::{SarvamCascade, SarvamEndpoints};
use crate::test_support::{
    MockHttp, MockServer, ServerSocket, close_with, collect_events, expect_close, next_event,
    next_json, send_json, sse, text_chunk, tool_chunk,
};
use crate::types::{ToolCall, ToolDeclaration};
use bytes::Bytes;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Notify;

fn provider(stt: &MockServer, chat: &MockHttp, tts: &str) -> SarvamCascade {
    SarvamCascade::new("key").with_endpoints(SarvamEndpoints {
        stt: stt.url.clone(),
        chat: chat.url.clone(),
        tts: tts.to_string(),
    })
}

fn config() -> LiveConfig {
    LiveConfig::new()
        .with_system_instruction("be brief")
        .with_tool(ToolDeclaration::new(
            "get_time",
            "time",
            json!({"type": "object"}),
        ))
}

/// Plays one TTS utterance: expects config, text..., flush; answers with
/// audio then `final`.
async fn tts_utterance(ws: &mut ServerSocket, expected_text: &str) -> Value {
    let config = next_json(ws).await;
    assert_eq!(config["type"], "config");
    let mut spoken = String::new();
    loop {
        let frame = next_json(ws).await;
        match frame["type"].as_str() {
            Some("text") => {
                if !spoken.is_empty() {
                    spoken.push(' ');
                }
                spoken.push_str(frame["data"]["text"].as_str().unwrap());
            }
            Some("flush") => break,
            other => panic!("unexpected tts frame {other:?}"),
        }
    }
    assert_eq!(spoken, expected_text);
    send_json(ws, json!({"type": "audio", "data": {"audio": "AQI="}})).await;
    send_json(
        ws,
        json!({"type": "event", "data": {"event_type": "final"}}),
    )
    .await;
    config
}

/// Next event that is not an incremental transcript.
async fn next_settled(session: &mut crate::LiveSession) -> LiveEvent {
    loop {
        match next_event(session).await {
            LiveEvent::OutputTranscript {
                is_final: false, ..
            }
            | LiveEvent::InputTranscript {
                is_final: false, ..
            } => {}
            other => return other,
        }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one whole conversation, asserted step by step
async fn a_spoken_question_calls_a_tool_and_speaks_the_answer() {
    let stt = MockServer::start(|mut ws, upgrade| async move {
        assert_eq!(upgrade.header("api-subscription-key"), Some("key"));
        assert!(upgrade.uri.contains("language_code=en-IN"));
        send_json(&mut ws, json!({"event": "session.begin"})).await;
        assert_eq!(next_json(&mut ws).await["event"], "audio_input");
        send_json(
            &mut ws,
            json!({"event": "transcript.partial", "text": "what"}),
        )
        .await;
        send_json(&mut ws, json!({"event": "transcript.partial", "text": ""})).await;
        send_json(
            &mut ws,
            json!({"event": "transcript.final", "text": "", "language": "en-IN"}),
        )
        .await;
        send_json(
            &mut ws,
            json!({"event": "transcript.final", "text": "what time is it", "language": "en-IN"}),
        )
        .await;
        // The host closes: STT gets the end-of-stream frame, then a close.
        assert_eq!(next_json(&mut ws).await["event"], "end");
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let chat = MockHttp::start(vec![
        (200, sse(&[tool_chunk("c1", "get_time", "{}")])),
        (200, sse(&[text_chunk("It is "), text_chunk("noon.")])),
    ])
    .await;
    let tts = MockServer::start(|mut ws, _| async move {
        let config = tts_utterance(&mut ws, "It is noon.").await;
        assert_eq!(config["data"]["language_code"], "en-IN");
        let _ = expect_close(&mut ws).await;
    })
    .await;

    let mut session = provider(&stt, &chat, &tts.url)
        .connect(config())
        .await
        .unwrap();
    let sender = session.sender();
    let LiveEvent::Ready(info) = next_event(&mut session).await else {
        panic!("expected ready")
    };
    assert_eq!(info.provider, "sarvam");
    assert_eq!(info.output_format.sample_rate, 24_000);
    sender.send_audio(vec![0_u8; 4]).await.unwrap();

    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::InputTranscript {
            text: "what".into(),
            is_final: false
        }
    );
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::InputTranscript {
            text: "what time is it".into(),
            is_final: true
        }
    );
    let LiveEvent::ToolCall(call) = next_settled(&mut session).await else {
        panic!("expected a tool call")
    };
    assert_eq!(call.name, "get_time");
    // A result for an unknown call is ignored; the real one resumes the turn.
    sender
        .send_tool_result(ToolResult::ok(
            &ToolCall {
                call_id: "nope".into(),
                name: "x".into(),
                args: json!({}),
            },
            "x",
        ))
        .await
        .unwrap();
    sender
        .send_tool_result(ToolResult::ok(&call, "12:00"))
        .await
        .unwrap();

    assert_eq!(
        next_settled(&mut session).await,
        LiveEvent::OutputTranscript {
            text: "It is noon.".into(),
            is_final: true
        }
    );
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::Audio(Bytes::from_static(&[1, 2]))
    );
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::TurnComplete { usage: None }
    );

    let requests = chat.requests.lock().unwrap().clone();
    assert_eq!(requests[0]["messages"][0]["role"], "system");
    assert_eq!(requests[0]["messages"][1]["content"], "what time is it");
    assert_eq!(requests[1]["messages"][2]["tool_calls"][0]["id"], "c1");
    assert_eq!(
        requests[1]["messages"][3],
        json!({"role": "tool", "tool_call_id": "c1", "content": "12:00"})
    );

    sender.close().await.unwrap();
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::Closed(CloseReason::Client)
    );
    stt.finish().await;
    tts.finish().await;
}

#[tokio::test]
async fn speech_during_a_reply_interrupts_it() {
    let heard_audio = Arc::new(Notify::new());
    let stt_gate = heard_audio.clone();
    let stt = MockServer::start(move |mut ws, _| async move {
        send_json(
            &mut ws,
            json!({"event": "transcript.final", "text": "tell me a story"}),
        )
        .await;
        stt_gate.notified().await;
        send_json(&mut ws, json!({"event": "vad.speech_start"})).await;
        // Speech with no turn running is not an interruption.
        send_json(&mut ws, json!({"event": "vad.speech_start"})).await;
        send_json(&mut ws, json!({"event": "session.end"})).await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let chat = MockHttp::start(vec![(
        200,
        sse(&[text_chunk("Once upon a time there was a fox.")]),
    )])
    .await;
    let tts = MockServer::start(|mut ws, _| async move {
        assert_eq!(next_json(&mut ws).await["type"], "config");
        assert_eq!(next_json(&mut ws).await["type"], "text");
        assert_eq!(next_json(&mut ws).await["type"], "flush");
        send_json(&mut ws, json!({"type": "audio", "data": {"audio": "AQI="}})).await;
        // Never finishes: the turn must be cut off by barge-in.
        let _ = expect_close(&mut ws).await;
    })
    .await;

    let mut session = provider(&stt, &chat, &tts.url)
        .connect(LiveConfig::new())
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Ready(_)
    ));
    loop {
        if let LiveEvent::Audio(_) = next_event(&mut session).await {
            break;
        }
    }
    heard_audio.notify_one();
    assert_eq!(next_settled(&mut session).await, LiveEvent::Interrupted);
    let rest = collect_events(&mut session).await;
    assert!(!rest.contains(&LiveEvent::Interrupted));
    assert!(matches!(
        rest.last(),
        Some(LiveEvent::Closed(CloseReason::Remote { .. }))
    ));
    stt.finish().await;
    tts.finish().await;
}

#[tokio::test]
async fn interrupting_while_a_tool_runs_cancels_the_call() {
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(
            &mut ws,
            json!({"event": "transcript.final", "text": "time?"}),
        )
        .await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let chat = MockHttp::start(vec![
        (200, sse(&[tool_chunk("c9", "get_time", "{}")])),
        (200, sse(&[text_chunk("Okay.")])),
    ])
    .await;
    let tts = MockServer::start_many(2, |index, mut ws, _| async move {
        if index == 1 {
            tts_utterance(&mut ws, "Okay.").await;
        }
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let mut session = provider(&stt, &chat, &tts.url)
        .connect(config())
        .await
        .unwrap();
    let sender = session.sender();
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Ready(_)
    ));
    assert!(matches!(
        next_settled(&mut session).await,
        LiveEvent::InputTranscript { is_final: true, .. }
    ));
    let LiveEvent::ToolCall(_) = next_settled(&mut session).await else {
        panic!("expected a tool call")
    };
    sender.interrupt().await.unwrap();
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::ToolCallCancelled {
            call_ids: vec!["c9".into()]
        }
    );
    assert_eq!(next_event(&mut session).await, LiveEvent::Interrupted);
    // Interrupting with nothing running is a no-op.
    sender.interrupt().await.unwrap();

    // The next turn's history answers the cancelled call, so the model
    // endpoint sees a valid conversation.
    sender.send_text("never mind").await.unwrap();
    loop {
        if let LiveEvent::TurnComplete { .. } = next_event(&mut session).await {
            break;
        }
    }
    let requests = chat.requests.lock().unwrap().clone();
    let messages = requests[1]["messages"].as_array().unwrap().clone();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "c9", "content": "Error: cancelled"})
    );
    sender.close().await.unwrap();
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::Closed(CloseReason::Client)
    );
    stt.finish().await;
    tts.finish().await;
}

#[tokio::test]
async fn first_message_text_input_and_fatal_stt_errors() {
    let fatal = Arc::new(Notify::new());
    let gate = fatal.clone();
    let stt = MockServer::start(move |mut ws, upgrade| async move {
        assert!(upgrade.uri.contains("language_code=auto"));
        // A detected language the cascade then speaks in.
        send_json(&mut ws, json!({"event": "vad.speech_end"})).await;
        gate.notified().await;
        send_json(
            &mut ws,
            json!({"event": "error", "message": "quota", "is_fatal": false}),
        )
        .await;
        send_json(
            &mut ws,
            json!({"event": "error", "message": "dead", "is_fatal": true}),
        )
        .await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let chat = MockHttp::start(vec![(200, sse(&[text_chunk("Hey.")]))]).await;
    let tts = MockServer::start_many(2, |index, mut ws, _| async move {
        let expected = if index == 0 { "Hello!" } else { "Hey." };
        let config = tts_utterance(&mut ws, expected).await;
        assert_eq!(config["data"]["language_code"], "en-IN");
        let _ = expect_close(&mut ws).await;
    })
    .await;

    let mut config = LiveConfig::new().with_provider_options(json!({"auto_language": true}));
    config.first_message = Some("Hello!".into());
    let mut session = provider(&stt, &chat, &tts.url)
        .connect(config)
        .await
        .unwrap();
    let sender = session.sender();
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Ready(_)
    ));
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::OutputTranscript {
            text: "Hello!".into(),
            is_final: true
        }
    );
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Audio(_)
    ));
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::TurnComplete { usage: None }
    );

    sender.send_text("   ").await.unwrap();
    sender.send_text("hi").await.unwrap();
    assert_eq!(
        next_settled(&mut session).await,
        LiveEvent::OutputTranscript {
            text: "Hey.".into(),
            is_final: true
        }
    );
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Audio(_)
    ));
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::TurnComplete { .. }
    ));
    let requests = chat.requests.lock().unwrap().clone();
    assert_eq!(requests[0]["messages"][0]["content"], "Hello!");
    assert_eq!(requests[0]["messages"][1]["content"], "hi");

    fatal.notify_one();
    let rest = collect_events(&mut session).await;
    assert!(rest.contains(&LiveEvent::Error {
        error: Error::Provider("quota".into()),
        fatal: false
    }));
    assert!(rest.contains(&LiveEvent::Error {
        error: Error::Provider("dead".into()),
        fatal: true
    }));
    assert_eq!(
        rest.last(),
        Some(&LiveEvent::Closed(CloseReason::Error(Error::Provider(
            "dead".into()
        ))))
    );
    stt.finish().await;
    tts.finish().await;
}

#[tokio::test]
async fn degrades_when_chat_or_tts_fail() {
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(&mut ws, json!({"event": "transcript.final", "text": "one"})).await;
        // Wait for the first turn to finish before the second utterance.
        let _ = next_json(&mut ws).await;
        send_json(&mut ws, json!({"event": "transcript.final", "text": "two"})).await;
        // And for the second turn before failing.
        let _ = next_json(&mut ws).await;
        ws.send_binary_garbage().await;
        close_with(&mut ws, 1003, "invalid key").await;
    })
    .await;
    let chat = MockHttp::start(vec![
        (500, r#"{"error":{"message":"down"}}"#.into()),
        (200, sse(&[text_chunk("Text only.")])),
    ])
    .await;
    // No TTS server at all: replies continue without audio.
    let mut session = provider(&stt, &chat, &crate::test_support::closed_url("ws").await)
        .connect(LiveConfig::new())
        .await
        .unwrap();
    let sender = session.sender();
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Ready(_)
    ));
    let mut saw_chat_error = false;
    loop {
        match next_event(&mut session).await {
            LiveEvent::Error {
                error: Error::Provider(message),
                ..
            } if message == "down" => saw_chat_error = true,
            LiveEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(saw_chat_error);
    sender.send_audio(vec![0_u8; 2]).await.unwrap();
    let mut saw_text = false;
    loop {
        match next_event(&mut session).await {
            LiveEvent::OutputTranscript {
                text,
                is_final: true,
            } => saw_text = text == "Text only.",
            LiveEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(saw_text);
    sender.send_audio(vec![0_u8; 2]).await.unwrap();
    let rest = collect_events(&mut session).await;
    assert!(rest.iter().any(|e| matches!(
        e,
        LiveEvent::Error {
            error: Error::Protocol(_),
            ..
        }
    )));
    assert_eq!(
        rest.last(),
        Some(&LiveEvent::Closed(CloseReason::Error(Error::Unauthorized)))
    );
    stt.finish().await;
}

#[tokio::test]
async fn stops_after_the_tool_round_limit() {
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(
            &mut ws,
            json!({"event": "transcript.final", "text": "loop"}),
        )
        .await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let responses = (0..MAX_TOOL_ROUNDS)
        .map(|i| (200, sse(&[tool_chunk(&format!("c{i}"), "get_time", "{}")])))
        .collect();
    let chat = MockHttp::start(responses).await;
    let tts = MockServer::start(|mut ws, _| async move {
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let mut session = provider(&stt, &chat, &tts.url)
        .connect(config())
        .await
        .unwrap();
    let sender = session.sender();
    let mut calls = 0;
    loop {
        match next_event(&mut session).await {
            LiveEvent::ToolCall(call) => {
                calls += 1;
                sender
                    .send_tool_result(ToolResult::error(&call, "busy"))
                    .await
                    .unwrap();
            }
            LiveEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert_eq!(calls, MAX_TOOL_ROUNDS);
    sender.close().await.unwrap();
    let _ = collect_events(&mut session).await;
    stt.finish().await;
    tts.finish().await;
}

#[test]
fn maps_stt_close_codes_and_languages() {
    assert_eq!(sarvam_close_error(1003, ""), Some(Error::Unauthorized));
    assert_eq!(sarvam_close_error(1008, ""), Some(Error::Timeout));
    assert_eq!(
        sarvam_close_error(4000, "bad param"),
        Some(Error::InvalidConfig("bad param".into()))
    );
    assert_eq!(sarvam_close_error(1000, ""), None);
    assert_eq!(
        stt_error("Invalid subscription key. Visit the API Dashboard".into()),
        Error::Unauthorized
    );
    assert_eq!(stt_error("other".into()), Error::Provider("other".into()));

    let ctx = |language: Option<&str>| TurnContext {
        http: reqwest::Client::new(),
        chat_endpoint: String::new(),
        tts_endpoint: String::new(),
        api_key: String::new(),
        config: LiveConfig::new(),
        language: language.map(str::to_string),
    };
    assert_eq!(ctx(Some("ta-IN")).speech_language(Some("hi-IN")), "ta-IN");
    assert_eq!(ctx(None).speech_language(Some("hi-IN")), "hi-IN");
    assert_eq!(ctx(None).speech_language(Some("ur-IN")), "en-IN");
    assert_eq!(ctx(None).speech_language(None), "en-IN");
}

trait Garbage {
    async fn send_binary_garbage(&mut self);
}

impl Garbage for ServerSocket {
    async fn send_binary_garbage(&mut self) {
        use futures_util::SinkExt as _;
        self.send(Message::Binary(Bytes::from_static(b"not json")))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn typed_input_during_a_reply_is_a_barge_in_and_the_cut_reply_is_not_kept() {
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(
            &mut ws,
            json!({"event": "transcript.final", "text": "tell me a story"}),
        )
        .await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let chat = MockHttp::start(vec![
        (200, sse(&[text_chunk("Once upon a time there was a fox.")])),
        (200, sse(&[text_chunk("Okay.")])),
    ])
    .await;
    let tts = MockServer::start_many(2, |index, mut ws, _| async move {
        if index == 0 {
            assert_eq!(next_json(&mut ws).await["type"], "config");
            assert_eq!(next_json(&mut ws).await["type"], "text");
            assert_eq!(next_json(&mut ws).await["type"], "flush");
            send_json(&mut ws, json!({"type": "audio", "data": {"audio": "AQI="}})).await;
        } else {
            tts_utterance(&mut ws, "Okay.").await;
        }
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let mut session = provider(&stt, &chat, &tts.url)
        .connect(LiveConfig::new())
        .await
        .unwrap();
    let sender = session.sender();
    loop {
        if let LiveEvent::Audio(_) = next_event(&mut session).await {
            break;
        }
    }
    sender.send_text("stop").await.unwrap();
    assert_eq!(next_settled(&mut session).await, LiveEvent::Interrupted);
    loop {
        if let LiveEvent::TurnComplete { .. } = next_event(&mut session).await {
            break;
        }
    }
    let requests = chat.requests.lock().unwrap().clone();
    let contents: Vec<&str> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    // The story was cut off before it was spoken, so it is not history.
    assert_eq!(contents, vec!["tell me a story", "stop"]);
    sender.close().await.unwrap();
    let _ = collect_events(&mut session).await;
    stt.finish().await;
    tts.finish().await;
}

#[tokio::test]
async fn a_cut_off_completion_is_neither_committed_nor_acted_on() {
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(&mut ws, json!({"event": "transcript.final", "text": "one"})).await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    // No `[DONE]` and no finish reason: the stream was cut off mid-call.
    let partial = format!("data: {}\n\n", tool_chunk("c1", "get_time", "{\"tz\": "));
    let chat = MockHttp::start(vec![(200, partial), (200, sse(&[text_chunk("Fine.")]))]).await;
    let mut session = provider(&stt, &chat, &crate::test_support::closed_url("ws").await)
        .connect(config())
        .await
        .unwrap();
    let sender = session.sender();
    let mut saw_cutoff = false;
    loop {
        match next_event(&mut session).await {
            LiveEvent::ToolCall(call) => panic!("acted on a cut-off call: {call:?}"),
            LiveEvent::Error {
                error: Error::Protocol(message),
                ..
            } if message.contains("ended early") => saw_cutoff = true,
            LiveEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(saw_cutoff);
    sender.send_text("again").await.unwrap();
    loop {
        if let LiveEvent::TurnComplete { .. } = next_event(&mut session).await {
            break;
        }
    }
    let requests = chat.requests.lock().unwrap().clone();
    let roles: Vec<&str> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["system", "user", "user"]);
    sender.close().await.unwrap();
    let _ = collect_events(&mut session).await;
    stt.finish().await;
}

#[tokio::test]
async fn ready_waits_for_session_begin_and_refusals_fail_connect() {
    // Refused right after the upgrade: `connect` reports it.
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(
            &mut ws,
            json!({"event": "error", "message": "Invalid subscription key", "is_fatal": true}),
        )
        .await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let chat = MockHttp::start(vec![]).await;
    let result = provider(&stt, &chat, &crate::test_support::closed_url("ws").await)
        .connect(LiveConfig::new())
        .await;
    assert_eq!(result.err(), Some(Error::Unauthorized));
    stt.finish().await;

    // Accepted: Ready comes with session.begin.
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(&mut ws, json!({"event": "session.begin"})).await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let mut session = provider(&stt, &chat, &crate::test_support::closed_url("ws").await)
        .connect(LiveConfig::new())
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Ready(_)
    ));
    session.sender().close().await.unwrap();
    let _ = collect_events(&mut session).await;
    stt.finish().await;
}

#[tokio::test]
async fn a_reply_whose_playback_fails_is_not_committed() {
    let stt = MockServer::start(|mut ws, _| async move {
        send_json(
            &mut ws,
            json!({"event": "transcript.final", "text": "hello"}),
        )
        .await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let chat = MockHttp::start(vec![
        (200, sse(&[text_chunk("Hi there, friend.")])),
        (200, sse(&[text_chunk("Okay.")])),
    ])
    .await;
    let tts = MockServer::start_many(2, |index, mut ws, _| async move {
        if index == 0 {
            // The socket dies mid-playback.
            let _ = next_json(&mut ws).await;
            let _ = next_json(&mut ws).await;
            let _ = next_json(&mut ws).await;
            close_with(&mut ws, 1011, "tts down").await;
        } else {
            tts_utterance(&mut ws, "Okay.").await;
            let _ = expect_close(&mut ws).await;
        }
    })
    .await;
    let mut session = provider(&stt, &chat, &tts.url)
        .connect(LiveConfig::new())
        .await
        .unwrap();
    let sender = session.sender();
    loop {
        if let LiveEvent::TurnComplete { .. } = next_event(&mut session).await {
            break;
        }
    }
    sender.send_text("again").await.unwrap();
    loop {
        if let LiveEvent::TurnComplete { .. } = next_event(&mut session).await {
            break;
        }
    }
    let requests = chat.requests.lock().unwrap().clone();
    let contents: Vec<&str> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, vec!["hello", "again"]);
    sender.close().await.unwrap();
    let _ = collect_events(&mut session).await;
    stt.finish().await;
    tts.finish().await;
}
