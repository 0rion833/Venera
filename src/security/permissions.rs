//! Permission model: which tools may run without asking.
//!
//! File writes and `bash` require confirmation unless the config allows them
//! (`AGENTS.md` §2). Denials are normal results, not crashes: the model has to see why a
//! call was refused.
//!
//! The index database is a third category. It is derived data inside `.venera/`, not a
//! user file, so refreshing it is allowed by default while writing project files is not.

use serde::{Deserialize, Serialize};

/// What a tool wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Read project files or the index.
    Read,
    /// Write, move, or delete project files.
    Write,
    /// Spawn a process.
    Execute,
    /// Write the derived index database only.
    Index,
}

impl Action {
    /// Short name used in messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Index => "index",
        }
    }
}

/// A refusal.
#[derive(Debug, thiserror::Error)]
#[error("`{action}` is not allowed: confirm it in the session or enable it in config")]
pub struct PermissionError {
    /// The refused action.
    pub action: &'static str,
}

/// What the session currently allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Writing project files.
    #[serde(default)]
    pub allow_write: bool,
    /// Running shell commands.
    #[serde(default)]
    pub allow_execute: bool,
    /// Updating the derived index.
    #[serde(default = "allow_by_default")]
    pub allow_index: bool,
}

fn allow_by_default() -> bool {
    true
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            allow_write: false,
            allow_execute: false,
            allow_index: true,
        }
    }
}

impl Permissions {
    /// Everything allowed; only for tests and an explicit `--yolo` style opt-in.
    pub fn permissive() -> Self {
        Self {
            allow_write: true,
            allow_execute: true,
            allow_index: true,
        }
    }

    /// Checks one action.
    pub fn check(&self, action: Action) -> Result<(), PermissionError> {
        let allowed = match action {
            Action::Read => true,
            Action::Write => self.allow_write,
            Action::Execute => self.allow_execute,
            Action::Index => self.allow_index,
        };

        if allowed {
            Ok(())
        } else {
            Err(PermissionError {
                action: action.as_str(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_index_writes_are_allowed_by_default() {
        let permissions = Permissions::default();

        assert!(permissions.check(Action::Read).is_ok());
        assert!(permissions.check(Action::Index).is_ok());
    }

    #[test]
    fn project_writes_and_shell_need_confirmation() {
        let permissions = Permissions::default();

        assert!(permissions.check(Action::Write).is_err());
        assert!(permissions.check(Action::Execute).is_err());
    }

    #[test]
    fn a_denial_names_the_action() {
        let error = Permissions::default().check(Action::Write).unwrap_err();

        assert!(error.to_string().contains("`write`"), "got: {error}");
    }
}
