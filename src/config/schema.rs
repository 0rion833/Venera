//! Config schema: the JSON Schema shipped with the binary plus its consistency check.
//!
//! `schema/config.schema.json` is the source of truth for field names and types; this
//! module keeps it embedded so `venera config check` can validate against it and so a
//! test fails if the schema and the shipped defaults drift apart.

/// The JSON Schema for `config.json`.
pub const SCHEMA: &str = include_str!("../../schema/config.schema.json");

/// Parses the embedded schema to prove it is valid JSON.
pub fn schema_json() -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(SCHEMA)
}
