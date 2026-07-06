//! The plans screens: a list of templates, and an editor for one plan's items.
//!
//! Editing model (the "create then edit in place" pattern): pressing `t`/`e` inserts a
//! new item with defaults and jumps the selection onto it; you then refine its fields
//! with single-key actions (`r` label, `a` amount, `d`/`m`/`p` cycle the coded fields).
//! This avoids a big multi-field entry form — every edit is one focused prompt or toggle.

use crate::{App, ConfirmAction, PromptKind, Screen};
use anyhow::Result;
use ballpark::models::{Direction, Kind, Mode, Plan, PlanItem, PeriodType};
use ballpark::ops;
use ballpark::queries::PlanSummary;
use chrono::Local;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

// --- Plans list ----------------------------------------------------------------

pub fn handle_list_key(app: &mut App, key: KeyEvent, summaries: &[PlanSummary]) -> Result<()> {
    app.status = None;
    let selected = summaries.get(app.plans_sel);

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.screen = Screen::Dashboard,
        KeyCode::Char('j') | KeyCode::Down => {
            if !summaries.is_empty() && app.plans_sel + 1 < summaries.len() {
                app.plans_sel += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.plans_sel = app.plans_sel.saturating_sub(1),

        KeyCode::Char('n') => app.open_text("New plan name", "", PromptKind::NewPlan),

        KeyCode::Enter => {
            if let Some(s) = selected {
                app.editor_sel = 0;
                app.screen = Screen::PlanEditor { plan_id: s.plan.id.clone() };
            }
        }
        KeyCode::Char('r') => {
            if let Some(s) = selected {
                app.open_text("Rename plan", s.plan.name.clone(), PromptKind::RenamePlan { id: s.plan.id.clone() });
            }
        }
        KeyCode::Char('x') => {
            if let Some(s) = selected {
                app.open_confirm(
                    format!("Delete plan “{}”? (stamped months are kept)", s.plan.name),
                    ConfirmAction::DeletePlan { id: s.plan.id.clone() },
                );
            }
        }
        KeyCode::Char('s') => {
            if let Some(s) = selected {
                let today = Local::now().date_naive();
                let suggested = crate::suggested_stamp_label(&app.conn, today)?;
                app.open_text(
                    format!("Stamp “{}” onto month (YYYY-MM)", s.plan.name),
                    suggested,
                    PromptKind::StampMonth { plan_id: s.plan.id.clone() },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn draw_list(frame: &mut Frame, app: &App, summaries: &[PlanSummary]) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
            .areas(frame.area());

    let title = Paragraph::new(Line::from(" Plans ".bold()))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, header);

    if summaries.is_empty() {
        let p = Paragraph::new("No plans yet — press n to create one.")
            .block(Block::default().borders(Borders::ALL).title(" Templates "));
        frame.render_widget(p, body);
    } else {
        let items: Vec<ListItem> = summaries
            .iter()
            .map(|s| {
                let count = format!("{} item{}", s.item_count, if s.item_count == 1 { "" } else { "s" });
                let line = Line::from(vec![
                    Span::raw(format!("{:<28}", crate::truncate(&s.plan.name, 28))),
                    Span::styled(count, Style::default().fg(Color::DarkGray)),
                ]);
                ListItem::new(line)
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(app.plans_sel));

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Templates "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▌");
        frame.render_stateful_widget(list, body, &mut state);
    }

    let hints = Line::from(vec![
        key(" n "), Span::raw(" new  "),
        key(" Enter "), Span::raw(" edit  "),
        key(" r "), Span::raw(" rename  "),
        key(" s "), Span::raw(" stamp  "),
        key(" x "), Span::raw(" delete  "),
        key(" Esc "), Span::raw(" back"),
    ]);
    crate::draw_status_footer(frame, footer, hints, &app.status);
}

// --- Plan editor ---------------------------------------------------------------

pub fn handle_editor_key(app: &mut App, key: KeyEvent, plan: &Plan, items: &[PlanItem]) -> Result<()> {
    app.status = None;
    let selected = items.get(app.editor_sel);

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.screen = Screen::Plans,
        KeyCode::Char('j') | KeyCode::Down => {
            if !items.is_empty() && app.editor_sel + 1 < items.len() {
                app.editor_sel += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.editor_sel = app.editor_sel.saturating_sub(1),

        // Add items, then jump the selection onto the new row once it reloads.
        KeyCode::Char('t') => {
            let id = ops::add_transaction_item(&app.conn, &plan.id)?;
            app.pending_select = Some(id);
            app.status = Some("Added a bill — set its label (r) and amount (a)".into());
        }
        KeyCode::Char('e') => {
            let id = ops::add_envelope_item(&app.conn, &plan.id)?;
            app.pending_select = Some(id);
            app.status = Some("Added an envelope — set its label (r) and amount (a)".into());
        }

        KeyCode::Char('r') => {
            if let Some(it) = selected {
                app.open_text("Item label", it.label.clone(), PromptKind::ItemLabel { id: it.id.clone() });
            }
        }
        KeyCode::Char('a') => {
            if let Some(it) = selected {
                app.open_text(
                    "Amount (dollars)",
                    crate::amount_edit_string(it.amount),
                    PromptKind::ItemAmount { id: it.id.clone() },
                );
            }
        }

        // Cycle the coded fields, respecting which apply to which kind.
        KeyCode::Char('d') => {
            if let Some(it) = selected {
                if it.kind == Kind::Transaction {
                    let next = match it.direction {
                        Some(Direction::Out) | None => Direction::In,
                        Some(Direction::In) => Direction::Out,
                    };
                    ops::set_item_direction(&app.conn, &it.id, next)?;
                } else {
                    app.status = Some("Direction applies to transactions, not envelopes".into());
                }
            }
        }
        KeyCode::Char('m') => {
            if let Some(it) = selected {
                if it.kind == Kind::Envelope {
                    // None (inherit) -> automatic -> manual -> None
                    let next = match it.mode {
                        None => Some(Mode::Automatic),
                        Some(Mode::Automatic) => Some(Mode::Manual),
                        Some(Mode::Manual) => None,
                    };
                    ops::set_item_mode(&app.conn, &it.id, next)?;
                } else {
                    app.status = Some("Mode applies to envelopes, not transactions".into());
                }
            }
        }
        KeyCode::Char('p') => {
            if let Some(it) = selected {
                if it.kind == Kind::Envelope {
                    let next = match it.period_type {
                        Some(PeriodType::Daily) => PeriodType::Weekly,
                        Some(PeriodType::Weekly) => PeriodType::Monthly,
                        Some(PeriodType::Monthly) | None => PeriodType::Daily,
                    };
                    ops::set_item_period(&app.conn, &it.id, next)?;
                } else {
                    app.status = Some("Period applies to envelopes, not transactions".into());
                }
            }
        }

        KeyCode::Char('x') => {
            if let Some(it) = selected {
                app.open_confirm(
                    format!("Delete “{}” from this plan?", it.label),
                    ConfirmAction::DeleteItem { id: it.id.clone() },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn draw_editor(frame: &mut Frame, app: &App, plan: &Plan, items: &[PlanItem]) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
            .areas(frame.area());

    let title = format!(" {} — {} item{} ", plan.name, items.len(), if items.len() == 1 { "" } else { "s" });
    let header_p = Paragraph::new(Line::from(title.bold()))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header_p, header);

    if items.is_empty() {
        let p = Paragraph::new("Empty plan — press t for a bill/paycheck, e for an envelope.")
            .block(Block::default().borders(Borders::ALL).title(" Items "));
        frame.render_widget(p, body);
    } else {
        let rows: Vec<ListItem> = items.iter().map(item_row).collect();
        let mut state = ListState::default();
        state.select(Some(app.editor_sel));
        let list = List::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" Items "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▌");
        frame.render_stateful_widget(list, body, &mut state);
    }

    let hints = Line::from(vec![
        key(" t/e "), Span::raw(" add  "),
        key(" r "), Span::raw(" label  "),
        key(" a "), Span::raw(" amount  "),
        key(" d/m/p "), Span::raw(" cycle  "),
        key(" x "), Span::raw(" del  "),
        key(" Esc "), Span::raw(" back"),
    ]);
    crate::draw_status_footer(frame, footer, hints, &app.status);
}

/// Render one plan item as a row: a kind tag, the label, its detail column (direction for
/// transactions; period + mode for envelopes), and the amount.
fn item_row(item: &PlanItem) -> ListItem<'static> {
    let (tag, tag_color, detail) = match item.kind {
        Kind::Transaction => {
            let dir = match item.direction {
                Some(Direction::In) => "in",
                _ => "out",
            };
            ("T", Color::Cyan, format!("{:<16}", dir))
        }
        Kind::Envelope => {
            let period = match item.period_type {
                Some(PeriodType::Daily) => "daily",
                Some(PeriodType::Weekly) => "weekly",
                _ => "monthly",
            };
            let mode = match item.mode {
                None => "inherit",
                Some(Mode::Automatic) => "auto",
                Some(Mode::Manual) => "manual",
            };
            ("E", Color::Magenta, format!("{:<8}{:<8}", period, mode))
        }
    };

    let line = Line::from(vec![
        Span::styled(format!("{} ", tag), Style::default().fg(tag_color).add_modifier(Modifier::BOLD)),
        Span::raw(format!("{:<20}", crate::truncate(&item.label, 20))),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{:>12}", item.amount.to_string())),
    ]);
    ListItem::new(line)
}

/// A small pill-styled key hint, e.g. ` n `.
fn key(label: &str) -> Span<'static> {
    Span::styled(label.to_string(), Style::default().fg(Color::Black).bg(Color::Gray))
}
