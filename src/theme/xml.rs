//! XML theme parsing.
//!
//! Hard requirements: `quick-xml` only, DTD processing disabled, no network access,
//! and a bounded input size so a malformed theme cannot stall the TUI.
//!
//! Format:
//!
//! ```xml
//! <theme name="venera-dark">
//!   <color slot="background" value="#000000"/>
//!   <color slot="text" value="#e4e4e4"/>
//! </theme>
//! ```
//!
//! A theme starts from [`builtin`](crate::theme::builtin) `venera-dark` and overrides what
//! it declares, so a partial theme is valid: declaring one color must not blank the rest.

use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::theme::DEFAULT_THEME;
use crate::theme::style::{Rgb, Slot, Theme};

/// Largest theme file accepted, in bytes.
pub const MAX_THEME_BYTES: usize = 64 * 1024;

/// Theme parsing failures.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("theme is {size} bytes, the limit is {limit}")]
    TooLarge { size: usize, limit: usize },
    #[error("theme XML is malformed: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("theme XML attribute is malformed: {0}")]
    Attribute(#[from] quick_xml::events::attributes::AttrError),
    #[error("theme XML has no <theme> root element")]
    NoRoot,
    #[error("theme element <{element}> needs a `{attribute}` attribute")]
    MissingAttribute {
        element: String,
        attribute: &'static str,
    },
    #[error("unknown palette slot `{name}`; expected one of: {}", Slot::names())]
    UnknownSlot { name: String },
    #[error("color `{value}` in slot `{slot}` is not #rrggbb")]
    BadColor { slot: String, value: String },
    #[error("theme declares a DTD, which is not allowed")]
    Dtd,
    #[error("failed to read theme {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("theme `{name}` was not found in {}", searched.join(", "))]
    NotFound { name: String, searched: Vec<String> },
}

/// Parses one theme document.
pub fn parse(text: &str) -> Result<Theme, ThemeError> {
    if text.len() > MAX_THEME_BYTES {
        return Err(ThemeError::TooLarge {
            size: text.len(),
            limit: MAX_THEME_BYTES,
        });
    }

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut theme = builtin_or_default();
    let mut seen_root = false;

    loop {
        match reader.read_event()? {
            Event::Start(element) | Event::Empty(element) => {
                let tag = element.name().as_ref().to_ascii_lowercase();

                match tag.as_str() {
                    "theme" => {
                        seen_root = true;
                        if let Some(name) = required(&element, "name")? {
                            // A partial theme starts from the builtin of the same name, so
                            // `mono.xml` with one color stays monochrome elsewhere. Unknown
                            // names still fall back to the default dark palette.
                            theme = crate::theme::builtin(&name).unwrap_or_else(builtin_or_default);
                            theme.rename(name);
                        }
                    }
                    "color" => {
                        let slot = required(&element, "slot")?.unwrap_or_default();
                        let value = required(&element, "value")?.unwrap_or_default();
                        apply(&mut theme, &slot, &value)?;
                    }
                    _ => {}
                }
            }
            Event::DocType(_) => return Err(ThemeError::Dtd),
            Event::Eof => break,
            _ => {}
        }
    }

    if !seen_root {
        return Err(ThemeError::NoRoot);
    }

    Ok(theme)
}

/// Reads a theme from `path`.
pub fn load_from_path(path: &Path) -> Result<Theme, ThemeError> {
    let text = std::fs::read_to_string(path).map_err(|source| ThemeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text)
}

/// The dark palette, or an empty theme named after it if the builtin is missing.
fn builtin_or_default() -> Theme {
    crate::theme::builtin(DEFAULT_THEME).unwrap_or_else(|| Theme::new(DEFAULT_THEME))
}

fn apply(theme: &mut Theme, slot_name: &str, value: &str) -> Result<(), ThemeError> {
    let slot = Slot::from_name(slot_name).ok_or_else(|| ThemeError::UnknownSlot {
        name: slot_name.to_owned(),
    })?;
    let color = Rgb::from_hex(value).ok_or_else(|| ThemeError::BadColor {
        slot: slot_name.to_owned(),
        value: value.to_owned(),
    })?;

    theme.set(slot, color);
    Ok(())
}

fn required(element: &BytesStart<'_>, name: &'static str) -> Result<Option<String>, ThemeError> {
    for attribute in element.attributes() {
        let attribute = attribute?;
        if attribute.key.as_ref() == name {
            return Ok(Some(attribute.value.into_owned()));
        }
    }

    Err(ThemeError::MissingAttribute {
        element: element.name().as_ref().to_owned(),
        attribute: name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: &str = r##"
<?xml version="1.0" encoding="utf-8"?>
<theme name="venera-dark">
  <color slot="background" value="#000000"/>
  <color slot="text" value="#e4e4e4"/>
  <color slot="accent" value="#ffffff"/>
</theme>
"##;

    #[test]
    fn parses_a_theme_and_keeps_defaults_for_missing_slots() {
        let theme = parse(DARK).expect("valid theme");

        assert_eq!(theme.name(), "venera-dark");
        assert_eq!(theme.get(Slot::Background), Rgb::BLACK);
        assert_eq!(theme.get(Slot::Text), Rgb::new(228, 228, 228));
        assert!(theme.is_complete(), "palette must stay complete");
    }

    #[test]
    fn rejects_a_dtd() {
        let text = r#"<!DOCTYPE theme [<!ENTITY x "y">]><theme name="x"/>"#;

        assert!(matches!(parse(text), Err(ThemeError::Dtd)));
    }

    #[test]
    fn rejects_unknown_slots_and_bad_colors() {
        let unknown = r##"<theme name="x"><color slot="hue" value="#fff"/></theme>"##;
        assert!(matches!(
            parse(unknown),
            Err(ThemeError::UnknownSlot { .. })
        ));

        let bad = r#"<theme name="x"><color slot="text" value="white"/></theme>"#;
        assert!(matches!(parse(bad), Err(ThemeError::BadColor { .. })));
    }

    #[test]
    fn requires_a_root_and_its_attributes() {
        assert!(matches!(parse(""), Err(ThemeError::NoRoot)));
        assert!(matches!(
            parse("<color slot=\"text\" value=\"#fff\"/>"),
            Err(ThemeError::NoRoot)
        ));
        assert!(matches!(
            parse("<theme name=\"x\"><color slot=\"text\"/></theme>"),
            Err(ThemeError::MissingAttribute { .. })
        ));
    }

    #[test]
    fn refuses_oversized_input() {
        let text = "x".repeat(MAX_THEME_BYTES + 1);

        assert!(matches!(parse(&text), Err(ThemeError::TooLarge { .. })));
    }

    #[test]
    fn a_partial_theme_inherits_the_builtin_with_the_same_name() {
        let text = r##"<theme name="mono"><color slot="text" value="#010203"/></theme>"##;
        let theme = parse(text).expect("valid theme");

        // Text is overwritten, everything else comes from `mono`, not from `venera-dark`.
        assert_eq!(theme.get(Slot::Text), Rgb::new(1, 2, 3));
        let mono = crate::theme::builtin("mono").expect("mono builtin");
        assert_eq!(theme.get(Slot::Background), mono.get(Slot::Background));
        assert_eq!(theme.get(Slot::Success), mono.get(Slot::Success));
    }

    #[test]
    fn an_unknown_name_falls_back_to_the_dark_palette() {
        let text = r##"<theme name="custom"><color slot="text" value="#010203"/></theme>"##;
        let theme = parse(text).expect("valid theme");
        let dark = crate::theme::builtin(DEFAULT_THEME).expect("dark builtin");

        assert_eq!(theme.get(Slot::Background), dark.get(Slot::Background));
        assert_eq!(theme.get(Slot::Text), Rgb::new(1, 2, 3));
    }

    #[test]
    fn loads_from_a_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("custom.xml");
        std::fs::write(&path, DARK)?;

        let theme = load_from_path(&path)?;
        assert_eq!(theme.name(), "venera-dark");
        Ok(())
    }
}
