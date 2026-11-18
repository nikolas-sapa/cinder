# cinder — Wire Protocol Specification

`PROTOCOL_VERSION = 1` (u16). This document is normative for `cinder-core`
(`proto` module) and `cinder-relay`. The end-to-end cipher suite is
`Noise_XX_25519_ChaChaPoly_BLAKE2s` via `snow` — no TLS anywhere; Noise IS the
crypto. The relay is a dumb blind TCP hub: it routes opaque frames by peer-id
and stores opaque mailbox blobs. It never sees keys or plaintext.

## 1. Constants

| Name | Value | Meaning |
|---|---|---|
| `PROTOCOL_VERSION` | `1` | u16, negotiated in `Hello`/`ServerHello` |
| `MAX_FRAME_HELLO` | `4096` B | hard cap on any frame before `Hello` completes |
| `MAX_FRAME_HANDSHAKE` | `4096` B | semantic cap on frames carrying Noise handshake payloads |
| `MAX_FRAME_TRANSPORT` | `16384` B | hard cap on every frame after `Hello` completes |
| `MAX_PLAINTEXT` | `8192` B | max application plaintext per transport message |
| `PAD_MIN` | `256` B | minimum padded plaintext block size |
| `PEER_ID_LEN` | `32` B | `SHA-256(x25519 static pubkey)` |
| `HS_ID_LEN` | `16` B | initiator-chosen handshake correlation id |
| `SESSION_ID_LEN` | `16` B | per-transport-session id, chosen by session initiator |
| `TOKEN_LEN` | `32` B | capability token (CSPRNG) |
| `MAILBOX_ID_LEN` | `32` B | random 256-bit mailbox id (NOT derived from peer-id) |
| `INVITE_TOKEN_LEN` | `16` B | rendezvous slot token |
| `MAILBOX_MAX_MSGS` | `500` | per-mailbox message count cap |
| `MAILBOX_MAX_BYTES` | `4 MiB` | per-mailbox byte cap |
| `MAILBOX_MAX_TTL` | `30 d` (`2592000` s) | enforced upper bound on mailbox TTL |
| `INVITE_MAX_TTL` | `72 h` (`259200` s) | enforced upper bound on invite slot TTL |
| `HANDSHAKE_TIMEOUT` | `10 s` | relay-side pending-handshake TTL |

## 2. Framing and serialization

Every unit on the wire is a length-prefixed frame:

```
[u32 BE len ‖ frame bytes]
```

- `len` = number of bytes in `frame bytes`, excluding the 4-byte prefix.
- The frame payload is exactly one bincode-serialized `ClientMsg` or
  `ServerMsg` depending on direction.

### Serialization format

bincode **1.3 legacy configuration** (the defaults of `bincode::serialize` /
`bincode::deserialize`): fixed-width little-endian integers, enum variant index
encoded as `u32`, sequences and `Vec`/`String` prefixed with `u64` length,
options as `u8` tag. Implementations MUST NOT enable varint encoding,
big-endian payload encoding, or trailing-byte extension — the wire format is
frozen by this spec. The `len` prefix itself is always big-endian `u32`.

### Length validation (MUST happen BEFORE allocation)

On every read:

1. Read 4 bytes → `len: u32` (BE).
2. Determine the cap for the current connection state:
   - before `Hello` exchange completes: `MAX_FRAME_HELLO` (4096)
   - after `Hello`: `MAX_FRAME_TRANSPORT` (16384)
3. If `len == 0` → `ProtocolViolation`, close connection.
4. If `len > cap` → `OversizeFrame`, close connection.
   A length prefix can never be resynchronized safely — any framing violation
   is fatal to the connection.
5. Only then allocate the read buffer and read exactly `len` bytes.
6. Deserialize. If the frame carries a Noise handshake payload
   (`Handshake`, `InviteAccept`) whose serialized `body`/`msg1` exceeds
   `MAX_FRAME_HANDSHAKE`, reject with `ProtocolViolation` (semantic cap —
   the outer frame may still be ≤ 16 KiB, but handshake material never is).
7. Bincode trailing bytes after a valid message → `ProtocolViolation`, close.

### Plaintext padding

Application plaintext is padded inside the Noise AEAD envelope (padding is
part of the transported plaintext, invisible to the relay):

```
padded_block = [u16 BE pt_len ‖ pt ‖ 0x00…]
block_size   = max(256, next_power_of_two(pt_len))   // pt_len ≤ 8192
```

The receiver strips padding by reading `pt_len` after AEAD-open. A `pt_len`
inconsistent with the block size, or nonzero padding bytes, MUST be treated as
a decrypt/integrity failure → fatal to the session (see §11). Maximum
ciphertext size per transport message: `8192 + 16` (AEAD tag) = `8208` B.

## 3. Core types

```rust
type PeerId      = [u8; 32];  // SHA-256(remote X25519 static pubkey)
type HsId        = [u8; 16];  // handshake correlation id, CSPRNG, per attempt
type SessionId   = [u8; 16];  // transport-session id, CSPRNG, per handshake
type Token       = [u8; 32];  // capability token, CSPRNG
type TokenHash   = [u8; 32];  // SHA-256(token) — the only thing relay stores
type MailboxId   = [u8; 32];  // CSPRNG; NOT derivable from any peer-id
type InviteToken = [u8; 16];  // rendezvous slot id+secret combined
```

All ids and tokens come from a CSPRNG (`rand::rngs::OsRng`). Nothing on the
wire is derived from a peer-id except `PeerId` itself.

```rust
enum HandshakeMsg {
    Msg1(Vec<u8>),   // Noise XX →    : e                      (≤ 4 KiB)
    Msg2(Vec<u8>),   // Noise XX ←    : e, ee, s, es           (≤ 4 KiB)
    Msg3(Vec<u8>),   // Noise XX →    : s, se                  (≤ 4 KiB)
}
```

Realistic sizes: Msg1 = 32 B, Msg2 = 96 B, Msg3 = 48 B (+ bincode overhead).
Anything near the 4 KiB cap is already anomalous; the cap exists only as a
hard bound.

```rust
struct DepositEnvelope {
    session_id: SessionId,   // groups frames of one Noise transport session
    body: Vec<u8>,           // Noise transport ciphertext, ≤ 8208 B
}

struct StoredEnvelope {
    msg_id: u64,             // relay-assigned, per-mailbox monotonic
    from: PeerId,            // depositor's CLAIMED peer-id — unauthenticated
    session_id: SessionId,
    body: Vec<u8>,           // opaque Noise transport ciphertext
    received_at: u64,        // relay unix time; informational only
}
```

## 4. `ClientMsg` — client → relay (C→R)

Every `to` field is a routing target; the relay stamps `from` with the
sender's claimed peer-id when forwarding. The first frame on a connection
MUST be `Hello`; anything else → `ProtocolViolation`, close.

```rust
enum ClientMsg {
    /// FIRST frame only. version = PROTOCOL_VERSION. peer_id is the client's
    /// claimed identity — the relay does not (cannot) verify it.
    /// Serialized ≤ 64 B; hard cap 4 KiB.
    Hello { version: u16, peer_id: PeerId },

    /// Create a one-time rendezvous slot. token is client-generated and becomes
    /// the slot's lookup key and secret. ttl_secs ≤ INVITE_MAX_TTL (72 h),
    /// ttl_secs == 0 → relay default (24 h). ≤ 64 B.
    InviteCreate { token: InviteToken, ttl_secs: u32 },

    /// Consume a rendezvous slot AND deliver Noise Msg1 in one frame (saves an
    /// RTT). Relay atomically deletes the slot and forwards a `Handshake` frame
    /// to the slot owner's connection. msg1 = serialized HandshakeMsg::Msg1.
    /// ≤ 4 KiB.
    InviteAccept { token: InviteToken, hs_id: HsId, msg1: Vec<u8> },

    /// Any Noise XX handshake message routed by destination peer-id.
    /// Reconnect path uses Msg1 here directly (no invite). hs_id chosen by the
    /// initiator, echoed in all 3 messages. ≤ 4 KiB.
    Handshake { to: PeerId, hs_id: HsId, body: HandshakeMsg },

    /// Opaque Noise transport ciphertext routed by destination peer-id.
    /// session_id lets the relay (and the peer) group frames that share a
    /// nonce stream. body = AEAD(padded plaintext). ≤ 8208 B body; whole
    /// frame ≤ 16 KiB.
    Transport { to: PeerId, session_id: SessionId, body: Vec<u8> },

    /// Register a mailbox. Client sends only token HASHES — the raw tokens
    /// never transit on this frame. mailbox_id is fresh CSPRNG output.
    /// ttl_secs ≤ MAILBOX_MAX_TTL (30 d); 0 → default 7 d. h_deposit gates
    /// deposits, h_fetch gates fetch/ack/rotate/close. ≤ 160 B.
    MailboxOpen { mailbox_id: MailboxId, h_deposit: TokenHash,
                  h_fetch: TokenHash, ttl_secs: u32 },

    /// Deposit an opaque envelope. token = raw deposit token; relay compares
    /// SHA-256(token) to stored h_deposit (constant-time). A full mailbox
    /// rejects with MailboxFull — the relay never drops individual frames,
    /// see §9. Frame ≤ 16 KiB.
    MailboxDeposit { mailbox_id: MailboxId, token: Token,
                     env: DepositEnvelope },

    /// Non-destructive fetch: returns up to max_msgs envelopes (oldest
    /// first); they remain stored until MailboxAck. max_msgs ≤ 500.
    /// ≤ 96 B.
    MailboxFetch { mailbox_id: MailboxId, token: Token, max_msgs: u16 },

    /// Delete fetched envelopes by msg_id. Ids not present are ignored.
    /// msg_ids.len() ≤ 500. ≤ ~4 KiB.
    MailboxAck { mailbox_id: MailboxId, token: Token, msg_ids: Vec<u64> },

    /// Rotate capability tokens. Authenticated by CURRENT fetch token.
    /// Atomically replaces both stored hashes; old tokens die immediately.
    /// ≤ 160 B.
    MailboxRotate { mailbox_id: MailboxId, token: Token,
                    new_h_deposit: TokenHash, new_h_fetch: TokenHash },

    /// Delete the whole mailbox and all contents. Authenticated by fetch
    /// token. ≤ 96 B.
    MailboxClose { mailbox_id: MailboxId, token: Token },

    /// Subscribe to online/offline status. peers.len() ≤ 256; replaces the
    /// connection's previous subscription set. ≤ ~8.4 KiB.
    PresenceSubscribe { peers: Vec<PeerId> },

    /// Liveness probe. Relay answers Pong. Clients SHOULD send at a jittered
    /// 45–75 s interval (see §12 idle timeout).
    Ping { nonce: u64 },
}
```

## 5. `ServerMsg` — relay → client (R→C)

```rust
enum ServerMsg {
    /// Reply to Hello. now_unix = relay wall clock (informational).
    ServerHello { version: u16, now_unix: u64 },

    /// Invite slot registered. expires_at is authoritative (ttl clamped).
    InviteCreated { token: InviteToken, expires_at: u64 },

    /// Forwarded handshake message — covers both the InviteAccept path
    /// (carrying Msg1) and the reconnect path (any of Msg1/2/3).
    /// `from` is the originator's claimed peer-id, stamped by the relay.
    Handshake { from: PeerId, hs_id: HsId, body: HandshakeMsg },

    /// Forwarded transport ciphertext.
    Transport { from: PeerId, session_id: SessionId, body: Vec<u8> },

    /// Mailbox registered; expires_at authoritative after TTL clamp.
    MailboxOpened { mailbox_id: MailboxId, expires_at: u64 },

    /// Deposit accepted; msg_id is the stored envelope's id.
    MailboxDeposited { msg_id: u64 },

    /// Fetch result (non-destructive). Oldest-first, ≤ requested max_msgs.
    MailboxMessages { msgs: Vec<StoredEnvelope> },

    /// Generic success for MailboxAck / MailboxRotate / MailboxClose /
    /// PresenceSubscribe.
    Ok,

    /// Presence event for a subscribed peer-id.
    PeerStatus { peer: PeerId, online: bool },

    /// Answer to Ping.
    Pong { nonce: u64 },

    /// Any failure. See §10 for codes and connection semantics.
    Error { code: ErrorCode, detail: String },   // detail ≤ 256 B, no secrets
}
```

## 6. Handshake sequence — Noise XX through the relay

`A` = initiator (acceptor or reconnecting peer), `B` = responder (inviter or
contact). The relay forwards `Handshake` frames opaquely by `to`/`from`
peer-id; it keeps a `pending_hs` entry keyed by `hs_id` for accounting only
(10 s TTL) and can parse nothing inside `body`.

```
 A (initiator)                 Relay                   B (responder)
     |                           |                         |
     |-- InviteAccept ---------->|  (or Handshake{Msg1} on  |
     |  {token, hs_id, Msg1}     |   reconnect path)        |
     |                           |-- Handshake ------------>|
     |                           |  {from:A, hs_id, Msg1:e} |
     |                           |                         |
     |                           |            B: read Msg1  |
     |                           |              write Msg2  |
     |                           |<- Handshake -------------|
     |<-- Handshake -------------|  {to:A,from:B,hs_id,     |
     |                           |   Msg2: e,ee,s,es}       |
     |  A: read Msg2             |                         |
     |    snow::get_remote_static()                        |
     |    → SHA-256 → constant-time compare                |
     |      vs PINNED peer-id (URI pubkey / contacts store)│
     |    MISMATCH → close session, loud error,            |
     |    Msg3 is NEVER sent                               |
     |  A: write Msg3          |                           |
     |-- Handshake ------------>|-- Handshake ------------>|
     |  {to:B, hs_id, Msg3:s,se}|                         |
     |                           |            B: read Msg3  |
     |                           |            get_remote_static()
     |                           |            → SHA-256 = A's peer-id
     |                           |            known contact → ct-compare
     |                           |            vs pinned; new contact →
     |                           |            TOFU pin (unverified)
     |=== both sides: snow → TransportState, fresh SessionId ===
     |    SAS material = H(handshake_hash ‖ min(pk_a,pk_b)   |
     |                     ‖ max(pk_a,pk_b))                 |
```

Pinning points (MUST be implemented — a missing check is Bitchat's MITM bug):

- **Initiator:** after reading Msg2, before writing Msg3. Compare
  `SHA-256(get_remote_static())` against the pinned peer-id (derived from the
  URI pubkey on first contact, or the contacts store on reconnect) using a
  constant-time 32-byte compare. Mismatch → abort, do not send Msg3, close.
- **Responder:** after reading Msg3. For an existing contact: constant-time
  compare vs pinned peer-id; mismatch → close + `security.identity_changed`
  semantics at app layer (blocks sending until dismissed — never silent
  rebind). For a new contact via invite: the learned key becomes the TOFU
  pin, contact enters `unverified` state.
- `hs_id` is generated fresh by the initiator for every handshake attempt and
  must be echoed unchanged in Msg2/Msg3 frames. A mismatched or unknown
  `hs_id` on Msg2/Msg3 → drop the frame (client side).

### SAS transcript binding

After the handshake completes, both sides compute identically:

```
sas_input = get_handshake_hash()            // snow, 32 B
          ‖ min(static_a, static_b)         // lexicographic on raw 32 B keys
          ‖ max(static_a, static_b)
sas       = SHA-256(sas_input)
```

Rendered to the user as 7 emoji (primary) or a digit string (fallback) per
the SAS spec in the TUI/agent layer. Because both statics are sorted, both
sides derive the same bytes without role negotiation. `sas` commits to this
exact handshake transcript — an active MITM cannot produce matching SAS.

## 7. Contact-add / rendezvous flow

### URI grammar

```
cinder://<base32-pubkey>@<relay-host>[:<port>]?rendezvous=<token>
```

- `<base32-pubkey>`: 32-byte X25519 static public key, RFC 4648 base32,
  lowercase, no padding (52 chars). `peer_id = SHA-256(pubkey)` is derived
  from it for routing and pinning.
- `<token>`: base32 of the 16-byte invite slot token (26 chars).
- Parsing MUST be strict: wrong lengths, bad alphabet, unknown query keys →
  reject.

### Sequence

```
 Inviter B                              Relay                    Acceptor A
    | `cinder id`                         |                         |
    |-- InviteCreate{token,ttl} --------->|  slot: token→B's peer-id  |
    |<-- InviteCreated{token,exp} --------|  single-use, expires      |
    |   prints URI, shares out-of-band    |                         |
    |                                     |     cinder add <uri>      |
    |                                     |<-- InviteAccept ----------|
    |                                     |   {token,hs_id,Msg1}      |
    |                                     |   slot CONSUMED (deleted) |
    |<-- Handshake{from:A,hs_id,Msg1} ----|   → forward to B          |
    |            [Noise XX per §6 completes through relay]            |
    |                                                                     |
    |  both sides: peer pinned (A: URI-anchored check; B: TOFU)           |
    |  contact state = UNVERIFIED on both sides                           |
    |  post-handshake transport msgs exchange mailbox grants (§8)         |
```

### Invite slot semantics

- **Single-use:** `InviteAccept` atomically consumes the slot. A second
  accept of the same token → `InviteInvalid`. An intercepted link is a dead
  link after one use.
- **Expiry:** slots expire at `expires_at` (relay-enforced, ttl clamped to
  ≤ 72 h). Expired or unknown token → `InviteInvalid`.
- **One outstanding invite per client** is recommended client-side; the
  relay caps slots per connection/IP (§12).

### Acceptor pending-contact states

The acceptor client tracks the add attempt (app-layer state, not on wire):

`connecting` → (handshake ok) → `unverified` contact.
`connecting` → (timeout / InviteInvalid / transport error) → `retry`
(backoff: e.g. 5 s, 30 s, 2 min, then give up) → `failed`.

`failed` entries keep the last error for the UI; they never auto-retry.

## 8. Capability tokens

### Format

- `token` = 32 bytes of CSPRNG output. The relay only ever stores
  `TokenHash = SHA-256(token)`.
- **Separate deposit and fetch tokens** per mailbox: the peer deposits with
  `deposit_token`, the owner fetches/acks/rotates/closes with `fetch_token`.
  Compromise of the deposit token (e.g. peer's device) grants spam ability
  but NOT read or delete access.

### Issuance

Tokens are generated by the mailbox owner and delivered to the peer **inside
the authenticated Noise transport channel** at contact-add — never through a
relay-visible cleartext path. Wire-level, this is an app message in the
established session:

```rust
// app-layer payload (inside Noise AEAD), NOT a relay-visible variant
struct MailboxGrant {
    mailbox_id: MailboxId,
    deposit_token: Token,     // what the peer uses in MailboxDeposit
    relay_hint: String,       // which relay the mailbox lives on
}
```

Immediately after the XX handshake at contact-add, each side sends its own
`MailboxGrant` (having already `MailboxOpen`ed on the relay). Mutual grants
are complete before either side reports the contact `unverified`/usable.

### Presentation and verification

`MailboxDeposit/Fetch/Ack` present the **raw** token; the relay computes
`SHA-256(presented)` and constant-time-compares with the stored hash.
`Rotate`/`Close` authenticate with the fetch token the same way.

- **Rotation:** `MailboxRotate` atomically swaps both hashes; the owner then
  delivers the new `deposit_token` to the peer inside the E2E channel. Old
  tokens die at rotate time. Recommended trigger: any peer key change, or
  every 90 d.
- **Revocation:** `MailboxClose` deletes mailbox + tokens. Rotating with a
  fresh random `h_deposit` effectively revokes a peer's deposit right.
- **Expiry:** tokens share the mailbox's `expires_at`. After expiry the
  mailbox and its hashes are gone — every op returns `UnknownMailbox`.

## 9. Mailbox semantics

- `mailbox_id` = fresh 256-bit CSPRNG value. Never derived from peer-id —
  knowing someone's identity must not reveal their mailbox.
- Caps: `MAILBOX_MAX_MSGS` = 500 **or** `MAILBOX_MAX_BYTES` = 4 MiB stored —
  whichever hits first → `MailboxFull` on deposit.
- TTL: `ttl_secs` at open, clamped to ≤ 30 d; expiry deletes mailbox +
  contents.
- **Atomic whole-session-batch drop:** if a relay must shed load or the
  mailbox is over quota at eviction/compaction, it MAY drop envelopes only in
  whole `session_id` groups — all frames of one session batch together.
  Dropping a subset desynchronizes the Noise nonce counter and bricks the
  session anyway, so partial drops are forbidden. Individual frames are
  dropped nowhere else: deposit is all-or-reject (`MailboxFull`), never
  truncate.
- Fetch is non-destructive (peek). `MailboxAck` deletes by `msg_id`;
  un-acked envelopes are redelivered on the next fetch. Clients SHOULD ack
  only after durably storing/decrypting.
- `from` on `StoredEnvelope` is the depositor's claimed peer-id —
  informational, unauthenticated; the real sender is proven inside the Noise
  plaintext.

## 10. Error codes

`ErrorCode` is a `u16` on the wire (bincode `u32` variant index — the numeric
codes below are the stable contract and MUST be matched by explicit
discriminants).

| Code | Name | Trigger | Connection after error |
|---|---|---|---|
| `0x0001` | `VersionMismatch` | `Hello.version != PROTOCOL_VERSION` | **close** (after sending error) |
| `0x0002` | `OversizeFrame` | `len` prefix > phase cap, or `len == 0` → see note | **close** (framing desync; error send best-effort) |
| `0x0003` | `RateLimited` | any §12 limit exceeded | stays open; **≥5 strikes/60 s → close** |
| `0x0004` | `UnknownMailbox` | mailbox_id unknown/expired | stays open |
| `0x0005` | `BadToken` | token hash mismatch | stays open; **≥10 strikes/60 s per IP → close** |
| `0x0006` | `MailboxFull` | deposit over 500 msgs or 4 MiB | stays open |
| `0x0007` | `HandshakeTimeout` | pending `hs_id` evicted after 10 s | stays open (attempt aborted) |
| `0x0008` | `ProtocolViolation` | wrong first frame, trailing bytes, handshake body >4 KiB, garbage | **close** |
| `0x0009` | `InviteInvalid` | invite token unknown/consumed/expired | stays open |
| `0x000A` | `PeerIdInUse` | `Hello` claims an already-connected peer-id | **close** (new connection rejected; existing session kept) |
| `0x000B` | `MailboxExists` | `MailboxOpen` with a live mailbox_id | stays open |

Errors carry a `detail` string ≤ 256 B, human-readable, and MUST NEVER
contain keys, tokens, or ciphertext.

## 11. Reconnect / resync rules

- A Noise `TransportState` is **never serialized, exported, or resumed**.
  Session death (TCP drop, decrypt failure, timeout, peer restart) ⇒ the
  session is destroyed and zeroized; a **new full XX handshake** is the only
  path back.
- **Any decrypt failure is fatal:** close session, zeroize, mark the peer
  pending. Never retry-skip a frame — a skipped frame desyncs the nonce
  stream permanently.
- The transport nonce is a local counter maintained identically on both
  ends; **nonce material never appears on the wire** — a frame carrying
  nonce-looking data is just ciphertext to be rejected by AEAD.
- On session death, the client marks the peer `pending` and queues outgoing
  app messages client-side (bounded per-peer queue, §12). After the new
  handshake completes, a **new `session_id`** is generated and the pending
  queue is flushed over the new transport — old `session_id` batches in a
  mailbox are dead ciphertext the peer will fail to decrypt and SHOULD be
  left to TTL or cleared by the owner.
- Re-handshake rate: client-side backoff (1 s, 2 s, 5 s, 15 s, 60 s cap,
  ±25 % jitter); relay-side subject to §12 limits.
- Peer-id pinning applies on every handshake (§6) — reconnects re-pin
  against the contacts store exactly like first contact.

## 12. Relay anti-abuse (normative limits)

All state is bounded; every queue has a hard cap and a defined shed policy.

| Resource | Limit |
|---|---|
| New TCP connections | 20 / min / IP |
| Handshake initiations (`InviteAccept` + `Handshake{Msg1}`) | 30 / 60 s / IP, burst 10 |
| Pending handshakes (`pending_hs`, 10 s TTL) | 8 / IP, 512 global; at cap → `RateLimited` |
| Handshake completion deadline | 10 s from Msg1 forward → evict + `HandshakeTimeout` |
| Invite slots | 8 / connection, 64 / IP; ttl ≤ 72 h |
| Mailboxes | 64 / IP; caps/TTL per §9 |
| Mailbox deposits | 60 / min / mailbox → `RateLimited` |
| Transport frames | 120 / s / connection → `RateLimited` |
| Inbound queue per connection | 256 frames, then `RateLimited` |
| Outbound queue per peer/connection | 256 frames; full → drop whole `session_id` batches first, still full → close the slow consumer |
| Idle timeout | 120 s without any frame → close (clients Ping at jittered 45–75 s) |
| Presence subscriptions | 4 frames / min / connection; ≤ 256 peers per set |

Rate-limit accounting is per-IP for pre-auth surfaces (connect, handshake,
slots, mailboxes) and per-connection for post-`Hello` traffic.

## 13. What the relay can and cannot see

| Relay CAN see | Relay CANNOT see |
|---|---|
| Claimed peer-ids of connected clients | Static private keys (never transmitted) |
| Which peer-ids exchange frames, when, and in which direction | Any plaintext — every payload is Noise ciphertext |
| Frame sizes and timing (lengths are padded but not constant-size) | Session keys, handshake internals (frames are opaque blobs) |
| IP addresses of all clients | SAS values / verification material |
| Invite token → owner-peer-id mapping, slot usage | Which human/agent a peer-id belongs to |
| Mailbox ids, counts, sizes, TTLs, token **hashes** | Message contents of mailbox blobs |
| `from`/`to` peer-id pairs on live traffic (social graph of live flows) | `deposit_token`/`fetch_token` raw values at rest (only SHA-256 stored) |
| `session_id` groupings (one opaque 16-byte tag per session) | Whether a transport frame is chat text vs. a mailbox grant vs. a ping inside E2E |

Residual risk (accepted, documented for v2): `Hello.peer_id` is an
unauthenticated claim — the relay has no key material to verify. An attacker
claiming a victim's peer-id is refused while the victim is connected
(`PeerIdInUse`); while the victim is offline the attacker can squat the id
and receive… ciphertext it cannot decrypt, while every real contact's
handshake fails peer-id pinning. Result is DoS/message-loss only, never
plaintext compromise. A proof-of-possession challenge at `Hello` is a v2
candidate.

## 14. Invariants checklist (grep-able MUSTs)

1. `len` validated against phase cap before any allocation; `len==0` or
   `len>cap` → close.
2. `SHA-256(get_remote_static())` constant-time compared to pinned peer-id
   after Msg2 (initiator) / Msg3 (responder); mismatch → close, no Msg3.
3. Decrypt failure → session destroyed, zeroized; never resume, never skip.
4. `TransportState` never serialized; nonce never read from the wire.
5. Padding: `pt_len` u16-BE prefix, zero-filled, next pow2 ≥ 256 B.
6. Relay stores `SHA-256(token)` only; raw tokens appear only in
   deposit/fetch/ack/rotate/close request frames.
7. Mailbox drops are whole-`session_id` atomic; no partial-frame eviction.
8. Invite slots single-use + expiring; mailbox ids and tokens CSPRNG.
9. `detail` strings and logs carry no keys, tokens, or ciphertext.
10. `PROTOCOL_VERSION` checked in `Hello`; mismatch → error + close.
