//! Named colors and styles.
//!
//! Styles reference colors by name, never by hex (`AGENTS.md` §8), so a theme can remap
//! the palette without touching layout code. The theme module has no dependency on a
//! terminal library: it stores plain RGB triples and the TUI converts them.

use std::collections::BTreeMap;

/// An RGB color, independent of any terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// Opaque black.
    pub const BLACK: Self = Self::new(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Self = Self::new(255, 255, 255);

    /// Builds a color from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#rrggbb` or `#rgb`.
    pub fn from_hex(text: &str) -> Option<Self> {
        let digits = text.trim().trim_start_matches('#');

        match digits.len() {
            3 => {
                let mut channels = [0u8; 3];
                for (index, ch) in digits.chars().enumerate() {
                    let value = ch.to_digit(16)? as u8;
                    channels[index] = value * 17;
                }
                Some(Self::new(channels[0], channels[1], channels[2]))
            }
            6 => {
                let value = u32::from_str_radix(digits, 16).ok()?;
                Some(Self::new(
                    ((value >> 16) & 0xff) as u8,
                    ((value >> 8) & 0xff) as u8,
                    (value & 0xff) as u8,
                ))
            }
            _ => None,
        }
    }

    /// Formats the color as `#rrggbb`.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Linear blend towards `other`: `t = 0` is `self`, `t = 1` is `other`.
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let channel = |a: u8, b: u8| {
            let value = f32::from(a) + (f32::from(b) - f32::from(a)) * t;
            value.round().clamp(0.0, 255.0) as u8
        };

        Self::new(
            channel(self.r, other.r),
            channel(self.g, other.g),
            channel(self.b, other.b),
        )
    }
}

/// Palette slots. Layout code addresses these names, never raw values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    /// Page background.
    Background,
    /// Raised surfaces: popups, status bar.
    Panel,
    /// Primary text.
    Text,
    /// Secondary text.
    Dim,
    /// Hints and separators.
    Muted,
    /// Highlight: the wordmark, the cursor, the active line.
    Accent,
    /// Text drawn on top of `Accent`.
    AccentText,
    /// Box borders.
    Border,
    /// Successful results.
    Success,
    /// Warnings and budget exhaustion.
    Warn,
    /// Errors.
    Error,
    /// The breathing dot and the head of the context bar.
    Pulse,
    /// The user's own lines.
    User,
    /// Everything the engine answers.
    Assistant,
    /// Background of fenced code blocks and inline code.
    CodeBg,
    /// Background of a selected row in a list or palette.
    Selection,
}

impl Slot {
    /// Every slot, in documentation order.
    pub const ALL: [Self; 16] = [
        Self::Background,
        Self::Panel,
        Self::Text,
        Self::Dim,
        Self::Muted,
        Self::Accent,
        Self::AccentText,
        Self::Border,
        Self::Success,
        Self::Warn,
        Self::Error,
        Self::Pulse,
        Self::User,
        Self::Assistant,
        Self::CodeBg,
        Self::Selection,
    ];

    /// Name used in theme files and error messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Panel => "panel",
            Self::Text => "text",
            Self::Dim => "dim",
            Self::Muted => "muted",
            Self::Accent => "accent",
            Self::AccentText => "accent-text",
            Self::Border => "border",
            Self::Success => "success",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Pulse => "pulse",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::CodeBg => "code-bg",
            Self::Selection => "selection",
        }
    }

    /// Parses a slot name, case-insensitively and with `_` accepted for `-`.
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL.into_iter().find(|slot| slot.name() == name)
    }

    /// Every slot name, for error messages.
    pub fn names() -> String {
        Self::ALL
            .iter()
            .map(|slot| slot.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A named palette: one color per slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    name: String,
    colors: BTreeMap<Slot, Rgb>,
}

impl Theme {
    /// An empty palette named `name`; every slot falls back to black.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            colors: BTreeMap::new(),
        }
    }

    /// Theme name, also the file stem when loaded from disk.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Renames the theme.
    pub fn rename(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// Color of `slot`.
    pub fn get(&self, slot: Slot) -> Rgb {
        self.colors.get(&slot).copied().unwrap_or(Rgb::BLACK)
    }

    /// Overrides one slot.
    pub fn set(&mut self, slot: Slot, color: Rgb) {
        self.colors.insert(slot, color);
    }

    /// How many slots are defined.
    pub fn len(&self) -> usize {
        self.colors.len()
    }

    /// True when no slot is defined.
    pub fn is_empty(&self) -> bool {
        self.colors.is_empty()
    }

    /// True when every slot has a value.
    pub fn is_complete(&self) -> bool {
        Slot::ALL.iter().all(|slot| self.colors.contains_key(slot))
    }

    /// The breathing color at `phase` (radians): a subtle shift of `dim` towards `pulse`.
    ///
    /// The amplitude stays between 25% and 80% of the pulse color, so the animation is
    /// visible on a black background without ever flashing white.
    pub fn pulse(&self, phase: f32) -> Rgb {
        let wave = 0.5 + 0.5 * phase.sin();
        let amount = 0.25 + 0.55 * wave;
        self.get(Slot::Dim).mix(self.get(Slot::Pulse), amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> Theme {
        let mut theme = Theme::new("test");
        theme.set(Slot::Dim, Rgb::new(60, 60, 60));
        theme.set(Slot::Pulse, Rgb::WHITE);
        theme
    }

    #[test]
    fn parses_long_and_short_hex() {
        assert_eq!(Rgb::from_hex("#ff8800"), Some(Rgb::new(255, 136, 0)));
        assert_eq!(Rgb::from_hex("ff8800"), Some(Rgb::new(255, 136, 0)));
        assert_eq!(Rgb::from_hex("#f80"), Some(Rgb::new(255, 136, 0)));
        assert_eq!(Rgb::from_hex("#fff"), Some(Rgb::WHITE));
        assert_eq!(Rgb::from_hex(""), None);
        assert_eq!(Rgb::from_hex("#12"), None);
        assert_eq!(Rgb::from_hex("#gggggg"), None);
    }

    #[test]
    fn hex_roundtrip_is_lossless() {
        for color in [Rgb::BLACK, Rgb::WHITE, Rgb::new(1, 2, 3)] {
            assert_eq!(Rgb::from_hex(&color.to_hex()), Some(color));
        }
    }

    #[test]
    fn mix_is_clamped_at_both_ends() {
        let black = Rgb::BLACK;
        let white = Rgb::WHITE;

        assert_eq!(black.mix(white, -1.0), black);
        assert_eq!(black.mix(white, 0.0), black);
        assert_eq!(black.mix(white, 1.0), white);
        assert_eq!(black.mix(white, 4.0), white);
        assert_eq!(black.mix(white, 0.5), Rgb::new(128, 128, 128));
    }

    #[test]
    fn every_slot_name_parses_back() {
        for slot in Slot::ALL {
            assert_eq!(Slot::from_name(slot.name()), Some(slot));
            assert_eq!(
                Slot::from_name(&slot.name().to_ascii_uppercase()),
                Some(slot)
            );
        }

        assert_eq!(Slot::from_name("accent_text"), Some(Slot::AccentText));
        assert_eq!(Slot::from_name("nope"), None);
    }

    #[test]
    fn missing_slots_fall_back_to_black() {
        let theme = Theme::new("empty");

        assert!(theme.is_empty());
        assert!(!theme.is_complete());
        assert_eq!(theme.get(Slot::Text), Rgb::BLACK);
    }

    #[test]
    fn the_pulse_moves_between_dim_and_white() {
        let theme = dark();
        let low = theme.pulse(0.0);
        let high = theme.pulse(std::f32::consts::FRAC_PI_2);

        assert!(low.r >= 60 && low.r < high.r);
        assert_ne!(low, high);
    }

    #[test]
    fn a_full_palette_is_complete() {
        let mut theme = Theme::new("full");
        for slot in Slot::ALL {
            theme.set(slot, Rgb::new(1, 1, 1));
        }

        assert_eq!(theme.len(), Slot::ALL.len());
        assert!(theme.is_complete());
    }
}
