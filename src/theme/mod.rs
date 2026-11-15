//! Themes: palette, XML parsing, and loading (future `venera-theme`).
//!
//! Parsed with `quick-xml` with DTD disabled. Themes live in `src/themes/<name>.xml`
//! (project `themes/` is still accepted) and are validated by
//! `venera theme validate <path>`.
//!
//! A layout never names a raw color: it asks for a [`Slot`] and the active [`Theme`]
//! resolves it. Switching themes is therefore a palette swap, not a code change.

pub mod style;
pub mod xml;

use std::path::Path;

pub use style::{Rgb, Slot, Theme};
pub use xml::{MAX_THEME_BYTES, ThemeError};

/// Theme used when neither the session nor the user picks one.
///
/// `orange` is the default warm palette; `venera-dark` stays available as the ultra-dark
/// alternative.
pub const DEFAULT_THEME: &str = "orange";

/// The default palette: a near-black blue-grey page, warm orange accent, and distinct
/// role colors (green user, cyan assistant, grey system).
const ORANGE_PALETTE: [(Slot, Rgb); 16] = [
    (Slot::Background, Rgb::new(0x14, 0x14, 0x19)),
    (Slot::Panel, Rgb::new(0x10, 0x10, 0x14)),
    (Slot::Text, Rgb::new(0xdc, 0xdc, 0xdc)),
    (Slot::Dim, Rgb::new(0xa0, 0xa0, 0xa0)),
    (Slot::Muted, Rgb::new(0x5a, 0x5a, 0x66)),
    (Slot::Accent, Rgb::new(0xfa, 0xb2, 0x83)),
    (Slot::AccentText, Rgb::new(0x14, 0x14, 0x19)),
    (Slot::Border, Rgb::new(0x2a, 0x2a, 0x33)),
    (Slot::Success, Rgb::new(0x7f, 0xd8, 0x8f)),
    (Slot::Warn, Rgb::new(0xf5, 0xc0, 0x7a)),
    (Slot::Error, Rgb::new(0xe0, 0x6c, 0x75)),
    (Slot::Pulse, Rgb::new(0xfa, 0xb2, 0x83)),
    (Slot::User, Rgb::new(0x7f, 0xd8, 0x8f)),
    (Slot::Assistant, Rgb::new(0x56, 0xb6, 0xc2)),
    (Slot::CodeBg, Rgb::new(0x1e, 0x1e, 0x24)),
    (Slot::Selection, Rgb::new(0x33, 0x33, 0x3d)),
];

/// The ultra-dark palette: black background, near-white text, one white accent.
///
/// Contrast is the point: the background is `#000000`, body text is `#e4e4e4`, and
/// nothing in between competes with the code being read.
const DARK_PALETTE: [(Slot, Rgb); 16] = [
    (Slot::Background, Rgb::new(0x00, 0x00, 0x00)),
    (Slot::Panel, Rgb::new(0x07, 0x07, 0x07)),
    (Slot::Text, Rgb::new(0xe4, 0xe4, 0xe4)),
    (Slot::Dim, Rgb::new(0x7a, 0x7a, 0x7a)),
    (Slot::Muted, Rgb::new(0x45, 0x45, 0x45)),
    (Slot::Accent, Rgb::new(0xff, 0xff, 0xff)),
    (Slot::AccentText, Rgb::new(0x00, 0x00, 0x00)),
    (Slot::Border, Rgb::new(0x1f, 0x1f, 0x1f)),
    (Slot::Success, Rgb::new(0x9b, 0xc9, 0xa4)),
    (Slot::Warn, Rgb::new(0xd6, 0xb3, 0x6a)),
    (Slot::Error, Rgb::new(0xe0, 0x7a, 0x7a)),
    (Slot::Pulse, Rgb::new(0xff, 0xff, 0xff)),
    (Slot::User, Rgb::new(0xff, 0xff, 0xff)),
    (Slot::Assistant, Rgb::new(0xcf, 0xcf, 0xcf)),
    (Slot::CodeBg, Rgb::new(0x0d, 0x0d, 0x0d)),
    (Slot::Selection, Rgb::new(0x26, 0x26, 0x26)),
];

/// Grayscale variant for terminals without color, or for screenshots.
const MONO_PALETTE: [(Slot, Rgb); 16] = [
    (Slot::Background, Rgb::BLACK),
    (Slot::Panel, Rgb::new(0x0a, 0x0a, 0x0a)),
    (Slot::Text, Rgb::new(0xff, 0xff, 0xff)),
    (Slot::Dim, Rgb::new(0x8c, 0x8c, 0x8c)),
    (Slot::Muted, Rgb::new(0x50, 0x50, 0x50)),
    (Slot::Accent, Rgb::WHITE),
    (Slot::AccentText, Rgb::BLACK),
    (Slot::Border, Rgb::new(0x2a, 0x2a, 0x2a)),
    (Slot::Success, Rgb::new(0xf0, 0xf0, 0xf0)),
    (Slot::Warn, Rgb::new(0xc8, 0xc8, 0xc8)),
    (Slot::Error, Rgb::WHITE),
    (Slot::Pulse, Rgb::WHITE),
    (Slot::User, Rgb::WHITE),
    (Slot::Assistant, Rgb::new(0xc8, 0xc8, 0xc8)),
    (Slot::CodeBg, Rgb::new(0x12, 0x12, 0x12)),
    (Slot::Selection, Rgb::new(0x30, 0x30, 0x30)),
];

/// Names of the themes compiled into the binary.
pub fn builtin_names() -> &'static [&'static str] {
    &["orange", "venera-dark", "mono"]
}

/// A compiled-in theme, or `None` when the name is unknown.
pub fn builtin(name: &str) -> Option<Theme> {
    let key = normalize(name);

    match key.as_str() {
        "orange" | "opencode" | "opencode-dark" => Some(from_palette("orange", &ORANGE_PALETTE)),
        "venera-dark" | "dark" | "venera" => Some(from_palette("venera-dark", &DARK_PALETTE)),
        "mono" | "monochrome" => Some(from_palette("mono", &MONO_PALETTE)),
        _ => None,
    }
}

/// Loads a theme by name: project `themes/`, then the user config directory, then builtins.
///
/// A file that exists but does not parse is an error — silently falling back to a builtin
/// would hide a typo in the theme the user is editing.
pub fn load(name: &str, root: Option<&Path>) -> Result<Theme, ThemeError> {
    let file = format!("{name}.xml");
    let mut searched = Vec::new();

    if let Some(root) = root {
        for relative in ["src/themes", "themes"] {
            let path = root.join(relative).join(&file);
            searched.push(path.display().to_string());
            if path.is_file() {
                return xml::load_from_path(&path);
            }
        }
    }

    if let Some(dirs) = directories::ProjectDirs::from("", "", "venera") {
        let path = dirs.config_dir().join("themes").join(&file);
        searched.push(path.display().to_string());
        if path.is_file() {
            return xml::load_from_path(&path);
        }
    }

    if let Some(theme) = builtin(name) {
        return Ok(theme);
    }

    Err(ThemeError::NotFound {
        name: name.to_owned(),
        searched,
    })
}

/// Every theme that can be selected, sorted and deduplicated.
pub fn available(root: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = builtin_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();

    for dir in theme_dirs(root) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "xml")
                    && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                {
                    names.push(stem.to_owned());
                }
            }
        }
    }

    names.sort();
    names.dedup();
    names
}

fn theme_dirs(root: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(root) = root {
        dirs.push(root.join("src/themes"));
        dirs.push(root.join("themes"));
    }
    if let Some(project) = directories::ProjectDirs::from("", "", "venera") {
        dirs.push(project.config_dir().join("themes"));
    }
    dirs
}

fn from_palette(name: &str, palette: &[(Slot, Rgb); 16]) -> Theme {
    let mut theme = Theme::new(name);
    for (slot, color) in palette {
        theme.set(*slot, *color);
    }
    theme
}

fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_is_complete_and_loads() {
        for name in builtin_names() {
            let theme = load(name, None).expect("builtin must load");

            assert!(theme.is_complete(), "{name} is missing slots");
        }

        let dark = builtin("venera-dark").expect("dark theme");
        assert_eq!(dark.get(Slot::Background), Rgb::BLACK);
    }

    #[test]
    fn the_default_theme_is_the_orange_palette() {
        let theme = builtin(DEFAULT_THEME).expect("default theme");

        assert_eq!(theme.name(), "orange");
        assert_eq!(theme.get(Slot::Background), Rgb::new(20, 20, 25));
        assert_eq!(theme.get(Slot::Text), Rgb::new(220, 220, 220));
        assert_eq!(theme.get(Slot::Accent), Rgb::new(0xfa, 0xb2, 0x83));
        // Roles stay distinguishable: green user, cyan assistant, grey system.
        assert_eq!(theme.get(Slot::User), Rgb::new(0x7f, 0xd8, 0x8f));
        assert_eq!(theme.get(Slot::Assistant), Rgb::new(0x56, 0xb6, 0xc2));
        assert!(theme.get(Slot::CodeBg) != theme.get(Slot::Background));
    }

    #[test]
    fn the_default_theme_exists() {
        assert!(builtin(DEFAULT_THEME).is_some());
    }

    #[test]
    fn the_dark_theme_is_black_with_near_white_text() {
        let theme = builtin("venera-dark").expect("dark theme");

        assert_eq!(theme.get(Slot::Background), Rgb::BLACK);
        assert_eq!(theme.get(Slot::Text), Rgb::new(0xe4, 0xe4, 0xe4));
        assert_eq!(theme.get(Slot::Accent), Rgb::WHITE);
    }

    #[test]
    fn project_themes_win_over_builtins() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("themes"))?;
        std::fs::write(
            dir.path().join("themes").join("venera-dark.xml"),
            r##"<theme name="venera-dark"><color slot="text" value="#010203"/></theme>"##,
        )?;

        let theme = load("venera-dark", Some(dir.path()))?;
        assert_eq!(theme.get(Slot::Text), Rgb::new(1, 2, 3));
        Ok(())
    }

    #[test]
    fn an_unknown_theme_is_reported_with_its_search_paths() {
        let error = load("nope", None).expect_err("must fail");

        assert!(matches!(error, ThemeError::NotFound { .. }));
        assert!(error.to_string().contains("nope"));
    }

    #[test]
    fn available_lists_builtins_and_project_files() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("themes"))?;
        std::fs::write(
            dir.path().join("themes").join("solar.xml"),
            r#"<theme name="solar"/>"#,
        )?;

        let names = available(Some(dir.path()));

        assert!(names.contains(&"venera-dark".to_owned()));
        assert!(names.contains(&"mono".to_owned()));
        assert!(names.contains(&"solar".to_owned()));
        Ok(())
    }

    /// Built-in themes.
    #[test]
    fn the_orange_file_matches_the_builtin() -> Result<(), Box<dyn std::error::Error>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/themes")
            .join("orange.xml");
        let from_file = xml::load_from_path(&path)?;
        let built_in = builtin(DEFAULT_THEME).expect("builtin");

        assert!(from_file.is_complete());
        for slot in Slot::ALL {
            assert_eq!(
                from_file.get(slot),
                built_in.get(slot),
                "slot `{}` differs between themes/orange.xml and the builtin",
                slot.name()
            );
        }
        Ok(())
    }

    /// The shipped file and the compiled-in palette must not drift apart.
    #[test]
    fn the_shipped_file_matches_the_builtin() -> Result<(), Box<dyn std::error::Error>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/themes")
            .join("venera-dark.xml");
        let from_file = xml::load_from_path(&path)?;
        let built_in = builtin("venera-dark").expect("builtin");

        for slot in Slot::ALL {
            assert_eq!(
                from_file.get(slot),
                built_in.get(slot),
                "slot `{}` differs between themes/venera-dark.xml and the builtin",
                slot.name()
            );
        }
        Ok(())
    }
}
