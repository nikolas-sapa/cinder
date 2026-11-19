//! FT coverage for the file/image transfer feature (files-images.md §13):
//! chunk staging round-trip, resume bitmap persistence, hash-mismatch
//! detection, range geometry, staging-cap accounting, save-path
//! sanitization, thumbnail bombs, voice metadata, event throttle,
//! cancel/reaper flows, and resume across a session death.
#![allow(dead_code)]

#[path = "../src/call.rs"]
mod call;
#[path = "../src/core.rs"]
mod core;
#[path = "../src/rpc/mod.rs"]
mod rpc;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/tui/mod.rs"]
mod tui;
#[path = "../src/xfer.rs"]
mod xfer;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cinder_core::identity::Identity;
use secrecy::ExposeSecret;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use crate::core::{chat_str, contact_str, Core, MetaState};
use crate::rpc::core::{
    CinderCore, ContactIdParams, EventSink, FileIdParams, FileSaveParams, FileSendParams,
};
use crate::store::{Dir, NewFile, Store};
use crate::tui::backend as tb;

const CONNECT_WAIT: Duration = Duration::from_secs(10);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(10);
const DELIVERY_WAIT: Duration = Duration::from_secs(20);

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xfer=debug".into()),
        )
        .try_init();
}

fn sha256_bytes(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

fn relay_hostport(a: std::net::SocketAddr) -> (std::net::SocketAddr, String) {
    (a, a.to_string())
}

async fn spawn_relay() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = cinder_relay::server::serve(listener, cinder_relay::server::RelayConfig::default())
            .await;
    });
    addr
}

async fn open_core(dir: &Path, relay: std::net::SocketAddr) -> Arc<Core> {
    Core::open(
        dir,
        Some(relay_hostport(relay)),
        Identity::generate().unwrap(),
    )
    .await
    .expect("core open")
}

async fn open_local(dir: &Path) -> Arc<Core> {
    Core::open(dir, None, Identity::generate().unwrap())
        .await
        .expect("core open")
}

async fn reopen_core(dir: &Path, relay: std::net::SocketAddr, blob: &[u8]) -> Arc<Core> {
    Core::open(
        dir,
        Some(relay_hostport(relay)),
        Identity::from_stored_blob(blob).expect("identity clone"),
    )
    .await
    .expect("core open")
}

async fn paired(relay: std::net::SocketAddr, dir_a: &Path, dir_b: &Path) -> (Arc<Core>, Arc<Core>) {
    let a = open_core(dir_a, relay).await;
    let b = open_core(dir_b, relay).await;
    assert!(a.wait_connected(CONNECT_WAIT).await);
    assert!(b.wait_connected(CONNECT_WAIT).await);
    let (link, _) = a.invite_link(3600).await.expect("invite.create");
    let peer_a = b
        .clone()
        .start_add(&link, None)
        .await
        .expect("invite.accept");
    assert!(
        b.wait_meta(&peer_a, MetaState::Active, HANDSHAKE_WAIT)
            .await
    );
    let peer_b = b.identity().peer_id();
    assert!(
        a.wait_meta(&peer_b, MetaState::PendingIn, HANDSHAKE_WAIT)
            .await
    );
    a.contact_accept(ContactIdParams {
        contact_id: contact_str(&peer_b),
    })
    .await
    .expect("contact.accept");
    assert!(
        a.wait_meta(&peer_b, MetaState::Active, HANDSHAKE_WAIT)
            .await
    );
    (a, b)
}

async fn stop(core: &Arc<Core>) {
    core.begin_shutdown();
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < Duration::from_secs(10) {
        if core.conn_state() == tb::ConnState::Offline {
            return;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
}

async fn wait_for<T>(mut f: impl FnMut() -> Option<T>, dur: Duration, what: &str) -> T {
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < dur {
        if let Some(t) = f() {
            return t;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    panic!("timed out waiting for {what}");
}

/// Poll `file.list` until a row matching `pred` appears.
async fn wait_file(
    core: &Arc<Core>,
    pred: impl Fn(&Value) -> bool + Send,
    dur: Duration,
    what: &str,
) -> Value {
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < dur {
        if let Ok(v) = core
            .file_list(crate::rpc::core::FileListParams {
                chat_id: None,
                state: None,
                cursor: None,
                limit: Some(50),
            })
            .await
        {
            if let Some(r) = v["files"]
                .as_array()
                .and_then(|fs| fs.iter().find(|f| pred(f)))
            {
                return r.clone();
            }
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    panic!("timed out waiting for {what}");
}

fn test_row(file_id: &str, dir: Dir, size: i64, chunks: i64, state: &str) -> NewFile {
    NewFile {
        file_id: file_id.into(),
        msg_id: format!("msg:{file_id}"),
        chat_id: "chat:deadbeef".into(),
        dir,
        name: "n.bin".into(),
        mime: "application/octet-stream".into(),
        size,
        sha256: vec![7u8; 32],
        chunk_size: 7168,
        chunks_total: chunks,
        state: state.into(),
        kind: "file".into(),
        thumb: None,
        audio_meta: None,
        fetch_token: Some(vec![9u8; 32]),
        caption: None,
        staged_path: None,
        client_xfer_id: None,
    }
}

// ---------------------------------------------------------------------------
// FT1: split → stage → reassemble → digest for edge sizes.
// ---------------------------------------------------------------------------
#[test]
fn ft1_chunked_staging_roundtrip() {
    let caps = xfer::XferCaps::default();
    for (size, want_chunks) in [(1u64, 1u64), (7_168, 1), (65_537, 10)] {
        let dir = tempfile::tempdir().unwrap();
        let src: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let rel = xfer::stage_rel(&[1u8; 32], &[2u8; 32]);
        let abs = xfer::stage_create(dir.path(), &rel, size).unwrap();
        let chunk = caps.chunk_bytes as u64;
        let mut off = 0u64;
        while off < size {
            let n = ((size - off).min(chunk)) as usize;
            xfer::stage_write_at(&abs, off, &src[off as usize..off as usize + n]).unwrap();
            off += n as u64;
        }
        assert_eq!(xfer::stage_sha256(&abs).unwrap(), sha256_bytes(&src));
        assert_eq!(size.div_ceil(chunk), want_chunks);
    }
}

// ---------------------------------------------------------------------------
// FT2 (persistence half): the resume bitmap + state survive a store reopen.
// ---------------------------------------------------------------------------
#[test]
fn ft2_resume_bitmap_persists() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Store::open(dir.path(), &[3u8; 32]).unwrap();
        let f = test_row("file:aa", Dir::In, 28_672, 4, "transferring");
        s.insert_file(&f).unwrap();
        let mut have = xfer::bitmap_new(4);
        xfer::bit_set(&mut have, 0);
        xfer::bit_set(&mut have, 2);
        s.set_file_have("file:aa", &have).unwrap();
        s.set_file_staged("file:aa", "xfer/blob.part").unwrap();
    }
    let s = Store::open(dir.path(), &[3u8; 32]).unwrap();
    let row = s.get_file("file:aa").unwrap().unwrap();
    assert_eq!(row.state, "transferring");
    assert!(xfer::bit_get(&row.have, 0));
    assert!(!xfer::bit_get(&row.have, 1));
    assert!(xfer::bit_get(&row.have, 2));
    assert_eq!(row.staged_path.as_deref(), Some("xfer/blob.part"));
    assert!(
        s.files_resumable()
            .unwrap()
            .iter()
            .any(|r| r.file_id == "file:aa"),
        "incomplete inbound transfer must be resumable after reopen"
    );
}

// ---------------------------------------------------------------------------
// FT3: a flipped byte in the .part flips the finalize digest (hash_mismatch).
// ---------------------------------------------------------------------------
#[test]
fn ft3_corrupt_part_detected() {
    let dir = tempfile::tempdir().unwrap();
    let src: Vec<u8> = (0..9000u64).map(|i| (i % 253) as u8).collect();
    let rel = xfer::stage_rel(&[4u8; 32], &[5u8; 32]);
    let abs = xfer::stage_create(dir.path(), &rel, src.len() as u64).unwrap();
    xfer::stage_write_at(&abs, 0, &src).unwrap();
    assert_eq!(xfer::stage_sha256(&abs).unwrap(), sha256_bytes(&src));
    xfer::stage_write_at(&abs, 1234, &[src[1234] ^ 0xff]).unwrap();
    assert_ne!(xfer::stage_sha256(&abs).unwrap(), sha256_bytes(&src));
}

// ---------------------------------------------------------------------------
// FT4: chunk geometry — no phantom tail chunk, OOB/inverted ranges rejected.
// ---------------------------------------------------------------------------
#[test]
fn ft4_range_geometry() {
    assert_eq!((2 * 7_168u64).div_ceil(7_168), 2); // exact multiple: no tail
    assert_eq!((2 * 7_168u64 + 1).div_ceil(7_168), 3);
    // merge_ranges: inverted and out-of-bounds rejected wholesale.
    let mut m = xfer::bitmap_new(3);
    assert!(xfer::merge_ranges(&mut m, 3, &[(0, 1), (1, 2)]).is_ok());
    assert!(xfer::all_set(&m, 3));
    let mut m2 = xfer::bitmap_new(3);
    assert!(xfer::merge_ranges(&mut m2, 3, &[(2, 1)]).is_err());
    assert!(xfer::merge_ranges(&mut m2, 3, &[(0, 4)]).is_err());
    assert_eq!(xfer::popcount(&m2), 0); // nothing merged on malformed input
                                        // ranges_of: coalesced inclusive ranges for the wire.
    let mut m3 = xfer::bitmap_new(8);
    xfer::bit_set(&mut m3, 0);
    xfer::bit_set(&mut m3, 1);
    xfer::bit_set(&mut m3, 5);
    assert_eq!(xfer::ranges_of(&m3, 8), vec![(0, 1), (5, 5)]);
    // bitmap bounds
    let mut b = xfer::bitmap_new(4);
    assert!(!xfer::bit_get(&b, 4));
    xfer::bit_set(&mut b, 3);
    assert!(xfer::bit_get(&b, 3));
    assert!(!xfer::bit_get(&b, 4));
}

// ---------------------------------------------------------------------------
// FT7 (accounting half): file_staging_bytes counts active inbound rows only;
// file_inflight is dir-scoped.
// ---------------------------------------------------------------------------
#[test]
fn ft7_staging_and_inflight_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), &[8u8; 32]).unwrap();
    s.insert_file(&test_row("file:i1", Dir::In, 10_000, 2, "transferring"))
        .unwrap();
    s.insert_file(&test_row("file:i2", Dir::In, 20_000, 3, "accepted"))
        .unwrap();
    s.insert_file(&test_row("file:io", Dir::In, 50_000, 7, "offered"))
        .unwrap();
    s.insert_file(&test_row("file:o1", Dir::Out, 30_000, 5, "transferring"))
        .unwrap();
    // offered is not staged yet; outbound rows never stage inbound.
    assert_eq!(s.file_staging_bytes().unwrap(), 30_000);
    // offered still occupies an inflight slot (active state).
    assert_eq!(s.file_inflight("in", None).unwrap(), 3);
    assert_eq!(s.file_inflight("out", None).unwrap(), 1);
    assert_eq!(s.file_inflight("in", Some("chat:deadbeef")).unwrap(), 3);
    assert_eq!(s.file_inflight("in", Some("chat:other")).unwrap(), 0);
}

// ---------------------------------------------------------------------------
// FT8: save-name sanitization + collision suffixes; file.save through the
// real core stays inside the destination dir even for hostile names.
// ---------------------------------------------------------------------------
#[test]
fn ft8_sanitize_names() {
    let cases = [
        ("../etc/passwd", "passwd"),
        ("a/b/../c.pdf", "c.pdf"),
        ("..\\win\\evil.exe", "evil.exe"),
        ("...", "file.bin"),
        ("", "file.bin"),
        ("safe name-1.txt", "safe name-1.txt"),
        ("uni\u{0}\u{1}code.bin", "unicode.bin"),
    ];
    for (raw, want) in cases {
        assert_eq!(xfer::sanitize_name(raw), want, "raw={raw:?}");
    }
    assert_eq!(xfer::collision_name("a.txt", 0), "a.txt");
    assert_eq!(xfer::collision_name("a.txt", 2), "a-2.txt");
    assert_eq!(xfer::collision_name("noext", 3), "noext-3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ft8_file_save_containment() {
    let data = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let core = open_local(data.path()).await;

    // Seed a complete inbound row + staged blob.
    let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let rel = xfer::stage_rel(&[6u8; 32], &[7u8; 32]);
    let abs = xfer::stage_create(data.path(), &rel, payload.len() as u64).unwrap();
    xfer::stage_write_at(&abs, 0, &payload).unwrap();
    let mut row = test_row("file:save1", Dir::In, payload.len() as i64, 1, "complete");
    row.name = "../../evil/../report.pdf".into();
    row.sha256 = sha256_bytes(&payload).to_vec();
    row.staged_path = Some(rel);
    core.store().insert_file(&row).unwrap();

    let saved = core
        .file_save(FileSaveParams {
            file_id: "file:save1".into(),
            dir: Some(out.path().to_string_lossy().to_string()),
            name: None,
        })
        .await
        .expect("file.save");
    let saved_path = std::path::PathBuf::from(saved["path"].as_str().unwrap());
    let canon_out = std::fs::canonicalize(out.path()).unwrap();
    assert_eq!(saved_path.parent(), Some(canon_out.as_path()));
    assert_eq!(
        saved_path.file_name().unwrap().to_str().unwrap(),
        "report.pdf"
    );
    assert_eq!(std::fs::read(&saved_path).unwrap(), payload);

    // Name override is sanitized too, and a second save collides → -1 suffix.
    let again = core
        .file_save(FileSaveParams {
            file_id: "file:save1".into(),
            dir: Some(out.path().to_string_lossy().to_string()),
            name: Some("../hack.bin".into()),
        })
        .await
        .expect("second save");
    let p2 = std::path::PathBuf::from(again["path"].as_str().unwrap());
    assert_eq!(p2.file_name().unwrap().to_str().unwrap(), "hack.bin");

    // Symlinked destination → conflict, not an escape.
    #[cfg(unix)]
    {
        let linkdir = data.path().join("linked");
        std::os::unix::fs::symlink(out.path(), &linkdir).unwrap();
        let err = core
            .file_save(FileSaveParams {
                file_id: "file:save1".into(),
                dir: Some(linkdir.to_string_lossy().to_string()),
                name: None,
            })
            .await
            .expect_err("symlinked save dir must fail");
        assert_eq!(err.kind, rpc::protocol::ErrorKind::Conflict);
    }
    stop(&core).await;
}

// ---------------------------------------------------------------------------
// FT9: thumbnail dimension bomb → None; legit image → bounded JPEG.
// ---------------------------------------------------------------------------
#[test]
fn ft9_thumb_dimension_bomb() {
    let caps = xfer::XferCaps::default();
    let dir = tempfile::tempdir().unwrap();

    // Craft a PNG with 100_000 × 100_000 IHDR.
    let mut png = Vec::new();
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    png.extend_from_slice(&(ihdr.len() as u32).to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&ihdr);
    png.extend_from_slice(&0u32.to_be_bytes()); // bogus crc — decode fails anyway
    let bomb = dir.path().join("bomb.png");
    std::fs::write(&bomb, &png).unwrap();
    assert_eq!(xfer::make_thumb(&bomb, &caps), None);
    assert_eq!(xfer::make_thumb(&dir.path().join("nope.png"), &caps), None);

    // A real image gets a bounded thumbnail.
    let img = image::RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let real = dir.path().join("real.png");
    image::DynamicImage::ImageRgb8(img).save(&real).unwrap();
    let t = xfer::make_thumb(&real, &caps).expect("thumb");
    assert!(t.len() <= caps.thumb_max_bytes);
    // A non-image never produces a thumb.
    let txt = dir.path().join("x.txt");
    std::fs::write(&txt, b"hello").unwrap();
    assert_eq!(xfer::make_thumb(&txt, &caps), None);
}

// ---------------------------------------------------------------------------
// FT11 (storage half): voice/audio metadata round-trips through the store.
// ---------------------------------------------------------------------------
#[test]
fn ft11_audio_meta_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), &[9u8; 32]).unwrap();
    let mut f = test_row("file:vo", Dir::Out, 48_000, 7, "offered");
    f.kind = "audio".into();
    f.audio_meta = Some(json!({"duration_ms": 4200, "waveform": [0, 4, 9, 2]}).to_string());
    s.insert_file(&f).unwrap();
    let row = s.get_file("file:vo").unwrap().unwrap();
    assert_eq!(row.kind, "audio");
    let meta: Value = serde_json::from_str(row.audio_meta.as_deref().unwrap()).unwrap();
    assert_eq!(meta["duration_ms"], 4200);
    assert_eq!(meta["waveform"].as_array().unwrap().len(), 4);
}

// ---------------------------------------------------------------------------
// FT12: emit_xfer coalesces progress to ≤1 event/s; transitions always emit.
// ---------------------------------------------------------------------------
struct RecSink(Mutex<Vec<(String, Value)>>);
impl EventSink for RecSink {
    fn emit(&self, kind: &str, _chat_id: Option<&str>, payload: Value) {
        self.0.lock().unwrap().push((kind.to_string(), payload));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ft12_file_state_throttle() {
    let data = tempfile::tempdir().unwrap();
    let core = open_local(data.path()).await;
    let sink = Arc::new(RecSink(Mutex::new(Vec::new())));
    core.attach_sink(sink.clone());

    // An inbound row in `transferring` anchored on a chat with ourselves.
    let chat = chat_str(&core.identity().peer_id());
    let mut f = test_row("file:em", Dir::In, 28_672, 4, "transferring");
    f.chat_id = chat.clone();
    core.store().insert_file(&f).unwrap();
    let row = core.store().get_file("file:em").unwrap().unwrap();

    // 50 rapid progress emits → at most one file_state frame.
    for _ in 0..50 {
        core.emit_xfer(&row, &[]);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let n = sink
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| k == "file_state")
        .count();
    assert!(n <= 1, "progress emits must coalesce to ≤1/s, got {n}");

    // A state transition always emits even inside the throttle window.
    let mut done = row.clone();
    done.state = "complete".into();
    core.emit_xfer(&done, &[]);
    {
        let v = sink.0.lock().unwrap();
        let last = v.iter().rfind(|(k, _)| k == "file_state").unwrap();
        assert_eq!(last.1["state"], "complete");
        assert_eq!(last.1["bytes_done"], 28_672);
    }
    stop(&core).await;
}

// ---------------------------------------------------------------------------
// FT14: cancel mid-transfer propagates: peer row → cancelled, .part freed.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ft14_cancel_mid_transfer() {
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;

    // Slow the sender so the transfer is still live when we cancel.
    a.test_caps_mut().send_pace = Duration::from_millis(60);
    let src = tempfile::NamedTempFile::new().unwrap();
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.path(), &payload).unwrap();

    let peer_a = a.identity().peer_id();
    let sent = b
        .file_send(FileSendParams {
            to: contact_str(&peer_a),
            path: Some(src.path().to_string_lossy().to_string()),
            data: None,
            name: Some("cancel-me.bin".into()),
            mime: None,
            kind: None,
            audio: None,
            caption: None,
            client_xfer_id: "ft14-x1".into(),
        })
        .await
        .expect("file.send");
    let fid = sent["file_id"].as_str().unwrap().to_string();

    // A auto-accepts (default policy "contacts") and starts receiving.
    wait_file(
        &a,
        |f| f["file_id"] == fid && (f["state"] == "transferring" || f["state"] == "accepted"),
        DELIVERY_WAIT,
        "A accepting inbound",
    )
    .await;

    b.file_cancel(FileIdParams {
        file_id: fid.clone(),
    })
    .await
    .expect("file.cancel");
    let cancelled = wait_file(
        &a,
        |f| f["file_id"] == fid && f["state"] == "cancelled",
        DELIVERY_WAIT,
        "peer cancellation",
    )
    .await;
    assert_eq!(cancelled["dir"], "in");
    // Staging blob freed on the receiver (A received the transfer).
    let parts: Vec<_> = std::fs::read_dir(data_dir_xfer(dir_a.path()))
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    assert!(
        parts.is_empty(),
        "cancel must free the receiver .part, found {parts:?}"
    );
    // A second cancel on a terminal row is a conflict, not a replay.
    let err = b
        .file_cancel(FileIdParams {
            file_id: fid.clone(),
        })
        .await
        .expect_err("cancel on cancelled must conflict");
    assert_eq!(err.code, rpc::protocol::CONFLICT);
    stop(&a).await;
    stop(&b).await;
}

fn data_dir_xfer(dir: &Path) -> std::path::PathBuf {
    dir.join("xfer")
}

// ---------------------------------------------------------------------------
// FT16: stale active rows reap → expired, .part + orphans unlinked.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ft16_reaper_expires_stale() {
    let data = tempfile::tempdir().unwrap();
    let core = open_local(data.path()).await;
    {
        let mut caps = core.test_caps_mut();
        caps.stage_ttl = Duration::ZERO;
        // The reaper's first tick fires at open (before the row exists) —
        // shorten the period so a second pass lands inside the test window.
        caps.reap_every = Duration::from_millis(1100);
    }

    // Stale inbound + a real .part + an orphan blob with no row.
    let rel = xfer::stage_rel(&[8u8; 32], &[9u8; 32]);
    let abs = xfer::stage_create(data.path(), &rel, 1024).unwrap();
    let mut f = test_row("file:stale", Dir::In, 1024, 1, "transferring");
    f.staged_path = Some(rel);
    core.store().insert_file(&f).unwrap();
    let orphan = xfer::stage_dir(data.path()).join("orphan.part");
    std::fs::write(&orphan, b"stale").unwrap();

    wait_for(
        || {
            let r = core.store().get_file("file:stale").unwrap().unwrap();
            (r.state == "expired").then_some(r)
        },
        Duration::from_secs(10),
        "reaper expiry",
    )
    .await;
    wait_for(
        || (!abs.exists() && !orphan.exists()).then_some(()),
        Duration::from_secs(10),
        ".part + orphan sweep",
    )
    .await;
    stop(&core).await;
}

// ---------------------------------------------------------------------------
// FT2 (wire half) + FT19: receiver dies mid-transfer, reopens, resumes from
// the persisted bitmap and completes with the right digest.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ft19_resume_after_receiver_death() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let blob_a = a.identity().stored_blob().expose_secret().to_vec();

    let src = tempfile::NamedTempFile::new().unwrap();
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.path(), &payload).unwrap();

    let peer_a = a.identity().peer_id();
    let sent = b
        .file_send(FileSendParams {
            to: contact_str(&peer_a),
            path: Some(src.path().to_string_lossy().to_string()),
            data: None,
            name: Some("resume.bin".into()),
            mime: None,
            kind: None,
            audio: None,
            caption: None,
            client_xfer_id: "ft19-x1".into(),
        })
        .await
        .expect("file.send");
    let fid = sent["file_id"].as_str().unwrap().to_string();

    // Wait for the first chunks to land on A, then kill the RECEIVER mid-
    // flight: the persisted bitmap + .part survive the restart and the
    // transfer resumes from the receiver's FileHave ranges.
    wait_file(
        &a,
        |f| f["file_id"] == fid && f["state"] == "transferring",
        DELIVERY_WAIT,
        "A transferring",
    )
    .await;
    stop(&a).await;
    drop(a);

    let a2 = reopen_core(dir_a.path(), relay, &blob_a).await;
    assert!(a2.wait_connected(CONNECT_WAIT).await);

    // Reaper must not kill the resumable row: give it a wide TTL anyway
    // (default is 7d, but shrink it to prove resume beats the cutoff).
    let done = wait_file(
        &a2,
        |f| f["file_id"] == fid && f["state"] == "complete",
        Duration::from_secs(60),
        "resumed completion",
    )
    .await;
    assert_eq!(done["dir"], "in");
    let got_sha = data_encoding::BASE64
        .decode(done["sha256"].as_str().unwrap().as_bytes())
        .unwrap();
    assert_eq!(got_sha, sha256_bytes(&payload).to_vec());
    // Chunks arrived exactly once each: bytes_done == size, no double writes.
    assert_eq!(done["chunks_done"], done["chunks_total"]);
    stop(&a2).await;
    stop(&b).await;
}
