//! `write_file` — create a project file, or replace one with an explicit flag.
//!
//! Writes are refused by [`crate::security::permissions::Permissions`] until the user
//! confirms them; the agent cannot silently overwrite sources. Existing files are
//! protected unless the caller passes `overwrite: true`.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::security::permissions::Action;
use crate::vce::index::query::Engine;
use crate::vce::tools::registry::{Tool, ToolError};
use crate::vce::tools::truncate::Truncated;

/// `write_file` tool.
pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create a NEW file; refuses existing unless overwrite=true. Use append_file to add text."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "content"],
            "properties": {
                "path": { "type": "string", "description": "Project-relative path." },
                "content": { "type": "string", "description": "Full new file content." },
                "overwrite": {
                    "type": "boolean",
                    "default": false,
                    "description": "Replace an existing file; never for adding text."
                }
            }
        })
    }

    fn action(&self) -> Action {
        Action::Write
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let path = required(engine, arguments, "path")?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: "write_file",
                message: "`content` must be a string".to_owned(),
            })?;

        let overwrite = arguments
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let target = target(engine, &path)?;
        if target.exists() && !overwrite {
            return Err(ToolError::InvalidArguments {
                tool: "write_file",
                message: format!(
                    "`{}` already exists; use append_file to add text, or edit_file to change a fragment. Use overwrite=true only when the user asked to replace the whole file.",
                    display(engine, &target)
                ),
            });
        }

        std::fs::write(&target, content).map_err(|source| {
            ToolError::Engine(crate::vce::index::query::EngineError::Io {
                path: display(engine, &target),
                source,
            })
        })?;
        crate::vce::tools::registry::Registry::mark_ai(engine, &path);

        Ok(Truncated::whole(format!(
            "wrote {} bytes to {}",
            content.len(),
            display(engine, &target)
        )))
    }
}

fn required(engine: &Engine, arguments: &Value, key: &str) -> Result<String, ToolError> {
    let value = arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArguments {
            tool: "write_file",
            message: format!("`{key}` must be a non-empty string"),
        })?;

    // Reject `..` before touching the disk; parents are checked separately.
    engine
        .guard()
        .check_lexical(value)
        .map_err(|error| ToolError::Engine(error.into()))?;
    Ok(value.to_owned())
}

fn target(engine: &Engine, path: &str) -> Result<PathBuf, ToolError> {
    let candidate = engine
        .guard()
        .check_lexical(path)
        .map_err(|error| ToolError::Engine(error.into()))?;

    if candidate.exists() {
        return engine
            .guard()
            .resolve(&candidate)
            .map_err(|error| ToolError::Engine(error.into()));
    }

    let parent = candidate
        .parent()
        .ok_or_else(|| ToolError::InvalidArguments {
            tool: "write_file",
            message: "path has no parent directory".to_owned(),
        })?;
    engine
        .guard()
        .resolve(parent)
        .map_err(|error| ToolError::Engine(error.into()))?;
    Ok(candidate)
}

fn display(engine: &Engine, path: &std::path::Path) -> String {
    engine
        .guard()
        .relative(path)
        .unwrap_or_else(|| path.display().to_string())
}
