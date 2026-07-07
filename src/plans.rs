//! The plans screens: a list of templates, and an editor for one plan's items.
//!
//! Editing model (the "create then edit in place" pattern): pressing `t`/`e` inserts a
//! new item with defaults and jumps the selection onto it; you then refine its fields
//! with single-key actions (`r` label, `a` amount, `d`/`m`/`p` cycle the coded fields).
//! This avoids a big multi-field entry form — every edit is one focused prompt or toggle.

use crate::{App, ConfirmAction, PromptKind, Screen};
use anyhow::Result;
use ballpark::models::{Direction, Kind, Mode, Plan, PlanEntry, PeriodType, Series};
use ballpark::ops;
use ballpark::queries::PlanSummary;
use chrono::Local;
use std::collections::HashSet;
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

pub fn handle_editor_key(app: &mut App, key: KeyEvent, plan: &Plan, entries: &[PlanEntry]) -> Result<()> {
    app.status = None;
    let selected = entries.get(app.editor_sel);

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.screen = Screen::Plans,
        KeyCode::Char('j') | KeyCode::Down => {
            if !entries.is_empty() && app.editor_sel + 1 < entries.len() {
                app.editor_sel += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.editor_sel = app.editor_sel.saturating_sub(1),

        // Add a NEW series + this plan entry, then jump onto it once it reloads.
        KeyCode::Char('t') => {
            let id = ops::add_new_transaction(&app.conn, &plan.id)?;
            app.pending_select = Some(id);
            app.status = Some("Added a bill — set its label (r) and amount (a)".into());
        }
        KeyCode::Char('e') => {
            let id = ops::add_new_envelope(&app.conn, &plan.id)?;
            app.pending_select = Some(id);
            app.status = Some("Added an envelope — set its label (r) and amount (a)".into());
        }
        // Insert an EXISTING series (the reuse picker).
        KeyCode::Char('i') => {
            app.picker_sel = 0;
            app.screen = Screen::SeriesPicker { plan_id: plan.id.clone() };
        }

        // `r` edits the shared series label (affects every plan); `a` edits this plan's amount.
        KeyCode::Char('r') => {
            if let Some(en) = selected {
                app.open_text(
                    "Series label (shared across plans)",
                    en.series.label.clone(),
                    PromptKind::SeriesLabel { series_id: en.series.id.clone() },
                );
            }
        }
        KeyCode::Char('a') => {
            if let Some(en) = selected {
                app.open_text(
                    "Amount for this plan (dollars)",
                    crate::amount_edit_string(en.amount),
                    PromptKind::ItemAmount { id: en.item_id.clone() },
                );
            }
        }

        // Cycle the coded fields on the SERIES (affects every plan that uses it).
        KeyCode::Char('d') => {
            if let Some(en) = selected {
                if en.series.kind == Kind::Transaction {
                    let next = match en.series.direction {
                        Some(Direction::Out) | None => Direction::In,
                        Some(Direction::In) => Direction::Out,
                    };
                    ops::set_series_direction(&app.conn, &en.series.id, next)?;
                    app.status = Some("Direction changed (affects all plans using this series)".into());
                } else {
                    app.status = Some("Direction applies to transactions, not envelopes".into());
                }
            }
        }
        KeyCode::Char('m') => {
            if let Some(en) = selected {
                if en.series.kind == Kind::Envelope {
                    // None (inherit) -> automatic -> manual -> None
                    let next = match en.series.mode {
                        None => Some(Mode::Automatic),
                        Some(Mode::Automatic) => Some(Mode::Manual),
                        Some(Mode::Manual) => None,
                    };
                    ops::set_series_mode(&app.conn, &en.series.id, next)?;
                    app.status = Some("Mode changed (affects all plans using this series)".into());
                } else {
                    app.status = Some("Mode applies to envelopes, not transactions".into());
                }
            }
        }
        KeyCode::Char('p') => {
            if let Some(en) = selected {
                if en.series.kind == Kind::Envelope {
                    let next = match en.series.period_type {
                        Some(PeriodType::Daily) => PeriodType::Weekly,
                        Some(PeriodType::Weekly) => PeriodType::Monthly,
                        Some(PeriodType::Monthly) | None => PeriodType::Daily,
                    };
                    ops::set_series_period(&app.conn, &en.series.id, next)?;
                    app.status = Some("Period changed (affects all plans using this series)".into());
                } else {
                    app.status = Some("Period applies to envelopes, not transactions".into());
                }
            }
        }

        // `x` removes the item from THIS plan; the series survives for other plans.
        KeyCode::Char('x') => {
            if let Some(en) = selected {
                app.open_confirm(
                    format!("Remove “{}” from this plan? (the series is kept)", en.series.label),
                    ConfirmAction::DeleteItem { id: en.item_id.clone() },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn draw_editor(frame: &mut Frame, app: &App, plan: &Plan, entries: &[PlanEntry]) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
            .areas(frame.area());

    let title = format!(" {} — {} item{} ", plan.name, entries.len(), if entries.len() == 1 { "" } else { "s" });
    let header_p = Paragraph::new(Line::from(title.bold()))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header_p, header);

    if entries.is_empty() {
        let p = Paragraph::new("Empty plan — t: new bill  e: new envelope  i: insert existing series.")
            .block(Block::default().borders(Borders::ALL).title(" Items "));
        frame.render_widget(p, body);
    } else {
        let rows: Vec<ListItem> = entries.iter().map(entry_row).collect();
        let mut state = ListState::default();
        state.select(Some(app.editor_sel));
        let list = List::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" Items "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▌");
        frame.render_stateful_widget(list, body, &mut state);
    }

    let hints = Line::from(vec![
        key(" t/e "), Span::raw(" new  "),
        key(" i "), Span::raw(" insert  "),
        key(" r "), Span::raw(" label  "),
        key(" a "), Span::raw(" amount  "),
        key(" d/m/p "), Span::raw(" cycle  "),
        key(" x "), Span::raw(" remove  "),
        key(" Esc "), Span::raw(" back"),
    ]);
    crate::draw_status_footer(frame, footer, hints, &app.status);
}

/// Render one plan entry: a kind tag, the series label, the detail column (direction for
/// transactions; period + mode for envelopes), and this plan's amount.
fn entry_row(entry: &PlanEntry) -> ListItem<'static> {
    let s = &entry.series;
    let (tag, tag_color, detail) = match s.kind {
        Kind::Transaction => {
            let dir = match s.direction {
                Some(Direction::In) => "in",
                _ => "out",
            };
            ("T", Color::Cyan, format!("{:<16}", dir))
        }
        Kind::Envelope => {
            let period = match s.period_type {
                Some(PeriodType::Daily) => "daily",
                Some(PeriodType::Weekly) => "weekly",
                _ => "monthly",
            };
            let mode = match s.mode {
                None => "inherit",
                Some(Mode::Automatic) => "auto",
                Some(Mode::Manual) => "manual",
            };
            ("E", Color::Magenta, format!("{:<8}{:<8}", period, mode))
        }
    };

    let line = Line::from(vec![
        Span::styled(format!("{} ", tag), Style::default().fg(tag_color).add_modifier(Modifier::BOLD)),
        Span::raw(format!("{:<20}", crate::truncate(&s.label, 20))),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{:>12}", entry.amount.to_string())),
    ]);
    ListItem::new(line)
}

// --- Series picker (reuse an existing series in this plan) ---------------------

pub fn handle_series_picker_key(
    app: &mut App,
    key: KeyEvent,
    plan_id: &str,
    all: &[Series],
    in_plan: &HashSet<String>,
) -> Result<()> {
    app.status = None;
    let selected = all.get(app.picker_sel);

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.screen = Screen::PlanEditor { plan_id: plan_id.to_string() };
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if !all.is_empty() && app.picker_sel + 1 < all.len() {
                app.picker_sel += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.picker_sel = app.picker_sel.saturating_sub(1),
        KeyCode::Enter => {
            if let Some(s) = selected {
                if in_plan.contains(&s.id) {
                    app.status = Some(format!("“{}” is already in this plan", s.label));
                } else {
                    let item_id = ops::add_existing_series(&app.conn, plan_id, &s.id)?;
                    app.pending_select = Some(item_id);
                    app.screen = Screen::PlanEditor { plan_id: plan_id.to_string() };
                    app.status = Some(format!("Added “{}” — set its amount (a)", s.label));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn draw_series_picker(frame: &mut Frame, app: &App, all: &[Series], in_plan: &HashSet<String>) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
            .areas(frame.area());

    let title = Paragraph::new(Line::from(" Insert an existing series ".bold()))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, header);

    if all.is_empty() {
        let p = Paragraph::new("No series exist yet. Go back and create one with t or e.")
            .block(Block::default().borders(Borders::ALL).title(" Series "));
        frame.render_widget(p, body);
    } else {
        let rows: Vec<ListItem> = all
            .iter()
            .map(|s| {
                let (tag, tag_color) = match s.kind {
                    Kind::Transaction => ("T", Color::Cyan),
                    Kind::Envelope => ("E", Color::Magenta),
                };
                let already = in_plan.contains(&s.id);
                let mut spans = vec![
                    Span::styled(format!("{} ", tag), Style::default().fg(tag_color).add_modifier(Modifier::BOLD)),
                    Span::raw(format!("{:<24}", crate::truncate(&s.label, 24))),
                ];
                if already {
                    spans.push(Span::styled("(in plan)", Style::default().fg(Color::DarkGray)));
                }
                // Dim rows already in the plan so it's clear they'd be a no-op.
                let style = if already { Style::default().fg(Color::DarkGray) } else { Style::default() };
                ListItem::new(Line::from(spans)).style(style)
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(app.picker_sel));
        let list = List::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" Series "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▌");
        frame.render_stateful_widget(list, body, &mut state);
    }

    let hints = Line::from(vec![
        key(" j/k "), Span::raw(" move  "),
        key(" Enter "), Span::raw(" add to plan  "),
        key(" Esc "), Span::raw(" back"),
    ]);
    crate::draw_status_footer(frame, footer, hints, &app.status);
}

/// A small pill-styled key hint, e.g. ` n `.
fn key(label: &str) -> Span<'static> {
    Span::styled(label.to_string(), Style::default().fg(Color::Black).bg(Color::Gray))
}
