mod common;

use common::rpc::dispatch::{serve_connection, Engine, EngineConfig, Transport};
use common::*;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        SharedBuf(self.0.clone())
    }
}

// P8 — a full scripted session over the wire: every byte on the client's
// inbound stream is newline-delimited JSON-RPC. No banner, no log bleed.
#[tokio::test]
async fn p8_stdout_purity() {
    let (cli, srv) = duplex(256 * 1024);
    let (srv_read, srv_write) = tokio::io::split(srv);
    let (cli_read, mut cli_write) = tokio::io::split(cli);

    let core = Arc::new(MockCore::basic());
    let cfg = EngineConfig {
        send_queue_frames: 64,
        ..Default::default()
    };
    let engine = Engine::new(core.clone(), cfg);
    core.attach(engine.events.clone());
    let task = tokio::spawn(serve_connection(
        engine,
        BufReader::new(srv_read),
        srv_write,
        Transport::Stdio,
    ));

    let script = [
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.hello\",\"params\":{\"protocol_version\":[1]}}",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"rpc.describe\",\"params\":{}}",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"msg.send\",\"params\":{\"to\":\"contact:9f3b12\",\"body\":\"purity check\",\"client_msg_id\":\"p8-1\"}}",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"events.subscribe\",\"params\":{\"kinds\":[\"msg_state\"]}}",
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"shutdown.request\",\"params\":{\"grace_ms\":1}}",
    ];
    // mid-session warn log: must never reach the frame channel
    let logbuf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(SharedBuf(logbuf.clone()))
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut raw = Vec::new();
    {
        let mut br = BufReader::new(cli_read);
        for (i, line) in script.iter().enumerate() {
            cli_write.write_all(line.as_bytes()).await.unwrap();
            cli_write.write_all(b"\n").await.unwrap();
            cli_write.flush().await.unwrap();
            tracing::warn!(
                iteration = i,
                "mid-session diagnostic that must stay off stdout"
            );
            // read until we get the response for this id
            loop {
                let mut buf = Vec::new();
                let n = br.read_until(b'\n', &mut buf).await.unwrap();
                if n == 0 {
                    panic!("EOF before response to line {i}");
                }
                raw.extend_from_slice(&buf[..n]);
                let v: Value = serde_json::from_slice(&buf[..n - 1]).unwrap_or_else(|e| {
                    panic!(
                        "line {i}: byte sequence is not a JSON frame: {:?} ({e})",
                        String::from_utf8_lossy(&buf)
                    )
                });
                if v.get("id").and_then(|id| id.as_u64()) == Some(i as u64 + 1) {
                    break;
                }
            }
        }
        // drain any trailing shutdown notice, then EOF
        let mut rest = Vec::new();
        let n = br.read_to_end(&mut rest).await.unwrap();
        raw.extend_from_slice(&rest[..n]);
    }
    let _ = task.await;

    // every byte on the wire belongs to a newline-terminated JSON-RPC frame
    let text = String::from_utf8(raw.clone()).expect("all frame bytes are UTF-8");
    let mut count = 0;
    for (i, l) in text.split('\n').enumerate() {
        if l.is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(l).unwrap_or_else(|e| panic!("line {i} not a frame: {l:?} ({e})"));
        assert!(
            v.get("jsonrpc") == Some(&Value::from("2.0")),
            "frame {i} missing jsonrpc marker: {l}"
        );
        count += 1;
    }
    assert!(count >= 7, "expected hello+describe+send+subscribe resp+backlog+shutdown resp+notice frames, got {count}");
    assert!(
        !logbuf.lock().unwrap().is_empty(),
        "warn log was captured somewhere else"
    );
    let binding = logbuf.lock().unwrap();
    let logs = String::from_utf8_lossy(&binding);
    assert!(logs.contains("mid-session diagnostic"));
}

// The writer flushes after every frame: a single request produces a complete
// newline-terminated frame without needing any further input.
#[tokio::test]
async fn flush_per_frame() {
    let (cli, srv) = duplex(64 * 1024);
    let (srv_read, srv_write) = tokio::io::split(srv);
    let (cli_read, mut cli_write) = tokio::io::split(cli);
    let core = Arc::new(MockCore::basic());
    let engine = Engine::new(core.clone(), EngineConfig::default());
    core.attach(engine.events.clone());
    let _task = tokio::spawn(serve_connection(
        engine,
        BufReader::new(srv_read),
        srv_write,
        Transport::Stdio,
    ));

    cli_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"rpc.describe\",\"params\":{}}\n")
        .await
        .unwrap();
    // no further input, no explicit client flush needed beyond write_all
    let mut br = BufReader::new(cli_read);
    let mut line = String::new();
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), br.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(
        n > 0,
        "frame must arrive without waiting for more input (flush per frame)"
    );
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["id"], 1);
}
