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

/// Subtle band behind a selected list row. Site: `--line-bright` (#45475a).
pub const SELECTION: Color = Color::Rgb(69, 71, 90);

/// Empty segment of an envelope meter. A touch lighter than [`SELECTION`] so an
/// empty meter stays visible even on a selected (banded) row — the site's track
/// colour equals SELECTION, which would vanish under the band here.
pub const METER_TRACK: Color = Color::Rgb(88, 91, 112);
