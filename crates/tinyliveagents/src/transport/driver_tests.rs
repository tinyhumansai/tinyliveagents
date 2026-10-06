//! Tests for the session driver, using an echo codec.

use super::*;
use crate::session::session_pair;
use crate::test_support::{
    MockServer, close_with, collect_events, next_event, next_json, send_json,
};
use crate::transport::{connect, json_frame};
use crate::types::ClientCommand;
use bytes::Bytes;
use serde_json::{Value, json};

/// Sends `{"hello":true}` on open, encodes text commands as `{"say":...}`,
/// answers `{"ping":n}` with `{"pong":n}`, and decodes `{"interrupt":true}`.
struct EchoCodec;

/// Like [`EchoCodec`] but emits nothing on open.
struct QuietCodec;

impl WireCodec for QuietCodec {
    fn on_open(&mut self) -> (Vec<Message>, Vec<LiveEvent>) {
        (vec![json_frame(&json!({"hello": true}))], Vec::new())
    }

    fn encode(&mut self, command: ClientCommand) -> Vec<Message> {
        EchoCodec.encode(command)
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Vec<Decoded>> {
        EchoCodec.decode(frame)
    }
}

impl WireCodec for EchoCodec {
    fn on_open(&mut self) -> (Vec<Message>, Vec<LiveEvent>) {
        (
            vec![json_frame(&json!({"hello": true}))],
            vec![LiveEvent::Interrupted],
        )
    }

    fn encode(&mut self, command: ClientCommand) -> Vec<Message> {
        match command {
            ClientCommand::Text(text) => vec![json_frame(&json!({ "say": text }))],
            _ => Vec::new(),
        }
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Vec<Decoded>> {
        let value: Value =
            serde_json::from_slice(frame).map_err(|e| Error::Protocol(e.to_string()))?;
        if let Some(n) = value.get("ping") {
            return Ok(vec![Decoded::Reply(json_frame(&json!({ "pong": n })))]);
        }
        if value.get("interrupt").is_some() {
            return Ok(vec![Decoded::Event(LiveEvent::Interrupted)]);
        }
        if value.get("fatal").is_some() {
            return Ok(vec![Decoded::Event(LiveEvent::Error {
                error: Error::Provider("boom".into()),
                fatal: true,
            })]);
        }
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn drives_open_encode_reply_decode_and_client_close() {
    let server = MockServer::start(|mut ws, _| async move {
        assert_eq!(next_json(&mut ws).await, json!({"hello": true}));
        assert_eq!(next_json(&mut ws).await, json!({"say": "hi"}));
        send_json(&mut ws, json!({"ping": 7})).await;
        assert_eq!(next_json(&mut ws).await, json!({"pong": 7}));
        send_json(&mut ws, json!({"interrupt": true})).await;
        ws.send(Message::Binary(Bytes::from_static(b"not json")))
            .await
            .unwrap();
        assert_eq!(crate::test_support::expect_close(&mut ws).await, Some(1000));
    })
    .await;

    let socket = connect(&server.url, &[("x-test", "1".into())])
        .await
        .unwrap();
    let (mut session, channels) = session_pair();
    let sender = session.sender();
    let task = tokio::spawn(drive(Box::new(socket), Box::new(EchoCodec), channels));

    assert_eq!(next_event(&mut session).await, LiveEvent::Interrupted);
    sender.send(ClientCommand::Text("hi".into())).await.unwrap();
    // Unsupported commands are dropped, not fatal.
    sender.send(ClientCommand::ActivityStart).await.unwrap();
    assert_eq!(next_event(&mut session).await, LiveEvent::Interrupted);
    assert!(matches!(
        next_event(&mut session).await,
        LiveEvent::Error {
            error: Error::Protocol(_),
            fatal: false
        }
    ));
    sender.close().await.unwrap();
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::Closed(CloseReason::Client)
    );
    task.await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn maps_remote_close_codes() {
    for (code, expected) in [
        (4401, Some(Error::Unauthorized)),
        (4402, Some(Error::InsufficientCredits)),
        (4408, Some(Error::Timeout)),
        (1000, None),
    ] {
        let server = MockServer::start(move |mut ws, _| async move {
            let _ = next_json(&mut ws).await;
            close_with(&mut ws, code, "bye").await;
        })
        .await;
        let socket = connect(&server.url, &[]).await.unwrap();
        let (mut session, channels) = session_pair();
        tokio::spawn(drive(Box::new(socket), Box::new(EchoCodec), channels));
        let events = collect_events(&mut session).await;
        let last = events.last().unwrap().clone();
        match expected {
            Some(error) => assert_eq!(last, LiveEvent::Closed(CloseReason::Error(error))),
            None => assert_eq!(
                last,
                LiveEvent::Closed(CloseReason::Remote {
                    code: Some(1000),
                    reason: "bye".into()
                })
            ),
        }
        server.finish().await;
    }
}

#[tokio::test]
async fn a_fatal_event_ends_the_session() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        send_json(&mut ws, json!({"fatal": true})).await;
        let _ = crate::test_support::expect_close(&mut ws).await;
    })
    .await;
    let socket = connect(&server.url, &[]).await.unwrap();
    let (mut session, channels) = session_pair();
    tokio::spawn(drive(Box::new(socket), Box::new(EchoCodec), channels));
    let events = collect_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(LiveEvent::Closed(CloseReason::Error(Error::Provider(_))))
    ));
    server.finish().await;
}

#[tokio::test]
async fn a_dropped_socket_closes_the_session_remotely() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        drop(ws);
    })
    .await;
    let socket = connect(&server.url, &[]).await.unwrap();
    let (mut session, channels) = session_pair();
    tokio::spawn(drive(Box::new(socket), Box::new(EchoCodec), channels));
    let events = collect_events(&mut session).await;
    assert!(matches!(events.last(), Some(LiveEvent::Closed(_))));
    server.finish().await;
}

#[tokio::test]
async fn dropping_the_host_side_stops_the_driver() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        send_json(&mut ws, json!({"interrupt": true})).await;
        let _ = crate::test_support::expect_close(&mut ws).await;
    })
    .await;
    let socket = connect(&server.url, &[]).await.unwrap();
    let (session, channels) = session_pair();
    drop(session);
    drive(Box::new(socket), Box::new(EchoCodec), channels).await;
    server.finish().await;
}

#[tokio::test]
async fn control_frames_are_skipped() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        ws.send(Message::Ping(Bytes::from_static(b"p")))
            .await
            .unwrap();
        send_json(&mut ws, json!({"interrupt": true})).await;
        let _ = crate::test_support::expect_close(&mut ws).await;
    })
    .await;
    let socket = connect(&server.url, &[]).await.unwrap();
    let (mut session, channels) = session_pair();
    tokio::spawn(drive(Box::new(socket), Box::new(QuietCodec), channels));
    assert_eq!(next_event(&mut session).await, LiveEvent::Interrupted);
    session.sender().close().await.unwrap();
    assert_eq!(
        next_event(&mut session).await,
        LiveEvent::Closed(CloseReason::Client)
    );
    server.finish().await;
}

#[tokio::test]
async fn a_host_that_leaves_mid_stream_stops_the_driver() {
    for frame in [json!({"interrupt": true}), json!("garbage")] {
        let server = MockServer::start(move |mut ws, _| async move {
            let _ = next_json(&mut ws).await;
            match frame {
                Value::String(_) => ws.send(Message::Text("not json".into())).await.unwrap(),
                other => send_json(&mut ws, other).await,
            }
            let _ = crate::test_support::expect_close(&mut ws).await;
        })
        .await;
        let socket = connect(&server.url, &[]).await.unwrap();
        let (session, channels) = session_pair();
        // Keep the sender so the command side stays open; only the event
        // stream goes away.
        let (sender, events) = session.split();
        drop(events);
        drive(Box::new(socket), Box::new(QuietCodec), channels).await;
        drop(sender);
        server.finish().await;
    }
}

#[test]
fn io_kinds_are_classified() {
    assert_eq!(
        io_kind(&tungstenite::Error::AlreadyClosed),
        "connection closed"
    );
    assert_eq!(
        io_kind(&tungstenite::Error::Utf8(String::new())),
        "protocol failure"
    );
    assert_eq!(
        io_kind(&tungstenite::Error::Io(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe
        ))),
        std::io::ErrorKind::BrokenPipe.to_string()
    );
}

/// An in-memory socket: yields scripted frames, then either ends or stays
/// open, and fails every send after `sends_ok` successful ones.
struct FakeSocket {
    incoming: std::collections::VecDeque<std::result::Result<Message, tungstenite::Error>>,
    end_when_drained: bool,
    sends_ok: usize,
}

impl futures_util::Stream for FakeSocket {
    type Item = std::result::Result<Message, tungstenite::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.incoming.pop_front() {
            Some(item) => std::task::Poll::Ready(Some(item)),
            None if self.end_when_drained => std::task::Poll::Ready(None),
            None => std::task::Poll::Pending,
        }
    }
}

impl futures_util::Sink<Message> for FakeSocket {
    type Error = tungstenite::Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        _item: Message,
    ) -> std::result::Result<(), Self::Error> {
        if self.sends_ok == 0 {
            return Err(tungstenite::Error::ConnectionClosed);
        }
        self.sends_ok -= 1;
        Ok(())
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}

fn fake(
    incoming: Vec<std::result::Result<Message, tungstenite::Error>>,
    end_when_drained: bool,
    sends_ok: usize,
) -> FakeSocket {
    FakeSocket {
        incoming: incoming.into(),
        end_when_drained,
        sends_ok,
    }
}

async fn last_event(socket: FakeSocket, command: Option<ClientCommand>) -> LiveEvent {
    let (mut session, channels) = session_pair();
    if let Some(command) = command {
        session.sender().send(command).await.unwrap();
    }
    let task = tokio::spawn(drive(Box::new(socket), Box::new(QuietCodec), channels));
    let events = collect_events(&mut session).await;
    task.await.unwrap();
    events.last().unwrap().clone()
}

fn send_failure() -> LiveEvent {
    LiveEvent::Closed(CloseReason::Error(Error::Connect(
        "send failed: connection closed".into(),
    )))
}

#[tokio::test]
async fn a_failed_opening_send_closes_with_an_error() {
    assert_eq!(
        last_event(fake(vec![], false, 0), None).await,
        send_failure()
    );
}

#[tokio::test]
async fn a_failed_command_send_closes_with_an_error() {
    let event = last_event(
        fake(vec![], false, 1),
        Some(ClientCommand::Text("hi".into())),
    )
    .await;
    assert_eq!(event, send_failure());
}

#[tokio::test]
async fn a_failed_reply_send_closes_with_an_error() {
    let ping = Message::Text(json!({"ping": 1}).to_string().into());
    assert_eq!(
        last_event(fake(vec![Ok(ping)], false, 1), None).await,
        send_failure()
    );
}

#[tokio::test]
async fn a_stream_that_ends_or_fails_closes_the_session() {
    assert_eq!(
        last_event(fake(vec![], true, 1), None).await,
        LiveEvent::Closed(CloseReason::Remote {
            code: None,
            reason: String::new()
        })
    );
    assert_eq!(
        last_event(
            fake(vec![Err(tungstenite::Error::ConnectionClosed)], false, 1),
            None
        )
        .await,
        LiveEvent::Closed(CloseReason::Error(Error::Connect(
            "receive failed: connection closed".into()
        )))
    );
    // A close frame without a code is a normal remote close.
    assert_eq!(
        last_event(fake(vec![Ok(Message::Close(None))], false, 1), None).await,
        LiveEvent::Closed(CloseReason::Remote {
            code: Some(1005),
            reason: String::new()
        })
    );
}
