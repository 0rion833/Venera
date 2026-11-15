//! tree-sitter parsing: symbols, spans, and summary inputs.
//!
//! CPU-heavy, so it runs on `rayon` or `spawn_blocking`, never on the async executor
//! (`AGENTS.md` §5).
