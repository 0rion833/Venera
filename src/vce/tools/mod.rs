//! Tools available to the model (future `venera-tools`).
//!
//! Every tool implements the `Tool` trait declared here, exposes a schema of about 40
//! tokens of description, and truncates its own output before returning: head + tail +
//! an "N lines hidden" marker, no more than ~1500 tokens per call (`AGENTS.md` §2).
//!
//! All path handling goes through `crate::security::path_guard`; a direct `std::fs` call
//! with an unvalidated path is a review blocker.

pub mod append;
pub mod bash;
pub mod check;
pub mod edit;
pub mod find;
pub mod glob;
pub mod grep;
pub mod read;
pub mod read_rules;
pub mod registry;
pub mod task;
pub mod truncate;
pub mod write;
