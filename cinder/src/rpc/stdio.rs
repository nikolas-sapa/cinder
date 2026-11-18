use crate::rpc::core::CinderCore;
use crate::rpc::dispatch::{serve_connection, Engine, EngineConfig, Transport};
use std::io;
use std::sync::Arc;
use tokio::io::{stdin, stdout, BufReader};

/// `cinder rpc`: one client, stdin->stdout. Stdout carries frames only;
/// diagnostics go to stderr via tracing.
pub async fn run_stdio(core: Arc<dyn CinderCore>, cfg: EngineConfig) -> io::Result<()> {
    let engine = Engine::new(core, cfg);
    let reader = BufReader::with_capacity(64 * 1024, stdin());
    serve_connection(engine, reader, stdout(), Transport::Stdio).await
}
