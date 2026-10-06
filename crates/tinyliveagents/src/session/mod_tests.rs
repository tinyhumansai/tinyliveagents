//! Unit tests for the session handles.

use super::*;
use crate::types::{CloseReason, ToolCall};
use serde_json::json;

#[tokio::test]
async fn sender_helpers_deliver_commands_in_order() {
    let (session, mut channels) = session_pair();
    let sender = session.sender();
    let call = ToolCall {
        call_id: "c".into(),
        name: "n".into(),
        args: json!({}),
    };
    sender.send_audio(vec![1_u8, 2]).await.unwrap();
    sender.send_text("hi").await.unwrap();
    sender
        .send_tool_result(ToolResult::ok(&call, "done"))
        .await
        .unwrap();
    sender.interrupt().await.unwrap();
    sender.close().await.unwrap();

    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(channels.commands.recv().await.unwrap());
    }
    assert_eq!(seen[0], ClientCommand::Audio(Bytes::from_static(&[1, 2])));
    assert_eq!(seen[1], ClientCommand::Text("hi".into()));
    assert!(matches!(seen[2], ClientCommand::ToolResult(_)));
    assert_eq!(seen[3], ClientCommand::Interrupt);
    assert_eq!(seen[4], ClientCommand::Close);
}

#[tokio::test]
async fn sending_after_the_provider_hangs_up_reports_closed() {
    let (session, channels) = session_pair();
    drop(channels);
    let sender = session.sender();
    assert!(sender.is_closed());
    assert_eq!(sender.send_text("x").await, Err(Error::Closed));
}

#[tokio::test]
async fn events_flow_to_the_host_and_report_a_dropped_stream() {
    let (mut session, channels) = session_pair();
    assert!(channels.events.emit(LiveEvent::Interrupted).await);
    assert_eq!(session.recv().await, Some(LiveEvent::Interrupted));
    drop(session);
    assert!(
        !channels
            .events
            .emit(LiveEvent::Closed(CloseReason::Client))
            .await
    );
}

#[tokio::test]
async fn recv_timeout_reports_a_timeout() {
    let (session, _channels) = session_pair();
    let (_sender, mut events) = session.split();
    let result = events.recv_timeout(Duration::from_millis(5)).await;
    assert_eq!(result, Err(Error::Timeout));
}

#[tokio::test]
async fn dropping_the_events_aborts_the_task() {
    let (session, _channels) = session_pair();
    let task = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let abort = task.abort_handle();
    let session = session.with_task(task);
    drop(session);
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(abort.is_finished());
}
