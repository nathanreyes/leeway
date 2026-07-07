//! The dashboard screen: the "what's left" daily loop.
//!
//! Two public entry points, mirroring every screen module: `draw` renders a frame from
//! read-only data, and `handle_key` mutates `App` (and the database) in response to a key.

use crate::{App, DashFocus, PromptKind, Screen};
use anyhow::Result;
use ballpark::models::{AccountType, Direction, Mode};
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
    let Some(view) = view else {
        // No month: only global keys work.
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
            KeyCode::Char('P') => {
                app.plans_sel = 0;
                app.screen = Screen::Plans;
            }
            _ => {}
        }
        return Ok(());
    };

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('P') => {
            app.plans_sel = 0;
            app.screen = Screen::Plans;
        }

        // Tab flips which panel the cursor keys and Enter act on.
        KeyCode::Tab => {
            app.dash_focus = match app.dash_focus {
                DashFocus::Transactions => DashFocus::Accounts,
                DashFocus::Accounts => DashFocus::Transactions,
            };
        }

        // j/k move within the *focused* list.
        KeyCode::Char('j') | KeyCode::Down => match app.dash_focus {
            DashFocus::Transactions => {
                if app.dash_sel + 1 < view.standalone.len() {
                    app.dash_sel += 1;
                }
            }
            DashFocus::Accounts => {
                if app.dash_acct_sel + 1 < view.accounts.len() {
                    app.dash_acct_sel += 1;
                }
            }
        },
        KeyCode::Char('k') | KeyCode::Up => match app.dash_focus {
            DashFocus::Transactions => app.dash_sel = app.dash_sel.saturating_sub(1),
            DashFocus::Accounts => app.dash_acct_sel = app.dash_acct_sel.saturating_sub(1),
        },

        // Enter / Space = the focused panel's primary action.
        KeyCode::Enter | KeyCode::Char(' ') => act_on_focus(app, view)?,

        // Explicit single-letter shortcuts, each valid for one panel.
        KeyCode::Char('p') => {
            if app.dash_focus == DashFocus::Transactions {
                act_on_focus(app, view)?;
            }
        }
        KeyCode::Char('e') => {
            if app.dash_focus == DashFocus::Accounts {
                act_on_focus(app, view)?;
            }
        }
        // Edit a credit card's limit (rarely changed, so it gets its own key).
        KeyCode::Char('l') => {
            if app.dash_focus == DashFocus::Accounts {
                if let Some(acct) = view.accounts.get(app.dash_acct_sel) {
                    if acct.account_type == AccountType::CreditCard {
                        app.open_text(
                            format!("Credit limit for {}", acct.name),
                            crate::amount_edit_string(acct.credit_limit.unwrap_or(Money::ZERO)),
                            PromptKind::CardLimit { id: acct.id.clone() },
                        );
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Do the focused panel's action: settle a bill, or open the balance-edit prompt.
fn act_on_focus(app: &mut App, view: &MonthView) -> Result<()> {
    match app.dash_focus {
        DashFocus::Transactions => {
            if let Some(txn) = view.standalone.get(app.dash_sel) {
                ops::toggle_settled(&app.conn, &txn.id, txn.settled)?;
            }
        }
        DashFocus::Accounts => {
            if let Some(acct) = view.accounts.get(app.dash_acct_sel) {
                match acct.account_type {
                    // Checking: edit the spendable balance.
                    AccountType::Checking => app.open_text(
                        format!("New balance for {}", acct.name),
                        crate::amount_edit_string(acct.balance),
                        PromptKind::AccountBalance { id: acct.id.clone() },
                    ),
                    // Credit card: the primary edit is available credit (owed is derived);
                    // the limit gets its own key (`l`).
                    AccountType::CreditCard => app.open_text(
                        format!("Available credit for {}", acct.name),
                        crate::amount_edit_string(acct.available_credit.unwrap_or(Money::ZERO)),
                        PromptKind::CardAvailable { id: acct.id.clone() },
                    ),
                }
            }
        }
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

    // Three columns now: accounts (editable), bills, envelopes.
    let [acct_area, txn_area, env_area] = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(38),
        Constraint::Percentage(32),
    ])
    .areas(body);

    draw_header(frame, header, view);
    draw_whats_left(frame, whats_left, view);
    draw_accounts(frame, acct_area, app, view);
    draw_transactions(frame, txn_area, app, view);
    draw_envelopes(frame, env_area, view);

    // Footer hints adapt to the focused panel so they always name what the keys do.
    let hints = match app.dash_focus {
        DashFocus::Transactions => Line::from(vec![
            key(" Tab "), Span::raw(" panel  "),
            key(" j/k "), Span::raw(" move  "),
            key(" Enter "), Span::raw(" toggle paid  "),
            key(" P "), Span::raw(" plans  "),
            key(" q "), Span::raw(" quit"),
        ]),
        DashFocus::Accounts => Line::from(vec![
            key(" Tab "), Span::raw(" panel  "),
            key(" j/k "), Span::raw(" move  "),
            key(" Enter "), Span::raw(" edit  "),
            key(" l "), Span::raw(" card limit  "),
            key(" P "), Span::raw(" plans  "),
            key(" q "), Span::raw(" quit"),
        ]),
    };
    crate::draw_status_footer(frame, footer, hints, &app.status);
}

/// The accounts panel. Checking shows its balance; a credit card shows what's owed plus a
/// dim "avail / limit" detail line. Highlighted only when this panel has focus.
fn draw_accounts(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let items: Vec<ListItem> = view
        .accounts
        .iter()
        .map(|a| match a.account_type {
            AccountType::Checking => {
                let color = if a.balance.cents() < 0 { Color::Red } else { Color::Green };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:<13}", crate::truncate(&a.name, 13))),
                    Span::styled(format!("{:>10}", a.balance.to_string()), Style::default().fg(color)),
                ]))
            }
            AccountType::CreditCard => {
                let owed = a.owed();
                // Owed > 0 is a debt (red); ≤ 0 is a statement credit in your favor (green).
                let owed_color = if owed.cents() > 0 { Color::Red } else { Color::Green };
                let line1 = Line::from(vec![
                    Span::raw(format!("{:<13}", crate::truncate(&a.name, 13))),
                    Span::styled(format!("owed {}", owed), Style::default().fg(owed_color)),
                ]);
                let line2 = Line::from(Span::styled(
                    format!(
                        "  avail {} / limit {}",
                        a.available_credit.unwrap_or(Money::ZERO),
                        a.credit_limit.unwrap_or(Money::ZERO)
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
                ListItem::new(vec![line1, line2])
            }
        })
        .collect();

    let focused = app.dash_focus == DashFocus::Accounts;
    let mut state = ListState::default();
    // Only show a highlight on the focused panel, so it's obvious which list is live.
    state.select(if focused { Some(app.dash_acct_sel) } else { None });

    let list = List::new(items)
        .block(panel_block(" Accounts ", focused))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, &mut state);
}

/// A bordered block whose border turns cyan when its panel is focused.
fn panel_block(title: &str, focused: bool) -> Block<'_> {
    let block = Block::default().borders(Borders::ALL).title(title.to_string());
    if focused {
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block
    }
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
            Span::styled(wl.card_debt.to_string(), Style::default().fg(Color::Red)),
            Span::raw(" card debt"),
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

    let focused = app.dash_focus == DashFocus::Transactions;
    let mut state = ListState::default();
    if focused && !view.standalone.is_empty() {
        state.select(Some(app.dash_sel));
    }

    let list = List::new(items)
        .block(panel_block(" Income & Bills ", focused))
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
