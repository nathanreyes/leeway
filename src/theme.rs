//! Shared UI colour palette.
//!
//! These mirror the marketing site's palette (`site/src/styles/global.css`) so
//! the app and landing page read as one product. Values are truecolor RGB; the
//! glow animation in `anim.rs` already handles `Color::Rgb` directly, so these
//! flow through it unchanged.

use ratatui::style::Color;

/// Primary accent — envelope meters and focused panel borders. Site: `--mauve`.
pub const MAUVE: Color = Color::Rgb(201, 166, 242);

/// Positive money: balances in the black, income/what's-left remaining, credits.
/// Site: `--green`.
pub const GREEN: Color = Color::Rgb(147, 224, 160);

/// Secondary accent — selection markers, active prompts, informational metrics.
/// Site: `--cyan`.
pub const CYAN: Color = Color::Rgb(134, 217, 230);

/// Band behind a selected list row. Catppuccin Mocha `Surface2` (#585b70) —
/// bright enough to read clearly against the terminal's dark base, where the
/// former `#45475a` tint nearly vanished.
pub const SELECTION: Color = Color::Rgb(88, 91, 112);

/// Empty segment of an envelope meter. A touch lighter than [`SELECTION`] so an
/// empty meter stays visible even on a selected (banded) row — a track colour at
/// or below SELECTION would vanish under the band here. Mocha `Overlay0` (#6c7086).
pub const METER_TRACK: Color = Color::Rgb(108, 112, 134);

/// Secondary text inside a list row — a settled transaction, a count, a period
/// column. Faded so the row's main content leads. Mocha `Overlay1` (#7f849c): a
/// step up from the old `DarkGray`, which sank too close to the background to
/// read at a glance.
pub const MUTED: Color = Color::Rgb(127, 132, 156);

/// [`MUTED`] on the selected row. The plain tone sits too near [`SELECTION`] to
/// survive the band, so a highlighted row lifts to Mocha `Subtext0` (#a6adc8):
/// still quieter than the rest of the row, still legible.
pub const MUTED_SELECTED: Color = Color::Rgb(166, 173, 200);

/// Pick the muted tone for a list row, given whether the selection band is
/// behind it.
pub fn muted(selected: bool) -> Color {
    if selected { MUTED_SELECTED } else { MUTED }
}

/// Gap segment above a bar on the Series trend chart. More subdued than
/// [`METER_TRACK`]: the chart sits on plain background (no selection band to
/// read over), so a dimmer slate keeps the filler quiet against the mauve
/// fill. Mocha `Surface1` (#45475a).
pub const CHART_TRACK: Color = Color::Rgb(69, 71, 90);
