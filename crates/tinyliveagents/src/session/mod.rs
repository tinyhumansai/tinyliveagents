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
    /// An event read ahead of the host (the `Ready` a provider waited for
    /// before returning from `connect`), delivered before anything else.
    pending: Option<LiveEvent>,
    task: TaskGuard,
}

impl LiveEvents {
    /// The next event, or `None` after [`LiveEvent::Closed`] has been
    /// delivered and the provider task has ended.
    pub async fn recv(&mut self) -> Option<LiveEvent> {
        if let Some(event) = self.pending.take() {
            return Some(event);
        }
        self.rx.recv().await
    }

    /// The next event, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when nothing arrives in time.
    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<LiveEvent>> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }
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
                pending: None,
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

/// How long a provider may take to accept a session's setup.
#[cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Waits for `session`'s first event and returns the session only once it is
/// [`LiveEvent::Ready`], which stays queued for the host. Anything else before
/// `Ready` means the provider refused the setup.
///
/// # Errors
///
/// The error a pre-ready `Error` or `Closed` event carries,
/// [`Error::Provider`] for a normal close before `Ready`, and
/// [`Error::Timeout`] when nothing arrives within `timeout`.
#[cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
pub(crate) async fn await_ready(
    mut session: LiveSession,
    timeout: Duration,
) -> Result<LiveSession> {
    use crate::types::CloseReason;
    match tokio::time::timeout(timeout, session.events.rx.recv()).await {
        Err(_) => Err(Error::Timeout),
        Ok(None) => Err(Error::Closed),
        Ok(Some(event @ LiveEvent::Ready(_))) => {
            session.events.pending = Some(event);
            Ok(session)
        }
        Ok(Some(LiveEvent::Error { error, .. } | LiveEvent::Closed(CloseReason::Error(error)))) => {
            Err(error)
        }
        Ok(Some(LiveEvent::Closed(CloseReason::Remote { code, reason }))) => Err(Error::Provider(
            format!("closed before the session was ready (code {code:?}): {reason}"),
        )),
        Ok(Some(_)) => Err(Error::Protocol("the first event was not ready".into())),
    }
}

/// The provider-side ends of a fresh session's channels.
#[cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
#[derive(Debug)]
pub(crate) struct SessionChannels {
    pub(crate) commands: mpsc::Receiver<ClientCommand>,
    pub(crate) events: EventSink,
}

/// Creates a session and the provider-side channel ends that drive it.
#[cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
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
#[cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
#[derive(Debug, Clone)]
pub(crate) struct EventSink {
    tx: mpsc::Sender<LiveEvent>,
}

#[cfg(any(feature = "gemini", feature = "elevenlabs", feature = "sarvam"))]
impl EventSink {
    /// Delivers an event. Returns `false` when the host dropped the stream, in
    /// which case the provider task should stop.
    pub(crate) async fn emit(&self, event: LiveEvent) -> bool {
        self.tx.send(event).await.is_ok()
    }
}

#[cfg(all(
    test,
    any(feature = "gemini", feature = "elevenlabs", feature = "sarvam")
))]
#[path = "mod_tests.rs"]
mod tests;
