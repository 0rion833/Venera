//! The turn loop: model call -> tool calls -> tool results -> next model call.
//!
//! Stops when the model answers without tool calls, when `limits.session_budget_rub` is
//! hit (then asks for confirmation), or when the compactor signals that the context is
//! beyond the safe window.
