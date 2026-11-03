//! Opening the database and keeping its schema current.
//!
//! `rusqlite` is synchronous — a call returns when the query is done. For a single-user
//! local app that's exactly what you want: no async runtime, no `.await`, just function
//! calls. The whole app shares one `Connection`.

use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction};
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
        M::up(include_str!("migration_003_plan_item_active_months.sql")),
        M::up(include_str!("migration_004_plan_item_forecast_method.sql")),
        M::up_with_hook(include_str!("migration_005_sync_ready.sql"), fix_month_ids),
        M::up(include_str!("migration_006_bank_sync.sql")),
    ])
});

/// Give every existing month the fixed id `ops::month_id` derives from its label, and move
/// its envelopes and transactions along with it. Foreign keys are checked at commit, so the
/// parent and child rows can change in any order.
fn fix_month_ids(tx: &Transaction) -> rusqlite_migration::HookResult {
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    let months: Vec<(String, String)> = tx
        .prepare("SELECT id, label FROM month")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (old_id, label) in months {
        let new_id = crate::ops::month_id(&label);
        if new_id == old_id {
            continue;
        }
        tx.execute("UPDATE month SET id = ?1 WHERE id = ?2", [&new_id, &old_id])?;
        tx.execute(
            "UPDATE envelope SET month_id = ?1 WHERE month_id = ?2",
            [&new_id, &old_id],
        )?;
        tx.execute(
            "UPDATE txn SET month_id = ?1 WHERE month_id = ?2",
            [&new_id, &old_id],
        )?;
    }
    Ok(())
}

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

    #[test]
    fn plan_items_carry_active_months_defaulting_to_null() {
        let conn = open_in_memory().expect("schema should apply");
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('plan_item')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(cols.iter().any(|c| c == "active_months"));

        // A row written without the column reads NULL — "every month", so the migration
        // changes nothing for plans that already exist.
        conn.execute("INSERT INTO plan (id, name) VALUES ('p', 'P')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO series (id, kind, label, mode) VALUES ('s', 'envelope', 'E', 'automatic')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plan_item (id, plan_id, series_id, amount_cents)
             VALUES ('i', 'p', 's', 100)",
            [],
        )
        .unwrap();
        let stored: Option<i64> = conn
            .query_row(
                "SELECT active_months FROM plan_item WHERE id = 'i'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, None);

        // The CHECK keeps a nonsense mask out of the column.
        assert!(
            conn.execute(
                "UPDATE plan_item SET active_months = 4096 WHERE id = 'i'",
                []
            )
            .is_err()
        );
        assert!(
            conn.execute("UPDATE plan_item SET active_months = 0 WHERE id = 'i'", [])
                .is_err()
        );
    }

    #[test]
    fn plan_items_default_to_static_forecasts() {
        let conn = open_in_memory().expect("schema should apply");
        conn.execute("INSERT INTO plan (id, name) VALUES ('p', 'P')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO series (id, kind, label, direction) VALUES ('s', 'transaction', 'T', 'out')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plan_item (id, plan_id, series_id, amount_cents)
             VALUES ('i', 'p', 's', 100)",
            [],
        )
        .unwrap();

        let method: String = conn
            .query_row(
                "SELECT forecast_method FROM plan_item WHERE id = 'i'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(method, "static");
    }

    fn updated_at(conn: &Connection, id: &str) -> String {
        conn.query_row("SELECT updated_at FROM plan WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn tombstones(conn: &Connection, table: &str, id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM deletion WHERE table_name = ?1 AND row_id = ?2",
            [table, id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn writes_keep_updated_at_current() {
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO plan (id, name) VALUES ('p', 'P')", [])
            .unwrap();
        let inserted = updated_at(&conn, "p");
        assert!(
            inserted.ends_with('Z') && inserted.len() == 24,
            "got {inserted}"
        );

        std::thread::sleep(std::time::Duration::from_millis(5));
        conn.execute("UPDATE plan SET name = 'Q' WHERE id = 'p'", [])
            .unwrap();
        assert!(updated_at(&conn, "p") > inserted);

        // A write that sets updated_at itself, as a sync applying a remote row would, keeps it.
        conn.execute(
            "UPDATE plan SET name = 'R', updated_at = '2020-01-01T00:00:00.000Z' WHERE id = 'p'",
            [],
        )
        .unwrap();
        assert_eq!(updated_at(&conn, "p"), "2020-01-01T00:00:00.000Z");
    }

    #[test]
    fn deletes_leave_tombstones_until_the_row_returns() {
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO plan (id, name) VALUES ('p', 'P')", [])
            .unwrap();
        assert_eq!(tombstones(&conn, "plan", "p"), 0);

        conn.execute("DELETE FROM plan WHERE id = 'p'", []).unwrap();
        assert_eq!(tombstones(&conn, "plan", "p"), 1);

        conn.execute("INSERT INTO plan (id, name) VALUES ('p', 'P')", [])
            .unwrap();
        assert_eq!(tombstones(&conn, "plan", "p"), 0);

        conn.execute(
            "DELETE FROM setting WHERE key = 'default_envelope_mode'",
            [],
        )
        .unwrap();
        assert_eq!(tombstones(&conn, "setting", "default_envelope_mode"), 1);
    }

    #[test]
    fn migration_gives_existing_months_fixed_ids() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        MIGRATIONS.to_version(&mut conn, 4).unwrap();
        conn.execute_batch(
            "INSERT INTO month (id, label, start_date, days_in_month)
                 VALUES ('random-id', '2026-10', '2026-10-01', 31);
             INSERT INTO envelope (id, month_id, label, amount_cents, stamped_amount_cents,
                                   period_type, mode)
                 VALUES ('e', 'random-id', 'Groceries', 60000, 60000, 'monthly', 'automatic');
             INSERT INTO txn (id, month_id, envelope_id, label, direction, amount_cents)
                 VALUES ('t', 'random-id', 'e', 'Tacos', 'out', 2450);",
        )
        .unwrap();

        MIGRATIONS.to_latest(&mut conn).unwrap();

        let fixed = crate::ops::month_id("2026-10");
        let month: String = conn
            .query_row("SELECT id FROM month WHERE label = '2026-10'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(month, fixed);
        for table in ["envelope", "txn"] {
            let month_id: String = conn
                .query_row(&format!("SELECT month_id FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(month_id, fixed, "{table} follows its month");
        }
        let broken: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(broken, 0);
    }
}
