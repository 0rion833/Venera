//! Signature extraction: the compact, model-facing view of a file.
//!
//! A signature is navigation metadata, never a replacement for source (`TOOL.md` §3).
//! It carries a line number so the model can ask for exactly the range it needs instead
//! of pulling the whole file — that is where the token savings come from.
//!
//! Rust is parsed with tree-sitter. Other languages use deterministic line heuristics:
//! cheaper, no grammar dependency, and good enough to answer "what lives where". The
//! heuristic never invents a symbol it did not see in the source.

use tree_sitter::Node;

use crate::vce::index::language::Language;
use crate::vce::index::scanner::SourceFile;

/// Longest signature kept, in characters. Anything longer is cut with an ellipsis.
const MAX_SIGNATURE_CHARS: usize = 120;

/// What a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A free function or method.
    Function,
    /// A struct or record.
    Struct,
    /// An enum.
    Enum,
    /// A trait or interface declaration.
    Trait,
    /// An impl block.
    Impl,
    /// A module or namespace.
    Module,
    /// A constant or static.
    Constant,
    /// A type alias.
    Type,
    /// An import or use declaration.
    Import,
    /// A CSS selector.
    Selector,
    /// An at-rule such as `@media`.
    Rule,
    /// A CSS custom property.
    Variable,
    /// A JS or CSS class.
    Class,
    /// A markup element.
    Element,
    /// A Markdown heading.
    Heading,
    /// A configuration key or section.
    Key,
}

impl SymbolKind {
    /// Short stable name used in the map.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "mod",
            Self::Constant => "const",
            Self::Type => "type",
            Self::Import => "use",
            Self::Selector => "sel",
            Self::Rule => "rule",
            Self::Variable => "var",
            Self::Class => "class",
            Self::Element => "el",
            Self::Heading => "h",
            Self::Key => "key",
        }
    }

    /// Parses the short name stored in the index; unknown kinds are dropped.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "fn" => Some(Self::Function),
            "struct" => Some(Self::Struct),
            "enum" => Some(Self::Enum),
            "trait" => Some(Self::Trait),
            "impl" => Some(Self::Impl),
            "mod" => Some(Self::Module),
            "const" => Some(Self::Constant),
            "type" => Some(Self::Type),
            "use" => Some(Self::Import),
            "sel" => Some(Self::Selector),
            "rule" => Some(Self::Rule),
            "var" => Some(Self::Variable),
            "class" => Some(Self::Class),
            "el" => Some(Self::Element),
            "h" => Some(Self::Heading),
            "key" => Some(Self::Key),
            _ => None,
        }
    }
}

/// One extracted definition.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// Index into [`crate::vce::index::scanner::Scan::files`].
    pub file: usize,
    /// What kind of definition this is.
    pub kind: SymbolKind,
    /// Name as written in the source.
    pub name: String,
    /// Name qualified by its owners, e.g. `Config::new` or `store::Config::new`.
    ///
    /// Two `fn new` in different impl blocks share `name` but not this. Stable IDs are
    /// built from this, never from the line number (`TOOL.md` §6).
    pub qualified_name: String,
    /// One-based line of the definition.
    pub line: u32,
    /// One-based line where the definition ends.
    ///
    /// For line-based heuristics this equals [`Self::line`]: the guide never invents a
    /// span it did not parse.
    pub end_line: u32,
    /// Single-line signature, already truncated.
    pub signature: String,
}

/// Extracts every signature the language allows for `file`.
pub fn extract(file_index: usize, file: &SourceFile) -> Vec<Symbol> {
    match file.language {
        Language::Rust => rust_symbols(file_index, &file.text),
        Language::Css => css_symbols(file_index, &file.text),
        Language::JavaScript => script_symbols(file_index, &file.text),
        Language::Html => html_symbols(file_index, &file.text),
        Language::Markdown => markdown_symbols(file_index, &file.text),
        Language::Json => json_symbols(file_index, &file.text),
        Language::Config => config_symbols(file_index, &file.text),
        Language::Python => python_symbols(file_index, &file.text),
        Language::Generic => generic_symbols(file_index, &file.text),
        Language::Other => Vec::new(),
    }
}

/// Rust, via tree-sitter. Parse errors degrade to "fewer symbols", never to a failure.
fn rust_symbols(file: usize, text: &str) -> Vec<Symbol> {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };

    let source = text.as_bytes();
    let mut symbols = Vec::new();
    collect_rust(tree.root_node(), source, file, "", &mut symbols);
    symbols
}

/// Walks the tree, tracking the owning `mod`, `impl`, or `trait`.
///
/// Only scopes that own members extend the qualifier. The body of a function does not:
/// an item declared inside a function is local and must not look like `Type::fn::item`.
fn collect_rust(node: Node<'_>, source: &[u8], file: usize, owner: &str, out: &mut Vec<Symbol>) {
    let kind = node.kind();

    if kind == "use_declaration" {
        let line = start_line(node);
        let raw = String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]);
        // Imports may contain braces; only the statement terminator ends them.
        let signature = truncate(&collapse_whitespace(
            raw.split(';').next().unwrap_or_default(),
        ));
        out.push(Symbol {
            file,
            kind: SymbolKind::Import,
            name: signature.clone(),
            qualified_name: signature.clone(),
            line,
            end_line: end_line(node),
            signature,
        });
        return;
    }

    let mut child_owner = owner.to_owned();

    if let Some((symbol_kind, field)) = rust_kind(kind) {
        let name = node
            .child_by_field_name(field)
            .and_then(|child| child.utf8_text(source).ok())
            .unwrap_or_default()
            .to_owned();

        if !name.is_empty() && name != "tests" {
            let qualified_name = qualify(owner, &name);
            out.push(Symbol {
                file,
                kind: symbol_kind,
                name,
                qualified_name: qualified_name.clone(),
                line: start_line(node),
                end_line: end_line(node),
                signature: signature_text(source, node.start_byte(), node.end_byte()),
            });

            // Only scopes that own members extend the qualifier; the body of a function
            // does not, so an item declared inside it stays local.
            child_owner = if owns_members(symbol_kind) {
                qualified_name
            } else {
                String::new()
            };
        } else {
            child_owner = String::new();
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_rust(child, source, file, &child_owner, out);
    }
}

/// Whether members of this symbol are qualified by its name.
fn owns_members(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Module | SymbolKind::Impl | SymbolKind::Trait
    )
}

/// `owner::name`, or just `name` at the top level.
fn qualify(owner: &str, name: &str) -> String {
    if owner.is_empty() {
        name.to_owned()
    } else {
        format!("{owner}::{name}")
    }
}

fn start_line(node: Node<'_>) -> u32 {
    node.start_position().row as u32 + 1
}

fn end_line(node: Node<'_>) -> u32 {
    node.end_position().row as u32 + 1
}

fn rust_kind(kind: &str) -> Option<(SymbolKind, &'static str)> {
    match kind {
        "function_item" => Some((SymbolKind::Function, "name")),
        "struct_item" => Some((SymbolKind::Struct, "name")),
        "enum_item" => Some((SymbolKind::Enum, "name")),
        "trait_item" => Some((SymbolKind::Trait, "name")),
        "mod_item" => Some((SymbolKind::Module, "name")),
        "const_item" | "static_item" => Some((SymbolKind::Constant, "name")),
        "type_item" => Some((SymbolKind::Type, "name")),
        "impl_item" => Some((SymbolKind::Impl, "type")),
        _ => None,
    }
}

/// CSS: selectors and at-rules at the top level, custom properties anywhere.
fn css_symbols(file: usize, text: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let mut depth = 0i32;

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let line_no = index as u32 + 1;

        if line.is_empty()
            || line.starts_with("/*")
            || line.starts_with('*')
            || line.starts_with("//")
        {
            continue;
        }

        if line.starts_with("--") && line.contains(':') {
            let name = line
                .split_once(':')
                .map(|(name, _)| name.trim())
                .unwrap_or_default();
            out.push(symbol(file, SymbolKind::Variable, name, line_no, line));
            continue;
        }

        if line.ends_with('{') {
            if depth == 0 || line.starts_with('@') {
                let head = line.trim_end_matches('{').trim();
                let kind = if head.starts_with('@') {
                    SymbolKind::Rule
                } else {
                    SymbolKind::Selector
                };
                out.push(symbol(file, kind, head, line_no, line));
            }
            depth += 1;
        } else if line.ends_with('}') {
            depth = (depth - 1).max(0);
        }
    }

    out
}

/// JavaScript and TypeScript: top-level declarations only.
///
/// Indented lines are skipped, so a closure or a local `const` inside a function never
/// enters the map. A top-level `const name = value` is a definition and is kept: without
/// it a small side-effect script such as an event-wiring file would show up empty.
fn script_symbols(file: usize, text: &str) -> Vec<Symbol> {
    let mut out = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            continue;
        }

        let line = raw.trim();
        let line_no = index as u32 + 1;
        let body = line
            .strip_prefix("export default ")
            .or_else(|| line.strip_prefix("export "))
            .or_else(|| line.strip_prefix("declare "))
            .unwrap_or(line);

        let Some((kind, rest)) = script_kind(body) else {
            continue;
        };

        let name = leading_identifier(rest);
        if name.is_empty() {
            continue;
        }

        out.push(symbol(file, kind, name, line_no, line));
    }

    out
}

fn script_kind(body: &str) -> Option<(SymbolKind, &str)> {
    for prefix in ["async function ", "function ", "async function* "] {
        if let Some(rest) = body.strip_prefix(prefix) {
            return Some((SymbolKind::Function, rest));
        }
    }
    for prefix in ["abstract class ", "class "] {
        if let Some(rest) = body.strip_prefix(prefix) {
            return Some((SymbolKind::Class, rest));
        }
    }
    for prefix in ["interface ", "type "] {
        if let Some(rest) = body.strip_prefix(prefix) {
            return Some((SymbolKind::Type, rest));
        }
    }
    if let Some(rest) = body.strip_prefix("import ") {
        return Some((SymbolKind::Import, rest));
    }

    for prefix in ["const ", "let ", "var "] {
        if let Some(rest) = body.strip_prefix(prefix) {
            let is_function =
                rest.contains("=>") || rest.contains("= function") || rest.contains("= async");
            if is_function {
                return Some((SymbolKind::Function, rest));
            }
            return Some((SymbolKind::Constant, rest));
        }
    }

    None
}

/// HTML: only structural landmarks, never every `div`.
/// Python: classes and functions by indentation, no grammar dependency.
fn python_symbols(file: usize, text: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut classes: Vec<(usize, String)> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_number = index as u32 + 1;
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let indent = raw.len() - trimmed.len();
        while classes.last().is_some_and(|(level, _)| indent <= *level) {
            classes.pop();
        }

        if let Some(rest) = trimmed.strip_prefix("class ") {
            let name = python_name(rest);
            if !name.is_empty() {
                let mut item = symbol(file, SymbolKind::Class, name, line_number, trimmed);
                item.end_line = python_block_end(text, index, indent);
                symbols.push(item);
                classes.push((indent, name.to_owned()));
            }
            continue;
        }

        let body = trimmed
            .strip_prefix("async def ")
            .or_else(|| trimmed.strip_prefix("def "));

        if let Some(rest) = body {
            let name = python_name(rest);
            if !name.is_empty() {
                let mut item = symbol(file, SymbolKind::Function, name, line_number, trimmed);
                item.end_line = python_block_end(text, index, indent);
                if let Some((_, owner)) = classes.last() {
                    item.qualified_name = format!("{owner}.{name}");
                }
                symbols.push(item);
            }
        }
    }

    symbols
}

/// Identifier after `class` or `def`.
fn python_name(rest: &str) -> &str {
    let end = rest
        .char_indices()
        .find(|(_, ch)| !(ch.is_alphanumeric() || *ch == '_'))
        .map(|(index, _)| index)
        .unwrap_or(rest.len());

    &rest[..end]
}

/// Last line of an indented block; stops at the first non-empty line with less indentation.
fn python_block_end(text: &str, start: usize, indent: usize) -> u32 {
    let lines: Vec<&str> = text.lines().collect();
    let mut end = start + 1;

    for (index, line) in lines.iter().enumerate().skip(start + 1) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.len() - trimmed.len() <= indent {
            break;
        }
        end = index + 1;
    }

    end as u32
}

/// Generic source files: a keyword scan over the common declaration syntaxes.
fn generic_symbols(file: usize, text: &str) -> Vec<Symbol> {
    const KEYWORDS: &[(&str, SymbolKind)] = &[
        ("class", SymbolKind::Class),
        ("struct", SymbolKind::Struct),
        ("interface", SymbolKind::Trait),
        ("trait", SymbolKind::Trait),
        ("enum", SymbolKind::Enum),
        ("func", SymbolKind::Function),
        ("fn", SymbolKind::Function),
        ("function", SymbolKind::Function),
        ("fun", SymbolKind::Function),
        ("def", SymbolKind::Function),
        ("sub", SymbolKind::Function),
        ("proc", SymbolKind::Function),
        ("method", SymbolKind::Function),
        ("type", SymbolKind::Type),
        ("module", SymbolKind::Module),
        ("namespace", SymbolKind::Module),
        ("object", SymbolKind::Module),
        ("impl", SymbolKind::Impl),
        ("const", SymbolKind::Constant),
    ];

    let lines: Vec<&str> = text.lines().collect();
    let mut symbols = Vec::new();

    for (index, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        for (keyword, kind) in KEYWORDS {
            let Some(rest) = strip_keyword(trimmed, keyword) else {
                continue;
            };
            let name = generic_name(rest);
            if name.is_empty() || is_reserved(name) {
                continue;
            }

            let mut item = symbol(file, *kind, name, index as u32 + 1, trimmed);
            item.end_line = brace_end(&lines, index);
            symbols.push(item);
            break;
        }
    }

    symbols
}

/// Text after `keyword`, when the keyword sits on a word boundary.
fn strip_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let position = line.find(keyword)?;
    let before = &line[..position];
    if !before.is_empty()
        && !before
            .ends_with(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '{' | '}' | '(' | ','))
    {
        return None;
    }

    let after = &line[position + keyword.len()..];
    if after.starts_with(|ch: char| ch.is_alphanumeric() || ch == '_') {
        return None;
    }

    Some(after.trim_start())
}

/// Identifier after a keyword, including `ns::Name` qualifiers.
fn generic_name(rest: &str) -> &str {
    let rest = rest.trim_start_matches(['<', '(', ':']);
    let end = rest
        .char_indices()
        .find(|(_, ch)| !(ch.is_alphanumeric() || *ch == '_' || *ch == ':'))
        .map(|(index, _)| index)
        .unwrap_or(rest.len());

    rest[..end].trim_end_matches(':')
}

/// Common control-flow words that follow keywords by accident.
fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        "if" | "for" | "while" | "return" | "new" | "self" | "this" | "static" | "final"
    )
}

/// End line by brace balance; heuristic languages without braces stay on one line.
fn brace_end(lines: &[&str], start: usize) -> u32 {
    let mut depth = 0i32;
    let mut opened = false;

    for (index, line) in lines.iter().enumerate().skip(start) {
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }

        if opened && depth <= 0 {
            return index as u32 + 1;
        }
        if index > start + 400 {
            break;
        }
    }

    start as u32 + 1
}

fn html_symbols(file: usize, text: &str) -> Vec<Symbol> {
    const TAGS: &[&str] = &[
        "title", "h1", "h2", "h3", "h4", "h5", "h6", "section", "main", "nav", "header", "footer",
        "article", "aside", "script", "link",
    ];

    let mut out = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix('<') else {
            continue;
        };

        let tag = rest
            .split(|ch: char| ch.is_whitespace() || ch == '>' || ch == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();

        if !TAGS.contains(&tag.as_str()) {
            continue;
        }

        let mut name = tag;
        if let Some(id) = attribute(rest, "id") {
            name.push('#');
            name.push_str(id);
        } else if let Some(class) = attribute(rest, "class") {
            name.push('.');
            name.push_str(class.split_whitespace().next().unwrap_or_default());
        }

        out.push(symbol(
            file,
            SymbolKind::Element,
            &name,
            index as u32 + 1,
            line,
        ));
    }

    out
}

/// Markdown: headings are the table of contents.
fn markdown_symbols(file: usize, text: &str) -> Vec<Symbol> {
    let mut out = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if !line.starts_with('#') {
            continue;
        }
        let name = line.trim_start_matches('#').trim();
        if name.is_empty() {
            continue;
        }
        out.push(symbol(
            file,
            SymbolKind::Heading,
            name,
            index as u32 + 1,
            line,
        ));
    }

    out
}

/// JSON: top-level keys only.
fn json_symbols(file: usize, text: &str) -> Vec<Symbol> {
    let mut out = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if !line.starts_with('"') {
            continue;
        }
        let Some((name, _)) = line[1..].split_once('"') else {
            continue;
        };
        out.push(symbol(
            file,
            SymbolKind::Key,
            name,
            index as u32 + 1,
            line.trim_end_matches(','),
        ));
    }

    out
}

/// TOML sections and YAML top-level keys.
fn config_symbols(file: usize, text: &str) -> Vec<Symbol> {
    let mut out = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            out.push(symbol(
                file,
                SymbolKind::Key,
                trimmed.trim_matches(['[', ']']),
                index as u32 + 1,
                trimmed,
            ));
            continue;
        }

        if raw.starts_with(' ') || raw.starts_with('\t') {
            continue;
        }

        let name = trimmed
            .split_once('=')
            .map(|(name, _)| name.trim())
            .or_else(|| trimmed.split_once(':').map(|(name, _)| name.trim()));

        if let Some(name) = name
            && !name.is_empty()
        {
            out.push(symbol(
                file,
                SymbolKind::Key,
                name,
                index as u32 + 1,
                trimmed,
            ));
        }
    }

    out
}

fn symbol(file: usize, kind: SymbolKind, name: &str, line: u32, signature: &str) -> Symbol {
    Symbol {
        file,
        kind,
        name: name.to_owned(),
        qualified_name: name.to_owned(),
        line,
        end_line: line,
        signature: truncate(signature),
    }
}

/// Takes the head of a definition: everything before the body or the statement end.
fn signature_text(source: &[u8], start: usize, end: usize) -> String {
    let raw = String::from_utf8_lossy(&source[start..end]);
    let head = raw.split(['{', ';']).next().unwrap_or_default();
    truncate(&collapse_whitespace(head))
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_SIGNATURE_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(MAX_SIGNATURE_CHARS).collect();
    format!("{head}…")
}

fn leading_identifier(text: &str) -> &str {
    text.split(|ch: char| {
        ch.is_whitespace() || matches!(ch, '(' | '=' | '<' | ':' | ';' | ',' | '{' | '[')
    })
    .next()
    .unwrap_or_default()
}

fn attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::scanner::{DEFAULT_MAX_FILE_SIZE, scan};

    fn symbols_of(name: &str, contents: &str) -> Vec<Symbol> {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(name), contents).expect("write");
        let scan = scan(dir.path(), DEFAULT_MAX_FILE_SIZE).expect("scan");
        extract(0, &scan.files[0])
    }

    #[test]
    fn extracts_rust_functions_methods_and_types() {
        let symbols = symbols_of(
            "lib.rs",
            "use std::path::Path;\n\npub struct Config { value: u8 }\n\nimpl Config {\n    pub fn new() -> Self { Self { value: 0 } }\n}\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();

        assert!(names.contains(&"Config"));
        assert!(names.contains(&"new"));
        assert!(names.iter().any(|name| name.contains("std::path::Path")));
        let new = symbols
            .iter()
            .find(|symbol| symbol.name == "new")
            .expect("new");
        assert_eq!(new.kind, SymbolKind::Function);
        assert_eq!(new.line, 6);
        assert!(!new.signature.contains('{'));
    }

    #[test]
    fn qualifies_methods_by_their_owner() {
        let symbols = symbols_of(
            "lib.rs",
            "struct A;\nimpl A {\n    fn new() -> Self { A }\n}\nstruct B;\nimpl B {\n    fn make() -> Self { B }\n}\nmod store {\n    pub struct Cart;\n    impl Cart {\n        pub fn new() -> Self { Cart }\n    }\n}\n",
        );
        let qualified: Vec<&str> = symbols
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect();

        assert!(qualified.contains(&"A::new"));
        assert!(qualified.contains(&"B::make"));
        assert!(qualified.contains(&"store"));
        assert!(qualified.contains(&"store::Cart"));
        assert!(qualified.contains(&"store::Cart::new"));
    }

    #[test]
    fn records_the_end_line_of_a_definition() {
        let symbols = symbols_of("lib.rs", "\nfn multi() {\n    let x = 1;\n}\n");
        let function = symbols
            .iter()
            .find(|symbol| symbol.name == "multi")
            .expect("multi");

        assert_eq!(function.line, 2);
        assert_eq!(function.end_line, 4);
    }

    #[test]
    fn extracts_css_selectors_and_custom_properties() {
        let symbols = symbols_of(
            "style.css",
            ":root {\n  --brand: #f00;\n}\n\n.btn, .btn-primary {\n  color: var(--brand);\n}\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();

        assert!(names.contains(&"--brand"));
        assert!(names.contains(&".btn, .btn-primary"));
        assert!(symbols.iter().any(|s| s.kind == SymbolKind::Variable));
    }

    #[test]
    fn extracts_top_level_script_declarations_only() {
        let symbols = symbols_of(
            "app.js",
            "const answer = 42;\nconst handler = () => {};\nexport class Store {}\nimport { x } from './x.js';\n\nfunction boot() {\n  const local = 1;\n}\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();

        assert!(names.contains(&"handler"));
        assert!(names.contains(&"Store"));
        assert!(names.contains(&"answer"));
        assert!(names.contains(&"boot"));
        assert!(!names.contains(&"local"));
    }

    #[test]
    fn extracts_generic_declarations() {
        let symbols = symbols_of(
            "main.go",
            "package main\n\nfunc Run() {\n}\n\ntype Server struct {\n    Name string\n}\n",
        );
        let names: Vec<&str> = symbols.iter().map(|item| item.name.as_str()).collect();

        assert!(names.contains(&"Run"));
        assert!(names.contains(&"Server"));
        assert_eq!(symbols[0].end_line, 4, "func span by braces");
    }

    #[test]
    fn extracts_python_classes_and_methods() {
        let symbols = symbols_of(
            "app.py",
            "class Service:\n    def run(self):\n        return 1\n\n    async def stop(self):\n        pass\n\ndef main():\n    pass\n",
        );
        let names: Vec<&str> = symbols
            .iter()
            .map(|item| item.qualified_name.as_str())
            .collect();

        assert!(names.contains(&"Service"));
        assert!(names.contains(&"Service.run"));
        assert!(names.contains(&"Service.stop"));
        assert!(names.contains(&"main"));
        assert_eq!(symbols[0].end_line, 6, "class spans its methods");
    }

    #[test]
    fn extracts_markup_landmarks_only() {
        let symbols = symbols_of(
            "index.html",
            "<header id=\"top\">\n<div class=\"noise\">\n<section class=\"hero main\">\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();

        assert!(names.contains(&"header#top"));
        assert!(names.contains(&"section.hero"));
        assert!(!names.iter().any(|name| name.starts_with("div")));
    }

    #[test]
    fn extracts_markdown_headings() {
        let symbols = symbols_of("README.md", "# Title\n\ntext\n\n## Install\n");
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();

        assert_eq!(names, vec!["Title", "Install"]);
    }

    #[test]
    fn caps_signature_length() {
        let long = "a".repeat(400);
        assert_eq!(truncate(&long).chars().count(), MAX_SIGNATURE_CHARS + 1);
    }
}
