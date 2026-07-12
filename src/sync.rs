//! Provider-agnostic folder synchronization.
//!
//! The live SQLite database always stays in Leeway's private app-data directory. A sync
//! folder contains immutable, validated SQLite backup snapshots and small JSON manifests.
//! This module deliberately knows nothing about Dropbox, iCloud, OneDrive, or Syncthing.

use crate::db;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, MAIN_DB, OpenFlags};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
pub const SYNC_DIR_NAME: &str = "Leeway";
pub const ORDINARY_RETENTION: usize = 20;
pub const LEASE_DURATION: Duration = Duration::from_secs(45);
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);
pub const PUBLISH_DEBOUNCE: Duration = Duration::from_millis(750);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub database: PathBuf,
    pub config: PathBuf,
    pub device: PathBuf,
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
            config: data_dir.join("config.json"),
            device: data_dir.join("device.json"),
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StorageMode {
    LocalOnly,
    FolderSync,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub mode: StorageMode,
    pub sync_parent: Option<PathBuf>,
    pub last_accepted_revision: Option<String>,
    pub last_published_digest: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: StorageMode::LocalOnly,
            sync_parent: None,
            last_accepted_revision: None,
            last_published_digest: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub device_id: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncRoot {
    pub protocol_version: u32,
    pub budget_id: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Head {
    pub revision_id: String,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lease {
    pub device_id: String,
    pub device_label: String,
    pub session_id: String,
    pub base_revision: Option<String>,
    pub acquired_at_ms: i64,
    pub heartbeat_at_ms: i64,
    pub expires_at_ms: i64,
    pub released: bool,
}

impl Lease {
    pub fn active_at(&self, now_ms: i64) -> bool {
        !self.released && self.expires_at_ms > now_ms
    }

    pub fn owned_by(&self, device_id: &str, session_id: &str) -> bool {
        self.device_id == device_id && self.session_id == session_id && !self.released
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Revision {
    pub revision_id: String,
    pub parents: Vec<String>,
    pub device_id: String,
    pub device_label: String,
    pub session_id: String,
    pub published_at_ms: i64,
    pub app_version: String,
    pub schema_version: u32,
    pub snapshot_name: String,
    pub byte_length: u64,
    pub sha256: String,
    #[serde(default)]
    pub protected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inspection {
    Empty {
        root: PathBuf,
    },
    Existing {
        root: PathBuf,
        revision: Box<Revision>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseDecision {
    Acquire,
    Refresh,
    ReadOnly { owner: String, expires_at_ms: i64 },
    TakeoverRequired { owner: String, expires_at_ms: i64 },
}

const SAME_DEVICE_OWNER: &str = "Another Leeway session on this computer";
const GENERIC_DEVICE_LABEL: &str = "This computer";
const UNNAMED_DEVICE_LABEL: &str = "Unnamed device";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    LocalOnly,
    Published { revision_id: String },
    Publishing,
    SavedLocally { message: String },
    ReadOnly { owner: String },
    Attention { message: String },
}

impl SyncStatus {
    pub fn label(&self) -> String {
        match self {
            Self::LocalOnly => "Local only".into(),
            Self::Published { .. } => "Published".into(),
            Self::Publishing => "Publishing".into(),
            Self::SavedLocally { .. } => "Saved locally — not published".into(),
            Self::ReadOnly { owner } => format!("Read-only — {owner} is editing"),
            Self::Attention { .. } => "Attention needed".into(),
        }
    }
}

#[derive(Clone, Debug)]
struct PublishRequest {
    database: PathBuf,
    root: PathBuf,
    expected_parent: Option<String>,
    device: Device,
    session_id: String,
    generation: u64,
    parents: Vec<String>,
}

#[derive(Debug)]
struct PublishResult {
    generation: u64,
    result: Result<Revision, String>,
}

/// Runtime state machine used by the UI. Publication runs on a worker so ordinary input
/// remains responsive while SQLite creates and validates a backup.
pub struct Runtime {
    paths: AppPaths,
    pub config: Config,
    pub device: Device,
    pub session_id: String,
    pub status: SyncStatus,
    local_generation: u64,
    published_generation: u64,
    dirty_since: Option<Instant>,
    observed_changes: u64,
    publish_rx: Option<Receiver<PublishResult>>,
    last_watch: Instant,
    last_heartbeat: Instant,
}

impl Runtime {
    pub fn load(paths: AppPaths, conn: &Connection) -> Result<Self> {
        paths.create()?;
        let config = load_or_default::<Config>(&paths.config)?;
        let device = load_or_create_device(&paths.device)?;
        let mut runtime = Self {
            paths,
            config,
            device,
            session_id: Uuid::new_v4().to_string(),
            status: SyncStatus::LocalOnly,
            local_generation: 0,
            published_generation: 0,
            dirty_since: None,
            observed_changes: conn.total_changes(),
            publish_rx: None,
            last_watch: Instant::now(),
            last_heartbeat: Instant::now(),
        };
        runtime.start_session()?;
        Ok(runtime)
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// On launch, adopt a newer synchronized head only when the local database still
    /// matches the last snapshot this computer accepted. Any local divergence is surfaced
    /// as a conflict instead of being overwritten.
    pub fn reconcile_on_launch(&mut self, conn: &mut Connection) -> Result<()> {
        if self.config.mode != StorageMode::FolderSync {
            return Ok(());
        }
        let root = self.sync_root().context("sync folder is not configured")?;
        let remote = match validate_head(&root) {
            Ok(remote) => remote,
            Err(error) => {
                self.status = SyncStatus::Attention {
                    message: error.to_string(),
                };
                return Ok(());
            }
        };
        if !provider_conflict_files(&root)?.is_empty() || !divergent_revisions(&root)?.is_empty() {
            self.status = SyncStatus::Attention {
                message: "Synchronized conflict candidates require review".into(),
            };
            return Ok(());
        }
        let clean = match self.config.last_published_digest.as_deref() {
            Some(expected) => connection_digest(conn, &self.paths.data_dir)? == expected,
            None => false,
        };
        if self.config.last_accepted_revision.as_deref() == Some(&remote.revision_id) {
            if clean {
                return Ok(());
            }
            if self.can_edit() {
                self.local_generation = self.local_generation.saturating_add(1);
                self.dirty_since = Some(Instant::now() - PUBLISH_DEBOUNCE);
                self.status = SyncStatus::SavedLocally {
                    message: "Recovered unpublished local work".into(),
                };
            } else {
                self.status = SyncStatus::Attention {
                    message: "Unpublished local work exists while another session owns editing"
                        .into(),
                };
            }
            return Ok(());
        }
        if !clean {
            self.status = SyncStatus::Attention {
                message: format!(
                    "Local work diverged from synchronized revision {}",
                    remote.revision_id
                ),
            };
            return Ok(());
        }
        archive_connection(conn, &self.paths.recovery, "before-remote-update")?;
        let snapshot = root.join("snapshots").join(&remote.snapshot_name);
        conn.restore(MAIN_DB, &snapshot, None::<fn(rusqlite::backup::Progress)>)
            .context("importing newer synchronized revision")?;
        db::migrate(conn)?;
        crate::queries::apply_active_currency(conn)?;
        let migrated = remote.schema_version < db::SCHEMA_VERSION;
        self.observed_changes = conn.total_changes();
        self.config.last_accepted_revision = Some(remote.revision_id.clone());
        self.config.last_published_digest = Some(remote.sha256);
        self.save_config()?;
        match self.acquire_lease(false) {
            Ok(()) => {
                self.status = SyncStatus::Published {
                    revision_id: remote.revision_id,
                };
            }
            Err(error) => {
                self.status = SyncStatus::ReadOnly {
                    owner: error.to_string(),
                };
            }
        }
        if migrated && self.can_edit() {
            self.local_generation = self.local_generation.saturating_add(1);
            self.dirty_since = Some(Instant::now() - PUBLISH_DEBOUNCE);
            self.status = SyncStatus::SavedLocally {
                message: "Database upgraded; publishing the new schema".into(),
            };
        }
        Ok(())
    }

    pub fn sync_root(&self) -> Option<PathBuf> {
        self.config
            .sync_parent
            .as_ref()
            .map(|parent| parent.join(SYNC_DIR_NAME))
    }

    pub fn can_edit(&self) -> bool {
        matches!(
            self.status,
            SyncStatus::LocalOnly
                | SyncStatus::Published { .. }
                | SyncStatus::Publishing
                | SyncStatus::SavedLocally { .. }
        )
    }

    pub fn note_changes(&mut self, conn: &Connection) -> Result<()> {
        let changes = conn.total_changes();
        if changes != self.observed_changes {
            self.observed_changes = changes;
            self.local_generation = self.local_generation.saturating_add(1);
            self.dirty_since = Some(Instant::now());
            if self.config.mode == StorageMode::FolderSync {
                self.status = SyncStatus::SavedLocally {
                    message: "Waiting to publish".into(),
                };
            }
        }
        Ok(())
    }

    pub fn tick(&mut self, conn: &Connection) -> Result<()> {
        self.note_changes(conn)?;
        self.collect_publication()?;
        if self.config.mode != StorageMode::FolderSync {
            return Ok(());
        }
        if self.last_watch.elapsed() >= WATCH_INTERVAL {
            self.last_watch = Instant::now();
            self.watch_remote()?;
        }
        if self.can_edit() && self.last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            self.refresh_lease()?;
            self.last_heartbeat = Instant::now();
        }
        if self.publish_rx.is_none()
            && self.can_edit()
            && self.local_generation > self.published_generation
            && self
                .dirty_since
                .is_some_and(|dirty| dirty.elapsed() >= PUBLISH_DEBOUNCE)
        {
            self.spawn_publication()?;
        }
        Ok(())
    }

    pub fn publish_now(&mut self) -> Result<()> {
        if self.config.mode != StorageMode::FolderSync || !self.can_edit() {
            return Ok(());
        }
        if self.local_generation == self.published_generation {
            self.local_generation = self.local_generation.saturating_add(1);
            self.dirty_since = Some(Instant::now() - PUBLISH_DEBOUNCE);
        }
        if self.publish_rx.is_none() {
            self.spawn_publication()?;
        }
        Ok(())
    }

    pub fn enable_new(&mut self, parent: &Path) -> Result<()> {
        let root = sync_root_for(parent);
        validate_parent(parent)?;
        if root.join("sync-root.json").exists() {
            bail!("a synchronized Leeway budget already exists there");
        }
        create_sync_layout(&root)?;
        let sync_root = SyncRoot {
            protocol_version: PROTOCOL_VERSION,
            budget_id: Uuid::new_v4().to_string(),
            created_at_ms: now_ms(),
        };
        atomic_json(&root.join("sync-root.json"), &sync_root)?;
        self.config.mode = StorageMode::FolderSync;
        self.config.sync_parent = Some(parent.to_path_buf());
        self.config.last_accepted_revision = None;
        self.config.last_published_digest = None;
        self.save_config()?;
        self.acquire_lease(false)?;
        self.local_generation = self.local_generation.saturating_add(1);
        self.dirty_since = Some(Instant::now() - PUBLISH_DEBOUNCE);
        self.status = SyncStatus::SavedLocally {
            message: "Initial publication queued".into(),
        };
        Ok(())
    }

    pub fn enable_existing(&mut self, parent: &Path, conn: &mut Connection) -> Result<()> {
        let root = sync_root_for(parent);
        let revision = validate_head(&root)?;
        archive_connection(conn, &self.paths.recovery, "before-sync-adoption")?;
        let snapshot = root.join("snapshots").join(&revision.snapshot_name);
        conn.restore(MAIN_DB, &snapshot, None::<fn(rusqlite::backup::Progress)>)
            .context("restoring synchronized budget")?;
        db::migrate(conn)?;
        crate::queries::apply_active_currency(conn)?;
        let migrated = revision.schema_version < db::SCHEMA_VERSION;
        self.observed_changes = conn.total_changes();
        self.config.mode = StorageMode::FolderSync;
        self.config.sync_parent = Some(parent.to_path_buf());
        self.config.last_accepted_revision = Some(revision.revision_id.clone());
        self.config.last_published_digest = Some(revision.sha256.clone());
        self.save_config()?;
        if let Err(error) = self.acquire_lease(false) {
            self.status = SyncStatus::ReadOnly {
                owner: error.to_string(),
            };
            return Ok(());
        }
        if migrated {
            self.local_generation = self.local_generation.saturating_add(1);
            self.dirty_since = Some(Instant::now() - PUBLISH_DEBOUNCE);
            self.status = SyncStatus::SavedLocally {
                message: "Database upgraded; publishing the new schema".into(),
            };
        } else {
            self.status = SyncStatus::Published {
                revision_id: revision.revision_id,
            };
        }
        Ok(())
    }

    pub fn replace_existing(&mut self, parent: &Path) -> Result<()> {
        let root = sync_root_for(parent);
        let previous = validate_head(&root)?;
        let existing_lease = read_optional_json::<Lease>(&root.join("lease.json"))?;
        let force_stale = match decide_lease(
            existing_lease.as_ref(),
            &self.device.device_id,
            &self.session_id,
            now_ms(),
        ) {
            LeaseDecision::ReadOnly { owner, .. } => {
                bail!("{owner} still has an active editing lease")
            }
            LeaseDecision::TakeoverRequired { .. } => true,
            LeaseDecision::Acquire | LeaseDecision::Refresh => false,
        };
        self.config.mode = StorageMode::FolderSync;
        self.config.sync_parent = Some(parent.to_path_buf());
        self.config.last_accepted_revision = Some(previous.revision_id.clone());
        self.config.last_published_digest = None;
        self.save_config()?;
        self.acquire_lease(force_stale)?;
        self.local_generation = self.local_generation.saturating_add(1);
        self.dirty_since = Some(Instant::now() - PUBLISH_DEBOUNCE);
        self.status = SyncStatus::SavedLocally {
            message: "Replacement publication queued".into(),
        };
        Ok(())
    }

    pub fn disable(&mut self) -> Result<()> {
        if self.config.mode == StorageMode::FolderSync {
            let _ = self.wait_for_publication();
            let _ = self.release_lease();
        }
        self.config = Config::default();
        self.save_config()?;
        self.status = SyncStatus::LocalOnly;
        self.publish_rx = None;
        Ok(())
    }

    pub fn takeover(&mut self) -> Result<()> {
        let root = self.sync_root().context("sync folder is not configured")?;
        let lease = read_optional_json::<Lease>(&root.join("lease.json"))?;
        match decide_lease(
            lease.as_ref(),
            &self.device.device_id,
            &self.session_id,
            now_ms(),
        ) {
            LeaseDecision::TakeoverRequired { .. } => self.acquire_lease(true),
            LeaseDecision::Acquire => self.acquire_lease(false),
            LeaseDecision::Refresh => self.refresh_lease(),
            LeaseDecision::ReadOnly { owner, .. } => {
                bail!("{owner} still has an active editing lease")
            }
        }
    }

    /// Resolve a divergent synchronized head without row-level merging. The local and
    /// synchronized candidates both become protected immutable revisions; a new two-parent
    /// resolution revision contains the selected database.
    pub fn resolve_conflict(&mut self, conn: &mut Connection, use_local: bool) -> Result<()> {
        let root = self.sync_root().context("sync folder is not configured")?;
        let remote = validate_head(&root)?;
        self.acquire_lease(false)?;

        let local_candidate = write_revision_snapshot(
            &self.paths.database,
            &root,
            &self.device,
            &self.session_id,
            self.config
                .last_accepted_revision
                .clone()
                .into_iter()
                .collect(),
            true,
        )?;
        protect_revision(&root, &local_candidate.revision_id)?;
        if use_local {
            protect_revision(&root, &remote.revision_id)?;
        }
        if !use_local {
            archive_connection(conn, &self.paths.recovery, "conflict-local-candidate")?;
            let remote_snapshot = root.join("snapshots").join(&remote.snapshot_name);
            conn.restore(
                MAIN_DB,
                &remote_snapshot,
                None::<fn(rusqlite::backup::Progress)>,
            )
            .context("restoring selected synchronized candidate")?;
            db::migrate(conn)?;
            crate::queries::apply_active_currency(conn)?;
            self.observed_changes = conn.total_changes();
        }

        verify_expected_head(&root, Some(&remote.revision_id))?;
        verify_owned_lease(&root, &self.device.device_id, &self.session_id)?;
        let resolution = write_revision_snapshot(
            &self.paths.database,
            &root,
            &self.device,
            &self.session_id,
            vec![remote.revision_id.clone(), local_candidate.revision_id],
            false,
        )?;
        atomic_json(
            &root.join("head.json"),
            &Head {
                revision_id: resolution.revision_id.clone(),
                updated_at_ms: now_ms(),
            },
        )?;
        validate_head(&root)?;
        self.config.last_accepted_revision = Some(resolution.revision_id.clone());
        self.config.last_published_digest = Some(resolution.sha256);
        self.save_config()?;
        self.local_generation = self.local_generation.saturating_add(1);
        self.published_generation = self.local_generation;
        self.dirty_since = None;
        self.status = SyncStatus::Published {
            revision_id: resolution.revision_id,
        };
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if self.config.mode != StorageMode::FolderSync {
            return Ok(());
        }
        self.wait_for_publication()?;
        if self.local_generation > self.published_generation && self.can_edit() {
            let request = self.publish_request(self.local_generation)?;
            match publish(request) {
                Ok(revision) => self.accept_publication(self.local_generation, revision)?,
                Err(error) => {
                    self.status = SyncStatus::SavedLocally {
                        message: error.to_string(),
                    };
                    return Err(error);
                }
            }
        }
        self.release_lease()
    }

    fn wait_for_publication(&mut self) -> Result<()> {
        let Some(rx) = self.publish_rx.take() else {
            return Ok(());
        };
        let message = rx
            .recv()
            .map_err(|_| anyhow::anyhow!("publication worker stopped unexpectedly"))?;
        match message.result {
            Ok(revision) => self.accept_publication(message.generation, revision),
            Err(message) => {
                self.status = SyncStatus::SavedLocally {
                    message: message.clone(),
                };
                bail!("{message}")
            }
        }
    }

    fn start_session(&mut self) -> Result<()> {
        if self.config.mode != StorageMode::FolderSync {
            self.status = SyncStatus::LocalOnly;
            return Ok(());
        }
        let Some(root) = self.sync_root() else {
            self.status = SyncStatus::Attention {
                message: "Sync configuration has no folder".into(),
            };
            return Ok(());
        };
        match validate_head(&root) {
            Ok(revision) => {
                if !provider_conflict_files(&root)?.is_empty() {
                    self.status = SyncStatus::Attention {
                        message: "Provider-created conflict files were found".into(),
                    };
                    return Ok(());
                }
                let divergent = divergent_revisions(&root)?;
                if !divergent.is_empty() {
                    self.status = SyncStatus::Attention {
                        message: format!(
                            "Found {} divergent revision candidate{}",
                            divergent.len(),
                            if divergent.len() == 1 { "" } else { "s" }
                        ),
                    };
                    return Ok(());
                }
                if let Some(local) = &self.config.last_accepted_revision
                    && local != &revision.revision_id
                {
                    self.status = SyncStatus::Attention {
                        message: format!(
                            "Synchronized revision {} differs from local base {}",
                            revision.revision_id, local
                        ),
                    };
                    return Ok(());
                }
                match self.acquire_lease(false) {
                    Ok(()) => {
                        self.status = SyncStatus::Published {
                            revision_id: revision.revision_id,
                        };
                    }
                    Err(error) => {
                        self.status = SyncStatus::ReadOnly {
                            owner: error.to_string(),
                        };
                    }
                }
            }
            Err(error) => {
                self.status = SyncStatus::Attention {
                    message: error.to_string(),
                };
            }
        }
        Ok(())
    }

    fn acquire_lease(&mut self, force: bool) -> Result<()> {
        let root = self.sync_root().context("sync folder is not configured")?;
        validate_sync_root(&root)?;
        let path = root.join("lease.json");
        let existing = read_optional_json::<Lease>(&path)?;
        match decide_lease(
            existing.as_ref(),
            &self.device.device_id,
            &self.session_id,
            now_ms(),
        ) {
            LeaseDecision::ReadOnly { owner, .. } if !force => bail!("{owner}"),
            LeaseDecision::TakeoverRequired { owner, .. } if !force => {
                bail!("stale lease from {owner}; confirm takeover")
            }
            _ => {}
        }
        let now = now_ms();
        let lease = Lease {
            device_id: self.device.device_id.clone(),
            device_label: self.device.label.clone(),
            session_id: self.session_id.clone(),
            base_revision: read_optional_json::<Head>(&root.join("head.json"))?
                .map(|head| head.revision_id),
            acquired_at_ms: now,
            heartbeat_at_ms: now,
            expires_at_ms: now + LEASE_DURATION.as_millis() as i64,
            released: false,
        };
        atomic_json(&path, &lease)?;
        let confirmed: Lease = read_json(&path)?;
        if !confirmed.owned_by(&self.device.device_id, &self.session_id) {
            bail!("lease changed while acquiring it");
        }
        self.status = self
            .config
            .last_accepted_revision
            .clone()
            .map(|revision_id| SyncStatus::Published { revision_id })
            .unwrap_or_else(|| SyncStatus::SavedLocally {
                message: "Ready for initial publication".into(),
            });
        Ok(())
    }

    fn refresh_lease(&mut self) -> Result<()> {
        let root = self.sync_root().context("sync folder is not configured")?;
        let path = root.join("lease.json");
        let mut lease: Lease = read_json(&path)?;
        if !lease.owned_by(&self.device.device_id, &self.session_id) {
            self.status = SyncStatus::Attention {
                message: "Editing lease changed unexpectedly".into(),
            };
            return Ok(());
        }
        let now = now_ms();
        lease.heartbeat_at_ms = now;
        lease.expires_at_ms = now + LEASE_DURATION.as_millis() as i64;
        atomic_json(&path, &lease)
    }

    fn release_lease(&mut self) -> Result<()> {
        let root = self.sync_root().context("sync folder is not configured")?;
        let path = root.join("lease.json");
        let Some(mut lease) = read_optional_json::<Lease>(&path)? else {
            return Ok(());
        };
        if lease.owned_by(&self.device.device_id, &self.session_id) {
            let now = now_ms();
            lease.released = true;
            lease.heartbeat_at_ms = now;
            lease.expires_at_ms = now;
            atomic_json(&path, &lease)?;
        }
        Ok(())
    }

    fn watch_remote(&mut self) -> Result<()> {
        let root = self.sync_root().context("sync folder is not configured")?;
        let was_read_only = matches!(self.status, SyncStatus::ReadOnly { .. });
        if !root.is_dir() {
            self.status = SyncStatus::Attention {
                message: "Sync folder is unavailable".into(),
            };
            return Ok(());
        }
        if !provider_conflict_files(&root)?.is_empty() {
            self.status = SyncStatus::Attention {
                message: "Provider-created conflict files were found".into(),
            };
            return Ok(());
        }
        let revision = validate_head(&root)?;
        let divergent = divergent_revisions(&root)?;
        if !divergent.is_empty() {
            self.status = SyncStatus::Attention {
                message: format!(
                    "Found {} divergent revision candidate{}",
                    divergent.len(),
                    if divergent.len() == 1 { "" } else { "s" }
                ),
            };
            return Ok(());
        }
        if self
            .config
            .last_accepted_revision
            .as_ref()
            .is_some_and(|id| id != &revision.revision_id)
        {
            self.status = SyncStatus::Attention {
                message: format!("Unexpected synchronized revision {}", revision.revision_id),
            };
            return Ok(());
        }
        let lease: Lease = read_json(&root.join("lease.json"))?;
        if !lease.owned_by(&self.device.device_id, &self.session_id) {
            if was_read_only {
                match decide_lease(
                    Some(&lease),
                    &self.device.device_id,
                    &self.session_id,
                    now_ms(),
                ) {
                    LeaseDecision::Acquire => self.acquire_lease(false)?,
                    LeaseDecision::ReadOnly { owner, .. } => {
                        self.status = SyncStatus::ReadOnly { owner };
                    }
                    LeaseDecision::TakeoverRequired { owner, .. } => {
                        self.status = SyncStatus::ReadOnly {
                            owner: format!("stale lease from {owner}; confirm takeover"),
                        };
                    }
                    LeaseDecision::Refresh => {}
                }
            } else {
                self.status = SyncStatus::Attention {
                    message: format!(
                        "Editing ownership changed to {}",
                        lease_owner_label(&lease, &self.device.device_id)
                    ),
                };
            }
        }
        Ok(())
    }

    fn spawn_publication(&mut self) -> Result<()> {
        let generation = self.local_generation;
        let request = self.publish_request(generation)?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = publish(request).map_err(|error| format!("{error:#}"));
            let _ = tx.send(PublishResult { generation, result });
        });
        self.publish_rx = Some(rx);
        self.status = SyncStatus::Publishing;
        Ok(())
    }

    fn publish_request(&self, generation: u64) -> Result<PublishRequest> {
        let root = self.sync_root().context("sync folder is not configured")?;
        let expected_parent = self.config.last_accepted_revision.clone();
        Ok(PublishRequest {
            database: self.paths.database.clone(),
            root,
            parents: expected_parent.clone().into_iter().collect(),
            expected_parent,
            device: self.device.clone(),
            session_id: self.session_id.clone(),
            generation,
        })
    }

    fn collect_publication(&mut self) -> Result<()> {
        let Some(rx) = self.publish_rx.as_ref() else {
            return Ok(());
        };
        let Ok(message) = rx.try_recv() else {
            return Ok(());
        };
        self.publish_rx = None;
        match message.result {
            Ok(revision) => self.accept_publication(message.generation, revision)?,
            Err(message) => {
                self.status = SyncStatus::SavedLocally { message };
            }
        }
        Ok(())
    }

    fn accept_publication(&mut self, generation: u64, revision: Revision) -> Result<()> {
        self.published_generation = self.published_generation.max(generation);
        self.config.last_accepted_revision = Some(revision.revision_id.clone());
        self.config.last_published_digest = Some(revision.sha256);
        self.save_config()?;
        self.status = SyncStatus::Published {
            revision_id: revision.revision_id,
        };
        if self.local_generation > generation {
            self.dirty_since = Some(Instant::now() - PUBLISH_DEBOUNCE);
        } else {
            self.dirty_since = None;
        }
        Ok(())
    }

    fn save_config(&self) -> Result<()> {
        atomic_json(&self.paths.config, &self.config)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.release_lease();
    }
}

pub fn inspect_parent(parent: &Path) -> Result<Inspection> {
    let expanded = expand_home(parent)?;
    validate_parent(&expanded)?;
    let root = sync_root_for(&expanded);
    if !root.join("sync-root.json").exists() {
        return Ok(Inspection::Empty { root });
    }
    let revision = validate_head(&root)?;
    let divergent = divergent_revisions(&root)?;
    if !divergent.is_empty() {
        bail!(
            "the synchronized budget has {} unresolved divergent revision(s)",
            divergent.len()
        );
    }
    Ok(Inspection::Existing {
        root,
        revision: Box::new(revision),
    })
}

pub fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .context("home directory is unavailable")?;
        let rest = text
            .strip_prefix("~/")
            .or_else(|| text.strip_prefix("~\\"))
            .unwrap_or("");
        return Ok(PathBuf::from(home).join(rest));
    }
    Ok(path.to_path_buf())
}

pub fn validate_parent(parent: &Path) -> Result<()> {
    if !parent.is_dir() {
        bail!("{} is not a directory", parent.display());
    }
    let probe = parent.join(format!(".leeway-write-test-{}", Uuid::new_v4()));
    let result = OpenOptions::new().write(true).create_new(true).open(&probe);
    match result {
        Ok(mut file) => {
            file.write_all(b"ok")?;
            file.sync_all()?;
            fs::remove_file(&probe)?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("writing to {}", parent.display())),
    }
}

pub fn decide_lease(
    lease: Option<&Lease>,
    device_id: &str,
    session_id: &str,
    now_ms: i64,
) -> LeaseDecision {
    let Some(lease) = lease else {
        return LeaseDecision::Acquire;
    };
    if lease.owned_by(device_id, session_id) {
        return LeaseDecision::Refresh;
    }
    if lease.released {
        return LeaseDecision::Acquire;
    }
    let owner = lease_owner_label(lease, device_id);
    if lease.active_at(now_ms) {
        return LeaseDecision::ReadOnly {
            owner,
            expires_at_ms: lease.expires_at_ms,
        };
    }
    LeaseDecision::TakeoverRequired {
        owner,
        expires_at_ms: lease.expires_at_ms,
    }
}

fn lease_owner_label(lease: &Lease, local_device_id: &str) -> String {
    if lease.device_id == local_device_id {
        SAME_DEVICE_OWNER.into()
    } else if lease.device_label.trim().is_empty() || lease.device_label == GENERIC_DEVICE_LABEL {
        UNNAMED_DEVICE_LABEL.into()
    } else {
        lease.device_label.clone()
    }
}

pub fn validate_head(root: &Path) -> Result<Revision> {
    validate_sync_root(root)?;
    let head: Head = read_json(&root.join("head.json")).context("reading synchronized head")?;
    let revision_path = root
        .join("revisions")
        .join(format!("{}.json", head.revision_id));
    let revision: Revision = read_json(&revision_path).context("reading revision descriptor")?;
    if revision.revision_id != head.revision_id {
        bail!("revision descriptor does not match head");
    }
    validate_revision(root, &revision)?;
    Ok(revision)
}

pub fn validate_revision(root: &Path, revision: &Revision) -> Result<()> {
    if revision.schema_version > db::SCHEMA_VERSION {
        bail!(
            "this budget uses schema {}; Leeway {} supports up to {}",
            revision.schema_version,
            env!("CARGO_PKG_VERSION"),
            db::SCHEMA_VERSION
        );
    }
    let snapshot = root.join("snapshots").join(&revision.snapshot_name);
    let metadata = fs::metadata(&snapshot)
        .with_context(|| format!("reading snapshot {}", snapshot.display()))?;
    if metadata.len() != revision.byte_length {
        bail!("snapshot length does not match its revision descriptor");
    }
    if sha256_file(&snapshot)? != revision.sha256 {
        bail!("snapshot digest does not match its revision descriptor");
    }
    verify_sqlite(&snapshot, revision.schema_version)
}

pub fn provider_conflict_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0_u8)];
    while let Some((directory, depth)) = pending.pop() {
        for entry in
            fs::read_dir(&directory).with_context(|| format!("scanning {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() && !file_type.is_symlink() && depth < 2 {
                pending.push((path, depth + 1));
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if (lower.contains("conflict") || lower.contains("conflicted"))
                && (lower.ends_with(".json") || lower.ends_with(".db"))
            {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// Find published branches that are not incorporated into the current head's ancestry.
/// Once a two-parent resolution revision is head, both former branches are ancestors and
/// no longer count as divergent.
pub fn divergent_revisions(root: &Path) -> Result<Vec<Revision>> {
    let head: Head = read_json(&root.join("head.json"))?;
    let mut revisions = HashMap::new();
    for entry in fs::read_dir(root.join("revisions"))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let revision: Revision = read_json(&path)?;
        revisions.insert(revision.revision_id.clone(), revision);
    }

    let mut ancestors = HashSet::new();
    let mut pending = vec![head.revision_id];
    while let Some(id) = pending.pop() {
        if !ancestors.insert(id.clone()) {
            continue;
        }
        if let Some(revision) = revisions.get(&id) {
            pending.extend(revision.parents.iter().cloned());
        }
    }

    Ok(revisions
        .into_values()
        .filter(|revision| !ancestors.contains(&revision.revision_id))
        .filter(|revision| {
            revision
                .parents
                .iter()
                .any(|parent| ancestors.contains(parent))
        })
        .collect())
}

pub fn archive_connection(conn: &Connection, recovery: &Path, reason: &str) -> Result<PathBuf> {
    fs::create_dir_all(recovery)?;
    let path = recovery.join(format!("{}-{}-{}.db", now_ms(), reason, Uuid::new_v4()));
    conn.backup(MAIN_DB, &path, None)
        .context("creating recovery backup")?;
    Ok(path)
}

pub fn import_legacy(conn: &mut Connection, legacy: &Path, recovery: &Path) -> Result<()> {
    verify_sqlite(legacy, 0).context("validating legacy database")?;
    archive_connection(conn, recovery, "before-legacy-import")?;
    conn.restore(MAIN_DB, legacy, None::<fn(rusqlite::backup::Progress)>)
        .context("importing legacy database")?;
    db::migrate(conn)?;
    crate::queries::apply_active_currency(conn)
}

fn publish(request: PublishRequest) -> Result<Revision> {
    validate_sync_root(&request.root)?;
    verify_expected_head(&request.root, request.expected_parent.as_deref())?;
    verify_owned_lease(
        &request.root,
        &request.device.device_id,
        &request.session_id,
    )?;

    let revision = write_revision_snapshot(
        &request.database,
        &request.root,
        &request.device,
        &request.session_id,
        request.parents,
        false,
    )?;

    verify_expected_head(&request.root, request.expected_parent.as_deref())?;
    verify_owned_lease(&request.root, &revision.device_id, &request.session_id)?;
    atomic_json(
        &request.root.join("head.json"),
        &Head {
            revision_id: revision.revision_id.clone(),
            updated_at_ms: now_ms(),
        },
    )?;
    let accepted = validate_head(&request.root)?;
    if accepted.revision_id != revision.revision_id {
        bail!("head changed while confirming publication");
    }
    prune_history(&request.root, ORDINARY_RETENTION)?;
    let _ = request.generation;
    Ok(revision)
}

fn write_revision_snapshot(
    database: &Path,
    root: &Path,
    device: &Device,
    session_id: &str,
    parents: Vec<String>,
    protected: bool,
) -> Result<Revision> {
    let revision_id = format!("{}-{}", now_ms(), Uuid::new_v4());
    let snapshot_name = format!("{revision_id}.db");
    let temp = root.join("snapshots").join(format!(".{snapshot_name}.tmp"));
    let final_snapshot = root.join("snapshots").join(&snapshot_name);
    let source = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} for publication", database.display()))?;
    source
        .backup(MAIN_DB, &temp, None)
        .context("creating SQLite snapshot")?;
    sync_file(&temp)?;
    verify_sqlite(&temp, db::SCHEMA_VERSION)?;
    let byte_length = fs::metadata(&temp)?.len();
    let sha256 = sha256_file(&temp)?;
    fs::rename(&temp, &final_snapshot).context("installing immutable snapshot")?;
    sync_parent(&final_snapshot)?;
    let revision = Revision {
        revision_id: revision_id.clone(),
        parents,
        device_id: device.device_id.clone(),
        device_label: device.label.clone(),
        session_id: session_id.into(),
        published_at_ms: now_ms(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        schema_version: db::SCHEMA_VERSION,
        snapshot_name,
        byte_length,
        sha256,
        protected,
    };
    atomic_json(
        &root.join("revisions").join(format!("{revision_id}.json")),
        &revision,
    )?;
    Ok(revision)
}

fn prune_history(root: &Path, retain: usize) -> Result<()> {
    let current = validate_head(root)?.revision_id;
    let mut revisions = Vec::new();
    for entry in fs::read_dir(root.join("revisions"))? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Ok(revision) = read_json::<Revision>(&path) {
            revisions.push((path, revision));
        }
    }
    revisions.sort_by_key(|(_, revision)| std::cmp::Reverse(revision.published_at_ms));
    for (index, (descriptor, revision)) in revisions.into_iter().enumerate() {
        if index < retain
            || revision.protected
            || root
                .join("protected")
                .join(format!("{}.keep", revision.revision_id))
                .exists()
            || revision.revision_id == current
        {
            continue;
        }
        let _ = fs::remove_file(root.join("snapshots").join(&revision.snapshot_name));
        let _ = fs::remove_file(descriptor);
    }
    Ok(())
}

fn validate_sync_root(root: &Path) -> Result<SyncRoot> {
    let sync_root: SyncRoot = read_json(&root.join("sync-root.json"))
        .with_context(|| format!("reading sync root at {}", root.display()))?;
    if sync_root.protocol_version != PROTOCOL_VERSION {
        bail!(
            "unsupported sync protocol {} (this build supports {})",
            sync_root.protocol_version,
            PROTOCOL_VERSION
        );
    }
    Ok(sync_root)
}

fn verify_expected_head(root: &Path, expected: Option<&str>) -> Result<()> {
    let actual = read_optional_json::<Head>(&root.join("head.json"))?;
    match (expected, actual.as_ref()) {
        (None, None) => Ok(()),
        (Some(expected), Some(actual)) if expected == actual.revision_id => Ok(()),
        _ => bail!("synchronized head changed from the expected parent"),
    }
}

fn verify_owned_lease(root: &Path, device_id: &str, session_id: &str) -> Result<()> {
    let lease: Lease = read_json(&root.join("lease.json"))?;
    if !lease.owned_by(device_id, session_id) || !lease.active_at(now_ms()) {
        bail!("this session no longer owns the editing lease");
    }
    Ok(())
}

fn create_sync_layout(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join("revisions"))?;
    fs::create_dir_all(root.join("snapshots"))?;
    fs::create_dir_all(root.join("protected"))?;
    Ok(())
}

fn protect_revision(root: &Path, revision_id: &str) -> Result<()> {
    atomic_write(
        &root.join("protected").join(format!("{revision_id}.keep")),
        b"protected by conflict resolution\n",
    )
}

fn sync_root_for(parent: &Path) -> PathBuf {
    parent.join(SYNC_DIR_NAME)
}

fn verify_sqlite(path: &Path, expected_schema: u32) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening snapshot {}", path.display()))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("SQLite integrity check failed: {integrity}");
    }
    let schema: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if expected_schema > 0 && schema != expected_schema {
        bail!("snapshot schema is {schema}, expected {expected_schema}");
    }
    if schema > db::SCHEMA_VERSION {
        bail!("snapshot schema {schema} is newer than this application");
    }
    Ok(())
}

fn load_or_create_device(path: &Path) -> Result<Device> {
    if path.exists() {
        let mut device: Device = read_json(path)?;
        if device.label.trim().is_empty()
            || device.label == GENERIC_DEVICE_LABEL
            || device.label == UNNAMED_DEVICE_LABEL
        {
            let resolved = system_device_label();
            if resolved != device.label {
                device.label = resolved;
                atomic_json(path, &device)?;
            }
        }
        return Ok(device);
    }
    let device = Device {
        device_id: Uuid::new_v4().to_string(),
        label: system_device_label(),
    };
    atomic_json(path, &device)?;
    Ok(device)
}

fn system_device_label() -> String {
    normalize_device_label(hostname::get().ok())
}

fn normalize_device_label(name: Option<std::ffi::OsString>) -> String {
    name.and_then(|name| name.into_string().ok())
        .map(|name| name.trim().trim_end_matches(".local").to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| UNNAMED_DEVICE_LABEL.into())
}

fn load_or_default<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned + Serialize + Default,
{
    if path.exists() {
        read_json(path)
    } else {
        let value = T::default();
        atomic_json(path, &value)?;
        Ok(value)
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn read_optional_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    read_json(path).map(Some)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        Uuid::new_v4()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    replace_atomically(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    sync_parent(path)
}

#[cfg(not(windows))]
fn replace_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sync_file(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn connection_digest(conn: &Connection, directory: &Path) -> Result<String> {
    let path = directory.join(format!(".digest-{}.db", Uuid::new_v4()));
    conn.backup(MAIN_DB, &path, None)?;
    let result = sha256_file(&path);
    let _ = fs::remove_file(path);
    result
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("leeway-sync-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_paths(name: &str) -> AppPaths {
        AppPaths::in_dir(temp_dir(name).join("data"))
    }

    fn seeded(paths: &AppPaths) -> Connection {
        paths.create().unwrap();
        db::open(&paths.database).unwrap()
    }

    #[test]
    fn expands_home_prefix_only() {
        let expanded = expand_home(Path::new("~/Budget Sync")).unwrap();
        assert!(expanded.ends_with("Budget Sync"));
        assert_eq!(
            expand_home(Path::new("~someone/Budget")).unwrap(),
            PathBuf::from("~someone/Budget")
        );
    }

    #[test]
    fn lease_decisions_fail_closed() {
        let lease = Lease {
            device_id: "a".into(),
            device_label: "Laptop".into(),
            session_id: "one".into(),
            base_revision: None,
            acquired_at_ms: 1,
            heartbeat_at_ms: 10,
            expires_at_ms: 100,
            released: false,
        };
        assert!(matches!(
            decide_lease(None, "b", "two", 20),
            LeaseDecision::Acquire
        ));
        assert!(matches!(
            decide_lease(Some(&lease), "a", "one", 20),
            LeaseDecision::Refresh
        ));
        assert!(matches!(
            decide_lease(Some(&lease), "b", "two", 20),
            LeaseDecision::ReadOnly { .. }
        ));
        assert_eq!(
            decide_lease(Some(&lease), "a", "two", 20),
            LeaseDecision::ReadOnly {
                owner: SAME_DEVICE_OWNER.into(),
                expires_at_ms: 100,
            }
        );
        assert!(matches!(
            decide_lease(Some(&lease), "b", "two", 101),
            LeaseDecision::TakeoverRequired { .. }
        ));
    }

    #[test]
    fn device_labels_use_hostname_and_safe_fallbacks() {
        assert_eq!(
            normalize_device_label(Some("Nathans-MacBook-Pro.local".into())),
            "Nathans-MacBook-Pro"
        );
        assert_eq!(
            normalize_device_label(Some("  ".into())),
            UNNAMED_DEVICE_LABEL
        );
        assert_eq!(normalize_device_label(None), UNNAMED_DEVICE_LABEL);
    }

    #[test]
    fn generic_device_label_is_upgraded_without_changing_identity() {
        let path = temp_dir("device-upgrade").join("device.json");
        atomic_json(
            &path,
            &Device {
                device_id: "stable-id".into(),
                label: GENERIC_DEVICE_LABEL.into(),
            },
        )
        .unwrap();

        let device = load_or_create_device(&path).unwrap();
        assert_eq!(device.device_id, "stable-id");
        assert_ne!(device.label, GENERIC_DEVICE_LABEL);
        assert_eq!(read_json::<Device>(&path).unwrap(), device);
    }

    #[test]
    fn generic_label_from_a_different_identity_is_not_called_this_computer() {
        let lease = Lease {
            device_id: "other".into(),
            device_label: GENERIC_DEVICE_LABEL.into(),
            session_id: "session".into(),
            base_revision: None,
            acquired_at_ms: 1,
            heartbeat_at_ms: 1,
            expires_at_ms: 100,
            released: false,
        };
        assert_eq!(
            decide_lease(Some(&lease), "local", "new-session", 20),
            LeaseDecision::ReadOnly {
                owner: UNNAMED_DEVICE_LABEL.into(),
                expires_at_ms: 100,
            }
        );
    }

    #[test]
    fn new_sync_publishes_a_valid_snapshot() {
        let paths = test_paths("publish");
        let conn = seeded(&paths);
        conn.execute("INSERT INTO plan (id, name) VALUES ('p', 'Plan')", [])
            .unwrap();
        let parent = temp_dir("remote");
        let mut runtime = Runtime::load(paths.clone(), &conn).unwrap();
        runtime.enable_new(&parent).unwrap();
        let request = runtime.publish_request(1).unwrap();
        let revision = publish(request).unwrap();
        runtime.accept_publication(1, revision.clone()).unwrap();

        let accepted = validate_head(&parent.join(SYNC_DIR_NAME)).unwrap();
        assert_eq!(accepted.revision_id, revision.revision_id);
        assert_eq!(accepted.schema_version, db::SCHEMA_VERSION);
        assert_eq!(accepted.parents, Vec::<String>::new());
    }

    #[test]
    fn shutdown_waits_for_background_publication_then_releases_lease() {
        let paths = test_paths("shutdown");
        let conn = seeded(&paths);
        let parent = temp_dir("shutdown-remote");
        let mut runtime = Runtime::load(paths, &conn).unwrap();
        runtime.enable_new(&parent).unwrap();
        runtime.publish_now().unwrap();
        runtime.shutdown().unwrap();

        let root = parent.join(SYNC_DIR_NAME);
        validate_head(&root).unwrap();
        let lease: Lease = read_json(&root.join("lease.json")).unwrap();
        assert!(lease.released);
    }

    #[test]
    fn launch_recovers_unobserved_local_changes_after_a_crash() {
        let paths = test_paths("crash-recovery");
        let conn = seeded(&paths);
        let parent = temp_dir("crash-recovery-remote");
        let mut first_runtime = Runtime::load(paths.clone(), &conn).unwrap();
        first_runtime.enable_new(&parent).unwrap();
        let first = publish(first_runtime.publish_request(1).unwrap()).unwrap();
        first_runtime.accept_publication(1, first.clone()).unwrap();
        first_runtime.release_lease().unwrap();

        conn.execute(
            "INSERT INTO plan (id, name) VALUES ('after', 'Crash-safe')",
            [],
        )
        .unwrap();
        drop(first_runtime);
        drop(conn);

        let mut reopened = db::open(&paths.database).unwrap();
        let mut runtime = Runtime::load(paths, &reopened).unwrap();
        runtime.reconcile_on_launch(&mut reopened).unwrap();
        assert!(matches!(runtime.status, SyncStatus::SavedLocally { .. }));
        runtime.shutdown().unwrap();
        let recovered = validate_head(&parent.join(SYNC_DIR_NAME)).unwrap();
        assert_ne!(recovered.revision_id, first.revision_id);
    }

    #[test]
    fn changed_head_prevents_publication() {
        let paths = test_paths("diverge");
        let conn = seeded(&paths);
        let parent = temp_dir("diverge-remote");
        let mut runtime = Runtime::load(paths, &conn).unwrap();
        runtime.enable_new(&parent).unwrap();
        let first = publish(runtime.publish_request(1).unwrap()).unwrap();
        runtime.accept_publication(1, first).unwrap();
        let request = runtime.publish_request(2).unwrap();
        atomic_json(
            &parent.join(SYNC_DIR_NAME).join("head.json"),
            &Head {
                revision_id: "other".into(),
                updated_at_ms: now_ms(),
            },
        )
        .unwrap();
        let error = publish(request).unwrap_err().to_string();
        assert!(error.contains("expected parent"));
    }

    #[test]
    fn corrupt_snapshot_is_rejected() {
        let paths = test_paths("corrupt");
        let conn = seeded(&paths);
        let parent = temp_dir("corrupt-remote");
        let mut runtime = Runtime::load(paths, &conn).unwrap();
        runtime.enable_new(&parent).unwrap();
        let revision = publish(runtime.publish_request(1).unwrap()).unwrap();
        fs::write(
            parent
                .join(SYNC_DIR_NAME)
                .join("snapshots")
                .join(&revision.snapshot_name),
            b"not sqlite",
        )
        .unwrap();
        assert!(validate_head(&parent.join(SYNC_DIR_NAME)).is_err());
    }

    #[test]
    fn adopting_existing_budget_archives_local_database() {
        let source_paths = test_paths("source");
        let source = seeded(&source_paths);
        source
            .execute(
                "INSERT INTO plan (id, name) VALUES ('remote', 'Remote')",
                [],
            )
            .unwrap();
        let parent = temp_dir("adopt-remote");
        let mut source_runtime = Runtime::load(source_paths, &source).unwrap();
        source_runtime.enable_new(&parent).unwrap();
        let revision = publish(source_runtime.publish_request(1).unwrap()).unwrap();
        source_runtime.accept_publication(1, revision).unwrap();
        source_runtime.release_lease().unwrap();

        let target_paths = test_paths("target");
        let mut target = seeded(&target_paths);
        target
            .execute("INSERT INTO plan (id, name) VALUES ('local', 'Local')", [])
            .unwrap();
        let mut target_runtime = Runtime::load(target_paths.clone(), &target).unwrap();
        target_runtime
            .enable_existing(&parent, &mut target)
            .unwrap();
        let count: i64 = target
            .query_row("SELECT COUNT(*) FROM plan WHERE id = 'remote'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert!(
            fs::read_dir(target_paths.recovery)
                .unwrap()
                .next()
                .is_some()
        );
    }

    #[test]
    fn read_only_device_retries_after_clean_handoff() {
        let first_paths = test_paths("handoff-first");
        let first_conn = seeded(&first_paths);
        let parent = temp_dir("handoff-remote");
        let mut first = Runtime::load(first_paths, &first_conn).unwrap();
        first.enable_new(&parent).unwrap();
        let revision = publish(first.publish_request(1).unwrap()).unwrap();
        first.accept_publication(1, revision).unwrap();

        let second_paths = test_paths("handoff-second");
        let mut second_conn = seeded(&second_paths);
        let mut second = Runtime::load(second_paths, &second_conn).unwrap();
        second.enable_existing(&parent, &mut second_conn).unwrap();
        assert!(matches!(second.status, SyncStatus::ReadOnly { .. }));
        assert!(
            second.takeover().is_err(),
            "an active lease cannot be taken over"
        );

        first.release_lease().unwrap();
        second.watch_remote().unwrap();
        assert!(second.can_edit());
        assert!(matches!(second.status, SyncStatus::Published { .. }));
    }

    #[test]
    fn conflict_resolution_keeps_two_parents_and_protects_local_candidate() {
        let paths = test_paths("resolution");
        let mut conn = seeded(&paths);
        conn.execute("INSERT INTO plan (id, name) VALUES ('local', 'Local')", [])
            .unwrap();
        let parent = temp_dir("resolution-remote");
        let mut runtime = Runtime::load(paths, &conn).unwrap();
        runtime.enable_new(&parent).unwrap();
        let first = publish(runtime.publish_request(1).unwrap()).unwrap();
        runtime.accept_publication(1, first.clone()).unwrap();

        conn.execute("UPDATE plan SET name = 'Local edit' WHERE id = 'local'", [])
            .unwrap();
        let root = parent.join(SYNC_DIR_NAME);
        let remote = Revision {
            revision_id: "remote-divergence".into(),
            parents: vec![first.revision_id],
            device_id: "other".into(),
            device_label: "Other computer".into(),
            session_id: "other-session".into(),
            published_at_ms: now_ms(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            schema_version: db::SCHEMA_VERSION,
            snapshot_name: first.snapshot_name.clone(),
            byte_length: first.byte_length,
            sha256: first.sha256,
            protected: true,
        };
        atomic_json(
            &root.join("revisions").join("remote-divergence.json"),
            &remote,
        )
        .unwrap();
        atomic_json(
            &root.join("head.json"),
            &Head {
                revision_id: remote.revision_id.clone(),
                updated_at_ms: now_ms(),
            },
        )
        .unwrap();

        assert_eq!(divergent_revisions(&root).unwrap().len(), 0);

        runtime.resolve_conflict(&mut conn, true).unwrap();
        let resolution = validate_head(&root).unwrap();
        assert_eq!(resolution.parents.len(), 2);
        assert_eq!(resolution.parents[0], remote.revision_id);
        let local: Revision = read_json(
            &root
                .join("revisions")
                .join(format!("{}.json", resolution.parents[1])),
        )
        .unwrap();
        assert!(local.protected);
        assert!(
            root.join("protected")
                .join("remote-divergence.keep")
                .exists()
        );
        assert!(divergent_revisions(&root).unwrap().is_empty());
    }
}
