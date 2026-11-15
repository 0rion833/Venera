//! First-run initialization: the user config and the interface language.
//!
//! `venera` with no arguments runs this once when no config exists, then opens the TUI.
//! `venera init` does the same explicitly; `--force` rewrites an existing config.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// User configuration directory, when the platform provides one.
pub fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "venera").map(|dirs| dirs.config_dir().to_path_buf())
}

/// Path of the user config file.
pub fn user_config() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.json"))
}

/// True when a project or user config already exists.
pub fn is_configured() -> bool {
    Path::new(".venera").join("config.json").is_file()
        || user_config().is_some_and(|path| path.is_file())
}

/// Writes the default config and `ru` language without prompts.
///
/// A bare `venera` launch uses this, so a double-click is enough: the app configures
/// itself and opens. The interactive wizard stays available as `venera init`.
pub fn ensure_defaults() -> bool {
    let Some(dir) = config_dir() else {
        return false;
    };
    let path = dir.join("config.json");

    if path.is_file() {
        return true;
    }

    let Ok(config) = Config::builtin() else {
        return false;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let Ok(text) = serde_json::to_string_pretty(&config) else {
        return false;
    };
    if std::fs::write(&path, text).is_err() {
        return false;
    }

    let _ = std::fs::write(dir.join("language"), "ru");
    true
}

/// Runs the wizard and writes the config; returns false on failure.
pub fn run(force: bool) -> bool {
    let Some(dir) = config_dir() else {
        eprintln!("venera: cannot find a configuration directory");
        return false;
    };
    let config_path = dir.join("config.json");

    if config_path.is_file() && !force {
        println!("VENERA · уже настроено: {}", config_path.display());
        println!("повторная настройка: venera init --force");
        return true;
    }

    println!("VENERA · первичная настройка");
    println!();

    let language = ask("Язык интерфейса: 1 — русский, 2 — English", "1");
    let language = if language.trim() == "2" { "en" } else { "ru" };

    let mut config = match Config::builtin() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("venera: {error}");
            return false;
        }
    };

    let default_model = config
        .defaults
        .model
        .clone()
        .unwrap_or_else(|| "gpt-6-luna".to_owned());
    let model = ask("Модель по умолчанию", &default_model);
    config.defaults.model = Some(model.clone());

    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("venera: cannot create {}: {error}", dir.display());
        return false;
    }

    let text = match serde_json::to_string_pretty(&config) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("venera: cannot serialize the config: {error}");
            return false;
        }
    };

    if let Err(error) = std::fs::write(&config_path, text) {
        eprintln!("venera: cannot write {}: {error}", config_path.display());
        return false;
    }
    let _ = std::fs::write(dir.join("language"), language);

    println!();
    println!("конфиг: {}", config_path.display());
    println!("язык:   {language}");
    println!("модель: {model}");
    println!("дальше: venera — TUI · venera index · venera map");
    true
}

/// Reads one line, falling back to `default` on EOF or an empty answer.
fn ask(prompt: &str, default: &str) -> String {
    print!("{prompt} [{default}]: ");
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return default.to_owned();
    }

    let line = line.trim();
    if line.is_empty() {
        default.to_owned()
    } else {
        line.to_owned()
    }
}
