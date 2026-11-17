//! SQLite-backed persistent device registry for the ZRMS controller.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use zrms_core::{Device, DeviceId, DeviceStatus};

#[derive(Debug, Clone)]
pub struct DeviceRecord {
    pub device: Device,
    pub last_seen: i64,
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
             PRAGMA user_version = 1;",
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
        connection.execute("UPDATE devices SET status = 'offline' WHERE status = 'online'", [])?;
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
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
