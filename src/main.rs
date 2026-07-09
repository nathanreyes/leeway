//! Ballpark's terminal UI.
//!
//! The app is now multi-screen, so this file holds the *shared* scaffolding and each
//! screen lives in its own module:
//!   - `dashboard` — the "what's left" view (the daily loop)
//!   - `plans`     — the plans list and the plan editor (templates you stamp)
//!
//! `main.rs` owns three cross-cutting concerns the screens share:
//!   1. `App` — all mutable UI state (the data itself stays in SQLite).
//!   2. The **modal** system — a text prompt and a yes/no confirm that float over any
//!      screen and, while open, capture all input.
//!   3. The event loop: for the current screen, load its data, draw, read one key, and
//!      route it either to the open modal or to the screen's own handler.

mod dashboard;
mod plans;
mod series;

use anyhow::Result;
use ballpark::models::{AccountType, Direction, Kind, PeriodType, Series};
use ballpark::money::Money;
use ballpark::view::SeriesTimeRange;
use ballpark::{db, ops, queries};
use chrono::{Datelike, Local, NaiveDate};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, HighlightSpacing, List, ListItem, Padding, Paragraph,
};
use ratatui::{DefaultTerminal, Frame};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Duration;

/// Which screen is showing. `PlanEditor` carries the id of the plan being edited —
/// that's how the loop knows which plan's items to load.
pub enum Screen {
    Dashboard,
    Series,
    Plans,
    PlanEditor { plan_id: String },
}

/// Which control the dashboard's keys act on. The month header owns month navigation; the
/// budget blocks own row-level actions; accounts remain a compact support panel for
/// balance edits. We track which is "focused" and route j/k, Enter, and `n` to it.
#[derive(Clone, Copy, PartialEq)]
pub enum DashFocus {
    Header,
    Income,
    Expenses,
    Envelopes,
    Accounts,
}

/// Which budget block is focused in the plan editor. It mirrors the dashboard's item
/// grouping, minus header/accounts.
#[derive(Clone, Copy, PartialEq)]
pub enum PlanFocus {
    Income,
    Expenses,
    Envelopes,
}

/// The Series page's membership filter: show every series, only those used by some plan, or
/// only "ad-hoc" ones (in no plan — freshly created, or only ever added straight into a
/// month). Classification is by `SeriesDetailView.plan_names`: empty ⟺ ad-hoc.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SeriesFilter {
    Both,
    Plans,
    AdHoc,
}

impl SeriesFilter {
    /// Cycle order for the `f` key: Both → Plans → Ad-hoc → Both.
    pub fn next(self) -> Self {
        match self {
            SeriesFilter::Both => SeriesFilter::Plans,
            SeriesFilter::Plans => SeriesFilter::AdHoc,
            SeriesFilter::AdHoc => SeriesFilter::Both,
        }
    }
}

/// A floating dialog that captures input while open. Free text (names, amounts, a month
/// to stamp), a destructive-action confirm, or a hotkey menu (Merge/Replace/Cancel).
pub enum Modal {
    Text(TextPrompt),
    SeriesSearch(SeriesSearch),
    Confirm(Confirm),
    Choice(Choice),
}

/// A shared budget block vocabulary for add flows that work in both the dashboard and
/// plan editor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BudgetBlock {
    Income,
    Expenses,
    Envelopes,
}

const BOX_LEFT_PADDING: u16 = 0;
const BOX_RIGHT_PADDING: u16 = 0;
const LIST_HIGHLIGHT_SYMBOL: &str = "▌";
const LIST_HIGHLIGHT_SYMBOL_WIDTH: u16 = 1;
const LIST_RIGHT_PADDING: u16 = 1;

pub(crate) fn bordered_block() -> Block<'static> {
    Block::default().borders(Borders::ALL).padding(Padding::new(
        BOX_LEFT_PADDING,
        BOX_RIGHT_PADDING,
        0,
        0,
    ))
}

pub(crate) fn titled_block(title: impl Into<String>) -> Block<'static> {
    bordered_block().title(title.into())
}

pub(crate) fn focusable_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    let block = titled_block(title);
    if focused {
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block
    }
}

pub(crate) fn selectable_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    focusable_block(title, focused).padding(Padding::new(0, LIST_RIGHT_PADDING, 0, 0))
}

pub(crate) fn selectable_list<'a>(items: Vec<ListItem<'a>>) -> List<'a> {
    List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(LIST_HIGHLIGHT_SYMBOL)
        .highlight_spacing(HighlightSpacing::Always)
}

pub(crate) fn selectable_list_content_width(area: Rect) -> usize {
    area.width
        .saturating_sub(
            2 + BOX_LEFT_PADDING
                + BOX_RIGHT_PADDING
                + LIST_RIGHT_PADDING
                + LIST_HIGHLIGHT_SYMBOL_WIDTH,
        )
        .into()
}

#[derive(Clone)]
pub enum AddDestination {
    Plan { plan_id: String },
    Month { month_id: String },
}

/// Search existing series in the focused block, or create one if there are no matches.
pub struct SeriesSearch {
    pub title: String,
    pub buffer: String,
    pub selected: usize,
    pub block: BudgetBlock,
    pub destination: AddDestination,
    pub all_series: Vec<Series>,
}

/// A little menu: each option has a hotkey, a label, and an action (`None` = just cancel).
pub struct Choice {
    pub title: String,
    pub options: Vec<ChoiceOption>,
}

#[derive(Clone)]
pub struct ChoiceOption {
    pub key: char,
    pub label: String,
    pub action: Option<ModalAction>,
}

/// The deferred effect a chosen option runs. Carries the ids it needs so the action is
/// self-contained when it fires (the modal is already closed by then).
#[derive(Clone)]
pub enum ModalAction {
    BeginNewAccount {
        account_type: AccountType,
    },
    RestampMerge {
        month_id: String,
        plan_id: String,
    },
    /// Replace, but scope (wipe vs keep items outside the target plan) is decided in a
    /// follow-up choice.
    RestampReplace {
        month_id: String,
        plan_id: String,
    },
    RestampReplaceScoped {
        month_id: String,
        plan_id: String,
        keep_outside_plan: bool,
    },
    SetSeriesRange {
        range: SeriesTimeRange,
    },
    /// Chosen the kind of a new series on the Series page; now prompt for its label. The
    /// block carries kind + direction (and "is envelope") via its existing helpers.
    BeginNewSeries {
        block: BudgetBlock,
    },
}

/// A single-line text input. `kind` records what to do with the text on submit.
pub struct TextPrompt {
    pub title: String,
    pub buffer: String,
    pub help: Vec<String>,
    pub replace_on_next_char: bool,
    pub kind: PromptKind,
}

/// What a text prompt's submitted value means.
pub enum PromptKind {
    NewPlan,
    RenamePlan {
        id: String,
    },
    /// Edit a series' label — affects every plan that includes it.
    SeriesLabel {
        series_id: String,
    },
    /// Edit a series' optional category — affects every plan that includes it.
    SeriesCategory {
        series_id: String,
    },
    /// Create a brand-new series on the Series page. The block fixes kind + direction (and
    /// seeds monthly/default-mode for envelopes); the submitted text is the label.
    NewSeries {
        block: BudgetBlock,
    },
    /// Edit a plan_item's per-plan budgeted amount.
    ItemAmount {
        id: String,
    },
    /// Collect the amount, then add a selected or newly-created series to a plan/month.
    SeriesAddAmount {
        destination: AddDestination,
        block: BudgetBlock,
        selection: SeriesSelection,
    },
    StampMonth {
        plan_id: String,
    },
    /// Navigate the dashboard to a typed `YYYY-MM` period (view only — no stamping).
    GoToMonth,
    AccountBalance {
        id: String,
    },
    CardAvailable {
        id: String,
    },
    CardLimit {
        id: String,
    },
    NewAccountName {
        account_type: AccountType,
    },
    NewCheckingBalance {
        name: String,
    },
    NewCardLimit {
        name: String,
    },
    NewCardAvailable {
        name: String,
        limit: Money,
    },
    AccountName {
        id: String,
    },
    AccountCarry {
        id: String,
    },
    /// Edit a month transaction's label (this instance only).
    TxnLabel {
        id: String,
    },
    /// Edit a month transaction's amount.
    TxnAmount {
        id: String,
    },
    /// Edit a month envelope's label.
    EnvelopeLabel {
        id: String,
    },
    /// Edit a month envelope's monthly amount.
    EnvelopeAmount {
        id: String,
    },
    /// File a spend into a (manual) envelope: the value is the dollar amount spent. Carries
    /// the month so the new txn lands in the right period.
    EnvelopeSpend {
        envelope_id: String,
        month_id: String,
    },
}

pub enum SeriesSelection {
    Existing { series_id: String, label: String },
    New { label: String },
}

pub struct Confirm {
    pub title: String,
    pub action: ConfirmAction,
}

/// Destructive actions that require a yes/no before running.
pub enum ConfirmAction {
    DeletePlan {
        id: String,
    },
    DeleteItem {
        id: String,
    },
    /// Delete a shared series definition. Guarded in `ops::delete_series` (blocked while any
    /// plan still uses it); orphaning its id on past months is the intended, safe outcome.
    DeleteSeries {
        series_id: String,
    },
    /// Delete a transaction instance from a month.
    DeleteTxn {
        id: String,
    },
    /// Delete an envelope instance (and any spending filed in it) from a month.
    DeleteEnvelope {
        id: String,
    },
    /// Delete an account when no transactions reference it.
    DeleteAccount {
        id: String,
    },
}

/// All mutable UI state. Each screen keeps its own selection index so moving between
/// screens doesn't scramble where you were.
pub struct App {
    pub conn: Connection,
    pub screen: Screen,
    pub should_quit: bool,
    pub dash_focus: DashFocus,
    /// The period the dashboard is showing, as a (year, month). Starts on today's calendar
    /// month and moves as you navigate the header; the view for it is looked up fresh each
    /// frame, so a period with no stamped month simply renders the "not stamped" prompt.
    pub viewed_year: i32,
    pub viewed_month: u32,
    pub dash_income_sel: usize,
    pub dash_expense_sel: usize,
    pub dash_env_sel: usize,
    pub dash_acct_sel: usize,
    pub plans_sel: usize,
    pub series_sel: usize,
    pub series_search: String,
    pub series_search_active: bool,
    pub series_range: SeriesTimeRange,
    pub series_filter: SeriesFilter,
    pub plan_focus: PlanFocus,
    pub editor_income_sel: usize,
    pub editor_expense_sel: usize,
    pub editor_env_sel: usize,
    /// After creating an item we want to jump the selection onto it, but its list
    /// position isn't known until the next reload (rows are sorted). We stash the id
    /// here and the loop resolves it to an index once the items are loaded.
    pub pending_select: Option<String>,
    /// The dashboard's counterparts to `pending_select`: after creating a txn or envelope
    /// we stash its id here, and the event loop resolves it to a list index on the next
    /// reload (the lists are sorted, so the position isn't known until then).
    pub pending_dash_txn: Option<String>,
    pub pending_dash_env: Option<String>,
    pub pending_dash_account: Option<String>,
    /// The Series page's counterpart to `pending_select`: after creating a series we stash
    /// its id here, and the event loop resolves it to the (search-filtered) list position on
    /// the next reload so the new series lands selected.
    pub pending_series_select: Option<String>,
    pub modal: Option<Modal>,
    /// A transient one-liner (errors, confirmations) shown in the footer.
    pub status: Option<String>,
}

impl App {
    fn open_text(&mut self, title: impl Into<String>, buffer: impl Into<String>, kind: PromptKind) {
        self.modal = Some(Modal::Text(TextPrompt {
            title: title.into(),
            buffer: buffer.into(),
            help: Vec::new(),
            replace_on_next_char: false,
            kind,
        }));
    }

    /// Like `open_text_replace_on_type` (the seeded text is preselected, so the first
    /// keystroke replaces it) but also shows explanatory `help` lines. Its only use today
    /// is the account carry-balance edit, which is an amount, so it preselects.
    fn open_text_with_help(
        &mut self,
        title: impl Into<String>,
        buffer: impl Into<String>,
        help: Vec<String>,
        kind: PromptKind,
    ) {
        self.modal = Some(Modal::Text(TextPrompt {
            title: title.into(),
            buffer: buffer.into(),
            help,
            replace_on_next_char: true,
            kind,
        }));
    }

    fn open_text_replace_on_type(
        &mut self,
        title: impl Into<String>,
        buffer: impl Into<String>,
        kind: PromptKind,
    ) {
        self.modal = Some(Modal::Text(TextPrompt {
            title: title.into(),
            buffer: buffer.into(),
            help: Vec::new(),
            replace_on_next_char: true,
            kind,
        }));
    }

    fn open_series_search(
        &mut self,
        destination: AddDestination,
        block: BudgetBlock,
    ) -> Result<()> {
        self.modal = Some(Modal::SeriesSearch(SeriesSearch {
            title: format!("Add {}", block.noun()),
            buffer: String::new(),
            selected: 0,
            block,
            destination,
            all_series: queries::list_series(&self.conn)?,
        }));
        Ok(())
    }

    fn open_confirm(&mut self, title: impl Into<String>, action: ConfirmAction) {
        self.modal = Some(Modal::Confirm(Confirm {
            title: title.into(),
            action,
        }));
    }

    fn open_choice(&mut self, title: impl Into<String>, options: Vec<ChoiceOption>) {
        self.modal = Some(Modal::Choice(Choice {
            title: title.into(),
            options,
        }));
    }
}

fn main() -> Result<()> {
    let path = PathBuf::from("ballpark.db");
    let mut conn = db::open(&path)?;
    // On a fresh database this stamps the current calendar month, satisfying "if no month
    // exists, create one" for a first-ever launch.
    ops::seed_demo(&mut conn)?;

    // The app opens on the current calendar month.
    let today = Local::now().date_naive();

    let mut app = App {
        conn,
        screen: Screen::Dashboard,
        should_quit: false,
        dash_focus: DashFocus::Header,
        viewed_year: today.year(),
        viewed_month: today.month(),
        dash_income_sel: 0,
        dash_expense_sel: 0,
        dash_env_sel: 0,
        dash_acct_sel: 0,
        plans_sel: 0,
        series_sel: 0,
        series_search: String::new(),
        series_search_active: false,
        series_range: SeriesTimeRange::Last12Stamped,
        series_filter: SeriesFilter::Both,
        plan_focus: PlanFocus::Income,
        editor_income_sel: 0,
        editor_expense_sel: 0,
        editor_env_sel: 0,
        pending_select: None,
        pending_dash_txn: None,
        pending_dash_env: None,
        pending_dash_account: None,
        pending_series_select: None,
        modal: None,
        status: None,
    };

    let terminal = ratatui::init();
    let result = run(terminal, &mut app);
    ratatui::restore();
    result
}

/// The event loop. Each iteration loads only the data the current screen needs, draws,
/// then reads and routes one key.
fn run(mut terminal: DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        // Resolve "today" fresh every iteration from the local system clock. Combined with
        // `read_key`'s idle wake-up (below), this means a date that rolls over while the app
        // sits open — e.g. left running past midnight — is picked up and redrawn on its own,
        // rather than being stuck on whatever day it was at launch.
        let today = Local::now().date_naive();

        // `match` on the screen keeps each branch's data local — the borrow of `app.conn`
        // for loading is released before we take a `&mut app` to handle input.
        match &app.screen {
            Screen::Dashboard => {
                let view = ballpark::view::MonthView::build_for(
                    &app.conn,
                    today,
                    app.viewed_year,
                    app.viewed_month,
                )?;
                match &view {
                    Some(v) => {
                        // Resolve "select the item I just created" now that the sorted lists
                        // are loaded (mirrors the plan editor's pending_select handling).
                        if let Some(target) = app.pending_dash_txn.take() {
                            if let Some(txn) = v.standalone.iter().find(|t| t.id == target) {
                                app.dash_focus = match txn.direction {
                                    ballpark::models::Direction::In => DashFocus::Income,
                                    ballpark::models::Direction::Out => DashFocus::Expenses,
                                };
                                if let Some(idx) = dashboard_txn_index(v, &target, app.dash_focus) {
                                    match app.dash_focus {
                                        DashFocus::Income => app.dash_income_sel = idx,
                                        DashFocus::Expenses => app.dash_expense_sel = idx,
                                        _ => {}
                                    }
                                }
                            }
                        }
                        if let Some(target) = app.pending_dash_env.take() {
                            if let Some(idx) =
                                v.envelopes.iter().position(|e| e.envelope.id == target)
                            {
                                app.dash_env_sel = idx;
                                app.dash_focus = DashFocus::Envelopes;
                            }
                        }
                        if let Some(target) = app.pending_dash_account.take() {
                            if v.is_current {
                                if let Some(idx) = v.accounts.iter().position(|a| a.id == target) {
                                    app.dash_acct_sel = idx;
                                    app.dash_focus = DashFocus::Accounts;
                                }
                            }
                        }
                        if v.is_current {
                            clamp(&mut app.dash_acct_sel, v.accounts.len());
                        } else {
                            app.dash_acct_sel = 0;
                            if app.dash_focus == DashFocus::Accounts {
                                app.dash_focus = DashFocus::Income;
                            }
                        }
                        clamp(
                            &mut app.dash_income_sel,
                            dashboard_txn_count(v, DashFocus::Income),
                        );
                        clamp(
                            &mut app.dash_expense_sel,
                            dashboard_txn_count(v, DashFocus::Expenses),
                        );
                        clamp(&mut app.dash_env_sel, v.envelopes.len());
                    }
                    // No month for this period → the header is the only sensible control, so
                    // pin focus there. That keeps j/k/m navigation working with nothing else
                    // on screen (and stops Tab from stranding focus on an absent panel).
                    None => app.dash_focus = DashFocus::Header,
                }
                terminal.draw(|f| {
                    dashboard::draw(f, app, &view);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key()? {
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else if handle_global_key(app, key) {
                    } else {
                        dashboard::handle_key(app, key, &view)?;
                    }
                }
            }

            Screen::Series => {
                let view =
                    ballpark::view::SeriesPageView::build(&app.conn, today, app.series_range)?;
                // Resolve "select the series I just created" now that the list is loaded.
                if let Some(target) = app.pending_series_select.take() {
                    series::select_series_by_id(app, &view, &target);
                }
                let visible_count = series::visible_count(app, &view);
                clamp(&mut app.series_sel, visible_count);
                terminal.draw(|f| {
                    series::draw(f, app, &view);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key()? {
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else if app.series_search_active {
                        // Search is a text-entry mode: it must own every key (including the
                        // would-be global jump/quit keys) so you can type them into the query.
                        series::handle_key(app, key, &view, today)?;
                    } else if handle_global_key(app, key) {
                    } else {
                        series::handle_key(app, key, &view, today)?;
                    }
                }
            }

            Screen::Plans => {
                let summaries = queries::plan_summaries(&app.conn)?;
                clamp(&mut app.plans_sel, summaries.len());
                terminal.draw(|f| {
                    plans::draw_list(f, app, &summaries);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key()? {
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else if handle_global_key(app, key) {
                    } else {
                        plans::handle_list_key(app, key, &summaries)?;
                    }
                }
            }

            Screen::PlanEditor { plan_id } => {
                let plan_id = plan_id.clone();
                // The plan may have been deleted (e.g. via a confirm) — fall back to the list.
                let Some(plan) = queries::get_plan(&app.conn, &plan_id)? else {
                    app.screen = Screen::Plans;
                    continue;
                };
                let entries = queries::load_plan_entries(&app.conn, &plan_id)?;

                // Resolve a pending "select this new item" request now that rows are loaded.
                if let Some(target) = app.pending_select.take() {
                    if let Some(entry) = entries.iter().find(|e| e.item_id == target) {
                        app.plan_focus = plan_focus_for_entry(entry);
                        if let Some(idx) = plan_entry_index(&entries, &target, app.plan_focus) {
                            match app.plan_focus {
                                PlanFocus::Income => app.editor_income_sel = idx,
                                PlanFocus::Expenses => app.editor_expense_sel = idx,
                                PlanFocus::Envelopes => app.editor_env_sel = idx,
                            }
                        }
                    }
                }
                clamp(
                    &mut app.editor_income_sel,
                    plan_entry_count(&entries, PlanFocus::Income),
                );
                clamp(
                    &mut app.editor_expense_sel,
                    plan_entry_count(&entries, PlanFocus::Expenses),
                );
                clamp(
                    &mut app.editor_env_sel,
                    plan_entry_count(&entries, PlanFocus::Envelopes),
                );

                terminal.draw(|f| {
                    plans::draw_editor(f, app, &plan, &entries);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key()? {
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else if handle_global_key(app, key) {
                    } else {
                        plans::handle_editor_key(app, key, &plan, &entries)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// How long the loop waits for input before waking on its own to redraw. This is what lets
/// the day tick over unattended: even with no keypresses we re-enter the loop at least this
/// often, re-resolve `today`, and repaint. A minute is far finer than a day, so the header
/// updates within a minute of midnight, at negligible idle cost.
const IDLE_TICK: Duration = Duration::from_secs(60);

/// Read one key *press*. Returns `None` for releases, resizes, mouse, or an idle timeout —
/// anything we don't act on (the next frame redraws regardless). `event::poll` returns as
/// soon as input arrives, so waiting up to `IDLE_TICK` never adds latency to real keystrokes.
fn read_key() -> Result<Option<KeyEvent>> {
    if !event::poll(IDLE_TICK)? {
        return Ok(None); // idle wake-up: no input, but let the loop redraw with a fresh date
    }
    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => Ok(Some(key)),
        _ => Ok(None),
    }
}

/// Keys that mean the same thing on *every* page, checked ahead of the page's own handler.
///
/// The two sub-pages each get an uppercase jump key — `P`lans and `S`eries — so they're reachable
/// from anywhere without hunting for a page-specific shortcut, and `q` always quits. The Dashboard
/// (the month view) is the home page you return to with `Esc`, so it needs no jump key of its own.
/// Keeping these in one place is what makes navigation consistent: previously each page decided
/// for itself what `q` did (Plans treated it as "go back", everyone else quit), and the jump keys
/// were a mix of upper- and lowercase scattered across the page handlers.
///
/// This runs *after* the modal check and (for Series) the search check in the event loop, so a
/// text-entry mode still receives these characters as literal input rather than losing them to
/// navigation. Lowercase `p`/`s`/`d` are left free for pages to use as their own verbs.
fn handle_global_key(app: &mut App, key: KeyEvent) -> bool {
    let screen = match key.code {
        KeyCode::Char('P') => Screen::Plans,
        KeyCode::Char('S') => Screen::Series,
        KeyCode::Char('q') => {
            app.should_quit = true;
            return true;
        }
        _ => return false,
    };
    app.screen = screen;
    app.status = None;
    true
}

/// Clamp a selection index so it always points at a real row (or 0 when the list empties).
fn clamp(sel: &mut usize, len: usize) {
    if len == 0 {
        *sel = 0;
    } else if *sel >= len {
        *sel = len - 1;
    }
}

impl BudgetBlock {
    fn noun(self) -> &'static str {
        match self {
            BudgetBlock::Income => "income",
            BudgetBlock::Expenses => "expense",
            BudgetBlock::Envelopes => "envelope",
        }
    }

    fn kind(self) -> Kind {
        match self {
            BudgetBlock::Income | BudgetBlock::Expenses => Kind::Transaction,
            BudgetBlock::Envelopes => Kind::Envelope,
        }
    }

    fn direction(self) -> Option<Direction> {
        match self {
            BudgetBlock::Income => Some(Direction::In),
            BudgetBlock::Expenses => Some(Direction::Out),
            BudgetBlock::Envelopes => None,
        }
    }

    fn matches_series(self, series: &Series) -> bool {
        match self {
            BudgetBlock::Income => {
                series.kind == Kind::Transaction && series.direction == Some(Direction::In)
            }
            BudgetBlock::Expenses => {
                series.kind == Kind::Transaction && series.direction == Some(Direction::Out)
            }
            BudgetBlock::Envelopes => series.kind == Kind::Envelope,
        }
    }
}

pub(crate) fn income_block_height(row_count: usize) -> u16 {
    row_count.saturating_add(2).clamp(3, 7) as u16
}

fn reset_dashboard_selections(app: &mut App) {
    app.dash_income_sel = 0;
    app.dash_expense_sel = 0;
    app.dash_env_sel = 0;
    app.dash_acct_sel = 0;
}

fn reset_editor_selections(app: &mut App) {
    app.plan_focus = PlanFocus::Income;
    app.editor_income_sel = 0;
    app.editor_expense_sel = 0;
    app.editor_env_sel = 0;
}

fn dashboard_txn_count(view: &ballpark::view::MonthView, focus: DashFocus) -> usize {
    view.standalone
        .iter()
        .filter(|txn| dashboard_txn_matches(txn, focus))
        .count()
}

fn dashboard_txn_index(
    view: &ballpark::view::MonthView,
    txn_id: &str,
    focus: DashFocus,
) -> Option<usize> {
    view.standalone
        .iter()
        .filter(|txn| dashboard_txn_matches(txn, focus))
        .position(|txn| txn.id == txn_id)
}

fn dashboard_txn_matches(txn: &ballpark::models::Txn, focus: DashFocus) -> bool {
    matches!(
        (focus, txn.direction),
        (DashFocus::Income, ballpark::models::Direction::In)
            | (DashFocus::Expenses, ballpark::models::Direction::Out)
    )
}

fn plan_entry_count(entries: &[ballpark::models::PlanEntry], focus: PlanFocus) -> usize {
    entries
        .iter()
        .filter(|entry| plan_entry_matches(entry, focus))
        .count()
}

fn plan_entry_index(
    entries: &[ballpark::models::PlanEntry],
    item_id: &str,
    focus: PlanFocus,
) -> Option<usize> {
    entries
        .iter()
        .filter(|entry| plan_entry_matches(entry, focus))
        .position(|entry| entry.item_id == item_id)
}

fn plan_focus_for_entry(entry: &ballpark::models::PlanEntry) -> PlanFocus {
    match entry.series.kind {
        ballpark::models::Kind::Envelope => PlanFocus::Envelopes,
        ballpark::models::Kind::Transaction => match entry.series.direction {
            Some(ballpark::models::Direction::In) => PlanFocus::Income,
            _ => PlanFocus::Expenses,
        },
    }
}

fn plan_entry_matches(entry: &ballpark::models::PlanEntry, focus: PlanFocus) -> bool {
    match focus {
        PlanFocus::Income => {
            entry.series.kind == ballpark::models::Kind::Transaction
                && entry.series.direction == Some(ballpark::models::Direction::In)
        }
        PlanFocus::Expenses => {
            entry.series.kind == ballpark::models::Kind::Transaction
                && entry.series.direction != Some(ballpark::models::Direction::In)
        }
        PlanFocus::Envelopes => entry.series.kind == ballpark::models::Kind::Envelope,
    }
}

// --- Modal input ---------------------------------------------------------------

fn handle_modal_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.modal.as_ref() {
        Some(Modal::Text(_)) => handle_text_key(app, key),
        Some(Modal::SeriesSearch(_)) => handle_series_search_key(app, key),
        Some(Modal::Confirm(_)) => handle_confirm_key(app, key),
        Some(Modal::Choice(_)) => handle_choice_key(app, key),
        None => Ok(()),
    }
}

fn handle_choice_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // Resolve the pressed key to an option's action *before* mutating the modal.
    // Outer Option: was there a matching key at all? Inner Option: does it act, or cancel?
    let resolved: Option<Option<ModalAction>> = match (app.modal.as_ref(), key.code) {
        (Some(Modal::Choice(_)), KeyCode::Esc) => Some(None),
        (Some(Modal::Choice(c)), KeyCode::Char(ch)) => c
            .options
            .iter()
            .find(|o| o.key == ch)
            .map(|o| o.action.clone()),
        _ => None,
    };

    match resolved {
        None => {}                      // no matching hotkey — ignore
        Some(None) => app.modal = None, // a cancel option (or Esc)
        Some(Some(action)) => {
            app.modal = None;
            run_modal_action(app, action)?;
        }
    }
    Ok(())
}

/// Execute a chosen restamp action. Replace fans out into a scope choice when the month
/// has rows outside the target plan, so we never silently wipe them.
fn run_modal_action(app: &mut App, action: ModalAction) -> Result<()> {
    match action {
        ModalAction::BeginNewAccount { account_type } => {
            let title = match account_type {
                AccountType::Checking => "New checking account name",
                AccountType::CreditCard => "New credit card name",
            };
            app.open_text(
                title,
                String::new(),
                PromptKind::NewAccountName { account_type },
            );
        }
        ModalAction::RestampMerge { month_id, plan_id } => {
            ops::restamp_merge(&mut app.conn, &month_id, &plan_id)?;
            finish_restamp(app, "Merged plan into the month");
        }
        ModalAction::RestampReplace { month_id, plan_id } => {
            if ops::month_has_items_outside_plan(&app.conn, &month_id, &plan_id)? {
                app.open_choice(
                    "This month has items outside this plan. Replace how?",
                    vec![
                        ChoiceOption {
                            key: 'w',
                            label: "Wipe outside-plan items".into(),
                            action: Some(ModalAction::RestampReplaceScoped {
                                month_id: month_id.clone(),
                                plan_id: plan_id.clone(),
                                keep_outside_plan: false,
                            }),
                        },
                        ChoiceOption {
                            key: 'k',
                            label: "Keep outside-plan items".into(),
                            action: Some(ModalAction::RestampReplaceScoped {
                                month_id,
                                plan_id,
                                keep_outside_plan: true,
                            }),
                        },
                        ChoiceOption {
                            key: 'c',
                            label: "Cancel".into(),
                            action: None,
                        },
                    ],
                );
            } else {
                ops::restamp_replace(&mut app.conn, &month_id, &plan_id, false)?;
                finish_restamp(app, "Replaced the month");
            }
        }
        ModalAction::RestampReplaceScoped {
            month_id,
            plan_id,
            keep_outside_plan,
        } => {
            ops::restamp_replace(&mut app.conn, &month_id, &plan_id, keep_outside_plan)?;
            let msg = if keep_outside_plan {
                "Replaced (kept outside-plan items)"
            } else {
                "Replaced the month"
            };
            finish_restamp(app, msg);
        }
        ModalAction::SetSeriesRange { range } => {
            app.series_range = range;
            app.status = Some(format!("Range: {}", range.label(Local::now().date_naive())));
        }
        ModalAction::BeginNewSeries { block } => {
            app.open_text(
                format!("New {} name", block.noun()),
                String::new(),
                PromptKind::NewSeries { block },
            );
        }
    }
    Ok(())
}

fn finish_restamp(app: &mut App, message: &str) {
    reset_dashboard_selections(app);
    app.screen = Screen::Dashboard;
    app.status = Some(message.to_string());
}

fn handle_text_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.modal = None,
        KeyCode::Enter => submit_text(app)?,
        KeyCode::Backspace => {
            if let Some(Modal::Text(p)) = &mut app.modal {
                p.buffer.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(Modal::Text(p)) = &mut app.modal {
                if p.replace_on_next_char {
                    p.buffer.clear();
                    p.replace_on_next_char = false;
                }
                p.buffer.push(c);
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_series_search_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.modal = None,
        KeyCode::Enter => submit_series_search(app)?,
        KeyCode::Backspace => {
            if let Some(Modal::SeriesSearch(p)) = &mut app.modal {
                p.buffer.pop();
                p.selected = 0;
            }
        }
        KeyCode::Down => {
            if let Some(Modal::SeriesSearch(p)) = &mut app.modal {
                let count = series_search_matches(p).len();
                if count > 0 && p.selected + 1 < count {
                    p.selected += 1;
                }
            }
        }
        KeyCode::Up => {
            if let Some(Modal::SeriesSearch(p)) = &mut app.modal {
                p.selected = p.selected.saturating_sub(1);
            }
        }
        KeyCode::Char(c) => {
            if let Some(Modal::SeriesSearch(p)) = &mut app.modal {
                p.buffer.push(c);
                p.selected = 0;
            }
        }
        _ => {}
    }
    Ok(())
}

fn series_search_matches(prompt: &SeriesSearch) -> Vec<&Series> {
    let needle = prompt.buffer.trim().to_lowercase();
    prompt
        .all_series
        .iter()
        .filter(|series| prompt.block.matches_series(series))
        .filter(|series| needle.is_empty() || series.label.to_lowercase().contains(&needle))
        .collect()
}

fn submit_series_search(app: &mut App) -> Result<()> {
    let Some(Modal::SeriesSearch(prompt)) = app.modal.take() else {
        return Ok(());
    };

    let matches = series_search_matches(&prompt);
    let selected = matches
        .get(prompt.selected.min(matches.len().saturating_sub(1)))
        .map(|series| (series.id.clone(), series.label.clone()));

    match selected {
        Some((series_id, label)) => {
            app.open_text_replace_on_type(
                format!("Amount for {}", label),
                amount_edit_string(Money::ZERO),
                PromptKind::SeriesAddAmount {
                    destination: prompt.destination,
                    block: prompt.block,
                    selection: SeriesSelection::Existing { series_id, label },
                },
            );
        }
        None => {
            let label = prompt.buffer.trim().to_string();
            if label.is_empty() {
                app.status = Some(format!("Type a {} name", prompt.block.noun()));
            } else {
                app.open_text_replace_on_type(
                    format!("Amount for new {}", prompt.block.noun()),
                    amount_edit_string(Money::ZERO),
                    PromptKind::SeriesAddAmount {
                        destination: prompt.destination,
                        block: prompt.block,
                        selection: SeriesSelection::New { label },
                    },
                );
            }
        }
    }
    Ok(())
}

impl SeriesSelection {
    fn label(&self) -> &str {
        match self {
            SeriesSelection::Existing { label, .. } | SeriesSelection::New { label } => label,
        }
    }
}

fn add_selected_series(
    app: &mut App,
    destination: AddDestination,
    block: BudgetBlock,
    selection: SeriesSelection,
    amount: Money,
) -> Result<()> {
    let label = selection.label().to_string();
    let series_id = match selection {
        SeriesSelection::Existing { series_id, .. } => series_id,
        SeriesSelection::New { label } => ops::create_series(
            &app.conn,
            block.kind(),
            &label,
            block.direction(),
            (block == BudgetBlock::Envelopes).then_some(PeriodType::Monthly),
            None,
        )?,
    };

    match destination {
        AddDestination::Plan { plan_id } => {
            let id = ops::add_plan_item(&app.conn, &plan_id, &series_id, amount)?;
            app.pending_select = Some(id);
            app.status = Some(format!("Added “{label}”"));
        }
        AddDestination::Month { month_id } => {
            match block {
                BudgetBlock::Income | BudgetBlock::Expenses => {
                    let id =
                        ops::add_series_txn_instance(&app.conn, &month_id, &series_id, amount)?;
                    app.pending_dash_txn = Some(id);
                }
                BudgetBlock::Envelopes => {
                    let id = ops::add_series_envelope_instance(
                        &app.conn, &month_id, &series_id, amount,
                    )?;
                    app.pending_dash_env = Some(id);
                }
            }
            app.status = Some(format!("Added “{label}”"));
        }
    }
    Ok(())
}

/// Apply a submitted text prompt. We `take()` the modal first so it closes exactly once,
/// then act on its kind. Validation failures re-report via `status` instead of mutating.
fn submit_text(app: &mut App) -> Result<()> {
    let Some(Modal::Text(prompt)) = app.modal.take() else {
        return Ok(());
    };
    let text = prompt.buffer.trim().to_string();

    match prompt.kind {
        PromptKind::NewPlan => {
            if text.is_empty() {
                app.status = Some("Plan name can't be empty".into());
                return Ok(());
            }
            let id = ops::create_plan(&app.conn, &text)?;
            reset_editor_selections(app);
            app.screen = Screen::PlanEditor { plan_id: id };
            app.status = Some(format!("Created plan “{text}”"));
        }
        PromptKind::RenamePlan { id } => {
            if !text.is_empty() {
                ops::rename_plan(&app.conn, &id, &text)?;
            }
        }
        PromptKind::SeriesLabel { series_id } => {
            if !text.is_empty() {
                ops::set_series_label(&app.conn, &series_id, &text)?;
            }
        }
        PromptKind::SeriesCategory { series_id } => {
            if text.is_empty() {
                app.status = Some("Category can't be empty".into());
            } else {
                ops::set_series_category(&app.conn, &series_id, Some(&text))?;
            }
        }
        PromptKind::NewSeries { block } => {
            if text.is_empty() {
                app.status = Some(format!("{} name can't be empty", block.noun()));
            } else {
                // Same construction the plan/dashboard add path uses (see add_selected_series):
                // block fixes kind + direction, envelopes seed monthly + default mode.
                let id = ops::create_series(
                    &app.conn,
                    block.kind(),
                    &text,
                    block.direction(),
                    (block == BudgetBlock::Envelopes).then_some(PeriodType::Monthly),
                    None,
                )?;
                app.pending_series_select = Some(id);
                app.status = Some(format!("Created “{text}”"));
            }
        }
        PromptKind::SeriesAddAmount {
            destination,
            block,
            selection,
        } => match Money::parse_dollars(&text) {
            Some(amount) => add_selected_series(app, destination, block, selection, amount)?,
            None => {
                app.status = Some(format!("Couldn't read “{text}” as an amount"));
                app.open_text(
                    format!("Amount for {}", selection.label()),
                    text,
                    PromptKind::SeriesAddAmount {
                        destination,
                        block,
                        selection,
                    },
                );
            }
        },
        PromptKind::ItemAmount { id } => match Money::parse_dollars(&text) {
            Some(amount) => ops::set_item_amount(&app.conn, &id, amount)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::StampMonth { plan_id } => stamp_from_input(app, &plan_id, &text)?,
        PromptKind::GoToMonth => match parse_year_month(&text) {
            Some((year, month)) => {
                app.viewed_year = year;
                app.viewed_month = month;
                // New period → old row indices are meaningless; start its lists at the top.
                reset_dashboard_selections(app);
            }
            None => app.status = Some(format!("Enter a month as YYYY-MM (got “{text}”)")),
        },
        PromptKind::AccountBalance { id } => match Money::parse_dollars(&text) {
            Some(balance) => ops::set_balance(&app.conn, &id, balance)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::CardAvailable { id } => match Money::parse_dollars(&text) {
            Some(available) => ops::set_available_credit(&app.conn, &id, available)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::CardLimit { id } => match Money::parse_dollars(&text) {
            Some(limit) => ops::set_credit_limit(&app.conn, &id, limit)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::NewAccountName { account_type } => {
            if text.is_empty() {
                app.status = Some("Account name can't be empty".into());
                return Ok(());
            }
            match account_type {
                AccountType::Checking => app.open_text_replace_on_type(
                    format!("Starting balance for {text}"),
                    amount_edit_string(Money::ZERO),
                    PromptKind::NewCheckingBalance { name: text },
                ),
                AccountType::CreditCard => app.open_text_replace_on_type(
                    format!("Credit limit for {text}"),
                    amount_edit_string(Money::ZERO),
                    PromptKind::NewCardLimit { name: text },
                ),
            }
        }
        PromptKind::NewCheckingBalance { name } => match Money::parse_dollars(&text) {
            Some(balance) => {
                let id = ops::create_checking_account(&app.conn, &name, balance)?;
                app.pending_dash_account = Some(id);
                app.status = Some(format!("Created account “{name}”"));
            }
            None => {
                app.status = Some(format!("Couldn't read “{text}” as an amount"));
                app.open_text(
                    format!("Starting balance for {name}"),
                    text,
                    PromptKind::NewCheckingBalance { name },
                );
            }
        },
        PromptKind::NewCardLimit { name } => match Money::parse_dollars(&text) {
            Some(limit) => app.open_text_replace_on_type(
                format!("Available credit for {name}"),
                amount_edit_string(limit),
                PromptKind::NewCardAvailable { name, limit },
            ),
            None => {
                app.status = Some(format!("Couldn't read “{text}” as an amount"));
                app.open_text(
                    format!("Credit limit for {name}"),
                    text,
                    PromptKind::NewCardLimit { name },
                );
            }
        },
        PromptKind::NewCardAvailable { name, limit } => match Money::parse_dollars(&text) {
            Some(available) => {
                let id = ops::create_credit_card_account(&app.conn, &name, limit, available)?;
                app.pending_dash_account = Some(id);
                app.status = Some(format!("Created account “{name}”"));
            }
            None => {
                app.status = Some(format!("Couldn't read “{text}” as an amount"));
                app.open_text(
                    format!("Available credit for {name}"),
                    text,
                    PromptKind::NewCardAvailable { name, limit },
                );
            }
        },
        PromptKind::AccountName { id } => {
            if !text.is_empty() {
                ops::rename_account(&app.conn, &id, &text)?;
            } else {
                app.status = Some("Account name can't be empty".into());
            }
        }
        PromptKind::AccountCarry { id } => match Money::parse_dollars(&text) {
            Some(carry) => ops::set_account_carry_balance(&app.conn, &id, carry)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::TxnLabel { id } => {
            if !text.is_empty() {
                ops::set_txn_label(&app.conn, &id, &text)?;
            }
        }
        PromptKind::TxnAmount { id } => match Money::parse_dollars(&text) {
            Some(amount) => ops::set_txn_amount(&app.conn, &id, amount)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::EnvelopeLabel { id } => {
            if !text.is_empty() {
                ops::set_envelope_label(&app.conn, &id, &text)?;
            }
        }
        PromptKind::EnvelopeAmount { id } => match Money::parse_dollars(&text) {
            Some(amount) => ops::set_envelope_amount(&app.conn, &id, amount)?,
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
        PromptKind::EnvelopeSpend {
            envelope_id,
            month_id,
        } => match Money::parse_dollars(&text) {
            Some(amount) => {
                ops::add_envelope_spending(&app.conn, &month_id, &envelope_id, "Spending", amount)?;
                app.status = Some(format!("Filed {amount} of spending"));
            }
            None => app.status = Some(format!("Couldn't read “{text}” as an amount")),
        },
    }
    Ok(())
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            if let Some(Modal::Confirm(confirm)) = app.modal.take() {
                match confirm.action {
                    ConfirmAction::DeletePlan { id } => {
                        ops::delete_plan(&mut app.conn, &id)?;
                        app.status = Some("Plan deleted".into());
                    }
                    ConfirmAction::DeleteItem { id } => {
                        ops::delete_plan_item(&app.conn, &id)?;
                        app.status = Some("Item deleted".into());
                    }
                    ConfirmAction::DeleteSeries { series_id } => {
                        // The UI pre-checks plan usage, but `delete_series` re-checks and
                        // errors if a plan snuck in; surface that as a status instead of
                        // crashing the loop.
                        match ops::delete_series(&app.conn, &series_id) {
                            Ok(()) => app.status = Some("Series deleted".into()),
                            Err(e) => app.status = Some(e.to_string()),
                        }
                    }
                    ConfirmAction::DeleteTxn { id } => {
                        ops::delete_txn(&app.conn, &id)?;
                        app.status = Some("Deleted".into());
                    }
                    ConfirmAction::DeleteEnvelope { id } => {
                        ops::delete_envelope(&mut app.conn, &id)?;
                        app.status = Some("Deleted".into());
                    }
                    ConfirmAction::DeleteAccount { id } => {
                        if ops::delete_account(&app.conn, &id)? {
                            app.status = Some("Account deleted".into());
                        } else {
                            app.status = Some("Account is used by transactions".into());
                        }
                    }
                }
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.modal = None,
        _ => {}
    }
    Ok(())
}

/// Parse a `YYYY-MM` month, validate it isn't already stamped, and stamp the plan onto it.
fn stamp_from_input(app: &mut App, plan_id: &str, input: &str) -> Result<()> {
    let Some((year, month)) = parse_year_month(input) else {
        app.status = Some(format!("Enter a month as YYYY-MM (got “{input}”)"));
        return Ok(());
    };
    let label = format!("{year:04}-{month:02}");

    // Land the dashboard on the month we're about to stamp (or restamp), so the result is
    // visible the moment we switch back to it — even when it's a future or past period.
    app.viewed_year = year;
    app.viewed_month = month;

    // Already stamped? Offer Merge / Replace instead of a fresh stamp.
    if let Some(month_id) = queries::month_id_for_label(&app.conn, &label)? {
        app.open_choice(
            format!("{label} is already stamped. Restamp how?"),
            vec![
                ChoiceOption {
                    key: 'm',
                    label: "Merge (additive; refresh planned)".into(),
                    action: Some(ModalAction::RestampMerge {
                        month_id: month_id.clone(),
                        plan_id: plan_id.to_string(),
                    }),
                },
                ChoiceOption {
                    key: 'r',
                    label: "Replace (clean slate)".into(),
                    action: Some(ModalAction::RestampReplace {
                        month_id,
                        plan_id: plan_id.to_string(),
                    }),
                },
                ChoiceOption {
                    key: 'c',
                    label: "Cancel".into(),
                    action: None,
                },
            ],
        );
        return Ok(());
    }

    let start = NaiveDate::from_ymd_opt(year, month, 1).expect("validated y-m");
    let days = ops::days_in_month(year, month);
    ops::stamp(&mut app.conn, plan_id, &label, start, days)?;
    reset_dashboard_selections(app);
    app.screen = Screen::Dashboard;
    app.status = Some(format!("Stamped {label}"));
    Ok(())
}

fn parse_year_month(input: &str) -> Option<(i32, u32)> {
    let (y, m) = input.trim().split_once('-')?;
    let year: i32 = y.parse().ok()?;
    let month: u32 = m.parse().ok()?;
    if (1..=12).contains(&month) {
        Some((year, month))
    } else {
        None
    }
}

/// The default month to suggest when stamping: the month after the latest stamped one,
/// or the current calendar month on a fresh database.
pub(crate) fn suggested_stamp_label(conn: &Connection, today: NaiveDate) -> Result<String> {
    let (base_year, base_month) = match queries::current_month(conn)? {
        Some(m) => {
            // Advance one month past the latest.
            if m.start_date.month() == 12 {
                (m.start_date.year() + 1, 1)
            } else {
                (m.start_date.year(), m.start_date.month() + 1)
            }
        }
        None => (today.year(), today.month()),
    };
    Ok(format!("{base_year:04}-{base_month:02}"))
}

// --- Modal rendering -----------------------------------------------------------

/// Draw the open modal (if any) as a centered popup over the current screen.
fn draw_modal(frame: &mut Frame, app: &App) {
    let Some(modal) = &app.modal else { return };

    match modal {
        Modal::Text(prompt) => {
            let height = if prompt.help.is_empty() { 20 } else { 34 };
            let area = centered_rect(60, height, frame.area());
            frame.render_widget(Clear, area); // erase whatever's underneath so the box is opaque
            let block = titled_block(format!(" {} ", prompt.title));
            let mut input = vec![Span::raw(" > ")];
            if prompt.replace_on_next_char && !prompt.buffer.is_empty() {
                input.push(Span::styled(
                    prompt.buffer.clone(),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ));
            } else {
                input.push(Span::raw(&prompt.buffer));
            }
            input.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
            let mut body = vec![Line::raw(""), Line::from(input), Line::raw("")];
            if !prompt.help.is_empty() {
                for line in &prompt.help {
                    body.push(Line::from(Span::styled(
                        format!(" {line}"),
                        Style::default().fg(Color::Gray),
                    )));
                }
                body.push(Line::raw(""));
            }
            body.push(Line::from(Span::styled(
                " Enter to confirm · Esc to cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(body).block(block), area);
        }
        Modal::SeriesSearch(prompt) => draw_series_search_modal(frame, prompt),
        Modal::Confirm(confirm) => {
            let area = centered_rect(60, 20, frame.area());
            frame.render_widget(Clear, area);
            let block = titled_block(" Confirm ").border_style(Style::default().fg(Color::Red));
            let body = vec![
                Line::raw(""),
                Line::from(Span::raw(format!(" {}", confirm.title))),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(" [y] ", Style::default().fg(Color::Black).bg(Color::Red)),
                    Span::raw(" yes    "),
                    Span::styled(" [n] ", Style::default().fg(Color::Black).bg(Color::Gray)),
                    Span::raw(" no"),
                ]),
            ];
            frame.render_widget(Paragraph::new(body).block(block), area);
        }
        Modal::Choice(choice) => {
            let area = centered_rect(60, 20, frame.area());
            frame.render_widget(Clear, area);
            let block = titled_block(" Choose ").border_style(Style::default().fg(Color::Cyan));
            let mut body = vec![
                Line::raw(""),
                Line::from(Span::raw(format!(" {}", choice.title))),
                Line::raw(""),
            ];
            for opt in &choice.options {
                body.push(Line::from(vec![
                    Span::styled(
                        format!(" [{}] ", opt.key),
                        Style::default().fg(Color::Black).bg(Color::Gray),
                    ),
                    Span::raw(format!(" {}", opt.label)),
                ]));
            }
            frame.render_widget(Paragraph::new(body).block(block), area);
        }
    }
}

fn draw_series_search_modal(frame: &mut Frame, prompt: &SeriesSearch) {
    let area = centered_rect(70, 42, frame.area());
    frame.render_widget(Clear, area);

    let block =
        titled_block(format!(" {} ", prompt.title)).border_style(Style::default().fg(Color::Cyan));

    let mut body = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw(" > "),
            Span::raw(&prompt.buffer),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]),
        Line::raw(""),
    ];

    let matches = series_search_matches(prompt);
    if matches.is_empty() {
        let label = prompt.buffer.trim();
        if label.is_empty() {
            body.push(Line::from(Span::styled(
                format!(" Type a {} name", prompt.block.noun()),
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            body.push(Line::from(vec![
                Span::styled(" Enter ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(format!(
                    " Create new {} named \"{}\"",
                    prompt.block.noun(),
                    truncate(label, 34)
                )),
            ]));
        }
    } else {
        let selected_idx = prompt.selected.min(matches.len().saturating_sub(1));
        let start = selected_idx.saturating_sub(5);
        for (idx, series) in matches.iter().enumerate().skip(start).take(6) {
            let is_selected = idx == selected_idx;
            let marker = if is_selected { "▌" } else { " " };
            let style = if is_selected {
                Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
            } else {
                Style::default()
            };
            body.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan)),
                Span::styled(format!(" {:<36}", truncate(&series.label, 36)), style),
            ]));
        }
        if start > 0 {
            body.insert(
                3,
                Line::from(Span::styled(
                    format!("   {} more above", start),
                    Style::default().fg(Color::DarkGray),
                )),
            );
        }
        let remaining = matches.len().saturating_sub(start + 6);
        if remaining > 0 {
            body.push(Line::from(Span::styled(
                format!("   {remaining} more"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    body.push(Line::raw(""));
    body.push(Line::from(Span::styled(
        " Enter to select/create · ↑/↓ to choose · Esc to cancel",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(body).block(block), area);
}

/// Compute a centered rectangle `percent_x` × `percent_y` of `area`. `Flex::Center` does
/// the centering along each axis.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [h] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(h);
    v
}

// --- Shared display helpers (used by more than one screen) ---------------------

/// A footer with context hints on the left and global navigation on the right. Status
/// messages take the right side while present.
pub(crate) fn draw_split_status_footer(
    frame: &mut Frame,
    area: Rect,
    left_hints: Line,
    right_hints: Line,
    status: &Option<String>,
) {
    let right_line = match status {
        Some(s) => Line::from(Span::styled(
            format!("{s} "),
            Style::default().fg(Color::Yellow),
        )),
        None => right_hints,
    };
    let right_width = (right_line.width() as u16).min(area.width);
    let left_width = area.width.saturating_sub(right_width);

    if left_width > 0 {
        let left_area = Rect {
            width: left_width,
            ..area
        };
        frame.render_widget(Paragraph::new(left_hints), left_area);
    }

    if right_width > 0 {
        let right_area = Rect {
            x: area.x + left_width,
            width: right_width,
            ..area
        };
        let p = Paragraph::new(right_line).alignment(Alignment::Right);
        frame.render_widget(p, right_area);
    }
}

/// Truncate a label to `max` chars with an ellipsis so columns don't overflow.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Format a `Money` as an editable plain number ("1500.00") to prefill the amount prompt.
pub(crate) fn amount_edit_string(m: Money) -> String {
    format!("{:.2}", m.cents() as f64 / 100.0)
}
