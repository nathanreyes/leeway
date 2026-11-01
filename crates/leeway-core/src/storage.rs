//! Local database paths and recovery backups.

use anyhow::{Context, Result};
use rusqlite::{Connection, MAIN_DB};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub database: PathBuf,
    pub recovery: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let data_dir = platform_data_dir()?;
        Ok(Self::in_dir(data_dir))
    }

    pub fn in_dir(data_dir: PathBuf) -> Self {
        Self {
            database: data_dir.join("leeway.db"),
            recovery: data_dir.join("recovery"),
            data_dir,
        }
    }

    pub fn create(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating {}", self.data_dir.display()))?;
        fs::create_dir_all(&self.recovery)
            .with_context(|| format!("creating {}", self.recovery.display()))?;
        Ok(())
    }
}

fn platform_data_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Leeway"))
    }
    #[cfg(target_os = "windows")]
    {
        let base = env::var_os("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(base).join("Leeway"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(base) = env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(base).join("leeway"));
        }
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".local/share/leeway"))
    }
}

/// Save a consistent SQLite snapshot without stopping the app.
pub fn archive_connection(conn: &Connection, recovery: &Path) -> Result<PathBuf> {
    fs::create_dir_all(recovery)?;
    let path = recovery.join(format!(
        "{}-{}.db",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        Uuid::new_v4()
    ));
    conn.backup(MAIN_DB, &path, None)
        .context("creating recovery backup")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn existing_local_budget_opens_without_reading_old_sync_files() {
        let paths =
            AppPaths::in_dir(env::temp_dir().join(format!("leeway-storage-{}", Uuid::new_v4())));
        paths.create().unwrap();
        let conn = db::open(&paths.database).unwrap();
        conn.execute(
            "INSERT INTO setting (key, value) VALUES ('currency', 'EUR')",
            [],
        )
        .unwrap();
        drop(conn);
        // Even unreadable legacy configuration must not block local use.
        let config = paths.data_dir.join("config.json");
        fs::write(&config, "invalid old sync config").unwrap();
        let old_backup = paths.recovery.join("old-backup.db");
        fs::write(&old_backup, "keep this file").unwrap();
        paths.create().unwrap();
        let conn = db::open(&paths.database).unwrap();
        let currency: String = conn
            .query_row(
                "SELECT value FROM setting WHERE key = 'currency'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(currency, "EUR");
        assert_eq!(
            fs::read_to_string(config).unwrap(),
            "invalid old sync config"
        );
        assert_eq!(fs::read_to_string(old_backup).unwrap(), "keep this file");
        drop(conn);
        fs::remove_dir_all(paths.data_dir).unwrap();
    }

    #[test]
    fn backups_capture_committed_wal_data_and_keep_previous_snapshots() {
        let paths =
            AppPaths::in_dir(env::temp_dir().join(format!("leeway-backup-{}", Uuid::new_v4())));
        paths.create().unwrap();
        let conn = db::open(&paths.database).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute(
            "INSERT INTO setting (key, value) VALUES ('currency', 'EUR')",
            [],
        )
        .unwrap();
        let first = archive_connection(&conn, &paths.recovery).unwrap();
        conn.execute(
            "UPDATE setting SET value = 'USD' WHERE key = 'currency'",
            [],
        )
        .unwrap();
        let second = archive_connection(&conn, &paths.recovery).unwrap();
        assert_ne!(first, second);
        for (path, expected) in [(first, "EUR"), (second, "USD")] {
            let restored = db::open(&path).unwrap();
            let currency: String = restored
                .query_row(
                    "SELECT value FROM setting WHERE key = 'currency'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(currency, expected);
            let check: String = restored
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
            assert_eq!(check, "ok");
        }
        drop(conn);
        fs::remove_dir_all(paths.data_dir).unwrap();
    }
}
