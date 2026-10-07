//! WebSocket plumbing shared by the providers.
//!
//! [`connect`] opens a socket with extra headers and maps handshake failures
//! onto [`Error`]. The single-socket providers (Gemini, ElevenLabs) also use
//! the `driver` submodule, which runs a whole session over one socket through
//! a provider's pure `WireCodec`.

use std::time::Duration;

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::{Error, Result};

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

/// A JSON text frame.
pub(crate) fn json_frame(value: &serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

#[cfg(any(feature = "gemini", feature = "elevenlabs"))]
mod driver;
#[cfg(any(feature = "gemini", feature = "elevenlabs"))]
pub(crate) use driver::{Decoded, WireCodec, drive};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
