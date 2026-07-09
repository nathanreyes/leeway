//! The dashboard screen: the "what's left" daily loop.
//!
//! Two public entry points, mirroring every screen module: `draw` renders a frame from
//! read-only data, and `handle_key` mutates `App` (and the database) in response to a key.

use crate::{
    AddDestination, App, BudgetBlock, ChoiceOption, ConfirmAction, DashFocus, EnvelopeDetail,
    Modal, ModalAction, PromptKind,
    anim::{SummaryAnimations, SummaryTerm, display_cents},
};
use anyhow::Result;
use ballpark::models::{AccountType, Direction, Mode, PeriodType};
use ballpark::money::Money;
use ballpark::ops;
use ballpark::view::{EnvelopeRow, MonthView};
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

    // Page jumps (`P`/`S`) and `q` to quit are handled globally in the event loop before we get
    // here. The Dashboard is the home page, so `Esc` — the canonical "go back" key on the
    // sub-pages — has nowhere further up to go and quits the app.
    if key.code == KeyCode::Esc {
        app.should_quit = true;
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
            // Jump straight to a typed month. Enter opens the same prompt as `m`, prefilled
            // with the current period so it's a small edit.
            KeyCode::Char('m') | KeyCode::Enter => app.open_text(
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

        // Enter / Space = the focused panel's primary action.
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
        // independent snapshot. Direction changes intentionally stay out of this fast path:
        // moving between income and expenses is safer as remove/re-add.
        KeyCode::Char('r') | KeyCode::Char('a') | KeyCode::Char('m') | KeyCode::Char('p')
            if app.dash_focus == DashFocus::Envelopes =>
        {
            app.status = Some("Press e to edit envelope details".into());
        }
        KeyCode::Char('r') => edit_label(app, view), // rename / label
        KeyCode::Char('a') => edit_amount(app, view), // amount
        KeyCode::Char('m') => cycle_mode(app, view)?, // envelope mode
        KeyCode::Char('p') => cycle_period(app, view)?, // envelope period type
        KeyCode::Char('s') => feed_spending(app, view), // file spending into a manual envelope
        KeyCode::Char('x') => delete_selected(app, view), // delete this month's row

        // Carry balance is account-only: checking reserves cash, cards forgive deferred debt.
        KeyCode::Char('c') => {
            if app.dash_focus == DashFocus::Accounts {
                edit_account_carry(app, view);
            }
        }

        // `e` is the Accounts panel's edit alias (Enter also works there).
        KeyCode::Char('e') => {
            if app.dash_focus == DashFocus::Accounts {
                act_on_focus(app, view)?;
            } else if app.dash_focus == DashFocus::Envelopes {
                open_envelope_detail(app, view);
            }
        }
        // Edit a credit card's limit (rarely changed, so it gets its own key).
        KeyCode::Char('l') if app.dash_focus == DashFocus::Accounts => {
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
fn selected_txn<'v>(app: &App, view: &'v MonthView) -> Option<&'v ballpark::models::Txn> {
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
) -> Option<&ballpark::models::Txn> {
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

/// `r`: edit the label/name of the selected row.
fn edit_label(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        app.open_text_replace_on_type(
            "Label",
            t.label.clone(),
            PromptKind::TxnLabel { id: t.id.clone() },
        );
    } else if let Some(e) = selected_env(app, view) {
        app.open_text_replace_on_type(
            "Envelope label",
            e.envelope.label.clone(),
            PromptKind::EnvelopeLabel {
                id: e.envelope.id.clone(),
            },
        );
    } else if app.dash_focus == DashFocus::Accounts
        && let Some(acct) = view.accounts.get(app.dash_acct_sel)
    {
        app.open_text_replace_on_type(
            "Account name",
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
                "It is subtracted from what's left, so a $500 buffer".into(),
                "makes $500 unavailable to spend.".into(),
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
            crate::amount_edit_string(ballpark::calc::envelope_period_amount(
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

fn open_envelope_detail(app: &mut App, view: &MonthView) {
    if let Some(e) = selected_env(app, view) {
        app.modal = Some(Modal::EnvelopeDetail(EnvelopeDetail {
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

/// `s` (and Enter on the Envelopes panel): file a spend into the selected manual envelope.
/// Works for any manual envelope. Automatic envelopes accrue by time, so there's nothing
/// to file.
fn feed_spending(app: &mut App, view: &MonthView) {
    if let Some(e) = selected_env(app, view) {
        match e.envelope.mode {
            Mode::Manual => app.open_text(
                format!("Spending label for {}", e.envelope.label),
                String::new(),
                PromptKind::EnvelopeSpendLabel {
                    envelope_id: e.envelope.id.clone(),
                    month_id: view.month.id.clone(),
                },
            ),
            Mode::Automatic => {
                app.status = Some(
                    "Automatic envelopes accrue by time; switch to manual (m) to file spending"
                        .into(),
                );
            }
        }
    }
}

/// `x`: delete the selected month instance (with a confirm). A stamped row carries its
/// series id for trend/restamp matching, but it is still this month's copy.
fn delete_selected(app: &mut App, view: &MonthView) {
    if let Some(t) = selected_txn(app, view) {
        app.open_confirm(
            format!("Delete “{}”?", t.label),
            ConfirmAction::DeleteTxn { id: t.id.clone() },
        );
    } else if let Some(e) = selected_env(app, view) {
        app.open_confirm(
            format!("Delete envelope “{}” and its spending?", e.envelope.label),
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

/// Do the focused panel's action: settle a bill, or open the balance-edit prompt.
fn act_on_focus(app: &mut App, view: &MonthView) -> Result<()> {
    match app.dash_focus {
        DashFocus::Income | DashFocus::Expenses => {
            if let Some(txn) = selected_txn(app, view) {
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
                    AccountType::Checking => app.open_text_replace_on_type(
                        format!("New balance for {}", acct.name),
                        crate::amount_edit_string(acct.balance),
                        PromptKind::AccountBalance {
                            id: acct.id.clone(),
                        },
                    ),
                    // Credit card: the primary edit is available credit (owed is derived);
                    // the limit gets its own key (`l`).
                    AccountType::CreditCard => app.open_text_replace_on_type(
                        format!("Available credit for {}", acct.name),
                        crate::amount_edit_string(acct.available_credit.unwrap_or(Money::ZERO)),
                        PromptKind::CardAvailable {
                            id: acct.id.clone(),
                        },
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

/// The full month view: accounts at the top, budget blocks in the middle, and the
/// "what's left" summary across the bottom.
fn draw_month_body(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let [acct_area, body, summary_area] = Layout::vertical([
        Constraint::Length(account_block_height(view)),
        Constraint::Min(0),
        // 7 rows so all breakdown lines (funds/card, income/bills/envelopes, carry)
        // fit inside the summary border.
        Constraint::Length(7),
    ])
    .areas(area);
    let [left_items, env_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    let [income_area, expense_area] = Layout::vertical([
        Constraint::Length(crate::income_block_height(txn_count(view, Direction::In))),
        Constraint::Min(0),
    ])
    .areas(left_items);

    draw_accounts(frame, acct_area, app, view);
    draw_transactions(frame, income_area, app, view, Direction::In);
    draw_transactions(frame, expense_area, app, view, Direction::Out);
    draw_envelopes(frame, env_area, app, view);
    draw_whats_left(frame, summary_area, view, &app.summary_anims, app.frame_now);
}

/// One row per account plus the bordered block, capped so a large account list does not
/// crowd out the budget blocks.
fn account_block_height(view: &MonthView) -> u16 {
    if view.is_current {
        view.accounts.len().saturating_add(2).clamp(3, 7) as u16
    } else {
        4
    }
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
            "  Press P to open Plans and stamp one onto it,",
            dim,
        )),
        Line::from(Span::styled(
            "  or k/j to step months and m to jump to one.",
            dim,
        )),
    ];
    let p = Paragraph::new(lines).block(crate::titled_block(" Ballpark "));
    frame.render_widget(p, area);
}

/// Footer hints, adapted to the focused control (and to whether a month exists — with none,
/// there are no panels to Tab to).
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
                key(" m "),
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
            key(" r/a "),
            Span::raw(" edit item  "),
            key(" x "),
            Span::raw(" del"),
        ]),
        DashFocus::Envelopes => Line::from(vec![
            key(" Tab "),
            Span::raw(" panel  "),
            key(" j/k "),
            Span::raw(" move  "),
            key(" s "),
            Span::raw(" spend  "),
            key(" e "),
            Span::raw(" detail  "),
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
            key(" r "),
            Span::raw(" name  "),
            key(" c "),
            Span::raw(" carry  "),
            key(" l "),
            Span::raw(" limit  "),
            key(" x "),
            Span::raw(" del"),
        ]),
    };
    let nav_hints = Line::from(vec![
        key(" P "),
        Span::raw(" plans  "),
        key(" S "),
        Span::raw(" series  "),
        key(" q "),
        Span::raw(" quit"),
    ]);
    crate::draw_split_status_footer(frame, area, left_hints, nav_hints, &app.status);
}

/// The accounts panel. Full-width rows keep the current balance/owed amount near the
/// account name and reserve the final column for each account's buffer/carry setting.
fn draw_accounts(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    if !view.is_current {
        let lines = vec![
            Line::from(Span::styled(
                " Account balances apply only to the current month.",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                " This month uses plan snapshot math: income - bills - envelopes.",
                Style::default().fg(Color::Gray),
            )),
        ];
        let p = Paragraph::new(lines).block(crate::focusable_block(" Accounts ", false));
        frame.render_widget(p, area);
        return;
    }

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
                    Color::Green
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
                    Color::Green
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
                        Style::default().fg(Color::DarkGray),
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
    let style = if carry == Money::ZERO {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Yellow)
    };
    Span::styled(format!("{:<width$}", format!("{label} {carry}")), style)
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
            format!(
                " Ballpark — {}   (day {} of {}) ",
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
            format!(" Ballpark — {label}   ({when}) ")
        }
        None => format!(" Ballpark — {label}   (not stamped) "),
    };

    let focused = app.dash_focus == DashFocus::Header;
    let block = crate::bordered_block();
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

fn draw_whats_left(
    frame: &mut Frame,
    area: Rect,
    view: &MonthView,
    anims: &SummaryAnimations,
    now: Instant,
) {
    let wl = &view.whats_left;
    let result_color = if wl.whats_left.cents() >= 0 {
        Color::Green
    } else {
        Color::Red
    };

    let mut lines = Vec::new();

    // The account-derived terms (funds, card debt, carry) are only part of the headline for
    // the current month; the view already zeroed them off-month. So we only *show* them for
    // the current month, and otherwise say why the balance is income − bills − envelopes.
    if view.is_current {
        let mut row = summary_term(
            SummaryTerm::Funds,
            Money(display_cents(SummaryTerm::Funds, wl)),
            "funds",
            Color::Cyan,
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
    } else {
        lines.push(Line::from(Span::styled(
            "account balances count only in the current month",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let mut row = summary_term(
        SummaryTerm::IncomeLeft,
        Money(display_cents(SummaryTerm::IncomeLeft, wl)),
        "income left",
        Color::Green,
        anims,
        now,
    );
    row.push(Span::raw("  "));
    row.extend(summary_term(
        SummaryTerm::BillsLeft,
        Money(display_cents(SummaryTerm::BillsLeft, wl)),
        "bills left",
        Color::Red,
        anims,
        now,
    ));
    lines.push(Line::from(row));

    lines.push(Line::from(summary_term(
        SummaryTerm::Envelopes,
        Money(display_cents(SummaryTerm::Envelopes, wl)),
        "envelopes",
        Color::Magenta,
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
    let items: Vec<ListItem> = view
        .standalone
        .iter()
        .filter(|t| t.direction == direction)
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
            ]);
            ListItem::new(line).style(style)
        })
        .collect();

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
    let mut state = ListState::default();
    if focused && !items.is_empty() {
        state.select(Some(selected));
    }

    let list = crate::selectable_list(items).block(crate::selectable_block(title, focused));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_envelopes(frame: &mut Frame, area: Rect, app: &App, view: &MonthView) {
    let items: Vec<ListItem> = view
        .envelopes
        .iter()
        .map(|e| {
            let mode = match e.envelope.mode {
                Mode::Automatic => "auto",
                Mode::Manual => "man",
            };
            let period = match e.envelope.period_type {
                PeriodType::Daily => "day",
                PeriodType::Weekly | PeriodType::Monthly => "mo",
            };
            let meter = meter_bar(e.consumed, e.envelope.amount, 8);
            let line = Line::from(vec![
                Span::raw(format!("{:<12}", crate::truncate(&e.envelope.label, 12))),
                Span::styled(
                    format!("{mode}/{period:<3} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:>10} mo ", e.envelope.amount.to_string())),
                Span::styled(meter, Style::default().fg(Color::Magenta)),
                Span::raw(format!(" {:>10} left", e.remaining.to_string())),
            ]);
            ListItem::new(line)
        })
        .collect();

    let focused = app.dash_focus == DashFocus::Envelopes;
    let mut state = ListState::default();
    if focused && !view.envelopes.is_empty() {
        state.select(Some(app.dash_env_sel));
    }

    let list = crate::selectable_list(items).block(crate::selectable_block(" Envelopes ", focused));
    frame.render_stateful_widget(list, area, &mut state);
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
    Span::styled(
        label.to_string(),
        Style::default().fg(Color::Black).bg(Color::Gray),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Screen;
    use ballpark::models::Kind;
    use chrono::NaiveDate;
    use ratatui::crossterm::event::KeyModifiers;
    use rusqlite::Connection;
    use uuid::Uuid;

    fn open_test_conn() -> Connection {
        let mut path = std::env::temp_dir();
        path.push(format!("ballpark-dashboard-{}.db", Uuid::new_v4()));
        ballpark::db::open(&path).unwrap()
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
            screen: Screen::Dashboard,
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
            series_range: ballpark::view::SeriesTimeRange::Last12Stamped,
            series_filter: crate::SeriesFilter::Both,
            plan_focus: crate::PlanFocus::Income,
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

    fn edit_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)
    }

    fn spend_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)
    }

    fn direction_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)
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
    fn period_key_points_envelope_edits_to_detail_modal() {
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

        assert_eq!(
            app.status.as_deref(),
            Some("Press e to edit envelope details")
        );
        let refreshed = month_view(&app);
        let dining = refreshed
            .envelopes
            .iter()
            .find(|row| row.envelope.id == expected_id)
            .unwrap();
        assert_eq!(dining.envelope.period_type, PeriodType::Monthly);
    }

    #[test]
    fn edit_key_opens_envelope_detail_modal() {
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

        handle_key(&mut app, edit_key(), &Some(view)).unwrap();

        match app.modal {
            Some(crate::Modal::EnvelopeDetail(detail)) => {
                assert_eq!(detail.envelope_id, expected_id);
                assert_eq!(detail.month_id, expected_month_id);
                assert_eq!(detail.selected_spend, 0);
            }
            _ => panic!("expected envelope detail modal"),
        }
    }

    #[test]
    fn spend_key_prompts_for_envelope_spending_label() {
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
                assert_eq!(prompt.title, "Spending label for Dining");
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
