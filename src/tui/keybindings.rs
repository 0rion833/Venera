//! Key mapping: terminal events to UI intents.
//!
//! Editing keys follow readline, because that is what people already have in their
//! fingers: `Ctrl+A`/`Ctrl+E` jump to the edges, `Ctrl+W` kills a word, `Ctrl+U` clears
//! the line, `Ctrl+Left`/`Ctrl+Right` jump by words. `↑`/`↓` walk the input history while
//! the transcript scrolls with `PgUp`/`PgDn`, `Ctrl+↑`/`Ctrl+↓`, and the wheel.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What a key press means to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Insert a character.
    Char(char),
    /// Insert a line break.
    Newline,
    /// Submit the input.
    Enter,
    /// Delete the character before the caret.
    Backspace,
    /// Delete the character under the caret.
    Delete,
    /// Move the caret one character left.
    Left,
    /// Move the caret one character right.
    Right,
    /// Move the caret to the previous word.
    WordLeft,
    /// Move the caret past the next word.
    WordRight,
    /// Move the caret to the start of the input.
    Home,
    /// Move the caret to the end of the input.
    End,
    /// Delete the word before the caret.
    KillWord,
    /// Empty the input.
    KillLine,
    /// Clear the input.
    Escape,
    /// Previous input-history line.
    Up,
    /// Next input-history line.
    Down,
    /// Scroll the transcript one line up.
    ScrollUp,
    /// Scroll the transcript one line down.
    ScrollDown,
    /// Scroll a page up.
    PageUp,
    /// Scroll a page down.
    PageDown,
    /// Complete the current slash command.
    Tab,
    /// Leave the UI.
    Quit,
    /// Clear the transcript.
    Clear,
}

/// Maps a terminal key event; `None` means the key is ignored.
pub fn map(event: KeyEvent) -> Option<Key> {
    // Windows consoles report both the key press and the key release. Only the press (and
    // auto-repeat while a key is held) may reach the UI, otherwise every character is
    // inserted twice: one `/` shows up as `//`.
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    let control = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    // AltGr arrives as `Ctrl+Alt` on Windows; the character it produces must still type.
    let alt_gr = control && alt;

    if control && !alt {
        match event.code {
            KeyCode::Char('c') => return Some(Key::Quit),
            KeyCode::Char('l') => return Some(Key::Clear),
            KeyCode::Char('a') => return Some(Key::Home),
            KeyCode::Char('e') => return Some(Key::End),
            KeyCode::Char('w') => return Some(Key::KillWord),
            KeyCode::Char('u') => return Some(Key::KillLine),
            KeyCode::Char('d') => return Some(Key::Delete),
            _ => {}
        }
    }

    match event.code {
        KeyCode::Char(ch) if !control || alt_gr => Some(Key::Char(ch)),
        KeyCode::Enter if event.modifiers.contains(KeyModifiers::SHIFT) || alt => {
            Some(Key::Newline)
        }
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Backspace if control => Some(Key::KillWord),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Delete => Some(Key::Delete),
        KeyCode::Left if control || alt => Some(Key::WordLeft),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right if control || alt => Some(Key::WordRight),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Home => Some(Key::Home),
        KeyCode::End => Some(Key::End),
        KeyCode::Up if control => Some(Key::ScrollUp),
        KeyCode::Down if control => Some(Key::ScrollDown),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::Esc => Some(Key::Escape),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn control_combinations_win_over_characters() {
        assert_eq!(
            map(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::Quit)
        );
        assert_eq!(
            map(key(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Some(Key::Clear)
        );
        assert_eq!(
            map(key(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(Key::KillWord)
        );
        assert_eq!(
            map(key(KeyCode::Backspace, KeyModifiers::CONTROL)),
            Some(Key::KillWord)
        );
    }

    #[test]
    fn readline_editing_keys_are_mapped() {
        assert_eq!(
            map(key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(Key::Home)
        );
        assert_eq!(
            map(key(KeyCode::Char('e'), KeyModifiers::CONTROL)),
            Some(Key::End)
        );
        assert_eq!(
            map(key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Some(Key::KillLine)
        );
        assert_eq!(
            map(key(KeyCode::Left, KeyModifiers::CONTROL)),
            Some(Key::WordLeft)
        );
        assert_eq!(
            map(key(KeyCode::Right, KeyModifiers::CONTROL)),
            Some(Key::WordRight)
        );
        assert_eq!(map(key(KeyCode::End, KeyModifiers::NONE)), Some(Key::End));
        assert_eq!(
            map(key(KeyCode::Delete, KeyModifiers::NONE)),
            Some(Key::Delete)
        );
    }

    #[test]
    fn a_shifted_enter_breaks_the_line() {
        assert_eq!(
            map(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(Key::Newline)
        );
        assert_eq!(
            map(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Key::Enter)
        );
    }

    #[test]
    fn arrows_browse_history_and_control_arrows_scroll() {
        assert_eq!(map(key(KeyCode::Up, KeyModifiers::NONE)), Some(Key::Up));
        assert_eq!(map(key(KeyCode::Down, KeyModifiers::NONE)), Some(Key::Down));
        assert_eq!(
            map(key(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(Key::ScrollUp)
        );
        assert_eq!(
            map(key(KeyCode::Down, KeyModifiers::CONTROL)),
            Some(Key::ScrollDown)
        );
    }

    #[test]
    fn alt_gr_characters_are_still_typed() {
        let alt_gr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(map(key(KeyCode::Char('@'), alt_gr)), Some(Key::Char('@')));
    }

    #[test]
    fn typed_characters_are_inserted() {
        assert_eq!(
            map(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(Key::Char('x'))
        );
        assert_eq!(
            map(key(KeyCode::Char('/'), KeyModifiers::SHIFT)),
            Some(Key::Char('/'))
        );
    }

    #[test]
    fn key_release_events_are_dropped() {
        let event = KeyEvent::new_with_kind(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(map(event), None, "a release must not type a second slash");

        let event =
            KeyEvent::new_with_kind(KeyCode::Char('/'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            map(event),
            Some(Key::Char('/')),
            "auto-repeat still inserts text"
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        assert_eq!(map(key(KeyCode::F(5), KeyModifiers::NONE)), None);
    }

    #[test]
    fn tab_completes_the_command() {
        assert_eq!(map(key(KeyCode::Tab, KeyModifiers::NONE)), Some(Key::Tab));
    }
}
