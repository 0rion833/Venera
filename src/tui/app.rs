//! Application state of the terminal UI.
//!
//! The UI is a thin shell over the engine: slash commands run synchronously; a plain
//! prompt goes to the active provider on a worker thread and its answer lands in the
//! transcript. Nothing streams yet.
//!
//! Three pieces of state live here and nowhere else: the session (transcript, model,
//! theme, spend), the model picker, and the pulse phase that animates the status bar.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use zeroize::Zeroizing;

use crate::commands::{self, Action, SPECS, Spec};
use crate::config::{Config, Encoding, LoadedConfig, ModelPrice, Reasoning};
use crate::core::cost::{format_rub, format_tokens, turn_cost};
use crate::provider::catalog::Catalog;
use crate::provider::keys::KeyStatus;
use crate::provider::openai_compat::{ChatMessage, ToolCall};
use crate::provider::tokenizer;
use crate::provider::usage::Usage;
use crate::provider::{keys, registry};
use crate::security::permissions::{Action as ToolAction, Permissions};
use crate::skills::{self, Skill};
use crate::theme::{self, DEFAULT_THEME, Theme};
use crate::tui::input::Input;
use crate::tui::job::{self, ChatResult, Job, JobDone};
use crate::tui::models;
use crate::tui::picker::{ModelPicker, PickerKind, Row};
use crate::tui::project::ProjectPicker;
use crate::tui::screens::{self, Hitboxes};
use crate::tui::session::{EntryKind, Session, SessionStore};
use crate::vce::backup;
use crate::vce::context::compress::CompressionLevel;
use crate::vce::context::distribute::{ContextProvider, ContextRequest, Distributor};
use crate::vce::context::route::ExecutionStrategy;
use crate::vce::index::map::MapOptions;
use crate::vce::index::query::Engine;
use crate::vce::index::refresh::RefreshOptions;
use crate::vce::tools::registry::Dispatcher;
use crate::vce::watch;

/// Lines of prepared context shown for one request.
const CONTEXT_PREVIEW_LINES: usize = 60;

/// Radians added to the pulse on every tick, roughly 12 frames per second.
const PULSE_STEP: f32 = 0.10;

/// Seconds one tick lasts, matching the event loop; the title screen intro counts in it.
const TICK_SECONDS: f32 = 0.08;

/// Tool rounds one prompt may take before the agent stops.
const MAX_AGENT_STEPS: usize = 12;

/// A write/edit call waiting for the user's confirmation.
pub struct PendingTool {
    /// The call to execute on approval.
    pub call: ToolCall,
    /// Parsed arguments.
    pub arguments: serde_json::Value,
}

/// Secret prompt for a provider's API key; the text is never logged or saved.
#[derive(Clone)]
pub struct ApiKeyInput {
    /// Provider the key belongs to.
    pub provider: String,
    /// What the user typed or pasted; wiped when the prompt closes.
    pub text: Zeroizing<String>,
}

/// Master-password prompt for the encrypted API-key vault.
pub struct VaultPrompt {
    /// True while creating a new vault, false while unlocking.
    pub create: bool,
    /// First password entry while creating.
    pub first: Option<Zeroizing<String>>,
    /// Current input.
    pub text: Zeroizing<String>,
    /// Validation error, if any.
    pub error: Option<String>,
}

impl VaultPrompt {
    fn new(create: bool) -> Self {
        Self {
            create,
            first: None,
            text: Zeroizing::new(String::new()),
            error: None,
        }
    }
}

/// The UI state.
pub struct App {
    /// Context engine of the current project.
    pub engine: Engine,
    /// Context distributor, including its runtime cache.
    distributor: Distributor,
    /// Skills discovered for the project.
    pub skills: Vec<Skill>,
    /// Configuration the UI runs with.
    config: Config,
    /// Catalog of the active provider, when it has been fetched at least once.
    catalog: Option<Catalog>,
    /// Active aggregator id.
    pub vendor: String,
    /// API key prompt, while it is open.
    pub api_input: Option<ApiKeyInput>,
    /// Master-password prompt, while the vault is locked.
    pub vault_prompt: Option<VaultPrompt>,
    /// True while `/back` waits for confirmation.
    pub confirm_backup: bool,
    /// True while `/vault reset` waits for confirmation.
    pub confirm_reset: bool,
    /// Permissions of this session; writes stay locked until the user confirms.
    permissions: Permissions,
    /// Conversation sent to the provider, including tool results.
    conversation: Vec<ChatMessage>,
    /// Tool calls from the last assistant message, not executed yet.
    pending_tools: VecDeque<ToolCall>,
    /// A write/edit call waiting for `Enter` or `Esc`.
    pub pending_tool: Option<PendingTool>,
    /// Tool rounds taken by the current prompt.
    agent_steps: usize,
    /// Current input: text, caret, and history.
    pub input: Input,
    /// How far the view is scrolled up from the tail.
    pub scroll_back: u16,
    /// Set when the user asked to leave.
    pub quit: bool,
    /// Active palette.
    pub theme: Theme,
    /// Project root, as shown in the header.
    pub cwd: String,
    /// Conversation: transcript, model, theme, spend.
    pub session: Session,
    /// Where sessions are persisted; `None` when no data directory exists.
    store: Option<SessionStore>,
    /// Model picker, open while the user chooses.
    pub picker: Option<ModelPicker>,
    /// Project picker, open while the user browses directories.
    pub project: Option<ProjectPicker>,
    /// Highlighted row of the command completion menu.
    pub completion: usize,
    /// True while the title screen is shown.
    pub start: bool,
    /// Seconds the title screen has been visible; turns the planet's light.
    pub start_age: f32,
    /// Project label; `None` until the user opens one.
    pub project_name: Option<String>,
    /// Path typed in the `/project` dialog.
    pub project_input: Option<String>,
    /// Why the last `/project` path was rejected; shown inside the dialog.
    pub project_error: Option<String>,
    /// Long-running engine action, running on a worker thread.
    pub job: Option<Job>,
    /// Polls the project for user edits; AI writes are reindexed before they are seen.
    watcher: Option<watch::Watcher>,
    /// Transcript row receiving streamed assistant text.
    stream_entry: Option<usize>,
    /// Text already shown from the current stream.
    streamed: String,
    /// Full-screen help overlay.
    pub help: bool,
    /// Animated phase of the status-bar pulse.
    pub pulse: f32,
    /// Largest scroll the last frame could offer, published by the renderer.
    pub max_scroll: Cell<u16>,
    /// Clickable rectangles of the last frame.
    pub hits: Hitboxes,
    /// Indexed files.
    pub files: usize,
    /// Indexed symbols.
    pub symbols: usize,
    /// Entries in the persistent cache.
    pub cache_entries: usize,
    /// Tokens of the last prepared context.
    pub last_context_tokens: usize,
    /// Budget a prepared context is allowed to use.
    pub context_budget: usize,
    /// Tokens of the project overview prepared by `/vce` or `/map`.
    pub vce_tokens: usize,
    /// What that overview would cost at the current model price, in RUB.
    pub vce_cost: f64,
    /// Price table of the active model.
    price: ModelPrice,
    /// Context window of the active model.
    window: u64,
    /// Refresh options taken from the configuration.
    refresh: RefreshOptions,
}

impl App {
    /// Opens the engine for `root` and restores the latest session of the project.
    pub fn new(loaded: &LoadedConfig, root: &str) -> anyhow::Result<Self> {
        let config = loaded.config.clone();
        let engine = Engine::open(root, &config.index.database)?;
        let mut vendor = config
            .defaults
            .vendor
            .clone()
            .filter(|id| registry::get(id).is_some())
            .unwrap_or_else(|| registry::default_provider().id.to_owned());
        let store = SessionStore::for_project(engine.root());
        let cwd = display_path(engine.root());
        let default_id = models::default_id(&config);

        let mut session = match store.as_ref().and_then(|store| store.latest()) {
            Some(session) => session,
            None => Session::new(
                cwd,
                default_id.clone().unwrap_or_else(|| "unknown".to_owned()),
                DEFAULT_THEME,
            ),
        };
        // A session saved before the prefix was stripped still holds the verbatim root.
        session.cwd = display_path(Path::new(&session.cwd));

        // A session saved with another provider keeps that provider when its catalog
        // is cached; otherwise the config default wins.
        let mut catalog = None;
        for provider in registry::PROVIDERS {
            if let Ok(Some(cached)) = Catalog::load_cached_for(provider.id)
                && cached.find(&session.model).is_some()
            {
                vendor = provider.id.to_owned();
                catalog = Some(cached);
                break;
            }
        }
        if catalog.is_none() {
            catalog = Catalog::load_cached_for(&vendor).ok().flatten();
        }

        let choice = models::resolve(&config, catalog.as_ref(), &session.model).or_else(|| {
            default_id
                .as_deref()
                .and_then(|id| models::resolve(&config, catalog.as_ref(), id))
        });

        let (model_id, price, window) = match &choice {
            Some(choice) => (
                choice.id.clone(),
                choice.price.clone(),
                choice.context_window,
            ),
            None => (
                session.model.clone(),
                models::unpriced(),
                config
                    .resolve_model(None, None)
                    .map(|resolved| resolved.model.context_window)
                    .unwrap_or(1_050_000),
            ),
        };
        if let Some(choice) = &choice {
            vendor.clone_from(&choice.provider);
        }

        let window = window.max(1_024);
        let context_budget = config.index.map.default_token_budget;

        let _ = keys::unlock_from_env();
        let vault_prompt = if keys::is_unlocked() {
            None
        } else {
            Some(VaultPrompt::new(!keys::vault_exists()))
        };

        let watcher = Some(watch::Watcher::spawn(engine.root()));
        let mut app = Self {
            engine,
            distributor: distributor(&config, window),
            skills: Vec::new(),
            config: config.clone(),
            catalog,
            vendor,
            api_input: None,
            vault_prompt,
            confirm_backup: false,
            confirm_reset: false,
            permissions: Permissions::default(),
            conversation: Vec::new(),
            pending_tools: VecDeque::new(),
            pending_tool: None,
            agent_steps: 0,
            input: Input::default(),
            scroll_back: 0,
            quit: false,
            theme: theme::builtin(DEFAULT_THEME).unwrap_or_else(|| Theme::new(DEFAULT_THEME)),
            cwd: session.cwd.clone(),
            session,
            store,
            picker: None,
            project: None,
            completion: 0,
            job: None,
            watcher,
            stream_entry: None,
            streamed: String::new(),
            start: true,
            start_age: 0.0,
            project_name: None,
            project_input: None,
            project_error: None,
            help: false,
            pulse: 0.0,
            max_scroll: Cell::new(0),
            hits: Hitboxes::default(),
            files: 0,
            symbols: 0,
            cache_entries: 0,
            last_context_tokens: 0,
            context_budget,
            vce_tokens: 0,
            vce_cost: 0.0,
            price,
            window,
            refresh: RefreshOptions {
                max_file_size: config.index.max_file_size,
                respect_gitignore: config.index.respect_gitignore,
                force: false,
            },
        };

        app.session.model.clone_from(&model_id);
        app.input.set_history(app.session.history.clone());
        app.skills = skills::discover(app.engine.root());
        app.load_theme(&app.session.theme.clone());
        app.refresh_counts();
        app.welcome();

        Ok(app)
    }

    /// Advances the animation clock.
    pub fn tick(&mut self) {
        self.pulse += PULSE_STEP;
        if self.pulse > std::f32::consts::TAU {
            self.pulse -= std::f32::consts::TAU;
        }

        // The intro runs once: the age stops growing after the title screen is gone.
        if self.start {
            self.start_age += TICK_SECONDS;
        }

        let deltas = self.job.as_ref().map(Job::try_text).unwrap_or_default();
        for delta in deltas {
            self.append_stream(&delta);
        }

        if let Some(done) = self.job.as_ref().and_then(Job::try_done) {
            if let Some(job) = self.job.take() {
                job.join();
            }
            self.finish_job(done);
        }

        self.poll_watcher();
    }

    /// Adds streamed text to the transcript, splitting it into real lines.
    fn append_stream(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.streamed.push_str(delta);

        let mut parts = delta.split('\n');
        if let Some(first) = parts.next() {
            if let Some(index) = self.stream_entry {
                if let Some(entry) = self.session.entries.get_mut(index) {
                    entry.text.push_str(first);
                } else {
                    self.session.push(EntryKind::Assistant, first.to_owned());
                    self.stream_entry = Some(self.session.entries.len() - 1);
                }
            } else {
                self.session.push(EntryKind::Assistant, first.to_owned());
                self.stream_entry = Some(self.session.entries.len() - 1);
            }
        }
        for part in parts {
            self.session.push(EntryKind::Assistant, part.to_owned());
            self.stream_entry = Some(self.session.entries.len() - 1);
        }
        self.scroll_back = 0;
    }

    /// Applies user edits seen by the filesystem poller to `index1` and `index2`.
    fn poll_watcher(&mut self) {
        let Some(watcher) = &self.watcher else {
            return;
        };
        let paths = watcher.take_events();
        if paths.is_empty() {
            return;
        }
        if let Err(error) = backup::sync(&self.engine, &paths) {
            self.push_error(&format!("watch: {error}"));
        }
    }

    /// Handles the master-password prompt; `false` when the vault is already open.
    fn handle_vault(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        let Some(mut prompt) = self.vault_prompt.take() else {
            return false;
        };

        match key {
            Key::Char(ch) => prompt.text.push(ch),
            Key::Backspace => {
                prompt.text.pop();
            }
            Key::Enter => {
                if prompt.create {
                    self.create_vault_from(prompt);
                } else {
                    self.unlock_vault_from(prompt);
                }
                return true;
            }
            Key::Escape => {
                if prompt.create {
                    self.quit = true;
                } else {
                    self.confirm_reset = true;
                    self.session.push(
                        EntryKind::System,
                        "сброс хранилища удалит все API-ключи · Enter — стереть, Esc — отмена"
                            .to_owned(),
                    );
                    self.vault_prompt = Some(prompt);
                }
                return true;
            }
            Key::Quit => {
                self.quit = true;
                return true;
            }
            _ => {}
        }

        self.vault_prompt = Some(prompt);
        true
    }

    fn create_vault_from(&mut self, mut prompt: VaultPrompt) {
        if prompt.text.chars().count() < 8 {
            prompt.error = Some("пароль короче 8 символов".to_owned());
            self.vault_prompt = Some(prompt);
            return;
        }

        if prompt.first.is_none() {
            prompt.first = Some(Zeroizing::new(prompt.text.as_str().to_owned()));
            prompt.text.clear();
            prompt.error = None;
            self.vault_prompt = Some(prompt);
            return;
        }

        if prompt.first.as_ref().map(|first| first.as_str()) != Some(prompt.text.as_str()) {
            prompt.first = None;
            prompt.text.clear();
            prompt.error = Some("пароли не совпали, начни заново".to_owned());
            self.vault_prompt = Some(prompt);
            return;
        }

        match keys::create_vault(prompt.text.as_str()) {
            Ok(()) => {
                self.session
                    .push(EntryKind::System, "хранилище ключей создано".to_owned());
            }
            Err(error) => {
                prompt.error = Some(error.to_string());
                self.vault_prompt = Some(prompt);
            }
        }
    }

    fn unlock_vault_from(&mut self, mut prompt: VaultPrompt) {
        match keys::unlock(prompt.text.as_str()) {
            Ok(()) => {}
            Err(error) => {
                prompt.error = Some(error.to_string());
                prompt.text.clear();
                self.vault_prompt = Some(prompt);
            }
        }
    }

    /// Confirms a `/back` restore; `false` when no restore is pending.
    fn handle_confirm_backup(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        if !self.confirm_backup {
            return false;
        }

        match key {
            Key::Enter | Key::Char('y') | Key::Char('Y') => {
                self.confirm_backup = false;
                self.session.push(
                    EntryKind::System,
                    "back: восстанавливаю index2 в фоне".to_owned(),
                );
                self.job = Some(job::restore(
                    self.engine.root().to_path_buf(),
                    self.config.index.database.clone(),
                ));
            }
            Key::Escape | Key::Char('n') | Key::Char('N') => {
                self.confirm_backup = false;
            }
            Key::Quit => self.quit = true,
            _ => {}
        }

        true
    }

    /// Confirms a vault reset; `false` when no reset is pending.
    fn handle_confirm_reset(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        if !self.confirm_reset {
            return false;
        }

        match key {
            Key::Enter | Key::Char('y') | Key::Char('Y') => {
                self.confirm_reset = false;
                match keys::reset_vault() {
                    Ok(()) => {
                        self.reset_agent();
                        self.vault_prompt = Some(VaultPrompt::new(true));
                        self.session.push(
                            EntryKind::System,
                            "хранилище сброшено, API-ключи удалены".to_owned(),
                        );
                    }
                    Err(error) => {
                        self.push_error(&format!("сброс не удался: {error}"));
                        self.vault_prompt = None;
                    }
                }
            }
            Key::Escape | Key::Char('n') | Key::Char('N') => {
                self.confirm_reset = false;
            }
            Key::Quit => self.quit = true,
            _ => {}
        }

        true
    }

    /// Processes one key press.
    pub fn handle(&mut self, key: crate::tui::keybindings::Key) {
        use crate::tui::keybindings::Key;

        if self.handle_vault(key) {
            return;
        }
        if self.handle_confirm_backup(key) {
            return;
        }

        if self.handle_confirm_reset(key) {
            return;
        }

        // The title screen owns the keyboard until the chat starts.
        if self.start {
            match key {
                Key::Enter | Key::Char(_) | Key::Escape | Key::Tab => self.start = false,
                Key::Quit => self.quit = true,
                _ => {}
            }
            return;
        }

        // Help is modal: it must not leak characters into the input behind it.
        if self.help {
            match key {
                Key::Quit => self.quit = true,
                Key::Escape | Key::Enter => self.help = false,
                _ => {}
            }
            return;
        }

        if self.handle_pending_tool(key) {
            return;
        }

        if self.handle_api_input(key) {
            return;
        }

        if self.handle_project_input(key) {
            return;
        }

        if self.handle_project(key) {
            return;
        }

        if self.handle_picker(key) {
            return;
        }

        // The command menu owns the arrows while it is open.
        // The command menu owns the arrows while the typed line is a prefix of a command
        // and not yet an exact one; a full command keeps the usual history navigation.
        if self.completes_command() && matches!(key, Key::Up | Key::Down) {
            let len = self.completions().len();
            self.completion = if matches!(key, Key::Up) {
                (self.completion + len - 1) % len
            } else {
                (self.completion + 1) % len
            };
            return;
        }

        match key {
            Key::Quit => self.quit = true,
            Key::Clear => {
                self.session.entries.clear();
                self.scroll_back = 0;
                self.welcome();
            }
            Key::Char(ch) => {
                self.input.insert(ch);
                self.completion = 0;
            }
            Key::Newline => self.input.insert('\n'),
            Key::Backspace => {
                self.input.backspace();
                self.completion = 0;
            }
            Key::Delete => self.input.delete(),
            Key::Left => self.input.left(),
            Key::Right => self.input.right(),
            Key::WordLeft => self.input.word_left(),
            Key::WordRight => self.input.word_right(),
            Key::Home => self.input.home(),
            Key::End => self.input.end(),
            Key::KillWord => self.input.kill_word(),
            Key::KillLine => self.input.clear(),
            Key::Enter => {
                if self.completes_command() {
                    self.complete_at(self.completion);
                } else {
                    self.submit();
                }
            }
            Key::Escape => self.input.clear(),
            Key::Tab => self.complete_at(self.completion),
            // History wins over scrolling: the transcript also moves with PgUp/PgDn,
            // Ctrl+arrows, and the wheel, while history has no other way in.
            Key::Up => {
                if !self.input.previous() {
                    self.scroll_up(1);
                }
            }
            Key::Down => {
                if !self.input.newer() {
                    self.scroll_down(1);
                }
            }
            Key::ScrollUp => self.scroll_up(1),
            Key::ScrollDown => self.scroll_down(1),
            Key::PageUp => self.scroll_up(10),
            Key::PageDown => self.scroll_down(10),
        }

        self.clamp_scroll();
    }

    /// Handles one mouse event: wheel scrolls, clicks hit overlays and the input.
    pub fn mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollUp => self.wheel(1),
            MouseEventKind::ScrollDown => self.wheel(-1),
            MouseEventKind::Down(MouseButton::Left) => self.click(event.column, event.row),
            _ => {}
        }
    }

    /// Wheel: the picker selection when it is open, the transcript otherwise.
    fn wheel(&mut self, direction: i32) {
        const STEPS: u16 = 3;

        if let Some(picker) = self.picker.as_mut() {
            // One notch is one model: a list is short, three rows per notch overshoots.
            if direction > 0 {
                picker.previous();
            } else {
                picker.next();
            }
            return;
        }
        if self.help {
            return;
        }

        if direction > 0 {
            self.scroll_up(STEPS);
        } else {
            self.scroll_down(STEPS);
        }
        self.clamp_scroll();
    }

    /// Left click: close help, choose a model, or complete a command.
    fn click(&mut self, x: u16, y: u16) {
        let hits = self.hits;

        if self.help {
            self.help = false;
            return;
        }

        if let Some(hit) = hits.picker {
            if screens::contains(hit.area, x, y) {
                if screens::contains(hit.inner, x, y) {
                    let index = hit.offset + (y - hit.inner.y) as usize;
                    self.pick_index(index);
                }
            } else {
                // A click outside the popup dismisses it, as in a menu.
                self.picker = None;
            }
            return;
        }

        if let Some(hit) = hits.completions
            && screens::contains(hit.inner, x, y)
        {
            self.complete_at(hit.offset + (y - hit.inner.y) as usize);
        }
    }

    /// Selects and applies one picker row.
    fn pick_index(&mut self, index: usize) {
        let Some(mut picker) = self.picker.take() else {
            return;
        };

        if index >= picker.visible().len() {
            self.picker = Some(picker);
            return;
        }

        picker.select(index);
        let kind = picker.kind();
        match picker.selected().map(|row| row.id.clone()) {
            Some(id) => match kind {
                PickerKind::Model => self.apply_model(&id),
                PickerKind::Theme => self.switch_theme(&id),
                PickerKind::Session => self.switch_session(&id),
                PickerKind::Provider => self.switch_provider(&id),
            },
            None => self.picker = Some(picker),
        }
    }

    /// Records the clickable rectangles of the last frame.
    pub fn set_hits(&mut self, hits: Hitboxes) {
        self.hits = hits;
    }

    /// Inserts a pasted block into the input at the caret.
    pub fn paste(&mut self, text: &str) {
        if let Some(input) = &mut self.api_input {
            input
                .text
                .extend(text.chars().filter(|ch| !ch.is_whitespace()));
            return;
        }

        if self.help || self.picker.is_some() {
            return;
        }

        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => self.input.insert(' '),
                _ => self.input.insert(ch),
            }
        }
    }

    fn scroll_up(&mut self, lines: u16) {
        self.scroll_back = self.scroll_back.saturating_add(lines);
    }

    fn scroll_down(&mut self, lines: u16) {
        self.scroll_back = self.scroll_back.saturating_sub(lines);
    }

    /// Keeps the scroll offset inside what the last frame could actually show.
    fn clamp_scroll(&mut self) {
        self.scroll_back = self.scroll_back.min(self.max_scroll.get());
    }

    /// Handles keys while the `/project` path dialog is open.
    fn handle_project_input(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        let Some(mut text) = self.project_input.take() else {
            return false;
        };

        let mut keep = true;

        match key {
            Key::Char(ch) => {
                text.push(ch);
                self.project_error = None;
            }
            Key::Backspace => {
                text.pop();
                self.project_error = None;
            }
            Key::Enter => {
                if text.trim().is_empty() {
                    self.project_error = Some("введите путь к каталогу проекта".to_owned());
                } else {
                    match self.open_project_text(&text) {
                        Ok(()) => return true,
                        Err(error) => self.project_error = Some(error),
                    }
                }
            }
            Key::Escape => {
                keep = false;
                self.project_error = None;
            }
            Key::Quit => {
                self.quit = true;
                keep = false;
            }
            _ => {}
        }

        if keep {
            self.project_input = Some(text);
        }

        true
    }

    /// Handles keys while the project picker is open; `false` when it is closed.
    fn handle_project(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        let Some(mut picker) = self.project.take() else {
            return false;
        };

        let mut keep = true;
        let mut open = None;

        match key {
            Key::Up => picker.previous(),
            Key::Down => picker.next(),
            Key::Right | Key::Tab => picker.descend(),
            Key::Left | Key::Backspace => picker.go_up(),
            Key::Enter => match picker.target() {
                Some(path) => {
                    open = Some(path);
                    keep = false;
                }
                None => picker.go_up(),
            },
            Key::Escape => keep = false,
            Key::Quit => {
                self.quit = true;
                keep = false;
            }
            _ => {}
        }

        if keep {
            self.project = Some(picker);
        }
        if let Some(path) = open {
            self.open_project(&path);
        }

        true
    }

    /// Handles keys while the model picker is open; `false` when it is closed.
    fn handle_picker(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        let Some(mut picker) = self.picker.take() else {
            return false;
        };

        let mut keep = true;
        let mut chosen = None;
        let kind = picker.kind();

        match key {
            Key::Char(ch) => picker.push(ch),
            Key::Backspace => picker.backspace(),
            Key::Up => picker.previous(),
            Key::Down => picker.next(),
            Key::PageUp => picker.previous(),
            Key::PageDown => picker.next(),
            Key::Enter => {
                chosen = picker.selected().map(|row| row.id.clone());
                keep = false;
            }
            Key::Escape => keep = false,
            Key::Quit => {
                self.quit = true;
                keep = false;
            }
            _ => {}
        }

        if keep {
            self.picker = Some(picker);
        }
        if let Some(id) = chosen {
            match kind {
                PickerKind::Model => self.apply_model(&id),
                PickerKind::Theme => self.switch_theme(&id),
                PickerKind::Session => self.switch_session(&id),
                PickerKind::Provider => self.switch_provider(&id),
            }
        }

        true
    }

    /// Handles Enter: runs a command or a context request.
    pub fn submit(&mut self) {
        let line = self.input.take();
        // A multi-line prompt is one request: the engine and the transcript both work with
        // a single line, so the stored line breaks become spaces.
        let line = collapse(&line);
        if line.is_empty() {
            return;
        }

        self.input.remember(&line);
        self.scroll_back = 0;
        self.session.push(EntryKind::User, line.clone());

        match commands::parse(&line) {
            Action::NotACommand => self.send_chat(&line),
            action => self.run(action),
        }

        self.persist();
    }

    /// Executes one parsed action.
    pub fn run(&mut self, action: Action) {
        match action {
            Action::NotACommand => {}
            Action::Help => self.help = true,
            Action::Clear => {
                self.session.entries.clear();
                self.scroll_back = 0;
                self.reset_agent();
                self.welcome();
            }
            Action::Quit => self.quit = true,
            Action::Index { force } => self.run_index(force),
            Action::Map { tokens } => self.run_map(tokens),
            Action::Vce { tokens } => self.run_vce(tokens),
            Action::Context {
                input,
                tokens,
                compression,
                strategy,
            } => self.run_context(&input, tokens, compression.as_deref(), strategy.as_deref()),
            Action::Search { query } => self.run_search(&query),
            Action::Outline { file } => self.run_outline(&file),
            Action::Cat { file, from, to } => self.run_cat(&file, from, to),
            Action::Deps { file, depth } => self.run_deps(&file, depth),
            Action::Cache => self.run_cache(),
            Action::CacheClear => self.run_cache_clear(),
            Action::Skills => self.run_skills(),
            Action::Skill { name } => self.run_skill(&name),
            Action::Models { filter } => {
                // `/models` is a picker, not a wall of text; a filter narrows the same popup.
                self.open_picker();

                if let Some(filter) = filter
                    && let Some(mut picker) = self.picker.take()
                {
                    for ch in filter.chars() {
                        picker.push(ch);
                    }
                    self.picker = Some(picker);
                }
            }
            Action::Install => self.install_models(),
            Action::Model { name } => match name {
                Some(name) => self.apply_model(&name),
                None => self.open_picker(),
            },
            Action::Providers { name } => match name {
                Some(id) => self.switch_provider(&id),
                None => self.open_providers(),
            },
            Action::Api { provider } => match provider {
                Some(id) => self.change_key(&id),
                None => self.show_api(),
            },
            Action::BackupRestore => {
                self.confirm_backup = true;
                self.session.push(
                    EntryKind::System,
                    "восстановить файлы из index2? Enter — да, Esc — отмена".to_owned(),
                );
            }
            Action::VaultReset => {
                self.confirm_reset = true;
                self.session.push(
                    EntryKind::System,
                    "сброс хранилища удалит все API-ключи · Enter — стереть, Esc — отмена"
                        .to_owned(),
                );
            }
            Action::Theme { name } => match name {
                Some(name) => self.switch_theme(&name),
                None => self.show_themes(),
            },
            Action::NewSession => self.new_session(),
            Action::Sessions => self.show_sessions(),
            Action::SwitchSession { id } => self.switch_session(&id),
            Action::Project { path } => match path {
                Some(path) => {
                    if let Err(error) = self.open_project_text(&path) {
                        self.push_error(&format!("проект: {error}"));
                    }
                }
                None => {
                    self.project_input = Some(String::new());
                    self.project_error = None;
                }
            },
            Action::Projects => {
                self.project = Some(ProjectPicker::new(self.engine.root().to_path_buf()));
            }
            Action::Cwd => {
                let cwd = self.cwd.clone();
                self.session
                    .push(EntryKind::System, format!("каталог: {cwd}"));
            }
            Action::Cost => self.show_cost(),
            Action::Unknown { name } => {
                self.push_error(&format!("неизвестная команда `/{name}`; /help"))
            }
        }
    }

    /// Handles keys while the API key prompt is open; `false` when it is closed.
    fn handle_api_input(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        let Some(mut input) = self.api_input.take() else {
            return false;
        };

        match key {
            Key::Char(ch) => input.text.push(ch),
            Key::Backspace => {
                input.text.pop();
            }
            Key::Enter => {
                self.submit_api_key(input);
                return true;
            }
            Key::Escape => {
                self.session
                    .push(EntryKind::System, "ввод ключа отменён".to_owned());
                return true;
            }
            Key::Quit => {
                self.quit = true;
                return true;
            }
            _ => {}
        }

        self.api_input = Some(input);
        true
    }

    /// Stores the key, switches to its provider, and asks for the catalog.
    fn submit_api_key(&mut self, input: ApiKeyInput) {
        let provider = input.provider.clone();
        let Some(info) = registry::get(&provider) else {
            self.push_error(&format!("неизвестный провайдер `{provider}`"));
            return;
        };

        match keys::store(&provider, input.text.as_str()) {
            Ok(origin) => {
                let masked = match keys::status(&provider) {
                    KeyStatus::Present { masked, .. } => masked,
                    _ => "…".to_owned(),
                };
                self.vendor.clone_from(&provider);
                self.catalog = Catalog::load_cached_for(&provider).ok().flatten();
                self.session.push(
                    EntryKind::System,
                    format!(
                        "ключ {masked} сохранён ({}) · {}",
                        origin.label(),
                        info.title
                    ),
                );
                self.refresh_catalog(provider);
            }
            Err(err) => {
                self.push_error(&format!("ключ не сохранён: {err}"));
                self.api_input = Some(input);
            }
        }
    }

    /// Opens the aggregator picker.
    pub fn open_providers(&mut self) {
        let current = self.vendor.clone();
        let rows: Vec<Row> = registry::PROVIDERS
            .iter()
            .map(|provider| {
                let models = Catalog::load_cached_for(provider.id)
                    .ok()
                    .flatten()
                    .map_or(0, |catalog| catalog.models.len());
                let models = if models == 0 {
                    "модели не загружены".to_owned()
                } else {
                    format!("{models} моделей")
                };

                Row {
                    id: provider.id.to_owned(),
                    label: provider.title.to_owned(),
                    detail: format!("{} · {models}", Self::key_detail(provider.id)),
                    current: provider.id == current,
                }
            })
            .collect();

        self.picker = Some(ModelPicker::list(
            PickerKind::Provider,
            "провайдеры",
            rows,
            &current,
        ));
    }

    /// Switches the active provider, asking for a key when none is stored.
    fn switch_provider(&mut self, id: &str) {
        let Some(provider) = registry::get(id) else {
            self.push_error(&format!("неизвестный провайдер `{id}`: /provider"));
            return;
        };

        self.vendor = provider.id.to_owned();
        self.catalog = Catalog::load_cached_for(provider.id).ok().flatten();

        match keys::status(provider.id) {
            KeyStatus::Missing => {
                self.session.push(
                    EntryKind::System,
                    format!("провайдер: {} · вставь API-ключ", provider.title),
                );
                self.api_input = Some(ApiKeyInput {
                    provider: provider.id.to_owned(),
                    text: Zeroizing::new(String::new()),
                });
            }
            KeyStatus::Present { masked, origin } => {
                self.session.push(
                    EntryKind::System,
                    format!(
                        "провайдер: {} · ключ {masked} ({})",
                        provider.title,
                        origin.label()
                    ),
                );
                self.refresh_catalog(provider.id.to_owned());
            }
            KeyStatus::Error(err) => {
                self.push_error(&format!("ключ {}: {err}", provider.title));
                self.api_input = Some(ApiKeyInput {
                    provider: provider.id.to_owned(),
                    text: Zeroizing::new(String::new()),
                });
            }
        }
    }

    /// Opens a fresh key prompt for an existing provider.
    fn change_key(&mut self, id: &str) {
        let Some(provider) = registry::get(id) else {
            self.push_error(&format!("неизвестный провайдер `{id}`: /provider"));
            return;
        };

        self.vendor = provider.id.to_owned();
        self.catalog = Catalog::load_cached_for(provider.id).ok().flatten();
        self.session.push(
            EntryKind::System,
            format!("новый ключ {}: вставь и нажми Enter", provider.title),
        );
        self.api_input = Some(ApiKeyInput {
            provider: provider.id.to_owned(),
            text: Zeroizing::new(String::new()),
        });
    }

    /// Lists every provider with its masked key and cached model count.
    fn show_api(&mut self) {
        self.session
            .push(EntryKind::System, "провайдеры и ключи:".to_owned());

        for provider in registry::PROVIDERS {
            let models = Catalog::load_cached_for(provider.id)
                .ok()
                .flatten()
                .map_or(0, |catalog| catalog.models.len());
            let models = if models == 0 {
                "модели не загружены".to_owned()
            } else {
                format!("{models} моделей")
            };
            let active = if provider.id == self.vendor { " *" } else { "" };

            self.session.push(
                EntryKind::Tool,
                format!(
                    "  {:<10}{active}  {} · {models}",
                    provider.title,
                    Self::key_detail(provider.id)
                ),
            );
        }

        self.session.push(
            EntryKind::System,
            "сменить ключ: /api <provider> · выбрать: /provider".to_owned(),
        );
    }

    /// Fetches one provider's catalog in the background.
    fn refresh_catalog(&mut self, provider: String) {
        if self.job.is_some() {
            self.push_error("дождись текущей задачи");
            return;
        }
        self.job = Some(job::catalog(provider));
    }

    /// `/install`: downloads the catalog of every provider in the background.
    fn install_models(&mut self) {
        if self.job.is_some() {
            self.push_error("дождись текущей задачи");
            return;
        }
        self.job = Some(job::install());
        self.session.push(
            EntryKind::System,
            "обновляю каталоги всех провайдеров…".to_owned(),
        );
    }

    fn key_detail(provider: &str) -> String {
        match keys::status(provider) {
            KeyStatus::Missing => "ключ не задан".to_owned(),
            KeyStatus::Present { masked, origin } => {
                format!("ключ {masked} ({})", origin.label())
            }
            KeyStatus::Error(err) => format!("ключ: ошибка ({err})"),
        }
    }

    /// Forgets the model conversation and the write permission of the old session.
    fn reset_agent(&mut self) {
        self.conversation.clear();
        self.pending_tools.clear();
        self.pending_tool = None;
        self.agent_steps = 0;
        self.stream_entry = None;
        self.streamed.clear();
        self.permissions = Permissions::default();
        self.vce_tokens = 0;
        self.vce_cost = 0.0;
        self.last_context_tokens = 0;
    }

    /// Sends a plain prompt to the active provider.
    fn send_chat(&mut self, line: &str) {
        if self.job.is_some() {
            self.push_error("дождись текущей задачи");
            return;
        }

        let provider = self.vendor.clone();
        match keys::status(&provider) {
            KeyStatus::Missing => {
                self.push_error(&format!("ключ не задан: /api {provider} — вставь API-ключ"));
                return;
            }
            KeyStatus::Error(err) => {
                self.push_error(&format!("ключ {provider}: {err}"));
                return;
            }
            KeyStatus::Present { .. } => {}
        }

        if self.conversation.is_empty() {
            let parts = crate::core::prompt::PromptParts::new("", true);
            if let Some(system) = parts.system_message() {
                self.conversation.push(ChatMessage::system(system));
            }
        }
        self.conversation.push(ChatMessage::user(line));
        self.agent_steps = 0;
        self.pending_tools.clear();
        self.stream_entry = None;
        self.streamed.clear();
        self.request_agent();
    }

    /// True when the selected model advertises function tools.
    fn model_supports_tools(&self) -> bool {
        models::resolve(&self.config, self.catalog.as_ref(), &self.session.model)
            .map(|choice| choice.tools)
            .unwrap_or(true)
    }

    /// Sends the current conversation to the provider with the tool schemas.
    fn request_agent(&mut self) {
        if self.job.is_some() {
            return;
        }

        let provider = self.vendor.clone();
        let tools = if self.model_supports_tools() {
            crate::vce::tools::registry::Registry::chat().schemas()
        } else {
            Vec::new()
        };
        let reasoning = self
            .config
            .resolve_model(Some(self.vendor.as_str()), Some(&self.session.model))
            .map(|resolved| resolved.model.reasoning)
            .unwrap_or(Reasoning::Off);

        self.job = Some(job::chat(
            provider,
            self.session.model.clone(),
            self.conversation.clone(),
            reasoning,
            tools,
        ));
    }

    /// Executes the next tool call; every write waits for confirmation.
    fn advance_agent(&mut self) {
        if self.agent_steps >= MAX_AGENT_STEPS {
            self.push_error("слишком много шагов инструментов; остановился");
            self.pending_tools.clear();
            return;
        }

        let Some(call) = self.pending_tools.pop_front() else {
            self.request_agent();
            return;
        };
        self.agent_steps += 1;

        let arguments = serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
        let action = Dispatcher::new(self.permissions)
            .registry()
            .get(&call.name)
            .map(|tool| tool.action());

        match action {
            None => {
                let message = format!("error: unknown tool `{}`", call.name);
                self.finish_tool(call, message);
            }
            Some(ToolAction::Write | ToolAction::Execute) => {
                let target = if action == Some(ToolAction::Execute) {
                    arguments
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?")
                        .to_owned()
                } else {
                    let path = arguments
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    format!("`{path}`")
                };
                let verb = if action == Some(ToolAction::Execute) {
                    "выполнить"
                } else {
                    match call.name.as_str() {
                        "append_file" => "добавить в",
                        "write_file" => "записать",
                        _ => "изменить",
                    }
                };
                self.session.push(
                    EntryKind::System,
                    format!("ИИ хочет {verb} {target} · Enter — разрешить, Esc — отклонить"),
                );
                self.pending_tool = Some(PendingTool { call, arguments });
            }
            Some(_) => self.execute_tool(call, arguments),
        }
    }

    /// Runs one tool through the permission-checked dispatcher.
    fn execute_tool(&mut self, call: ToolCall, arguments: serde_json::Value) {
        self.run_tool(call, arguments, self.permissions);
    }

    /// Runs a write that the user just approved, without widening later calls.
    fn execute_confirmed(&mut self, call: ToolCall, arguments: serde_json::Value) {
        let permissions = Permissions {
            allow_write: true,
            allow_execute: true,
            ..self.permissions
        };
        self.run_tool(call, arguments, permissions);
    }

    fn run_tool(&mut self, call: ToolCall, arguments: serde_json::Value, permissions: Permissions) {
        let dispatcher = Dispatcher::new(permissions);
        let text = match dispatcher.call(&self.engine, &call.name, &arguments) {
            Ok(result) => result.text,
            Err(error) => format!("error: {error}"),
        };
        self.finish_tool(call, text);
    }

    /// Appends a tool result to the conversation and continues the agent loop.
    fn finish_tool(&mut self, call: ToolCall, text: String) {
        let preview: String = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect();
        self.session
            .push(EntryKind::Tool, format!("{}: {preview}", call.name));
        self.conversation.push(ChatMessage::tool(call.id, text));
        self.persist();
        self.advance_agent();
    }

    /// Confirms or denies a pending write; `false` when no prompt is open.
    fn handle_pending_tool(&mut self, key: crate::tui::keybindings::Key) -> bool {
        use crate::tui::keybindings::Key;

        let Some(pending) = self.pending_tool.take() else {
            return false;
        };

        match key {
            Key::Enter | Key::Char('y') | Key::Char('Y') => {
                self.execute_confirmed(pending.call, pending.arguments);
            }
            Key::Escape | Key::Char('n') | Key::Char('N') => {
                self.finish_tool(pending.call, "user denied the file modification".to_owned());
            }
            Key::Quit => {
                self.pending_tool = Some(pending);
                self.quit = true;
            }
            _ => self.pending_tool = Some(pending),
        }

        true
    }

    /// Opens the model picker over the catalog and the config.
    pub fn open_picker(&mut self) {
        let choices = models::choices(&self.config, self.catalog.as_ref());
        if choices.is_empty() {
            self.push_error(
                "список моделей пуст: `venera models --refresh` или models в config.json",
            );
            return;
        }

        self.picker = Some(ModelPicker::new(&choices, &self.session.model));
    }

    fn apply_model(&mut self, name: &str) {
        let Some(choice) = models::resolve(&self.config, self.catalog.as_ref(), name) else {
            self.push_error(&format!("модель `{name}` не найдена: /models"));
            return;
        };

        self.session.model.clone_from(&choice.id);
        if choice.provider != self.vendor {
            self.vendor.clone_from(&choice.provider);
            self.catalog = Catalog::load_cached_for(&self.vendor).ok().flatten();
        }
        self.price = choice.price.clone();
        self.window = choice.context_window.max(1_024);
        self.distributor = distributor(&self.config, self.window);

        let context = if choice.context_window == 0 {
            "ctx неизвестен".to_owned()
        } else {
            format!("ctx {}", format_tokens(choice.context_window))
        };
        let source = if choice.curated {
            "config"
        } else {
            "каталог"
        };
        self.session.push(
            EntryKind::System,
            format!(
                "модель: {} · {} · {context} · {source} · {}",
                choice.label,
                choice.price_label(),
                if choice.price.rates.cache_read.is_some() {
                    "кэш учтён"
                } else {
                    "кэш по цене входа"
                }
            ),
        );
        self.persist();
    }

    fn charge_input(&mut self, tokens: u64) {
        let usage = Usage {
            input: tokens,
            ..Usage::default()
        };
        self.session.charge(&usage, &self.price);
    }

    /// Remembers the project overview size and its price, without a model turn.
    fn remember_vce(&mut self, tokens: usize) {
        let usage = Usage {
            input: tokens as u64,
            ..Usage::default()
        };
        self.vce_tokens = tokens;
        self.vce_cost = turn_cost(&self.price, &usage).total_rub();
    }

    fn show_themes(&mut self) {
        let names = theme::available(Some(self.engine.root()));
        let current = self.theme.name().to_owned();
        let rows: Vec<Row> = names
            .iter()
            .map(|name| Row {
                id: name.clone(),
                label: name.clone(),
                detail: if theme::builtin(name).is_some() {
                    "встроенная".to_owned()
                } else {
                    "файл".to_owned()
                },
                current: name == &current,
            })
            .collect();

        if rows.is_empty() {
            self.push_error("темы не найдены");
            return;
        }

        self.picker = Some(ModelPicker::list(PickerKind::Theme, "темы", rows, &current));
    }

    fn load_theme(&mut self, name: &str) -> bool {
        match theme::load(name, Some(self.engine.root())) {
            Ok(theme) => {
                self.session.theme = theme.name().to_owned();
                self.theme = theme;
                true
            }
            Err(error) => {
                self.push_error(&format!("тема: {error}"));
                self.theme =
                    theme::builtin(DEFAULT_THEME).unwrap_or_else(|| Theme::new(DEFAULT_THEME));
                self.session.theme = DEFAULT_THEME.to_owned();
                false
            }
        }
    }

    fn switch_theme(&mut self, name: &str) {
        if self.load_theme(name) {
            let active = self.session.theme.clone();
            self.session
                .push(EntryKind::System, format!("тема: {active}"));
            self.persist();
        }
    }

    fn new_session(&mut self) {
        self.persist();

        let cwd = self.cwd.clone();
        let model = self.session.model.clone();
        let theme = self.session.theme.clone();
        self.session = Session::new(cwd, model, theme);
        self.reset_agent();
        self.session.history = self.input.history().to_vec();
        self.scroll_back = 0;
        self.session.push(
            EntryKind::System,
            format!("новая сессия · {}", self.session.id),
        );
        self.persist();
    }

    fn show_sessions(&mut self) {
        let Some(store) = &self.store else {
            self.push_error("каталог сессий недоступен");
            return;
        };

        let sessions = store.list();
        if sessions.is_empty() {
            self.session.push(EntryKind::System, "сессий пока нет");
            return;
        }

        let current = self.session.id.clone();
        let rows: Vec<Row> = sessions
            .iter()
            .take(50)
            .map(|session| {
                let summary = session.summary();
                let label: String = summary.chars().take(48).collect();
                Row {
                    id: session.id.clone(),
                    label,
                    detail: session.id.clone(),
                    current: session.id == current,
                }
            })
            .collect();

        self.picker = Some(ModelPicker::list(
            PickerKind::Session,
            "сессии",
            rows,
            &current,
        ));
    }

    fn switch_session(&mut self, id: &str) {
        if id == self.session.id {
            self.session
                .push(EntryKind::System, "эта сессия уже открыта");
            return;
        }

        let Some(store) = &self.store else {
            self.push_error("каталог сессий недоступен");
            return;
        };

        let Some(session) = store.load(id) else {
            self.push_error(&format!("сессия `{id}` не найдена: /sessions"));
            return;
        };

        self.persist();
        self.session = session;
        self.reset_agent();
        self.input.set_history(self.session.history.clone());
        self.cwd = self.session.cwd.clone();
        self.scroll_back = 0;

        if let Some(choice) =
            models::resolve(&self.config, self.catalog.as_ref(), &self.session.model)
        {
            self.session.model.clone_from(&choice.id);
            self.price = choice.price.clone();
            self.window = choice.context_window.max(1_024);
            self.distributor = distributor(&self.config, self.window);
        }

        let theme_name = self.session.theme.clone();
        self.load_theme(&theme_name);
        self.session.push(
            EntryKind::System,
            format!("сессия {} открыта", self.session.id),
        );
        self.persist();
    }

    /// Switches the UI to another project root.
    pub fn open_project(&mut self, path: &Path) -> bool {
        self.persist();

        let engine = match Engine::open(path, &self.config.index.database) {
            Ok(engine) => engine,
            Err(error) => {
                self.push_error(&format!("проект: {error}"));
                return false;
            }
        };

        let store = SessionStore::for_project(engine.root());
        let cwd = display_path(engine.root());
        let mut session = match store.as_ref().and_then(|store| store.latest()) {
            Some(session) => session,
            None => Session::new(
                cwd.clone(),
                self.session.model.clone(),
                self.session.theme.clone(),
            ),
        };
        session.cwd = display_path(Path::new(&session.cwd));

        self.engine = engine;
        self.watcher = Some(watch::Watcher::spawn(self.engine.root()));
        self.store = store;
        self.project_name = Some(cwd.clone());
        self.cwd = cwd;
        self.session = session;
        self.project = None;
        self.reset_agent();
        self.scroll_back = 0;
        self.input.set_history(self.session.history.clone());

        if let Some(choice) =
            models::resolve(&self.config, self.catalog.as_ref(), &self.session.model)
        {
            self.session.model.clone_from(&choice.id);
            self.price = choice.price.clone();
            self.window = choice.context_window.max(1_024);
            self.distributor = distributor(&self.config, self.window);
        }

        let theme = self.session.theme.clone();
        self.load_theme(&theme);
        self.skills = skills::discover(self.engine.root());
        self.refresh_counts();
        self.welcome();
        self.persist();
        true
    }

    /// Resolves a typed project path and switches to it; the error is user-facing.
    fn open_project_text(&mut self, text: &str) -> Result<(), String> {
        let path = resolve_project_path(text, self.engine.root())?;

        if self.open_project(&path) {
            self.project_error = None;
            Ok(())
        } else {
            Err(format!(
                "не удалось открыть {}",
                crate::paths::display(&path)
            ))
        }
    }

    fn show_cost(&mut self) {
        let spend = self.session.spend;
        let price = &self.price.rates;

        self.session.push(
            EntryKind::System,
            format!("сессия {} · {} ходов", self.session.id, spend.turns),
        );
        self.session.push(
            EntryKind::Tool,
            format!(
                "  вход {} tok · кэш {} tok · выход {} tok",
                format_tokens(spend.input_tokens),
                format_tokens(spend.cache_read_tokens),
                format_tokens(spend.output_tokens)
            ),
        );
        self.session.push(
            EntryKind::Tool,
            format!(
                "  ставки {} / {} / кэш {} руб. за 1M · итого {}",
                price.input,
                price.output,
                price.cache_read_or_input(),
                format_rub(spend.rub)
            ),
        );
        self.session.push(
            EntryKind::System,
            "считается подготовленный контекст; ответы модели появятся с провайдером".to_owned(),
        );
    }

    fn run_index(&mut self, force: bool) {
        if self.job.is_some() {
            self.push_error("уже выполняется фоновая задача: дождитесь её окончания");
            return;
        }

        let options = RefreshOptions {
            force,
            ..self.refresh
        };
        self.session
            .push(EntryKind::System, "index: запущено в фоне".to_owned());
        self.job = Some(job::index(
            self.engine.root().to_path_buf(),
            self.config.index.database.clone(),
            options,
        ));
    }

    fn run_map(&mut self, tokens: Option<usize>) {
        if self.job.is_some() {
            self.push_error("уже выполняется фоновая задача: дождитесь её окончания");
            return;
        }

        let options = MapOptions {
            max_tokens: tokens.unwrap_or(self.context_budget),
            ..MapOptions::default()
        };
        self.session
            .push(EntryKind::System, "map: запущено в фоне".to_owned());
        self.job = Some(job::map(
            self.engine.root().to_path_buf(),
            self.config.index.database.clone(),
            options,
        ));
    }

    /// `/vce`: refresh the index and render the map in one background job.
    fn run_vce(&mut self, tokens: Option<usize>) {
        if self.job.is_some() {
            self.push_error("уже выполняется фоновая задача: дождитесь её окончания");
            return;
        }

        let refresh = RefreshOptions {
            force: false,
            ..self.refresh
        };
        let map = MapOptions {
            max_tokens: tokens.unwrap_or(self.context_budget),
            ..MapOptions::default()
        };

        self.session.push(
            EntryKind::System,
            "vce: индекс и карта запущены в фоне".to_owned(),
        );
        self.job = Some(job::vce(
            self.engine.root().to_path_buf(),
            self.config.index.database.clone(),
            refresh,
            map,
        ));
    }

    /// Applies a finished background job on the UI thread.
    fn finish_job(&mut self, done: JobDone) {
        match done {
            JobDone::Index(report) => {
                self.session.push(
                    EntryKind::Tool,
                    format!(
                        "index: {} files, {} symbols, {} imports · added {}, modified {}, deleted {}, parsed {}, unchanged {}",
                        report.files,
                        report.symbols,
                        report.imports,
                        report.added,
                        report.modified,
                        report.deleted,
                        report.reparsed,
                        report.unchanged
                    ),
                );
                self.refresh_counts();
            }
            JobDone::Map(map) => {
                self.last_context_tokens = map.tokens;
                self.remember_vce(map.tokens);
                self.push_text(&map.text, CONTEXT_PREVIEW_LINES);
                self.session.push(
                    EntryKind::System,
                    format!(
                        "map: {} tokens, {} symbols in {} files",
                        map.tokens, map.symbols, map.files
                    ),
                );
            }
            JobDone::Vce { index, map, backup } => {
                self.session.push(
                    EntryKind::Tool,
                    format!(
                        "index: {} files, {} symbols, {} imports · added {}, modified {}, deleted {}, parsed {}, unchanged {}",
                        index.files,
                        index.symbols,
                        index.imports,
                        index.added,
                        index.modified,
                        index.deleted,
                        index.reparsed,
                        index.unchanged
                    ),
                );
                self.last_context_tokens = map.tokens;
                self.remember_vce(map.tokens);
                self.push_text(&map.text, CONTEXT_PREVIEW_LINES);
                self.session.push(
                    EntryKind::System,
                    format!(
                        "map: {} tokens, {} symbols in {} files",
                        map.tokens, map.symbols, map.files
                    ),
                );
                self.session.push(
                    EntryKind::System,
                    format!(
                        "backup: {} files in index2 · saved {}, ai skipped {}",
                        backup.files, backup.saved, backup.skipped_ai
                    ),
                );
                self.session.push(
                    EntryKind::System,
                    format!(
                        "vce: {} токенов проекта · {}",
                        format_tokens(self.vce_tokens as u64),
                        Self::money(self.vce_cost)
                    ),
                );
                self.refresh_counts();
            }
            JobDone::Restore(report) => {
                self.session.push(
                    EntryKind::Tool,
                    format!(
                        "back: restored {}, removed {}, unchanged {}, snapshot {}",
                        report.restored, report.removed, report.unchanged, report.files
                    ),
                );
                self.refresh_counts();
            }
            JobDone::Chat(result) => {
                let ChatResult {
                    message,
                    usage,
                    cost_rub,
                } = *result;
                let tool_calls = message.tool_calls.clone();
                let text = message.content.clone();

                let calculated = turn_cost(&self.price, &usage).total_rub();
                self.session.charge(&usage, &self.price);
                if let Some(text) = text
                    && !text.is_empty()
                    && self.streamed.is_empty()
                {
                    for line in text.lines() {
                        self.session.push(EntryKind::Assistant, line.to_owned());
                    }
                }
                self.streamed.clear();
                self.stream_entry = None;
                self.conversation.push(message);

                if tool_calls.is_empty() {
                    self.session.push(
                        EntryKind::System,
                        format!(
                            "usage: in {} · out {} · {}",
                            format_tokens(usage.input + usage.cache_read),
                            format_tokens(usage.output),
                            format_rub(calculated)
                        ),
                    );
                    if let Some(cost) = cost_rub {
                        self.session.push(
                            EntryKind::System,
                            format!("провайдер списал: {}", format_rub(cost)),
                        );
                    }
                } else {
                    self.pending_tools.extend(tool_calls);
                    self.advance_agent();
                }
            }
            JobDone::Catalog(catalog) => {
                if catalog.vendor == self.vendor {
                    let title = registry::label(&catalog.vendor).to_owned();
                    let count = catalog.models.len();
                    self.catalog = Some(*catalog);
                    self.session.push(
                        EntryKind::System,
                        format!("каталог {title}: {count} моделей · /models"),
                    );
                }
            }
            JobDone::Install { providers, models } => {
                self.catalog = Catalog::load_cached_for(&self.vendor).ok().flatten();
                self.session.push(
                    EntryKind::System,
                    format!(
                        "каталоги обновлены: {providers} провайдеров · {models} моделей · /models"
                    ),
                );
            }
            JobDone::Failed(error) => self.push_error(&error),
        }
        self.persist();
    }

    fn run_context(
        &mut self,
        input: &str,
        tokens: Option<usize>,
        compression: Option<&str>,
        strategy: Option<&str>,
    ) {
        if input.trim().is_empty() {
            self.push_error("нужен текст запроса");
            return;
        }

        let level = compression
            .and_then(CompressionLevel::from_name)
            .unwrap_or(CompressionLevel::Balanced);
        let forced = strategy.and_then(ExecutionStrategy::from_name);

        let request = ContextRequest {
            input: input.to_owned(),
            max_tokens: tokens.unwrap_or(self.context_budget),
            compression: level,
            strategy: forced,
        };

        match self.distributor.prepare(&self.engine, &request) {
            Ok(output) => {
                self.last_context_tokens = output.final_tokens;
                self.charge_input(output.final_tokens as u64);
                self.session.push(
                    EntryKind::System,
                    format!(
                        "{} · {} → {} tokens ({}% off) · segments {} · cache {}",
                        output.strategy.as_str(),
                        output.original_tokens,
                        output.final_tokens,
                        (output.compression.reduction_ratio * 100.0).round(),
                        output.segments,
                        if output.cache_hits > 0 { "hit" } else { "miss" }
                    ),
                );
                if !output.validation_missing.is_empty() {
                    self.push_error(&format!(
                        "не сохранилось: {}",
                        output.validation_missing.join(", ")
                    ));
                }
                self.push_text(&output.content, CONTEXT_PREVIEW_LINES);
                self.refresh_counts();
            }
            Err(error) => self.push_error(&format!("context: {error}")),
        }
    }

    fn run_search(&mut self, query: &str) {
        if query.trim().is_empty() {
            self.push_error("нужен запрос для поиска");
            return;
        }

        match self.engine.search_files(query, 20) {
            Ok(hits) => {
                self.session
                    .push(EntryKind::Tool, format!("files ({}):", hits.len()));
                for hit in hits {
                    self.session.push(
                        EntryKind::Tool,
                        format!("  {} ({} symbols, {})", hit.path, hit.symbols, hit.language),
                    );
                }
            }
            Err(error) => self.push_error(&format!("search: {error}")),
        }

        match self.engine.search_symbols(query, 20) {
            Ok(hits) => {
                self.session
                    .push(EntryKind::Tool, format!("symbols ({}):", hits.len()));
                for hit in hits {
                    self.session.push(
                        EntryKind::Tool,
                        format!(
                            "  {}:{}-{} {} {}",
                            hit.path, hit.start_line, hit.end_line, hit.kind, hit.qualified_name
                        ),
                    );
                }
            }
            Err(error) => self.push_error(&format!("search: {error}")),
        }
    }

    fn run_outline(&mut self, file: &str) {
        match self.engine.outline(file) {
            Ok(text) => self.push_text(&text, CONTEXT_PREVIEW_LINES),
            Err(error) => self.push_error(&format!("outline: {error}")),
        }
    }

    fn run_cat(&mut self, file: &str, from: Option<u32>, to: Option<u32>) {
        match self.engine.source(file, from, to) {
            Ok(view) => {
                let tokens = tokenizer::count(Encoding::O200kBase, &view.text) as u64;
                self.last_context_tokens = tokens as usize;
                self.charge_input(tokens);

                self.session.push(
                    EntryKind::System,
                    format!(
                        "{}: lines {}-{} of {}{}",
                        view.path,
                        view.start_line,
                        view.end_line,
                        view.total_lines,
                        if view.refreshed { " · reindexed" } else { "" }
                    ),
                );
                self.push_text(&view.text, CONTEXT_PREVIEW_LINES);
            }
            Err(error) => self.push_error(&format!("cat: {error}")),
        }
    }

    fn run_deps(&mut self, file: &str, depth: u32) {
        match self.engine.dependencies(file, depth) {
            Ok(deps) => {
                self.session.push(EntryKind::Tool, deps.path.to_string());
                self.session.push(
                    EntryKind::Tool,
                    format!("imports ({}):", deps.imports.len()),
                );
                for import in deps.imports {
                    self.session.push(
                        EntryKind::Tool,
                        format!(
                            "  {} {}{}",
                            import.depth,
                            import.path,
                            if import.resolved { "" } else { " (external)" }
                        ),
                    );
                }
                self.session.push(
                    EntryKind::Tool,
                    format!("imported by ({}):", deps.imported_by.len()),
                );
                for path in deps.imported_by {
                    self.session.push(EntryKind::Tool, format!("  {path}"));
                }
            }
            Err(error) => self.push_error(&format!("deps: {error}")),
        }
    }

    fn run_cache(&mut self) {
        match self.engine.store().cache_totals() {
            Ok(totals) => {
                self.cache_entries = totals.entries;
                let generation = self.engine.store().generation().unwrap_or(0);
                self.session.push(
                    EntryKind::Tool,
                    format!(
                        "cache M1: {} entries, {} bytes, {} accesses · generation {}",
                        totals.entries, totals.bytes, totals.accesses, generation
                    ),
                );
            }
            Err(error) => self.push_error(&format!("cache: {error}")),
        }
    }

    fn run_cache_clear(&mut self) {
        match self.engine.store().cache_clear() {
            Ok(removed) => {
                self.session
                    .push(EntryKind::Tool, format!("cache: removed {removed} entries"));
                self.cache_entries = 0;
            }
            Err(error) => self.push_error(&format!("cache: {error}")),
        }
    }

    fn run_skills(&mut self) {
        if self.skills.is_empty() {
            self.session.push(
                EntryKind::System,
                "навыков нет: положите SKILL.md в .venera/skills/<name>/",
            );
            return;
        }

        for skill in &self.skills {
            self.session.push(
                EntryKind::Tool,
                format!("  /skill {:<16} {}", skill.name, skill.description),
            );
        }
    }

    fn run_skill(&mut self, name: &str) {
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name).cloned() else {
            self.push_error(&format!("навык `{name}` не найден"));
            return;
        };

        self.session.push(
            EntryKind::System,
            format!("# {} — {}", skill.name, skill.description),
        );
        self.session.push(
            EntryKind::System,
            format!("source: {}", skill.path.display()),
        );
        self.push_text(&skill.body, CONTEXT_PREVIEW_LINES);
    }

    fn welcome(&mut self) {
        if self.session.entries.is_empty() {
            self.session.push(
                EntryKind::System,
                format!("Venera {} · {}", env!("CARGO_PKG_VERSION"), self.cwd),
            );
            self.session.push(
                EntryKind::System,
                "/help — команды · /model — модель · /theme — тема · /new — сессия",
            );

            if self.files == 0 {
                self.session
                    .push(EntryKind::System, "индекс пуст: /index или `venera index`");
            }
            if self.catalog.is_none() {
                self.session.push(
                    EntryKind::System,
                    "каталог ProxyAPI не найден: `venera models --refresh`",
                );
            }
            if !self.skills.is_empty() {
                self.session.push(
                    EntryKind::System,
                    format!("навыков: {} · /skills", self.skills.len()),
                );
            }
        }
    }

    /// Re-reads file, symbol, and cache counters.
    pub fn refresh_counts(&mut self) {
        if let Ok(counts) = self.engine.store().counts() {
            self.files = counts.files;
            self.symbols = counts.symbols;
        }
        if let Ok(totals) = self.engine.store().cache_totals() {
            self.cache_entries = totals.entries;
        }
    }

    /// Appends plain text, one transcript line per input line.
    pub fn push_text(&mut self, text: &str, limit: usize) {
        for (count, line) in text.lines().enumerate() {
            if count >= limit {
                self.session.push(EntryKind::Tool, "… (сокращено)");
                break;
            }
            self.session.push(EntryKind::Tool, line.to_owned());
        }
    }

    /// Appends an error line.
    pub fn push_error(&mut self, text: &str) {
        self.session.push(EntryKind::Error, text.to_owned());
    }

    /// Stores the session; failures go to the log, never to the screen.
    pub fn persist(&mut self) {
        let Some(store) = &self.store else {
            return;
        };

        // The prompt history lives in the editor while the app runs; a session file is the
        // only place that survives a restart.
        self.session.history = self.input.history().to_vec();

        if let Err(error) = store.save(&self.session) {
            crate::tui::logging::log(&format!("cannot save session: {error}"));
        }
    }

    /// Short model name for the header.
    pub fn model_label(&self) -> &str {
        self.session
            .model
            .rsplit('/')
            .next()
            .unwrap_or(&self.session.model)
    }

    /// Prompt budget of the active model.
    pub fn window(&self) -> u64 {
        self.window
    }

    /// How much of the context budget the last request used, between 0 and 1.
    pub fn context_ratio(&self) -> f64 {
        if self.context_budget == 0 {
            return 0.0;
        }
        (self.last_context_tokens as f64 / self.context_budget as f64).clamp(0.0, 1.0)
    }

    /// `in 12,4k · out 0 · 0,0321 руб.`.
    /// Money for the project overview: no six decimals for a fraction of a kopeck.
    fn money(rub: f64) -> String {
        if rub > 0.0 && rub < 0.01 {
            "<0,01 руб.".to_owned()
        } else {
            format_rub(rub)
        }
    }

    pub fn spend_line(&self) -> String {
        let spend = self.session.spend;
        let base = if spend.cache_read_tokens > 0 {
            format!(
                "in {} · cache {} · out {} · {}",
                format_tokens(spend.input_tokens),
                format_tokens(spend.cache_read_tokens),
                format_tokens(spend.output_tokens),
                format_rub(spend.rub)
            )
        } else {
            format!(
                "in {} · out {} · {}",
                format_tokens(spend.input_tokens),
                format_tokens(spend.output_tokens),
                format_rub(spend.rub)
            )
        };

        if self.vce_tokens == 0 {
            base
        } else {
            format!(
                "{base} · vce {} tok {}",
                format_tokens(self.vce_tokens as u64),
                Self::money(self.vce_cost)
            )
        }
    }

    /// True when Enter should complete the command instead of running it: the menu is
    /// open and the line is not yet an exact command name.
    fn completes_command(&self) -> bool {
        let matches = self.completions();
        if matches.is_empty() {
            return false;
        }

        let typed = self.input.text().trim_start_matches('/');
        !matches.iter().any(|spec| spec.name == typed)
    }

    /// Slash commands matching the current input.
    pub fn completions(&self) -> Vec<&'static Spec> {
        let Some(rest) = self.input.text().strip_prefix('/') else {
            return Vec::new();
        };
        if rest.contains(' ') {
            return Vec::new();
        }

        let needle = rest.to_ascii_lowercase();

        SPECS
            .iter()
            .filter(|spec| spec.name.starts_with(&needle))
            .collect()
    }

    /// Fills the input with the `index`-th visible command; commands with arguments get a
    /// trailing space so the next keystroke is the argument.
    fn complete_at(&mut self, index: usize) {
        let (name, needs_args) = {
            let matches = self.completions();
            match matches.get(index) {
                Some(spec) => (spec.name, !spec.usage.is_empty()),
                None => return,
            }
        };

        self.input.set_text(if needs_args {
            format!("/{name} ")
        } else {
            format!("/{name}")
        });
        self.completion = 0;
    }

    /// Current project root.
    pub fn root(&self) -> &Path {
        self.engine.root()
    }
}

/// Joins the lines of a multi-line prompt into one request line.
fn collapse(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strips the Windows verbatim prefix so the header shows `C:\project`, not `\\?\C:\project`.
pub fn display_path(path: &Path) -> String {
    crate::paths::display(path)
}

/// Trims a typed path and removes one matching pair of quotes.
fn clean_project_path(text: &str) -> &str {
    let text = text.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner.trim();
        }
    }
    text
}

/// Resolves `/project` input: a relative path is tried from the current project root.
fn resolve_project_path(text: &str, root: &Path) -> Result<PathBuf, String> {
    let raw = clean_project_path(text);
    if raw.is_empty() {
        return Err("пустой путь".to_owned());
    }

    let path = PathBuf::from(raw);
    let candidate = if path.is_absolute() || path.exists() {
        path
    } else {
        root.join(path)
    };

    if !candidate.exists() {
        return Err(format!(
            "каталог не найден: {}",
            crate::paths::display(&candidate)
        ));
    }
    if !candidate.is_dir() {
        return Err(format!(
            "это файл, а не каталог: {}",
            crate::paths::display(&candidate)
        ));
    }

    Ok(candidate)
}

fn distributor(config: &Config, window: u64) -> Distributor {
    let settings = &config.context;

    Distributor::new(Encoding::O200kBase, window, None)
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
        .with_validation(settings.compression.validate_output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::screens;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A real engine on a throwaway directory, without touching the user's session store.
    fn test_app() -> Result<(tempfile::TempDir, App), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config = Config::builtin()?;
        let root = dir.path().to_string_lossy().into_owned();
        let engine = Engine::open(&root, &config.index.database)?;
        let cwd = display_path(engine.root());

        let mut session = Session::new(cwd.clone(), "openai/gpt-6-luna", DEFAULT_THEME);
        session.push(EntryKind::User, "hello");
        session.push(EntryKind::Assistant, "hi");

        let app = App {
            engine,
            distributor: distributor(&config, 1_050_000),
            skills: Vec::new(),
            config: config.clone(),
            catalog: None,
            vendor: "proxyapi".to_owned(),
            api_input: None,
            vault_prompt: None,
            confirm_backup: false,
            confirm_reset: false,
            permissions: Permissions::default(),
            conversation: Vec::new(),
            pending_tools: VecDeque::new(),
            pending_tool: None,
            agent_steps: 0,
            input: Input::default(),
            scroll_back: 0,
            quit: false,
            theme: theme::builtin(DEFAULT_THEME).expect("builtin theme"),
            cwd: cwd.clone(),
            session,
            store: None,
            picker: None,
            project: None,
            completion: 0,
            job: None,
            watcher: None,
            stream_entry: None,
            streamed: String::new(),
            start: false,
            start_age: 0.0,
            project_name: Some(cwd.clone()),
            project_input: None,
            project_error: None,
            help: false,
            pulse: 0.0,
            max_scroll: Cell::new(0),
            hits: Hitboxes::default(),
            files: 3,
            symbols: 4,
            cache_entries: 5,
            last_context_tokens: 1_200,
            context_budget: 8_192,
            vce_tokens: 0,
            vce_cost: 0.0,
            price: models::unpriced(),
            window: 1_050_000,
            refresh: RefreshOptions {
                max_file_size: config.index.max_file_size,
                respect_gitignore: config.index.respect_gitignore,
                force: false,
            },
        };

        Ok((dir, app))
    }

    fn render(app: &App) -> Result<(Hitboxes, String), Box<dyn std::error::Error>> {
        let mut terminal = Terminal::new(TestBackend::new(100, 24))?;
        let mut hits = Hitboxes::default();
        terminal.draw(|frame| {
            hits = screens::draw(frame, app);
        })?;

        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        Ok((hits, text))
    }

    /// The header is the only place that answers "where am I and which model is this".
    #[test]
    fn the_header_shows_the_brand_the_directory_and_the_model()
    -> Result<(), Box<dyn std::error::Error>> {
        let (dir, app) = test_app()?;
        let (hits, text) = render(&app)?;

        assert_eq!(hits.input.height, 3, "the input box must have a border");
        assert!(hits.transcript.height > 3, "the transcript must get room");
        assert!(text.contains("VENERA"), "missing wordmark: {text:?}");
        assert!(text.contains("gpt-6-luna"), "missing model: {text:?}");

        let leaf = dir
            .path()
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        assert!(text.contains(&leaf), "missing directory: {text:?}");
        Ok(())
    }

    #[test]
    fn clicking_a_picker_row_applies_that_model() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, mut app) = test_app()?;
        app.open_picker();
        assert!(app.picker.is_some(), "the picker must open");

        let (hits, _) = render(&app)?;
        let hit = hits.picker.expect("picker hitboxes");
        assert!(hit.rows >= 2, "the curated list has several rows");
        app.set_hits(hits);

        let before = app.session.model.clone();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.inner.x + 2,
            row: hit.inner.y + 1,
            modifiers: KeyModifiers::NONE,
        });

        assert!(app.picker.is_none(), "a row click closes the picker");
        assert_ne!(app.session.model, before, "the second row is another model");
        Ok(())
    }

    #[test]
    fn scrolling_clamps_to_what_the_last_frame_could_show() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, mut app) = test_app()?;
        for index in 0..100 {
            app.session.push(EntryKind::Tool, format!("line {index}"));
        }

        let (hits, _) = render(&app)?;
        assert!(app.max_scroll.get() > 0, "the test transcript must scroll");
        let limit = app.max_scroll.get();

        for _ in 0..50 {
            app.handle(crate::tui::keybindings::Key::PageUp);
        }
        assert_eq!(app.scroll_back, limit);

        app.handle(crate::tui::keybindings::Key::PageDown);
        assert_eq!(app.scroll_back, limit.saturating_sub(10));
        assert!(hits.transcript.height > 0);
        Ok(())
    }

    #[test]
    fn help_swallows_characters_instead_of_typing_behind_it()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::tui::keybindings::Key;

        let (_dir, mut app) = test_app()?;
        for ch in "/help".chars() {
            app.handle(Key::Char(ch));
        }
        app.handle(Key::Enter);
        assert!(app.help, "the help overlay must open");

        app.handle(Key::Char('x'));
        assert!(app.input.is_empty(), "help must swallow the keystroke");

        app.handle(Key::Escape);
        assert!(!app.help);
        Ok(())
    }

    #[test]
    fn the_input_box_grows_with_the_prompt_and_stops_at_the_cap()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::tui::keybindings::Key;

        let (_dir, mut app) = test_app()?;
        let (hits, _) = render(&app)?;
        assert_eq!(hits.input.height, 3, "an empty prompt is one row");

        app.handle(Key::Newline);
        app.handle(Key::Char('x'));
        let (hits, _) = render(&app)?;
        assert_eq!(hits.input.height, 4, "two rows plus the border");

        for _ in 0..20 {
            app.handle(Key::Newline);
        }
        let (hits, _) = render(&app)?;
        assert_eq!(hits.input.height, 8, "six rows plus the border is the cap");
        Ok(())
    }

    #[test]
    fn enter_sends_a_multi_line_prompt_as_one_line() {
        use crate::tui::keybindings::Key;

        let Ok((_dir, mut app)) = test_app() else {
            panic!("the test app must open");
        };

        for ch in "найди".chars() {
            app.handle(Key::Char(ch));
        }
        app.handle(Key::Newline);
        for ch in "тесты".chars() {
            app.handle(Key::Char(ch));
        }
        assert_eq!(app.input.text(), "найди\nтесты");

        app.handle(Key::Enter);
        assert!(app.input.is_empty(), "the prompt is handed over");

        let sent = app
            .session
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::User && entry.text == "найди тесты")
            .expect("the request is in the transcript as one line");

        assert_eq!(sent.kind, EntryKind::User);
        assert_eq!(app.input.history(), &["найди тесты".to_owned()]);
    }

    #[test]
    fn the_up_arrow_brings_back_the_last_request() {
        use crate::tui::keybindings::Key;

        let Ok((_dir, mut app)) = test_app() else {
            panic!("the test app must open");
        };

        for ch in "/cwd".chars() {
            app.handle(Key::Char(ch));
        }
        app.handle(Key::Enter);
        assert!(app.input.is_empty());

        app.handle(Key::Up);
        assert_eq!(app.input.text(), "/cwd");
        assert_eq!(app.input.cursor(), 4, "the caret lands at the end");

        app.handle(Key::Down);
        assert!(
            app.input.is_empty(),
            "past the newest line the empty draft returns"
        );
    }

    #[test]
    fn api_lists_every_supported_provider() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, mut app) = test_app()?;
        app.run(Action::Api { provider: None });

        let lines = app
            .session
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        for provider in registry::PROVIDERS {
            assert!(lines.contains(provider.title), "missing {}", provider.id);
        }
        assert!(lines.contains("ключ"));
        Ok(())
    }

    #[test]
    fn provider_picker_opens_from_the_command() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, mut app) = test_app()?;
        app.run(Action::Providers { name: None });

        let picker = app.picker.as_ref().expect("provider picker");
        assert_eq!(picker.kind(), PickerKind::Provider);
        assert!(picker.visible().len() >= registry::PROVIDERS.len());
        Ok(())
    }

    #[test]
    fn changing_a_key_opens_the_secret_prompt() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, mut app) = test_app()?;
        app.run(Action::Api {
            provider: Some("aitunnel".to_owned()),
        });

        assert_eq!(app.vendor, "aitunnel");
        let input = app.api_input.as_ref().expect("secret prompt");
        assert_eq!(input.provider, "aitunnel");
        assert!(input.text.is_empty());
        Ok(())
    }

    #[test]
    fn a_write_call_waits_for_confirmation() -> Result<(), Box<dyn std::error::Error>> {
        let (dir, mut app) = test_app()?;
        app.pending_tools.push_back(ToolCall {
            id: "call_1".to_owned(),
            name: "write_file".to_owned(),
            arguments: r#"{"path":"note.txt","content":"x"}"#.to_owned(),
        });

        app.advance_agent();

        assert!(app.pending_tool.is_some(), "the modal must open");
        assert!(!dir.path().join("note.txt").exists(), "nothing written yet");
        assert!(!app.permissions.allow_write);
        Ok(())
    }

    #[test]
    fn streamed_text_is_split_into_transcript_lines() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, mut app) = test_app()?;
        app.session.entries.clear();
        app.append_stream("hel");
        app.append_stream("lo\nworld");

        let assistant: Vec<&str> = app
            .session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Assistant)
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(assistant, vec!["hello", "world"]);
        assert_eq!(app.streamed, "hello\nworld");
        Ok(())
    }

    #[test]
    fn every_write_call_waits_for_confirmation() -> Result<(), Box<dyn std::error::Error>> {
        let (dir, mut app) = test_app()?;
        for (id, name) in [("call_1", "one.txt"), ("call_2", "two.txt")] {
            app.pending_tools.push_back(ToolCall {
                id: id.to_owned(),
                name: "write_file".to_owned(),
                arguments: format!(r#"{{"path":"{name}","content":"x"}}"#),
            });
        }

        app.advance_agent();
        assert!(app.pending_tool.is_some(), "the first modal must open");

        app.handle_pending_tool(crate::tui::keybindings::Key::Enter);
        assert!(dir.path().join("one.txt").exists(), "the first write runs");
        assert!(
            !dir.path().join("two.txt").exists(),
            "the second stays unapproved"
        );
        assert!(app.pending_tool.is_some(), "the second modal must open");
        Ok(())
    }

    #[test]
    fn project_path_accepts_quotes_and_relative_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("my project");
        std::fs::create_dir_all(&target)?;

        let quoted = format!("\"{}\"", target.display());
        assert_eq!(resolve_project_path(&quoted, dir.path())?, target);
        assert_eq!(resolve_project_path("my project", dir.path())?, target);

        let file = dir.path().join("note.txt");
        std::fs::write(&file, "x")?;
        assert!(resolve_project_path(file.to_string_lossy().as_ref(), dir.path()).is_err());
        assert!(resolve_project_path("missing", dir.path()).is_err());
        Ok(())
    }
}
