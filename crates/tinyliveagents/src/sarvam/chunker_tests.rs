//! Tests for the TTS sentence chunker.

use super::*;

#[test]
fn releases_whole_sentences() {
    let mut chunker = SentenceChunker::default();
    assert!(
        chunker.push("It is fourteen").is_empty(),
        "expected nothing"
    );
    assert_eq!(
        chunker.push(" oh five. And the"),
        vec!["It is fourteen oh five."]
    );
    assert_eq!(chunker.finish(), Some("And the".into()));
    assert_eq!(chunker.finish(), None);
}

#[test]
fn keeps_short_sentences_together_and_splits_on_newlines_and_dandas() {
    let mut chunker = SentenceChunker::default();
    // "Hi. " is shorter than MIN_CHUNK, so it waits for more text.
    assert!(chunker.push("Hi. ").is_empty(), "expected nothing");
    assert_eq!(
        chunker.push("How are you today?\nGood"),
        vec!["Hi. How are you today?"]
    );
    assert_eq!(chunker.finish(), Some("Good".into()));

    let mut hindi = SentenceChunker::default();
    assert_eq!(
        hindi.push("अभी दोपहर के दो बजे हैं। ठीक"),
        vec!["अभी दोपहर के दो बजे हैं।"]
    );
    let mut lines = SentenceChunker::default();
    assert_eq!(
        lines.push("first line here\nsecond"),
        vec!["first line here"]
    );
}

#[test]
fn breaks_long_runs_at_clauses_or_spaces() {
    let mut chunker = SentenceChunker::default();
    let long = format!("{}, and then more words", "word ".repeat(30));
    let out = chunker.push(&long);
    assert_eq!(out.len(), 1);
    assert!(out[0].ends_with(','));

    let mut spaces = SentenceChunker::default();
    let out = spaces.push(&"word ".repeat(40));
    assert!(!out.is_empty(), "expected something");
    assert!(out.iter().all(|chunk| chunk.len() <= MAX_CHUNK));

    let mut unbroken = SentenceChunker::default();
    let out = unbroken.push(&"ह".repeat(100));
    assert!(!out.is_empty(), "expected something");
    assert!(out[0].len() <= MAX_CHUNK);
}

#[test]
fn whitespace_only_input_yields_nothing() {
    let mut chunker = SentenceChunker::default();
    assert!(chunker.push("   ").is_empty(), "expected nothing");
    assert_eq!(chunker.finish(), None);
}
