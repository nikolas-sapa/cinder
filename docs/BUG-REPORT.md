# cinder — Security & Correctness Bug Hunt Report

Read-only review of `cinder`, `cinder-core`, `cinder-relay`. Focus: attacker-
and peer-controlled input, session/state-machine correctness, async races,
resource bounds, error handling, crypto/protocol logic, TUI safety, wire/serde
compatibility, dead code, portability.

Status tags: **CONFIRMED** = exact path traced end to end. **SUSPECTED** =
strong evidence, trigger plausible but not fully exercised.

All line numbers refer to the tree under review (`/Users/nikolassapalidis/Developer/cinder`).

---

## Findings, ranked by likelihood × severity

### 1. CONFIRMED — `mailbox_drain` self-deadlocks `op_lock`; all relay request ops freeze

`Core::mailbox_drain` acquires the op lock and keeps the guard alive across
the entire drain:

```rust
let _g = self.op_lock.lock().await;            // core.rs:998
...
let _ = self.request(ClientMsg::MailboxAck{..}, ReqSlot::Ok).await;  // :1033
```

`request()` unconditionally takes the same non-reentrant `tokio::Mutex`:

```rust
let _g = self.op_lock.lock().await;            // core.rs:919
```

Trigger: link-up while our relay mailbox holds ≥1 envelope (i.e. any offline
delivery). `on_link_up` → `mailbox_drain` → fetch returns envelopes →
`ack_ids` non-empty → `request(MailboxAck)` waits on the lock the same task
already holds → waits forever. The `REQ_TIMEOUT` at :935 only wraps `rx`, never
the lock acquisition at :919, so it cannot fire.

Blast radius: the dead `_g` keeps `op_lock` held, so **every** later
`request()` — `ensure_mailbox`, `MailboxDeposit`, `InviteCreate`,
`PresenceSubscribe`, `MailboxAck` — pends forever. Offline delivery to granted
peers dies silently; the drain also parks the `on_link_up` task. Recovery only
when the link drops and the spawn is aborted (idle-timeout ~90 s + reconnect),
then it recurs on the next link-up with pending mail.

`cinder/src/core.rs:998`, `:1033`, `:919`, `:935`. Severity: high.

### 2. CONFIRMED — failed `contact.add` wedges the contact in `pending_out` forever; re-add permanently rejected

`start_add` records `MetaState::PendingOut` + `substate="connecting"`
(`core.rs:2030-2032`) and refuses re-add unless the existing state is
`Declined` (:2022-2028). Every failure path in `outbound_handshake`
(:1519-1544) calls `fail_pending` but never resets `meta.state`/`substate`,
and `on_link_up` (:875-880) never retries PendingOut contacts — the comment at
:2045 ("retried when the link comes up") is false.

Triggers: expired/used invite (`InviteInvalid`), handshake timeout, peer
id-mismatch, adding while offline, wrong-relay invite accepted while offline
(the `host != ours` guard at :2036 is skipped when `conn_state != Connected`,
:2044). In all cases the contact is stuck `pending_out/"connecting"`; every
future `contact.add` on the same link returns `Conflict (:2024)`; no API path
other than removing the contact can clear it. `ensure_handshake` is never
re-armed for it either (:1495).

`cinder/src/core.rs:2022-2033`, `:2044-2047`, `:1519-1544`, `:875-880`,
`:1181`. Severity: medium-high functional wedge.

### 3. CONFIRMED — peer can rewrite/delete OUR sent-message history (unscoped Edit/Delete/React)

Inbound `AppMsg::Edit`/`Delete`/`React` are applied with the peer-supplied
`msg_id` and no check that the target message is inbound or belongs to
`chat_str(peer)`:

- `handle_app` Edit → `s.edit_message(&msg_id, &body)` (`core.rs:1784-1791`)
- Delete → `s.soft_delete(&msg_id)` (:1808-1814)
- React → `s.add_reaction(&id, &e, &reactor)` (:1829-1843)

`edit_message`/`soft_delete` filter only on `deleted = 0` (`store.rs:895-918`) —
no `dir`/`chat_id` constraint. A contact edits or tombstones the messages WE
sent them (they trivially know those `msg_id`s): our store then shows forged
outgoing bodies (`revision`/`edited_at` bumped) or wipes our sent history.
Cross-chat targeting is blocked only by `msg_id` unguessability, not by any
authorization check. Emitted `"message.received"` events then propagate the
forgery to RPC subscribers/TUI.

`cinder/src/core.rs:1784-1858`, `cinder/src/store.rs:895-918`. Severity:
high integrity (history falsification), trivial trigger for any contact.

### 4. CONFIRMED — messages queued while offline are never flushed on reconnect

`deliver_or_queue` queues the item and calls `ensure_handshake`
(`core.rs:1423`), which returns immediately when
`conn_state() != Connected` (:1495-1496) — queue grows, no driver armed.
`on_link_up` (:875-880) runs only `ensure_mailbox`, `mailbox_drain`,
`presence_subscribe`; nothing walks `pending_out`/`hs_inflight`/`meta` to
re-kick delivery. The only recovery is `PeerStatus{online:true}` with a
non-empty queue (:827-840) — but `presence_subscribe` only subscribes
`MetaState::Active` contacts (:1054), and a peer who never reconnects, or a
non-active contact, gets no kick. Pending sends to a peer who was already
online simply sit `queued` until the user sends another message to that peer
or an inbound session happens to install (`flush_pending`, :1426).

`cinder/src/core.rs:1399-1424`, `:875-880`, `:1495-1496`, `:1054`.
Severity: medium-high (silent message loss from the user's view).

### 5. CONFIRMED — any relay `Error` frame aborts ALL in-flight handshakes and drains ALL request slots

`route()` broadcasts a single `ClientEvent::Error` to every handshake waiter
and consumes every request slot:

```rust
for tx in senders { let _ = tx.try_send(ev.clone()); }   // core.rs:852-854
for slot in [invite, mbox_open, deposit, ok] { ... }     // :855-866
reqs.fetch.take()                                        // :867
```

Relay `Error` frames are also used for **non-fatal** per-request replies
(`InviteInvalid`, `MailboxFull`, `RateLimited`, `UnknownMailbox`, … — relay
`SendError` vs `SendErrorThenClose` in `cinder-relay/src/server.rs`). So one
failed deposit/invite while a handshake is in flight injects `Error` into
that handshake's `DemuxLink::recv` (:395-403) → `Err(Protocol)` → session
attempt dies → `fail_pending` marks queued messages failed. Unrelated ops
that genuinely failed leave every other slot's waiter to hit the 15 s timeout.

`cinder/src/core.rs:842-869`, `:395-403`, `:1443-1457`. Severity: medium
(reliability/cross-op interference).

### 6. CONFIRMED — unbounded responder-task spawn and unbounded `early` map growth

(a) `inbound_loop` spawns a `respond` task per queued Msg1 with no concurrency
bound (`core.rs:1548-1555`). `INBOUND_CAP` (64) only bounds the channel; the
drain rate is unbounded, so a relay-side attacker at the ~120 msg/s transport
bucket keeps ~1200 full Noise-XX responder tasks (10 s timeout) alive —
CPU/memory burn for free. `hs_inflight` gates only outbound.

(b) `handle_transport` parks frames in `sh.early` keyed by `PeerId`, cap 8
frames **per peer** but no cap on peer count (:1624-1633); entries are
removed only by `on_session_ready` (:1156-1162) or `on_link_down` (:900).
`from` is relay-stamped but attacker-chosen (registering a fresh identity is
free), so many fake peers × 8 × up to 16 KiB bodies accumulate for the life of
the link.

`cinder/src/core.rs:1548-1555`, `:1624-1633`, `:1090-1099` (only `dead_sids`
is bounded). Severity: medium DoS.

### 7. CONFIRMED — unread counters never reset; `Store::mark_read_upto` is dead in production

`insert_message` increments `chats.unread` for every inbound message
(`store.rs:790-797`). The only reset, `mark_read_upto` (:1072-1082), has no
production caller — grep finds it only in `cinder/tests/store.rs`. The TUI's
`ChatBuffer::pending_mark` (`tui/app.rs:18`, consumed :245-246) only moves the
visual read-marker; nothing persists read state, so `unread` and `read_upto`
never update. Result: unread badges grow monotonically across restarts; the
read-marker reappears at the wrong place after reload.

`cinder/src/store.rs:790-797`, `:1072-1082`; `cinder/src/tui/app.rs:18`,
`:200-206`, `:245-246`. Severity: medium UX/state correctness.

### 8. CONFIRMED — `TrustState::KeyChanged` / send-block machinery is unreachable dead code

`check_send_allowed` gates on `ContactStore::is_send_blocked` →
`KeyChangedBlocked` (`core.rs:1914-1919`), and `TrustState::KeyChanged` exists
(`contacts.rs:197-221`), but the only writers — `record_key_change`
(`contacts.rs:186`), `dismiss_key_change` (:205), `Session::detect_key_change`
(`session.rs:362`) — have zero production callers (tests only). Moreover a
static-key change produces a *different* `PeerId`, so the responder side can
never even observe "same peer, new key"; initiator-side changes surface only
as `PeerIdMismatch` → `security.identity_changed` event (`core.rs:1519-1538`)
with `new_fingerprint: null`. Net effect: `key_changed` trust state,
`KEY_CHANGED_BLOCKED`, and the TUI's red ✕ glyph can never occur — the API
advertises a security gate that cannot fire.

`cinder-core/src/session.rs:362-372`, `cinder-core/src/contacts.rs:186-221`,
`cinder/src/core.rs:1914-1919`, `:2126-2127`, `:2876-2877`. Severity:
low-impact dead code, but misleading security surface.

### 9. CONFIRMED — `PendingContact` bookkeeping is entirely dead

`ContactStore::add_pending` / `set_pending_state` / `remove_pending` /
`pending` (`contacts.rs:251-279`) have no production callers; `MetaState`
pending bookkeeping lives in `meta` instead, and finding 2 shows the failure
paths never update it. The persisted `pending` table is write-never/read-never
in production — invite acceptance/decline history is silently lost.

`cinder-core/src/contacts.rs:251-279`. Severity: low (dead code).

### 10. CONFIRMED — `MailboxGrant.fetch_token_hash` stores the deposit-token hash

`AppMsg::Grant` handler stores `Sha256(deposit_token)` into **both**
`deposit_token_hash` and `fetch_token_hash` (`core.rs:1777-1778`). The peer's
fetch token is never sent on the wire, so the recorded "fetch" hash is wrong;
the field is never verified anywhere, so nothing fails today — it is stale,
misleading data in the pin store.

`cinder/src/core.rs:1771-1782`. Severity: low.

### 11. CONFIRMED — closed sessions linger in `sessions`/`sid_index` until replaced or link-down

`session()` returns `None` for closed sessions but does not remove the entry
(`core.rs:1111-1117`); the closed `PeerSession` and its `remote_sid` stay in
`sid_index` until a new session replaces it (:1076-1083) or `on_link_down`
clears everything (:898-900). Bounded by contact count — minor resource note,
not exploitable beyond memory pinning.

`cinder/src/core.rs:1111-1117`. Severity: trivial.

### 12. CONFIRMED — panic-propagation paths in `sblk`/`cblk` and shared-state `unwrap()`s

Every store op runs under `spawn_blocking(...).await.expect("store task
panicked")` (`core.rs:656-658`); `cblk` adds `contacts.lock().unwrap()` inside
the blocking task (:668) with another `.expect` (:672). `shared.lock().unwrap()`
is pervasive (e.g. :641, :677, :887, :1599). A panic inside any store/contacts
closure (or mutex poisoning after the first panic) converts into a panic of
the calling async task — pump or RPC handler — killing the core's driver task
without a structured error. Attacker reachability requires inducing the first
panic (no such path was confirmed), so severity is availability-only and
dependent — listed CONFIRMED as a code defect, trigger SUSPECTED.

`cinder/src/core.rs:650-672`. Severity: low-medium.

---

## Confirmed bugs — one line each

1. `mailbox_drain` re-locks `op_lock` via `request()` → total request-op deadlock whenever offline mail exists — `cinder/src/core.rs:998` + `:1033` + `:919`.
2. Failed add wedges contact `pending_out/"connecting"`; `contact.add` then always `Conflict` — `cinder/src/core.rs:2022-2033` + `:1519-1544`.
3. Peer `Edit`/`Delete` rewrite/tombstone our outbound messages — no dir/chat scoping — `cinder/src/core.rs:1784-1827` + `cinder/src/store.rs:895-918`.
4. Offline-queued sends never flushed on reconnect — `cinder/src/core.rs:1399-1424` + `:1495-1496` + `:875-880`.
5. One relay `Error` frame aborts all pending handshakes and steals all request slots — `cinder/src/core.rs:842-869`.
6. Unbounded inbound responder tasks (Msg1 flood) — `cinder/src/core.rs:1548-1555`.
7. `early` frame map unbounded across fake peers — `cinder/src/core.rs:1624-1633`.
8. `unread`/`read_upto` never reset — `mark_read_upto` has no production caller — `cinder/src/store.rs:790-797`, `:1072-1082`, `cinder/src/tui/app.rs:245`.
9. `KeyChanged` trust/send-block machinery unreachable — `cinder-core/src/session.rs:362`, `contacts.rs:186-221`, `cinder/src/core.rs:1914`.
10. `PendingContact` store dead — `cinder-core/src/contacts.rs:251-279`.
11. `MailboxGrant.fetch_token_hash` holds the deposit hash — `cinder/src/core.rs:1777-1778`.
12. Closed sessions linger in `sessions`/`sid_index` — `cinder/src/core.rs:1111-1117`.
13. `.expect`/`.unwrap` panic propagation in `sblk`/`cblk`/shared locks — `cinder/src/core.rs:650-672`.

---

## Top 5 suspected bugs

1. **SUSPECTED — simultaneous-handshake generation churn.** Two drivers
   (inbound `respond` + outbound `ensure_handshake`) can both complete and
   both call `on_session_ready`; `install_session` last-writer-wins
   (`core.rs:1076`). Each side may keep a different generation → cross-gen
   frames fail decrypt → fatal-close + auto-re-handshake (:1652-1668) →
   possible oscillation before convergence. Self-heals probabilistically;
   no pinning on initiator preference. `core.rs:1068-1086`, `:1560-1586`.
2. **SUSPECTED — `AppMsg::Ack` id-list amplification.** Each attacker-chosen
   `msg_id` costs a `spawn_blocking` store lookup (`core.rs:1746-1759`);
   ~1k ids/frame × contact → blocking-pool saturation slowing all store ops.
3. **SUSPECTED — `dead_sids` exhaustion by a contact.** Every bound-mismatch
   frame tombstones a sid (`core.rs:1640`); 8192 fresh tombstones (:1091-1098)
   fill the map → inserts skipped → stale-stream protection silently off
   until entries age past 600 s.
4. **SUSPECTED — parked frames with foreign sids poison the next session.**
   Frames tombstoned pre-install (`handle_transport` :1636-1642) stay dead
   after `install_session`, so a legit peer whose frames raced install has its
   new sid permanently tombstoned → extra handshake round-trip churn.
   `core.rs:1600-1615`, `:1150-1162`.
5. **SUSPECTED — attacker-controlled strings reach event JSON unescaped.**
   `msg_id`/`emoji` from `AppMsg` flow into `message_json`/`emit`
   (`core.rs:1725-1733`, `:1845-1856`) and into the reactions table
   (`store.rs:926-930`) without length/format checks; TUI sanitizes at render
   (`tui/sanitize.rs`), but RPC consumers receive raw peer text fields —
   verify every render/consumer path treats them as hostile.

---

## Areas cleared cleanly

- **Frame codec (`cinder-core/src/frame.rs:83-111`):** length checked before
  buffer growth, zero-length rejected, `max_frame` enforced pre/post-Hello,
  `wire_deserialize` rejects trailing bytes (fixed-int, LE config).
- **Padding (`padding.rs:13-40`):** exact padded-length check, zero-padding
  verification, 8192 plaintext cap, block size `max(256, next_pow2(len))`.
- **Error-code table:** `Reserved = 0` keeps enum numbering stable; fatal set
  matches PROTOCOL §10.
- **Handshake flow (`handshake_flow.rs:14-69`):** initiator verifies expected
  `PeerId`; responder TOFU-pins via `contacts.get`; `PeerIdMismatch` surfaces
  loudly as `security.identity_changed` with no silent rebind.
- **`normalize_handshake` (`core.rs:411-418`):** single-layer unwrap only when
  the inner payload parses as `Msg1`; raw-noise payloads can't parse → both
  relay wrapping layouts interop; no panic on attacker bytes.
- **Contacts persistence (`contacts.rs:110-123`):** HMAC verified before
  deserialize; `record_key_change` uses `ct_eq` for the same-key no-op.
- **Keystore (`keystore.rs:143-160`):** temp-file + fsync + 0600 + atomic
  persist + directory sync.
- **Relay server (`server.rs`):** per-IP/per-conn limits, pending-handshake
  caps, bounded outbound queues with batch-shed, token-strike closure, idle
  timeout, cleanup removes handshakes/registrations/subs/queues (:624-660);
  pre-hello frame cap; `Hello`-post-hello → `ProtocolViolation` close.
- **Relay mailbox (`mailbox.rs`):** SHA-256 + `ct_eq` token checks (:91-95),
  whole-session eviction on expiry (:44-59), 500-msg/4 MiB caps, envelope
  body cap, invite slots TTL + per-conn/IP bounds, atomic single-use invite
  consume (:343-407); `depo` `from` stamped by relay, not client.
- **RPC protocol/server:** 4 MiB line cap with drain-before-error; named
  params only; scope checks; `msg_send` body/`client_msg_id` validation
  (`methods.rs:209-249`); dedup with payload fingerprint + bounded table
  (`dispatch.rs:21,89-131`); replay seq monotonic via AUTOINCREMENT,
  `event.gap` marker when `since < oldest_seq-1` (`events.rs:185-235`,
  `store.rs:955-`); bounded per-client queues, saturated clients closed.
- **TUI:** sanitizer strips CSI/OSC/DCS, C0/C1, bidi overrides, zero-width
  chars (`tui/sanitize.rs:73-107`); peer text never rendered raw (ui tests
  assert no ESC/bidi); `read_marker_line`/scroll math uses saturating ops;
  `fuzzy_lines` uses `titles.get(idx)` — no panic indexing.
- **Transport/crypto:** `dead_sids` + lazy `remote_sid` bind +
  bound-mismatch tombstone *before* decrypt (`core.rs:1600-1648`); decrypt
  failure fatal-closes session and tombstones (:1652-1668); relay `from` is
  treated as unauthenticated routing data only; rekey ordering correct
  (data `ct` before `ctrl`, both over the same FIFO `cmd_tx`; a lost `ctrl`
  implies link death, which already kills sessions per §11 — not a bug).
- **Dedup/idempotency:** `client_msg_id`/`client_op_id` conflicts detected,
  replays flagged `deduped`; store-level `INSERT OR IGNORE` + dedup table
  (`store.rs:708-800`) consistent.
- **State machine (`store.rs:805-844`):** `update_state` enforces
  `can_reach`, `Read` rejected at the API, audit log appended atomically.
- **Wire:** transport `body` >8210 reaches recipients only as undecryptable
  garbage → same fatal-close as any junk frame; not a distinct attack.

---

## Notes on fix direction (non-binding, no source changes made)

- `mailbox_drain`: drop `_g` before calling `request()`, or send `MailboxAck`
  directly on `cmd_tx` without a waiter slot.
- Add a meta transition on handshake failure (`substate="failed:<reason>"`,
  allow re-add from `PendingOut`) and an `on_link_up` sweep re-arming
  `hs_inflight`/`pending_out`.
- Scope `Edit`/`Delete`/`React`/`Ack` to `chat_str(peer)` + `dir='in'` targets
  (Edits/Deletes) at the store layer, not just the handler.
- Attribute `ClientEvent::Error` to the triggering op (require matching
  slot/`hs_id`), or drop `hs_waiters` fan-out for non-fatal codes.
- Bound inbound responder concurrency (semaphore) and cap `early` globally,
  not per-peer.
- Wire `mark_read_upto` to the TUI/RPC read path, or delete the dead
  `read_upto`/unread machinery; either implement `PendingContact` flows or
  remove them.
