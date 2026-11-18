use crate::rpc::dispatch::{serve_connection, Engine, Transport};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(unix)]
use crate::rpc::peercred;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    /// v1 auth: peer uid must equal the daemon's. Injectable for tests.
    pub expected_uid: Option<u32>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            expected_uid: None,
        }
    }
}

/// `$XDG_RUNTIME_DIR/cinder/daemon.sock`, else `~/.local/state/cinder/daemon.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Path::new(&dir).join("cinder").join("daemon.sock");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home).join(".local/state/cinder/daemon.sock");
    }
    PathBuf::from("cinder/daemon.sock")
}

#[cfg(unix)]
fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    ensure_parent_dir(path)?;
    if let Ok(meta) = std::fs::metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_socket() {
            std::fs::remove_file(path)?;
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(unix)]
fn same_uid(stream: &UnixStream, expected: u32) -> bool {
    match peercred::peer_uid(stream) {
        Ok(uid) => uid == expected,
        Err(e) => {
            tracing::warn!(err = %e, "peer credential check failed; rejecting");
            false
        }
    }
}

/// `cinder daemon`: the line protocol over a unix socket, multi-client,
/// same-uid-only. Each accepted connection gets its own bounded send queue;
/// one stalled client never blocks the others.
#[cfg(unix)]
pub async fn run_daemon(engine: Arc<Engine>, dcfg: DaemonConfig) -> io::Result<()> {
    let listener = bind_listener(&dcfg.socket_path)?;
    let expected_uid = dcfg.expected_uid.unwrap_or_else(peercred::own_uid);
    let shutdown = engine.shutdown_handle();

    loop {
        if engine.is_shutting_down() {
            break;
        }
        tokio::select! {
            _ = shutdown.notified() => break,
            res = listener.accept() => {
                let (stream, _) = match res {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(err = %e, "accept failed");
                        continue;
                    }
                };
                if !same_uid(&stream, expected_uid) {
                    tracing::warn!("rejecting peer with mismatched uid");
                    drop(stream);
                    continue;
                }
                let eng = engine.clone();
                tokio::spawn(async move {
                    let (read, write) = stream.into_split();
                    let reader = tokio::io::BufReader::with_capacity(64 * 1024, read);
                    let _ = serve_connection(eng, reader, write, Transport::Daemon).await;
                });
            }
        }
    }
    Ok(())
}
