//! Token usage and cache accounting.
//!
//! Mirrors the provider usage block: prompt, completion, `cache_read`, `cache_write`.
//! `input` counts only uncached prompt tokens; cached tokens are reported separately
//! because they are billed at a different rate.
//!
//! The cost counter reads this struct after every turn; `cache_read` growth is the signal
//! that the request prefix stayed stable.

/// Token usage of a single request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Uncached prompt tokens.
    pub input: u64,
    /// Completion tokens, thinking tokens included.
    pub output: u64,
    /// Thinking tokens, already counted inside `output`.
    ///
    /// Kept separately for the status bar and for reasoning-cost reports; adding it to
    /// `total_tokens()` would bill the same tokens twice.
    pub reasoning: u64,
    /// Prompt tokens served from the provider cache.
    pub cache_read: u64,
    /// Prompt tokens written to the provider cache.
    pub cache_write: u64,
}

impl Usage {
    /// All prompt tokens, cached or not.
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// Prompt plus completion tokens.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens() + self.output
    }

    /// True when no tokens were used.
    pub fn is_empty(&self) -> bool {
        self.total_tokens() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_tokens_are_not_billed_twice() {
        let usage = Usage {
            output: 1_000,
            reasoning: 600,
            ..Usage::default()
        };

        assert_eq!(usage.total_tokens(), 1_000);
        assert!(!usage.is_empty());
    }

    #[test]
    fn prompt_tokens_include_both_cache_buckets() {
        let usage = Usage {
            input: 10,
            cache_read: 20,
            cache_write: 30,
            output: 5,
            reasoning: 4,
        };

        assert_eq!(usage.prompt_tokens(), 60);
        assert_eq!(usage.total_tokens(), 65);
    }
}
