//! Tests for the Sarvam STT frames.

use super::*;
use crate::types::VadConfig;

fn text_of(message: &Message) -> Value {
    match message {
        Message::Text(text) => serde_json::from_str(text).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn builds_the_socket_url_with_options() {
    let mut config = LiveConfig::new().with_provider_options(serde_json::json!({
        "stt_model": "saaras:v4",
        "stt_prompt": "medical terms"
    }));
    config.vad = VadConfig {
        silence_ms: Some(400),
        threshold: Some(0.5),
        ..VadConfig::default()
    };
    let url = stt_url(DEFAULT_STT_ENDPOINT, &config, "hi-IN").unwrap();
    assert!(url.starts_with(DEFAULT_STT_ENDPOINT));
    for part in [
        "language_code=hi-IN",
        "model=saaras%3Av4",
        "encoding=linear16",
        "sample_rate=16000",
        "endpointing=vad",
        "silence_duration_ms=400",
        "threshold=0.5",
        "prompt=medical+terms",
    ] {
        assert!(url.contains(part), "{url} lacks {part}");
    }
    let plain = stt_url(DEFAULT_STT_ENDPOINT, &LiveConfig::new(), "en-IN").unwrap();
    assert!(plain.contains("model=saaras%3Av3-realtime"));
    assert!(!plain.contains("silence_duration_ms"));
    assert!(stt_url("nope", &LiveConfig::new(), "en-IN").is_err());
}

#[test]
fn encodes_client_frames() {
    assert_eq!(
        text_of(&audio_frame(&[1, 2, 3])),
        serde_json::json!({"event": "audio_input", "audio": "AQID"})
    );
    assert_eq!(text_of(&ping_frame())["event"], "ping");
    assert_eq!(text_of(&end_frame())["event"], "end");
}

#[test]
fn decodes_server_frames() {
    let decode_json = |v: Value| decode(v.to_string().as_bytes()).unwrap();
    assert_eq!(
        decode_json(serde_json::json!({"event": "session.begin"})),
        SttEvent::Begin
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "transcript.partial", "text": " hel "})),
        SttEvent::Partial("hel".into())
    );
    assert_eq!(
        decode_json(
            serde_json::json!({"event": "transcript.final", "text": "hello", "language": "en-IN"})
        ),
        SttEvent::Final {
            text: "hello".into(),
            language: Some("en-IN".into())
        }
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "transcript.final"})),
        SttEvent::Final {
            text: String::new(),
            language: None
        }
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "vad.speech_start"})),
        SttEvent::SpeechStart
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "vad.speech_end"})),
        SttEvent::SpeechEnd
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "error", "message": "bad", "is_fatal": true})),
        SttEvent::Error {
            message: "bad".into(),
            fatal: true
        }
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "error"})),
        SttEvent::Error {
            message: "unknown error".into(),
            fatal: false
        }
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "session.end"})),
        SttEvent::End
    );
    assert_eq!(
        decode_json(serde_json::json!({"event": "pong"})),
        SttEvent::Ignored
    );
    assert!(decode(b"nope").is_err());
}
