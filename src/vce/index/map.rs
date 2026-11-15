//! Project map: the compact, budgeted overview the model sees instead of the repo.
//!
//! The map is built deterministically: symbols come from [`crate::vce::index::signatures`],
//! importance comes from how often a name is referenced elsewhere in the project, and
//! rendering stops when the token budget is reached. Nothing is invented and nothing is
//! silently dropped — omitted items are counted and reported (`TOOL.md` §8, §9).
//!
//! Ranking is a cheap stand-in for PageRank: a name referenced from many files is more
//! likely to matter than one referenced from none. It is a hint for navigation, not a
//! claim about runtime behaviour.
//!
//! Imports are left out of the map. They are cheap to see when a file is read and they
//! otherwise dominate the ranking, because `std` and `crate` names are referenced
//! everywhere.

use std::collections::HashMap;

use crate::config::Encoding;
use crate::provider::tokenizer;
use crate::vce::index::imports;
use crate::vce::index::m1::{Store, StoreError};
use crate::vce::index::scanner::Scan;
use crate::vce::index::signatures::{self, Symbol, SymbolKind};
use crate::vce::index::text;

/// Tokens reserved for the "omitted" footer.
const FOOTER_RESERVE: usize = 16;

/// How many times a render retries when cross-line token merging overshoots the budget.
const RENDER_ATTEMPTS: usize = 3;

/// Share of the budget file headers may take; the rest is kept for symbols.
const HEADER_DIVISOR: usize = 2;

/// Options for [`render`].
#[derive(Debug, Clone, Copy)]
pub struct MapOptions {
    /// Hard token budget for the rendered map.
    pub max_tokens: usize,
    /// Encoding used for counting.
    pub encoding: Encoding,
    /// Symbols shown per file; `0` means unlimited.
    pub max_symbols_per_file: usize,
}

impl Default for MapOptions {
    fn default() -> Self {
        Self {
            max_tokens: 1000,
            encoding: Encoding::O200kBase,
            max_symbols_per_file: 40,
        }
    }
}

/// Symbols, weights, and the file order derived from them.
#[derive(Debug, Clone)]
pub struct ProjectIndex {
    /// Absolute-free, project-relative paths, in index order.
    pub files: Vec<String>,
    /// Every extracted symbol, imports excluded.
    pub symbols: Vec<Symbol>,
    /// Reference count per symbol, used for ranking.
    pub weights: Vec<usize>,
    /// Symbol indices grouped by file.
    pub by_file: Vec<Vec<usize>>,
    /// File indices ordered by importance.
    pub file_order: Vec<usize>,
}

/// How much one unit of graph centrality is worth next to one reference count.
///
/// Centrality from PageRank is a share of the graph (roughly `1 / files`), so it is
/// scaled by the file count; ten is enough to break ties between equally referenced
/// files without letting the graph outvote task relevance.
const CENTRALITY_WEIGHT: f64 = 10.0;

impl ProjectIndex {
    /// Extracts symbols and ranks files by how often their symbols are referenced.
    pub fn build(scan: &Scan) -> Self {
        let files: Vec<String> = scan
            .files
            .iter()
            .map(|file| file.rel_path.clone())
            .collect();
        let mut symbols = Vec::new();
        let mut by_file = vec![Vec::new(); scan.files.len()];

        for (file_index, file) in scan.files.iter().enumerate() {
            for symbol in signatures::extract(file_index, file) {
                if symbol.kind == SymbolKind::Import {
                    continue;
                }
                by_file[file_index].push(symbols.len());
                symbols.push(symbol);
            }
        }

        let counts = token_counts(scan);
        let weights: Vec<usize> = symbols
            .iter()
            .map(|symbol| mention_weight(&symbol.name, &counts))
            .collect();

        let mut scores = vec![0f64; scan.files.len()];
        for (index, symbol) in symbols.iter().enumerate() {
            scores[symbol.file] += weights[index] as f64;
        }

        let file_order = order_files(&files, &scores);

        Self {
            files,
            symbols,
            weights,
            by_file,
            file_order,
        }
    }

    /// Loads symbols, weights, and graph centrality from the persistent index.
    ///
    /// The database stores token counts per file, so the weights match what
    /// [`Self::build`] would compute from source — the map does not change depending on
    /// where it was rendered from.
    pub fn from_store(store: &Store) -> Result<Self, StoreError> {
        let rows = store.files()?;
        let files: Vec<String> = rows.iter().map(|row| row.path.clone()).collect();
        let index_of: HashMap<i64, usize> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.id, index))
            .collect();

        let mentions = store.mentions()?;
        let mut symbols = Vec::new();
        let mut by_file = vec![Vec::new(); files.len()];

        for row in store.symbols()? {
            let Some(&file_index) = index_of.get(&row.file_id) else {
                continue;
            };
            let Some(kind) = SymbolKind::from_name(&row.kind) else {
                continue;
            };

            by_file[file_index].push(symbols.len());
            symbols.push(Symbol {
                file: file_index,
                kind,
                name: row.name,
                qualified_name: row.qualified_name,
                line: row.start_line,
                end_line: row.end_line,
                signature: row.signature,
            });
        }

        let weights: Vec<usize> = symbols
            .iter()
            .map(|symbol| mention_weight(&symbol.name, &mentions))
            .collect();

        let mut scores = vec![0f64; files.len()];
        for (index, symbol) in symbols.iter().enumerate() {
            scores[symbol.file] += weights[index] as f64;
        }

        let edges: Vec<(usize, usize)> = store
            .edges()?
            .into_iter()
            .filter_map(|(source, target)| Some((*index_of.get(&source)?, *index_of.get(&target)?)))
            .collect();
        let centrality = imports::pagerank(files.len(), &edges, 0.85);
        let scale = files.len() as f64 * CENTRALITY_WEIGHT;
        for (file_index, rank) in centrality.iter().enumerate() {
            scores[file_index] += rank * scale;
        }

        let file_order = order_files(&files, &scores);

        Ok(Self {
            files,
            symbols,
            weights,
            by_file,
            file_order,
        })
    }

    /// Symbols of one file, most referenced first.
    pub fn ranked_symbols(&self, file_index: usize) -> Vec<usize> {
        let mut indices = self.by_file[file_index].clone();
        indices.sort_by(|left, right| {
            self.weights[*right]
                .cmp(&self.weights[*left])
                .then_with(|| self.symbols[*left].line.cmp(&self.symbols[*right].line))
        });
        indices
    }
}

/// Weight of a symbol name: its project-wide occurrence count minus its own definition.
fn mention_weight(name: &str, counts: &HashMap<String, u32>) -> usize {
    lookup_key(name)
        .and_then(|key| counts.get(key).copied())
        .map_or(0, |count| (count as usize).saturating_sub(1))
}

/// File indices by descending score, ties broken by path.
fn order_files(files: &[String], scores: &[f64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|left, right| {
        scores[*right]
            .total_cmp(&scores[*left])
            .then_with(|| files[*left].cmp(&files[*right]))
    });
    order
}

/// Rendered map plus the accounting needed to report truncation.
#[derive(Debug, Clone, Default)]
pub struct ProjectMap {
    /// The map text itself.
    pub text: String,
    /// Exact token count of [`Self::text`].
    pub tokens: usize,
    /// Symbols included.
    pub symbols: usize,
    /// Files included.
    pub files: usize,
    /// Symbols that did not fit.
    pub omitted_symbols: usize,
    /// Files whose header did not fit.
    pub omitted_files: usize,
}

/// Renders the map for `scan` inside the configured budget.
///
/// `order` is the file priority: [`ProjectIndex::file_order`] for a generic overview, or
/// a task-specific ranking. Every file should appear exactly once.
///
/// Selection is breadth-first on purpose. A flat "most referenced first" pass would let
/// one large file eat the whole budget and hide the rest of the project, which is the
/// opposite of what a map is for. Headers come first (half the budget), then one symbol
/// per file, then the remaining symbols by rank. Output is always grouped by file, so a
/// symbol never appears without the path it belongs to.
///
/// The rendered text never exceeds `max_tokens`. The footer that reports omissions is
/// sized after the selection and shortened to whatever budget is left, so a tiny budget
/// produces a tiny map instead of an overrun (`TOOL.md` §8, §9).
pub fn render(index: &ProjectIndex, options: &MapOptions, order: &[usize]) -> ProjectMap {
    let mut budget = options.max_tokens.saturating_sub(FOOTER_RESERVE);

    for _ in 0..RENDER_ATTEMPTS {
        let map = render_once(index, options, order, budget);
        if map.tokens <= options.max_tokens {
            return map;
        }

        // Cross-line token merging can add a token or two beyond the per-line sum.
        let overflow = map.tokens - options.max_tokens;
        match budget.checked_sub(overflow) {
            Some(smaller) if smaller < budget => budget = smaller,
            _ => break,
        }
    }

    // Nothing fits at all: report the omission counts and stay inside the budget.
    ProjectMap {
        text: String::new(),
        tokens: 0,
        symbols: 0,
        files: 0,
        omitted_symbols: index.symbols.len(),
        omitted_files: index.files.len(),
    }
}

/// One selection pass for an inner `budget` that already excludes the footer.
fn render_once(
    index: &ProjectIndex,
    options: &MapOptions,
    order: &[usize],
    budget: usize,
) -> ProjectMap {
    let header_budget = budget / HEADER_DIVISOR;
    let mut included = vec![false; index.files.len()];
    let mut selected: Vec<Vec<usize>> = vec![Vec::new(); index.files.len()];
    let mut used = 0usize;
    let mut files = 0usize;
    let mut symbols = 0usize;

    // Phase 1: one header per file, while the header share of the budget lasts.
    for file_index in order {
        let cost = tokenizer::count(options.encoding, &index.files[*file_index]) + 1;
        if used + cost > header_budget {
            continue;
        }
        used += cost;
        included[*file_index] = true;
        files += 1;
    }

    let cap = if options.max_symbols_per_file == 0 {
        usize::MAX
    } else {
        options.max_symbols_per_file
    };

    // Phase 2: one symbol per included file, so every file gets a face.
    for file_index in order {
        if !included[*file_index] || selected[*file_index].len() >= cap {
            continue;
        }
        if let Some(symbol_index) = index.ranked_symbols(*file_index).first().copied()
            && try_select(
                &mut used,
                budget,
                options.encoding,
                index,
                symbol_index,
                &mut selected[*file_index],
            )
        {
            symbols += 1;
        }
    }

    // Phase 3: the remaining symbols, most referenced first.
    for file_index in order {
        if !included[*file_index] {
            continue;
        }
        for symbol_index in index.ranked_symbols(*file_index).into_iter().skip(1) {
            if selected[*file_index].len() >= cap {
                break;
            }
            if try_select(
                &mut used,
                budget,
                options.encoding,
                index,
                symbol_index,
                &mut selected[*file_index],
            ) {
                symbols += 1;
            }
        }
    }

    let body = build_text(index, order, &included, &selected);
    let body_tokens = tokenizer::count(options.encoding, &body);
    let omitted_symbols = index.symbols.len().saturating_sub(symbols);
    let omitted_files = index.files.len().saturating_sub(files);
    let remaining = options.max_tokens.saturating_sub(body_tokens);
    let footer = footer_text(omitted_symbols, omitted_files, remaining, options.encoding);

    let mut text = body;
    text.push_str(&footer);

    ProjectMap {
        tokens: tokenizer::count(options.encoding, &text),
        text,
        symbols,
        files,
        omitted_symbols,
        omitted_files,
    }
}

/// The omission footer, shortened until it fits `budget` tokens.
///
/// Returns an empty string when even the shortest form does not fit: the map never
/// overruns the budget to say what it left out.
fn footer_text(
    omitted_symbols: usize,
    omitted_files: usize,
    budget: usize,
    encoding: Encoding,
) -> String {
    if omitted_symbols == 0 && omitted_files == 0 {
        return String::new();
    }

    let candidates = [
        format!("\n… {omitted_symbols} symbols and {omitted_files} files omitted (budget)"),
        format!("\n… {omitted_symbols} symbols, {omitted_files} files omitted"),
        "\n… omitted".to_owned(),
    ];

    candidates
        .into_iter()
        .find(|candidate| tokenizer::count(encoding, candidate) <= budget)
        .unwrap_or_default()
}

/// Adds one symbol to `chosen` if the budget still allows it.
fn try_select(
    used: &mut usize,
    budget: usize,
    encoding: Encoding,
    index: &ProjectIndex,
    symbol_index: usize,
    chosen: &mut Vec<usize>,
) -> bool {
    let symbol = &index.symbols[symbol_index];
    let cost = tokenizer::count(
        encoding,
        &format!("  {}: {}\n", symbol.line, symbol.signature),
    );
    if *used + cost > budget {
        return false;
    }
    *used += cost;
    chosen.push(symbol_index);
    true
}

/// Emits the map grouped by file, in priority order.
fn build_text(
    index: &ProjectIndex,
    order: &[usize],
    included: &[bool],
    selected: &[Vec<usize>],
) -> String {
    let mut text = String::new();

    for file_index in order {
        if !included[*file_index] {
            continue;
        }
        text.push_str(&index.files[*file_index]);
        text.push('\n');

        // Selection is by rank, display is by line: the map should read like an outline.
        let mut chosen = selected[*file_index].clone();
        chosen.sort_by_key(|symbol_index| index.symbols[*symbol_index].line);

        for symbol_index in &chosen {
            let symbol = &index.symbols[*symbol_index];
            text.push_str(&format!("  {}: {}\n", symbol.line, symbol.signature));
        }
    }

    text
}

/// Occurrences of every identifier-like token in the project.
fn token_counts(scan: &Scan) -> HashMap<String, u32> {
    let mut counts = HashMap::new();

    for file in &scan.files {
        text::add_name_counts(&file.text, &mut counts);
    }

    counts
}

/// The longest identifier-like run inside a display name.
///
/// A CSS selector such as `.btn, .btn-primary` is looked up as `btn-primary`, and
/// `header#top` as `header`.
fn lookup_key(name: &str) -> Option<&str> {
    name.split(|ch: char| !text::is_name_char(ch))
        .filter(|part| part.chars().count() >= text::MIN_TOKEN)
        .max_by_key(|part| part.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::scanner::{DEFAULT_MAX_FILE_SIZE, scan};

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Scan) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }
        let scan = scan(dir.path(), DEFAULT_MAX_FILE_SIZE).expect("scan");
        (dir, scan)
    }

    #[test]
    fn ranks_referenced_symbols_above_unused_ones() {
        let (_dir, scan) = project(&[
            (
                "core.rs",
                "pub fn used_everywhere() {}\npub fn never_called() {}\n",
            ),
            ("a.rs", "fn a() { used_everywhere(); }\n"),
            ("b.rs", "fn b() { used_everywhere(); }\n"),
        ]);
        let index = ProjectIndex::build(&scan);
        let core = scan
            .files
            .iter()
            .position(|file| file.rel_path == "core.rs")
            .expect("core.rs");

        let ranked = index.ranked_symbols(core);
        let first = &index.symbols[ranked[0]];

        assert_eq!(first.name, "used_everywhere");
    }

    #[test]
    fn stays_inside_the_token_budget() {
        let body: String = (0..500)
            .map(|n| format!("pub fn function_number_{n}() -> u32 {{ {n} }}\n"))
            .collect();
        let (_dir, scan) = project(&[("big.rs", &body)]);
        let index = ProjectIndex::build(&scan);
        let map = render(
            &index,
            &MapOptions {
                max_tokens: 200,
                ..MapOptions::default()
            },
            &index.file_order,
        );

        assert!(map.tokens <= 200, "map used {} tokens", map.tokens);
        assert!(map.omitted_symbols > 0);
        assert!(map.text.contains("omitted"));
    }

    #[test]
    fn output_is_deterministic() {
        let (_dir, scan) = project(&[
            ("b.rs", "pub fn beta() {}\n"),
            ("a.rs", "pub fn alpha() { beta(); }\n"),
        ]);
        let index = ProjectIndex::build(&scan);
        let options = MapOptions::default();

        assert_eq!(
            render(&index, &options, &index.file_order).text,
            render(&index, &options, &index.file_order).text
        );
    }

    #[test]
    fn renders_every_file_header_when_the_budget_allows() {
        let (_dir, scan) = project(&[
            ("style.css", ".btn { color: red; }\n"),
            ("index.html", "<h1 id=\"title\">Hi</h1>\n"),
        ]);
        let index = ProjectIndex::build(&scan);
        let map = render(&index, &MapOptions::default(), &index.file_order);

        assert!(map.text.contains("style.css"));
        assert!(map.text.contains("index.html"));
        assert_eq!(map.omitted_files, 0);
    }

    #[test]
    fn symbols_stay_under_their_file_header() {
        let (_dir, scan) = project(&[
            ("a.rs", "pub fn alpha() {}\npub fn beta() {}\n"),
            ("b.rs", "pub fn gamma() {}\n"),
        ]);
        let index = ProjectIndex::build(&scan);
        let map = render(&index, &MapOptions::default(), &index.file_order);
        let lines: Vec<&str> = map.text.lines().collect();

        let header = lines.iter().position(|line| *line == "a.rs").expect("a.rs");
        assert!(lines[header + 1].starts_with("  1: "));
        assert!(lines[header + 2].starts_with("  2: "));
    }

    #[test]
    fn imports_are_not_part_of_the_map() {
        let (_dir, scan) = project(&[(
            "a.rs",
            "use std::path::Path;\npub fn alpha(_path: &Path) {}\n",
        )]);
        let index = ProjectIndex::build(&scan);

        assert_eq!(index.symbols.len(), 1);
        assert_eq!(index.symbols[0].name, "alpha");
    }

    #[test]
    fn breadth_wins_over_one_huge_file() {
        let big: String = (0..300)
            .map(|n| format!("pub fn big_{n}() {{}}\n"))
            .collect();
        let (_dir, scan) = project(&[("big.rs", &big), ("small.rs", "pub fn small() {}\n")]);
        let index = ProjectIndex::build(&scan);
        let map = render(
            &index,
            &MapOptions {
                max_tokens: 200,
                ..MapOptions::default()
            },
            &index.file_order,
        );

        assert!(map.text.contains("small.rs"));
    }

    #[test]
    fn never_exceeds_a_tiny_budget() {
        let big: String = (0..200)
            .map(|n| format!("pub fn function_number_{n}() -> u32 {{ {n} }}\n"))
            .collect();
        let (_dir, scan) = project(&[("big.rs", &big), ("small.rs", "pub fn small() {}\n")]);
        let index = ProjectIndex::build(&scan);

        for max_tokens in 0..48 {
            let map = render(
                &index,
                &MapOptions {
                    max_tokens,
                    ..MapOptions::default()
                },
                &index.file_order,
            );

            assert!(
                map.tokens <= max_tokens,
                "budget {max_tokens}: map used {} tokens\n{}",
                map.tokens,
                map.text
            );
        }
    }

    #[test]
    fn caps_symbols_per_file() {
        let body: String = (0..50)
            .map(|n| format!("pub fn function_number_{n}() {{}}\n"))
            .collect();
        let (_dir, scan) = project(&[("big.rs", &body)]);
        let index = ProjectIndex::build(&scan);
        let map = render(
            &index,
            &MapOptions {
                max_tokens: 4000,
                max_symbols_per_file: 5,
                ..MapOptions::default()
            },
            &index.file_order,
        );

        assert_eq!(map.symbols, 5);
        assert_eq!(map.omitted_symbols, 45);
    }

    #[test]
    fn unlimited_symbols_per_file_when_capped_at_zero() {
        let body: String = (0..30)
            .map(|n| format!("pub fn function_number_{n}() {{}}\n"))
            .collect();
        let (_dir, scan) = project(&[("big.rs", &body)]);
        let index = ProjectIndex::build(&scan);
        let map = render(
            &index,
            &MapOptions {
                max_tokens: 4000,
                max_symbols_per_file: 0,
                ..MapOptions::default()
            },
            &index.file_order,
        );

        assert_eq!(map.symbols, 30);
        assert_eq!(map.omitted_symbols, 0);
    }
}
