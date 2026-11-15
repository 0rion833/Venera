//! Output truncation shared by all tools: head + tail + "N lines hidden" marker.
//!
//! Tool output is truncated on the client, before it enters history, so one runaway
//! result cannot consume the turn's budget (`AGENTS.md` §2). The marker is always
//! explicit: the model must be able to tell a complete file from a slice.
//!
//! Current budgets (`AGENTS.md` §6, `TOOL.md` §9): 400 lines per call; search results are
//! capped separately by the tool that produces them.

/// Lines a single tool result may occupy.
pub const MAX_TOOL_LINES: usize = 400;

/// Share of the kept lines given to the head.
const HEAD_SHARE: usize = 2;

/// A truncated body plus the accounting needed to say what was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncated {
    /// The text to hand to the model.
    pub text: String,
    /// Lines removed from the middle.
    pub hidden_lines: usize,
    /// Whether anything was removed at all.
    pub truncated: bool,
}

impl Truncated {
    /// An untouched body.
    pub fn whole(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            hidden_lines: 0,
            truncated: false,
        }
    }
}

/// Keeps the head and the tail, replacing the middle with an explicit marker.
pub fn head_tail(text: &str, max_lines: usize) -> Truncated {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines || max_lines == 0 {
        return Truncated::whole(text);
    }

    let head = (max_lines * HEAD_SHARE / (HEAD_SHARE + 1)).max(1);
    let tail = max_lines.saturating_sub(head);
    let hidden = lines.len().saturating_sub(head + tail);

    let mut output = String::with_capacity(text.len());
    for line in &lines[..head] {
        output.push_str(line);
        output.push('\n');
    }
    output.push_str(&format!("… {hidden} lines hidden …\n"));
    for line in &lines[lines.len() - tail..] {
        output.push_str(line);
        output.push('\n');
    }

    Truncated {
        text: output,
        hidden_lines: hidden,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(count: usize) -> String {
        (1..=count)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn short_output_is_untouched() {
        let result = head_tail("one\ntwo", 10);

        assert_eq!(result, Truncated::whole("one\ntwo"));
        assert!(!result.truncated);
    }

    #[test]
    fn long_output_keeps_head_and_tail_with_a_marker() {
        let result = head_tail(&numbered(1_000), 10);

        assert!(result.truncated);
        assert_eq!(result.hidden_lines, 990);
        assert!(result.text.starts_with("line 1\n"));
        assert!(result.text.ends_with("line 1000\n"));
        assert!(result.text.contains("… 990 lines hidden …"));
        assert_eq!(result.text.lines().count(), 11, "6 head + marker + 4 tail");
    }

    #[test]
    fn the_budget_counts_inclusive_of_the_marker() {
        let result = head_tail(&numbered(100), 5);

        assert!(result.text.lines().count() <= 7, "got: {}", result.text);
        assert!(result.text.contains("lines hidden"));
    }

    #[test]
    fn a_zero_budget_is_treated_as_no_truncation() {
        let text = numbered(50);
        assert_eq!(head_tail(&text, 0), Truncated::whole(text));
    }
}
