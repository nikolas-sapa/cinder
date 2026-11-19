use crate::rpc::codec::{is_blank, FrameWriter, LineRead, LineReader, MAX_LINE_BYTES};
use crate::rpc::core::CinderCore;
use crate::rpc::describe;
use crate::rpc::events::{EventManager, OutMsg};
use crate::rpc::methods::{self, DedupHit, DedupTable};
use crate::rpc::protocol::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, Notify};

pub const MAX_FRAME_BYTES: usize = MAX_LINE_BYTES;
pub const MAX_BODY_BYTES: usize = 8192;
pub const HISTORY_LIMIT_MAX: u32 = 500;
pub const SEND_QUEUE_FRAMES: usize = 1024;
pub const REPLAY_LOG_MAX_EVENTS: usize = 16384;
pub const DEDUP_MAX_ENTRIES: usize = 65536;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub max_frame_bytes: usize,
    pub max_body_bytes: usize,
    pub history_limit_max: u32,
    pub send_queue_frames: usize,
    pub replay_log_max_events: usize,
    pub dedup_ttl: Duration,
    pub dedup_max_entries: usize,
    pub supported_versions: &'static [u32],
    pub server_version: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_body_bytes: MAX_BODY_BYTES,
            history_limit_max: HISTORY_LIMIT_MAX,
            send_queue_frames: SEND_QUEUE_FRAMES,
            replay_log_max_events: REPLAY_LOG_MAX_EVENTS,
            dedup_ttl: Duration::from_secs(24 * 3600),
            dedup_max_entries: DEDUP_MAX_ENTRIES,
            supported_versions: &[1],
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl EngineConfig {
    pub fn limits_json(&self) -> Value {
        json!({
            "max_frame_bytes": self.max_frame_bytes,
            "max_body_bytes": self.max_body_bytes,
            "history_limit_max": self.history_limit_max,
            "send_queue_frames": self.send_queue_frames,
            "replay_log_max_events": self.replay_log_max_events,
            // Mirror of xfer::XferCaps::default() — kept literal so the rpc
            // tree still compiles standalone in #[path] test includes.
            "file_max_bytes": 64 * 1024 * 1024,
            "file_chunk_bytes": 7168,
            "file_max_inflight_peer": 4,
            "file_max_inflight": 16,
            "file_inline_max_bytes": 1024 * 1024,
            "file_voice_max_ms": 300_000,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Daemon,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Daemon => "daemon",
        }
    }

    pub fn auth_method(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Daemon => "peer_uid",
        }
    }
}

pub struct Engine {
    pub cfg: EngineConfig,
    core: Arc<dyn CinderCore>,
    dedup: Mutex<DedupTable>,
    pub events: Arc<EventManager>,
    shutting_down: AtomicBool,
    shutdown_notify: Arc<Notify>,
}

impl Engine {
    /// Default wiring: in-memory replay log. Used by tests via `#[path]`
    /// include; production builds the engine with `with_events` +
    /// `SqliteReplayLog` (main.rs `build_engine`).
    #[allow(dead_code)]
    pub fn new(core: Arc<dyn CinderCore>, cfg: EngineConfig) -> Arc<Self> {
        let events = Arc::new(EventManager::new(
            Box::new(crate::rpc::events::InMemoryReplayLog::new(
                cfg.replay_log_max_events,
            )),
            cfg.send_queue_frames,
        ));
        Arc::new(Self {
            dedup: Mutex::new(DedupTable::new(cfg.dedup_ttl, cfg.dedup_max_entries)),
            core,
            cfg,
            events,
            shutting_down: AtomicBool::new(false),
            shutdown_notify: Arc::new(Notify::new()),
        })
    }

    pub fn with_events(
        core: Arc<dyn CinderCore>,
        cfg: EngineConfig,
        events: Arc<EventManager>,
    ) -> Arc<Self> {
        Arc::new(Self {
            dedup: Mutex::new(DedupTable::new(cfg.dedup_ttl, cfg.dedup_max_entries)),
            core,
            cfg,
            events,
            shutting_down: AtomicBool::new(false),
            shutdown_notify: Arc::new(Notify::new()),
        })
    }

    pub fn core(&self) -> &Arc<dyn CinderCore> {
        &self.core
    }

    pub fn dedup_lookup(&self, key: &str, fingerprint: &str) -> DedupHit {
        self.dedup.lock().unwrap().lookup(key, fingerprint)
    }

    pub fn dedup_insert(&self, key: String, fingerprint: String, result: Value) {
        self.dedup.lock().unwrap().insert(key, fingerprint, result);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn begin_shutdown(&self, _grace_ms: u64) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
    }

    pub fn shutdown_handle(&self) -> Arc<Notify> {
        self.shutdown_notify.clone()
    }

    /// Attach a client: returns its Session plus the queue receiver the
    /// transport's writer task drains. Bounded per AGENT-API §8.1.
    pub fn connect(self: &Arc<Self>, transport: Transport) -> (Session, mpsc::Receiver<OutMsg>) {
        let (tx, rx) = mpsc::channel(self.cfg.send_queue_frames.max(1));
        let dead = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let conn_id = self
            .events
            .register_conn(tx.clone(), dead.clone(), wake.clone());
        (
            Session {
                engine: self.clone(),
                conn_id,
                transport,
                out: tx,
                dead,
                wake,
                helloed: false,
                scopes: HashSet::new(),
                subscriptions: HashSet::new(),
                after_response: None,
            },
            rx,
        )
    }
}

pub enum Verdict {
    Continue,
    Close,
}

pub struct Session {
    pub engine: Arc<Engine>,
    pub conn_id: u64,
    pub transport: Transport,
    pub out: mpsc::Sender<OutMsg>,
    pub dead: Arc<AtomicBool>,
    pub wake: Arc<Notify>,
    pub helloed: bool,
    pub scopes: HashSet<String>,
    pub subscriptions: HashSet<String>,
    /// Post-response hook (e.g. the replay task) so pushed frames always land
    /// after the request's response on the wire.
    pub after_response: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl Session {
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// The always-error rule lives here: any complete line that is not a
    /// whitespace-only keepalive yields a frame, or the connection itself
    /// closes loudly. Nothing is silently dropped.
    pub async fn handle_line(&mut self, line: &[u8]) -> Verdict {
        if is_blank(line) {
            return Verdict::Continue;
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => {
                self.send_frame(error_response(Value::Null, &RpcError::parse_error()));
                return Verdict::Continue;
            }
        };
        let Some(obj) = value.as_object() else {
            let hint = if value.is_array() {
                "batch arrays are not supported; send one request per line"
            } else {
                "expected a JSON-RPC 2.0 request object"
            };
            self.send_frame(error_response(
                Value::Null,
                &RpcError::invalid_request(hint),
            ));
            return Verdict::Continue;
        };

        let id = obj.get("id").cloned();
        let method_ok = obj
            .get("method")
            .map(|m| m.is_string() && !m.as_str().unwrap_or_default().is_empty())
            .unwrap_or(false);
        let version_ok = obj.get("jsonrpc").map(|j| j == "2.0").unwrap_or(false);
        if !version_ok || !method_ok || obj.contains_key("result") || obj.contains_key("error") {
            let hint = if !version_ok {
                "missing or wrong jsonrpc field; expected \"2.0\""
            } else if !method_ok {
                "request must carry a non-empty string method"
            } else {
                "responses are not accepted from clients"
            };
            // §2.2: errors from non-Request input always carry "id":null.
            self.send_frame(error_response(
                Value::Null,
                &RpcError::invalid_request(hint),
            ));
            return Verdict::Continue;
        }
        let method = obj["method"].as_str().unwrap().to_string();
        if let Some(p) = obj.get("params") {
            if !p.is_object() && !p.is_null() {
                self.send_frame(error_response(
                    id.unwrap_or(Value::Null),
                    &RpcError::invalid_params("params must be an object (named params only)"),
                ));
                return Verdict::Continue;
            }
        }
        let params = obj.get("params").cloned();

        match id {
            None => {
                match self.dispatch(&method, params).await {
                    Ok(_) => {
                        if let Some(hook) = self.after_response.take() {
                            hook();
                        }
                    }
                    Err(e) => tracing::warn!(method, err = %e, "notification processing failed"),
                }
                Verdict::Continue
            }
            Some(id) => {
                let res = self.dispatch(&method, params).await;
                let ok = res.is_ok();
                let frame = match res {
                    Ok(result) => result_response(id, result),
                    Err(e) => error_response(id, &e),
                };
                if !self.send_frame(frame) {
                    return Verdict::Close;
                }
                if ok {
                    if let Some(hook) = self.after_response.take() {
                        hook();
                    }
                    if method == "session.hello" && self.helloed {
                        methods::post_hello_backlog(&self.engine, self.conn_id).await;
                    }
                }
                Verdict::Continue
            }
        }
    }

    fn send_frame(&self, frame: Value) -> bool {
        match self.out.try_send(OutMsg::Frame(frame.to_string())) {
            Ok(()) => true,
            Err(_) => {
                self.dead.store(true, Ordering::SeqCst);
                false
            }
        }
    }

    async fn dispatch(&mut self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        if self.engine.is_shutting_down() {
            return Err(RpcError::internal("server shutting down"));
        }
        let Some(def) = describe::method_def(method) else {
            return Err(RpcError::unknown_method());
        };
        if !self.helloed && !def.scope.is_empty() {
            return Err(RpcError::pre_hello());
        }
        if !def.scope.is_empty() && !self.scopes.contains("*") && !self.scopes.contains(def.scope) {
            return Err(RpcError::new(
                ErrorKind::UnauthorizedScope,
                format!(
                    "scope '{}' required; reconnect with a broader grant",
                    def.scope
                ),
            ));
        }
        methods::dispatch_method(&self.engine.clone(), self, method, params).await
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.engine.events.remove_conn(self.conn_id);
    }
}

/// One client over any byte stream: read frames, route, write+flush. Shared
/// by the stdio binding and every accepted unix-socket connection.
pub async fn serve_connection<R, W>(
    engine: Arc<Engine>,
    reader: R,
    writer: W,
    transport: Transport,
) -> io::Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (mut sess, mut rx) = engine.connect(transport);
    let wake = sess.wake.clone();
    let shutdown_wake = engine.shutdown_handle();

    let writer_task = tokio::spawn(async move {
        let mut w = FrameWriter::new(writer);
        while let Some(msg) = rx.recv().await {
            let frame = match msg {
                OutMsg::Frame(f) => f,
                OutMsg::Hangup => break,
            };
            if w.write_frame(frame.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut lines = LineReader::with_cap(reader, engine.cfg.max_frame_bytes);
    loop {
        tokio::select! {
            _ = wake.notified() => break,
            _ = shutdown_wake.notified() => break,
            res = lines.read_line() => {
                match res {
                    Ok(None) => break,
                    Ok(Some(LineRead::Oversized)) => {
                        // Final frame before the mandated close (AGENT-API §1.3).
                        sess.send_frame(error_response(
                            Value::Null,
                            &RpcError::new(ErrorKind::ParseError, "line exceeds 4 MiB frame cap"),
                        ));
                        break;
                    }
                    Ok(Some(LineRead::Line(line))) => {
                        if matches!(sess.handle_line(&line).await, Verdict::Close)
                            || sess.is_dead()
                            || engine.is_shutting_down()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    drop(sess);
    let _ = writer_task.await;
    Ok(())
}
