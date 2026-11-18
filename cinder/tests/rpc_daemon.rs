#![cfg(unix)]

mod common;

use common::rpc::codec::MAX_LINE_BYTES;
use common::rpc::dispatch::{Engine, EngineConfig};
use common::rpc::peercred::{own_uid, peer_uid};
use common::rpc::server::{run_daemon, DaemonConfig};
use common::*;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout, Duration};

fn daemon_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

async fn daemon_client(
    path: &std::path::Path,
) -> (
    BufReader<tokio::io::ReadHalf<UnixStream>>,
    tokio::io::WriteHalf<UnixStream>,
) {
    let s = UnixStream::connect(path).await.expect("connect");
    let (r, w) = tokio::io::split(s);
    (BufReader::new(r), w)
}

async fn rpc_line(
    r: &mut BufReader<tokio::io::ReadHalf<UnixStream>>,
    w: &mut tokio::io::WriteHalf<UnixStream>,
    line: &str,
) -> Value {
    w.write_all(line.as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    w.flush().await.unwrap();
    let mut buf = String::new();
    let n = timeout(Duration::from_secs(5), r.read_line(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(n > 0, "expected a frame");
    serde_json::from_str(buf.trim()).unwrap()
}

// peer_uid extraction works on a real connected socket.
#[tokio::test]
async fn peer_uid_extraction() {
    let dir = daemon_dir();
    let path = dir.path().join("s.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let cli = UnixStream::connect(&path).await.unwrap();
    let (srv, _) = listener.accept().await.unwrap();
    assert_eq!(peer_uid(&srv).unwrap(), own_uid());
    drop(cli);
}

// A daemon serving real clients over a unix socket, with 0700 dir / 0600
// socket, and per-client independent bounded queues (P10).
#[tokio::test]
async fn p10_daemon_multiclient() {
    let dir = daemon_dir();
    let sock = dir.path().join("daemon.sock");
    let core = Arc::new(MockCore::basic());
    let engine = Engine::new(core.clone(), EngineConfig::default());
    core.attach(engine.events.clone());
    let dcfg = DaemonConfig {
        socket_path: sock.clone(),
        expected_uid: None,
    };
    let daemon = tokio::spawn(run_daemon(engine, dcfg));
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }

    use std::os::unix::fs::PermissionsExt;
    let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "parent dir mode");
    let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(sock_mode, 0o600, "socket file mode");

    // client 1: subscribes, then gets killed off via the line cap
    let (mut r1, mut w1) = daemon_client(&sock).await;
    let f = rpc_line(&mut r1, &mut w1, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.hello\",\"params\":{\"protocol_version\":1}}").await;
    assert_eq!(f["result"]["server"]["transport"], "daemon");
    assert_eq!(f["result"]["capabilities"]["auth_method"], "peer_uid");
    let f = rpc_line(
        &mut r1,
        &mut w1,
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"events.subscribe\",\"params\":{}}",
    )
    .await;
    assert!(f["result"]["subscription_id"].is_string());
    // backlog marker arrives
    let mut buf = String::new();
    timeout(Duration::from_secs(2), r1.read_line(&mut buf))
        .await
        .unwrap()
        .unwrap();

    // client 2: healthy, keeps operating
    let (mut r2, mut w2) = daemon_client(&sock).await;
    let f = rpc_line(&mut r2, &mut w2, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.hello\",\"params\":{\"protocol_version\":1}}").await;
    assert_eq!(f["result"]["protocol_version"], 1);
    let f = rpc_line(
        &mut r2,
        &mut w2,
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"events.subscribe\",\"params\":{}}",
    )
    .await;
    assert!(f["result"]["subscription_id"].is_string());
    buf.clear();
    timeout(Duration::from_secs(2), r2.read_line(&mut buf))
        .await
        .unwrap()
        .unwrap();

    // fanout reaches both clients over real sockets.
    core.emit(
        "msg_state",
        Some(CHAT),
        json!({"chat_id": CHAT, "msg_id": "msg:1", "client_msg_id": "c", "state": "sent"}),
    );
    buf.clear();
    timeout(Duration::from_secs(2), r1.read_line(&mut buf))
        .await
        .unwrap()
        .unwrap();
    buf.clear();
    timeout(Duration::from_secs(2), r2.read_line(&mut buf))
        .await
        .unwrap()
        .unwrap();

    // conn isolation: client1 sends an oversized line -> one -32700 frame,
    // then that conn closes. Client2 is unaffected.
    let big = vec![b'x'; MAX_LINE_BYTES + 1];
    w1.write_all(&big).await.unwrap();
    w1.write_all(b"\n").await.unwrap();
    w1.flush().await.unwrap();
    buf.clear();
    timeout(Duration::from_secs(5), r1.read_line(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(buf.trim()).unwrap();
    assert_eq!(v["error"]["code"], -32700);
    buf.clear();
    let n = timeout(Duration::from_secs(5), r1.read_line(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "oversized-line conn must close after the error frame");

    // client2 still fully functional while client1 is gone
    let f = rpc_line(
        &mut r2,
        &mut w2,
        "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"chat.list\",\"params\":{}}",
    )
    .await;
    assert!(f["result"]["chats"].is_array());

    // graceful stop via rpc
    let f = rpc_line(
        &mut r2,
        &mut w2,
        "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"shutdown.request\",\"params\":{}}",
    )
    .await;
    assert_eq!(f["result"]["shutting_down"], true);
    let _ = timeout(Duration::from_secs(3), daemon).await;
}

// Wrong-uid peers are rejected at accept: connection closes with no frames.
#[tokio::test]
async fn wrong_uid_rejected() {
    let dir = daemon_dir();
    let sock = dir.path().join("daemon.sock");
    let core = Arc::new(MockCore::basic());
    // expected_uid = something that is NOT our uid
    let bogus = own_uid().wrapping_add(1).max(1);
    let dcfg = DaemonConfig {
        socket_path: sock.clone(),
        expected_uid: Some(bogus),
    };
    tokio::spawn(run_daemon(Engine::new(core, EngineConfig::default()), dcfg));
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }

    let (mut r, mut w) = daemon_client(&sock).await;
    // Peer may already be rejected before our write lands — reset is fine.
    let _ = w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.hello\",\"params\":{\"protocol_version\":1}}\n").await;
    let _ = w.flush().await;
    let mut buf = String::new();
    // Linux delivers ECONNRESET (close with unread data); macOS delivers EOF.
    match timeout(Duration::from_secs(3), r.read_line(&mut buf))
        .await
        .unwrap()
    {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("mismatched-uid peer must be rejected with no frames, got {other:?}"),
    }
}
