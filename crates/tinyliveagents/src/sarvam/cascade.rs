//! The Sarvam cascade: STT → chat → TTS behind one live session.
//!
//! The session task owns the STT socket and the conversation history. Every
//! finished user utterance (`transcript.final`, or a typed
//! [`ClientCommand::Text`]) starts a *turn*: a task that streams a chat
//! completion, speaks the reply through a fresh TTS socket as it streams, and,
//! when the model calls tools, emits [`LiveEvent::ToolCall`]s and waits for the
//! host's [`ToolResult`]s before asking the model again.
//!
//! Barge-in: a `vad.speech_start` (or [`ClientCommand::Interrupt`]) while a turn
//! is running aborts it, cancels its outstanding tool calls and emits
//! [`LiveEvent::Interrupted`], so the host drops queued playback. History the
//! turn already committed (the user message, finished assistant and tool
//! messages) is kept; the interrupted reply is not.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use super::chat::{self, ChatAccumulator};
use super::chunker::SentenceChunker;
use super::stt::{self, SttEvent};
use super::tts::{TtsEvent, TtsStream};
use crate::error::{Error, Result};
use crate::session::{EventSink, SessionChannels};
use crate::transport::{WsStream, common_close_error};
use crate::types::{ClientCommand, CloseReason, LiveConfig, LiveEvent, SessionInfo, ToolResult};

/// Keepalive interval for the STT socket.
pub(crate) const STT_PING_INTERVAL: Duration = Duration::from_secs(15);
/// Longest wait for TTS to finish speaking a reply.
pub(crate) const TTS_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Most model ↔ tool round trips in one turn.
pub(crate) const MAX_TOOL_ROUNDS: usize = 8;

/// Languages Sarvam TTS can speak.
pub(crate) const TTS_LANGUAGES: &[&str] = &[
    "en-IN", "hi-IN", "bn-IN", "gu-IN", "kn-IN", "ml-IN", "mr-IN", "od-IN", "pa-IN", "ta-IN",
    "te-IN",
];

/// Everything a turn needs that does not change during the session.
#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) http: reqwest::Client,
    pub(crate) chat_endpoint: String,
    pub(crate) tts_endpoint: String,
    pub(crate) api_key: String,
    pub(crate) config: LiveConfig,
    pub(crate) language: Option<String>,
}

impl TurnContext {
    /// The language to speak in: the configured one, else the one STT
    /// detected (when TTS supports it), else English (India).
    pub(crate) fn speech_language(&self, detected: Option<&str>) -> String {
        self.language
            .clone()
            .or_else(|| {
                detected
                    .filter(|lang| TTS_LANGUAGES.contains(lang))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "en-IN".to_string())
    }
}

/// What a turn reports back to the session task.
#[derive(Debug)]
pub(crate) enum TurnMsg {
    /// An event for the host.
    Event(LiveEvent),
    /// A message to append to the history.
    Commit(Value),
    /// The turn ended.
    Done,
}

/// How a turn starts.
#[derive(Debug, Clone)]
pub(crate) enum TurnInput {
    /// Ask the model (the user message is already in `messages`).
    Ask,
    /// Speak this text verbatim (the first message).
    Say(String),
}

struct Turn {
    id: u64,
    task: JoinHandle<()>,
    tools: mpsc::Sender<ToolResult>,
    pending_calls: Vec<String>,
}

/// Runs the session until the host closes it or STT ends.
// One `select!` over commands, STT frames, turn messages and the keepalive;
// splitting it would scatter the state it shares.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run(
    mut stt_ws: WsStream,
    ctx: Arc<TurnContext>,
    channels: SessionChannels,
    info: SessionInfo,
    first_message: Option<String>,
) {
    let SessionChannels {
        mut commands,
        events,
    } = channels;
    if !events.emit(LiveEvent::Ready(info)).await {
        let _ = stt_ws.close(None).await;
        return;
    }
    let mut history: Vec<Value> = ctx
        .config
        .system_instruction
        .as_deref()
        .map(chat::system_message)
        .into_iter()
        .collect();
    let (turn_tx, mut turn_rx) = mpsc::channel::<(u64, TurnMsg)>(crate::CHANNEL_CAPACITY);
    let mut turn: Option<Turn> = None;
    let mut next_id = 0_u64;
    let mut detected: Option<String> = None;
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + STT_PING_INTERVAL,
        STT_PING_INTERVAL,
    );

    if let Some(text) = first_message {
        next_id += 1;
        turn = Some(spawn_turn(
            next_id,
            &ctx,
            history.clone(),
            TurnInput::Say(text),
            ctx.speech_language(None),
            &turn_tx,
        ));
    }

    let reason = loop {
        tokio::select! {
            command = commands.recv() => match command {
                None | Some(ClientCommand::Close) => {
                    let _ = stt_ws.send(stt::end_frame()).await;
                    let _ = stt_ws.close(None).await;
                    break CloseReason::Client;
                }
                Some(ClientCommand::Audio(pcm)) => {
                    if stt_ws.send(stt::audio_frame(&pcm)).await.is_err() {
                        break CloseReason::Error(Error::Connect("sarvam stt socket closed".into()));
                    }
                }
                Some(ClientCommand::Text(text)) => {
                    if !text.trim().is_empty() {
                        stop_turn(&mut turn, &events, false).await;
                        history.push(chat::user_message(&text));
                        next_id += 1;
                        turn = Some(spawn_turn(next_id, &ctx, history.clone(), TurnInput::Ask, ctx.speech_language(detected.as_deref()), &turn_tx));
                    }
                }
                Some(ClientCommand::ToolResult(result)) => {
                    if let Some(active) = turn.as_mut() {
                        active.pending_calls.retain(|id| id != &result.call_id);
                        let _ = active.tools.send(result).await;
                    }
                }
                Some(ClientCommand::Interrupt) => stop_turn(&mut turn, &events, true).await,
                Some(_) => {}
            },
            frame = stt_ws.next() => {
                let payload = match frame {
                    None => break CloseReason::Remote { code: None, reason: String::new() },
                    Some(Err(_)) => break CloseReason::Error(Error::Connect("sarvam stt socket failed".into())),
                    Some(Ok(Message::Close(frame))) => {
                        let (code, text) = frame.map_or((1005, String::new()), |f| {
                            (u16::from(f.code), f.reason.to_string())
                        });
                        break match sarvam_close_error(code, &text) {
                            Some(error) => CloseReason::Error(error),
                            None => CloseReason::Remote { code: Some(code), reason: text },
                        };
                    }
                    Some(Ok(Message::Text(text))) => text.as_bytes().to_vec(),
                    Some(Ok(Message::Binary(bytes))) => bytes.to_vec(),
                    Some(Ok(_)) => continue,
                };
                let event = match stt::decode(&payload) {
                    Ok(event) => event,
                    Err(error) => {
                        events.emit(LiveEvent::Error { error, fatal: false }).await;
                        continue;
                    }
                };
                match event {
                    SttEvent::Partial(text) if !text.is_empty() => {
                        events.emit(LiveEvent::InputTranscript { text, is_final: false }).await;
                    }
                    SttEvent::Final { text, language } if !text.is_empty() => {
                        events.emit(LiveEvent::InputTranscript { text: text.clone(), is_final: true }).await;
                        if language.is_some() {
                            detected = language;
                        }
                        stop_turn(&mut turn, &events, false).await;
                        history.push(chat::user_message(&text));
                        next_id += 1;
                        turn = Some(spawn_turn(next_id, &ctx, history.clone(), TurnInput::Ask, ctx.speech_language(detected.as_deref()), &turn_tx));
                    }
                    SttEvent::SpeechStart => stop_turn(&mut turn, &events, true).await,
                    SttEvent::Error { message, fatal } => {
                        let error = stt_error(message);
                        events.emit(LiveEvent::Error { error: error.clone(), fatal }).await;
                        if fatal {
                            let _ = stt_ws.close(None).await;
                            break CloseReason::Error(error);
                        }
                    }
                    SttEvent::End => break CloseReason::Remote { code: None, reason: "stt session ended".into() },
                    _ => {}
                }
            }
            message = turn_rx.recv() => {
                let Some((id, message)) = message else { continue };
                match message {
                    TurnMsg::Commit(value) => history.push(value),
                    TurnMsg::Event(event) => {
                        let current = turn.as_mut().filter(|t| t.id == id);
                        if let Some(active) = current {
                            if let LiveEvent::ToolCall(call) = &event {
                                active.pending_calls.push(call.call_id.clone());
                            }
                            events.emit(event).await;
                        }
                    }
                    TurnMsg::Done => {
                        if turn.as_ref().is_some_and(|t| t.id == id) {
                            turn = None;
                        }
                    }
                }
            }
            _ = ping.tick() => {
                if stt_ws.send(stt::ping_frame()).await.is_err() {
                    break CloseReason::Error(Error::Connect("sarvam stt socket closed".into()));
                }
            }
        }
    };
    if let Some(active) = turn.take() {
        active.task.abort();
    }
    events.emit(LiveEvent::Closed(reason)).await;
}

/// Classifies an STT error message. Sarvam reports a refused key as an
/// `error` event ("Invalid subscription key ...") before closing.
pub(crate) fn stt_error(message: String) -> Error {
    if message.to_ascii_lowercase().contains("subscription key") {
        Error::Unauthorized
    } else {
        Error::Provider(message)
    }
}

/// Close codes the STT socket uses.
pub(crate) fn sarvam_close_error(code: u16, reason: &str) -> Option<Error> {
    match code {
        1003 => Some(Error::Unauthorized),
        1008 => Some(Error::Timeout),
        4000 => Some(Error::InvalidConfig(reason.to_string())),
        _ => common_close_error(code, reason),
    }
}

/// Aborts the running turn, if any. With `barge_in`, tells the host to drop
/// playback; outstanding tool calls are always cancelled.
async fn stop_turn(turn: &mut Option<Turn>, events: &EventSink, barge_in: bool) {
    let Some(active) = turn.take() else {
        return;
    };
    active.task.abort();
    tracing::debug!(
        turn = active.id,
        barge_in,
        "tinyliveagents: sarvam turn stopped"
    );
    if !active.pending_calls.is_empty() {
        events
            .emit(LiveEvent::ToolCallCancelled {
                call_ids: active.pending_calls,
            })
            .await;
    }
    if barge_in {
        events.emit(LiveEvent::Interrupted).await;
    }
}

fn spawn_turn(
    id: u64,
    ctx: &Arc<TurnContext>,
    messages: Vec<Value>,
    input: TurnInput,
    language: String,
    out: &mpsc::Sender<(u64, TurnMsg)>,
) -> Turn {
    let (tools_tx, tools_rx) = mpsc::channel(32);
    let out = out.clone();
    let ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let sink = TurnSink { id, out };
        run_turn(&ctx, messages, input, &language, &sink, tools_rx).await;
        sink.send(TurnMsg::Done).await;
    });
    Turn {
        id,
        task,
        tools: tools_tx,
        pending_calls: Vec::new(),
    }
}

/// Tags a turn's messages with its id.
struct TurnSink {
    id: u64,
    out: mpsc::Sender<(u64, TurnMsg)>,
}

impl TurnSink {
    async fn send(&self, message: TurnMsg) {
        let _ = self.out.send((self.id, message)).await;
    }

    async fn event(&self, event: LiveEvent) {
        self.send(TurnMsg::Event(event)).await;
    }

    async fn error(&self, error: Error) {
        self.event(LiveEvent::Error {
            error,
            fatal: false,
        })
        .await;
    }
}

/// The next TTS event, or never when there is no socket.
async fn tts_next(tts: &mut Option<TtsStream>) -> Option<Result<TtsEvent>> {
    match tts.as_mut() {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Forwards a TTS event; drops the socket when it failed or closed.
async fn forward_tts(
    event: Option<Result<TtsEvent>>,
    tts: &mut Option<TtsStream>,
    sink: &TurnSink,
) {
    match event {
        Some(Ok(TtsEvent::Audio(audio))) => sink.event(LiveEvent::Audio(audio)).await,
        Some(Ok(_)) => {}
        Some(Err(error)) => {
            sink.error(error).await;
            *tts = None;
        }
        None => *tts = None,
    }
}

async fn open_tts(ctx: &TurnContext, language: &str, sink: &TurnSink) -> Option<TtsStream> {
    match TtsStream::open(&ctx.tts_endpoint, &ctx.api_key, &ctx.config, language).await {
        Ok(stream) => Some(stream),
        Err(error) => {
            // Keep the turn going text-only; the transcript still reaches the host.
            sink.error(error).await;
            None
        }
    }
}

async fn speak(tts: &mut Option<TtsStream>, text: &str, sink: &TurnSink) {
    if let Some(stream) = tts.as_mut()
        && let Err(error) = stream.speak(text).await
    {
        sink.error(error).await;
        *tts = None;
    }
}

async fn flush(tts: &mut Option<TtsStream>, sink: &TurnSink) {
    if let Some(stream) = tts.as_mut()
        && let Err(error) = stream.flush().await
    {
        sink.error(error).await;
        *tts = None;
    }
}

/// Waits for flushed speech to finish, forwarding its audio.
async fn drain(tts: &mut Option<TtsStream>, sink: &TurnSink) {
    let deadline = tokio::time::Instant::now() + TTS_DRAIN_TIMEOUT;
    while tts.as_ref().is_some_and(TtsStream::is_busy) {
        let Ok(event) = tokio::time::timeout_at(deadline, tts_next(tts)).await else {
            sink.error(Error::Timeout).await;
            break;
        };
        forward_tts(event, tts, sink).await;
    }
}

// The model/tool loop of one turn reads best top to bottom.
#[allow(clippy::too_many_lines)]
async fn run_turn(
    ctx: &TurnContext,
    mut messages: Vec<Value>,
    input: TurnInput,
    language: &str,
    sink: &TurnSink,
    mut tool_results: mpsc::Receiver<ToolResult>,
) {
    if let TurnInput::Say(text) = input {
        let mut tts = open_tts(ctx, language, sink).await;
        speak(&mut tts, &text, sink).await;
        flush(&mut tts, sink).await;
        sink.send(TurnMsg::Commit(chat::assistant_message(&text, &[])))
            .await;
        sink.event(LiveEvent::OutputTranscript {
            text,
            is_final: true,
        })
        .await;
        drain(&mut tts, sink).await;
        sink.event(LiveEvent::TurnComplete { usage: None }).await;
        if let Some(stream) = tts {
            stream.close().await;
        }
        return;
    }

    let mut tts: Option<TtsStream> = None;
    let mut tts_opened = false;
    for _round in 0..MAX_TOOL_ROUNDS {
        let body = chat::request_body(&ctx.config, &messages);
        let started = if tts_opened {
            chat::start(&ctx.http, &ctx.chat_endpoint, &ctx.api_key, &body).await
        } else {
            // Open TTS while the model thinks, hiding the connect latency.
            let (started, opened) = tokio::join!(
                chat::start(&ctx.http, &ctx.chat_endpoint, &ctx.api_key, &body),
                open_tts(ctx, language, sink)
            );
            tts = opened;
            tts_opened = true;
            started
        };
        let mut stream = match started {
            Ok(stream) => stream,
            Err(error) => {
                sink.error(error).await;
                break;
            }
        };

        let mut acc = ChatAccumulator::default();
        let mut chunker = SentenceChunker::default();
        loop {
            tokio::select! {
                chunk = stream.next() => match chunk {
                    None => break,
                    Some(Err(error)) => {
                        sink.error(error).await;
                        break;
                    }
                    Some(Ok(chunk)) => {
                        if let Some(text) = acc.apply(&chunk) {
                            sink.event(LiveEvent::OutputTranscript { text: acc.text.clone(), is_final: false }).await;
                            for piece in chunker.push(&text) {
                                speak(&mut tts, &piece, sink).await;
                            }
                        }
                    }
                },
                event = tts_next(&mut tts) => forward_tts(event, &mut tts, sink).await,
            }
        }

        if let Some(rest) = chunker.finish() {
            speak(&mut tts, &rest, sink).await;
        }
        flush(&mut tts, sink).await;
        let calls = acc.tool_calls();
        let assistant = chat::assistant_message(&acc.text, &calls);
        messages.push(assistant.clone());
        sink.send(TurnMsg::Commit(assistant)).await;
        if !acc.text.is_empty() {
            sink.event(LiveEvent::OutputTranscript {
                text: acc.text.clone(),
                is_final: true,
            })
            .await;
        }

        if calls.is_empty() {
            drain(&mut tts, sink).await;
            sink.event(LiveEvent::TurnComplete { usage: acc.usage })
                .await;
            if let Some(stream) = tts {
                stream.close().await;
            }
            return;
        }

        let mut waiting: HashSet<String> = calls.iter().map(|c| c.call_id.clone()).collect();
        for call in calls {
            sink.event(LiveEvent::ToolCall(call)).await;
        }
        while !waiting.is_empty() {
            tokio::select! {
                result = tool_results.recv() => {
                    let Some(result) = result else { return };
                    if waiting.remove(&result.call_id) {
                        let message = chat::tool_message(&result);
                        messages.push(message.clone());
                        sink.send(TurnMsg::Commit(message)).await;
                    }
                }
                event = tts_next(&mut tts) => forward_tts(event, &mut tts, sink).await,
            }
        }
    }
    // Ran out of rounds or the model failed: finish what is being said.
    drain(&mut tts, sink).await;
    sink.event(LiveEvent::TurnComplete { usage: None }).await;
    if let Some(stream) = tts {
        stream.close().await;
    }
}

#[cfg(test)]
#[path = "cascade_tests.rs"]
mod tests;
