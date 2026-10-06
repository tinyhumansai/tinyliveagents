//! The crate-wide error type.
//!
//! Every fallible public function returns [`Result`]. Variants name *what went
//! wrong* in provider-neutral terms so a host can react the same way whichever
//! provider is behind a session: an [`Error::Unauthorized`] means "the
//! credential or ticket was refused" for Gemini, ElevenLabs and Sarvam alike.
//! Messages are lowercase and free of trailing punctuation, and never carry
//! credentials or user content.

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The configuration is unusable for the chosen provider (a missing model,
    /// an unsupported sample rate, an empty API key, ...).
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// The WebSocket or HTTP connection could not be established.
    #[error("connection failed: {0}")]
    Connect(String),
    /// The provider refused the credential, ticket, or signed URL.
    #[error("the provider refused the credential")]
    Unauthorized,
    /// The account behind the credential has no balance left for the session.
    #[error("insufficient credits for a live session")]
    InsufficientCredits,
    /// The provider rate limited or throttled the caller.
    #[error("rate limited by the provider")]
    RateLimited,
    /// The session idled out or reached its maximum duration.
    #[error("the live session timed out")]
    Timeout,
    /// The provider (or a relay in front of it) failed.
    #[error("provider error: {0}")]
    Provider(String),
    /// The provider sent a frame this crate could not decode.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The session has already closed; nothing more can be sent on it.
    #[error("the live session is closed")]
    Closed,
}

/// The crate's standard result type.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
