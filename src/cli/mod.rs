//! Command-line surface and the only place that wires the other modules together.
//!
//! Commands (`AGENTS.md` §4): `config check`, `models`, `index`, `theme validate <path>`,
//! `run "<task>"`, plus `--test <text>` for offline token/price math.
//!
//! Parsing is clap-derive; the bodies live in [`commands`].

pub mod commands;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::{self, Reasoning};
use crate::core::budget::Committed;

/// Venera — a Rust CLI coding agent.
#[derive(Debug, Parser)]
#[command(name = "venera", version, about, long_about = None)]
pub struct Cli {
    /// Enable debug logging.
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Aggregator to use (overrides `defaults.vendor`).
    #[arg(long, global = true, value_name = "NAME")]
    pub vendor: Option<String>,

    /// Model to use (overrides `defaults.model`); any catalog id works.
    #[arg(long, global = true, value_name = "NAME")]
    pub model: Option<String>,

    /// Thinking level; without a value it means `medium`.
    #[arg(
        long,
        global = true,
        value_name = "LEVEL",
        num_args = 0..=1,
        default_missing_value = "medium",
        value_parser = ["off", "low", "medium", "high"]
    )]
    pub reasoning: Option<String>,

    /// Do not send the system prompt and the AGENTS.md digest.
    #[arg(long, global = true)]
    pub bare: bool,

    /// Estimate tokens and RUB cost for a text without calling the API.
    #[arg(long, value_name = "TEXT")]
    pub test: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Validate config.json and model availability.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List models with prices in RUB.
    Models {
        /// Re-download the full catalog from the aggregator.
        #[arg(long)]
        refresh: bool,
        /// Include models priced per character, second, or minute.
        #[arg(long)]
        all: bool,
        /// Only models whose id or name contains this text.
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
        /// Maximum number of catalog rows to print.
        #[arg(long, default_value_t = 40, value_name = "N")]
        limit: usize,
    },
    /// List supported aggregators, their keys, and cached catalogs.
    Providers {
        /// Re-download every provider catalog.
        #[arg(long)]
        refresh: bool,
    },
    /// Rebuild the M1 index: scan, parse only what changed, report.
    Index {
        /// Project root (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Reparse every file, ignoring the size/mtime hint.
        #[arg(long)]
        force: bool,
        /// Drop the database before indexing.
        #[arg(long)]
        rebuild: bool,
    },
    /// Print the budgeted project map: signatures only, no source.
    Map {
        /// Project root (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Token budget for the map (default: `index.map.default_token_budget`).
        #[arg(long, value_name = "N")]
        max_tokens: Option<usize>,
        /// Task text used to order files by relevance.
        #[arg(long, value_name = "TEXT")]
        task: Option<String>,
        /// Print the structural delta against the previous map.
        #[arg(long)]
        delta: bool,
    },
    /// Compare tokens and RUB before and after the index.
    Measure {
        /// Project root (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// RUB per 1M input tokens.
        #[arg(long, default_value_t = 30.0)]
        rate: f64,
        /// Token budget for the map (default: `index.map.default_token_budget`).
        #[arg(long, value_name = "N")]
        max_tokens: Option<usize>,
        /// How many files a baseline agent reads.
        #[arg(long, default_value_t = 5)]
        baseline_top: usize,
        /// Task text used to rank files by relevance.
        #[arg(long, value_name = "TEXT")]
        task: Option<String>,
    },
    /// Search indexed files and symbols.
    Search {
        /// What to look for.
        query: String,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
        /// Maximum number of rows per section.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print a file's symbols with line ranges, without reading the body.
    Outline {
        /// Project-relative file path.
        file: String,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Print a verified line range of a file.
    Cat {
        /// Project-relative file path.
        file: String,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
        /// First line (one-based).
        #[arg(long)]
        from: Option<u32>,
        /// Last line (one-based).
        #[arg(long)]
        to: Option<u32>,
    },
    /// Find declarations that mention a name.
    Refs {
        /// Symbol or identifier name.
        name: String,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
        /// Maximum number of rows.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show imports of a file and the files that import it.
    Deps {
        /// Project-relative file path.
        file: String,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
        /// How many hops to walk.
        #[arg(long, default_value_t = 1)]
        depth: u32,
    },
    /// Synchronize the index with the filesystem after a task.
    Sync {
        /// Project-relative files the task touched.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
        /// Re-parse even when the content hash is unchanged.
        #[arg(long)]
        force: bool,
    },
    /// Run one model-facing tool offline.
    Tool {
        /// Tool name, see `venera tool --help`.
        name: String,
        /// JSON arguments, e.g. `{"query":"auth"}`.
        #[arg(long)]
        args: Option<String>,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Explain how a model window is split between context categories.
    Budget {
        /// Model context window.
        #[arg(long, default_value_t = 1_050_000)]
        window: u64,
        /// Tokens reserved for the reply (default: a tenth of the window).
        #[arg(long)]
        output: Option<u64>,
        /// Tokens already spent on the system prompt.
        #[arg(long, default_value_t = 0)]
        system: u64,
        /// Tokens already spent on tool schemas.
        #[arg(long, default_value_t = 0)]
        tools: u64,
        /// Tokens already spent on the AGENTS.md digest.
        #[arg(long, default_value_t = 0)]
        instructions: u64,
        /// Tokens already spent on the conversation.
        #[arg(long, default_value_t = 0)]
        history: u64,
        /// Tokens of the current user message.
        #[arg(long, default_value_t = 0)]
        user: u64,
    },
    /// Prepare context for a request without calling a model.
    Context {
        /// The request, in quotes.
        input: String,
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
        /// Token ceiling for the prepared context.
        #[arg(long, default_value_t = 1000)]
        max_tokens: usize,
        /// Compression level: none, light, balanced, aggressive, semantic.
        #[arg(long)]
        compression: Option<String>,
        /// Strategy: adaptive, direct-read, partial-read, indexed-retrieval, project-map, hybrid.
        #[arg(long)]
        strategy: Option<String>,
        /// Output format: text or json.
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// First-run setup: write the user config and choose the interface language.
    Init {
        /// Overwrite an existing configuration.
        #[arg(long)]
        force: bool,
    },
    /// Interactive terminal UI.
    Tui {
        /// Project root (default: current directory).
        #[arg(default_value = ".")]
        root: String,
    },
    /// Inspect or clear the prepared-context cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Validate an XML theme.
    Theme {
        #[command(subcommand)]
        action: ThemeAction,
    },
    /// Headless mode: run a single task and exit.
    Run {
        /// The task for the agent.
        task: String,
    },
}

/// `venera config ...`
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Validate config.json and model availability.
    Check,
}

/// `venera cache ...`
#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Show stored entries, bytes, and the index generation.
    Stats {
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Remove every stored entry.
    Clear {
        /// Project root (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,
    },
}

/// `venera theme ...`
#[derive(Debug, Subcommand)]
pub enum ThemeAction {
    /// Validate the theme at `<path>`.
    Validate {
        /// Path to the theme XML file.
        path: String,
    },
}

/// Parses `args` (including the program name) and dispatches to a command.
///
/// Async because `models --refresh` talks to the aggregator; the binary owns the
/// runtime and calls this through `block_on`.
pub async fn run(args: Vec<OsString>) -> ExitCode {
    let cli = Cli::parse_from(args);
    init_logging(cli.verbose);

    // The first terminal launch makes `venera` reachable by name; piped calls are left alone.
    let _ = crate::path_setup::install();
    // Non-interactive commands can unlock the key vault from the environment.
    let _ = crate::provider::keys::unlock_from_env();

    let reasoning = cli.reasoning.as_deref().and_then(Reasoning::from_name);

    if let Some(text) = cli.test.as_deref() {
        return with_config(|loaded| {
            commands::token_test(
                loaded,
                cli.vendor.as_deref(),
                cli.model.as_deref(),
                text,
                reasoning,
                cli.bare,
            )
        });
    }

    match cli.command {
        Some(Command::Config {
            action: ConfigAction::Check,
        }) => with_config(commands::config_check),
        Some(Command::Models {
            refresh,
            all,
            filter,
            limit,
        }) => match config::Config::load() {
            Ok(loaded) => {
                commands::models(
                    &loaded,
                    cli.vendor.as_deref(),
                    &commands::ModelsOptions {
                        refresh,
                        all,
                        filter: filter.as_deref(),
                        limit,
                    },
                )
                .await
            }
            Err(err) => {
                eprintln!("venera: {err}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Providers { refresh }) => commands::providers(refresh).await,
        Some(Command::Index {
            path,
            force,
            rebuild,
        }) => with_config(|loaded| commands::index_db(loaded, &path, force, rebuild)),
        Some(Command::Map {
            path,
            max_tokens,
            task,
            delta,
        }) => with_config(|loaded| {
            commands::project_map(loaded, &path, max_tokens, task.as_deref(), delta)
        }),
        Some(Command::Measure {
            path,
            rate,
            max_tokens,
            baseline_top,
            task,
        }) => with_config(|loaded| {
            commands::measure_project(
                loaded,
                &path,
                rate,
                max_tokens,
                baseline_top,
                task.as_deref(),
            )
        }),
        Some(Command::Search { query, root, limit }) => {
            with_config(|loaded| commands::search(loaded, &root, &query, limit))
        }
        Some(Command::Outline { file, root }) => {
            with_config(|loaded| commands::outline(loaded, &root, &file))
        }
        Some(Command::Cat {
            file,
            root,
            from,
            to,
        }) => with_config(|loaded| commands::cat(loaded, &root, &file, from, to)),
        Some(Command::Refs { name, root, limit }) => {
            with_config(|loaded| commands::references(loaded, &root, &name, limit))
        }
        Some(Command::Deps { file, root, depth }) => {
            with_config(|loaded| commands::dependencies(loaded, &root, &file, depth))
        }
        Some(Command::Sync { paths, root, force }) => {
            with_config(|loaded| commands::sync_project(loaded, &root, &paths, force))
        }
        Some(Command::Tool { name, args, root }) => {
            with_config(|loaded| commands::tool(loaded, &root, &name, args.as_deref()))
        }
        Some(Command::Budget {
            window,
            output,
            system,
            tools,
            instructions,
            history,
            user,
        }) => commands::budget(
            window,
            output,
            config::Config::load()
                .ok()
                .and_then(|loaded| loaded.config.limits.max_output_tokens_per_turn),
            Committed {
                system,
                tools,
                instructions,
                history,
                user,
            },
        ),
        Some(Command::Context {
            input,
            root,
            max_tokens,
            compression,
            strategy,
            format,
        }) => with_config(|loaded| {
            commands::context(
                loaded,
                &root,
                &input,
                max_tokens,
                compression.as_deref(),
                strategy.as_deref(),
                &format,
            )
        }),
        Some(Command::Init { force }) => {
            if crate::init::run(force) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Some(Command::Tui { root }) => with_config(|loaded| commands::tui(loaded, &root)),
        Some(Command::Cache { action }) => match action {
            CacheAction::Stats { root } => {
                with_config(|loaded| commands::cache_stats(loaded, &root))
            }
            CacheAction::Clear { root } => {
                with_config(|loaded| commands::cache_clear(loaded, &root))
            }
        },
        Some(Command::Theme {
            action: ThemeAction::Validate { path },
        }) => commands::theme_validate(&path),
        Some(Command::Run { task }) => with_config(|loaded| {
            commands::run_task(loaded, cli.vendor.as_deref(), cli.model.as_deref(), &task)
        }),
        None => {
            // Out of the box: a bare launch silently writes defaults when needed and
            // opens the TUI. The interactive setup stays at `venera init`.
            if !crate::init::is_configured() {
                let _ = crate::init::ensure_defaults();
            }

            with_config(|loaded| commands::tui(loaded, "."))
        }
    }
}

/// Loads the config, turning a failure into a clean exit code.
fn with_config(body: impl FnOnce(&config::LoadedConfig) -> ExitCode) -> ExitCode {
    match config::Config::load() {
        Ok(loaded) => body(&loaded),
        Err(err) => {
            eprintln!("venera: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Initializes `tracing` once. Level is `info` in release and `debug` behind `--verbose`.
fn init_logging(verbose: bool) {
    let fallback = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
