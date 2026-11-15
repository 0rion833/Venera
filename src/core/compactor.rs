//! Context compaction.
//!
//! Fires at 60-70% of the model window and keeps M1 under 272K tokens, where long-context
//! pricing doubles (`AGENTS.md` §2). Compaction must never rewrite the request prefix.
