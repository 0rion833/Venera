//! LLM providers (future `venera-provider`).
//!
//! Every LLM call in Venera goes through this module — no direct `reqwest` to an API from
//! any other module (`AGENTS.md` §2).
//!
//! The `Provider` trait lands here next and must support SSE streaming plus usage
//! accounting including `cache_read`/`cache_write`.

pub mod catalog;
pub mod keys;
pub mod openai_compat;
pub mod registry;
pub mod sse;
pub mod tokenizer;
pub mod usage;
