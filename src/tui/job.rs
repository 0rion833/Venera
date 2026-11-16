//! Background jobs for slow engine actions (`/index`, `/map`).
//!
//! Both can take seconds on a large project. They run on a worker thread with their own
//! engine handle and publish progress through atomics, so a frame never waits on SQLite
//! and the UI keeps redrawing. Results come back through a channel and are applied on the
//! UI thread in [`crate::tui::app::App::tick`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::config::Reasoning;
use crate::provider::catalog::Catalog;
use crate::provider::openai_compat::{self, ChatMessage};
use crate::provider::usage::Usage;
use crate::provider::{keys, registry};
use crate::vce::backup::{self, BackupReport, RestoreReport};
use crate::vce::index::map::{MapOptions, ProjectMap};
use crate::vce::index::query::Engine;
use crate::vce::index::refresh::{self, IndexReport, RefreshOptions};

/// Progress of a running job, readable from the UI thread every frame.
#[derive(Debug)]
pub struct Progress {
    done: AtomicUsize,
    total: AtomicUsize,
    note: Mutex<String>,
}

impl Progress {
    fn new(note: &str) -> Arc<Self> {
        Arc::new(Self {
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            note: Mutex::new(note.to_owned()),
        })
    }

    /// Records how many units the job has in total.
    pub fn set_total(&self, total: usize) {
        self.total.store(total.max(1), Ordering::Relaxed);
    }

    /// Records how many units are finished.
    pub fn advance(&self, done: usize) {
        self.done.store(done, Ordering::Relaxed);
    }

    /// `(done, total)` as of this frame.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }

    /// Percent complete, `None` while the total is still unknown.
    pub fn percent(&self) -> Option<u8> {
        let (done, total) = self.counts();
        if total == 0 {
            return None;
        }
        Some((done.saturating_mul(100) / total).min(100) as u8)
    }

    /// Current phase, shown after the bar.
    pub fn note(&self) -> String {
        self.note
            .lock()
            .map(|note| note.clone())
            .unwrap_or_default()
    }

    fn set_note(&self, text: &str) {
        if let Ok(mut note) = self.note.lock() {
            *note = text.to_owned();
        }
    }
}

/// What a finished job produced.
#[derive(Debug)]
pub enum JobDone {
    /// The index was refreshed.
    Index(Box<IndexReport>),
    /// The map was rendered.
    Map(Box<ProjectMap>),
    /// Index refresh and map render in one job.
    Vce {
        /// Index report.
        index: Box<IndexReport>,
        /// Rendered map.
        map: Box<ProjectMap>,
        /// What was copied into `index2`.
        backup: Box<BackupReport>,
    },
    /// A `/back` restore finished.
    Restore(Box<RestoreReport>),
    /// A provider catalog was downloaded.
    Catalog(Box<Catalog>),
    /// Every provider catalog was downloaded by `/install`.
    Install {
        /// Providers whose catalog was refreshed.
        providers: usize,
        /// Models across those catalogs.
        models: usize,
    },
    /// A chat completion arrived.
    Chat(Box<ChatResult>),
    /// The job failed; the message is ready for the transcript.
    Failed(String),
}

/// One answer from a provider.
#[derive(Debug)]
pub struct ChatResult {
    /// Assistant message, including tool calls.
    pub message: ChatMessage,
    /// Provider-reported usage.
    pub usage: Usage,
    /// Provider-reported charge, when it is available.
    pub cost_rub: Option<f64>,
}

/// A running background job.
pub struct Job {
    /// Command name shown in the status bar.
    pub title: &'static str,
    /// Live progress.
    pub progress: Arc<Progress>,
    receiver: Receiver<JobDone>,
    stream: Option<Receiver<String>>,
    handle: Option<JoinHandle<()>>,
}

impl Job {
    /// Takes a finished result without blocking.
    pub fn try_done(&self) -> Option<JobDone> {
        match self.receiver.try_recv() {
            Ok(done) => Some(done),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(JobDone::Failed(
                "фоновый поток завершился без результата".to_owned(),
            )),
        }
    }

    /// Drains text deltas received since the previous frame.
    pub fn try_text(&self) -> Vec<String> {
        let Some(stream) = &self.stream else {
            return Vec::new();
        };
        let mut deltas = Vec::new();
        while let Ok(delta) = stream.try_recv() {
            deltas.push(delta);
        }
        deltas
    }

    /// Waits for the worker; used when the UI leaves so the log is not cut mid-write.
    pub fn join(self) {
        if let Some(handle) = self.handle {
            let _ = handle.join();
        }
    }
}

/// Starts an incremental refresh on a worker thread.
pub fn index(root: PathBuf, database: String, options: RefreshOptions) -> Job {
    let progress = Progress::new("индекс");
    let (sender, receiver) = mpsc::channel();
    let worker = Arc::clone(&progress);

    let handle = std::thread::spawn(move || {
        let done = match run_index(&root, &database, &options, &worker) {
            Ok(report) => JobDone::Index(Box::new(report)),
            Err(error) => JobDone::Failed(error),
        };
        let _ = sender.send(done);
    });

    Job {
        title: "index",
        progress,
        receiver,
        stream: None,
        handle: Some(handle),
    }
}

/// Starts a map render on a worker thread.
pub fn map(root: PathBuf, database: String, options: MapOptions) -> Job {
    let progress = Progress::new("читаю индекс");
    progress.set_total(3);
    let (sender, receiver) = mpsc::channel();
    let worker = Arc::clone(&progress);

    let handle = std::thread::spawn(move || {
        let done = match run_map(&root, &database, &options, &worker) {
            Ok(map) => JobDone::Map(Box::new(map)),
            Err(error) => JobDone::Failed(error),
        };
        let _ = sender.send(done);
    });

    Job {
        title: "map",
        progress,
        receiver,
        stream: None,
        handle: Some(handle),
    }
}

/// Starts an index refresh followed by a map render on one worker thread.
pub fn vce(
    root: PathBuf,
    database: String,
    refresh_options: RefreshOptions,
    map_options: MapOptions,
) -> Job {
    let progress = Progress::new("индекс");
    let (sender, receiver) = mpsc::channel();
    let worker = Arc::clone(&progress);

    let handle = std::thread::spawn(move || {
        let done = match run_vce(&root, &database, &refresh_options, &map_options, &worker) {
            Ok((index, map, backup)) => JobDone::Vce {
                index: Box::new(index),
                map: Box::new(map),
                backup: Box::new(backup),
            },
            Err(error) => JobDone::Failed(error),
        };
        let _ = sender.send(done);
    });

    Job {
        title: "vce",
        progress,
        receiver,
        stream: None,
        handle: Some(handle),
    }
}

/// Restores `index2` on a worker thread.
pub fn restore(root: PathBuf, database: String) -> Job {
    let progress = Progress::new("восстанавливаю");
    progress.set_total(1);
    let worker = Arc::clone(&progress);
    let (sender, receiver) = mpsc::channel();

    let handle = std::thread::spawn(move || {
        let done = match run_restore(&root, &database, &worker) {
            Ok(report) => JobDone::Restore(Box::new(report)),
            Err(error) => JobDone::Failed(error),
        };
        let _ = sender.send(done);
    });

    Job {
        title: "back",
        progress,
        receiver,
        stream: None,
        handle: Some(handle),
    }
}

/// Sends one non-streaming chat request on a worker thread.
pub fn chat(
    provider: String,
    model: String,
    messages: Vec<ChatMessage>,
    reasoning: Reasoning,
    tools: Vec<serde_json::Value>,
) -> Job {
    let progress = Progress::new("жду ответ");
    progress.set_total(1);
    let (sender, receiver) = mpsc::channel();
    let (stream_tx, stream_rx) = mpsc::channel();

    let handle = std::thread::spawn(move || {
        let done = match run_chat(&provider, &model, &messages, reasoning, &tools, &stream_tx) {
            Ok(result) => JobDone::Chat(Box::new(result)),
            Err(error) => JobDone::Failed(error),
        };
        let _ = sender.send(done);
    });

    Job {
        title: "chat",
        progress,
        receiver,
        stream: Some(stream_rx),
        handle: Some(handle),
    }
}

fn run_chat(
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    reasoning: Reasoning,
    tools: &[serde_json::Value],
    stream: &mpsc::Sender<String>,
) -> Result<ChatResult, String> {
    let Some(provider) = registry::get(provider_id) else {
        return Err(format!("неизвестный провайдер `{provider_id}`"));
    };
    let Some(key) = keys::resolve(provider.id).map_err(|error| error.to_string())? else {
        return Err(format!(
            "ключ не задан: /api {provider_id} — вставь API-ключ"
        ));
    };

    let tools = if tools.is_empty() { None } else { Some(tools) };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;

    runtime
        .block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()
                .map_err(|error| error.to_string())?;
            let delta_sender = stream.clone();
            openai_compat::complete_stream(
                &client,
                provider,
                &key.secret,
                model,
                messages,
                reasoning,
                tools,
                move |delta| {
                    let _ = delta_sender.send(delta);
                },
            )
            .await
            .map_err(|error| error.to_string())
        })
        .map(|completion| ChatResult {
            message: completion.message,
            usage: completion.usage,
            cost_rub: completion.cost_rub,
        })
}

/// Downloads one provider's catalog on a worker thread.
pub fn catalog(provider: String) -> Job {
    let progress = Progress::new("качаю модели");
    progress.set_total(1);
    let (sender, receiver) = mpsc::channel();

    let handle = std::thread::spawn(move || {
        let done = match run_catalog(&provider) {
            Ok(catalog) => JobDone::Catalog(Box::new(catalog)),
            Err(error) => JobDone::Failed(error),
        };
        let _ = sender.send(done);
    });

    Job {
        title: "catalog",
        progress,
        receiver,
        stream: None,
        handle: Some(handle),
    }
}

fn run_catalog(provider: &str) -> Result<Catalog, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;

    runtime
        .block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(45))
                .build()?;
            let catalog = Catalog::fetch_for(provider, &client).await?;
            let _ = catalog.save_cached();
            Ok::<Catalog, crate::provider::catalog::CatalogError>(catalog)
        })
        .map_err(|error| error.to_string())
}

/// Downloads the catalog of every supported aggregator on a worker thread.
pub fn install() -> Job {
    let progress = Progress::new("качаю каталоги");
    progress.set_total(registry::PROVIDERS.len());
    let worker = Arc::clone(&progress);
    let (sender, receiver) = mpsc::channel();

    let handle = std::thread::spawn(move || {
        let done = match run_install(&worker) {
            Ok((providers, models)) => JobDone::Install { providers, models },
            Err(error) => JobDone::Failed(error),
        };
        let _ = sender.send(done);
    });

    Job {
        title: "install",
        progress,
        receiver,
        stream: None,
        handle: Some(handle),
    }
}

fn run_install(progress: &Progress) -> Result<(usize, usize), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;

    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|error| error.to_string())?;

        let mut providers = 0usize;
        let mut models = 0usize;
        let mut failures: Vec<String> = Vec::new();

        for (index, provider) in registry::PROVIDERS.iter().enumerate() {
            progress.set_note(&format!("качаю каталог {}", provider.id));
            match Catalog::fetch_for(provider.id, &client).await {
                Ok(catalog) => {
                    models += catalog.models.len();
                    catalog.save_cached().map_err(|error| error.to_string())?;
                    providers += 1;
                }
                // One aggregator being down must not block the rest.
                Err(error) => failures.push(format!("{}: {error}", provider.id)),
            }
            progress.advance(index + 1);
        }

        if providers == 0 {
            return Err(format!(
                "не удалось скачать ни одного каталога: {}",
                failures.join("; ")
            ));
        }

        progress.set_note("готово");
        Ok((providers, models))
    })
}

fn run_index(
    root: &Path,
    database: &str,
    options: &RefreshOptions,
    progress: &Progress,
) -> Result<IndexReport, String> {
    let engine = Engine::open(root, database).map_err(|error| error.to_string())?;
    refresh::refresh_with_progress(engine.store(), root, options, &mut |done, total| {
        progress.set_total(total);
        progress.advance(done);
    })
    .map_err(|error| error.to_string())
}

fn run_map(
    root: &Path,
    database: &str,
    options: &MapOptions,
    progress: &Progress,
) -> Result<ProjectMap, String> {
    let engine = Engine::open(root, database).map_err(|error| error.to_string())?;
    progress.advance(1);
    progress.set_note("считаю карту");
    let map = engine
        .map(options, None)
        .map_err(|error| error.to_string())?;
    progress.advance(3);
    Ok(map)
}

fn run_vce(
    root: &Path,
    database: &str,
    refresh_options: &RefreshOptions,
    map_options: &MapOptions,
    progress: &Progress,
) -> Result<(IndexReport, ProjectMap, BackupReport), String> {
    let engine = Engine::open(root, database).map_err(|error| error.to_string())?;
    let index = refresh::refresh_with_progress(
        engine.store(),
        root,
        refresh_options,
        &mut |done, total| {
            progress.set_total(total);
            progress.advance(done);
        },
    )
    .map_err(|error| error.to_string())?;

    progress.set_note("строю карту");
    let map = engine
        .map(map_options, None)
        .map_err(|error| error.to_string())?;

    progress.set_note("делаю бэкап");
    let backup = backup::snapshot(&engine).map_err(|error| error.to_string())?;

    Ok((index, map, backup))
}

fn run_restore(root: &Path, database: &str, progress: &Progress) -> Result<RestoreReport, String> {
    let engine = Engine::open(root, database).map_err(|error| error.to_string())?;
    let report = backup::restore(&engine).map_err(|error| error.to_string())?;
    progress.advance(1);
    Ok(report)
}
