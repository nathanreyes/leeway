//! The shared budget workspace: months and plans in one sidebar, with the selected
//! target rendered in the detail area.

use crate::{App, BudgetTarget, DashFocus, PlanFocus, PromptKind};
use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use leeway::models::{Month, Plan, PlanEntry};
use leeway::queries::PlanSummary;
use leeway::view::MonthView;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState};

pub fn draw_month(
    frame: &mut Frame,
    app: &App,
    months: &[Month],
    plans: &[PlanSummary],
    view: &Option<MonthView>,
    today: NaiveDate,
) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(frame.area());
    let [sidebar, detail] = workspace_columns(body);
    draw_sidebar(frame, sidebar, app, months, plans, today);
    crate::dashboard::draw_detail(frame, detail, app, view);
    draw_footer(frame, footer, app, plans.is_empty());
}

pub fn draw_plan(
    frame: &mut Frame,
    app: &App,
    months: &[Month],
    plans: &[PlanSummary],
    plan: Option<&Plan>,
    entries: &[PlanEntry],
    today: NaiveDate,
) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(frame.area());
    let [sidebar, detail] = workspace_columns(body);
    draw_sidebar(frame, sidebar, app, months, plans, today);
    crate::plans::draw_detail(frame, detail, app, plan, entries);
    draw_footer(frame, footer, app, plans.is_empty());
}

fn workspace_columns(area: Rect) -> [Rect; 2] {
    let sidebar_width = if area.width >= 90 {
        28
    } else {
        (area.width / 3).clamp(16, 24)
    };
    Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(0)]).areas(area)
}

#[derive(Clone)]
struct SidebarRow {
    target: Option<BudgetTarget>,
    line: Line<'static>,
}

fn sidebar_rows(
    target: &BudgetTarget,
    months: &[Month],
    plans: &[PlanSummary],
    today: NaiveDate,
) -> Vec<SidebarRow> {
    let mut labels: Vec<String> = months.iter().map(|month| month.label.clone()).collect();
    let current = format!("{:04}-{:02}", today.year(), today.month());
    if !labels.contains(&current) {
        labels.push(current.clone());
    }
    if let BudgetTarget::Month { year, month } = target {
        let selected = format!("{year:04}-{month:02}");
        if !labels.contains(&selected) {
            labels.push(selected);
        }
    }
    labels.sort_by(|a, b| b.cmp(a));

    let mut rows = vec![SidebarRow {
        target: None,
        line: Line::from(Span::styled(
            "MONTHS",
            Style::default()
                .fg(crate::theme::CYAN)
                .add_modifier(Modifier::BOLD),
        )),
    }];
    rows.extend(labels.into_iter().filter_map(|label| {
        let (year, month) = parse_label(&label)?;
        let marker = if label == current { " *" } else { "" };
        Some(SidebarRow {
            target: Some(BudgetTarget::Month { year, month }),
            line: Line::from(vec![
                Span::raw(label),
                Span::styled(marker, Style::default().fg(crate::theme::CYAN)),
            ]),
        })
    }));

    rows.push(SidebarRow {
        target: None,
        line: Line::raw(""),
    });
    rows.push(SidebarRow {
        target: None,
        line: Line::from(Span::styled(
            "PLANS",
            Style::default()
                .fg(crate::theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        )),
    });
    if plans.is_empty() {
        rows.push(SidebarRow {
            target: None,
            line: Line::from(Span::styled(
                "No plans — n to create",
                Style::default().fg(Color::DarkGray),
            )),
        });
    } else {
        rows.extend(plans.iter().map(|summary| SidebarRow {
            target: Some(BudgetTarget::Plan {
                plan_id: summary.plan.id.clone(),
            }),
            line: Line::raw(crate::truncate(&summary.plan.name, 24)),
        }));
    }
    rows
}

fn draw_sidebar(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    months: &[Month],
    plans: &[PlanSummary],
    today: NaiveDate,
) {
    let rows = sidebar_rows(&app.budget_target, months, plans, today);
    let selected = rows
        .iter()
        .position(|row| row.target.as_ref() == Some(&app.budget_target));
    let items = rows
        .iter()
        .map(|row| ListItem::new(row.line.clone()))
        .collect();
    let focused = sidebar_focused(app);
    let mut state = ListState::default();
    state.select(selected);
    let list = crate::selectable_list(items).block(crate::selectable_block(" Budgets ", focused));
    frame.render_stateful_widget(list, area, &mut state);
    crate::render_list_scrollbar(frame, area, rows.len(), state.offset(), focused);
}

pub fn sidebar_focused(app: &App) -> bool {
    match app.budget_target {
        BudgetTarget::Month { .. } => app.dash_focus == DashFocus::Header,
        BudgetTarget::Plan { .. } => app.plan_focus == PlanFocus::List,
    }
}

pub enum SidebarDetail<'a> {
    Month(Option<&'a MonthView>),
    Plan {
        plan: Option<&'a Plan>,
        entries: &'a [PlanEntry],
    },
}

pub fn handle_sidebar_key(
    app: &mut App,
    key: KeyEvent,
    months: &[Month],
    plans: &[PlanSummary],
    today: NaiveDate,
    detail: SidebarDetail<'_>,
) -> Result<()> {
    let (month_view, plan, entries) = match detail {
        SidebarDetail::Month(view) => (view, None, &[][..]),
        SidebarDetail::Plan { plan, entries } => (None, plan, entries),
    };
    app.status = None;
    match key.code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => move_target(app, 1, months, plans, today),
        KeyCode::Char('k') | KeyCode::Up => move_target(app, -1, months, plans, today),
        KeyCode::Char('m') => {
            let target = BudgetTarget::Month {
                year: app.viewed_year,
                month: app.viewed_month,
            };
            select_target(app, target, plans);
        }
        KeyCode::Char('p') => select_last_plan(app, plans),
        KeyCode::Char('g') if matches!(app.budget_target, BudgetTarget::Month { .. }) => app
            .open_text(
                "Go to month (YYYY-MM)",
                format!("{:04}-{:02}", app.viewed_year, app.viewed_month),
                PromptKind::GoToMonth,
            ),
        KeyCode::Tab => match app.budget_target {
            BudgetTarget::Month { .. } => {
                if let Some(view) = month_view {
                    app.dash_focus = if view.is_current {
                        DashFocus::Accounts
                    } else {
                        DashFocus::Income
                    };
                }
            }
            BudgetTarget::Plan { .. } if plan.is_some() => app.plan_focus = PlanFocus::Income,
            BudgetTarget::Plan { .. } => {}
        },
        KeyCode::BackTab => match app.budget_target {
            BudgetTarget::Month { .. } if month_view.is_some() => {
                app.dash_focus = DashFocus::Envelopes
            }
            BudgetTarget::Plan { .. } if plan.is_some() => app.plan_focus = PlanFocus::Envelopes,
            _ => {}
        },
        KeyCode::Char('n') if plans.is_empty() => {
            app.open_text("New plan name", "", PromptKind::NewPlan)
        }
        _ => {
            if matches!(app.budget_target, BudgetTarget::Plan { .. }) {
                crate::plans::handle_key(app, key, plans, plan, entries)?;
            }
        }
    }
    Ok(())
}

fn move_target(
    app: &mut App,
    delta: i32,
    months: &[Month],
    plans: &[PlanSummary],
    today: NaiveDate,
) {
    let targets: Vec<BudgetTarget> = sidebar_rows(&app.budget_target, months, plans, today)
        .into_iter()
        .filter_map(|row| row.target)
        .collect();
    let Some(current) = targets
        .iter()
        .position(|target| target == &app.budget_target)
    else {
        return;
    };
    let next = (current as i32 + delta).clamp(0, targets.len().saturating_sub(1) as i32) as usize;
    select_target(app, targets[next].clone(), plans);
}

pub fn select_target(app: &mut App, target: BudgetTarget, plans: &[PlanSummary]) {
    match &target {
        BudgetTarget::Month { year, month } => {
            app.viewed_year = *year;
            app.viewed_month = *month;
            app.dash_focus = DashFocus::Header;
        }
        BudgetTarget::Plan { plan_id } => {
            app.last_plan_id = Some(plan_id.clone());
            if let Some(index) = plans.iter().position(|summary| summary.plan.id == *plan_id) {
                app.plans_sel = index;
            }
            app.plan_focus = PlanFocus::List;
        }
    }
    app.budget_target = target;
}

fn select_last_plan(app: &mut App, plans: &[PlanSummary]) {
    let plan_id = app
        .last_plan_id
        .as_ref()
        .filter(|id| plans.iter().any(|summary| summary.plan.id == id.as_str()))
        .cloned()
        .or_else(|| plans.first().map(|summary| summary.plan.id.clone()));
    if let Some(plan_id) = plan_id {
        select_target(app, BudgetTarget::Plan { plan_id }, plans);
    }
}

fn parse_label(label: &str) -> Option<(i32, u32)> {
    let (year, month) = label.split_once('-')?;
    Some((year.parse().ok()?, month.parse().ok()?))
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App, no_plans: bool) {
    let local = if sidebar_focused(app) {
        match app.budget_target {
            BudgetTarget::Month { .. } => {
                let mut spans = vec![
                    key(" j/k "),
                    Span::raw(" budget  "),
                    key(" p "),
                    Span::raw(" plans  "),
                    key(" g "),
                    Span::raw(" go to month"),
                ];
                if no_plans {
                    spans.extend([key(" n "), Span::raw(" new plan")]);
                }
                Line::from(spans)
            }
            BudgetTarget::Plan { .. } => Line::from(vec![
                key(" j/k "),
                Span::raw(" budget  "),
                key(" m "),
                Span::raw(" months  "),
                key(" n "),
                Span::raw(" new  "),
                key(" l "),
                Span::raw(" label  "),
                key(" s "),
                Span::raw(" stamp  "),
                key(" x "),
                Span::raw(" delete"),
            ]),
        }
    } else {
        match app.budget_target {
            BudgetTarget::Month { .. } => crate::dashboard::footer_hints(app),
            BudgetTarget::Plan { .. } => crate::plans::footer_hints(app),
        }
    };
    let global = Line::from(vec![
        key(" Tab "),
        Span::raw(" panel  "),
        key(" h "),
        Span::raw(" help  "),
        key(" S "),
        Span::raw(" series  "),
        key(" , "),
        Span::raw(" settings  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    let status = crate::footer_status(app);
    crate::draw_screen_footer(frame, area, local, global, status.as_deref());
}

fn key(label: &str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::default().fg(Color::Black).bg(Color::Gray),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use leeway::models::Plan;

    #[test]
    fn label_parser_accepts_sidebar_months() {
        assert_eq!(parse_label("2026-08"), Some((2026, 8)));
        assert_eq!(parse_label("bad"), None);
    }

    #[test]
    fn sidebar_groups_newest_months_before_named_plans() {
        let months = vec![Month {
            id: "july".into(),
            plan_id: None,
            label: "2026-07".into(),
            start_date: NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            days_in_month: 31,
        }];
        let plans = vec![PlanSummary {
            plan: Plan {
                id: "baseline".into(),
                name: "Baseline".into(),
            },
            item_count: 3,
        }];
        let target = BudgetTarget::Month {
            year: 2026,
            month: 6,
        };
        let targets: Vec<BudgetTarget> = sidebar_rows(
            &target,
            &months,
            &plans,
            NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
        )
        .into_iter()
        .filter_map(|row| row.target)
        .collect();

        assert_eq!(
            targets,
            vec![
                BudgetTarget::Month {
                    year: 2026,
                    month: 8,
                },
                BudgetTarget::Month {
                    year: 2026,
                    month: 7,
                },
                BudgetTarget::Month {
                    year: 2026,
                    month: 6,
                },
                BudgetTarget::Plan {
                    plan_id: "baseline".into(),
                },
            ]
        );
    }
}
