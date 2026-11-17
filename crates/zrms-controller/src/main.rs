use anyhow::{Context, Result};
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader as AsyncBufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use zrms_protocol::{AgentMessage, PROTOCOL_VERSION};

#[derive(Debug, Parser)]
#[command(name = "zrmsd", about = "ZRMS secure controller daemon")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:7443")]
    listen: String,
    #[arg(long)]
    cert: PathBuf,
    #[arg(long)]
    key: PathBuf,
    #[arg(long)]
    client_ca: PathBuf,
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
fn tls_acceptor(args: &Args) -> Result<TlsAcceptor> {
    let mut roots = RootCertStore::empty();
    for cert in certificates(&args.client_ca)? {
        roots.add(cert)?;
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates(&args.cert)?, private_key(&args.key)?)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
async fn handle(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    acceptor: TlsAcceptor,
) -> Result<()> {
    let tls = acceptor
        .accept(stream)
        .await
        .context("mutual-TLS handshake failed")?;
    info!(%peer, "agent authenticated; waiting for protocol hello");
    let mut lines = AsyncBufReader::new(tls).lines();
    while let Some(line) = lines.next_line().await? {
        if line.len() > 64 * 1024 {
            anyhow::bail!("agent frame exceeds 64 KiB");
        }
        let message: AgentMessage = serde_json::from_str(&line).context("invalid agent message")?;
        match message {
            AgentMessage::Hello(hello) => {
                if hello.protocol_version != PROTOCOL_VERSION {
                    warn!(%peer, received = hello.protocol_version, supported = PROTOCOL_VERSION, "protocol version mismatch");
                    anyhow::bail!("unsupported protocol version");
                }
                info!(%peer, device_id = ?hello.device.id, name = %hello.device.name, hostname = %hello.device.hostname, platform = %hello.device.platform, agent_version = %hello.device.agent_version, "device connected");
            }
            AgentMessage::Heartbeat(heartbeat) => {
                info!(%peer, device_id = ?heartbeat.device_id, sequence = heartbeat.sequence, "heartbeat");
            }
        }
    }
    info!(%peer, "agent disconnected");
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let acceptor = tls_acceptor(&args)?;
    let listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    info!(listen = %args.listen, "ZRMS controller listening with mutual TLS");
    loop {
        let (stream, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(stream, peer, acceptor).await {
                error!(%peer, error = %err, "agent connection failed");
            }
        });
    }
}
