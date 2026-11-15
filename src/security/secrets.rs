//! Secret masking for anything that leaves the process: logs, tool output, session files.
//!
//! Keys are held with `secrecy` and wiped with `zeroize`; masking is the second line of
//! defense against leaks into context (`AGENTS.md` §2).
