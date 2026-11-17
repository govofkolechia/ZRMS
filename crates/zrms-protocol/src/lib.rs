//! Versioned message types shared by the controller and remote agent.
//! Transport and authentication are implemented in later milestones.
use serde::{Deserialize, Serialize};
use zrms_core::{Device, DeviceId};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHello {
    pub protocol_version: u32,
    pub device: Device,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerHello {
    pub protocol_version: u32,
    pub accepted: bool,
    pub message: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub device_id: DeviceId,
    pub sequence: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessage {
    Hello(AgentHello),
    Heartbeat(Heartbeat),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControllerMessage {
    Hello(ControllerHello),
    Ping { sequence: u64 },
}
