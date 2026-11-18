//! Local message store: SQLCipher-encrypted SQLite at `<data_dir>/cinder.db`.
//!
//! All methods are synchronous and block on SQLite I/O. Async callers (RPC,
//! TUI) MUST invoke them inside `tokio::task::spawn_blocking`. `Store` is
//! `Send + Sync` and serializes access internally.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use data_encoding::BASE64URL_NOPAD;
use hkdf::Hkdf;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

const STORE_HKDF_INFO: &[u8] = b"cinder-store-v1";
const SCHEMA_VERSION: i64 = 1;
const EVENT_LOG_KEEP: i64 = 16384;
const DEDUP_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_WINDOW: u32 = 500;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid cursor")]
    InvalidCursor,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid state transition: {from:?} -> {to:?}")]
    InvalidTransition {
        from: Option<MsgState>,
        to: MsgState,
    },
    #[error("client_msg_id replayed with different payload")]
    Conflict,
    #[error("internal: {0}")]
    Internal(&'static str),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Dir::In => "in",
            Dir::Out => "out",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "in" => Ok(Dir::In),
            "out" => Ok(Dir::Out),
            _ => Err(StoreError::Internal("bad dir")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Text,
    Edit,
    Delete,
    Reaction,
}

impl MsgKind {
    fn as_str(self) -> &'static str {
        match self {
            MsgKind::Text => "text",
            MsgKind::Edit => "edit",
            MsgKind::Delete => "delete",
            MsgKind::Reaction => "reaction",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "text" => Ok(MsgKind::Text),
            "edit" => Ok(MsgKind::Edit),
            "delete" => Ok(MsgKind::Delete),
            "reaction" => Ok(MsgKind::Reaction),
            _ => Err(StoreError::Internal("bad kind")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgState {
    Queued,
    Sent,
    Delivered,
    Read,
    Failed,
    Expired,
}

impl MsgState {
    fn as_str(self) -> &'static str {
        match self {
            MsgState::Queued => "queued",
            MsgState::Sent => "sent",
            MsgState::Delivered => "delivered",
            MsgState::Read => "read",
            MsgState::Failed => "failed",
            MsgState::Expired => "expired",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "queued" => Ok(MsgState::Queued),
            "sent" => Ok(MsgState::Sent),
            "delivered" => Ok(MsgState::Delivered),
            "read" => Ok(MsgState::Read),
            "failed" => Ok(MsgState::Failed),
            "expired" => Ok(MsgState::Expired),
            _ => Err(StoreError::Internal("bad state")),
        }
    }

    fn rank(self) -> u8 {
        match self {
            MsgState::Queued => 0,
            MsgState::Sent => 1,
            MsgState::Delivered => 2,
            MsgState::Read => 3,
            MsgState::Failed | MsgState::Expired => 255,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, MsgState::Read | MsgState::Failed | MsgState::Expired)
    }

    fn can_reach(self, to: MsgState) -> bool {
        if self.is_terminal() {
            return false;
        }
        if to.is_terminal() {
            return to != MsgState::Read;
        }
        to.rank() > self.rank()
    }
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub msg_id: String,
    pub chat_id: String,
    pub dir: Dir,
    pub kind: MsgKind,
    pub body: String,
    pub state: Option<MsgState>,
    pub client_msg_id: Option<String>,
    pub ts: i64,
}

#[derive(Debug, Clone)]
pub struct Reaction {
    pub emoji: String,
    pub reactor: [u8; 32],
    pub ts: i64,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub msg_id: String,
    pub chat_id: String,
    pub dir: Dir,
    pub kind: MsgKind,
    pub body: String,
    pub state: Option<MsgState>,
    pub client_msg_id: Option<String>,
    pub ts: i64,
    pub revision: i64,
    pub edited_at: Option<i64>,
    pub deleted: bool,
    pub reactions: Vec<Reaction>,
}

#[derive(Debug, Clone)]
pub struct Chat {
    pub chat_id: String,
    pub peer_id: [u8; 32],
    pub title: Option<String>,
    pub created_at: i64,
    pub last_activity: i64,
    pub unread: i64,
    pub verified: bool,
    pub read_upto: i64,
    pub last_message: Option<Box<Message>>,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub seq: u64,
    pub kind: String,
    pub chat_id: Option<String>,
    pub payload: Value,
    pub ts: i64,
}

#[derive(Debug)]
pub struct EventsPage {
    pub events: Vec<Event>,
    /// True when `since` predates the retained log: caller emits `event.gap`.
    pub gap: bool,
    pub oldest_seq: u64,
    pub latest_seq: u64,
}

type MsgRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<i64>,
    i64,
);

#[derive(Serialize, Deserialize)]
struct Cursor {
    c: String,
    t: i64,
    m: String,
}

fn encode_cursor(chat_id: &str, ts: i64, msg_id: &str) -> String {
    let cur = Cursor {
        c: chat_id.to_string(),
        t: ts,
        m: msg_id.to_string(),
    };
    let json = serde_json::to_vec(&cur).expect("cursor serialize");
    format!("cur:{}", BASE64URL_NOPAD.encode(&json))
}

fn decode_cursor(s: &str) -> Result<Cursor> {
    let body = s.strip_prefix("cur:").ok_or(StoreError::InvalidCursor)?;
    let json = BASE64URL_NOPAD
        .decode(body.as_bytes())
        .map_err(|_| StoreError::InvalidCursor)?;
    let cur: Cursor = serde_json::from_slice(&json).map_err(|_| StoreError::InvalidCursor)?;
    if cur.c.is_empty() || cur.m.is_empty() {
        return Err(StoreError::InvalidCursor);
    }
    Ok(cur)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn derive_key(static_secret: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, static_secret);
    let mut okm = [0u8; 32];
    hk.expand(STORE_HKDF_INFO, &mut okm)
        .expect("HKDF expand 32 bytes");
    okm
}

pub struct Store {
    conn: Mutex<Connection>,
    db_path: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the encrypted store. `static_secret` is the
    /// account identity static key; the DB key is HKDF-SHA256(secret,
    /// info="cinder-store-v1") applied via `PRAGMA key` in raw-key form.
    pub fn open(data_dir: &Path, static_secret: &[u8]) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))?;
        }

        let db_path = data_dir.join("cinder.db");
        {
            // Pre-create with 0600 so the file is never world-readable.
            let mut opts = OpenOptions::new();
            opts.write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            opts.open(&db_path)?;
        }

        let conn = Connection::open(&db_path)?;
        let key = derive_key(static_secret);
        let key_hex = data_encoding::HEXLOWER.encode(&key);
        // sqlcipher raw-key form bypasses its passphrase KDF (already HKDF'd).
        conn.execute_batch(&format!(
            "PRAGMA key = \"x'{key_hex}'\";
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;"
        ))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600))?;
        }

        let store = Store {
            conn: Mutex::new(conn),
            db_path,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Internal("store mutex poisoned"))
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn()?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE schema_migrations(
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );
            CREATE TABLE chats(
                chat_id TEXT PRIMARY KEY,
                peer_id BLOB UNIQUE NOT NULL,
                title TEXT,
                created_at INTEGER NOT NULL,
                last_activity INTEGER NOT NULL,
                unread INTEGER NOT NULL DEFAULT 0,
                verified INTEGER NOT NULL DEFAULT 0,
                read_upto INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE messages(
                msg_id TEXT PRIMARY KEY,
                chat_id TEXT NOT NULL,
                dir TEXT NOT NULL CHECK(dir IN('in','out')),
                kind TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '',
                state TEXT,
                client_msg_id TEXT,
                ts INTEGER NOT NULL,
                edited_at INTEGER,
                revision INTEGER NOT NULL DEFAULT 0,
                deleted INTEGER NOT NULL DEFAULT 0
            );
            CREATE UNIQUE INDEX messages_out_dedup
                ON messages(client_msg_id) WHERE dir = 'out' AND client_msg_id IS NOT NULL;
            CREATE INDEX messages_chat_ts ON messages(chat_id, ts DESC, msg_id DESC);
            CREATE TABLE msg_states(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                msg_id TEXT NOT NULL,
                state TEXT NOT NULL,
                ts INTEGER NOT NULL
            );
            CREATE INDEX msg_states_msg ON msg_states(msg_id);
            CREATE TABLE event_log(
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                chat_id TEXT,
                payload TEXT NOT NULL,
                ts INTEGER NOT NULL
            );
            CREATE TABLE dedup(
                client_msg_id TEXT PRIMARY KEY,
                msg_id TEXT NOT NULL,
                ts INTEGER NOT NULL
            );
            CREATE TABLE reactions(
                msg_id TEXT NOT NULL,
                emoji TEXT NOT NULL,
                reactor BLOB NOT NULL,
                ts INTEGER NOT NULL,
                PRIMARY KEY(msg_id, emoji, reactor)
            );
            CREATE VIRTUAL TABLE messages_fts USING fts5(
                body, content='messages', content_rowid='rowid'
            );
            CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, body) VALUES(new.rowid, new.body);
            END;
            CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, body)
                    VALUES('delete', old.rowid, old.body);
            END;
            CREATE TRIGGER messages_au AFTER UPDATE OF body ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, body)
                    VALUES('delete', old.rowid, old.body);
                INSERT INTO messages_fts(rowid, body) VALUES(new.rowid, new.body);
            END;
            CREATE TABLE meta(key TEXT PRIMARY KEY, val TEXT NOT NULL);
            INSERT INTO meta(key, val) VALUES
                ('protocol_version', '1'),
                ('oldest_event_seq', '0');",
        )?;
        tx.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES(?1, ?2)",
            params![SCHEMA_VERSION, now_ms()],
        )?;
        tx.commit()?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, val: &str) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO meta(key, val) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET val = excluded.val",
            params![key, val],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row("SELECT val FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn upsert_chat(
        &self,
        chat_id: &str,
        peer_id: &[u8; 32],
        title: Option<&str>,
        verified: bool,
    ) -> Result<()> {
        let now = now_ms();
        self.conn()?.execute(
            "INSERT INTO chats(chat_id, peer_id, title, created_at, last_activity, verified)
             VALUES(?1, ?2, ?3, ?4, ?4, ?5)
             ON CONFLICT(chat_id) DO UPDATE SET
                peer_id = excluded.peer_id,
                title = COALESCE(excluded.title, chats.title),
                verified = excluded.verified",
            params![chat_id, peer_id.as_slice(), title, now, verified as i64],
        )?;
        Ok(())
    }

    pub fn list_chats_sorted(&self, limit: u32) -> Result<Vec<Chat>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT chat_id, peer_id, title, created_at, last_activity, unread,
                    verified, read_upto
             FROM chats ORDER BY last_activity DESC, chat_id ASC LIMIT ?1",
        )?;
        let chats = stmt
            .query_map([limit.clamp(1, MAX_WINDOW) as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        chats
            .into_iter()
            .map(
                |(chat_id, peer, title, created, activity, unread, verified, read_upto)| {
                    let peer_id: [u8; 32] = peer
                        .try_into()
                        .map_err(|_| StoreError::Internal("peer_id len"))?;
                    Ok(Chat {
                        last_message: self.last_message_locked(&conn, &chat_id)?.map(Box::new),
                        chat_id,
                        peer_id,
                        title,
                        created_at: created,
                        last_activity: activity,
                        unread,
                        verified: verified != 0,
                        read_upto,
                    })
                },
            )
            .collect()
    }

    fn last_message_locked(&self, conn: &Connection, chat_id: &str) -> Result<Option<Message>> {
        let row = conn
            .query_row(
                "SELECT msg_id, dir, kind, body, state, client_msg_id, ts,
                        revision, edited_at, deleted
                 FROM messages
                 WHERE chat_id = ?1 AND deleted = 0
                 ORDER BY ts DESC, msg_id DESC LIMIT 1",
                [chat_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, Option<i64>>(8)?,
                        r.get::<_, i64>(9)?,
                    ))
                },
            )
            .optional()?;
        row.map(|t| self.tuple_to_message(conn, chat_id, t))
            .transpose()
    }

    fn tuple_to_message(&self, conn: &Connection, chat_id: &str, t: MsgRow) -> Result<Message> {
        let (msg_id, dir, kind, body, state, client_msg_id, ts, revision, edited_at, deleted) = t;
        Ok(Message {
            reactions: self.reactions_locked(conn, &msg_id)?,
            msg_id,
            chat_id: chat_id.to_string(),
            dir: Dir::parse(&dir)?,
            kind: MsgKind::parse(&kind)?,
            body,
            state: state.map(|s| MsgState::parse(&s)).transpose()?,
            client_msg_id,
            ts,
            revision,
            edited_at,
            deleted: deleted != 0,
        })
    }

    fn reactions_locked(&self, conn: &Connection, msg_id: &str) -> Result<Vec<Reaction>> {
        let mut stmt = conn.prepare(
            "SELECT emoji, reactor, ts FROM reactions WHERE msg_id = ?1
             ORDER BY ts ASC",
        )?;
        let rows = stmt
            .query_map([msg_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(emoji, reactor, ts)| {
                Ok(Reaction {
                    emoji,
                    reactor: reactor
                        .try_into()
                        .map_err(|_| StoreError::Internal("reactor len"))?,
                    ts,
                })
            })
            .collect()
    }

    /// Windowed, newest-first history page. Never loads more than `limit`
    /// rows; returns `(messages, next_before_cursor)`. `before` is exclusive
    /// of its anchor — no message appears in two consecutive pages.
    pub fn window(
        &self,
        chat_id: &str,
        before: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<Message>, Option<String>)> {
        let limit = limit.clamp(1, MAX_WINDOW) as i64;
        let conn = self.conn()?;
        let tuples = match before {
            None => {
                let mut stmt = conn.prepare(
                    "SELECT msg_id, dir, kind, body, state, client_msg_id, ts,
                            revision, edited_at, deleted
                     FROM messages
                     WHERE chat_id = ?1
                     ORDER BY ts DESC, msg_id DESC LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(params![chat_id, limit + 1], Self::msg_tuple_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            }
            Some(cur) => {
                let c = decode_cursor(cur)?;
                if c.c != chat_id {
                    return Err(StoreError::InvalidCursor);
                }
                let mut stmt = conn.prepare(
                    "SELECT msg_id, dir, kind, body, state, client_msg_id, ts,
                            revision, edited_at, deleted
                     FROM messages
                     WHERE chat_id = ?1
                       AND (ts < ?2 OR (ts = ?2 AND msg_id < ?3))
                     ORDER BY ts DESC, msg_id DESC LIMIT ?4",
                )?;
                let rows = stmt
                    .query_map(params![chat_id, c.t, c.m, limit + 1], Self::msg_tuple_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            }
        };
        let has_more = tuples.len() as i64 > limit;
        let page: Vec<MsgRow> = tuples.into_iter().take(limit as usize).collect();
        let next = if has_more {
            page.last().map(|t| encode_cursor(chat_id, t.6, &t.0))
        } else {
            None
        };
        page.into_iter()
            .map(|t| self.tuple_to_message(&conn, chat_id, t))
            .collect::<Result<Vec<_>>>()
            .map(|msgs| (msgs, next))
    }

    #[doc(hidden)]
    pub fn window_plan(&self, chat_id: &str) -> Result<String> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "EXPLAIN QUERY PLAN SELECT msg_id FROM messages
             WHERE chat_id = ?1 AND (ts < ?2 OR (ts = ?2 AND msg_id < ?3))
             ORDER BY ts DESC, msg_id DESC LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![chat_id, 0i64, "", 50i64], |r| r.get::<_, String>(3))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.join("\n"))
    }

    fn msg_tuple_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MsgRow> {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
        ))
    }

    /// Inserts a message. For `dir=out` + `client_msg_id`: idempotent — a
    /// replay returns the original `msg_id` with `deduped = true`; a replay
    /// carrying a different body returns `Err(Conflict)`. Bumps the chat's
    /// `last_activity` (and `unread` for inbound).
    pub fn insert_message(&self, msg: &NewMessage) -> Result<(String, bool)> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        if let (Dir::Out, Some(cmid)) = (msg.dir, &msg.client_msg_id) {
            if let Some(existing) = tx
                .query_row(
                    "SELECT msg_id FROM dedup WHERE client_msg_id = ?1",
                    [cmid],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                let stored: Option<(String, String)> = tx
                    .query_row(
                        "SELECT body, chat_id FROM messages WHERE msg_id = ?1",
                        [&existing],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if stored.as_ref().map(|(b, c)| (b.as_str(), c.as_str()))
                    != Some((msg.body.as_str(), msg.chat_id.as_str()))
                {
                    return Err(StoreError::Conflict);
                }
                tx.commit()?;
                return Ok((existing, true));
            }
            tx.execute("DELETE FROM dedup WHERE ts < ?1", [now_ms() - DEDUP_TTL_MS])?;
        }

        let affected = tx.execute(
            "INSERT OR IGNORE INTO messages
                (msg_id, chat_id, dir, kind, body, state, client_msg_id, ts)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                msg.msg_id,
                msg.chat_id,
                msg.dir.as_str(),
                msg.kind.as_str(),
                msg.body,
                msg.state.map(|s| s.as_str()),
                msg.client_msg_id,
                msg.ts
            ],
        )?;
        if affected == 0 {
            // PK or partial-dedup-unique conflict: recover the winner.
            let existing: Option<String> = if msg.client_msg_id.is_some() {
                tx.query_row(
                    "SELECT msg_id FROM messages
                     WHERE dir = 'out' AND client_msg_id = ?1",
                    [&msg.client_msg_id],
                    |r| r.get(0),
                )
                .optional()?
            } else {
                None
            };
            let existing = existing.unwrap_or_else(|| msg.msg_id.clone());
            if let (Dir::Out, Some(cmid)) = (msg.dir, &msg.client_msg_id) {
                tx.execute(
                    "INSERT INTO dedup(client_msg_id, msg_id, ts) VALUES(?1, ?2, ?3)
                     ON CONFLICT(client_msg_id) DO NOTHING",
                    params![cmid, existing, now_ms()],
                )?;
            }
            tx.commit()?;
            return Ok((existing, true));
        }

        if let (Dir::Out, Some(cmid)) = (msg.dir, &msg.client_msg_id) {
            tx.execute(
                "INSERT INTO dedup(client_msg_id, msg_id, ts) VALUES(?1, ?2, ?3)
                 ON CONFLICT(client_msg_id) DO NOTHING",
                params![cmid, msg.msg_id, now_ms()],
            )?;
        }
        let unread_delta = if msg.dir == Dir::In { 1 } else { 0 };
        tx.execute(
            "UPDATE chats SET
                last_activity = MAX(last_activity, ?2),
                unread = unread + ?3
             WHERE chat_id = ?1",
            params![msg.chat_id, msg.ts, unread_delta],
        )?;
        tx.commit()?;
        Ok((msg.msg_id.clone(), false))
    }

    /// Applies a delivery-lifecycle transition per AGENT-API §5.2 and appends
    /// to the `msg_states` audit log. `read` is rejected: reserved enum, never
    /// emitted in v1 (no read receipts exist on the wire).
    pub fn update_state(&self, msg_id: &str, to: MsgState) -> Result<()> {
        if to == MsgState::Read {
            return Err(StoreError::InvalidTransition { from: None, to });
        }
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        let raw: Option<Option<String>> = tx
            .query_row(
                "SELECT state FROM messages WHERE msg_id = ?1",
                [msg_id],
                |r| r.get(0),
            )
            .optional()?;
        let raw = raw.ok_or_else(|| StoreError::NotFound(msg_id.to_string()))?;
        let from = raw.as_deref().map(MsgState::parse).transpose()?;
        let ok = match from {
            Some(f) => f.can_reach(to),
            None => to == MsgState::Queued,
        };
        if !ok {
            return Err(StoreError::InvalidTransition { from, to });
        }
        debug_assert!(from.map(|f| !f.is_terminal()).unwrap_or(true));
        tx.execute(
            "INSERT INTO msg_states(msg_id, state, ts) VALUES(?1, ?2, ?3)",
            params![msg_id, to.as_str(), now_ms()],
        )?;
        tx.execute(
            "UPDATE messages SET state = ?2 WHERE msg_id = ?1",
            params![msg_id, to.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn state_history(&self, msg_id: &str) -> Result<Vec<(MsgState, i64)>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("SELECT state, ts FROM msg_states WHERE msg_id = ?1 ORDER BY id")?;
        let rows = stmt
            .query_map([msg_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(s, ts)| MsgState::parse(&s).map(|st| (st, ts)))
            .collect()
    }

    /// Single-message lookup by id. Needed by the rpc/core seams: `msg.edit`,
    /// `msg.delete`, and `msg.react` must resolve a `msg_id` to its chat,
    /// direction, and tombstone state before they can act.
    pub fn get_message(&self, msg_id: &str) -> Result<Option<Message>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT chat_id, msg_id, dir, kind, body, state, client_msg_id,
                        ts, revision, edited_at, deleted
                 FROM messages WHERE msg_id = ?1",
                [msg_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        (
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                            r.get::<_, i64>(7)?,
                            r.get::<_, i64>(8)?,
                            r.get::<_, Option<i64>>(9)?,
                            r.get::<_, i64>(10)?,
                        ),
                    ))
                },
            )
            .optional()?;
        row.map(|(chat_id, t)| self.tuple_to_message(&conn, &chat_id, t))
            .transpose()
    }

    pub fn edit_message(&self, msg_id: &str, new_body: &str) -> Result<()> {
        let affected = self.conn()?.execute(
            "UPDATE messages SET body = ?2, edited_at = ?3, revision = revision + 1
             WHERE msg_id = ?1 AND deleted = 0",
            params![msg_id, new_body, now_ms()],
        )?;
        if affected == 0 {
            return Err(StoreError::NotFound(msg_id.to_string()));
        }
        Ok(())
    }

    /// Peer-supplied mutation gate: an Edit may only touch an INBOUND message
    /// inside the chat bound to that peer — a contact can never rewrite our
    /// outbound history or a row in another chat.
    pub fn apply_peer_edit(&self, msg_id: &str, chat_id: &str, new_body: &str) -> Result<()> {
        let affected = self.conn()?.execute(
            "UPDATE messages SET body = ?3, edited_at = ?4, revision = revision + 1
             WHERE msg_id = ?1 AND chat_id = ?2 AND dir = 'in' AND deleted = 0",
            params![msg_id, chat_id, new_body, now_ms()],
        )?;
        if affected == 0 {
            return Err(StoreError::NotFound(msg_id.to_string()));
        }
        Ok(())
    }

    /// Same scoping for peer tombstones: inbound-only, this peer's chat only.
    pub fn apply_peer_delete(&self, msg_id: &str, chat_id: &str) -> Result<()> {
        let affected = self.conn()?.execute(
            "UPDATE messages SET deleted = 1, body = ''
             WHERE msg_id = ?1 AND chat_id = ?2 AND dir = 'in'",
            params![msg_id, chat_id],
        )?;
        if affected == 0 {
            return Err(StoreError::NotFound(msg_id.to_string()));
        }
        Ok(())
    }

    /// Peer reactions attach to either direction but only on a live message
    /// inside the chat bound to that peer.
    pub fn apply_peer_reaction(
        &self,
        msg_id: &str,
        chat_id: &str,
        emoji: &str,
        reactor: &[u8; 32],
    ) -> Result<()> {
        let conn = self.conn()?;
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM messages
                 WHERE msg_id = ?1 AND chat_id = ?2 AND deleted = 0",
                params![msg_id, chat_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound(msg_id.to_string()));
        }
        conn.execute(
            "INSERT INTO reactions(msg_id, emoji, reactor, ts) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(msg_id, emoji, reactor) DO UPDATE SET ts = excluded.ts",
            params![msg_id, emoji, reactor.as_slice(), now_ms()],
        )?;
        Ok(())
    }

    /// Peer un-react, same chat scoping.
    pub fn remove_peer_reaction(
        &self,
        msg_id: &str,
        chat_id: &str,
        reactor: &[u8; 32],
    ) -> Result<()> {
        let conn = self.conn()?;
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM messages WHERE msg_id = ?1 AND chat_id = ?2",
                params![msg_id, chat_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound(msg_id.to_string()));
        }
        conn.execute(
            "DELETE FROM reactions WHERE msg_id = ?1 AND reactor = ?2",
            params![msg_id, reactor.as_slice()],
        )?;
        Ok(())
    }

    /// Tombstone: marks deleted and clears the body so plaintext does not
    /// linger in history, FTS, or `chat.list` previews.
    pub fn soft_delete(&self, msg_id: &str) -> Result<()> {
        let affected = self.conn()?.execute(
            "UPDATE messages SET deleted = 1, body = '' WHERE msg_id = ?1",
            [msg_id],
        )?;
        if affected == 0 {
            return Err(StoreError::NotFound(msg_id.to_string()));
        }
        Ok(())
    }

    pub fn add_reaction(&self, msg_id: &str, emoji: &str, reactor: &[u8; 32]) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO reactions(msg_id, emoji, reactor, ts) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(msg_id, emoji, reactor) DO UPDATE SET ts = excluded.ts",
            params![msg_id, emoji, reactor.as_slice(), now_ms()],
        )?;
        Ok(())
    }

    /// `emoji: None` removes all of `reactor`'s reactions on the message.
    pub fn remove_reaction(
        &self,
        msg_id: &str,
        emoji: Option<&str>,
        reactor: &[u8; 32],
    ) -> Result<()> {
        let conn = self.conn()?;
        match emoji {
            Some(e) => conn.execute(
                "DELETE FROM reactions WHERE msg_id = ?1 AND emoji = ?2 AND reactor = ?3",
                params![msg_id, e, reactor.as_slice()],
            )?,
            None => conn.execute(
                "DELETE FROM reactions WHERE msg_id = ?1 AND reactor = ?2",
                params![msg_id, reactor.as_slice()],
            )?,
        };
        Ok(())
    }

    /// Appends to the bounded replay log. `seq` is AUTOINCREMENT — monotonic
    /// and never reused even after pruning (gap semantics preserved).
    pub fn append_event(&self, kind: &str, chat_id: Option<&str>, payload: &Value) -> Result<u64> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        let prev: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM event_log", [], |r| {
            r.get(0)
        })?;
        tx.execute(
            "INSERT INTO event_log(kind, chat_id, payload, ts) VALUES(?1, ?2, ?3, ?4)",
            params![kind, chat_id, payload.to_string(), now_ms()],
        )?;
        let seq = tx.last_insert_rowid();
        debug_assert!(seq > prev, "event seq regressed: {seq} <= {prev}");
        tx.execute(
            "DELETE FROM event_log WHERE seq <= ?1",
            [seq.saturating_sub(EVENT_LOG_KEEP)],
        )?;
        let oldest: i64 = tx.query_row("SELECT COALESCE(MIN(seq), 0) FROM event_log", [], |r| {
            r.get(0)
        })?;
        tx.execute(
            "INSERT INTO meta(key, val) VALUES('oldest_event_seq', ?1)
             ON CONFLICT(key) DO UPDATE SET val = excluded.val",
            [oldest.to_string()],
        )?;
        tx.commit()?;
        Ok(seq as u64)
    }

    pub fn oldest_seq(&self) -> Result<u64> {
        Ok(self
            .conn()?
            .query_row("SELECT COALESCE(MIN(seq), 0) FROM event_log", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
    }

    pub fn latest_seq(&self) -> Result<u64> {
        Ok(self
            .conn()?
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM event_log", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
    }

    /// Replay `[since+1 .. latest_seq]`, oldest-first, capped at `limit`.
    /// `gap = true` when `since < oldest_seq - 1` (the caller then emits
    /// `event.gap` before the first replayed event). `since > latest_seq`
    /// or `since == u64::MAX` semantics error → `InvalidCursor` (RPC -32017).
    pub fn events_since(&self, since: u64, limit: u32) -> Result<EventsPage> {
        let conn = self.conn()?;
        let latest: i64 =
            conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM event_log", [], |r| {
                r.get(0)
            })?;
        let oldest: i64 =
            conn.query_row("SELECT COALESCE(MIN(seq), 0) FROM event_log", [], |r| {
                r.get(0)
            })?;
        let since_i = since as i64;
        if since > latest as u64 {
            return Err(StoreError::InvalidCursor);
        }
        let gap = oldest > 0 && since_i + 1 < oldest;
        let mut stmt = conn.prepare(
            "SELECT seq, kind, chat_id, payload, ts FROM event_log
             WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
        )?;
        let events = stmt
            .query_map(
                params![since_i, limit.clamp(1, EVENT_LOG_KEEP as u32) as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let events = events
            .into_iter()
            .map(|(seq, kind, chat_id, payload, ts)| {
                Ok(Event {
                    seq: seq as u64,
                    kind,
                    chat_id,
                    payload: serde_json::from_str(&payload)
                        .map_err(|_| StoreError::Internal("event payload"))?,
                    ts,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(EventsPage {
            events,
            gap,
            oldest_seq: oldest as u64,
            latest_seq: latest as u64,
        })
    }

    pub fn mark_read_upto(&self, chat_id: &str, ts: i64) -> Result<()> {
        let affected = self.conn()?.execute(
            "UPDATE chats SET unread = 0, read_upto = MAX(read_upto, ?2)
             WHERE chat_id = ?1",
            params![chat_id, ts],
        )?;
        if affected == 0 {
            return Err(StoreError::NotFound(chat_id.to_string()));
        }
        Ok(())
    }

    /// FTS5 search over message bodies, newest-first, limit-bounded.
    pub fn search(&self, query: &str, chat_id: Option<&str>, limit: u32) -> Result<Vec<Message>> {
        let conn = self.conn()?;
        let sql = "SELECT m.msg_id, m.dir, m.kind, m.body, m.state,
                          m.client_msg_id, m.ts, m.revision, m.edited_at,
                          m.deleted, m.chat_id
                   FROM messages_fts JOIN messages m ON m.rowid = messages_fts.rowid
                   WHERE messages_fts MATCH ?1";
        let tuples: Vec<(String, MsgRow)> = match chat_id {
            Some(cid) => {
                let mut stmt = conn.prepare(&format!(
                    "{sql} AND m.chat_id = ?2 ORDER BY m.ts DESC LIMIT ?3"
                ))?;
                let rows = stmt
                    .query_map(
                        params![query, cid, limit.clamp(1, MAX_WINDOW) as i64],
                        |r| Ok((r.get::<_, String>(10)?, Self::msg_tuple_row(r)?)),
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            }
            None => {
                let mut stmt = conn.prepare(&format!("{sql} ORDER BY m.ts DESC LIMIT ?2"))?;
                let rows = stmt
                    .query_map(params![query, limit.clamp(1, MAX_WINDOW) as i64], |r| {
                        Ok((r.get::<_, String>(10)?, Self::msg_tuple_row(r)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            }
        };
        tuples
            .into_iter()
            .map(|(cid, t)| self.tuple_to_message(&conn, &cid, t))
            .collect()
    }
}
