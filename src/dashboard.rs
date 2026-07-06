//! The dashboard screen: the "what's left" daily loop.
//!
//! Two public entry points, mirroring every screen module: `draw` renders a frame from
//! read-only data, and `handle_key` mutates `App` (and the database) in response to a key.

use crate::{App, Screen};
use anyhow::Result;
use ballpark::models::{Direction, Mode};
use ballpark::money::Money;
use ballpark::ops;
use ballpark::view::MonthView;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

pub fn handle_key(app: &mut App, key: KeyEvent, view: &Option<MonthView>) -> Result<()> {
    // Clear any leftover status the moment the user acts again.
    app.status = None;

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('P') => {
            app.plans_sel = 0;
            app.screen = Screen::Plans;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(v) = view {
                if !v.standalone.is_empty() && app.dash_sel + 1 < v.standalone.len() {
                    app.dash_sel += 1;
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.dash_sel = app.dash_sel.saturating_sub(1),
        KeyCode::Char('p') | KeyCode::Char(' ') | KeyCode::Enter => {
            if let Some(v) = view {
                if let Some(txn) = v.standalone.get(app.dash_sel) {
                    ops::toggle_settled(&app.conn, &txn.id, txn.settled)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn draw(frame: &mut Frame, app: &App, view: &Option<MonthView>) {
    let Some(view) = view else {
        let p = Paragraph::new("No month stamped yet. Press P to open Plans and stamp one.")
            .block(Block::default().borders(Borders::ALL).title(" Ballpark "));
        frame.render_widget(p, frame.area());
        return;
    };

    let [header, whats_left, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(6),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [txn_area, env_area] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);

    draw_header(frame, header, view);
    draw_whats_left(frame, whats_left, view);
    draw_transactions(frame, txn_area, app, view);
    draw_envelopes(frame, env_area, view);

    let hints = Line::from(vec![
        key(" j/k "), Span::raw(" move  "),
        key(" p "), Span::raw(" toggle paid  "),
        key(" P "), Span::raw(" plans  "),
        key(" q "), Span::raw(" quit"),
    ]);
    crate::draw_status_footer(frame, footer, hints, &app.status);
}

fn draw_header(frame: &mut Frame, area: Rect, view: &MonthView) {
    let title = format!(
        " Ballpark — {}   (day {} of {}) ",
        view.month.label, view.days_elapsed, view.month.days_in_month
    );
    let p = Paragraph::new(Line::from(title.bold()))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(p, area);
}

fn draw_whats_left(frame: &mut Frame, area: Rect, view: &MonthView) {
    let wl = &view.whats_left;
    let headline_color = if wl.whats_left.cents() >= 0 { Color::Green } else { Color::Red };

    let lines = vec![
        Line::from(vec![
            Span::raw("What's left:  "),
            Span::styled(
                wl.whats_left.to_string(),
                Style::default().fg(headline_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled(format!("{:>12}", wl.funds_available.to_string()), Style::default().fg(Color::Cyan)),
            Span::raw("  funds"),
            Span::raw("   − "),
            Span::styled(wl.protected.to_string(), Style::default().fg(Color::Yellow)),
            Span::raw(" protected"),
        ]),
        Line::from(vec![
            Span::raw("   + "),
            Span::styled(wl.income_remaining.to_string(), Style::default().fg(Color::Green)),
            Span::raw(" income left    − "),
            Span::styled(wl.bills_remaining.to_string(), Style::default().fg(Color::Red)),
            Span::raw(" bills left    − "),
            Span::styled(wl.envelopes_remaining.to_string(), Style::default().fg(Color::Magenta)),
            Span::raw(" envelopes"),
        ]),
    ];

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" What's left "));
    frame.render_widget(p, area);
}

fn draw_transactions(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let items: Vec<ListItem> = view
        .standalone
        .iter()
        .map(|t| {
            let check = if t.settled { "[x]" } else { "[ ]" };
            let sign = match t.direction {
                Direction::In => "+",
                Direction::Out => "−",
            };
            let amount = format!("{}{}", sign, t.amount);
            let style = if t.settled {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::raw(format!("{} ", check)),
                Span::raw(format!("{:<20}", crate::truncate(&t.label, 20))),
                Span::styled(format!("{:>12}", amount), style),
            ]);
            ListItem::new(line).style(style)
        })
        .collect();

    let mut state = ListState::default();
    if !view.standalone.is_empty() {
        state.select(Some(app.dash_sel));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Income & Bills "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_envelopes(frame: &mut Frame, area: Rect, view: &MonthView) {
    let items: Vec<ListItem> = view
        .envelopes
        .iter()
        .map(|e| {
            let mode = match e.effective_mode {
                Mode::Automatic => "auto",
                Mode::Manual => "man ",
            };
            let meter = meter_bar(e.consumed, e.envelope.amount, 10);
            let line = Line::from(vec![
                Span::raw(format!("{:<14}", crate::truncate(&e.envelope.label, 14))),
                Span::styled(format!("{} ", mode), Style::default().fg(Color::DarkGray)),
                Span::styled(meter, Style::default().fg(Color::Magenta)),
                Span::raw(format!("  {:>10} left", e.remaining.to_string())),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Envelopes "));
    frame.render_widget(list, area);
}

/// A `██████░░░░`-style bar showing `consumed / total`, `width` chars wide.
fn meter_bar(consumed: Money, total: Money, width: usize) -> String {
    if total.cents() <= 0 {
        return "░".repeat(width);
    }
    let frac = (consumed.cents() as f64 / total.cents() as f64).clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let mut s = "█".repeat(filled);
    s.push_str(&"░".repeat(width.saturating_sub(filled)));
    s
}

/// A small pill-styled key hint, e.g. ` j/k `.
fn key(label: &str) -> Span<'_> {
    Span::styled(label.to_string(), Style::default().fg(Color::Black).bg(Color::Gray))
}
