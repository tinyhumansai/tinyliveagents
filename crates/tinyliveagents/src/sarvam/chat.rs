//! Sarvam chat completions with streaming and tool calling.
//!
//! The endpoint is OpenAI-compatible: `messages` with `system` / `user` /
//! `assistant` (optionally carrying `tool_calls`) / `tool` roles, `tools` as
//! function declarations, and `stream: true` for server-sent events whose
//! `choices[0].delta` carries `content` text and `tool_calls` fragments keyed
//! by `index`. [`ChatAccumulator`] folds the fragments into the reply text and
//! complete [`ToolCall`]s.

use std::collections::BTreeMap;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::types::{LiveConfig, ToolCall, ToolDeclaration, ToolResult, Usage};

/// Sarvam's chat completions endpoint.
pub const DEFAULT_CHAT_ENDPOINT: &str = "https://api.sarvam.ai/v1/chat/completions";
/// The default chat model: Sarvam's low-latency conversational model.
pub const DEFAULT_CHAT_MODEL: &str = "sarvam-105b-conversations";

/// A system message.
pub(crate) fn system_message(text: &str) -> Value {
    json!({ "role": "system", "content": text })
}

/// A user message.
pub(crate) fn user_message(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

/// An assistant message, with the tool calls it made (if any).
pub(crate) fn assistant_message(text: &str, calls: &[ToolCall]) -> Value {
    let mut message = json!({ "role": "assistant", "content": text });
    if !calls.is_empty() {
        message["tool_calls"] = calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.call_id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.args.to_string() },
                })
            })
            .collect();
    }
    message
}

/// A tool result message.
pub(crate) fn tool_message(result: &ToolResult) -> Value {
    let content = if result.is_error {
        format!("Error: {}", result.output_text())
    } else {
        result.output_text()
    };
    json!({ "role": "tool", "tool_call_id": result.call_id, "content": content })
}

/// The request body.
pub(crate) fn request_body(config: &LiveConfig, messages: &[Value]) -> Value {
    let mut body = json!({
        "model": config.model.as_deref().unwrap_or(DEFAULT_CHAT_MODEL),
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !config.tools.is_empty() {
        body["tools"] = config.tools.iter().map(tool_spec).collect();
    }
    for key in ["temperature", "max_tokens", "reasoning_effort", "top_p"] {
        if let Some(value) = config.provider_options.get(key) {
            body[key] = value.clone();
        }
    }
    body
}

fn tool_spec(tool: &ToolDeclaration) -> Value {
    let parameters = if tool.parameters.is_object() {
        tool.parameters.clone()
    } else {
        json!({ "type": "object", "properties": {} })
    };
    json!({
        "type": "function",
        "function": { "name": tool.name, "description": tool.description, "parameters": parameters },
    })
}

/// Splits a server-sent-event byte stream into `data:` payloads.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    buffer: String,
}

impl SseParser {
    /// Feeds bytes and returns every complete `data:` payload.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(end) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=end).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data:") {
                out.push(data.trim_start().to_string());
            }
        }
        out
    }
}

/// Folds streamed deltas into the reply.
#[derive(Debug, Default)]
pub(crate) struct ChatAccumulator {
    pub(crate) text: String,
    calls: BTreeMap<u64, (String, String, String)>,
    pub(crate) usage: Option<Usage>,
}

impl ChatAccumulator {
    /// Applies one chunk; returns the text it added, if any.
    pub(crate) fn apply(&mut self, chunk: &Value) -> Option<String> {
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Usage {
                input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
                output_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
                total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
                audio_seconds: None,
            });
        }
        let delta = chunk.pointer("/choices/0/delta")?;
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.0 = id.to_string();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    entry.1.push_str(name);
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.2.push_str(args);
                }
            }
        }
        let text = delta.get("content").and_then(Value::as_str)?;
        if text.is_empty() {
            return None;
        }
        self.text.push_str(text);
        Some(text.to_string())
    }

    /// The completed tool calls. Unparseable arguments become `{}` with the
    /// raw text kept under `_raw`, so the host can still answer the call.
    pub(crate) fn tool_calls(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .filter(|(_, (_, name, _))| !name.is_empty())
            .map(|(index, (id, name, args))| ToolCall {
                call_id: if id.is_empty() {
                    format!("call-{index}")
                } else {
                    id.clone()
                },
                name: name.clone(),
                args: if args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(args).unwrap_or_else(|_| json!({ "_raw": args }))
                },
            })
            .collect()
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// A streaming chat completion in progress.
pub(crate) struct ChatStream {
    bytes: ByteStream,
    parser: SseParser,
    queued: std::collections::VecDeque<String>,
    done: bool,
}

impl std::fmt::Debug for ChatStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatStream")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl ChatStream {
    /// The next parsed chunk, or `None` at the end of the stream.
    ///
    /// # Errors
    ///
    /// [`Error::Connect`] when the body stream fails and [`Error::Protocol`]
    /// for a chunk that is not JSON.
    pub(crate) async fn next(&mut self) -> Option<Result<Value>> {
        loop {
            if let Some(data) = self.queued.pop_front() {
                if data == "[DONE]" {
                    self.done = true;
                    return None;
                }
                return Some(
                    serde_json::from_str(&data)
                        .map_err(|_| Error::Protocol("sarvam chat chunk is not json".into())),
                );
            }
            if self.done {
                return None;
            }
            match self.bytes.next().await {
                None => {
                    self.done = true;
                    return None;
                }
                Some(Err(_)) => {
                    self.done = true;
                    return Some(Err(Error::Connect("sarvam chat stream failed".into())));
                }
                Some(Ok(bytes)) => self.queued.extend(self.parser.push(&bytes)),
            }
        }
    }
}

/// Starts a streaming completion.
///
/// # Errors
///
/// [`Error::Unauthorized`], [`Error::RateLimited`] or [`Error::Provider`] for
/// a refused request, and [`Error::Connect`] when the endpoint is unreachable.
pub(crate) async fn start(
    http: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    body: &Value,
) -> Result<ChatStream> {
    let response = http
        .post(endpoint)
        .header("api-subscription-key", api_key)
        .json(body)
        .send()
        .await
        .map_err(|_| Error::Connect("sarvam chat endpoint unreachable".into()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let message = response
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| {
                v.pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("http {status}"));
        return Err(match status {
            401 | 403 => Error::Unauthorized,
            429 => Error::RateLimited,
            _ => Error::Provider(message),
        });
    }
    Ok(ChatStream {
        bytes: Box::pin(response.bytes_stream()),
        parser: SseParser::default(),
        queued: std::collections::VecDeque::new(),
        done: false,
    })
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
