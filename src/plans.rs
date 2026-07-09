//! The plans screens: a list of templates, and an editor for one plan's items.
//!
//! Editing model: focus Income, Expenses, or Envelopes, press `n` to search/create a
//! series in that block, then fill the plan amount. The editor only touches plan-scoped
//! things — `a` sets this plan's amount, `x` removes the item from this plan. Editing the
//! shared series itself (label, mode, period) lives on the Series page (`S`), so a
//! plan can never silently rewrite a definition used by other plans.

use crate::{AddDestination, App, BudgetBlock, ConfirmAction, PlanFocus, PromptKind, Screen};
use anyhow::Result;
use ballpark::models::{Direction, Kind, Mode, PeriodType, Plan, PlanEntry};
use ballpark::queries::PlanSummary;
use chrono::Local;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph};

// --- Plans list ----------------------------------------------------------------

pub fn handle_list_key(app: &mut App, key: KeyEvent, summaries: &[PlanSummary]) -> Result<()> {
    app.status = None;
    let selected = summaries.get(app.plans_sel);

    match key.code {
        // `q` (quit) and `S` (jump to Series) are handled globally; `Esc` goes back to the
        // Dashboard/month view.
        KeyCode::Esc => app.screen = Screen::Dashboard,
        KeyCode::Char('j') | KeyCode::Down => {
            if !summaries.is_empty() && app.plans_sel + 1 < summaries.len() {
                app.plans_sel += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.plans_sel = app.plans_sel.saturating_sub(1),

        KeyCode::Char('n') => app.open_text("New plan name", "", PromptKind::NewPlan),

        KeyCode::Enter => {
            if let Some(s) = selected {
                app.plan_focus = PlanFocus::Income;
                app.editor_income_sel = 0;
                app.editor_expense_sel = 0;
                app.editor_env_sel = 0;
                app.screen = Screen::PlanEditor {
                    plan_id: s.plan.id.clone(),
                };
            }
        }
        KeyCode::Char('r') => {
            if let Some(s) = selected {
                app.open_text_replace_on_type(
                    "Rename plan",
                    s.plan.name.clone(),
                    PromptKind::RenamePlan {
                        id: s.plan.id.clone(),
                    },
                );
            }
        }
        KeyCode::Char('x') => {
            if let Some(s) = selected {
                app.open_confirm(
                    format!("Delete plan “{}”? (stamped months are kept)", s.plan.name),
                    ConfirmAction::DeletePlan {
                        id: s.plan.id.clone(),
                    },
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
                    PromptKind::StampMonth {
                        plan_id: s.plan.id.clone(),
                    },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn draw_list(frame: &mut Frame, app: &App, summaries: &[PlanSummary]) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let title = Paragraph::new(Line::from(" Plans ".bold()))
        .alignment(Alignment::Center)
        .block(crate::bordered_block());
    frame.render_widget(title, header);

    if summaries.is_empty() {
        let p = Paragraph::new("No plans yet — press n to create one.")
            .block(crate::titled_block(" Templates "));
        frame.render_widget(p, body);
    } else {
        let items: Vec<ListItem> = summaries
            .iter()
            .map(|s| {
                let count = format!(
                    "{} item{}",
                    s.item_count,
                    if s.item_count == 1 { "" } else { "s" }
                );
                let line = Line::from(vec![
                    Span::raw(format!("{:<28}", crate::truncate(&s.plan.name, 28))),
                    Span::styled(count, Style::default().fg(Color::DarkGray)),
                ]);
                ListItem::new(line)
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(app.plans_sel));

        let list =
            crate::selectable_list(items).block(crate::selectable_block(" Templates ", false));
        frame.render_stateful_widget(list, body, &mut state);
    }

    let hints = Line::from(vec![
        key(" n "),
        Span::raw(" new  "),
        key(" Enter "),
        Span::raw(" edit  "),
        key(" r "),
        Span::raw(" rename  "),
        key(" s "),
        Span::raw(" stamp  "),
        key(" x "),
        Span::raw(" delete"),
    ]);
    let nav_hints = Line::from(vec![
        key(" S "),
        Span::raw(" series  "),
        key(" Esc "),
        Span::raw(" back  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    crate::draw_split_status_footer(frame, footer, hints, nav_hints, &app.status);
}

// --- Plan editor ---------------------------------------------------------------

pub fn handle_editor_key(
    app: &mut App,
    key: KeyEvent,
    plan: &Plan,
    entries: &[PlanEntry],
) -> Result<()> {
    app.status = None;

    match key.code {
        // `q` quits globally; `Esc` steps back up to the plans list.
        KeyCode::Esc => app.screen = Screen::Plans,
        KeyCode::Tab => {
            app.plan_focus = next_plan_focus(app.plan_focus);
        }
        KeyCode::BackTab => {
            app.plan_focus = previous_plan_focus(app.plan_focus);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let selected = current_plan_selection(app);
            if selected + 1 < entry_count(entries, app.plan_focus) {
                set_plan_selection(app, selected + 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let selected = current_plan_selection(app);
            set_plan_selection(app, selected.saturating_sub(1));
        }

        // Search existing series in this block, or create one from the typed label.
        KeyCode::Char('n') => app.open_series_search(
            AddDestination::Plan {
                plan_id: plan.id.clone(),
            },
            budget_block_for_focus(app.plan_focus),
        )?,

        // The plan editor only changes plan-scoped things: which series are in the plan and
        // this plan's amount for each. Label, mode, and period belong to the *shared* series
        // (they'd change every plan), so those edits live on the Series page. Redirect the
        // old keys there rather than leaving them as silent dead ends.
        KeyCode::Char('r') | KeyCode::Char('m') | KeyCode::Char('p') => {
            app.status = Some("Edit the series itself on the Series page (S)".into());
        }
        KeyCode::Char('a') => {
            if let Some(en) = selected_entry(app, entries) {
                app.open_text_replace_on_type(
                    "Amount for this plan (dollars)",
                    crate::amount_edit_string(en.amount),
                    PromptKind::ItemAmount {
                        id: en.item_id.clone(),
                    },
                );
            }
        }

        // `x` removes the item from THIS plan; the series survives for other plans.
        KeyCode::Char('x') => {
            if let Some(en) = selected_entry(app, entries) {
                app.open_confirm(
                    format!(
                        "Remove “{}” from this plan? (the series is kept)",
                        en.series.label
                    ),
                    ConfirmAction::DeleteItem {
                        id: en.item_id.clone(),
                    },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn next_plan_focus(current: PlanFocus) -> PlanFocus {
    match current {
        PlanFocus::Income => PlanFocus::Expenses,
        PlanFocus::Expenses => PlanFocus::Envelopes,
        PlanFocus::Envelopes => PlanFocus::Income,
    }
}

fn previous_plan_focus(current: PlanFocus) -> PlanFocus {
    match current {
        PlanFocus::Income => PlanFocus::Envelopes,
        PlanFocus::Expenses => PlanFocus::Income,
        PlanFocus::Envelopes => PlanFocus::Expenses,
    }
}

fn selected_entry<'e>(app: &App, entries: &'e [PlanEntry]) -> Option<&'e PlanEntry> {
    entries
        .iter()
        .filter(|entry| entry_matches_focus(entry, app.plan_focus))
        .nth(current_plan_selection(app))
}

fn current_plan_selection(app: &App) -> usize {
    match app.plan_focus {
        PlanFocus::Income => app.editor_income_sel,
        PlanFocus::Expenses => app.editor_expense_sel,
        PlanFocus::Envelopes => app.editor_env_sel,
    }
}

fn set_plan_selection(app: &mut App, selected: usize) {
    match app.plan_focus {
        PlanFocus::Income => app.editor_income_sel = selected,
        PlanFocus::Expenses => app.editor_expense_sel = selected,
        PlanFocus::Envelopes => app.editor_env_sel = selected,
    }
}

fn entry_count(entries: &[PlanEntry], focus: PlanFocus) -> usize {
    entries
        .iter()
        .filter(|entry| entry_matches_focus(entry, focus))
        .count()
}

fn entry_matches_focus(entry: &PlanEntry, focus: PlanFocus) -> bool {
    match focus {
        PlanFocus::Income => {
            entry.series.kind == Kind::Transaction && entry.series.direction == Some(Direction::In)
        }
        PlanFocus::Expenses => {
            entry.series.kind == Kind::Transaction && entry.series.direction != Some(Direction::In)
        }
        PlanFocus::Envelopes => entry.series.kind == Kind::Envelope,
    }
}

fn budget_block_for_focus(focus: PlanFocus) -> BudgetBlock {
    match focus {
        PlanFocus::Income => BudgetBlock::Income,
        PlanFocus::Expenses => BudgetBlock::Expenses,
        PlanFocus::Envelopes => BudgetBlock::Envelopes,
    }
}

pub fn draw_editor(frame: &mut Frame, app: &App, plan: &Plan, entries: &[PlanEntry]) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let title = format!(
        " {} — {} item{} ",
        plan.name,
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    let header_p = Paragraph::new(Line::from(title.bold()))
        .alignment(Alignment::Center)
        .block(crate::bordered_block());
    frame.render_widget(header_p, header);

    let [left_items, env_area] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)]).areas(body);
    let [income_area, expense_area] = Layout::vertical([
        Constraint::Length(crate::income_block_height(entry_count(
            entries,
            PlanFocus::Income,
        ))),
        Constraint::Min(0),
    ])
    .areas(left_items);

    draw_plan_block(frame, income_area, app, entries, PlanFocus::Income);
    draw_plan_block(frame, expense_area, app, entries, PlanFocus::Expenses);
    draw_plan_block(frame, env_area, app, entries, PlanFocus::Envelopes);

    // The editor's verbs are the same in every block now that series-definition edits moved
    // to the Series page: add, set this plan's amount, remove from this plan.
    let hints = Line::from(vec![
        key(" Tab "),
        Span::raw(" block  "),
        key(" j/k "),
        Span::raw(" move  "),
        key(" n "),
        Span::raw(" new  "),
        key(" a "),
        Span::raw(" amount  "),
        key(" x "),
        Span::raw(" remove"),
    ]);
    let nav_hints = Line::from(vec![
        key(" S "),
        Span::raw(" series  "),
        key(" Esc "),
        Span::raw(" back  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    crate::draw_split_status_footer(frame, footer, hints, nav_hints, &app.status);
}

fn draw_plan_block(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    entries: &[PlanEntry],
    focus: PlanFocus,
) {
    let rows: Vec<ListItem> = entries
        .iter()
        .filter(|entry| entry_matches_focus(entry, focus))
        .map(|entry| entry_row(entry, focus))
        .collect();

    let focused = app.plan_focus == focus;
    let selected = match focus {
        PlanFocus::Income => app.editor_income_sel,
        PlanFocus::Expenses => app.editor_expense_sel,
        PlanFocus::Envelopes => app.editor_env_sel,
    };

    let mut state = ListState::default();
    if focused && !rows.is_empty() {
        state.select(Some(selected));
    }

    let list = crate::selectable_list(rows)
        .block(crate::selectable_block(plan_block_title(focus), focused));
    frame.render_stateful_widget(list, area, &mut state);
}

fn plan_block_title(focus: PlanFocus) -> &'static str {
    match focus {
        PlanFocus::Income => " Income ",
        PlanFocus::Expenses => " Expenses ",
        PlanFocus::Envelopes => " Envelopes ",
    }
}

/// Render one plan entry inside its logical block. Transaction blocks already imply
/// direction, so envelopes are the only rows that need mode/period details.
fn entry_row(entry: &PlanEntry, focus: PlanFocus) -> ListItem<'static> {
    let s = &entry.series;
    let line = match focus {
        PlanFocus::Income | PlanFocus::Expenses => Line::from(vec![
            Span::raw(format!("{:<24}", crate::truncate(&s.label, 24))),
            Span::raw(format!("{:>12}", entry.amount.to_string())),
        ]),
        PlanFocus::Envelopes => {
            let period = match s.period_type {
                Some(PeriodType::Daily) => "daily",
                Some(PeriodType::Weekly) => "weekly",
                _ => "monthly",
            };
            // Envelope series always carry a concrete mode; None is unreachable here.
            let mode = match s.mode {
                Some(Mode::Manual) => "manual",
                _ => "auto",
            };
            Line::from(vec![
                Span::raw(format!("{:<18}", crate::truncate(&s.label, 18))),
                Span::styled(
                    format!("{:<8}{:<8}", period, mode),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:>12}", entry.amount.to_string())),
            ])
        }
    };

    ListItem::new(line)
}

/// A small pill-styled key hint, e.g. ` n `.
fn key(label: &str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::default().fg(Color::Black).bg(Color::Gray),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ballpark::money::Money;
    use ballpark::ops;
    use ballpark::queries;
    use ratatui::crossterm::event::KeyModifiers;
    use rusqlite::Connection;
    use uuid::Uuid;

    fn open_test_conn() -> Connection {
        let mut path = std::env::temp_dir();
        path.push(format!("ballpark-plans-{}.db", Uuid::new_v4()));
        ballpark::db::open(&path).unwrap()
    }

    fn app_with_transaction_plan() -> (App, Plan, Vec<PlanEntry>, String) {
        let conn = open_test_conn();
        let plan_id = ops::create_plan(&conn, "Normal").unwrap();
        let rent_series_id = ops::create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        ops::add_plan_item(
            &conn,
            &plan_id,
            &rent_series_id,
            Money::from_dollars(1800.0),
        )
        .unwrap();

        let plan = queries::get_plan(&conn, &plan_id).unwrap().unwrap();
        let entries = queries::load_plan_entries(&conn, &plan_id).unwrap();
        let app = App {
            conn,
            screen: Screen::PlanEditor {
                plan_id: plan_id.clone(),
            },
            should_quit: false,
            dash_focus: crate::DashFocus::Income,
            viewed_year: 2026,
            viewed_month: 9,
            dash_income_sel: 0,
            dash_expense_sel: 0,
            dash_env_sel: 0,
            dash_acct_sel: 0,
            plans_sel: 0,
            series_sel: 0,
            series_search: String::new(),
            series_search_active: false,
            series_range: ballpark::view::SeriesTimeRange::Last12Stamped,
            series_filter: crate::SeriesFilter::Both,
            plan_focus: PlanFocus::Expenses,
            editor_income_sel: 0,
            editor_expense_sel: 0,
            editor_env_sel: 0,
            pending_select: None,
            pending_dash_txn: None,
            pending_dash_env: None,
            pending_dash_account: None,
            pending_series_select: None,
            summary_anims: crate::anim::SummaryAnimations::new(),
            frame_now: std::time::Instant::now(),
            modal: None,
            status: None,
        };

        (app, plan, entries, rent_series_id)
    }

    fn direction_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)
    }

    fn rename_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)
    }

    fn backtab_key() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)
    }

    #[test]
    fn backtab_cycles_plan_blocks_backward() {
        let (mut app, plan, entries, _) = app_with_transaction_plan();

        handle_editor_key(&mut app, backtab_key(), &plan, &entries).unwrap();
        assert!(app.plan_focus == PlanFocus::Income);

        handle_editor_key(&mut app, backtab_key(), &plan, &entries).unwrap();
        assert!(app.plan_focus == PlanFocus::Envelopes);
    }

    #[test]
    fn rename_key_no_longer_edits_shared_series_label() {
        // Series-definition edits moved to the Series page; `r` in the plan editor must not
        // open a rename prompt or touch the shared label — it just points the user there.
        let (mut app, plan, entries, rent_series_id) = app_with_transaction_plan();

        handle_editor_key(&mut app, rename_key(), &plan, &entries).unwrap();

        assert!(app.modal.is_none(), "no rename prompt opened");
        assert!(app.status.is_some(), "shows a redirect hint instead");
        let series = queries::get_series(&app.conn, &rent_series_id)
            .unwrap()
            .unwrap();
        assert_eq!(series.label, "Rent", "shared label untouched");
    }

    #[test]
    fn direction_key_does_not_change_transaction_series_direction() {
        let (mut app, plan, entries, rent_series_id) = app_with_transaction_plan();
        let rent = entries
            .iter()
            .find(|entry| entry.series.id == rent_series_id)
            .unwrap();
        assert_eq!(rent.series.direction, Some(Direction::Out));

        handle_editor_key(&mut app, direction_key(), &plan, &entries).unwrap();

        assert!(app.status.is_none());
        assert!(app.pending_select.is_none());
        let refreshed = queries::get_series(&app.conn, &rent_series_id)
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.direction, Some(Direction::Out));
    }
}
