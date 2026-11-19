#![allow(dead_code)] // #[path] includes expose items other tests use

#[path = "../src/store.rs"]
mod store;

use serde_json::json;
use store::*;
use tempfile::TempDir;

const SECRET: [u8; 32] = [0x42; 32];
const PEER: [u8; 32] = [0x07; 32];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn open(dir: &TempDir) -> Store {
    Store::open(dir.path(), &SECRET).unwrap()
}

fn msg(msg_id: &str, chat_id: &str, dir: Dir, body: &str, ts: i64) -> NewMessage {
    NewMessage {
        msg_id: msg_id.to_string(),
        chat_id: chat_id.to_string(),
        dir,
        kind: MsgKind::Text,
        body: body.to_string(),
        state: match dir {
            Dir::Out => Some(MsgState::Queued),
            Dir::In => Some(MsgState::Delivered),
        },
        client_msg_id: None,
        ts,
        expires_at: None,
    }
}

#[test]
fn window_pagination_no_overlap() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();

    let mut expected = Vec::new();
    for i in 0..500 {
        // Tight ts spacing plus a same-ts cluster at i in [490,500) to
        // exercise the (ts, msg_id) tiebreak across a page boundary.
        let ts = 1_000_000 + if i < 490 { i * 10 } else { 4900 };
        let id = format!("msg:{i:04}");
        expected.push(id.clone());
        s.insert_message(&msg(&id, "chat:a", Dir::In, &format!("body {i}"), ts))
            .unwrap();
    }
    expected.reverse(); // newest-first, ts desc; msg_id desc on ties
    let mut tie: Vec<String> = expected.split_off(490);
    tie.sort_unstable_by(|a, b| b.cmp(a));
    expected.extend(tie);

    let plan = s.window_plan("chat:a").unwrap();
    assert!(
        plan.contains("SEARCH") && plan.contains("messages_chat_ts"),
        "window must be index-bounded, got:\n{plan}"
    );
    assert!(!plan.contains("SCAN messages"), "full scan:\n{plan}");

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let (page, next) = s.window("chat:a", cursor.as_deref(), 50).unwrap();
        assert!(page.len() <= 50, "page exceeded limit");
        pages += 1;
        seen.extend(page.iter().map(|m| m.msg_id.clone()));
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
        assert!(pages <= 11, "runaway pagination");
    }
    assert_eq!(pages, 10, "500 msgs / 50 per page");
    assert_eq!(seen.len(), 500);
    let mut dedup_check = seen.clone();
    dedup_check.sort();
    dedup_check.dedup();
    assert_eq!(
        dedup_check.len(),
        500,
        "message appeared twice across pages"
    );
    assert_eq!(seen, expected, "newest-first order");
}

#[test]
fn window_cursor_bound_to_chat() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();
    s.upsert_chat("chat:b", &[0x09; 32], None, false).unwrap();
    for i in 0..120 {
        s.insert_message(&msg(&format!("msg:a{i}"), "chat:a", Dir::In, "x", i))
            .unwrap();
        s.insert_message(&msg(&format!("msg:b{i}"), "chat:b", Dir::In, "x", i))
            .unwrap();
    }
    let (_, cur) = s.window("chat:a", None, 50).unwrap();
    let err = s.window("chat:b", cur.as_deref(), 50);
    assert!(matches!(err, Err(StoreError::InvalidCursor)));
    assert!(matches!(
        s.window("chat:a", Some("cur:!!!not-base64!!!"), 50),
        Err(StoreError::InvalidCursor)
    ));
}

#[test]
fn dedup_returns_same_msg_id() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();

    let mut m = msg("msg:first", "chat:a", Dir::Out, "hello", 1000);
    m.client_msg_id = Some("cmid-1".to_string());
    let (id1, deduped1) = s.insert_message(&m).unwrap();
    assert_eq!((id1.as_str(), deduped1), ("msg:first", false));

    let mut retry = m.clone();
    retry.msg_id = "msg:second".to_string(); // caller regenerates on retry
    let (id2, deduped2) = s.insert_message(&retry).unwrap();
    assert_eq!((id2.as_str(), deduped2), ("msg:first", true));

    retry.body = "different payload".to_string();
    assert!(matches!(
        s.insert_message(&retry),
        Err(StoreError::Conflict)
    ));

    let (page, _) = s.window("chat:a", None, 50).unwrap();
    assert_eq!(page.len(), 1, "dedup must not double-insert");
}

#[test]
fn state_transitions_enforced() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();
    s.insert_message(&msg("msg:m", "chat:a", Dir::Out, "hi", 1000))
        .unwrap();

    s.update_state("msg:m", MsgState::Sent).unwrap();
    s.update_state("msg:m", MsgState::Delivered).unwrap();
    let hist = s.state_history("msg:m").unwrap();
    assert_eq!(
        hist.iter().map(|(st, _)| *st).collect::<Vec<_>>(),
        vec![MsgState::Sent, MsgState::Delivered]
    );

    assert!(matches!(
        s.update_state("msg:m", MsgState::Queued),
        Err(StoreError::InvalidTransition { .. })
    ));
    assert!(matches!(
        s.update_state("msg:m", MsgState::Read),
        Err(StoreError::InvalidTransition { .. })
    ));

    s.update_state("msg:m", MsgState::Failed).unwrap();
    assert!(matches!(
        s.update_state("msg:m", MsgState::Sent),
        Err(StoreError::InvalidTransition { .. })
    ));

    s.insert_message(&msg("msg:q", "chat:a", Dir::Out, "hi2", 2000))
        .unwrap();
    s.update_state("msg:q", MsgState::Delivered).unwrap(); // skip allowed
    s.update_state("msg:q", MsgState::Expired).unwrap();
    assert!(matches!(
        s.update_state("msg:q", MsgState::Queued),
        Err(StoreError::InvalidTransition { .. })
    ));
}

#[test]
fn event_log_bounded_replay_with_gap() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    for i in 0..20_000u64 {
        s.append_event("message.received", Some("chat:a"), &json!({"i": i}))
            .unwrap();
    }

    let latest = s.latest_seq().unwrap();
    let oldest = s.oldest_seq().unwrap();
    assert_eq!(latest, 20_000);
    assert_eq!(oldest, 20_000 - 16384 + 1);
    assert_eq!(
        s.meta("oldest_event_seq").unwrap().as_deref(),
        Some(oldest.to_string().as_str())
    );

    // Expired position: gap marker semantics + newest retained events.
    let page = s.events_since(0, 16384).unwrap();
    assert!(page.gap);
    assert_eq!(page.events.len() as u64, latest - oldest + 1);
    assert_eq!(page.events.first().unwrap().seq, oldest);
    assert_eq!(page.events.last().unwrap().seq, latest);
    assert!(
        page.events.windows(2).all(|w| w[1].seq == w[0].seq + 1),
        "replayed seqs must be contiguous"
    );

    // Boundary: since = oldest-1 replays without a gap.
    let edge = s.events_since(oldest - 1, 5).unwrap();
    assert!(!edge.gap);
    assert_eq!(edge.events.first().unwrap().seq, oldest);

    // In-window resume: no gap, starts exactly at since+1.
    let mid = s.events_since(19_000, 100).unwrap();
    assert!(!mid.gap);
    assert_eq!(mid.events.first().unwrap().seq, 19_001);

    // Never renumbers: seq survives pruning of earlier rows.
    let last = s.append_event("ping", None, &json!(null)).unwrap();
    assert_eq!(last, 20_001);

    assert!(matches!(
        s.events_since(last + 1, 10),
        Err(StoreError::InvalidCursor)
    ));
}

#[test]
fn encryption_at_rest_and_reopen() {
    let needle = "ZEBRA-NEEDLE-plaintext-must-never-appear";
    let dir = TempDir::new().unwrap();
    {
        let s = open(&dir);
        s.upsert_chat("chat:a", &PEER, None, false).unwrap();
        s.insert_message(&msg("msg:secret", "chat:a", Dir::In, needle, 1000))
            .unwrap();
    }
    for name in ["cinder.db", "cinder.db-wal", "cinder.db-shm"] {
        let p = dir.path().join(name);
        if !p.exists() {
            continue;
        }
        let bytes = std::fs::read(&p).unwrap();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "plaintext needle found in {name}"
        );
    }
    let db = std::fs::read(dir.path().join("cinder.db")).unwrap();
    assert!(
        !db.starts_with(b"SQLite format 3"),
        "sqlite header visible: db not encrypted"
    );

    let s = open(&dir);
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert_eq!(page[0].body, needle);
    drop(s);

    let wrong = Store::open(dir.path(), &[0x99; 32]);
    assert!(wrong.is_err(), "wrong key must fail to open");
}

#[test]
fn file_permissions_0600_0700() {
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("vault");
    let s = Store::open(&store_dir, &SECRET).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dm = std::fs::metadata(&store_dir).unwrap().permissions().mode();
        let fm = std::fs::metadata(s.db_path()).unwrap().permissions().mode();
        assert_eq!(dm & 0o777, 0o700, "dir mode {dm:o}");
        assert_eq!(fm & 0o777, 0o600, "db mode {fm:o}");
    }
    let _ = s;
}

#[test]
fn chat_list_unread_and_read_marker() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, Some("alpha"), true).unwrap();
    s.upsert_chat("chat:b", &[0x09; 32], None, false).unwrap();
    let t = 1_900_000_000_000i64; // > now_ms() so activity ordering is deterministic
    s.insert_message(&msg("msg:a1", "chat:a", Dir::In, "old", t + 1000))
        .unwrap();
    s.insert_message(&msg("msg:a2", "chat:a", Dir::In, "new", t + 3000))
        .unwrap();
    s.insert_message(&msg("msg:b1", "chat:b", Dir::In, "mid", t + 2000))
        .unwrap();

    let chats = s.list_chats_sorted(10).unwrap();
    assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].chat_id, "chat:a", "activity-sorted newest first");
    assert_eq!(chats[0].unread, 2);
    assert!(chats[0].verified);
    assert_eq!(chats[0].last_message.as_ref().unwrap().msg_id, "msg:a2");
    assert_eq!(chats[1].unread, 1);

    s.mark_read_upto("chat:a", t + 3000).unwrap();
    let chats = s.list_chats_sorted(10).unwrap();
    assert_eq!(chats[0].unread, 0);
    assert_eq!(chats[0].read_upto, t + 3000);
    assert!(matches!(
        s.mark_read_upto("chat:missing", 1),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn reactions_edit_and_soft_delete() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();
    s.insert_message(&msg("msg:m", "chat:a", Dir::In, "hello", 1000))
        .unwrap();

    s.add_reaction("msg:m", "🫡", &PEER).unwrap();
    s.add_reaction("msg:m", "👍", &[0x11; 32]).unwrap();
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert_eq!(page[0].reactions.len(), 2);
    s.remove_reaction("msg:m", Some("👍"), &[0x11; 32]).unwrap();
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert_eq!(page[0].reactions.len(), 1);

    s.edit_message("msg:m", "hello edited").unwrap();
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert_eq!(page[0].body, "hello edited");
    assert_eq!(page[0].revision, 1);
    assert!(page[0].edited_at.is_some());

    s.soft_delete("msg:m").unwrap();
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert!(page[0].deleted);
    assert_eq!(page[0].body, "");
    assert!(matches!(
        s.edit_message("msg:m", "nope"),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn meta_roundtrip() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    assert_eq!(s.meta("protocol_version").unwrap().as_deref(), Some("1"));
    s.set_meta("account_peer_id", "peer:abc").unwrap();
    assert_eq!(
        s.meta("account_peer_id").unwrap().as_deref(),
        Some("peer:abc")
    );
    assert!(s.meta("missing").unwrap().is_none());
}

#[test]
fn fts_search() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();
    s.insert_message(&msg(
        "msg:1",
        "chat:a",
        Dir::In,
        "rendezvous at the port",
        1000,
    ))
    .unwrap();
    s.insert_message(&msg("msg:2", "chat:a", Dir::In, "unrelated text", 2000))
        .unwrap();
    s.insert_message(&msg("msg:3", "chat:a", Dir::In, "rendezvous moved", 3000))
        .unwrap();

    let hits = s.search("rendezvous", Some("chat:a"), 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].msg_id, "msg:3", "newest first");
    assert!(s
        .search("rendezvous", None, 10)
        .unwrap()
        .iter()
        .all(|m| m.chat_id == "chat:a"));
    assert!(s.search("nonexistent", None, 10).unwrap().is_empty());
}

#[test]
fn expire_due_tombstones_and_unwinds_unread() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();
    let now = now_ms();

    let mut dying = msg("msg:ephem", "chat:a", Dir::In, "vanish", now);
    dying.expires_at = Some(now + 1_000);
    s.insert_message(&dying).unwrap();
    let mut live = msg("msg:keep", "chat:a", Dir::In, "stay", now);
    live.expires_at = Some(now + 60_000);
    s.insert_message(&live).unwrap();
    s.insert_message(&msg("msg:plain", "chat:a", Dir::In, "no ttl", now))
        .unwrap();
    // one outbound row with a deadline too — sweep tombstones both dirs
    let mut out = msg("msg:out", "chat:a", Dir::Out, "mine", now);
    out.expires_at = Some(now + 1_000);
    s.insert_message(&out).unwrap();

    // before the deadline: both expiring rows visible with their deadline
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert_eq!(page.len(), 4);
    let dying_row = page.iter().find(|m| m.msg_id == "msg:ephem").unwrap();
    assert_eq!(dying_row.expires_at, Some(now + 1_000));
    assert!(!dying_row.deleted);
    assert!(s.expire_due(now + 999).unwrap().is_empty());

    // past the deadline: sweep tombstones + unwinds the unread inbound
    let gone = s.expire_due(now + 1_000).unwrap();
    assert_eq!(gone.len(), 2);
    assert!(gone.contains(&("chat:a".to_string(), "msg:ephem".to_string())));
    assert!(gone.contains(&("chat:a".to_string(), "msg:out".to_string())));

    // audit: the row survives as a tombstone with body cleared, deadline kept;
    // both non-terminal states (delivered in, queued out) transition + log
    let t = s.get_message("msg:ephem").unwrap().unwrap();
    assert!(t.deleted);
    assert_eq!(t.body, "");
    assert_eq!(t.expires_at, Some(now + 1_000));
    assert_eq!(t.state, Some(MsgState::Expired));
    let t = s.get_message("msg:out").unwrap().unwrap();
    assert_eq!(t.state, Some(MsgState::Expired));
    for mid in ["msg:ephem", "msg:out"] {
        let hist = s.state_history(mid).unwrap();
        assert_eq!(
            hist.iter().map(|(st, _)| *st).collect::<Vec<_>>(),
            vec![MsgState::Expired]
        );
    }

    // unread: 3 inbound arrived, the expiring one was still unread → back to 2
    let chats = s.list_chats_sorted(10).unwrap();
    assert_eq!(chats[0].unread, 2);

    // second sweep is a no-op — tombstones don't re-emit
    assert!(s.expire_due(now + 1_000).unwrap().is_empty());
}

#[test]
fn window_excludes_expired_rows() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();
    let now = now_ms();

    let mut past = msg("msg:gone", "chat:a", Dir::In, "too late", now - 10_000);
    past.expires_at = Some(now - 5_000); // lapsed but not yet swept
    s.insert_message(&past).unwrap();
    let mut fut = msg("msg:here", "chat:a", Dir::In, "still alive", now - 1_000);
    fut.expires_at = Some(now + 60_000);
    s.insert_message(&fut).unwrap();
    s.insert_message(&msg("msg:plain", "chat:a", Dir::In, "forever", now))
        .unwrap();

    // history hides lapsed rows even before the sweep tombstones them
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    let ids: Vec<&str> = page.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(ids, vec!["msg:plain", "msg:here"]);

    // chat.list's last_message skips the lapsed row too
    let chats = s.list_chats_sorted(10).unwrap();
    assert_eq!(chats[0].last_message.as_ref().unwrap().msg_id, "msg:plain");
}

#[test]
fn born_expired_never_displays() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();
    let now = now_ms();

    // receiver path: deadline already passed when the frame arrived —
    // persisted as a tombstone for audit, never readable, never unread.
    let mut late = msg("msg:late", "chat:a", Dir::In, "secret", now - 2_000);
    late.expires_at = Some(now - 1_000);
    s.insert_message(&late).unwrap();

    let m = s.get_message("msg:late").unwrap().unwrap();
    assert!(m.deleted);
    assert_eq!(m.body, "");
    assert_eq!(m.state, Some(MsgState::Expired));
    assert_eq!(m.expires_at, Some(now - 1_000));

    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert!(page.is_empty(), "born-expired must never render in history");
    let chats = s.list_chats_sorted(10).unwrap();
    assert_eq!(chats[0].unread, 0, "born-expired never bumps unread");
}

#[test]
fn v1_to_v2_migration_preserves_rows() {
    let dir = TempDir::new().unwrap();
    // Build a v1-shaped store by hand: same SQLCipher key derivation, the
    // pre-v2 DDL, one chat + one message. Store::open then applies v2.
    {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, &SECRET);
        let mut okm = [0u8; 32];
        hk.expand(b"cinder-store-v1", &mut okm).unwrap();
        let key_hex = data_encoding::HEXLOWER.encode(&okm);
        let conn = rusqlite::Connection::open(dir.path().join("cinder.db")).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA key = \"x'{key_hex}'\";
             CREATE TABLE schema_migrations(
                version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             INSERT INTO schema_migrations VALUES(1, 0);
             CREATE TABLE chats(
                chat_id TEXT PRIMARY KEY, peer_id BLOB UNIQUE NOT NULL, title TEXT,
                created_at INTEGER NOT NULL, last_activity INTEGER NOT NULL,
                unread INTEGER NOT NULL DEFAULT 0, verified INTEGER NOT NULL DEFAULT 0,
                read_upto INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE messages(
                msg_id TEXT PRIMARY KEY, chat_id TEXT NOT NULL,
                dir TEXT NOT NULL CHECK(dir IN('in','out')), kind TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '', state TEXT, client_msg_id TEXT,
                ts INTEGER NOT NULL, edited_at INTEGER,
                revision INTEGER NOT NULL DEFAULT 0, deleted INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE reactions(
                msg_id TEXT NOT NULL, emoji TEXT NOT NULL, reactor BLOB NOT NULL,
                ts INTEGER NOT NULL, PRIMARY KEY(msg_id, emoji, reactor));"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO chats VALUES('chat:a', ?1, NULL, 0, 0, 1, 0, 0)",
            rusqlite::params![PEER.as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages(msg_id, chat_id, dir, kind, body, state, ts)
             VALUES('msg:old', 'chat:a', 'in', 'text', 'pre-migration', 'delivered', 42)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }

    let s = open(&dir);
    // v2 columns landed and the pre-migration rows survived untouched
    let chats = s.list_chats_sorted(10).unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].disappear_ttl_ms, 0);
    assert_eq!(chats[0].unread, 1);
    let m = s.get_message("msg:old").unwrap().unwrap();
    assert_eq!(m.body, "pre-migration");
    assert_eq!(m.expires_at, None);
    let (page, _) = s.window("chat:a", None, 10).unwrap();
    assert_eq!(page.len(), 1);

    // migrated db takes v2 features: chat default + per-message deadlines
    s.set_disappearing("chat:a", 5_000).unwrap();
    assert_eq!(s.disappearing_ttl("chat:a").unwrap(), Some(5_000));
    let mut eph = msg("msg:new", "chat:a", Dir::In, "post-migration", now_ms());
    eph.expires_at = Some(now_ms() + 60_000);
    s.insert_message(&eph).unwrap();
    assert_eq!(
        s.get_message("msg:new").unwrap().unwrap().expires_at,
        eph.expires_at
    );
}

#[test]
fn chat_disappearing_default_roundtrip() {
    let dir = TempDir::new().unwrap();
    let s = open(&dir);
    s.upsert_chat("chat:a", &PEER, None, false).unwrap();

    assert_eq!(s.disappearing_ttl("chat:a").unwrap(), None);
    assert_eq!(s.list_chats_sorted(10).unwrap()[0].disappear_ttl_ms, 0);

    s.set_disappearing("chat:a", 30_000).unwrap();
    assert_eq!(s.disappearing_ttl("chat:a").unwrap(), Some(30_000));
    assert_eq!(s.list_chats_sorted(10).unwrap()[0].disappear_ttl_ms, 30_000);

    // ttl 0 disables; upsert_chat never stomps the setting
    s.upsert_chat("chat:a", &PEER, Some("alpha"), false)
        .unwrap();
    assert_eq!(s.disappearing_ttl("chat:a").unwrap(), Some(30_000));
    s.set_disappearing("chat:a", 0).unwrap();
    assert_eq!(s.disappearing_ttl("chat:a").unwrap(), None);

    assert!(matches!(
        s.set_disappearing("chat:missing", 1),
        Err(StoreError::NotFound(_))
    ));
}
