//! SQLite-backed persistent device registry for the ZRMS controller.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use zrms_core::{Device, DeviceId, DeviceStatus};

#[derive(Debug, Clone)]
pub struct DeviceRecord {
    pub device: Device,
    pub last_seen: i64,
}

#[derive(Debug, Clone)]
pub struct CommandAuditRecord {
    pub request_id: String,
    pub device_id: DeviceId,
    pub program: String,
    pub args_json: String,
    pub initiator_uid: u32,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub timed_out: bool,
    pub outcome: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create database directory {}", parent.display()))?;
            }
        }

        let database = Self { path };
        let connection = database.connect()?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS devices (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                hostname TEXT NOT NULL,
                platform TEXT NOT NULL,
                agent_version TEXT NOT NULL,
                status TEXT NOT NULL,
                last_seen INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_devices_status ON devices(status);
             CREATE TABLE IF NOT EXISTS command_history (
                request_id TEXT PRIMARY KEY NOT NULL,
                device_id TEXT NOT NULL,
                program TEXT NOT NULL,
                args_json TEXT NOT NULL,
                initiator_uid INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                exit_code INTEGER,
                duration_ms INTEGER,
                timed_out INTEGER NOT NULL DEFAULT 0,
                outcome TEXT NOT NULL,
                error TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_command_history_started
                ON command_history(started_at DESC);
             CREATE INDEX IF NOT EXISTS idx_command_history_device
                ON command_history(device_id, started_at DESC);
             PRAGMA user_version = 2;",
        )?;
        Ok(database)
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("open database {}", self.path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(connection)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn register_device(&self, device: &Device) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "INSERT INTO devices (id, name, hostname, platform, agent_version, status, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 'online', ?6)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                hostname = excluded.hostname,
                platform = excluded.platform,
                agent_version = excluded.agent_version,
                status = 'online',
                last_seen = excluded.last_seen",
            params![
                device.id.0.to_string(),
                device.name,
                device.hostname,
                device.platform,
                device.agent_version,
                now_unix(),
            ],
        )?;
        Ok(())
    }

    pub fn record_heartbeat(&self, id: DeviceId) -> Result<bool> {
        let connection = self.connect()?;
        let changed = connection.execute(
            "UPDATE devices SET status = 'online', last_seen = ?1 WHERE id = ?2",
            params![now_unix(), id.0.to_string()],
        )?;
        Ok(changed == 1)
    }

    pub fn mark_offline(&self, id: DeviceId) -> Result<bool> {
        let connection = self.connect()?;
        let changed = connection.execute(
            "UPDATE devices SET status = 'offline' WHERE id = ?1",
            params![id.0.to_string()],
        )?;
        Ok(changed == 1)
    }

    /// A controller restart invalidates all previously live transport sessions.
    pub fn mark_all_offline(&self) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE devices SET status = 'offline' WHERE status = 'online'",
            [],
        )?;
        Ok(())
    }

    pub fn list_devices(&self) -> Result<Vec<DeviceRecord>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT id, name, hostname, platform, agent_version, status, last_seen
             FROM devices ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;

        let mut devices = Vec::new();
        for row in rows {
            let (id, name, hostname, platform, agent_version, status, last_seen) = row?;
            let id = uuid::Uuid::parse_str(&id).context("invalid device ID in database")?;
            let status = match status.as_str() {
                "online" => DeviceStatus::Online,
                "offline" => DeviceStatus::Offline,
                "busy" => DeviceStatus::Busy,
                _ => DeviceStatus::Unknown,
            };
            devices.push(DeviceRecord {
                device: Device {
                    id: DeviceId(id),
                    name,
                    hostname,
                    platform,
                    status,
                    agent_version,
                },
                last_seen,
            });
        }
        Ok(devices)
    }

    pub fn counts(&self) -> Result<(u64, u64)> {
        let connection = self.connect()?;
        let total: i64 =
            connection.query_row("SELECT COUNT(*) FROM devices", [], |row| row.get(0))?;
        let online: i64 = connection.query_row(
            "SELECT COUNT(*) FROM devices WHERE status = 'online'",
            [],
            |row| row.get(0),
        )?;
        Ok((total as u64, online as u64))
    }

    pub fn record_command_started(
        &self,
        request_id: uuid::Uuid,
        device_id: DeviceId,
        program: &str,
        args: &[String],
        initiator_uid: u32,
    ) -> Result<()> {
        let connection = self.connect()?;
        let args_json = serde_json::to_string(args)?;
        connection.execute(
            "INSERT INTO command_history
             (request_id, device_id, program, args_json, initiator_uid, started_at, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running')",
            params![
                request_id.to_string(),
                device_id.0.to_string(),
                program,
                args_json,
                initiator_uid,
                now_unix_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn complete_command(
        &self,
        request_id: uuid::Uuid,
        outcome: &str,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
        timed_out: bool,
        error: Option<&str>,
    ) -> Result<bool> {
        let connection = self.connect()?;
        let changed = connection.execute(
            "UPDATE command_history
             SET completed_at = ?1, exit_code = ?2, duration_ms = ?3,
                 timed_out = ?4, outcome = ?5, error = ?6
             WHERE request_id = ?7",
            params![
                now_unix_ms(),
                exit_code,
                duration_ms.map(|value| value as i64),
                timed_out,
                outcome,
                error,
                request_id.to_string(),
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn list_command_history(&self, limit: u32) -> Result<Vec<CommandAuditRecord>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT request_id, device_id, program, args_json, initiator_uid,
                    started_at, completed_at, exit_code, duration_ms, timed_out,
                    outcome, error
             FROM command_history
             ORDER BY started_at DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.clamp(1, 500)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i32>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, bool>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (request_id, device_id, program, args_json, initiator_uid,
                started_at, completed_at, exit_code, duration_ms, timed_out,
                outcome, error) = row?;
            records.push(CommandAuditRecord {
                request_id,
                device_id: DeviceId(uuid::Uuid::parse_str(&device_id)
                    .context("invalid device ID in command history")?),
                program,
                args_json,
                initiator_uid,
                started_at,
                completed_at,
                exit_code,
                duration_ms: duration_ms.map(|value| value as u64),
                timed_out,
                outcome,
                error,
            });
        }
        Ok(records)
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
