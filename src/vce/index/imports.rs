//! Import graph: file -> file edges used for cheap reconnaissance.
//!
//! Lets the agent walk dependencies instead of grepping the whole tree, which is what
//! keeps the token budget under control.
//!
//! Extraction is deliberately lexical (`TOOL.md` §7: tree-sitter references are evidence,
//! not compiler-resolved calls). One declaration per line is enough for navigation:
//! a multi-line `use a::{ ... }` contributes its module path, not each item.
//!
//! Resolution only answers "is this target inside the project?". Anything else — a crate
//! from crates.io, `std`, an npm package — stays unresolved on purpose: pretending to
//! resolve it would invent edges that do not exist.

use crate::vce::index::language::Language;

/// One import declaration found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDecl {
    /// Imported module or path as written, without quotes or a trailing `;`.
    pub name: String,
    /// One-based line of the declaration.
    pub line: u32,
}

/// Collects import declarations for `language`.
pub fn extract(language: Language, text: &str) -> Vec<ImportDecl> {
    let mut imports = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_no = index as u32 + 1;
        let line = raw.trim();

        let name = match language {
            Language::Rust => rust_import(line),
            Language::JavaScript => script_import(line),
            Language::Python => python_import(line),
            Language::Generic => generic_import(line),
            _ => None,
        };

        if let Some(name) = name {
            imports.push(ImportDecl {
                name,
                line: line_no,
            });
        }
    }

    imports
}

/// `use a::b;` and `mod foo;`, with an optional `pub` / `pub(crate)` prefix.
fn rust_import(line: &str) -> Option<String> {
    let line = strip_visibility(line);
    let line = line.trim_start();

    if let Some(rest) = line.strip_prefix("use ") {
        let rest = rest.split(';').next().unwrap_or_default().trim();
        return head_of_use(rest);
    }

    // `mod foo;` declares a child module; `mod foo { .. }` is inline and imports nothing.
    if let Some(rest) = line.strip_prefix("mod ") {
        let name = rest
            .split(|ch: char| ch == ';' || ch == '{' || ch.is_whitespace())
            .next()?;
        if rest.contains('{') || name.is_empty() {
            return None;
        }
        return Some(name.to_owned());
    }

    None
}

fn strip_visibility(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else {
        return line;
    };
    // `pub`, `pub(crate)`, `pub(super)`, `pub(in path)`.
    if rest.starts_with('(')
        && let Some(close) = rest.find(')')
    {
        return rest[close + 1..].trim_start();
    }
    rest.trim_start()
}

/// Reduces `a::{b, c}` and `a::b as c` to their module path.
fn head_of_use(rest: &str) -> Option<String> {
    let cut = rest
        .find(['{', ' ', ','])
        .map(|index| &rest[..index])
        .unwrap_or(rest);
    let name = cut.trim_end_matches("::").trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// `import x from "./y"`, `import "./y"`, `export .. from "./y"`, `require("./y")`.
fn script_import(line: &str) -> Option<String> {
    if line.starts_with("//") || line.starts_with('*') {
        return None;
    }

    if line.starts_with("import ") || line.starts_with("export ") {
        if let Some(from) = line.find(" from ") {
            return quoted(&line[from + " from ".len()..]);
        }
        if let Some(rest) = line.strip_prefix("import ") {
            return quoted(rest);
        }
        return None;
    }

    if let Some(position) = line.find("require(") {
        return quoted(&line[position + "require(".len()..]);
    }

    None
}

/// `import a.b` and `from a.b import c` — the module path only.
fn python_import(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("from ") {
        let module = rest.split_whitespace().next()?;
        if module.is_empty() || module.starts_with('.') {
            return None;
        }
        return Some(module.trim_end_matches(',').to_owned());
    }

    let rest = line.strip_prefix("import ")?;
    let module = rest
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .next()?;

    if module.is_empty() {
        None
    } else {
        Some(module.to_owned())
    }
}

/// `#include`, `import`, `using`, and `use` across compiled languages.
fn generic_import(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("#include ") {
        return quoted_or_angle(rest);
    }

    if let Some(rest) = line.strip_prefix("import ") {
        return quoted(rest).or_else(|| bare_module(rest));
    }
    if let Some(rest) = line.strip_prefix("using ") {
        return bare_module(rest);
    }
    if let Some(rest) = line.strip_prefix("use ") {
        return bare_module(rest);
    }

    None
}

fn quoted_or_angle(text: &str) -> Option<String> {
    if let Some(rest) = text.strip_prefix('<') {
        let end = rest.find('>')?;
        let value = rest[..end].trim();
        return (!value.is_empty()).then(|| value.to_owned());
    }

    quoted(text)
}

fn bare_module(text: &str) -> Option<String> {
    let value = text
        .split(|ch: char| ch.is_whitespace() || ch == ';')
        .next()?
        .trim_end_matches([';', ',']);

    (!value.is_empty()).then(|| value.to_owned())
}

/// The first quoted string in `text`.
fn quoted(text: &str) -> Option<String> {
    let quote = text.find(['"', '\''])?;
    let rest = &text[quote + 1..];
    let end = rest.find(['"', '\''])?;
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// Resolves `name` to a project-relative path, when the target is inside the project.
///
/// `files` must be sorted by path — that is the order [`crate::vce::index::m1::Store::files`]
/// returns, and the binary search relies on it.
pub fn resolve(name: &str, source_path: &str, files: &[String]) -> Option<String> {
    debug_assert!(
        files.windows(2).all(|window| window[0] <= window[1]),
        "file list must be sorted by path"
    );

    let candidates = candidates(name, source_path);

    candidates.into_iter().find(|candidate| {
        files
            .binary_search_by(|file| file.as_str().cmp(candidate.as_str()))
            .is_ok()
    })
}

/// Candidate project paths for an import, most specific first.
fn candidates(name: &str, source_path: &str) -> Vec<String> {
    let source_dir = source_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or_default();

    if name.starts_with('.') {
        return relative_candidates(source_dir, name);
    }

    if name.contains('/') && !name.contains("::") {
        // A bare JS specifier such as `react` or `@scope/pkg` points outside the project.
        return Vec::new();
    }

    rust_candidates(name, source_path)
}

fn relative_candidates(source_dir: &str, specifier: &str) -> Vec<String> {
    let Some(base) = join_relative(source_dir, specifier) else {
        return Vec::new();
    };

    const EXTENSIONS: &[&str] = &[".js", ".ts", ".jsx", ".tsx", ".mjs", ".cjs"];

    let mut candidates = vec![base.clone()];
    for extension in EXTENSIONS {
        candidates.push(format!("{base}{extension}"));
    }
    for extension in EXTENSIONS {
        candidates.push(format!("{base}/index{extension}"));
    }
    candidates
}

fn rust_candidates(name: &str, source_path: &str) -> Vec<String> {
    let module_base = module_base(source_path);
    let mut segments: Vec<&str> = name.split("::").filter(|part| !part.is_empty()).collect();
    let mut base = String::new();
    let mut relative = false;

    while let Some(first) = segments.first().copied() {
        match first {
            "crate" => {
                segments.remove(0);
                base = crate_root(source_path).to_owned();
            }
            "self" => {
                segments.remove(0);
                base.clone_from(&module_base);
                relative = true;
            }
            "super" => {
                segments.remove(0);
                base = parent_dir(if relative { &base } else { &module_base });
                relative = true;
            }
            "std" | "core" | "alloc" | "proc_macro" => return Vec::new(),
            _ => break,
        }
    }

    if segments.is_empty() {
        return Vec::new();
    }

    // `mod foo;` is a single segment relative to the module that declares it.
    if segments.len() == 1 && !name.contains("::") {
        let module = segments[0];
        return vec![
            join(&module_base, &format!("{module}.rs")),
            join(&module_base, &format!("{module}/mod.rs")),
        ];
    }

    let mut candidates = Vec::new();
    for take in (1..=segments.len()).rev() {
        let path = segments[..take].join("/");
        candidates.push(join(&base, &format!("{path}.rs")));
        candidates.push(join(&base, &format!("{path}/mod.rs")));
    }
    candidates
}

/// Directory that holds the child modules of `source_path`.
///
/// Rust 2018: a crate root (`main.rs`, `lib.rs`, `mod.rs`) owns its siblings,
/// `src/auth.rs` owns `src/auth/`.
fn module_base(source_path: &str) -> String {
    let (directory, file) = match source_path.rsplit_once('/') {
        Some((directory, file)) => (directory, file),
        None => ("", source_path),
    };
    let stem = file.strip_suffix(".rs").unwrap_or(file);

    if matches!(stem, "main" | "lib" | "mod") {
        directory.to_owned()
    } else {
        join(directory, stem)
    }
}

/// Crate root directory: `src` for the usual layout, the project root otherwise.
fn crate_root(source_path: &str) -> &str {
    if source_path.starts_with("src/") {
        "src"
    } else {
        ""
    }
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(dir, _)| dir.to_owned())
        .unwrap_or_default()
}

fn join(base: &str, tail: &str) -> String {
    if base.is_empty() {
        tail.to_owned()
    } else {
        format!("{base}/{tail}")
    }
}

/// Joins a relative specifier onto a directory, refusing to climb above the root.
fn join_relative(base: &str, specifier: &str) -> Option<String> {
    let mut parts: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };

    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// PageRank over `n` files with directed edges `(source, target)`.
///
/// Used as a tie-breaker when ranking files: a module many files depend on is a more
/// useful thing to show than an isolated one. Damping 0.85, 20 iterations, fixed start
/// vector — the result is deterministic for identical input.
pub fn pagerank(n: usize, edges: &[(usize, usize)], damping: f64) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }

    let mut outgoing = vec![Vec::new(); n];
    for (source, target) in edges {
        if *source < n && *target < n {
            outgoing[*source].push(*target);
        }
    }

    let initial = 1.0 / n as f64;
    let mut rank = vec![initial; n];
    let mut next = vec![initial; n];

    for _ in 0..20 {
        let dangling: f64 = (0..n)
            .filter(|source| outgoing[*source].is_empty())
            .map(|source| rank[source])
            .sum();

        for (target, value) in next.iter_mut().enumerate() {
            let _ = target;
            *value = (1.0 - damping) / n as f64 + damping * dangling / n as f64;
        }

        for (source, targets) in outgoing.iter().enumerate() {
            if targets.is_empty() {
                continue;
            }
            let share = damping * rank[source] / targets.len() as f64;
            for target in targets {
                next[*target] += share;
            }
        }

        std::mem::swap(&mut rank, &mut next);
    }

    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_use_and_module_declarations() {
        let imports = extract(
            Language::Rust,
            "use std::path::Path;\npub use crate::auth::{User, Token};\nmod middleware;\nmod inline { }\nuse serde::Serialize as S;\n",
        );
        let names: Vec<(&str, u32)> = imports
            .iter()
            .map(|import| (import.name.as_str(), import.line))
            .collect();

        assert_eq!(
            names,
            vec![
                ("std::path::Path", 1),
                ("crate::auth", 2),
                ("middleware", 3),
                ("serde::Serialize", 5),
            ]
        );
    }

    #[test]
    fn extracts_javascript_imports() {
        let imports = extract(
            Language::JavaScript,
            "import x from './x.js';\nimport './side.css'\nexport { a } from '../lib/a';\nconst y = require('./y');\n// import nope from './nope'\n",
        );
        let names: Vec<&str> = imports.iter().map(|import| import.name.as_str()).collect();

        assert_eq!(names, vec!["./x.js", "./side.css", "../lib/a", "./y"]);
    }

    #[test]
    fn extracts_python_imports() {
        let imports = extract(
            Language::Python,
            "import os\nfrom app.services import event_service\nfrom . import local\n",
        );
        let names: Vec<&str> = imports.iter().map(|item| item.name.as_str()).collect();

        assert_eq!(names, vec!["os", "app.services"]);
    }

    #[test]
    fn extracts_generic_includes_and_imports() {
        let imports = extract(
            Language::Generic,
            "#include <vector>\n#include \"app.h\"\nimport java.util.List;\n",
        );
        let names: Vec<&str> = imports.iter().map(|item| item.name.as_str()).collect();

        assert_eq!(names, vec!["vector", "app.h", "java.util.List"]);
    }

    #[test]
    fn resolves_crate_paths_to_project_files() {
        let files = vec![
            "src/auth/middleware.rs".to_owned(),
            "src/auth/mod.rs".to_owned(),
            "src/main.rs".to_owned(),
        ];

        assert_eq!(
            resolve("crate::auth::middleware", "src/main.rs", &files).as_deref(),
            Some("src/auth/middleware.rs")
        );
        assert_eq!(
            resolve("crate::auth::middleware::Guard", "src/main.rs", &files).as_deref(),
            Some("src/auth/middleware.rs")
        );
    }

    #[test]
    fn resolves_relative_module_declarations() {
        let files = vec![
            "src/auth.rs".to_owned(),
            "src/auth/token.rs".to_owned(),
            "src/main.rs".to_owned(),
        ];

        assert_eq!(
            resolve("token", "src/auth.rs", &files).as_deref(),
            Some("src/auth/token.rs")
        );
        assert_eq!(
            resolve("auth", "src/main.rs", &files).as_deref(),
            Some("src/auth.rs")
        );
    }

    #[test]
    fn resolves_relative_scripts_and_rejects_packages() {
        let files = vec![
            "static/lib/index.js".to_owned(),
            "static/script.js".to_owned(),
            "static/util.js".to_owned(),
        ];

        assert_eq!(
            resolve("./util", "static/script.js", &files).as_deref(),
            Some("static/util.js")
        );
        assert_eq!(
            resolve("./lib", "static/script.js", &files).as_deref(),
            Some("static/lib/index.js")
        );
        assert_eq!(resolve("react", "static/script.js", &files), None);
        assert_eq!(resolve("std::fmt", "src/main.rs", &files), None);
        assert_eq!(resolve("../../../etc/passwd", "src/main.rs", &files), None);
    }

    #[test]
    fn pagerank_prefers_files_others_depend_on() {
        // 0 <- 1, 0 <- 2, 1 <- 2
        let edges = vec![(1, 0), (2, 0), (2, 1)];
        let rank = pagerank(3, &edges, 0.85);

        assert!(rank[0] > rank[1], "the shared module ranks above its user");
        assert!(rank[1] > rank[2], "the leaf ranks lowest");
        assert!((rank.iter().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pagerank_is_deterministic() {
        let edges = vec![(0, 1), (1, 2), (2, 0)];
        assert_eq!(pagerank(3, &edges, 0.85), pagerank(3, &edges, 0.85));
    }

    #[test]
    fn pagerank_handles_an_empty_project() {
        assert!(pagerank(0, &[], 0.85).is_empty());
    }
}
