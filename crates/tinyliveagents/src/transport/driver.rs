//! The single-socket session driver.
//!
//! [`drive`] runs one session: it pumps [`ClientCommand`]s through a
//! provider's [`WireCodec`] onto the socket and decodes every frame the socket
//! delivers back into [`LiveEvent`]s, until either side closes. A provider
//! supplies only the codec — the pure translation between the standard
//! vocabulary and its wire format — which is what makes each provider testable
//! frame by frame.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};

use futures_util::{Sink, Stream};

use super::common_close_error;
use crate::error::{Error, Result};
use crate::session::SessionChannels;
use crate::types::{ClientCommand, CloseReason, LiveEvent};

/// A message-level WebSocket: a connected `WsStream`, or a test double.
pub(crate) trait Socket:
    Sink<Message, Error = tungstenite::Error>
    + Stream<Item = std::result::Result<Message, tungstenite::Error>>
    + Unpin
    + Send
{
}

impl<T> Socket for T where
    T: Sink<Message, Error = tungstenite::Error>
        + Stream<Item = std::result::Result<Message, tungstenite::Error>>
        + Unpin
        + Send
{
}

/// What a codec makes of one incoming frame.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Decoded {
    /// An event for the host.
    Event(LiveEvent),
    /// A frame to send straight back (a protocol-level pong, say).
    #[cfg_attr(not(feature = "elevenlabs"), allow(dead_code))]
    Reply(Message),
}

/// The pure translation between the standard vocabulary and a provider's
/// wire format.
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
// Trait objects rather than generics: one compiled copy serves every
// provider and socket.
pub(crate) async fn drive(
    mut ws: Box<dyn Socket>,
    mut codec: Box<dyn WireCodec>,
    channels: SessionChannels,
) {
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
            let _ = ws.send(Message::Close(None)).await;
            return;
        }
    }

    let reason = loop {
        tokio::select! {
            command = commands.recv() => {
                let command = match command {
                    None | Some(ClientCommand::Close) => {
                        let _ = ws.send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "client closed".into(),
                        }))).await;
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
                        let (code, text) = close_parts(frame);
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
                            let _ = ws.send(Message::Close(None)).await;
                            break reason;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "tinyliveagents: undecodable frame");
                        if !events.emit(LiveEvent::Error { error, fatal: false }).await {
                            let _ = ws.send(Message::Close(None)).await;
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
async fn send_all(ws: &mut Box<dyn Socket>, frames: Vec<Message>) -> Result<()> {
    for frame in frames {
        ws.send(frame).await.map_err(send_error)?;
    }
    Ok(())
}

/// The code and reason of a close frame; 1005 ("no status") when it has none.
fn close_parts(frame: Option<CloseFrame>) -> (u16, String) {
    match frame {
        Some(frame) => (u16::from(frame.code), frame.reason.to_string()),
        None => (1005, String::new()),
    }
}

/// The error a failed send means.
#[allow(clippy::needless_pass_by_value)] // used as a `map_err` function
fn send_error(error: tungstenite::Error) -> Error {
    Error::Connect(format!("send failed: {}", io_kind(&error)))
}

fn io_kind(error: &tungstenite::Error) -> String {
    match error {
        tungstenite::Error::Io(io) => io.kind().to_string(),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            "connection closed".into()
        }
        _ => "protocol failure".into(),
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
