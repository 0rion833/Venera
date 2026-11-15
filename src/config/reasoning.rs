//! Reasoning effort for models that can think before answering.
//!
//! Stored in config as `"reasoning": "off" | "low" | "medium" | "high"`, overridable
//! per turn with `--reasoning [LEVEL]`. The value is a hint for the provider: thinking
//! tokens are billed at the output rate and arrive in `Usage::reasoning` as a subset of
//! `Usage::output`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much thinking the model may spend on a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reasoning {
    /// No thinking; the cheapest and the default.
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl Reasoning {
    /// Every accepted name, in ascending order of effort.
    pub const NAMES: [&'static str; 4] = ["off", "low", "medium", "high"];

    /// Wire name, also used on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// True when the model is allowed to think.
    pub fn is_on(self) -> bool {
        self != Self::Off
    }

    /// Value for the OpenAI-compatible `reasoning_effort` field, if thinking is on.
    pub fn effort(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
        }
    }

    /// Parses a wire name, case-insensitively.
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();

        if name.eq_ignore_ascii_case("off") {
            Some(Self::Off)
        } else if name.eq_ignore_ascii_case("low") {
            Some(Self::Low)
        } else if name.eq_ignore_ascii_case("medium") {
            Some(Self::Medium)
        } else if name.eq_ignore_ascii_case("high") {
            Some(Self::High)
        } else {
            None
        }
    }
}

impl fmt::Display for Reasoning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Reasoning {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::from_name(name).ok_or_else(|| {
            format!(
                "unknown reasoning level `{name}`; expected one of: {}",
                Self::NAMES.join(", ")
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_level_case_insensitively() {
        let expected = [
            Reasoning::Off,
            Reasoning::Low,
            Reasoning::Medium,
            Reasoning::High,
        ];

        for (name, level) in Reasoning::NAMES.into_iter().zip(expected) {
            assert_eq!(Reasoning::from_name(name), Some(level));
            assert_eq!(Reasoning::from_name(&name.to_uppercase()), Some(level));
            assert_eq!(name.parse::<Reasoning>(), Ok(level));
        }
    }

    #[test]
    fn rejects_unknown_levels() {
        assert_eq!(Reasoning::from_name("maximum"), None);
        assert!(Reasoning::from_str("maximum").is_err());
    }

    #[test]
    fn off_has_no_request_effort() {
        assert!(!Reasoning::Off.is_on());
        assert_eq!(Reasoning::Off.effort(), None);
        assert_eq!(Reasoning::High.effort(), Some("high"));
        assert_eq!(Reasoning::High.to_string(), "high");
    }

    #[test]
    fn serde_uses_lowercase_names() {
        let level: Reasoning = serde_json::from_str("\"medium\"").expect("valid level");
        assert_eq!(level, Reasoning::Medium);
        assert_eq!(
            serde_json::to_string(&Reasoning::Low).unwrap_or_default(),
            "\"low\""
        );
    }
}
