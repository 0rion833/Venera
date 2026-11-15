//! Venera Context Engine (`vce`): everything that turns a project into context.
//!
//! The engine is deterministic, works offline, and never calls a model:
//!
//! - `index` — scanning, SQLite, symbols, imports, ranking, retrieval, map, delta;
//! - `context` — routing, segmentation, compression, validation, distribution;
//! - `cache` — runtime (M0) and persistent (M1) prepared contexts;
//! - `tools` — the model-facing wrappers around the engine.
//!
//! Borders are deliberate: `vce` never calls a provider, and `core` only drives it.

pub mod backup;
pub mod cache;
pub mod context;
pub mod index;
pub mod session;
pub mod sync;
pub mod tools;
pub mod watch;
