//! Token counting.
//!
//! Counts are produced locally with `tiktoken-rs`, so they work without an API key and
//! without network access. The numbers match what OpenAI-compatible providers bill for
//! the same text plus the documented chat framing overhead.
//!
//! Framing follows the OpenAI chat format: 3 tokens per message plus 3 tokens once per
//! request to prime the reply. Providers may add a little more; the estimate is close
//! enough to plan a budget and is clearly labelled as an estimate.

use crate::config::Encoding;

/// Tokens the chat wire format adds per message.
pub const MESSAGE_OVERHEAD: usize = 3;

/// Tokens added once per request to prime the reply.
pub const REPLY_PRIMING: usize = 3;

/// Token count split into text and framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCount {
    /// Tokens of the message text itself.
    pub text: usize,
    /// Tokens of the chat framing.
    pub framing: usize,
}

impl TokenCount {
    /// Text plus framing.
    pub fn total(&self) -> usize {
        self.text + self.framing
    }
}

/// Counts the tokens of plain text in the given encoding.
pub fn count(encoding: Encoding, text: &str) -> usize {
    match encoding {
        Encoding::O200kBase => tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len(),
        Encoding::Cl100kBase => tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(text)
            .len(),
    }
}

/// Counts a full chat request made of plain-text messages.
pub fn count_messages(encoding: Encoding, messages: &[&str]) -> TokenCount {
    let text = messages
        .iter()
        .map(|message| count(encoding, message))
        .sum();
    let framing = messages.len() * MESSAGE_OVERHEAD + REPLY_PRIMING;
    TokenCount { text, framing }
}
