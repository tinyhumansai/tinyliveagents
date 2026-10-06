//! Tests for the shared WebSocket driver, using an echo codec.

use super::*;
use crate::session::session_pair;
use crate::testkit::{MockServer, close_with, collect_events, next_event, next_json, send_json};
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
        assert_eq!(crate::testkit::expect_close(&mut ws).await, Some(1000));
    })
    .await;

    let socket = connect(&server.url, &[("x-test", "1".into())])
        .await
        .unwrap();
    let (mut session, channels) = session_pair();
    let sender = session.sender();
    let task = tokio::spawn(drive(socket, EchoCodec, channels));

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
        tokio::spawn(drive(socket, EchoCodec, channels));
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
        let _ = crate::testkit::expect_close(&mut ws).await;
    })
    .await;
    let socket = connect(&server.url, &[]).await.unwrap();
    let (mut session, channels) = session_pair();
    tokio::spawn(drive(socket, EchoCodec, channels));
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
    tokio::spawn(drive(socket, EchoCodec, channels));
    let events = collect_events(&mut session).await;
    assert!(matches!(events.last(), Some(LiveEvent::Closed(_))));
    server.finish().await;
}

#[tokio::test]
async fn dropping_the_host_side_stops_the_driver() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        send_json(&mut ws, json!({"interrupt": true})).await;
        let _ = crate::testkit::expect_close(&mut ws).await;
    })
    .await;
    let socket = connect(&server.url, &[]).await.unwrap();
    let (session, channels) = session_pair();
    drop(session);
    drive(socket, EchoCodec, channels).await;
    server.finish().await;
}

#[tokio::test]
async fn connect_rejects_bad_urls_and_headers() {
    assert!(matches!(
        connect("not a url", &[]).await,
        Err(Error::Connect(_))
    ));
    assert!(matches!(
        connect("ws://127.0.0.1:9", &[("x-bad", "line\nbreak".into())]).await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        connect("ws://127.0.0.1:9", &[]).await,
        Err(Error::Connect(_))
    ));
}

#[tokio::test]
async fn connect_maps_http_refusals() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (status, expected) in [
        (401, Error::Unauthorized),
        (403, Error::Unauthorized),
        (402, Error::InsufficientCredits),
        (429, Error::RateLimited),
        (500, Error::Connect("upgrade refused with http 500".into())),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf).await.unwrap();
            let response =
                format!("HTTP/1.1 {status} Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        assert_eq!(connect(&url, &[]).await.err(), Some(expected));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn control_frames_are_skipped() {
    let server = MockServer::start(|mut ws, _| async move {
        let _ = next_json(&mut ws).await;
        ws.send(Message::Ping(Bytes::from_static(b"p")))
            .await
            .unwrap();
        send_json(&mut ws, json!({"interrupt": true})).await;
        let _ = crate::testkit::expect_close(&mut ws).await;
    })
    .await;
    let socket = connect(&server.url, &[]).await.unwrap();
    let (mut session, channels) = session_pair();
    tokio::spawn(drive(socket, QuietCodec, channels));
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
            let _ = crate::testkit::expect_close(&mut ws).await;
        })
        .await;
        let socket = connect(&server.url, &[]).await.unwrap();
        let (session, channels) = session_pair();
        // Keep the sender so the command side stays open; only the event
        // stream goes away.
        let (sender, events) = session.split();
        drop(events);
        drive(socket, QuietCodec, channels).await;
        drop(sender);
        server.finish().await;
    }
}

#[tokio::test]
async fn a_silent_server_times_out_the_handshake() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let hold = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });
    let result = connect_within(&url, &[], std::time::Duration::from_millis(50)).await;
    assert_eq!(result.err(), Some(Error::Timeout));
    hold.abort();
}

#[test]
fn common_close_codes_map_to_errors() {
    assert_eq!(common_close_error(1000, ""), None);
    assert_eq!(common_close_error(1001, ""), None);
    assert_eq!(common_close_error(1005, ""), None);
    assert_eq!(
        common_close_error(1008, "Quota exceeded"),
        Some(Error::RateLimited)
    );
    assert!(matches!(
        common_close_error(1008, "other"),
        Some(Error::Provider(_))
    ));
    assert!(matches!(
        common_close_error(1011, "x"),
        Some(Error::Provider(_))
    ));
    assert_eq!(common_close_error(4429, ""), Some(Error::RateLimited));
    assert!(matches!(
        common_close_error(4999, ""),
        Some(Error::Provider(_))
    ));
}

#[test]
fn handshake_errors_do_not_echo_urls() {
    let error = handshake_error(&tungstenite::Error::ConnectionClosed);
    assert_eq!(error, Error::Connect("handshake failed".into()));
    let error = handshake_error(&tungstenite::Error::Io(std::io::Error::from(
        std::io::ErrorKind::ConnectionRefused,
    )));
    assert!(matches!(error, Error::Connect(_)));
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
