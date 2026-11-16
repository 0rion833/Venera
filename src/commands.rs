//! Slash commands: the same actions in the terminal UI and in the headless CLI.
//!
//! Parsing is deliberately small and synchronous. Commands never call a model; they run
//! the engine and print what it found. A line that does not start with `/` is not a
//! command — the caller decides what to do with it.

/// One parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Print the command list.
    Help,
    /// Clear the transcript.
    Clear,
    /// Leave the UI.
    Quit,
    /// Refresh the index.
    Index {
        /// Reparse everything.
        force: bool,
    },
    /// Render the project map.
    Map {
        /// Token budget.
        tokens: Option<usize>,
    },
    /// Refresh the index and render the map in one step.
    Vce {
        /// Token budget for the map.
        tokens: Option<usize>,
    },
    /// Prepare context for a request.
    Context {
        /// The request text.
        input: String,
        /// Token ceiling.
        tokens: Option<usize>,
        /// Compression level name.
        compression: Option<String>,
        /// Strategy name.
        strategy: Option<String>,
    },
    /// Search files and symbols.
    Search {
        /// What to look for.
        query: String,
    },
    /// List a file's symbols.
    Outline {
        /// Project-relative path.
        file: String,
    },
    /// Read a verified range.
    Cat {
        /// Project-relative path.
        file: String,
        /// First line.
        from: Option<u32>,
        /// Last line.
        to: Option<u32>,
    },
    /// Imports and importers.
    Deps {
        /// Project-relative path.
        file: String,
        /// Hops to walk.
        depth: u32,
    },
    /// Show cache statistics.
    Cache,
    /// Drop every cached prepared context.
    CacheClear,
    /// List skills.
    Skills,
    /// Show one skill.
    Skill {
        /// Skill name.
        name: String,
    },
    /// List models, optionally filtered.
    Models {
        /// Substring of an id or a name.
        filter: Option<String>,
    },
    /// Download the model catalog of every supported aggregator.
    Install,
    /// Choose a model; without a name the picker opens.
    Model {
        /// Model key, id, or short name.
        name: Option<String>,
    },
    /// Choose an aggregator; without a name the picker opens.
    Providers {
        /// Provider id, e.g. `aitunnel`.
        name: Option<String>,
    },
    /// Show aggregators and their masked API keys, or replace one key.
    Api {
        /// Provider whose key should be replaced; `None` lists all.
        provider: Option<String>,
    },
    /// Restore the user snapshot from `.venera/index2.db`.
    BackupRestore,
    /// Forget the master password and delete every stored API key.
    VaultReset,
    /// Switch theme; without a name the list is shown.
    Theme {
        /// Theme name, also the file stem.
        name: Option<String>,
    },
    /// Start a new session.
    NewSession,
    /// List stored sessions.
    Sessions,
    /// Open a stored session.
    SwitchSession {
        /// Session id from `/sessions`.
        id: String,
    },
    /// Open another project directory.
    Project {
        /// Directory to open; without it the path dialog opens.
        path: Option<String>,
    },
    /// Browse for a project directory.
    Projects,
    /// Show the project directory.
    Cwd,
    /// Show the spend of the current session.
    Cost,
    /// A command that does not exist.
    Unknown {
        /// The name that was typed.
        name: String,
    },
    /// The line is not a command.
    NotACommand,
}

/// Documentation of one command.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// Command name without the slash.
    pub name: &'static str,
    /// Arguments.
    pub usage: &'static str,
    /// One-line description.
    pub description: &'static str,
}

/// Every command the UI accepts.
pub const SPECS: &[Spec] = &[
    Spec {
        name: "help",
        usage: "",
        description: "show this list",
    },
    Spec {
        name: "index",
        usage: "[--force]",
        description: "refresh the project index",
    },
    Spec {
        name: "map",
        usage: "[--tokens N]",
        description: "print the budgeted project map",
    },
    Spec {
        name: "vce",
        usage: "[--tokens N]",
        description: "refresh the index, render the map, and snapshot the user backup",
    },
    Spec {
        name: "back",
        usage: "",
        description: "restore user files from the index2 snapshot",
    },
    Spec {
        name: "context",
        usage: "<request> [--tokens N] [--level balanced] [--strategy hybrid]",
        description: "prepare context for a request",
    },
    Spec {
        name: "search",
        usage: "<query>",
        description: "search files and symbols",
    },
    Spec {
        name: "outline",
        usage: "<file>",
        description: "list a file's symbols",
    },
    Spec {
        name: "cat",
        usage: "<file> [--from N] [--to N]",
        description: "read a verified range",
    },
    Spec {
        name: "deps",
        usage: "<file> [--depth N]",
        description: "imports and importers",
    },
    Spec {
        name: "cache",
        usage: "",
        description: "cache statistics",
    },
    Spec {
        name: "cache-clear",
        usage: "",
        description: "drop cached contexts",
    },
    Spec {
        name: "skills",
        usage: "",
        description: "list skills",
    },
    Spec {
        name: "skill",
        usage: "<name>",
        description: "show one skill",
    },
    Spec {
        name: "model",
        usage: "[name]",
        description: "pick a model from the list",
    },
    Spec {
        name: "models",
        usage: "[filter]",
        description: "list available models",
    },
    Spec {
        name: "install",
        usage: "",
        description: "download the model catalog of every aggregator",
    },
    Spec {
        name: "provider",
        usage: "[id]",
        description: "choose an aggregator and its API key",
    },
    Spec {
        name: "api",
        usage: "[provider]",
        description: "show aggregators and masked API keys",
    },
    Spec {
        name: "vault",
        usage: "reset",
        description: "delete the master password and every stored API key",
    },
    Spec {
        name: "project",
        usage: "[path]",
        description: "switch the project directory",
    },
    Spec {
        name: "projects",
        usage: "",
        description: "browse for a project directory",
    },
    Spec {
        name: "theme",
        usage: "[name]",
        description: "switch the palette or list themes",
    },
    Spec {
        name: "new",
        usage: "",
        description: "start a new session",
    },
    Spec {
        name: "sessions",
        usage: "",
        description: "list stored sessions",
    },
    Spec {
        name: "session",
        usage: "<id>",
        description: "open a stored session",
    },
    Spec {
        name: "cwd",
        usage: "",
        description: "show the project directory",
    },
    Spec {
        name: "cost",
        usage: "",
        description: "show this session's spend",
    },
    Spec {
        name: "clear",
        usage: "",
        description: "clear the transcript",
    },
    Spec {
        name: "quit",
        usage: "",
        description: "leave the UI",
    },
];

/// Parses one input line.
pub fn parse(line: &str) -> Action {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('/') else {
        return Action::NotACommand;
    };

    let mut parts = rest.split_whitespace();
    let Some(name) = parts.next() else {
        return Action::Unknown {
            name: String::new(),
        };
    };
    let mut args: Vec<String> = parts.map(str::to_owned).collect();

    match name {
        "help" | "?" => Action::Help,
        "clear" | "cls" => Action::Clear,
        "quit" | "exit" | "q" => Action::Quit,
        "index" => Action::Index {
            force: take_flag(&mut args, "--force"),
        },
        "map" => Action::Map {
            tokens: take_value(&mut args, "--tokens").and_then(|value| value.parse().ok()),
        },
        "vce" => Action::Vce {
            tokens: take_value(&mut args, "--tokens").and_then(|value| value.parse().ok()),
        },
        "back" | "backup" => Action::BackupRestore,
        "context" => {
            let tokens = take_value(&mut args, "--tokens").and_then(|value| value.parse().ok());
            let compression =
                take_value(&mut args, "--level").or_else(|| take_value(&mut args, "--compression"));
            let strategy = take_value(&mut args, "--strategy");

            Action::Context {
                input: args.join(" "),
                tokens,
                compression,
                strategy,
            }
        }
        "search" => Action::Search {
            query: args.join(" "),
        },
        "outline" => Action::Outline {
            file: args.join(" "),
        },
        "cat" => Action::Cat {
            file: first_positional(&args),
            from: take_value(&mut args, "--from").and_then(|value| value.parse().ok()),
            to: take_value(&mut args, "--to").and_then(|value| value.parse().ok()),
        },
        "deps" => Action::Deps {
            file: first_positional(&args),
            depth: take_value(&mut args, "--depth")
                .and_then(|value| value.parse().ok())
                .unwrap_or(1),
        },
        "cache" => Action::Cache,
        "cache-clear" => Action::CacheClear,
        "skills" => Action::Skills,
        "skill" => Action::Skill {
            name: args.join(" "),
        },
        "model" => Action::Model {
            name: optional_text(&args),
        },
        "models" => Action::Models {
            filter: optional_text(&args),
        },
        "install" => Action::Install,
        "provider" | "providers" => Action::Providers {
            name: optional_text(&args),
        },
        "api" | "keys" => Action::Api {
            provider: optional_text(&args),
        },
        "vault" => Action::VaultReset,
        "theme" => Action::Theme {
            name: optional_text(&args),
        },
        "new" | "new-session" => Action::NewSession,
        "sessions" => Action::Sessions,
        "session" => Action::SwitchSession { id: args.join(" ") },
        "cwd" | "pwd" => Action::Cwd,
        "project" | "open" => Action::Project {
            path: optional_text(&args),
        },
        "projects" | "browse" => Action::Projects,
        "cost" | "spend" => Action::Cost,
        other => Action::Unknown {
            name: other.to_owned(),
        },
    }
}

/// Removes `flag` from `args`, reporting whether it was present.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|arg| arg != flag);
    args.len() != before
}

/// Removes `flag` and its value from `args`.
fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let position = args.iter().position(|arg| arg == flag)?;
    args.remove(position);
    if position < args.len() {
        Some(args.remove(position))
    } else {
        None
    }
}

/// First argument that is not a flag.
fn first_positional(args: &[String]) -> String {
    args.iter()
        .find(|arg| !arg.starts_with("--"))
        .cloned()
        .unwrap_or_default()
}

/// Free text of an optional argument: `None` when it is empty.
fn optional_text(args: &[String]) -> Option<String> {
    let text = args.join(" ");
    let text = text.trim();

    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_without_a_slash_is_not_a_command() {
        assert_eq!(parse("read src/main.rs"), Action::NotACommand);
        assert_eq!(parse("   "), Action::NotACommand);
    }

    #[test]
    fn parses_simple_commands() {
        assert_eq!(parse("/help"), Action::Help);
        assert_eq!(parse("/quit"), Action::Quit);
        assert_eq!(parse("/q"), Action::Quit);
        assert_eq!(parse("/cache"), Action::Cache);
        assert_eq!(parse("/install"), Action::Install);
        assert_eq!(parse("/index --force"), Action::Index { force: true });
    }

    #[test]
    fn parses_map_and_context_options() {
        assert_eq!(
            parse("/map --tokens 500"),
            Action::Map { tokens: Some(500) }
        );
        assert_eq!(
            parse("/context explain the router --tokens 700 --level aggressive"),
            Action::Context {
                input: "explain the router".to_owned(),
                tokens: Some(700),
                compression: Some("aggressive".to_owned()),
                strategy: None,
            }
        );
    }

    #[test]
    fn keeps_spaces_in_free_text() {
        assert_eq!(
            parse("/search  auth  middleware "),
            Action::Search {
                query: "auth middleware".to_owned()
            }
        );
    }

    #[test]
    fn parses_cat_ranges_and_deps_depth() {
        assert_eq!(
            parse("/cat src/main.rs --from 3 --to 9"),
            Action::Cat {
                file: "src/main.rs".to_owned(),
                from: Some(3),
                to: Some(9),
            }
        );
        assert_eq!(
            parse("/deps src/lib.rs --depth 2"),
            Action::Deps {
                file: "src/lib.rs".to_owned(),
                depth: 2,
            }
        );
        assert_eq!(
            parse("/deps src/lib.rs"),
            Action::Deps {
                file: "src/lib.rs".to_owned(),
                depth: 1,
            }
        );
    }

    #[test]
    fn unknown_commands_are_reported() {
        assert_eq!(
            parse("/nope"),
            Action::Unknown {
                name: "nope".to_owned()
            }
        );
        assert_eq!(
            parse("/"),
            Action::Unknown {
                name: String::new()
            }
        );
    }

    #[test]
    fn parses_session_model_and_theme_commands() {
        assert_eq!(parse("/model"), Action::Model { name: None });
        assert_eq!(
            parse("/model gpt-6-luna"),
            Action::Model {
                name: Some("gpt-6-luna".to_owned())
            }
        );
        assert_eq!(parse("/models"), Action::Models { filter: None });
        assert_eq!(
            parse("/models luna"),
            Action::Models {
                filter: Some("luna".to_owned())
            }
        );
        assert_eq!(parse("/theme"), Action::Theme { name: None });
        assert_eq!(
            parse("/theme mono"),
            Action::Theme {
                name: Some("mono".to_owned())
            }
        );
        assert_eq!(parse("/new"), Action::NewSession);
        assert_eq!(parse("/sessions"), Action::Sessions);
        assert_eq!(
            parse("/session s-1731234567"),
            Action::SwitchSession {
                id: "s-1731234567".to_owned()
            }
        );
        assert_eq!(parse("/cwd"), Action::Cwd);
        assert_eq!(parse("/cost"), Action::Cost);
    }

    #[test]
    fn every_documented_command_is_reachable() {
        for spec in SPECS {
            let line = format!("/{}", spec.name);
            assert!(
                !matches!(parse(&line), Action::Unknown { .. }),
                "documented command `{}` does not parse",
                spec.name
            );
        }
    }
}
