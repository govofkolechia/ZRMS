//! Shared domain types for the ZRMS controller and agent.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier assigned to a managed device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub Uuid);

impl DeviceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for DeviceId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Online,
    Offline,
    Busy,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub status: DeviceStatus,
    pub agent_version: String,
}

impl Device {
    pub fn new(
        name: impl Into<String>,
        hostname: impl Into<String>,
        platform: impl Into<String>,
    ) -> Self {
        Self {
            id: DeviceId::new(),
            name: name.into(),
            hostname: hostname.into(),
            platform: platform.into(),
            status: DeviceStatus::Unknown,
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}
