//! Terminal UI (future `venera-tui`), built on `ratatui`.
//!
//! The shell is deliberately synchronous: a key press runs an engine action and redraws.
//! There is no model loop yet, so nothing streams; the transcript shows what the engine
//! returned, how many tokens it cost, and whether it came from the cache.
//!
//! While the alternate screen is active nothing may print to stdout or stderr, so errors
//! go to `~/.local/share/venera/logs/tui.log` (`AGENTS.md` §5). The screen is owned by a
//! guard: even when opening the project fails, raw mode and mouse capture are restored.

pub mod app;
pub mod highlight;
pub mod i18n;
pub mod input;
pub mod job;
pub mod keybindings;
pub mod logging;
pub mod markdown;
pub mod models;
pub mod picker;
pub mod project;
pub mod screens;
pub mod session;

use std::io;
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::LoadedConfig;
use crate::tui::app::App;

/// Frames per second of the status-bar pulse.
const TICK: Duration = Duration::from_millis(80);

/// Runs the interactive shell until the user leaves.
pub fn run(loaded: &LoadedConfig, root: &str) -> anyhow::Result<()> {
    // Legacy `cmd.exe` prints UTF-8 bytes through a cp866/cp1251 console otherwise, which
    // turns every Russian label into mojibake. Set the console to UTF-8 first.
    enable_utf8_console();

    // Legacy `cmd.exe` ignores alternate-screen and cursor escapes without VT mode, so
    // frames smear over the previous buffer instead of replacing it.
    enable_vt();

    // Open the project before touching the terminal: a broken index then reports itself
    // on a normal screen instead of leaving a half-initialised alternate screen behind.
    let mut app = App::new(loaded, root)?;
    logging::log(&format!("tui started in {}", app.root().display()));

    let screen = Screen::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let outcome = event_loop(&mut terminal, &mut app);

    if let Err(error) = &outcome {
        logging::log(&format!("tui stopped with error: {error}"));
    }

    drop(screen);
    terminal.show_cursor().ok();

    outcome
}

/// Raw mode, the alternate screen, mouse reporting, and bracketed paste, undone on drop.
struct Screen;

impl Screen {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // From here on the guard owns the terminal, so a failed `execute!` still restores it.
        let screen = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
        let _ = disable_raw_mode();
    }
}

/// Switches a Windows console to UTF-8 so Cyrillic labels are readable.
///
/// `chcp.com` changes the code page of the shared console without an `unsafe` WinAPI
/// call, which keeps `#![forbid(unsafe_code)]` intact.
#[cfg(windows)]
fn enable_utf8_console() {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("chcp.com")
        .arg("65001")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(not(windows))]
fn enable_utf8_console() {}

/// Enables ANSI/VT processing on Windows consoles; other platforms already speak it.
///
/// `crossterm::ansi_support::supports_ansi` performs the console-mode switch as a side
/// effect, so the TUI does not need its own unsafe WinAPI block.
#[cfg(windows)]
fn enable_vt() {
    let _ = crossterm::ansi_support::supports_ansi();
}

#[cfg(not(windows))]
fn enable_vt() {}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| {
            let hits = screens::draw(frame, app);
            app.set_hits(hits);
        })?;

        if app.quit {
            break;
        }

        // Keys, clicks, wheel, and paste all arrive as crossterm events. The timeout keeps
        // the pulse breathing while the user is idle.
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(key) = keybindings::map(key) {
                        app.handle(key);
                    }
                }
                Event::Mouse(mouse) => app.mouse(mouse),
                Event::Paste(text) => app.paste(&text),
                Event::Resize(_, _) => {
                    // A resized console keeps the old cells outside the diff area.
                    let _ = terminal.clear();
                }
                _ => {}
            }
        }

        app.tick();
    }

    if let Some(job) = app.job.take() {
        job.join();
    }

    app.persist();
    Ok(())
}
