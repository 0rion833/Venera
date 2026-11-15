//! Task session lifecycle: temporary context has its own lifetime.
//!
//! A session tracks what a task touched, so the project index can be synchronized from
//! the actual filesystem and the task's temporary state can be dropped afterwards. The
//! session never stores project memory: symbols, hashes, and imports live in the index
//! and survive cleanup (`prirote.md` §2.1).
//!
//! Cleanup is intentionally idempotent and does not touch the database or the cache. That
//! is the whole invariant: clearing a task context is not clearing project memory.

use std::collections::{BTreeMap, BTreeSet};

use crate::vce::index::m1::StoreError;
use crate::vce::index::query::Engine;
use crate::vce::sync::{self, SyncError, SyncOptions, SyncReport};

/// Where a task is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Created, nothing tracked yet.
    Created,
    /// The task is running.
    Active,
    /// Changes are being written back to the index.
    Synchronizing,
    /// Temporary context is being dropped.
    Cleaning,
    /// Finished and synchronized.
    Completed,
    /// Finished with an error; changes may still exist.
    Failed,
    /// Abandoned by the caller.
    Cancelled,
}

impl SessionState {
    /// Short stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Active => "active",
            Self::Synchronizing => "synchronizing",
            Self::Cleaning => "cleaning",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One task's temporary state.
#[derive(Debug, Clone)]
pub struct TaskSession {
    /// Identifier unique inside this process.
    pub task_id: String,
    /// Project the task works on.
    pub project_id: String,
    /// Current state.
    pub state: SessionState,
    /// Files read during the task.
    pub accessed_files: BTreeSet<String>,
    /// Files the task claims to have changed.
    pub modified_files: BTreeSet<String>,
    /// Index generation when the task started.
    pub generation_at_start: i64,
}

/// What cleanup released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    /// Task that was cleaned.
    pub task_id: String,
    /// Tracked files dropped with the task.
    pub released_files: usize,
    /// True when the task had already been cleaned.
    pub already_clean: bool,
}

/// A session failure.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// No such task in this process.
    #[error("unknown task `{id}`")]
    Unknown {
        /// Requested identifier.
        id: String,
    },
    /// The database failed during synchronization.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Synchronization failed.
    #[error(transparent)]
    Sync(#[from] SyncError),
}

/// Owns task sessions for one project.
#[derive(Debug)]
pub struct SessionManager {
    project_id: String,
    sessions: BTreeMap<String, TaskSession>,
    next_id: u64,
}

impl SessionManager {
    /// Creates a manager for `project_id`.
    pub fn new(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            sessions: BTreeMap::new(),
            next_id: 0,
        }
    }

    /// Starts a task and returns its identifier.
    pub fn begin_task(&mut self, generation_at_start: i64) -> String {
        self.next_id += 1;
        let task_id = format!("task-{}", self.next_id);
        self.sessions.insert(
            task_id.clone(),
            TaskSession {
                task_id: task_id.clone(),
                project_id: self.project_id.clone(),
                state: SessionState::Created,
                accessed_files: BTreeSet::new(),
                modified_files: BTreeSet::new(),
                generation_at_start,
            },
        );
        task_id
    }

    /// One session by id.
    pub fn session(&self, id: &str) -> Option<&TaskSession> {
        self.sessions.get(id)
    }

    /// Number of live sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether no session is alive.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Records that a file was read by the task.
    pub fn track_access(&mut self, id: &str, path: &str) -> Result<(), SessionError> {
        let session = self.session_mut(id)?;
        session.accessed_files.insert(normalize(path));
        session.state = SessionState::Active;
        Ok(())
    }

    /// Records that the task claims to have changed a file.
    ///
    /// The claim is only a hint: synchronization checks the filesystem and decides what
    /// actually changed (`prirote.md` §2.3).
    pub fn track_modification(&mut self, id: &str, path: &str) -> Result<(), SessionError> {
        let session = self.session_mut(id)?;
        session.modified_files.insert(normalize(path));
        session.state = SessionState::Active;
        Ok(())
    }

    /// Synchronizes the project after the task, even if the task failed.
    pub fn synchronize(
        &mut self,
        id: &str,
        engine: &Engine,
        options: &SyncOptions,
    ) -> Result<SyncReport, SessionError> {
        let paths: Vec<String> = {
            let session = self.session_mut(id)?;
            session.state = SessionState::Synchronizing;
            session.modified_files.iter().cloned().collect()
        };

        match sync::synchronize(engine, &paths, options) {
            Ok(report) => {
                if let Some(session) = self.sessions.get_mut(id) {
                    session.state = SessionState::Completed;
                }
                Ok(report)
            }
            Err(error) => {
                if let Some(session) = self.sessions.get_mut(id) {
                    session.state = SessionState::Failed;
                }
                Err(SessionError::Sync(error))
            }
        }
    }

    /// Marks the task failed. Its changes still have to be synchronized.
    pub fn fail(&mut self, id: &str) -> Result<(), SessionError> {
        self.session_mut(id)?.state = SessionState::Failed;
        Ok(())
    }

    /// Marks the task cancelled.
    pub fn cancel(&mut self, id: &str) -> Result<(), SessionError> {
        self.session_mut(id)?.state = SessionState::Cancelled;
        Ok(())
    }

    /// Drops the task's temporary state.
    ///
    /// Idempotent: a second call reports `already_clean` and touches nothing else. It
    /// never writes to the database or the cache, so project memory survives.
    pub fn cleanup(&mut self, id: &str) -> CleanupReport {
        match self.sessions.remove(id) {
            Some(session) => CleanupReport {
                task_id: session.task_id,
                released_files: session.accessed_files.len() + session.modified_files.len(),
                already_clean: false,
            },
            None => CleanupReport {
                task_id: id.to_owned(),
                released_files: 0,
                already_clean: true,
            },
        }
    }

    fn session_mut(&mut self, id: &str) -> Result<&mut TaskSession, SessionError> {
        self.sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::Unknown { id: id.to_owned() })
    }
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
    use crate::vce::index::query::Engine;
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
    fn a_task_tracks_files_until_cleanup() {
        let mut manager = SessionManager::new("demo");
        let id = manager.begin_task(7);

        manager.track_access(&id, "src/a.rs").expect("access");
        manager.track_modification(&id, "src/a.rs").expect("modify");

        let session = manager.session(&id).expect("session");
        assert_eq!(session.state, SessionState::Active);
        assert_eq!(session.generation_at_start, 7);

        let report = manager.cleanup(&id);
        assert!(!report.already_clean);
        assert_eq!(report.released_files, 2);
        assert!(manager.session(&id).is_none());
    }

    #[test]
    fn cleanup_is_idempotent() {
        let mut manager = SessionManager::new("demo");
        let id = manager.begin_task(0);
        manager.cleanup(&id);

        let again = manager.cleanup(&id);

        assert!(again.already_clean);
        assert_eq!(again.released_files, 0);
    }

    #[test]
    fn unknown_tasks_are_reported() {
        let mut manager = SessionManager::new("demo");

        assert!(matches!(
            manager.track_access("nope", "src/a.rs"),
            Err(SessionError::Unknown { .. })
        ));
    }

    #[test]
    fn a_failed_task_still_synchronizes_its_changes() {
        let (dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        let mut manager = SessionManager::new("demo");
        let id = manager.begin_task(0);

        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn run() {}\npub fn stop() {}\n",
        )
        .expect("rewrite");
        manager
            .track_modification(&id, "src/lib.rs")
            .expect("track");
        manager.fail(&id).expect("fail");

        let report = manager
            .synchronize(&id, &engine, &SyncOptions::default())
            .expect("sync");

        assert_eq!(report.modified, vec!["src/lib.rs".to_owned()]);
        assert!(
            engine
                .store()
                .symbols()
                .expect("symbols")
                .iter()
                .any(|s| s.name == "stop")
        );
        manager.cleanup(&id);
    }

    #[test]
    fn cleanup_does_not_touch_project_memory() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        let mut manager = SessionManager::new("demo");
        let id = manager.begin_task(0);
        manager.track_access(&id, "src/lib.rs").expect("track");
        let before = engine.store().counts().expect("counts");

        manager.cleanup(&id);

        let after = engine.store().counts().expect("counts");
        assert_eq!(before, after);
    }
}
