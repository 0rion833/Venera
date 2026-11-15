//! Project synchronization: bring the index in line with the actual filesystem after a
//! task, without a full rebuild (`prirote.md` §7).
//!
//! The tracked file list is a hint. Every path is checked on disk: a claim of modification
//! with unchanged bytes records nothing, a deleted file loses its rows, and a file changed
//! outside the task is picked up exactly like one changed inside it.
//!
//! Only changed files are parsed; the generation advances only when something changed, so
//! an idle task costs one hash per tracked path and nothing else.

use crate::vce::index::m1::StoreError;
use crate::vce::index::query::{Engine, EngineError};
use crate::vce::index::refresh;
use crate::vce::index::scanner;

/// Options of one synchronization.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    /// Re-parse even when the content hash is unchanged.
    pub force: bool,
}

/// One path that could not be synchronized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFailure {
    /// Project-relative path.
    pub path: String,
    /// Why it failed.
    pub reason: String,
}

/// What synchronization did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Paths examined.
    pub checked: usize,
    /// New files added to the index.
    pub created: Vec<String>,
    /// Files whose content changed.
    pub modified: Vec<String>,
    /// Files removed from the index.
    pub deleted: Vec<String>,
    /// Files that matched the index.
    pub unchanged: usize,
    /// Paths that could not be synchronized.
    pub failed: Vec<SyncFailure>,
    /// Index generation after synchronization.
    pub generation: i64,
    /// Cache entries dropped because they were built from a changed file.
    pub invalidated: usize,
    /// Whether anything changed.
    pub changed: bool,
    /// Symbols in the index afterwards.
    pub symbols: usize,
}

/// A synchronization failure that stops the run.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The engine failed.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// The database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Synchronizes `paths` with the disk.
pub fn synchronize(
    engine: &Engine,
    paths: &[String],
    options: &SyncOptions,
) -> Result<SyncReport, SyncError> {
    let mut unique: Vec<String> = paths.iter().map(|path| normalize(path)).collect();
    unique.retain(|path| !path.is_empty());
    unique.sort();
    unique.dedup();

    let mut report = SyncReport::default();

    for path in &unique {
        report.checked += 1;
        let stored = engine.store().file(path)?;

        // A file that is gone from disk loses its rows, whatever the task claimed.
        let Ok(absolute) = engine.guard().resolve(path) else {
            if stored.is_some() && engine.store().delete_file(path)? {
                report.deleted.push(path.clone());
            }
            continue;
        };

        let Ok(metadata) = std::fs::metadata(&absolute) else {
            report.failed.push(SyncFailure {
                path: path.clone(),
                reason: "cannot read metadata".to_owned(),
            });
            continue;
        };

        if metadata.len() > scanner::DEFAULT_MAX_FILE_SIZE {
            report.failed.push(SyncFailure {
                path: path.clone(),
                reason: "file is too large".to_owned(),
            });
            continue;
        }

        let Ok(bytes) = std::fs::read(&absolute) else {
            report.failed.push(SyncFailure {
                path: path.clone(),
                reason: "cannot read the file".to_owned(),
            });
            continue;
        };

        if scanner::looks_binary(&bytes) {
            if stored.is_some() && engine.store().delete_file(path)? {
                report.deleted.push(path.clone());
            }
            continue;
        }

        let hash = blake3::hash(&bytes).to_hex().to_string();
        if !options.force && stored.as_ref().is_some_and(|row| row.content_hash == hash) {
            report.unchanged += 1;
            continue;
        }

        let existed = stored.is_some();
        match refresh::reindex_file(engine.store(), engine.root(), path) {
            Ok(true) if existed => report.modified.push(path.clone()),
            Ok(true) => report.created.push(path.clone()),
            Ok(false) => report.failed.push(SyncFailure {
                path: path.clone(),
                reason: "the file could not be re-indexed".to_owned(),
            }),
            Err(error) => report.failed.push(SyncFailure {
                path: path.clone(),
                reason: error.to_string(),
            }),
        }
    }

    report.changed =
        !report.created.is_empty() || !report.modified.is_empty() || !report.deleted.is_empty();

    report.generation = if report.changed {
        engine.store().bump_generation()?
    } else {
        engine.store().generation()?
    };

    if report.changed {
        let mut touched: Vec<String> = Vec::new();
        touched.extend(report.created.iter().cloned());
        touched.extend(report.modified.iter().cloned());
        touched.extend(report.deleted.iter().cloned());
        report.invalidated = engine.store().cache_invalidate_paths(&touched)?;
    }

    report.symbols = engine.store().counts()?.symbols;

    Ok(report)
}

/// Project-relative path with forward slashes.
fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./");
    path.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::refresh::{RefreshOptions, refresh};

    fn engine(files: &[(&str, &str)]) -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }
        let engine = Engine::open(dir.path(), ".venera/index.db").expect("engine");
        refresh(engine.store(), dir.path(), &RefreshOptions::default()).expect("refresh");
        (dir, engine)
    }

    #[test]
    fn a_modified_file_is_reparsed_and_the_generation_advances() {
        let (dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn run() {}\npub fn stop() {}\n",
        )
        .expect("rewrite");

        let report = synchronize(&engine, &["src/lib.rs".to_owned()], &SyncOptions::default())
            .expect("sync");

        assert_eq!(report.modified, vec!["src/lib.rs".to_owned()]);
        assert!(report.changed);
        assert!(report.generation > 0);
        assert!(
            engine
                .store()
                .symbols()
                .expect("symbols")
                .iter()
                .any(|symbol| symbol.name == "stop")
        );
    }

    #[test]
    fn an_unchanged_file_costs_nothing() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        let before = engine.store().generation().expect("generation");

        let report = synchronize(&engine, &["src/lib.rs".to_owned()], &SyncOptions::default())
            .expect("sync");

        assert_eq!(report.unchanged, 1);
        assert!(!report.changed);
        assert_eq!(report.generation, before, "an idle sync must not advance");
    }

    #[test]
    fn a_created_file_is_added() {
        let (dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        std::fs::write(dir.path().join("src/new.rs"), "pub fn fresh() {}\n").expect("write");

        let report = synchronize(&engine, &["src/new.rs".to_owned()], &SyncOptions::default())
            .expect("sync");

        assert_eq!(report.created, vec!["src/new.rs".to_owned()]);
        assert!(engine.store().file("src/new.rs").expect("file").is_some());
    }

    #[test]
    fn a_deleted_file_loses_its_rows() {
        let (dir, engine) = engine(&[
            ("src/a.rs", "pub fn a() {}\n"),
            ("src/b.rs", "pub fn b() {}\n"),
        ]);
        std::fs::remove_file(dir.path().join("src/b.rs")).expect("remove");

        let report =
            synchronize(&engine, &["src/b.rs".to_owned()], &SyncOptions::default()).expect("sync");

        assert_eq!(report.deleted, vec!["src/b.rs".to_owned()]);
        assert!(engine.store().file("src/b.rs").expect("file").is_none());
    }

    #[test]
    fn a_claim_without_a_change_records_nothing() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);

        let report = synchronize(&engine, &["src/lib.rs".to_owned()], &SyncOptions::default())
            .expect("sync");

        assert!(report.modified.is_empty());
        assert_eq!(report.unchanged, 1);
    }

    #[test]
    fn paths_are_normalized_and_deduplicated() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);

        let report = synchronize(
            &engine,
            &["./src/lib.rs".to_owned(), "src\\lib.rs".to_owned()],
            &SyncOptions::default(),
        )
        .expect("sync");

        assert_eq!(report.checked, 1, "both spellings are one path");
    }
}
