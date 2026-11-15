//! Markdown rendering for the transcript (`gui.md`).
//!
//! `pulldown-cmark` events are flattened into `ratatui::Line`s, because the transcript
//! scrolls by a plain row offset: every visual row must be a real line, not something
//! `Paragraph` wraps on its own.
//!
//! Supported: headings, wrapped paragraphs, ordered and unordered lists, inline code,
//! fenced code blocks with a language title and `syntect` highlighting, block quotes,
//! bold/italic/strikethrough, and horizontal rules. An unclosed fence at the end of a
//! streaming answer is rendered as plain text with a `streaming` marker.
//!
//! Finished messages are rendered once: the cache is keyed by the BLAKE3 hash of the text,
//! the width, and the base color, so a redraw only re-renders the message being streamed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use textwrap::core::display_width;

use crate::theme::{Rgb, Slot, Theme};
use crate::tui::highlight::Highlighter;

/// Rendered blocks kept in memory; the cache is cleared when it is full.
pub const CACHE_LIMIT: usize = 64;

/// A rendered block, shared by every frame until the text changes.
pub type Block = Rc<Vec<Line<'static>>>;

/// Markdown renderer bound to one palette.
pub struct Markdown {
    theme: Theme,
    highlighter: Highlighter,
    cache: RefCell<HashMap<u64, Block>>,
}

impl Markdown {
    /// Builds a renderer for `theme`; a theme switch creates a new instance.
    pub fn new(theme: Theme) -> Self {
        Self {
            theme,
            highlighter: Highlighter::new(),
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Renders `text` at `width` cells, with `base` as the body color slot.
    pub fn render(&self, text: &str, width: usize, base: Slot) -> Block {
        let width = width.max(1);
        let key = cache_key(text, width, base);

        if let Some(block) = self.cache.borrow().get(&key) {
            return Rc::clone(block);
        }

        let block = Rc::new(self.compute(text, width, base));

        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, Rc::clone(&block));
        block
    }

    fn compute(&self, text: &str, width: usize, base: Slot) -> Vec<Line<'static>> {
        let (stable, streaming) = split_open_fence(text);
        let mut builder = Builder::new(self, width, base);

        let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for event in Parser::new_ext(stable, options) {
            builder.event(event);
        }
        builder.flush();

        if let Some(open) = streaming {
            builder.streaming(open);
        }

        if builder.lines.is_empty() {
            builder.lines.push(Line::default());
        }
        builder.lines
    }

    fn color(&self, slot: Slot) -> ratatui::style::Color {
        rgb(self.theme.get(slot))
    }

    fn text_style(&self, base: Slot) -> Style {
        Style::default().fg(self.color(base))
    }
}

/// Converts a palette color for `ratatui`.
pub fn rgb(value: Rgb) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(value.r, value.g, value.b)
}

/// Everything a block needs while events are consumed.
struct Builder<'a> {
    markdown: &'a Markdown,
    width: usize,
    base: Slot,
    lines: Vec<Line<'static>>,
    /// Inline tokens of the block being read; each item keeps its own style.
    runs: Vec<(Style, String)>,
    /// Stack of inline styles, so `**bold *italic* **` nests.
    styles: Vec<Style>,
    /// Quote nesting, drawn as a vertical bar.
    quote: usize,
    /// Open lists; the item marker depends on the top one.
    lists: Vec<ListState>,
    /// Marker of the item being read, consumed by the first visual row.
    marker: Option<String>,
    /// True once the marker of the current item has been drawn.
    marker_used: bool,
    /// Fence being read: language and accumulated code.
    code: Option<CodeState>,
    /// Target of an open link or image, appended in parentheses.
    pending_link: Option<String>,
}

struct ListState {
    next: u64,
    ordered: bool,
}

struct CodeState {
    language: Option<String>,
    text: String,
}

impl<'a> Builder<'a> {
    fn new(markdown: &'a Markdown, width: usize, base: Slot) -> Self {
        Self {
            markdown,
            width,
            base,
            lines: Vec::new(),
            runs: Vec::new(),
            styles: Vec::new(),
            quote: 0,
            lists: Vec::new(),
            marker: None,
            marker_used: false,
            code: None,
            pending_link: None,
        }
    }

    fn style(&self) -> Style {
        self.styles
            .last()
            .copied()
            .unwrap_or_else(|| self.markdown.text_style(self.base))
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(code) = self.code.as_mut() {
                    code.text.push_str(&text);
                } else {
                    self.push_run(self.style(), text.to_string());
                }
            }
            Event::Code(text) => {
                let style = Style::default()
                    .fg(self.markdown.color(Slot::Accent))
                    .bg(self.markdown.color(Slot::CodeBg));
                self.push_run(style, text.to_string());
            }
            Event::SoftBreak => self.push_run(self.style(), " ".to_owned()),
            Event::HardBreak => self.flush_runs(),
            Event::Rule => {
                self.flush();
                let rule = "─".repeat(self.width);
                self.lines.push(Line::from(Span::styled(
                    rule,
                    Style::default().fg(self.markdown.color(Slot::Border)),
                )));
                self.blank_line();
            }
            Event::TaskListMarker(done) => {
                self.push_run(self.style(), if done { "☑ " } else { "☐ " }.to_owned());
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.push_run(self.style(), text.to_string());
            }
            Event::FootnoteReference(name) => {
                self.push_run(self.style(), format!("[{name}]"));
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_run(self.style(), text.to_string());
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { .. } => {
                self.flush();
                let accent = self.markdown.color(Slot::Accent);
                self.styles
                    .push(Style::default().fg(accent).add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some(CodeState {
                    language,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(ListState {
                    next: start.unwrap_or(1),
                    ordered: start.is_some(),
                });
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(list) if list.ordered => {
                        let number = list.next;
                        list.next = list.next.saturating_add(1);
                        format!("{number}. ")
                    }
                    Some(_) => "• ".to_owned(),
                    None => "• ".to_owned(),
                };
                self.marker = Some(format!("{}{}", "  ".repeat(depth), marker));
                self.marker_used = false;
            }
            Tag::Emphasis => self.push_style_modifier(Modifier::ITALIC),
            Tag::Strong => self.push_style_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.push_style_modifier(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => {
                let style = self
                    .style()
                    .fg(self.markdown.color(Slot::Accent))
                    .add_modifier(Modifier::UNDERLINED);
                self.styles.push(style);
                // The URL is kept so a link stays copyable in a terminal.
                self.pending_link = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.push_run(self.style(), "🖼 ".to_owned());
                self.pending_link = Some(dest_url.to_string());
            }
            Tag::HtmlBlock => {}
            Tag::FootnoteDefinition(name) => {
                self.flush();
                self.marker = Some(format!("[{name}] "));
            }
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {}
            Tag::Superscript => {}
            Tag::Subscript => {}
            Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_runs();
                self.blank_line();
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush_runs();
                self.blank_line();
            }
            TagEnd::BlockQuote(_) => {
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.render_code(code);
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Item => {
                self.flush_runs();
                self.marker = None;
                self.marker_used = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                if let Some(url) = self.pending_link.take() {
                    self.push_run(
                        Style::default().fg(self.markdown.color(Slot::Muted)),
                        format!(" ({url})"),
                    );
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_runs();
                self.marker = None;
            }
            TagEnd::HtmlBlock => {}
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => {}
            TagEnd::Superscript | TagEnd::Subscript => {}
            TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_style_modifier(&mut self, modifier: Modifier) {
        let style = self.style().add_modifier(modifier);
        self.styles.push(style);
    }

    fn push_run(&mut self, style: Style, text: String) {
        if text.is_empty() {
            return;
        }
        match self.runs.last_mut() {
            Some((last, content)) if *last == style => content.push_str(&text),
            _ => self.runs.push((style, text)),
        }
    }

    /// Wraps the accumulated inline runs and appends them as visual rows.
    fn flush_runs(&mut self) {
        if self.runs.is_empty() {
            return;
        }

        let runs = std::mem::take(&mut self.runs);
        let quote = self.quote;
        let marker_width = self.marker.as_deref().map_or(0, display_width);
        let column_width = self
            .width
            .saturating_sub(quote * 2)
            .saturating_sub(marker_width);

        for row in wrap(runs, column_width) {
            let mut spans = Vec::new();
            for _ in 0..quote {
                spans.push(Span::styled(
                    "│ ",
                    Style::default().fg(self.markdown.color(Slot::Muted)),
                ));
            }

            if let Some(marker) = self.marker.as_ref() {
                let head = if self.marker_used {
                    " ".repeat(marker_width)
                } else {
                    marker.clone()
                };
                spans.push(Span::styled(
                    head,
                    Style::default().fg(self.markdown.color(Slot::Accent)),
                ));
                self.marker_used = true;
            }
            spans.extend(row);
            self.lines.push(Line::from(spans));
        }
    }

    /// Ends the current block: flush inline runs and leave a blank row between blocks.
    fn flush(&mut self) {
        self.flush_runs();
        self.blank_line();
    }

    fn blank_line(&mut self) {
        if self.lines.last().is_none_or(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn render_code(&mut self, code: CodeState) {
        let width = self.width.max(4);
        let label = code.language.clone().unwrap_or_else(|| "code".to_owned());
        let accent = self.markdown.color(Slot::Accent);
        let border = self.markdown.color(Slot::Border);
        let code_bg = self.markdown.color(Slot::CodeBg);

        // `═`/`─` frame: the language sits in the top rule, exactly like `gui.md` asks.
        let head = format!("╭─ {label} ");
        let head_width = display_width(&head);
        let tail = "─".repeat(width.saturating_sub(head_width + 1));
        self.lines.push(Line::from(vec![
            Span::styled(
                head,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(tail, Style::default().fg(border)),
            Span::styled("╮", Style::default().fg(border)),
        ]));

        let inner = width.saturating_sub(2);
        let block = self
            .highlighter()
            .highlight(code.language.as_deref(), &code.text);
        for line in block.iter() {
            let mut spans = Vec::new();
            let mut used = 0usize;

            for span in &line.spans {
                let text = truncate(&span.content, inner.saturating_sub(used));
                if text.is_empty() {
                    continue;
                }
                used += display_width(&text);
                spans.push(Span::styled(text, span.style));
            }

            let pad = inner.saturating_sub(used);
            let mut row = vec![Span::styled("│", Style::default().fg(border).bg(code_bg))];
            let mut content = spans;
            if pad > 0 {
                content.push(Span::styled(" ".repeat(pad), Style::default().bg(code_bg)));
            }
            row.append(&mut content);
            row.push(Span::styled("│", Style::default().fg(border).bg(code_bg)));
            self.lines
                .push(Line::from(row).style(Style::default().bg(code_bg)));
        }

        self.lines.push(Line::from(vec![
            Span::styled("╰", Style::default().fg(border)),
            Span::styled(
                "─".repeat(width.saturating_sub(2)),
                Style::default().fg(border),
            ),
            Span::styled("╯", Style::default().fg(border)),
        ]));
        self.blank_line();
    }

    fn highlighter(&self) -> &Highlighter {
        &self.markdown.highlighter
    }

    /// Renders an unclosed fence as plain text with a `streaming` marker.
    fn streaming(&mut self, text: &str) {
        self.blank_line();
        self.lines.push(Line::from(vec![
            Span::styled("▋ ", Style::default().fg(self.markdown.color(Slot::Accent))),
            Span::styled(
                "streaming · open code block",
                Style::default()
                    .fg(self.markdown.color(Slot::Muted))
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));

        for line in text.lines() {
            let styled = self.markdown.text_style(self.base);
            self.runs.push((styled, line.to_owned()));
            self.flush_runs();
        }
    }
}

/// Splits inline runs into visual rows of at most `width` cells.
fn wrap(runs: Vec<(Style, String)>, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for (style, text) in runs {
        for token in tokenize(&text) {
            let token_width = display_width(&token);
            let space = token.chars().all(char::is_whitespace);

            if space {
                if used > 0 && used < width {
                    row.push(Span::styled(token, style));
                    used += token_width;
                }
                continue;
            }

            if used + token_width > width && used > 0 {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }

            if token_width <= width {
                row.push(Span::styled(token, style));
                used += token_width;
                continue;
            }

            // A single word longer than the row is broken at the cell boundary.
            let mut chunk = String::new();
            let mut chunk_width = 0usize;
            for ch in token.chars() {
                let ch_width = display_width(&ch.to_string());
                if chunk_width + ch_width > width && chunk_width > 0 {
                    row.push(Span::styled(std::mem::take(&mut chunk), style));
                    rows.push(std::mem::take(&mut row));
                    chunk_width = 0;
                }
                chunk.push(ch);
                chunk_width += ch_width;
            }
            if !chunk.is_empty() {
                row.push(Span::styled(chunk, style));
                used = chunk_width;
            }
        }
    }

    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows.into_iter().map(merge_spans).collect()
}

/// Joins neighbouring spans that share a style, so a wrapped run stays one span.
fn merge_spans(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut merged: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if last.style == span.style => {
                last.content.to_mut().push_str(&span.content);
            }
            _ => merged.push(span),
        }
    }
    merged
}

/// Splits a run into words that keep their trailing space.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch == '\n' {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
        if ch.is_whitespace() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Cuts `text` to `width` cells, appending `…` when something was dropped.
fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_owned();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = display_width(&ch.to_string());
        if used + ch_width > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

/// Separates the trailing unclosed fence from the stable part of a streaming answer.
fn split_open_fence(text: &str) -> (&str, Option<&str>) {
    let mut open = false;
    let mut offset = 0usize;
    let mut last_open = 0usize;

    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            if open {
                open = false;
            } else {
                open = true;
                last_open = offset;
            }
        }
        offset += line.len();
    }

    if open {
        (&text[..last_open], Some(&text[last_open..]))
    } else {
        (text, None)
    }
}

/// Cache key: text, width, and base color.
fn cache_key(text: &str, width: usize, base: Slot) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(base.name().as_bytes());
    hasher.update(b"\0");
    hasher.update(&(width as u64).to_le_bytes());
    hasher.update(text.as_bytes());

    let digest = hasher.finalize();
    u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{DEFAULT_THEME, builtin};

    fn renderer() -> Markdown {
        Markdown::new(builtin(DEFAULT_THEME).expect("theme"))
    }

    fn text_of(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_heading_is_bold_and_accented() {
        let markdown = renderer();
        let lines = markdown.render("# Title", 40, Slot::Assistant);

        let title = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("Title"))
            .expect("heading text");

        assert!(title.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(title.style.fg, Some(markdown.color(Slot::Accent)));
    }

    #[test]
    fn inline_code_gets_the_code_background() {
        let markdown = renderer();
        let lines = markdown.render("use `cargo test` now", 60, Slot::Assistant);

        let code = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("cargo test"))
            .expect("inline code");

        assert_eq!(code.style.bg, Some(markdown.color(Slot::CodeBg)));
        assert_eq!(code.style.fg, Some(markdown.color(Slot::Accent)));
    }

    #[test]
    fn a_fenced_block_gets_a_frame_with_its_language() {
        let markdown = renderer();
        let lines = markdown.render("```rust\nfn main() {}\n```", 40, Slot::Assistant);
        let text = text_of(&lines);

        assert!(text.contains("╭─ rust"), "missing language title: {text}");
        assert!(text.contains("fn main"), "missing code body: {text}");
        assert!(text.contains('╰'), "missing bottom border: {text}");
    }

    #[test]
    fn an_unclosed_fence_is_plain_text_with_a_streaming_marker() {
        let markdown = renderer();
        let lines = markdown.render("intro\n\n```py\nprint(1)", 40, Slot::Assistant);
        let text = text_of(&lines);

        assert!(text.contains("streaming"), "missing marker: {text}");
        assert!(
            text.contains("print(1)"),
            "the partial code must stay: {text}"
        );
        assert!(
            !text.contains("╭─ py"),
            "an unfinished block must not pretend to be closed: {text}"
        );
    }

    #[test]
    fn paragraphs_wrap_to_the_requested_width() {
        let markdown = renderer();
        let lines = markdown.render(&"word ".repeat(40), 20, Slot::Assistant);

        for line in lines.iter() {
            let width: usize = line
                .spans
                .iter()
                .map(|span| display_width(&span.content))
                .sum();
            assert!(width <= 20, "row is {width} cells wide: {line:?}");
        }
    }

    #[test]
    fn lists_keep_their_markers() {
        let markdown = renderer();
        let lines = markdown.render("1. first\n2. second", 30, Slot::Assistant);
        let text = text_of(&lines);

        assert!(text.contains("1. first"), "got: {text}");
        assert!(text.contains("2. second"), "got: {text}");
    }

    #[test]
    fn quotes_get_a_vertical_bar() {
        let markdown = renderer();
        let lines = markdown.render("> quoted", 30, Slot::Assistant);

        assert!(text_of(&lines).contains("│ quoted"));
    }

    #[test]
    fn a_finished_block_is_cached() {
        let markdown = renderer();
        let first = markdown.render("hello", 30, Slot::Assistant);
        let second = markdown.render("hello", 30, Slot::Assistant);

        assert!(Rc::ptr_eq(&first, &second));
    }

    #[test]
    fn the_base_color_is_part_of_the_cache_key() {
        let markdown = renderer();
        let user = markdown.render("hello", 30, Slot::User);
        let assistant = markdown.render("hello", 30, Slot::Assistant);

        assert!(!Rc::ptr_eq(&user, &assistant));
        assert_ne!(user[0].spans[0].style.fg, assistant[0].spans[0].style.fg);
    }
}
