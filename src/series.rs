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
use ratatui::widgets::{ListItem, ListState, Paragraph};

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
    draw_detail_content(frame, body, app, detail, &view.range_label);
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
        Layout::horizontal([Constraint::Percentage(33), Constraint::Percentage(67)]).areas(area);
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
    let selected_row = selected_row_index(&rows, app.series_sel);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| match row {
            SidebarRow::Header(group) => ListItem::new(Line::from(Span::styled(
                group.label().to_string(),
                Style::default()
                    .fg(crate::theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ))),
            SidebarRow::Item(idx) => {
                series_row(&view.details[*idx], inner_width, selected_row == Some(i))
            }
        })
        .collect();

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

/// `selected` says whether the selection band sits behind this row; the avg column
/// fades less there so it stays readable over the band.
fn series_row(detail: &SeriesDetailView, width: usize, selected: bool) -> ListItem<'static> {
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
            Style::default().fg(crate::theme::muted(selected)),
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

    draw_detail_content(frame, area, app, detail, &view.range_label);
}

fn draw_detail_content(
    frame: &mut Frame,
    area: Rect,
    app: &App,
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

    draw_chart(frame, chart_area, app, detail, range_label);
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
        stat_row(
            "planned avg",
            format_money_opt(stats.planned_avg),
            key_width,
        ),
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

fn draw_chart(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    detail: &SeriesDetailView,
    range_label: &str,
) {
    if detail.points.is_empty() {
        draw_empty_chart(frame, area, "No stamped months in this range", range_label);
        return;
    }
    // Needs at least one non-zero amount: an all-$0 (or all-missing) range has no
    // scale, which would otherwise render every bar as a full-height empty track.
    if !detail
        .points
        .iter()
        .any(|point| point.effective.is_some_and(|m| m.cents() != 0))
    {
        draw_empty_chart(frame, area, "No trend data in this range", range_label);
        return;
    }

    let block = crate::titled_block(format!(" Amount - {range_label} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Animated bar heights when we're paging between series; falls back to the
    // static per-month heights when the tween's shape doesn't match this series
    // (e.g. the first frame before the loop has synced it).
    let heights = app.series_chart_anim.heights(app.frame_now);
    let heights = (heights.len() == detail.points.len()).then_some(heights.as_slice());

    let lines = segmented_chart_lines(&detail.points, heights, inner.width, inner.height);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Build the trend chart as lines of styled cells, echoing the dashboard's
/// envelope meters (`MAUVE` fill over a subdued `CHART_TRACK` gap) but drawn vertically:
/// each month is a column of `▮` segments, mauve from the baseline up to its
/// value and subdued above. A left gutter carries value ticks ($0, a midpoint,
/// the peak) beside a `│`/`┤`/`└` axis; month labels ride the baseline and a
/// compact value sits under each bar.
fn segmented_chart_lines(
    points: &[SeriesTrendPoint],
    heights: Option<&[f64]>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    const SEG: &str = "▮";
    let inner_w = width as usize;
    let inner_h = height as usize;
    if inner_w == 0 || inner_h == 0 {
        return Vec::new();
    }

    // Peak drives the shared vertical scale; absolute cents so a negative series
    // still charts by magnitude (mirrors the old `bar_value`).
    let max_cents = chart_peak_cents(points);

    // The gutter holds the tick labels. Pin it to a fixed default so it doesn't
    // shift as you page between series of different magnitudes — a width of 5
    // fits the common range (`$0`, `$450`, `$3.0k`, `$120k`, `$1.0m`). It only
    // grows for genuinely larger labels (so nothing clips) and shrinks only when
    // a narrow pane can't spare the room.
    const DEFAULT_GUTTER_WIDTH: usize = 5;
    let max_label = compact_money(Money(max_cents as i64));
    let mid_label = compact_money(Money((max_cents / 2) as i64));
    let zero_label = compact_money(Money::ZERO);
    let gutter_w = max_label
        .chars()
        .count()
        .max(mid_label.chars().count())
        .max(zero_label.chars().count())
        .max(DEFAULT_GUTTER_WIDTH)
        .min(inner_w / 2);

    // Left prefix = gutter + a space + the 1-col axis; the plot fills the rest.
    let axis_col = gutter_w + 2;
    let plot_width = inner_w.saturating_sub(axis_col);
    // Two rows sit below the bars: the baseline (with month labels) and values.
    let bar_rows = inner_h.saturating_sub(2);

    let count = points.len();
    if plot_width == 0 || count == 0 {
        return Vec::new();
    }
    let (bar_width, bar_gap) = bar_chart_spacing(plot_width as u16, count);
    let bar_width = bar_width as usize;
    let bar_gap = bar_gap as usize;
    let pitch = bar_width + bar_gap;
    let content_width = count * bar_width + count.saturating_sub(1) * bar_gap;
    let left_pad = plot_width.saturating_sub(content_width) / 2;

    // Filled-cell count per present month; `None` months stay blank gaps. When an
    // animated height is supplied for a bar we scale off that (a tween 0.0..=1.0);
    // otherwise we fall back to the month's own peak-relative fraction.
    let fills: Vec<Option<usize>> = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            p.effective.map(|m| match heights.and_then(|h| h.get(i)) {
                Some(&frac) => {
                    ((frac.clamp(0.0, 1.0) * bar_rows as f64).round() as usize).min(bar_rows)
                }
                None => bar_fill_rows(m.cents().unsigned_abs(), max_cents, bar_rows),
            })
        })
        .collect();

    let axis_style = Style::default().fg(Color::Gray);
    let mid_row = if bar_rows >= 2 {
        Some(bar_rows / 2)
    } else {
        None
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);

    // Bar rows, peak (top) to baseline (bottom).
    for r in 0..bar_rows {
        let is_tick = r == 0 || Some(r) == mid_row;
        let tick = if r == 0 {
            max_label.as_str()
        } else if Some(r) == mid_row {
            mid_label.as_str()
        } else {
            ""
        };
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(format!("{tick:>gutter_w$} "), axis_style),
            Span::styled(if is_tick { "┤" } else { "│" }.to_string(), axis_style),
            Span::raw(" ".repeat(left_pad)),
        ];
        for (i, fill) in fills.iter().enumerate() {
            match fill {
                // Bottom `filled` rows are the mauve fill; the rest is track.
                Some(filled) => {
                    let color = if r >= bar_rows - filled {
                        crate::theme::MAUVE
                    } else {
                        crate::theme::CHART_TRACK
                    };
                    spans.push(Span::styled(
                        SEG.repeat(bar_width),
                        Style::default().fg(color),
                    ));
                }
                None => spans.push(Span::raw(" ".repeat(bar_width))),
            }
            if i + 1 < count {
                spans.push(Span::raw(" ".repeat(bar_gap)));
            }
        }
        lines.push(Line::from(spans));
    }

    // Baseline: the `└` corner and a `─` axis carrying centered month labels.
    let mut baseline = vec!['─'; plot_width];
    for (i, p) in points.iter().enumerate() {
        write_centered(
            &mut baseline,
            left_pad + i * pitch,
            bar_width,
            &short_month_label(&p.month_label),
        );
    }
    lines.push(Line::from(vec![
        Span::styled(format!("{zero_label:>gutter_w$} └"), axis_style),
        Span::styled(baseline.into_iter().collect::<String>(), axis_style),
    ]));

    // Values: a compact amount under each present bar. Skipped when it can't fit
    // the bar width, so a narrow pane shows nothing rather than a clipped "$1.".
    let mut values = vec![' '; plot_width];
    for (i, p) in points.iter().enumerate() {
        if let Some(m) = p.effective {
            let value = compact_money(m);
            if value.chars().count() <= bar_width {
                write_centered(&mut values, left_pad + i * pitch, bar_width, &value);
            }
        }
    }
    lines.push(Line::from(vec![
        Span::raw(format!("{:>axis_col$}", "")),
        Span::styled(
            values.into_iter().collect::<String>(),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    lines
}

/// Filled cell count for a vertical bar of `rows` showing `value / max`. Same
/// fraction-rounded shape as the dashboard's `meter_fill`, applied to cents.
fn bar_fill_rows(value: u64, max: u64, rows: usize) -> usize {
    if max == 0 || rows == 0 {
        return 0;
    }
    let frac = (value as f64 / max as f64).clamp(0.0, 1.0);
    ((frac * rows as f64).round() as usize).min(rows)
}

/// Overwrite `text` (truncated to `field`) centered within `buf[start..start + field]`.
fn write_centered(buf: &mut [char], start: usize, field: usize, text: &str) {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len().min(field);
    let off = start + (field - len) / 2;
    for (k, ch) in chars.iter().take(len).enumerate() {
        if let Some(slot) = buf.get_mut(off + k) {
            *slot = *ch;
        }
    }
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

pub fn selected_detail<'v>(app: &App, view: &'v SeriesPageView) -> Option<&'v SeriesDetailView> {
    let indices = visible_indices(app, view);
    indices
        .get(app.series_sel.min(indices.len().saturating_sub(1)))
        .and_then(|idx| view.details.get(*idx))
}

/// Normalized bar heights (`0.0..=1.0`), one per month, for driving the chart's
/// tween. Uses the same peak-relative fill math as the chart: `0.0` for gap
/// months and when the series has no positive amount.
pub fn chart_targets(detail: &SeriesDetailView) -> Vec<f64> {
    let max = chart_peak_cents(&detail.points);
    detail
        .points
        .iter()
        .map(|p| match p.effective {
            Some(m) if max > 0 => (m.cents().unsigned_abs() as f64 / max as f64).clamp(0.0, 1.0),
            _ => 0.0,
        })
        .collect()
}

/// Identity of a chart selection: `Some(series_id)` when there's data to plot
/// (a change animates the bars), `None` for an empty or all-$0 range so the
/// tween resets rather than animating from a stale shape.
pub fn chart_key(detail: &SeriesDetailView) -> Option<&str> {
    detail
        .points
        .iter()
        .any(|p| p.effective.is_some_and(|m| m.cents() != 0))
        .then_some(detail.series.id.as_str())
}

/// Peak magnitude (absolute cents) across the charted months; the shared scale.
fn chart_peak_cents(points: &[SeriesTrendPoint]) -> u64 {
    points
        .iter()
        .filter_map(|p| p.effective)
        .map(|m| m.cents().unsigned_abs())
        .max()
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(label: &str, cents: Option<i64>) -> SeriesTrendPoint {
        SeriesTrendPoint {
            month_label: label.to_string(),
            effective: cents.map(Money),
            planned: None,
            occurrence_count: 1,
            settled_count: 0,
            unsettled_count: 0,
        }
    }

    /// Flatten a rendered line back into a plain string for content assertions.
    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Count the mauve-filled bar cells across all rows (each filled row of a bar
    /// is one `▮` span in the fill colour). For a single-bar chart this is the
    /// bar's height in cells.
    fn mauve_fill_cells(lines: &[Line]) -> usize {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content.contains('▮') && s.style.fg == Some(crate::theme::MAUVE))
            .count()
    }

    #[test]
    fn animated_heights_drive_the_mauve_fill() {
        // The tween feeds normalized heights straight into the fill: a bar at 0.0
        // has no mauve (all track), a bar at 1.0 is full, so mid-tween sits between.
        let points = vec![pt("2026-01", Some(100000))];
        let empty = segmented_chart_lines(&points, Some(&[0.0]), 20, 12);
        let half = segmented_chart_lines(&points, Some(&[0.5]), 20, 12);
        let full = segmented_chart_lines(&points, Some(&[1.0]), 20, 12);

        assert_eq!(mauve_fill_cells(&empty), 0);
        assert!(mauve_fill_cells(&half) > 0);
        assert!(mauve_fill_cells(&full) > mauve_fill_cells(&half));
    }

    #[test]
    fn segmented_chart_draws_axis_bars_labels_and_values() {
        let points = vec![
            pt("2026-01", Some(120000)),
            pt("2026-02", None), // missing month: a genuine gap
            pt("2026-04", Some(300000)),
        ];
        let lines = segmented_chart_lines(&points, None, 48, 12);
        let top = text(&lines[0]);
        // Peak tick + segmented bars at the top row.
        assert!(top.starts_with("$3.0k ┤"), "top row: {top:?}");
        assert!(top.contains('▮'));
        // Baseline carries the $0 tick, the corner, and centered month labels.
        let baseline = text(&lines[lines.len() - 2]);
        assert!(baseline.contains("$0 └"), "baseline: {baseline:?}");
        assert!(baseline.contains("Jan 26") && baseline.contains("Apr 26"));
        // Values sit under the present bars only.
        let values = text(&lines[lines.len() - 1]);
        assert!(values.contains("$1.2k") && values.contains("$3.0k"));
    }

    #[test]
    fn missing_month_is_a_blank_gap_not_a_bar() {
        // A single missing month between two present ones must leave its column
        // clear rather than drawing a zero-height (or full-track) bar.
        let points = vec![
            pt("2026-01", Some(100000)),
            pt("2026-02", None),
            pt("2026-03", Some(100000)),
        ];
        let lines = segmented_chart_lines(&points, None, 40, 10);
        // Every bar row must contain a run of spaces wide enough to be the gap.
        let bar_rows = &lines[..lines.len() - 2];
        assert!(
            bar_rows.iter().all(|l| text(l).contains("   ")),
            "expected a blank gap column in each bar row"
        );
    }

    #[test]
    fn tiny_frames_do_not_panic() {
        let points = vec![pt("2026-01", Some(100000)), pt("2026-02", Some(50000))];
        for (w, h) in [(0, 0), (1, 1), (3, 2), (8, 3), (2, 12)] {
            let _ = segmented_chart_lines(&points, None, w, h);
        }
    }

    #[test]
    fn bar_fill_rows_scales_and_clamps() {
        assert_eq!(bar_fill_rows(0, 100, 10), 0);
        assert_eq!(bar_fill_rows(100, 100, 10), 10);
        assert_eq!(bar_fill_rows(50, 100, 10), 5);
        assert_eq!(bar_fill_rows(999, 100, 10), 10); // over-max clamps to full
        assert_eq!(bar_fill_rows(50, 0, 10), 0); // no peak → empty
        assert_eq!(bar_fill_rows(50, 100, 0), 0); // no rows → empty
    }
}
