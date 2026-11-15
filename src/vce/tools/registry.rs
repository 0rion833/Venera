//! Model-facing tools that expose the context engine (`TOOL.md` §10).
//!
//! Every tool is a thin, deterministic wrapper over [`Engine`]: it validates arguments,
//! applies the permission model, and returns text the dispatcher truncates before it can
//! enter history. No tool calls a model, and none of them can reach the filesystem except
//! through the engine, which owns the [`crate::security::path_guard::PathGuard`].
//!
//! Files are addressed by relative path, not by an opaque id: the path is the lookup key
//! of the index (`TOOL.md` §4), and a model that has to translate ids into paths spends
//! tokens without learning anything.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::security::permissions::{Action, PermissionError, Permissions};
use crate::vce::index::map::MapOptions;
use crate::vce::index::query::{Engine, EngineError, SymbolHit};
use crate::vce::index::refresh::{self, RefreshOptions};
use crate::vce::tools::truncate::{self, MAX_TOOL_LINES, Truncated};
use crate::vce::tools::{
    append::AppendFileTool, bash::BashTool, edit::EditFileTool, write::WriteFileTool,
};

/// Most rows any search tool returns.
pub const MAX_RESULTS: usize = 50;

/// Lines `get_file` returns when the caller does not say.
const DEFAULT_FILE_LINES: usize = 300;

/// A tool failure, always reported to the model as a result rather than a crash.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// No tool with this name is registered.
    #[error("unknown tool `{name}`")]
    Unknown {
        /// Requested name.
        name: String,
    },

    /// Arguments did not match the schema.
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArguments {
        /// Tool name.
        tool: &'static str,
        /// What was wrong.
        message: String,
    },

    /// The permission model refused the call.
    #[error(transparent)]
    Permission(#[from] PermissionError),

    /// The engine failed.
    #[error(transparent)]
    Engine(#[from] EngineError),

    /// A child process could not be started or observed.
    #[error("command failed: {0}")]
    Execution(String),
}

/// One callable tool.
pub trait Tool: Send + Sync {
    /// Stable name used in the request schema.
    fn name(&self) -> &'static str;

    /// One-line description; every token here is paid on every turn.
    fn description(&self) -> &'static str;

    /// JSON Schema of the arguments.
    fn schema(&self) -> Value;

    /// What the tool does to the project.
    fn action(&self) -> Action {
        Action::Read
    }

    /// Runs the tool and returns model-facing text.
    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError>;
}

/// The set of tools offered to the model.
#[derive(Clone)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    /// Every built-in tool, in the order the schemas must be sent.
    ///
    /// The order is part of the immutable request prefix (`AGENTS.md` §2): tools are
    /// appended, never reordered.
    pub fn builtin() -> Self {
        Self {
            tools: vec![
                Arc::new(ProjectMapTool),
                Arc::new(SearchFilesTool),
                Arc::new(SearchSymbolsTool),
                Arc::new(GetFileTool),
                Arc::new(GetFileRangeTool),
                Arc::new(GetFileOutlineTool),
                Arc::new(FindReferencesTool),
                Arc::new(GetDependenciesTool),
                Arc::new(RefreshIndexTool),
                Arc::new(BashTool),
                Arc::new(AppendFileTool),
                Arc::new(WriteFileTool),
                Arc::new(EditFileTool),
            ],
        }
    }

    /// The compact set offered to the chat model: read, find, append, write, edit, shell.
    ///
    /// Every schema is paid on every request, so the chat set deliberately leaves out
    /// map/outline/deps tools that the model can live without.
    pub fn chat() -> Self {
        Self {
            tools: vec![
                Arc::new(SearchFilesTool),
                Arc::new(GetFileTool),
                Arc::new(AppendFileTool),
                Arc::new(WriteFileTool),
                Arc::new(EditFileTool),
                Arc::new(BashTool),
            ],
        }
    }

    /// Reindexes a file the model just wrote and marks it as AI-owned.
    ///
    /// Best effort: a parse failure must not turn a successful write into an error.
    pub(crate) fn mark_ai(engine: &Engine, path: &str) {
        let guard = engine.guard();
        let Ok(absolute) = guard.resolve(path) else {
            return;
        };
        let Some(relative) = guard.relative(&absolute) else {
            return;
        };
        let _ = refresh::reindex_file(engine.store(), engine.root(), &relative);
        let _ = engine.store().set_origin(&relative, "ai");
    }

    /// Tool names in registration order.
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// Name and description pairs, for prompts and `--help`.
    pub fn describe(&self) -> Vec<(&'static str, &'static str)> {
        self.tools
            .iter()
            .map(|tool| (tool.name(), tool.description()))
            .collect()
    }

    /// Schemas, ready to become the request's `tools` array.
    pub fn schemas(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.schema(),
                    }
                })
            })
            .collect()
    }

    /// Looks a tool up by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name)
    }
}

/// Validates arguments, applies permissions, runs the tool, truncates the result.
pub struct Dispatcher {
    registry: Registry,
    permissions: Permissions,
}

impl Dispatcher {
    /// Builds a dispatcher with the built-in registry.
    pub fn new(permissions: Permissions) -> Self {
        Self {
            registry: Registry::builtin(),
            permissions,
        }
    }

    /// The registry behind this dispatcher.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Runs one tool call.
    pub fn call(
        &self,
        engine: &Engine,
        name: &str,
        arguments: &Value,
    ) -> Result<Truncated, ToolError> {
        let tool = self.registry.get(name).ok_or_else(|| ToolError::Unknown {
            name: name.to_owned(),
        })?;

        self.permissions.check(tool.action())?;

        let inner = tool.run(engine, arguments)?;
        let outer = truncate::head_tail(&inner.text, MAX_TOOL_LINES);
        Ok(Truncated {
            text: outer.text,
            hidden_lines: inner.hidden_lines + outer.hidden_lines,
            truncated: inner.truncated || outer.truncated,
        })
    }
}

fn required_str<'a>(
    tool: &'static str,
    arguments: &'a Value,
    key: &str,
) -> Result<&'a str, ToolError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArguments {
            tool,
            message: format!("`{key}` must be a non-empty string"),
        })
}

fn optional_usize(
    tool: &'static str,
    arguments: &Value,
    key: &str,
    default: usize,
) -> Result<usize, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .map(|number| number as usize)
            .filter(|number| *number > 0)
            .ok_or_else(|| ToolError::InvalidArguments {
                tool,
                message: format!("`{key}` must be a positive integer"),
            }),
    }
}

fn optional_u32(
    tool: &'static str,
    arguments: &Value,
    key: &str,
) -> Result<Option<u32>, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(|number| number.min(u64::from(u32::MAX)) as u32)
            .ok_or_else(|| ToolError::InvalidArguments {
                tool,
                message: format!("`{key}` must be a positive integer"),
            })
            .map(Some),
    }
}

fn optional_bool(arguments: &Value, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Renders search hits, one line each.
fn render_symbols(hits: &[SymbolHit]) -> String {
    let mut text = String::new();
    for hit in hits {
        let name = if hit.qualified_name == hit.name {
            hit.name.clone()
        } else {
            hit.qualified_name.clone()
        };
        text.push_str(&format!(
            "{}:{}-{} {} {}\n",
            hit.path, hit.start_line, hit.end_line, hit.kind, name
        ));
    }
    text
}

struct ProjectMapTool;

impl Tool for ProjectMapTool {
    fn name(&self) -> &'static str {
        "project_map"
    }

    fn description(&self) -> &'static str {
        "Compact project map: file paths with signatures, inside a token budget."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "max_tokens": { "type": "integer", "minimum": 1 }
            }
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let max_tokens = optional_usize(self.name(), arguments, "max_tokens", 1000)?;
        let map = engine.map(
            &MapOptions {
                max_tokens,
                ..MapOptions::default()
            },
            None,
        )?;

        let mut text = map.text;
        text.push_str(&format!(
            "map: {} tokens, {} symbols, {} files\n",
            map.tokens, map.symbols, map.files
        ));
        Ok(Truncated::whole(text))
    }
}

struct SearchFilesTool;

impl Tool for SearchFilesTool {
    fn name(&self) -> &'static str {
        "search_files"
    }

    fn description(&self) -> &'static str {
        "Find files by path or name."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["query"]
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let query = required_str(self.name(), arguments, "query")?;
        let limit = optional_usize(self.name(), arguments, "limit", 20)?.min(MAX_RESULTS);

        let hits = engine.search_files(query, limit)?;
        if hits.is_empty() {
            return Ok(Truncated::whole(format!("no files match `{query}`\n")));
        }

        let mut text = format!("{} files\n", hits.len());
        for hit in hits {
            text.push_str(&format!(
                "{} ({} symbols, {})\n",
                hit.path, hit.symbols, hit.language
            ));
        }
        Ok(Truncated::whole(text))
    }
}

struct SearchSymbolsTool;

impl Tool for SearchSymbolsTool {
    fn name(&self) -> &'static str {
        "search_symbols"
    }

    fn description(&self) -> &'static str {
        "Find functions, types, and classes by name, signature, or kind."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "kind": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["query"]
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let query = required_str(self.name(), arguments, "query")?;
        let limit = optional_usize(self.name(), arguments, "limit", 20)?.min(MAX_RESULTS);
        let kind = arguments.get("kind").and_then(Value::as_str);

        let mut hits = engine.search_symbols(query, limit)?;
        if let Some(kind) = kind {
            hits.retain(|hit| hit.kind == kind);
        }

        if hits.is_empty() {
            return Ok(Truncated::whole(format!("no symbols match `{query}`\n")));
        }

        let mut text = format!("{} symbols\n", hits.len());
        text.push_str(&render_symbols(&hits));
        Ok(Truncated::whole(text))
    }
}

struct GetFileTool;

impl Tool for GetFileTool {
    fn name(&self) -> &'static str {
        "get_file"
    }

    fn description(&self) -> &'static str {
        "Read a file, or as much of it as the line budget allows."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "max_lines": { "type": "integer", "minimum": 1 }
            },
            "required": ["path"]
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let path = required_str(self.name(), arguments, "path")?;
        let max_lines = optional_usize(self.name(), arguments, "max_lines", DEFAULT_FILE_LINES)?;

        let view = engine.source(path, None, None)?;
        let body = truncate::head_tail(&view.text, max_lines);
        let hash = view.content_hash.get(..12).unwrap_or(&view.content_hash);

        let mut text = format!(
            "{}: {} lines, hash {hash}{}\n",
            view.path,
            view.total_lines,
            if body.truncated { ", truncated" } else { "" }
        );
        text.push_str(&body.text);
        Ok(Truncated {
            text,
            hidden_lines: body.hidden_lines,
            truncated: body.truncated,
        })
    }
}

struct GetFileRangeTool;

impl Tool for GetFileRangeTool {
    fn name(&self) -> &'static str {
        "get_file_range"
    }

    fn description(&self) -> &'static str {
        "Read an exact line range; cheaper than reading the whole file."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer", "minimum": 1 },
                "end_line": { "type": "integer", "minimum": 1 }
            },
            "required": ["path", "start_line", "end_line"]
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let path = required_str(self.name(), arguments, "path")?;
        let start = optional_u32(self.name(), arguments, "start_line")?;
        let end = optional_u32(self.name(), arguments, "end_line")?;

        let view = engine.source(path, start, end)?;
        let mut text = format!(
            "{}: lines {}-{} of {}\n",
            view.path, view.start_line, view.end_line, view.total_lines
        );
        text.push_str(&view.text);
        text.push('\n');
        Ok(Truncated::whole(text))
    }
}

struct GetFileOutlineTool;

impl Tool for GetFileOutlineTool {
    fn name(&self) -> &'static str {
        "get_file_outline"
    }

    fn description(&self) -> &'static str {
        "List a file's symbols with line ranges, without reading the body."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let path = required_str(self.name(), arguments, "path")?;
        engine
            .outline(path)
            .map(Truncated::whole)
            .map_err(ToolError::from)
    }
}

struct FindReferencesTool;

impl Tool for FindReferencesTool {
    fn name(&self) -> &'static str {
        "find_references"
    }

    fn description(&self) -> &'static str {
        "Find declarations mentioning a name. Lexical evidence, not a call graph."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["name"]
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let name = required_str(self.name(), arguments, "name")?;
        let limit = optional_usize(self.name(), arguments, "limit", 20)?.min(MAX_RESULTS);

        let hits = engine.references(name, limit)?;
        if hits.is_empty() {
            return Ok(Truncated::whole(format!("nothing mentions `{name}`\n")));
        }

        let mut text = format!("{} references\n", hits.len());
        text.push_str(&render_symbols(&hits));
        Ok(Truncated::whole(text))
    }
}

struct GetDependenciesTool;

impl Tool for GetDependenciesTool {
    fn name(&self) -> &'static str {
        "get_dependencies"
    }

    fn description(&self) -> &'static str {
        "Imports of a file and files that import it, up to a depth."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "depth": { "type": "integer", "minimum": 1 }
            },
            "required": ["path"]
        })
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let path = required_str(self.name(), arguments, "path")?;
        let depth = optional_usize(self.name(), arguments, "depth", 1)?.min(5) as u32;

        let dependencies = engine.dependencies(path, depth)?;
        let mut text = format!("{}\n", dependencies.path);

        if dependencies.imports.is_empty() {
            text.push_str("imports: none\n");
        } else {
            text.push_str("imports:\n");
            for import in &dependencies.imports {
                let marker = if import.resolved { "" } else { " (external)" };
                text.push_str(&format!("  {} {}{marker}\n", import.depth, import.path));
            }
        }

        if dependencies.imported_by.is_empty() {
            text.push_str("imported by: none\n");
        } else {
            text.push_str("imported by:\n");
            for path in &dependencies.imported_by {
                text.push_str(&format!("  {path}\n"));
            }
        }

        Ok(Truncated::whole(text))
    }
}

struct RefreshIndexTool;

impl Tool for RefreshIndexTool {
    fn name(&self) -> &'static str {
        "refresh_index"
    }

    fn description(&self) -> &'static str {
        "Update the index after files changed. Never runs on every turn."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "force": { "type": "boolean" } }
        })
    }

    fn action(&self) -> Action {
        Action::Index
    }

    fn run(&self, engine: &Engine, arguments: &Value) -> Result<Truncated, ToolError> {
        let report = engine.refresh(&RefreshOptions {
            force: optional_bool(arguments, "force"),
            ..RefreshOptions::default()
        })?;

        Ok(Truncated::whole(format!(
            "indexed {} files: {} added, {} modified, {} deleted, {} parsed, {} unchanged; {} symbols, {} imports\n",
            report.files,
            report.added,
            report.modified,
            report.deleted,
            report.reparsed,
            report.unchanged,
            report.symbols,
            report.imports
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::refresh::RefreshOptions;

    fn engine(files: &[(&str, &str)]) -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }

        let engine = Engine::open(dir.path(), ".venera/index.db").expect("engine");
        engine.refresh(&RefreshOptions::default()).expect("refresh");
        (dir, engine)
    }

    fn dispatcher() -> Dispatcher {
        Dispatcher::new(Permissions::default())
    }

    #[test]
    fn every_documented_tool_is_registered_in_order() {
        let registry = Registry::builtin();

        assert_eq!(
            registry.names(),
            vec![
                "project_map",
                "search_files",
                "search_symbols",
                "get_file",
                "get_file_range",
                "get_file_outline",
                "find_references",
                "get_dependencies",
                "refresh_index",
                "bash",
                "append_file",
                "write_file",
                "edit_file",
            ]
        );
        assert_eq!(registry.schemas().len(), 13);
    }

    #[test]
    fn chat_tools_stay_small() {
        let json = serde_json::to_string(&Registry::chat().schemas()).expect("json");
        let tokens = crate::provider::tokenizer::count(crate::config::Encoding::O200kBase, &json);
        println!("chat tool schema tokens: {tokens}");
        assert!(tokens < 1200, "chat tools cost {tokens} tokens");
    }

    #[test]
    fn writes_are_refused_without_permission() {
        let (_dir, engine) = engine(&[("src/main.rs", "fn main() {}\n")]);
        let dispatcher = Dispatcher::new(Permissions::default());

        let error = dispatcher
            .call(
                &engine,
                "write_file",
                &json!({"path": "src/new.rs", "content": "fn new() {}\n"}),
            )
            .expect_err("default permissions refuse writes");

        assert!(error.to_string().contains("write"), "got: {error}");
    }

    #[test]
    fn write_and_edit_tools_change_the_project_after_permission() {
        let (dir, engine) = engine(&[("src/main.rs", "fn main() {}\n")]);
        let dispatcher = Dispatcher::new(Permissions::permissive());

        let written = dispatcher
            .call(
                &engine,
                "write_file",
                &json!({"path": "src/ai.rs", "content": "fn old() {}\n"}),
            )
            .expect("write");
        assert!(written.text.contains("wrote"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/ai.rs")).expect("file"),
            "fn old() {}\n"
        );

        let edited = dispatcher
            .call(
                &engine,
                "edit_file",
                &json!({"path": "src/ai.rs", "old": "old", "new": "new"}),
            )
            .expect("edit");
        assert!(edited.text.contains("replaced 1"));
        assert!(
            std::fs::read_to_string(dir.path().join("src/ai.rs"))
                .expect("file")
                .contains("fn new()")
        );
    }

    #[test]
    fn bash_requires_permission_and_runs_in_the_project() {
        let (_dir, engine) = engine(&[("src/main.rs", "fn main() {}\n")]);
        let strict = Dispatcher::new(Permissions::default());
        let refused = strict
            .call(&engine, "bash", &json!({"command": "echo hi"}))
            .expect_err("shell must be confirmed");
        assert!(matches!(refused, ToolError::Permission(_)));

        let dispatcher = Dispatcher::new(Permissions::permissive());
        let result = dispatcher
            .call(
                &engine,
                "bash",
                &json!({
                    "command": if cfg!(windows) { "echo hello" } else { "printf hello" }
                }),
            )
            .expect("run");
        assert!(result.text.contains("exit: 0"), "got: {}", result.text);
        assert!(result.text.contains("hello"), "got: {}", result.text);
    }

    #[test]
    fn append_keeps_existing_text_and_creates_missing_files() {
        let (dir, engine) = engine(&[("test.txt", "test")]);
        let dispatcher = Dispatcher::new(Permissions::permissive());

        let appended = dispatcher
            .call(
                &engine,
                "append_file",
                &json!({"path": "test.txt", "content": "hello"}),
            )
            .expect("append");
        assert!(
            appended.text.contains("with a newline"),
            "got: {}",
            appended.text
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.txt")).expect("file"),
            "test\nhello"
        );

        dispatcher
            .call(
                &engine,
                "append_file",
                &json!({"path": "test.txt", "content": " world"}),
            )
            .expect("inline append");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.txt")).expect("file"),
            "test\nhello world"
        );

        let created = dispatcher
            .call(
                &engine,
                "append_file",
                &json!({"path": "note.txt", "content": "fresh"}),
            )
            .expect("create");
        assert!(created.text.contains("created"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("file"),
            "fresh"
        );

        dispatcher
            .call(
                &engine,
                "append_file",
                &json!({"path": "note.txt", "content": "!", "separator": "none"}),
            )
            .expect("exact append");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("file"),
            "fresh!"
        );
    }

    #[test]
    fn edit_file_refuses_to_wipe_most_of_a_fragment_without_a_flag() {
        let old = "def calculate(value):\n    return value * 2\n";
        let (dir, engine) = engine(&[("app.py", old)]);
        let dispatcher = Dispatcher::new(Permissions::permissive());

        let error = dispatcher
            .call(
                &engine,
                "edit_file",
                &json!({"path": "app.py", "old": old, "new": "def"}),
            )
            .expect_err("a destructive shrink needs an explicit flag");
        assert!(error.to_string().contains("allow_shrink"), "got: {error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.py")).expect("file"),
            old
        );

        dispatcher
            .call(
                &engine,
                "edit_file",
                &json!({
                    "path": "app.py",
                    "old": old,
                    "new": "def",
                    "allow_shrink": true
                }),
            )
            .expect("explicit shrink");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.py")).expect("file"),
            "def"
        );
    }

    #[test]
    fn write_file_refuses_to_erase_an_existing_file_without_overwrite() {
        let (dir, engine) = engine(&[("test.txt", "test")]);
        let dispatcher = Dispatcher::new(Permissions::permissive());

        let error = dispatcher
            .call(
                &engine,
                "write_file",
                &json!({"path": "test.txt", "content": "hello"}),
            )
            .expect_err("existing file must be protected");
        assert!(error.to_string().contains("already exists"), "got: {error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.txt")).expect("file"),
            "test"
        );

        dispatcher
            .call(
                &engine,
                "write_file",
                &json!({"path": "test.txt", "content": "hello", "overwrite": true}),
            )
            .expect("explicit overwrite");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.txt")).expect("file"),
            "hello"
        );
    }

    #[test]
    fn schemas_are_valid_function_definitions() {
        for schema in Registry::builtin().schemas() {
            assert_eq!(schema["type"], "function");
            assert!(schema["function"]["name"].is_string());
            assert_eq!(schema["function"]["parameters"]["type"], "object");
        }
    }

    #[test]
    fn an_unknown_tool_is_an_error_not_a_panic() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        let error = dispatcher()
            .call(&engine, "nope", &json!({}))
            .expect_err("unknown tool");

        assert!(matches!(error, ToolError::Unknown { .. }));
    }

    #[test]
    fn search_symbols_returns_paths_and_ranges() {
        let (_dir, engine) = engine(&[("src/auth.rs", "pub fn authenticate() {\n}\n")]);
        let result = dispatcher()
            .call(
                &engine,
                "search_symbols",
                &json!({ "query": "authenticate" }),
            )
            .expect("call");

        assert!(
            result.text.contains("src/auth.rs:1-2"),
            "got: {}",
            result.text
        );
    }

    #[test]
    fn get_file_range_returns_the_requested_lines() {
        let (_dir, engine) = engine(&[("src/lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n")]);
        let result = dispatcher()
            .call(
                &engine,
                "get_file_range",
                &json!({ "path": "src/lib.rs", "start_line": 2, "end_line": 3 }),
            )
            .expect("call");

        assert!(
            result.text.contains("lines 2-3 of 3"),
            "got: {}",
            result.text
        );
        assert!(result.text.contains("fn two()"));
        assert!(!result.text.contains("fn one()"));
    }

    #[test]
    fn missing_arguments_are_rejected_with_the_tool_name() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        let error = dispatcher()
            .call(&engine, "search_symbols", &json!({}))
            .expect_err("missing query");

        match error {
            ToolError::InvalidArguments { tool, .. } => assert_eq!(tool, "search_symbols"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn large_reads_are_truncated_with_an_explicit_marker() {
        let body: String = (0..600)
            .map(|line| format!("fn f{line}() {{}}\n"))
            .collect();
        let (_dir, engine) = engine(&[("src/big.rs", &body)]);
        let result = dispatcher()
            .call(&engine, "get_file", &json!({ "path": "src/big.rs" }))
            .expect("call");

        assert!(result.truncated);
        assert!(result.text.contains("lines hidden"));
        assert!(result.hidden_lines > 0);
    }

    #[test]
    fn the_index_write_permission_is_enforced() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        let strict = Dispatcher::new(Permissions {
            allow_index: false,
            ..Permissions::default()
        });
        let error = strict
            .call(&engine, "refresh_index", &json!({}))
            .expect_err("denied");

        assert!(matches!(error, ToolError::Permission(_)));
    }

    #[test]
    fn refresh_index_reports_what_it_did() {
        let (_dir, engine) = engine(&[("src/lib.rs", "pub fn run() {}\n")]);
        let result = dispatcher()
            .call(&engine, "refresh_index", &json!({}))
            .expect("call");

        assert!(
            result.text.contains("indexed 1 files"),
            "got: {}",
            result.text
        );
    }

    #[test]
    fn dependencies_tool_lists_both_directions() {
        let (_dir, engine) = engine(&[
            ("src/main.rs", "use crate::util;\nfn main() {}\n"),
            ("src/util.rs", "pub fn run() {}\n"),
        ]);
        let result = dispatcher()
            .call(
                &engine,
                "get_dependencies",
                &json!({ "path": "src/util.rs" }),
            )
            .expect("call");

        assert!(result.text.contains("imported by:"), "got: {}", result.text);
        assert!(result.text.contains("src/main.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn write_and_append_refuse_a_dangling_symlink_outside_the_root() {
        use std::os::unix::fs::symlink;

        let (dir, engine) = engine(&[("notes.txt", "keep me\n")]);
        let outside = tempfile::tempdir().expect("outside dir");

        // Dangling symlink pointing outside the project.
        let dangling = dir
            .path()
            .join("notes.lnk")
            .join("..")
            .join("..")
            .join(outside.path().file_name().unwrap());
        let dangling = dir.path().join(".link-dangling");
        std::fs::remove_file(&dangling).ok();
        symlink(outside.path().join("created.txt"), &dangling).expect("symlink");

        let write = dispatcher().call(
            &engine,
            "write_file",
            &json!({ "path": ".link-dangling", "content": "x" }),
        );
        assert!(write.is_err(), "write must refuse a dangling symlink");
        assert!(!outside.path().join("created.txt").exists());

        let append = dispatcher().call(
            &engine,
            "append_file",
            &json!({ "path": ".link-dangling", "content": "y" }),
        );
        assert!(append.is_err(), "append must refuse a dangling symlink");
        assert!(!outside.path().join("created.txt").exists());

        // Symlink to an existing external file is also refused.
        std::fs::write(outside.path().join("outside.txt"), "external").expect("file");
        let link = dir.path().join("to-outside");
        std::fs::remove_file(&link).ok();
        symlink(outside.path().join("outside.txt"), &link).expect("symlink");

        let write = dispatcher().call(
            &engine,
            "write_file",
            &json!({ "path": "to-outside", "content": "x" }),
        );
        assert!(write.is_err(), "write must refuse an external symlink");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("outside.txt")).unwrap(),
            "external"
        );
    }
}
