//! Prompt editor: text, caret, and the history of submitted lines.
//!
//! The caret is measured in characters, not bytes: it must land between two Cyrillic
//! letters exactly where it lands between two ASCII ones. The text stays a `String`, so a
//! character index is resolved to a byte offset before every edit.
//!
//! History is a plain list of submitted lines, newest last, browsed with `↑`/`↓`. While
//! the user browses, the half-typed line is kept aside as a draft and restored when the
//! browse walks past the newest entry, so no keystroke is ever lost.

/// Submitted lines kept for `↑`/`↓`; the oldest are dropped first.
const HISTORY_LIMIT: usize = 200;

/// Editable prompt contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    /// Everything typed, `\n` included.
    text: String,
    /// Caret position in characters, from the start of `text`.
    cursor: usize,
    /// Submitted lines, oldest first.
    history: Vec<String>,
    /// Index into `history` while the user browses it.
    browse: Option<usize>,
    /// The line that was being typed when browsing started.
    draft: String,
}

impl Input {
    /// The text as typed, line breaks included.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// True when nothing has been typed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Caret position, in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Number of characters in the text.
    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Submitted lines, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Replaces the history, e.g. with the one restored from a session.
    pub fn set_history(&mut self, history: Vec<String>) {
        let skip = history.len().saturating_sub(HISTORY_LIMIT);
        self.history = history.into_iter().skip(skip).collect();
        self.browse = None;
        self.draft.clear();
    }

    /// Inserts one character at the caret.
    pub fn insert(&mut self, ch: char) {
        let at = self.byte_index(self.cursor);
        self.text.insert(at, ch);
        self.cursor += 1;
        self.forget_browse();
    }

    /// Removes the character before the caret.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }

        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        self.forget_browse();
    }

    /// Removes the character under the caret.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }

        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
        self.forget_browse();
    }

    /// Moves the caret one character left.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Moves the caret one character right.
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    /// Moves the caret to the start of the text.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Moves the caret to the end of the text.
    pub fn end(&mut self) {
        self.cursor = self.char_count();
    }

    /// Jumps to the start of the previous word.
    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    /// Jumps past the end of the next word.
    pub fn word_right(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.cursor;

        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        while index < chars.len() && !chars[index].is_whitespace() {
            index += 1;
        }

        self.cursor = index;
    }

    /// Deletes the word before the caret (`Ctrl+W`, `Ctrl+Backspace`).
    pub fn kill_word(&mut self) {
        let start = self.word_start();
        if start == self.cursor {
            return;
        }

        let range = self.byte_index(start)..self.byte_index(self.cursor);
        self.text.replace_range(range, "");
        self.cursor = start;
        self.forget_browse();
    }

    /// Empties the editor without touching the history.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.forget_browse();
    }

    /// Replaces the text and puts the caret at its end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.char_count();
        self.forget_browse();
    }

    /// Hands the text to the caller and empties the editor.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.forget_browse();
        text
    }

    /// Records a submitted line; a repeated line is not stored twice in a row.
    pub fn remember(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        if self.history.last().map(String::as_str) != Some(line) {
            self.history.push(line.to_owned());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }

        self.forget_browse();
    }

    /// Replaces the editor with the previous submitted line; `false` when there is none.
    pub fn previous(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }

        let index = match self.browse {
            None => {
                self.draft.clone_from(&self.text);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };

        self.browse = Some(index);
        self.show_history(index);
        true
    }

    /// Replaces the editor with the next newer submitted line, then with the draft.
    pub fn newer(&mut self) -> bool {
        let Some(index) = self.browse else {
            return false;
        };

        if index + 1 < self.history.len() {
            self.browse = Some(index + 1);
            self.show_history(index + 1);
            return true;
        }

        self.browse = None;
        let draft = std::mem::take(&mut self.draft);
        self.text = draft;
        self.cursor = self.char_count();
        true
    }

    /// Puts one history entry into the editor with the caret at its end.
    fn show_history(&mut self, index: usize) {
        self.text.clone_from(&self.history[index]);
        self.cursor = self.char_count();
    }

    /// Leaves browse mode: the next edit starts a fresh line.
    fn forget_browse(&mut self) {
        self.browse = None;
        self.draft.clear();
    }

    /// Character index → byte offset; the end of the text for a past-the-end index.
    fn byte_index(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map(|(byte, _)| byte)
            .unwrap_or(self.text.len())
    }

    /// Start of the word before the caret.
    fn word_start(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.cursor;

        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !chars[index - 1].is_whitespace() {
            index -= 1;
        }

        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Input {
        let mut input = Input::default();
        for ch in text.chars() {
            input.insert(ch);
        }
        input
    }

    #[test]
    fn editing_happens_where_the_caret_is() {
        let mut input = typed("/help");
        input.left();
        input.left();
        assert_eq!(input.cursor(), 3, "the caret sits between `e` and `l`");

        input.insert('x');
        assert_eq!(input.text(), "/hexlp");
        assert_eq!(input.cursor(), 4);

        input.backspace();
        assert_eq!(input.text(), "/help");
        assert_eq!(input.cursor(), 3);

        input.delete();
        assert_eq!(input.text(), "/hep");
    }

    #[test]
    fn the_caret_stops_at_both_edges() {
        let mut input = typed("ab");
        for _ in 0..5 {
            input.left();
        }
        assert_eq!(input.cursor(), 0);

        for _ in 0..5 {
            input.right();
        }
        assert_eq!(input.cursor(), 2);

        input.home();
        assert_eq!(input.cursor(), 0);
        input.end();
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn cyrillic_counts_as_one_character_per_cell() {
        let mut input = typed("привет");
        input.left();
        input.backspace();

        assert_eq!(input.text(), "привт");
        assert_eq!(input.cursor(), 4);
    }

    #[test]
    fn words_are_jumped_and_killed_by_whitespace() {
        let mut input = typed("find greps in target");

        input.kill_word();
        assert_eq!(input.text(), "find greps in ");
        assert_eq!(input.cursor(), 14);

        input.kill_word();
        assert_eq!(input.text(), "find greps ");

        input.word_left();
        assert_eq!(input.cursor(), 5, "the start of `greps`");
        input.word_right();
        assert_eq!(input.cursor(), 10, "just past `greps`");
    }

    #[test]
    fn history_survives_a_draft_and_restores_it() {
        let mut input = Input::default();
        input.remember("/index");
        input.remember("/map --delta");

        assert!(input.previous(), "the newest line comes first");
        assert_eq!(input.text(), "/map --delta");

        assert!(input.previous());
        assert_eq!(input.text(), "/index");
        assert!(
            input.previous(),
            "the oldest line is a stop, not a fallback to scrolling"
        );
        assert_eq!(input.text(), "/index");

        assert!(input.newer());
        assert_eq!(input.text(), "/map --delta");
        assert!(input.newer());
        assert_eq!(input.text(), "", "past the newest line the draft returns");
        assert!(!input.newer(), "there is nothing left to browse");

        input.insert('x');
        assert!(!input.newer(), "typing leaves browse mode");
    }

    #[test]
    fn the_draft_is_kept_while_browsing() {
        let mut input = typed("черновик");
        input.remember("/help");

        assert!(input.previous());
        assert_eq!(input.text(), "/help");
        assert!(input.newer());
        assert_eq!(input.text(), "черновик");
    }

    #[test]
    fn a_repeated_line_is_stored_once() {
        let mut input = Input::default();
        input.remember("/help");
        input.remember("/help");
        input.remember("  ");

        assert_eq!(input.history(), &["/help".to_owned()]);
    }

    #[test]
    fn taking_the_text_empties_the_editor_but_not_the_history() {
        let mut input = typed("hello");
        input.remember("hello");

        assert_eq!(input.take(), "hello");
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
        assert_eq!(input.history().len(), 1);
    }
}
