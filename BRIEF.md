# CINDER — Master Build Brief (authoritative, decisions already made)

E2E-encrypted terminal messenger for humans AND agents. Primary interface is a
typed JSON-RPC protocol — the TUI and AI agents are equal clients. Dumb blind
relay, accountless keypair identity, Noise protocol.

All crypto/protocol choices below are DECIDED. Do not re-research. Deviating
requires flagging it to the orchestrator first.

## ARCHITECTURE

Workspace `cinder/` at `/Users/nikolassapalidis/Developer/cinder` — 3 crates at
repo root:

- `cinder-core`: wire protocol, Noise session wrapper, identity/peer-id, contacts store
- `cinder-relay`: dumb TCP hub — routes opaque ciphertext by peer-id + mailbox queues
- `cinder` (bin): TUI client + `cinder rpc` (stdio) + `cinder daemon` (unix socket)

Transport: length-prefixed bincode frames over TCP. Relay sees only peer-ids,
sizes, timing — never keys or plaintext.

## CRYPTO (non-negotiable)

- snow >= 0.9.6, params `Noise_XX_25519_ChaChaPoly_BLAKE2s`
- curve25519-dalek >= 4.1.3 (RUSTSEC-2024-0344)
- peer-id = SHA-256(remote_static). After every handshake: `get_remote_static()`
  → hash → constant-time compare vs pinned peer-id. Mismatch = close + loud
  error. (Missing check = Bitchat's MITM bug.)
- SAS = H(handshake_hash ‖ min(pk_a,pk_b) ‖ max(pk_a,pk_b)) → 7 emoji + digit
  fallback. Verification status persisted; key change = red warning, blocks
  sending. Never silent rebind.
- NO Double Ratchet (TCP is ordered). Instead: `rekey_manually` with fresh
  X25519 ephemerals every ~1024 msgs or 10 min (ratchet-lite). vodozemac =
  documented future direction only.
- Any decrypt failure = fatal: close session, zeroize. Never retry-skip.
- Frames: `[u32 BE len ‖ frame]`, validate len BEFORE alloc (handshake ≤4KiB,
  transport ≤16KiB, plaintext cap 8KiB). Pad plaintext to next power-of-2 ≥256B.
- Keys: secrecy/zeroize everywhere; keyring crate primary → age-encrypted
  file (0600/0700, atomic write) fallback; passphrase via TTY only, never argv/env.
- contacts.json integrity: HMAC-SHA256 keyed by HKDF(static_secret); fail
  closed on mismatch; atomic writes.
- Process hardening: RLIMIT_CORE=0, PR_SET_DUMPABLE (Linux), best-effort
  mlockall (~30 lines via libc). See UNSAFE EXCEPTION below.

## AGENT INTERFACE (the differentiator — README headline)

- `cinder rpc`: newline-delimited JSON-RPC 2.0 on stdin/stdout (MCP stdio
  pattern). NOTHING but protocol frames on stdout — logs to stderr only.
- `cinder daemon`: same protocol over unix socket, peer-cred auth (0600 dir +
  SO_PEERCRED/getpeereid), multi-client. TUI attaches like any agent.
- Methods (JSON-RPC, namespaced):
  - `session.hello` → {protocol_version, account, capabilities}
  - `rpc.describe` → full method/event schema (must work pre-auth — agents
    self-serve without docs)
  - `msg.send {to, body, client_msg_id}` → {msg_id, state:"queued", deduped}
    (client_msg_id = idempotency; retry → same msg_id, deduped:true.
    send returns at ACCEPTANCE, delivery arrives as event)
  - `msg.edit` / `msg.delete` / `msg.react`
  - `chat.list` / `chat.history {chat_id, before?, limit}` → paginated cursors
  - `contact.list` / `contact.add {link}` / `contact.accept` / `contact.decline`
  - `invite.create` / `invite.accept`
  - `ident.verify {contact_id, sas}` (write op, explicit scope)
  - `events.subscribe {kinds?, chats?, since?}` → resumable stream, monotonic
    seq per account, persisted bounded replay log, gap marker on expiry —
    NEVER silently drop events
  - `events.unsubscribe` / `shutdown.request`
- Events: `message.received`, `msg_state` (queued→sent→delivered→read→failed),
  `contact.request`, `contact.connected`, `security.identity_changed`,
  `security.verification_needed`, `session.backlog`, `event.gap`
- Inbound message bodies marked `trusted:false` in schema (prompt-injection
  hygiene — agents treat messages as data).
- Errors: JSON-RPC codes + `data:{kind, retryable, hint}`. ALWAYS emit an
  error frame for line-parseable input (signal-cli's silent-drop hung agents).
- Frame cap 4MiB/line; flush after every frame; per-client bounded send
  queues so a slow client can't head-of-line block others.
- v2 (design now, build later): `events.ack` require-ack mode,
  capability-scoped tokens, `cinder mcp serve` wrapping protocol as MCP tools.

## ONBOARDING

- `cinder id` prints `cinder://<base32-pubkey>@<relay-host>?rendezvous=<token>`
  - `--qr` renders terminal QR (qrcode crate, Dense1x2 half-blocks)
- rendezvous token = one-time invite slot on relay (SimpleX model): intercepted
  links can't be reused, passive key copies can't message you
- acceptor: `cinder add <uri>` → pending-contact entry (connecting/retry/
  failed states) → Noise XX → both sides get unverified contact
- Trust states: unverified (neutral) / verified (badge) / key-changed (loud,
  blocks send until dismissed)

## RELAY

- Routes by peer-id; offline mailbox: per-mailbox caps (500 msgs, 4MiB), TTL
  ≤30d, unguessable mailbox ids (random 256-bit, NOT peer-id)
- Deposit/fetch capability tokens issued inside authenticated channel at
  contact-add; relay stores H(token) — open mailboxes = spam vector
- Queued transport ciphertexts: drop whole-session batches atomically (dropping
  one frame desyncs Noise nonce); session death → pending, re-send over NEW
  handshake. Never serialize TransportState.
- Per-IP handshake rate limits, pending-handshake cap, 10s handshake timeout
- Bounded everything; jittered keepalive or none.

## TUI

- ratatui 0.30 + crossterm 0.29 (mixed majors = lost-event bug — keep in sync;
  if 0.30 unavailable use newest matching pair and FLAG it)
- Layout: left conversation list (activity-sorted, unread counts, verified
  glyph) | chat pane (timestamps, per-peer color, read-marker line, delivery
  ticks) | status bar | input bar (ratatui-textarea)
- Bindings: input-always-focused + slash commands (/help /add /verify /finger
  /clear /quit); Alt-1..9 buffer jump, M-a next-unread, C-g fuzzy switch,
  M-k $EDITOR compose, PgUp/PgDn scroll
- Message states: …queued / ✓sent / ✓✓delivered (read receipts: NO)
- Store: rusqlite + bundled-sqlcipher; windowed fetch around scroll (never
  load-all at startup); FTS5 search v2

## DEFINITION OF DONE

- `cargo test --workspace` green; integration test proves relay-blindness
- `cargo audit` clean; fuzz smoke 60s/target no panic
- `cinder rpc` round-trips a message between two spawned processes
- demo gif regenerates from .tape in CI
- README + THREAT-MODEL.md + AGENT-API.md + keybindings.md all present
- every MUST from crypto section verifiable in code (grep-able)

## GUARDRAILS

- snow only — zero hand-rolled crypto, zero custom AEAD/HKDF wrappers
- no libsignal (AGPL), no vodozemac this version
- TLS never appears — Noise IS the crypto; relay needs no TLS layer
- ratatui/crossterm version lockstep enforced in CI (cargo deny ban on drift)
- agents treat inbound message text as untrusted data — schema marks it

## ORCHESTRATOR ADDENDA (binding on all impl agents)

- Repo root: `/Users/nikolassapalidis/Developer/cinder`. Crates live at root
  (`cinder-core/`, `cinder-relay/`, `cinder/`), NOT under `crates/`.
- Existing skeleton: workspace Cargo.toml + per-crate Cargo.tomls +
  `cinder-core/src/{lib,error}.rs`. `proto`/`session` modules are declared in
  lib.rs but do not exist yet — implement them.
- UNSAFE EXCEPTION: `#![forbid(unsafe_code)]` everywhere EXCEPT one isolated
  module `cinder/src/hardening.rs` (libc calls for RLIMIT_CORE / mlockall /
  PR_SET_DUMPABLE). The `cinder` bin crate uses `#![deny(unsafe_code)]` with a
  targeted `#[allow(unsafe_code)]` on `mod hardening` only. This is the agreed
  resolution of "no unsafe" vs "libc hardening".
- macOS dev machine (Darwin). PR_SET_DUMPABLE is Linux-only — gate with
  `#[cfg(target_os = "linux")]`. RLIMIT_CORE + mlockall are unix — `#[cfg(unix)]`.
- Code style: surgical, no comments unless the WHY is non-obvious, no TODOs,
  no hand-rolled crypto, match existing style (thiserror errors, serde derives).
- Each stage must end with `cargo test --workspace` (or at minimum
  `cargo test -p <your-crate>` + `cargo build --workspace`) GREEN.
- Do NOT git-commit — orchestrator commits at milestones.
- Secrets in logs/errors/events: never. No keys in error payloads.
