//! Delta project map: what changed since the last rendered map (`TOOL.md` §15).
//!
//! The full map stays the source of truth. A delta is only a hint about movement: it is
//! built from the symbol snapshot stored next to the previous map, so it costs one query
//! and no parsing.
//!
//! The comparison key is `(path, qualified_name)` — the same identity the index uses.
//! Line numbers are deliberately ignored: moving code around must not look like a change
//! of the project structure.

use std::collections::BTreeSet;

use crate::vce::index::m1::{Store, StoreError};

/// A structural difference between two map generations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delta {
    /// Symbols in the current index that the previous map did not show.
    pub added: Vec<(String, String)>,
    /// Symbols the previous map showed that no longer exist.
    pub removed: Vec<(String, String)>,
    /// Files whose symbol set is unchanged.
    pub unchanged_files: usize,
    /// Files with at least one addition or removal.
    pub changed_files: usize,
    /// Generation of the map the delta was computed against.
    pub previous_generation: i64,
}

impl Delta {
    /// Whether nothing structural changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// Renderable form, sized by item count.
    ///
    /// `max_items` caps each list separately; the rest is reported as a count, so the
    /// text never silently hides work.
    pub fn render(&self, max_items: usize) -> String {
        let mut text = String::from("PROJECT DELTA\n");

        if self.is_empty() {
            text.push_str("no structural changes\n");
            text.push_str(&format!("unchanged: {} files\n", self.unchanged_files));
            return text;
        }

        let mut files: BTreeSet<&str> = BTreeSet::new();
        for (path, _) in &self.added {
            files.insert(path);
        }
        for (path, _) in &self.removed {
            files.insert(path);
        }

        for path in &files {
            let added: Vec<&str> = self
                .added
                .iter()
                .filter(|(candidate, _)| candidate == path)
                .map(|(_, name)| name.as_str())
                .collect();
            let removed: Vec<&str> = self
                .removed
                .iter()
                .filter(|(candidate, _)| candidate == path)
                .map(|(_, name)| name.as_str())
                .collect();

            text.push_str(path);
            text.push('\n');
            push_items(&mut text, '+', &added, max_items);
            push_items(&mut text, '-', &removed, max_items);
        }

        text.push_str(&format!(
            "changed: {} files, unchanged: {} files\n",
            self.changed_files, self.unchanged_files
        ));
        text
    }
}

fn push_items(text: &mut String, marker: char, items: &[&str], max_items: usize) {
    for item in items.iter().take(max_items) {
        text.push_str(&format!("  {marker} {item}\n"));
    }
    if items.len() > max_items {
        text.push_str(&format!("  {marker} … {} more\n", items.len() - max_items));
    }
}

/// Compares the current index with the snapshot of the previous map.
///
/// `None` means no map has been rendered yet, so there is nothing to compare against.
pub fn compute(store: &Store) -> Result<Option<Delta>, StoreError> {
    let Some((_, _, generation)) = store.map_state()? else {
        return Ok(None);
    };

    let previous: BTreeSet<(String, String)> = store.map_symbols()?.into_iter().collect();
    let current: BTreeSet<(String, String)> = store.symbol_keys()?.into_iter().collect();

    let added: Vec<(String, String)> = current.difference(&previous).cloned().collect();
    let removed: Vec<(String, String)> = previous.difference(&current).cloned().collect();

    let mut changed: BTreeSet<String> = BTreeSet::new();
    for (path, _) in added.iter().chain(removed.iter()) {
        changed.insert(path.clone());
    }

    let all_files: BTreeSet<&str> = current
        .iter()
        .chain(previous.iter())
        .map(|(path, _)| path.as_str())
        .collect();

    Ok(Some(Delta {
        added,
        removed,
        unchanged_files: all_files.len().saturating_sub(changed.len()),
        changed_files: changed.len(),
        previous_generation: generation,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(previous: &[(&str, &str)]) -> Store {
        let store = Store::open_in_memory().expect("store");
        let snapshot: Vec<(String, String)> = previous
            .iter()
            .map(|(path, name)| ((*path).to_owned(), (*name).to_owned()))
            .collect();
        store
            .set_map_state("map", "hash", 3, &snapshot)
            .expect("state");
        store
    }

    fn index_file(store: &Store, path: &str, symbols: &[&str]) {
        let meta = crate::vce::index::m1::FileMeta {
            path: path.to_owned(),
            content_hash: format!("hash-{path}"),
            size: 10,
            modified_at: 1,
            language: "rust".to_owned(),
            parser_status: "parsed".to_owned(),
        };
        let id = store.upsert_file(&meta).expect("file");
        let rows: Vec<crate::vce::index::m1::NewSymbol> = symbols
            .iter()
            .map(|name| crate::vce::index::m1::NewSymbol {
                name: (*name).to_owned(),
                qualified_name: (*name).to_owned(),
                kind: "fn".to_owned(),
                signature: format!("fn {name}()"),
                start_line: 1,
                end_line: 1,
            })
            .collect();
        store.replace_symbols(id, &rows).expect("symbols");
    }

    #[test]
    fn there_is_no_delta_without_a_previous_map() {
        let store = Store::open_in_memory().expect("store");
        assert!(compute(&store).expect("delta").is_none());
    }

    #[test]
    fn detects_additions_and_removals() {
        let store = store_with(&[("src/a.rs", "old"), ("src/a.rs", "kept")]);
        index_file(&store, "src/a.rs", &["kept", "fresh"]);

        let delta = compute(&store).expect("delta").expect("generation");

        assert_eq!(
            delta.added,
            vec![("src/a.rs".to_owned(), "fresh".to_owned())]
        );
        assert_eq!(
            delta.removed,
            vec![("src/a.rs".to_owned(), "old".to_owned())]
        );
        assert_eq!(delta.changed_files, 1);
        assert_eq!(delta.previous_generation, 3);
    }

    #[test]
    fn an_unchanged_project_has_an_empty_delta() {
        let store = store_with(&[("src/a.rs", "kept")]);
        index_file(&store, "src/a.rs", &["kept"]);

        let delta = compute(&store).expect("delta").expect("generation");

        assert!(delta.is_empty());
        assert_eq!(delta.unchanged_files, 1);
        assert!(delta.render(10).contains("no structural changes"));
    }

    #[test]
    fn renders_per_file_and_caps_the_lists() {
        let store = store_with(&[("src/a.rs", "one"), ("src/a.rs", "two")]);
        index_file(&store, "src/a.rs", &["three", "four", "five"]);

        let delta = compute(&store).expect("delta").expect("generation");
        let text = delta.render(1);

        assert!(text.starts_with("PROJECT DELTA\nsrc/a.rs"));
        assert!(text.contains("+ … 2 more"), "got: {text}");
        assert!(text.contains("changed: 1 files"));
    }
}
