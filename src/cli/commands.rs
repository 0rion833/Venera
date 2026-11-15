//! One function per CLI command.
//!
//! `--test`, `models`, `config check`, and the index commands (`index`, `map`,
//! `measure`) are real: they run offline, need no API key, and print the token math
//! behind every price. `models --refresh` is the only command that touches the network.
//! `theme validate` and `run` are still stubs.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{Encoding, LoadedConfig, Model, Reasoning, Vendor};
use crate::core::budget::{Committed, ContextBudget};
use crate::core::cost::{format_money, format_rate, format_rub, format_tokens, turn_cost};
use crate::core::prompt;
use crate::core::selection::{self, Origin, Selection};
use crate::paths;
use crate::provider::catalog::{Catalog, CatalogModel, PriceUnit};
use crate::provider::keys::{self, KeyStatus};
use crate::provider::registry;
use crate::provider::usage::Usage;
use crate::security::permissions::Permissions;
use crate::vce::context::compress::CompressionLevel;
use crate::vce::context::distribute::{
    ContextOutput, ContextProvider, ContextRequest, Distributor,
};
use crate::vce::context::route::ExecutionStrategy;
use crate::vce::index::delta;
use crate::vce::index::map::{MapOptions, ProjectMap};
use crate::vce::index::measure::{self, MeasureOptions};
use crate::vce::index::query::{Dependencies, Engine, SymbolHit};
use crate::vce::index::refresh::{IndexReport, RefreshOptions};
use crate::vce::index::scanner;
use crate::vce::sync::{self, SyncOptions};
use crate::vce::tools::registry::Dispatcher;

/// Options of `venera models`.
#[derive(Debug, Clone, Copy)]
pub struct ModelsOptions<'a> {
    /// Re-download the catalog before listing.
    pub refresh: bool,
    /// Include models priced per character, second, or minute.
    pub all: bool,
    /// Case-insensitive substring over id and name.
    pub filter: Option<&'a str>,
    /// Maximum number of catalog rows.
    pub limit: usize,
}

/// `venera config check` — validate the config and report where it came from.
pub fn config_check(loaded: &LoadedConfig) -> ExitCode {
    if let Err(err) = loaded.config.validate() {
        eprintln!("venera: config is invalid: {err}");
        return ExitCode::FAILURE;
    }

    let models: usize = loaded
        .config
        .vendors
        .values()
        .map(|vendor| vendor.models.len())
        .sum();
    let defaults = match (
        loaded.config.defaults.vendor.as_deref(),
        loaded.config.defaults.model.as_deref(),
    ) {
        (Some(vendor), Some(model)) => format!("{vendor} / {model}"),
        _ => "not set".to_owned(),
    };

    println!("config        {}", loaded.source.describe());
    println!("vendors       {}", loaded.config.vendors.len());
    println!("models        {models}");
    println!("defaults      {defaults}");

    match Catalog::load_cached() {
        Ok(Some(catalog)) => println!(
            "catalog       {} models, fetched {}",
            catalog.models.len(),
            format_age(catalog.fetched_at_unix)
        ),
        Ok(None) => println!("catalog       not cached (run `venera models --refresh`)"),
        Err(err) => println!("catalog       unreadable: {err}"),
    }

    for (name, vendor) in &loaded.config.vendors {
        println!("key           {name}  {}", key_state(vendor));
    }

    println!("result        ok");
    ExitCode::SUCCESS
}

/// `venera models` — list curated models and the fetched aggregator catalog.
pub async fn models(
    loaded: &LoadedConfig,
    vendor: Option<&str>,
    options: &ModelsOptions<'_>,
) -> ExitCode {
    let name = match vendor.or(loaded.config.defaults.vendor.as_deref()) {
        Some(name) => name,
        None => {
            eprintln!("venera: no vendor given and `defaults.vendor` is not set");
            return ExitCode::FAILURE;
        }
    };

    let vendor_cfg = match loaded.config.vendor(name) {
        Ok(vendor_cfg) => vendor_cfg,
        Err(err) => {
            eprintln!("venera: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("vendor        {name}  {}", vendor_cfg.chat_url());
    println!("key           {}", key_state(vendor_cfg));

    let catalog = if options.refresh {
        match refresh_catalog().await {
            Ok((catalog, path)) => {
                println!(
                    "refreshed     {} models from {} → {}",
                    catalog.models.len(),
                    catalog.source_url,
                    paths::display(&path)
                );
                Some(catalog)
            }
            Err(err) => {
                eprintln!("venera: catalog refresh failed: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        cached_catalog()
    };

    match &catalog {
        Some(catalog) => println!(
            "catalog       {} models, fetched {}",
            catalog.models.len(),
            format_age(catalog.fetched_at_unix)
        ),
        None => println!("catalog       not cached (run `venera models --refresh`)"),
    }

    let needle = options.filter.map(str::to_ascii_lowercase);
    let matches = |id: &str, name: &str| match &needle {
        Some(needle) => {
            id.to_ascii_lowercase().contains(needle) || name.to_ascii_lowercase().contains(needle)
        }
        None => true,
    };

    let curated: Vec<(&String, &Model)> = vendor_cfg
        .models
        .iter()
        .filter(|(key, model)| matches(key, &model.id))
        .collect();

    println!();
    println!("CURATED ({})", curated.len());
    if curated.is_empty() {
        println!("no curated model matches the filter");
    } else {
        println!(
            "{:<18} {:>12} {:>13} {:>13} {:>12} {:>12}  THINKING",
            "MODEL", "CONTEXT", "IN", "OUT", "CACHE R", "CACHE W"
        );
        for (key, model) in &curated {
            let rates = &model.price.rates;
            println!(
                "{:<18} {:>12} {:>13} {:>13} {:>12} {:>12}  {}",
                key,
                format_tokens(model.context_window),
                format_rate(rates.input),
                format_rate(rates.output),
                optional_rate(rates.cache_read),
                optional_rate(rates.cache_write),
                model.reasoning
            );
        }
    }

    let Some(catalog) = &catalog else {
        return ExitCode::SUCCESS;
    };

    let curated_ids: BTreeSet<&str> = vendor_cfg
        .models
        .values()
        .map(|model| model.id.as_str())
        .collect();
    let priced_count = catalog.token_models(None).len();

    let rows: Vec<&CatalogModel> = catalog
        .models
        .iter()
        .filter(|model| options.all || model.is_token_priced())
        .filter(|model| !curated_ids.contains(model.id.as_str()))
        .filter(|model| matches(&model.id, &model.name))
        .collect();

    println!();
    if options.all {
        println!(
            "CATALOG (all {} models, {} match)",
            catalog.models.len(),
            rows.len()
        );
    } else {
        println!(
            "CATALOG ({priced_count} token-priced of {} models, {} match)",
            catalog.models.len(),
            rows.len()
        );
    }

    if rows.is_empty() {
        println!("no catalog model matches the filter");
        return ExitCode::SUCCESS;
    }

    println!(
        "{:<40} {:>12} {:>13} {:>13} {:>9}  TAGS",
        "MODEL", "CONTEXT", "IN", "OUT", "UNIT"
    );
    for model in rows.iter().take(options.limit) {
        let tags = model.tags();
        println!(
            "{:<40} {:>12} {:>13} {:>13} {:>9}  {}",
            model.id,
            format_context(model.context_window),
            catalog_rate(model.price.and_then(|price| price.input), model.unit),
            catalog_rate(model.price.and_then(|price| price.output), model.unit),
            model.unit.map_or("—", PriceUnit::short),
            if tags.is_empty() {
                "—".to_owned()
            } else {
                tags.join(", ")
            }
        );
    }

    let shown = options.limit.min(rows.len());
    if shown < rows.len() {
        println!(
            "… {} more; raise --limit or narrow --filter",
            rows.len() - shown
        );
    }

    ExitCode::SUCCESS
}

/// `--test <text>` — count tokens locally and price the request in RUB.
pub fn token_test(
    loaded: &LoadedConfig,
    vendor: Option<&str>,
    model: Option<&str>,
    text: &str,
    reasoning: Option<Reasoning>,
    bare: bool,
) -> ExitCode {
    let catalog = cached_catalog();

    let selection = match selection::select(&loaded.config, catalog.as_ref(), vendor, model) {
        Ok(selection) => selection,
        Err(err) => {
            eprintln!("venera: {err}");
            return ExitCode::FAILURE;
        }
    };

    let reasoning = reasoning.unwrap_or(selection.reasoning);
    let breakdown = prompt::count(selection.encoding, text, !bare);
    let usage = Usage {
        input: breakdown.total() as u64,
        ..Usage::default()
    };
    let cost = turn_cost(&selection.price, &usage);
    let rates = selection.price.rates_for(cost.prompt_tokens);

    println!("venera · token test — offline, no API key needed");
    println!();
    print_selection(&selection, reasoning);
    println!();
    println!("text          {text:?}");
    if bare {
        println!(
            "prompt        user {} + framing {} = {} tokens (bare: no system prompt)",
            breakdown.user,
            breakdown.framing,
            breakdown.total()
        );
    } else {
        println!(
            "prompt        system {} + digest {} + user {} + framing {} = {} tokens",
            breakdown.system,
            breakdown.digest,
            breakdown.user,
            breakdown.framing,
            breakdown.total()
        );
    }
    println!();
    println!(
        "input         {} tok × {} = {}",
        format_tokens(usage.input),
        format_rate(rates.input),
        format_rub(cost.input_rub)
    );
    println!(
        "output        {} tok × {} = {}",
        format_tokens(usage.output),
        format_rate(rates.output),
        format_rub(cost.output_rub)
    );
    println!(
        "cache read    {} tok × {} = {}",
        format_tokens(usage.cache_read),
        format_rate(rates.cache_read_or_input()),
        format_rub(cost.cache_read_rub)
    );
    println!(
        "cache write   {} tok × {} = {}",
        format_tokens(usage.cache_write),
        format_rate(rates.cache_write_or_input()),
        format_rub(cost.cache_write_rub)
    );
    println!();
    println!(
        "tier          {}",
        if cost.long_context {
            "long context"
        } else {
            "normal"
        }
    );
    println!(
        "note          output and thinking tokens are known only after a real reply; real requests also send tool schemas"
    );
    println!("ИТОГО         {}", format_rub(cost.total_rub()));
    ExitCode::SUCCESS
}

/// Opens the engine, reporting a clean message on failure.
fn open_engine(loaded: &LoadedConfig, root: &str) -> Option<Engine> {
    match Engine::open(root, &loaded.config.index.database) {
        Ok(engine) => Some(engine),
        Err(err) => {
            eprintln!("venera: cannot open the index: {err}");
            None
        }
    }
}

fn refresh_options(loaded: &LoadedConfig, force: bool) -> RefreshOptions {
    RefreshOptions {
        max_file_size: loaded.config.index.max_file_size,
        respect_gitignore: loaded.config.index.respect_gitignore,
        force,
    }
}

/// `venera providers` — supported aggregators, keys, cached catalogs.
pub async fn providers(refresh: bool) -> ExitCode {
    for provider in registry::PROVIDERS {
        println!("provider      {}  {}", provider.id, provider.title);
        println!("site          {}", provider.site);
        println!("api           {}", provider.base_url);

        let key = match keys::status(provider.id) {
            KeyStatus::Missing => "not set (paste it in the TUI: /provider)".to_owned(),
            KeyStatus::Present { masked, origin } => format!("{masked} ({})", origin.label()),
            KeyStatus::Error(err) => format!("error: {err}"),
        };
        println!("key           {key}");

        match Catalog::load_cached_for(provider.id) {
            Ok(Some(catalog)) => println!(
                "catalog       {} models, fetched {}",
                catalog.models.len(),
                format_age(catalog.fetched_at_unix)
            ),
            Ok(None) => println!("catalog       not cached"),
            Err(err) => println!("catalog       broken cache: {err}"),
        }
        println!();
    }

    if refresh {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                eprintln!("venera: cannot build the HTTP client: {err}");
                return ExitCode::FAILURE;
            }
        };

        for provider in registry::PROVIDERS {
            match Catalog::fetch_for(provider.id, &client).await {
                Ok(catalog) => match catalog.save_cached() {
                    Ok(path) => println!(
                        "refreshed     {}: {} models → {}",
                        provider.id,
                        catalog.models.len(),
                        paths::display(&path)
                    ),
                    Err(err) => eprintln!("venera: {}: {err}", provider.id),
                },
                Err(err) => eprintln!("venera: {}: {err}", provider.id),
            }
        }
    }

    ExitCode::SUCCESS
}

/// `venera index` — bring the persistent index in line with the disk.
pub fn index_db(loaded: &LoadedConfig, path: &str, force: bool, rebuild: bool) -> ExitCode {
    let Some(engine) = open_engine(loaded, path) else {
        return ExitCode::FAILURE;
    };

    if rebuild && let Err(err) = engine.store().clear() {
        eprintln!("venera: cannot clear the index: {err}");
        return ExitCode::FAILURE;
    }

    let report = match engine.refresh(&refresh_options(loaded, force || rebuild)) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("venera: cannot index `{path}`: {err}");
            return ExitCode::FAILURE;
        }
    };

    print_index_report(&engine, &report);
    ExitCode::SUCCESS
}

fn print_index_report(engine: &Engine, report: &IndexReport) {
    println!("venera · index — offline, no API key needed");
    println!();
    println!("root          {}", paths::display(engine.root()));
    println!("database      {}", paths::display(engine.store().path()));
    println!("files         {}", format_tokens(report.files as u64));
    println!("symbols       {}", format_tokens(report.symbols as u64));
    println!("imports       {}", format_tokens(report.imports as u64));
    println!("added         {}", report.added);
    println!("modified      {}", report.modified);
    println!("unchanged     {}", report.unchanged);
    println!("deleted       {}", report.deleted);
    println!("parsed        {}", report.reparsed);
}

/// `venera map <path>` — print the budgeted map and nothing else.
pub fn project_map(
    loaded: &LoadedConfig,
    root: &str,
    max_tokens: Option<usize>,
    task: Option<&str>,
    show_delta: bool,
) -> ExitCode {
    let Some(engine) = open_engine(loaded, root) else {
        return ExitCode::FAILURE;
    };

    let index_config = &loaded.config.index;
    let max_tokens = max_tokens.unwrap_or(index_config.map.default_token_budget);
    let options = MapOptions {
        max_tokens,
        max_symbols_per_file: index_config.map.max_symbols_per_file,
        ..MapOptions::default()
    };

    let map = match engine.map(&options, task) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("venera: cannot map `{root}`: {err}");
            return ExitCode::FAILURE;
        }
    };

    if show_delta {
        match delta::compute(engine.store()) {
            Ok(Some(delta)) => println!("{}\n", delta.render(20)),
            Ok(None) => println!("no previous map generation; run `venera map` first\n"),
            Err(err) => {
                eprintln!("venera: cannot compute the delta: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    if map.text.is_empty() {
        println!(
            "venera: nothing to map in `{}`",
            paths::display(engine.root())
        );
    } else {
        println!("{}", map.text);
        println!();
        println!(
            "map           {} tokens, {} symbols in {} files (budget {})",
            format_tokens(map.tokens as u64),
            map.symbols,
            map.files,
            format_tokens(max_tokens as u64)
        );
        if let Some(task) = task {
            println!("task          {task}");
        }
    }

    if let Err(err) = save_map(&engine, &map) {
        eprintln!("venera: warning: cannot store the map generation: {err}");
    }

    ExitCode::SUCCESS
}

/// Stores the rendered map and its symbol snapshot, so the next run can diff.
fn save_map(engine: &Engine, map: &ProjectMap) -> Result<(), crate::vce::index::m1::StoreError> {
    let previous = engine
        .store()
        .map_state()?
        .map_or(0, |(_, _, generation)| generation);
    let hash = blake3::hash(map.text.as_bytes()).to_hex().to_string();
    let keys = engine.store().symbol_keys()?;
    engine
        .store()
        .set_map_state(&map.text, &hash, previous + 1, &keys)
}

/// Opens the engine and prints a failure as an exit code.
macro_rules! engine_or_fail {
    ($loaded:expr, $root:expr) => {
        match open_engine($loaded, $root) {
            Some(engine) => engine,
            None => return ExitCode::FAILURE,
        }
    };
}

/// `venera search <query>` — files and symbols in one pass.
pub fn search(loaded: &LoadedConfig, root: &str, query: &str, limit: usize) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    let files = match engine.search_files(query, limit) {
        Ok(hits) => hits,
        Err(err) => {
            eprintln!("venera: search failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    let symbols = match engine.search_symbols(query, limit) {
        Ok(hits) => hits,
        Err(err) => {
            eprintln!("venera: search failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("files ({}):", files.len());
    for hit in &files {
        println!("  {} ({} symbols, {})", hit.path, hit.symbols, hit.language);
    }

    println!();
    println!("symbols ({}):", symbols.len());
    for hit in &symbols {
        println!(
            "  {}:{}-{} {} {}",
            hit.path, hit.start_line, hit.end_line, hit.kind, hit.qualified_name
        );
    }

    ExitCode::SUCCESS
}

/// `venera sync <paths...>` — reconcile the index with the disk after a task.
pub fn sync_project(loaded: &LoadedConfig, root: &str, paths: &[String], force: bool) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    let report = match sync::synchronize(&engine, paths, &SyncOptions { force }) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("venera: sync failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("venera · sync — offline, no API key needed");
    println!();
    println!("root          {}", paths::display(engine.root()));
    println!("checked       {}", report.checked);
    println!("created       {}", report.created.len());
    for path in &report.created {
        println!("  + {path}");
    }
    println!("modified      {}", report.modified.len());
    for path in &report.modified {
        println!("  ~ {path}");
    }
    println!("deleted       {}", report.deleted.len());
    for path in &report.deleted {
        println!("  - {path}");
    }
    println!("unchanged     {}", report.unchanged);
    println!("failed        {}", report.failed.len());
    for failure in &report.failed {
        eprintln!("  ! {}: {}", failure.path, failure.reason);
    }
    println!("generation    {}", report.generation);
    println!("invalidated   {}", report.invalidated);
    println!("symbols       {}", report.symbols);
    println!(
        "state         {}",
        if report.changed { "changed" } else { "clean" }
    );

    ExitCode::SUCCESS
}

/// `venera outline <file>` — symbols of one file, without reading it.
pub fn outline(loaded: &LoadedConfig, root: &str, file: &str) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    match engine.outline(file) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("venera: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `venera cat <file>` — a verified line range of one file.
pub fn cat(
    loaded: &LoadedConfig,
    root: &str,
    file: &str,
    from: Option<u32>,
    to: Option<u32>,
) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    let view = match engine.source(file, from, to) {
        Ok(view) => view,
        Err(err) => {
            eprintln!("venera: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "{}: lines {}-{} of {} (hash {}, {}{})",
        view.path,
        view.start_line,
        view.end_line,
        view.total_lines,
        view.content_hash.get(..12).unwrap_or(&view.content_hash),
        if view.complete { "complete" } else { "partial" },
        if view.refreshed { ", reindexed" } else { "" }
    );
    println!();
    println!("{}", view.text);
    ExitCode::SUCCESS
}

/// `venera refs <name>` — declarations mentioning a name.
pub fn references(loaded: &LoadedConfig, root: &str, name: &str, limit: usize) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    match engine.references(name, limit) {
        Ok(hits) => {
            print_symbol_hits(&hits);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("venera: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `venera deps <file>` — imports and importers.
pub fn dependencies(loaded: &LoadedConfig, root: &str, file: &str, depth: u32) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    match engine.dependencies(file, depth) {
        Ok(dependencies) => {
            print_dependencies(&dependencies);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("venera: {err}");
            ExitCode::FAILURE
        }
    }
}

fn print_symbol_hits(hits: &[SymbolHit]) {
    println!("{} hits", hits.len());
    for hit in hits {
        println!(
            "  {}:{}-{} {} {}",
            hit.path, hit.start_line, hit.end_line, hit.kind, hit.qualified_name
        );
    }
}

fn print_dependencies(dependencies: &Dependencies) {
    println!("{}", dependencies.path);

    println!("imports ({}):", dependencies.imports.len());
    for import in &dependencies.imports {
        println!(
            "  depth {} {}{}",
            import.depth,
            import.path,
            if import.resolved { "" } else { " (external)" }
        );
    }

    println!("imported by ({}):", dependencies.imported_by.len());
    for path in &dependencies.imported_by {
        println!("  {path}");
    }
}

/// `venera tool <name>` — run one model-facing tool offline.
pub fn tool(loaded: &LoadedConfig, root: &str, name: &str, arguments: Option<&str>) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    let arguments: serde_json::Value = match arguments {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("venera: --args is not valid JSON: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => serde_json::json!({}),
    };

    let dispatcher = Dispatcher::new(Permissions::default());
    match dispatcher.call(&engine, name, &arguments) {
        Ok(output) => {
            print!("{}", output.text);
            if output.truncated {
                println!("… output truncated ({} lines hidden)", output.hidden_lines);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("venera: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `venera budget` — show how a window is split between context categories.
pub fn budget(
    window: u64,
    output: Option<u64>,
    turn_cap: Option<u64>,
    committed: Committed,
) -> ExitCode {
    let budget = match output {
        Some(reserve) => ContextBudget::new(window, reserve),
        None => ContextBudget::for_turn(
            window,
            None,
            turn_cap.unwrap_or_else(|| (window / 10).max(1_024)),
        ),
    };
    let allocation = budget.allocate(&committed);

    println!("venera · budget — offline, no API key needed");
    println!();
    println!("window        {}", format_tokens(window));
    println!("committed     {}", format_tokens(committed.total()));
    println!("reserve       {}", format_tokens(budget.reserve_output));
    if let Some(cap) = turn_cap {
        println!(
            "turn cap      {} (limits.max_output_tokens_per_turn)",
            format_tokens(cap)
        );
    }
    println!("margin        {}", format_tokens(budget.safety_margin));
    println!("usable        {}", format_tokens(allocation.usable));
    println!();
    println!("project map   {}", format_tokens(allocation.project_map));
    println!("retrieval     {}", format_tokens(allocation.retrieval));
    println!("conversation  {}", format_tokens(allocation.conversation));
    println!("free          {}", format_tokens(allocation.free));

    ExitCode::SUCCESS
}

/// `venera measure <path>` — tokens and RUB before and after the index.
pub fn measure_project(
    loaded: &LoadedConfig,
    path: &str,
    rate: f64,
    max_tokens: Option<usize>,
    baseline_top: usize,
    task: Option<&str>,
) -> ExitCode {
    if !rate.is_finite() || rate < 0.0 {
        eprintln!("venera: --rate must be a non-negative number");
        return ExitCode::FAILURE;
    }
    if baseline_top == 0 {
        eprintln!("venera: --baseline-top must be at least 1");
        return ExitCode::FAILURE;
    }

    let index_config = &loaded.config.index;
    let scan = match scanner::scan(path, index_config.max_file_size) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("venera: cannot measure `{path}`: {err}");
            return ExitCode::FAILURE;
        }
    };

    let options = MeasureOptions {
        encoding: Encoding::O200kBase,
        map_tokens: max_tokens.unwrap_or(index_config.map.default_token_budget),
        max_symbols_per_file: index_config.map.max_symbols_per_file,
        rate_rub: rate,
        baseline_top,
        task: task.map(str::to_owned),
    };
    let result = measure::measure(&scan, &options);

    println!("venera · measure — offline, no API key needed");
    println!();
    println!("root          {}", paths::display(&scan.root));
    println!("rate          {}", format_rate(rate));
    if let Some(task) = task {
        println!("task          {task}");
    }
    println!(
        "files         {}  ({} symbols)",
        result.files, result.symbols
    );
    println!();

    println!("{:<12} {:>12} {:>14}", "PATH", "TOKENS", "RUB");
    println!(
        "{:<12} {:>12} {:>14}",
        "listing",
        format_tokens(result.listing_tokens as u64),
        format_rub(rub(result.listing_tokens, rate))
    );
    println!(
        "{:<12} {:>12} {:>14}",
        "read all",
        format_tokens(result.all_tokens as u64),
        format_rub(result.all_rub(rate))
    );
    println!(
        "{:<12} {:>12} {:>14}",
        format!("read top {}", result.baseline_files),
        format_tokens(result.baseline_tokens as u64),
        format_rub(result.baseline_rub(rate))
    );
    println!(
        "{:<12} {:>12} {:>14}",
        "tool",
        format_tokens(result.tool_tokens as u64),
        format_rub(result.tool_rub(rate))
    );
    println!(
        "{:<12} {:>12} {:>14}",
        "saved all",
        signed_tokens(result.all_tokens as i64 - result.tool_tokens as i64),
        format_rub(result.saved_all_rub(rate))
    );
    println!(
        "{:<12} {:>12} {:>14}",
        format!("saved top {}", result.baseline_files),
        signed_tokens(result.baseline_tokens as i64 - result.tool_tokens as i64),
        format_rub(result.saved_rub(rate))
    );
    println!();

    println!(
        "baseline      listing + {} files read in full",
        result.baseline_files
    );
    println!(
        "tool          map {} tokens + 1 file {} tokens",
        format_tokens(result.map.tokens as u64),
        format_tokens(result.retrieval_tokens as u64)
    );
    println!(
        "saving        {:.1}% vs read all, {:.1}% vs read top {}",
        result.savings_all_ratio() * 100.0,
        result.savings_ratio() * 100.0,
        result.baseline_files
    );

    if let Some(break_even) = result.break_even_files() {
        println!(
            "break-even    the map pays off from about {break_even} files of this size; this project has {}",
            result.files
        );
    } else if result.files > 0 {
        println!(
            "break-even    none: the whole project is smaller than the map, so reading it directly wins"
        );
    }

    ExitCode::SUCCESS
}

/// RUB for `tokens` at `rate` per 1M tokens.
fn rub(tokens: usize, rate: f64) -> f64 {
    tokens as f64 / 1_000_000.0 * rate
}

/// Token delta with an explicit sign, so "saved" is never ambiguous.
fn signed_tokens(value: i64) -> String {
    if value < 0 {
        format!("-{}", format_tokens(value.unsigned_abs()))
    } else {
        format_tokens(value as u64)
    }
}

/// `venera context "<input>"` — prepare context without calling a model.
pub fn context(
    loaded: &LoadedConfig,
    root: &str,
    input: &str,
    max_tokens: usize,
    compression: Option<&str>,
    strategy: Option<&str>,
    format: &str,
) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);
    let settings = &loaded.config.context;

    let level = match compression {
        Some(name) => match CompressionLevel::from_name(name) {
            Some(level) => level,
            None => {
                eprintln!(
                    "venera: unknown compression level `{name}`; expected none, light, balanced, aggressive, semantic"
                );
                return ExitCode::FAILURE;
            }
        },
        None => CompressionLevel::from_name(&settings.compression.default_level)
            .unwrap_or(CompressionLevel::Balanced),
    };
    let level = if settings.enabled && settings.compression.enabled {
        level
    } else {
        CompressionLevel::None
    };

    let requested = strategy
        .map(str::to_owned)
        .unwrap_or_else(|| settings.default_strategy.clone());
    let forced = if requested == "adaptive" {
        None
    } else {
        match ExecutionStrategy::from_name(&requested) {
            Some(strategy) => Some(strategy),
            None => {
                eprintln!(
                    "venera: unknown strategy `{requested}`; expected adaptive, direct-read, partial-read, indexed-retrieval, project-map, hybrid"
                );
                return ExitCode::FAILURE;
            }
        }
    };

    let window = loaded
        .config
        .resolve_model(None, None)
        .map(|resolved| resolved.model.context_window)
        .unwrap_or(1_050_000);

    let mut distributor = Distributor::new(Encoding::O200kBase, window, None)
        .with_cache(
            settings.cache.m0_max_entries,
            settings.cache.m1_ttl_seconds,
            settings.cache.enabled && settings.enabled,
        )
        .with_rules(
            settings.distribution.prefer_partial_source,
            settings.distribution.avoid_map_for_direct_tasks,
            settings.distribution.allow_context_expansion,
        )
        .with_compression(
            settings.compression.target_retained_ratio,
            settings.compression.minimum_savings_ratio,
        )
        .with_validation(settings.compression.validate_output);

    let request = ContextRequest {
        input: input.to_owned(),
        max_tokens,
        compression: level,
        strategy: forced,
    };

    let output = match distributor.prepare(&engine, &request) {
        Ok(output) => output,
        Err(err) => {
            eprintln!("venera: cannot prepare context: {err}");
            return ExitCode::FAILURE;
        }
    };

    match format {
        "json" => match serde_json::to_string_pretty(&output) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("venera: cannot encode JSON: {err}");
                return ExitCode::FAILURE;
            }
        },
        "text" => print_context(&output),
        other => {
            eprintln!("venera: unknown format `{other}`; expected text or json");
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

fn print_context(output: &ContextOutput) {
    println!("strategy      {}", output.strategy.as_str());
    println!(
        "original      {} tokens",
        format_tokens(output.original_tokens as u64)
    );
    println!(
        "final         {} tokens",
        format_tokens(output.final_tokens as u64)
    );
    println!(
        "compression   {} ({:.1}% off)",
        output.compression.level.as_str(),
        output.compression.reduction_ratio * 100.0
    );
    println!("segments      {}", output.segments);
    println!(
        "sources       {}",
        if output.sources.is_empty() {
            "—".to_owned()
        } else {
            output.sources.join(", ")
        }
    );
    println!(
        "cache         {}",
        if output.cache_hits > 0 { "hit" } else { "miss" }
    );
    if !output.validation_missing.is_empty() {
        println!(
            "validation    missing: {}",
            output.validation_missing.join(", ")
        );
    }
    println!();
    println!("{}", output.content);
}

/// `venera cache stats` — what the working cache holds.
pub fn cache_stats(loaded: &LoadedConfig, root: &str) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    let totals = match engine.store().cache_totals() {
        Ok(totals) => totals,
        Err(err) => {
            eprintln!("venera: cannot read the cache: {err}");
            return ExitCode::FAILURE;
        }
    };
    let generation = engine.store().generation().unwrap_or(0);

    println!("venera · cache");
    println!();
    println!("project       {}", paths::display(engine.root()));
    println!("m1 entries    {}", totals.entries);
    println!("m1 bytes      {}", format_tokens(totals.bytes as u64));
    println!("m1 accesses   {}", totals.accesses);
    println!("generation    {generation}");
    println!("m0            runtime only; a fresh process starts empty");

    ExitCode::SUCCESS
}

/// `venera cache clear` — drop every stored entry.
pub fn cache_clear(loaded: &LoadedConfig, root: &str) -> ExitCode {
    let engine = engine_or_fail!(loaded, root);

    match engine.store().cache_clear() {
        Ok(removed) => {
            println!("removed {removed} cache entries");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("venera: cannot clear the cache: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `venera tui` — interactive terminal shell over the context engine.
pub fn tui(loaded: &LoadedConfig, root: &str) -> ExitCode {
    match crate::tui::run(loaded, root) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("venera: cannot start the TUI: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `venera theme validate <path>` — validate an XML theme.
pub fn theme_validate(path: &str) -> ExitCode {
    match crate::theme::xml::load_from_path(std::path::Path::new(path)) {
        Ok(theme) => {
            println!(
                "theme         {}  (palette complete, {} slots)",
                theme.name(),
                theme.len()
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("venera: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `venera run "task"` — headless mode.
pub fn run_task(
    _loaded: &LoadedConfig,
    _vendor: Option<&str>,
    _model: Option<&str>,
    task: &str,
) -> ExitCode {
    tracing::debug!(task, "headless run requested");
    not_implemented("run")
}

fn print_selection(selection: &Selection, reasoning: Reasoning) {
    let origin = match selection.origin {
        Origin::Config => "config",
        Origin::Catalog => "catalog",
    };

    println!(
        "vendor        {}  {}",
        selection.vendor_name, selection.chat_url
    );
    if selection.model_key == selection.model_id {
        println!("model         {}  (from {origin})", selection.model_id);
    } else {
        println!(
            "model         {}  ({}, from {origin})",
            selection.model_id, selection.model_key
        );
    }
    if selection.origin == Origin::Catalog {
        println!(
            "encoding      {} (approximation for non-OpenAI models)",
            selection.encoding.as_str()
        );
    } else {
        println!("encoding      {}", selection.encoding.as_str());
    }
    match (
        selection.context_window,
        selection.price.long_context_threshold(),
    ) {
        (Some(window), Some(threshold)) => println!(
            "context       {} tokens (long-context tier above {})",
            format_tokens(window),
            format_tokens(threshold)
        ),
        (Some(window), None) => println!(
            "context       {} tokens (single tier)",
            format_tokens(window)
        ),
        (None, _) => println!("context       unknown (catalog does not list it)"),
    }
    if reasoning.is_on() {
        println!("reasoning     {reasoning} — thinking tokens are billed as output");
    } else {
        println!("reasoning     off");
    }
}

/// Reads the cached catalog; a broken cache is a warning, not a failure.
fn cached_catalog() -> Option<Catalog> {
    match Catalog::load_cached() {
        Ok(catalog) => catalog,
        Err(err) => {
            eprintln!("venera: ignoring catalog cache: {err}");
            None
        }
    }
}

/// Fetches the catalog from the aggregator and stores it in the cache.
async fn refresh_catalog() -> anyhow::Result<(Catalog, PathBuf)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let catalog = Catalog::fetch(&client).await?;
    let path = catalog.save_cached()?;
    Ok((catalog, path))
}

fn key_state(vendor: &Vendor) -> String {
    let Some(source) = &vendor.api_key else {
        return "not configured (offline only)".to_owned();
    };

    match source.resolve() {
        Ok(Some(_)) => format!("{} — set", source.describe()),
        Ok(None) => format!("{} — not set (offline tests still work)", source.describe()),
        Err(err) => format!("{} — error: {err}", source.describe()),
    }
}

fn optional_rate(rate: Option<f64>) -> String {
    rate.map_or_else(|| "—".to_owned(), format_rate)
}

fn format_context(window: Option<u64>) -> String {
    window.map_or_else(|| "—".to_owned(), format_tokens)
}

/// Catalog prices carry their own unit, so `руб./1M` would be wrong for video and audio.
fn catalog_rate(price: Option<f64>, unit: Option<PriceUnit>) -> String {
    let Some(value) = price else {
        return "—".to_owned();
    };

    match unit {
        Some(PriceUnit::Chars) => format!("{} руб./1M chr", format_money(value)),
        Some(PriceUnit::Second) => format!("{} руб./sec", format_money(value)),
        Some(PriceUnit::Minute) => format!("{} руб./min", format_money(value)),
        _ => format_rate(value),
    }
}

fn format_age(fetched_at_unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let seconds = now.saturating_sub(fetched_at_unix);

    match seconds {
        0..=3600 => "just now".to_owned(),
        3601..=86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

fn not_implemented(command: &str) -> ExitCode {
    eprintln!("venera: `{command}` is not implemented yet");
    ExitCode::FAILURE
}
