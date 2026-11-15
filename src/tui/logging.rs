//! File logging for the terminal UI.
//!
//! While the alternate screen is active, `stderr` is not an option: anything printed
//! there corrupts the drawing. Problems go to `~/.local/share/venera/logs/tui.log`
//! instead (`AGENTS.md` §5).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// Log file path, when a data directory is available.
pub fn log_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "venera")
        .map(|dirs| dirs.data_dir().join("logs").join("tui.log"))
}

/// Appends one line to the log; failures are ignored on purpose.
pub fn log(message: &str) {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_path_is_inside_a_venera_data_directory() {
        if let Some(path) = log_path() {
            assert!(path.ends_with("tui.log"));
            assert!(path.to_string_lossy().contains("venera"));
        }
    }
}
