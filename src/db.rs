//! Opening the database and keeping its schema current.
//!
//! `rusqlite` is synchronous — a call returns when the query is done. For a single-user
//! local app that's exactly what you want: no async runtime, no `.await`, just function
//! calls. The whole app shares one `Connection`.

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};
use std::path::Path;
use std::sync::LazyLock;

/// The versioned list of migrations. `M::up(...)` is a forward migration; the position
/// in this slice is its version number. To evolve the schema later you *append* a new
/// `M::up(...)` — never edit an existing one, since users' databases have already run it.
///
/// `LazyLock` builds this once, on first use, and caches it. `include_str!` pastes the
/// contents of schema.sql into the binary at compile time, so there's no file to ship
/// alongside the executable.
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("schema.sql")),
        M::up(include_str!("migration_002_integrity_and_indexes.sql")),
    ])
});

/// Open the database at `path` (creating the file if missing) and bring its schema up to
/// the latest version. Returns the live connection the rest of the app uses.
pub fn open(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;

    // Enforce foreign-key constraints (SQLite leaves them off by default).
    conn.pragma_update(None, "foreign_keys", true)
        .context("enabling foreign keys")?;

    // Apply any migrations this database hasn't run yet. On a brand-new file this runs
    // schema.sql; on an up-to-date file it does nothing.
    MIGRATIONS
        .to_latest(&mut conn)
        .context("applying database migrations")?;

    Ok(conn)
}

/// Open a throwaway in-memory database with the schema applied — used by tests so they
/// never touch a real file.
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", true)?;
    MIGRATIONS.to_latest(&mut conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_applies_cleanly() {
        let conn = open_in_memory().expect("schema should apply");
        // The default setting row from schema.sql should be present.
        let mode: String = conn
            .query_row(
                "SELECT value FROM setting WHERE key = 'default_envelope_mode'",
                [],
                |r| r.get(0),
            )
            .expect("default setting row");
        assert_eq!(mode, "automatic");
    }

    #[test]
    fn account_uses_card_model() {
        let conn = open_in_memory().expect("schema should apply");
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('account')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(cols.iter().any(|c| c == "credit_limit_cents"));
        assert!(cols.iter().any(|c| c == "available_credit_cents"));
        assert!(
            !cols.iter().any(|c| c == "protected"),
            "protected flag dropped"
        );
    }
}
