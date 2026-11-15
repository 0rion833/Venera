//! Model picker: a filtered list with a movable selection.
//!
//! The picker holds no policy: it only answers "which row is selected" and "what does the
//! current filter match". Applying a model belongs to [`crate::tui::app::App`].

use crate::tui::models::ModelChoice;

/// One row of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Model id, the value applied on Enter.
    pub id: String,
    /// Short label: `gpt-6-luna`.
    pub label: String,
    /// `ctx 1.05M · 30/150 руб./1M · config`.
    pub detail: String,
    /// True for the model the session is already using.
    pub current: bool,
}

/// What confirming a picker row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// A model id.
    Model,
    /// A theme name.
    Theme,
    /// A stored session id.
    Session,
    /// An aggregator id.
    Provider,
}

/// Filtered list with a selection; models, themes, and sessions share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPicker {
    kind: PickerKind,
    title: &'static str,
    rows: Vec<Row>,
    filter: String,
    selected: usize,
}

impl ModelPicker {
    /// Builds the picker from resolved choices, marking `current`.
    pub fn new(choices: &[ModelChoice], current: &str) -> Self {
        let rows: Vec<Row> = choices
            .iter()
            .map(|choice| {
                let mut detail = Vec::new();
                let context = choice.context_label();
                if !context.is_empty() {
                    detail.push(context);
                }
                detail.push(choice.price_label());
                detail.push(choice.provider.clone());
                detail.push(if choice.curated {
                    "config".to_owned()
                } else {
                    "каталог".to_owned()
                });
                if !choice.tools {
                    detail.push("без инструментов".to_owned());
                }

                Row {
                    id: choice.id.clone(),
                    label: choice.label.clone(),
                    detail: detail.join(" · "),
                    current: choice.id == current,
                }
            })
            .collect();

        let selected = rows.iter().position(|row| row.id == current).unwrap_or(0);

        Self {
            kind: PickerKind::Model,
            title: "модели",
            rows,
            filter: String::new(),
            selected,
        }
    }

    /// Builds a picker over pre-built rows.
    pub fn list(kind: PickerKind, title: &'static str, rows: Vec<Row>, current: &str) -> Self {
        let selected = rows.iter().position(|row| row.id == current).unwrap_or(0);

        Self {
            kind,
            title,
            rows,
            filter: String::new(),
            selected,
        }
    }

    /// What confirming a row does.
    pub fn kind(&self) -> PickerKind {
        self.kind
    }

    /// Window title, for example `модели`.
    pub fn title(&self) -> &'static str {
        self.title
    }

    /// The active filter text.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Rows matching the filter, in list order.
    pub fn visible(&self) -> Vec<&Row> {
        let needle = self.filter.to_ascii_lowercase();

        self.rows
            .iter()
            .filter(|row| {
                needle.is_empty()
                    || row.label.to_ascii_lowercase().contains(&needle)
                    || row.id.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }

    /// The selected row, if the filter leaves anything.
    pub fn selected(&self) -> Option<&Row> {
        let visible = self.visible();
        visible.get(self.selected).copied()
    }

    /// Moves the selection down, wrapping at the end.
    pub fn next(&mut self) {
        let len = self.visible().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    /// Moves the selection up, wrapping at the start.
    pub fn previous(&mut self) {
        let len = self.visible().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    /// Selects the `index`-th visible row; out-of-range indices clamp to the last row.
    pub fn select(&mut self, index: usize) {
        let len = self.visible().len();
        if len > 0 {
            self.selected = index.min(len - 1);
        }
    }

    /// Adds a character to the filter and resets the selection.
    pub fn push(&mut self, ch: char) {
        self.filter.push(ch);
        self.selected = 0;
    }

    /// Removes the last filter character.
    pub fn backspace(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    /// Index of the selected row inside [`Self::visible`].
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Clears the filter.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::prices::{ModelPrice, PriceRates};

    fn choice(id: &str, label: &str, curated: bool) -> ModelChoice {
        ModelChoice {
            id: id.to_owned(),
            provider: "proxyapi".to_owned(),
            label: label.to_owned(),
            price: ModelPrice {
                rates: PriceRates {
                    input: 30.0,
                    output: 150.0,
                    cache_read: Some(3.0),
                    cache_write: None,
                },
                long_context: None,
            },
            context_window: 1_050_000,
            curated,
            tools: true,
        }
    }

    fn picker() -> ModelPicker {
        ModelPicker::new(
            &[
                choice("openai/gpt-6-luna", "gpt-6-luna", true),
                choice("openai/gpt-6-sol", "gpt-6-sol", false),
                choice("google/gemini-pro", "gemini-pro", false),
            ],
            "openai/gpt-6-luna",
        )
    }

    #[test]
    fn starts_on_the_current_model() {
        let picker = picker();

        assert_eq!(picker.visible().len(), 3);
        assert_eq!(
            picker.selected().map(|row| row.id.as_str()),
            Some("openai/gpt-6-luna")
        );
        assert!(picker.visible()[0].current);
    }

    #[test]
    fn the_filter_narrows_by_label_and_id() {
        let mut picker = picker();
        picker.push('s');
        picker.push('o');
        picker.push('l');

        let visible = picker.visible();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "openai/gpt-6-sol");
        assert_eq!(
            picker.selected().map(|row| row.id.as_str()),
            Some("openai/gpt-6-sol")
        );

        picker.backspace();
        assert_eq!(picker.filter(), "so");
        assert_eq!(picker.selected_index(), 0);
    }

    #[test]
    fn selection_wraps_and_survives_a_narrow_filter() {
        let mut picker = picker();
        picker.next();
        picker.next();
        assert_eq!(
            picker.selected().map(|row| row.label.as_str()),
            Some("gemini-pro")
        );
        picker.next();
        assert_eq!(
            picker.selected().map(|row| row.label.as_str()),
            Some("gpt-6-luna")
        );
        picker.previous();
        assert_eq!(
            picker.selected().map(|row| row.label.as_str()),
            Some("gemini-pro")
        );

        picker.push('z');
        assert!(picker.visible().is_empty());
        assert!(picker.selected().is_none());
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        let mut picker = picker();
        picker.push('z');
        picker.clear_filter();

        assert_eq!(picker.visible().len(), 3);
    }

    #[test]
    fn a_clicked_index_selects_and_clamps() {
        let mut picker = picker();

        picker.select(2);
        assert_eq!(
            picker.selected().map(|row| row.label.as_str()),
            Some("gemini-pro")
        );

        picker.select(99);
        assert_eq!(
            picker.selected().map(|row| row.label.as_str()),
            Some("gemini-pro")
        );

        picker.push('s');
        picker.push('o');
        picker.push('l');
        picker.select(0);
        assert_eq!(
            picker.selected().map(|row| row.label.as_str()),
            Some("gpt-6-sol")
        );
    }
}
