# Gamified Summary Animations on the Dashboard

Status: Approved design, not yet implemented.

## Context

The Dashboard's "Summary" panel (`draw_whats_left`) already updates correctly whenever you
clear a transaction, edit an account balance, or add envelope spend — every number is
recomputed from SQLite on each loop iteration. But those updates are *silent*: the number
snaps to its new value with no cue that anything happened, and nothing visually ties
"I cleared this bill" to "the total moved."

The goal is to make each change feel alive and connected. When a Summary number changes,
**roll it** from its old value to its new one, and **flash its background** in a
celebratory glow — the term's own color, fading out over ~half a second — with the text
inverted while the glow is lit. This applies to every Summary term *and* the bold
"= … what's left" headline, so a single edit visibly ripples into the total.

Confirmed design choices:
- **Smooth RGB glow fade** — background lights up in the term's color at full strength and
  eases back toward the panel background. Introduces `Color::Rgb` (the app currently uses
  only named colors; see the dark-terminal note below).
- **Every changed term + the headline** animate (funds, buffer, card debt, carry, income
  left, bills left, envelopes, and the headline).

## The core obstacle

The app has **no animation loop today**. `run()` in `src/main.rs` draws once, then blocks
in `read_key()` on `event::poll(IDLE_TICK)` (60 s) until a key arrives. There is no
per-frame tick, no `Instant`-based clock, and `App` holds no Summary values (they live only
in the per-iteration `MonthView`). So this change has three parts: (1) a place to remember
previous values and in-flight animations, (2) a clock plus faster redraw *while* animating,
and (3) render code that draws the tween + glow.

## Design

### 1. New module: `src/anim.rs` (declared `mod anim;` in `main.rs`, beside `dashboard`)

Heavily commented (build-and-read style). Contents:

- `pub enum SummaryTerm { Funds, Buffer, CardDebt, Carry, IncomeLeft, BillsLeft, Envelopes, WhatsLeft }`
  — one per animatable number. `#[derive(Clone, Copy, PartialEq, Eq, Hash)]`.

- `fn display_cents(term, wl: &WhatsLeft) -> i64` — the **single source of truth** for each
  term's *display-signed* value, mirroring exactly what `draw_whats_left` shows today:
  Funds `+funds_available`, Buffer `-checking_buffer`, CardDebt `-card_debt`,
  Carry `+card_carry`, IncomeLeft `+income_remaining`, BillsLeft `-bills_remaining`,
  Envelopes `-envelopes_remaining`, WhatsLeft `+whats_left`. Used by both `sync` and the
  render call sites so the tracked value and the drawn value never drift.

- `struct Anim { from: i64, to: i64, start: Instant }` and
  ```rust
  pub struct SummaryAnimations {
      prev: HashMap<SummaryTerm, i64>,        // last steady value per term
      active: HashMap<SummaryTerm, Anim>,     // in-flight tweens
      last_period: Option<(i32, u32, bool)>,  // (year, month, is_current)
  }
  ```

- Constants: `ANIM_DURATION = Duration::from_millis(550)`. Easing helpers
  `ease_out_cubic(t) = 1 - (1-t)^3` (count roll) and `glow(t) = (1-t)^2` (fast-fading flash,
  1.0 at start → 0.0 at end). A `term_rgb(Color) -> (u8,u8,u8)` map for the palette actually
  used (Cyan/Yellow/Red/Green/Magenta; fallback white) and `lerp_rgb(a, b, t) -> Color::Rgb`.

- Methods:
  - `sync(&mut self, wl: Option<&WhatsLeft>, is_current: bool, period: (i32,u32), now: Instant)`:
    - `wl == None` (unstamped month) → clear `active` + `prev`, `last_period = None`, return.
    - period (incl. `is_current`) changed vs `last_period` → **baseline only**: set `prev` to
      the current `display_cents` for all terms, clear `active`, store `last_period`. This is
      what prevents month-navigation and first-load from flashing everything.
    - same period → for each term compare `display_cents` to `prev`; on change insert an
      `Anim { from: current displayed value (smooth if mid-flight, else prev), to: cur }` and
      update `prev`. A term going 0→nonzero (buffer/carry appearing) rolls up from 0.
    - Prune `active` entries whose elapsed ≥ `ANIM_DURATION`.
  - `is_animating(&self, now) -> bool` — any live anim; drives the poll cadence.
  - `render(&self, term, base_cents, color, now) -> (i64, Style)` — if a live anim exists:
    `p = ease_out_cubic(t)`, `disp = from + ((to-from) as f64 * p).round()`; `g = glow(t)`,
    `bg = lerp_rgb(black, term_rgb(color), g)`, `fg = lerp_rgb(term_rgb(color), black, g)`
    (text starts black-on-bright and hands off to the normal colored text as the glow dies);
    returns `(disp, Style::default().fg(fg).bg(bg))`. Otherwise `(base_cents, Style::default().fg(color))`.

  **Dark-terminal note:** the glow fades toward black `(0,0,0)`, matching the app's existing
  `fg(Black).bg(Cyan)` badge convention and the common dark-TUI case. Document this
  assumption in a comment; on a light terminal the tail of the fade would read as a dark
  smudge (acceptable, and tunable via `term_rgb`/the fade target later).

### 2. Wire state + clock into `App` and the loop (`src/main.rs`)

- Add `use std::time::Instant;` and `mod anim; use anim::SummaryAnimations;`.
- `App` gains `summary_anims: SummaryAnimations` and `frame_now: Instant`; initialize in
  `main()` (~`main.rs:481`) with `SummaryAnimations::new()` and `Instant::now()`.
- In the **Dashboard** branch of `run()` (`main.rs:531`), after the existing pending/clamp
  handling and before `terminal.draw`:
  ```rust
  let now = Instant::now();
  app.frame_now = now;
  app.summary_anims.sync(
      view.as_ref().map(|v| &v.whats_left),
      view.as_ref().map(|v| v.is_current).unwrap_or(false),
      (app.viewed_year, app.viewed_month),
      now,
  );
  ```
  The `terminal.draw(|f| dashboard::draw(f, app, &view))` closure is unchanged — it reads
  `app.summary_anims` / `app.frame_now` immutably (the `&mut` `sync` finished before it).
- Faster redraw while animating: change `read_key()` → `read_key(timeout: Duration)`
  (`main.rs:716`) and, in the Dashboard branch only, pass
  `if app.summary_anims.is_animating(now) { FRAME_TICK } else { IDLE_TICK }`. Add
  `const FRAME_TICK: Duration = Duration::from_millis(33);` (~30 fps). The other three
  screen branches pass `IDLE_TICK`, preserving today's 0 %-idle-CPU behavior. When the last
  anim finishes, `is_animating` returns false and the loop parks on the 60 s poll again.

### 3. Render the tween + glow (`src/dashboard.rs`, `draw_whats_left` / `summary_term`)

- `draw_whats_left(frame, area, view)` → `draw_whats_left(frame, area, view, anims: &SummaryAnimations, now: Instant)`;
  the caller `draw_month_body` (`dashboard.rs:535`) has `app`, so pass
  `&app.summary_anims, app.frame_now`. Add `use crate::anim::{SummaryAnimations, SummaryTerm};`.
- `summary_term(amount, label, color)` → `summary_term(term: SummaryTerm, amount, label, color, anims, now)`.
  Inside: `let (cents, amount_style) = anims.render(term, amount.cents(), color, now);` then
  derive the sign and `Money(cents.abs())` from `cents`, and style the amount span with
  `amount_style` instead of the fixed `fg(color)`. Update each existing call to pass its
  `SummaryTerm` (e.g. `summary_term(SummaryTerm::Funds, wl.funds_available, "funds", Color::Cyan, anims, now)`,
  `summary_term(SummaryTerm::CardDebt, Money(-wl.card_debt.cents()), "card debt", Color::Red, anims, now)`, …).
- Headline: route through the same machinery —
  `let (wl_cents, wl_style) = anims.render(SummaryTerm::WhatsLeft, wl.whats_left.cents(), result_color, now);`
  build the bold span from `Money(wl_cents)` with `wl_style.add_modifier(Modifier::BOLD)`.
- **Cleanup while here:** collapse the identical `if view.is_current { … } else { … }` arms
  for `envelopes` at `dashboard.rs:852-864` into one call.

### Critical files

- `src/anim.rs` — **new**; all animation state, easing, RGB fade, `display_cents`.
- `src/main.rs` — `mod anim;`, `App` fields + init, `sync`/`frame_now` in the Dashboard
  branch, `read_key(timeout)` + `FRAME_TICK` (and 3 other call sites updated).
- `src/dashboard.rs` — `draw_whats_left` / `summary_term` signatures + glow rendering;
  headline through `render`; envelopes-arm cleanup.
- Reuse (no change): `WhatsLeft` (`src/calc.rs:61`), `Money` (`src/money.rs`),
  `MonthView.whats_left`/`is_current` (`src/view.rs`).

## Testing

1. `cargo build` then `cargo clippy` — confirm the new signatures, `Color::Rgb`, and
   `Instant` usage compile cleanly.
2. `cargo run` on the current month and exercise each path, watching the Summary panel:
   - Toggle a bill/income **paid** (`Enter`) → `bills left`/`income left` **and** the
     headline should roll to the new value with a fading colored glow.
   - Edit an **account balance** (`Enter` on Accounts) → `funds` glows/rolls; `what's left`
     follows.
   - Add **envelope spend** (`s`) → `envelopes` glows/rolls; `what's left` follows.
   - A change that flips buffer/carry between zero and nonzero should make that term appear
     and roll up from 0.
3. Confirm the *non*-animation cases stay quiet: **navigating months** (`k`/`j`/`m`) and the
   **first frame** must not flash; an **unstamped month** shows no glow and doesn't panic.
4. Confirm idle behavior: with no animation in flight the app still parks (no busy-spin) and
   the header still rolls the date over on the 60 s idle tick.
5. `cargo test` — ensure nothing in `calc`/`view`/`money` regressed.
