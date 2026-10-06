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

/// Answers every tool call of the last assistant message that has no `tool`
/// message yet with `Error: cancelled`. Chat endpoints reject a history whose
/// assistant `tool_calls` are not all answered, so a turn interrupted while
/// tools ran must not leave one behind.
pub(crate) fn close_dangling_tool_calls(history: &mut Vec<Value>) {
    let Some(last) = history.iter().rposition(|m| {
        m.get("role").and_then(Value::as_str) == Some("assistant") && m.get("tool_calls").is_some()
    }) else {
        return;
    };
    let answered: Vec<&str> = history[last + 1..]
        .iter()
        .filter_map(|m| m.get("tool_call_id").and_then(Value::as_str))
        .collect();
    let missing: Vec<String> = history[last]["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .filter(|id| !answered.contains(id))
        .map(str::to_string)
        .collect();
    for id in missing {
        history.push(json!({ "role": "tool", "tool_call_id": id, "content": "Error: cancelled" }));
    }
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

/// The longest SSE line buffered while waiting for its terminator.
pub(crate) const MAX_SSE_LINE: usize = 1 << 20;

/// Splits a server-sent-event byte stream into `data:` payloads.
///
/// Bytes are buffered until a whole line arrives and only then decoded, so a
/// multi-byte UTF-8 character split across network chunks stays intact.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    /// Feeds bytes and returns every complete `data:` payload.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when a line grows past [`MAX_SSE_LINE`] bytes.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        // LF, CRLF and bare CR all end a line. A CR at the very end may be
        // the first half of a CRLF, so it waits for the next bytes.
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n' || *b == b'\r') {
            if end > MAX_SSE_LINE {
                return Err(Error::Protocol(
                    "sarvam chat stream line is too long".into(),
                ));
            }
            let terminator = match (self.buffer[end], self.buffer.get(end + 1)) {
                (b'\r', Some(b'\n')) => 2,
                (b'\r', None) => break,
                _ => 1,
            };
            let raw: Vec<u8> = self.buffer.drain(..end + terminator).collect();
            Self::take_line(&raw[..end], &mut out);
        }
        if self.buffer.len() > MAX_SSE_LINE {
            return Err(Error::Protocol(
                "sarvam chat stream line is too long".into(),
            ));
        }
        Ok(out)
    }

    /// Returns the payload of a final line the stream ended without
    /// terminating.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        let raw = std::mem::take(&mut self.buffer);
        let mut out = Vec::new();
        Self::take_line(raw.strip_suffix(b"\r").unwrap_or(&raw), &mut out);
        out
    }

    fn take_line(line: &[u8], out: &mut Vec<String>) {
        let line = String::from_utf8_lossy(line);
        if let Some(data) = line.strip_prefix("data:") {
            out.push(data.trim_start().to_string());
        }
    }
}

/// Folds streamed deltas into the reply.
#[derive(Debug, Default)]
pub(crate) struct ChatAccumulator {
    pub(crate) text: String,
    calls: BTreeMap<u64, (String, String, String)>,
    pub(crate) usage: Option<Usage>,
    finish_reason: Option<String>,
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
        if let Some(reason) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            self.finish_reason = Some(reason.to_string());
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

    /// Whether the model reported why it stopped (`stop`, `tool_calls`, ...),
    /// i.e. the completion was not cut off.
    pub(crate) fn finished(&self) -> bool {
        self.finish_reason.is_some()
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
    saw_done: bool,
}

impl std::fmt::Debug for ChatStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatStream")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl ChatStream {
    /// Whether the server ended the stream with `[DONE]`.
    pub(crate) fn completed(&self) -> bool {
        self.saw_done
    }

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
                    self.saw_done = true;
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
                    let rest = self.parser.finish();
                    if rest.is_empty() {
                        return None;
                    }
                    // Hand back the unterminated last line before ending.
                    self.done = false;
                    self.bytes = Box::pin(futures_util::stream::empty());
                    self.queued.extend(rest);
                }
                Some(Err(_)) => {
                    self.done = true;
                    return Some(Err(Error::Connect("sarvam chat stream failed".into())));
                }
                Some(Ok(bytes)) => match self.parser.push(&bytes) {
                    Ok(lines) => self.queued.extend(lines),
                    Err(error) => {
                        self.done = true;
                        return Some(Err(error));
                    }
                },
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
        saw_done: false,
    })
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
