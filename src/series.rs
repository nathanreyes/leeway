//! The Series screen: recurring-item management plus range-scoped trend stats.

use crate::{App, BudgetBlock, ChoiceOption, ConfirmAction, ModalAction, PromptKind, SeriesFilter};
use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use leeway::models::{Kind, Mode, PeriodType};
use leeway::money::Money;
use leeway::view::{
    SeriesDetailView, SeriesGroup, SeriesPageView, SeriesTimeRange, SeriesTrendPoint,
};
use leeway::{ops, queries};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, ListItem, ListState, Paragraph};

enum SidebarRow {
    Header(SeriesGroup),
    Item(usize),
}

pub fn visible_count(app: &App, view: &SeriesPageView) -> usize {
    visible_indices(app, view).len()
}

pub fn handle_key(
    app: &mut App,
    key: KeyEvent,
    view: &SeriesPageView,
    today: NaiveDate,
) -> Result<()> {
    app.status = None;

    if app.series_search_active {
        return handle_search_key(app, key, view);
    }

    match key.code {
        // `q` (quit), `P` (jump to Plans), and `S` (expand Detail to List) are handled
        // globally. `Esc` returns to whichever screen opened this Series workflow.
        KeyCode::Esc => app.return_from_series(),
        KeyCode::Char('/') => app.series_search_active = true,
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, view, 1),
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, view, -1),
        KeyCode::Char('t') => open_range_choice(app, today),
        // `f` cycles the membership filter (Plans → Ad-hoc → Both). Reset the selection so it
        // doesn't linger past the end of a now-shorter list.
        KeyCode::Char('f') => {
            app.series_filter = app.series_filter.next();
            app.series_sel = 0;
        }
        // `n` creates a series. The list is a flat, grouped sidebar (no block focus), so we
        // pick the kind with a little menu first, then prompt for the label.
        KeyCode::Char('n') => open_new_series_choice(app),
        // `x` deletes the selected series, with the plan/month guard the user asked for.
        KeyCode::Char('x') => delete_selected_series(app, view)?,
        KeyCode::Char('l') => {
            if let Some(detail) = selected_detail(app, view) {
                open_label_edit(app, detail);
            }
        }
        KeyCode::Char('m') => {
            if let Some(detail) = selected_detail(app, view) {
                toggle_mode(app, detail)?;
            }
        }
        KeyCode::Char('p') => {
            if let Some(detail) = selected_detail(app, view) {
                cycle_period(app, detail)?;
            }
        }
        _ => {}
    }

    Ok(())
}

/// Input for the focused single-series mode. Global `S`, `P`, help, and quit are handled by
/// the event loop first; this handler owns only back, range, and shared-definition edits.
pub fn handle_detail_key(
    app: &mut App,
    key: KeyEvent,
    detail: &SeriesDetailView,
    today: NaiveDate,
) -> Result<()> {
    app.status = None;
    match key.code {
        KeyCode::Esc => app.return_from_series(),
        KeyCode::Char('t') => open_range_choice(app, today),
        KeyCode::Char('l') => open_label_edit(app, detail),
        KeyCode::Char('m') => toggle_mode(app, detail)?,
        KeyCode::Char('p') => cycle_period(app, detail)?,
        _ => {}
    }
    Ok(())
}

fn open_label_edit(app: &mut App, detail: &SeriesDetailView) {
    app.open_text_replace_on_type(
        "Series label (shared across plans)",
        detail.series.label.clone(),
        PromptKind::SeriesLabel {
            series_id: detail.series.id.clone(),
        },
    );
}

fn toggle_mode(app: &mut App, detail: &SeriesDetailView) -> Result<()> {
    if detail.series.kind == Kind::Envelope {
        let next = match detail.series.mode {
            Some(Mode::Manual) => Mode::Automatic,
            _ => Mode::Manual,
        };
        ops::set_series_mode(&app.conn, &detail.series.id, next)?;
        app.status = Some("Mode changed (affects all plans using this series)".into());
    } else {
        app.status = Some("Mode applies to envelopes".into());
    }
    Ok(())
}

fn cycle_period(app: &mut App, detail: &SeriesDetailView) -> Result<()> {
    if detail.series.kind == Kind::Envelope {
        let next = match detail.series.period_type {
            Some(PeriodType::Daily) => PeriodType::Monthly,
            Some(PeriodType::Weekly) | Some(PeriodType::Monthly) | None => PeriodType::Daily,
        };
        ops::set_series_period(&app.conn, &detail.series.id, next)?;
        app.status = Some("Period changed; plan amounts converted on a 30-day basis".into());
    } else {
        app.status = Some("Period applies to envelopes".into());
    }
    Ok(())
}

fn handle_search_key(app: &mut App, key: KeyEvent, view: &SeriesPageView) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            if app.series_search.is_empty() {
                app.series_search_active = false;
            } else {
                app.series_search.clear();
                app.series_sel = 0;
            }
        }
        KeyCode::Enter => app.series_search_active = false,
        KeyCode::Backspace => {
            app.series_search.pop();
            app.series_sel = 0;
        }
        KeyCode::Down => move_selection(app, view, 1),
        KeyCode::Up => move_selection(app, view, -1),
        KeyCode::Char(c) => {
            app.series_search.push(c);
            app.series_sel = 0;
        }
        _ => {}
    }
    Ok(())
}

fn open_range_choice(app: &mut App, today: NaiveDate) {
    app.open_choice(
        "Series time range",
        vec![
            ChoiceOption {
                key: '1',
                label: "Last 12 stamped months".into(),
                action: Some(ModalAction::SetSeriesRange {
                    range: SeriesTimeRange::Last12Stamped,
                }),
            },
            ChoiceOption {
                key: 't',
                label: format!("This year ({})", today.year()),
                action: Some(ModalAction::SetSeriesRange {
                    range: SeriesTimeRange::ThisYear,
                }),
            },
            ChoiceOption {
                key: 'l',
                label: format!("Last year ({})", today.year() - 1),
                action: Some(ModalAction::SetSeriesRange {
                    range: SeriesTimeRange::LastYear,
                }),
            },
            ChoiceOption {
                key: 'a',
                label: "All history".into(),
                action: Some(ModalAction::SetSeriesRange {
                    range: SeriesTimeRange::AllHistory,
                }),
            },
            ChoiceOption {
                key: 'c',
                label: "Cancel".into(),
                action: None,
            },
        ],
    );
}

/// The kind chooser for `n`. Each option carries the `BudgetBlock` that fixes the new
/// series' kind + direction (and, for envelopes, seeds monthly/default-mode); picking one
/// opens the label prompt via `ModalAction::BeginNewSeries`.
fn open_new_series_choice(app: &mut App) {
    app.open_choice(
        "New series",
        vec![
            ChoiceOption {
                key: 'i',
                label: "Income (transaction in)".into(),
                action: Some(ModalAction::BeginNewSeries {
                    block: BudgetBlock::Income,
                }),
            },
            ChoiceOption {
                key: 'e',
                label: "Expense (transaction out)".into(),
                action: Some(ModalAction::BeginNewSeries {
                    block: BudgetBlock::Expenses,
                }),
            },
            ChoiceOption {
                key: 'v',
                label: "Envelope".into(),
                action: Some(ModalAction::BeginNewSeries {
                    block: BudgetBlock::Envelopes,
                }),
            },
            ChoiceOption {
                key: 'c',
                label: "Cancel".into(),
                action: None,
            },
        ],
    );
}

/// Delete the selected series, guarding references the way the user chose:
///   - used by any plan  -> refuse, and name the plans (a live FK; remove it there first);
///   - only in past months -> confirm, noting history/trends are preserved (the copied
///     `series_id` is intentionally orphaned — see `ops::delete_series`);
///   - unused anywhere -> a plain confirm.
fn delete_selected_series(app: &mut App, view: &SeriesPageView) -> Result<()> {
    let Some(detail) = selected_detail(app, view) else {
        return Ok(());
    };
    let series_id = detail.series.id.clone();
    let label = detail.series.label.clone();

    let plans = queries::plan_names_for_series(&app.conn, &series_id)?;
    if !plans.is_empty() {
        app.status = Some(format!(
            "“{label}” is used in plans: {} — remove it there first",
            plans.join(", ")
        ));
        return Ok(());
    }

    let months = queries::series_month_usage(&app.conn, &series_id)?;
    let title = if months > 0 {
        format!(
            "Delete “{label}”? Used in {months} past month{}; history is kept.",
            if months == 1 { "" } else { "s" }
        )
    } else {
        format!("Delete “{label}”?")
    };
    app.open_confirm(title, ConfirmAction::DeleteSeries { series_id });
    Ok(())
}

/// Move the selection onto the series with this id (used after creating one). No-op if it
/// isn't currently visible — e.g. a search filter is hiding it.
pub fn select_series_by_id(app: &mut App, view: &SeriesPageView, series_id: &str) {
    let indices = visible_indices(app, view);
    if let Some(pos) = indices
        .iter()
        .position(|&idx| view.details[idx].series.id == series_id)
    {
        app.series_sel = pos;
    }
}

/// Make a requested series visible before selecting it. This is used when Detail expands to
/// List: stale list state should not make the contextual target appear to have been lost.
pub fn reveal_series_by_id(app: &mut App, view: &SeriesPageView, series_id: &str) {
    let Some(detail) = detail_by_id(view, series_id) else {
        return;
    };
    let needle = app.series_search.trim().to_lowercase();
    if !needle.is_empty() && !detail.series.label.to_lowercase().contains(&needle) {
        app.series_search.clear();
    }
    let hidden_by_filter = match app.series_filter {
        SeriesFilter::Both => false,
        SeriesFilter::Plans => detail.plan_names.is_empty(),
        SeriesFilter::AdHoc => !detail.plan_names.is_empty(),
    };
    if hidden_by_filter {
        app.series_filter = SeriesFilter::Both;
    }
    select_series_by_id(app, view, series_id);
}

pub fn detail_by_id<'v>(view: &'v SeriesPageView, series_id: &str) -> Option<&'v SeriesDetailView> {
    view.details
        .iter()
        .find(|detail| detail.series.id == series_id)
}

pub fn draw(frame: &mut Frame, app: &App, view: &SeriesPageView) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    draw_header(frame, header, app, view);
    draw_body(frame, body, app, view);
    draw_footer(frame, footer, app, view);
}

pub fn draw_detail_screen(
    frame: &mut Frame,
    app: &App,
    view: &SeriesPageView,
    detail: &SeriesDetailView,
) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let title = format!(" {} - {} ", detail.series.label, view.range_label);
    frame.render_widget(
        Paragraph::new(Line::from(title.bold()))
            .alignment(Alignment::Center)
            .block(crate::bordered_block()),
        header,
    );
    draw_detail_content(frame, body, detail, &view.range_label);
    draw_detail_footer(frame, footer, app, detail);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, view: &SeriesPageView) {
    let total = view.details.len();
    let visible = visible_count(app, view);
    // Show the plain total unless a search or the membership filter is hiding rows, in which
    // case surface "visible of total" so the count matches what's on screen.
    let label = if visible == total {
        format!(
            " Series - {total} recurring item{} - {} ",
            if total == 1 { "" } else { "s" },
            view.range_label
        )
    } else {
        format!(" Series - {visible} of {total} - {} ", view.range_label)
    };
    let p = Paragraph::new(Line::from(label.bold()))
        .alignment(Alignment::Center)
        .block(crate::bordered_block());
    frame.render_widget(p, area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App, view: &SeriesPageView) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);
    draw_sidebar(frame, left, app, view);
    draw_detail(frame, right, app, view);
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App, view: &SeriesPageView) {
    let [search_area, list_area, filter_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .areas(area);
    draw_search(frame, search_area, app);
    draw_series_list(frame, list_area, app, view);
    draw_filter(frame, filter_area, app);
}

/// The membership filter, shown as a segmented control under the list. The active segment is
/// highlighted; `f` cycles it. Drives `visible_indices`.
fn draw_filter(frame: &mut Frame, area: Rect, app: &App) {
    let segment = |label: &str, active: bool| {
        if active {
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(crate::theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray))
        }
    };
    let line = Line::from(vec![
        Span::raw(" "),
        segment("Plans", app.series_filter == SeriesFilter::Plans),
        Span::raw(" "),
        segment("Ad-hoc", app.series_filter == SeriesFilter::AdHoc),
        Span::raw(" "),
        segment("Both", app.series_filter == SeriesFilter::Both),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(crate::titled_block(" Filter ")),
        area,
    );
}

fn draw_search(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.series_search_active;
    let block = crate::focusable_block(" Search ", focused);
    let line = if focused {
        Line::from(vec![
            Span::raw(" "),
            Span::raw(&app.series_search),
            Span::styled("|", Style::default().fg(crate::theme::CYAN)),
        ])
    } else if app.series_search.is_empty() {
        Line::from(Span::styled(
            " / to search",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::raw(" "),
            Span::raw(&app.series_search),
            Span::styled("  / edit", Style::default().fg(Color::DarkGray)),
        ])
    };
    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_series_list(frame: &mut Frame, area: Rect, app: &App, view: &SeriesPageView) {
    let rows = sidebar_rows(app, view);
    if rows.is_empty() {
        let msg = if view.details.is_empty() {
            " No series yet"
        } else {
            " No matching series"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(Color::DarkGray),
            )))
            .block(crate::selectable_block(" Series ", false)),
            area,
        );
        return;
    }

    let inner_width = crate::selectable_list_content_width(area);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            SidebarRow::Header(group) => ListItem::new(Line::from(Span::styled(
                group.label().to_string(),
                Style::default()
                    .fg(crate::theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ))),
            SidebarRow::Item(idx) => series_row(&view.details[*idx], inner_width),
        })
        .collect();

    let selected_row = selected_row_index(&rows, app.series_sel);
    let mut state = ListState::default();
    state.select(selected_row);

    // The right column is the range average (stats.avg), not a stored default — a series has
    // no default amount. A right-aligned title sits directly over that column so its meaning
    // can't be missed; the detail pane labels the same figure "avg".
    let block = crate::selectable_block(" Series ", true)
        .title_top(Line::from(" avg / mo ").right_aligned());
    let list = crate::selectable_list(items).block(block);
    frame.render_stateful_widget(list, area, &mut state);
}

fn series_row(detail: &SeriesDetailView, width: usize) -> ListItem<'static> {
    let avg = detail
        .stats
        .avg
        .map(|m| m.to_string())
        .unwrap_or_else(|| "--".into());
    let avg_width = 12;
    // Fill the full content width (label + right-aligned amount) with no extra leading space,
    // so the label's left edge lines up under the "Series" title and the amount's right edge
    // lines up under the right-aligned "avg / mo" title. The amount's own right-alignment
    // padding within `avg_width` keeps a gap between a long label and the number.
    let label_width = width.saturating_sub(avg_width).max(8);
    // The mode (auto/manual) shows in the Details pane, so it's not repeated here.
    let label = crate::truncate(&detail.series.label, label_width);
    ListItem::new(Line::from(vec![
        Span::raw(format!("{:<label_width$}", label)),
        Span::styled(
            format!("{:>avg_width$}", avg),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
}

fn selected_row_index(rows: &[SidebarRow], selected_item: usize) -> Option<usize> {
    let mut seen = 0;
    for (idx, row) in rows.iter().enumerate() {
        if matches!(row, SidebarRow::Item(_)) {
            if seen == selected_item {
                return Some(idx);
            }
            seen += 1;
        }
    }
    None
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App, view: &SeriesPageView) {
    let Some(detail) = selected_detail(app, view) else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Select a series",
                Style::default().fg(Color::DarkGray),
            )))
            .block(crate::titled_block(" Detail ")),
            area,
        );
        return;
    };

    draw_detail_content(frame, area, detail, &view.range_label);
}

fn draw_detail_content(
    frame: &mut Frame,
    area: Rect,
    detail: &SeriesDetailView,
    range_label: &str,
) {
    let [chart_area, mid_area, current_area] = Layout::vertical([
        Constraint::Min(8),
        Constraint::Length(10),
        Constraint::Length(4),
    ])
    .areas(area);

    // Three panels across the mid row: series metadata (type, and mode/period for envelopes)
    // on the left, the aligned stat column in the middle, and plan membership on the right.
    let [details_area, stats_area, plans_area] = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .areas(mid_area);

    draw_chart(frame, chart_area, detail, range_label);
    draw_details(frame, details_area, detail);
    draw_stats(frame, stats_area, detail);
    draw_plans_used(frame, plans_area, detail);
    draw_current(frame, current_area, detail);
}

fn draw_detail_footer(frame: &mut Frame, area: Rect, app: &App, detail: &SeriesDetailView) {
    let mut actions = vec![
        key(" l "),
        Span::raw(" label  "),
        key(" t "),
        Span::raw(" range"),
    ];
    if detail.series.kind == Kind::Envelope {
        actions.extend([Span::raw("  "), key(" m/p "), Span::raw(" mode/period")]);
    }
    let nav = Line::from(vec![
        key(" S "),
        Span::raw(" all series  "),
        key(" h "),
        Span::raw(" help  "),
        key(" P "),
        Span::raw(" plans  "),
        key(" Esc "),
        Span::raw(" back  "),
        key(" , "),
        Span::raw(" settings  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    let status = crate::footer_status(app);
    crate::draw_screen_footer(frame, area, Line::from(actions), nav, status.as_deref());
}

/// Series metadata: the type, plus mode and period for envelopes. This is the authoritative
/// place to read a series' mode/period, so toggling them (m/p) shows up here immediately.
fn draw_details(frame: &mut Frame, area: Rect, detail: &SeriesDetailView) {
    let type_label = match detail.group {
        SeriesGroup::Income => "Income",
        SeriesGroup::Expenses => "Expense",
        SeriesGroup::Envelopes => "Envelope",
    };
    // Keys here are short ("period" is the longest), so a tight column keeps the values close.
    let key_width = 8;
    let mut lines = vec![stat_row("type", type_label.to_string(), key_width)];
    if detail.series.kind == Kind::Envelope {
        // Envelope series always carry a concrete mode/period; None is unreachable here but
        // falls back to the calculation defaults for safety.
        let mode = match detail.series.mode {
            Some(Mode::Manual) => "manual",
            _ => "automatic",
        };
        let period = match detail.series.period_type {
            Some(PeriodType::Daily) => "daily",
            _ => "monthly",
        };
        lines.push(stat_row("mode", mode.to_string(), key_width));
        lines.push(stat_row("period", period.to_string(), key_width));
    }
    frame.render_widget(
        Paragraph::new(lines).block(crate::titled_block(" Details ")),
        area,
    );
}

fn draw_stats(frame: &mut Frame, area: Rect, detail: &SeriesDetailView) {
    let stats = &detail.stats;
    let delta = stats
        .avg_delta
        .map(format_signed_money)
        .unwrap_or_else(|| "--".into());

    // A single aligned column: fixed-width labels, values starting at the same x. This reads
    // cleanly at any panel width and lines up regardless of amount magnitude.
    // "planned avg" is the longest key, so pad one past it to keep a gap before the values.
    let key_width = 12;
    let lines = vec![
        stat_row("latest", format_money_opt(stats.latest), key_width),
        stat_row("avg", format_money_opt(stats.avg), key_width),
        stat_row("min", format_money_opt(stats.min), key_width),
        stat_row("max", format_money_opt(stats.max), key_width),
        stat_row("planned avg", format_money_opt(stats.planned_avg), key_width),
        stat_row("avg delta", delta, key_width),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(crate::titled_block(" Stats ")),
        area,
    );
}

/// One aligned `label   value` row: a readable (not too dim) label padded to `key_width`, then
/// the value in the terminal's default foreground for contrast. Each panel sizes `key_width` to
/// its own longest key so values sit just past it rather than a shared, over-wide column.
fn stat_row(label: &str, value: String, key_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {label:<key_width$}"),
            Style::default().fg(Color::Gray),
        ),
        Span::raw(value),
    ])
}

/// The plans that currently include this series, in their own panel beside the stats.
fn draw_plans_used(frame: &mut Frame, area: Rect, detail: &SeriesDetailView) {
    let block = crate::titled_block(" Used in plans ");
    let width = area.width.saturating_sub(3) as usize;
    let lines: Vec<Line> = if detail.plan_names.is_empty() {
        vec![Line::from(Span::styled(
            " Not in any plan",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        detail
            .plan_names
            .iter()
            .map(|name| Line::from(Span::raw(format!(" {}", crate::truncate(name, width)))))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_chart(frame: &mut Frame, area: Rect, detail: &SeriesDetailView, range_label: &str) {
    if detail.points.is_empty() {
        draw_empty_chart(frame, area, "No stamped months in this range", range_label);
        return;
    }
    if !detail.points.iter().any(|point| point.effective.is_some()) {
        draw_empty_chart(frame, area, "No trend data in this range", range_label);
        return;
    }

    let block = crate::titled_block(format!(" Amount - {range_label} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let bars = trend_bars(&detail.points);
    let (bar_width, bar_gap) = bar_chart_spacing(inner.width, bars.len());
    let chart_area = centered_bar_chart_area(inner, bars.len(), bar_width, bar_gap);
    let chart = BarChart::new(bars)
        .bar_width(bar_width)
        .bar_gap(bar_gap)
        .bar_style(Style::default().fg(crate::theme::CYAN))
        .value_style(
            Style::default()
                .fg(Color::Black)
                .bg(crate::theme::CYAN)
                .add_modifier(Modifier::BOLD),
        )
        .label_style(Style::default().fg(Color::Gray));
    frame.render_widget(chart, chart_area);
}

fn draw_empty_chart(frame: &mut Frame, area: Rect, msg: &str, range_label: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::DarkGray),
        )))
        .block(crate::titled_block(format!(" Amount - {range_label} "))),
        area,
    );
}

fn draw_current(frame: &mut Frame, area: Rect, detail: &SeriesDetailView) {
    let line = match &detail.current {
        Some(current) => {
            let count = format!(
                "{} occurrence{}",
                current.occurrence_count,
                if current.occurrence_count == 1 {
                    ""
                } else {
                    "s"
                }
            );
            let settlement = if detail.series.kind == Kind::Transaction {
                format!(
                    "  {} settled / {} open",
                    current.settled_count, current.unsettled_count
                )
            } else {
                String::new()
            };
            Line::from(vec![
                Span::raw(format!(" {}  ", current.month_label)),
                Span::raw(count),
                Span::raw(format!("  amount {}", current.amount)),
                Span::styled(settlement, Style::default().fg(Color::DarkGray)),
            ])
        }
        None => Line::from(Span::styled(
            " No current-month occurrence",
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(
        Paragraph::new(line).block(crate::titled_block(" Current month ")),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App, view: &SeriesPageView) {
    let envelope = selected_detail(app, view)
        .map(|detail| detail.series.kind == Kind::Envelope)
        .unwrap_or(false);
    let left = if envelope {
        Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" n "),
            Span::raw(" new  "),
            key(" l "),
            Span::raw(" label  "),
            key(" m/p "),
            Span::raw(" mode/period  "),
            key(" x "),
            Span::raw(" delete  "),
            key(" / "),
            Span::raw(" search  "),
            key(" f "),
            Span::raw(" filter  "),
            key(" t "),
            Span::raw(" range"),
        ])
    } else {
        Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" n "),
            Span::raw(" new  "),
            key(" l "),
            Span::raw(" label  "),
            key(" x "),
            Span::raw(" delete  "),
            key(" / "),
            Span::raw(" search  "),
            key(" f "),
            Span::raw(" filter  "),
            key(" t "),
            Span::raw(" range"),
        ])
    };
    let right = Line::from(vec![
        key(" h "),
        Span::raw(" help  "),
        key(" P "),
        Span::raw(" plans  "),
        key(" Esc "),
        Span::raw(" back  "),
        key(" , "),
        Span::raw(" settings  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    let status = crate::footer_status(app);
    crate::draw_screen_footer(frame, area, left, right, status.as_deref());
}

fn key(label: &str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::default().fg(Color::Black).bg(Color::Gray),
    )
}

fn selected_detail<'v>(app: &App, view: &'v SeriesPageView) -> Option<&'v SeriesDetailView> {
    let indices = visible_indices(app, view);
    indices
        .get(app.series_sel.min(indices.len().saturating_sub(1)))
        .and_then(|idx| view.details.get(*idx))
}

fn move_selection(app: &mut App, view: &SeriesPageView, delta: isize) {
    let count = visible_count(app, view);
    if count == 0 {
        app.series_sel = 0;
        return;
    }
    if delta < 0 {
        app.series_sel = app.series_sel.saturating_sub(delta.unsigned_abs());
    } else {
        app.series_sel = (app.series_sel + delta as usize).min(count - 1);
    }
}

fn sidebar_rows(app: &App, view: &SeriesPageView) -> Vec<SidebarRow> {
    let indices = visible_indices(app, view);
    let mut rows = Vec::new();
    let mut current_group = None;
    for idx in indices {
        let group = view.details[idx].group;
        if current_group != Some(group) {
            rows.push(SidebarRow::Header(group));
            current_group = Some(group);
        }
        rows.push(SidebarRow::Item(idx));
    }
    rows
}

fn visible_indices(app: &App, view: &SeriesPageView) -> Vec<usize> {
    let needle = app.series_search.trim().to_lowercase();
    view.details
        .iter()
        .enumerate()
        // Membership filter: a series is "in a plan" iff some plan lists it (plan_names).
        .filter(|(_, detail)| match app.series_filter {
            SeriesFilter::Both => true,
            SeriesFilter::Plans => !detail.plan_names.is_empty(),
            SeriesFilter::AdHoc => detail.plan_names.is_empty(),
        })
        .filter(|(_, detail)| {
            needle.is_empty() || detail.series.label.to_lowercase().contains(&needle)
        })
        .map(|(idx, _)| idx)
        .collect()
}

fn trend_bars(points: &[SeriesTrendPoint]) -> Vec<Bar<'static>> {
    points
        .iter()
        .map(|point| {
            let label = short_month_label(&point.month_label);
            match point.effective {
                Some(amount) => {
                    Bar::with_label(label, bar_value(amount)).text_value(compact_money(amount))
                }
                None => Bar::with_label(label, 0)
                    .text_value("")
                    .style(Style::default().fg(Color::DarkGray)),
            }
        })
        .collect()
}

fn bar_chart_spacing(width: u16, bar_count: usize) -> (u16, u16) {
    let count = u16::try_from(bar_count).unwrap_or(u16::MAX).max(1);
    let desired_bar_width = 6;
    let bar_gap = if width >= count.saturating_mul(desired_bar_width + 1) {
        1
    } else {
        0
    };
    let gap_width = count.saturating_sub(1).saturating_mul(bar_gap);
    let bar_width = width
        .saturating_sub(gap_width)
        .checked_div(count)
        .unwrap_or(1)
        .clamp(1, desired_bar_width);
    (bar_width, bar_gap)
}

fn centered_bar_chart_area(area: Rect, bar_count: usize, bar_width: u16, bar_gap: u16) -> Rect {
    let count = u16::try_from(bar_count).unwrap_or(u16::MAX);
    let content_width = count
        .saturating_mul(bar_width)
        .saturating_add(count.saturating_sub(1).saturating_mul(bar_gap));
    if content_width == 0 || content_width >= area.width {
        area
    } else {
        Rect {
            x: area.x + (area.width - content_width) / 2,
            width: content_width,
            ..area
        }
    }
}

fn bar_value(value: Money) -> u64 {
    value.cents().unsigned_abs()
}

fn short_month_label(label: &str) -> String {
    let Some((year, month)) = label.split_once('-') else {
        return label.to_string();
    };
    format!(
        "{} {}",
        month_name_from_number(month),
        year.chars().skip(2).collect::<String>()
    )
}

fn month_name_from_number(month: &str) -> &str {
    match month {
        "01" => "Jan",
        "02" => "Feb",
        "03" => "Mar",
        "04" => "Apr",
        "05" => "May",
        "06" => "Jun",
        "07" => "Jul",
        "08" => "Aug",
        "09" => "Sep",
        "10" => "Oct",
        "11" => "Nov",
        "12" => "Dec",
        _ => month,
    }
}

fn compact_money(value: Money) -> String {
    let currency = leeway::currency::active();
    let sign = if value.cents() < 0 { "-" } else { "" };
    // Work in whole major units (dollars/yen/...) so the k/m thresholds hold across
    // currencies regardless of how many minor-unit digits they carry.
    let major = value.cents().unsigned_abs() as f64 / currency.scale() as f64;
    let body = if major >= 1_000_000.0 {
        format!("{:.1}m", major / 1_000_000.0)
    } else if major >= 10_000.0 {
        format!("{:.0}k", major / 1_000.0)
    } else if major >= 1_000.0 {
        format!("{:.1}k", major / 1_000.0)
    } else {
        format!("{}", major as u64)
    };
    currency.wrap(sign, &body)
}

fn format_money_opt(value: Option<Money>) -> String {
    value.map(|m| m.to_string()).unwrap_or_else(|| "--".into())
}

fn format_signed_money(value: Money) -> String {
    if value.cents() > 0 {
        format!("+{}", value)
    } else {
        value.to_string()
    }
}
