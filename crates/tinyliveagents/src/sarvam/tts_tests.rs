//! Tests for the Sarvam TTS frames and socket.

use super::*;
use crate::test_support::{MockServer, close_with, next_json};

fn wav(pcm: &[u8]) -> Vec<u8> {
    let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&3_u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // three bytes plus a pad byte
    out.extend_from_slice(b"data");
    out.extend_from_slice(&u32::try_from(pcm.len()).unwrap().to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[test]
fn strips_wav_headers_and_leaves_pcm_alone() {
    assert_eq!(strip_wav_header(wav(&[1, 2, 3, 4])), vec![1, 2, 3, 4]);
    assert_eq!(strip_wav_header(vec![1, 2, 3]), vec![1, 2, 3]);
    let mut no_data = b"RIFF\0\0\0\0WAVE".to_vec();
    no_data.extend_from_slice(b"fmt ");
    no_data.extend_from_slice(&2_u32.to_le_bytes());
    no_data.extend_from_slice(&[0, 0]);
    assert!(strip_wav_header(no_data).is_empty(), "expected nothing");
}

#[test]
fn builds_the_config_frame() {
    let frame = config_frame(&LiveConfig::new(), "en-IN");
    assert_eq!(
        frame,
        serde_json::json!({"type": "config", "data": {
            "language_code": "en-IN",
            "speaker": "shubh",
            "model": "bulbul:v3",
            "output_audio_codec": "linear16",
            "speech_sample_rate": "24000"
        }})
    );
    let custom = LiveConfig::new()
        .with_voice("priya")
        .with_provider_options(serde_json::json!({
            "tts_model": "bulbul:v2",
            "pace": 1.2,
            "output_sample_rate": 16000
        }));
    let frame = config_frame(&custom, "hi-IN");
    assert_eq!(frame["data"]["speaker"], "priya");
    assert_eq!(frame["data"]["model"], "bulbul:v2");
    assert_eq!(frame["data"]["pace"], 1.2);
    assert_eq!(frame["data"]["speech_sample_rate"], "16000");
    let odd =
        LiveConfig::new().with_provider_options(serde_json::json!({"output_sample_rate": 44100}));
    assert_eq!(output_rate(&odd), 24_000);
}

#[test]
fn decodes_server_frames() {
    let decode_json = |v: Value| decode(v.to_string().as_bytes()).unwrap();
    assert_eq!(
        decode_json(serde_json::json!({"type": "audio", "data": {"audio": "AQI="}})),
        TtsEvent::Audio(Bytes::from_static(&[1, 2]))
    );
    assert_eq!(
        decode_json(serde_json::json!({"type": "event", "data": {"event_type": "final"}})),
        TtsEvent::Final
    );
    assert_eq!(
        decode_json(serde_json::json!({"type": "event", "data": {"event_type": "other"}})),
        TtsEvent::Ignored
    );
    assert_eq!(
        decode_json(serde_json::json!({"type": "error", "data": {"message": "bad"}})),
        TtsEvent::Error("bad".into())
    );
    assert_eq!(
        decode_json(serde_json::json!({"type": "error"})),
        TtsEvent::Error("unknown error".into())
    );
    assert_eq!(
        decode_json(serde_json::json!({"type": "pong"})),
        TtsEvent::Ignored
    );
    assert!(decode(b"nope").is_err());
    assert!(decode(br#"{"type":"audio","data":{"audio":"!!"}}"#).is_err());
}

#[tokio::test]
async fn socket_configures_speaks_flushes_and_tracks_finals() {
    let server = MockServer::start(|mut ws, upgrade| async move {
        assert_eq!(upgrade.header("api-subscription-key"), Some("k"));
        assert!(upgrade.uri.contains("send_completion_event=true"));
        assert_eq!(next_json(&mut ws).await["type"], "config");
        assert_eq!(next_json(&mut ws).await["data"]["text"], "Hello there.");
        assert_eq!(next_json(&mut ws).await["type"], "flush");
        for frame in [
            serde_json::json!({"type": "event", "data": {"event_type": "start"}}),
            serde_json::json!({"type": "audio", "data": {"audio": "AQI="}}),
            serde_json::json!({"type": "event", "data": {"event_type": "final"}}),
            serde_json::json!({"type": "error", "data": {"message": "late"}}),
        ] {
            ws.send(Message::Text(frame.to_string().into()))
                .await
                .unwrap();
        }
        ws.send(Message::Binary(
            serde_json::json!({"type": "audio", "data": {"audio": "AwQ="}})
                .to_string()
                .into_bytes()
                .into(),
        ))
        .await
        .unwrap();
        close_with(&mut ws, 1000, "").await;
    })
    .await;

    let mut tts = TtsStream::open(&server.url, "k", &LiveConfig::new(), "en-IN")
        .await
        .unwrap();
    // Nothing queued: flush is a no-op and blank text is not sent.
    tts.flush().await.unwrap();
    tts.speak("  ").await.unwrap();
    assert!(!tts.is_busy());
    tts.speak("Hello there.").await.unwrap();
    tts.flush().await.unwrap();
    assert!(tts.is_busy());
    assert_eq!(
        tts.next().await.unwrap().unwrap(),
        TtsEvent::Audio(Bytes::from_static(&[1, 2]))
    );
    assert_eq!(tts.next().await.unwrap().unwrap(), TtsEvent::Final);
    assert!(!tts.is_busy());
    assert_eq!(
        tts.next().await.unwrap(),
        Err(Error::Provider("late".into()))
    );
    assert_eq!(
        tts.next().await.unwrap().unwrap(),
        TtsEvent::Audio(Bytes::from_static(&[3, 4]))
    );
    assert!(tts.next().await.is_none());
    assert!(tts.speak("again").await.is_err());
    tts.close().await;
    server.finish().await;
}

#[tokio::test]
async fn open_rejects_a_bad_endpoint() {
    assert!(matches!(
        TtsStream::open("nope", "k", &LiveConfig::new(), "en-IN").await,
        Err(Error::InvalidConfig(_))
    ));
}
