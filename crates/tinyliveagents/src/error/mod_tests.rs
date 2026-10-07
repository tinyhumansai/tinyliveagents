//! Unit tests for the crate-wide error type.

use super::*;

#[test]
fn messages_are_lowercase_and_unpunctuated() {
    let all = [
        Error::InvalidConfig("x".into()),
        Error::Connect("x".into()),
        Error::Unauthorized,
        Error::InsufficientCredits,
        Error::RateLimited,
        Error::Timeout,
        Error::Provider("x".into()),
        Error::Protocol("x".into()),
        Error::Closed,
    ];
    for error in all {
        let message = error.to_string();
        let first = message.chars().next().unwrap_or_default();
        assert!(!first.is_uppercase(), "{message}");
        assert!(!message.ends_with('.'), "{message}");
    }
}

#[test]
fn is_a_standard_error() {
    fn assert_error<E: std::error::Error>(_: &E) {}
    assert_error(&Error::Closed);
}
