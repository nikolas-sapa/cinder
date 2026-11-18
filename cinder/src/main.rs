#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod hardening;
mod core;
mod rpc;
mod store;
mod tui;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cinder_core::identity::Identity;
use tokio::io::{stdin, stdout, BufReader};

use crate::core::{contact_str, Core, MetaState, SqliteReplayLog, TuiBackend};
use crate::rpc::core::CinderCore;
use crate::rpc::dispatch::{serve_connection, Engine, EngineConfig, Transport};
use crate::rpc::events::EventManager;
use crate::rpc::server::{run_daemon, DaemonConfig};

const CONNECT_WAIT: Duration = Duration::from_secs(10);
const ADD_WAIT: Duration = Duration::from_secs(30);
const DEFAULT_RELAY_PORT: u16 = 7000;

#[derive(Parser)]
#[command(name = "cinder", version, about = "E2E-encrypted terminal messenger")]
struct Cli {
    /// Data directory (identity, contacts, sqlite store)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Relay host[:port]. Falls back to $CINDER_RELAY, then the last
    /// address that connected successfully (persisted in the store).
    #[arg(long, global = true)]
    relay: Option<String>,
    /// Print the keybinding reference and exit
    #[arg(long)]
    keybindings: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print your cinder:// identity URI
    Id {
        /// Render the URI as a terminal QR code
        #[arg(long)]
        qr: bool,
    },
    /// Add a contact from a cinder:// URI
    Add { uri: String },
    /// Newline-delimited JSON-RPC 2.0 on stdin/stdout
    Rpc,
    /// Same RPC protocol over a unix socket, multi-client
    Daemon,
    /// Terminal UI (default)
    Tui,
    /// Run a relay
    Relay {
        #[arg(long, default_value = "127.0.0.1:7000")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hardening::apply();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mut cli = Cli::parse();
    if cli.keybindings {
        print!("{}", include_str!("../../docs/keybindings.md"));
        return Ok(());
    }
    let cmd = cli.command.take().unwrap_or(Command::Tui);
    match cmd {
        Command::Id { qr } => cmd_id(&cli, qr).await,
        Command::Add { uri } => cmd_add(&cli, &uri).await,
        Command::Rpc => cmd_rpc(&cli).await,
        Command::Daemon => cmd_daemon(&cli).await,
        Command::Tui => cmd_tui(&cli).await,
        Command::Relay { listen } => {
            let listener = tokio::net::TcpListener::bind(listen).await?;
            eprintln!("cinder-relay listening on {}", listener.local_addr()?);
            cinder_relay::server::serve(listener, cinder_relay::server::RelayConfig::default())
                .await?;
            Ok(())
        }
    }
}

fn data_dir(flag: Option<PathBuf>) -> Result<PathBuf> {
    let dir = flag
        .or_else(|| std::env::var("CINDER_DATA_DIR").ok().map(PathBuf::from))
        .or_else(|| dirs::data_dir().map(|d| d.join("cinder")))
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/cinder")))
        .ok_or_else(|| anyhow!("no data dir: pass --data-dir"))?;
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

async fn load_identity(dir: &Path) -> Result<Identity> {
    if std::env::var("CINDER_IDENTITY").ok().as_deref() == Some("ephemeral") {
        return Identity::generate().context("identity generate");
    }
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || Identity::load_or_create(&dir))
        .await?
        .context("identity load_or_create")
}

/// Relay selection: `--relay` > `CINDER_RELAY` > persisted last-good address
/// (resolved inside `Core::open`). Returns `(addr, hostport-string)`.
async fn resolve_relay(cli: &Cli) -> Option<(SocketAddr, String)> {
    let host = cli
        .relay
        .clone()
        .or_else(|| std::env::var("CINDER_RELAY").ok())?;
    let host = if host.contains(':') {
        host
    } else {
        format!("{host}:{DEFAULT_RELAY_PORT}")
    };
    let addr = tokio::net::lookup_host(&host).await.ok()?.next()?;
    Some((addr, host))
}

async fn open_core(cli: &Cli, relay: Option<(SocketAddr, String)>) -> Result<(Arc<Core>, PathBuf)> {
    let dir = data_dir(cli.data_dir.clone())?;
    let identity = load_identity(&dir).await?;
    let relay = match relay {
        Some(r) => Some(r),
        None => resolve_relay(cli).await,
    };
    let core = Core::open(&dir, relay, identity).await?;
    Ok((core, dir))
}

async fn build_engine(core: &Arc<Core>) -> Result<Arc<Engine>> {
    let cfg = EngineConfig::default();
    let log = SqliteReplayLog::open(core.store().clone(), cfg.replay_log_max_events).await?;
    let events = Arc::new(EventManager::new(Box::new(log), cfg.send_queue_frames));
    core.attach_sink(events.clone());
    let dyn_core: Arc<dyn CinderCore> = core.clone();
    Ok(Engine::with_events(dyn_core, cfg, events))
}

async fn cmd_id(cli: &Cli, qr: bool) -> Result<()> {
    let (core, _dir) = open_core(cli, None).await?;
    core.wait_connected(Duration::from_secs(3)).await;
    let uri = match core.invite_link(24 * 3600).await {
        Ok((link, _)) => link,
        Err(_) => {
            eprintln!("cinder id: relay unreachable; printing static URI (no rendezvous, no relay)");
            core.identity().uri_static()
        }
    };
    println!("{uri}");
    if qr {
        let code = qrcode::QrCode::new(uri.as_bytes()).context("qr encode")?;
        let img = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(false)
            .build();
        println!("{img}");
    }
    core.begin_shutdown();
    Ok(())
}

async fn cmd_add(cli: &Cli, uri: &str) -> Result<()> {
    // `cinder add` connects to the relay named IN THE URI: the rendezvous
    // slot (or the static address) lives there.
    let (_pk, host, _tok) = Identity::parse_uri(uri).context("bad cinder uri")?;
    let host = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:{DEFAULT_RELAY_PORT}")
    };
    let addr: SocketAddr = tokio::net::lookup_host(&host)
        .await?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve relay host {host}"))?;
    let (core, _dir) = open_core(cli, Some((addr, host))).await?;
    if !core.wait_connected(CONNECT_WAIT).await {
        anyhow::bail!("relay {addr} unreachable");
    }
    let peer = core
        .clone()
        .start_add(uri, None)
        .await
        .map_err(|e| anyhow!("{}: {}", e.kind.as_str(), e.hint.as_deref().unwrap_or(&e.message)))?;
    if core.wait_meta(&peer, MetaState::Active, ADD_WAIT).await {
        println!("connected {}", contact_str(&peer));
        core.begin_shutdown();
        return Ok(());
    }
    let sub = core
        .meta_state(&peer)
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "pending_out".into());
    eprintln!("cinder add: contact still {sub} after {ADD_WAIT:?}");
    core.begin_shutdown();
    std::process::exit(1);
}

async fn cmd_rpc(cli: &Cli) -> Result<()> {
    let (core, _dir) = open_core(cli, None).await?;
    let engine = build_engine(&core).await?;
    let reader = BufReader::with_capacity(64 * 1024, stdin());
    serve_connection(engine, reader, stdout(), Transport::Stdio).await?;
    core.begin_shutdown();
    Ok(())
}

async fn cmd_daemon(cli: &Cli) -> Result<()> {
    let (core, _dir) = open_core(cli, None).await?;
    let engine = build_engine(&core).await?;
    eprintln!("cinder daemon on {}", DaemonConfig::default().socket_path.display());
    run_daemon(engine, DaemonConfig::default()).await?;
    core.begin_shutdown();
    Ok(())
}

async fn cmd_tui(cli: &Cli) -> Result<()> {
    let (core, _dir) = open_core(cli, None).await?;
    let backend = TuiBackend::new(core.clone());
    let result = tui::run(backend).await;
    core.begin_shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses() {
        Cli::command().debug_assert();
    }
}
