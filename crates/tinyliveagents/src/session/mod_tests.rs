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

fn info() -> crate::types::SessionInfo {
    crate::types::SessionInfo {
        provider: "p".into(),
        session_id: None,
        model: None,
        input_format: crate::types::AudioFormat::default(),
        output_format: crate::types::AudioFormat::default(),
    }
}

#[tokio::test]
async fn await_ready_keeps_ready_queued_for_the_host() {
    let (session, channels) = session_pair();
    channels.events.emit(LiveEvent::Ready(info())).await;
    channels.events.emit(LiveEvent::Interrupted).await;
    let mut session = await_ready(session, Duration::from_secs(1)).await.unwrap();
    assert!(matches!(session.recv().await, Some(LiveEvent::Ready(_))));
    assert_eq!(session.recv().await, Some(LiveEvent::Interrupted));

    let (session, channels) = session_pair();
    channels.events.emit(LiveEvent::Ready(info())).await;
    let (_sender, mut events) = await_ready(session, Duration::from_secs(1))
        .await
        .unwrap()
        .split();
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(1)).await,
        Ok(Some(LiveEvent::Ready(_)))
    ));
}

#[tokio::test]
async fn await_ready_turns_pre_ready_events_into_errors() {
    let cases = [
        (
            LiveEvent::Error {
                error: Error::Unauthorized,
                fatal: false,
            },
            Error::Unauthorized,
        ),
        (
            LiveEvent::Closed(CloseReason::Error(Error::InsufficientCredits)),
            Error::InsufficientCredits,
        ),
        (
            LiveEvent::Closed(CloseReason::Remote {
                code: Some(1000),
                reason: "bye".into(),
            }),
            Error::Provider("closed before the session was ready (code Some(1000)): bye".into()),
        ),
        (
            LiveEvent::Interrupted,
            Error::Protocol("the first event was not ready".into()),
        ),
    ];
    for (event, expected) in cases {
        let (session, channels) = session_pair();
        channels.events.emit(event).await;
        let result = await_ready(session, Duration::from_secs(1)).await;
        assert_eq!(result.err(), Some(expected));
    }
}

#[tokio::test]
async fn await_ready_times_out_or_sees_a_vanished_provider() {
    let (session, _channels) = session_pair();
    assert_eq!(
        await_ready(session, Duration::from_millis(5)).await.err(),
        Some(Error::Timeout)
    );
    let (session, channels) = session_pair();
    drop(channels);
    assert_eq!(
        await_ready(session, Duration::from_secs(1)).await.err(),
        Some(Error::Closed)
    );
}
