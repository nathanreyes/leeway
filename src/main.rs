//! Ballpark's terminal UI.
//!
//! The app is now multi-screen, so this file holds the *shared* scaffolding and each
//! screen lives in its own module:
//!   - `dashboard` — the "what's left" view (the daily loop)
//!   - `plans`     — the plans list and the plan editor (templates you stamp)
//!
//! `main.rs` owns three cross-cutting concerns the screens share:
//!   1. `App` — all mutable UI state (the data itself stays in SQLite).
//!   2. The **modal** system — a text prompt and a yes/no confirm that float over any
//!      screen and, while open, capture all input.
//!   3. The event loop: for the current screen, load its data, draw, read one key, and
//!      route it either to the open modal or to the screen's own handler.

mod dashboard;
mod plans;

use anyhow::Result;
use ballpark::money::Money;
use ballpark::{db, ops, queries};
use chrono::{Datelike, Local, NaiveDate};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use rusqlite::Connection;
use std::path::PathBuf;

/// Which screen is showing. `PlanEditor` carries the id of the plan being edited —
/// that's how the loop knows which plan's items to load.
pub enum Screen {
    Dashboard,
    Plans,
    PlanEditor { plan_id: String },
}

/// A floating dialog that captures input while open. Two flavors cover everything we
/// need: free text (names, amounts, a month to stamp) and a destructive-action confirm.
pub enum Modal {
    Text(TextPrompt),
    Confirm(Confirm),
}

/// A single-line text input. `kind` records what to do with the text on submit.
pub struct TextPrompt {
    pub title: String,
    pub buffer: String,
    pub kind: PromptKind,
}

/// What a text prompt's submitted value means.
pub enum PromptKind {
    NewPlan,
    RenamePlan { id: String },
    ItemLabel { id: String },
    ItemAmount { id: String },
    StampMonth { plan_id: String },
}

pub struct Confirm {
    pub title: String,
    pub action: ConfirmAction,
}

/// Destructive actions that require a yes/no before running.
pub enum ConfirmAction {
    DeletePlan { id: String },
    DeleteItem { id: String },
}

/// All mutable UI state. Each screen keeps its own selection index so moving between
/// screens doesn't scramble where you were.
pub struct App {
    pub conn: Connection,
    pub screen: Screen,
    pub should_quit: bool,
    pub dash_sel: usize,
    pub plans_sel: usize,
    pub editor_sel: usize,
    /// After creating an item we want to jump the selection onto it, but its list
    /// position isn't known until the next reload (rows are sorted). We stash the id
    /// here and the loop resolves it to an index once the items are loaded.
    pub pending_select: Option<String>,
    pub modal: Option<Modal>,
    /// A transient one-liner (errors, confirmations) shown in the footer.
    pub status: Option<String>,
}

impl App {
    fn open_text(&mut self, title: impl Into<String>, buffer: impl Into<String>, kind: PromptKind) {
        self.modal = Some(Modal::Text(TextPrompt {
            title: title.into(),
            buffer: buffer.into(),
            kind,
        }));
    }

    fn open_confirm(&mut self, title: impl Into<String>, action: ConfirmAction) {
        self.modal = Some(Modal::Confirm(Confirm { title: title.into(), action }));
    }
}

fn main() -> Result<()> {
    let path = PathBuf::from("ballpark.db");
    let mut conn = db::open(&path)?;
    ops::seed_demo(&mut conn)?;

    let mut app = App {
        conn,
        screen: Screen::Dashboard,
        should_quit: false,
        dash_sel: 0,
        plans_sel: 0,
        editor_sel: 0,
        pending_select: None,
        modal: None,
        status: None,
    };

    let terminal = ratatui::init();
    let result = run(terminal, &mut app);
    ratatui::restore();
    result
}

/// The event loop. Each iteration loads only the data the current screen needs, draws,
/// then reads and routes one key.
fn run(mut terminal: DefaultTerminal, app: &mut App) -> Result<()> {
    let today = Local::now().date_naive();

    while !app.should_quit {
        // `match` on the screen keeps each branch's data local — the borrow of `app.conn`
        // for loading is released before we take a `&mut app` to handle input.
        match &app.screen {
            Screen::Dashboard => {
                let view = ballpark::view::MonthView::build(&app.conn, today)?;
                if let Some(v) = &view {
                    clamp(&mut app.dash_sel, v.standalone.len());
                }
                terminal.draw(|f| {
                    dashboard::draw(f, app, &view);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key()? {
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else {
                        dashboard::handle_key(app, key, &view)?;
                    }
                }
            }

            Screen::Plans => {
                let summaries = queries::plan_summaries(&app.conn)?;
                clamp(&mut app.plans_sel, summaries.len());
                terminal.draw(|f| {
                    plans::draw_list(f, app, &summaries);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key()? {
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else {
                        plans::handle_list_key(app, key, &summaries)?;
                    }
                }
            }

            Screen::PlanEditor { plan_id } => {
                let plan_id = plan_id.clone();
                // The plan may have been deleted (e.g. via a confirm) — fall back to the list.
                let Some(plan) = queries::get_plan(&app.conn, &plan_id)? else {
                    app.screen = Screen::Plans;
                    continue;
                };
                let items = queries::load_plan_items(&app.conn, &plan_id)?;

                // Resolve a pending "select this new item" request now that rows are loaded.
                if let Some(target) = app.pending_select.take() {
                    if let Some(idx) = items.iter().position(|i| i.id == target) {
                        app.editor_sel = idx;
                    }
                }
                clamp(&mut app.editor_sel, items.len());

                terminal.draw(|f| {
                    plans::draw_editor(f, app, &plan, &items);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key()? {
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else {
                        plans::handle_editor_key(app, key, &plan, &items)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Read one key *press*. Returns `None` for releases, resizes, mouse — anything we don't
/// act on (the next frame redraws regardless).
fn read_key() -> Result<Option<KeyEvent>> {
    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => Ok(Some(key)),
        _ => Ok(None),
    }
}

/// Clamp a selection index so it always points at a real row (or 0 when the list empties).
fn clamp(sel: &mut usize, len: usize) {
    if len == 0 {
        *sel = 0;
    } else if *sel >= len {
        *sel = len - 1;
    }
}

// --- Modal input ---------------------------------------------------------------

fn handle_modal_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.modal.as_ref() {
        Some(Modal::Text(_)) => handle_text_key(app, key),
        Some(Modal::Confirm(_)) => handle_confirm_key(app, key),
        None => Ok(()),
    }
}

fn handle_text_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.modal = None,
        KeyCode::Enter => submit_text(app)?,
        KeyCode::Backspace => {
            if let Some(Modal::Text(p)) = &mut app.modal {
                p.buffer.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(Modal::Text(p)) = &mut app.modal {
                p.buffer.push(c);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Apply a submitted text prompt. We `take()` the modal first so it closes exactly once,
/// then act on its kind. Validation failures re-report via `status` instead of mutating.
fn submit_text(app: &mut App) -> Result<()> {
    let Some(Modal::Text(prompt)) = app.modal.take() else {
        return Ok(());
    };
    let text = prompt.buffer.trim().to_string();

    match prompt.kind {
        PromptKind::NewPlan => {
            if text.is_empty() {
                app.status = Some("Plan name can't be empty".into());
                return Ok(());
            }
            let id = ops::create_plan(&app.conn, &text)?;
            app.editor_sel = 0;
            app.screen = Screen::PlanEditor { plan_id: id };
            app.status = Some(format!("Created plan “{text}”"));
        }
        PromptKind::RenamePlan { id } => {
            if !text.is_empty() {
                ops::rename_plan(&app.conn, &id, &text)?;
            }
        }
        PromptKind::ItemLabel { id } => {
            if !text.is_empty() {
                ops::set_item_label(&app.conn, &id, &text)?;
            }
        }
        PromptKind::ItemAmount { id } => match Money::parse_dollars(&text) {
            Some(amount) => ops::set_item_amount(&app.conn, &id, amount)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::StampMonth { plan_id } => stamp_from_input(app, &plan_id, &text)?,
    }
    Ok(())
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            if let Some(Modal::Confirm(confirm)) = app.modal.take() {
                match confirm.action {
                    ConfirmAction::DeletePlan { id } => {
                        ops::delete_plan(&mut app.conn, &id)?;
                        app.status = Some("Plan deleted".into());
                    }
                    ConfirmAction::DeleteItem { id } => {
                        ops::delete_plan_item(&app.conn, &id)?;
                        app.status = Some("Item deleted".into());
                    }
                }
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.modal = None,
        _ => {}
    }
    Ok(())
}

/// Parse a `YYYY-MM` month, validate it isn't already stamped, and stamp the plan onto it.
fn stamp_from_input(app: &mut App, plan_id: &str, input: &str) -> Result<()> {
    let Some((year, month)) = parse_year_month(input) else {
        app.status = Some(format!("Enter a month as YYYY-MM (got “{input}”)"));
        return Ok(());
    };
    let label = format!("{year:04}-{month:02}");
    if queries::month_label_exists(&app.conn, &label)? {
        app.status = Some(format!("{label} is already stamped"));
        return Ok(());
    }
    let start = NaiveDate::from_ymd_opt(year, month, 1).expect("validated y-m");
    let days = ops::days_in_month(year, month);
    ops::stamp(&mut app.conn, plan_id, &label, start, days)?;
    app.dash_sel = 0;
    app.screen = Screen::Dashboard;
    app.status = Some(format!("Stamped {label}"));
    Ok(())
}

fn parse_year_month(input: &str) -> Option<(i32, u32)> {
    let (y, m) = input.trim().split_once('-')?;
    let year: i32 = y.parse().ok()?;
    let month: u32 = m.parse().ok()?;
    if (1..=12).contains(&month) { Some((year, month)) } else { None }
}

/// The default month to suggest when stamping: the month after the latest stamped one,
/// or the current calendar month on a fresh database.
pub(crate) fn suggested_stamp_label(conn: &Connection, today: NaiveDate) -> Result<String> {
    let (base_year, base_month) = match queries::current_month(conn)? {
        Some(m) => {
            // Advance one month past the latest.
            if m.start_date.month() == 12 {
                (m.start_date.year() + 1, 1)
            } else {
                (m.start_date.year(), m.start_date.month() + 1)
            }
        }
        None => (today.year(), today.month()),
    };
    Ok(format!("{base_year:04}-{base_month:02}"))
}

// --- Modal rendering -----------------------------------------------------------

/// Draw the open modal (if any) as a centered popup over the current screen.
fn draw_modal(frame: &mut Frame, app: &App) {
    let Some(modal) = &app.modal else { return };
    let area = centered_rect(60, 20, frame.area());
    frame.render_widget(Clear, area); // erase whatever's underneath so the box is opaque

    match modal {
        Modal::Text(prompt) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", prompt.title));
            let body = vec![
                Line::raw(""),
                Line::from(vec![
                    Span::raw(" > "),
                    Span::raw(&prompt.buffer),
                    Span::styled("▏", Style::default().fg(Color::Cyan)), // fake cursor
                ]),
                Line::raw(""),
                Line::from(Span::styled(
                    " Enter to confirm · Esc to cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(Paragraph::new(body).block(block), area);
        }
        Modal::Confirm(confirm) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(" Confirm ");
            let body = vec![
                Line::raw(""),
                Line::from(Span::raw(format!(" {}", confirm.title))),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(" [y] ", Style::default().fg(Color::Black).bg(Color::Red)),
                    Span::raw(" yes    "),
                    Span::styled(" [n] ", Style::default().fg(Color::Black).bg(Color::Gray)),
                    Span::raw(" no"),
                ]),
            ];
            frame.render_widget(Paragraph::new(body).block(block), area);
        }
    }
}

/// Compute a centered rectangle `percent_x` × `percent_y` of `area`. `Flex::Center` does
/// the centering along each axis.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [h] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(h);
    v
}

// --- Shared display helpers (used by more than one screen) ---------------------

/// A footer that shows key hints on the left and the transient `status` on the right.
pub(crate) fn draw_status_footer(frame: &mut Frame, area: Rect, hints: Line, status: &Option<String>) {
    frame.render_widget(Paragraph::new(hints), area);
    if let Some(s) = status {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("{s} "),
            Style::default().fg(Color::Yellow),
        )))
        .alignment(Alignment::Right);
        frame.render_widget(p, area);
    }
}

/// Truncate a label to `max` chars with an ellipsis so columns don't overflow.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Format a `Money` as an editable plain number ("1500.00") to prefill the amount prompt.
pub(crate) fn amount_edit_string(m: Money) -> String {
    format!("{:.2}", m.cents() as f64 / 100.0)
}
