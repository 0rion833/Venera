//! The retrieval engine: one object that owns the database, the path sandbox, and every
//! read the model is allowed to make (`TOOL.md` §10, §11).
//!
//! Every method is deterministic and works offline. Nothing here calls a language model;
//! `venera-tools` only wraps these calls in schemas.
//!
//! Source reads are verified against the stored hash before they are returned. A file that
//! changed on disk is re-indexed on the spot, so the model never receives cached code
//! presented as current.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::security::path_guard::{GuardError, PathGuard};
use crate::vce::index::m1::{FileRow, Store, StoreError, SymbolRow};
use crate::vce::index::map::{self, MapOptions, ProjectIndex, ProjectMap};
use crate::vce::index::rank;
use crate::vce::index::refresh::{self, IndexReport, RefreshError, RefreshOptions};

/// A retrieval failure.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The path escaped the project root.
    #[error(transparent)]
    Guard(#[from] GuardError),

    /// The database rejected a read or write.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The index could not be refreshed.
    #[error(transparent)]
    Refresh(#[from] RefreshError),

    /// A file could not be read.
    #[error("cannot read `{path}`: {source}")]
    Io {
        /// Project-relative path.
        path: String,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },

    /// The file is not on disk.
    #[error("`{path}` does not exist inside the project")]
    Missing {
        /// Project-relative path.
        path: String,
    },

    /// The file has never been indexed.
    #[error("`{path}` is not in the index; run `venera index` first")]
    NotIndexed {
        /// Project-relative path.
        path: String,
    },
}

/// A file row plus its symbol count, as returned by search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    /// Project-relative path.
    pub path: String,
    /// Detected language.
    pub language: String,
    /// Symbols stored for the file.
    pub symbols: usize,
}

/// A symbol as returned by search, outline, or reference lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolHit {
    /// Owning file.
    pub path: String,
    /// Name as written.
    pub name: String,
    /// Owner-qualified name.
    pub qualified_name: String,
    /// Short kind name.
    pub kind: String,
    /// One-line signature.
    pub signature: String,
    /// First line, one-based.
    pub start_line: u32,
    /// Last line, one-based.
    pub end_line: u32,
}

/// One dependency edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Imported path, or the raw import name when it points outside the project.
    pub path: String,
    /// How many hops away.
    pub depth: u32,
    /// Whether the target is a project file.
    pub resolved: bool,
}

/// Imports of a file and the files that import it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependencies {
    /// The file asked about.
    pub path: String,
    /// Outgoing edges, nearest first.
    pub imports: Vec<Dependency>,
    /// Files that import this one.
    pub imported_by: Vec<String>,
}

/// A verified slice of source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceView {
    /// Project-relative path.
    pub path: String,
    /// BLAKE3 of the content that was read, not the content that was indexed.
    pub content_hash: String,
    /// First returned line, one-based.
    pub start_line: u32,
    /// Last returned line, one-based.
    pub end_line: u32,
    /// Lines in the file.
    pub total_lines: u32,
    /// Whether the slice is the whole file.
    pub complete: bool,
    /// Whether the index row was refreshed because the file had changed.
    pub refreshed: bool,
    /// The requested text.
    pub text: String,
}

/// The context engine.
pub struct Engine {
    root: PathBuf,
    guard: PathGuard,
    store: Store,
}

impl Engine {
    /// Opens (or creates) the index database under `root`.
    ///
    /// `database` is relative to the project root unless it is absolute.
    pub fn open(root: impl AsRef<Path>, database: &str) -> Result<Self, EngineError> {
        let guard = PathGuard::new(root)?;
        let root = guard.root().to_path_buf();
        let database = PathBuf::from(database);
        let path = if database.is_absolute() {
            database
        } else {
            root.join(database)
        };

        Ok(Self {
            root,
            guard,
            store: Store::open(path)?,
        })
    }

    /// Project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path sandbox.
    pub fn guard(&self) -> &PathGuard {
        &self.guard
    }

    /// The database.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Brings the index in line with the disk.
    pub fn refresh(&self, options: &RefreshOptions) -> Result<IndexReport, EngineError> {
        Ok(refresh::refresh(&self.store, &self.root, options)?)
    }

    /// Renders the budgeted project map, optionally ordered by task relevance.
    pub fn map(&self, options: &MapOptions, task: Option<&str>) -> Result<ProjectMap, EngineError> {
        let index = ProjectIndex::from_store(&self.store)?;
        let order = match task {
            Some(task) => self
                .ranked_order(task)?
                .unwrap_or_else(|| index.file_order.clone()),
            None => index.file_order.clone(),
        };

        Ok(map::render(&index, options, &order))
    }

    /// Files whose path contains `query`.
    ///
    /// A miss means the query may name a file created after the last index refresh,
    /// so one incremental refresh is attempted before answering "no files".
    pub fn search_files(&self, query: &str, limit: usize) -> Result<Vec<FileHit>, EngineError> {
        let mut rows = self.store.search_files(query, limit)?;
        if rows.is_empty() {
            self.refresh(&RefreshOptions::default())?;
            rows = self.store.search_files(query, limit)?;
        }

        let mut hits = Vec::new();
        for file in rows {
            let symbols = self.store.symbols_of_file(file.id)?.len();
            hits.push(FileHit {
                path: file.path,
                language: file.language,
                symbols,
            });
        }
        Ok(hits)
    }

    /// Symbols whose name, qualified name, or signature matches `query`.
    pub fn search_symbols(&self, query: &str, limit: usize) -> Result<Vec<SymbolHit>, EngineError> {
        self.hit_rows(self.store.search_symbols(query, limit)?)
    }

    /// Signatures of every symbol of one file, without reading the file.
    pub fn outline(&self, path: &str) -> Result<String, EngineError> {
        let file = self.stored_file(path)?;
        let symbols = self.store.symbols_of_file(file.id)?;

        if symbols.is_empty() {
            return Ok(format!(
                "{}: no symbols indexed ({})",
                file.path, file.parser_status
            ));
        }

        let mut text = format!("{} ({} symbols)\n", file.path, symbols.len());
        for symbol in &symbols {
            text.push_str(&format!(
                "  {}-{} {}\n",
                symbol.start_line, symbol.end_line, symbol.signature
            ));
        }
        Ok(text)
    }

    /// Reads a verified range of source, refreshing the index row when the file changed.
    pub fn source(
        &self,
        path: &str,
        start: Option<u32>,
        end: Option<u32>,
    ) -> Result<SourceView, EngineError> {
        let mut refreshed = false;
        let file = match self.store.file(path)? {
            Some(file) => file,
            None => {
                let absolute = self.guard.resolve(path).map_err(|_| EngineError::Missing {
                    path: path.to_owned(),
                })?;
                if !absolute.is_file() {
                    return Err(EngineError::Missing {
                        path: path.to_owned(),
                    });
                }
                let rel_path = self
                    .guard
                    .relative(&absolute)
                    .unwrap_or_else(|| path.to_owned());
                if !refresh::reindex_file(&self.store, &self.root, &rel_path)? {
                    return Err(EngineError::NotIndexed {
                        path: path.to_owned(),
                    });
                }
                refreshed = true;
                self.stored_file(&rel_path)?
            }
        };
        let absolute = self.guard.resolve(path).map_err(|_| EngineError::Missing {
            path: path.to_owned(),
        })?;

        let bytes = std::fs::read(&absolute).map_err(|source| EngineError::Io {
            path: path.to_owned(),
            source,
        })?;
        let content_hash = blake3::hash(&bytes).to_hex().to_string();

        if file.content_hash != content_hash {
            refreshed = refresh::reindex_file(&self.store, &self.root, &file.path)?;
        }

        let text = String::from_utf8_lossy(&bytes).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len() as u32;

        if total_lines == 0 {
            return Ok(SourceView {
                path: path.to_owned(),
                content_hash,
                start_line: 0,
                end_line: 0,
                total_lines: 0,
                complete: true,
                refreshed,
                text: String::new(),
            });
        }

        let start_line = start.unwrap_or(1).clamp(1, total_lines);
        let end_line = end.unwrap_or(total_lines).clamp(start_line, total_lines);
        let slice = lines[(start_line - 1) as usize..end_line as usize].join("\n");

        Ok(SourceView {
            path: path.to_owned(),
            content_hash,
            start_line,
            end_line,
            total_lines,
            complete: start_line == 1 && end_line == total_lines,
            refreshed,
            text: slice,
        })
    }

    /// Declarations that mention `name`, as lexical evidence, not a call graph.
    pub fn references(&self, name: &str, limit: usize) -> Result<Vec<SymbolHit>, EngineError> {
        self.hit_rows(self.store.references_to(name, limit)?)
    }

    /// Imports of a file and the files that import it, up to `depth` hops.
    pub fn dependencies(&self, path: &str, depth: u32) -> Result<Dependencies, EngineError> {
        let file = self.stored_file(path)?;
        let depth = depth.max(1);

        let mut visited: HashSet<i64> = HashSet::from([file.id]);
        let mut frontier = vec![(file.id, 0u32)];
        let mut imports = Vec::new();

        while let Some((file_id, current)) = frontier.pop() {
            if current >= depth {
                continue;
            }

            for (name, target, _line) in self.store.imports_from(file_id)? {
                match target {
                    Some(target_id) => {
                        let Some(row) = self.store.file_by_id(target_id)? else {
                            continue;
                        };
                        imports.push(Dependency {
                            path: row.path.clone(),
                            depth: current + 1,
                            resolved: true,
                        });
                        if visited.insert(target_id) {
                            frontier.push((target_id, current + 1));
                        }
                    }
                    None => imports.push(Dependency {
                        path: name,
                        depth: current + 1,
                        resolved: false,
                    }),
                }
            }
        }

        imports.sort_by(|left, right| {
            left.depth
                .cmp(&right.depth)
                .then_with(|| left.path.cmp(&right.path))
        });

        let mut imported_by = Vec::new();
        for id in self.store.imported_by(file.id)? {
            if let Some(row) = self.store.file_by_id(id)? {
                imported_by.push(row.path);
            }
        }
        imported_by.sort();

        Ok(Dependencies {
            path: file.path,
            imports,
            imported_by,
        })
    }

    /// File rows with symbol counts, for reports.
    pub fn files(&self) -> Result<Vec<FileHit>, EngineError> {
        let mut hits = Vec::new();
        for file in self.store.files()? {
            let symbols = self.store.symbols_of_file(file.id)?.len();
            hits.push(FileHit {
                path: file.path,
                language: file.language,
                symbols,
            });
        }
        Ok(hits)
    }

    fn stored_file(&self, path: &str) -> Result<FileRow, EngineError> {
        self.store
            .file(path)?
            .ok_or_else(|| EngineError::NotIndexed {
                path: path.to_owned(),
            })
    }

    fn hit_rows(&self, rows: Vec<SymbolRow>) -> Result<Vec<SymbolHit>, EngineError> {
        let mut paths: HashMap<i64, String> = HashMap::new();
        let mut hits = Vec::with_capacity(rows.len());

        for row in rows {
            let path = match paths.get(&row.file_id) {
                Some(path) => path.clone(),
                None => {
                    let file = self.store.file_by_id(row.file_id)?.ok_or_else(|| {
                        EngineError::NotIndexed {
                            path: format!("<file {}>", row.file_id),
                        }
                    })?;
                    paths.insert(row.file_id, file.path.clone());
                    file.path
                }
            };

            hits.push(SymbolHit {
                path,
                name: row.name,
                qualified_name: row.qualified_name,
                kind: row.kind,
                signature: row.signature,
                start_line: row.start_line,
                end_line: row.end_line,
            });
        }

        Ok(hits)
    }

    /// File order for a task, or `None` when nothing matches.
    ///
    /// Ranking from the database uses paths, symbol names, signatures, and imports; it
    /// does not read bodies, because reading them is exactly the cost the index exists to
    /// avoid.
    fn ranked_order(&self, task: &str) -> Result<Option<Vec<usize>>, EngineError> {
        let Some(query) = rank::Query::new(task) else {
            return Ok(None);
        };

        let rows = self.store.files()?;
        let files: Vec<String> = rows.iter().map(|row| row.path.clone()).collect();
        let index_of: HashMap<i64, usize> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.id, index))
            .collect();

        let mut names = vec![Vec::new(); files.len()];
        let mut qualified = vec![Vec::new(); files.len()];
        let mut signatures = vec![Vec::new(); files.len()];

        for row in self.store.symbols()? {
            let Some(&file_index) = index_of.get(&row.file_id) else {
                continue;
            };
            names[file_index].push(row.name);
            qualified[file_index].push(row.qualified_name);
            signatures[file_index].push(row.signature);
        }

        let mut imports_per_file = vec![Vec::new(); files.len()];
        for (index, row) in rows.iter().enumerate() {
            imports_per_file[index] = self
                .store
                .imports_from(row.id)?
                .into_iter()
                .map(|(name, _, _)| name)
                .collect();
        }

        let mentions = self.store.mentions()?;
        let scores: Vec<f64> = (0..files.len())
            .map(|index| {
                let signals = rank::Signals {
                    path: &files[index],
                    names: &names[index],
                    qualified_names: &qualified[index],
                    signatures: &signatures[index],
                    imports: &imports_per_file[index],
                    body: None,
                    mentions: &mentions,
                };
                signals.score(&query)
            })
            .collect();

        if scores.iter().all(|score| *score <= 0.0) {
            return Ok(None);
        }

        Ok(Some(rank::order(&files, |index| scores[index])))
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Engine")
            .field("root", &self.root)
            .field("database", &self.store.path())
            .finish()
    }
}

/// Re-exported so callers do not depend on `index::refresh` directly.
pub use refresh::IndexReport as Report;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }

        let engine = Engine::open(dir.path(), ".venera/index.db").expect("engine");
        engine.refresh(&RefreshOptions::default()).expect("refresh");
        (dir, engine)
    }

    fn site() -> (tempfile::TempDir, Engine) {
        project(&[
            (
                "src/main.rs",
                "use crate::auth::middleware;\n\nfn main() {\n    middleware::guard();\n}\n",
            ),
            (
                "src/auth/middleware.rs",
                "pub fn guard() {\n    check();\n}\n\nfn check() {}\n",
            ),
            (
                "static/script.js",
                "import { boot } from './boot.js';\nconst button = document.querySelector('button');\n",
            ),
            ("static/boot.js", "export function boot() {}\n"),
            ("style/style.css", "button { color: red; }\n"),
        ])
    }

    #[test]
    fn searches_files_and_symbols() {
        let (_dir, engine) = site();

        let files = engine.search_files("auth", 10).expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/auth/middleware.rs");

        let symbols = engine.search_symbols("guard", 10).expect("symbols");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].qualified_name, "guard");
        assert_eq!(symbols[0].path, "src/auth/middleware.rs");

        assert!(
            engine
                .search_symbols("zzz", 10)
                .expect("symbols")
                .is_empty()
        );
    }

    #[test]
    fn outlines_a_file_without_reading_it() {
        let (_dir, engine) = site();
        let outline = engine.outline("src/auth/middleware.rs").expect("outline");

        assert!(outline.contains("middleware.rs (2 symbols)"));
        assert!(outline.contains("1-3 pub fn guard()"));
        assert!(outline.contains("5-5 fn check()"));
        assert!(engine.outline("src/missing.rs").is_err());
    }

    #[test]
    fn returns_a_requested_range_and_reports_completeness() {
        let (_dir, engine) = site();

        let view = engine
            .source("src/auth/middleware.rs", Some(2), Some(3))
            .expect("source");
        assert_eq!(view.start_line, 2);
        assert_eq!(view.end_line, 3);
        assert!(!view.complete);
        assert_eq!(view.text, "    check();\n}");

        let whole = engine
            .source("src/auth/middleware.rs", None, None)
            .expect("source");
        assert!(whole.complete);
        assert_eq!(whole.total_lines, 5);

        let clamped = engine
            .source("src/auth/middleware.rs", Some(4), Some(999))
            .expect("source");
        assert_eq!((clamped.start_line, clamped.end_line), (4, 5));
    }

    #[test]
    fn a_changed_file_is_reindexed_before_it_is_returned() {
        let (dir, engine) = site();

        std::fs::write(
            dir.path().join("src/auth/middleware.rs"),
            "pub fn guard_v2() {\n    check();\n}\n",
        )
        .expect("rewrite");

        let view = engine
            .source("src/auth/middleware.rs", None, None)
            .expect("source");
        assert!(view.refreshed);
        assert!(view.text.contains("guard_v2"));

        let symbols = engine.search_symbols("guard_v2", 10).expect("symbols");
        assert_eq!(symbols.len(), 1, "the index row must be refreshed too");
    }

    #[test]
    fn walks_dependencies_in_both_directions() {
        let (_dir, engine) = site();

        let deps = engine.dependencies("src/main.rs", 2).expect("deps");
        assert!(
            deps.imports
                .iter()
                .any(|import| import.path == "src/auth/middleware.rs" && import.depth == 1)
        );

        let reverse = engine
            .dependencies("src/auth/middleware.rs", 1)
            .expect("deps");
        assert_eq!(reverse.imported_by, vec!["src/main.rs".to_owned()]);
    }

    #[test]
    fn reports_unresolved_imports_instead_of_hiding_them() {
        let (_dir, engine) = project(&[("src/main.rs", "use serde::Serialize;\nfn main() {}\n")]);

        let deps = engine.dependencies("src/main.rs", 1).expect("deps");
        assert!(
            deps.imports
                .iter()
                .any(|import| import.path == "serde::Serialize" && !import.resolved)
        );
    }

    #[test]
    fn ranks_files_by_a_russian_task() {
        let (_dir, engine) = project(&[
            ("src/buttons.rs", "pub fn draw_button() {}\n"),
            ("src/database.rs", "pub fn connect() {}\n"),
            ("README.md", "# Кнопки\n"),
        ]);

        let map = engine
            .map(
                &MapOptions {
                    max_tokens: 200,
                    ..MapOptions::default()
                },
                Some("сделай кнопку красной"),
            )
            .expect("map");

        let first_line = map.text.lines().next().unwrap_or_default();
        assert!(
            first_line.contains("buttons") || first_line.contains("README"),
            "map starts with a file that matches the task: {first_line}"
        );
    }

    #[test]
    fn builds_a_map_from_the_database() {
        let (_dir, engine) = site();
        let map = engine.map(&MapOptions::default(), None).expect("map");

        assert!(map.files >= 5);
        assert!(map.tokens <= 1000);
        assert!(map.text.contains("src/auth/middleware.rs"));
    }

    #[test]
    fn unknown_paths_are_reported_and_new_files_are_read() {
        let (dir, engine) = site();

        assert!(matches!(
            engine.outline("src/never-seen.rs"),
            Err(EngineError::NotIndexed { .. })
        ));

        std::fs::write(dir.path().join("src/fresh.rs"), "pub fn fresh() {}\n").expect("write");
        let view = engine
            .source("src/fresh.rs", None, None)
            .expect("a file created after the index must still be readable");
        assert_eq!(view.text, "pub fn fresh() {}");
        assert!(view.refreshed, "the on-demand read must populate the index");
    }

    #[test]
    fn search_refreshes_before_reporting_no_files() {
        let (dir, engine) = site();
        std::fs::write(dir.path().join("test.txt"), "hello\n").expect("write");

        let files = engine.search_files("test.txt", 10).expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "test.txt");
    }

    #[test]
    fn dependencies_are_deep_and_deterministic() {
        let (_dir, engine) = project(&[
            ("src/a.rs", "use crate::b;\npub fn a() {}\n"),
            ("src/b.rs", "use crate::c;\npub fn b() {}\n"),
            ("src/c.rs", "pub fn c() {}\n"),
        ]);

        let twice = engine.dependencies("src/a.rs", 2).expect("deps");
        let again = engine.dependencies("src/a.rs", 2).expect("deps");
        assert_eq!(twice, again);

        let paths: BTreeSet<(u32, String)> = twice
            .imports
            .iter()
            .map(|import| (import.depth, import.path.clone()))
            .collect();
        assert!(paths.contains(&(1, "src/b.rs".to_owned())));
        assert!(paths.contains(&(2, "src/c.rs".to_owned())));
    }
}
