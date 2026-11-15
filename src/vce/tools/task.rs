//! `task` — delegate a sub-task to a scout-profile agent and return only its summary.
//!
//! This is the main token-saving path: the sub-agent burns the cheap model's budget and
//! the main context receives a digest instead of raw tool output.
