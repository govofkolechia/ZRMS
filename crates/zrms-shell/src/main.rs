use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use zrms_core::DeviceStatus;
use zrms_storage::Database;

#[derive(Debug, Parser)]
#[command(name = "zrms", version, about = "Zyfin Remote Management System")]
struct Cli {
    /// Path to the controller's SQLite database.
    #[arg(long, global = true, default_value = "zrms.db")]
    database: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List devices registered with this controller.
    Devices,
    /// Show controller and fleet status.
    Status,
}

fn age_label(timestamp: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let seconds = now.saturating_sub(timestamp);
    if seconds < 5 {
        "just now".to_owned()
    } else if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let database = Database::open(&cli.database)?;

    match cli.command {
        Some(Command::Devices) => {
            let devices = database.list_devices()?;
            if devices.is_empty() {
                println!("No devices have registered with this controller yet.");
                return Ok(());
            }

            println!(
                "{:<38} {:<28} {:<18} {:<10} {}",
                "DEVICE ID", "NAME", "PLATFORM", "STATUS", "LAST SEEN"
            );
            for record in &devices {
                let status = match record.device.status {
                    DeviceStatus::Online => "ONLINE",
                    DeviceStatus::Offline => "OFFLINE",
                    DeviceStatus::Busy => "BUSY",
                    DeviceStatus::Unknown => "UNKNOWN",
                };
                println!(
                    "{:<38} {:<28} {:<18} {:<10} {}",
                    record.device.id.0,
                    record.device.name,
                    record.device.platform,
                    status,
                    age_label(record.last_seen)
                );
            }
            let (total, online) = database.counts()?;
            println!();
            println!("{total} device(s) registered · {online} online");
        }
        Some(Command::Status) => {
            let (total, online) = database.counts()?;
            println!("ZRMS fleet status");
            println!("  Registered devices: {total}");
            println!("  Online devices:     {online}");
            println!("  Offline devices:    {}", total.saturating_sub(online));
            println!("  Database:           {}", database.path().display());
        }
        None => {
            println!("ZRMS> Zyfin Remote Management System");
            println!("Run zrms --help to see available commands.");
        }
    }
    Ok(())
}
