//! Plan detail and commands for the shared budget workspace.
//!
//! Editing model: focus the plan list to act on whole plans — `n` new, `l` label, `s` stamp,
//! `x` delete. Focus an item pane to act on that block's items — `n` searches/creates a
//! series and fills the plan amount, `a` sets this plan's amount, `M` sets which months the
//! plan runs it in, `x` removes the item from this plan. The screen only touches plan-scoped
//! things; editing the shared series itself (label, mode, period) lives on the Series page
//! (`S`), so a plan can never silently rewrite a definition used by other plans.

use crate::{AddDestination, App, BudgetBlock, ConfirmAction, PlanFocus, PromptKind};
use anyhow::Result;
use chrono::Local;
use leeway::calc::PlanProjection;
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

/// Route a key for a selected plan. Workspace-global keys act first; everything else
/// dispatches to the sidebar or focused item pane.
pub fn handle_key(
    app: &mut App,
    key: KeyEvent,
    summaries: &[PlanSummary],
    plan: Option<&Plan>,
    entries: &[PlanEntry],
) -> Result<()> {
    app.status = None;

    match key.code {
        // Escape returns from an item pane to the budget sidebar. From the sidebar it exits.
        KeyCode::Esc => {
            if app.plan_focus == PlanFocus::List {
                app.should_quit = true;
            } else {
                app.plan_focus = PlanFocus::List;
            }
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

        // Which months this plan runs the item in — per-plan, like the amount, which is why
        // it sits here and not behind the lowercase `m` redirect to the shared series.
        KeyCode::Char('M') => {
            if let Some(en) = selected_entry(app, entries) {
                app.open_text_replace_on_type(
                    "Active months (e.g. mar,jul,nov or all)",
                    en.active_months.edit_string(),
                    PromptKind::ItemMonths {
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

#[cfg(test)]
pub fn draw(frame: &mut Frame, app: &App, summaries: &[PlanSummary], entries: &[PlanEntry]) {
    // The summary grows only when the plan has seasonal items to list, so a plan without
    // them keeps the layout it has always had.
    let projection = leeway::calc::project_plan(entries);
    let [header, body, summary_area, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(6 + seasonal_line_count(&projection) as u16),
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
    draw_plan_summary(frame, summary_area, &projection);

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
            key(" M "),
            Span::raw(" months  "),
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

/// Draw one plan inside the shared budget workspace. The workspace owns the sidebar and
/// footer; this keeps the same four-panel shape as a month.
pub fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    plan: Option<&Plan>,
    entries: &[PlanEntry],
) {
    let projection = leeway::calc::project_plan(entries);
    let [header, body, summary_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(6 + seasonal_line_count(&projection) as u16),
    ])
    .areas(area);

    let title = match plan {
        Some(plan) => format!(" Leeway — Plan: {} ", plan.name),
        None => " Leeway — No plan selected ".into(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(title.bold()))
            .alignment(Alignment::Center)
            .block(crate::bordered_block()),
        header,
    );

    let income_count = entry_count(entries, PlanFocus::Income);
    let expense_count = entry_count(entries, PlanFocus::Expenses);
    let envelope_count = entry_count(entries, PlanFocus::Envelopes);
    if body.width >= 72 {
        let (items_height, envelope_height) = crate::dashboard::budget_panel_heights(
            body.height,
            income_count.max(expense_count),
            envelope_count,
        );
        let [items_area, env_area] = Layout::vertical([
            Constraint::Length(items_height),
            Constraint::Length(envelope_height),
        ])
        .areas(body);
        let [income_area, expense_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(items_area);
        draw_plan_block(frame, income_area, app, entries, PlanFocus::Income);
        draw_plan_block(frame, expense_area, app, entries, PlanFocus::Expenses);
        draw_plan_block(frame, env_area, app, entries, PlanFocus::Envelopes);
    } else {
        let [income_area, expense_area, env_area] = Layout::vertical([
            Constraint::Min(crate::income_block_height(income_count)),
            Constraint::Min(crate::income_block_height(expense_count)),
            Constraint::Min(crate::income_block_height(envelope_count)),
        ])
        .areas(body);
        draw_plan_block(frame, income_area, app, entries, PlanFocus::Income);
        draw_plan_block(frame, expense_area, app, entries, PlanFocus::Expenses);
        draw_plan_block(frame, env_area, app, entries, PlanFocus::Envelopes);
    }
    draw_plan_summary(frame, summary_area, &projection);
}

/// Detail-panel hints used by the shared budget footer.
pub fn footer_hints(_app: &App) -> Line<'static> {
    Line::from(vec![
        key(" j/k "),
        Span::raw(" move  "),
        key(" n "),
        Span::raw(" new  "),
        key(" a "),
        Span::raw(" amount  "),
        key(" M "),
        Span::raw(" months  "),
        key(" x "),
        Span::raw(" remove"),
    ])
}

/// The master plan list. The selected row stays highlighted whether or not the list is the
/// focused pane, so it's always clear which plan the detail panes belong to; the mauve border
/// (not the highlight) signals focus.
#[cfg(test)]
fn draw_plan_list(frame: &mut Frame, area: Rect, app: &App, summaries: &[PlanSummary]) {
    let focused = app.plan_focus == PlanFocus::List;
    if summaries.is_empty() {
        let p = Paragraph::new("No plans yet — press n to create one.")
            .block(crate::selectable_block(" Templates ", focused));
        frame.render_widget(p, area);
        return;
    }

    // The selection band always sits on `plans_sel` here, focused or not, so that
    // row's count column needs the lighter muted tone to stay readable.
    let items: Vec<ListItem> = summaries
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let count = format!(
                "{} item{}",
                s.item_count,
                if s.item_count == 1 { "" } else { "s" }
            );
            let line = Line::from(vec![
                Span::raw(format!("{:<20}", crate::truncate(&s.plan.name, 20))),
                Span::styled(
                    count,
                    Style::default().fg(crate::theme::muted(i == app.plans_sel)),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let item_count = items.len();
    let mut state = ListState::default();
    state.select(Some(app.plans_sel));

    let list = crate::selectable_list(items).block(crate::selectable_block(" Templates ", focused));
    frame.render_stateful_widget(list, area, &mut state);
    crate::render_list_scrollbar(frame, area, item_count, state.offset(), focused);
}

/// Render the reusable plan's cash-flow scenario without any live account terms.
/// At most this many seasonal items get a line of their own; the rest collapse to a count.
const SEASONAL_LINES_SHOWN: usize = 3;

/// Extra rows the summary block needs for its seasonal section — zero for a plan whose
/// items all run every month.
fn seasonal_line_count(projection: &PlanProjection) -> usize {
    let listed = projection.seasonal.len().min(SEASONAL_LINES_SHOWN);
    listed + usize::from(projection.seasonal.len() > SEASONAL_LINES_SHOWN)
}

fn draw_plan_summary(frame: &mut Frame, area: Rect, projection: &PlanProjection) {
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

    let mut lines = vec![
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
    ];

    // Seasonal items sit outside the totals above, so name them here — a plan that budgets
    // birthday gifts in March and July should say so without distorting an ordinary month.
    for item in projection.seasonal.iter().take(SEASONAL_LINES_SHOWN) {
        let mut spans = plan_summary_term(
            item.net,
            &crate::truncate(&item.label, 17),
            crate::theme::MAUVE,
        );
        spans.push(Span::styled(
            format!("in {}", item.months.short_label()),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::from(spans));
    }
    if projection.seasonal.len() > SEASONAL_LINES_SHOWN {
        lines.push(Line::from(Span::styled(
            format!(
                "  + {} more seasonal",
                projection.seasonal.len() - SEASONAL_LINES_SHOWN
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }

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
    let focused = app.plan_focus == focus;
    let selected = match focus {
        PlanFocus::List | PlanFocus::Income => app.editor_income_sel,
        PlanFocus::Expenses => app.editor_expense_sel,
        PlanFocus::Envelopes => app.editor_env_sel,
    };

    let content_width = crate::selectable_list_content_width(area);
    let rows: Vec<ListItem> = entries
        .iter()
        .filter(|entry| entry_matches_focus(entry, focus))
        .enumerate()
        .map(|(i, entry)| entry_row(entry, focus, focused && i == selected, content_width))
        .collect();

    let row_count = rows.len();
    let mut state = ListState::default();
    if focused && !rows.is_empty() {
        state.select(Some(selected));
    }

    let mut block = crate::selectable_block(plan_block_title(focus), focused);
    let has_daily_envelopes = focus == PlanFocus::Envelopes
        && entries.iter().any(|entry| {
            entry_matches_focus(entry, focus) && entry.series.period_type == Some(PeriodType::Daily)
        });
    if has_daily_envelopes {
        block = block.title_bottom(Line::from(Span::styled(
            format!(
                " Daily envelope rates assume a {}-day month. ",
                leeway::calc::PLAN_PROJECTION_DAYS
            ),
            Style::default().fg(crate::theme::MUTED),
        )));
    }
    let list = crate::selectable_list(rows).block(block);
    frame.render_stateful_widget(list, area, &mut state);
    crate::render_list_scrollbar(frame, area, row_count, state.offset(), focused);
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
/// `selected` says whether the selection band sits behind this row, which decides
/// how far the secondary columns can fade.
fn entry_row(
    entry: &PlanEntry,
    focus: PlanFocus,
    selected: bool,
    content_width: usize,
) -> ListItem<'static> {
    let s = &entry.series;
    let muted = Style::default().fg(crate::theme::muted(selected));
    let amount_width = 12.min(content_width.saturating_sub(1));
    let compact_label_width = content_width.saturating_sub(amount_width + 1).max(1);
    let mut line = match focus {
        PlanFocus::List | PlanFocus::Income | PlanFocus::Expenses => Line::from(vec![
            Span::raw(format!(
                "{:<compact_label_width$}",
                crate::truncate(&s.label, compact_label_width)
            )),
            Span::raw(" "),
            Span::raw(format!("{:>amount_width$}", entry.amount.to_string())),
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
            if content_width >= 44 {
                let label_width = content_width.saturating_sub(14 + amount_width + 1);
                Line::from(vec![
                    Span::raw(format!(
                        "{:<label_width$}",
                        crate::truncate(&s.label, label_width)
                    )),
                    Span::styled(format!("{:<6}{:<8}", period, mode), muted),
                    Span::raw(" "),
                    Span::raw(format!("{:>amount_width$}", entry.amount.to_string())),
                ])
            } else {
                Line::from(vec![
                    Span::raw(format!(
                        "{:<compact_label_width$}",
                        crate::truncate(&s.label, compact_label_width)
                    )),
                    Span::raw(" "),
                    Span::raw(format!("{:>amount_width$}", entry.amount.to_string())),
                ])
            }
        }
    };

    // Only seasonal rows carry a tag, so an ordinary plan looks exactly as it always has.
    if !entry.active_months.is_all() {
        line.push_span(Span::styled(
            format!("  {}", entry.active_months.short_label()),
            muted,
        ));
    }

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
    use crate::Screen;
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
            screen: Screen::Budget,
            budget_target: crate::BudgetTarget::Plan {
                plan_id: plan_id.clone(),
            },
            last_plan_id: Some(plan_id.clone()),
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
    fn plans_screen_hides_the_projection_note_without_daily_envelopes() {
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
        assert!(!text.contains("Daily envelope rates assume a 30-day month."));
        assert!(!text.contains("planned bills"));
        // A plan with nothing seasonal says nothing about months.
        assert!(!text.contains("Mar, Jul, Nov"));
    }

    #[test]
    fn projection_note_sits_on_the_envelopes_border_with_readable_color() {
        let (mut app, plan, _, _) = app_with_transaction_plan();
        let daily = ops::create_series(
            &app.conn,
            Kind::Envelope,
            "Dining",
            None,
            Some(PeriodType::Daily),
            Some(Mode::Automatic),
        )
        .unwrap();
        ops::add_plan_item(&app.conn, &plan.id, &daily, Money::from_dollars(20.0)).unwrap();
        let entries = queries::load_plan_entries(&app.conn, &plan.id).unwrap();
        app.plan_focus = PlanFocus::Envelopes;
        let backend = TestBackend::new(70, 5);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                draw_plan_block(frame, frame.area(), &app, &entries, PlanFocus::Envelopes)
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let bottom = buffer.area.height - 1;
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, bottom)].symbol())
            .collect();
        let note = "Daily envelope rates assume a 30-day month.";
        let start = row
            .find(note)
            .expect("note should sit on the bottom border") as u16;

        assert_eq!(buffer[(start, bottom)].fg, crate::theme::MUTED);
    }

    /// Press a key and answer the prompt it opens, the way a user would.
    fn answer_prompt(app: &mut App, typed: &str) {
        match app.modal.as_mut() {
            Some(crate::Modal::Text(prompt)) => prompt.buffer = typed.into(),
            _ => panic!("expected a text prompt to be open"),
        }
        crate::submit_text(app).unwrap();
    }

    #[test]
    fn m_sets_the_months_a_plan_runs_an_item_in() {
        let (mut app, plan, entries, _) = app_with_transaction_plan();
        app.plan_focus = PlanFocus::Expenses;
        let item_id = entries[0].item_id.clone();
        let months_key = KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE);

        // The prompt opens prefilled with today's answer, "all".
        handle_key(&mut app, months_key, &[], Some(&plan), &entries).unwrap();
        match app.modal.as_ref() {
            Some(crate::Modal::Text(prompt)) => assert_eq!(prompt.buffer, "all"),
            _ => panic!("expected the months prompt"),
        }

        answer_prompt(&mut app, "mar,jul,nov");
        let reloaded = queries::load_plan_entries(&app.conn, &plan.id).unwrap();
        let entry = reloaded.iter().find(|e| e.item_id == item_id).unwrap();
        assert_eq!(
            entry.active_months,
            leeway::models::MonthSet::parse("mar,jul,nov").unwrap()
        );

        // Reopening prefills what was saved, so editing is a tweak rather than a retype.
        handle_key(&mut app, months_key, &[], Some(&plan), &reloaded).unwrap();
        match app.modal.as_ref() {
            Some(crate::Modal::Text(prompt)) => assert_eq!(prompt.buffer, "mar,jul,nov"),
            _ => panic!("expected the months prompt"),
        }

        // Nonsense is refused with a message and leaves the saved months alone.
        answer_prompt(&mut app, "septembre");
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("septembre"))
        );
        let reloaded = queries::load_plan_entries(&app.conn, &plan.id).unwrap();
        let entry = reloaded.iter().find(|e| e.item_id == item_id).unwrap();
        assert_eq!(
            entry.active_months,
            leeway::models::MonthSet::parse("mar,jul,nov").unwrap()
        );

        // And "all" puts it back to every month.
        handle_key(&mut app, months_key, &[], Some(&plan), &reloaded).unwrap();
        answer_prompt(&mut app, "all");
        let reloaded = queries::load_plan_entries(&app.conn, &plan.id).unwrap();
        let entry = reloaded.iter().find(|e| e.item_id == item_id).unwrap();
        assert!(entry.active_months.is_all());
    }

    #[test]
    fn seasonal_items_are_tagged_in_the_row_and_listed_below_whats_left() {
        let (mut app, plan, _, _) = app_with_transaction_plan();

        // Birthday gifts: an envelope this plan only runs in three months.
        let gifts = ops::create_series(
            &app.conn,
            Kind::Envelope,
            "Kid gifts",
            None,
            Some(leeway::models::PeriodType::Monthly),
            Some(leeway::models::Mode::Automatic),
        )
        .unwrap();
        let item =
            ops::add_plan_item(&app.conn, &plan.id, &gifts, Money::from_dollars(120.0)).unwrap();
        ops::set_item_active_months(
            &app.conn,
            &item,
            leeway::models::MonthSet::parse("mar,jul,nov").unwrap(),
        )
        .unwrap();

        app.plan_focus = PlanFocus::Envelopes;
        let entries = queries::load_plan_entries(&app.conn, &plan.id).unwrap();
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw(frame, &app, &summaries, &entries))
            .unwrap();
        let text = buffer_text(&terminal);

        // The row carries the months, and the summary names it under what's left.
        assert!(text.contains("Kid gifts"));
        assert!(text.contains("Mar, Jul, Nov"));
        assert!(text.contains("in Mar, Jul, Nov"));
        // It stays out of the headline: planned envelopes is still zero.
        assert!(text.contains("planned envelopes"));
        assert!(!text.contains("-$120.00  planned envelopes"));
        assert!(!text.contains("Daily envelope rates assume a 30-day month."));
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
    fn an_overflowing_item_pane_draws_a_scrollbar() {
        // Item panes cap at 7 rows, so a long expense list has to advertise the overflow.
        let (mut app, _, _, _) = app_with_transaction_plan();
        let plan_id = queries::plan_summaries(&app.conn).unwrap()[0]
            .plan
            .id
            .clone();
        for i in 0..10 {
            let series_id = ops::create_series(
                &app.conn,
                Kind::Transaction,
                &format!("Bill {i}"),
                Some(Direction::Out),
                None,
                None,
            )
            .unwrap();
            ops::add_plan_item(&app.conn, &plan_id, &series_id, Money::from_dollars(10.0)).unwrap();
        }
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        let entries = queries::load_plan_entries(&app.conn, &plan_id).unwrap();
        app.plan_focus = PlanFocus::Expenses;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &summaries, &entries))
            .unwrap();

        // The thumb sits in the pane's rightmost column, over the border.
        let buffer = terminal.backend().buffer();
        let thumbs = (0..buffer.area.height)
            .filter(|y| buffer[(99, *y)].symbol() == "█")
            .count();
        assert!(thumbs > 0, "no scrollbar thumb in the right-hand column");
    }

    #[test]
    fn an_overflowing_templates_list_draws_a_scrollbar() {
        let (mut app, _, _, _) = app_with_transaction_plan();
        for i in 0..30 {
            ops::create_plan(&app.conn, &format!("Plan {i}")).unwrap();
        }
        let summaries = queries::plan_summaries(&app.conn).unwrap();
        app.plan_focus = PlanFocus::List;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &summaries, &[]))
            .unwrap();

        // The list column is 34 wide, so its right border — and the thumb — sits at x = 33.
        let buffer = terminal.backend().buffer();
        let thumbs = (0..buffer.area.height)
            .filter(|y| buffer[(33, *y)].symbol() == "█")
            .count();
        assert!(thumbs > 0, "no scrollbar thumb on the Templates list");
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
    fn escape_returns_to_sidebar_then_exits() {
        let (mut app, plan, entries, _) = app_with_transaction_plan();
        app.plan_focus = PlanFocus::Expenses;
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        handle_key(&mut app, escape, &[], Some(&plan), &entries).unwrap();
        assert!(app.plan_focus == PlanFocus::List);
        assert!(!app.should_quit);

        handle_key(&mut app, escape, &[], Some(&plan), &entries).unwrap();
        assert!(app.should_quit);
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
