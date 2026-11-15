//! Venera — a Rust CLI coding agent.
//!
//! The module tree mirrors the crate layout from `AGENTS.md` §3, collapsed into a single
//! package until the workspace split happens. Each module maps 1:1 to a future crate
//! (`config` -> `venera-config`, `core` -> `venera-core`, and so on).
//!
//! Dependency direction is strict and only flows downward:
//!
//! ```text
//! tui -> core -> {provider, index, tools, security} -> {config, theme}
//! ```
//!
//! `cli` sits on top and is the only module allowed to wire the others together.
//! A reverse dependency is a review blocker.
//!
//! Note: the `core` module shadows the `core` crate inside this package. Use
//! `crate::core` for the module and `::core` when the standard `core` crate is meant.

#![forbid(unsafe_code)]

pub mod cli;
pub mod commands;
pub mod config;
pub mod core;
pub mod init;
pub mod path_setup;
pub mod paths;
pub mod provider;
pub mod security;
pub mod skills;
pub mod theme;
pub mod tui;
pub mod vce;

pub use crate::config::{Config, ConfigError, LoadedConfig};
pub use crate::core::cost::{CostBreakdown, CostCounter, format_rub, turn_cost};
