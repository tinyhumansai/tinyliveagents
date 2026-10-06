//! End-to-end tests for the ElevenLabs provider against a mock agent.

use super::*;
use crate::testkit::{MockServer, close_with, collect_events, next_event, next_json, send_json};
use crate::types::{CloseReason, ToolResult};
use crate::{LiveEvent, ToolDeclaration};
use serde_json::json;

#[tokio::test]
async fn signed_url_session_handles_pings_and_client_tools() {
    let server = MockServer::start(|mut ws, upgrade| async move {
        assert!(upgrade.uri.contains("signature=abc"));
        let init = next_json(&mut ws).await;
        assert_eq!(init["type"], "conversation_initiation_client_data");
        assert_eq!(init["custom_llm_extra_body"]["user"], "tok");
        send_json(
            &mut ws,
            json!({
                "type": "conversation_initiation_metadata",
                "conversation_initiation_metadata_event": {
                    "conversation_id": "c1",
                    "agent_output_audio_format": "pcm_16000",
                    "user_input_audio_format": "pcm_16000"
                }
            }),
        )
        .await;
        send_json(&mut ws, json!({"type": "ping", "ping_event": {"event_id": 3}})).await;
        assert_eq!(
            next_json(&mut ws).await,
            json!({"type": "pong", "event_id": 3})
        );
        send_json(
            &mut ws,
            json!({"type": "client_tool_call", "client_tool_call": {"tool_name": "get_time", "tool_call_id": "t1", "parameters": {}}}),
        )
        .await;
        let result = next_json(&mut ws).await;
        assert_eq!(result["type"], "client_tool_result");
        assert_eq!(result["result"], "noon");
        close_with(&mut ws, 1000, "done").await;
    })
    .await;

    let provider = ElevenLabsConvai::connect_url(format!("{}/convai?signature=abc", server.url));
    assert_eq!(provider.id(), "elevenlabs");
    assert!(!provider.capabilities().native_audio);
    assert!(format!("{provider:?}").contains("signed-url"));

    // Tool declarations are ignored: client tools live on the agent.
    let config = LiveConfig::new()
        .with_provider_options(json!({"custom_llm_extra_body": {"user": "tok"}}))
        .with_tool(ToolDeclaration::new("t", "d", json!({})));
    let mut session = provider.connect(config).await.unwrap();
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Ready(_)
    ));
    let LiveEvent::ToolCall(call) = next_event(&mut session).await else {
        panic!("expected a tool call")
    };
    session
        .sender()
        .send_tool_result(ToolResult::ok(&call, "noon"))
        .await
        .unwrap();
    let events = collect_events(&mut session).await;
    assert_eq!(
        events.last(),
        Some(&LiveEvent::Closed(CloseReason::Remote {
            code: Some(1000),
            reason: "done".into()
        }))
    );
    server.finish().await;
}

#[tokio::test]
async fn agent_target_sends_the_key_header_and_agent_id() {
    let server = MockServer::start(|mut ws, upgrade| async move {
        assert!(upgrade.uri.contains("agent_id=agent-7"));
        assert_eq!(upgrade.header("xi-api-key"), Some("xi-key"));
        let _ = next_json(&mut ws).await;
        close_with(&mut ws, 1008, "Override for field voice_id is not allowed").await;
    })
    .await;
    let provider = ElevenLabsConvai::agent("agent-7", "xi-key").with_endpoint(server.url.clone());
    assert!(!format!("{provider:?}").contains("xi-key"));
    let mut session = provider.connect(LiveConfig::new()).await.unwrap();
    let events = collect_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(LiveEvent::Closed(CloseReason::Error(Error::Provider(_))))
    ));
    server.finish().await;
}

#[tokio::test]
async fn agent_target_validates_its_inputs() {
    assert!(matches!(
        ElevenLabsConvai::agent("", "k")
            .connect(LiveConfig::new())
            .await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        ElevenLabsConvai::agent("a", "k")
            .with_endpoint("not a url")
            .connect(LiveConfig::new())
            .await,
        Err(Error::InvalidConfig(_))
    ));
    // `with_endpoint` is a no-op for signed URLs.
    let signed = ElevenLabsConvai::connect_url("ws://127.0.0.1:9").with_endpoint("ws://x");
    assert!(matches!(
        signed.connect(LiveConfig::new()).await,
        Err(Error::Connect(_))
    ));
    let mut config = LiveConfig::new();
    config.input_format = crate::AudioFormat::pcm16(11_025);
    assert!(matches!(
        signed.connect(config).await,
        Err(Error::InvalidConfig(_))
    ));
}
