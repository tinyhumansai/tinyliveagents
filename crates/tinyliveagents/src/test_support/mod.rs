//! Test support: an in-process WebSocket server that scripts a provider.
//!
//! Each test starts a [`MockServer`] with a handler that receives the accepted
//! socket and the upgrade request it came with, so a test can assert on the
//! path, query and headers a provider sent, then play the provider's side of
//! the protocol frame by frame.

#![cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
// Fixtures for `#[cfg(test)]` code only: a failed fixture step must fail the
// test, which is what unwrap/expect/panic do. Clippy's `allow-*-in-tests`
// settings cover `#[test]` functions and `tests` modules but not a shared
// fixture module, so the same allowance is stated here, for this module only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// tungstenite's handshake callback returns `Result<Response, ErrorResponse>`;
// the large error type is its signature, not a choice made here.
#![allow(clippy::result_large_err)]

use std::future::Future;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// The server side of one mocked connection.
pub(crate) type ServerSocket = WebSocketStream<TcpStream>;

/// What the client sent in its upgrade request.
#[derive(Debug, Clone, Default)]
pub(crate) struct Upgrade {
    pub(crate) uri: String,
    pub(crate) headers: Vec<(String, String)>,
}

impl Upgrade {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A one-connection mock provider.
pub(crate) struct MockServer {
    pub(crate) url: String,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// Accepts one connection and runs `handler` on it.
    pub(crate) async fn start<F, Fut>(handler: F) -> Self
    where
        F: FnOnce(ServerSocket, Upgrade) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (socket, upgrade) = accept(&listener).await;
            handler(socket, upgrade).await;
        });
        Self { url, task }
    }

    // Only the Sarvam cascade opens several sockets (one TTS socket per turn).
    #[cfg(feature = "sarvam")]
    /// Accepts `count` connections in order and runs `handler` on each with
    /// its index.
    pub(crate) async fn start_many<F, Fut>(count: usize, handler: F) -> Self
    where
        F: FnOnce(usize, ServerSocket, Upgrade) -> Fut + Send + Clone + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut running = Vec::new();
            for index in 0..count {
                let (socket, upgrade) = accept(&listener).await;
                let handler = handler.clone();
                running.push(tokio::spawn(handler(index, socket, upgrade)));
            }
            for task in running {
                task.await.unwrap();
            }
        });
        Self { url, task }
    }

    /// Waits for every handler to finish, surfacing assertion failures.
    pub(crate) async fn finish(self) {
        self.task.await.unwrap();
    }
}

/// Accepts one WebSocket connection, capturing its upgrade request.
async fn accept(listener: &TcpListener) -> (ServerSocket, Upgrade) {
    let (stream, _) = listener.accept().await.unwrap();
    let captured = Arc::new(Mutex::new(Upgrade::default()));
    let sink = captured.clone();
    let socket = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            let mut upgrade = sink.lock().unwrap();
            upgrade.uri = request.uri().to_string();
            upgrade.headers = request
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            Ok(response)
        },
    )
    .await
    .unwrap();
    let upgrade = captured.lock().unwrap().clone();
    (socket, upgrade)
}

/// Reads frames until the next JSON text or binary frame and parses it.
pub(crate) async fn next_json(socket: &mut ServerSocket) -> Value {
    loop {
        match socket
            .next()
            .await
            .expect("socket ended")
            .expect("socket error")
        {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Binary(bytes) => return serde_json::from_slice(&bytes).unwrap(),
            Message::Close(frame) => panic!("unexpected close: {frame:?}"),
            _ => {}
        }
    }
}

/// Reads frames until the client closes, returning the close code.
pub(crate) async fn expect_close(socket: &mut ServerSocket) -> Option<u16> {
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Close(frame) = message {
            return frame.map(|f| u16::from(f.code));
        }
    }
    None
}

/// Sends a JSON text frame.
pub(crate) async fn send_json(socket: &mut ServerSocket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

/// Closes with `code` and `reason`.
pub(crate) async fn close_with(socket: &mut ServerSocket, code: u16, reason: &str) {
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: code.into(),
            reason: reason.to_string().into(),
        })))
        .await;
    // Drain until the client acknowledges, so the close is observed.
    while let Some(Ok(message)) = socket.next().await {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }
}

/// Collects session events until `Closed`, with a generous safety timeout.
pub(crate) async fn collect_events(session: &mut crate::LiveSession) -> Vec<crate::LiveEvent> {
    let mut out = Vec::new();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), session.recv())
            .await
            .expect("timed out waiting for events");
        match next {
            None => return out,
            Some(event) => {
                let closed = matches!(event, crate::LiveEvent::Closed(_));
                out.push(event);
                if closed {
                    return out;
                }
            }
        }
    }
}

/// Waits for the next event, panicking after ten seconds.
pub(crate) async fn next_event(session: &mut crate::LiveSession) -> crate::LiveEvent {
    tokio::time::timeout(std::time::Duration::from_secs(10), session.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("session ended")
}

/// Request headers, one list per request.
#[cfg(feature = "sarvam")]
pub(crate) type RecordedHeaders = Arc<Mutex<Vec<Vec<(String, String)>>>>;

#[cfg(feature = "sarvam")]
/// A sequential mock HTTP server: answers the `n`th request with the `n`th
/// canned response and records every request body as JSON.
pub(crate) struct MockHttp {
    pub(crate) url: String,
    pub(crate) requests: Arc<Mutex<Vec<Value>>>,
    pub(crate) headers: RecordedHeaders,
}

#[cfg(feature = "sarvam")]
impl MockHttp {
    /// Serves `responses` (status, body) in order, one connection each.
    pub(crate) async fn start(responses: Vec<(u16, String)>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let (req_sink, head_sink) = (requests.clone(), headers.clone());
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut raw = Vec::new();
                let mut buf = [0_u8; 4096];
                let (head_end, length) = loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..pos]).to_string();
                        let mut length = 0;
                        let mut parsed = Vec::new();
                        for line in head.lines().skip(1) {
                            if let Some((k, v)) = line.split_once(':') {
                                if k.eq_ignore_ascii_case("content-length") {
                                    length = v.trim().parse().unwrap();
                                }
                                parsed.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                            }
                        }
                        head_sink.lock().unwrap().push(parsed);
                        break (pos + 4, length);
                    }
                };
                while raw.len() < head_end + length {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                // A client that hung up early leaves a short body; record
                // what arrived (or null) rather than slicing past it.
                let end = head_end.saturating_add(length).min(raw.len());
                let body_json = raw
                    .get(head_end..end)
                    .and_then(|body| serde_json::from_slice(body).ok())
                    .unwrap_or(Value::Null);
                req_sink.lock().unwrap().push(body_json);
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            url,
            requests,
            headers,
        }
    }
}

#[cfg(feature = "sarvam")]
/// An SSE body carrying `chunks` and the `[DONE]` sentinel.
pub(crate) fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str("data: ");
        out.push_str(&chunk.to_string());
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[cfg(feature = "sarvam")]
/// A chat chunk carrying text.
pub(crate) fn text_chunk(text: &str) -> Value {
    serde_json::json!({ "choices": [{ "delta": { "content": text }, "index": 0 }] })
}

#[cfg(feature = "sarvam")]
/// A chat chunk carrying one whole tool call.
pub(crate) fn tool_chunk(id: &str, name: &str, args: &str) -> Value {
    serde_json::json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 0, "id": id, "function": { "name": name, "arguments": args } }
    ] }, "index": 0 }] })
}

/// A loopback URL nothing listens on: a port bound and released by this
/// process, so connecting fails fast without touching any real service.
pub(crate) async fn closed_url(scheme: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("{scheme}://{addr}")
}
