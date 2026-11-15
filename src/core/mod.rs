//! Agent engine (future `venera-core`).
//!
//! Owns the turn loop, tool dispatch, context compaction, and the RUB cost counter.
//! Changes here can invalidate the prompt cache, which makes requests up to 10x more
//! expensive — see `AGENTS.md` §6 before touching request assembly.

pub mod agent;
pub mod budget;
pub mod compactor;
pub mod cost;
pub mod dispatcher;
pub mod prompt;
pub mod selection;
