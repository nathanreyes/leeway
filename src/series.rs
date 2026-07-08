//! The Series screen: recurring-item management plus range-scoped trend stats.

use crate::{App, ChoiceOption, ModalAction, PromptKind, Screen};
use anyhow::Result;
use ballpark::models::{Kind, Mode, PeriodType};
use ballpark::money::Money;
use ballpark::ops;
use ballpark::view::{
    SeriesDetailView, SeriesGroup, SeriesPageView, SeriesTimeRange, SeriesTrendPoint,
};
use chrono::{Datelike, NaiveDate};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, ListItem, ListState, Paragraph};

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
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Esc | KeyCode::Char('d') => app.screen = Screen::Dashboard,
        KeyCode::Char('P') => app.screen = Screen::Plans,
        KeyCode::Char('/') => app.series_search_active = true,
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, view, 1),
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, view, -1),
        KeyCode::Char('t') => open_range_choice(app, today),
        KeyCode::Char('r') => {
            if let Some(detail) = selected_detail(app, view) {
                let label = detail.series.label.clone();
                let id = detail.series.id.clone();
                app.open_text_replace_on_type(
                    "Series label (shared across plans)",
                    label,
                    PromptKind::SeriesLabel { series_id: id },
                );
            }
        }
        KeyCode::Char('c') => {
            if let Some(detail) = selected_detail(app, view) {
                let category = detail.series.category.clone().unwrap_or_default();
                let id = detail.series.id.clone();
                app.open_text_replace_on_type(
                    "Series category",
                    category,
                    PromptKind::SeriesCategory { series_id: id },
                );
            }
        }
        KeyCode::Char('m') => {
            if let Some(detail) = selected_detail(app, view) {
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
            }
        }
        KeyCode::Char('p') => {
            if let Some(detail) = selected_detail(app, view) {
                if detail.series.kind == Kind::Envelope {
                    let next = match detail.series.period_type {
                        Some(PeriodType::Daily) => PeriodType::Weekly,
                        Some(PeriodType::Weekly) => PeriodType::Monthly,
                        Some(PeriodType::Monthly) | None => PeriodType::Daily,
                    };
                    ops::set_series_period(&app.conn, &detail.series.id, next)?;
                    app.status =
                        Some("Period changed (affects all plans using this series)".into());
                } else {
                    app.status = Some("Period applies to envelopes".into());
                }
            }
        }
        _ => {}
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

pub fn draw(frame: &mut Frame, app: &App, view: &SeriesPageView) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app, view);
    draw_body(frame, body, app, view);
    draw_footer(frame, footer, app, view);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, view: &SeriesPageView) {
    let visible = visible_count(app, view);
    let label = if app.series_search.is_empty() {
        format!(
            " Series - {} recurring item{} - {} ",
            view.details.len(),
            if view.details.len() == 1 { "" } else { "s" },
            view.range_label
        )
    } else {
        format!(
            " Series - {} of {} matches - {} ",
            visible,
            view.details.len(),
            view.range_label
        )
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
    let [search_area, list_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    draw_search(frame, search_area, app);
    draw_series_list(frame, list_area, app, view);
}

fn draw_search(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.series_search_active;
    let block = crate::focusable_block(" Search ", focused);
    let line = if focused {
        Line::from(vec![
            Span::raw(" "),
            Span::raw(&app.series_search),
            Span::styled("|", Style::default().fg(Color::Cyan)),
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
                format!(" {}", group.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))),
            SidebarRow::Item(idx) => series_row(&view.details[*idx], inner_width),
        })
        .collect();

    let selected_row = selected_row_index(&rows, app.series_sel);
    let mut state = ListState::default();
    state.select(selected_row);

    let list = crate::selectable_list(items).block(crate::selectable_block(" Series ", true));
    frame.render_stateful_widget(list, area, &mut state);
}

fn series_row(detail: &SeriesDetailView, width: usize) -> ListItem<'static> {
    let avg = detail
        .stats
        .avg
        .map(|m| m.to_string())
        .unwrap_or_else(|| "--".into());
    let avg_width = 12;
    let label_width = width.saturating_sub(avg_width + 2).max(8);
    let label = match detail.group {
        SeriesGroup::Envelopes => {
            let meta = envelope_meta(detail);
            crate::truncate(&format!("{} {}", detail.series.label, meta), label_width)
        }
        _ => crate::truncate(&detail.series.label, label_width),
    };
    ListItem::new(Line::from(vec![
        Span::raw(format!(" {:<label_width$}", label)),
        Span::styled(
            format!("{:>avg_width$}", avg),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
}

fn envelope_meta(detail: &SeriesDetailView) -> &'static str {
    match detail.series.mode {
        Some(Mode::Manual) => "manual",
        _ => "auto",
    }
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

    let [summary_area, chart_area, current_area] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(8),
        Constraint::Length(4),
    ])
    .areas(area);

    draw_summary(frame, summary_area, detail);
    draw_chart(frame, chart_area, detail, &view.range_label);
    draw_current(frame, current_area, detail);
}

fn draw_summary(frame: &mut Frame, area: Rect, detail: &SeriesDetailView) {
    let kind = match detail.group {
        SeriesGroup::Income => "income",
        SeriesGroup::Expenses => "expense",
        SeriesGroup::Envelopes => "envelope",
    };
    let category = detail.series.category.as_deref().unwrap_or("--");
    let plans = if detail.plan_names.is_empty() {
        "--".into()
    } else {
        detail.plan_names.join(", ")
    };
    let stats = &detail.stats;
    let delta = stats
        .avg_delta
        .map(format_signed_money)
        .unwrap_or_else(|| "--".into());

    let lines = vec![
        Line::from(vec![
            Span::styled(
                crate::truncate(&detail.series.label, 32),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {kind}"), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::styled("category: ", Style::default().fg(Color::DarkGray)),
            Span::raw(crate::truncate(category, 22)),
            Span::raw("  "),
            Span::styled("used in plans: ", Style::default().fg(Color::DarkGray)),
            Span::raw(crate::truncate(&plans, 32)),
        ]),
        stat_pair("latest", stats.latest, "avg", stats.avg),
        stat_pair("min", stats.min, "max", stats.max),
        Line::from(vec![
            Span::styled("planned avg: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_money_opt(stats.planned_avg)),
            Span::raw("  "),
            Span::styled("avg delta: ", Style::default().fg(Color::DarkGray)),
            Span::raw(delta),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(crate::titled_block(" Summary ")),
        area,
    );
}

fn stat_pair(
    left_label: &str,
    left: Option<Money>,
    right_label: &str,
    right: Option<Money>,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{left_label}: "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format!("{:<14}", format_money_opt(left))),
        Span::styled(
            format!("{right_label}: "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format_money_opt(right)),
    ])
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

    let effective_segments = chart_segments(&detail.points, |point| point.effective);
    let planned_segments = chart_segments(&detail.points, |point| point.planned);
    let values: Vec<f64> = detail
        .points
        .iter()
        .flat_map(|point| [point.effective, point.planned])
        .flatten()
        .map(money_as_dollars)
        .collect();
    let (y_min, y_max) = y_bounds(&values);
    let x_max = detail.points.len().saturating_sub(1).max(1) as f64;

    let mut datasets = Vec::new();
    for (idx, segment) in effective_segments.iter().enumerate() {
        let mut dataset = Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Color::Cyan)
            .data(segment.as_slice());
        if idx == 0 {
            dataset = dataset.name("effective");
        }
        datasets.push(dataset);
    }
    for (idx, segment) in planned_segments.iter().enumerate() {
        let mut dataset = Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Color::DarkGray)
            .data(segment.as_slice());
        if idx == 0 {
            dataset = dataset.name("planned");
        }
        datasets.push(dataset);
    }

    let x_axis = Axis::default()
        .title("month")
        .bounds([0.0, x_max])
        .labels(axis_month_labels(&detail.points));
    let y_axis = Axis::default()
        .title("amount")
        .bounds([y_min, y_max])
        .labels(axis_money_labels(y_min, y_max));
    let chart = Chart::new(datasets)
        .block(crate::titled_block(format!(" Amount - {range_label} ")))
        .x_axis(x_axis)
        .y_axis(y_axis);
    frame.render_widget(chart, area);
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
            key(" / "),
            Span::raw(" search  "),
            key(" t "),
            Span::raw(" range  "),
            key(" r/c "),
            Span::raw(" label/category  "),
            key(" m/p "),
            Span::raw(" mode/period"),
        ])
    } else {
        Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" / "),
            Span::raw(" search  "),
            key(" t "),
            Span::raw(" range  "),
            key(" r/c "),
            Span::raw(" label/category"),
        ])
    };
    let right = Line::from(vec![
        key(" d "),
        Span::raw(" dashboard  "),
        key(" P "),
        Span::raw(" plans  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    crate::draw_split_status_footer(frame, area, left, right, &app.status);
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
        .filter(|(_, detail)| {
            needle.is_empty()
                || detail.series.label.to_lowercase().contains(&needle)
                || detail
                    .series
                    .category
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&needle)
        })
        .map(|(idx, _)| idx)
        .collect()
}

fn chart_segments(
    points: &[SeriesTrendPoint],
    value: fn(&SeriesTrendPoint) -> Option<Money>,
) -> Vec<Vec<(f64, f64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for (idx, point) in points.iter().enumerate() {
        if let Some(amount) = value(point) {
            current.push((idx as f64, money_as_dollars(amount)));
        } else if !current.is_empty() {
            segments.push(current);
            current = Vec::new();
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn y_bounds(values: &[f64]) -> (f64, f64) {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if (max - min).abs() < f64::EPSILON {
        let pad = (max.abs() * 0.10).max(1.0);
        (min - pad, max + pad)
    } else {
        let pad = ((max - min) * 0.15).max(1.0);
        (min - pad, max + pad)
    }
}

fn axis_month_labels(points: &[SeriesTrendPoint]) -> Vec<Line<'static>> {
    if points.is_empty() {
        return vec!["".into(), "".into()];
    }
    let mid = points.len() / 2;
    vec![
        short_month_label(&points[0].month_label).into(),
        short_month_label(&points[mid].month_label).into(),
        short_month_label(&points[points.len() - 1].month_label).into(),
    ]
}

fn short_month_label(label: &str) -> String {
    let Some((year, month)) = label.split_once('-') else {
        return label.to_string();
    };
    let month = match month {
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
    };
    format!("{month} {}", year.chars().skip(2).collect::<String>())
}

fn axis_money_labels(min: f64, max: f64) -> Vec<Line<'static>> {
    let mid = min + (max - min) / 2.0;
    vec![
        compact_dollars(min).into(),
        compact_dollars(mid).into(),
        compact_dollars(max).into(),
    ]
}

fn compact_dollars(value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let abs = value.abs();
    if abs >= 1000.0 {
        format!("{sign}${:.1}k", abs / 1000.0)
    } else {
        format!("{sign}${:.0}", abs)
    }
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

fn money_as_dollars(value: Money) -> f64 {
    value.cents() as f64 / 100.0
}
