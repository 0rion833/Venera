//! System prompt assembly.
//!
//! DANGER ZONE. The request prefix is byte-for-byte immutable and its order is fixed:
//! tool schemas -> system prompt -> AGENTS.md digest (`AGENTS.md` §2). No timestamps,
//! no request ids, no per-turn text in the prefix.
//!
//! [`SYSTEM_PROMPT`] and [`AGENTS_DIGEST`] are compiled-in constants: changing either
//! invalidates the provider cache for every session. After any change here run
//! `cargo test cache_prefix_stable` and confirm `cache_read` grows, then
//! update the digest in `AGENTS.md` §10 to match.
//!
//! `--bare` skips both blocks, leaving only the user message and its framing.

use crate::config::Encoding;
use crate::provider::tokenizer::{self, MESSAGE_OVERHEAD, REPLY_PRIMING};

/// The immutable system prompt. Keep it short: every token is paid on every turn.
pub const SYSTEM_PROMPT: &str = "\
You are Venera, a coding agent working inside the user's project from a terminal.

Operating rules:
- Read a file before editing it; project-map summaries can be stale.
- To add text, use `append_file` (auto newline); to change a fragment, use `edit_file`. Never rewrite a file just to add a line.
- Never remove existing text unless the user asked to remove it.
- Run the project's checks after changes and report failures honestly.
- Never print API keys, tokens, or environment dumps.
- File writes and shell commands need confirmation unless the config allows them.
- Keep answers short and concrete, with paths like `src/main.rs`.

Reply discipline, because every reply token costs five input tokens:
- Call the tool first: no preamble, no plan in prose, no restating the task.
- Never repeat unchanged code; emit only the lines that differ.
- Answer code questions with `path:line`, not a paragraph.";

/// The AGENTS.md digest, verbatim from `AGENTS.md` §10.
pub const AGENTS_DIGEST: &str = "Venera is a Rust CLI agent. Rules: M1 is a hint — read the original before editing; request prefix is immutable; truncate tool output; no secrets in context; bash and write require confirmation; edit, don't overwrite; run check after edits.";

/// Separator between the system prompt and the digest inside one system message.
pub const SYSTEM_SEPARATOR: &str = "\n\n";

/// The messages of a single turn, before the provider adds tool schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptParts<'a> {
    /// System prompt, absent with `--bare`.
    pub system: Option<&'static str>,
    /// AGENTS.md digest, absent with `--bare`.
    pub digest: Option<&'static str>,
    /// The user's text for this turn.
    pub user: &'a str,
}

impl<'a> PromptParts<'a> {
    /// Builds the parts for `user_text`; `include_context` is false for `--bare`.
    pub fn new(user_text: &'a str, include_context: bool) -> Self {
        Self {
            system: include_context.then_some(SYSTEM_PROMPT),
            digest: include_context.then_some(AGENTS_DIGEST),
            user: user_text,
        }
    }

    /// The system message: prompt and digest joined, or `None` with `--bare`.
    pub fn system_message(&self) -> Option<String> {
        match (self.system, self.digest) {
            (Some(system), Some(digest)) => Some(format!("{system}{SYSTEM_SEPARATOR}{digest}")),
            (Some(system), None) => Some(system.to_owned()),
            (None, Some(digest)) => Some(digest.to_owned()),
            (None, None) => None,
        }
    }

    /// Final message list, in the order the request must send it.
    pub fn messages(&self) -> Vec<String> {
        let mut messages = Vec::with_capacity(2);
        if let Some(system) = self.system_message() {
            messages.push(system);
        }
        messages.push(self.user.to_owned());
        messages
    }
}

/// Token count of a turn split into its parts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptBreakdown {
    /// Tokens of [`SYSTEM_PROMPT`].
    pub system: usize,
    /// Tokens of [`AGENTS_DIGEST`].
    pub digest: usize,
    /// Tokens of the user's text.
    pub user: usize,
    /// Chat framing: 3 tokens per message plus 3 to prime the reply.
    pub framing: usize,
}

impl PromptBreakdown {
    /// Sum of every part.
    pub fn total(&self) -> usize {
        self.system + self.digest + self.user + self.framing
    }

    /// Tokens that are not the user's own text (system, digest, framing).
    pub fn context(&self) -> usize {
        self.system + self.digest + self.framing
    }
}

/// Counts the parts of a turn made of `user_text`.
pub fn count(encoding: Encoding, user_text: &str, include_context: bool) -> PromptBreakdown {
    count_parts(encoding, &PromptParts::new(user_text, include_context))
}

/// Counts the parts of an already assembled prompt.
pub fn count_parts(encoding: Encoding, parts: &PromptParts<'_>) -> PromptBreakdown {
    PromptBreakdown {
        system: parts
            .system
            .map_or(0, |text| tokenizer::count(encoding, text)),
        digest: parts
            .digest
            .map_or(0, |text| tokenizer::count(encoding, text)),
        user: tokenizer::count(encoding, parts.user),
        framing: parts.messages().len() * MESSAGE_OVERHEAD + REPLY_PRIMING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_the_agents_md_text() {
        assert!(AGENTS_DIGEST.starts_with("Venera is a Rust CLI agent."));
        assert!(AGENTS_DIGEST.contains("M1 is a hint"));
        assert!(AGENTS_DIGEST.contains("edit, don't overwrite"));
        assert!(!AGENTS_DIGEST.contains('\n'));
    }

    #[test]
    fn system_prompt_is_stable_and_mentions_the_rules() {
        assert!(SYSTEM_PROMPT.contains("Venera"));
        assert!(SYSTEM_PROMPT.contains("Read a file before editing"));
        assert!(SYSTEM_PROMPT.contains("append_file"));
        assert!(SYSTEM_PROMPT.contains("Never remove existing text"));
        assert!(SYSTEM_PROMPT.contains("Never print API keys"));
        assert!(!SYSTEM_PROMPT.contains("202"));
    }

    #[test]
    fn system_prompt_keeps_replies_small() {
        assert!(SYSTEM_PROMPT.contains("no preamble"));
        assert!(SYSTEM_PROMPT.contains("Never repeat unchanged code"));
        assert!(SYSTEM_PROMPT.contains("path:line"));
    }

    #[test]
    fn bare_prompt_has_one_message_and_six_framing_tokens() {
        let parts = PromptParts::new("привет", false);
        assert_eq!(parts.messages().len(), 1);
        assert_eq!(parts.system_message(), None);

        let breakdown = count(Encoding::O200kBase, "привет", false);
        assert_eq!(breakdown.system, 0);
        assert_eq!(breakdown.digest, 0);
        assert_eq!(breakdown.framing, 6);
        assert_eq!(breakdown.total(), breakdown.user + 6);
    }

    #[test]
    fn full_prompt_has_two_messages_and_nine_framing_tokens() {
        let parts = PromptParts::new("привет", true);
        let messages = parts.messages();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains(SYSTEM_PROMPT));
        assert!(messages[0].contains(AGENTS_DIGEST));
        assert_eq!(messages[1], "привет");

        let breakdown = count(Encoding::O200kBase, "привет", true);
        assert_eq!(breakdown.framing, 9);
        assert!(breakdown.system > 0);
        assert!(breakdown.digest > 0);
        assert_eq!(
            breakdown.total(),
            breakdown.system + breakdown.digest + breakdown.user + 9
        );
    }

    #[test]
    fn full_prompt_costs_more_than_bare() {
        let bare = count(Encoding::O200kBase, "привет", false);
        let full = count(Encoding::O200kBase, "привет", true);
        assert!(full.total() > bare.total());
        assert_eq!(full.context(), full.total() - full.user);
    }

    /// The prefix must be byte-for-byte identical between turns, or the provider
    /// cache is lost and every request becomes up to 10x more expensive.
    #[test]
    fn cache_prefix_stable() {
        let first = PromptParts::new("hi", true).messages();
        let second = PromptParts::new("hi", true).messages();

        assert_eq!(first, second);
        assert!(first[0].starts_with(SYSTEM_PROMPT));
        assert!(first[0].ends_with(AGENTS_DIGEST));
        assert_eq!(first[1], "hi");
    }
}
