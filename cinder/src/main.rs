#![deny(unsafe_code)]

use clap::{Parser, Subcommand};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(name = "cinder", version, about = "E2E-encrypted terminal messenger")]
struct Cli {
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
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui) {
        Command::Id { .. } => {
            anyhow::bail!("cinder id: not implemented in M1 (needs identity persistence, M2)")
        }
        Command::Add { .. } => anyhow::bail!("cinder add: not implemented in M1"),
        Command::Rpc => anyhow::bail!("cinder rpc: not implemented in M1 (M3)"),
        Command::Daemon => anyhow::bail!("cinder daemon: not implemented in M1 (M3)"),
        Command::Tui => anyhow::bail!("cinder tui: not implemented in M1 (M4)"),
        Command::Relay { listen } => {
            let listener = tokio::net::TcpListener::bind(listen).await?;
            eprintln!("cinder-relay listening on {}", listener.local_addr()?);
            cinder_relay::server::serve(listener, cinder_relay::server::RelayConfig::default())
                .await?;
        }
    }
    Ok(())
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
