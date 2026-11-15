//! Before/after accounting: how many tokens the tool actually saves.
//!
//! Three paths are compared on the same project and the same task:
//!
//! * **Read all** — an agent with no index reads every file. Upper bound of the waste.
//! * **Read top N** — an agent with no index lists the tree, guesses which files matter
//!   from the task text, and reads those. This is deliberately generous: it assumes a
//!   perfect guess, which a blind agent rarely makes.
//! * **Tool** — the project map plus the one file the map points at.
//!
//! Reporting both baselines matters: the map has a fixed cost, so on a small project it
//! can lose to reading five files directly. Showing where the break-even lies is more
//! useful than a single flattering percentage (`TOOL.md` §20).
//!
//! Everything is counted with the local tokenizer, so the numbers need no API key and no
//! network.

use crate::config::Encoding;
use crate::provider::tokenizer;
use crate::vce::index::map::{self, MapOptions, ProjectIndex, ProjectMap};
use crate::vce::index::scanner::{Scan, SourceFile};

/// Shortest task word treated as a keyword.
const MIN_KEYWORD: usize = 3;

/// Options for [`measure`].
#[derive(Debug, Clone)]
pub struct MeasureOptions {
    /// Encoding used for counting.
    pub encoding: Encoding,
    /// Budget for the rendered map.
    pub map_tokens: usize,
    /// Symbols shown per file in the map; `0` means unlimited.
    pub max_symbols_per_file: usize,
    /// RUB per 1M input tokens.
    pub rate_rub: f64,
    /// How many files the conservative baseline reads.
    pub baseline_top: usize,
    /// Task text used to rank files; without it, importance ranking stands in.
    pub task: Option<String>,
}

impl Default for MeasureOptions {
    fn default() -> Self {
        Self {
            encoding: Encoding::O200kBase,
            map_tokens: 1000,
            max_symbols_per_file: 40,
            rate_rub: 30.0,
            baseline_top: 5,
            task: None,
        }
    }
}

/// Result of one measurement.
#[derive(Debug, Clone)]
pub struct Measurement {
    /// Files in the project.
    pub files: usize,
    /// Symbols extracted across all files.
    pub symbols: usize,
    /// Tokens of the bare file listing.
    pub listing_tokens: usize,
    /// Tokens of listing plus every file read in full.
    pub all_tokens: usize,
    /// Tokens of listing plus the top `baseline_top` files.
    pub baseline_tokens: usize,
    /// Files the conservative baseline actually read.
    pub baseline_files: usize,
    /// The rendered map.
    pub map: ProjectMap,
    /// Tokens of the single file the map points at.
    pub retrieval_tokens: usize,
    /// Tokens the tool path spends: map plus retrieval.
    pub tool_tokens: usize,
    /// Token cost of each file in relevance order, used for the break-even point.
    pub ranked_file_tokens: Vec<usize>,
}

impl Measurement {
    /// Share saved against reading the whole project, in `0.0..=1.0`.
    pub fn savings_all_ratio(&self) -> f64 {
        ratio(self.all_tokens, self.tool_tokens)
    }

    /// Share saved against reading the top N files, in `0.0..=1.0`.
    ///
    /// Negative when the map costs more than the files it replaces.
    pub fn savings_ratio(&self) -> f64 {
        ratio(self.baseline_tokens, self.tool_tokens)
    }

    /// Cost of reading everything, in RUB at `rate_rub` per 1M tokens.
    pub fn all_rub(&self, rate_rub: f64) -> f64 {
        per_million(self.all_tokens, rate_rub)
    }

    /// Cost of the conservative baseline, in RUB.
    pub fn baseline_rub(&self, rate_rub: f64) -> f64 {
        per_million(self.baseline_tokens, rate_rub)
    }

    /// Cost of the tool path, in RUB.
    pub fn tool_rub(&self, rate_rub: f64) -> f64 {
        per_million(self.tool_tokens, rate_rub)
    }

    /// RUB saved against reading everything.
    pub fn saved_all_rub(&self, rate_rub: f64) -> f64 {
        self.all_rub(rate_rub) - self.tool_rub(rate_rub)
    }

    /// RUB saved against the conservative baseline.
    pub fn saved_rub(&self, rate_rub: f64) -> f64 {
        self.baseline_rub(rate_rub) - self.tool_rub(rate_rub)
    }

    /// How many files a blind agent has to read before the map pays for itself.
    ///
    /// This is the honest counterweight to a flattering percentage: the map has a fixed
    /// cost, so on a small project reading one or two files directly is cheaper.
    /// `None` means no number of files breaks even — the whole project is smaller than
    /// the map.
    pub fn break_even_files(&self) -> Option<usize> {
        let mut cumulative = self.listing_tokens;

        for (index, tokens) in self.ranked_file_tokens.iter().enumerate() {
            cumulative += tokens;
            if cumulative >= self.tool_tokens {
                return Some(index + 1);
            }
        }

        None
    }
}

fn ratio(reference: usize, actual: usize) -> f64 {
    if reference == 0 {
        return 0.0;
    }
    1.0 - actual as f64 / reference as f64
}

/// tokens / 1M * RUB per 1M tokens.
fn per_million(tokens: usize, rate_rub: f64) -> f64 {
    tokens as f64 / 1_000_000.0 * rate_rub
}

/// Builds the index, renders the map, and compares every path.
pub fn measure(scan: &Scan, options: &MeasureOptions) -> Measurement {
    let index = ProjectIndex::build(scan);

    let relevance = match options.task.as_deref() {
        Some(task) => rank_by_task(scan, task).unwrap_or_else(|| index.file_order.clone()),
        None => index.file_order.clone(),
    };

    let map = map::render(
        &index,
        &MapOptions {
            max_tokens: options.map_tokens,
            encoding: options.encoding,
            max_symbols_per_file: options.max_symbols_per_file,
        },
        &relevance,
    );

    let listing_tokens = tokenizer::count(
        options.encoding,
        &scan
            .files
            .iter()
            .map(|file| file.rel_path.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let all_tokens = listing_tokens
        + scan
            .files
            .iter()
            .map(|file| file_tokens(options.encoding, file))
            .sum::<usize>();

    let ranked_file_tokens: Vec<usize> = relevance
        .iter()
        .map(|file_index| file_tokens(options.encoding, &scan.files[*file_index]))
        .collect();

    let baseline_files = options.baseline_top.min(relevance.len());
    let baseline_tokens = listing_tokens
        + ranked_file_tokens
            .iter()
            .take(baseline_files)
            .sum::<usize>();

    let retrieval_tokens = ranked_file_tokens.first().copied().unwrap_or(0);

    let tool_tokens = map.tokens + retrieval_tokens;

    Measurement {
        files: scan.files.len(),
        symbols: index.symbols.len(),
        listing_tokens,
        all_tokens,
        baseline_tokens,
        baseline_files,
        map,
        retrieval_tokens,
        tool_tokens,
        ranked_file_tokens,
    }
}

/// Tokens of reading one file the way an agent would: a path header plus the body.
fn file_tokens(encoding: Encoding, file: &SourceFile) -> usize {
    tokenizer::count(encoding, &file.rel_path) + tokenizer::count(encoding, &file.text) + 1
}

/// Ranks files by how many task keywords appear in their path and text.
///
/// Returns `None` when no keyword matches anything, which happens whenever the task is
/// written in a different language than the code. Falling back to the importance order
/// is better than returning an alphabetical list, which would look like a real ranking.
///
/// Public because `venera map --task` reuses the same ordering, so the map and the
/// measurement never disagree about which file matters.
pub fn rank_by_task(scan: &Scan, task: &str) -> Option<Vec<usize>> {
    let keywords: Vec<String> = task
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.chars().count() >= MIN_KEYWORD)
        .map(str::to_owned)
        .collect();

    let mut scored: Vec<(usize, usize)> = (0..scan.files.len())
        .map(|file_index| {
            let file = &scan.files[file_index];
            let path = file.rel_path.to_ascii_lowercase();
            let text = file.text.to_ascii_lowercase();
            let score = keywords
                .iter()
                .map(|keyword| {
                    path.matches(keyword.as_str()).count() * 4
                        + text.matches(keyword.as_str()).count()
                })
                .sum();
            (file_index, score)
        })
        .collect();

    if scored.iter().all(|(_, score)| *score == 0) {
        return None;
    }

    scored.sort_by(|left, right| {
        right.1.cmp(&left.1).then_with(|| {
            scan.files[left.0]
                .rel_path
                .cmp(&scan.files[right.0].rel_path)
        })
    });

    Some(
        scored
            .into_iter()
            .map(|(file_index, _)| file_index)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::scanner::{DEFAULT_MAX_FILE_SIZE, scan};

    fn project(files: Vec<(String, String)>) -> (tempfile::TempDir, Scan) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(&name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }
        let scan = scan(dir.path(), DEFAULT_MAX_FILE_SIZE).expect("scan");
        (dir, scan)
    }

    /// A site-like tree: many small files, one stylesheet.
    fn site(files: usize, lines: usize) -> (tempfile::TempDir, Scan) {
        let mut entries = Vec::new();
        for file in 0..files {
            let body: String = (0..lines)
                .map(|line| {
                    format!("export function page{file}_row{line}() {{ return {line}; }}\n")
                })
                .collect();
            entries.push((format!("src/page_{file}.js"), body));
        }
        entries.push((
            "style/global.css".to_owned(),
            ".btn { color: red; }\n".to_owned(),
        ));
        project(entries)
    }

    #[test]
    fn the_tool_beats_reading_the_whole_project() {
        let (_dir, scan) = site(12, 20);
        let measurement = measure(&scan, &MeasureOptions::default());

        assert_eq!(measurement.files, 13);
        assert!(measurement.tool_tokens < measurement.all_tokens);
        assert!(measurement.savings_all_ratio() > 0.5);
    }

    #[test]
    fn the_tool_beats_a_conservative_baseline_on_a_realistic_project() {
        let (_dir, scan) = site(20, 30);
        let options = MeasureOptions {
            baseline_top: 10,
            ..MeasureOptions::default()
        };
        let measurement = measure(&scan, &options);

        assert!(measurement.tool_tokens < measurement.baseline_tokens);
        assert!(measurement.savings_ratio() > 0.3);
    }

    #[test]
    fn prices_every_path_at_the_given_rate() {
        let (_dir, scan) = site(12, 20);
        let measurement = measure(&scan, &MeasureOptions::default());

        let expected = measurement.baseline_tokens as f64 / 1_000_000.0 * 30.0;
        assert!((measurement.baseline_rub(30.0) - expected).abs() < 1e-12);
        assert!(measurement.saved_all_rub(30.0) > 0.0);
    }

    #[test]
    fn task_keywords_pull_the_matching_file_first() {
        let (_dir, scan) = project(vec![
            (
                "style/global.css".to_owned(),
                ".btn { color: red; }\n".to_owned(),
            ),
            (
                "src/app.js".to_owned(),
                "export function boot() {}\n".to_owned(),
            ),
        ]);
        let options = MeasureOptions {
            task: Some("why is the button colour wrong in global css".to_owned()),
            ..MeasureOptions::default()
        };
        let measurement = measure(&scan, &options);

        let best = rank_by_task(&scan, "button global css").expect("keywords match");
        assert_eq!(scan.files[best[0]].rel_path, "style/global.css");
        assert!(measurement.retrieval_tokens > 0);
    }

    #[test]
    fn handles_an_empty_project_without_dividing_by_zero() {
        let (_dir, scan) = project(Vec::new());
        let measurement = measure(&scan, &MeasureOptions::default());

        assert_eq!(measurement.files, 0);
        assert_eq!(measurement.all_tokens, 0);
        assert_eq!(measurement.savings_ratio(), 0.0);
        assert_eq!(measurement.savings_all_ratio(), 0.0);
        assert_eq!(measurement.break_even_files(), None);
    }

    #[test]
    fn reports_where_the_map_pays_for_itself() {
        let (_dir, scan) = site(20, 30);
        let measurement = measure(&scan, &MeasureOptions::default());
        let break_even = measurement.break_even_files().expect("break-even");

        let spent = |files: usize| {
            measurement.listing_tokens
                + measurement
                    .ranked_file_tokens
                    .iter()
                    .take(files)
                    .sum::<usize>()
        };

        assert!(break_even >= 1 && break_even <= measurement.files);
        assert!(spent(break_even) >= measurement.tool_tokens);
        assert!(spent(break_even - 1) < measurement.tool_tokens);
    }
}
