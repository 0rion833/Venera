//! Incremental indexing: compare the disk with the database and do only the work that
//! is actually needed (`TOOL.md` §5).
//!
//! Order of the check, from cheapest to most expensive: size and modification time from
//! the directory entry, then BLAKE3 of the content, then parsing. A file whose hash is
//! unchanged keeps its cached symbols; only genuinely new content is parsed.
//!
//! Bodies are never kept in memory beyond one file, so memory use does not grow with the
//! repository (`TOOL.md` §18).
//!
//! Limitation, stated instead of hidden: a change that preserves both size and
//! modification time is invisible to the hint and is picked up only by `--force`. That is
//! the price of not hashing every file on every run.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use thiserror::Error;

use crate::security::path_guard::{GuardError, PathGuard};
use crate::vce::index::ignore::IgnoreRules;
use crate::vce::index::imports;
use crate::vce::index::language::Language;
use crate::vce::index::m1::{
    FileMeta, NewImport, NewSymbol, PARSER_VERSION, Store, StoreError, now_unix,
};
use crate::vce::index::scanner::{self, SourceFile};
use crate::vce::index::signatures;
use crate::vce::index::text;

/// Options of one refresh.
#[derive(Debug, Clone, Copy)]
pub struct RefreshOptions {
    /// Files above this size are skipped.
    pub max_file_size: u64,
    /// Honour `.gitignore` and `.veneraignore`.
    pub respect_gitignore: bool,
    /// Reparse everything, ignoring the size/mtime hint and the hash cache.
    pub force: bool,
}

impl Default for RefreshOptions {
    fn default() -> Self {
        Self {
            max_file_size: scanner::DEFAULT_MAX_FILE_SIZE,
            respect_gitignore: true,
            force: false,
        }
    }
}

/// What one refresh did, used by `venera index` and the `refresh_index` tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// Files in the index afterwards.
    pub files: usize,
    /// Symbols in the index afterwards.
    pub symbols: usize,
    /// Resolved import edges in the index afterwards.
    pub imports: usize,
    /// New files.
    pub added: usize,
    /// Files whose content changed.
    pub modified: usize,
    /// Files that needed no work.
    pub unchanged: usize,
    /// Files removed from the index because they disappeared.
    pub deleted: usize,
    /// Files actually parsed in this run.
    pub reparsed: usize,
}

/// A refresh failure that stops the whole run.
#[derive(Debug, Error)]
pub enum RefreshError {
    /// The project root could not be guarded.
    #[error(transparent)]
    Guard(#[from] GuardError),

    /// The database rejected a write.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// A directory could not be read.
    #[error("cannot read directory `{path}`: {source}")]
    ReadDir {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },

    /// A file could not be read.
    #[error("cannot read file `{path}`: {source}")]
    ReadFile {
        /// File that failed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// A file found on disk, before its content is read.
struct Entry {
    rel_path: String,
    abs_path: PathBuf,
    size: u64,
    modified_at: i64,
}

/// Brings the index in line with the disk.
pub fn refresh(
    store: &Store,
    root: impl AsRef<Path>,
    options: &RefreshOptions,
) -> Result<IndexReport, RefreshError> {
    refresh_with_progress(store, root, options, &mut |_, _| {})
}

/// Same as [`refresh`], reporting `(processed, total)` files as it goes.
///
/// The callback runs on the indexing thread. The TUI uses it to draw a real progress bar
/// instead of freezing while a large project is reparsed.
pub fn refresh_with_progress(
    store: &Store,
    root: impl AsRef<Path>,
    options: &RefreshOptions,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<IndexReport, RefreshError> {
    let guard = PathGuard::new(root)?;
    let root = guard.root().to_path_buf();
    let rules = if options.respect_gitignore {
        IgnoreRules::load(&root)
    } else {
        IgnoreRules::empty()
    };

    let entries = walk(&guard, &rules, options.max_file_size)?;
    let paths: Vec<String> = entries.iter().map(|entry| entry.rel_path.clone()).collect();
    let known: std::collections::HashMap<String, crate::vce::index::m1::FileRow> = store
        .files()?
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect();

    // A parser upgrade invalidates every cached symbol, even for unchanged files.
    let parser_matches = store.parser_version()?.as_deref() == Some(PARSER_VERSION);
    let force = options.force || !parser_matches;

    let mut report = IndexReport::default();
    let mut seen = std::collections::HashSet::new();

    let total = entries.len();
    for (position, entry) in entries.iter().enumerate() {
        progress(position + 1, total);
        seen.insert(entry.rel_path.clone());
        let previous = known.get(&entry.rel_path);

        if !force
            && let Some(previous) = previous
            && previous.size == entry.size
            && previous.modified_at == entry.modified_at
            && previous.parser_status != "stale"
        {
            report.unchanged += 1;
            continue;
        }

        let bytes = std::fs::read(&entry.abs_path).map_err(|source| RefreshError::ReadFile {
            path: entry.abs_path.clone(),
            source,
        })?;

        if scanner::looks_binary(&bytes) {
            if store.delete_file(&entry.rel_path)? {
                report.deleted += 1;
            }
            continue;
        }

        let hash = blake3::hash(&bytes).to_hex().to_string();
        if !force
            && let Some(previous) = previous
            && previous.content_hash == hash
        {
            // Touched but not changed: keep the cached symbols, refresh the timestamp.
            store.touch_file(previous.id, entry.modified_at)?;
            report.unchanged += 1;
            continue;
        }

        let content = String::from_utf8_lossy(&bytes).into_owned();

        // One transaction per file: thousands of row inserts become one commit.
        store.begin()?;
        match store_parsed(store, entry, &content, &hash, &paths) {
            Ok(()) => store.commit()?,
            Err(err) => {
                store.rollback();
                return Err(err);
            }
        }

        report.reparsed += 1;
        if previous.is_some() {
            report.modified += 1;
        } else {
            report.added += 1;
        }
    }

    // A file that imported a module processed later in the alphabet could not have been
    // resolved on the first pass. Retry every unresolved edge once all files are known.
    resolve_pending(store, &paths)?;

    for path in known.keys() {
        if !seen.contains(path) && store.delete_file(path)? {
            report.deleted += 1;
        }
    }

    store.set_state("parser_version", PARSER_VERSION)?;
    store.set_state("last_full_scan", &now_unix().to_string())?;

    // Any source change invalidates cached prepared contexts through the key.
    if report.reparsed > 0 || report.deleted > 0 {
        store.bump_generation()?;
    }

    let counts = store.counts()?;
    report.files = counts.files;
    report.symbols = counts.symbols;
    report.imports = counts.imports;

    Ok(report)
}

/// Extracts and stores everything the index knows about one file.
fn store_parsed(
    store: &Store,
    entry: &Entry,
    content: &str,
    hash: &str,
    paths: &[String],
) -> Result<(), RefreshError> {
    let language = Language::from_path(&entry.rel_path);
    let source = SourceFile {
        rel_path: entry.rel_path.clone(),
        language,
        bytes: entry.size,
        lines: content.lines().count() as u32,
        hash: hash.to_owned(),
        text: content.to_owned(),
    };

    let symbols: Vec<NewSymbol> = signatures::extract(0, &source)
        .into_iter()
        .map(|symbol| NewSymbol {
            name: symbol.name,
            qualified_name: symbol.qualified_name,
            kind: symbol.kind.as_str().to_owned(),
            signature: symbol.signature,
            start_line: symbol.line,
            end_line: symbol.end_line,
        })
        .collect();

    let declarations: Vec<NewImport> = imports::extract(language, content)
        .into_iter()
        .map(|declaration| NewImport {
            target_path: imports::resolve(&declaration.name, &entry.rel_path, paths),
            name: declaration.name,
            line: declaration.line,
        })
        .collect();

    let mut mentions: Vec<(String, u32)> = text::name_counts(content).into_iter().collect();
    mentions.sort_by(|left, right| left.0.cmp(&right.0));

    let meta = FileMeta {
        path: entry.rel_path.clone(),
        content_hash: hash.to_owned(),
        size: entry.size,
        modified_at: entry.modified_at,
        language: language.as_str().to_owned(),
        parser_status: if language.has_signatures() {
            "parsed".to_owned()
        } else {
            "metadata_only".to_owned()
        },
    };

    let id = store.upsert_file(&meta)?;
    store.replace_symbols(id, &symbols)?;
    store.replace_imports(id, &declarations)?;
    store.replace_mentions(id, &mentions)?;
    Ok(())
}

/// Re-indexes a single file after its content changed on disk (`TOOL.md` §11).
///
/// Returns `false` when the file is gone, unreadable, too large, or binary: the caller
/// then reports the stale reference instead of pretence of freshness.
pub fn reindex_file(
    store: &Store,
    root: impl AsRef<Path>,
    rel_path: &str,
) -> Result<bool, RefreshError> {
    let guard = PathGuard::new(root)?;
    let Ok(abs_path) = guard.resolve(rel_path) else {
        return Ok(false);
    };
    let Ok(metadata) = std::fs::metadata(&abs_path) else {
        return Ok(false);
    };
    if metadata.len() > scanner::DEFAULT_MAX_FILE_SIZE {
        return Ok(false);
    }

    let bytes = std::fs::read(&abs_path).map_err(|source| RefreshError::ReadFile {
        path: abs_path.clone(),
        source,
    })?;
    if scanner::looks_binary(&bytes) {
        return Ok(false);
    }

    let hash = blake3::hash(&bytes).to_hex().to_string();
    let content = String::from_utf8_lossy(&bytes).into_owned();

    let mut paths: Vec<String> = store.files()?.into_iter().map(|file| file.path).collect();
    if !paths.iter().any(|path| path == rel_path) {
        paths.push(rel_path.to_owned());
        paths.sort();
    }

    let entry = Entry {
        rel_path: rel_path.to_owned(),
        abs_path,
        size: metadata.len(),
        modified_at: modified_at(&metadata),
    };

    store.begin()?;
    match store_parsed(store, &entry, &content, &hash, &paths) {
        Ok(()) => {
            resolve_pending(store, &paths)?;
            store.commit()?;
        }
        Err(err) => {
            store.rollback();
            return Err(err);
        }
    }

    // A single-file reindex changes project state just like a full refresh does. Without
    // this bump the runtime cache (M0) keeps its old `key:generation` entry and serves
    // stale context after `Engine::source` refreshed a changed file.
    store.bump_generation()?;
    Ok(true)
}

/// Walks the project, skipping excluded directories, ignored paths, and symlinks.
fn walk(
    guard: &PathGuard,
    rules: &IgnoreRules,
    max_file_size: u64,
) -> Result<Vec<Entry>, RefreshError> {
    let mut entries = Vec::new();
    let mut pending = vec![guard.root().to_path_buf()];

    while let Some(directory) = pending.pop() {
        let read = std::fs::read_dir(&directory).map_err(|source| RefreshError::ReadDir {
            path: directory.clone(),
            source,
        })?;

        for item in read {
            let item = item.map_err(|source| RefreshError::ReadDir {
                path: directory.clone(),
                source,
            })?;
            let path = item.path();
            let Some(rel_path) = guard.relative(&path) else {
                continue;
            };

            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let file_type = metadata.file_type();

            if file_type.is_symlink() {
                continue;
            }

            if file_type.is_dir() {
                let name = rel_path.rsplit('/').next().unwrap_or(&rel_path);
                if scanner::excluded_dir(name) || rules.is_ignored(&rel_path, true) {
                    continue;
                }
                pending.push(path);
                continue;
            }

            if !file_type.is_file() || metadata.len() > max_file_size {
                continue;
            }

            if rules.is_ignored(&rel_path, false) {
                continue;
            }

            entries.push(Entry {
                rel_path,
                abs_path: path,
                size: metadata.len(),
                modified_at: modified_at(&metadata),
            });
        }
    }

    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(entries)
}

/// Modification time in Unix nanoseconds; an unreadable time counts as zero.
///
/// Nanosecond precision matters: two edits of the same size within one second must
/// invalidate the index entry, and second-granularity timestamps silently miss them.
fn modified_at(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or_default()
}

/// Resolves import edges that pointed at a file the walk had not stored yet.
fn resolve_pending(store: &Store, paths: &[String]) -> Result<(), StoreError> {
    for (file_id, source_path, name) in store.unresolved_imports()? {
        if let Some(target) = imports::resolve(&name, &source_path, paths) {
            store.resolve_imports(file_id, &[(name, target)])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }
        let store = Store::open_in_memory().expect("store");
        (dir, store)
    }

    fn run(dir: &tempfile::TempDir, store: &Store, options: RefreshOptions) -> IndexReport {
        refresh(store, dir.path(), &options).expect("refresh")
    }

    #[test]
    fn indexes_a_project_and_reports_what_it_found() {
        let (dir, store) = project(&[
            (
                "src/main.rs",
                "use crate::util;\nfn main() { util::run(); }\n",
            ),
            ("src/util.rs", "pub fn run() {}\n"),
            ("style.css", ".btn { color: red; }\n"),
        ]);

        let report = run(&dir, &store, RefreshOptions::default());

        assert_eq!(report.files, 3);
        assert_eq!(report.added, 3);
        assert_eq!(report.reparsed, 3);
        assert!(report.symbols >= 3);
        assert_eq!(report.imports, 1, "main -> util must resolve");
    }

    #[test]
    fn a_second_run_parses_nothing() {
        let (dir, store) = project(&[
            ("src/main.rs", "fn main() {}\n"),
            ("src/lib.rs", "pub fn run() {}\n"),
        ]);

        run(&dir, &store, RefreshOptions::default());
        let second = run(&dir, &store, RefreshOptions::default());

        assert_eq!(second.reparsed, 0);
        assert_eq!(second.unchanged, 2);
        assert_eq!(second.added, 0);
    }

    #[test]
    fn a_modified_file_is_reparsed_and_a_timestamp_only_change_is_not() {
        let (dir, store) = project(&[("src/lib.rs", "pub fn run() {}\n")]);
        run(&dir, &store, RefreshOptions::default());

        // Same content, new timestamp: the hash matches, so the cached symbols stay.
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn run() {}\n").expect("rewrite");
        let touched = run(&dir, &store, RefreshOptions::default());
        assert_eq!(touched.reparsed, 0);

        // Different content: the file must be parsed again.
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn run() {}\npub fn stop() {}\n",
        )
        .expect("rewrite");
        let changed = run(&dir, &store, RefreshOptions::default());

        assert_eq!(changed.modified, 1);
        assert_eq!(changed.reparsed, 1);
        let symbols = store.symbols().expect("symbols");
        assert!(symbols.iter().any(|symbol| symbol.name == "stop"));
    }

    #[test]
    fn deleted_files_disappear_from_the_index() {
        let (dir, store) = project(&[
            ("src/a.rs", "pub fn a() {}\n"),
            ("src/b.rs", "pub fn b() {}\n"),
        ]);
        run(&dir, &store, RefreshOptions::default());

        std::fs::remove_file(dir.path().join("src/b.rs")).expect("remove");
        let report = run(&dir, &store, RefreshOptions::default());

        assert_eq!(report.deleted, 1);
        assert_eq!(report.files, 1);
        assert!(
            store
                .symbols()
                .expect("symbols")
                .iter()
                .all(|symbol| symbol.name == "a")
        );
    }

    #[test]
    fn honours_gitignore_and_the_builtin_exclusions() {
        let (dir, store) = project(&[
            (".gitignore", "generated/\n*.log\n"),
            ("src/lib.rs", "pub fn run() {}\n"),
            ("generated/code.rs", "pub fn generated() {}\n"),
            ("debug.log", "noise\n"),
            ("target/cache.rs", "pub fn cached() {}\n"),
        ]);

        let report = run(&dir, &store, RefreshOptions::default());

        assert_eq!(report.files, 2, "only .gitignore and src/lib.rs");
        let paths: Vec<String> = store
            .files()
            .expect("files")
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert!(paths.contains(&"src/lib.rs".to_owned()));
        assert!(!paths.iter().any(|path| path.starts_with("generated/")));
        assert!(!paths.iter().any(|path| path.starts_with("target/")));
    }

    #[test]
    fn resolving_imports_works_regardless_of_processing_order() {
        // `src/app.rs` sorts before `src/zeta.rs` and imports it.
        let (dir, store) = project(&[
            ("src/app.rs", "use crate::zeta;\npub fn app() {}\n"),
            ("src/zeta.rs", "pub fn zeta() {}\n"),
        ]);

        let report = run(&dir, &store, RefreshOptions::default());

        assert_eq!(report.imports, 1);
    }

    #[test]
    fn force_reparses_everything() {
        let (dir, store) = project(&[("src/lib.rs", "pub fn run() {}\n")]);
        run(&dir, &store, RefreshOptions::default());

        let forced = run(
            &dir,
            &store,
            RefreshOptions {
                force: true,
                ..RefreshOptions::default()
            },
        );

        assert_eq!(forced.reparsed, 1);
    }

    #[test]
    fn skips_binary_files() {
        let (dir, store) = project(&[("src/lib.rs", "pub fn run() {}\n")]);
        std::fs::write(dir.path().join("logo.png"), [0u8, 1, 2, 3]).expect("write");

        let report = run(&dir, &store, RefreshOptions::default());

        assert_eq!(report.files, 1);
    }

    #[test]
    fn two_edits_within_one_second_invalidate_the_cache() {
        // Same size, different content, both writes inside a single second: a
        // second-granularity mtime would report "unchanged" and keep stale symbols.
        let (dir, store) = project(&[("src/lib.rs", "fn a() {}\n")]);
        run(&dir, &store, RefreshOptions::default());

        std::fs::write(dir.path().join("src/lib.rs"), "fn b() {}\n").expect("rewrite");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let report = run(&dir, &store, RefreshOptions::default());

        assert_eq!(report.reparsed, 1, "same-second edit must reparse");
        assert_eq!(report.modified, 1);
        let symbols = store.symbols().expect("symbols");
        assert!(symbols.iter().any(|symbol| symbol.name == "b"));
        assert!(!symbols.iter().any(|symbol| symbol.name == "a"));
    }

    #[test]
    fn a_parser_upgrade_invalidates_the_cache() {
        let (dir, store) = project(&[("src/lib.rs", "pub fn run() {}\n")]);
        run(&dir, &store, RefreshOptions::default());

        store
            .set_state("parser_version", "0")
            .expect("set parser version");
        let report = run(&dir, &store, RefreshOptions::default());

        assert_eq!(report.reparsed, 1);
    }
}
