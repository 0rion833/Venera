//! Language detection by file extension.
//!
//! The scanner needs a cheap, deterministic mapping from path to language before it
//! parses anything. Unsupported languages still get file metadata and full-text
//! retrieval (`TOOL.md` §6), so `Other` is a valid answer, not a failure.

use std::path::Path;

/// A language the indexer knows how to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Language {
    /// Rust, parsed with tree-sitter.
    Rust,
    /// CSS and other stylesheet dialects.
    Css,
    /// JavaScript, TypeScript, and their module variants.
    JavaScript,
    /// HTML and template markup.
    Html,
    /// Markdown and MDX.
    Markdown,
    /// JSON and JSONC.
    Json,
    /// TOML and YAML configuration.
    Config,
    /// Python, parsed with line heuristics.
    Python,
    /// Other source languages, parsed with a keyword scan.
    Generic,
    /// Anything else: metadata only.
    Other,
}

impl Language {
    /// Detects the language from the file extension.
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        let extension = path
            .as_ref()
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        match extension.as_str() {
            "rs" => Self::Rust,
            "css" | "scss" | "sass" | "less" => Self::Css,
            "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" => Self::JavaScript,
            "html" | "htm" | "xhtml" | "vue" | "svelte" => Self::Html,
            "md" | "markdown" | "mdx" => Self::Markdown,
            "json" | "jsonc" => Self::Json,
            "toml" | "yaml" | "yml" | "ini" | "conf" => Self::Config,
            "py" | "pyi" => Self::Python,
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "cs" | "java" | "kt"
            | "kts" | "scala" | "swift" | "go" | "rb" | "php" | "pl" | "pm" | "lua" | "dart"
            | "ex" | "exs" | "erl" | "hrl" | "hs" | "fs" | "fsx" | "clj" | "cljs" | "zig"
            | "nim" | "jl" | "r" | "sql" | "sh" | "bash" | "zsh" | "fish" | "ps1" | "groovy"
            | "gradle" | "vb" | "asm" | "s" | "sol" | "tf" | "hcl" | "d" | "pas" | "cmake" => {
                Self::Generic
            }
            _ => Self::Other,
        }
    }

    /// Short stable name used in the map and in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Css => "css",
            Self::JavaScript => "javascript",
            Self::Html => "html",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Config => "config",
            Self::Python => "python",
            Self::Generic => "generic",
            Self::Other => "other",
        }
    }

    /// Whether signature extraction can produce anything useful for this language.
    pub fn has_signatures(self) -> bool {
        !matches!(self, Self::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_extensions_case_insensitively() {
        assert_eq!(Language::from_path("src/main.rs"), Language::Rust);
        assert_eq!(Language::from_path("style/GLOBAL.CSS"), Language::Css);
        assert_eq!(Language::from_path("app/index.tsx"), Language::JavaScript);
        assert_eq!(Language::from_path("pages/index.html"), Language::Html);
        assert_eq!(Language::from_path("README.md"), Language::Markdown);
        assert_eq!(Language::from_path("config/default.json"), Language::Json);
        assert_eq!(Language::from_path("Cargo.toml"), Language::Config);
        assert_eq!(Language::from_path("app/main.py"), Language::Python);
        assert_eq!(Language::from_path("stubs/types.pyi"), Language::Python);
        assert_eq!(Language::from_path("main.go"), Language::Generic);
        assert_eq!(Language::from_path("App.java"), Language::Generic);
        assert_eq!(Language::from_path("schema.sql"), Language::Generic);
        assert_eq!(Language::from_path("logo.png"), Language::Other);
        assert_eq!(Language::from_path("Makefile"), Language::Other);
    }

    #[test]
    fn marks_unknown_languages_as_metadata_only() {
        assert!(!Language::Other.has_signatures());
        assert!(Language::Rust.has_signatures());
    }
}
