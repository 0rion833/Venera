//! Static user backup in `.venera/index2.db`.
//!
//! `index1` is the live, dynamic index the model reads. `index2` is the user's snapshot:
//! `/vce` copies files whose last change came from the user, and `/back` restores them.
//! Model-authored files are marked `ai` in `index1` and are never copied; `/back` removes
//! them when they have no user snapshot.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use thiserror::Error;

use crate::security::path_guard::GuardError;
use crate::vce::index::m1::StoreError;
use crate::vce::index::query::{Engine, EngineError};
use crate::vce::index::refresh::{self, RefreshError, RefreshOptions};
use crate::vce::index::scanner::{self, DEFAULT_MAX_FILE_SIZE};

/// Database file, relative to the project root.
pub const BACKUP_DATABASE: &str = ".venera/index2.db";

/// A backup failure.
#[derive(Debug, Error)]
pub enum BackupError {
    /// SQLite failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),

    /// The live index failed.
    #[error(transparent)]
    Index(#[from] StoreError),

    /// The engine failed.
    #[error(transparent)]
    Engine(#[from] EngineError),

    /// A path escaped the project sandbox.
    #[error(transparent)]
    Guard(#[from] GuardError),

    /// A file could not be reindexed.
    #[error(transparent)]
    Refresh(#[from] RefreshError),

    /// A backup file could not be read or written.
    #[error("cannot access `{path}`: {source}")]
    Io {
        /// Project-relative path.
        path: String,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// What `/vce` wrote into `index2`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackupReport {
    /// Files in the backup afterwards.
    pub files: usize,
    /// Files copied or refreshed in this run.
    pub saved: usize,
    /// AI-owned files left out of the backup.
    pub skipped_ai: usize,
}

/// What `/back` restored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Backup files in `index2`.
    pub files: usize,
    /// Files rewritten from the backup.
    pub restored: usize,
    /// AI-created files removed because no user snapshot exists.
    pub removed: usize,
    /// Files already identical to the backup.
    pub unchanged: usize,
    /// Index/refresh steps that failed after a file was restored on disk.
    pub index_failures: Vec<String>,
}

/// The static backup database.
pub struct BackupStore {
    conn: Connection,
    path: PathBuf,
}

impl BackupStore {
    /// Opens or creates `.venera/index2.db` under `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, BackupError> {
        let root = root.as_ref();
        let path = root.join(BACKUP_DATABASE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| BackupError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS backup_files (
                path         TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                content      BLOB NOT NULL,
                size         INTEGER NOT NULL,
                saved_at     INTEGER NOT NULL
             );",
        )?;
        Ok(Self { conn, path })
    }

    /// Database path, for reports.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Inserts or refreshes one snapshot.
    pub fn upsert(&self, path: &str, hash: &str, content: &[u8]) -> Result<(), BackupError> {
        self.conn.execute(
            "INSERT INTO backup_files (path, content_hash, content, size, saved_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                content_hash = excluded.content_hash,
                content = excluded.content,
                size = excluded.size,
                saved_at = excluded.saved_at",
            params![path, hash, content, content.len() as i64, unix_now()],
        )?;
        Ok(())
    }

    /// Removes one path from the snapshot.
    pub fn remove(&self, path: &str) -> Result<(), BackupError> {
        self.conn
            .execute("DELETE FROM backup_files WHERE path = ?1", params![path])?;
        Ok(())
    }

    /// Number of files in the snapshot.
    pub fn count(&self) -> Result<usize, BackupError> {
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM backup_files", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Every snapshot row, ordered by path.
    pub fn rows(&self) -> Result<Vec<BackupRow>, BackupError> {
        let mut statement = self
            .conn
            .prepare("SELECT path, content_hash, content FROM backup_files ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            Ok(BackupRow {
                path: row.get(0)?,
                content_hash: row.get(1)?,
                content: row.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(BackupError::from)
    }
}

/// One restored snapshot row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRow {
    /// Project-relative path.
    pub path: String,
    /// Hash at snapshot time.
    pub content_hash: String,
    /// Full text at snapshot time.
    pub content: Vec<u8>,
}

/// Copies every user-owned text file into `index2`; AI-owned files are left alone.
pub fn snapshot(engine: &Engine) -> Result<BackupReport, BackupError> {
    engine.refresh(&RefreshOptions::default())?;
    let backup = BackupStore::open(engine.root())?;
    let mut report = BackupReport::default();

    for file in engine.store().files()? {
        let origin = engine
            .store()
            .origin(&file.path)?
            .unwrap_or_else(|| "user".to_owned());
        if origin == "ai" {
            report.skipped_ai += 1;
            continue;
        }

        let absolute = engine.guard().resolve(&file.path)?;
        let bytes = std::fs::read(&absolute).map_err(|source| BackupError::Io {
            path: file.path.clone(),
            source,
        })?;
        if bytes.len() as u64 > DEFAULT_MAX_FILE_SIZE || scanner::looks_binary(&bytes) {
            continue;
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        backup.upsert(&file.path, &hash, &bytes)?;
        report.saved += 1;
    }

    report.files = backup.count()?;
    Ok(report)
}

/// Restores every snapshot file and removes AI-created files that have no snapshot.
pub fn restore(engine: &Engine) -> Result<RestoreReport, BackupError> {
    engine.refresh(&RefreshOptions::default())?;
    let backup = BackupStore::open(engine.root())?;
    let rows = backup.rows()?;
    let mut report = RestoreReport {
        files: rows.len(),
        ..RestoreReport::default()
    };
    let mut backed_up = BTreeSet::new();

    for row in rows {
        backed_up.insert(row.path.clone());
        let target = engine.guard().check_lexical(&row.path)?;
        let current = std::fs::read(&target).ok();
        if current.as_deref() == Some(row.content.as_slice()) {
            report.unchanged += 1;
            continue;
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| BackupError::Io {
                path: row.path.clone(),
                source,
            })?;
        }
        std::fs::write(&target, &row.content).map_err(|source| BackupError::Io {
            path: row.path.clone(),
            source,
        })?;
        if let Err(error) = refresh::reindex_file(engine.store(), engine.root(), &row.path) {
            report
                .index_failures
                .push(format!("reindex {}: {error}", row.path));
        }
        if let Err(error) = engine.store().set_origin(&row.path, "user") {
            report
                .index_failures
                .push(format!("origin {}: {error}", row.path));
        }
        report.restored += 1;
    }

    for file in engine.store().files()? {
        if backed_up.contains(&file.path) {
            continue;
        }
        let origin = engine
            .store()
            .origin(&file.path)?
            .unwrap_or_else(|| "user".to_owned());
        if origin != "ai" {
            continue;
        }
        let target = engine.guard().resolve(&file.path)?;
        std::fs::remove_file(&target).map_err(|source| BackupError::Io {
            path: file.path.clone(),
            source,
        })?;
        if let Err(error) = engine.store().delete_file(&file.path) {
            report
                .index_failures
                .push(format!("delete {}: {error}", file.path));
        }
        report.removed += 1;
    }

    if let Err(error) = engine.refresh(&RefreshOptions::default()) {
        report.index_failures.push(format!("refresh: {error}"));
    }
    Ok(report)
}

/// Reconciles paths the poller saw. A changed file is user-owned unless the live index
/// already has the same hash (that is an AI write, which reindexed itself first).
pub fn sync(engine: &Engine, paths: &[String]) -> Result<usize, BackupError> {
    let backup = BackupStore::open(engine.root())?;
    let mut changed = 0usize;

    for path in paths {
        let target = engine.guard().check_lexical(path)?;
        if !target.is_file() {
            backup.remove(path)?;
            let _ = engine.store().delete_file(path);
            changed += 1;
            continue;
        }

        let bytes = std::fs::read(&target).map_err(|source| BackupError::Io {
            path: path.clone(),
            source,
        })?;
        if bytes.len() as u64 > DEFAULT_MAX_FILE_SIZE || scanner::looks_binary(&bytes) {
            continue;
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let indexed = engine.store().file(path)?.map(|file| file.content_hash);
        if indexed.as_deref() == Some(hash.as_str()) {
            continue;
        }

        engine.store().set_origin(path, "user")?;
        let _ = refresh::reindex_file(engine.store(), engine.root(), path);
        engine.store().set_origin(path, "user")?;
        backup.upsert(path, &hash, &bytes)?;
        changed += 1;
    }

    Ok(changed)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::query::Engine;

    fn project() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("user.txt"), "user one").expect("write");
        std::fs::write(dir.path().join("ai.txt"), "ai old").expect("write");
        let engine = Engine::open(dir.path(), ".venera/index.db").expect("engine");
        engine.refresh(&RefreshOptions::default()).expect("refresh");
        engine.store().set_origin("ai.txt", "ai").expect("origin");
        (dir, engine)
    }

    #[test]
    fn snapshot_skips_ai_files_and_restore_undoes_them() {
        let (dir, engine) = project();

        let saved = snapshot(&engine).expect("snapshot");
        assert_eq!(saved.saved, 1);
        assert_eq!(saved.skipped_ai, 1);
        assert_eq!(saved.files, 1);

        std::fs::write(dir.path().join("user.txt"), "user edited").expect("edit");
        std::fs::write(dir.path().join("ai-new.txt"), "ai new").expect("write");
        engine.refresh(&RefreshOptions::default()).expect("refresh");
        engine
            .store()
            .set_origin("ai-new.txt", "ai")
            .expect("origin");

        let restored = restore(&engine).expect("restore");
        assert_eq!(restored.restored, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("user.txt")).expect("read"),
            "user one"
        );
        assert!(!dir.path().join("ai-new.txt").exists());
    }

    #[test]
    fn restore_keeps_a_clean_index_report_and_yells_when_the_index_is_locked() {
        let (dir, engine) = project();
        snapshot(&engine).expect("snapshot");

        std::fs::write(dir.path().join("ai-new.txt"), "ai new").expect("write");
        engine.refresh(&RefreshOptions::default()).expect("refresh");
        engine
            .store()
            .set_origin("ai-new.txt", "ai")
            .expect("origin");

        let report = restore(&engine).expect("restore");
        assert_eq!(
            report.removed, 2,
            "ai.txt and ai-new.txt have no user snapshot"
        );
        assert_eq!(report.restored, 0);
        assert_eq!(report.unchanged, 1, "user.txt is already in the backup");
        assert!(
            report.index_failures.is_empty(),
            "clean restore must not invent failures, got {report:?}"
        );

        // With the database held by a second writer any index write fails with
        // SQLITE_BUSY. The pre-fix code dropped some of those errors with `let _ =` and
        // reported success; restore must return an explicit error instead.
        let blocker = crate::vce::index::m1::Store::open(dir.path().join(".venera/index.db"))
            .expect("blocker");
        blocker.begin().expect("lock");
        let error = restore(&engine).expect_err("locked restore must fail loudly");
        assert!(
            error.to_string().contains("database is locked"),
            "expected a busy error, got {error}"
        );
    }

    #[test]
    fn sync_updates_user_files_and_ignores_ai_writes() {
        let (dir, engine) = project();
        snapshot(&engine).expect("snapshot");

        std::fs::write(dir.path().join("user.txt"), "user new").expect("edit");
        assert_eq!(sync(&engine, &["user.txt".to_owned()]).expect("sync"), 1);

        std::fs::write(dir.path().join("ai.txt"), "ai new").expect("edit");
        engine.store().set_origin("ai.txt", "ai").expect("origin");
        let _ = refresh::reindex_file(engine.store(), engine.root(), "ai.txt");
        assert_eq!(sync(&engine, &["ai.txt".to_owned()]).expect("sync"), 0);

        std::fs::write(dir.path().join("new.txt"), "user created").expect("write");
        assert_eq!(sync(&engine, &["new.txt".to_owned()]).expect("sync"), 1);

        let rows = BackupStore::open(dir.path())
            .expect("backup")
            .rows()
            .expect("rows");
        let paths: BTreeSet<&str> = rows.iter().map(|row| row.path.as_str()).collect();
        assert!(paths.contains("user.txt"));
        assert!(paths.contains("new.txt"));
        assert!(!paths.contains("ai.txt"));
    }

    #[test]
    fn snapshot_refreshes_a_user_file_but_keeps_the_ai_version_out() {
        let (dir, engine) = project();
        snapshot(&engine).expect("snapshot");

        std::fs::write(dir.path().join("user.txt"), "user two").expect("edit");
        engine.refresh(&RefreshOptions::default()).expect("refresh");
        let saved = snapshot(&engine).expect("snapshot");
        assert_eq!(saved.saved, 1);

        restore(&engine).expect("restore");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("user.txt")).expect("read"),
            "user two"
        );
    }
}
