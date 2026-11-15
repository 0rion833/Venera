//! `PathGuard` — the single gate for filesystem paths.
//!
//! Everything must resolve inside the project root; symlinks pointing outside are blocked.
//! Every path used by `crate::tools` goes through this type; a direct `std::fs::read(path)`
//! from the tools is a review blocker (`AGENTS.md` §2, §6).
//!
//! The guard works by canonicalisation: a path is joined to the root, resolved through the
//! filesystem (which follows symlinks), and then checked to still start with the canonical
//! root. A symlink that points outside therefore fails the check instead of silently
//! escaping.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// A path that cannot be used safely.
#[derive(Debug, Error)]
pub enum GuardError {
    /// The resolved path lies outside the project root.
    #[error("path `{path}` escapes the project root `{root}`")]
    Escape {
        /// The offending path after resolution.
        path: PathBuf,
        /// The canonical project root.
        root: PathBuf,
    },

    /// The filesystem could not resolve the path.
    #[error("cannot resolve `{path}`: {source}")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// Confines every path to one canonical root directory.
#[derive(Debug, Clone)]
pub struct PathGuard {
    root: PathBuf,
}

impl PathGuard {
    /// Canonicalises `root` and returns a guard pinned to it.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, GuardError> {
        let root = root.as_ref();
        let root = root.canonicalize().map_err(|source| GuardError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        Ok(Self { root })
    }

    /// The canonical root every path must stay inside.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `path` against the root and proves the result is still inside it.
    ///
    /// Absolute paths are accepted only when they already lie inside the root.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, GuardError> {
        let path = path.as_ref();
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let resolved = joined.canonicalize().map_err(|source| GuardError::Io {
            path: joined,
            source,
        })?;

        if resolved.starts_with(&self.root) {
            Ok(resolved)
        } else {
            Err(GuardError::Escape {
                path: resolved,
                root: self.root.clone(),
            })
        }
    }

    /// Checks a path that may not exist yet, without touching the filesystem.
    ///
    /// `..` components are rejected outright: a lexical escape is a bug even when the
    /// target happens to exist. The result is only guaranteed to be safe lexically;
    /// existing paths must still go through [`resolve`](Self::resolve) before I/O.
    pub fn check_lexical(&self, path: impl AsRef<Path>) -> Result<PathBuf, GuardError> {
        let path = path.as_ref();

        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(GuardError::Escape {
                path: path.to_path_buf(),
                root: self.root.clone(),
            });
        }

        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        if joined.starts_with(&self.root) {
            Ok(joined)
        } else {
            Err(GuardError::Escape {
                path: joined,
                root: self.root.clone(),
            })
        }
    }

    /// Path relative to the root, using forward slashes for stable display.
    pub fn relative(&self, path: impl AsRef<Path>) -> Option<String> {
        let path = path.as_ref();
        let stripped = path.strip_prefix(&self.root).ok()?;
        let text = stripped
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> (tempfile::TempDir, PathGuard) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        let guard = PathGuard::new(dir.path()).expect("guard");
        (dir, guard)
    }

    #[test]
    fn resolves_paths_inside_the_root() {
        let (_dir, guard) = guard();
        let resolved = guard.resolve("src/main.rs").expect("inside");
        assert!(resolved.ends_with("main.rs"));
        assert_eq!(guard.relative(&resolved).as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn rejects_escapes_through_parent_components() {
        let (_dir, guard) = guard();
        for path in [
            "../../etc/passwd",
            "src/../../outside.txt",
            "src/../inside.txt",
            "../outside.txt",
        ] {
            let err = guard.check_lexical(path).expect_err("escape");
            assert!(matches!(err, GuardError::Escape { .. }), "path: {path}");
        }
    }

    #[test]
    fn rejects_parent_components_below_the_root() {
        let (_dir, guard) = guard();
        let err = guard
            .check_lexical("src/../main.rs")
            .expect_err("parent component");
        assert!(matches!(err, GuardError::Escape { .. }));
    }

    #[test]
    fn lexical_check_allows_plain_relative_paths() {
        let (_dir, guard) = guard();
        let checked = guard.check_lexical("src/main.rs").expect("plain path");
        assert_eq!(checked, guard.root().join("src/main.rs"));
        assert_eq!(
            guard.check_lexical("new/file.rs").unwrap(),
            guard.root().join("new/file.rs")
        );
    }

    #[test]
    fn rejects_paths_that_do_not_exist() {
        let (_dir, guard) = guard();
        let err = guard.resolve("src/missing.rs").expect_err("missing");
        assert!(matches!(err, GuardError::Io { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_pointing_outside() {
        let (dir, guard) = guard();
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");

        let err = guard.resolve("link").expect_err("symlink escape");
        assert!(matches!(err, GuardError::Escape { .. }));
    }
}
