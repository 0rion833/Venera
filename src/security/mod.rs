//! Permissions, path sandbox, and secret masking (future `venera-security`).
//!
//! These checks are never disabled "temporarily, for a test" (`AGENTS.md` §9).

pub mod path_guard;
pub mod permissions;
pub mod secrets;
pub mod vault;
