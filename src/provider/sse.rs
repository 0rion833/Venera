//! Server-sent events: framing, incremental decoding, and stream termination.
//!
//! Must tolerate chunk boundaries in the middle of a frame and surface transport errors
//! as typed errors instead of silently ending the stream.
