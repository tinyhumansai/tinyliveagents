//! A running live session: a command sender and an event stream.
//!
//! Providers return a [`LiveSession`] from `connect`. The host keeps the
//! [`LiveSender`] (cloneable, so the microphone pump and the tool runner can
//! each hold one) and drains [`LiveEvents`]. Dropping the event stream stops
//! the provider task and closes its sockets, so a host that loses interest
//! cannot leak a connection.
//!
//! [`LiveSession::from_channels`] builds a session from raw channels. Providers
//! use it internally, and test doubles in downstream crates use it to script a
//! fake provider without opening a socket.

use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::types::{ClientCommand, LiveEvent, ToolResult};

/// Buffered commands and events per session. Audio chunks of ~100 ms mean this
/// holds several seconds either way before back-pressure applies.
pub const CHANNEL_CAPACITY: usize = 256;

/// The sending half of a session. Cheap to clone.
#[derive(Debug, Clone)]
pub struct LiveSender {
    tx: mpsc::Sender<ClientCommand>,
}

impl LiveSender {
    /// Sends any command.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] once the session has ended.
    pub async fn send(&self, command: ClientCommand) -> Result<()> {
        self.tx.send(command).await.map_err(|_| Error::Closed)
    }

    /// Sends a chunk of PCM16 audio in the session's input format.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] once the session has ended.
    pub async fn send_audio(&self, pcm16: impl Into<Bytes>) -> Result<()> {
        self.send(ClientCommand::Audio(pcm16.into())).await
    }

    /// Sends a typed user message.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] once the session has ended.
    pub async fn send_text(&self, text: impl Into<String>) -> Result<()> {
        self.send(ClientCommand::Text(text.into())).await
    }

    /// Answers a tool call.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] once the session has ended.
    pub async fn send_tool_result(&self, result: ToolResult) -> Result<()> {
        self.send(ClientCommand::ToolResult(result)).await
    }

    /// Interrupts the agent's current reply.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] once the session has ended.
    pub async fn interrupt(&self) -> Result<()> {
        self.send(ClientCommand::Interrupt).await
    }

    /// Asks the session to close. The event stream ends with
    /// [`LiveEvent::Closed`].
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] once the session has ended.
    pub async fn close(&self) -> Result<()> {
        self.send(ClientCommand::Close).await
    }

    /// Whether the session has stopped accepting commands.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Aborts the provider task when the event stream is dropped.
#[derive(Debug)]
struct TaskGuard(Option<JoinHandle<()>>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// The receiving half of a session.
#[derive(Debug)]
pub struct LiveEvents {
    rx: mpsc::Receiver<LiveEvent>,
    task: TaskGuard,
}

impl LiveEvents {
    /// The next event, or `None` after [`LiveEvent::Closed`] has been
    /// delivered and the provider task has ended.
    pub async fn recv(&mut self) -> Option<LiveEvent> {
        self.rx.recv().await
    }

    /// The next event, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when nothing arrives in time.
    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<LiveEvent>> {
        tokio::time::timeout(timeout, self.rx.recv())
            .await
            .map_err(|_| Error::Timeout)
    }
}

/// A connected live session.
#[derive(Debug)]
pub struct LiveSession {
    sender: LiveSender,
    events: LiveEvents,
}

impl LiveSession {
    /// Builds a session from raw channels. The provider (or test double) reads
    /// commands from the receiver paired with `commands` and writes events to
    /// the sender paired with `events`.
    #[must_use]
    pub fn from_channels(
        commands: mpsc::Sender<ClientCommand>,
        events: mpsc::Receiver<LiveEvent>,
    ) -> Self {
        Self {
            sender: LiveSender { tx: commands },
            events: LiveEvents {
                rx: events,
                task: TaskGuard(None),
            },
        }
    }

    /// Ties a background task to the session; it is aborted when the event
    /// stream is dropped.
    #[must_use]
    pub fn with_task(mut self, task: JoinHandle<()>) -> Self {
        self.events.task = TaskGuard(Some(task));
        self
    }

    /// A sender for this session.
    #[must_use]
    pub fn sender(&self) -> LiveSender {
        self.sender.clone()
    }

    /// The next event (see [`LiveEvents::recv`]).
    pub async fn recv(&mut self) -> Option<LiveEvent> {
        self.events.recv().await
    }

    /// Splits the session so commands and events can be handled by different
    /// tasks.
    #[must_use]
    pub fn split(self) -> (LiveSender, LiveEvents) {
        (self.sender, self.events)
    }
}

/// The provider-side ends of a fresh session's channels.
#[derive(Debug)]
pub(crate) struct SessionChannels {
    pub(crate) commands: mpsc::Receiver<ClientCommand>,
    pub(crate) events: EventSink,
}

/// Creates a session and the provider-side channel ends that drive it.
pub(crate) fn session_pair() -> (LiveSession, SessionChannels) {
    let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (ev_tx, ev_rx) = mpsc::channel(CHANNEL_CAPACITY);
    (
        LiveSession::from_channels(cmd_tx, ev_rx),
        SessionChannels {
            commands: cmd_rx,
            events: EventSink { tx: ev_tx },
        },
    )
}

/// Where a provider task writes events.
#[derive(Debug, Clone)]
pub(crate) struct EventSink {
    tx: mpsc::Sender<LiveEvent>,
}

impl EventSink {
    /// Delivers an event. Returns `false` when the host dropped the stream, in
    /// which case the provider task should stop.
    pub(crate) async fn emit(&self, event: LiveEvent) -> bool {
        self.tx.send(event).await.is_ok()
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
