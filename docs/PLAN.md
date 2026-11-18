# CINDER — Implementation Plan

Spec for building cinder per `BRIEF.md` (authoritative). Test plan first,
implementation (milestone ladder) second. Every test has numeric pass criteria.

Repo: `/Users/nikolassapalidis/Developer/cinder`. Crates at root:
`cinder-core/`, `cinder-relay/`, `cinder/`.

Chosen defaults not specified in brief (flagged, injectable in tests):

| Knob | Default | Test value |
|---|---|---|
| Per-IP handshake rate limit | 10 handshakes/min | configurable, test trips at 10 |
| Pending-handshake cap (global) | 256 | test cap = 4 |
| Handshake timeout | 10s (brief-mandated) | tokio time-crate pause or 50ms injected |
| events replay log bound | 10,000 events/account | test cap = 4 |
| Per-client send queue | 256 frames | test cap = 4 |
| Keepalive | none (brief allows "or none") | n/a |
| Rekey policy | 1024 msgs OR 600s | test = 4 msgs / 50ms |

---

## 1. NON-GOALS (this version)

- No group chats. 1:1 only.
- No multi-device. One identity keypair per install.
- No mobile targets. Desktop terminals only.
- No federation. One relay serves one address book; relays never peer.
- No voice, no video, no file transfer.
- No read receipts (`msg_state` stops at `delivered` for outbound UX; no
  `read` state is ever transmitted).
- No Double Ratchet. vodozemac is a future design doc only — banned in CI
  (cargo deny). Ratchet-lite rekey (M6) is the whole story.
- No external security audit. Self-verified via the crypto MUSTs below.
- No message search indexing (FTS5 deferred to v2 per brief).
- No `events.ack` require-ack mode, no capability-scoped agent tokens, no
  `cinder mcp serve` (all v2 — schema designed so they slot in).

---

## 2. TEST PLAN (lands before implementation, per spec discipline)

Locations: unit tests inline `#[cfg(test)]` next to code; integration tests in
`<crate>/tests/`; proptests marked `proptest!`; fuzz in `fuzz/fuzz_targets/`.
Two-process e2e lives in `cinder/tests/e2e.rs` via `env!("CARGO_BIN_EXE_cinder")`.

### 2.1 cinder-core — frame codec

| # | Test | Location | Pass criteria |
|---|---|---|---|
| F1 | Oversize handshake frame rejected pre-alloc | `cinder-core/tests/frame.rs` | Feed `[u32 BE len=4097]` header only, 0 body bytes → decoder returns `Error::Protocol` immediately (not `None`/awaiting body); assert error within 1 poll |
| F2 | Oversize transport frame rejected pre-alloc | `cinder-core/tests/frame.rs` | len=16385 header only → same behavior as F1 |
| F3 | len-validated-before-alloc proof | `cinder-core/tests/frame.rs` | Header claims len=16384 (max valid) but stream holds 0 bytes → decoder returns `Ok(None)` (waiting); header claims 16385 → `Err` on header alone. Distinguishes "validate then alloc" from "alloc then validate" |
| F4 | Plaintext cap | `cinder-core/src/proto.rs` unit | `encode_plaintext(8193 bytes)` → `Err`; 8192 → `Ok` |
| F5 | Padding to next pow2 ≥256 | `cinder-core/tests/frame.rs` | 1,000 random plaintext lens in 1..=8192 → padded len ∈ {256,512,1024,2048,4096,8192}, padded ≥ input, padded < 2×input for input>256 |
| F6 | Enum round-trip | `cinder-core/tests/frame.rs` (proptest, 256 cases) | Every generated `ClientMsg`/`ServerMsg` survives encode→decode byte-identical |
| F7 | Fuzz | `fuzz/fuzz_targets/frame_decode.rs` | 60s `cargo fuzz run -- -max_total_time=60`, 0 panics, 0 OOM-abort |

### 2.2 cinder-core — Noise handshake + peer-id binding

| # | Test | Location | Pass criteria |
|---|---|---|---|
| H1 | XX both directions | `cinder-core/tests/handshake.rs` | A-initiates-B and B-initiates-A both reach transport mode; `get_remote_static()` equals peer's static on both sides; 1 ciphertext round-trip each way succeeds |
| H2 | Peer-id mismatch → close | `cinder-core/tests/handshake.rs` | Pin peer-id of key C, handshake with B → post-handshake `peer_id()` check fails, `Error::Protocol`/peer-id-mismatch returned, session state = Closed, 0 transport msgs emitted after |
| H3 | MITM substitution sweep | `cinder-core/tests/handshake.rs` | 50 trials, attacker static swapped in → detection 50/50 (100%) |
| H4 | Handshake bytes ≤ cap | `cinder-core/tests/handshake.rs` | All 3 XX handshake messages each ≤4KiB on the wire |

### 2.3 cinder-core — SAS

| # | Test | Location | Pass criteria |
|---|---|---|---|
| S1 | Determinism | `cinder-core/tests/sas.rs` | Same (handshake_hash, pk_a, pk_b) → identical 7-emoji string + identical digit fallback, 100/100 iterations |
| S2 | Order symmetry | `cinder-core/tests/sas.rs` | `sas(a,b) == sas(b,a)` for 100 random keypairs (min/max canonical ordering) |
| S3 | Cross-vectors | `cinder-core/tests/sas.rs` | ≥5 fixtures: handshake_hash + pubkeys → emoji computed by hand (SHA-256 in `sha2`, index table inlined in test). Byte-identical match required |
| S4 | Distinctness sanity | `cinder-core/tests/sas.rs` | 1,000 distinct keypairs → ≤10 full-string collisions (≤1%) |

### 2.4 cinder-core — contacts store

| # | Test | Location | Pass criteria |
|---|---|---|---|
| C1 | HMAC fail-closed | `cinder-core/tests/contacts.rs` | Valid file; flip each of first 32 file bytes in turn → 32/32 loads return integrity `Err`, 0 contacts returned |
| C2 | HMAC keying | `cinder-core/src/contacts.rs` unit | HMAC-SHA256 keyed by HKDF(static_secret): file valid under identity A fails under identity B, 100% of 20 trials |
| C3 | Atomic write survives kill -9 | `cinder-core/tests/contacts.rs` | Child proc loops `store.save()` with incrementing counter; SIGKILL at random 1–50ms offset; ×50 runs. After each kill, parent loads file: result is either complete v_n or complete v_{n+1} — 0 partial/corrupt reads across 50 runs (tmpfile+rename strategy; assert HMAC valid on every survivor) |
| C4 | Permissions | `cinder-core/tests/contacts.rs` | Written contacts.json mode = 0600, parent dir = 0700 (unix `MetadataExt`) |
| C5 | Trust transitions | `cinder-core/tests/contacts.rs` | unverified→verified persists across save/load; re-add with different static → state `key-changed` + `send_blocked == true` |

### 2.5 cinder-core — session (transport)

| # | Test | Location | Pass criteria |
|---|---|---|---|
| SE1 | Monotonic nonces | `cinder-core/tests/session.rs` | 1,000 sends → `send_count()` strictly 0,1,2…999; replayed captured ciphertext → decrypt `Err`, 0 accepts |
| SE2 | Decrypt failure = fatal | `cinder-core/tests/session.rs` | Corrupt 1 byte of valid ciphertext → `Err`, `session.is_closed()==true`, all subsequent `encrypt`/`decrypt` calls `Err` (≥10 attempts), 0 plaintext bytes emitted post-failure |
| SE3 | No decrypt after close | `cinder-core/tests/session.rs` | After fatal close, feed 100 more valid-format ciphertexts → 0 successful decrypts |
| SE4 | Rekey at msg threshold (injected) | `cinder-core/tests/session.rs` | `RekeyPolicy{msgs:4, interval:600s}` → after 4th send, rekey counter == 1 exactly; after 8th send == 2 |
| SE5 | Rekey at time threshold (injected) | `cinder-core/tests/session.rs` | `RekeyPolicy{msgs:u64::MAX, interval:50ms}` with 1 msg/10ms → rekey fires within 100ms; counter ==1 at t=60ms±tolerance |
| SE6 | Ordering across rekey | `cinder-core/tests/session.rs` | 3 msgs before + 3 after rekey boundary → peer decrypts all 6 in order, 0 failures, 0 retries |
| SE7 | Zeroize on close | `cinder-core/tests/session.rs` | After fatal/normal close, session key material probe (test-only accessor over `Zeroizing` fields) reads 0 non-zero bytes OR compile-guarantee: all key fields typed `Zeroizing`/`secrecy::Secret` — grep gate: `grep -n "Zeroizing\\|Secret" session.rs` ≥1 per key field |

### 2.6 cinder-relay

| # | Test | Location | Pass criteria |
|---|---|---|---|
| R1 | Echo routing | `cinder-relay/tests/echo.rs` | 2 in-process clients, frame routed by peer-id arrives byte-identical, <100ms |
| R2 | Per-IP rate limit trip | `cinder-relay/tests/limits.rs` | Limit=10/min: handshakes 1–10 from 127.0.0.1 accepted, 11th refused with protocol error; accepted count asserted == 10 exactly |
| R3 | Pending-handshake cap | `cinder-relay/tests/limits.rs` | Cap=4 injected: 4 stalled handshakes held, 5th refused immediately (TCP close <100ms); `pending_count` never >4 |
| R4 | Handshake timeout | `cinder-relay/tests/limits.rs` | Stalled handshake → connection closed at 10s±0.5s (or injected 50ms timeout, assert wall-clock <1s) |
| R5 | Mailbox count cap | `cinder-relay/tests/mailbox.rs` | Deposit 500 msgs → all accepted; 501st → rejected error; mailbox still exactly 500 |
| R6 | Mailbox byte cap | `cinder-relay/tests/mailbox.rs` | Fill to 4MiB total; 1 more byte → rejected; cumulative stored ≤4MiB asserted |
| R7 | TTL expiry | `cinder-relay/tests/mailbox.rs` | Injectable clock: msgs stamped now−29d kept, now−31d dropped on fetch AND purged within 1 sweep cycle; fetch returns exactly the non-expired subset |
| R8 | Capability-token auth | `cinder-relay/tests/mailbox.rs` | Deposit with wrong token → rejected 20/20 wrong tokens; deposit token cannot fetch, fetch token cannot deposit (cross-scope attempts 0% accepted) |
| R9 | Relay stores H(token) only | `cinder-relay/tests/mailbox.rs` + grep gate | Serialize mailbox store → raw token bytes never appear: `assert!(!store_bytes.windows(32).any(|w| w==token))`; CI grep: token fields stored only as `Sha256` digest type |
| R10 | Atomic batch drop | `cinder-relay/tests/mailbox.rs` | 5 queued transport frames for a dead session → after session death, mailbox holds 0 or 5 frames, never 1–4 (50 trials) |
| R11 | Mailbox-id unguessable | `cinder-relay/src/mailbox.rs` unit | 100 generated ids: each 32 bytes, all unique, none equal to any peer-id bytes |
| R12 | Relay blindness (also in e2e) | `cinder/tests/e2e.rs` | See I2 |

### 2.7 cinder rpc (stdio + unix-socket daemon)

| # | Test | Location | Pass criteria |
|---|---|---|---|
| P1 | Happy path, all methods | `cinder/tests/rpc_methods.rs` | Table-driven: `session.hello`, `rpc.describe`, `msg.send`, `msg.edit`, `msg.delete`, `msg.react`, `chat.list`, `chat.history`, `contact.list`, `contact.add`, `contact.accept`, `contact.decline`, `invite.create`, `invite.accept`, `ident.verify`, `events.subscribe`, `events.unsubscribe`, `shutdown.request` → 18/18 return JSON-RPC result (not error) with required schema fields present |
| P2 | `rpc.describe` pre-auth | `cinder/tests/rpc_methods.rs` | Called before any auth/setup → returns schema covering ≥18 methods + ≥8 event kinds |
| P3 | Error frame per line-parseable input | `cinder/tests/rpc_errors.rs` | ≥8 malformed inputs (`{`, `[]`, `{"method":null}`, unknown method, wrong param types, missing id, batch array, 4MiB+1 line) → each produces exactly 1 stdout line parseable as JSON-RPC error with `data:{kind,retryable,hint}` all present. 0 silent drops |
| P4 | client_msg_id idempotency | `cinder/tests/rpc_msg.rs` | `msg.send` same `client_msg_id` twice → identical `msg_id`, second response `deduped:true`, relay receives exactly 1 message |
| P5 | events.subscribe replay w/ since= | `cinder/tests/rpc_events.rs` | Emit 5 events; subscribe `since`=seq2 → receive seqs 3,4,5 in order, strictly monotonic, 0 duplicates |
| P6 | Gap marker on expiry | `cinder/tests/rpc_events.rs` | Replay bound injected cap=4, emit 10, subscribe `since`=evicted seq → first frame is `event.gap` event with resume seq, then remaining events; silent-drop count == 0 (gap marker present, not absence) |
| P7 | Line cap | `cinder/tests/rpc_errors.rs` | 4MiB+1-byte line → error frame + connection handling, 0 panic; exactly-4MiB line → processed or clean `Err`, never panic/hang <5s |
| P8 | stdout purity | `cinder/tests/rpc_stdout.rs` | Full scripted session (hello→describe→send→subscribe→shutdown): every stdout byte belongs to a newline-delimited JSON frame — each line `serde_json::from_str` Ok; inject `log::warn!` mid-session → warn bytes appear on stderr, stdout frame count unchanged |
| P9 | shutdown | `cinder/tests/rpc_methods.rs` | `shutdown.request` → exit code 0; queued-but-unsent msg reports `failed` event before exit |
| P10 | No head-of-line block | `cinder/tests/rpc_daemon.rs` | 3 unix-socket clients; client3 pauses reads 500ms → client1+client2 events still arrive <2s; per-client queue (cap 256) overflows → that client disconnected with error, others unaffected |
| P11 | trusted:false schema | `cinder/tests/rpc_events.rs` | `message.received` payload contains `"trusted": false` on body field (grep on emitted JSON, exact) |

### 2.8 Integration (two spawned `cinder rpc` processes + in-process relay)

| # | Test | Location | Pass criteria |
|---|---|---|---|
| I1 | Round-trip | `cinder/tests/e2e.rs` | Spawn relay in-process + two `cinder rpc` processes; contact add (direct peer-id path until M5, rendezvous after) → send "MARKER-PLAINTEXT-7f3a" → peer's `message.received` body matches exactly; `msg_state` events observed in order queued→sent→delivered |
| I2 | Relay blindness | `cinder/tests/e2e.rs` | Relay wraps listener with byte-capture; plaintext needle appears 0 times in relay-bound bytes across whole session (`needle.matches(captured).count() == 0`); captured bytes all decode as frames or Noise ciphertext — no plaintext field |
| I3 | Latency | `cinder/tests/e2e.rs` | send→`message.received` <2s on localhost, 20 runs, p95 <2s |

### 2.9 Perf sanity

| # | Test | Location | Pass criteria |
|---|---|---|---|
| PF1 | Handshake speed | `cinder/tests/perf.rs` | Full XX handshake <500ms localhost, 20 runs, all <500ms |
| PF2 | msg.send→event | `cinder/tests/perf.rs` | `msg.send` accepted → `message.received` at peer <2s, 20 runs |
| PF3 | TUI windowed fetch proof | `cinder/tests/tui_store.rs` | Seed 10,000 history rows; instrumented query log: every SELECT has `LIMIT ≤100`; startup→first render <1s; rows materialized ≤500 |

### 2.10 Fuzz

| # | Target | Location | Pass criteria |
|---|---|---|---|
| Z1 | frame_decode | `fuzz/fuzz_targets/frame_decode.rs` | 60s `-max_total_time=60`, 0 panic. Needs nightly — documented in CI section |
| Z2 | handshake_handler (relay-side unauth bytes) | `fuzz/fuzz_targets/handshake_handler.rs` | 60s, 0 panic, pending-handshake counter never exceeds cap under fuzz input |

### 2.11 proptest — session state machine

`cinder-core/tests/session_state.rs`, `proptest!` 500 cases, ops drawn from
{send, recv, corrupt-ciphertext, close, rekey, peer-id-mismatch-handshake}.
Invariants asserted every step:

- V1: post-Close successful decrypts == 0
- V2: accepted-ciphertext nonce sequence strictly increasing per direction
- V3: `Trusted` state reachable only after peer-id check passed (mismatched-pin strategies never reach Trusted — count == 0)
- V4: `rekey_count ≤ floor(sent/threshold) + 1` under msg-threshold policy

### Test totals

| Component | Named tests | proptest cases | fuzz targets |
|---|---|---|---|
| cinder-core | 25 | 2 suites (256 + 500 cases) | 2 |
| cinder-relay | 11 | — | shared |
| cinder rpc/daemon | 11 | — | — |
| integration/perf | 6 | — | — |
| **Total** | **53** | **756 cases** | **2** |

---

## 3. MILESTONE LADDER (each: compiles + tests green)

### M1 — cinder-core wire proto + relay echo

- **Entry:** workspace skeleton; `lib.rs` declares `proto`/`session` but files
  don't exist → workspace currently does NOT compile. Fix is part of M1.
- **Deliver:** `proto.rs` (peer-id type, `ClientMsg`/`ServerMsg` enums,
  len-prefixed tokio-util codec with pre-alloc len validation, padding),
  minimal `session.rs` stub satisfying lib.rs exports, relay TCP listener
  routing frames by peer-id (echo/route only — no mailbox yet).
- **Exit:** `cargo test --workspace` green; F1–F6 pass; R1 pass; manual
  `nc 127.0.0.1 <port>` echo-check scripted into test (no manual step left).
- **Files touched:** `cinder-core/src/proto.rs` (new), `cinder-core/src/session.rs`
  (new stub), `cinder-core/tests/frame.rs` (new), `cinder-relay/src/{main,server}.rs`
  (new), `cinder-relay/tests/echo.rs` (new), `cinder-relay/Cargo.toml`
  (+`cinder-core`, `tokio`), `cinder-core/Cargo.toml` (snow `"0.9"` → `"0.9.6"` pin).
- **Rollback:** `git revert` milestone commit; relay crate self-contained —
  delete `cinder-relay/src/` to fall back to core-only.

### M2 — Noise session, peer-id binding, SAS, contacts store

- **Entry:** M1 green; proto codec stable.
- **Deliver:** real `session.rs` — `Identity` (X25519 static, secrecy/zeroize),
  XX initiator+responder wrappers on snow, post-handshake
  `get_remote_static`→SHA-256→constant-time peer-id verify (fail = close +
  loud error), `Sas` (7-emoji + digit fallback, canonical min/max ordering),
  `contacts.rs` (serde store, HMAC-SHA256 keyed by HKDF(static_secret),
  tmpfile+rename atomic write, 0600/0700), key storage (keyring crate
  primary → age-encrypted file fallback, passphrase TTY-only).
- **Exit:** H1–H4, S1–S4, C1–C5, SE1–SE3, SE7 green. `#![forbid(unsafe_code)]`
  on crate root verified by grep gate (0 `unsafe` hits in `cinder-core/src`).
- **Files touched:** `cinder-core/src/{session,identity,sas,contacts,keystore}.rs`
  (new/rewrite), `cinder-core/src/lib.rs` (export updates),
  `cinder-core/tests/{handshake,sas,contacts,session}.rs` (new),
  `cinder-core/Cargo.toml` (+`hmac`,`hkdf`,`zeroize`,`secrecy`,`keyring`,
  `age`,`subtle`,`tempfile` dev-dep).
- **Rollback:** revert to M1 `session.rs` stub; contacts/keystore modules are
  additive — gate behind removal of lib.rs exports.

### M3 — `cinder rpc` stdio protocol (+ unix-socket daemon)

- **Entry:** M2 green. Two peers can handshake + exchange ciphertext.
- **Deliver:** `cinder` bin real `main.rs` + clap subcommands (`rpc`,
  `daemon`, `id`, `add`), JSON-RPC 2.0 engine (newline-delimited, 4MiB line
  cap, flush-per-frame, error frame per parseable line), all 18 methods,
  idempotency map `client_msg_id→msg_id`, bounded persisted replay log
  (monotonic seq, gap marker), per-client bounded send queues, daemon =
  same engine over unix socket + SO_PEERCRED/getpeereid + 0600 dir.
  Stdout = frames only; all logs stderr.
- **Exit:** P1–P11, I1–I3 green. (`I1` uses direct peer-id contact add;
  `invite.*`/rendezvous methods return `kind:"unimplemented"` until M5 —
  error-frame contract still enforced.) `AGENT-API.md` drafted.
- **Files touched:** `cinder/src/{main,rpc/mod,rpc/methods,rpc/events,daemon,id}.rs`
  (new), `cinder/tests/{rpc_methods,rpc_errors,rpc_msg,rpc_events,rpc_stdout,rpc_daemon,e2e,perf}.rs`
  (new), `cinder/Cargo.toml` (+`serde_json`,`jsonrpc`-types or hand-rolled
  types — flag choice, `tokio`,`futures`,`base32`/`data-encoding`,
  `dirs`,`libc` for peercred), docs `AGENT-API.md`.
- **Rollback:** bin crate is terminal node — `git revert`; core+relay
  unaffected. Daemon behind `daemon` subcommand — can ship M3 stdio-only by
  deferring `daemon.rs` without breaking stdio tests.

### M4 — TUI chat

- **Entry:** M3 green; rpc engine proven by e2e.
- **Deliver:** ratatui TUI as an rpc client of the daemon. Layout per brief
  (conversation list / chat pane / status / input via ratatui-textarea),
  slash commands `/help /add /verify /finger /clear /quit`, Alt-1..9 buffer
  jump, M-a next-unread, C-g fuzzy switch, M-k $EDITOR, PgUp/PgDn scroll,
  ticks `…/✓/✓✓` (no read state). Store: rusqlite + bundled-sqlcipher,
  windowed fetch only.
- **Exit:** PF3 green (windowed fetch proof, <1s first render, ≤100-row
  LIMIT); headless `TestBackend` render smoke ≥5 screens, 0 panic; cargo deny
  ratatui↔crossterm lockstep ban rule active and demonstrated failing on a
  drifted pair (test-the-gate: temporary mismatched pair must go red).
  `keybindings.md` written.
- **Files touched:** `cinder/src/{tui/mod,tui/app,tui/ui,tui/store,main}.rs`,
  `cinder/tests/tui_store.rs` + render tests, `cinder/Cargo.toml` —
  **bump ratatui 0.29→0.30 + crossterm 0.28→0.29** (skeleton ships the older
  matched pair; brief mandates 0.30/0.29 — if 0.30 unavailable at build time,
  keep newest matching pair + FLAG per brief), +`rusqlite/bundled-sqlcipher`,
  `ratatui-textarea`, `qrcode`. `deny.toml` lockstep rule.
- **Rollback:** TUI is additive on top of rpc — revert `tui/` dir; `cinder rpc`
  keeps working. Version bump isolated in one commit for clean revert.

### M5 — relay mailbox + capability tokens + rendezvous

- **Entry:** M3 green (relay echo already live since M1).
- **Deliver:** relay mailbox module (per-mailbox 500 msg / 4MiB caps, TTL ≤30d,
  256-bit random mailbox ids, atomic batch drop on session death), capability
  tokens issued inside authenticated channel at contact-add (deposit/fetch
  scoped; relay stores H(token)), rendezvous one-time invite slots →
  `invite.create`/`invite.accept` + `contact.add {link}` full path,
  per-IP rate limits, pending-handshake cap, 10s timeout.
- **Exit:** R2–R11 green; P1 happy-path now covers `invite.*` for real;
  I1 re-run through rendezvous-link flow (not direct add).
- **Files touched:** `cinder-relay/src/{mailbox,tokens,limits}.rs` (new),
  `cinder-relay/src/server.rs` (wire-in), `cinder-relay/tests/{mailbox,limits}.rs`,
  `cinder-relay/Cargo.toml` (+`sha2`,`subtle`,`rand`,`dashmap` or std map),
  `cinder/src/rpc/methods.rs` (invite/contact.add real impl),
  `cinder-core/src/proto.rs` (mailbox msg variants — additive enum arms).
- **Rollback:** mailbox is relay-internal — revert relay crate to M1 echo
  behavior; online messaging unaffected. Rendezvous falls back to direct-add.

### M6 — ratchet-lite rekey

- **Entry:** M2 session stable, M5 green.
- **Deliver:** `RekeyPolicy{msgs, interval}` injectable into `Session`;
  `rekey_manually` with fresh X25519 ephemerals triggered at ≥1024 msgs or
  ≥600s (defaults), whichever first; rekey handshake re-verifies peer-id
  (key-changed → `security.identity_changed` event + send-block).
- **Exit:** SE4–SE6 green; proptest suite (V1–V4) 500 cases green; 0 decrypt
  retries observed across all session tests (retry counter == 0 — decrypt
  failure is always fatal, never retried).
- **Files touched:** `cinder-core/src/session.rs`, `cinder-core/src/lib.rs`
  (export `RekeyPolicy`), `cinder-core/tests/session_state.rs` (proptest),
  `cinder/src/rpc/events.rs` (emit `security.identity_changed`),
  `cinder-core/Cargo.toml` (+`proptest` dev-dep).
- **Rollback:** `RekeyPolicy{msgs:u64::MAX, interval:u64::MAX}` disables
  rekey without code change — config-level rollback, no revert needed.

---

## 4. CI / RELEASE PIPELINE

- **Toolchain:** `rust-toolchain.toml` at root pins stable (`channel =
  "1.8x"`, components `clippy,rustfmt`). Fuzz job alone uses `nightly` +
  `cargo-fuzz`. All builds/tests `--locked`.
- **Gates per push/PR:**
  1. `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`
  2. `cargo test --workspace --locked`
  3. Unsafe gate: `grep -rn "unsafe" cinder-core/src cinder-relay/src` → must
     return 0; `grep -rn "unsafe" cinder/src` → hits allowed ONLY inside
     `cinder/src/hardening.rs`. `cargo geiger` report: unsafe count outside
     hardening module == 0. Policy: `#![forbid(unsafe_code)]` on cinder-core +
     cinder-relay; `#![deny(unsafe_code)]` on cinder bin with single
     `#[allow(unsafe_code)]` on `mod hardening` (libc RLIMIT_CORE / mlockall
     `#[cfg(unix)]` / PR_SET_DUMPABLE `#[cfg(target_os="linux")]`).
  4. Crypto guardrail greps: `grep -rn "tls\\|openssl\\|native-tls" */src`
     → 0 hits; `grep -rn "libsignal\\|vodozemac" Cargo.lock` → 0 hits;
     `grep -c "get_remote_static" cinder-core/src/session.rs` ≥ 1;
     `grep -n "Zeroizing\\|Secret" cinder-core/src` ≥ 1 per key field.
  5. `cargo deny check`: advisories + licenses (allow MIT/Apache-2.0/BSD/
     ISC/Unicode-DFS; **deny AGPL** = libsignal wall) + bans:
     `ratatui`/`crossterm` lockstep pair rule (only the designated matched
     versions permitted), `vodozemac`, `libsignal-*`, `openssl`, multiple-
     versions of `curve25519-dalek` banned; `curve25519-dalek` minimum
     4.1.3 enforced (RUSTSEC-2024-0344) via deny + audit.
  6. Fuzz smoke (nightly, required): 60s each target, `no panic` gate.
  7. Demo gif: `.tape` (vhs) regenerated in CI, artifact uploaded.
- **cargo audit:** on every `Cargo.lock` diff AND weekly cron; any advisory
  → fail. RUSTSEC-2024-0344 specifically pinned in deny config.
- **Release (cargo-dist):** git tag → cargo-dist builds per-target archives +
  installers → `minisign` signs every artifact (pubkey in repo) →
  `cargo cyclonedx` SBOM attached per release → GitHub Release with
  checksums + signatures + SBOM. `cargo-dist` plan job runs on PR to keep
  release config green.

---

## 5. DEFINITION OF DONE (mirrors brief) + phase→test map

DoD checklist (all must hold simultaneously):

- [ ] `cargo test --workspace` green — all 53 named tests + proptests
- [ ] Integration test proves relay-blindness (I2: needle count == 0)
- [ ] `cargo audit` clean; fuzz smoke 60s/target, 0 panic
- [ ] `cinder rpc` round-trips a message between two spawned processes (I1)
- [ ] Demo gif regenerates from `.tape` in CI
- [ ] `README.md` + `THREAT-MODEL.md` + `AGENT-API.md` + `keybindings.md` present
- [ ] Every crypto MUST grep-verifiable: `Noise_XX_25519_ChaChaPoly_BLAKE2s`,
      `get_remote_static` + constant-time compare, `Noise_XX` + `rekey_manually`,
      `Zeroizing`, 4KiB/16KiB/8KiB caps, 0600/0700, RLIMIT_CORE in hardening.rs

Phase → test mapping:

| Milestone | Tests that must be green at exit |
|---|---|
| M1 proto+relay echo | F1–F6, R1 |
| M2 session/SAS/contacts | H1–H4, S1–S4, C1–C5, SE1–SE3, SE7 |
| M3 rpc+daemon | P1–P11, I1–I3 (I1 via direct-add) |
| M4 TUI | PF3, render smoke ×5, deny-lockstep red-test |
| M5 mailbox+tokens | R2–R11, P1 full incl. invite.*, I1 via rendezvous |
| M6 rekey | SE4–SE6, proptest V1–V4 (500 cases), retry-count==0 |
| Release | Z1–Z2 (60s each), PF1–PF2, audit clean, dist+minisign+SBOM |

---

## FLAGS / CONTRADICTIONS FOUND IN BRIEF vs SKELETON

1. `cinder/Cargo.toml` ships `ratatui 0.29` + `crossterm 0.28`; brief mandates
   `0.30` + `0.29`. Plan: bump in M4 (or at skeleton fix — pair must move
   together; brief's "if 0.30 unavailable use newest matching pair and FLAG"
   applies if crates.io disagrees).
2. `lib.rs` exports `proto`/`session` items that don't exist — workspace
   doesn't compile out of the box. M1 entry criteria covers this.
3. `snow = "0.9"` in cinder-core Cargo.toml — brief says ≥0.9.6. "0.9"
   resolves to 0.9.6 but pin `"0.9.6"` for auditability (M1).
4. `curve25519-dalek ≥4.1.3` is transitive via snow — no direct dep exists to
   pin; enforced through cargo-deny minimum-version ban + audit instead.
5. Brief lists `cinder daemon` as a core interface but task milestone ladder
   had no daemon milestone — folded into M3 (same engine, second transport;
   deferrable within M3 without breaking stdio tests).
6. `contact.add {link}` / `invite.*` need rendezvous tokens = relay token
   infra → sequenced to M5. M3/M4 contact-add tests use direct peer-id path.
   `invite.*` returns typed `unimplemented` error frames in M3–M4 (error
   contract still tested).
7. Chosen-but-unspecified defaults (rate limit 10/min, pending cap 256,
   replay bound 10k, send queue 256, no keepalive) — all injectable, all
   listed in the defaults table. Flag to orchestrator if any should differ.
