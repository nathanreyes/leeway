//! The dashboard screen: the "what's left" daily loop.
//!
//! Two public entry points, mirroring every screen module: `draw` renders a frame from
//! read-only data, and `handle_key` mutates `App` (and the database) in response to a key.

use crate::{App, ConfirmAction, DashFocus, PromptKind, Screen};
use anyhow::Result;
use ballpark::models::{AccountType, Direction, Mode, PeriodType};
use ballpark::money::Money;
use ballpark::view::{EnvelopeRow, MonthView};
use ballpark::{ops, queries};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

pub fn handle_key(app: &mut App, key: KeyEvent, view: &Option<MonthView>) -> Result<()> {
    // Clear any leftover status the moment the user acts again.
    app.status = None;

    // Global keys work on every focus, and whether or not a month is stamped.
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.should_quit = true;
            return Ok(());
        }
        KeyCode::Char('p') => {
            app.plans_sel = 0;
            app.screen = Screen::Plans;
            return Ok(());
        }
        _ => {}
    }

    // The month header owns month navigation. It's focusable like the panels, and when the
    // viewed period has no stamped month it's the *only* focus (pinned in the event loop),
    // so we handle its keys up front — independently of whether `view` is present.
    if app.dash_focus == DashFocus::Header {
        match key.code {
            // j/k (and arrows) step the viewed period; k = previous, j = next.
            KeyCode::Char('k') | KeyCode::Up => step_month(app, -1),
            KeyCode::Char('j') | KeyCode::Down => step_month(app, 1),
            // Jump straight to a typed month. Enter opens the same prompt as `m`, prefilled
            // with the current period so it's a small edit.
            KeyCode::Char('m') | KeyCode::Enter => app.open_text(
                "Go to month (YYYY-MM)",
                format!("{:04}-{:02}", app.viewed_year, app.viewed_month),
                PromptKind::GoToMonth,
            ),
            // Only leave the header if there's a month whose panels we can move onto.
            KeyCode::Tab if view.is_some() => app.dash_focus = DashFocus::Transactions,
            _ => {}
        }
        return Ok(());
    }

    // Past here a panel is focused, which only happens when a month exists.
    let Some(view) = view else { return Ok(()) };

    match key.code {
        // Tab cycles Transactions → Envelopes → Accounts → Header (Header→Transactions above).
        KeyCode::Tab => {
            app.dash_focus = match app.dash_focus {
                DashFocus::Transactions => DashFocus::Envelopes,
                DashFocus::Envelopes => DashFocus::Accounts,
                DashFocus::Accounts => DashFocus::Header,
                DashFocus::Header => DashFocus::Transactions, // unreachable; keeps match total
            };
        }

        // j/k move within the *focused* list.
        KeyCode::Char('j') | KeyCode::Down => match app.dash_focus {
            DashFocus::Transactions => {
                if app.dash_sel + 1 < view.standalone.len() {
                    app.dash_sel += 1;
                }
            }
            DashFocus::Envelopes => {
                if app.dash_env_sel + 1 < view.envelopes.len() {
                    app.dash_env_sel += 1;
                }
            }
            DashFocus::Accounts => {
                if app.dash_acct_sel + 1 < view.accounts.len() {
                    app.dash_acct_sel += 1;
                }
            }
            DashFocus::Header => {}
        },
        KeyCode::Char('k') | KeyCode::Up => match app.dash_focus {
            DashFocus::Transactions => app.dash_sel = app.dash_sel.saturating_sub(1),
            DashFocus::Envelopes => app.dash_env_sel = app.dash_env_sel.saturating_sub(1),
            DashFocus::Accounts => app.dash_acct_sel = app.dash_acct_sel.saturating_sub(1),
            DashFocus::Header => {}
        },

        // Enter / Space = the focused panel's primary action.
        KeyCode::Enter | KeyCode::Char(' ') => act_on_focus(app, view)?,

        // `n` = add a new ad-hoc item to the focused list (a bill/income, or an envelope).
        KeyCode::Char('n') => add_adhoc(app, view)?,

        // The create-then-edit verbs. Each mirrors the plan editor's keys and, on the
        // transactions/envelopes panels, acts on the *selected ad-hoc row* (plan-derived
        // rows are managed in Plans — `edit_target`/`selected_*` enforce that with a nudge).
        KeyCode::Char('r') => edit_label(app, view),   // rename / label
        KeyCode::Char('a') => edit_amount(app, view),  // amount
        KeyCode::Char('d') => cycle_direction(app, view)?, // transaction direction
        KeyCode::Char('m') => cycle_mode(app, view)?,      // envelope mode
        KeyCode::Char('t') => cycle_period(app, view)?,    // envelope period type
        KeyCode::Char('s') => feed_spending(app, view),    // file spending into a manual envelope
        KeyCode::Char('x') => delete_adhoc(app, view),     // delete an ad-hoc row

        // `e` is the Accounts panel's edit alias (Enter also works there).
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

/// The currently-selected standalone transaction, if the Transactions panel is focused.
fn selected_txn<'v>(app: &App, view: &'v MonthView) -> Option<&'v ballpark::models::Txn> {
    (app.dash_focus == DashFocus::Transactions)
        .then(|| view.standalone.get(app.dash_sel))
        .flatten()
}

/// The currently-selected envelope row, if the Envelopes panel is focused.
fn selected_env<'v>(app: &App, view: &'v MonthView) -> Option<&'v EnvelopeRow> {
    (app.dash_focus == DashFocus::Envelopes)
        .then(|| view.envelopes.get(app.dash_env_sel))
        .flatten()
}

/// The nudge shown when an edit/delete key is pressed on a plan-derived row: those are the
/// stamped plan's snapshot, edited in Plans — the dashboard only edits ad-hoc rows.
const PLAN_ROW_HINT: &str = "That's a plan item — edit it in Plans (p), or add an ad-hoc one (n)";

/// Add a new ad-hoc row to whichever list is focused, then select it once it reloads. New
/// items start with placeholder values the user refines with r/a/d (txn) or r/a/m/t (env).
fn add_adhoc(app: &mut App, view: &MonthView) -> Result<()> {
    match app.dash_focus {
        DashFocus::Transactions => {
            let id = ops::add_oneoff_txn(&app.conn, &view.month.id, "New item", Direction::Out, Money::ZERO)?;
            app.pending_dash_txn = Some(id);
            app.status = Some("Added a one-off — set label (r), amount (a), direction (d)".into());
        }
        DashFocus::Envelopes => {
            // Seed the mode from the global default, exactly like a new series does.
            let mode = queries::default_mode(&app.conn)?;
            let id = ops::add_oneoff_envelope(
                &app.conn, &view.month.id, "New envelope", Money::ZERO, PeriodType::Monthly, mode,
            )?;
            app.pending_dash_env = Some(id);
            app.status = Some("Added an envelope — set label (r), amount (a), mode (m)".into());
        }
        // The other panels have their own creation flows (accounts) or none (header).
        _ => app.status = Some("Focus Bills or Envelopes to add an ad-hoc item".into()),
    }
    Ok(())
}

/// `r`: edit the label of the selected ad-hoc txn or envelope.
fn edit_label(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        if t.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            app.open_text("Label", t.label.clone(), PromptKind::TxnLabel { id: t.id.clone() });
        }
    } else if let Some(e) = selected_env(app, view) {
        if e.envelope.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            app.open_text("Envelope label", e.envelope.label.clone(), PromptKind::EnvelopeLabel { id: e.envelope.id.clone() });
        }
    }
}

/// `a`: edit the amount of the selected ad-hoc txn or envelope.
fn edit_amount(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        if t.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            app.open_text("Amount (dollars)", crate::amount_edit_string(t.amount), PromptKind::TxnAmount { id: t.id.clone() });
        }
    } else if let Some(e) = selected_env(app, view) {
        if e.envelope.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            app.open_text("Envelope amount (dollars)", crate::amount_edit_string(e.envelope.amount), PromptKind::EnvelopeAmount { id: e.envelope.id.clone() });
        }
    }
}

/// `d`: flip an ad-hoc transaction's direction (out ⇄ in).
fn cycle_direction(app: &mut App, view: &MonthView) -> Result<()> {
    if let Some(t) = selected_txn(app, view) {
        if t.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            let next = match t.direction {
                Direction::Out => Direction::In,
                Direction::In => Direction::Out,
            };
            ops::set_txn_direction(&app.conn, &t.id, next)?;
        }
    }
    Ok(())
}

/// `m`: flip an ad-hoc envelope's mode (automatic ⇄ manual).
fn cycle_mode(app: &mut App, view: &MonthView) -> Result<()> {
    if let Some(e) = selected_env(app, view) {
        if e.envelope.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            let next = match e.envelope.mode {
                Mode::Manual => Mode::Automatic,
                Mode::Automatic => Mode::Manual,
            };
            ops::set_envelope_mode(&app.conn, &e.envelope.id, next)?;
        }
    }
    Ok(())
}

/// `t`: cycle an ad-hoc envelope's period type (daily → weekly → monthly → …).
fn cycle_period(app: &mut App, view: &MonthView) -> Result<()> {
    if let Some(e) = selected_env(app, view) {
        if e.envelope.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            let next = match e.envelope.period_type {
                PeriodType::Daily => PeriodType::Weekly,
                PeriodType::Weekly => PeriodType::Monthly,
                PeriodType::Monthly => PeriodType::Daily,
            };
            ops::set_envelope_period(&app.conn, &e.envelope.id, next)?;
        }
    }
    Ok(())
}

/// `s` (and Enter on the Envelopes panel): file a spend into the selected manual envelope.
/// Works for any manual envelope — plan-derived or ad-hoc — since that's the only way to
/// feed one. Automatic envelopes accrue by time, so there's nothing to file.
fn feed_spending(app: &mut App, view: &MonthView) {
    if let Some(e) = selected_env(app, view) {
        match e.envelope.mode {
            Mode::Manual => app.open_text(
                format!("Spend in “{}” (dollars)", e.envelope.label),
                String::new(),
                PromptKind::EnvelopeSpend { envelope_id: e.envelope.id.clone(), month_id: view.month.id.clone() },
            ),
            Mode::Automatic => {
                app.status = Some("Automatic envelopes accrue by time; switch to manual (m) to file spending".into());
            }
        }
    }
}

/// `x`: delete the selected ad-hoc row (with a confirm). Plan-derived rows are refused.
fn delete_adhoc(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        if t.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            app.open_confirm(format!("Delete “{}”?", t.label), ConfirmAction::DeleteTxn { id: t.id.clone() });
        }
    } else if let Some(e) = selected_env(app, view) {
        if e.envelope.series_id.is_some() {
            app.status = Some(PLAN_ROW_HINT.into());
        } else {
            app.open_confirm(
                format!("Delete envelope “{}” and its spending?", e.envelope.label),
                ConfirmAction::DeleteEnvelope { id: e.envelope.id.clone() },
            );
        }
    }
}

/// Move the viewed period by `delta` calendar months (−1 = previous, +1 = next), rolling
/// the year across the December/January boundary. Row selections reset because they point
/// at the old month's lists.
fn step_month(app: &mut App, delta: i32) {
    // Work in "absolute months" (year*12 + month-1) so the roll-over is one bit of math.
    let zero_based = app.viewed_year * 12 + (app.viewed_month as i32 - 1) + delta;
    app.viewed_year = zero_based.div_euclid(12);
    app.viewed_month = zero_based.rem_euclid(12) as u32 + 1;
    app.dash_sel = 0;
    app.dash_acct_sel = 0;
}

/// Do the focused panel's action: settle a bill, or open the balance-edit prompt.
fn act_on_focus(app: &mut App, view: &MonthView) -> Result<()> {
    match app.dash_focus {
        DashFocus::Transactions => {
            if let Some(txn) = view.standalone.get(app.dash_sel) {
                ops::toggle_settled(&app.conn, &txn.id, txn.settled)?;
            }
        }
        // On the envelopes panel the primary action is to file a spend (manual envelopes);
        // for automatic ones `feed_spending` shows the "accrues by time" hint instead.
        DashFocus::Envelopes => feed_spending(app, view),
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
        // The header has no per-row action; its keys (j/k/m) are handled in handle_key.
        DashFocus::Header => {}
    }
    Ok(())
}

pub fn draw(frame: &mut Frame, app: &App, view: &Option<MonthView>) {
    // The header and footer are always present — they carry month navigation, which must
    // work even when the viewed period has no stamped month. Only the middle changes.
    let [header, middle, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app, view.as_ref());
    match view {
        Some(view) => draw_month_body(frame, middle, app, view),
        None => draw_missing_month(frame, middle, app),
    }
    draw_footer(frame, footer, app, view);
}

/// The full month view: the "what's left" rollup on top, then the three columns
/// (accounts, bills, envelopes).
fn draw_month_body(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    // 7 rows for "what's left" so all breakdown lines (funds/card, income/bills/envelopes,
    // carry) fit inside the borders.
    let [whats_left, body] =
        Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).areas(area);

    let [acct_area, txn_area, env_area] = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(38),
        Constraint::Percentage(32),
    ])
    .areas(body);

    draw_whats_left(frame, whats_left, view);
    draw_accounts(frame, acct_area, app, view);
    draw_transactions(frame, txn_area, app, view);
    draw_envelopes(frame, env_area, app, view);
}

/// Shown when the viewed period isn't stamped: name the period and point at the ways
/// forward (stamp it, or keep navigating).
fn draw_missing_month(frame: &mut Frame, area: Rect, app: &App) {
    let label = format!("{:04}-{:02}", app.viewed_year, app.viewed_month);
    let dim = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::raw(""),
        Line::from(format!("  {label} isn't stamped yet.").bold()),
        Line::raw(""),
        Line::from(Span::styled("  Press p to open Plans and stamp one onto it,", dim)),
        Line::from(Span::styled("  or k/j to step months and m to jump to one.", dim)),
    ];
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Ballpark "));
    frame.render_widget(p, area);
}

/// Footer hints, adapted to the focused control (and to whether a month exists — with none,
/// there are no panels to Tab to).
fn draw_footer(frame: &mut Frame, area: Rect, app: &App, view: &Option<MonthView>) {
    let hints = match app.dash_focus {
        DashFocus::Header => {
            let mut spans = Vec::new();
            if view.is_some() {
                spans.push(key(" Tab "));
                spans.push(Span::raw(" panel  "));
            }
            spans.extend([
                key(" k/j "), Span::raw(" prev/next month  "),
                key(" m "), Span::raw(" go to month  "),
                key(" p "), Span::raw(" plans  "),
                key(" q "), Span::raw(" quit"),
            ]);
            Line::from(spans)
        }
        DashFocus::Transactions => Line::from(vec![
            key(" Tab "), Span::raw(" panel  "),
            key(" j/k "), Span::raw(" move  "),
            key(" Enter "), Span::raw(" paid  "),
            key(" n "), Span::raw(" add  "),
            key(" r/a/d "), Span::raw(" edit  "),
            key(" x "), Span::raw(" del  "),
            key(" q "), Span::raw(" quit"),
        ]),
        DashFocus::Envelopes => Line::from(vec![
            key(" Tab "), Span::raw(" panel  "),
            key(" j/k "), Span::raw(" move  "),
            key(" s "), Span::raw(" spend  "),
            key(" n "), Span::raw(" add  "),
            key(" r/a/m/t "), Span::raw(" edit  "),
            key(" x "), Span::raw(" del  "),
            key(" q "), Span::raw(" quit"),
        ]),
        DashFocus::Accounts => Line::from(vec![
            key(" Tab "), Span::raw(" panel  "),
            key(" j/k "), Span::raw(" move  "),
            key(" Enter "), Span::raw(" edit  "),
            key(" l "), Span::raw(" card limit  "),
            key(" p "), Span::raw(" plans  "),
            key(" q "), Span::raw(" quit"),
        ]),
    };
    crate::draw_status_footer(frame, area, hints, &app.status);
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

/// The month header, and the handle for month navigation. Always drawn from `app`'s viewed
/// period so it appears even when that period has no month; `view` (when present) adds the
/// day counter and current/past/upcoming tag. A cyan border cues that it holds focus.
fn draw_header(frame: &mut Frame, area: Rect, app: &App, view: Option<&MonthView>) {
    let label = format!("{:04}-{:02}", app.viewed_year, app.viewed_month);
    let title = match view {
        // The live month gets the "day X of N" progress counter. `days_elapsed` is whole
        // days *since* the 1st (0 on the 1st) — right for accrual, but as a day-of-month
        // label it reads a day short, so +1 turns it into the calendar day (7th → "day 7").
        Some(v) if v.is_current => {
            format!(" Ballpark — {}   (day {} of {}) ", label, v.days_elapsed + 1, v.month.days_in_month)
        }
        // Any other stamped month is wholly in the past or future (only the calendar month
        // contains today), so `days_elapsed` at either extreme tells us which.
        Some(v) => {
            let when = if v.days_elapsed >= v.month.days_in_month { "past" } else { "upcoming" };
            format!(" Ballpark — {label}   ({when}) ")
        }
        None => format!(" Ballpark — {label}   (not stamped) "),
    };

    let focused = app.dash_focus == DashFocus::Header;
    let block = Block::default().borders(Borders::ALL);
    let block = if focused {
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block
    };
    let p = Paragraph::new(Line::from(title.bold()))
        .alignment(Alignment::Center)
        .block(block);
    frame.render_widget(p, area);
}

fn draw_whats_left(frame: &mut Frame, area: Rect, view: &MonthView) {
    let wl = &view.whats_left;
    let headline_color = if wl.whats_left.cents() >= 0 { Color::Green } else { Color::Red };

    let mut lines = vec![
        Line::from(vec![
            Span::raw("What's left:  "),
            Span::styled(
                wl.whats_left.to_string(),
                Style::default().fg(headline_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
    ];

    // The account-derived terms (funds, card debt, carry) are only part of the headline for
    // the current month; the view already zeroed them off-month. So we only *show* them for
    // the current month, and otherwise say why the balance is income − bills − envelopes.
    if view.is_current {
        lines.push(Line::from(vec![
            Span::styled(format!("{:>12}", wl.funds_available.to_string()), Style::default().fg(Color::Cyan)),
            Span::raw("  funds"),
            Span::raw("   − "),
            Span::styled(wl.card_debt.to_string(), Style::default().fg(Color::Red)),
            Span::raw(" card debt"),
        ]));
    }

    lines.push(Line::from(vec![
        Span::raw("   + "),
        Span::styled(wl.income_remaining.to_string(), Style::default().fg(Color::Green)),
        Span::raw(" income left    − "),
        Span::styled(wl.bills_remaining.to_string(), Style::default().fg(Color::Red)),
        Span::raw(" bills left    − "),
        Span::styled(wl.envelopes_remaining.to_string(), Style::default().fg(Color::Magenta)),
        Span::raw(" envelopes"),
    ]));

    if view.is_current {
        // Net carry effect: already signed (checking buffers pull down, card carryovers
        // push up), so it's shown as a single "+ <signed>" term folded into the total.
        lines.push(Line::from(vec![
            Span::raw("   + "),
            Span::styled(wl.carry_adjustment.to_string(), Style::default().fg(Color::Yellow)),
            Span::raw(" carry (buffers −, card carryovers +)"),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "   account balances count only in the current month",
            Style::default().fg(Color::DarkGray),
        )));
    }

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
                Span::raw(format!("{:<18}", crate::truncate(&t.label, 18))),
                Span::styled(format!("{:>10}", amount), style),
                origin_marker(t.series_id.is_none()),
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

fn draw_envelopes(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let items: Vec<ListItem> = view
        .envelopes
        .iter()
        .map(|e| {
            let mode = match e.envelope.mode {
                Mode::Automatic => "auto",
                Mode::Manual => "man ",
            };
            let meter = meter_bar(e.consumed, e.envelope.amount, 8);
            let line = Line::from(vec![
                Span::raw(format!("{:<12}", crate::truncate(&e.envelope.label, 12))),
                Span::styled(format!("{} ", mode), Style::default().fg(Color::DarkGray)),
                Span::styled(meter, Style::default().fg(Color::Magenta)),
                Span::raw(format!("  {:>9} left", e.remaining.to_string())),
                origin_marker(e.envelope.series_id.is_none()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let focused = app.dash_focus == DashFocus::Envelopes;
    let mut state = ListState::default();
    if focused && !view.envelopes.is_empty() {
        state.select(Some(app.dash_env_sel));
    }

    let list = List::new(items)
        .block(panel_block(" Envelopes ", focused))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, &mut state);
}

/// The trailing plan-vs-ad-hoc marker, rendered as its own right-hand column so the rows'
/// left edges (checkboxes, labels) stay aligned: `⟳` (dim) = came from a stamped plan,
/// `+` (yellow) = ad-hoc, added straight into this month.
fn origin_marker(is_adhoc: bool) -> Span<'static> {
    if is_adhoc {
        Span::styled("  +", Style::default().fg(Color::Yellow))
    } else {
        Span::styled("  ⟳", Style::default().fg(Color::DarkGray))
    }
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
