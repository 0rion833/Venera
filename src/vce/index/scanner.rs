//! Project scanning: discover eligible files, hash them, and keep the source text.
//!
//! The scanner never modifies the project and never follows a symlink (`TOOL.md` §4).
//! Every path it touches goes through [`PathGuard`]. Results are sorted by relative path
//! so two runs over the same tree produce byte-identical output.
//!
//! Source text is kept in memory because the first milestone only measures token
//! economics; persistence in SQLite arrives with the database layer. Files larger than
//! [`DEFAULT_MAX_FILE_SIZE`] are skipped, which bounds memory use.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::security::path_guard::{GuardError, PathGuard};
use crate::vce::index::language::Language;

/// Files above this size are not indexed by default.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;

/// Directories that never contain project source worth indexing.
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".venera",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    ".cache",
    "coverage",
    ".idea",
    ".vscode",
    "__pycache__",
];

/// One indexed file.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Path relative to the project root, with forward slashes.
    pub rel_path: String,
    /// Detected language.
    pub language: Language,
    /// Size on disk in bytes.
    pub bytes: u64,
    /// Number of lines in the text.
    pub lines: u32,
    /// BLAKE3 content hash, hex encoded.
    pub hash: String,
    /// Full source text.
    pub text: String,
}

/// Why a discovered file was left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The file looks binary.
    Binary,
    /// The file exceeds the configured size limit.
    TooLarge,
    /// The entry is a symlink and is not followed.
    Symlink,
    /// The file could not be read.
    Unreadable,
}

impl SkipReason {
    /// Short stable name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::TooLarge => "too large",
            Self::Symlink => "symlink",
            Self::Unreadable => "unreadable",
        }
    }
}

/// A file that was discovered but not indexed.
#[derive(Debug, Clone)]
pub struct Skipped {
    /// Path relative to the project root.
    pub rel_path: String,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// Result of scanning a project.
#[derive(Debug, Clone)]
pub struct Scan {
    /// Canonical project root.
    pub root: PathBuf,
    /// Indexed files, sorted by relative path.
    pub files: Vec<SourceFile>,
    /// Discovered files that were skipped.
    pub skipped: Vec<Skipped>,
}

impl Scan {
    /// Total size of all indexed files in bytes.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    /// Total number of indexed lines.
    pub fn total_lines(&self) -> u64 {
        self.files.iter().map(|file| u64::from(file.lines)).sum()
    }
}

/// A scan failure that stops the whole run.
#[derive(Debug, Error)]
pub enum ScanError {
    /// The root could not be guarded.
    #[error(transparent)]
    Guard(#[from] GuardError),

    /// A directory could not be read.
    #[error("cannot read directory `{path}`: {source}")]
    Io {
        /// The directory that failed.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// Walks `root` and indexes every eligible file.
pub fn scan(root: impl AsRef<Path>, max_file_size: u64) -> Result<Scan, ScanError> {
    let guard = PathGuard::new(root)?;
    let root = guard.root().to_path_buf();

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut pending = vec![root.clone()];

    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|source| ScanError::Io {
            path: dir.clone(),
            source,
        })?;

        for entry in entries {
            let entry = entry.map_err(|source| ScanError::Io {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            let Some(rel_path) = guard.relative(&path) else {
                continue;
            };

            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    skipped.push(Skipped {
                        rel_path,
                        reason: SkipReason::Unreadable,
                    });
                    continue;
                }
            };

            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                skipped.push(Skipped {
                    rel_path,
                    reason: SkipReason::Symlink,
                });
                continue;
            }

            if file_type.is_dir() {
                if !is_excluded_dir(&rel_path) {
                    pending.push(path);
                }
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            match read_file(&path, &rel_path, metadata.len(), max_file_size) {
                Ok(file) => files.push(file),
                Err(reason) => skipped.push(Skipped { rel_path, reason }),
            }
        }
    }

    files.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    skipped.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));

    Ok(Scan {
        root,
        files,
        skipped,
    })
}

fn is_excluded_dir(rel_path: &str) -> bool {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    excluded_dir(name)
}

/// Whether a directory name is excluded from indexing.
///
/// Shared with the incremental indexer, so a rebuild and an update agree on what the
/// project even contains.
pub fn excluded_dir(name: &str) -> bool {
    EXCLUDED_DIRS.contains(&name)
}

fn read_file(
    path: &Path,
    rel_path: &str,
    size: u64,
    max_file_size: u64,
) -> Result<SourceFile, SkipReason> {
    if size > max_file_size {
        return Err(SkipReason::TooLarge);
    }

    let bytes = std::fs::read(path).map_err(|_| SkipReason::Unreadable)?;
    if looks_binary(&bytes) {
        return Err(SkipReason::Binary);
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();
    let hash = blake3::hash(&bytes).to_hex().to_string();

    Ok(SourceFile {
        rel_path: rel_path.to_owned(),
        language: Language::from_path(rel_path),
        bytes: size,
        lines: text.lines().count() as u32,
        hash,
        text,
    })
}

/// A NUL byte in the first few kilobytes is the cheap, reliable binary signal.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).expect("mkdir deps");
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("write rs");
        std::fs::write(dir.path().join("style.css"), "body { color: red; }\n").expect("write css");
        std::fs::write(dir.path().join("node_modules/pkg/index.js"), "x\n").expect("write dep");
        std::fs::write(dir.path().join("logo.png"), [0u8, 1, 2, 3]).expect("write bin");
        dir
    }

    #[test]
    fn indexes_source_and_skips_dependencies_and_binaries() {
        let dir = project();
        let scan = scan(dir.path(), DEFAULT_MAX_FILE_SIZE).expect("scan");
        let paths: Vec<&str> = scan
            .files
            .iter()
            .map(|file| file.rel_path.as_str())
            .collect();

        assert_eq!(paths, vec!["src/main.rs", "style.css"]);
        assert!(
            scan.skipped
                .iter()
                .any(|item| { item.rel_path == "logo.png" && item.reason == SkipReason::Binary })
        );
        assert!(!paths.iter().any(|path| path.contains("node_modules")));
    }

    #[test]
    fn produces_stable_hashes_and_detects_changes() {
        let dir = project();
        let first = scan(dir.path(), DEFAULT_MAX_FILE_SIZE).expect("scan");
        let second = scan(dir.path(), DEFAULT_MAX_FILE_SIZE).expect("scan");

        assert_eq!(first.files[0].hash, second.files[0].hash);

        std::fs::write(dir.path().join("src/main.rs"), "fn main() { run(); }\n").expect("rewrite");
        let third = scan(dir.path(), DEFAULT_MAX_FILE_SIZE).expect("scan");
        assert_ne!(first.files[0].hash, third.files[0].hash);
    }

    #[test]
    fn skips_files_above_the_size_limit() {
        let dir = project();
        let scan = scan(dir.path(), 4).expect("scan");

        assert!(scan.files.is_empty());
        assert!(
            scan.skipped.iter().any(|item| {
                item.rel_path == "src/main.rs" && item.reason == SkipReason::TooLarge
            })
        );
    }
}
