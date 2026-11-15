//! Project picker: a small directory browser.
//!
//! `Enter` opens the highlighted directory as the project, `→`/`Tab` walks into it,
//! `←`/`Backspace` walks up, `Esc` closes. Only directories are listed; hidden and build
//! directories are skipped.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories never worth showing as a project.
const SKIP: &[&str] = &[".git", ".venera", "target", "node_modules", ".idea", ".vs"];

/// One row of the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    /// Directory the row points at.
    pub path: PathBuf,
    /// Short name shown in the list.
    pub label: String,
    /// What the directory looks like: `rust`, `node`, `папка`.
    pub detail: String,
    /// True for the `..` row.
    pub parent: bool,
}

/// Directory listing with a movable selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPicker {
    dir: PathBuf,
    rows: Vec<ProjectRow>,
    selected: usize,
}

impl ProjectPicker {
    /// Opens the listing at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let mut picker = Self {
            dir: dir.into(),
            rows: Vec::new(),
            selected: 0,
        };
        picker.reload();
        picker
    }

    /// Current directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Rows of the current directory.
    pub fn rows(&self) -> &[ProjectRow] {
        &self.rows
    }

    /// Highlighted row.
    pub fn selected(&self) -> Option<&ProjectRow> {
        self.rows.get(self.selected)
    }

    /// Index of the highlighted row.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Moves the highlight down, wrapping.
    pub fn next(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1) % self.rows.len();
        }
    }

    /// Moves the highlight up, wrapping.
    pub fn previous(&mut self) {
        let len = self.rows.len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    /// Enters the highlighted directory; the `..` row goes up.
    pub fn descend(&mut self) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        if row.parent {
            self.go_up();
            return;
        }
        self.dir = row.path;
        self.selected = 0;
        self.reload();
    }

    /// Moves the listing to the parent directory.
    pub fn go_up(&mut self) {
        if let Some(parent) = self.dir.parent() {
            self.dir = parent.to_path_buf();
            self.selected = 0;
            self.reload();
        }
    }

    /// Directory Enter opens; `None` when the `..` row is selected.
    pub fn target(&self) -> Option<PathBuf> {
        let row = self.selected()?;
        if row.parent {
            None
        } else {
            Some(row.path.clone())
        }
    }

    fn reload(&mut self) {
        let mut rows = Vec::new();

        if let Some(parent) = self.dir.parent() {
            rows.push(ProjectRow {
                path: parent.to_path_buf(),
                label: "..".to_owned(),
                detail: "наверх".to_owned(),
                parent: true,
            });
        }

        let mut dirs: Vec<PathBuf> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && visible(path))
            .collect();
        dirs.sort();

        for path in dirs {
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            let detail = marker(&path);
            rows.push(ProjectRow {
                path,
                label,
                detail,
                parent: false,
            });
        }

        self.rows = rows;
        self.selected = 0;
    }
}

/// Hidden and build directories are noise in a project list.
fn visible(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| !name.starts_with('.') && !SKIP.contains(&name))
}

/// A tiny marker instead of a full language detector.
fn marker(path: &Path) -> String {
    for (file, label) in [
        (".git", "git"),
        ("Cargo.toml", "rust"),
        ("package.json", "node"),
        ("pyproject.toml", "python"),
        ("go.mod", "go"),
    ] {
        if path.join(file).exists() {
            return label.to_owned();
        }
    }

    "папка".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for name in ["beta", "alpha", "target", ".git"] {
            std::fs::create_dir_all(dir.path().join(name)).expect("mkdir");
        }
        std::fs::write(dir.path().join("alpha/Cargo.toml"), "[package]\n").expect("file");
        dir
    }

    #[test]
    fn lists_visible_directories_with_the_parent_first() {
        let dir = dir();
        let picker = ProjectPicker::new(dir.path());

        let labels: Vec<&str> = picker.rows().iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels.first(), Some(&".."));
        assert!(labels.contains(&"alpha"));
        assert!(labels.contains(&"beta"));
        assert!(!labels.contains(&"target"), "build dirs are skipped");
        assert!(!labels.contains(&".git"), "hidden dirs are skipped");
    }

    #[test]
    fn arrows_wrap_and_enter_targets_a_directory() {
        let dir = dir();
        let mut picker = ProjectPicker::new(dir.path());
        let count = picker.rows().len();

        picker.previous();
        assert_eq!(picker.selected_index(), count - 1);

        picker.next();
        assert_eq!(picker.selected_index(), 0);
        assert!(picker.target().is_none(), "the `..` row has no target");

        picker.next();
        assert!(picker.target().is_some());
    }

    #[test]
    fn descending_and_going_up_change_the_listing() {
        let dir = dir();
        let mut picker = ProjectPicker::new(dir.path());
        let alpha = dir.path().join("alpha");

        let index = picker
            .rows()
            .iter()
            .position(|row| row.path == alpha)
            .expect("alpha row");
        for _ in 0..index {
            picker.next();
        }
        picker.descend();
        assert_eq!(picker.dir(), alpha);

        picker.go_up();
        assert_eq!(picker.dir(), dir.path());
    }
}
