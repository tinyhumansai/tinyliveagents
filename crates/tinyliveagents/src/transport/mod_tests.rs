//! Tests for WebSocket connection setup and close-code mapping.

use super::*;
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
}
