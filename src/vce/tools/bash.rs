//! `bash` — run a shell command in the project root.
//!
//! The command always needs confirmation: it can change anything the user can. Output is
//! capped at 400 lines, and only the tail of a timed-out process is reported. Secrets that
//! Venera itself uses are removed from the child environment.

use std::io::{ErrorKind, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
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

/// Extra time allowed for pipe readers after the process exits, before the tool returns.
const GRACE: Duration = Duration::from_secs(5);

/// Bytes a single child stream may occupy before being cut off.
///
/// The model only ever sees [`MAX_TOOL_LINES`] lines, but the process must not be
/// allowed to buffer arbitrary amounts of output in RAM before that truncation runs.
const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;

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
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = out_tx.send(read_all(stdout));
        });
        std::thread::spawn(move || {
            let _ = err_tx.send(read_all(stderr));
        });

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

        // The pipe readers exit when every process holding the pipe closes it. A grandchild
        // that inherited the handles can keep them open, so do not join forever: wait for a
        // bounded grace period and then move on.
        let deadline = started + Duration::from_secs(timeout) + GRACE;
        let stdout = out_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        let stderr = err_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        let code = status.and_then(|status| status.code()).unwrap_or(-1);
        let stdout_cut = stdout.truncated;
        let stderr_cut = stderr.truncated;
        let stdout = String::from_utf8_lossy(&stdout.bytes);
        let stderr = String::from_utf8_lossy(&stderr.bytes);

        let mut body = format!("exit: {code}\n");
        if timed_out {
            body.push_str(&format!("timed out after {timeout}s\n"));
        }
        if stdout_cut {
            body.push_str("stdout: output truncated by byte limit\n");
        }
        if !stdout.trim().is_empty() {
            body.push_str("stdout:\n");
            body.push_str(&stdout);
            if !stdout.ends_with('\n') {
                body.push('\n');
            }
        }
        if stderr_cut {
            body.push_str("stderr: output truncated by byte limit\n");
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

/// Bounded pipe content: bytes kept plus whether the limit cut the stream.
#[derive(Default)]
struct LimitedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_all(mut pipe: impl Read) -> LimitedOutput {
    let mut bytes = Vec::with_capacity(MAX_STREAM_BYTES.min(64 * 1024));
    let mut truncated = false;
    let mut chunk = [0u8; 8192];

    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = MAX_STREAM_BYTES.saturating_sub(bytes.len());
                if n > remaining {
                    bytes.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    // Keep draining so the writer does not block forever, but stop storing.
                    let mut sink = [0u8; 8192];
                    while let Ok(count) = pipe.read(&mut sink) {
                        if count == 0 {
                            break;
                        }
                    }
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    LimitedOutput { bytes, truncated }
}
