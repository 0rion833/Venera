//! M1 storage: SQLite schema, migrations, and lookups.
//!
//! The key is the content hash of a file (blake3). Table-format changes bump
//! [`SCHEMA_VERSION`]; a database written by a newer build is refused instead of being
//! silently corrupted, and `.venera/index.db` stays out of version control.
//!
//! # Schema (TOOL.md §14)
//!
//! ```text
//! files(id, path, content_hash, size, modified_at, language, parser_status)
//!     UNIQUE(path) — the relative path is the lookup key, `id` the database reference
//! symbols(id, file_id, name, qualified_name, kind, signature, start_line, end_line)
//!     UNIQUE(file_id, kind, qualified_name) — line numbers are attributes, not identity
//! imports(id, source_file, target_file, import_name, line)
//! file_mentions(file_id, name, count) — incremental token frequency, no rescan needed
//! map_state(id, text, hash, generation, updated_at) — last rendered map, for deltas
//! map_symbols(path, qualified_name) — symbol snapshot behind that map
//! index_state(key, value) — parser version, configuration hash, last full scan
//! symbols_fts — FTS5 external-content index over name, qualified_name, signature
//! ```
//!
//! Rules the schema keeps:
//!
//! - `id` is a surrogate integer key, never a hash: a file keeps its identity across
//!   edits and renames, and a symbol keeps its identity across line shifts.
//! - `qualified_name` is the only symbol identity that survives unrelated edits.
//! - A hash proves current content, not correctness of an old row: retrieval re-checks
//!   the file before returning source (`TOOL.md` §11).
//! - Migrations are versioned. A version mismatch rebuilds the index; it never silently
//!   deletes a user's database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

/// Version of the table layout written by this build.
pub const SCHEMA_VERSION: i64 = 4;

/// Version of the extractors that produced the stored symbols.
///
/// Bumping it invalidates the parser cache: stored symbols are reused only while the
/// extractor that wrote them is still the current one (`TOOL.md` §18).
pub const PARSER_VERSION: &str = "3";

/// A storage failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database file could not be opened or its directory created.
    #[error("cannot open index database {path}: {source}")]
    Open {
        /// Database path.
        path: PathBuf,
        /// Underlying SQLite error.
        #[source]
        source: rusqlite::Error,
    },

    /// Filesystem failure while preparing the database path.
    #[error("cannot prepare the index directory: {0}")]
    Io(#[from] std::io::Error),

    /// Any other SQLite failure.
    #[error("index database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database was written by a newer build.
    #[error(
        "index schema {found} is newer than {supported}; upgrade Venera or delete {path} to rebuild"
    )]
    NewerSchema {
        /// Version found on disk.
        found: i64,
        /// Version this build understands.
        supported: i64,
        /// Database path.
        path: PathBuf,
    },
}

/// Result alias for storage calls.
pub type Result<T> = std::result::Result<T, StoreError>;

/// A row of `files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Surrogate database id, stable across edits.
    pub id: i64,
    /// Path relative to the project root, forward slashes.
    pub path: String,
    /// BLAKE3 of the content the row was built from.
    pub content_hash: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub modified_at: i64,
    /// Detected language.
    pub language: String,
    /// `parsed`, `metadata_only`, or `stale`.
    pub parser_status: String,
}

/// A row of `symbols`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    /// Surrogate database id.
    pub id: i64,
    /// Owning file.
    pub file_id: i64,
    /// Name as written in the source.
    pub name: String,
    /// Owner-qualified name, the stable identity of the symbol.
    pub qualified_name: String,
    /// `fn`, `struct`, `class`, and so on.
    pub kind: String,
    /// One-line signature.
    pub signature: String,
    /// First line, one-based.
    pub start_line: u32,
    /// Last line, one-based.
    pub end_line: u32,
}

/// A symbol to write for a file.
#[derive(Debug, Clone)]
pub struct NewSymbol {
    /// Name as written in the source.
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

/// An import edge to write for a file.
#[derive(Debug, Clone)]
pub struct NewImport {
    /// Imported module or symbol as written.
    pub name: String,
    /// Line of the declaration.
    pub line: u32,
    /// Resolved target path, when the target is inside the project.
    pub target_path: Option<String>,
}

/// What to store about one file.
#[derive(Debug, Clone)]
pub struct FileMeta {
    /// Relative path, forward slashes.
    pub path: String,
    /// BLAKE3 hex digest.
    pub content_hash: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, seconds since the epoch.
    pub modified_at: i64,
    /// Detected language.
    pub language: String,
    /// `parsed` or `metadata_only`.
    pub parser_status: String,
}

/// Counts used by reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Files in the index.
    pub files: usize,
    /// Symbols in the index.
    pub symbols: usize,
    /// Resolved import edges.
    pub imports: usize,
}

/// One prepared context kept in the working cache (M1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRow {
    /// Cache key, a hash of strategy, input, budget, and index generation.
    pub key: String,
    /// Prepared context.
    pub content: String,
    /// Tokens of `content`.
    pub token_count: usize,
    /// Creation time, Unix seconds.
    pub created_at: i64,
    /// Last access, Unix seconds.
    pub last_used_at: i64,
    /// How many times the entry was served.
    pub access_count: u32,
    /// Index generation when the entry was built; entries without recorded sources are
    /// only valid while it matches the current one.
    pub generation: i64,
}

/// Aggregate cache statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheTotals {
    /// Entries stored.
    pub entries: usize,
    /// Bytes stored.
    pub bytes: usize,
    /// Sum of access counts.
    pub accesses: u64,
}

/// The M1 database.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Opens (or creates) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&path).map_err(|source| StoreError::Open {
            path: path.clone(),
            source,
        })?;
        let store = Self { conn, path };
        store.prepare()?;
        Ok(store)
    }

    /// Opens an in-memory database, used by tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn,
            path: PathBuf::from(":memory:"),
        };
        store.prepare()?;
        Ok(store)
    }

    /// Path this store was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn prepare(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let _ = self.conn.execute_batch("PRAGMA journal_mode = WAL;");
        // WAL plus a normal sync is the usual trade for an index: crash-safe against
        // process death, and an order of magnitude fewer fsyncs than the default.
        let _ = self.conn.execute_batch("PRAGMA synchronous = NORMAL;");
        self.migrate()
    }

    /// Starts a write transaction.
    ///
    /// Indexing writes thousands of rows; one transaction per file turns thousands of
    /// fsyncs into one, which is the difference between seconds and minutes on Windows.
    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    /// Commits the current write transaction.
    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Rolls back the current write transaction; errors are ignored on purpose, because
    /// the caller is already unwinding another failure.
    pub fn rollback(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;

        if version > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema {
                found: version,
                supported: SCHEMA_VERSION,
                path: self.path.clone(),
            });
        }

        if version < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
        }

        if version < 2 {
            self.conn.execute_batch(SCHEMA_V2)?;
        }

        if version < 3 {
            self.conn.execute_batch(SCHEMA_V3)?;
        }

        if version < 4 {
            self.conn.execute_batch(SCHEMA_V4)?;
        }

        self.conn
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        Ok(())
    }

    /// Removes every row, keeping the schema. Used by `index --rebuild`.
    pub fn clear(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM map_symbols;
             DELETE FROM map_state;
             DELETE FROM file_mentions;
             DELETE FROM imports;
             DELETE FROM symbols;
             DELETE FROM files;
             DELETE FROM file_origins;
             DELETE FROM index_state;",
        )?;
        Ok(())
    }

    /// All files, ordered by path.
    pub fn files(&self) -> Result<Vec<FileRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, path, content_hash, size, modified_at, language, parser_status
             FROM files ORDER BY path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(FileRow {
                id: row.get(0)?,
                path: row.get(1)?,
                content_hash: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                modified_at: row.get(4)?,
                language: row.get(5)?,
                parser_status: row.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Records who last changed a file: the user or the model.
    pub fn set_origin(&self, path: &str, origin: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO file_origins (path, origin) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET origin = excluded.origin",
            params![path, origin],
        )?;
        Ok(())
    }

    /// Last known origin of a file; `None` is treated as `user`.
    pub fn origin(&self, path: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT origin FROM file_origins WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// One file by relative path.
    pub fn file(&self, path: &str) -> Result<Option<FileRow>> {
        self.conn
            .query_row(
                "SELECT id, path, content_hash, size, modified_at, language, parser_status
                 FROM files WHERE path = ?1",
                params![path],
                |row| {
                    Ok(FileRow {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        content_hash: row.get(2)?,
                        size: row.get::<_, i64>(3)? as u64,
                        modified_at: row.get(4)?,
                        language: row.get(5)?,
                        parser_status: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// One file by database id.
    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        self.conn
            .query_row(
                "SELECT id, path, content_hash, size, modified_at, language, parser_status
                 FROM files WHERE id = ?1",
                params![id],
                |row| {
                    Ok(FileRow {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        content_hash: row.get(2)?,
                        size: row.get::<_, i64>(3)? as u64,
                        modified_at: row.get(4)?,
                        language: row.get(5)?,
                        parser_status: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Inserts a file or updates its metadata, returning its stable id.
    pub fn upsert_file(&self, meta: &FileMeta) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, content_hash, size, modified_at, language, parser_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                 content_hash = excluded.content_hash,
                 size = excluded.size,
                 modified_at = excluded.modified_at,
                 language = excluded.language,
                 parser_status = excluded.parser_status",
            params![
                meta.path,
                meta.content_hash,
                meta.size as i64,
                meta.modified_at,
                meta.language,
                meta.parser_status
            ],
        )?;

        self.conn
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params![meta.path],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    /// Records a new modification time without touching the cached symbols.
    pub fn touch_file(&self, id: i64, modified_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET modified_at = ?1 WHERE id = ?2",
            params![modified_at, id],
        )?;
        Ok(())
    }

    /// Marks a file as having changed on disk while its row is out of date.
    pub fn mark_stale(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET parser_status = 'stale' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Deletes a file and, through the cascade, its symbols and imports.
    pub fn delete_file(&self, path: &str) -> Result<bool> {
        self.conn
            .execute("DELETE FROM file_origins WHERE path = ?1", params![path])?;
        let removed = self
            .conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(removed > 0)
    }

    /// Replaces every symbol of a file in one transaction.
    pub fn replace_symbols(&self, file_id: i64, symbols: &[NewSymbol]) -> Result<()> {
        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;

        let mut statement = self.conn.prepare(
            "INSERT OR IGNORE INTO symbols
                (file_id, name, qualified_name, kind, signature, start_line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for symbol in symbols {
            statement.execute(params![
                file_id,
                symbol.name,
                symbol.qualified_name,
                symbol.kind,
                symbol.signature,
                symbol.start_line,
                symbol.end_line
            ])?;
        }
        Ok(())
    }

    /// Replaces every import of a file.
    pub fn replace_imports(&self, file_id: i64, imports: &[NewImport]) -> Result<()> {
        self.conn.execute(
            "DELETE FROM imports WHERE source_file = ?1",
            params![file_id],
        )?;

        let mut statement = self.conn.prepare(
            "INSERT INTO imports (source_file, target_file, import_name, line)
             VALUES (?1, (SELECT id FROM files WHERE path = ?2), ?3, ?4)",
        )?;
        for import in imports {
            statement.execute(params![
                file_id,
                import.target_path,
                import.name,
                import.line
            ])?;
        }
        Ok(())
    }

    /// Points an existing import row at its resolved target.
    pub fn resolve_imports(&self, file_id: i64, resolved: &[(String, String)]) -> Result<()> {
        let mut statement = self.conn.prepare(
            "UPDATE imports SET target_file = (SELECT id FROM files WHERE path = ?1)
             WHERE source_file = ?2 AND import_name = ?3",
        )?;
        for (name, target) in resolved {
            statement.execute(params![target, file_id, name])?;
        }
        Ok(())
    }

    /// Replaces the token frequency of a file, so project-wide counts stay incremental.
    pub fn replace_mentions(&self, file_id: i64, mentions: &[(String, u32)]) -> Result<()> {
        self.conn.execute(
            "DELETE FROM file_mentions WHERE file_id = ?1",
            params![file_id],
        )?;

        let mut statement = self
            .conn
            .prepare("INSERT INTO file_mentions (file_id, name, count) VALUES (?1, ?2, ?3)")?;
        for (name, count) in mentions {
            statement.execute(params![file_id, name, *count as i64])?;
        }
        Ok(())
    }

    /// Project-wide token counts, rebuilt from the per-file table.
    pub fn mentions(&self) -> Result<HashMap<String, u32>> {
        let mut statement = self
            .conn
            .prepare("SELECT name, SUM(count) FROM file_mentions GROUP BY name")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
        })?;

        let mut counts = HashMap::new();
        for row in rows {
            let (name, count) = row?;
            counts.insert(name, count);
        }
        Ok(counts)
    }

    /// Every symbol, ordered by file then line.
    pub fn symbols(&self) -> Result<Vec<SymbolRow>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id, s.file_id, s.name, s.qualified_name, s.kind, s.signature,
                    s.start_line, s.end_line
             FROM symbols s JOIN files f ON f.id = s.file_id
             ORDER BY f.path, s.start_line, s.qualified_name",
        )?;
        let rows = statement.query_map([], map_symbol)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Symbols of one file, ordered by line.
    pub fn symbols_of_file(&self, file_id: i64) -> Result<Vec<SymbolRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, file_id, name, qualified_name, kind, signature, start_line, end_line
             FROM symbols WHERE file_id = ?1 ORDER BY start_line, qualified_name",
        )?;
        let rows = statement.query_map(params![file_id], map_symbol)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Full-text symbol search over names, qualified names, and signatures.
    pub fn search_symbols(&self, query: &str, limit: usize) -> Result<Vec<SymbolRow>> {
        let Some(match_query) = fts_query(query) else {
            return Ok(Vec::new());
        };

        let mut statement = self.conn.prepare(
            "SELECT s.id, s.file_id, s.name, s.qualified_name, s.kind, s.signature,
                    s.start_line, s.end_line
             FROM symbols_fts f JOIN symbols s ON s.id = f.rowid
             WHERE symbols_fts MATCH ?1
             ORDER BY f.rank, s.start_line
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![match_query, limit as i64], map_symbol)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Path search, case-insensitive substring.
    pub fn search_files(&self, query: &str, limit: usize) -> Result<Vec<FileRow>> {
        let needle = format!("%{}%", query.to_lowercase());
        let mut statement = self.conn.prepare(
            "SELECT id, path, content_hash, size, modified_at, language, parser_status
             FROM files WHERE lower(path) LIKE ?1 ORDER BY path LIMIT ?2",
        )?;
        let rows = statement.query_map(params![needle, limit as i64], |row| {
            Ok(FileRow {
                id: row.get(0)?,
                path: row.get(1)?,
                content_hash: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                modified_at: row.get(4)?,
                language: row.get(5)?,
                parser_status: row.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Imported names of a file, with resolved target ids when known.
    pub fn imports_from(&self, file_id: i64) -> Result<Vec<(String, Option<i64>, u32)>> {
        let mut statement = self.conn.prepare(
            "SELECT import_name, target_file, line FROM imports
             WHERE source_file = ?1 ORDER BY line, import_name",
        )?;
        let rows = statement.query_map(params![file_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u32))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Files that import `file_id`.
    pub fn imported_by(&self, file_id: i64) -> Result<Vec<i64>> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT source_file FROM imports WHERE target_file = ?1")?;
        let rows = statement.query_map(params![file_id], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Imports whose target is still unknown: `(source file, source path, import name)`.
    ///
    /// The indexer resolves them after every file is stored, so an edge to a file that
    /// sorts later in the alphabet is not lost.
    pub fn unresolved_imports(&self) -> Result<Vec<(i64, String, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT i.source_file, f.path, i.import_name
             FROM imports i JOIN files f ON f.id = i.source_file
             WHERE i.target_file IS NULL
             ORDER BY f.path, i.line",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Resolved edges `(source, target)` over the whole project.
    pub fn edges(&self) -> Result<Vec<(i64, i64)>> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT source_file, target_file FROM imports WHERE target_file IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Symbols whose name or signature mentions `name`.
    ///
    /// Lexical evidence only: this is a text match inside parsed declarations, not a
    /// compiler-resolved call graph (`TOOL.md` §7).
    pub fn references_to(&self, name: &str, limit: usize) -> Result<Vec<SymbolRow>> {
        let exact = format!("%{name}%");
        let mut statement = self.conn.prepare(
            "SELECT s.id, s.file_id, s.name, s.qualified_name, s.kind, s.signature,
                    s.start_line, s.end_line
             FROM symbols s
             WHERE s.name = ?1 OR s.signature LIKE ?2
             ORDER BY s.name = ?1 DESC, s.start_line
             LIMIT ?3",
        )?;
        let rows = statement.query_map(params![name, exact, limit as i64], map_symbol)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Row counts for reports.
    pub fn counts(&self) -> Result<Counts> {
        let files = self.count("files")?;
        let symbols = self.count("symbols")?;
        let imports = self.conn.query_row(
            "SELECT COUNT(*) FROM imports WHERE target_file IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;

        Ok(Counts {
            files,
            symbols,
            imports,
        })
    }

    fn count(&self, table: &str) -> Result<usize> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let count: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Reads a value from `index_state`.
    pub fn state(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM index_state WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Writes a value into `index_state`.
    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO index_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Last rendered map and the generation it belongs to.
    pub fn map_state(&self) -> Result<Option<(String, String, i64)>> {
        self.conn
            .query_row(
                "SELECT text, hash, generation FROM map_state WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)?)),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Stores the rendered map together with a snapshot of the symbols it showed.
    pub fn set_map_state(
        &self,
        text: &str,
        hash: &str,
        generation: i64,
        snapshot: &[(String, String)],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO map_state (id, text, hash, generation, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 text = excluded.text,
                 hash = excluded.hash,
                 generation = excluded.generation,
                 updated_at = excluded.updated_at",
            params![text, hash, generation, now_unix()],
        )?;

        self.conn.execute("DELETE FROM map_symbols", [])?;
        let mut statement = self
            .conn
            .prepare("INSERT OR IGNORE INTO map_symbols (path, qualified_name) VALUES (?1, ?2)")?;
        for (path, qualified_name) in snapshot {
            statement.execute(params![path, qualified_name])?;
        }
        Ok(())
    }

    /// Symbols that were part of the previously rendered map.
    pub fn map_symbols(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT path, qualified_name FROM map_symbols ORDER BY path, qualified_name",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Every `(path, qualified_name)` pair in the index, ordered.
    ///
    /// This is the identity of the project structure: comparing two of these sets gives
    /// the delta between map generations without touching line numbers.
    pub fn symbol_keys(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT f.path, s.qualified_name
             FROM symbols s JOIN files f ON f.id = s.file_id
             ORDER BY f.path, s.qualified_name",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Reads a cached prepared context and bumps its access counters.
    pub fn cache_get(&self, key: &str) -> Result<Option<CacheRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT key, content, token_count, created_at, last_used_at, access_count,
                        generation
                 FROM cache_entries WHERE key = ?1",
                params![key],
                |row| {
                    Ok(CacheRow {
                        key: row.get(0)?,
                        content: row.get(1)?,
                        token_count: row.get::<_, i64>(2)? as usize,
                        created_at: row.get(3)?,
                        last_used_at: row.get(4)?,
                        access_count: row.get::<_, i64>(5)? as u32,
                        generation: row.get(6)?,
                    })
                },
            )
            .optional()?;

        if row.is_some() {
            self.conn.execute(
                "UPDATE cache_entries
                 SET last_used_at = ?1, access_count = access_count + 1
                 WHERE key = ?2",
                params![now_unix(), key],
            )?;
        }

        Ok(row)
    }

    /// Stores a prepared context.
    pub fn cache_put(
        &self,
        key: &str,
        content: &str,
        token_count: usize,
        generation: i64,
    ) -> Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO cache_entries
                (key, content, token_count, created_at, last_used_at, access_count, generation)
             VALUES (?1, ?2, ?3, ?4, ?4, 0, ?5)
             ON CONFLICT(key) DO UPDATE SET
                 content = excluded.content,
                 token_count = excluded.token_count,
                 generation = excluded.generation",
            params![key, content, token_count as i64, now, generation],
        )?;
        Ok(())
    }

    /// Removes one entry and its recorded sources.
    pub fn cache_delete(&self, key: &str) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM cache_entries WHERE key = ?1", params![key])?;
        self.conn
            .execute("DELETE FROM cache_sources WHERE key = ?1", params![key])?;
        Ok(removed > 0)
    }

    /// Removes every entry and every recorded source.
    pub fn cache_clear(&self) -> Result<usize> {
        let removed = self.conn.execute("DELETE FROM cache_entries", [])?;
        self.conn.execute("DELETE FROM cache_sources", [])?;
        Ok(removed)
    }

    /// Entry count, stored bytes, and total accesses.
    pub fn cache_totals(&self) -> Result<CacheTotals> {
        self.conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(content)), 0), COALESCE(SUM(access_count), 0)
                 FROM cache_entries",
                [],
                |row| {
                    Ok(CacheTotals {
                        entries: row.get::<_, i64>(0)? as usize,
                        bytes: row.get::<_, i64>(1)? as usize,
                        accesses: row.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .map_err(StoreError::from)
    }

    /// Removes entries not used since `older_than` (Unix seconds).
    pub fn cache_prune(&self, older_than: i64) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM cache_entries WHERE last_used_at < ?1",
            params![older_than],
        )?;
        self.conn.execute(
            "DELETE FROM cache_sources WHERE key NOT IN (SELECT key FROM cache_entries)",
            [],
        )?;
        Ok(removed)
    }

    /// Paths a cached artifact was built from.
    pub fn cache_sources(&self, key: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM cache_sources WHERE key = ?1 ORDER BY path")?;
        let rows = statement.query_map(params![key], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<String>, _>>()
            .map_err(StoreError::from)
    }

    /// Records the files a cached artifact was built from, with their current hashes.
    pub fn cache_set_sources(&self, key: &str, paths: &[String]) -> Result<()> {
        self.conn
            .execute("DELETE FROM cache_sources WHERE key = ?1", params![key])?;

        for path in paths {
            let hash: Option<String> = self
                .conn
                .query_row(
                    "SELECT content_hash FROM files WHERE path = ?1",
                    params![path],
                    |row| row.get(0),
                )
                .optional()?;
            self.conn.execute(
                "INSERT OR REPLACE INTO cache_sources (key, path, hash) VALUES (?1, ?2, ?3)",
                params![key, path, hash.unwrap_or_default()],
            )?;
        }

        Ok(())
    }

    /// Whether any recorded source changed or disappeared since the entry was built.
    ///
    /// This is what keeps an unrelated entry valid across a generation bump, and what
    /// kills an entry whose file was rewritten behind the index's back.
    pub fn cache_sources_stale(&self, key: &str) -> Result<bool> {
        let stale: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM cache_sources s
             LEFT JOIN files f ON f.path = s.path
             WHERE s.key = ?1 AND (f.content_hash IS NULL OR f.content_hash <> s.hash)",
            params![key],
            |row| row.get(0),
        )?;
        Ok(stale > 0)
    }

    /// Drops entries built from any of `paths`; unrelated entries are kept.
    pub fn cache_invalidate_paths(&self, paths: &[String]) -> Result<usize> {
        let mut removed = 0;

        for path in paths {
            removed += self.conn.execute(
                "DELETE FROM cache_entries
                 WHERE key IN (SELECT key FROM cache_sources WHERE path = ?1)",
                params![path],
            )?;
        }

        self.conn.execute(
            "DELETE FROM cache_sources WHERE key NOT IN (SELECT key FROM cache_entries)",
            [],
        )?;

        Ok(removed)
    }

    /// Current index generation, bumped whenever indexing changes anything.
    pub fn generation(&self) -> Result<i64> {
        Ok(self
            .state("generation")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    /// Increments the index generation, invalidating every cached context key.
    pub fn bump_generation(&self) -> Result<i64> {
        let next = self.generation()? + 1;
        self.set_state("generation", &next.to_string())?;
        Ok(next)
    }

    /// Parser version that wrote the stored symbols, if any.
    pub fn parser_version(&self) -> Result<Option<String>> {
        self.state("parser_version")
    }
}

fn map_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        name: row.get(2)?,
        qualified_name: row.get(3)?,
        kind: row.get(4)?,
        signature: row.get(5)?,
        start_line: row.get::<_, i64>(6)? as u32,
        end_line: row.get::<_, i64>(7)? as u32,
    })
}

/// Current time as Unix seconds; a clock before 1970 counts as zero.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Turns free text into an FTS5 query: each token is quoted and prefix-matched.
///
/// Quoting keeps FTS operators out of user input, so a query can never be a syntax
/// error or an injection into the MATCH expression.
fn fts_query(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"*", token.replace('"', "")))
        .collect();

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id            INTEGER PRIMARY KEY,
    path          TEXT    NOT NULL UNIQUE,
    content_hash  TEXT    NOT NULL,
    size          INTEGER NOT NULL,
    modified_at   INTEGER NOT NULL,
    language      TEXT    NOT NULL,
    parser_status TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS symbols (
    id             INTEGER PRIMARY KEY,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name           TEXT    NOT NULL,
    qualified_name TEXT    NOT NULL,
    kind           TEXT    NOT NULL,
    signature      TEXT    NOT NULL,
    start_line     INTEGER NOT NULL,
    end_line       INTEGER NOT NULL,
    UNIQUE (file_id, kind, qualified_name)
);

CREATE INDEX IF NOT EXISTS symbols_file ON symbols(file_id);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name);

CREATE TABLE IF NOT EXISTS imports (
    id          INTEGER PRIMARY KEY,
    source_file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    target_file INTEGER REFERENCES files(id) ON DELETE SET NULL,
    import_name TEXT    NOT NULL,
    line        INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS imports_source ON imports(source_file);
CREATE INDEX IF NOT EXISTS imports_target ON imports(target_file);

CREATE TABLE IF NOT EXISTS file_mentions (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name    TEXT    NOT NULL,
    count   INTEGER NOT NULL,
    PRIMARY KEY (file_id, name)
);

CREATE TABLE IF NOT EXISTS map_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    text       TEXT    NOT NULL,
    hash       TEXT    NOT NULL,
    generation INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS map_symbols (
    path           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    PRIMARY KEY (path, qualified_name)
);

CREATE TABLE IF NOT EXISTS index_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    name,
    qualified_name,
    signature,
    content = 'symbols',
    content_rowid = 'id'
);

CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
    INSERT INTO symbols_fts (rowid, name, qualified_name, signature)
    VALUES (new.id, new.name, new.qualified_name, new.signature);
END;

CREATE TRIGGER IF NOT EXISTS symbols_ad AFTER DELETE ON symbols BEGIN
    INSERT INTO symbols_fts (symbols_fts, rowid, name, qualified_name, signature)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.signature);
END;

CREATE TRIGGER IF NOT EXISTS symbols_au AFTER UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts (symbols_fts, rowid, name, qualified_name, signature)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.signature);
    INSERT INTO symbols_fts (rowid, name, qualified_name, signature)
    VALUES (new.id, new.name, new.qualified_name, new.signature);
END;
"#;

/// Working cache for prepared contexts (`TOOL.md` extension §7.2).
///
/// A separate table, not a copy of the index: it stores assembled context strings keyed by
/// the request and the index generation, so any source change invalidates it wholesale.
const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS cache_entries (
    key          TEXT PRIMARY KEY,
    content      TEXT    NOT NULL,
    token_count  INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    access_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS cache_last_used ON cache_entries(last_used_at);
"#;

/// Targeted invalidation (`prirote.md` §8): every entry knows the files it was built
/// from, so a source change drops exactly the affected entries instead of the whole cache.
const SCHEMA_V3: &str = r#"
ALTER TABLE cache_entries ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS cache_sources (
    key  TEXT NOT NULL,
    path TEXT NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY (key, path)
);

CREATE INDEX IF NOT EXISTS cache_sources_path ON cache_sources(path);
"#;

/// Per-file provenance used by the user backup (`index2`): `user` files are copied,
/// `ai` files are not, so `/back` can undo the model without copying its output.
const SCHEMA_V4: &str = r#"
CREATE TABLE IF NOT EXISTS file_origins (
    path   TEXT PRIMARY KEY,
    origin TEXT NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(path: &str, hash: &str) -> FileMeta {
        FileMeta {
            path: path.to_owned(),
            content_hash: hash.to_owned(),
            size: 10,
            modified_at: 1,
            language: "rust".to_owned(),
            parser_status: "parsed".to_owned(),
        }
    }

    fn symbol(name: &str, qualified: &str) -> NewSymbol {
        NewSymbol {
            name: name.to_owned(),
            qualified_name: qualified.to_owned(),
            kind: "fn".to_owned(),
            signature: format!("pub fn {name}()"),
            start_line: 1,
            end_line: 3,
        }
    }

    #[test]
    fn stores_files_with_stable_ids() -> Result<()> {
        let store = Store::open_in_memory()?;
        let first = store.upsert_file(&meta("src/a.rs", "h1"))?;
        let second = store.upsert_file(&meta("src/a.rs", "h2"))?;

        assert_eq!(first, second, "an edit must not change the file id");
        assert_eq!(store.files()?.len(), 1);
        assert_eq!(store.file("src/a.rs")?.expect("file").content_hash, "h2");
        Ok(())
    }

    #[test]
    fn deleting_a_file_cascades_to_symbols_and_mentions() -> Result<()> {
        let store = Store::open_in_memory()?;
        let id = store.upsert_file(&meta("src/a.rs", "h1"))?;
        store.replace_symbols(id, &[symbol("alpha", "alpha")])?;
        store.replace_mentions(id, &[("alpha".to_owned(), 1)])?;
        store.replace_imports(
            id,
            &[NewImport {
                name: "std::fmt".to_owned(),
                line: 1,
                target_path: None,
            }],
        )?;

        assert!(store.delete_file("src/a.rs")?);
        assert_eq!(store.counts()?.symbols, 0);
        assert!(store.mentions()?.is_empty());
        assert_eq!(store.imports_from(id)?.len(), 0);
        Ok(())
    }

    #[test]
    fn finds_symbols_through_fts() -> Result<()> {
        let store = Store::open_in_memory()?;
        let id = store.upsert_file(&meta("src/auth.rs", "h1"))?;
        store.replace_symbols(
            id,
            &[
                symbol("authenticate_request", "Auth::authenticate_request"),
                symbol("logout", "Auth::logout"),
            ],
        )?;

        let hits = store.search_symbols("authenticate", 10)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "authenticate_request");

        assert!(store.search_symbols("!!", 10)?.is_empty());
        assert_eq!(store.search_symbols("auth", 10)?.len(), 2);
        Ok(())
    }

    #[test]
    fn resolves_import_targets_to_file_ids() -> Result<()> {
        let store = Store::open_in_memory()?;
        let auth = store.upsert_file(&meta("src/auth.rs", "h1"))?;
        let main = store.upsert_file(&meta("src/main.rs", "h2"))?;
        store.replace_imports(
            main,
            &[NewImport {
                name: "crate::auth".to_owned(),
                line: 3,
                target_path: Some("src/auth.rs".to_owned()),
            }],
        )?;

        let edges = store.edges()?;
        assert_eq!(edges, vec![(main, auth)]);
        assert_eq!(store.imported_by(auth)?, vec![main]);
        assert_eq!(store.imports_from(main)?.len(), 1);
        Ok(())
    }

    #[test]
    fn keeps_symbol_snapshots_for_deltas() -> Result<()> {
        let store = Store::open_in_memory()?;
        store.set_map_state(
            "map",
            "hash",
            7,
            &[("src/a.rs".to_owned(), "alpha".to_owned())],
        )?;

        let (text, hash, generation) = store.map_state()?.expect("state");
        assert_eq!(
            (text.as_str(), hash.as_str(), generation),
            ("map", "hash", 7)
        );
        assert_eq!(
            store.map_symbols()?,
            vec![("src/a.rs".to_owned(), "alpha".to_owned())]
        );
        Ok(())
    }

    #[test]
    fn refuses_a_database_from_a_newer_build() -> Result<()> {
        let store = Store::open_in_memory()?;
        store.conn.execute_batch("PRAGMA user_version = 99;")?;

        let error = store.migrate().expect_err("newer schema must be refused");
        assert!(matches!(error, StoreError::NewerSchema { found: 99, .. }));
        Ok(())
    }

    #[test]
    fn sums_mentions_across_files() -> Result<()> {
        let store = Store::open_in_memory()?;
        let a = store.upsert_file(&meta("a.rs", "h1"))?;
        let b = store.upsert_file(&meta("b.rs", "h2"))?;
        store.replace_mentions(a, &[("alpha".to_owned(), 2)])?;
        store.replace_mentions(b, &[("alpha".to_owned(), 3), ("beta".to_owned(), 1)])?;

        let mentions = store.mentions()?;
        assert_eq!(mentions.get("alpha"), Some(&5));
        assert_eq!(mentions.get("beta"), Some(&1));
        Ok(())
    }
}
