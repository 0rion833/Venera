//! Tiny flat-YAML catalogs (`src/i18n/ru.yml`, `src/i18n/en.yml`).
//!
//! Only `key: value` lines are supported: enough for UI strings, no parser crate and no
//! network dependency. `VENERA_LANG=en` switches the catalog; the default is Russian.

use std::collections::HashMap;
use std::sync::OnceLock;

const RU: &str = include_str!("../i18n/ru.yml");
const EN: &str = include_str!("../i18n/en.yml");

fn catalog() -> &'static HashMap<String, String> {
    static CATALOG: OnceLock<HashMap<String, String>> = OnceLock::new();

    CATALOG.get_or_init(|| {
        let language = std::env::var("VENERA_LANG")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(configured_language)
            .unwrap_or_else(|| "ru".to_owned());

        let source = if language.eq_ignore_ascii_case("en") {
            EN
        } else {
            RU
        };

        parse(source)
    })
}

/// Language written by `venera init` next to the user config.
fn configured_language() -> Option<String> {
    let path = directories::ProjectDirs::from("", "", "venera")?
        .config_dir()
        .join("language");
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim().to_lowercase();

    (!text.is_empty()).then_some(text)
}

/// Parses `key: value` lines, ignoring blanks and `#` comments.
pub fn parse(source: &str) -> HashMap<String, String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }

            let (key, value) = line.split_once(':')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(value)
                .replace("\\n", "\n");

            Some((key.trim().to_owned(), value))
        })
        .collect()
}

/// Looks a key up; a missing key returns the key itself, so the UI never disappears.
pub fn text(key: &str) -> String {
    catalog()
        .get(key)
        .cloned()
        .unwrap_or_else(|| key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_yaml_and_quoted_values() {
        let map = parse("a: b\nproject.none: \"project: none\"\n# c\n");

        assert_eq!(map.get("a").map(String::as_str), Some("b"));
        assert_eq!(
            map.get("project.none").map(String::as_str),
            Some("project: none")
        );
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn a_missing_key_returns_itself() {
        assert_eq!(text("nope"), "nope");
    }
}
