//! The unified Plans screen: a master list of plan templates on the left, the selected
//! plan's item sublists (Income, Expenses, Envelopes) stacked on the right, and the plan's
//! cash-flow Summary across the bottom.
//!
//! Editing model: focus the plan list to act on whole plans — `n` new, `l` label, `s` stamp,
//! `x` delete. Focus an item pane to act on that block's items — `n` searches/creates a
//! series and fills the plan amount, `a` sets this plan's amount, `x` removes the item from
//! this plan. The screen only touches plan-scoped things; editing the shared series itself
//! (label, mode, period) lives on the Series page (`S`), so a plan can never silently
//! rewrite a definition used by other plans.

use crate::{AddDestination, App, BudgetBlock, ConfirmAction, PlanFocus, PromptKind, Screen};
use anyhow::Result;
use chrono::Local;
use leeway::models::{Direction, Kind, Mode, PeriodType, Plan, PlanEntry};
use leeway::money::Money;
use leeway::queries::PlanSummary;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph};

// --- Key handling --------------------------------------------------------------

/// Route a key on the unified Plans screen. Screen-global keys (leave, cycle focus) act
/// regardless of pane; everything else dispatches to the focused pane's handler.
pub fn handle_key(
    app: &mut App,
    key: KeyEvent,
    summaries: &[PlanSummary],
    plan: Option<&Plan>,
    entries: &[PlanEntry],
) -> Result<()> {
    app.status = None;

    match key.code {
        // `q` (quit) and `S` (jump to Series) are handled globally; `Esc` goes back to the
        // Dashboard/month view.
        KeyCode::Esc => {
            app.screen = Screen::Dashboard;
            return Ok(());
        }
        KeyCode::Tab => {
            app.plan_focus = next_plan_focus(app.plan_focus);
            return Ok(());
        }
        KeyCode::BackTab => {
            app.plan_focus = previous_plan_focus(app.plan_focus);
            return Ok(());
        }
        _ => {}
    }

    if app.plan_focus == PlanFocus::List {
        handle_list_key(app, key, summaries)
    } else {
        handle_item_key(app, key, plan, entries)
    }
}

/// Plan-scoped verbs, active while the master list is focused.
fn handle_list_key(app: &mut App, key: KeyEvent, summaries: &[PlanSummary]) -> Result<()> {
    let selected = summaries.get(app.plans_sel);

    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if !summaries.is_empty() && app.plans_sel + 1 < summaries.len() {
                app.plans_sel += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.plans_sel = app.plans_sel.saturating_sub(1),

        KeyCode::Char('n') => app.open_text("New plan name", "", PromptKind::NewPlan),

        KeyCode::Char('l') => {
            if let Some(s) = selected {
                app.open_text_replace_on_type(
                    "Plan label",
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
        // `Enter` is intentionally inert: the detail already tracks the selected plan live, so
        // there is nothing to "open". Tab moves focus into the item panes.
        _ => {}
    }
    Ok(())
}

/// Item-scoped verbs, active while one of the Income/Expenses/Envelopes panes is focused.
fn handle_item_key(
    app: &mut App,
    key: KeyEvent,
    plan: Option<&Plan>,
    entries: &[PlanEntry],
) -> Result<()> {
    match key.code {
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
        KeyCode::Char('n') => {
            if let Some(plan) = plan {
                app.open_series_search(
                    AddDestination::Plan {
                        plan_id: plan.id.clone(),
                    },
                    budget_block_for_focus(app.plan_focus),
                )?;
            }
        }

        // The screen only changes plan-scoped things: which series are in the plan and this
        // plan's amount for each. Label, mode, and period belong to the *shared* series
        // (they'd change every plan), so those edits live on the Series page. Redirect the
        // mode and period keys there rather than leaving them as silent dead ends.
        KeyCode::Char('m') | KeyCode::Char('p') => {
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
        PlanFocus::List => PlanFocus::Income,
        PlanFocus::Income => PlanFocus::Expenses,
        PlanFocus::Expenses => PlanFocus::Envelopes,
        PlanFocus::Envelopes => PlanFocus::List,
    }
}

fn previous_plan_focus(current: PlanFocus) -> PlanFocus {
    match current {
        PlanFocus::List => PlanFocus::Envelopes,
        PlanFocus::Income => PlanFocus::List,
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

/// The durable series behind the focused plan row, used by the contextual global `S` jump.
pub fn selected_series_id(app: &App, entries: &[PlanEntry]) -> Option<String> {
    selected_entry(app, entries).map(|entry| entry.series.id.clone())
}

fn current_plan_selection(app: &App) -> usize {
    match app.plan_focus {
        PlanFocus::List => app.plans_sel,
        PlanFocus::Income => app.editor_income_sel,
        PlanFocus::Expenses => app.editor_expense_sel,
        PlanFocus::Envelopes => app.editor_env_sel,
    }
}

fn set_plan_selection(app: &mut App, selected: usize) {
    match app.plan_focus {
        PlanFocus::List => app.plans_sel = selected,
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
        // The master list holds no plan items, so nothing matches it.
        PlanFocus::List => false,
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
        // Only ever called from an item pane; List falls back to Income for totality.
        PlanFocus::List | PlanFocus::Income => BudgetBlock::Income,
        PlanFocus::Expenses => BudgetBlock::Expenses,
        PlanFocus::Envelopes => BudgetBlock::Envelopes,
    }
}

pub fn draw(frame: &mut Frame, app: &App, summaries: &[PlanSummary], entries: &[PlanEntry]) {
    let [header, body, summary_area, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(7),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let title = Paragraph::new(Line::from(" Plans ".bold()))
        .alignment(Alignment::Center)
        .block(crate::bordered_block());
    frame.render_widget(title, header);

    // Master list on the left; the selected plan's stacked item panes on the right.
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).areas(body);
    draw_plan_list(frame, list_area, app, summaries);

    // Each pane asks for its content height as a floor; ratatui shares the leftover space
    // evenly across the three `Min`s, so an empty (or lopsided) plan stays balanced instead
    // of letting one block swallow all the slack.
    let [income_area, expense_area, env_area] = Layout::vertical([
        Constraint::Min(crate::income_block_height(entry_count(
            entries,
            PlanFocus::Income,
        ))),
        Constraint::Min(crate::income_block_height(entry_count(
            entries,
            PlanFocus::Expenses,
        ))),
        Constraint::Min(crate::income_block_height(entry_count(
            entries,
            PlanFocus::Envelopes,
        ))),
    ])
    .areas(detail_area);

    draw_plan_block(frame, income_area, app, entries, PlanFocus::Income);
    draw_plan_block(frame, expense_area, app, entries, PlanFocus::Expenses);
    draw_plan_block(frame, env_area, app, entries, PlanFocus::Envelopes);
    draw_plan_summary(frame, summary_area, entries);

    // Local verbs switch with focus: plan-scoped in the list, item-scoped in a pane.
    let hints = if app.plan_focus == PlanFocus::List {
        Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" n "),
            Span::raw(" new  "),
            key(" l "),
            Span::raw(" label  "),
            key(" s "),
            Span::raw(" stamp  "),
            key(" x "),
            Span::raw(" delete"),
        ])
    } else {
        Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" n "),
            Span::raw(" new  "),
            key(" a "),
            Span::raw(" amount  "),
            key(" x "),
            Span::raw(" remove"),
        ])
    };
    let nav_hints = Line::from(vec![
        key(" Tab "),
        Span::raw(" pane  "),
        key(" h "),
        Span::raw(" help  "),
        key(" S "),
        Span::raw(" series  "),
        key(" Esc "),
        Span::raw(" back  "),
        key(" , "),
        Span::raw(" settings  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    let status = crate::footer_status(app);
    crate::draw_screen_footer(frame, footer, hints, nav_hints, status.as_deref());
}

/// The master plan list. The selected row stays highlighted whether or not the list is the
/// focused pane, so it's always clear which plan the detail panes belong to; the mauve border
/// (not the highlight) signals focus.
fn draw_plan_list(frame: &mut Frame, area: Rect, app: &App, summaries: &[PlanSummary]) {
    let focused = app.plan_focus == PlanFocus::List;
    if summaries.is_empty() {
        let p = Paragraph::new("No plans yet — press n to create one.")
            .block(crate::selectable_block(" Templates ", focused));
        frame.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = summaries
        .iter()
        .map(|s| {
            let count = format!(
                "{} item{}",
                s.item_count,
                if s.item_count == 1 { "" } else { "s" }
            );
            let line = Line::from(vec![
                Span::raw(format!("{:<20}", crate::truncate(&s.plan.name, 20))),
                Span::styled(count, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.plans_sel));

    let list = crate::selectable_list(items).block(crate::selectable_block(" Templates ", focused));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Render the reusable plan's cash-flow scenario without any live account terms.
fn draw_plan_summary(frame: &mut Frame, area: Rect, entries: &[PlanEntry]) {
    let projection = leeway::calc::project_plan(entries);
    let result_color = if projection.whats_left.cents() >= 0 {
        crate::theme::GREEN
    } else {
        Color::Red
    };

    let mut income_and_expenses =
        plan_summary_term(projection.income, "planned income", crate::theme::GREEN);
    income_and_expenses.push(Span::raw("  "));
    income_and_expenses.extend(plan_summary_term(
        Money::ZERO - projection.expenses,
        "planned expenses",
        Color::Red,
    ));

    let lines = vec![
        Line::from(income_and_expenses),
        Line::from(plan_summary_term(
            Money::ZERO - projection.envelopes,
            "planned envelopes",
            crate::theme::MAUVE,
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("= "),
            Span::styled(
                format!("{:>10}", projection.whats_left),
                Style::default()
                    .fg(result_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  what's left"),
        ]),
        Line::from(Span::styled(
            format!(
                "Daily envelope rates assume a {}-day month.",
                leeway::calc::PLAN_PROJECTION_DAYS
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(crate::titled_block(" Summary ")),
        area,
    );
}

fn plan_summary_term(amount: Money, label: &str, color: Color) -> Vec<Span<'static>> {
    let cents = amount.cents();
    let sign = if cents < 0 { "−" } else { "+" };
    vec![
        Span::raw(format!("{sign} ")),
        Span::styled(
            format!("{:>10}", Money(cents.saturating_abs())),
            Style::default().fg(color),
        ),
        Span::raw(format!("  {:<17}", label)),
    ]
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
        PlanFocus::List | PlanFocus::Income => app.editor_income_sel,
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
        // Only the three item panes render a block title; List has its own list widget.
        PlanFocus::List | PlanFocus::Income => " Income ",
        PlanFocus::Expenses => " Expenses ",
        PlanFocus::Envelopes => " Envelopes ",
    }
}

/// Render one plan entry inside its logical block. Transaction blocks already imply
/// direction, so envelopes are the only rows that need mode/period details.
fn entry_row(entry: &PlanEntry, focus: PlanFocus) -> ListItem<'static> {
    let s = &entry.series;
    let line = match focus {
        PlanFocus::List | PlanFocus::Income | PlanFocus::Expenses => Line::from(vec![
            Span::raw(format!("{:<24}", crate::truncate(&s.label, 24))),
            Span::raw(format!("{:>12}", entry.amount.to_string())),
        ]),
        PlanFocus::Envelopes => {
            let period = match s.period_type {
                Some(PeriodType::Daily) => "/day",
                Some(PeriodType::Weekly) | Some(PeriodType::Monthly) | None => "/mo",
            };
            // Envelope series always carry a concrete mode; None is unreachable here.
            let mode = match s.mode {
                Some(Mode::Manual) => "manual",
                _ => "auto",
            };
            Line::from(vec![
                Span::raw(format!("{:<18}", crate::truncate(&s.label, 18))),
                Span::styled(
                    format!("{:<6}{:<8}", period, mode),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:>14}", entry.amount.to_string())),
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
    use leeway::money::Money;
    use leeway::ops;
    use leeway::queries;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use rusqlite::Connection;
    use uuid::Uuid;

    fn open_test_conn() -> Connection {
        let mut path = std::env::temp_dir();
        path.push(format!("leeway-plans-{}.db", Uuid::new_v4()));
        leeway::db::open(&path).unwrap()
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
            screen: Screen::Plans,
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
            series_range: leeway::view::SeriesTimeRange::Last12Stamped,
            series_filter: crate::SeriesFilter::Both,
            plan_focus: PlanFocus::Expenses,
            editor_income_sel: 0,
            editor_expense_sel: 0,
            editor_env_sel: 0,
            settings_general_sel: 0,
            pending_select: None,
            pending_dash_txn: None,
            pending_dash_env: None,
            pending_dash_account: None,
            pending_series_select: None,
            pending_plan_select: None,
            summary_anims: crate::anim::SummaryAnimations::new(),
            series_chart_anim: crate::anim::ChartAnimation::new(),
            frame_now: std::time::Instant::now(),
            modal: None,
            status: None,
            sync: None,
        };

        (app, plan, entries, rent_series_id)
    }

    fn direction_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)
    }

    fn r_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)
    }

    fn backtab_key() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect()
    }

    #[test]
    fn plans_screen_renders_expenses_summary_and_projection_note() {
        let (app, _, entries, _) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw(frame, &app, &summaries, &entries))
            .unwrap();
        let text = buffer_text(&terminal);

        // The master list and the selected plan's detail render together.
        assert!(text.contains("Templates"));
        assert!(text.contains("Normal"));
        assert!(text.contains("Expenses"));
        assert!(text.contains("Summary"));
        assert!(text.contains("planned income"));
        assert!(text.contains("planned expenses"));
        assert!(text.contains("planned envelopes"));
        assert!(text.contains("-$1,800.00"));
        assert!(text.contains("Daily envelope rates assume a 30-day month."));
        assert!(!text.contains("planned bills"));
    }

    #[test]
    fn empty_item_panes_are_evenly_balanced() {
        // With no items, the three panes should split the detail column evenly rather than
        // letting Expenses swallow the slack (the regression this guards).
        let (app, _, _, _) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw(frame, &app, &summaries, &[]))
            .unwrap();

        let title_row = |needle: &str| -> u16 {
            let buffer = terminal.backend().buffer();
            for y in 0..buffer.area.height {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                if row.contains(needle) {
                    return y;
                }
            }
            panic!("no row contains {needle:?}");
        };

        let income = title_row(" Income ");
        let expenses = title_row(" Expenses ");
        let envelopes = title_row(" Envelopes ");
        let top = expenses - income;
        let bottom = envelopes - expenses;
        assert!(
            top.abs_diff(bottom) <= 1,
            "panes unbalanced: Income@{income} Expenses@{expenses} Envelopes@{envelopes}"
        );
    }

    #[test]
    fn draw_survives_a_tiny_frame() {
        // The fixed header/summary/footer bands (12 rows) exceed a short terminal; ratatui
        // must clamp rather than panic.
        let (app, _, entries, _) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        let backend = TestBackend::new(20, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw(frame, &app, &summaries, &entries))
            .unwrap();
    }

    #[test]
    fn draw_with_no_plans_shows_the_empty_prompt() {
        let (mut app, _, _, _) = app_with_transaction_plan();
        app.plan_focus = PlanFocus::List;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &app, &[], &[])).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("No plans yet"));
    }

    #[test]
    fn backtab_cycles_panes_backward() {
        // The cycle now includes the master list: Expenses → Income → List.
        let (mut app, plan, entries, _) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();

        handle_key(&mut app, backtab_key(), &summaries, Some(&plan), &entries).unwrap();
        assert!(app.plan_focus == PlanFocus::Income);

        handle_key(&mut app, backtab_key(), &summaries, Some(&plan), &entries).unwrap();
        assert!(app.plan_focus == PlanFocus::List);
    }

    #[test]
    fn r_key_is_inert_in_item_pane() {
        // `r` is no longer a label-edit shortcut, even as a redirect to the Series page.
        let (mut app, plan, entries, rent_series_id) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();

        handle_key(&mut app, r_key(), &summaries, Some(&plan), &entries).unwrap();

        assert!(app.modal.is_none(), "no label prompt opened");
        assert!(app.status.is_none(), "r has no action");
        let series = queries::get_series(&app.conn, &rent_series_id)
            .unwrap()
            .unwrap();
        assert_eq!(series.label, "Rent", "shared label untouched");
    }

    #[test]
    fn direction_key_does_not_change_transaction_series_direction() {
        let (mut app, plan, entries, rent_series_id) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        let rent = entries
            .iter()
            .find(|entry| entry.series.id == rent_series_id)
            .unwrap();
        assert_eq!(rent.series.direction, Some(Direction::Out));

        handle_key(&mut app, direction_key(), &summaries, Some(&plan), &entries).unwrap();

        assert!(app.status.is_none());
        assert!(app.pending_select.is_none());
        let refreshed = queries::get_series(&app.conn, &rent_series_id)
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.direction, Some(Direction::Out));
    }

    #[test]
    fn contextual_series_id_comes_from_the_focused_plan_row() {
        let (app, _, entries, rent_series_id) = app_with_transaction_plan();

        assert_eq!(selected_series_id(&app, &entries), Some(rent_series_id));
    }

    #[test]
    fn contextual_series_id_is_none_when_the_list_is_focused() {
        // Focused on the master list, `S` should open the series list, not a detail.
        let (mut app, _, entries, _) = app_with_transaction_plan();
        app.plan_focus = PlanFocus::List;

        assert_eq!(selected_series_id(&app, &entries), None);
    }

    #[test]
    fn tab_cycles_forward_through_all_four_panes() {
        let (mut app, plan, entries, _) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        app.plan_focus = PlanFocus::List;
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);

        for expected in [
            PlanFocus::Income,
            PlanFocus::Expenses,
            PlanFocus::Envelopes,
            PlanFocus::List,
        ] {
            handle_key(&mut app, tab, &summaries, Some(&plan), &entries).unwrap();
            assert!(app.plan_focus == expected);
        }
    }

    #[test]
    fn list_focus_n_opens_the_new_plan_prompt() {
        let (mut app, plan, entries, _) = app_with_transaction_plan();
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        app.plan_focus = PlanFocus::List;
        let n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);

        handle_key(&mut app, n, &summaries, Some(&plan), &entries).unwrap();

        assert!(matches!(
            app.modal,
            Some(crate::Modal::Text(crate::TextPrompt {
                kind: PromptKind::NewPlan,
                ..
            }))
        ));
    }

    #[test]
    fn list_focus_n_opens_new_plan_even_with_no_plans() {
        // The empty-state prompt must still work: `n` creates the first plan.
        let (mut app, plan, entries, _) = app_with_transaction_plan();
        app.plan_focus = PlanFocus::List;
        let n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);

        handle_key(&mut app, n, &[], Some(&plan), &entries).unwrap();

        assert!(matches!(
            app.modal,
            Some(crate::Modal::Text(crate::TextPrompt {
                kind: PromptKind::NewPlan,
                ..
            }))
        ));
    }
}
