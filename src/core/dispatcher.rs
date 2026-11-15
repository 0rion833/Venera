//! Tool-call dispatch: validates arguments, applies permissions and the path sandbox,
//! executes the tool, and truncates the result before it enters history.
//!
//! Permission denial is a normal tool result, not a crash — the model must see why.
