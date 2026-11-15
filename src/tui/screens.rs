//! Screen layout: header, transcript, pulsing status bar, input box, overlays.
//!
//! Layout code never names a raw color: it resolves a [`Slot`] through the active
//! [`Theme`], so switching palettes is a data change. Long transcript lines are wrapped
//! here rather than by `Paragraph`, which keeps `scroll` exact without relying on the
//! unstable `rendered-line-info` feature of `ratatui`.
//!
//! [`draw`] also reports the clickable rectangles of the frame ([`Hitboxes`]); the app
//! stores them and resolves mouse events against them. Nothing in this module mutates the
//! session except the scroll limit published through [`App::max_scroll`].

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::commands::SPECS;
use crate::core::cost::format_tokens;
use crate::theme::{Rgb, Slot, Theme};
use crate::tui::app::App;
use crate::tui::input::Input;
use crate::tui::session::{Entry, EntryKind};

/// Width of the context meter, in cells.
const BAR_WIDTH: usize = 12;

/// Rows of the completion popup, at most.
const COMPLETION_ROWS: usize = 8;

/// Rows of the model picker, at most.
const PICKER_ROWS: usize = 14;

/// Rows the prompt grows to before it scrolls inside the box.
const INPUT_ROWS: u16 = 6;

/// Cells the prompt sign `› ` occupies on every row.
const PROMPT: u16 = 2;

/// Body indent, so text never touches the terminal edge.
const GUTTER: u16 = 1;

/// The wordmark used when `ascii.txt` starts with the planet and has no logo block.
const LOGO_FALLBACK: [&str; 4] = [
    r" __   __",
    r" \ \ / /__ _ _  ___ _ _ __ _",
    r"  \ V / -_) ' \/ -_) '_/ _` |",
    r"   \_/\___|_||_\___|_| \__,_|",
];

/// Rows the planet needs before it is worth drawing at all.
const PLANET_MIN_ROWS: u16 = 6;

/// Largest planet the title screen draws: the artwork is detailed enough at this size,
/// and a bigger disc crowds the logo, the title and the hint.
const PLANET_MAX_ROWS: u16 = 14;
const PLANET_MAX_COLS: u16 = 44;

/// Sun longitude advanced per second, so the planet turns a full circle in ~6 seconds.
const SPIN_SPEED: f32 = 1.0;

/// Rows kept under the planet: one blank line, the title, and the hint.
const BELOW: u16 = 3;

/// Start screen art: the logo block and the planet block, parsed from `ascii.txt`.
struct StartArt {
    logo: Vec<&'static str>,
    planet: Vec<&'static str>,
}

/// Parses `ascii.txt`: everything before the first blank line is the logo, the rest is
/// the planet. The planet keeps its native resolution and is scaled at draw time, so it
/// can use whatever room the terminal actually has.
fn start_art() -> &'static StartArt {
    static ART: std::sync::OnceLock<StartArt> = std::sync::OnceLock::new();

    ART.get_or_init(|| {
        let source: &'static str = include_str!("../../ascii.txt");
        let mut lines = source.lines();
        let logo: Vec<&'static str> = lines
            .by_ref()
            .take_while(|line| !line.trim().is_empty())
            .collect();
        let planet: Vec<&'static str> = lines.filter(|line| !line.trim().is_empty()).collect();
        let logo = if logo.is_empty() {
            LOGO_FALLBACK.to_vec()
        } else {
            logo
        };

        StartArt { logo, planet }
    })
}

/// The stride that fits the planet into `rows` × `cols` cells, skipping the same number
/// of rows and columns. A terminal cell is about twice as tall as it is wide, so scaling
/// one axis only turns the disc into an ellipse.
fn planet_step(planet: &[&str], rows: u16, cols: u16) -> usize {
    let height = planet.len() as u16;
    let width = planet
        .iter()
        .map(|line| text_width(line) as u16)
        .max()
        .unwrap_or(0);

    let by_rows = (height.div_ceil(rows.max(1))) as usize;
    let by_cols = (width.div_ceil(cols.max(1))) as usize;

    by_rows.max(by_cols).max(1)
}

/// Ink of one character of the artwork, from empty to solid.
fn ink(ch: char) -> f32 {
    match ch {
        ' ' | '\t' => 0.0,
        '.' | ',' | '\'' | '`' => 0.15,
        ':' | ';' | '-' | '_' | '~' | '"' => 0.3,
        '=' => 0.45,
        '+' => 0.55,
        '*' => 0.65,
        '#' | '&' => 0.8,
        '%' => 0.9,
        '@' => 1.0,
        ch if ch.is_alphanumeric() => 0.6,
        _ => 0.5,
    }
}

/// Keeps every `step`-th block of the planet, turning each block into one density
/// character. Dropping characters instead would thin the disc into noise.
fn scaled_planet(planet: &[&str], step: usize) -> Vec<String> {
    let step = step.max(1);
    if step == 1 {
        return planet
            .iter()
            .map(|line| line.trim_end().to_owned())
            .collect();
    }

    const RAMP: [char; 10] = [' ', '.', ':', '-', '=', '+', '*', '#', '%', '@'];
    let grid: Vec<Vec<char>> = planet.iter().map(|line| line.chars().collect()).collect();
    let width = grid.iter().map(Vec::len).max().unwrap_or(0);
    let rows = planet.len().div_ceil(step);
    let cols = width.div_ceil(step);

    (0..rows)
        .map(|row| {
            (0..cols)
                .map(|col| {
                    let mut sum = 0.0;
                    let mut count = 0.0;
                    for line in grid.iter().skip(row * step).take(step) {
                        for index in col * step..((col + 1) * step).min(width) {
                            sum += ink(line.get(index).copied().unwrap_or(' '));
                            count += 1.0;
                        }
                    }
                    let density = if count == 0.0 { 0.0 } else { sum / count };
                    let index = (density * (RAMP.len() - 1) as f32).round() as usize;
                    RAMP[index.min(RAMP.len() - 1)]
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// Clickable parts of one frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hitboxes {
    /// The transcript area.
    pub transcript: Rect,
    /// The input box, border included.
    pub input: Rect,
    /// The model picker, while it is open.
    pub picker: Option<PickerHit>,
    /// The command completion popup, while it is visible.
    pub completions: Option<CompletionHit>,
    /// The help overlay, while it is open.
    pub help: Option<Rect>,
}

/// Rows of the picker and where they start.
#[derive(Debug, Clone, Copy, Default)]
pub struct PickerHit {
    /// The whole popup, border included.
    pub area: Rect,
    /// The list area, border excluded.
    pub inner: Rect,
    /// Index of the first visible row inside the filtered list.
    pub offset: usize,
    /// Number of visible rows.
    pub rows: usize,
}

/// The completion popup: rows start at `inner.y`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompletionHit {
    /// The list area, border excluded.
    pub inner: Rect,
    /// Number of visible rows.
    pub rows: usize,
    /// Index of the first visible row inside the match list.
    pub offset: usize,
}

/// Draws the whole screen and reports its hitboxes.
pub fn draw(frame: &mut Frame, app: &App) -> Hitboxes {
    if app.vault_prompt.is_some() {
        draw_vault(frame, app);
        if app.confirm_reset {
            draw_confirm_reset(frame, app);
        }
        return Hitboxes::default();
    }

    if app.start {
        draw_start(frame, app);
        return Hitboxes::default();
    }

    // Paint the theme background first: an empty console otherwise shows its own colour
    // through the gaps and the dark palette never reads as intended.
    frame.render_widget(
        Block::default().style(Style::default().bg(color(&app.theme, Slot::Background))),
        frame.area(),
    );

    let screen = frame.area();
    // The input box grows with the prompt, up to `INPUT_ROWS`, so a long request stays
    // visible instead of scrolling out of a single line.
    let input_rows = prompt_rows(app, screen.width);
    let rows = Layout::vertical([
        Constraint::Length(1),              // header
        Constraint::Length(1),              // rule
        Constraint::Min(3),                 // transcript
        Constraint::Length(1),              // status
        Constraint::Length(input_rows + 2), // input box
        Constraint::Length(1),              // hints
    ])
    .split(screen);

    let (header, rule, body, status, input, hints) =
        (rows[0], rows[1], rows[2], rows[3], rows[4], rows[5]);

    let mut hits = Hitboxes {
        transcript: body,
        input,
        ..Hitboxes::default()
    };

    draw_header(frame, app, header);
    draw_rule(frame, app, rule);
    draw_transcript(frame, app, body);
    draw_status(frame, app, status);
    draw_input(frame, app, input);
    draw_hints(frame, app, hints);

    if !app.completions().is_empty() {
        hits.completions = draw_completions(frame, app, input, status);
    }
    if app.picker.is_some() {
        hits.picker = draw_picker(frame, app);
    }
    if app.project.is_some() {
        draw_project(frame, app);
    }
    if app.project_input.is_some() {
        draw_project_input(frame, app);
    }
    if app.api_input.is_some() {
        draw_api_input(frame, app);
    }
    if app.pending_tool.is_some() {
        draw_pending_tool(frame, app);
    }
    if app.confirm_backup {
        draw_confirm_backup(frame, app);
    }
    if app.confirm_reset {
        draw_confirm_reset(frame, app);
    }
    if app.help {
        hits.help = Some(draw_help(frame, app));
    }

    hits
}

/// Light of the turning planet. `x` and `y` are cell positions spread over `[-1, 1]`
/// across the disc, `phase` is the sun's longitude in radians.
///
/// The sun stays in front of the disc and travels from one limb to the other: the
/// highlight and the terminator follow it, which reads as a turning planet. A sun that
/// went behind the disc would black it out for a quarter of the cycle, so it never does.
fn sphere_shade(x: f32, y: f32, phase: f32) -> f32 {
    let radius = x * x + y * y;
    if radius > 1.0 {
        return 0.0;
    }

    let depth = (1.0 - radius).max(0.0).sqrt();
    let (sun, front) = phase.sin_cos();
    let lit = (x * sun + y * 0.22 + depth * front).clamp(0.0, 1.0);

    0.15 + 0.85 * lit.powf(0.9)
}

/// Maps an index to `[-1, 1]` across a disc; used for both axes of the planet.
fn spread(index: usize, count: usize) -> f32 {
    if count <= 1 {
        return 0.0;
    }

    index as f32 / (count - 1) as f32 * 2.0 - 1.0
}

/// The planet as styled rows: the artwork keeps its own characters and its light angle
/// moves with the animation clock, so the disc turns instead of sitting still.
fn planet_lines(theme: &Theme, planet: &[String], phase: f32) -> Vec<Line<'static>> {
    let width = planet
        .iter()
        .map(|line| text_width(line))
        .max()
        .unwrap_or(0);
    let accent = theme.get(Slot::Accent);
    let night = accent.mix(Rgb::BLACK, 0.84);
    let day = accent.mix(Rgb::WHITE, 0.55);

    planet
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let y = spread(row, planet.len());
            let spans: Vec<Span> = line
                .chars()
                .enumerate()
                .map(|(col, ch)| {
                    let shade = sphere_shade(spread(col, width), y, phase);
                    let tone = night.mix(day, shade);
                    Span::styled(ch.to_string(), Style::default().fg(rgb(tone)))
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

fn draw_start(frame: &mut Frame, app: &App) {
    let theme = &app.theme;
    let screen = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(color(theme, Slot::Panel))),
        screen,
    );

    let start = start_art();
    let logo = &start.logo;
    let logo_width = logo.iter().map(|line| text_width(line)).max().unwrap_or(0) as u16;
    let logo_height = logo.len() as u16;

    // The planet at full resolution when it fits, evenly downsampled when it does not.
    let room = screen
        .height
        .saturating_sub(logo_height + 1 + BELOW)
        .min(PLANET_MAX_ROWS);
    let cols = screen.width.min(PLANET_MAX_COLS);
    let planet = if room < PLANET_MIN_ROWS {
        Vec::new()
    } else {
        let step = planet_step(&start.planet, room, cols);
        scaled_planet(&start.planet, step)
    };
    let planet_width = planet
        .iter()
        .map(|line| text_width(line))
        .max()
        .unwrap_or(0) as u16;
    let planet_height = planet.len() as u16;

    let art_height = logo_height
        + if planet.is_empty() {
            0
        } else {
            1 + planet_height
        };
    let top = screen.height.saturating_sub(art_height + BELOW) / 2;

    let art_area = Rect {
        x: screen.x + screen.width.saturating_sub(logo_width.min(screen.width)) / 2,
        y: screen.y + top,
        width: logo_width.min(screen.width),
        height: logo_height.min(screen.height),
    };

    let art: Vec<Line> = logo
        .iter()
        .map(|line| {
            Line::from(Span::styled(
                (*line).to_owned(),
                Style::default()
                    .fg(color(theme, Slot::Accent))
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(art), art_area);

    let planet_bottom = if planet.is_empty() {
        art_area.bottom()
    } else {
        let planet_area = Rect {
            x: screen.x + screen.width.saturating_sub(planet_width.min(screen.width)) / 2,
            y: art_area.bottom().saturating_add(1),
            width: planet_width.min(screen.width),
            height: planet_height,
        };
        let phase = (app.start_age * SPIN_SPEED) % std::f32::consts::TAU;
        frame.render_widget(
            Paragraph::new(planet_lines(theme, &planet, phase)),
            planet_area,
        );
        planet_area.bottom()
    };

    let below = Rect {
        x: screen.x,
        y: planet_bottom.saturating_add(1),
        width: screen.width,
        height: 2,
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                crate::tui::i18n::text("start.title"),
                Style::default().fg(color(theme, Slot::Text)),
            )),
            Line::from(Span::styled(
                crate::tui::i18n::text("start.hint"),
                Style::default().fg(color(theme, Slot::Muted)),
            )),
        ])
        .alignment(Alignment::Center),
        below,
    );

    let status = Rect {
        x: screen.x,
        y: screen.bottom().saturating_sub(1),
        width: screen.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("{} · /project", crate::tui::i18n::text("project.none")),
            Style::default().fg(color(theme, Slot::Dim)),
        ))
        .alignment(Alignment::Center),
        status,
    );
}

fn draw_project_input(frame: &mut Frame, app: &App) {
    let Some(text) = &app.project_input else {
        return;
    };
    let theme = &app.theme;
    let screen = frame.area();
    let width = screen.width.saturating_sub(8).clamp(40, 100);
    let height = if app.project_error.is_some() { 7 } else { 6 };
    let area = centered(screen, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Accent)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            format!(" {} ", crate::tui::i18n::text("project.title")),
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(
            crate::tui::i18n::text("project.prompt"),
            Style::default().fg(color(theme, Slot::Muted)),
        )),
        Line::from(Span::styled(
            format!("> {text}"),
            Style::default().fg(color(theme, Slot::Text)),
        )),
        Line::from(Span::styled(
            crate::tui::i18n::text("project.hint"),
            Style::default().fg(color(theme, Slot::Dim)),
        )),
    ];
    if let Some(error) = &app.project_error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(color(theme, Slot::Error)),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_api_input(frame: &mut Frame, app: &App) {
    let Some(input) = &app.api_input else {
        return;
    };
    let theme = &app.theme;
    let screen = frame.area();
    let width = screen.width.saturating_sub(8).clamp(40, 90);
    let area = centered(screen, width, 6);
    let title = format!(
        " {} · {} ",
        crate::tui::i18n::text("api.title"),
        crate::provider::registry::label(&input.provider)
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Accent)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            title,
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    // Only bullets leave the app state; the key itself is never rendered.
    let room = inner.width.saturating_sub(2) as usize;
    let mut masked = "•".repeat(input.text.chars().count().min(room));
    if input.text.chars().count() > room {
        masked.push('…');
    }
    let shown = if masked.is_empty() {
        Span::styled(
            crate::tui::i18n::text("api.prompt"),
            Style::default().fg(color(theme, Slot::Muted)),
        )
    } else {
        Span::styled(masked, Style::default().fg(color(theme, Slot::Text)))
    };

    let lines = vec![
        Line::from(shown),
        Line::from(Span::styled(
            crate::tui::i18n::text("api.hint"),
            Style::default().fg(color(theme, Slot::Dim)),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_vault(frame: &mut Frame, app: &App) {
    let Some(prompt) = &app.vault_prompt else {
        return;
    };
    let theme = &app.theme;
    let screen = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(color(theme, Slot::Background))),
        screen,
    );

    let width = screen.width.saturating_sub(8).clamp(40, 72);
    let area = centered(screen, width, 9);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Accent)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            format!(" {} ", crate::tui::i18n::text("vault.title")),
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let label = if prompt.create {
        if prompt.first.is_some() {
            crate::tui::i18n::text("vault.repeat")
        } else {
            crate::tui::i18n::text("vault.create")
        }
    } else {
        crate::tui::i18n::text("vault.unlock")
    };
    let hint = if prompt.create {
        crate::tui::i18n::text("vault.hint_create")
    } else {
        crate::tui::i18n::text("vault.hint_unlock")
    };

    let room = inner.width.saturating_sub(2) as usize;
    let bullets = "•".repeat(prompt.text.chars().count().min(room));
    let mut lines = vec![
        Line::from(Span::styled(
            label,
            Style::default().fg(color(theme, Slot::Muted)),
        )),
        Line::from(Span::styled(
            if bullets.is_empty() {
                "…".to_owned()
            } else {
                bullets
            },
            Style::default().fg(color(theme, Slot::Text)),
        )),
    ];
    if let Some(error) = &prompt.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(color(theme, Slot::Error)),
        )));
    }
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(color(theme, Slot::Dim)),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm_backup(frame: &mut Frame, app: &App) {
    let theme = &app.theme;
    let screen = frame.area();
    let width = screen.width.saturating_sub(8).clamp(40, 78);
    let area = centered(screen, width, 9);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Warn)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            " восстановление ",
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            "вернуть файлы из index2?",
            Style::default().fg(color(theme, Slot::Text)),
        )),
        Line::from(Span::styled(
            "файлы, созданные ИИ без снимка, будут удалены",
            Style::default().fg(color(theme, Slot::Warn)),
        )),
        Line::from(Span::styled(
            "Enter — восстановить · Esc — отмена",
            Style::default().fg(color(theme, Slot::Dim)),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm_reset(frame: &mut Frame, app: &App) {
    let theme = &app.theme;
    let screen = frame.area();
    let width = screen.width.saturating_sub(8).clamp(40, 72);
    let area = centered(screen, width, 7);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Error)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            format!(" {} ", crate::tui::i18n::text("vault.reset_title")),
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            crate::tui::i18n::text("vault.reset_text"),
            Style::default().fg(color(theme, Slot::Text)),
        )),
        Line::from(Span::styled(
            crate::tui::i18n::text("vault.reset_hint"),
            Style::default().fg(color(theme, Slot::Dim)),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_pending_tool(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_tool else {
        return;
    };
    let theme = &app.theme;
    let screen = frame.area();
    let width = screen.width.saturating_sub(8).clamp(40, 90);
    let path = pending
        .arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let name = pending.call.name.as_str();
    let exists = pending_path_exists(app, path);
    let content = pending
        .arguments
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let old = pending
        .arguments
        .get("old")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let new = pending
        .arguments
        .get("new")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let old_len = old.chars().count();
    let new_len = new.chars().count();
    let is_shell = name == "bash";
    let shrink = name == "edit_file" && old_len > 40 && new_len * 3 < old_len;
    let danger = (name == "write_file" && exists) || shrink;
    let tone = if danger {
        Slot::Error
    } else if name == "append_file" {
        Slot::Success
    } else {
        Slot::Warn
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let label = match name {
        "append_file" => "добавить в конец",
        "write_file" if exists => "ПОЛНАЯ ПЕРЕЗАПИСЬ",
        "write_file" => "создать файл",
        "edit_file" => "заменить фрагмент",
        "bash" => "выполнить команду",
        _ => "изменить файл",
    };
    lines.push(Line::from(Span::styled(
        if is_shell {
            format!("{label}:")
        } else {
            format!("{label}: {path}")
        },
        Style::default().fg(color(theme, tone)),
    )));
    lines.push(Line::from(Span::styled(
        if is_shell {
            format!("оболочка · каталог {}", app.cwd)
        } else {
            format!(
                "{} · {}",
                name,
                if exists {
                    "файл существует"
                } else {
                    "новый файл"
                }
            )
        },
        Style::default().fg(color(theme, Slot::Muted)),
    )));

    match name {
        "bash" => {
            let command = pending
                .arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            push_block(&mut lines, "$", command, theme, Slot::Text);
            lines.push(Line::from(Span::styled(
                "команда может изменить проект",
                Style::default().fg(color(theme, Slot::Warn)),
            )));
        }
        "append_file" => {
            lines.push(Line::from(Span::styled(
                "существующий текст сохранится",
                Style::default().fg(color(theme, Slot::Success)),
            )));
            lines.push(Line::from(Span::styled(
                "разделитель: авто (новая строка при необходимости)",
                Style::default().fg(color(theme, Slot::Dim)),
            )));
            push_block(&mut lines, "+", content, theme, Slot::Success);
        }
        "write_file" => {
            if exists {
                lines.push(Line::from(Span::styled(
                    "старое содержимое будет удалено",
                    Style::default().fg(color(theme, Slot::Error)),
                )));
            }
            push_block(&mut lines, "+", content, theme, Slot::Text);
        }
        "edit_file" => {
            lines.push(Line::from(Span::styled(
                format!("- {old_len} символов → + {new_len} символов"),
                Style::default().fg(color(theme, Slot::Muted)),
            )));
            push_block(&mut lines, "-", old, theme, Slot::Error);
            push_block(&mut lines, "+", new, theme, Slot::Success);
            if shrink {
                lines.push(Line::from(Span::styled(
                    "замена намного короче: проверь, что не удаляешь нужное",
                    Style::default().fg(color(theme, Slot::Error)),
                )));
            }
        }
        _ => {}
    }

    lines.push(Line::from(Span::styled(
        crate::tui::i18n::text("confirm.hint"),
        Style::default().fg(color(theme, Slot::Dim)),
    )));

    let height = (lines.len() as u16 + 2).clamp(7, screen.height.saturating_sub(4).max(7));
    let area = centered(screen, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, tone)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            format!(" {} ", crate::tui::i18n::text("confirm.title")),
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

fn pending_path_exists(app: &App, path: &str) -> bool {
    use std::path::Component;

    let candidate = std::path::Path::new(path);
    let mut depth = 0usize;
    for component in candidate.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            _ => return false,
        }
    }

    depth > 0 && std::path::Path::new(&app.cwd).join(candidate).is_file()
}

fn push_block(lines: &mut Vec<Line<'static>>, prefix: &str, text: &str, theme: &Theme, slot: Slot) {
    if text.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {prefix} (пусто)"),
            Style::default().fg(color(theme, Slot::Dim)),
        )));
        return;
    }

    let mut total = 0usize;
    for line in text.lines() {
        total += 1;
        if total > 3 {
            continue;
        }
        let mut clipped: String = line.chars().take(100).collect();
        if line.chars().count() > 100 {
            clipped.push('…');
        }
        lines.push(Line::from(Span::styled(
            format!("  {prefix} {clipped}"),
            Style::default().fg(color(theme, slot)),
        )));
    }

    if total > 3 {
        lines.push(Line::from(Span::styled(
            format!("  {prefix} … ещё {} строк", total - 3),
            Style::default().fg(color(theme, Slot::Dim)),
        )));
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let model = format!(
        "{} · {} · ctx {} ",
        app.model_label(),
        crate::provider::registry::label(&app.vendor),
        crate::tui::models::compact_tokens(app.window())
    );
    let right = (text_width(&model) as u16 + 1).min(area.width / 2);
    let columns = Layout::horizontal([Constraint::Min(10), Constraint::Length(right)]).split(area);

    let left_width = columns[0].width as usize;
    let path_room = left_width.saturating_sub(9 + 2);
    let mut left = vec![
        Span::styled(
            " VENERA ",
            Style::default()
                .fg(color(theme, Slot::AccentText))
                .bg(color(theme, Slot::Accent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            match &app.project_name {
                Some(name) => format!(" {} ", shorten_path(name, path_room)),
                None => format!(" {} ", crate::tui::i18n::text("project.none")),
            },
            Style::default().fg(color(theme, Slot::Text)),
        ),
    ];

    if app.files > 0 {
        left.push(Span::styled(
            format!(
                "· {} {} · {} {} ",
                app.files,
                crate::tui::i18n::text("header.files"),
                format_tokens(app.symbols as u64),
                crate::tui::i18n::text("header.symbols")
            ),
            Style::default().fg(color(theme, Slot::Muted)),
        ));
    }

    left.push(Span::styled(
        format!(
            "· {} {} ",
            crate::tui::i18n::text("header.session"),
            app.session.id
        ),
        Style::default().fg(color(theme, Slot::Muted)),
    ));

    frame.render_widget(Paragraph::new(Line::from(left)), columns[0]);

    let model_line = Line::from(vec![
        Span::styled(
            format!(
                "{} · {}",
                app.model_label(),
                crate::provider::registry::label(&app.vendor)
            ),
            Style::default()
                .fg(color(theme, Slot::Accent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " · ctx {}",
                crate::tui::models::compact_tokens(app.window())
            ),
            Style::default().fg(color(theme, Slot::Muted)),
        ),
        Span::raw(" "),
    ]);
    frame.render_widget(
        Paragraph::new(model_line).alignment(Alignment::Right),
        columns[1],
    );
}

fn draw_rule(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new("─".repeat(area.width as usize))
            .style(Style::default().fg(color(&app.theme, Slot::Border))),
        area,
    );
}

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let gutter = GUTTER.min(area.width / 4);
    let content = Rect {
        x: area.x + gutter,
        y: area.y,
        width: area.width.saturating_sub(gutter * 2),
        height: area.height,
    };
    let width = content.width.max(1) as usize;

    let mut lines: Vec<Line> = Vec::new();
    let mut previous: Option<EntryKind> = None;

    for entry in app.session.entries.iter() {
        // A blank line opens every user turn, and one closes a block of engine output, so
        // a run of tool lines reads as one answer instead of a wall.
        let space = match previous {
            None => false,
            Some(kind) => {
                entry.kind == EntryKind::User
                    || (kind == EntryKind::Tool && entry.kind != EntryKind::Tool)
            }
        };
        if space {
            lines.push(Line::default());
        }

        lines.extend(entry_lines(entry, width, &app.theme));
        previous = Some(entry.kind);
    }

    let height = content.height.max(1) as usize;
    // Chat grows upwards from the input: short transcripts sit at the bottom instead of
    // leaving a hole between the text and the prompt.
    if lines.len() < height {
        lines.splice(
            0..0,
            std::iter::repeat_n(Line::default(), height - lines.len()),
        );
    }
    let max_scroll = lines.len().saturating_sub(height);
    app.max_scroll.set(max_scroll.min(u16::MAX as usize) as u16);

    let offset = max_scroll
        .saturating_sub(app.scroll_back as usize)
        .min(u16::MAX as usize);

    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), content);
}

fn entry_lines(entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let (prefix, slot) = match entry.kind {
        EntryKind::User => ("› ", Slot::User),
        EntryKind::Assistant => ("● ", Slot::Assistant),
        EntryKind::System => ("· ", Slot::Dim),
        EntryKind::Tool => ("│ ", Slot::Border),
        EntryKind::Error => ("! ", Slot::Error),
    };

    let head_style = match entry.kind {
        EntryKind::User | EntryKind::Assistant => Style::default()
            .fg(color(theme, Slot::Accent))
            .add_modifier(Modifier::BOLD),
        EntryKind::Error => Style::default()
            .fg(color(theme, Slot::Error))
            .add_modifier(Modifier::BOLD),
        _ => Style::default().fg(color(theme, Slot::Muted)),
    };

    let content = width.saturating_sub(prefix.chars().count()).max(1);
    let body = split_chunks(&entry.text, content);
    let indent = " ".repeat(prefix.chars().count());

    body.into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let head = if index == 0 { prefix } else { indent.as_str() };
            Line::from(vec![
                Span::styled(head.to_owned(), head_style),
                Span::styled(
                    chunk.clone(),
                    Style::default().fg(color(theme, line_slot(entry.kind, &chunk, slot))),
                ),
            ])
        })
        .collect()
}

/// Color of one body line: diff markers inside engine output read as added or removed
/// lines instead of a grey wall of text.
fn line_slot(kind: EntryKind, line: &str, base: Slot) -> Slot {
    if !matches!(kind, EntryKind::Tool | EntryKind::Assistant) {
        return base;
    }

    let trimmed = line.trim_start();

    if trimmed.starts_with("+++") || trimmed.starts_with("---") {
        base
    } else if trimmed.starts_with('+') {
        Slot::Success
    } else if trimmed.starts_with('-') {
        Slot::Error
    } else if trimmed.starts_with("@@") {
        Slot::Muted
    } else {
        base
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let panel = Style::default().bg(color(theme, Slot::Panel));
    frame.render_widget(Block::default().style(panel), area);

    // The ruble counter is pinned right: a long context reading must never push the price
    // off the edge of a narrow terminal.
    let spend = app.spend_line();
    let right = (spend.chars().count() as u16 + 2).min(area.width.saturating_sub(24));
    let columns = Layout::horizontal([Constraint::Min(20), Constraint::Length(right)]).split(area);

    let breath = color(theme, Slot::Pulse);

    let line = if let Some(job) = &app.job {
        let (done, total) = job.progress.counts();
        let percent = job.progress.percent().unwrap_or(0);
        let filled = (percent as usize * BAR_WIDTH / 100).min(BAR_WIDTH);

        Line::from(vec![
            Span::styled(" ● ", Style::default().fg(breath)),
            Span::styled(
                format!("{} ", job.title),
                Style::default().fg(color(theme, Slot::Text)),
            ),
            Span::styled(
                "█".repeat(filled),
                Style::default().fg(color(theme, Slot::Accent)),
            ),
            Span::styled(
                "·".repeat(BAR_WIDTH - filled),
                Style::default().fg(color(theme, Slot::Muted)),
            ),
            Span::styled(
                format!(" {percent:>3}% "),
                Style::default().fg(color(theme, Slot::Dim)),
            ),
            Span::styled(
                format!("{done}/{total} "),
                Style::default().fg(color(theme, Slot::Dim)),
            ),
            Span::styled(
                job.progress.note(),
                Style::default().fg(color(theme, Slot::Muted)),
            ),
        ])
    } else {
        let ratio = app.context_ratio();
        let filled = ((ratio * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);

        Line::from(vec![
            Span::styled(" ● ", Style::default().fg(breath)),
            Span::styled("ctx ", Style::default().fg(color(theme, Slot::Muted))),
            Span::styled("█".repeat(filled), Style::default().fg(breath)),
            Span::styled(
                "·".repeat(BAR_WIDTH - filled),
                Style::default().fg(color(theme, Slot::Muted)),
            ),
            Span::styled(
                format!(" {:>3.0}% ", ratio * 100.0),
                Style::default().fg(color(theme, Slot::Dim)),
            ),
            Span::styled("· ", Style::default().fg(color(theme, Slot::Muted))),
            Span::styled(
                format!(
                    "{} / {} ",
                    format_tokens(app.last_context_tokens as u64),
                    format_tokens(app.context_budget as u64)
                ),
                Style::default().fg(color(theme, Slot::Dim)),
            ),
        ])
    };

    frame.render_widget(Paragraph::new(line), columns[0]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("{spend} "),
            Style::default().fg(color(theme, Slot::Dim)),
        ))
        .style(panel)
        .alignment(Alignment::Right),
        columns[1],
    );
}

/// One visual row of the prompt and the caret cell when the caret sits on it.
struct PromptRow {
    /// Characters shown on the row.
    text: String,
    /// Caret column inside the row, without the prompt sign.
    caret: Option<u16>,
}

/// Rows the input box needs for the current prompt, borders excluded.
fn prompt_rows(app: &App, screen_width: u16) -> u16 {
    let width = screen_width.saturating_sub(2 + PROMPT).max(1);
    let rows = wrap_prompt(&app.input, width as usize).len() as u16;

    rows.clamp(1, INPUT_ROWS)
}

/// Wraps the prompt at `width` characters, honouring the line breaks the user typed.
///
/// The break goes at the last space of the row when there is one, so a word is never cut
/// in half unless it is longer than the box. Columns are characters, exactly like the
/// transcript, so the caret column is the character offset inside the row.
fn wrap_prompt(input: &Input, width: usize) -> Vec<PromptRow> {
    let width = width.max(1);
    let chars: Vec<char> = input.text().chars().collect();

    let mut rows: Vec<String> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut row: Vec<char> = Vec::new();
    let mut space: Option<usize> = None;
    let mut start = 0;

    for (index, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            rows.push(row.iter().collect());
            starts.push(start);
            row.clear();
            space = None;
            start = index + 1;
            continue;
        }

        if row.len() == width {
            // Carry the tail of the row over: the space stays on the finished row, the
            // word starts the next one.
            let split = match space {
                Some(position) if position > 0 => position + 1,
                _ => row.len(),
            };
            let tail: Vec<char> = row.split_off(split);
            rows.push(row.iter().collect());
            starts.push(start);
            start += split;
            row = tail;
            space = row.iter().position(|ch| ch.is_whitespace());
        }

        if ch.is_whitespace() {
            space = Some(row.len());
        }
        row.push(*ch);
    }
    rows.push(row.iter().collect());
    starts.push(start);

    // The caret belongs to the last row that can hold it: at a wrap boundary that is the
    // start of the next row, not the cell just past the right edge.
    let cursor = input.cursor();
    let mut caret_row = rows.len() - 1;
    let mut caret_col = cursor.saturating_sub(starts[caret_row]);
    for index in (0..rows.len()).rev() {
        let begin = starts[index];
        let end = begin + rows[index].chars().count();
        if cursor >= begin && cursor <= end {
            caret_row = index;
            caret_col = cursor - begin;
            break;
        }
    }

    let mut wrapped: Vec<PromptRow> = rows
        .into_iter()
        .map(|text| PromptRow { text, caret: None })
        .collect();
    if let Some(row) = wrapped.get_mut(caret_row) {
        row.caret = Some(caret_col.min(width) as u16);
    }

    wrapped
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let border = if app.input.is_empty() {
        Slot::Border
    } else {
        Slot::Accent
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, border)))
        .style(Style::default().bg(color(theme, Slot::Panel)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let room = inner.width.saturating_sub(PROMPT);
    let rows = wrap_prompt(&app.input, room.max(1) as usize);

    if app.input.is_empty() {
        let spans = vec![
            Span::styled(
                "› ",
                Style::default()
                    .fg(color(theme, Slot::Accent))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "спроси что-нибудь или /help · Shift+Enter — новая строка",
                Style::default().fg(color(theme, Slot::Muted)),
            ),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
        frame.set_cursor_position(Position::new(inner.x + PROMPT, inner.y));
        return;
    }

    let visible = inner.height as usize;
    let caret_row = rows
        .iter()
        .position(|row| row.caret.is_some())
        .unwrap_or(rows.len() - 1);
    // Keep the caret on screen: a prompt taller than the box scrolls inside it.
    let top = caret_row.saturating_sub(visible.saturating_sub(1));

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(top)
        .take(visible)
        .map(|(index, row)| {
            let head = if index == 0 { "› " } else { "  " };
            let head_style = if index == 0 {
                Style::default()
                    .fg(color(theme, Slot::Accent))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color(theme, Slot::Muted))
            };

            Line::from(vec![
                Span::styled(head.to_owned(), head_style),
                Span::styled(
                    row.text.clone(),
                    Style::default().fg(color(theme, Slot::Text)),
                ),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);

    let caret = rows[caret_row].caret.unwrap_or(0);
    frame.set_cursor_position(Position::new(
        (inner.x + PROMPT + caret).min(inner.right().saturating_sub(1)),
        (inner.y + (caret_row - top) as u16).min(inner.bottom().saturating_sub(1)),
    ));
}

fn draw_hints(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let text = if app.help {
        " Esc или клик — закрыть ".to_owned()
    } else if app.picker.is_some() {
        " ↑/↓ или колесо — выбор · Enter или клик — применить · Esc — отмена ".to_owned()
    } else if app.scroll_back > 0 {
        format!(
            " прокручено на {} строк · PgDn, Ctrl+↓ или колесо — к последним ",
            app.scroll_back
        )
    } else if app.input.text().starts_with('/') {
        " ↑/↓ выбор · Tab/Enter дополнить · Esc очистить ".to_owned()
    } else {
        " Enter отправить · Shift+Enter новая строка · ↑/↓ история · PgUp/PgDn чат · /help "
            .to_owned()
    };

    frame.render_widget(
        Paragraph::new(Span::styled(
            text,
            Style::default().fg(color(theme, Slot::Muted)),
        )),
        area,
    );
}

fn draw_completions(
    frame: &mut Frame,
    app: &App,
    input_area: Rect,
    status_area: Rect,
) -> Option<CompletionHit> {
    let theme = &app.theme;
    let matches = app.completions();
    if matches.is_empty() {
        return None;
    }

    let rows = matches.len().min(COMPLETION_ROWS);
    let selected = app.completion.min(matches.len() - 1);
    let offset = selected.saturating_sub(rows.saturating_sub(1));
    let width = input_area
        .width
        .saturating_sub(4)
        .clamp(24, 72)
        .min(frame.area().width.saturating_sub(2));
    let height = rows as u16 + 2;
    let area = Rect {
        x: input_area.x + 2,
        // Sit on top of the status bar, never over it: the context meter must stay visible.
        y: status_area.y.saturating_sub(height),
        width,
        height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Border)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            " команды · Tab ",
            Style::default().fg(color(theme, Slot::Muted)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = matches
        .iter()
        .skip(offset)
        .take(rows)
        .enumerate()
        .map(|(index, spec)| {
            let marker = if index + offset == selected {
                "▌"
            } else {
                " "
            };
            Line::from(vec![
                Span::styled(
                    format!(" {marker} /{:<10}", spec.name),
                    Style::default()
                        .fg(color(theme, Slot::Accent))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<16}", spec.usage),
                    Style::default().fg(color(theme, Slot::Muted)),
                ),
                Span::styled(
                    crate::tui::i18n::text(&format!("help.{}", spec.name)),
                    Style::default().fg(color(theme, Slot::Dim)),
                ),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
    Some(CompletionHit {
        inner,
        rows,
        offset,
    })
}

fn draw_picker(frame: &mut Frame, app: &App) -> Option<PickerHit> {
    let picker = app.picker.as_ref()?;
    let theme = &app.theme;
    let screen = frame.area();
    let visible = picker.visible();
    let width = screen.width.saturating_sub(6).clamp(30, 84);
    let height = (visible.len().min(PICKER_ROWS) as u16 + 2)
        .min(screen.height.saturating_sub(4))
        .max(3);
    let area = centered(screen, width, height);

    let title = if picker.filter().is_empty() {
        format!(" {} · {} ", picker.title(), visible.len())
    } else {
        format!(
            " {} · {} по фильтру «{}» ",
            picker.title(),
            visible.len(),
            picker.filter()
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Muted)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            title,
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " ничего не найдено ",
                Style::default().fg(color(theme, Slot::Warn)),
            )),
            inner,
        );
        return Some(PickerHit {
            area,
            inner,
            offset: 0,
            rows: 0,
        });
    }

    let height = inner.height as usize;
    let selected = picker.selected_index().min(visible.len() - 1);
    let offset = selected.saturating_sub(height.saturating_sub(1));

    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(index, row)| {
            let current = row.current;
            let marker = if index == selected {
                "▌"
            } else if current {
                "●"
            } else {
                " "
            };

            let line = Line::from(vec![
                Span::styled(
                    format!(" {marker} {:<28}", row.label),
                    Style::default().fg(color(
                        theme,
                        if index == selected {
                            Slot::Accent
                        } else {
                            Slot::Text
                        },
                    )),
                ),
                Span::styled(
                    format!(" {} ", row.detail),
                    Style::default().fg(color(
                        theme,
                        if index == selected {
                            Slot::Text
                        } else {
                            Slot::Muted
                        },
                    )),
                ),
            ]);

            if index == selected {
                line.style(Style::default().bg(color(theme, Slot::Muted)))
            } else {
                line
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
    Some(PickerHit {
        area,
        inner,
        offset,
        rows: height,
    })
}

fn draw_project(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.project else {
        return;
    };
    let theme = &app.theme;
    let screen = frame.area();
    let rows = picker.rows();
    let width = screen.width.saturating_sub(6).clamp(30, 84);
    let height = (rows.len().min(PICKER_ROWS) as u16 + 2)
        .min(screen.height.saturating_sub(4))
        .max(3);
    let area = centered(screen, width, height);

    let title_path = picker.dir().display().to_string();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Accent)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            format!(
                " проекты · {} ",
                shorten_path(&title_path, width.saturating_sub(14) as usize)
            ),
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " нет подкаталогов ",
                Style::default().fg(color(theme, Slot::Muted)),
            )),
            inner,
        );
        return;
    }

    let visible = inner.height as usize;
    let selected = picker.selected_index().min(rows.len() - 1);
    let offset = selected.saturating_sub(visible.saturating_sub(1));

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, row)| {
            let marker = if index == selected { "▌" } else { " " };
            let mut line = Line::from(vec![
                Span::styled(
                    format!(" {marker} {:<28}", row.label),
                    Style::default().fg(color(
                        theme,
                        if index == selected {
                            Slot::Accent
                        } else {
                            Slot::Text
                        },
                    )),
                ),
                Span::styled(
                    format!(" {} ", row.detail),
                    Style::default().fg(color(
                        theme,
                        if index == selected {
                            Slot::Text
                        } else {
                            Slot::Muted
                        },
                    )),
                ),
            ]);
            if index == selected {
                line = line.style(Style::default().bg(color(theme, Slot::Muted)));
            }
            line
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_help(frame: &mut Frame, app: &App) -> Rect {
    let theme = &app.theme;
    let screen = frame.area();
    let area = centered(
        screen,
        screen.width.saturating_sub(6),
        screen.height.saturating_sub(4),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme, Slot::Muted)))
        .style(Style::default().bg(color(theme, Slot::Panel)))
        .title(Span::styled(
            " команды · Esc или клик — закрыть ",
            Style::default().fg(color(theme, Slot::Dim)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = SPECS
        .iter()
        .take(inner.height as usize)
        .map(|spec| {
            let usage: String = spec.usage.chars().take(30).collect();

            Line::from(vec![
                Span::styled(
                    format!(" /{:<10}", spec.name),
                    Style::default()
                        .fg(color(theme, Slot::Accent))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{usage:<32}"),
                    Style::default().fg(color(theme, Slot::Muted)),
                ),
                Span::styled(
                    crate::tui::i18n::text(&format!("help.{}", spec.name)),
                    Style::default().fg(color(theme, Slot::Dim)),
                ),
            ])
        })
        .collect();

    if lines.len() < inner.height as usize {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " ↑/↓ история ввода · Shift+Enter перенос строки · Enter отправить ",
            Style::default().fg(color(theme, Slot::Muted)),
        )));
    }
    if lines.len() < inner.height as usize {
        lines.push(Line::from(Span::styled(
            " Ctrl+←/→ по словам · Ctrl+W слово · Ctrl+U очистить · Esc очистить ",
            Style::default().fg(color(theme, Slot::Muted)),
        )));
    }
    if lines.len() < inner.height as usize {
        lines.push(Line::from(Span::styled(
            " мышь: колесо — история чата, клик по модели — выбрать ",
            Style::default().fg(color(theme, Slot::Muted)),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
    area
}

/// Resolves a palette slot for `ratatui`.
fn color(theme: &Theme, slot: Slot) -> Color {
    rgb(theme.get(slot))
}

/// Converts a theme color to a `ratatui` color.
fn rgb(value: Rgb) -> Color {
    Color::Rgb(value.r, value.g, value.b)
}

/// Display width of `text`, in terminal cells. Wide characters are counted as one; the
/// transcript has no CJK alignment guarantee, but wrapping stays inside the area.
fn text_width(text: &str) -> usize {
    text.chars().count()
}

/// Splits text into chunks of at most `width` characters.
fn split_chunks(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();

    if chars.is_empty() {
        return vec![String::new()];
    }

    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Keeps the tail of a long path: `…\src\tui\app.rs`.
fn shorten_path(path: &str, width: usize) -> String {
    let limit = width.max(8);
    let count = path.chars().count();
    if count <= limit {
        return path.to_owned();
    }

    let tail: String = path.chars().skip(count - (limit - 1)).collect();
    format!("…{tail}")
}

/// Places a `width` × `height` rectangle in the middle of `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);

    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// True when `(x, y)` is inside `area`.
pub fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> Input {
        let mut input = Input::default();
        for ch in text.chars() {
            input.insert(ch);
        }
        input
    }

    #[test]
    fn the_prompt_wraps_at_the_box_width() {
        let rows = wrap_prompt(&prompt("abcdef"), 4);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "abcd");
        assert_eq!(rows[1].text, "ef");
        assert_eq!(rows[1].caret, Some(2), "the caret closes the wrapped line");
    }

    #[test]
    fn words_are_not_cut_in_half_by_the_wrap() {
        let rows = wrap_prompt(&prompt("hello world"), 8);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "hello ");
        assert_eq!(rows[1].text, "world");
    }

    #[test]
    fn a_word_longer_than_the_box_is_still_broken() {
        let rows = wrap_prompt(&prompt("abcdefghij"), 4);

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "abcd");
        assert_eq!(rows[1].text, "efgh");
        assert_eq!(rows[2].text, "ij");
    }

    #[test]
    fn the_caret_at_a_wrap_boundary_moves_to_the_next_row() {
        let mut input = prompt("abcdef");
        input.left();
        input.left();

        let rows = wrap_prompt(&input, 4);
        assert_eq!(
            rows[1].caret,
            Some(0),
            "a caret at the boundary belongs to the next row, not past the edge"
        );
    }

    #[test]
    fn a_typed_line_break_starts_a_new_row() {
        let mut input = prompt("ab");
        input.insert('\n');
        input.insert('c');

        let rows = wrap_prompt(&input, 8);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "ab");
        assert_eq!(rows[1].text, "c");
        assert_eq!(rows[1].caret, Some(1));
    }

    #[test]
    fn an_empty_prompt_keeps_one_row_for_the_caret() {
        let rows = wrap_prompt(&Input::default(), 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "");
        assert_eq!(rows[0].caret, Some(0));
    }

    #[test]
    fn the_planet_is_scaled_by_the_same_step_on_both_axes() {
        let planet = ["########", "#......#", "#......#", "########"];

        let scaled = scaled_planet(&planet, 2);
        assert_eq!(scaled.len(), 2, "rows halve");
        assert!(
            scaled.iter().all(|row| row.chars().count() == 4),
            "columns halve with them, so the disc stays round"
        );
    }

    #[test]
    fn the_title_planet_stays_within_its_box() {
        let art = start_art();
        let step = planet_step(&art.planet, PLANET_MAX_ROWS, PLANET_MAX_COLS);
        let scaled = scaled_planet(&art.planet, step);

        assert!(!scaled.is_empty());
        assert!(scaled.len() as u16 <= PLANET_MAX_ROWS);
        let width = scaled
            .iter()
            .map(|line| text_width(line))
            .max()
            .unwrap_or(0);
        assert!(width as u16 <= PLANET_MAX_COLS);
    }

    #[test]
    fn the_light_moves_with_the_clock() {
        let before = sphere_shade(-0.5, 0.0, 0.0);
        let after = sphere_shade(-0.5, 0.0, 1.0);

        assert!(
            (before - after).abs() > 0.2,
            "one second of the clock turns the lit side"
        );
    }

    #[test]
    fn the_step_grows_until_the_planet_fits() {
        let planet = ["########"; 24];

        // 24 rows into 6 cells needs a stride of 4.
        assert_eq!(planet_step(&planet, 6, 100), 4);
        // A narrow terminal forces a larger stride even when the height is fine.
        assert_eq!(planet_step(&planet, 100, 2), 4);
    }

    #[test]
    fn the_light_stops_at_the_edge_of_the_disc() {
        assert_eq!(sphere_shade(1.5, 0.0, 0.0), 0.0);
        assert!(sphere_shade(0.0, 0.0, 0.0) > 0.0, "the front is lit");
    }

    #[test]
    fn the_sun_crosses_the_disc_instead_of_blacking_it_out() {
        let phase = 0.8;
        let lit = sphere_shade(0.6, 0.0, phase);
        let shadow = sphere_shade(-0.6, 0.0, phase);

        assert!(lit > shadow, "the terminator follows the sun");
        assert!(
            sphere_shade(0.0, 0.0, -std::f32::consts::FRAC_PI_2) > 0.0,
            "the planet never goes fully dark"
        );
    }
}
