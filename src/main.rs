//! Leeway's terminal UI.
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

mod anim;
mod dashboard;
mod help;
mod plans;
mod series;
mod theme;

use anim::SummaryAnimations;
use anyhow::{Context, Result};
use leeway::models::{AccountType, Direction, Kind, Mode, PeriodType, Series, Txn};
use leeway::money::Money;
use leeway::view::SeriesTimeRange;
use leeway::{calc, db, ops, queries};
use chrono::{Datelike, Local, NaiveDate};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, HighlightSpacing, List, ListItem, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState,
};
use ratatui::{DefaultTerminal, Frame};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) const SERIES_RENAME_GUIDANCE: &str = "Rename this item from its Series page — press S";

/// Which screen is showing. `PlanEditor` carries the id of the plan being edited —
/// that's how the loop knows which plan's items to load. Series carries its own compact
/// navigation state so a contextual detail drill-in can return to its exact origin.
pub enum Screen {
    Dashboard,
    EnvelopeDetail { detail: EnvelopeDetail },
    Series { state: SeriesScreen },
    Plans,
    PlanEditor { plan_id: String },
}

#[derive(Clone)]
pub struct SeriesScreen {
    pub mode: SeriesMode,
    pub origin: SeriesOrigin,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SeriesMode {
    Detail { series_id: String },
    List,
}

/// A deliberately shallow return address for the Series workflow. This is not intended to
/// grow into navigation history; if another workflow needs nested back-stack behavior, that
/// should become a shared navigation abstraction instead.
#[derive(Clone)]
pub enum SeriesOrigin {
    Dashboard,
    EnvelopeDetail { detail: EnvelopeDetail },
    Plans,
    PlanEditor { plan_id: String },
}

impl SeriesOrigin {
    fn from_screen(screen: &Screen) -> Option<Self> {
        match screen {
            Screen::Dashboard => Some(Self::Dashboard),
            Screen::EnvelopeDetail { detail } => Some(Self::EnvelopeDetail {
                detail: detail.clone(),
            }),
            Screen::Plans => Some(Self::Plans),
            Screen::PlanEditor { plan_id } => Some(Self::PlanEditor {
                plan_id: plan_id.clone(),
            }),
            Screen::Series { .. } => None,
        }
    }

    fn into_screen(self) -> Screen {
        match self {
            Self::Dashboard => Screen::Dashboard,
            Self::EnvelopeDetail { detail } => Screen::EnvelopeDetail { detail },
            Self::Plans => Screen::Plans,
            Self::PlanEditor { plan_id } => Screen::PlanEditor { plan_id },
        }
    }
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
    Plans,
    AdHoc,
    Both,
}

impl SeriesFilter {
    /// Cycle order for the `f` key: Plans → Ad-hoc → Both → Plans. Plans leads because it's the
    /// common case, and seeing the two exclusive halves first gives "Both" (their union) context.
    pub fn next(self) -> Self {
        match self {
            SeriesFilter::Plans => SeriesFilter::AdHoc,
            SeriesFilter::AdHoc => SeriesFilter::Both,
            SeriesFilter::Both => SeriesFilter::Plans,
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
    /// Contextual help for the focused box. Opened with `h` on any screen; see `help`.
    Help(help::HelpState),
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
        block.border_style(Style::default().fg(theme::MAUVE))
    } else {
        block
    }
}

pub(crate) fn selectable_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    focusable_block(title, focused).padding(Padding::new(0, LIST_RIGHT_PADDING, 0, 0))
}

/// Subtle background band marking the selected row. We use a quiet tint rather
/// than inverse video (`Modifier::REVERSED`): inverse video swaps every cell's
/// fg/bg, which inverts colored content like the envelope meters and reads as
/// harsh. Paired with the `▌` highlight symbol for a clear-but-quiet cue.
const SELECTION_BG: Color = theme::SELECTION;

pub(crate) fn selection_style() -> Style {
    Style::default().bg(SELECTION_BG)
}

pub(crate) fn selectable_list<'a>(items: Vec<ListItem<'a>>) -> List<'a> {
    List::new(items)
        .highlight_style(selection_style())
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

/// Draw a vertical scrollbar over a bordered list block's right edge, but only when
/// the content actually overflows the viewport — so panels that fit stay clean. Call
/// this *after* rendering the list, passing the post-render `ListState::offset()` (the
/// index of the top visible row).
///
/// This behaves like a web viewport scrollbar: the thumb tracks the *scroll window*,
/// not the cursor, so it stays put while you move the selection among already-visible
/// rows and only travels when the list actually scrolls. Getting that out of ratatui's
/// `Scrollbar` takes a bit of input massaging. It sizes/places the thumb against
/// `M = content_length - 1 + viewport_content_length`. Feeding it the raw item count as
/// `content_length` makes `M` overshoot, so the thumb is undersized and never reaches
/// the bottom (the offset tops out at `item_count - viewport`, well short of `M`).
/// Setting `content_length = item_count - viewport + 1` collapses `M` to exactly
/// `item_count`, which yields a thumb sized to the visible fraction (`viewport /
/// item_count`) and positioned at `offset / item_count` — anchored to the bottom at the
/// maximum offset.
///
/// `area` is the full block area (border included). The scrollbar occupies the
/// rightmost column between the top and bottom borders, clear of the list's content
/// which sits inside the block padding.
pub(crate) fn render_list_scrollbar(
    frame: &mut Frame,
    area: Rect,
    item_count: usize,
    offset: usize,
    focused: bool,
) {
    // Inner rows available for content = block height minus the top/bottom borders.
    let viewport = area.height.saturating_sub(2) as usize;
    if item_count <= viewport {
        return;
    }
    // Number of distinct scroll positions; see the doc comment for why this, not the
    // raw item count, is the `content_length` we hand ratatui.
    let scroll_stops = item_count - viewport + 1;
    let mut state = ScrollbarState::new(scroll_stops)
        .viewport_content_length(viewport)
        .position(offset);
    let thumb = if focused { theme::MAUVE } else { Color::Gray };
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_style(Style::default().fg(thumb))
        .track_style(Style::default().fg(theme::METER_TRACK));
    // Vertical margin of 1 keeps the track between the borders; horizontal 0 leaves
    // it in the block's rightmost column (over the vertical border segment).
    frame.render_stateful_widget(
        scrollbar,
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvelopeDetailFocus {
    Details,
    Transactions,
}

impl EnvelopeDetailFocus {
    fn other(self) -> Self {
        match self {
            Self::Details => Self::Transactions,
            Self::Transactions => Self::Details,
        }
    }
}

#[derive(Clone)]
pub struct EnvelopeDetail {
    pub month_id: String,
    pub envelope_id: String,
    pub focus: EnvelopeDetailFocus,
    pub selected_spend: usize,
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
    pub return_to_envelope_detail: Option<EnvelopeDetail>,
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
    /// Edit a seriesless month transaction or an individual envelope-spending label.
    TxnLabel {
        id: String,
    },
    /// Edit a month transaction's amount.
    TxnAmount {
        id: String,
    },
    /// Edit a legacy seriesless month envelope's label.
    EnvelopeLabel {
        id: String,
    },
    /// Edit a month envelope's monthly amount.
    EnvelopeAmount {
        id: String,
        period_type: PeriodType,
        days_in_month: i64,
    },
    /// Record an envelope transaction: first collect the label, then the amount. Carries the
    /// month so the new transaction lands in the right period, regardless of envelope mode.
    EnvelopeSpendLabel {
        envelope_id: String,
        month_id: String,
    },
    EnvelopeSpendAmount {
        envelope_id: String,
        month_id: String,
        label: String,
    },
}

pub enum SeriesSelection {
    Existing {
        series_id: String,
        label: String,
        period_type: Option<PeriodType>,
    },
    New {
        label: String,
    },
}

pub struct Confirm {
    pub title: String,
    pub action: ConfirmAction,
    pub return_to_envelope_detail: Option<EnvelopeDetail>,
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
    pub summary_anims: SummaryAnimations,
    pub frame_now: Instant,
    pub modal: Option<Modal>,
    /// A transient one-liner (errors, confirmations) shown in the footer.
    pub status: Option<String>,
}

impl App {
    fn return_from_series(&mut self) {
        let origin = match &self.screen {
            Screen::Series { state } => Some(state.origin.clone()),
            _ => None,
        };
        if let Some(origin) = origin {
            self.screen = origin.into_screen();
            self.series_search_active = false;
        }
    }

    fn open_text_prompt(
        &mut self,
        title: impl Into<String>,
        buffer: impl Into<String>,
        help: Vec<String>,
        replace_on_next_char: bool,
        kind: PromptKind,
        return_to_envelope_detail: Option<EnvelopeDetail>,
    ) {
        self.modal = Some(Modal::Text(TextPrompt {
            title: title.into(),
            buffer: buffer.into(),
            help,
            replace_on_next_char,
            kind,
            return_to_envelope_detail,
        }));
    }

    fn open_text(&mut self, title: impl Into<String>, buffer: impl Into<String>, kind: PromptKind) {
        self.open_text_prompt(title, buffer, Vec::new(), false, kind, None);
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
        self.open_text_prompt(title, buffer, help, true, kind, None);
    }

    fn open_text_replace_on_type(
        &mut self,
        title: impl Into<String>,
        buffer: impl Into<String>,
        kind: PromptKind,
    ) {
        self.open_text_prompt(title, buffer, Vec::new(), true, kind, None);
    }

    fn open_text_from_envelope_detail(
        &mut self,
        title: impl Into<String>,
        buffer: impl Into<String>,
        kind: PromptKind,
        detail: EnvelopeDetail,
        replace_on_next_char: bool,
    ) {
        self.open_text_prompt(
            title,
            buffer,
            Vec::new(),
            replace_on_next_char,
            kind,
            Some(detail),
        );
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
            return_to_envelope_detail: None,
        }));
    }

    fn open_confirm_from_envelope_detail(
        &mut self,
        title: impl Into<String>,
        action: ConfirmAction,
        detail: EnvelopeDetail,
    ) {
        self.modal = Some(Modal::Confirm(Confirm {
            title: title.into(),
            action,
            return_to_envelope_detail: Some(detail),
        }));
    }

    fn open_choice(&mut self, title: impl Into<String>, options: Vec<ChoiceOption>) {
        self.modal = Some(Modal::Choice(Choice {
            title: title.into(),
            options,
        }));
    }

    /// Open contextual help for whatever box is currently focused. Resolves the
    /// topic and sibling ring from the current screen and focus (see `help`).
    fn open_help(&mut self) {
        let state = help::HelpState::new(self);
        self.modal = Some(Modal::Help(state));
    }
}

fn main() -> Result<()> {
    let path = PathBuf::from("leeway.db");
    let mut conn = db::open(&path)?;
    // On a fresh database this stamps the current calendar month, satisfying "if no month
    // exists, create one" for a first-ever launch.
    ops::seed_starter(&mut conn)?;

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
        // Land on the common case: series that belong to a plan. `f` cycles to Ad-hoc, then Both.
        series_filter: SeriesFilter::Plans,
        plan_focus: PlanFocus::Income,
        editor_income_sel: 0,
        editor_expense_sel: 0,
        editor_env_sel: 0,
        pending_select: None,
        pending_dash_txn: None,
        pending_dash_env: None,
        pending_dash_account: None,
        pending_series_select: None,
        summary_anims: SummaryAnimations::new(),
        frame_now: Instant::now(),
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
                let view = leeway::view::MonthView::build_for(
                    &app.conn,
                    today,
                    app.viewed_year,
                    app.viewed_month,
                )?;
                match &view {
                    Some(v) => {
                        // Resolve "select the item I just created" now that the sorted lists
                        // are loaded (mirrors the plan editor's pending_select handling).
                        if let Some(target) = app.pending_dash_txn.take()
                            && let Some(txn) = v.standalone.iter().find(|t| t.id == target)
                        {
                            app.dash_focus = match txn.direction {
                                leeway::models::Direction::In => DashFocus::Income,
                                leeway::models::Direction::Out => DashFocus::Expenses,
                            };
                            if let Some(idx) = dashboard_txn_index(v, &target, app.dash_focus) {
                                match app.dash_focus {
                                    DashFocus::Income => app.dash_income_sel = idx,
                                    DashFocus::Expenses => app.dash_expense_sel = idx,
                                    _ => {}
                                }
                            }
                        }
                        if let Some(target) = app.pending_dash_env.take()
                            && let Some(idx) =
                                v.envelopes.iter().position(|e| e.envelope.id == target)
                        {
                            app.dash_env_sel = idx;
                            app.dash_focus = DashFocus::Envelopes;
                        }
                        if let Some(target) = app.pending_dash_account.take()
                            && v.is_current
                            && let Some(idx) = v.accounts.iter().position(|a| a.id == target)
                        {
                            app.dash_acct_sel = idx;
                            app.dash_focus = DashFocus::Accounts;
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
                let now = Instant::now();
                app.frame_now = now;
                app.summary_anims.sync(
                    view.as_ref().map(|v| &v.whats_left),
                    view.as_ref().map(|v| v.is_current).unwrap_or(false),
                    (app.viewed_year, app.viewed_month),
                    now,
                );
                terminal.draw(|f| {
                    dashboard::draw(f, app, &view);
                    draw_modal(f, app);
                })?;
                let tick = if app.summary_anims.is_animating(now) {
                    FRAME_TICK
                } else {
                    IDLE_TICK
                };
                if let Some(key) = read_key(tick)? {
                    let contextual_series_id = view
                        .as_ref()
                        .and_then(|view| dashboard::selected_series_id(app, view));
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else if handle_global_key_with_series(app, key, contextual_series_id) {
                    } else {
                        dashboard::handle_key(app, key, &view)?;
                    }
                }
            }

            Screen::EnvelopeDetail { detail } => {
                let detail = detail.clone();
                // The envelope may have been deleted from a confirmation dialog. Return to the
                // dashboard rather than rendering a stale drill-in screen.
                let Some(envelope) = load_detail_envelope(app, &detail)? else {
                    app.screen = Screen::Dashboard;
                    app.status = Some("Envelope no longer exists".into());
                    continue;
                };

                terminal.draw(|f| {
                    draw_envelope_detail_screen(f, app, &detail);
                    draw_modal(f, app);
                })?;
                if let Some(key) = read_key(IDLE_TICK)? {
                    let contextual_series_id = envelope.series_id.clone();
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else if handle_global_key_with_series(app, key, contextual_series_id) {
                    } else {
                        handle_envelope_detail_key(app, key)?;
                    }
                }
            }

            Screen::Series { state } => {
                let state = state.clone();
                let view = leeway::view::SeriesPageView::build(&app.conn, today, app.series_range)?;
                match state.mode {
                    SeriesMode::Detail { series_id } => {
                        let Some(detail) = series::detail_by_id(&view, &series_id) else {
                            app.return_from_series();
                            app.status = Some("Series no longer exists".into());
                            continue;
                        };
                        terminal.draw(|f| {
                            series::draw_detail_screen(f, app, &view, detail);
                            draw_modal(f, app);
                        })?;
                        if let Some(key) = read_key(IDLE_TICK)? {
                            if app.modal.is_some() {
                                handle_modal_key(app, key)?;
                            } else if handle_global_key(app, key) {
                            } else {
                                series::handle_detail_key(app, key, detail, today)?;
                            }
                        }
                    }
                    SeriesMode::List => {
                        // Resolve creation and detail-to-list promotion now that the sorted,
                        // filtered list is loaded.
                        if let Some(target) = app.pending_series_select.take() {
                            series::reveal_series_by_id(app, &view, &target);
                        }
                        let visible_count = series::visible_count(app, &view);
                        clamp(&mut app.series_sel, visible_count);
                        terminal.draw(|f| {
                            series::draw(f, app, &view);
                            draw_modal(f, app);
                        })?;
                        if let Some(key) = read_key(IDLE_TICK)? {
                            if app.modal.is_some() {
                                handle_modal_key(app, key)?;
                            } else if app.series_search_active {
                                // Search is a text-entry mode: it must own every key (including
                                // would-be global jump/quit keys) so they remain literal input.
                                series::handle_key(app, key, &view, today)?;
                            } else if handle_global_key(app, key) {
                            } else {
                                series::handle_key(app, key, &view, today)?;
                            }
                        }
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
                if let Some(key) = read_key(IDLE_TICK)? {
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
                if let Some(target) = app.pending_select.take()
                    && let Some(entry) = entries.iter().find(|e| e.item_id == target)
                {
                    app.plan_focus = plan_focus_for_entry(entry);
                    if let Some(idx) = plan_entry_index(&entries, &target, app.plan_focus) {
                        match app.plan_focus {
                            PlanFocus::Income => app.editor_income_sel = idx,
                            PlanFocus::Expenses => app.editor_expense_sel = idx,
                            PlanFocus::Envelopes => app.editor_env_sel = idx,
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
                if let Some(key) = read_key(IDLE_TICK)? {
                    let contextual_series_id = plans::selected_series_id(app, &entries);
                    if app.modal.is_some() {
                        handle_modal_key(app, key)?;
                    } else if handle_global_key_with_series(app, key, contextual_series_id) {
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
const FRAME_TICK: Duration = Duration::from_millis(33);

/// Read one key *press*. Returns `None` for releases, resizes, mouse, or an idle timeout —
/// anything we don't act on (the next frame redraws regardless). `event::poll` returns as
/// soon as input arrives, so waiting up to the requested timeout never adds latency to real
/// keystrokes.
fn read_key(timeout: Duration) -> Result<Option<KeyEvent>> {
    if !event::poll(timeout)? {
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
    handle_global_key_with_series(app, key, None)
}

fn handle_global_key_with_series(
    app: &mut App,
    key: KeyEvent,
    contextual_series_id: Option<String>,
) -> bool {
    let screen = match key.code {
        KeyCode::Char('P') => Screen::Plans,
        KeyCode::Char('S') => {
            if let Screen::Series { state } = &mut app.screen {
                if let SeriesMode::Detail { series_id } = &state.mode {
                    app.pending_series_select = Some(series_id.clone());
                    state.mode = SeriesMode::List;
                }
                app.series_search_active = false;
                app.status = None;
                return true;
            }

            let Some(origin) = SeriesOrigin::from_screen(&app.screen) else {
                return true;
            };
            let mode = contextual_series_id
                .map(|series_id| SeriesMode::Detail { series_id })
                .unwrap_or(SeriesMode::List);
            Screen::Series {
                state: SeriesScreen { mode, origin },
            }
        }
        // Universal contextual help for the focused box. Runs after the modal/search
        // short-circuits, so `h` typed into a prompt stays literal (see the doc comment).
        KeyCode::Char('h') => {
            app.open_help();
            return true;
        }
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

fn dashboard_txn_count(view: &leeway::view::MonthView, focus: DashFocus) -> usize {
    view.standalone
        .iter()
        .filter(|txn| dashboard_txn_matches(txn, focus))
        .count()
}

fn dashboard_txn_index(
    view: &leeway::view::MonthView,
    txn_id: &str,
    focus: DashFocus,
) -> Option<usize> {
    view.standalone
        .iter()
        .filter(|txn| dashboard_txn_matches(txn, focus))
        .position(|txn| txn.id == txn_id)
}

fn dashboard_txn_matches(txn: &leeway::models::Txn, focus: DashFocus) -> bool {
    matches!(
        (focus, txn.direction),
        (DashFocus::Income, leeway::models::Direction::In)
            | (DashFocus::Expenses, leeway::models::Direction::Out)
    )
}

fn plan_entry_count(entries: &[leeway::models::PlanEntry], focus: PlanFocus) -> usize {
    entries
        .iter()
        .filter(|entry| plan_entry_matches(entry, focus))
        .count()
}

fn plan_entry_index(
    entries: &[leeway::models::PlanEntry],
    item_id: &str,
    focus: PlanFocus,
) -> Option<usize> {
    entries
        .iter()
        .filter(|entry| plan_entry_matches(entry, focus))
        .position(|entry| entry.item_id == item_id)
}

fn plan_focus_for_entry(entry: &leeway::models::PlanEntry) -> PlanFocus {
    match entry.series.kind {
        leeway::models::Kind::Envelope => PlanFocus::Envelopes,
        leeway::models::Kind::Transaction => match entry.series.direction {
            Some(leeway::models::Direction::In) => PlanFocus::Income,
            _ => PlanFocus::Expenses,
        },
    }
}

fn plan_entry_matches(entry: &leeway::models::PlanEntry, focus: PlanFocus) -> bool {
    match focus {
        PlanFocus::Income => {
            entry.series.kind == leeway::models::Kind::Transaction
                && entry.series.direction == Some(leeway::models::Direction::In)
        }
        PlanFocus::Expenses => {
            entry.series.kind == leeway::models::Kind::Transaction
                && entry.series.direction != Some(leeway::models::Direction::In)
        }
        PlanFocus::Envelopes => entry.series.kind == leeway::models::Kind::Envelope,
    }
}

// --- Modal input ---------------------------------------------------------------

fn handle_modal_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.modal.as_ref() {
        Some(Modal::Text(_)) => handle_text_key(app, key),
        Some(Modal::SeriesSearch(_)) => handle_series_search_key(app, key),
        Some(Modal::Confirm(_)) => handle_confirm_key(app, key),
        Some(Modal::Choice(_)) => handle_choice_key(app, key),
        Some(Modal::Help(_)) => handle_help_key(app, key),
        None => Ok(()),
    }
}

/// Keys while the help overlay is open. `h`/`Esc`/`q` close it; `Tab`/`o` navigate
/// topics; `j`/`k` and the arrows/pages scroll, clamped to the last-rendered extent.
fn handle_help_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let Some(Modal::Help(state)) = app.modal.as_mut() else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('q') => app.modal = None,
        KeyCode::Tab => state.cycle(true),
        KeyCode::BackTab => state.cycle(false),
        KeyCode::Char('o') => state.show_overview(),
        KeyCode::Char('j') | KeyCode::Down => {
            state.scroll = (state.scroll + 1).min(state.max_scroll.get());
        }
        KeyCode::Char('k') | KeyCode::Up => state.scroll = state.scroll.saturating_sub(1),
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_add(10).min(state.max_scroll.get());
        }
        KeyCode::PageUp => state.scroll = state.scroll.saturating_sub(10),
        _ => {}
    }
    Ok(())
}

fn handle_envelope_detail_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let Screen::EnvelopeDetail { detail } = &app.screen else {
        return Ok(());
    };
    let detail = detail.clone();
    let Some(envelope) = load_detail_envelope(app, &detail)? else {
        app.screen = Screen::Dashboard;
        app.status = Some("Envelope no longer exists".into());
        return Ok(());
    };
    let spending = queries::load_envelope_txns(&app.conn, &detail.month_id, &detail.envelope_id)?;

    match key.code {
        KeyCode::Esc => app.screen = Screen::Dashboard,
        KeyCode::Tab | KeyCode::BackTab => {
            if let Screen::EnvelopeDetail { detail } = &mut app.screen {
                detail.focus = detail.focus.other();
            }
        }
        _ => match detail.focus {
            EnvelopeDetailFocus::Details => match key.code {
                KeyCode::Char('l') => {
                    if envelope.series_id.is_some() {
                        app.status = Some(SERIES_RENAME_GUIDANCE.into());
                    } else {
                        app.open_text_from_envelope_detail(
                            "Envelope label",
                            envelope.label,
                            PromptKind::EnvelopeLabel { id: envelope.id },
                            detail.clone(),
                            true,
                        );
                    }
                }
                KeyCode::Char('a') => {
                    let days_in_month = queries::month_days(&app.conn, &detail.month_id)?;
                    let amount = calc::envelope_period_amount(
                        envelope.amount,
                        envelope.period_type,
                        days_in_month,
                    );
                    let title = match calc::active_period(envelope.period_type) {
                        PeriodType::Daily => {
                            format!("Daily amount for {}", envelope.display_label())
                        }
                        PeriodType::Monthly | PeriodType::Weekly => {
                            format!("Monthly amount for {}", envelope.display_label())
                        }
                    };
                    app.open_text_from_envelope_detail(
                        title,
                        amount_edit_string(amount),
                        PromptKind::EnvelopeAmount {
                            id: envelope.id,
                            period_type: envelope.period_type,
                            days_in_month,
                        },
                        detail.clone(),
                        true,
                    );
                }
                KeyCode::Char('m') => {
                    let next = match envelope.mode {
                        Mode::Manual => Mode::Automatic,
                        Mode::Automatic => Mode::Manual,
                    };
                    ops::set_envelope_mode(&app.conn, &envelope.id, next)?;
                }
                KeyCode::Char('p') => {
                    let next = match calc::active_period(envelope.period_type) {
                        PeriodType::Daily => PeriodType::Monthly,
                        PeriodType::Monthly | PeriodType::Weekly => PeriodType::Daily,
                    };
                    ops::set_envelope_period(&app.conn, &envelope.id, next)?;
                }
                KeyCode::Char('x') => {
                    app.open_confirm_from_envelope_detail(
                        format!(
                            "Delete envelope “{}” and its transactions?",
                            envelope.display_label()
                        ),
                        ConfirmAction::DeleteEnvelope { id: envelope.id },
                        detail.clone(),
                    );
                }
                _ => {}
            },
            EnvelopeDetailFocus::Transactions => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Screen::EnvelopeDetail { detail } = &mut app.screen
                        && detail.selected_spend + 1 < spending.len()
                    {
                        detail.selected_spend += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Screen::EnvelopeDetail { detail } = &mut app.screen {
                        detail.selected_spend = detail.selected_spend.saturating_sub(1);
                    }
                }
                KeyCode::Char('s') | KeyCode::Char('n') => {
                    app.open_text_from_envelope_detail(
                        format!("Transaction label for {}", envelope.display_label()),
                        String::new(),
                        PromptKind::EnvelopeSpendLabel {
                            envelope_id: envelope.id,
                            month_id: detail.month_id.clone(),
                        },
                        detail.clone(),
                        false,
                    );
                }
                KeyCode::Char('a') => {
                    if let Some(txn) = selected_spending(&spending, detail.selected_spend) {
                        app.open_text_from_envelope_detail(
                            format!("Amount for {}", txn.display_label()),
                            amount_edit_string(txn.amount),
                            PromptKind::TxnAmount { id: txn.id.clone() },
                            detail.clone(),
                            true,
                        );
                    }
                }
                KeyCode::Char('l') => {
                    if let Some(txn) = selected_spending(&spending, detail.selected_spend) {
                        app.open_text_from_envelope_detail(
                            "Transaction label",
                            txn.label.clone(),
                            PromptKind::TxnLabel { id: txn.id.clone() },
                            detail.clone(),
                            true,
                        );
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(txn) = selected_spending(&spending, detail.selected_spend) {
                        app.open_confirm_from_envelope_detail(
                            format!("Delete transaction “{}”?", txn.display_label()),
                            ConfirmAction::DeleteTxn { id: txn.id.clone() },
                            detail.clone(),
                        );
                    }
                }
                _ => {}
            },
        },
    }
    Ok(())
}

fn load_detail_envelope(
    app: &App,
    detail: &EnvelopeDetail,
) -> Result<Option<leeway::models::Envelope>> {
    Ok(queries::load_envelopes(&app.conn, &detail.month_id)?
        .into_iter()
        .find(|envelope| envelope.id == detail.envelope_id))
}

fn selected_spending(spending: &[Txn], selected: usize) -> Option<&Txn> {
    spending.get(selected.min(spending.len().saturating_sub(1)))
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

fn restore_envelope_detail_screen(app: &mut App, detail: Option<EnvelopeDetail>) {
    if let Some(detail) = detail {
        app.screen = Screen::EnvelopeDetail { detail };
    }
}

fn handle_text_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            let return_to = match app.modal.take() {
                Some(Modal::Text(prompt)) => prompt.return_to_envelope_detail,
                other => {
                    app.modal = other;
                    None
                }
            };
            restore_envelope_detail_screen(app, return_to);
        }
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
        .map(|series| (series.id.clone(), series.label.clone(), series.period_type));

    match selected {
        Some((series_id, label, period_type)) => {
            app.open_text_replace_on_type(
                series_amount_prompt_title(prompt.block, &label, period_type),
                amount_edit_string(Money::ZERO),
                PromptKind::SeriesAddAmount {
                    destination: prompt.destination,
                    block: prompt.block,
                    selection: SeriesSelection::Existing {
                        series_id,
                        label,
                        period_type,
                    },
                },
            );
        }
        None => {
            let label = prompt.buffer.trim().to_string();
            if label.is_empty() {
                app.status = Some(format!("Type a {} name", prompt.block.noun()));
            } else {
                app.open_text_replace_on_type(
                    series_amount_prompt_title(prompt.block, &label, None),
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

    fn period_type(&self) -> Option<PeriodType> {
        match self {
            SeriesSelection::Existing { period_type, .. } => *period_type,
            SeriesSelection::New { .. } => None,
        }
    }
}

fn series_amount_prompt_title(
    block: BudgetBlock,
    label: &str,
    period_type: Option<PeriodType>,
) -> String {
    if block == BudgetBlock::Envelopes {
        match period_type.map(calc::active_period) {
            Some(PeriodType::Daily) => format!("Daily amount for {label}"),
            _ => format!("Monthly amount for {label}"),
        }
    } else {
        format!("Amount for {label}")
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
    let return_to_envelope_detail = prompt.return_to_envelope_detail;

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
                app.open_text_prompt(
                    series_amount_prompt_title(block, selection.label(), selection.period_type()),
                    text,
                    Vec::new(),
                    false,
                    PromptKind::SeriesAddAmount {
                        destination,
                        block,
                        selection,
                    },
                    return_to_envelope_detail.clone(),
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
                app.open_text_prompt(
                    format!("Starting balance for {name}"),
                    text,
                    Vec::new(),
                    false,
                    PromptKind::NewCheckingBalance { name },
                    return_to_envelope_detail.clone(),
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
                app.open_text_prompt(
                    format!("Credit limit for {name}"),
                    text,
                    Vec::new(),
                    false,
                    PromptKind::NewCardLimit { name },
                    return_to_envelope_detail.clone(),
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
                app.open_text_prompt(
                    format!("Available credit for {name}"),
                    text,
                    Vec::new(),
                    false,
                    PromptKind::NewCardAvailable { name, limit },
                    return_to_envelope_detail.clone(),
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
        PromptKind::EnvelopeAmount {
            id,
            period_type,
            days_in_month,
        } => match Money::parse_dollars(&text) {
            Some(amount) => {
                let monthly_amount =
                    calc::monthlyized_envelope_amount(amount, period_type, days_in_month);
                ops::set_envelope_amount(&app.conn, &id, monthly_amount)?;
            }
            None => {
                app.status = Some(format!("Couldn't read “{text}” as an amount"));
                app.open_text_prompt(
                    "Envelope amount",
                    text,
                    Vec::new(),
                    false,
                    PromptKind::EnvelopeAmount {
                        id,
                        period_type,
                        days_in_month,
                    },
                    return_to_envelope_detail.clone(),
                );
            }
        },
        PromptKind::EnvelopeSpendLabel {
            envelope_id,
            month_id,
        } => {
            if text.is_empty() {
                app.status = Some("Spending label can't be empty".into());
                app.open_text_prompt(
                    "Spending label",
                    text,
                    Vec::new(),
                    false,
                    PromptKind::EnvelopeSpendLabel {
                        envelope_id,
                        month_id,
                    },
                    return_to_envelope_detail.clone(),
                );
            } else {
                app.open_text_prompt(
                    format!("Amount for {text}"),
                    String::new(),
                    Vec::new(),
                    false,
                    PromptKind::EnvelopeSpendAmount {
                        envelope_id,
                        month_id,
                        label: text,
                    },
                    return_to_envelope_detail.clone(),
                );
            }
        }
        PromptKind::EnvelopeSpendAmount {
            envelope_id,
            month_id,
            label,
        } => match Money::parse_dollars(&text) {
            Some(amount) => {
                ops::add_envelope_spending(&app.conn, &month_id, &envelope_id, &label, amount)?;
                app.status = Some(format!("Filed {amount} for {label}"));
            }
            None => {
                app.status = Some(format!("Couldn't read “{text}” as an amount"));
                app.open_text_prompt(
                    format!("Amount for {label}"),
                    text,
                    Vec::new(),
                    false,
                    PromptKind::EnvelopeSpendAmount {
                        envelope_id,
                        month_id,
                        label,
                    },
                    return_to_envelope_detail.clone(),
                );
            }
        },
    }
    if app.modal.is_none() {
        restore_envelope_detail_screen(app, return_to_envelope_detail);
    }
    Ok(())
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            if let Some(Modal::Confirm(confirm)) = app.modal.take() {
                let return_to = confirm.return_to_envelope_detail;
                let deletes_parent_envelope =
                    matches!(&confirm.action, ConfirmAction::DeleteEnvelope { .. });
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
                if deletes_parent_envelope && return_to.is_some() {
                    app.screen = Screen::Dashboard;
                } else if app.modal.is_none() {
                    restore_envelope_detail_screen(app, return_to);
                }
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            let return_to = match app.modal.take() {
                Some(Modal::Confirm(confirm)) => confirm.return_to_envelope_detail,
                other => {
                    app.modal = other;
                    None
                }
            };
            restore_envelope_detail_screen(app, return_to);
        }
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
fn draw_modal(frame: &mut Frame, _app: &App) {
    let Some(modal) = &_app.modal else { return };

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
                    Style::default().fg(Color::Black).bg(theme::CYAN),
                ));
            } else {
                input.push(Span::raw(&prompt.buffer));
            }
            input.push(Span::styled("▏", Style::default().fg(theme::CYAN)));
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
            let block = titled_block(" Choose ").border_style(Style::default().fg(theme::CYAN));
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
        Modal::Help(state) => draw_help_modal(frame, state),
    }
}

/// Draw the contextual help overlay: a large centered box over the current screen
/// with the focused box's help, scrollable, plus an in-modal nav footer.
fn draw_help_modal(frame: &mut Frame, state: &help::HelpState) {
    let content = help::content(state.current);
    let area = centered_rect(80, 80, frame.area());
    frame.render_widget(Clear, area); // opaque over whatever's behind it
    let block = titled_block(format!(" Help · {} ", content.title))
        .border_style(Style::default().fg(theme::MAUVE));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Body fills the box; a single-line footer sits at the bottom.
    let [body_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    // Lines are pre-wrapped to the body width, so their count is the exact content
    // height — clamp scrolling to what actually overflows and stash it for the key
    // handler (which has no frame dimensions of its own).
    let lines = help::lines(&content, body_area.width);
    let max_scroll = (lines.len() as u16).saturating_sub(body_area.height);
    state.max_scroll.set(max_scroll);
    let scroll = state.scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll, 0)),
        body_area,
    );

    // Footer: topic/scroll/close hints on the left, a "more below" cue on the right.
    let footer = Line::from(vec![
        modal_key(" h "),
        Span::styled(" close  ", Style::default().fg(Color::Gray)),
        modal_key(" Tab "),
        Span::styled(" topic  ", Style::default().fg(Color::Gray)),
        modal_key(" o "),
        Span::styled(" overview  ", Style::default().fg(Color::Gray)),
        modal_key(" j/k "),
        Span::styled(" scroll", Style::default().fg(Color::Gray)),
    ]);
    let more = if scroll < max_scroll {
        Line::from(Span::styled(
            "more ↓ ",
            Style::default().fg(theme::CYAN),
        ))
    } else {
        Line::raw("")
    };
    draw_split_status_footer(frame, footer_area, footer, more, &None);
}

fn draw_envelope_detail_screen(frame: &mut Frame, app: &App, detail: &EnvelopeDetail) {
    let area = frame.area();

    let (title, details, transactions) = match envelope_detail_content(app, detail) {
        Ok(content) => content,
        Err(err) => {
            let body = vec![
                Line::raw(""),
                Line::from(Span::styled(
                    format!(" Could not load envelope: {err}"),
                    Style::default().fg(Color::Red),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    " Esc to close",
                    Style::default().fg(Color::Gray),
                )),
            ];
            frame.render_widget(Paragraph::new(body).block(titled_block(" Envelope ")), area);
            return;
        }
    };
    let block = titled_block(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [details_area, transactions_area, footer_area] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(7),
        Constraint::Length(1),
    ])
    .areas(inner);
    draw_envelope_detail_section(
        frame,
        details_area,
        " Details ",
        details,
        detail.focus == EnvelopeDetailFocus::Details,
    );
    draw_envelope_detail_section(
        frame,
        transactions_area,
        " Transactions ",
        transactions,
        detail.focus == EnvelopeDetailFocus::Transactions,
    );
    let focused_commands = match detail.focus {
        EnvelopeDetailFocus::Details => details_footer_line(),
        EnvelopeDetailFocus::Transactions => transactions_footer_line(),
    };
    crate::draw_split_status_footer(
        frame,
        footer_area,
        focused_commands,
        envelope_detail_navigation_line(),
        &app.status,
    );
}

fn draw_envelope_detail_section(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    lines: Vec<Line<'static>>,
    focused: bool,
) {
    let block = focusable_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn details_footer_line() -> Line<'static> {
    Line::from(vec![
        modal_key(" a "),
        Span::raw(" amount  "),
        modal_key(" m "),
        Span::raw(" mode  "),
        modal_key(" p "),
        Span::raw(" period  "),
        modal_key(" x "),
        Span::raw(" delete"),
    ])
}

fn transactions_footer_line() -> Line<'static> {
    Line::from(vec![
        modal_key(" s/n "),
        Span::raw(" add  "),
        modal_key(" j/k "),
        Span::raw(" move  "),
        modal_key(" a "),
        Span::raw(" amount  "),
        modal_key(" l "),
        Span::raw(" label  "),
        modal_key(" x "),
        Span::raw(" delete"),
    ])
}

fn envelope_detail_navigation_line() -> Line<'static> {
    Line::from(vec![
        modal_key(" Tab "),
        Span::styled(" section  ", Style::default().fg(Color::Gray)),
        modal_key(" h "),
        Span::styled(" help  ", Style::default().fg(Color::Gray)),
        modal_key(" Esc "),
        Span::styled(" back  ", Style::default().fg(Color::Gray)),
        modal_key(" P "),
        Span::styled(" plans  ", Style::default().fg(Color::Gray)),
        modal_key(" S "),
        Span::styled(" series  ", Style::default().fg(Color::Gray)),
        modal_key(" q "),
        Span::styled(" quit", Style::default().fg(Color::Gray)),
    ])
}

fn modal_key(label: &'static str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::default().fg(Color::Black).bg(Color::Gray),
    )
}

fn envelope_detail_content(
    app: &App,
    detail: &EnvelopeDetail,
) -> Result<(String, Vec<Line<'static>>, Vec<Line<'static>>)> {
    let month = queries::month_by_id(&app.conn, &detail.month_id)?
        .with_context(|| format!("month not found: {}", detail.month_id))?;
    let envelope = load_detail_envelope(app, detail)?
        .with_context(|| format!("envelope not found: {}", detail.envelope_id))?;
    let transactions =
        queries::load_envelope_txns(&app.conn, &detail.month_id, &detail.envelope_id)?;
    let fraction = calc::elapsed_fraction(
        month.start_date,
        month.days_in_month,
        Local::now().date_naive(),
    );
    let consumed = calc::envelope_consumed(&envelope, envelope.mode, &transactions, fraction);
    let remaining = calc::envelope_remaining(&envelope, consumed);
    let period = calc::active_period(envelope.period_type);
    let period_label = match period {
        PeriodType::Daily => "daily",
        PeriodType::Monthly | PeriodType::Weekly => "monthly",
    };
    let mode_label = match envelope.mode {
        Mode::Automatic => "automatic",
        Mode::Manual => "manual",
    };
    let entered_amount = calc::envelope_period_amount(envelope.amount, period, month.days_in_month);
    let unit = match period {
        PeriodType::Daily => "/day",
        PeriodType::Monthly | PeriodType::Weekly => "/mo",
    };

    let details = vec![
        envelope_detail_metric_line(
            ("Mode", mode_label.to_string(), Color::White),
            ("Cadence", period_label.to_string(), Color::White),
            ("Entered", format!("{entered_amount}{unit}"), Color::White),
        ),
        envelope_detail_metric_line(
            ("Monthly", envelope.amount.to_string(), Color::White),
            ("Consumed", consumed.to_string(), Color::White),
            ("Remaining", remaining.to_string(), theme::CYAN),
        ),
    ];

    let mut transaction_lines = vec![Line::from(Span::styled(
        match envelope.mode {
            Mode::Automatic => {
                " Recorded transactions do not affect this automatic envelope's balance."
            }
            Mode::Manual => " Transactions determine this manual envelope's consumed amount.",
        },
        Style::default().fg(Color::Gray),
    ))];
    if transactions.is_empty() {
        transaction_lines.push(Line::raw(""));
        transaction_lines.push(Line::from(Span::styled(
            " No transactions recorded",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (idx, txn) in transactions.iter().enumerate() {
            let selected = detail.focus == EnvelopeDetailFocus::Transactions
                && idx
                    == detail
                        .selected_spend
                        .min(transactions.len().saturating_sub(1));
            let style = if selected {
                selection_style().fg(Color::White)
            } else {
                Style::default().fg(Color::White)
            };
            transaction_lines.push(Line::from(vec![
                Span::styled(
                    if selected { "▌" } else { " " },
                    Style::default().fg(theme::CYAN),
                ),
                Span::styled(
                    format!(
                        " {:<42} {:>14}",
                        truncate(txn.display_label(), 42),
                        txn.amount
                    ),
                    style,
                ),
            ]));
        }
    }

    Ok((
        format!(" Envelope · {} ", envelope.display_label()),
        details,
        transaction_lines,
    ))
}

fn envelope_detail_metric_line(
    first: (&str, String, Color),
    second: (&str, String, Color),
    third: (&str, String, Color),
) -> Line<'static> {
    let mut spans = Vec::new();
    for (label, value, color) in [first, second, third] {
        spans.push(Span::styled(
            format!(" {label:<10}"),
            Style::default().fg(Color::Gray),
        ));
        spans.push(Span::styled(
            format!("{value:<15}"),
            Style::default().fg(color),
        ));
    }
    Line::from(spans)
}

fn draw_series_search_modal(frame: &mut Frame, prompt: &SeriesSearch) {
    let area = centered_rect(70, 42, frame.area());
    frame.render_widget(Clear, area);

    let block =
        titled_block(format!(" {} ", prompt.title)).border_style(Style::default().fg(theme::CYAN));

    let mut body = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw(" > "),
            Span::raw(&prompt.buffer),
            Span::styled("▏", Style::default().fg(theme::CYAN)),
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
                selection_style()
            } else {
                Style::default()
            };
            body.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(theme::CYAN)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use uuid::Uuid;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_with_envelope_detail() -> (App, EnvelopeDetail, String) {
        let mut path = std::env::temp_dir();
        path.push(format!("leeway-envelope-detail-{}.db", Uuid::new_v4()));
        let mut conn = db::open(&path).unwrap();
        let plan_id = ops::create_plan(&conn, "Normal").unwrap();
        let dining_series = ops::create_series(
            &conn,
            Kind::Envelope,
            "Dining",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Manual),
        )
        .unwrap();
        ops::add_plan_item(&conn, &plan_id, &dining_series, Money::from_dollars(300.0)).unwrap();

        let month_id = ops::stamp(
            &mut conn,
            &plan_id,
            "2026-09",
            NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
            30,
        )
        .unwrap();
        let envelope = queries::load_envelopes(&conn, &month_id)
            .unwrap()
            .into_iter()
            .find(|envelope| envelope.label == "Dining")
            .unwrap();
        let detail = EnvelopeDetail {
            month_id,
            envelope_id: envelope.id.clone(),
            focus: EnvelopeDetailFocus::Details,
            selected_spend: 0,
        };

        let app = App {
            conn,
            screen: Screen::EnvelopeDetail {
                detail: detail.clone(),
            },
            should_quit: false,
            dash_focus: DashFocus::Envelopes,
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
            summary_anims: SummaryAnimations::new(),
            frame_now: Instant::now(),
            modal: None,
            status: None,
        };

        (app, detail, envelope.id)
    }

    fn assert_envelope_detail_screen(app: &App, expected: &EnvelopeDetail) {
        match &app.screen {
            Screen::EnvelopeDetail { detail: actual } => {
                assert_eq!(actual.month_id, expected.month_id);
                assert_eq!(actual.envelope_id, expected.envelope_id);
                assert_eq!(actual.focus, expected.focus);
                assert_eq!(actual.selected_spend, expected.selected_spend);
            }
            _ => panic!("expected envelope detail screen"),
        }
    }

    fn series_id_for_detail(app: &App, detail: &EnvelopeDetail) -> String {
        load_detail_envelope(app, detail)
            .unwrap()
            .unwrap()
            .series_id
            .unwrap()
    }

    #[test]
    fn contextual_s_opens_detail_and_remembers_its_origin() {
        let (mut app, envelope_detail, _) = app_with_envelope_detail();
        let series_id = series_id_for_detail(&app, &envelope_detail);

        assert!(handle_global_key_with_series(
            &mut app,
            key(KeyCode::Char('S')),
            Some(series_id.clone()),
        ));

        match &app.screen {
            Screen::Series { state } => {
                assert_eq!(
                    state.mode,
                    SeriesMode::Detail {
                        series_id: series_id.clone()
                    }
                );
                match &state.origin {
                    SeriesOrigin::EnvelopeDetail { detail } => {
                        assert_eq!(detail.envelope_id, envelope_detail.envelope_id)
                    }
                    _ => panic!("expected envelope-detail origin"),
                }
            }
            _ => panic!("expected contextual Series detail"),
        }

        assert!(handle_global_key(&mut app, key(KeyCode::Char('S'))));
        match &app.screen {
            Screen::Series { state } => assert_eq!(state.mode, SeriesMode::List),
            _ => panic!("expected Series list"),
        }
        assert_eq!(
            app.pending_series_select.as_deref(),
            Some(series_id.as_str())
        );

        app.return_from_series();
        assert_envelope_detail_screen(&app, &envelope_detail);
    }

    #[test]
    fn direct_series_list_remembers_the_plans_origin() {
        let (mut app, _, _) = app_with_envelope_detail();
        app.screen = Screen::Plans;

        assert!(handle_global_key(&mut app, key(KeyCode::Char('S'))));
        match &app.screen {
            Screen::Series { state } => {
                assert_eq!(state.mode, SeriesMode::List);
                assert!(matches!(state.origin, SeriesOrigin::Plans));
            }
            _ => panic!("expected Series list"),
        }

        app.return_from_series();
        assert!(matches!(app.screen, Screen::Plans));
    }

    #[test]
    fn contextual_series_returns_to_the_originating_plan_editor() {
        let (mut app, envelope_detail, _) = app_with_envelope_detail();
        let series_id = series_id_for_detail(&app, &envelope_detail);
        let plan_id = queries::plans(&app.conn).unwrap()[0].id.clone();
        app.screen = Screen::PlanEditor {
            plan_id: plan_id.clone(),
        };

        handle_global_key_with_series(&mut app, key(KeyCode::Char('S')), Some(series_id));
        app.return_from_series();

        match &app.screen {
            Screen::PlanEditor { plan_id: actual } => assert_eq!(actual, &plan_id),
            _ => panic!("expected the originating plan editor"),
        }
    }

    #[test]
    fn contextual_series_detail_renders_and_edits_then_returns() {
        let (mut app, envelope_detail, _) = app_with_envelope_detail();
        let series_id = series_id_for_detail(&app, &envelope_detail);
        handle_global_key_with_series(&mut app, key(KeyCode::Char('S')), Some(series_id.clone()));
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let view =
            leeway::view::SeriesPageView::build(&app.conn, today, SeriesTimeRange::Last12Stamped)
                .unwrap();
        let detail = series::detail_by_id(&view, &series_id).unwrap();

        let backend = ratatui::backend::TestBackend::new(100, 32);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| series::draw_detail_screen(frame, &app, &view, detail))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Dining - envelope series"));
        assert!(text.contains("Summary"));
        assert!(text.contains("all series"));

        series::handle_detail_key(&mut app, key(KeyCode::Char('m')), detail, today).unwrap();
        let changed = queries::get_series(&app.conn, &series_id).unwrap().unwrap();
        assert_eq!(changed.mode, Some(Mode::Automatic));

        series::handle_detail_key(&mut app, key(KeyCode::Esc), detail, today).unwrap();
        assert_envelope_detail_screen(&app, &envelope_detail);
    }

    #[test]
    fn detail_promotion_relaxes_only_conflicting_list_state() {
        let (mut app, envelope_detail, _) = app_with_envelope_detail();
        let series_id = series_id_for_detail(&app, &envelope_detail);
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let view =
            leeway::view::SeriesPageView::build(&app.conn, today, SeriesTimeRange::Last12Stamped)
                .unwrap();
        app.series_filter = SeriesFilter::AdHoc;
        app.series_search = "not dining".into();

        series::reveal_series_by_id(&mut app, &view, &series_id);

        assert!(app.series_filter == SeriesFilter::Both);
        assert!(app.series_search.is_empty());
        assert_eq!(app.series_sel, 0);
    }

    #[test]
    fn series_backed_envelope_label_key_points_to_the_series_page() {
        let (mut app, detail, _) = app_with_envelope_detail();

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('l'))).unwrap();

        assert!(app.modal.is_none());
        assert_eq!(app.status.as_deref(), Some(SERIES_RENAME_GUIDANCE));
        assert_envelope_detail_screen(&app, &detail);
    }

    #[test]
    fn seriesless_envelope_label_still_has_a_local_compatibility_editor() {
        let (mut app, mut detail, _) = app_with_envelope_detail();
        let envelope_id = ops::add_oneoff_envelope(
            &app.conn,
            &detail.month_id,
            "Legacy",
            Money::from_dollars(50.0),
            PeriodType::Monthly,
            Mode::Manual,
        )
        .unwrap();
        detail.envelope_id = envelope_id.clone();
        app.screen = Screen::EnvelopeDetail {
            detail: detail.clone(),
        };

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('l'))).unwrap();
        match &app.modal {
            Some(Modal::Text(prompt)) => {
                assert_eq!(prompt.title, "Envelope label");
                assert!(prompt.return_to_envelope_detail.is_some());
            }
            _ => panic!("expected envelope label prompt"),
        }
        for c in "Travel".chars() {
            handle_modal_key(&mut app, key(KeyCode::Char(c))).unwrap();
        }
        handle_modal_key(&mut app, key(KeyCode::Enter)).unwrap();

        assert_envelope_detail_screen(&app, &detail);
        let envelope = queries::load_envelopes(&app.conn, &detail.month_id)
            .unwrap()
            .into_iter()
            .find(|envelope| envelope.id == envelope_id)
            .unwrap();
        assert_eq!(envelope.label, "Travel");
    }

    #[test]
    fn r_no_longer_edits_an_envelope_label() {
        let (mut app, detail, _) = app_with_envelope_detail();

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('r'))).unwrap();

        assert!(app.modal.is_none());
        assert_envelope_detail_screen(&app, &detail);
    }

    #[test]
    fn envelope_detail_focus_scopes_controls_to_its_section() {
        let (mut app, detail, _) = app_with_envelope_detail();

        // Details owns envelope-level edits at first; transaction creation is inert here.
        handle_envelope_detail_key(&mut app, key(KeyCode::Char('s'))).unwrap();
        assert_envelope_detail_screen(&app, &detail);

        handle_envelope_detail_key(&mut app, key(KeyCode::Tab)).unwrap();
        let mut transactions_detail = detail.clone();
        transactions_detail.focus = EnvelopeDetailFocus::Transactions;
        assert_envelope_detail_screen(&app, &transactions_detail);

        // Transactions own transaction creation; envelope label edits are inert here.
        handle_envelope_detail_key(&mut app, key(KeyCode::Char('l'))).unwrap();
        assert_envelope_detail_screen(&app, &transactions_detail);

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('s'))).unwrap();
        match &app.modal {
            Some(Modal::Text(prompt)) => {
                assert_eq!(prompt.title, "Transaction label for Dining");
                assert_eq!(
                    prompt
                        .return_to_envelope_detail
                        .as_ref()
                        .map(|detail| detail.focus),
                    Some(EnvelopeDetailFocus::Transactions)
                );
            }
            _ => panic!("expected transaction label prompt"),
        }
    }

    #[test]
    fn x_confirms_deleting_the_selected_envelope_transaction() {
        let (mut app, mut detail, _) = app_with_envelope_detail();
        let transaction_id = ops::add_envelope_spending(
            &app.conn,
            &detail.month_id,
            &detail.envelope_id,
            "Coffee",
            Money::from_dollars(4.5),
        )
        .unwrap();
        handle_envelope_detail_key(&mut app, key(KeyCode::Tab)).unwrap();
        detail.focus = EnvelopeDetailFocus::Transactions;

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('x'))).unwrap();

        match &app.modal {
            Some(Modal::Confirm(confirm)) => {
                assert_eq!(confirm.title, "Delete transaction “Coffee”?");
                match &confirm.action {
                    ConfirmAction::DeleteTxn { id } => assert_eq!(id, &transaction_id),
                    _ => panic!("expected transaction delete confirmation"),
                }
                assert_eq!(
                    confirm
                        .return_to_envelope_detail
                        .as_ref()
                        .map(|detail| detail.focus),
                    Some(EnvelopeDetailFocus::Transactions)
                );
            }
            _ => panic!("expected delete confirmation"),
        }
    }

    #[test]
    fn a_edits_the_selected_envelope_transaction_amount() {
        let (mut app, mut detail, _) = app_with_envelope_detail();
        let transaction_id = ops::add_envelope_spending(
            &app.conn,
            &detail.month_id,
            &detail.envelope_id,
            "Coffee",
            Money::from_dollars(4.5),
        )
        .unwrap();
        handle_envelope_detail_key(&mut app, key(KeyCode::Tab)).unwrap();
        detail.focus = EnvelopeDetailFocus::Transactions;

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('a'))).unwrap();

        match &app.modal {
            Some(Modal::Text(prompt)) => {
                assert_eq!(prompt.title, "Amount for Coffee");
                match &prompt.kind {
                    PromptKind::TxnAmount { id } => assert_eq!(id, &transaction_id),
                    _ => panic!("expected transaction amount prompt"),
                }
            }
            _ => panic!("expected amount prompt"),
        }
    }

    #[test]
    fn escape_returns_from_envelope_detail_to_dashboard() {
        let (mut app, _, _) = app_with_envelope_detail();

        handle_envelope_detail_key(&mut app, key(KeyCode::Esc)).unwrap();

        assert!(matches!(app.screen, Screen::Dashboard));
    }

    #[test]
    fn canceling_envelope_spend_amount_returns_to_detail_screen() {
        let (mut app, mut detail, _) = app_with_envelope_detail();
        handle_envelope_detail_key(&mut app, key(KeyCode::Tab)).unwrap();
        detail.focus = EnvelopeDetailFocus::Transactions;

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('s'))).unwrap();
        for c in "Coffee".chars() {
            handle_modal_key(&mut app, key(KeyCode::Char(c))).unwrap();
        }
        handle_modal_key(&mut app, key(KeyCode::Enter)).unwrap();
        match &app.modal {
            Some(Modal::Text(prompt)) => {
                assert_eq!(prompt.title, "Amount for Coffee");
                assert!(prompt.return_to_envelope_detail.is_some());
            }
            _ => panic!("expected envelope spend amount prompt"),
        }

        handle_modal_key(&mut app, key(KeyCode::Esc)).unwrap();

        assert_envelope_detail_screen(&app, &detail);
    }

    #[test]
    fn automatic_envelope_accepts_recorded_transactions() {
        let (mut app, mut detail, envelope_id) = app_with_envelope_detail();
        ops::set_envelope_mode(&app.conn, &envelope_id, Mode::Automatic).unwrap();
        handle_envelope_detail_key(&mut app, key(KeyCode::Tab)).unwrap();
        detail.focus = EnvelopeDetailFocus::Transactions;

        handle_envelope_detail_key(&mut app, key(KeyCode::Char('s'))).unwrap();
        for c in "Coffee".chars() {
            handle_modal_key(&mut app, key(KeyCode::Char(c))).unwrap();
        }
        handle_modal_key(&mut app, key(KeyCode::Enter)).unwrap();
        for c in "12.50".chars() {
            handle_modal_key(&mut app, key(KeyCode::Char(c))).unwrap();
        }
        handle_modal_key(&mut app, key(KeyCode::Enter)).unwrap();

        assert_envelope_detail_screen(&app, &detail);
        let transactions =
            queries::load_envelope_txns(&app.conn, &detail.month_id, &detail.envelope_id).unwrap();
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].label, "Coffee");
        assert_eq!(transactions[0].amount, Money::from_dollars(12.5));
    }

    // --- Contextual help -------------------------------------------------------

    /// Flatten a rendered TestBackend buffer into a single string for substring checks.
    fn buffer_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn h_opens_help_for_the_focused_box() {
        let (mut app, _, _) = app_with_envelope_detail();
        // Focus is on the Details section of the envelope drill-in.
        assert_eq!(help::topic_for(&app), help::HelpTopic::EnvDetails);

        assert!(handle_global_key(&mut app, key(KeyCode::Char('h'))));
        match &app.modal {
            Some(Modal::Help(state)) => assert_eq!(state.current, help::HelpTopic::EnvDetails),
            _ => panic!("expected help modal"),
        }
    }

    #[test]
    fn help_cycles_siblings_and_opens_overview_then_closes() {
        let (mut app, _, _) = app_with_envelope_detail();
        handle_global_key(&mut app, key(KeyCode::Char('h')));

        // Tab walks the two-topic envelope-detail ring and wraps back.
        handle_modal_key(&mut app, key(KeyCode::Tab)).unwrap();
        match &app.modal {
            Some(Modal::Help(s)) => assert_eq!(s.current, help::HelpTopic::EnvTransactions),
            _ => panic!("expected help modal"),
        }
        handle_modal_key(&mut app, key(KeyCode::Tab)).unwrap();
        match &app.modal {
            Some(Modal::Help(s)) => assert_eq!(s.current, help::HelpTopic::EnvDetails),
            _ => panic!("expected help modal"),
        }

        // `o` jumps to the app overview; `h` closes the overlay.
        handle_modal_key(&mut app, key(KeyCode::Char('o'))).unwrap();
        match &app.modal {
            Some(Modal::Help(s)) => assert_eq!(s.current, help::HelpTopic::Overview),
            _ => panic!("expected help modal"),
        }
        handle_modal_key(&mut app, key(KeyCode::Char('h'))).unwrap();
        assert!(app.modal.is_none());
    }

    #[test]
    fn help_modal_renders_topic_content() {
        let (mut app, _, _) = app_with_envelope_detail();
        handle_global_key(&mut app, key(KeyCode::Char('h')));

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_modal(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Help"), "title bar should name the help modal");
        assert!(text.contains("Details"), "should render the focused topic");
        assert!(text.contains("How it fits"), "should render authored sections");
    }

    #[test]
    fn h_is_literal_inside_a_text_prompt() {
        // With a text modal open, `h` is typed into the buffer, not swallowed as help.
        // (The event loop consults the modal before the global `h` handler.)
        let (mut app, _, _) = app_with_envelope_detail();
        handle_envelope_detail_key(&mut app, key(KeyCode::Tab)).unwrap();
        handle_envelope_detail_key(&mut app, key(KeyCode::Char('s'))).unwrap();

        handle_modal_key(&mut app, key(KeyCode::Char('h'))).unwrap();
        match &app.modal {
            Some(Modal::Text(prompt)) => assert_eq!(prompt.buffer, "h"),
            _ => panic!("typing h should stay in the text prompt, not open help"),
        }
    }

    #[test]
    fn help_modal_survives_a_tiny_frame() {
        // Clamping math must not panic when the box is smaller than its content.
        let (mut app, _, _) = app_with_envelope_detail();
        handle_global_key(&mut app, key(KeyCode::Char('h')));

        let backend = ratatui::backend::TestBackend::new(12, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_modal(f, &app)).unwrap();
    }
}
