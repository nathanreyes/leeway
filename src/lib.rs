//! Leeway — the headless core.
//!
//! This library crate holds everything *except* the UI: the data model, money type,
//! SQLite access, the §4 calculations, the §5 operations, and the read-model the UI
//! renders. It has no dependency on ratatui, so it could be reused unchanged behind a
//! web or desktop frontend. The terminal UI lives in `main.rs` and depends on this.

pub mod calc;
pub mod currency;
pub mod db;
pub mod models;
pub mod money;
pub mod ops;
pub mod queries;
pub mod sync;
pub mod view;
