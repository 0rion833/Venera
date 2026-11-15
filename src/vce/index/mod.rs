//! Project index (future `venera-index`).
//!
//! Holds M1 — a compressed project map in SQLite with FTS5 search and an import graph.
//! M1 is a hint, not truth: it can be stale, so a file is edited only after reading the
//! original (`AGENTS.md` §2).
//!
//! The deterministic half of the engine — scanning, signatures, the budgeted map, and
//! the token accounting — works without an LLM and without network access
//! (`TOOL.md` §1). Persistence in SQLite is the next milestone; today the index is built
//! in memory for one command and thrown away.

pub mod delta;
pub mod fts;
pub mod ignore;
pub mod imports;
pub mod language;
pub mod m1;
pub mod map;
pub mod measure;
pub mod parse;
pub mod query;
pub mod rank;
pub mod refresh;
pub mod scanner;
pub mod signatures;
pub mod text;
