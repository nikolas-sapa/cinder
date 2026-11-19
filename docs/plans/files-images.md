# cinder — Plan: encrypted file & image transfer

Status: scoped implementation spec, not yet implemented. Normative deltas are
relative to **PROTOCOL.md**, **AGENT-API.md**, and the `AppMsg` app layer in
`cinder/src/core.rs`. This plan lifts the v1 non-goal "no file transfer"
(PLAN §1, THREAT-MODEL §7) for the version it ships in.

The short version: files move **inside the existing Noise session** as new
`AppMsg` variants. The relay stays byte-blind — a chunk is just another
padded 8 KiB transport message. Sender splits, receiver reassembles to a
staging file, verifies a whole-file SHA-256, then the chat shows a normal
message of `kind:"file"`. Mailbox mechanics make small files work offline;
big files resume across reconnects via receiver-advertised chunk bitmaps.

---

## 1. Goals / non-goals

### Goals

- Any file up to `FILE_MAX_BYTES` (64 MiB) 1:1, E2E-encrypted, resumable.
- Image sends carry a client-generated thumbnail (≤16 KiB) inside the offer.
- Voice notes ride the same machinery: an audio file + duration/codec
  metadata. No separate voice-note pipeline (§8).
- Offline-capable for small files via the existing mailbox path (§4.4);
  larger files degrade gracefully to send-while-online + resume.
- Progress is observable (`file_state` events, TUI inline progress).
- Everything reachable via JSON-RPC under the existing scope model.

### Non-goals

- No group/multi-recipient fan-out (1:1 only, same as v1 messaging).
- No server-side processing: no relay transcode, no relay thumbnails, no
  virus scanning. The relay learns nothing new about payloads.
- No in-terminal image display (sixel/kitty/iTerm protocols). Placeholder +
  `/save` only.
- No automatic decompression, preview generation, or `open` on the receiver.
- No CDN/blob-store on the relay. Chunks are ordinary envelopes; the relay
  gets no new protocol variants.
- No forward-secrecy upgrade: chunks inherit the session's rekey window
  exactly like chat text (THREAT-MODEL §5).
- No download-again-from-peer fetch protocol: resume completes an
  *interrupted* transfer; it is not a general re-request API (a `file:`
  blob is not a server).

---

## 2. Constants (normative, injectable for tests)

| Name | Value | Meaning |
|---|---|---|
| `FILE_MAX_BYTES` | `64 MiB` (`67 108 864`) | Hard cap on `FileOffer.size` |
| `FILE_CHUNK_BYTES` | `7168` B (7 KiB) | Default plaintext data per `FileChunk`; sender MAY use 512–7168, MUST hold it constant within a transfer |
| `FILE_MAX_CHUNKS` | `16 384` | `chunks_total` cap (7168 B × 16 384 ≈ 112 MiB headroom; `size` cap binds first) |
| `FILE_THUMB_MAX_BYTES` | `16 384` B | Thumbnail cap in `FileOffer.thumb` |
| `FILE_THUMB_MAX_DIM` | `320` px | Long-edge cap for generated thumbnails |
| `FILE_MAX_INFLIGHT_PEER` | `4` | Concurrent active transfers per peer (either direction combined) |
| `FILE_MAX_INFLIGHT` | `16` | Concurrent active transfers per account |
| `FILE_STAGE_MAX_BYTES` | `512 MiB` | Aggregate on-disk staging cap across all inbound transfers |
| `FILE_STAGE_TTL` | `7 d` | Stale `.part` + `have` state is reaped; transfer → `expired` |
| `FILE_OUT_QUEUE` | `128` chunks | Per-transfer outbound chunk channel cap (disk→wire refill) |
| `FILE_HAVE_INTERVAL` | `64` chunks | Receiver emits `FileHave` at least every N verified chunks |
| `FILE_SEND_PACE` | `80` fps | Default chunk pacing; relay cap is 120 transport frames/s/conn (PROTOCOL §12) — headroom is for chat + acks |
| `FILE_RESUME_RETRY` | `600 s` | Deposit-resume poll after `MailboxFull` while peer offline |
| `VOICE_MAX_MS` | `300 000` | Advisory duration cap for `kind:"voice"` offers |

Wire math: a 7168 B chunk + `FileChunk` overhead + `AppMsg` variant tag
serializes to ≈ 7.2–7.3 KiB, under `MAX_PLAINTEXT` (8192). Padded, every
chunk lands in the 8192 B bucket → ciphertext 8210 B, identical for every
chunk. A 64 MiB file is ≈ 9 400 chunks ≈ 75 MiB of padded wire. At the
default pace that is ≈ 2 min of pure chunk traffic at best; real relay paths
will be slower — **transfer time is not a spec guarantee**, boundedness is.

---

## 3. Wire deltas — `AppMsg` additions

All file messages are `AppMsg` variants: bincode-serialized inside the Noise
AEAD envelope (`cinder/src/core.rs:150`). They add **no** `ClientMsg`/
`ServerMsg` variants — PROTOCOL §4/§5 is unchanged; the app payload enum is
client-internal per PROTOCOL §11. Interop: a peer that doesn't implement
this spec MUST drop unknown `AppMsg` variants without decrypt failure
(already the case — unknown variants simply don't match).

```rust
enum AppMsg {
    // ... Text, Edit, Delete, React, Grant, Ack unchanged ...

    /// Transfer offer. metadata ≤ ~2 KiB; thumb ≤ FILE_THUMB_MAX_BYTES.
    FileOffer {
        file_id: [u8; 32],          // CSPRNG — NOT derivable from msg/chat/peer
        fetch_token: [u8; 32],      // capability: names the blob, binds resume
        name: String,               // ≤ 255 B, UNTRUSTED display name
        mime: String,               // ≤ 64 B, claimed-only content type
        size: u64,                  // ≤ FILE_MAX_BYTES
        sha256: [u8; 32],           // whole-file digest — the integrity root
        chunk_size: u32,            // sender-chosen, 512..=FILE_CHUNK_BYTES
        chunks_total: u32,          // = ceil(size / chunk_size), ≤ FILE_MAX_CHUNKS
        kind: FileKind,             // File | Image | Audio
        thumb: Option<Vec<u8>>,     // image only; sender-generated
        audio: Option<AudioMeta>,   // voice notes: duration + codec (§8)
        caption: Option<String>,    // ≤ 1024 B, UNTRUSTED
        reply_msg: String,          // the anchoring chat msg_id ("msg:…")
    },

    /// One chunk of file data. `index ∈ [0, chunks_total)`; `data.len() ==
    /// chunk_size` for all chunks except the last (len = size % chunk_size,
    /// or chunk_size when size is a multiple).
    FileChunk { file_id: [u8; 32], index: u32, data: Vec<u8> },

    /// Receiver → sender progress/resume vector. `ranges` is a sorted,
    /// coalesced list of inclusive [start,end] chunk indexes the receiver
    /// has VERIFIED on disk. Sent every FILE_HAVE_INTERVAL chunks, on
    /// FileAccept, and on resume re-negotiation.
    FileHave { file_id: [u8; 32], ranges: Vec<(u32, u32)> },

    /// Inbound-side decision: begin (or resume) the transfer.
    /// `ranges` mirrors FileHave semantics for restart-after-crash.
    FileAccept { file_id: [u8; 32], ranges: Vec<(u32, u32)> },

    /// Verified-completion receipt: receiver computed sha256(assembled) ==
    /// offer.sha256. Terminal for the sender.
    FileDone { file_id: [u8; 32], sha256: [u8; 32] },

    /// Sender- or receiver-initiated abort. Either direction, terminal.
    FileCancel { file_id: [u8; 32], reason: FileCancelReason },

    /// Receiver declined the offer (manual decline or policy). Terminal.
    FileDecline { file_id: [u8; 32] },
}

enum FileKind { File, Image, Audio }
struct AudioMeta { duration_ms: u32, codec: String, sample_rate_hz: u32, channels: u8 }
enum FileCancelReason { User, Timeout, HashMismatch, StorageFull, Policy }
```

Rules:

- `file_id` is `OsRng` output — same class as `MailboxId` (PROTOCOL §9): a
  random 256-bit id, never derived from peer-id, msg_id, or content. It is
  unenumerable by construction; combined with `fetch_token` it is the
  transfer's capability pair (§5.4).
- `FileOffer` is itself a normal message: it rides `deliver_now`/`deposit`
  exactly like `AppMsg::Text` (core.rs:1711–1826) — live transport when the
  peer is online, mailbox deposit when offline-with-grant. Chunks use the
  same paths but a dedicated per-transfer out-queue (§4.3), never
  `pending_out` (a 9 400-chunk transfer would evict real messages from the
  64-deep cap).
- `FileChunk.data` MUST NOT exceed `chunk_size`; receivers enforce
  `index < chunks_total` and the declared length rules *before* writing —
  violations drop the chunk; a second violation → `FileCancel{Policy}` +
  `failed`. (First-violation tolerance covers a buggy peer, not a stream of
  them.)
- `FileDone` and `FileAccept` are the ack machinery: **send state advances
  only on peer messages**, never on relay acceptance of a chunk deposit.
- `reply_msg` ties the transfer to its chat message (`msg:` id) so history
  and `message.received` render one object, and `msg.delete`/`msg.react`
  keep working on file messages.

---

## 4. Transfer lifecycle

```
 SENDER                                        RECEIVER
   | file.send accepted                           |
   |  write files row (state:offered)             |
   |--- FileOffer ------------------------------->|  validate offer
   |                                              |  files row (state:offered)
   |                                              |  emit message.received
   |                                              |  (kind:"file") — §5.2
   |                                              |  policy → FileAccept
   |<-- FileAccept{ranges:[]} --------------------|  (or FileDecline)
   |  state: transferring                         |  state: transferring
   |--- FileChunk{0..N} (paced 80fps) ----------->|  write .part at
   |                                              |   index*chunk_size
   |                                              |  update have-bitmap
   |<-- FileHave (every 64 chunks) ---------------|
   |                     [...]                    |
   |                                              |  last chunk → sha256
   |                                              |  match → state: complete
   |<-- FileDone ---------------------------------|  emit file_state:complete
   |  state: complete; emit file_state            |
```

State enum (both directions): `offered → accepted/declined → transferring →
verifying → complete`, plus terminal `failed`, `cancelled`, `expired`.
`declined/cancelled/failed/expired` are terminal; `complete` is terminal.

### 4.1 Inbound accept policy

`file.accept` is emitted automatically by the receiver when policy allows.
Policy (`files.auto_accept`, local setting, default `contacts`):

| Value | Behavior |
|---|---|
| `contacts` (default) | Auto-accept from `active` contacts subject to caps |
| `verified` | Auto-accept only `trust:"verified"` contacts; others wait for `file.accept` |
| `manual` | Every inbound offer waits for `file.accept`/`file.decline` |

`offered` transfers older than `FILE_STAGE_TTL` with no accept → `expired`,
staged bytes reaped. Caps that trigger automatic `FileDecline`: inflight
per-peer ≥ `FILE_MAX_INFLIGHT_PEER`, inflight total ≥ `FILE_MAX_INFLIGHT`,
staging ≥ `FILE_STAGE_MAX_BYTES` — the decline is silent to the peer's
wallet but reported locally as `file_state{state:"declined",reason}`.

### 4.2 Resumability

One mechanism, two triggers — the **receiver always drives resume** because
only the receiver knows what it has:

- The receiver's `have` bitmap is authoritative and **persisted** (sqlite,
  §6) — crash-safe to the last flushed batch (every `FILE_HAVE_INTERVAL`
  chunks or on clean pause).
- *Receiver restart/reconnect:* any `files` row in a non-terminal state
  with staged bytes → re-send `FileAccept{ranges}` (bitmap of verified
  chunks). The sender skips ranges it already got acked for and resumes
  the pump at the first gap. Re-accept is idempotent by construction —
  ranges are a set, not a counter.
- *Sender restart:* sender holds a `transferring` row with nothing else to
  say — it re-emits `FileOffer` carrying the **identical `file_id` and
  `fetch_token`**. The receiver matches the offer to its existing row by
  constant-time `fetch_token` compare and answers `FileAccept{ranges}` —
  this is what the token binds. An offer with a known `file_id` but wrong
  `fetch_token` → drop, count, never merge state.
- *Stall nudge:* a receiver whose `transferring` transfer makes no progress
  for `FILE_RESUME_RETRY` re-sends `FileAccept{ranges}`. The sender never
  initiates resume on its own — it has no way to know which chunks landed.
- Sender-side `sent`/`acked` state also persists (the same `have` bitmap is
  reused on the outbound row to record `FileHave`-acked chunks).

- Session death (`dead_sids`, decrypt-fatal, relay whole-`session_id` batch
  drop — PROTOCOL §9/§11) loses only unacked chunks; the `have` bitmap only
  records *verified* chunks, so resume automatically resends everything
  lost. There is no chunk-level negative-ack; absence from `ranges` IS the
  nack.

### 4.3 Outbound pump

Per active transfer: read `chunk_size` from the source fd at `index *
chunk_size`, seal via the live session or mailbox deposit, paced by
`FILE_SEND_PACE` tokens/s **shared with chat** (the relay's 120/s cap is
per-connection, PROTOCOL §12; the chunk pump yields to `pending_out` chat
frames — chat latency beats file throughput). Out-queue `FILE_OUT_QUEUE`
bounds memory: chunks are re-read from disk on refill, never held whole.

### 4.4 Mailbox interplay (the 4 MiB wall)

`MAILBOX_MAX_BYTES` = 4 MiB / `MAILBOX_MAX_MSGS` = 500 envelopes ≈ **≤3.6 MiB
of chunk data** queueable for an offline peer (each chunk is one ~8.2 KiB
envelope). Rules:

- `FileOffer` (≤ ~18 KiB padded) deposits fine. Chunks deposit until
  `MailboxFull` (PROTOCOL §10 `0x0006`) — deposit then **pauses**; the
  transfer stays `transferring` with `pending: mailbox_full` and retries on
  `FILE_RESUME_RETRY` timer or peer presence (whichever first).
- Files ≤ ~3 MiB are fully offline-deliverable; larger ones are
  deliver-while-draining. **Stated plainly: a 64 MiB file to a peer who
  never comes online does not complete. The mailbox is a queue, not
  storage.**
- Relay whole-`session_id` atomic drops (§9) are indistinguishable from
  packet loss at this layer — `have` bitmaps cover them.
- Mailbox deposit still counts toward `msg_state`-style "sent" only for the
  offer's anchoring `msg:`. Chunks do not advance the anchoring message's
  state; `file_state` is the transfer's real lifecycle.

### 4.5 Completion, verify, and failure

- After the last chunk: `sha256(staged)` vs `offer.sha256`. Match →
  `FileDone`, state `complete`, anchored `msg:` body gets `state:"delivered"`
  via the normal ack path (the `FileDone` IS the delivery-ack for file
  messages — `Ack{msg_ids}` is also sent for uniformity).
- Mismatch → delete `.part`, `FileCancel{HashMismatch}`, `file_state`
  `failed` with `reason:"hash_mismatch"`. No partial delivery, no "keep
  what we got" mode — integrity is all-or-nothing by design.
- `FileOffer.size == 0` → `chunks_total` MUST be 0; completes on offer
  accept with an empty assembled file and a real sha256 (of b"").

---

## 5. RPC deltas (AGENT-API surface)

Naming follows existing conventions (`msg.send`, `chat.history`). New id
prefix: `file:` (b32 of `file_id`). New scope: none — file methods ride
`send`/`read` scopes (a send-capable agent can send files; a read-capable
agent can list/history them; `file.save` is `read`-scope because it only
materializes local data).

### 5.1 `file.send`

Queue a file transfer. Returns at ACCEPTANCE (offer stored + anchored `msg:`
created), not at delivery — same contract as `msg.send` (AGENT-API §4.3).

**Params**

| Field | Type | Req | Description |
|---|---|---|---|
| `to` | string | yes | `contact:`/`chat:`/`peer:` id |
| `path` | string | yes* | Absolute or `~/`-relative path; resolved server-side, MUST be a regular file |
| `data` | string (b64) | yes* | Inline bytes, decoded ≤ 1 MiB — for agents generating content; exactly one of `path`/`data` |
| `name` | string | no | Override display name; defaults to `path`'s basename (or `blob`) |
| `mime` | string | no | Override claimed MIME; default `mime_guess` from name, else `application/octet-stream` |
| `kind` | `"file"\|"image"\|"voice"` | no | Hint; `"image"` triggers thumbnail attempt, `"voice"` requires `audio` meta derivable or provided |
| `audio` | object | no | `{duration_ms:int, codec:string, sample_rate_hz?:int, channels?:int}` — required when `kind:"voice"` |
| `caption` | string | no | ≤1024 B chat-visible caption |
| `client_xfer_id` | string | yes | Idempotency key — same semantics as `client_msg_id` (§4.3): replay → same `file_id` + `deduped:true`; conflict → `-32018` |

**Result:** `{file_id:"file:…", msg_id:"msg:…", chat_id, state:"offered",
deduped:bool, chunks:int, bytes:int}`

**Errors:** `-32602` (both/neither of `path`/`data`, `size > FILE_MAX_BYTES`,
unreadable path, `kind:"voice"` without `audio`), `-32017`/`not_found`
(unknown `to`), `-32011`/`peer_offline` (peer unreachable AND no mailbox
grant — same rule as `msg.send`), `-32012`/`contact_blocked`,
`-32013`/`key_changed_blocked`, `-32015`/`rate_limited` (inflight caps hit,
`data.retry_after_ms` MAY be set), `-32018`/`conflict` (`client_xfer_id`
reuse with different args).

**Scope:** `send`.

### 5.2 `file.accept` / `file.decline` / `file.cancel`

| Method | Params | Result | Scope |
|---|---|---|---|
| `file.accept` | `{file_id}` | `{file_id, state:"transferring"}` | `send` |
| `file.decline` | `{file_id}` | `{file_id, state:"declined"}` | `send` |
| `file.cancel` | `{file_id}` | `{file_id, state:"cancelled"}` | `send` |

`accept`/`decline` apply to inbound `offered` transfers only (`conflict`
otherwise). `cancel` applies to any non-terminal transfer in either
direction (sends `FileCancel` when the peer is reachable; local-only
tombstone otherwise — documented: a peer MAY keep sending chunks for an
in-flight-cancelled transfer; receiver drops them by `file_id` tombstone).

### 5.3 `file.info` / `file.list`

`file.info {file_id}` → the `files` row: `{file_id, msg_id, chat_id, dir,
name, mime, size, sha256(b64), chunks_total, chunks_done, state, reason,
kind, has_thumb, audio?, caption, saved_path, created_at, updated_at}`.
`chunks_done` = popcount of `have` bitmap — progress without re-reading
the `.part`.

`file.list {chat_id?, state?, limit?, cursor?}` → same object list,
newest-first, cursor rules identical to `chat.list` (AGENT-API §4.7).

Scope: `read` / `read` respectively. `not_found` for unknown ids.

### 5.4 `file.save`

Materialize a `complete` inbound blob to a destination path.

**Params:** `{file_id, dir?, name?}` — `dir` default
`dirs::download_dir()` else `~/Downloads` else `$CINDER_DATA_DIR/saved`;
`name` override is sanitized identically to the offer's `name`.

**Result:** `{file_id, path, bytes}` — `path` is the canonicalized final
location.

**Traversal rules (normative):**

1. Candidate name = `name` param ‖ `files.name`. Strip: path separators
   (`/`, `\`), `..` components, NUL, all C0/C1 controls, leading `.`
   (no hidden saves). Result empty → `file.bin`.
2. `save_dir` canonicalized at call time; final path = `save_dir /
   sanitized`. On basename collision → `name-1.ext`, `-2`, … bounded at 64
   attempts → `internal` error.
3. `canonicalize(final)` MUST `starts_with(canonicalize(save_dir))` —
   re-check after collision-suffixing; a symlinked save_dir component makes
   canonicalize resolve elsewhere → `conflict` with hint.
4. Written `0600`, never exec'd, never added to `$PATH`-adjacent dirs by
   default.

**Errors:** `-32017`/`not_found` (unknown id), `-32018`/`conflict` (state ≠
`complete`, `hint:"transfer not finished"`), `-32602` (dir not writable /
not a directory), `-32603`/`internal` (staging vanished, hash re-verify
failure at save — the saved bytes are re-hashed before rename).

**Scope:** `read`. (`file.save` grants bytes an agent already has via the
sqlcipher store; it is a filesystem-materialization call, not a
confidentiality boundary — the token `fetch_token` never appears on the RPC
surface.)

### 5.5 Events

New `file_state` event (mirrors `msg_state` shape; added to
`CORE_EVENT_KINDS` and `rpc.describe` events):

```json
{"jsonrpc":"2.0","method":"file_state","params":{"subscription_id":"sub:1","seq":1500,"ts":"…","file_id":"file:ab12…","msg_id":"msg:e01d90","chat_id":"chat:77aa01","dir":"in","state":"transferring","bytes_done":1048576,"bytes_total":4194304,"reason":null}}
```

`state ∈ offered|accepted|transferring|verifying|complete|failed|cancelled|
declined|expired`; `reason` on terminal failures ∈ `peer_offline|
mailbox_full|hash_mismatch|storage_full|policy|user|timeout|key_changed|
internal`. Progress updates are **throttled: ≤1 `file_state` event per
transfer per second** (a 9 400-chunk transfer MUST NOT emit 9 400 events);
every state *transition* emits, progress within `transferring` coalesces.

`message.received` gains `kind:"file"`: `body` becomes `{text, trusted:false,
file:{file_id, name, mime, size, kind, has_thumb, audio?}}` — `text` holds a
renderable fallback (`"[file] report.pdf · 2.1 MiB"`) so v1 agents degrade
cleanly. `name`/`mime`/`caption` are peer-controlled → **covered by
`trusted:false`**; `file.size`/`file_id` are also peer-claimed at this stage
(the offer is unverified until `complete` — agents MUST NOT trust claimed
sizes for resource decisions; §9).

`chat.history` `Message` objects gain the same `body.file` extension;
`Message.kind` enum gains `"file"` — additive, tolerated by v1 consumers per
§9 compat rules.

`rpc.describe` additions: 6 methods + `file_state` event + `file` id-prefix
row in §2.6 conventions. `capabilities.limits` gains `file_max_bytes`,
`file_chunk_bytes`, `file_max_inflight_peer`.

---

## 6. Store schema — migration v3

`SCHEMA_VERSION: i64 = 3` (v2 = disappearing messages, in flight). Migrations
are additive `ALTER`/`CREATE`, same `schema_migrations` bookkeeping
(`cinder/src/store.rs:367`).

```sql
CREATE TABLE files(
    file_id      TEXT PRIMARY KEY,              -- "file:"+b32
    msg_id       TEXT NOT NULL REFERENCES messages(msg_id),
    chat_id      TEXT NOT NULL,
    dir          TEXT NOT NULL CHECK(dir IN('in','out')),
    name         TEXT NOT NULL,
    mime         TEXT,
    size         INTEGER NOT NULL,
    sha256       BLOB NOT NULL,                 -- 32 B
    chunk_size   INTEGER NOT NULL,
    chunks_total INTEGER NOT NULL,
    have         BLOB NOT NULL DEFAULT x'',     -- bitmap, bit i = chunk i received/sent-acked
    state        TEXT NOT NULL,                 -- lifecycle enum §4
    reason       TEXT,
    kind         TEXT NOT NULL DEFAULT 'file',  -- file|image|audio
    thumb        BLOB,                          -- ≤16 KiB
    audio_meta   TEXT,                          -- JSON AudioMeta
    fetch_token  BLOB,                          -- 32 B, outbound + inbound rows
    caption      TEXT,
    staged_path  TEXT,                          -- xfer/*.part relative to data_dir
    saved_path   TEXT,
    client_xfer_id TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE UNIQUE INDEX files_out_dedup
    ON files(client_xfer_id) WHERE dir='out' AND client_xfer_id IS NOT NULL;
CREATE INDEX files_chat ON files(chat_id, created_at DESC);
CREATE INDEX files_state ON files(state) WHERE state NOT IN
    ('complete','failed','cancelled','declined','expired');
```

- `messages.kind` accepts `'file'`; `messages.body` for file rows stores the
  JSON fallback text (`[file] name · size`) so history/search (FTS) and the
  TUI last-message preview work unchanged. `files.msg_id` links the rich
  metadata.
- `have` bitmap: `ceil(chunks_total/8)` bytes ≤ 2 KiB at cap — updated in the
  same txn as every `FILE_HAVE_INTERVAL` flush; cheap to persist.
- Staging on **disk**, not sqlite: `<data_dir>/xfer/<b32(sha256(file_id ‖
  fetch_token))>.part`. Hash-named: a directory listing cannot map blob →
  transfer without the token; dir `0700`, file `0600`. `set_len(size)` at
  accept (sparse OK); writes positioned at `index*chunk_size`.
- Completed inbound blobs stay at the `.part` path until `file.save` copies
  (or hard-links, same-fs) to the destination — then `saved_path` recorded,
  `.part` freed. `state:"complete"` + missing `.part` on re-read →
  `file_state{failed, internal}` at save time (never at listing).
- Reaper: on `Core::open` and hourly, non-terminal rows with
  `updated_at < now-FILE_STAGE_TTL` → `expired`, `.part` unlinked; orphan
  `.part` files (no row) older than TTL → unlinked. Reaper count is a log
  line, not an event.
- Dedup: `(client_xfer_id)` uses the `files_out_dedup` index — mirrors
  `messages_out_dedup`; retry window = same `dedup` table semantics (≥24 h).

---

## 7. Thumbnails (images)

Sender-side only:

- `file.send` with `kind:"image"` (or `mime` matching `image/*`): decode via
  `image` crate (already in Cargo.lock). **Decode guards BEFORE decode:**
  declared dimensions from header probe ≤ 8192×8192 AND ≤ 100 Mpx total;
  reject/skip thumbnail on violation — still send the file, `thumb:null`.
- Resize long-edge to `FILE_THUMB_MAX_DIM`, encode JPEG q70; cap at
  `FILE_THUMB_MAX_BYTES` — if the encode exceeds it, drop to `thumb:null`
  (no quality-doom loop).
- Animated/multi-frame: first frame only. RAW/HEIC/TIFF: attempt what the
  crate's enabled features decode; failure → `thumb:null` (not an error).
- Receiver never decodes `thumb` for storage — TUI may base64-render it
  only as a future sixel experiment (non-goal). `thumb` is peer-controlled
  data: any consumer MUST decode with the same size guards; the agent API
  exposes `has_thumb:bool` + `file.thumb {file_id}` fetch returning b64 —
  `trusted:false` semantics apply. `file.thumb` listed for completeness:
  `{file_id}` → `{file_id, thumb_b64, mime}` — `read` scope.

## 8. Voice notes

A voice note is an **audio file transfer plus metadata** — nothing else:

- Sender records/encodes **client-side**: Opus in Ogg container
  (`mime:"audio/ogg; codecs=opus"`, suggested deps `opus`/`audiopus` + `cpal`
  capture + `ogg` muxer — client concern, wire-agnostic). `kind:"voice"`,
  `audio:{duration_ms, codec:"opus", sample_rate_hz:48000, channels:1}`.
- `VOICE_MAX_MS` is an advisory sender-side cap; receiver does not reject
  longer notes (it rejects size>cap as usual).
- TUI renders `[voice 0:42]` with progress identical to files; `file.save`
  yields an `.ogg` playable externally. No in-TUI playback in v1 (non-goal:
  no audio output dep in the TUI).
- Everything else — chunking, resume, hash, tokens, store — is shared. A
  voice note is a 5 KB–2 MB file with `kind:"audio"` in the schema.

## 9. Security notes

- **Path traversal on save** — §5.4's four-step sanitize+canonicalize is
  normative; offer `name` is attacker-controlled and must never reach the
  filesystem unsanitized (`.part` paths never use `name` at all — hash-named).
- **Decompression bombs** — thumbnail decode is guarded pre-decode
  (§7); file bytes themselves are never decoded anywhere in the pipeline —
  `file.save` copies opaque bytes. No auto-decompress of zip/gz/tar anywhere.
- **Content-type spoofing** — `mime` is a claim, never a fact. TUI labels it
  `claimed`; `file.save` does not associate the saved name with handler
  mappings; nothing in cinder executes saved files. A `.png` containing an
  ELF is faithfully delivered — the peer is *trusted*, the *bytes* are not.
- **Unbounded-size lie** — receiver allocates `.part` to declared `size`
  (sparse; quota-checked against `FILE_STAGE_MAX_BYTES` first), and every
  `FileChunk` is bounds-checked against the declared geometry. A peer that
  sends more data than declared trips the violation path (§3).
- **Integrity model** — per-chunk integrity comes from Noise AEAD;
  `offer.sha256` additionally covers truncation, reordering, staging
  corruption, and a dishonest sender. Chunks carry no per-chunk hash — the
  bitmap + whole-file hash suffice because all-or-nothing completion is the
  only terminal-accept path.
- **`file_id`/`fetch_token` enumeration** — `file_id` is CSPRNG (not
  msg-derived); blob paths are `sha256(file_id‖token)`; the relay never sees
  either (all inside AEAD). Token exists to bind resume offers (§4.2) and
  name blobs — it is NOT a second auth layer on `file.save` (same-uid
  daemon clients already have full capability, THREAT-MODEL §3.6).
- **Spam surface** — inbound offers are bounded by the contact gate (only
  `active` contacts can offer) + inflight caps + staging cap +
  auto-decline. A contact can still waste `FILE_STAGE_MAX_BYTES` of disk;
  `file.decline` + `contact.decline` is the out, same class as message spam.
- **`trusted:false` extension** — `name`, `mime`, `caption`, and `size`
  inside `body.file` are peer-controlled; `rpc.describe` schema must carry
  the same untrusted-data description as `body.text` (AGENT-API §6).
- **Secret hygiene** — `fetch_token` never appears in RPC results, events,
  errors, or logs (same rule as mailbox tokens, AGENT-API §7.2). `file.info`
  omits it by construction.

**What is NOT protected:** the relay still sees *that a bulk transfer is
happening* — a run of ~9 400 uniform 8 210 B frames between two peer-ids is
a distinctive traffic shape; padding hides per-chunk content but not volume,
duration, or the pairwise transfer itself (same class as THREAT-MODEL §3.1
metadata residuals — stated, not mitigated). File names/types never reach
the relay; transfer existence does.

---

## 10. TUI deltas

- `tb::Backend` (`cinder/src/tui/backend.rs`) gains: `send_file(chat_id,
  path)` → `SendAck`, `file_save(file_id, dir)` → path, `file_cancel`,
  `file_accept`/`file_decline`; `Event` gains `FileState{…}`;
  `Message` gains `file: Option<FileMeta>` — render path treats it like any
  message line.
- Rendering: `[file] report.pdf · 2.1 MiB · transferring 45%` /
  `[image] photo.png · 800 KiB` / `[voice 0:42]` — same sanitize pass as
  text (C0/C1/ANSI strip applies to `name`/`caption`, THREAT-MODEL §6.7).
- Commands: `/save [file_id]` (most recent file in focused chat when
  omitted), `/files` (list panel for the chat), `/cancel <file_id>`;
  `/send <path>` convenience alias for `file.send` on the focused chat.
  keybindings.md gains rows.

---

## 11. Files touched (implementation map)

| File | Delta |
|---|---|
| `cinder/src/core.rs` | `AppMsg` variants, transfer engine (accept/pump/reaper/resume), `FileMeta` on message emit, new RPC impls on `Core` |
| `cinder/src/store.rs` | schema v3 (`files` table, `messages.kind='file'`), `have`-bitmap helpers, staging-path helpers |
| `cinder/src/rpc/methods.rs` | `file.*` handlers + dedup wiring + prefix table |
| `cinder/src/rpc/core.rs` | `CinderCore` trait methods, `CORE_EVENT_KINDS += "file_state"`, `FileSendParams` etc. |
| `cinder/src/rpc/describe.rs` | method/event schemas, `file:` prefix, limits |
| `cinder/src/rpc/protocol.rs` | no new kinds needed — existing cover all cases |
| `cinder/src/tui/backend.rs` | backend trait + `FileMeta` on `Message` |
| `cinder/src/tui/{app,ui,widgets}.rs` | render + `/save`,`/files`,`/cancel`,`/send` |
| `docs/AGENT-API.md` | §2.6 `file:` prefix, §4 methods, §5.9 `file_state`, §8.4 limits |
| `docs/keybindings.md` | new commands |
| `cinder/Cargo.toml` | `+image` (already in lock), `+mime_guess`, optional `audiopus`/`ogg` for recording |

PROTOCOL.md needs **zero changes** — no new `ClientMsg`/`ServerMsg`, no cap
changes. THREAT-MODEL gains a row only if shipped (metadata residual §9).

---

## 12. Test plan (lands before impl, per spec discipline)

| # | Test | Location | Pass criteria |
|---|---|---|---|
| FT1 | Split/reassemble roundtrip | `cinder/tests/xfer.rs` | sizes {0, 1, 511, 512, 7167, 7168, 7169, 1 MiB, 8 MiB}: receiver-assembled bytes == source, sha256 equal, `chunks_total == ceil(size/chunk_size)` |
| FT2 | Resume mid-transfer | `cinder/tests/xfer.rs` | kill receiver at ≥40% → restart → `FileAccept{ranges}` covers exactly the flushed chunks → completes; resent count ≤ missing count; 0 duplicated writes (bitmap audit) |
| FT3 | Whole-file hash failure | `cinder/tests/xfer.rs` | flip 1 byte in staged `.part` before completion → `FileCancel{HashMismatch}` sent, local state `failed`, `.part` deleted, 20/20 trials |
| FT4 | Chunk geometry violations | `cinder/tests/xfer.rs` | `index ≥ chunks_total` → dropped, 0 writes; `data.len() ≠ chunk_size` on non-last chunk → drop+count; 2nd violation → `FileCancel{Policy}` + `failed` |
| FT5 | Mailbox-cap behavior | `cinder/tests/xfer.rs` + in-process relay | 8 MiB send to offline peer → deposits pause on `MailboxFull` (≤500 envelopes), state stays `transferring`; drain + retry → completes; resume used ≤ `FILE_RESUME_RETRY` poll |
| FT6 | Inflight caps | `cinder/tests/rpc_file.rs` | 5th concurrent per-peer `file.send` → `-32015` `rate_limited`; 17th global → same; after completion slot frees within 1 s |
| FT7 | Staging cap | `cinder/tests/xfer.rs` | aggregate inbound staged > `FILE_STAGE_MAX_BYTES` → new offers `FileDecline`d automatically; local `file_state declined reason:"storage_full"` |
| FT8 | Path traversal on save | `cinder/tests/xfer.rs` | names `../../etc/passwd`, `/etc/passwd`, `a\\b.exe`, `..`, `nul`, name with NUL+ESC → every saved path `starts_with(save_dir)`; basename matches `^[A-Za-z0-9._ -]+$` or `file.bin`; symlinked save_dir → `conflict` |
| FT9 | Thumbnail bomb | `cinder/tests/xfer.rs` | crafted header 100 000×100 000 px PNG → `thumb:null`, file still offered; decode-time cap asserted (no alloc > 100 Mpx); 64 MiB-pixel JPEG rejected pre-decode |
| FT10 | e2e roundtrip | `cinder/tests/e2e.rs` | two `cinder rpc` procs + in-process relay: 4 MiB random file → peer `message.received kind:"file"`, `file_state` sequence offered→transferring→verifying→complete in order both sides, `file.save` bytes == source, wall <120 s localhost |
| FT11 | Voice-note shape | `cinder/tests/xfer.rs` | `kind:"voice"` + `audio` meta → `files.kind=='audio'`, `audio_meta` JSON round-trips duration/codec; missing `audio` → `-32602` |
| FT12 | Event throttle | `cinder/tests/xfer.rs` | 9 400-chunk transfer emits ≥ 4 and ≤ 200 `file_state` notifications total; transitions never out-of-order per file_id |
| FT13 | Idempotent send | `cinder/tests/rpc_file.rs` | same `client_xfer_id` twice → same `file_id`, `deduped:true`, exactly 1 `files` row + 1 `FileOffer` on wire |
| FT14 | Cancel semantics | `cinder/tests/xfer.rs` | mid-transfer `file.cancel` both directions → terminal `cancelled`, `FileCancel` observed by peer, further chunks dropped, `.part` freed |
| FT15 | Blindness re-run | `cinder/tests/e2e.rs` | needle bytes (known plaintext of file) appear 0 times in relay-captured bytes; every captured frame parses as valid `ClientMsg`/`ServerMsg` |
| FT16 | Orphan reaper | `cinder/tests/xfer.rs` | `.part` + row in `transferring` older than TTL → state `expired`, file unlinked; complete rows never reaped |
| FT17 | `message.received` untrusted fields | `cinder/tests/rpc_events.rs` | emitted `body.file` carries `trusted:false`; `name` containing ANSI appears stripped in TUI render test |
| FT18 | rpc.describe completeness | `cinder/tests/rpc_methods.rs` | describe covers ≥ 24 methods incl. all `file.*`; `file_state` in events catalog; `file:` prefix in conventions |
| FT19 | Session-death mid-transfer | `cinder/tests/xfer.rs` | kill TCP mid-transfer → sender `transferring` (not failed), resume after new handshake completes transfer; 0 hash mismatches across 20 runs |
| FT20 | dedup conflict | `cinder/tests/rpc_file.rs` | `client_xfer_id` reused with different path → `-32018` `conflict`, 0 new rows |

---

## 13. Open flags

1. **`FileOffer` re-emit on sender restart** (§4.2) — the chosen resume is
   receiver-driven `FileAccept{ranges}` + sender re-offer with identical
   `file_id`+`fetch_token`. Alternative considered: sender-driven
   `FileHave`-request variant; rejected (two resume paths). If impl finds a
   real gap (receiver never returns), revisit at code review, not by
   patching around it.
2. **Per-chunk hashes** — omitted deliberately (AEAD + whole-file hash +
   all-or-nothing). A variant allowing >64 MiB partial retention would need
   them; flagged as a future revision, not spec'd.
3. **`data` inline param size** — 1 MiB chosen (JSON line cap is 4 MiB; b64
   inflation ≈ 1.33×). A `path`-only API was considered; `data` retained for
   agent-generated images — flag if it complicates streaming impl.
4. **Auto-accept default `contacts`** — matches message acceptance
   semantics; `verified` is stricter-by-choice for high-risk users. If UX
   testing shows surprise disk use, flip default to `verified` before ship.
