#![forbid(unsafe_code)]

use clap::Parser;
use std::net::SocketAddr;

#[derive(Parser)]
#[command(name = "cinder-relay", about = "cinder dumb blind relay")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:7000")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    eprintln!("cinder-relay listening on {}", listener.local_addr()?);
    cinder_relay::server::serve(listener, cinder_relay::server::RelayConfig::default()).await?;
    Ok(())
}
