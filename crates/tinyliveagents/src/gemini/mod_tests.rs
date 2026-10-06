//! End-to-end tests for the Gemini providers against a mock Live server.

use super::*;
use crate::testkit::{
    MockServer, close_with, collect_events, expect_close, next_event, next_json, send_json,
};
use crate::types::{CloseReason, ToolDeclaration, ToolResult};
use crate::{LiveEvent, ToolCall};
use bytes::Bytes;
use serde_json::json;

#[tokio::test]
async fn direct_session_round_trips_audio_and_a_tool_call() {
    let server = MockServer::start(|mut ws, upgrade| async move {
        assert_eq!(upgrade.header("x-goog-api-key"), Some("key-1"));
        let setup = next_json(&mut ws).await;
        assert_eq!(setup["setup"]["model"], "models/gemini-test");
        assert_eq!(
            setup["setup"]["tools"][0]["functionDeclarations"][0]["name"],
            "get_time"
        );
        send_json(&mut ws, json!({"setupComplete": {}})).await;

        let audio = next_json(&mut ws).await;
        assert_eq!(audio["realtimeInput"]["audio"]["data"], "AAA=");
        send_json(
            &mut ws,
            json!({"toolCall": {"functionCalls": [{"id": "c1", "name": "get_time", "args": {}}]}}),
        )
        .await;
        let response = next_json(&mut ws).await;
        assert_eq!(
            response["toolResponse"]["functionResponses"][0]["response"],
            json!({"result": "noon"})
        );
        send_json(
            &mut ws,
            json!({"serverContent": {
                "modelTurn": {"parts": [{"inlineData": {"data": "AQI="}}]},
                "turnComplete": true
            }}),
        )
        .await;
        assert_eq!(expect_close(&mut ws).await, Some(1000));
    })
    .await;

    let provider = GeminiLive::new("key-1").with_endpoint(server.url.clone());
    assert_eq!(provider.id(), "gemini");
    assert!(provider.capabilities().tools);
    assert!(!format!("{provider:?}").contains("key-1"));

    let config = LiveConfig::new()
        .with_model("gemini-test")
        .with_tool(ToolDeclaration::new("get_time", "time", json!({})));
    let mut session = provider.connect(config).await.unwrap();
    let sender = session.sender();

    let LiveEvent::Ready(info) = next_event(&mut session).await else {
        panic!("expected ready");
    };
    assert_eq!(info.model.as_deref(), Some("gemini-test"));
    sender.send_audio(vec![0_u8, 0]).await.unwrap();

    let LiveEvent::ToolCall(call) = next_event(&mut session).await else {
        panic!("expected a tool call");
    };
    assert_eq!(
        call,
        ToolCall {
            call_id: "c1".into(),
            name: "get_time".into(),
            args: json!({})
        }
    );
    sender
        .send_tool_result(ToolResult::ok(&call, "noon"))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::Audio(Bytes::from_static(&[1, 2]))
    );
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::TurnComplete { usage: None }
    );
    sender.close().await.unwrap();
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::Closed(CloseReason::Client)
    );
    server.finish().await;
}

#[tokio::test]
async fn direct_rejects_an_empty_key_and_bad_config() {
    let provider = GeminiLive::new("  ");
    assert!(matches!(
        provider.connect(LiveConfig::new()).await,
        Err(Error::InvalidConfig(_))
    ));
    let provider = GeminiLive::new("k").with_endpoint("ws://127.0.0.1:9");
    let mut config = LiveConfig::new();
    config.input_format = crate::AudioFormat::pcm16(8_000);
    assert!(matches!(
        provider.connect(config).await,
        Err(Error::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn direct_fails_connect_when_setup_is_refused() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        close_with(&mut ws, 1007, "Request contains an invalid argument").await;
    })
    .await;
    let provider = GeminiLive::new("k").with_endpoint(server.url.clone());
    let result = provider.connect(LiveConfig::new()).await;
    assert_eq!(
        result.err(),
        Some(Error::InvalidConfig(
            "Request contains an invalid argument".into()
        ))
    );
    server.finish().await;
}

#[tokio::test]
async fn direct_fails_connect_on_a_pre_ready_error_frame() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        send_json(&mut ws, json!({"error": {"message": "model not found"}})).await;
        let _ = expect_close(&mut ws).await;
    })
    .await;
    let provider = GeminiLive::new("k").with_endpoint(server.url.clone());
    let result = provider.connect(LiveConfig::new()).await;
    assert_eq!(
        result.err(),
        Some(Error::Provider("model not found".into()))
    );
    server.finish().await;
}

#[tokio::test]
async fn relay_is_ready_at_once_and_maps_credit_exhaustion() {
    let server = MockServer::start(|mut ws, upgrade| async move {
        assert!(upgrade.uri.contains("ticket=t1"));
        let frame = next_json(&mut ws).await;
        assert!(frame.get("setup").is_none());
        assert_eq!(frame["realtimeInput"]["audio"]["data"], "AAA=");
        close_with(&mut ws, 4402, "insufficient credits").await;
    })
    .await;
    let provider = GeminiRelay::connect_url(format!("{}/live/ws?ticket=t1", server.url))
        .with_session("sess-1", "gemini-3.8-live");
    assert_eq!(provider.id(), "gemini-relay");
    assert!(provider.capabilities().resumption);
    let debug = format!("{provider:?}");
    assert!(debug.contains("sess-1") && !debug.contains("t1\""));

    // Tools are declared at ticket time, so they are accepted but not resent.
    let config = LiveConfig::new().with_tool(ToolDeclaration::new("t", "d", json!({})));
    let mut session = provider.connect(config).await.unwrap();
    let LiveEvent::Ready(info) = next_event(&mut session).await else {
        panic!("expected ready");
    };
    assert_eq!(info.session_id.as_deref(), Some("sess-1"));
    assert_eq!(info.model.as_deref(), Some("gemini-3.8-live"));
    session.sender().send_audio(vec![0_u8, 0]).await.unwrap();
    let events = collect_events(&mut session).await;
    assert_eq!(
        events.last(),
        Some(&LiveEvent::Closed(CloseReason::Error(
            Error::InsufficientCredits
        )))
    );
    server.finish().await;
}

#[tokio::test]
async fn relay_surfaces_refused_tickets() {
    let provider = GeminiRelay::connect_url("ws://127.0.0.1:9/live");
    assert!(matches!(
        provider.connect(LiveConfig::new()).await,
        Err(Error::Connect(_))
    ));
}
