//! `edit_file` — replace an exact fragment in a project file.
//!
//! Preferred over `write_file` for single-function changes. Fails when the fragment is
//! not unique unless the caller opts into replacing every occurrence. Adding text without
//! deleting anything is `append_file`, not this tool.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::security::permissions::Action;
use crate::vce::index::query::Engine;
use crate::vce::tools::registry::{Tool, ToolError};
use crate::vce::tools::truncate::Truncated;

/// `edit_file` tool.
pub struct EditFileTool;

impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace one unique fragment; use append_file to add text."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "old", "new"],
            "properties": {
                "path": { "type": "string", "description": "Project-relative path." },
                "old": { "type": "string", "description": "Exact unique text to replace." },
                "new": { "type": "string", "description": "Replacement; empty removes it." },
                "replace_all": { "type": "boolean", "default": false },
                "allow_shrink": {
                    "type": "boolean",
                    "default": false,
                    "description": "Allow replacing most of the fragment with much less text."
                }
            }
        })
    }

    fn action(&self) -> Action {
        Action::Write
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let path = required(engine, arguments, "path")?;
        let old = arguments
            .get("old")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: "edit_file",
                message: "`old` must be a non-empty string".to_owned(),
            })?;
        let new = arguments
            .get("new")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: "edit_file",
                message: "`new` must be a string".to_owned(),
            })?;
        let replace_all = arguments
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let allow_shrink = arguments
            .get("allow_shrink")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let old_len = old.chars().count();
        let new_len = new.chars().count();
        if old_len >= 40 && new_len.saturating_mul(4) < old_len && !allow_shrink {
            return Err(ToolError::InvalidArguments {
                tool: "edit_file",
                message: "the replacement removes most of the fragment; pass allow_shrink=true only if the user asked to delete it".to_owned(),
            });
        }

        let target = existing(engine, &path)?;
        let text = std::fs::read_to_string(&target).map_err(|source| {
            ToolError::Engine(crate::vce::index::query::EngineError::Io {
                path: display(engine, &target),
                source,
            })
        })?;

        let matches = text.matches(old).count();
        if matches == 0 {
            return Err(ToolError::InvalidArguments {
                tool: "edit_file",
                message: "the old fragment was not found".to_owned(),
            });
        }
        if matches > 1 && !replace_all {
            return Err(ToolError::InvalidArguments {
                tool: "edit_file",
                message: format!(
                    "the old fragment occurs {matches} times; pass replace_all=true or add context"
                ),
            });
        }

        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        std::fs::write(&target, &updated).map_err(|source| {
            ToolError::Engine(crate::vce::index::query::EngineError::Io {
                path: display(engine, &target),
                source,
            })
        })?;
        crate::vce::tools::registry::Registry::mark_ai(engine, &path);

        Ok(Truncated::whole(format!(
            "replaced {matches} occurrence(s) in {}",
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
            tool: "edit_file",
            message: format!("`{key}` must be a non-empty string"),
        })?;

    engine
        .guard()
        .check_lexical(value)
        .map_err(|error| ToolError::Engine(error.into()))?;
    Ok(value.to_owned())
}

fn existing(engine: &Engine, path: &str) -> Result<PathBuf, ToolError> {
    let candidate = engine
        .guard()
        .check_lexical(path)
        .map_err(|error| ToolError::Engine(error.into()))?;
    engine
        .guard()
        .resolve(&candidate)
        .map_err(|error| ToolError::Engine(error.into()))
}

fn display(engine: &Engine, path: &std::path::Path) -> String {
    engine
        .guard()
        .relative(path)
        .unwrap_or_else(|| path.display().to_string())
}
