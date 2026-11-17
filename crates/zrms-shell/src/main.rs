use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{self, Write};
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
    /// Show recent remote command executions.
    History {
        /// Maximum number of entries to display (1-500).
        #[arg(long, default_value_t = 20)]
        limit: u32,
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


fn resolve_device(database: &Database, query: &str) -> Result<zrms_core::Device> {
    let devices = database.list_devices()?;
    let matches: Vec<_> = devices.into_iter().filter(|record| {
        let id = record.device.id.0.to_string();
        id == query || id.starts_with(query)
            || record.device.name.eq_ignore_ascii_case(query)
            || record.device.hostname.eq_ignore_ascii_case(query)
    }).map(|record| record.device).collect();
    match matches.as_slice() {
        [device] => Ok(device.clone()),
        [] => anyhow::bail!("no device matches '{query}'"),
        _ => anyhow::bail!("'{query}' matches multiple devices; use the full UUID"),
    }
}

fn print_devices(database: &Database) -> Result<()> {
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
    Ok(())
}

fn print_history(database: &Database, limit: u32) -> Result<()> {
    let records = database.list_command_history(limit)?;
    if records.is_empty() {
        println!("No remote command executions have been recorded yet.");
        return Ok(());
    }
    println!("{:<19} {:<10} {:<10} {:<8} {:<12} COMMAND", "STARTED (UNIX MS)", "DEVICE", "USER UID", "EXIT", "OUTCOME");
    for record in records {
        let device = record.device_id.0.to_string();
        let device_short = &device[..8];
        let exit = record.exit_code.map_or_else(|| "-".to_owned(), |code| code.to_string());
        println!("{:<19} {:<10} {:<10} {:<8} {:<12} {} {}", record.started_at, device_short, record.initiator_uid, exit, record.outcome, record.program, record.args_json);
        if let Some(error) = record.error { println!("  error: {error}"); }
    }
    Ok(())
}

async fn run_interactive(database_path: &PathBuf, socket_path: &PathBuf) -> Result<()> {
    let database = Database::open(database_path)?;
    let mut focused: Option<zrms_core::Device> = None;
    println!("ZRMS interactive shell · type 'help' for commands, 'exit' to quit");
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        match &focused {
            Some(device) => print!("ZRMS[{}]> ", device.name),
            None => print!("ZRMS> "),
        }
        io::stdout().flush()?;
        line.clear();
        if stdin.read_line(&mut line)? == 0 { println!(); break; }
        let mut words = line.split_whitespace();
        let Some(action) = words.next() else { continue; };
        let args: Vec<&str> = words.collect();
        match action.to_ascii_lowercase().as_str() {
            "exit" | "quit" => break,
            "help" => {
                println!("  devices                 List registered devices");
                println!("  focus <device>          Focus by name, hostname, UUID, or UUID prefix");
                println!("  unfocus                 Clear the focused device");
                println!("  exec <program> [args]   Run a command on the focused device");
                println!("  history [limit]         Show recent command history");
                println!("  status                  Show fleet status");
                println!("  help                    Show this help");
                println!("  exit                    Leave the ZRMS shell");
                println!("Arguments are whitespace-separated; use 'sh -c' for shell syntax.");
            }
            "devices" => if let Err(error) = print_devices(&database) { eprintln!("Error: {error:#}"); },
            "status" => match database.counts() {
                Ok((total, online)) => println!("Registered: {total} · Online: {online} · Offline: {}", total.saturating_sub(online)),
                Err(error) => eprintln!("Error: {error:#}"),
            },
            "focus" => {
                if args.len() != 1 { println!("Usage: focus <device name, hostname, or UUID>"); continue; }
                match resolve_device(&database, args[0]) {
                    Ok(device) => {
                        println!("Focused device: {} ({})", device.name, device.id.0);
                        focused = Some(device);
                    }
                    Err(error) => eprintln!("Error: {error:#}"),
                }
            }
            "unfocus" => { focused = None; println!("Device focus cleared."); }
            "exec" => {
                let Some(device) = focused.as_ref() else { println!("No device focused. Use: focus <device>"); continue; };
                if args.is_empty() { println!("Usage: exec <program> [arguments...]"); continue; }
                let command: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
                if let Err(error) = execute_remote(socket_path, &device.id.0.to_string(), &command, 10_000).await {
                    eprintln!("Error: {error:#}");
                }
            }
            "history" => {
                let limit = match args.first() {
                    Some(value) => match value.parse::<u32>() { Ok(limit) => limit, Err(_) => { println!("Usage: history [limit]"); continue; } },
                    None => 20,
                };
                if let Err(error) = print_history(&database, limit) { eprintln!("Error: {error:#}"); }
            }
            _ => println!("Unknown command '{action}'. Type 'help' for available commands."),
        }
    }
    println!("Leaving ZRMS.");
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
        Some(Command::History { limit }) => {
            let database = Database::open(&cli.database)?;
            let records = database.list_command_history(limit)?;
            if records.is_empty() {
                println!("No remote command executions have been recorded yet.");
                return Ok(());
            }
            println!("{:<19} {:<10} {:<10} {:<8} {:<12} COMMAND", "STARTED (UNIX MS)", "DEVICE", "USER UID", "EXIT", "OUTCOME");
            for record in records {
                let device = record.device_id.0.to_string();
                let device_short = &device[..8];
                let exit = record.exit_code.map_or_else(|| "-".to_owned(), |code| code.to_string());
                println!(
                    "{:<19} {:<10} {:<10} {:<8} {:<12} {} {}",
                    record.started_at,
                    device_short,
                    record.initiator_uid,
                    exit,
                    record.outcome,
                    record.program,
                    record.args_json,
                );
                if let Some(error) = record.error {
                    println!("  error: {error}");
                }
            }
        }
        None => { run_interactive(&cli.database, &cli.socket).await?; }
    }
    Ok(())
}
