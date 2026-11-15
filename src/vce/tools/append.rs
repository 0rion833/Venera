//! `append_file` — add text to the end of a project file without replacing it.
//!
//! This is the safe tool for "add hello": the existing bytes stay untouched. Creating a
//! missing file is allowed, so the tool covers both new and existing targets.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::security::permissions::Action;
use crate::vce::index::query::Engine;
use crate::vce::tools::registry::{Tool, ToolError};
use crate::vce::tools::truncate::Truncated;

/// `append_file` tool.
pub struct AppendFileTool;

impl Tool for AppendFileTool {
    fn name(&self) -> &'static str {
        "append_file"
    }

    fn description(&self) -> &'static str {
        "Append text; keeps existing content; auto-adds a newline when needed."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "content"],
            "properties": {
                "path": { "type": "string", "description": "Project-relative path." },
                "content": {
                    "type": "string",
                    "description": "Text to append; a leading space appends inline."
                },
                "separator": {
                    "type": "string",
                    "enum": ["auto", "none"],
                    "default": "auto",
                    "description": "auto adds a newline when needed; none appends exactly."
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
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: "append_file",
                message: "`content` must be a non-empty string".to_owned(),
            })?;

        let separator = arguments
            .get("separator")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        if !matches!(separator, "auto" | "none") {
            return Err(ToolError::InvalidArguments {
                tool: "append_file",
                message: "`separator` must be `auto` or `none`".to_owned(),
            });
        }

        let target = target(engine, &path)?;
        let existed = target.is_file();
        let add_newline = separator == "auto" && needs_newline(&target, content);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&target)
            .map_err(|source| {
                ToolError::Engine(crate::vce::index::query::EngineError::Io {
                    path: display(engine, &target),
                    source,
                })
            })?;
        if add_newline {
            file.write_all(b"\n").map_err(|source| {
                ToolError::Engine(crate::vce::index::query::EngineError::Io {
                    path: display(engine, &target),
                    source,
                })
            })?;
        }
        file.write_all(content.as_bytes()).map_err(|source| {
            ToolError::Engine(crate::vce::index::query::EngineError::Io {
                path: display(engine, &target),
                source,
            })
        })?;
        crate::vce::tools::registry::Registry::mark_ai(engine, &path);

        let verb = if existed { "appended" } else { "created" };
        let newline = if add_newline { " with a newline" } else { "" };
        let written = content.len() + if add_newline { 1 } else { 0 };
        Ok(Truncated::whole(format!(
            "{verb} {written} bytes{newline} to {}",
            display(engine, &target)
        )))
    }
}

/// True when the file has text, ends without whitespace, and `content` does not start
/// with whitespace. In that case the caller wants a new line, not `testhello`.
fn needs_newline(target: &Path, content: &str) -> bool {
    if content.chars().next().is_some_and(char::is_whitespace) {
        return false;
    }

    let Ok(mut file) = std::fs::File::open(target) else {
        return false;
    };
    if file.metadata().map(|meta| meta.len()).unwrap_or(0) == 0 {
        return false;
    }
    if file.seek(SeekFrom::End(-1)).is_err() {
        return false;
    }

    let mut last = [0u8; 1];
    if file.read_exact(&mut last).is_err() {
        return false;
    }
    !(last[0] as char).is_ascii_whitespace()
}

fn required(engine: &Engine, arguments: &Value, key: &str) -> Result<String, ToolError> {
    let value = arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArguments {
            tool: "append_file",
            message: format!("`{key}` must be a non-empty string"),
        })?;

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
            tool: "append_file",
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
