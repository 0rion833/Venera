//! `bash` — run a shell command in the project root.
//!
//! The command always needs confirmation: it can change anything the user can. Output is
//! capped at 400 lines, and only the tail of a timed-out process is reported. Secrets that
//! Venera itself uses are removed from the child environment.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::security::permissions::Action;
use crate::vce::index::query::Engine;
use crate::vce::tools::registry::{Tool, ToolError};
use crate::vce::tools::truncate::{self, MAX_TOOL_LINES, Truncated};

/// Commands longer than this are refused: they are almost always a mistake.
const MAX_COMMAND_BYTES: usize = 8_000;

/// Default wall-clock limit.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Hard wall-clock limit.
const MAX_TIMEOUT_SECS: u64 = 600;

/// `bash` tool.
pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the project root; needs confirmation."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["command"],
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command line passed to cmd /C or sh -c."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_SECS,
                    "default": DEFAULT_TIMEOUT_SECS
                }
            }
        })
    }

    fn action(&self) -> Action {
        Action::Execute
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: "bash",
                message: "`command` must be a non-empty string".to_owned(),
            })?;
        if command.len() > MAX_COMMAND_BYTES {
            return Err(ToolError::InvalidArguments {
                tool: "bash",
                message: format!("`command` is longer than {MAX_COMMAND_BYTES} bytes"),
            });
        }

        let timeout = arguments
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        let mut child = shell(command)
            .current_dir(engine.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("PROXYAPI_KEY")
            .env_remove("AITUNNEL_API_KEY")
            .env_remove("VENERA_VAULT_PASSWORD")
            .spawn()
            .map_err(|error| ToolError::Execution(error.to_string()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("stdout pipe is unavailable".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("stderr pipe is unavailable".to_owned()))?;
        let out_reader = std::thread::spawn(move || read_all(stdout));
        let err_reader = std::thread::spawn(move || read_all(stderr));

        let started = Instant::now();
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(done)) => break Some(done),
                Ok(None) => {}
                Err(error) => return Err(ToolError::Execution(error.to_string())),
            }

            if started.elapsed() >= Duration::from_secs(timeout) {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(25));
        };

        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        let code = status.and_then(|status| status.code()).unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);

        let mut body = format!("exit: {code}\n");
        if timed_out {
            body.push_str(&format!("timed out after {timeout}s\n"));
        }
        if !stdout.trim().is_empty() {
            body.push_str("stdout:\n");
            body.push_str(&stdout);
            if !stdout.ends_with('\n') {
                body.push('\n');
            }
        }
        if !stderr.trim().is_empty() {
            body.push_str("stderr:\n");
            body.push_str(&stderr);
            if !stderr.ends_with('\n') {
                body.push('\n');
            }
        }

        Ok(truncate::head_tail(&body, MAX_TOOL_LINES))
    }
}

fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut child = Command::new("cmd");
        child.args(["/C", command]);
        child
    }
    #[cfg(not(windows))]
    {
        let mut child = Command::new("/bin/sh");
        child.args(["-c", command]);
        child
    }
}

fn read_all(mut pipe: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    bytes
}
