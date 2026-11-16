//! Sessions: transcript, model choice, and spend, kept on disk.
//!
//! A session is a JSON document per project, so `venera tui` reopens where the user left
//! off and the transcript survives a crash. Nothing here touches the network, and no
//! message ever contains an API key: keys live in the keyring and never enter a session.
//!
//! Sessions are scoped to a project: the directory is `.venera`-independent but keyed by
//! the canonical project path, so two checkouts never mix their transcripts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::ModelPrice;
use crate::core::cost::turn_cost;
use crate::provider::usage::Usage;

/// Disambiguates sessions created within the same millisecond.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Transcript entries kept in a session; the oldest are dropped first.
pub const MAX_TRANSCRIPT_ENTRIES: usize = 4_000;

/// What a transcript line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Typed by the user.
    User,
    /// Produced by the model.
    Assistant,
    /// UI and engine messages.
    System,
    /// Engine output: maps, sources, search hits.
    Tool,
    /// A failure.
    Error,
}

/// One transcript line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Who wrote it.
    pub kind: EntryKind,
    /// Plain text, one line.
    pub text: String,
}

impl Entry {
    /// Builds an entry.
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// Tokens and rubles accumulated in a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spend {
    /// Uncached prompt tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Prompt tokens served from the provider cache.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Completion tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Total billed, in RUB.
    #[serde(default)]
    pub rub: f64,
    /// Requests charged.
    #[serde(default)]
    pub turns: u64,
}

impl Spend {
    /// Charges one request at `price`.
    pub fn charge(&mut self, usage: &Usage, price: &ModelPrice) {
        let cost = turn_cost(price, usage);

        self.input_tokens = self.input_tokens.saturating_add(usage.input);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(usage.cache_read);
        self.output_tokens = self.output_tokens.saturating_add(usage.output);
        self.rub += cost.total_rub();
        self.turns = self.turns.saturating_add(1);
    }

    /// All prompt tokens, cached or not.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens
    }
}

/// One conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// Stable id, also the file stem.
    pub id: String,
    /// Human title, taken from the first user line.
    pub title: String,
    /// Project root the session belongs to.
    pub cwd: String,
    /// Model id used for pricing and the header.
    pub model: String,
    /// Theme name.
    pub theme: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Unix seconds.
    pub updated_at: u64,
    /// Transcript.
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// Tokens and rubles.
    #[serde(default)]
    pub spend: Spend,
    /// Submitted prompt lines, newest last; `↑`/`↓` walk them back.
    #[serde(default)]
    pub history: Vec<String>,
}

impl Session {
    /// Starts a new session for `cwd`.
    pub fn new(cwd: impl Into<String>, model: impl Into<String>, theme: impl Into<String>) -> Self {
        let created_at = unix_now();
        let sequence = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);

        Self {
            id: format!("s-{}-{sequence}", unix_millis()),
            title: "new session".to_owned(),
            cwd: cwd.into(),
            model: model.into(),
            theme: theme.into(),
            created_at,
            updated_at: created_at,
            entries: Vec::new(),
            spend: Spend::default(),
            history: Vec::new(),
        }
    }

    /// Appends a line and stamps the session.
    pub fn push(&mut self, kind: EntryKind, text: impl Into<String>) {
        let text = text.into();
        if kind == EntryKind::User && self.title == "new session" {
            self.title = shorten(&text, 48);
        }

        // The transcript is kept in memory for redraws; without a cap a long session would
        // copy every entry into styled lines on every frame.
        if self.entries.len() >= MAX_TRANSCRIPT_ENTRIES {
            let drop = self.entries.len() - MAX_TRANSCRIPT_ENTRIES + 1;
            self.entries.drain(..drop);
        }

        self.entries.push(Entry::new(kind, text));
        self.updated_at = unix_now();
    }

    /// Records a charge and stamps the session.
    pub fn charge(&mut self, usage: &Usage, price: &ModelPrice) {
        self.spend.charge(usage, price);
        self.updated_at = unix_now();
    }

    /// Short one-line form for `/sessions`.
    pub fn summary(&self) -> String {
        format!(
            "{}  {:>8} {}  {} turns · {:.4} руб.",
            self.id, self.model, self.title, self.spend.turns, self.spend.rub
        )
    }
}

/// Where sessions of one project live.
///
/// Default: `<data-local>/venera/sessions/<project-key>/`. Two projects never share a
/// directory because the key is the BLAKE3 of the canonical root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStore {
    dir: PathBuf,
    fallback: Option<PathBuf>,
}

impl SessionStore {
    /// Opens the store for `root`, or `None` when no storage directory is available.
    ///
    /// Portable first: sessions sit next to the executable, so a copied folder keeps its
    /// history. The data directory is only a fallback when the executable path is unknown.
    pub fn for_project(root: &Path) -> Option<Self> {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
        let key = &digest.to_hex()[..12];

        let (dir, fallback) = match std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.join("sessions")))
        {
            Some(dir) => {
                let fallback = directories::BaseDirs::new().map(|dirs| {
                    dirs.data_local_dir()
                        .join("venera")
                        .join("sessions")
                        .join(key)
                });
                (dir.join(key), fallback)
            }
            None => {
                let dir = directories::BaseDirs::new().map(|dirs| {
                    dirs.data_local_dir()
                        .join("venera")
                        .join("sessions")
                        .join(key)
                })?;
                (dir, None)
            }
        };

        Some(Self { dir, fallback })
    }

    /// Builds a store at an explicit directory; used by tests.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            fallback: None,
        }
    }

    /// Directory holding the session files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of one session file.
    pub fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Writes the session, creating the directory on first use.
    ///
    /// When the executable sits in a read-only folder (installed, not portable), the
    /// sibling-directory write fails and the data directory is tried instead.
    pub fn save(&self, session: &Session) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(session).map_err(std::io::Error::other)?;
        match self.save_to(&self.dir, &session.id, &text) {
            Ok(()) => Ok(()),
            Err(primary) => self
                .fallback
                .as_ref()
                .map(|fallback| self.save_to(fallback, &session.id, &text))
                .unwrap_or(Err(primary)),
        }
    }

    fn save_to(&self, dir: &Path, id: &str, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let temporary = dir.join(format!("{id}.json.tmp"));
        let target = dir.join(format!("{id}.json"));
        std::fs::write(&temporary, text)?;
        std::fs::rename(temporary, target)?;
        Ok(())
    }

    /// Reads one session; `None` when the file is missing or unreadable.
    ///
    /// The fallback directory is checked too: a session saved next to a read-only
    /// executable must be found on the next launch.
    pub fn load(&self, id: &str) -> Option<Session> {
        let text = read_text(&self.path_for(id)).or_else(|| {
            self.fallback
                .as_ref()
                .and_then(|dir| read_text(&dir.join(format!("{id}.json"))))
        })?;
        let mut session: Session = serde_json::from_str(&text).ok()?;

        // Old transcripts printed `₽`, which some consoles render as a box.
        for entry in &mut session.entries {
            entry.text = entry.text.replace('₽', "руб.");
        }

        Some(session)
    }

    /// Every readable session from the primary and fallback directories, newest first.
    pub fn list(&self) -> Vec<Session> {
        let mut sessions = list_dir(&self.dir);
        if let Some(fallback) = &self.fallback {
            for session in list_dir(fallback) {
                if !sessions.iter().any(|known| known.id == session.id) {
                    sessions.push(session);
                }
            }
        }

        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        sessions
    }

    /// The most recent session, if any.
    pub fn latest(&self) -> Option<Session> {
        self.list().into_iter().next()
    }

    /// Deletes one session.
    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.path_for(id))
    }
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Reads every session file in `dir`; an unreadable directory is simply empty.
fn list_dir(dir: &Path) -> Vec<Session> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn shorten(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::prices::PriceRates;

    fn price() -> ModelPrice {
        ModelPrice {
            rates: PriceRates {
                input: 30.0,
                output: 150.0,
                cache_read: Some(3.0),
                cache_write: Some(37.5),
            },
            long_context: None,
        }
    }

    #[test]
    fn a_new_session_is_titled_from_the_first_user_line() {
        let mut session = Session::new("D:/p", "openai/gpt-6-luna", "venera-dark");
        assert_eq!(session.title, "new session");

        session.push(EntryKind::System, "welcome");
        session.push(EntryKind::User, "read src/main.rs and explain the router");
        session.push(EntryKind::User, "another line");

        assert!(session.title.starts_with("read src/main.rs"));
        assert_eq!(session.entries.len(), 3);
    }

    #[test]
    fn the_transcript_is_capped_to_keep_redraws_cheap() {
        let mut session = Session::new("D:/p", "m", "venera-dark");

        for index in 0..MAX_TRANSCRIPT_ENTRIES + 5 {
            session.push(EntryKind::Tool, format!("line {index}"));
        }

        assert_eq!(session.entries.len(), MAX_TRANSCRIPT_ENTRIES);
        assert_eq!(session.entries[0].text, "line 5");
    }

    #[test]
    fn sessions_created_together_have_different_ids() {
        let first = Session::new("D:/p", "m", "venera-dark");
        let second = Session::new("D:/p", "m", "venera-dark");

        assert_ne!(first.id, second.id);
    }

    #[test]
    fn long_titles_are_shortened() {
        let mut session = Session::new("D:/p", "m", "venera-dark");
        session.push(EntryKind::User, "x".repeat(200));

        assert!(session.title.ends_with('…'));
        assert!(session.title.chars().count() <= 49);
    }

    #[test]
    fn spend_uses_the_cache_and_output_rates() {
        let mut spend = Spend::default();
        let usage = Usage {
            input: 1_000_000,
            cache_read: 1_000_000,
            output: 1_000_000,
            ..Usage::default()
        };

        spend.charge(&usage, &price());

        assert_eq!(spend.input_tokens, 1_000_000);
        assert_eq!(spend.prompt_tokens(), 2_000_000);
        assert!((spend.rub - 183.0).abs() < 1e-9);
        assert_eq!(spend.turns, 1);
    }

    #[test]
    fn a_writable_data_dir_takes_over_when_the_primary_is_unusable()
    -> Result<(), Box<dyn std::error::Error>> {
        let primary = tempfile::tempdir()?;
        let fallback = tempfile::tempdir()?;

        // A path under an existing *file* can never be created: create_dir_all fails,
        // which is exactly the installed-binary case where the folder next to the exe
        // is read-only. The data directory must then receive the session.
        let blocker = primary.path().join("blocker");
        std::fs::write(&blocker, "not a dir")?;
        let store = SessionStore {
            dir: blocker.join("sessions").join("key"),
            fallback: Some(fallback.path().to_path_buf()),
        };
        let session = Session::new("D:/p", "m", "venera-dark");
        store.save(&session)?;

        let saved = fallback.path().join(format!("{}.json", session.id));
        assert!(
            saved.exists(),
            "the fallback directory must receive the session"
        );
        let loaded = serde_json::from_str::<Session>(&std::fs::read_to_string(saved)?)?;
        assert_eq!(loaded.id, session.id);

        // Reading is symmetric with writing: the fallback copy is found.
        assert_eq!(
            store.load(&session.id).map(|item| item.id),
            Some(session.id.clone())
        );
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.latest().map(|item| item.id), Some(session.id.clone()));
        Ok(())
    }

    #[test]
    fn sessions_roundtrip_through_the_store() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = SessionStore::at(dir.path());

        let mut session = Session::new("D:/p", "openai/gpt-6-luna", "venera-dark");
        session.push(EntryKind::User, "привет");
        session.push(EntryKind::Assistant, "готово");
        session.charge(
            &Usage {
                input: 1_000,
                output: 100,
                ..Usage::default()
            },
            &price(),
        );
        store.save(&session)?;

        let loaded = store.load(&session.id).expect("session must load");
        assert_eq!(loaded, session);
        assert_eq!(store.latest().map(|item| item.id), Some(session.id.clone()));
        assert_eq!(store.list().len(), 1);
        Ok(())
    }

    #[test]
    fn the_latest_session_is_the_most_recently_updated() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = SessionStore::at(dir.path());

        let mut older = Session::new("D:/p", "m", "venera-dark");
        older.updated_at = 10;
        let mut newer = Session::new("D:/p", "m", "venera-dark");
        newer.updated_at = 20;

        store.save(&older)?;
        store.save(&newer)?;

        assert_eq!(store.latest().map(|item| item.id), Some(newer.id));
        Ok(())
    }

    /// The UI persists after every turn, so saving over an existing file must work.
    #[test]
    fn saving_the_same_session_twice_replaces_the_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = SessionStore::at(dir.path());

        let mut session = Session::new("D:/p", "m", "venera-dark");
        store.save(&session)?;

        session.push(EntryKind::User, "второй ход");
        store.save(&session)?;

        let loaded = store.load(&session.id).expect("session must load");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(store.list().len(), 1);
        Ok(())
    }

    #[test]
    fn a_missing_store_is_empty_not_an_error() {
        let store = SessionStore::at("D:/Venera/definitely-missing-dir");

        assert!(store.list().is_empty());
        assert!(store.latest().is_none());
        assert!(store.load("nope").is_none());
    }

    #[test]
    fn sessions_are_scoped_to_the_project() -> Result<(), Box<dyn std::error::Error>> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;

        let a = SessionStore::for_project(first.path()).expect("store");
        let b = SessionStore::for_project(second.path()).expect("store");

        // Different projects must never share a transcript directory.
        assert_ne!(a.dir(), b.dir());

        // The same project always resolves to the same key.
        let again = SessionStore::for_project(first.path()).expect("store");
        assert_eq!(a.dir(), again.dir());
        Ok(())
    }
}
