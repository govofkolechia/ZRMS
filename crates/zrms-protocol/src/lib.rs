//! Versioned message types shared by the controller and remote agent.
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zrms_core::{Device, DeviceId};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_COMMAND_TIMEOUT_MS: u64 = 30_000;
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

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

/// A program and argument vector. Arguments are passed directly; no shell is involved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteCommand {
    pub request_id: Uuid,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional remote working directory for this process.
    #[serde(default)]
    pub working_directory: Option<String>,
    pub timeout_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub request_id: Uuid,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
}

/// Local controller IPC request. The Unix socket is restricted to its owning user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub device_id: DeviceId,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional remote working directory for this process.
    #[serde(default)]
    pub working_directory: Option<String>,
    pub timeout_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub result: Option<CommandResult>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessage {
    Hello(AgentHello),
    Heartbeat(Heartbeat),
    CommandResult(CommandResult),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControllerMessage {
    Hello(ControllerHello),
    Ping { sequence: u64 },
    Execute(ExecuteCommand),
}
