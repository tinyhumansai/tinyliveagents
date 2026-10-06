//! Tests for the Sarvam chat client.

use super::*;
use crate::testkit::{MockHttp, sse, text_chunk, tool_chunk};

#[test]
fn builds_messages() {
    assert_eq!(
        system_message("s"),
        json!({"role": "system", "content": "s"})
    );
    assert_eq!(user_message("u"), json!({"role": "user", "content": "u"}));
    assert_eq!(
        assistant_message("hi", &[]),
        json!({"role": "assistant", "content": "hi"})
    );
    let call = ToolCall {
        call_id: "c1".into(),
        name: "f".into(),
        args: json!({"a": 1}),
    };
    assert_eq!(
        assistant_message("", std::slice::from_ref(&call))["tool_calls"][0],
        json!({"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}})
    );
    assert_eq!(
        tool_message(&ToolResult::ok(&call, "done")),
        json!({"role": "tool", "tool_call_id": "c1", "content": "done"})
    );
    assert_eq!(
        tool_message(&ToolResult::error(&call, "denied"))["content"],
        "Error: denied"
    );
}

#[test]
fn builds_the_request_body() {
    let config = LiveConfig::new()
        .with_tool(ToolDeclaration::new("f", "d", json!({"type": "object"})))
        .with_tool(ToolDeclaration::new("g", "d", Value::Null))
        .with_provider_options(json!({"temperature": 0.3, "max_tokens": 200, "other": 1}));
    let body = request_body(&config, &[user_message("hi")]);
    assert_eq!(body["model"], DEFAULT_CHAT_MODEL);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["tools"][0]["function"]["name"], "f");
    assert_eq!(
        body["tools"][1]["function"]["parameters"],
        json!({"type": "object", "properties": {}})
    );
    assert_eq!(body["temperature"], 0.3);
    assert_eq!(body["max_tokens"], 200);
    assert!(body.get("other").is_none());

    let bare = request_body(&LiveConfig::new().with_model("sarvam-105b"), &[]);
    assert_eq!(bare["model"], "sarvam-105b");
    assert!(bare.get("tools").is_none());
}

#[test]
fn parses_sse_across_chunk_boundaries() {
    let mut parser = SseParser::default();
    assert!(parser.push(b"data: {\"a\"").is_empty());
    assert_eq!(
        parser.push(b":1}\r\n\r\n: comment\ndata:[DONE]\n"),
        vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]
    );
}

#[test]
fn accumulates_text_tool_calls_and_usage() {
    let mut acc = ChatAccumulator::default();
    assert_eq!(acc.apply(&text_chunk("Hel")), Some("Hel".into()));
    assert_eq!(acc.apply(&text_chunk("")), None);
    assert_eq!(acc.apply(&json!({"choices": []})), None);
    acc.apply(&tool_chunk("c1", "get_", "{\"tz\": "));
    acc.apply(&json!({"choices": [{"delta": {"tool_calls": [
        {"index": 0, "id": null, "function": {"name": "time", "arguments": "\"UTC\"}"}},
        {"index": 1, "function": {"name": "broken", "arguments": "{oops"}},
        {"index": 2, "function": {"name": "noargs"}},
        {"index": 3, "function": {"arguments": "{}"}}
    ]}}]}));
    acc.apply(&json!({"choices": [{"delta": {}}], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}));
    assert_eq!(acc.text, "Hel");
    let calls = acc.tool_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].call_id, "c1");
    assert_eq!(calls[0].name, "get_time");
    assert_eq!(calls[0].args, json!({"tz": "UTC"}));
    assert_eq!(calls[1].call_id, "call-1");
    assert_eq!(calls[1].args, json!({"_raw": "{oops"}));
    assert_eq!(calls[2].args, json!({}));
    assert_eq!(acc.usage.unwrap().total_tokens, Some(7));
}

#[tokio::test]
async fn streams_a_completion() {
    let server = MockHttp::start(vec![(200, sse(&[text_chunk("Hi"), text_chunk(" there")]))]).await;
    let http = reqwest::Client::new();
    let mut stream = start(&http, &server.url, "key", &json!({"x": 1}))
        .await
        .unwrap();
    let mut acc = ChatAccumulator::default();
    while let Some(chunk) = stream.next().await {
        acc.apply(&chunk.unwrap());
    }
    assert!(stream.next().await.is_none());
    assert_eq!(acc.text, "Hi there");
    assert_eq!(server.requests.lock().unwrap()[0], json!({"x": 1}));
    let headers = server.headers.lock().unwrap()[0].clone();
    assert!(headers.contains(&("api-subscription-key".into(), "key".into())));
    assert!(format!("{stream:?}").contains("done"));
}

#[tokio::test]
async fn reports_refusals_and_bad_chunks() {
    let server = MockHttp::start(vec![
        (401, String::new()),
        (429, String::new()),
        (500, r#"{"error":{"message":"overloaded"}}"#.into()),
        (400, "not json".into()),
        (200, "data: nope\n\n".into()),
        (200, "data: {}\n\n".into()),
    ])
    .await;
    let http = reqwest::Client::new();
    let body = json!({});
    assert_eq!(
        start(&http, &server.url, "k", &body).await.err(),
        Some(Error::Unauthorized)
    );
    assert_eq!(
        start(&http, &server.url, "k", &body).await.err(),
        Some(Error::RateLimited)
    );
    assert_eq!(
        start(&http, &server.url, "k", &body).await.err(),
        Some(Error::Provider("overloaded".into()))
    );
    assert_eq!(
        start(&http, &server.url, "k", &body).await.err(),
        Some(Error::Provider("http 400".into()))
    );
    let mut stream = start(&http, &server.url, "k", &body).await.unwrap();
    assert!(matches!(stream.next().await, Some(Err(Error::Protocol(_)))));
    // A stream that ends without [DONE] simply ends.
    let mut stream = start(&http, &server.url, "k", &body).await.unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());
    assert_eq!(
        start(&http, "http://127.0.0.1:9/x", "k", &body).await.err(),
        Some(Error::Connect("sarvam chat endpoint unreachable".into()))
    );
}

#[test]
fn answers_dangling_tool_calls_once() {
    let call = |id: &str| ToolCall {
        call_id: id.into(),
        name: "f".into(),
        args: json!({}),
    };
    let mut history = vec![
        user_message("q"),
        assistant_message("", &[call("a"), call("b")]),
        tool_message(&ToolResult::ok(&call("a"), "done")),
    ];
    close_dangling_tool_calls(&mut history);
    assert_eq!(history.len(), 4);
    assert_eq!(
        history[3],
        json!({"role": "tool", "tool_call_id": "b", "content": "Error: cancelled"})
    );
    // Already consistent: nothing more is added.
    close_dangling_tool_calls(&mut history);
    assert_eq!(history.len(), 4);
    // No tool calls at all: untouched.
    let mut plain = vec![user_message("q"), assistant_message("hi", &[])];
    close_dangling_tool_calls(&mut plain);
    assert_eq!(plain.len(), 2);
}
