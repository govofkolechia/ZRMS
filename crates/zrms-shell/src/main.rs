use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use zrms_core::DeviceStatus;
use zrms_protocol::{ControlRequest, ControlResponse, MAX_COMMAND_TIMEOUT_MS};
use zrms_storage::Database;

#[derive(Debug, Parser)]
#[command(name = "zrms", version, about = "Zyfin Remote Management System")]
struct Cli {
    /// Path to the controller's SQLite database.
    #[arg(long, global = true, default_value = "zrms.db")]
    database: PathBuf,
    /// Path to the local controller control socket.
    #[arg(long, global = true, default_value = "zrms.sock")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List devices registered with this controller.
    Devices,
    /// Show controller and fleet status.
    Status,
    /// Execute a program on a connected device (no shell expansion).
    Exec {
        /// UUID of the target device.
        device_id: String,
        /// Program followed by its arguments, after --.
        #[arg(required = true, trailing_var_arg = true, num_args = 1..)]
        command: Vec<String>,
        /// Maximum execution time in milliseconds (max 30000).
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
    },
}

fn age_label(timestamp: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let seconds = now.saturating_sub(timestamp);
    if seconds < 5 { "just now".to_owned() }
    else if seconds < 60 { format!("{seconds}s ago") }
    else if seconds < 3_600 { format!("{}m ago", seconds / 60) }
    else if seconds < 86_400 { format!("{}h ago", seconds / 3_600) }
    else { format!("{}d ago", seconds / 86_400) }
}

async fn execute_remote(socket_path: &PathBuf, device_id: &str, command: &[String], timeout_ms: u64) -> Result<()> {
    let id = uuid::Uuid::parse_str(device_id).context("device ID must be a UUID")?;
    let (program, args) = command.split_first().context("missing program to execute")?;
    let socket = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to controller socket {}", socket_path.display()))?;
    let (read_half, mut write_half) = tokio::io::split(socket);
    let request = ControlRequest {
        device_id: zrms_core::DeviceId(id),
        program: program.clone(),
        args: args.to_vec(),
        timeout_ms: timeout_ms.clamp(1, MAX_COMMAND_TIMEOUT_MS),
    };
    let mut frame = serde_json::to_vec(&request)?;
    frame.push(b'\n');
    write_half.write_all(&frame).await?;
    drop(write_half);
    let mut lines = BufReader::new(read_half).lines();
    let line = lines.next_line().await?.context("controller closed control socket without a response")?;
    let response: ControlResponse = serde_json::from_str(&line).context("invalid controller response")?;
    if let Some(error) = response.error {
        anyhow::bail!("{error}");
    }
    let result = response.result.context("controller returned an empty command result")?;
    if !result.stdout.is_empty() { print!("{}", result.stdout); }
    if !result.stderr.is_empty() { eprint!("{}", result.stderr); }
    println!("\n[exit: {} | {} ms{}]", result.exit_code.map_or_else(|| "signal".to_owned(), |code| code.to_string()), result.duration_ms, if result.timed_out { " | timed out" } else { "" });
    if result.exit_code.unwrap_or(1) != 0 || result.timed_out {
        anyhow::bail!("remote command failed");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Devices) => {
            let database = Database::open(&cli.database)?;
            let devices = database.list_devices()?;
            if devices.is_empty() {
                println!("No devices have registered with this controller yet.");
                return Ok(());
            }
            println!("{:<38} {:<28} {:<18} {:<10} {}", "DEVICE ID", "NAME", "PLATFORM", "STATUS", "LAST SEEN");
            for record in &devices {
                let status = match record.device.status {
                    DeviceStatus::Online => "ONLINE",
                    DeviceStatus::Offline => "OFFLINE",
                    DeviceStatus::Busy => "BUSY",
                    DeviceStatus::Unknown => "UNKNOWN",
                };
                println!("{:<38} {:<28} {:<18} {:<10} {}", record.device.id.0, record.device.name, record.device.platform, status, age_label(record.last_seen));
            }
            let (total, online) = database.counts()?;
            println!("\n{total} device(s) registered · {online} online");
        }
        Some(Command::Status) => {
            let database = Database::open(&cli.database)?;
            let (total, online) = database.counts()?;
            println!("ZRMS fleet status");
            println!("  Registered devices: {total}");
            println!("  Online devices:     {online}");
            println!("  Offline devices:    {}", total.saturating_sub(online));
            println!("  Database:           {}", database.path().display());
        }
        Some(Command::Exec { device_id, command, timeout_ms }) => {
            execute_remote(&cli.socket, &device_id, &command, timeout_ms).await?;
        }
        None => {
            println!("ZRMS> Zyfin Remote Management System");
            println!("Run zrms --help to see available commands.");
        }
    }
    Ok(())
}
