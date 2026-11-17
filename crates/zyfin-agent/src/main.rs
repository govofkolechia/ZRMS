use anyhow::{Context, Result};
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::{self, Duration};
use tokio_rustls::TlsConnector;
use tracing::info;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use zrms_core::{Device, DeviceId, DeviceStatus};
use zrms_protocol::{
    AgentHello, AgentMessage, CommandResult, ControllerMessage, ExecuteCommand, Heartbeat,
    MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_TIMEOUT_MS, PROTOCOL_VERSION,
};

#[derive(Debug, Parser)]
#[command(name = "zyfin-agent", about = "ZRMS remote Linux agent")]
struct Args {
    /// Controller network address, such as 192.0.2.10:7443 or controller.example.net:7443.
    #[arg(long)]
    controller: String,
    /// TLS certificate identity presented by the controller; independent of its network address.
    #[arg(long, default_value = "zrms-controller")]
    server_name: String,
    #[arg(long)]
    ca: PathBuf,
    #[arg(long)]
    cert: PathBuf,
    #[arg(long)]
    key: PathBuf,
    #[arg(long, default_value = "zyfin-agent-id")]
    state_file: PathBuf,
    #[arg(long)]
    name: Option<String>,
}
fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader =
        BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    Ok(rustls_pemfile::certs(&mut reader).collect::<std::result::Result<Vec<_>, _>>()?)
}
fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut reader =
        BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    rustls_pemfile::private_key(&mut reader)?.context("no private key found in PEM file")
}
fn tls_connector(args: &Args) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    for cert in certificates(&args.ca)? {
        roots.add(cert)?;
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates(&args.cert)?, private_key(&args.key)?)?;
    Ok(TlsConnector::from(Arc::new(config)))
}
fn device_identity(args: &Args) -> Result<Device> {
    if let Some(parent) = args.state_file.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let id = if args.state_file.exists() {
        Uuid::parse_str(fs::read_to_string(&args.state_file)?.trim())
            .context("invalid device ID in state file")?
    } else {
        let id = Uuid::new_v4();
        fs::write(&args.state_file, format!("{id}\n"))
            .with_context(|| format!("write {}", args.state_file.display()))?;
        id
    };
    let hostname = fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "linux-device".to_owned())
        .trim()
        .to_owned();
    let name = args.name.clone().unwrap_or_else(|| hostname.clone());
    Ok(Device {
        id: DeviceId(id),
        name,
        hostname,
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        status: DeviceStatus::Unknown,
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}
async fn send<W: AsyncWriteExt + Unpin>(writer: &mut W, message: &AgentMessage) -> Result<()> {
    let mut frame = serde_json::to_vec(message)?;
    frame.push(b'\n');
    writer.write_all(&frame).await?;
    Ok(())
}
async fn execute(request: ExecuteCommand) -> CommandResult {
    let started = Instant::now();
    let timeout_ms = request.timeout_ms.clamp(1, MAX_COMMAND_TIMEOUT_MS);
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("ZRMS_REMOTE", "1");
    let result = if request.program.is_empty() {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "program must not be empty",
        ))
    } else {
        match command.spawn() {
            Ok(child) => {
                match time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
                    .await
                {
                    Ok(Ok(output)) => {
                        let stdout = String::from_utf8_lossy(
                            &output.stdout[..output.stdout.len().min(MAX_COMMAND_OUTPUT_BYTES)],
                        )
                        .into_owned();
                        let stderr = String::from_utf8_lossy(
                            &output.stderr[..output.stderr.len().min(MAX_COMMAND_OUTPUT_BYTES)],
                        )
                        .into_owned();
                        Ok((stdout, stderr, output.status.code(), false))
                    }
                    Ok(Err(error)) => Err(error),
                    Err(_) => Ok((
                        String::new(),
                        format!("command timed out after {timeout_ms} ms"),
                        None,
                        true,
                    )),
                }
            }
            Err(error) => Err(error),
        }
    };
    let (stdout, stderr, exit_code, timed_out) = match result {
        Ok(values) => values,
        Err(error) => (String::new(), error.to_string(), None, false),
    };
    CommandResult {
        request_id: request.request_id,
        stdout,
        stderr,
        exit_code,
        duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        timed_out,
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let device = device_identity(&args)?;
    let connector = tls_connector(&args)?;
    let socket = TcpStream::connect(&args.controller)
        .await
        .with_context(|| format!("connect to controller {}", args.controller))?;
    let server_name =
        ServerName::try_from(args.server_name.clone()).context("invalid TLS server name")?;
    let stream = connector
        .connect(server_name, socket)
        .await
        .context("mutual-TLS handshake failed")?;
    let device_id = device.id;
    info!(controller = %args.controller, server_name = %args.server_name, device_id = ?device_id, hostname = %device.hostname, "secure controller connection established");

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = AsyncBufReader::new(read_half).lines();
    send(
        &mut write_half,
        &AgentMessage::Hello(AgentHello {
            protocol_version: PROTOCOL_VERSION,
            device,
        }),
    )
    .await?;

    let mut sequence = 0_u64;
    let mut interval = time::interval(Duration::from_secs(10));
    interval.tick().await; // Skip the immediate first tick; hello was just sent.
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else {
                    anyhow::bail!("controller closed the connection");
                };
                if line.len() > 64 * 1024 {
                    anyhow::bail!("controller frame exceeds 64 KiB");
                }
                let message: ControllerMessage = serde_json::from_str(&line)
                    .context("invalid controller message")?;
                if let ControllerMessage::Execute(request) = message {
                    info!(request_id = %request.request_id, program = %request.program, "executing remote command");
                    let result = execute(request).await;
                    send(&mut write_half, &AgentMessage::CommandResult(result)).await?;
                }
            }
            _ = interval.tick() => {
                sequence += 1;
                send(
                    &mut write_half,
                    &AgentMessage::Heartbeat(Heartbeat { device_id, sequence }),
                ).await.context("send heartbeat")?;
            }
        }
    }
}
