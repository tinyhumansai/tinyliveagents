//! Splits streamed reply text into speakable pieces.
//!
//! Sending every token to TTS gives choppy prosody; waiting for the whole
//! reply adds latency. The chunker releases text at sentence ends (`.`, `!`,
//! `?`, the Devanagari danda `।`, or a newline) once at least
//! [`MIN_CHUNK`] characters are buffered, and at a clause break when the
//! buffer grows past [`MAX_CHUNK`].

/// Minimum characters released at a sentence end.
pub(crate) const MIN_CHUNK: usize = 12;
/// Buffer size past which a clause break (`,` `;` `:`) or space releases text.
pub(crate) const MAX_CHUNK: usize = 160;

/// Buffers reply text and releases speakable chunks.
#[derive(Debug, Default)]
pub(crate) struct SentenceChunker {
    buffer: String,
}

impl SentenceChunker {
    /// Adds text and returns the chunks it completes.
    pub(crate) fn push(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(text);
        let mut out = Vec::new();
        while let Some(cut) = self.cut_point() {
            let chunk: String = self.buffer.drain(..cut).collect();
            let chunk = chunk.trim().to_string();
            if !chunk.is_empty() {
                out.push(chunk);
            }
        }
        out
    }

    /// Returns whatever is left.
    pub(crate) fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buffer);
        let rest = rest.trim();
        (!rest.is_empty()).then(|| rest.to_string())
    }

    fn cut_point(&self) -> Option<usize> {
        let mut chars = self.buffer.char_indices().peekable();
        let mut clause = None;
        while let Some((index, ch)) = chars.next() {
            let end = index + ch.len_utf8();
            let followed_by_space = chars.peek().is_some_and(|(_, next)| next.is_whitespace());
            if ch == '\n' && end >= MIN_CHUNK {
                return Some(end);
            }
            if matches!(ch, '.' | '!' | '?' | '।') && followed_by_space && end >= MIN_CHUNK {
                return Some(end);
            }
            if matches!(ch, ',' | ';' | ':') && followed_by_space {
                clause = Some(end);
            }
        }
        if self.buffer.len() > MAX_CHUNK {
            let mut limit = MAX_CHUNK;
            while !self.buffer.is_char_boundary(limit) {
                limit -= 1;
            }
            return clause
                .or_else(|| self.buffer[..limit].rfind(' ').map(|i| i + 1))
                .or(Some(limit));
        }
        None
    }
}

#[cfg(test)]
#[path = "chunker_tests.rs"]
mod tests;
