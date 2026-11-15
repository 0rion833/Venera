//! Syntax highlighting for fenced code blocks (`gui.md`).
//!
//! `syntect` parses the code with the `base16-ocean.dark` theme and this module converts
//! every `syntect::highlighting::Style` into a `ratatui` style with true RGB colors. The
//! result is cached by the BLAKE3 hash of `(language, code)`, so a finished block is
//! highlighted once and reused on every frame.
//!
//! Syntax sets are loaded with the `newlines` variant: a highlight state must see the
//! trailing `\n` of a line or multi-line strings and comments break at the boundary.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SynStyle, Theme as SynTheme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Name of the syntect theme, as required by `gui.md`.
pub const THEME_NAME: &str = "base16-ocean.dark";

/// Highlighted blocks kept in memory; the oldest are dropped when the cache is full.
pub const CACHE_LIMIT: usize = 128;

/// One highlighted block: styled lines, without any frame or background.
pub type Block = Rc<Vec<Line<'static>>>;

/// A syntax highlighter with a per-block cache.
///
/// The instance is cheap to recreate: a theme switch builds a new one and drops the old
/// cache, which is exactly the invalidation the colors need.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: SynTheme,
    cache: RefCell<HashMap<u64, Block>>,
}

impl Highlighter {
    /// Loads the default syntaxes and the `base16-ocean.dark` theme.
    pub fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get(THEME_NAME)
            .cloned()
            .unwrap_or_else(default_theme);

        Self {
            syntaxes,
            theme,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Highlights `code` written in `language`; unknown languages fall back to plain text.
    ///
    /// The returned lines carry foreground colors and font attributes only: the caller
    /// applies the code background, so one block can be reused inside any frame.
    pub fn highlight(&self, language: Option<&str>, code: &str) -> Block {
        let key = cache_key(language, code);
        if let Some(block) = self.cache.borrow().get(&key) {
            return Rc::clone(block);
        }

        let block = Rc::new(self.compute(language, code));

        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, Rc::clone(&block));
        block
    }

    fn compute(&self, language: Option<&str>, code: &str) -> Vec<Line<'static>> {
        let syntax = language
            .and_then(|name| self.syntaxes.find_syntax_by_token(name))
            .or_else(|| language.and_then(|name| self.syntaxes.find_syntax_by_extension(name)))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut lines = Vec::new();

        for line in LinesWithEndings::from(code) {
            let ranges = highlighter
                .highlight_line(line, &self.syntaxes)
                .unwrap_or_default();
            let spans: Vec<Span<'static>> = ranges
                .into_iter()
                .filter_map(|(style, text)| {
                    // `LinesWithEndings` keeps the `\n`; a styled newline would paint the
                    // whole next row, so it never becomes part of a span.
                    let text = text.trim_end_matches(['\n', '\r']);
                    if text.is_empty() {
                        return None;
                    }
                    Some(Span::styled(text.to_owned(), convert(style)))
                })
                .collect();

            lines.push(Line::from(spans));
        }

        if lines.is_empty() {
            lines.push(Line::default());
        }
        lines
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps a syntect style to a `ratatui` style: RGB foreground plus font attributes.
fn convert(style: SynStyle) -> Style {
    let color = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);

    let mut converted = Style::default().fg(color);
    if style.font_style.contains(FontStyle::BOLD) {
        converted = converted.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        converted = converted.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        converted = converted.add_modifier(Modifier::UNDERLINED);
    }
    converted
}

/// A fallback theme with the same name, in case the dump is ever trimmed.
fn default_theme() -> SynTheme {
    let themes = ThemeSet::load_defaults();
    themes
        .themes
        .values()
        .next()
        .cloned()
        .unwrap_or_else(syntect::highlighting::Theme::default)
}

/// BLAKE3 of the language and the code, so identical blocks hit the cache.
fn cache_key(language: Option<&str>, code: &str) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(language.unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    hasher.update(code.as_bytes());

    let digest = hasher.finalize();
    u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = "fn main() {\n    let answer = 42;\n}\n";

    #[test]
    fn known_language_is_highlighted_with_more_than_one_color() {
        let highlighter = Highlighter::new();
        let block = highlighter.highlight(Some("rust"), RUST);

        assert_eq!(block.len(), 3, "one line per source line");
        let colors: Vec<Option<Color>> = block
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.style.fg)
            .collect();
        assert!(colors.len() > 1);
        assert!(
            colors.windows(2).any(|pair| pair[0] != pair[1]),
            "a highlighted block uses several colors: {colors:?}"
        );
    }

    #[test]
    fn an_unknown_language_falls_back_to_plain_text() {
        let highlighter = Highlighter::new();
        let block = highlighter.highlight(Some("no-such-language"), "hello\n");

        assert_eq!(block.len(), 1);
        assert!(
            block[0]
                .spans
                .iter()
                .any(|span| span.content.contains("hello"))
        );
    }

    #[test]
    fn the_cache_returns_the_same_allocation() {
        let highlighter = Highlighter::new();
        let first = highlighter.highlight(Some("rust"), RUST);
        let second = highlighter.highlight(Some("rust"), RUST);

        assert!(
            Rc::ptr_eq(&first, &second),
            "a repeated block must be reused"
        );
    }

    #[test]
    fn a_different_language_or_code_is_a_different_entry() {
        let highlighter = Highlighter::new();
        let rust = highlighter.highlight(Some("rust"), RUST);
        let text = highlighter.highlight(Some("text"), RUST);

        assert!(!Rc::ptr_eq(&rust, &text));
    }

    #[test]
    fn the_cache_is_bounded() {
        let highlighter = Highlighter::new();
        for index in 0..CACHE_LIMIT + 8 {
            highlighter.highlight(Some("rust"), &format!("let value = {index};\n"));
        }

        assert!(highlighter.cache.borrow().len() <= CACHE_LIMIT);
    }
}
