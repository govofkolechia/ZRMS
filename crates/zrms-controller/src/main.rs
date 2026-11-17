use anyhow::{Context, Result};
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{self, Duration};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use zrms_core::DeviceId;
use zrms_protocol::{
    AgentMessage, CommandResult, ControlRequest, ControlResponse, ControllerMessage,
    ExecuteCommand, MAX_COMMAND_TIMEOUT_MS, PROTOCOL_VERSION,
};
use zrms_storage::Database;

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
    #[arg(long, default_value = "zrms.db")]
    database: PathBuf,
    #[arg(long, default_value = "zrms.sock")]
    control_socket: PathBuf,
}

type AgentMap = Arc<Mutex<HashMap<DeviceId, (Uuid, mpsc::Sender<ControllerMessage>)>>>;
type PendingMap = Arc<Mutex<HashMap<Uuid, oneshot::Sender<CommandResult>>>>;

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

async fn handle_agent(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    acceptor: TlsAcceptor,
    database: Database,
    agents: AgentMap,
    pending: PendingMap,
) -> Result<()> {
    let tls = acceptor
        .accept(stream)
        .await
        .context("mutual-TLS handshake failed")?;
    info!(%peer, "agent authenticated; waiting for protocol hello");
    let (read_half, mut write_half) = tokio::io::split(tls);
    let mut lines = AsyncBufReader::new(read_half).lines();
    let (tx, mut rx) = mpsc::channel::<ControllerMessage>(32);
    let connection_id = Uuid::new_v4();
    let mut registered_device: Option<DeviceId> = None;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break; };
                if line.len() > 64 * 1024 { anyhow::bail!("agent frame exceeds 64 KiB"); }
                let message: AgentMessage = serde_json::from_str(&line).context("invalid agent message")?;
                match message {
                    AgentMessage::Hello(hello) => {
                        if hello.protocol_version != PROTOCOL_VERSION {
                            warn!(%peer, received = hello.protocol_version, supported = PROTOCOL_VERSION, "protocol version mismatch");
                            anyhow::bail!("unsupported protocol version");
                        }
                        if registered_device.is_some() { anyhow::bail!("agent sent more than one hello message"); }
                        let id = hello.device.id;
                        database.register_device(&hello.device)?;
                        agents.lock().await.insert(id, (connection_id, tx.clone()));
                        registered_device = Some(id);
                        info!(%peer, device_id = ?id, name = %hello.device.name, hostname = %hello.device.hostname, platform = %hello.device.platform, agent_version = %hello.device.agent_version, "device registered and connected");
                    }
                    AgentMessage::Heartbeat(heartbeat) => {
                        if registered_device != Some(heartbeat.device_id) {
                            anyhow::bail!("heartbeat device ID does not match authenticated session");
                        }
                        if !database.record_heartbeat(heartbeat.device_id)? {
                            anyhow::bail!("heartbeat received for an unregistered device");
                        }
                        info!(%peer, device_id = ?heartbeat.device_id, sequence = heartbeat.sequence, "heartbeat");
                    }
                    AgentMessage::CommandResult(result) => {
                        if registered_device.is_none() { anyhow::bail!("command result before hello"); }
                        if let Some(reply) = pending.lock().await.remove(&result.request_id) {
                            let _ = reply.send(result);
                        } else {
                            warn!(%peer, "received command result with unknown request ID");
                        }
                    }
                }
            }
            message = rx.recv() => {
                let Some(message) = message else { break; };
                let mut frame = serde_json::to_vec(&message)?;
                frame.push(b'\n');
                write_half.write_all(&frame).await.context("send command to agent")?;
            }
        }
    }

    if let Some(id) = registered_device {
        let mut map = agents.lock().await;
        if map
            .get(&id)
            .is_some_and(|(active_id, _)| *active_id == connection_id)
        {
            map.remove(&id);
            database.mark_offline(id)?;
            info!(%peer, device_id = ?id, "device marked offline");
        }
    }
    info!(%peer, "agent disconnected");
    Ok(())
}

async fn handle_control(
    socket: tokio::net::UnixStream,
    agents: AgentMap,
    pending: PendingMap,
    database: Database,
) -> Result<()> {
    let initiator_uid = socket.peer_cred()?.uid();
    let (read_half, mut write_half) = tokio::io::split(socket);
    let mut lines = AsyncBufReader::new(read_half).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    if line.len() > 64 * 1024 {
        anyhow::bail!("control request exceeds 64 KiB");
    }
    let request: ControlRequest =
        serde_json::from_str(&line).context("invalid local control request")?;
    if request.program.trim().is_empty() {
        anyhow::bail!("program must not be empty");
    }
    let request_id = Uuid::new_v4();
    database.record_command_started(
        request_id,
        request.device_id,
        &request.program,
        &request.args,
        initiator_uid,
    )?;

    let (reply_tx, reply_rx) = oneshot::channel();
    let sender = agents
        .lock()
        .await
        .get(&request.device_id)
        .map(|(_, tx)| tx.clone());
    let response = if let Some(sender) = sender {
        pending.lock().await.insert(request_id, reply_tx);
        let command = ControllerMessage::Execute(ExecuteCommand {
            request_id,
            program: request.program,
            args: request.args,
            timeout_ms: request.timeout_ms.clamp(1, MAX_COMMAND_TIMEOUT_MS),
        });
        if sender.send(command).await.is_err() {
            pending.lock().await.remove(&request_id);
            ControlResponse {
                result: None,
                error: Some("agent disconnected before command delivery".into()),
            }
        } else {
            match time::timeout(
                Duration::from_millis(MAX_COMMAND_TIMEOUT_MS + 5_000),
                reply_rx,
            )
            .await
            {
                Ok(Ok(result)) => ControlResponse {
                    result: Some(result),
                    error: None,
                },
                Ok(Err(_)) => ControlResponse {
                    result: None,
                    error: Some("agent connection ended before returning a result".into()),
                },
                Err(_) => {
                    pending.lock().await.remove(&request_id);
                    ControlResponse {
                        result: None,
                        error: Some("timed out waiting for agent response".into()),
                    }
                }
            }
        }
    } else {
        ControlResponse {
            result: None,
            error: Some("device is not currently connected".into()),
        }
    };

    if let Some(result) = response.result.as_ref() {
        let outcome = if result.timed_out {
            "timed_out"
        } else if result.exit_code == Some(0) {
            "succeeded"
        } else {
            "failed"
        };
        database.complete_command(
            request_id,
            outcome,
            result.exit_code,
            Some(result.duration_ms),
            result.timed_out,
            None,
        )?;
    } else {
        let error = response.error.as_deref().unwrap_or("unknown command failure");
        let outcome = if error.contains("not currently connected") {
            "unavailable"
        } else if error.contains("timed out") {
            "response_timeout"
        } else {
            "delivery_failed"
        };
        database.complete_command(request_id, outcome, None, None, false, Some(error))?;
    }

    let mut frame = serde_json::to_vec(&response)?;
    frame.push(b'\n');
    write_half.write_all(&frame).await?;
    Ok(())
}

async fn control_listener(
    path: PathBuf,
    agents: AgentMap,
    pending: PendingMap,
    database: Database,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    if path.exists() {
        tokio::fs::remove_file(&path)
            .await
            .context("remove stale control socket")?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind control socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    info!(socket = %path.display(), "local control socket listening");
    loop {
        let (socket, _) = listener.accept().await?;
        let agents = agents.clone();
        let pending = pending.clone();
        let database = database.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_control(socket, agents, pending, database).await {
                warn!(%error, "local control request failed");
            }
        });
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
    let database = Database::open(&args.database)?;
    database.mark_all_offline()?;
    let acceptor = tls_acceptor(&args)?;
    let listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    let agents: AgentMap = Arc::new(Mutex::new(HashMap::new()));
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let control_path = args.control_socket.clone();
    tokio::spawn(control_listener(
        control_path,
        agents.clone(),
        pending.clone(),
        database.clone(),
    ));
    info!(listen = %args.listen, database = %database.path().display(), "ZRMS controller listening with mutual TLS");
    loop {
        let (stream, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let database = database.clone();
        let agents = agents.clone();
        let pending = pending.clone();
        tokio::spawn(async move {
            if let Err(error) =
                handle_agent(stream, peer, acceptor, database, agents, pending).await
            {
                error!(%peer, error = %error, "agent connection failed");
            }
        });
    }
}
