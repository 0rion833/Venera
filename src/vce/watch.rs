//! A small cross-platform poller for user edits.
//!
//! Native filesystem notifications are fast but differ per OS and need another dependency.
//! A one-second metadata walk is enough here: it catches files the user creates or edits
//! outside Venera, while AI writes are already reindexed before the next tick and therefore
//! look unchanged to this poller.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, UNIX_EPOCH};

use crate::vce::index::scanner;

/// Poll interval. Long enough to stay cheap, short enough to feel immediate.
const INTERVAL: Duration = Duration::from_secs(1);

/// Extra directories that are never user source.
const EXTRA_SKIPPED: &[&str] = &[".venv", "venv", ".pytest_cache"];

/// One poller thread.
pub struct Watcher {
    receiver: Receiver<Vec<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// Starts polling `root`; the initial scan is the baseline and produces no events.
    pub fn spawn(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (sender, receiver) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            let mut previous = scan(&root);
            while !worker_stop.load(Ordering::Relaxed) {
                std::thread::sleep(INTERVAL);
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                let current = scan(&root);
                let changed = diff(&previous, &current);
                previous = current;
                if !changed.is_empty() && sender.send(changed).is_err() {
                    break;
                }
            }
        });

        Self {
            receiver,
            stop,
            handle: Some(handle),
        }
    }

    /// Drains all pending changed paths, without duplicates.
    pub fn take_events(&self) -> Vec<String> {
        let mut paths = BTreeSet::new();
        while let Ok(changed) = self.receiver.try_recv() {
            paths.extend(changed);
        }
        paths.into_iter().collect()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn scan(root: &Path) -> HashMap<String, (u64, i64)> {
    let mut files = HashMap::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if !scanner::excluded_dir(name) && !EXTRA_SKIPPED.contains(&name) {
                    pending.push(path);
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let Some(relative) = relative(root, &path) else {
                continue;
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos() as i64)
                .unwrap_or_default();
            files.insert(relative, (metadata.len(), modified));
        }
    }

    files
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let stripped = path.strip_prefix(root).ok()?;
    Some(
        stripped
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn diff(
    previous: &HashMap<String, (u64, i64)>,
    current: &HashMap<String, (u64, i64)>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, signature) in current {
        if previous.get(path) != Some(signature) {
            changed.push(path.clone());
        }
    }
    for path in previous.keys() {
        if !current.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed.dedup();
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_created_modified_and_deleted_files() {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(dir.path().join("a.txt"), "one").expect("write");
        let first = scan(dir.path());
        assert!(first.contains_key("a.txt"));

        std::fs::write(dir.path().join("b.txt"), "two").expect("write");
        let second = scan(dir.path());
        let changed = diff(&first, &second);
        assert_eq!(changed, vec!["b.txt"]);

        std::fs::remove_file(dir.path().join("a.txt")).expect("remove");
        let third = scan(dir.path());
        assert_eq!(diff(&second, &third), vec!["a.txt"]);
    }

    #[test]
    fn ignores_internal_and_dependency_directories() {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::create_dir_all(dir.path().join(".venera")).expect("mkdir");
        std::fs::write(dir.path().join(".venera/index.db"), "db").expect("write");
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).expect("mkdir");
        std::fs::write(dir.path().join("node_modules/pkg/a.js"), "x").expect("write");

        let files = scan(dir.path());
        assert!(!files.keys().any(|path| path.starts_with(".venera/")));
        assert!(!files.keys().any(|path| path.starts_with("node_modules/")));
    }
}
