//! Month detail for the shared budget workspace: the "what's left" daily loop.
//!
//! Two public entry points, mirroring every screen module: `draw` renders a frame from
//! read-only data, and `handle_key` mutates `App` (and the database) in response to a key.

use crate::{
    AddDestination, App, BudgetBlock, ChoiceOption, ConfirmAction, DashFocus, EnvelopeManage,
    Modal, ModalAction, PromptKind,
    anim::{SummaryAnimations, SummaryTerm, display_cents},
};
use anyhow::Result;
use leeway::calc;
use leeway::models::{AccountType, CreditCardEntryMode, Direction, Mode, PeriodType};
use leeway::money::Money;
use leeway::ops;
use leeway::view::{EnvelopeRow, MonthView};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph};
use std::time::Instant;

pub fn handle_key(app: &mut App, key: KeyEvent, view: &Option<MonthView>) -> Result<()> {
    // Clear any leftover status the moment the user acts again.
    app.status = None;

    // Escape returns from a detail pane to the budget sidebar. From the sidebar it exits,
    // matching the workspace handler that normally receives that second press.
    if key.code == KeyCode::Esc {
        if app.dash_focus == DashFocus::Header {
            app.should_quit = true;
        } else {
            app.dash_focus = DashFocus::Header;
        }
        return Ok(());
    }

    // The month header owns month navigation. It's focusable like the panels, and when the
    // viewed period has no stamped month it's the *only* focus (pinned in the event loop),
    // so we handle its keys up front — independently of whether `view` is present.
    if app.dash_focus == DashFocus::Header {
        match key.code {
            // j/k (and arrows) step the viewed period; k = previous, j = next.
            KeyCode::Char('k') | KeyCode::Up => step_month(app, -1),
            KeyCode::Char('j') | KeyCode::Down => step_month(app, 1),
            // Jump straight to a typed month, prefilled with the current period.
            KeyCode::Char('g') | KeyCode::Enter => app.open_text(
                "Go to month (YYYY-MM)",
                format!("{:04}-{:02}", app.viewed_year, app.viewed_month),
                PromptKind::GoToMonth,
            ),
            // Only leave the header if there's a month whose panels we can move onto.
            // Off-month account balances are explanatory only, so skip Accounts there.
            KeyCode::Tab => {
                if let Some(view) = view {
                    app.dash_focus = next_dash_focus(app.dash_focus, view.is_current);
                }
            }
            KeyCode::BackTab => {
                if let Some(view) = view {
                    app.dash_focus = previous_dash_focus(app.dash_focus, view.is_current);
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Past here a panel is focused, which only happens when a month exists.
    let Some(view) = view else { return Ok(()) };

    match key.code {
        // Tab cycles Accounts → Income → Expenses → Envelopes → Header on the current
        // month. Off-month, Accounts is explanatory only and is skipped.
        KeyCode::Tab => {
            app.dash_focus = next_dash_focus(app.dash_focus, view.is_current);
        }
        KeyCode::BackTab => {
            app.dash_focus = previous_dash_focus(app.dash_focus, view.is_current);
        }

        // j/k move within the *focused* list.
        KeyCode::Char('j') | KeyCode::Down => match app.dash_focus {
            DashFocus::Income => {
                if app.dash_income_sel + 1 < txn_count(view, Direction::In) {
                    app.dash_income_sel += 1;
                }
            }
            DashFocus::Expenses => {
                if app.dash_expense_sel + 1 < txn_count(view, Direction::Out) {
                    app.dash_expense_sel += 1;
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
            DashFocus::Income => app.dash_income_sel = app.dash_income_sel.saturating_sub(1),
            DashFocus::Expenses => app.dash_expense_sel = app.dash_expense_sel.saturating_sub(1),
            DashFocus::Envelopes => app.dash_env_sel = app.dash_env_sel.saturating_sub(1),
            DashFocus::Accounts => app.dash_acct_sel = app.dash_acct_sel.saturating_sub(1),
            DashFocus::Header => {}
        },

        // Enter opens an envelope's transactions modal; otherwise it performs the focused
        // row's primary action. Space always keeps the lightweight primary action.
        KeyCode::Enter if app.dash_focus == DashFocus::Envelopes => {
            open_envelope_modal(app, view);
        }
        KeyCode::Enter | KeyCode::Char(' ') => act_on_focus(app, view)?,

        // `n` = create something in the focused panel.
        KeyCode::Char('n') => {
            if app.dash_focus == DashFocus::Accounts {
                add_account(app);
            } else {
                add_adhoc(app, view)?;
            }
        }

        // The edit verbs mirror the plan editor's keys, but here they edit this month's
        // independent snapshot. Amount/mode/period act on the focused envelope directly;
        // only an envelope's label (its identity) is steered to the series level via
        // `edit_label`. Direction changes intentionally stay out of this fast path: moving
        // between income and expenses is safer as remove/re-add.
        KeyCode::Char('l') => edit_label(app, view),
        KeyCode::Char('a') => edit_amount(app, view), // amount
        KeyCode::Char('m') => cycle_mode(app, view)?, // envelope mode
        KeyCode::Char('p') => cycle_period(app, view)?, // envelope period type
        KeyCode::Char('s') => feed_spending(app, view), // record an envelope transaction
        KeyCode::Char('x') => delete_selected(app, view), // delete this month's row

        // Carry balance is account-only: checking reserves cash, cards forgive deferred debt.
        KeyCode::Char('c') => {
            if app.dash_focus == DashFocus::Accounts {
                edit_account_carry(app, view);
            }
        }

        // Edit a credit card's limit (rarely changed, so it gets its own key).
        KeyCode::Char('L') if app.dash_focus == DashFocus::Accounts => {
            if let Some(acct) = view.accounts.get(app.dash_acct_sel)
                && acct.account_type == AccountType::CreditCard
            {
                app.open_text_replace_on_type(
                    format!("Credit limit for {}", acct.name),
                    crate::amount_edit_string(acct.credit_limit.unwrap_or(Money::ZERO)),
                    PromptKind::CardLimit {
                        id: acct.id.clone(),
                    },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn next_dash_focus(current: DashFocus, is_current_month: bool) -> DashFocus {
    match current {
        DashFocus::Header => {
            if is_current_month {
                DashFocus::Accounts
            } else {
                DashFocus::Income
            }
        }
        DashFocus::Accounts => DashFocus::Income,
        DashFocus::Income => DashFocus::Expenses,
        DashFocus::Expenses => DashFocus::Envelopes,
        DashFocus::Envelopes => DashFocus::Header,
    }
}

fn previous_dash_focus(current: DashFocus, is_current_month: bool) -> DashFocus {
    match current {
        DashFocus::Header => DashFocus::Envelopes,
        DashFocus::Accounts => DashFocus::Header,
        DashFocus::Income => {
            if is_current_month {
                DashFocus::Accounts
            } else {
                DashFocus::Header
            }
        }
        DashFocus::Expenses => DashFocus::Income,
        DashFocus::Envelopes => DashFocus::Expenses,
    }
}

/// The currently-selected standalone transaction, if an income/expense panel is focused.
fn selected_txn<'v>(app: &App, view: &'v MonthView) -> Option<&'v leeway::models::Txn> {
    match app.dash_focus {
        DashFocus::Income => selected_txn_by_direction(view, Direction::In, app.dash_income_sel),
        DashFocus::Expenses => {
            selected_txn_by_direction(view, Direction::Out, app.dash_expense_sel)
        }
        _ => None,
    }
}

fn selected_txn_by_direction(
    view: &MonthView,
    direction: Direction,
    selected: usize,
) -> Option<&leeway::models::Txn> {
    view.standalone
        .iter()
        .filter(|txn| txn.direction == direction)
        .nth(selected)
}

fn txn_count(view: &MonthView, direction: Direction) -> usize {
    view.standalone
        .iter()
        .filter(|txn| txn.direction == direction)
        .count()
}

/// The currently-selected envelope row, if the Envelopes panel is focused.
fn selected_env<'v>(app: &App, view: &'v MonthView) -> Option<&'v EnvelopeRow> {
    (app.dash_focus == DashFocus::Envelopes)
        .then(|| view.envelopes.get(app.dash_env_sel))
        .flatten()
}

/// The durable series behind the focused budget row, for the global contextual `S` jump.
/// Header/accounts and legacy seriesless month rows intentionally return `None`, which makes
/// the jump fall back to the complete Series list.
pub fn selected_series_id(app: &App, view: &MonthView) -> Option<String> {
    match app.dash_focus {
        DashFocus::Income | DashFocus::Expenses => selected_txn(app, view)?.series_id.clone(),
        DashFocus::Envelopes => selected_env(app, view)?.envelope.series_id.clone(),
        DashFocus::Header | DashFocus::Accounts => None,
    }
}

/// Start a reusable-series add flow for whichever budget block is focused. Nothing is
/// inserted until the user selects/creates a series and confirms an amount.
fn add_adhoc(app: &mut App, view: &MonthView) -> Result<()> {
    match app.dash_focus {
        DashFocus::Income => app.open_series_search(
            AddDestination::Month {
                month_id: view.month.id.clone(),
            },
            BudgetBlock::Income,
        )?,
        DashFocus::Expenses => app.open_series_search(
            AddDestination::Month {
                month_id: view.month.id.clone(),
            },
            BudgetBlock::Expenses,
        )?,
        DashFocus::Envelopes => app.open_series_search(
            AddDestination::Month {
                month_id: view.month.id.clone(),
            },
            BudgetBlock::Envelopes,
        )?,
        // The other panels have their own creation flows (accounts) or none (header).
        _ => app.status = Some("Focus Income, Expenses, or Envelopes to add an item".into()),
    }
    Ok(())
}

/// Start account creation with an explicit type choice.
fn add_account(app: &mut App) {
    app.open_choice(
        "Create which account type?",
        vec![
            ChoiceOption {
                key: 'h',
                label: "Checking".into(),
                action: Some(ModalAction::BeginNewAccount {
                    account_type: AccountType::Checking,
                }),
            },
            ChoiceOption {
                key: 'c',
                label: "Credit card".into(),
                action: Some(ModalAction::BeginNewAccount {
                    account_type: AccountType::CreditCard,
                }),
            },
        ],
    );
}

/// `l`: account names and legacy seriesless labels stay local. A series-backed budget
/// row points at the canonical Series workflow without silently navigating there.
fn edit_label(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        if t.series_id.is_some() {
            app.status = Some(crate::SERIES_RENAME_GUIDANCE.into());
        } else {
            app.open_text_replace_on_type(
                "Label",
                t.label.clone(),
                PromptKind::TxnLabel { id: t.id.clone() },
            );
        }
    } else if let Some(e) = selected_env(app, view) {
        if e.envelope.series_id.is_some() {
            app.status = Some(crate::SERIES_RENAME_GUIDANCE.into());
        } else {
            app.open_text_replace_on_type(
                "Envelope label",
                e.envelope.label.clone(),
                PromptKind::EnvelopeLabel {
                    id: e.envelope.id.clone(),
                },
            );
        }
    } else if app.dash_focus == DashFocus::Accounts
        && let Some(acct) = view.accounts.get(app.dash_acct_sel)
    {
        app.open_text_replace_on_type(
            "Account label",
            acct.name.clone(),
            PromptKind::AccountName {
                id: acct.id.clone(),
            },
        );
    }
}

/// `c`: edit the selected account's carry balance.
fn edit_account_carry(app: &mut App, view: &MonthView) {
    if let Some(acct) = view.accounts.get(app.dash_acct_sel) {
        let help = match acct.account_type {
            AccountType::Checking => vec![
                "Buffer is cash you want to keep parked in this account.".into(),
                "It is subtracted from what's left, so the buffer amount".into(),
                "you set is made unavailable to spend.".into(),
                "Use it for minimum balance, cushion, or money you".into(),
                "do not want counted.".into(),
            ],
            AccountType::CreditCard => vec![
                "Carry is card debt you do not plan to pay this month.".into(),
                "It is added back to what's left, offsetting that much".into(),
                "of the card's owed balance.".into(),
                "Use it for planned carryover, promo balances, or debt".into(),
                "handled outside this month.".into(),
            ],
        };
        app.open_text_with_help(
            format!("Carry balance for {}", acct.name),
            crate::amount_edit_string(acct.carry_balance.unwrap_or(Money::ZERO)),
            help,
            PromptKind::AccountCarry {
                id: acct.id.clone(),
            },
        );
    }
}

/// `a`: edit the amount of the selected month txn or envelope.
fn edit_amount(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        app.open_text_replace_on_type(
            "Amount (dollars)",
            crate::amount_edit_string(t.amount),
            PromptKind::TxnAmount { id: t.id.clone() },
        );
    } else if let Some(e) = selected_env(app, view) {
        app.open_text_replace_on_type(
            "Envelope amount (dollars)",
            crate::amount_edit_string(leeway::calc::envelope_period_amount(
                e.envelope.amount,
                e.envelope.period_type,
                view.month.days_in_month,
            )),
            PromptKind::EnvelopeAmount {
                id: e.envelope.id.clone(),
                period_type: e.envelope.period_type,
                days_in_month: view.month.days_in_month,
            },
        );
    }
}

fn open_envelope_modal(app: &mut App, view: &MonthView) {
    if let Some(e) = selected_env(app, view) {
        app.modal = Some(Modal::Envelope(EnvelopeManage {
            month_id: view.month.id.clone(),
            envelope_id: e.envelope.id.clone(),
            selected_spend: 0,
        }));
    }
}

/// `m`: flip a month envelope's mode (automatic ⇄ manual).
fn cycle_mode(app: &mut App, view: &MonthView) -> Result<()> {
    if let Some(e) = selected_env(app, view) {
        let next = match e.envelope.mode {
            Mode::Manual => Mode::Automatic,
            Mode::Automatic => Mode::Manual,
        };
        ops::set_envelope_mode(&app.conn, &e.envelope.id, next)?;
    }
    Ok(())
}

/// Cycle a month envelope's period type between daily and monthly.
fn cycle_period(app: &mut App, view: &MonthView) -> Result<()> {
    if let Some(e) = selected_env(app, view) {
        let next = match e.envelope.period_type {
            PeriodType::Daily => PeriodType::Monthly,
            PeriodType::Weekly | PeriodType::Monthly => PeriodType::Daily,
        };
        ops::set_envelope_period(&app.conn, &e.envelope.id, next)?;
    }
    Ok(())
}

/// `s`: record a transaction in the selected envelope. Automatic envelopes keep it as a
/// record; manual envelopes also use it to calculate their consumed amount.
fn feed_spending(app: &mut App, view: &MonthView) {
    if let Some(e) = selected_env(app, view) {
        app.open_text(
            format!("Transaction label for {}", e.envelope.display_label()),
            String::new(),
            PromptKind::EnvelopeSpendLabel {
                envelope_id: e.envelope.id.clone(),
                month_id: view.month.id.clone(),
            },
        );
    }
}

/// `x`: delete the selected month instance (with a confirm). A stamped row carries its
/// series id for trend/restamp matching, but it is still this month's copy.
fn delete_selected(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        app.open_confirm(
            format!("Delete “{}”?", t.display_label()),
            ConfirmAction::DeleteTxn { id: t.id.clone() },
        );
    } else if let Some(e) = selected_env(app, view) {
        app.open_confirm(
            format!(
                "Delete envelope “{}” and its transactions?",
                e.envelope.display_label()
            ),
            ConfirmAction::DeleteEnvelope {
                id: e.envelope.id.clone(),
            },
        );
    } else if app.dash_focus == DashFocus::Accounts
        && let Some(acct) = view.accounts.get(app.dash_acct_sel)
    {
        app.open_confirm(
            format!("Delete account “{}”?", acct.name),
            ConfirmAction::DeleteAccount {
                id: acct.id.clone(),
            },
        );
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
    app.dash_income_sel = 0;
    app.dash_expense_sel = 0;
    app.dash_env_sel = 0;
    app.dash_acct_sel = 0;
}

/// Do the focused panel's lightweight action: settle a bill, record an envelope transaction,
/// or open the balance-edit prompt. Enter opens an envelope's full detail screen instead.
fn act_on_focus(app: &mut App, view: &MonthView) -> Result<()> {
    match app.dash_focus {
        DashFocus::Income | DashFocus::Expenses => {
            if let Some(txn) = selected_txn(app, view) {
                ops::toggle_settled(&app.conn, &txn.id, txn.settled)?;
            }
        }
        // Space records a transaction without leaving the dashboard. Enter opens details.
        DashFocus::Envelopes => feed_spending(app, view),
        DashFocus::Accounts => {
            if let Some(acct) = view.accounts.get(app.dash_acct_sel) {
                match acct.account_type {
                    // Checking: edit the spendable balance.
                    AccountType::Checking => app.open_text_replace_on_type(
                        format!("New balance for {}", acct.name),
                        crate::amount_edit_string(acct.balance),
                        PromptKind::AccountBalance {
                            id: acct.id.clone(),
                        },
                    ),
                    // Credit cards store available credit internally, but the preferred
                    // input can be either that figure or the derived current balance.
                    AccountType::CreditCard => {
                        let mode = leeway::queries::credit_card_entry_mode(&app.conn)
                            .unwrap_or(CreditCardEntryMode::AvailableCredit);
                        let limit = acct.credit_limit.unwrap_or(Money::ZERO);
                        let available = acct.available_credit.unwrap_or(Money::ZERO);
                        app.open_text_replace_on_type(
                            format!("{} for {}", mode.label(), acct.name),
                            crate::amount_edit_string(mode.entered_amount(limit, available)),
                            PromptKind::CardEntry {
                                id: acct.id.clone(),
                                name: acct.name.clone(),
                                limit,
                                mode,
                            },
                        );
                    }
                }
            }
        }
        // The header has no per-row action; its keys (j/k/m) are handled in handle_key.
        DashFocus::Header => {}
    }
    Ok(())
}

#[cfg(test)]
pub fn draw(frame: &mut Frame, app: &App, view: &Option<MonthView>) {
    // The header and footer are always present — they carry month navigation, which must
    // work even when the viewed period has no stamped month. Only the middle changes.
    let [header, middle, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    draw_header(
        frame,
        header,
        app,
        view.as_ref(),
        app.dash_focus == DashFocus::Header,
    );
    match view {
        Some(view) => draw_month_body(frame, middle, app, view),
        None => draw_missing_month(frame, middle, app),
    }
    draw_footer(frame, footer, app, view);
}

/// Draw the month side of the shared budget workspace. The sidebar owns navigation and
/// the workspace owns the footer, so this renders only the month header and body.
pub fn draw_detail(frame: &mut Frame, area: Rect, app: &App, view: &Option<MonthView>) {
    let [header, middle] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    draw_header(frame, header, app, view.as_ref(), false);
    match view {
        Some(view) => draw_month_body(frame, middle, app, view),
        None => draw_missing_month(frame, middle, app),
    }
}

/// The full month view. Only the current month shows Accounts; every other month uses
/// the same budget-block-first shape as a plan.
fn draw_month_body(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let (account_area, body, summary_area) = if view.is_current {
        let [account_area, body, summary_area] = Layout::vertical([
            Constraint::Length(account_block_height(view)),
            Constraint::Min(0),
            Constraint::Length(7),
        ])
        .areas(area);
        (Some(account_area), body, summary_area)
    } else {
        let [body, summary_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(7)]).areas(area);
        (None, body, summary_area)
    };
    // Mirror the summary's relative positioning: income and expenses share the top row
    // side-by-side, and envelopes sit in a full-width box beneath them. Size the two
    // tiers from their contents so spare vertical room prevents unnecessary scrolling.
    let income_rows = view
        .standalone
        .iter()
        .filter(|txn| txn.direction == Direction::In)
        .count();
    let expense_rows = view
        .standalone
        .iter()
        .filter(|txn| txn.direction == Direction::Out)
        .count();
    let (items_height, envelope_height) = budget_panel_heights(
        body.height,
        income_rows.max(expense_rows),
        view.envelopes.len(),
    );
    let [items_area, env_area] = Layout::vertical([
        Constraint::Length(items_height),
        Constraint::Length(envelope_height),
    ])
    .areas(body);
    let [income_area, expense_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(items_area);

    if let Some(account_area) = account_area {
        draw_accounts(frame, account_area, app, view);
    }
    draw_transactions(frame, income_area, app, view, Direction::In);
    draw_transactions(frame, expense_area, app, view, Direction::Out);
    draw_envelopes(frame, env_area, app, view);
    draw_whats_left(frame, summary_area, view, &app.summary_anims, app.frame_now);
}

/// Divide the budget body between the side-by-side transaction tier and the envelope tier.
/// Each tier gets enough rows to show all of its content whenever possible. When both cannot
/// fit, divide the usable space in proportion to their requested heights; the list renderers
/// then add scrollbars only to the tiers that still overflow.
pub(crate) fn budget_panel_heights(
    available: u16,
    transaction_rows: usize,
    envelope_rows: usize,
) -> (u16, u16) {
    const MIN_BLOCK_HEIGHT: usize = 3;

    let available = available as usize;
    if available < MIN_BLOCK_HEIGHT * 2 {
        let items = available.div_ceil(2);
        return (items as u16, (available - items) as u16);
    }

    let desired_items = transaction_rows.saturating_add(2).max(MIN_BLOCK_HEIGHT);
    let desired_envelopes = envelope_rows.saturating_add(2).max(MIN_BLOCK_HEIGHT);

    if desired_items.saturating_add(desired_envelopes) <= available {
        // Preserve the dashboard's roomy upper tier once every row in both tiers fits.
        return (
            (available - desired_envelopes) as u16,
            desired_envelopes as u16,
        );
    }

    let distributable = available - MIN_BLOCK_HEIGHT * 2;
    let item_demand = desired_items - MIN_BLOCK_HEIGHT;
    let envelope_demand = desired_envelopes - MIN_BLOCK_HEIGHT;
    let total_demand = item_demand.saturating_add(envelope_demand);
    let item_extra = distributable
        .saturating_mul(item_demand)
        .checked_div(total_demand)
        .unwrap_or(distributable / 2);
    let items = MIN_BLOCK_HEIGHT + item_extra;
    (items as u16, (available - items) as u16)
}

/// One row per account plus the bordered block, capped so a large account list does not
/// crowd out the budget blocks.
fn account_block_height(view: &MonthView) -> u16 {
    view.accounts.len().saturating_add(2).clamp(3, 7) as u16
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
        Line::from(Span::styled(
            "  Choose a plan in the sidebar and press s to stamp it,",
            dim,
        )),
        Line::from(Span::styled(
            "  or k/j to step months and m to jump to one.",
            dim,
        )),
    ];
    let p = Paragraph::new(lines).block(crate::titled_block(" Leeway "));
    frame.render_widget(p, area);
}

/// Footer hints, adapted to the focused control (and to whether a month exists — with none,
/// there are no panels to Tab to).
#[cfg(test)]
fn draw_footer(frame: &mut Frame, area: Rect, app: &App, view: &Option<MonthView>) {
    let left_hints = match app.dash_focus {
        DashFocus::Header => {
            let mut spans = Vec::new();
            if view.is_some() {
                spans.push(key(" Tab "));
                spans.push(Span::raw(" panel  "));
            }
            spans.extend([
                key(" k/j "),
                Span::raw(" prev/next month  "),
                key(" g "),
                Span::raw(" go to month"),
            ]);
            Line::from(spans)
        }
        DashFocus::Income | DashFocus::Expenses => Line::from(vec![
            key(" Tab "),
            Span::raw(" panel  "),
            key(" j/k "),
            Span::raw(" move  "),
            key(" Enter "),
            Span::raw(" paid  "),
            key(" n "),
            Span::raw(" new  "),
            key(" a "),
            Span::raw(" amount  "),
            key(" x "),
            Span::raw(" del"),
        ]),
        DashFocus::Envelopes => Line::from(vec![
            key(" Tab "),
            Span::raw(" panel  "),
            key(" j/k "),
            Span::raw(" move  "),
            key(" Enter "),
            Span::raw(" txns  "),
            key(" a "),
            Span::raw(" amount  "),
            key(" m "),
            Span::raw(" mode  "),
            key(" p "),
            Span::raw(" period  "),
            key(" s "),
            Span::raw(" record  "),
            key(" n "),
            Span::raw(" new  "),
            key(" x "),
            Span::raw(" del"),
        ]),
        DashFocus::Accounts => Line::from(vec![
            key(" Tab "),
            Span::raw(" panel  "),
            key(" j/k "),
            Span::raw(" move  "),
            key(" Enter "),
            Span::raw(" edit  "),
            key(" n "),
            Span::raw(" new  "),
            key(" l "),
            Span::raw(" label  "),
            key(" c "),
            Span::raw(" carry  "),
            key(" L "),
            Span::raw(" limit  "),
            key(" x "),
            Span::raw(" del"),
        ]),
    };
    let nav_hints = Line::from(vec![
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
    crate::draw_screen_footer(frame, area, left_hints, nav_hints, status.as_deref());
}

/// Detail-panel hints used by the shared budget footer.
pub fn footer_hints(app: &App) -> Line<'static> {
    match app.dash_focus {
        DashFocus::Header => Line::from(vec![
            key(" j/k "),
            Span::raw(" budget  "),
            key(" g "),
            Span::raw(" go to month"),
        ]),
        DashFocus::Income | DashFocus::Expenses => Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" Enter "),
            Span::raw(" paid  "),
            key(" n "),
            Span::raw(" new  "),
            key(" a "),
            Span::raw(" amount  "),
            key(" x "),
            Span::raw(" del"),
        ]),
        DashFocus::Envelopes => Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" Enter "),
            Span::raw(" txns  "),
            key(" a "),
            Span::raw(" amount  "),
            key(" m "),
            Span::raw(" mode  "),
            key(" p "),
            Span::raw(" period  "),
            key(" s "),
            Span::raw(" record  "),
            key(" n "),
            Span::raw(" new  "),
            key(" x "),
            Span::raw(" del"),
        ]),
        DashFocus::Accounts => Line::from(vec![
            key(" j/k "),
            Span::raw(" move  "),
            key(" Enter "),
            Span::raw(" edit  "),
            key(" n "),
            Span::raw(" new  "),
            key(" l "),
            Span::raw(" label  "),
            key(" c "),
            Span::raw(" carry  "),
            key(" L "),
            Span::raw(" limit  "),
            key(" x "),
            Span::raw(" del"),
        ]),
    }
}

/// The accounts panel. Full-width rows keep the current balance/owed amount near the
/// account name and reserve the final column for each account's buffer/carry setting.
fn draw_accounts(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let inner_width = crate::selectable_list_content_width(area);
    let name_width = inner_width
        .saturating_mul(22)
        .checked_div(100)
        .unwrap_or(18)
        .clamp(14, 26);
    let primary_width = 20;
    let adjustment_width = 18;
    let fixed_width = name_width + primary_width + adjustment_width;
    let detail_width = inner_width.saturating_sub(fixed_width).max(1);

    let items: Vec<ListItem> = view
        .accounts
        .iter()
        .map(|a| match a.account_type {
            AccountType::Checking => {
                let color = if a.balance.cents() < 0 {
                    Color::Red
                } else {
                    crate::theme::GREEN
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "{:<name_width$}",
                        crate::truncate(&a.name, name_width)
                    )),
                    Span::styled(
                        format!("{:<primary_width$}", format!("balance {}", a.balance)),
                        Style::default().fg(color),
                    ),
                    carry_column("buffer", a.carry_balance, adjustment_width),
                    Span::raw(format!("{:<detail_width$}", "")),
                ]))
            }
            AccountType::CreditCard => {
                let owed = a.owed();
                // Owed > 0 is a debt (red); ≤ 0 is a statement credit in your favor (green).
                let owed_color = if owed.cents() > 0 {
                    Color::Red
                } else {
                    crate::theme::GREEN
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "{:<name_width$}",
                        crate::truncate(&a.name, name_width)
                    )),
                    Span::styled(
                        format!("{:<primary_width$}", format!("owed {}", owed)),
                        Style::default().fg(owed_color),
                    ),
                    carry_column("carry", a.carry_balance, adjustment_width),
                    Span::styled(
                        format!(
                            "{:>detail_width$}",
                            crate::truncate(
                                &format!(
                                    "avail {} / limit {}",
                                    a.available_credit.unwrap_or(Money::ZERO),
                                    a.credit_limit.unwrap_or(Money::ZERO)
                                ),
                                detail_width
                            )
                        ),
                        Style::default().fg(Color::Gray),
                    ),
                ]))
            }
        })
        .collect();

    let focused = app.dash_focus == DashFocus::Accounts;
    let mut state = ListState::default();
    // Only show a highlight on the focused panel, so it's obvious which list is live.
    state.select(if focused {
        Some(app.dash_acct_sel)
    } else {
        None
    });

    let list = crate::selectable_list(items).block(crate::selectable_block(" Accounts ", focused));
    frame.render_stateful_widget(list, area, &mut state);
}

fn carry_column(label: &str, carry_balance: Option<Money>, width: usize) -> Span<'static> {
    let carry = carry_balance.unwrap_or(Money::ZERO);
    Span::styled(
        format!("{:<width$}", format!("{label} {carry}")),
        Style::default().fg(Color::Yellow),
    )
}

/// The month header, and the handle for month navigation. Always drawn from `app`'s viewed
/// period so it appears even when that period has no month; `view` (when present) adds the
/// day counter and current/past/upcoming tag. A cyan border cues that it holds focus.
fn draw_header(frame: &mut Frame, area: Rect, app: &App, view: Option<&MonthView>, focused: bool) {
    let label = format!("{:04}-{:02}", app.viewed_year, app.viewed_month);
    let title = match view {
        // The live month gets the "day X of N" progress counter. `days_elapsed` is whole
        // days *since* the 1st (0 on the 1st) — right for accrual, but as a day-of-month
        // label it reads a day short, so +1 turns it into the calendar day (7th → "day 7").
        Some(v) if v.is_current => {
            format!(
                " Leeway — {}   (day {} of {}) ",
                label,
                v.days_elapsed + 1,
                v.month.days_in_month
            )
        }
        // Any other stamped month is wholly in the past or future (only the calendar month
        // contains today), so `days_elapsed` at either extreme tells us which.
        Some(v) => {
            let when = if v.days_elapsed >= v.month.days_in_month {
                "past"
            } else {
                "upcoming"
            };
            format!(" Leeway — {label}   ({when}) ")
        }
        None => format!(" Leeway — {label}   (not stamped) "),
    };

    let block = crate::bordered_block();
    let block = if focused {
        block.border_style(Style::default().fg(crate::theme::MAUVE))
    } else {
        block
    };
    let p = Paragraph::new(Line::from(title.bold()))
        .alignment(Alignment::Center)
        .block(block);
    frame.render_widget(p, area);
}

fn draw_whats_left(
    frame: &mut Frame,
    area: Rect,
    view: &MonthView,
    anims: &SummaryAnimations,
    now: Instant,
) {
    let wl = &view.whats_left;
    let result_color = if wl.whats_left.cents() >= 0 {
        crate::theme::GREEN
    } else {
        Color::Red
    };

    let mut lines = Vec::new();

    // Account terms belong only to the current month. Off-month summaries begin with the
    // same income/expenses row as plan summaries.
    if view.is_current {
        let mut row = summary_term(
            SummaryTerm::Funds,
            Money(display_cents(SummaryTerm::Funds, wl)),
            "funds",
            crate::theme::CYAN,
            anims,
            now,
        );
        row.push(Span::raw("  "));
        if wl.checking_buffer != Money::ZERO {
            row.extend(summary_term(
                SummaryTerm::Buffer,
                Money(display_cents(SummaryTerm::Buffer, wl)),
                "buffer",
                Color::Yellow,
                anims,
                now,
            ));
            row.push(Span::raw("  "));
        }
        row.extend(summary_term(
            SummaryTerm::CardDebt,
            Money(display_cents(SummaryTerm::CardDebt, wl)),
            "card debt",
            Color::Red,
            anims,
            now,
        ));
        if wl.card_carry != Money::ZERO {
            row.push(Span::raw("  "));
            row.extend(summary_term(
                SummaryTerm::Carry,
                Money(display_cents(SummaryTerm::Carry, wl)),
                "carry",
                Color::Yellow,
                anims,
                now,
            ));
        }
        lines.push(Line::from(row));
    }

    let mut row = summary_term(
        SummaryTerm::IncomeLeft,
        Money(display_cents(SummaryTerm::IncomeLeft, wl)),
        "income left",
        crate::theme::GREEN,
        anims,
        now,
    );
    row.push(Span::raw("  "));
    row.extend(summary_term(
        SummaryTerm::BillsLeft,
        Money(display_cents(SummaryTerm::BillsLeft, wl)),
        "expenses left",
        Color::Red,
        anims,
        now,
    ));
    lines.push(Line::from(row));

    lines.push(Line::from(summary_term(
        SummaryTerm::Envelopes,
        Money(display_cents(SummaryTerm::Envelopes, wl)),
        "envelopes",
        crate::theme::MAUVE,
        anims,
        now,
    )));

    let (wl_cents, wl_style) = anims.render(
        SummaryTerm::WhatsLeft,
        display_cents(SummaryTerm::WhatsLeft, wl),
        result_color,
        now,
    );
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("= "),
        Span::styled(
            format!("{:>10}", Money(wl_cents).to_string()),
            wl_style.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  what's left"),
    ]));

    let p = Paragraph::new(lines).block(crate::titled_block(" Summary "));
    frame.render_widget(p, area);
}

fn summary_term(
    term: SummaryTerm,
    amount: Money,
    label: &str,
    color: Color,
    anims: &SummaryAnimations,
    now: Instant,
) -> Vec<Span<'static>> {
    let (cents, amount_style) = anims.render(term, amount.cents(), color, now);
    let sign = if cents < 0 { "−" } else { "+" };
    let amount = Money(cents.abs());
    vec![
        Span::raw(format!("{sign} ")),
        Span::styled(format!("{:>10}", amount.to_string()), amount_style),
        Span::raw(format!("  {:<width$}", label, width = 11)),
    ]
}

fn draw_transactions(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    view: &MonthView,
    direction: Direction,
) {
    let (title, focused, selected) = match direction {
        Direction::In => (
            " Income ",
            app.dash_focus == DashFocus::Income,
            app.dash_income_sel,
        ),
        Direction::Out => (
            " Expenses ",
            app.dash_focus == DashFocus::Expenses,
            app.dash_expense_sel,
        ),
    };

    let content_width = crate::selectable_list_content_width(area);
    let amount_width = 11.min(content_width.saturating_sub(4));
    let label_width = content_width.saturating_sub(4 + amount_width).max(1);
    let items: Vec<ListItem> = view
        .standalone
        .iter()
        .filter(|t| t.direction == direction)
        .enumerate()
        .map(|(i, t)| {
            let check = if t.settled { "[x]" } else { "[ ]" };
            let sign = match t.direction {
                Direction::In => "+",
                Direction::Out => "−",
            };
            let amount = format!("{}{}", sign, t.amount);
            // Settled rows fade back, but the selection band is close enough to
            // that fade to swallow it — so the highlighted row gets the lighter
            // tone instead.
            let style = if t.settled {
                Style::default().fg(crate::theme::muted(focused && i == selected))
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::raw(format!("{} ", check)),
                Span::raw(format!(
                    "{:<label_width$}",
                    crate::truncate(t.display_label(), label_width)
                )),
                Span::styled(format!("{:>amount_width$}", amount), style),
            ]);
            ListItem::new(line).style(style)
        })
        .collect();

    let item_count = items.len();
    let mut state = ListState::default();
    if focused && !items.is_empty() {
        state.select(Some(selected));
    }

    let list = crate::selectable_list(items).block(crate::selectable_block(title, focused));
    frame.render_stateful_widget(list, area, &mut state);
    crate::render_list_scrollbar(frame, area, item_count, state.offset(), focused);
}

fn draw_envelopes(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let content_width = crate::selectable_list_content_width(area);
    let items: Vec<ListItem> = view
        .envelopes
        .iter()
        .map(|e| {
            let mode_icon = match e.envelope.mode {
                Mode::Automatic => "↻",
                Mode::Manual => "✎",
            };
            let daily_rate = matches!(e.envelope.period_type, PeriodType::Daily).then(|| {
                let entered_amount = calc::envelope_period_amount(
                    e.envelope.amount,
                    e.envelope.period_type,
                    view.month.days_in_month,
                );
                format!("{entered_amount}/day")
            });

            // Segmented meter, echoing the landing page's block style: discrete
            // `▮` cells (the glyph carries its own side gaps) split into a mauve
            // fill and a lighter slate track. Two spans so each gets its own
            // colour; both render over the selection band unchanged.
            const REMAINING_WIDTH: usize = 18;
            const TOTAL_WIDTH: usize = 16;
            let wide = content_width >= 60;
            let (label_width, meter_width) = if wide {
                let flexible = content_width.saturating_sub(3 + REMAINING_WIDTH + TOTAL_WIDTH);
                let label = flexible.saturating_div(2).clamp(12, 28);
                (label, flexible.saturating_sub(label).min(24))
            } else {
                (content_width.saturating_sub(3 + REMAINING_WIDTH).max(1), 0)
            };
            let filled = meter_fill(e.consumed, e.envelope.amount, meter_width);
            let meter_fill_span =
                Span::styled("▮".repeat(filled), Style::default().fg(crate::theme::MAUVE));
            let meter_track_span = Span::styled(
                "▮".repeat(meter_width - filled),
                Style::default().fg(crate::theme::METER_TRACK),
            );

            // Fixed, nearby columns keep the row easy to scan without sending the
            // monthly total all the way to the panel's right edge. Daily envelopes
            // append their entered rate as context; monthly is the implicit default.
            let mut spans = vec![
                Span::raw(format!(
                    "{:<label_width$}",
                    crate::truncate(e.envelope.display_label(), label_width)
                )),
                Span::styled(format!("{mode_icon}  "), Style::default().fg(Color::Gray)),
                Span::raw(format!(
                    "{:>REMAINING_WIDTH$}",
                    format!("{} left", e.remaining)
                )),
            ];
            if wide {
                spans.insert(2, meter_fill_span);
                spans.insert(3, meter_track_span);
                spans.push(Span::styled(
                    format!("{:>TOTAL_WIDTH$}", format!("of {}", e.envelope.amount)),
                    Style::default().fg(Color::Gray),
                ));
            }
            if wide
                && content_width >= 96
                && let Some(daily_rate) = daily_rate
            {
                spans.push(Span::styled(
                    format!("  ({daily_rate})"),
                    Style::default().fg(Color::Gray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let focused = app.dash_focus == DashFocus::Envelopes;
    let item_count = items.len();
    let mut state = ListState::default();
    if focused && !view.envelopes.is_empty() {
        state.select(Some(app.dash_env_sel));
    }

    let list = crate::selectable_list(items).block(crate::selectable_block(" Envelopes ", focused));
    frame.render_stateful_widget(list, area, &mut state);
    crate::render_list_scrollbar(frame, area, item_count, state.offset(), focused);
}

/// Number of filled cells in a `width`-wide meter showing `consumed / total`.
/// Zero when there's no budget to fill against.
fn meter_fill(consumed: Money, total: Money, width: usize) -> usize {
    if total.cents() <= 0 {
        return 0;
    }
    let frac = (consumed.cents() as f64 / total.cents() as f64).clamp(0.0, 1.0);
    (frac * width as f64).round() as usize
}

/// A small pill-styled key hint, e.g. ` j/k `.
fn key(label: &str) -> Span<'_> {
    Span::styled(
        label.to_string(),
        Style::default().fg(Color::Black).bg(Color::Gray),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Screen;
    use chrono::NaiveDate;
    use leeway::models::Kind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use rusqlite::Connection;
    use uuid::Uuid;

    fn open_test_conn() -> Connection {
        let mut path = std::env::temp_dir();
        path.push(format!("leeway-dashboard-{}.db", Uuid::new_v4()));
        leeway::db::open(&path).unwrap()
    }

    fn app_with_stamped_month() -> App {
        let mut conn = open_test_conn();
        let plan = ops::create_plan(&conn, "Normal").unwrap();

        let rent = ops::create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        ops::add_plan_item(&conn, &plan, &rent, Money::from_dollars(1800.0)).unwrap();

        let dining = ops::create_series(
            &conn,
            Kind::Envelope,
            "Dining",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Manual),
        )
        .unwrap();
        ops::add_plan_item(&conn, &plan, &dining, Money::from_dollars(300.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        ops::stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

        App {
            conn,
            screen: Screen::Budget,
            budget_target: crate::BudgetTarget::Month {
                year: 2026,
                month: 9,
            },
            last_plan_id: None,
            should_quit: false,
            dash_focus: DashFocus::Income,
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
            plan_focus: crate::PlanFocus::Income,
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
        }
    }

    fn month_view(app: &App) -> MonthView {
        MonthView::build_for(
            &app.conn,
            NaiveDate::from_ymd_opt(2026, 9, 15).unwrap(),
            app.viewed_year,
            app.viewed_month,
        )
        .unwrap()
        .unwrap()
    }

    fn off_month_view(app: &App) -> MonthView {
        MonthView::build_for(
            &app.conn,
            NaiveDate::from_ymd_opt(2026, 10, 15).unwrap(),
            app.viewed_year,
            app.viewed_month,
        )
        .unwrap()
        .unwrap()
    }

    fn tab_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
    }

    fn backtab_key() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)
    }

    fn delete_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
    }

    fn new_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)
    }

    fn carry_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)
    }

    fn period_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)
    }

    fn enter_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn spend_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)
    }

    fn direction_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)
    }

    #[test]
    fn budget_panels_show_every_row_when_space_is_available() {
        let (items, envelopes) = budget_panel_heights(50, 23, 12);

        assert_eq!(items + envelopes, 50);
        assert!(items.saturating_sub(2) >= 23);
        assert!(envelopes.saturating_sub(2) >= 12);
        assert_eq!(envelopes, 14);
    }

    #[test]
    fn budget_panels_share_constrained_space_and_allow_both_to_scroll() {
        let (items, envelopes) = budget_panel_heights(20, 23, 12);

        assert_eq!(items + envelopes, 20);
        assert!(items >= 3);
        assert!(envelopes >= 3);
        assert!(items.saturating_sub(2) < 23);
        assert!(envelopes.saturating_sub(2) < 12);
    }

    fn label_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect()
    }

    fn buffer_lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn off_month_layout_omits_accounts_and_the_account_summary_note() {
        let app = app_with_stamped_month();
        let view = Some(off_month_view(&app));
        let mut terminal = Terminal::new(TestBackend::new(180, 30)).unwrap();

        terminal
            .draw(|frame| draw_detail(frame, frame.area(), &app, &view))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Income"));
        assert!(text.contains("Expenses"));
        assert!(text.contains("Envelopes"));
        assert!(text.contains("Summary"));
        assert!(!text.contains("Accounts"));
        assert!(!text.contains("account balances count"));
        assert!(!text.contains("Account balances apply"));
    }

    #[test]
    fn series_backed_label_key_points_to_the_series_page() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Expenses;
        let view = month_view(&app);

        handle_key(&mut app, label_key(), &Some(view)).unwrap();

        assert!(app.modal.is_none());
        assert_eq!(app.status.as_deref(), Some(crate::SERIES_RENAME_GUIDANCE));
    }

    #[test]
    fn seriesless_label_key_keeps_the_local_compatibility_editor() {
        let mut app = app_with_stamped_month();
        let month_id = month_view(&app).month.id;
        let legacy_id = ops::add_oneoff_txn(
            &app.conn,
            &month_id,
            "ZZ Legacy",
            Direction::Out,
            Money::from_dollars(12.0),
        )
        .unwrap();
        app.dash_focus = DashFocus::Expenses;
        let view = month_view(&app);
        app.dash_expense_sel = view
            .standalone
            .iter()
            .filter(|txn| txn.direction == Direction::Out)
            .position(|txn| txn.id == legacy_id)
            .unwrap();

        handle_key(&mut app, label_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::Text(prompt)) => match prompt.kind {
                PromptKind::TxnLabel { id } => assert_eq!(id, legacy_id),
                _ => panic!("expected transaction label prompt"),
            },
            _ => panic!("expected local label prompt"),
        }
    }

    #[test]
    fn expense_footer_advertises_amount_but_not_label_editing() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Expenses;
        let view = Some(month_view(&app));
        let mut terminal = Terminal::new(TestBackend::new(180, 30)).unwrap();

        terminal.draw(|frame| draw(frame, &app, &view)).unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("amount"));
        assert!(!text.contains("label/amount"));
    }

    #[test]
    fn envelope_footer_advertises_transactions_and_the_direct_edit_verbs() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let view = Some(month_view(&app));
        let mut terminal = Terminal::new(TestBackend::new(180, 30)).unwrap();

        terminal.draw(|frame| draw(frame, &app, &view)).unwrap();
        let text = buffer_text(&terminal);

        // Enter opens the transactions modal; amount/mode/period are edited right here.
        assert!(text.contains("txns"));
        assert!(text.contains("amount"));
        assert!(text.contains("mode"));
        assert!(text.contains("period"));
    }

    #[test]
    fn envelope_rows_show_entered_rate_in_compact_fixed_columns() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let month_id = month_view(&app).month.id;
        ops::add_oneoff_envelope(
            &app.conn,
            &month_id,
            "Long Household Supplies",
            Money::from_dollars(25.0),
            PeriodType::Daily,
            Mode::Manual,
        )
        .unwrap();
        ops::add_oneoff_envelope(
            &app.conn,
            &month_id,
            "Automatic Fuel",
            Money::from_dollars(100.0),
            PeriodType::Monthly,
            Mode::Automatic,
        )
        .unwrap();
        let view = Some(month_view(&app));
        let mut terminal = Terminal::new(TestBackend::new(180, 30)).unwrap();

        terminal.draw(|frame| draw(frame, &app, &view)).unwrap();
        let lines = buffer_lines(&terminal);
        let row = lines
            .iter()
            .find(|line| line.contains("Long Household Supplies"))
            .unwrap();

        assert!(row.contains("$25.00/day"));
        assert!(row.contains("✎"));
        assert_eq!(row.matches('▮').count(), 24);
        assert!(row.contains("$750.00 left      of $750.00  ($25.00/day)"));
        assert!(row.find("$25.00/day").unwrap() > row.find("of $750.00").unwrap());

        let monthly_row = lines.iter().find(|line| line.contains("Dining")).unwrap();
        assert!(!monthly_row.contains("/mo"));
        assert!(!monthly_row.contains("/day"));

        let automatic_row = lines
            .iter()
            .find(|line| line.contains("Automatic Fuel"))
            .unwrap();
        assert!(automatic_row.contains("↻"));
    }

    #[test]
    fn account_label_key_still_opens_the_account_editor() {
        let mut app = app_with_stamped_month();
        let account_id =
            ops::create_checking_account(&app.conn, "Everyday", Money::from_dollars(100.0))
                .unwrap();
        app.dash_focus = DashFocus::Accounts;
        let view = month_view(&app);

        handle_key(&mut app, label_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::Text(prompt)) => match prompt.kind {
                PromptKind::AccountName { id } => assert_eq!(id, account_id),
                _ => panic!("expected account name prompt"),
            },
            _ => panic!("expected account name prompt"),
        }
    }

    #[test]
    fn account_buffer_and_carry_columns_share_the_same_color() {
        let buffer = carry_column("buffer", Some(Money::from_dollars(5000.0)), 18);
        let carry = carry_column("carry", Some(Money::ZERO), 18);

        assert_eq!(buffer.style.fg, Some(Color::Yellow));
        assert_eq!(carry.style.fg, buffer.style.fg);
    }

    #[test]
    fn header_tab_enters_accounts_only_for_current_month() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Header;
        let view = month_view(&app);

        handle_key(&mut app, tab_key(), &Some(view)).unwrap();

        assert!(app.dash_focus == DashFocus::Accounts);
    }

    #[test]
    fn escape_returns_to_sidebar_then_exits() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Expenses;
        let view = Some(month_view(&app));
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        handle_key(&mut app, escape, &view).unwrap();
        assert!(app.dash_focus == DashFocus::Header);
        assert!(!app.should_quit);

        handle_key(&mut app, escape, &view).unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn header_tab_skips_accounts_off_month() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Header;
        let view = off_month_view(&app);
        assert!(!view.is_current);

        handle_key(&mut app, tab_key(), &Some(view)).unwrap();

        assert!(app.dash_focus == DashFocus::Income);
    }

    #[test]
    fn header_backtab_enters_envelopes_for_current_month() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Header;
        let view = month_view(&app);

        handle_key(&mut app, backtab_key(), &Some(view)).unwrap();

        assert!(app.dash_focus == DashFocus::Envelopes);
    }

    #[test]
    fn backtab_enters_accounts_only_for_current_month() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Income;
        let view = month_view(&app);

        handle_key(&mut app, backtab_key(), &Some(view)).unwrap();

        assert!(app.dash_focus == DashFocus::Accounts);
    }

    #[test]
    fn backtab_skips_accounts_off_month() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Income;
        let view = off_month_view(&app);
        assert!(!view.is_current);

        handle_key(&mut app, backtab_key(), &Some(view)).unwrap();

        assert!(app.dash_focus == DashFocus::Header);
    }

    #[test]
    fn direction_key_does_not_move_transaction_between_blocks() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Expenses;
        let view = month_view(&app);
        let rent = view
            .standalone
            .iter()
            .find(|txn| txn.label == "Rent")
            .unwrap();
        assert_eq!(rent.direction, Direction::Out);
        let rent_id = rent.id.clone();

        handle_key(&mut app, direction_key(), &Some(view)).unwrap();

        assert!(app.status.is_none());
        assert!(app.pending_dash_txn.is_none());
        let refreshed = month_view(&app);
        let rent = refreshed
            .standalone
            .iter()
            .find(|txn| txn.id == rent_id)
            .unwrap();
        assert_eq!(rent.direction, Direction::Out);
    }

    #[test]
    fn contextual_series_id_tracks_the_focused_budget_row() {
        let mut app = app_with_stamped_month();
        let view = month_view(&app);

        app.dash_focus = DashFocus::Expenses;
        let rent_series = selected_series_id(&app, &view).expect("rent has a series");
        assert_eq!(
            view.standalone
                .iter()
                .find(|txn| txn.label == "Rent")
                .and_then(|txn| txn.series_id.as_deref()),
            Some(rent_series.as_str())
        );

        app.dash_focus = DashFocus::Envelopes;
        assert_eq!(
            selected_series_id(&app, &view).as_deref(),
            view.envelopes[0].envelope.series_id.as_deref()
        );

        app.dash_focus = DashFocus::Header;
        assert!(selected_series_id(&app, &view).is_none());
    }

    #[test]
    fn new_key_opens_account_type_choice_on_accounts() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Accounts;
        let view = month_view(&app);

        handle_key(&mut app, new_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::Choice(choice)) => {
                assert_eq!(choice.title, "Create which account type?");
                let checking = choice.options.iter().find(|o| o.key == 'h').unwrap();
                match checking.action.as_ref() {
                    Some(ModalAction::BeginNewAccount { account_type }) => {
                        assert_eq!(*account_type, AccountType::Checking);
                    }
                    _ => panic!("expected checking account action"),
                }
                let card = choice.options.iter().find(|o| o.key == 'c').unwrap();
                match card.action.as_ref() {
                    Some(ModalAction::BeginNewAccount { account_type }) => {
                        assert_eq!(*account_type, AccountType::CreditCard);
                    }
                    _ => panic!("expected credit card account action"),
                }
            }
            _ => panic!("expected account type choice modal"),
        }
    }

    #[test]
    fn new_key_keeps_budget_item_flow_outside_accounts() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Expenses;
        let view = month_view(&app);

        handle_key(&mut app, new_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::SeriesSearch(prompt)) => {
                assert!(prompt.block == BudgetBlock::Expenses);
            }
            _ => panic!("expected series search modal"),
        }
    }

    #[test]
    fn carry_key_edits_selected_account_carry_balance() {
        let mut app = app_with_stamped_month();
        let account_id =
            ops::create_checking_account(&app.conn, "Everyday", Money::from_dollars(100.0))
                .unwrap();
        app.dash_focus = DashFocus::Accounts;
        let view = month_view(&app);

        handle_key(&mut app, carry_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::Text(prompt)) => match prompt.kind {
                PromptKind::AccountCarry { id } => assert_eq!(id, account_id),
                _ => panic!("expected account carry prompt"),
            },
            _ => panic!("expected text prompt"),
        }
    }

    #[test]
    fn period_key_cycles_the_focused_envelope_period() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let view = month_view(&app);
        let dining = view
            .envelopes
            .iter()
            .find(|row| row.envelope.label == "Dining")
            .unwrap();
        assert_eq!(dining.envelope.period_type, PeriodType::Monthly);
        let expected_id = dining.envelope.id.clone();

        handle_key(&mut app, period_key(), &Some(view)).unwrap();

        // `p` now flips the period directly on the dashboard rather than deferring it.
        let refreshed = month_view(&app);
        let dining = refreshed
            .envelopes
            .iter()
            .find(|row| row.envelope.id == expected_id)
            .unwrap();
        assert_eq!(dining.envelope.period_type, PeriodType::Daily);
    }

    #[test]
    fn enter_key_opens_envelope_transactions_modal() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let view = month_view(&app);
        let dining = view
            .envelopes
            .iter()
            .find(|row| row.envelope.label == "Dining")
            .unwrap();
        let expected_id = dining.envelope.id.clone();
        let expected_month_id = view.month.id.clone();

        handle_key(&mut app, enter_key(), &Some(view)).unwrap();

        match &app.modal {
            Some(Modal::Envelope(manage)) => {
                assert_eq!(manage.envelope_id, expected_id);
                assert_eq!(manage.month_id, expected_month_id);
                assert_eq!(manage.selected_spend, 0);
            }
            _ => panic!("expected envelope transactions modal"),
        }
        assert!(matches!(app.screen, Screen::Budget));
    }

    #[test]
    fn e_key_does_not_open_the_envelope_modal() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let view = month_view(&app);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &Some(view),
        )
        .unwrap();

        assert!(matches!(app.screen, Screen::Budget));
        assert!(app.modal.is_none());
    }

    #[test]
    fn spend_key_prompts_for_envelope_transaction_label() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let view = month_view(&app);
        let dining = view
            .envelopes
            .iter()
            .find(|row| row.envelope.label == "Dining")
            .unwrap();
        let expected_id = dining.envelope.id.clone();
        let expected_month_id = view.month.id.clone();

        handle_key(&mut app, spend_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::Text(prompt)) => {
                assert_eq!(prompt.title, "Transaction label for Dining");
                match prompt.kind {
                    PromptKind::EnvelopeSpendLabel {
                        envelope_id,
                        month_id,
                    } => {
                        assert_eq!(envelope_id, expected_id);
                        assert_eq!(month_id, expected_month_id);
                    }
                    _ => panic!("expected envelope spending label prompt"),
                }
            }
            _ => panic!("expected text prompt"),
        }
    }

    #[test]
    fn spend_key_allows_automatic_envelope_transactions() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let view = month_view(&app);
        let envelope_id = view.envelopes[0].envelope.id.clone();
        ops::set_envelope_mode(&app.conn, &envelope_id, Mode::Automatic).unwrap();
        let automatic_view = month_view(&app);

        handle_key(&mut app, spend_key(), &Some(automatic_view)).unwrap();

        match app.modal {
            Some(crate::Modal::Text(prompt)) => {
                assert_eq!(prompt.title, "Transaction label for Dining");
            }
            _ => panic!("expected transaction label prompt"),
        }
    }

    #[test]
    fn delete_key_allows_stamped_transaction_instances() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Expenses;
        let view = month_view(&app);
        let rent = view
            .standalone
            .iter()
            .find(|txn| txn.label == "Rent")
            .unwrap();
        assert!(rent.series_id.is_some());
        let expected_id = rent.id.clone();

        handle_key(&mut app, delete_key(), &Some(view)).unwrap();

        assert!(app.status.is_none());
        match app.modal {
            Some(crate::Modal::Confirm(confirm)) => match confirm.action {
                ConfirmAction::DeleteTxn { id } => assert_eq!(id, expected_id),
                _ => panic!("expected delete transaction confirmation"),
            },
            _ => panic!("expected confirmation modal"),
        }
    }

    #[test]
    fn delete_key_confirms_account_delete_on_accounts() {
        let mut app = app_with_stamped_month();
        let account_id =
            ops::create_checking_account(&app.conn, "Everyday", Money::from_dollars(100.0))
                .unwrap();
        app.dash_focus = DashFocus::Accounts;
        let view = month_view(&app);

        handle_key(&mut app, delete_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::Confirm(confirm)) => match confirm.action {
                ConfirmAction::DeleteAccount { id } => assert_eq!(id, account_id),
                _ => panic!("expected delete account confirmation"),
            },
            _ => panic!("expected confirmation modal"),
        }
    }

    #[test]
    fn delete_key_allows_stamped_envelope_instances() {
        let mut app = app_with_stamped_month();
        app.dash_focus = DashFocus::Envelopes;
        let view = month_view(&app);
        let dining = view
            .envelopes
            .iter()
            .find(|row| row.envelope.label == "Dining")
            .unwrap();
        assert!(dining.envelope.series_id.is_some());
        let expected_id = dining.envelope.id.clone();

        handle_key(&mut app, delete_key(), &Some(view)).unwrap();

        assert!(app.status.is_none());
        match app.modal {
            Some(crate::Modal::Confirm(confirm)) => match confirm.action {
                ConfirmAction::DeleteEnvelope { id } => assert_eq!(id, expected_id),
                _ => panic!("expected delete envelope confirmation"),
            },
            _ => panic!("expected confirmation modal"),
        }
    }
}
