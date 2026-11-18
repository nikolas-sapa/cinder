# cinder — Code Audit

Read-only audit of the implementation against `BRIEF.md`, `docs/PROTOCOL.md`,
`docs/AGENT-API.md`, `docs/THREAT-MODEL.md`, `docs/SECURITY-REVIEW.md`.
Every finding cites `file:line`. No source was modified; nothing committed.

Counts: **CRITICAL 0 · HIGH 1 · MEDIUM 3 · LOW 3**

| Sev | Finding |
|---|---|
| HIGH | Key-change trust state is dead code: `record_key_change`/`KeyChanged`/`is_send_blocked` are unreachable in production; a peer-id mismatch emits a transient event but persists nothing and never produces `key_changed_blocked`. |
| MED | Declined/blocked contacts still complete inbound handshakes and deliver messages; `MetaState` is only consulted on the outbound send path. |
| MED | Rekey control frame silently dropped when a message is deposited to the peer's mailbox; local egress key still rotates → next live frame fails AEAD → fatal session kill (self-desync). |
| MED | `msg.react` never calls `check_send_allowed`; reactions propagate to blocked/declined contacts. |
| LOW | Reaction `emoji` length unbounded; >8 KiB values store locally then wedge the pending queue (encode fails padding cap on every delivery attempt). |
| LOW | Relay stores raw `InviteToken`s as `HashMap` keys in memory (mailbox capability tokens are correctly hashed; invites are not). |
| LOW | `security.identity_changed` event reports `"new_fingerprint": null`, discarding the actual mismatched identity, and `contact.request`/`contact.connected` semantics for declined contacts are ungated (pairs with MED-1). |

---

## Findings

### H-1 — Key-change detection is dead code; key-changed state never persists (HIGH)

**Where**
- `cinder-core/src/session.rs:362-376` — `Session::detect_key_change` is defined but has **no production caller** (only `cinder-core/tests/*` call it).
- `cinder-core/src/contacts.rs:186-205` — `ContactStore::record_key_change` is only reachable via `detect_key_change`; hence `TrustState::KeyChanged` is unreachable at runtime.
- `cinder-core/src/contacts.rs:213-219` — `is_send_blocked` can never return `true`; the `key_changed_blocked` RPC error (`cinder/src/core.rs:1914-1920`) is unreachable.
- `cinder/src/core.rs:1519-1539` — on `NetErr::PeerIdMismatch` the outbound path emits `security.identity_changed` with `trust:"key_changed"` but does **not** call `record_key_change`, does not touch the stored contact, and reports `"new_fingerprint": Value::Null`.
- `cinder/src/tui/app.rs:281-285` — TUI sets `Trust::KeyChanged` from that one-shot event; the store (`contact.list`/`chat.list`) still reports `"verified"` on next read.

**Why it matters.** BRIEF/THREAT-MODEL require "key change → record `key_changed` → block sends until re-verify". In code: the Noise pin itself fails closed (`session.rs:214-228` + `handshake_flow.rs` gate before Msg3 — a changed static *cannot* establish a session, so no MITM bypass is possible), but every layer above it is inert. A peer-id collision under a changed key produces only a transient UI event.

**Exploit sketch.** Relay/peer operator rotates (or swaps) the far-end static key. Client attempts `msg.send` → handshake → `PeerIdMismatch` → TUI flashes "key changed" once, RPC sees `peer_offline` per retry, `contact.list`/`chat.list` keep reporting `verified`. An agent client relying on the `trust` field — or a user who missed the one-shot event — treats a compromised identity as verified forever. Nothing ever yields `key_changed_blocked`.

**Fix.** Surface the learned remote static in `NetErr::PeerIdMismatch` (or read `session.remote_static()` before drop), call `contacts.record_key_change(peer, actual)` in `outbound_handshake`'s mismatch arm, populate `new_fingerprint` from `actual` peer-id, and emit `contact.connected`/state refresh so persisted trust agrees with the event. Delete or wire `detect_key_change`.

### M-1 — Declined/blocked contacts keep a live inbound channel (MEDIUM)

**Where**
- `cinder/src/core.rs:1560-1581` — `respond` runs the responder handshake for any Msg1 whose `from` passes `contacts.get`; `MetaState` is never consulted.
- `cinder/src/core.rs:1131-1183` — `on_session_ready` installs the session and (for a declined contact still in `contacts`) continues to the "known contact" branch, emitting `contact.connected`/`security.verification_needed` at 1221-1252 rather than rejecting.
- `cinder/src/core.rs:1902-1911` — `check_send_allowed` enforces `Declined`/`Blocked` **only on outbound**.
- `MetaState::Blocked` has no production writer at all (dead enum arm; set only in tests).

**Why it matters.** `contact.decline` is advertised as refusing further requests (AGENT-API `contact.decline`; THREAT-MODEL auto-decline of repeat requests). In code, a declined peer can re-handshake freely: session installs, `contact.request` re-emits (core.rs:1242-1252), and `handle_app` still stores + emits their messages. Decline is cosmetic on the inbound side.

**Exploit sketch.** Harasser is declined → re-initiates Noise (their identity/key unchanged) → session installs → messages and edits land in the store and on `message.received`, plus a fresh `contact.request` event per re-handshake. The user can never actually refuse.

**Fix.** In `respond`/`on_session_ready` (and `handle_app` as a belt), drop sessions/frames whose `meta.state ∈ {Declined, Blocked}` before install; emit nothing. Populate `MetaState::Blocked` or remove the variant.

### M-2 — Rekey control frame dropped on mailbox-deposit path → guaranteed session desync (MEDIUM)

**Where**
- `cinder/src/core.rs:1303-1323` — `deliver_now` computes `(ct, ctrl)` via `encrypt` + `maybe_rekey`; when `offline_deposit` is true it returns `self.deposit(peer, sid, ct, item)` at 1321-1323 — `ctrl` is **discarded, never deposited**.
- `cinder-core/src/session.rs:391-428` — `rekey(false)` already rotated the egress key (`rekey_outgoing` at 421-424). The peer will never see the empty-plaintext control unit that triggers `rekey_incoming` (decrypt path 339-343).

**Why it matters.** PROTOCOL §11: the control frame "MUST be transmitted in order … before the next encrypt output". Depositing skips it entirely, so the next live frame under the rotated key fails AEAD at the peer → `DecryptFatal` → session destroyed.

**Exploit sketch.** Any 1024th outbound message (or first after 10 min) sent while the peer is offline-with-grant silently breaks the session; next online message kills it. An attacker who can infer counter/window boundaries can't force it, but timing makes it inevitable in long-lived offline-heavy sessions.

**Fix.** Deposit `ctrl` before `ct` in the same atomic `session_id` mailbox batch, or defer `rekey_outgoing` until a live-send actually ships the control frame.

### M-3 — `msg.react` bypasses the contact-blocked send gate (MEDIUM)

**Where**
- `cinder/src/core.rs:2412-2458` — `msg_react` resolves the peer and enqueues `AppMsg::React` without `check_send_allowed` (contrast `msg_send` 1931, `msg_edit` 2303, `msg_delete` ~2345).
- `cinder/src/rpc/methods.rs:314-332` — RPC layer only validates `msg_id` prefix + non-empty emoji.

**Why it matters.** Blocked/declined/key-changed contacts still receive reaction writes — inconsistent enforcement of the same policy the other mutating ops enforce.

**Exploit sketch.** After `contact.decline`, an agent calls `msg.react` on any stored msg_id in that chat → `deliver_or_queue` ships `AppMsg::React` to the declined peer.

**Fix.** Call `self.check_send_allowed(&peer).await?` after `peer_of_chat` resolution, mirroring `msg_edit`.

### L-1 — Reaction emoji unbounded (LOW)
`methods.rs:326-330` checks only non-empty; `core.rs:2424-2430` stores it, `padding.rs` then rejects >8192 B at encode — the pending entry is retried forever and the reaction persists locally. Peer-supplied emoji strings also reach `message.received` unbounded. Fix: cap `emoji` (e.g. ≤64 B) in `msg_react` and enforce in `handle_app` for inbound reactions.

### L-2 — Invite tokens raw in relay memory (LOW)
`cinder-relay/src/mailbox.rs:75,343-403` — `invites: HashMap<InviteToken, InviteSlot>` holds live 16-byte bearer tokens in cleartext, unlike mailbox capability tokens which store only `sha256` (`token_ok`, lines 92-95). A relay memory read exposes all pending invites for race-consumption. Fix: key the map by `H(token)` and compare constant-time, same as `token_ok`.

### L-3 — Identity-change event discards the new fingerprint (LOW)
`cinder/src/core.rs:1534-1535` — `new_fingerprint` is hard `null` although `PeerIdMismatch{actual}` is available at the error site; agents can't show what key was actually presented. Fix: carry `actual` through `NetErr::PeerIdMismatch` into the event.

---

## Checklist verdicts

1. **Peer-id binding — PASS.** `verify_peer_id` (session.rs:214-228) derives `PeerId::from_static_key` from `get_remote_static` (capture at 447-454), constant-time `ct_eq`, `fatal_close(PeerIdMismatch)` on miss. `finish_handshake` (244-279) refuses without `pin_ok`; `accept_learned_peer` (233-240) funnels TOFU through the same gate so `pin_ok` is the *only* path to `Trusted`. Initiator: `accept Msg2 → verify → produce Msg3` (`handshake_flow.rs` initiator) — gate before Msg3 confirmed; Msg3 write also self-blocks at session.rs:138-142. Responder: `accept Msg3 → learned peer-id → pin-or-TOFU` — verified. Claimed `from`/Hello strings never feed the comparison.

2. **Decrypt-fatal — PASS.** AEAD failure → `fatal_close(DecryptFatal)` at session.rs:331-336; padding decode failure → 348-351; handshake-variant and AEAD anomalies fatal at 169-208; encrypt failure fatal at 302-307. `fatal_close` drops hs/transport/sas (459-464; snow offers no zeroize — noted limitation). No retry/skip path exists. Core-side: dead `session_id`s tombstoned and dropped pre-decrypt (`core.rs:1600-1616`, bound-mismatch tombstone at ~1637); decrypt failure inside `handle_transport` closes+tombstones. Mailbox-fetch path deposits ciphertext opaquely — client decrypts under the same fatal rules.

3. **Frame/plaintext caps — PASS.** `frame.rs` parses `[u32 BE len]`, rejects 0 and `>max_frame` *before* `reserve`; bincode `reject_trailing_bytes`. `pre_hello()`=4096, `transport()`=16384. Semantic caps layered: handshake payload ≤4 KiB (`session.rs:186-189`, `server.rs:756-760`), envelope body ≤8210 (`server.rs:866`), `MailboxFetch.max_msgs ≤500` (`server.rs:888`), `ack ≤500` (`server.rs:904`), `PresenceSubscribe ≤256` (`server.rs:934`/`proto.rs:205-206`), plaintext ≤8192 (`padding.rs` `MAX_PLAINTEXT` + exact-size + zero-padding checks). RPC side: 4 MiB line cap with drain (`codec.rs:32-65`), 8192 body cap (`methods.rs:212-217`, 283-288).

4. **Nonces/session ids — PASS.** No nonce field is deserialized into Noise state; `TransportState` never serialized; `SessionId::random()` per `finish_handshake` (session.rs:275) and per `RelayTransport::connect` (transport.rs `connection_id`). `Ping.nonce` is echo-only protocol data, not a Noise nonce. Tombstone map bounded (8192/10 min, core.rs:1090-1098).

5. **Capability tokens — PASS.** `MailboxEntry` stores `h_deposit`/`h_fetch` `TokenHash` only (`mailbox.rs:29-30,197-198`); `token_ok` = `sha256(presented)` + `ct_eq` (92-95); deposit→`h_deposit` (226), fetch/ack/rotate/close→`h_fetch` (266, 290, 312, 329) — separation enforced. Relay sheds whole `session_id` batches on expiry/eviction (atomic batch drop). Raw `Token` `Debug` redacted via `secret_id!` (proto.rs); `TokenHash` is public-by-design. Invite tokens raw — see L-2.

6. **Contacts HMAC — PASS.** `contacts.rs` `load` computes `self.mac(file.payload)` → `ct_eq` → `ContactsIntegrity` on mismatch **before** payload JSON parse; no empty-store fallback (tamper ⇒ `Core::open` error ⇒ client refuses to start — true fail-closed). Key via HKDF over static secret. Writes atomic via `keystore.rs` temp-file+rename at 0600.

7. **Secret hygiene — PASS with caveats.** `Token`/`InviteToken` redacted Debug/Display; `PeerId`/`HsId`/`SessionId`/`TokenHash`/`MailboxId` public (correct). `Session` manual Debug excludes all key material (session.rs:517-529). Identity secret wrapped in `secrecy` + zeroized on Drop (`identity.rs`). `MailboxGrant` persists hashes only. `tracing::debug!` sites log peer-ids/error kinds only (core.rs:1293,1299,1308,1542,1578); relay `Error.detail` truncated ≤256 B (proto.rs). `PeerGrant`/own-mailbox tokens legitimately persist in encrypted meta (they're held capabilities, not peer secrets). Passphrase via `rpassword` TTY, never argv/env. Caveat: `cinder://` URIs embed `?rendezvous=` invite tokens; `main.rs` `id` prints the URI to stdout — the invite token reaches terminal/logs/history — matching spec behavior but note the exposure in §"not verified".

8. **Unsafe confinement — PASS.** `ci/check-unsafe.sh` → OK: `forbid` on `cinder-core` + `cinder-relay` lib/main roots; `deny` on `cinder` crate root (`main.rs`); `unsafe`/`#[allow(unsafe_code)]` tokens only in `cinder/src/hardening.rs` (RLIMIT_CORE/mlockall/prctl) and `cinder/src/rpc/peercred.rs` (`getpeereid`/`SO_PEERCRED`); ≤1 allow per file.

9. **Prompt-injection surface — PASS with one residue.** Every peer-derived render path is sanitized: `chat_title` sanitizes `display_name` (`ui.rs:59-64`), message text via `wrap_text(&sanitize(&m.text), …)` (`ui.rs:182`), finger-overlay name (`mod.rs:272-274`); `sanitize` strips CSI/OSC/DCS/charset, C0/C1, bidi, zero-width (`sanitize.rs`). Peer ids/msg-ids are base32 — safe. `status_line`/`Unknown`/error strings carry only internal or user-typed text; `last_message` previews are stored but never rendered in the chat list. RPC marks every inbound body `"trusted":false` (`describe.rs` TRUSTED_DESC + `message_json` via `dir`); `message.received` schema repeats it. Residual: relay `Error.detail` strings can ride RPC error `hint`s into the TUI `status_line` un-sanitized — bounded to static 256-B strings today, but worth a defensive `sanitize()` on `status_line` setters (LOW, unlisted above: no attacker-controlled escape payload currently reaches it).

10. **SAS + key-change — PARTIAL (H-1).** Formula exact: `H(hh ‖ min‖max)` over statics (session.rs:531-544), 7 emoji `digest[i]&0x3f`, digit fallback; `sas` computed per `finish_handshake` (277) → most recent handshake wins; `ident.verify` compares normalized input against current emoji+digits then `mark_verified` (core.rs:2726). `security.verification_needed` carries `sas_hint` with dedup (1256-1283). Persistence/`is_send_blocked`/`KeyChanged` broken per H-1; TUI `/verify` blocks locally on `Trust::KeyChanged` (mod.rs:200-204) but that state is event-transient only.

11. **Relay blindness — PASS.** `server.rs` only inspects routing fields (`to`/`session_id`/`mailbox_id`/payload *lengths*); bodies forwarded opaquely (`from: conn.peer_id` stamped at 786/801/821 — relay-attested connection identity, not sender-claimed per-frame). No decrypt call exists in the crate; no body content is logged (no tracing of bodies/envelopes). Queue-shed drops whole `session_id` batches (197+). Mailbox stores hashes + opaque `StoredEnvelope`s.

12. **Deny-by-default — PASS with M-1 caveat.** `Hello.peer_id` is routing metadata verified only via Noise pin; `from` stamps are relay-side. Invite tokens single-use + TTL (`mailbox.rs:394-403` consume-once); mailbox ops require token-hash match; `cinder://` URI parse is strict (`identity.rs`); responders adopt learned peer-ids as *unverified* TOFU (`accept_learned_peer` → `add_unverified` at core.rs:1140); `from`/`to` never drive trust inside `handshake_flow`. Caveat M-1: `MetaState::Declined/Blocked` is not consulted inbound, so a *policy* denial isn't enforced at the session layer (cryptographic identity is still verified — this is policy, not auth).

---

## Commands run

| Command | Result |
|---|---|
| `bash ci/check-unsafe.sh` | **PASS** — "forbid on libs, unsafe confined to hardening.rs/peercred.rs" |
| `bash ci/check-tui-lockstep.sh` | **PASS** — ratatui 0.30.2 + crossterm 0.29.0 matched pair |
| `bash ci/check-guardrails.sh` | **PASS** |
| `cargo audit` | **0 vulnerabilities**; 5 warnings, all `unmaintained` (`zbus_names` RUSTSEC-2026-0371, `zvariant` -0370, `zvariant_derive` -0372, +2 in the same zbus/keyring chain) — allowed by `deny.toml`/`audit` config; no fixed versions exist |
| `cargo deny check advisories` | **PASS** — `advisories ok` |

## Not verified / limitations

- `cargo test` suite was **not run** (audit scope = requested commands only; CI script greps were verified to gate what they claim).
- `docs/AGENT-API.md` was consulted via targeted greps (trusted-marking, sas_hint, error table, example frames), not read cover-to-cover.
- `cinder-core/src/proto.rs` mid-section and `cinder/src/rpc/core.rs`/`protocol.rs` were partially read (enough to confirm field bounds, error envelope `{code,message,data:{kind,retryable,hint}}`, and ID types); every enum arm wasn't line-read.
- `snow`/`sqlcipher`/`rusqlite` internals (nonce internals, SQLCipher key handling) are dependency-trusted, not audited.
- Linux-only `prctl(PR_SET_DUMPABLE)` and `SO_PEERCRED` paths are cfg-gated; compiled paths verified by reading, not executed (host is macOS).
- `store.rs` column-level secret hygiene checked by grep (no token/key columns found) rather than full schema read.
- Whether a *declined* inbound peer re-emits `contact.request` on every re-handshake depends on the `PendingIn` branch at core.rs:1200-1230, read partially — the M-1 core claim (no inbound gate anywhere) is fully verified regardless.
- Em-dash rule not applicable; no commits made.
