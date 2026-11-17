use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "zrms", version, about = "Zyfin Remote Management System")]
struct Cli { #[command(subcommand)] command: Option<Command> }
#[derive(Debug, Subcommand)]
enum Command { /// List devices registered with this controller.
Devices, /// Show controller status.
Status }

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Devices) => println!("No devices registered. Controller connectivity is not implemented yet."),
        Some(Command::Status) => println!("ZRMS controller: foundation build (network services not yet available)."),
        None => { println!("ZRMS> Zyfin Remote Management System"); println!("Run zrms --help to see available commands."); }
    }
    Ok(())
}
