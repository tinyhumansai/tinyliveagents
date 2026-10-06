//! WebSocket plumbing shared by the single-socket providers.
//!
//! [`connect`] opens a socket with extra headers and maps handshake failures
//! onto [`Error`]. [`drive`] runs one session: it pumps
//! [`ClientCommand`]s through a provider's [`WireCodec`] onto the socket and
//! decodes every frame the socket delivers back into [`LiveEvent`]s, until
//! either side closes. A provider supplies only the codec — the pure
//! translation between the standard vocabulary and its wire format — which is
//! what makes each provider testable frame by frame.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::{Error, Result};
use crate::session::SessionChannels;
use crate::types::{ClientCommand, CloseReason, LiveEvent};

/// A connected client socket.
pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long a handshake may take before the connection is abandoned.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Opens a WebSocket to `url`, adding `headers` to the upgrade request.
///
/// # Errors
///
/// [`Error::Unauthorized`], [`Error::InsufficientCredits`] or
/// [`Error::RateLimited`] when the upgrade is refused with 401/403, 402 or 429;
/// [`Error::Timeout`] when the handshake does not finish in
/// [`CONNECT_TIMEOUT`]; [`Error::Connect`] otherwise.
pub(crate) async fn connect(url: &str, headers: &[(&'static str, String)]) -> Result<WsStream> {
    connect_within(url, headers, CONNECT_TIMEOUT).await
}

/// [`connect`] with an explicit handshake timeout.
pub(crate) async fn connect_within(
    url: &str,
    headers: &[(&'static str, String)],
    timeout: Duration,
) -> Result<WsStream> {
    let mut request = url
        .into_client_request()
        .map_err(|_| Error::Connect("invalid url".into()))?;
    for (name, value) in headers {
        let value = HeaderValue::from_str(value)
            .map_err(|_| Error::InvalidConfig(format!("header {name} is not valid")))?;
        request.headers_mut().insert(*name, value);
    }
    let attempt = tokio_tungstenite::connect_async(request);
    match tokio::time::timeout(timeout, attempt).await {
        Err(_) => Err(Error::Timeout),
        Ok(Err(error)) => Err(handshake_error(&error)),
        Ok(Ok((stream, _response))) => Ok(stream),
    }
}

/// Maps a failed handshake onto the crate error, without echoing the URL
/// (which can carry a ticket or key).
pub(crate) fn handshake_error(error: &tungstenite::Error) -> Error {
    match error {
        tungstenite::Error::Http(response) => match response.status().as_u16() {
            401 | 403 => Error::Unauthorized,
            402 => Error::InsufficientCredits,
            429 => Error::RateLimited,
            status => Error::Connect(format!("upgrade refused with http {status}")),
        },
        tungstenite::Error::Io(io) => Error::Connect(io.kind().to_string()),
        _ => Error::Connect("handshake failed".into()),
    }
}

/// Maps the close codes every provider shares. Provider codecs check their own
/// codes first and fall back to this.
pub(crate) fn common_close_error(code: u16, reason: &str) -> Option<Error> {
    match code {
        1000 | 1001 | 1005 => None,
        1008 if reason.to_ascii_lowercase().contains("quota") => Some(Error::RateLimited),
        1008 => Some(Error::Provider(format!("policy violation: {reason}"))),
        1011 => Some(Error::Provider(format!("server error: {reason}"))),
        4401 => Some(Error::Unauthorized),
        4402 => Some(Error::InsufficientCredits),
        4408 => Some(Error::Timeout),
        4429 => Some(Error::RateLimited),
        _ => Some(Error::Provider(format!(
            "closed with code {code}: {reason}"
        ))),
    }
}

/// What a codec makes of one incoming frame.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(any(feature = "gemini", feature = "elevenlabs")), allow(dead_code))]
pub(crate) enum Decoded {
    /// An event for the host.
    Event(LiveEvent),
    /// A frame to send straight back (a protocol-level pong, say).
    #[cfg_attr(not(feature = "elevenlabs"), allow(dead_code))]
    Reply(Message),
}

/// The pure translation between the standard vocabulary and a provider's
/// wire format.
#[cfg_attr(not(any(feature = "gemini", feature = "elevenlabs")), allow(dead_code))]
pub(crate) trait WireCodec: Send + 'static {
    /// Frames to send and events to emit as soon as the socket is open.
    fn on_open(&mut self) -> (Vec<Message>, Vec<LiveEvent>);

    /// Frames that carry `command`. An empty vector means the provider has no
    /// equivalent and the command is dropped.
    fn encode(&mut self, command: ClientCommand) -> Vec<Message>;

    /// Decodes one text or binary frame.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] for a frame that is not valid for the provider. The
    /// driver reports it as a non-fatal [`LiveEvent::Error`] and carries on.
    fn decode(&mut self, frame: &[u8]) -> Result<Vec<Decoded>>;

    /// The error a close code means, or `None` for a normal close.
    fn close_error(&self, code: u16, reason: &str) -> Option<Error> {
        common_close_error(code, reason)
    }
}

/// Runs a session over `ws` with `codec` until either side closes, then emits
/// [`LiveEvent::Closed`] exactly once.
// One `select!` over commands and socket frames; see `WireCodec` for the
// provider-specific parts.
#[allow(clippy::too_many_lines)]
#[cfg_attr(not(any(feature = "gemini", feature = "elevenlabs")), allow(dead_code))]
pub(crate) async fn drive<C: WireCodec>(mut ws: WsStream, mut codec: C, channels: SessionChannels) {
    let SessionChannels {
        mut commands,
        events,
    } = channels;
    let (opening, initial_events) = codec.on_open();
    if let Err(error) = send_all(&mut ws, opening).await {
        events
            .emit(LiveEvent::Closed(CloseReason::Error(error)))
            .await;
        return;
    }
    for event in initial_events {
        if !events.emit(event).await {
            let _ = ws.close(None).await;
            return;
        }
    }

    let reason = loop {
        tokio::select! {
            command = commands.recv() => {
                let command = match command {
                    None | Some(ClientCommand::Close) => {
                        let _ = ws.close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "client closed".into(),
                        })).await;
                        break CloseReason::Client;
                    }
                    Some(command) => command,
                };
                if let Err(error) = send_all(&mut ws, codec.encode(command)).await {
                    break CloseReason::Error(error);
                }
            }
            frame = ws.next() => {
                let payload = match frame {
                    None => break CloseReason::Remote { code: None, reason: String::new() },
                    Some(Err(error)) => {
                        break CloseReason::Error(Error::Connect(format!("receive failed: {}", io_kind(&error))));
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, text) = frame.map_or((1005, String::new()), |f| {
                            (u16::from(f.code), f.reason.to_string())
                        });
                        tracing::debug!(code, "tinyliveagents: remote closed");
                        break match codec.close_error(code, &text) {
                            Some(error) => CloseReason::Error(error),
                            None => CloseReason::Remote { code: Some(code), reason: text },
                        };
                    }
                    Some(Ok(Message::Text(text))) => text.as_bytes().to_vec(),
                    Some(Ok(Message::Binary(bytes))) => bytes.to_vec(),
                    Some(Ok(_)) => continue,
                };
                match codec.decode(&payload) {
                    Ok(decoded) => {
                        let mut stop = None;
                        for item in decoded {
                            match item {
                                Decoded::Event(event) => {
                                    let fatal = matches!(event, LiveEvent::Error { fatal: true, .. });
                                    if !events.emit(event).await {
                                        stop = Some(CloseReason::Client);
                                        break;
                                    }
                                    if fatal {
                                        stop = Some(CloseReason::Error(Error::Provider("fatal provider error".into())));
                                        break;
                                    }
                                }
                                Decoded::Reply(frame) => {
                                    if let Err(error) = send_all(&mut ws, vec![frame]).await {
                                        stop = Some(CloseReason::Error(error));
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(reason) = stop {
                            let _ = ws.close(None).await;
                            break reason;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "tinyliveagents: undecodable frame");
                        if !events.emit(LiveEvent::Error { error, fatal: false }).await {
                            let _ = ws.close(None).await;
                            break CloseReason::Client;
                        }
                    }
                }
            }
        }
    };
    events.emit(LiveEvent::Closed(reason)).await;
}

/// Sends `frames` in order.
#[cfg_attr(not(any(feature = "gemini", feature = "elevenlabs")), allow(dead_code))]
async fn send_all(ws: &mut WsStream, frames: Vec<Message>) -> Result<()> {
    for frame in frames {
        ws.send(frame)
            .await
            .map_err(|error| Error::Connect(format!("send failed: {}", io_kind(&error))))?;
    }
    Ok(())
}

#[cfg_attr(not(any(feature = "gemini", feature = "elevenlabs")), allow(dead_code))]
fn io_kind(error: &tungstenite::Error) -> String {
    match error {
        tungstenite::Error::Io(io) => io.kind().to_string(),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            "connection closed".into()
        }
        _ => "protocol failure".into(),
    }
}

/// A JSON text frame.
pub(crate) fn json_frame(value: &serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
