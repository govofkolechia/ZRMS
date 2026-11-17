use anyhow::{Context, Result};
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::{self, Duration};
use tokio_rustls::TlsConnector;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use zrms_core::{Device, DeviceId, DeviceStatus};
use zrms_protocol::{AgentHello, AgentMessage, Heartbeat, PROTOCOL_VERSION};

#[derive(Debug, Parser)]
#[command(name = "zyfin-agent", about = "ZRMS remote Linux agent")]
struct Args {
    #[arg(long, default_value = "192.168.1.11:7443")]
    controller: String,
    #[arg(long, default_value = "192.168.1.11")]
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
fn certificates(path: &PathBuf) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    Ok(rustls_pemfile::certs(&mut reader).collect::<std::result::Result<Vec<_>, _>>()?)
}
fn private_key(path: &PathBuf) -> Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    rustls_pemfile::private_key(&mut reader)?.context("no private key found in PEM file")
}
fn tls_connector(args: &Args) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    for cert in certificates(&args.ca)? { roots.add(cert)?; }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates(&args.cert)?, private_key(&args.key)?)?;
    Ok(TlsConnector::from(Arc::new(config)))
}
fn device_identity(args: &Args) -> Result<Device> {
    if let Some(parent) = args.state_file.parent() {
        if !parent.as_os_str().is_empty() { fs::create_dir_all(parent)?; }
    }
    let id = if args.state_file.exists() {
        Uuid::parse_str(fs::read_to_string(&args.state_file)?.trim()).context("invalid device ID in state file")?
    } else {
        let id = Uuid::new_v4();
        fs::write(&args.state_file, format!("{id}\n")).with_context(|| format!("write {}", args.state_file.display()))?;
        id
    };
    let hostname = fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "linux-device".to_owned()).trim().to_owned();
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
async fn send(stream: &mut tokio_rustls::client::TlsStream<TcpStream>, message: &AgentMessage) -> Result<()> {
    let mut frame = serde_json::to_vec(message)?;
    frame.push(b'\n');
    stream.write_all(&frame).await?;
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();
    let args = Args::parse();
    let device = device_identity(&args)?;
    let connector = tls_connector(&args)?;
    let socket = TcpStream::connect(&args.controller).await.with_context(|| format!("connect to controller {}", args.controller))?;
    let server_name = ServerName::try_from(args.server_name.clone()).context("invalid TLS server name")?;
    let mut stream = connector.connect(server_name, socket).await.context("mutual-TLS handshake failed")?;
    let device_id = device.id;
    info!(controller = %args.controller, device_id = ?device_id, hostname = %device.hostname, "secure controller connection established");
    send(&mut stream, &AgentMessage::Hello(AgentHello { protocol_version: PROTOCOL_VERSION, device })).await?;
    let mut sequence = 0_u64;
    let mut interval = time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        sequence += 1;
        if let Err(err) = send(&mut stream, &AgentMessage::Heartbeat(Heartbeat { device_id, sequence })).await {
            warn!(error = %err, "heartbeat failed; agent will exit for service-manager restart");
            return Err(err);
        }
    }
}