# cinder — Plan: voice & video calls (phased)

Status: scoped implementation spec. **Phase A is implementable from this
document alone**; Phases B and C are design sketches with explicit decision
points. Normative deltas relative to PROTOCOL.md, AGENT-API.md, and the
`AppMsg` layer in `cinder/src/core.rs`. Lifts the v1 non-goal "voice/video"
(PLAN §1, THREAT-MODEL §7) for Phase A only.

The short version: Phase A is a 1:1 Opus voice call where **signaling and
media both ride the existing Noise session through the relay**. The relay
stays blind; nothing about NAT traversal is needed because the dumb TCP hub
already forwards everything. The cost — TCP head-of-line latency — is the
price of zero new transport and zero new trust surface. UDP media (Phase B)
and video (Phase C) are spec'd as upgrades with their real trade-offs named.

---

## 1. Goals / non-goals

### Phase A goals (v0, implementable)

- 1:1 full-duplex voice call between two contacts over the existing
  `Noise_XX_25519_ChaChaPoly_BLAKE2s` session: signaling
  (`call.ring`/`accept`/`decline`/`hangup` + events) and Opus media frames
  as `AppMsg` variants.
- Bounded everywhere: ring timeouts, one active call per account, capped
  jitter/jitter-buffer and media queues, no unbounded state.
- Failure is loud: every call ends in a `call.state` event with a terminal
  reason. A dead session mid-call ends the call; there is no silent
  media-continue path.
- JSON-RPC parity: TUI and agents place/answer calls through the same
  `call.*` surface.

### Non-goals (Phase A)

- **No acoustic echo cancellation, noise suppression, or AGC.** v0 assumes
  headphones or speakerphone tolerance; these are DSP pipeline work, not
  wire work. Stated because "call quality" complaints will otherwise land
  as bugs against this spec.
- No video, no screen share, no group calls, no stereo, no DTMF, no call
  recording, no voicemail, no PSTN.
- No second simultaneous call (`CALL_MAX_ACTIVE = 1`) and no call waiting —
  `busy` is the answer.
- No offline calls: calls require a live session both ways. Ring does NOT
  deposit to mailboxes (a ringing phone in a queue is a bad UX and a worse
  security smell — see §8).
- No auto-redial, no call park/transfer, no statistics beyond coarse
  loss/rtt surfaced in events.
- No UDP, no NAT traversal, no jitter-correcting transport in Phase A (that
  is Phase B's entire reason to exist).
- No voice-based SAS verification — reuse `ident.verify` out of band. A
  SAS-reading-over-the-call ceremony is a real option, explicitly deferred.

---

## 2. Phase A transport decision — evaluated honestly

Two candidate media paths were on the table:

| | **(a) Media inside the Noise session over TCP through the relay** | **(b) UDP datagrams, ChaChaPoly keyed by HKDF(session handshake_hash)** |
|---|---|---|
| New wire surface | None — `Transport` frames exactly like chat | New socket per call, new datagram format, seq + anti-replay window |
| NAT traversal | Solved already — the relay forwards TCP | Real problem: needs STUN/ICE-lite, or a relay UDP forwarder the dumb hub doesn't have; symmetric NAT still fails without TURN |
| IP↔peer binding | Preserved: peer sees only relay | **Regresses**: direct UDP reveals each party's IP to the other — a metadata leak the relay-blind model exists to prevent |
| Latency | Mediocre: TCP head-of-line under loss stalls audio ~100–300 ms per drop; no reordering needed | Real-time correct: jitter buffer + late-drop, ~60–120 ms glass-to-glass |
| Relay load | 2×~13 KB/s padded per direction | Zero if direct; same as (a) if relay-forwarded |
| Code delta | `AppMsg` + call engine + cpal/opus deps | + datagram codec, anti-replay, key schedule, NAT story, jitter buffer, failover logic |
| Failure modes | Congestion → degraded but ordered, decrypt-fatal discipline unchanged | Loss/reorder normal — must NOT feed into the session's fatal-decrypt rule (separate key stream) |

**Recommendation: (a) for Phase A.**

Rationale, in order of weight:

1. **Privacy model is preserved for free.** cinder's differentiator is that
   the relay is blind and IP addresses are not shared with contacts. Direct
   UDP hands your IP to every contact you call — a strictly worse position
   than v1 messaging. Doing (b) *privately* needs relay UDP forwarding,
   which is a bigger relay change than (a) is a client change.
2. **It is implementable from existing machinery.** Session, pacing,
   mailbox semantics, bounded queues, decrypt-fatal discipline — all
   inherited. The whole new surface is ~6 `AppMsg` variants and a call
   engine. A spec that ships beats a spec that's right.
3. **TCP HoL is survivable at v0 quality bar.** Opus 20 ms @ 48 kbps ≈ 120 B
   packets, padded to the 256 B bucket → ~13.6 KB/s/direction ≈ 50 frames/s,
   inside the relay's 120 frames/s/conn cap (PROTOCOL §12) with headroom
   for chat + acks. On lossy WANs calls degrade audibly; v0's answer is a
   `quality` hint in `call.state` events, not heroics.
4. **(b) is not foreclosed** — the signaling layer is written so Phase B
   swaps the media path underneath without touching `call.*` (§9): the same
   `call_id` state machine, different frame carrier.

Explicit acceptance of the downside: on sustained packet loss a Phase A
call stutters where (b) would not. That is a known, documented limitation,
not a discovered one — `call.state{quality:"degraded"}` surfaces it.

---

## 3. Phase A wire deltas — `AppMsg` additions

Same layering rule as files: bincode inside Noise AEAD, no `ClientMsg`/
`ServerMsg` changes, peers that don't implement drop unknown variants
without decrypt failure.

```rust
enum AppMsg {
    // ... Text, Edit, Delete, React, Grant, Ack (+ file variants per
    //      docs/plans/files-images.md) unchanged ...

    /// Call offer. Sent over the live session only — NEVER deposited to a
    /// mailbox (§8). `cap` is a capability bitmask for forward-compat;
    /// bit0 = audio/opus48k, bit1 = video (unused in A), bits2-7 = 0.
    CallRing  { call_id: [u8; 16], media: u8, cap: u8,
                expires_unix: u64 },

    /// Callee → caller: "ringing" (delivered to a live client, not yet
    /// answered). Distinguishes no_answer from never-arrived.
    CallRinging { call_id: [u8; 16] },

    /// Callee → caller: answered. Both sides start media.
    CallAccept  { call_id: [u8; 16], cap: u8 },

    /// Either direction: polite reject or end.
    /// reason ∈ {hangup, decline, busy, no_answer, failed, timeout}
    CallEnd     { call_id: [u8; 16], reason: u8 },

    /// One 20 ms Opus packet. seq is per-direction u16 wraparound;
    /// ts_ms is sender call-relative ms for lip-sync-free ordering
    /// (informational — seq orders).
    CallFrame   { call_id: [u8; 16], seq: u16, ts_ms: u32,
                  payload: Vec<u8> },   // Opus packet, 20–400 B
}
```

Rules:

- `call_id` = CSPRNG 16 B (same class as `SessionId`), chosen by the caller,
  unique per call attempt; RPC id is `call:` + b32.
- `CallFrame` is the only variant exempt from reliable-delivery
  bookkeeping: it is never queued to `pending_out`, never deposited, never
  retried. If the session can't take it now, it's dropped (counted) — audio
  is isochronous; a late frame is worse than none.
- All other call variants are control traffic and DO ride the normal
  delivery path — but never the mailbox (§8: control goes live-or-fails).
- `CallFrame.payload` ≤ 400 B enforced at encode (`invalid` → drop locally,
  never on wire). Whole `AppMsg::CallFrame` ≤ ~450 B plaintext → 512 B pad
  bucket → ~530 B ciphertext @ 50 fps ≈ 26.5 KB/s/direction worst-case
  bound for pacing arithmetic; typical ≈ 13.6 KB/s at the 256 B bucket.
- Media ordering is per-direction `seq`; over TCP it can only arrive
  in-order — the seq exists so the receiver detects *drops* (relay
  whole-`session_id` batch drop, sender backpressure drop) and feeds the
  decoder's PLC. `seq` wraparound (u16) is handled by modular compare.

### Media format

- Opus, 48 kHz, mono, 20 ms frames, bitrate 48 kbps default (negotiated
  floor 32 kbps, ceiling 64 kbps via `cap` bitmask; both sides clamp to
  `min(local_max, remote_max)`). Complexity 5, VBR on, DTX **off** in v0
  (DTX + silence suppression makes drop-vs-silence ambiguous at the jitter
  buffer — revisit in Phase B).
- Encode via `audiopus` (opus-sys: needs `libopus` — flag: builds via
  `pkg-config` or bundled `cmake` feature; CI needs the dep noted).
- Capture/playback via `cpal` — real platform surface (ALSA/CoreAudio/
  WASAPI). Tests use a `NullDevice` + loopback `DuplexDevice` trait shim;
  `cpal` is a build-time dep of `cinder` only.

---

## 4. Phase A state machine

One call per account. `idle` is the only state that accepts new calls.

```
 CALLER                                     CALLEE
   call.ring accepted                         |
   state: outgoing                            |
   |--- CallRing{cap,expires} --------------->|  validate → call.incoming
   |                                          |  auto: CallRinging
   |<-- CallRinging --------------------------|  state: ringing
   |  state: ringing                          |
   |          [user/agent decides]            |
   |<-- CallAccept{cap} -----------------------|  state: active
   |  state: active                           |
   |=== CallFrame ↔ CallFrame (50/s each) ====|
   |                                          |
   |--- CallEnd{hangup} --- OR --- CallEnd -->|  state: ended
   |  state: ended(reason)                    |  call.state terminal
```

Terminal `ended` reasons: `hangup` (either side), `decline` (callee),
`busy` (callee in a call or `CALL_MAX_ACTIVE`), `no_answer` (ring TTL),
`failed` (session loss / transport error), `timeout` (ring-delivery
timeout), `key_changed` (contact's key changed mid-setup — call aborted
like sends are blocked).

**Timeouts & bounds (normative, injectable):**

| Name | Value | Meaning |
|---|---|---|
| `CALL_RING_TIMEOUT` | `60 s` | Ring TTL from `CallRing` sent → `no_answer` |
| `CALL_DELIVER_TIMEOUT` | `10 s` | No `CallRinging` → `failed` reason `timeout` (never arrived) |
| `CALL_MAX_ACTIVE` | `1` | Per account; inbound while active → auto `CallEnd{busy}` |
| `CALL_FRAME_QUEUE` | `64` | Outbound media channel cap; full → drop oldest, count `drops_out` |
| `CALL_JITTER_TARGET` | `120 ms` | Playout delay target = 6 frames |
| `CALL_JITTER_MAX` | `240 ms` | Frames older than 12-frame horizon dropped, count `drops_late` |
| `CALL_MEDIA_KEEPALIVE` | `1 s` | No-frames-observed grace before `quality:"degraded"` event |
| `CALL_MEDIA_DEAD` | `10 s` | No frames → `ended(failed)` |
| `CALL_MAX_DURATION` | `4 h` | Hard cap → `ended(timeout)`; prevents zombie calls eating a Noise session |
| `CALL_RING_MIN_GAP` | `5 s` | Per-peer re-ring throttle → `rate_limited` |

- Session death mid-call → `ended(failed)` within 2 s on the side that
  notices; peer sees `ended(failed)` when `CALL_MEDIA_DEAD` fires or the
  new handshake lands (a re-handshake does NOT resume the call — calls are
  pinned to the `session_id` they started on; a `CallFrame` under a new
  session is a different call epoch → dropped).
- On `security.identity_changed` for a contact in `ringing`/`active`: the
  call ends `failed` + the call surface blocks re-ring until `ident.verify`
  (same send-block rule as `msg.send`).

---

## 5. Phase A RPC deltas

New id prefix: `call:`. Scope: `send` for all call methods (`call.ring` is
a send-shaped action; `call.accept`/`hangup` control a send-capable
session). No new scope in v0 — flag: a `calls` scope is the obvious v2
token split if operators want to deny agents telephony.

### Methods

| Method | Params | Result | Notes |
|---|---|---|---|
| `call.ring` | `{to, media?:"audio", client_call_id?}` | `{call_id, chat_id, state:"outgoing"}` | `to` = contact/chat/peer; `client_call_id` dedups retries like `client_msg_id` — replay → same `call_id` `deduped:true` |
| `call.accept` | `{call_id}` | `{call_id, state:"active"}` | from `ringing` inbound only |
| `call.decline` | `{call_id, reason?}` | `{call_id, state:"ended", reason:"decline"}` | inbound `ringing` only |
| `call.hangup` | `{call_id}` | `{call_id, state:"ended", reason:"hangup"}` | any non-terminal call |
| `call.info` | `{call_id}` | `{call_id, peer_id, contact_id, dir, state, since_ts, media, quality?}` | `read` scope |

### Events (new `CORE_EVENT_KINDS`)

```
call.incoming  {call_id, contact_id, peer_id, chat_id, media, cap}
call.state     {call_id, state, reason, dir, quality?, drops_out?,
                drops_late?, rtt_ms?}
```

`call.state` fires on every transition (`outgoing→ringing→active→ended`)
plus throttled `quality` updates (`quality ∈ "good"|"degraded"`, ≤1/5 s).
`ended` is always emitted exactly once — agents can rely on terminal
resolution, same contract as `msg_state` (AGENT-API §5.2).

Errors: `call.ring` → `-32011`/`peer_offline` (no live session — calls
never mailbox), `-32012`/`contact_blocked`, `-32013`/`key_changed_blocked`,
`-32018`/`conflict` (`CALL_MAX_ACTIVE` busy — `hint:"a call is already active"`),
`-32015`/`rate_limited` (re-ring gap). `accept`/`decline`/`hangup`/`info` →
`-32017`/`not_found` (unknown/terminal call_id), `-32018`/`conflict`
(wrong-state).

`rpc.describe` gains the 5 methods + 2 events + `call:` prefix row +
`limits.call_ring_timeout_s`, `limits.call_max_active`.

---

## 6. Phase A media pipeline (bounded, concrete)

```
 mic → cpal capture (48k mono f32) → opus encode (20ms) ──► bounded
 chan CALL_FRAME_QUEUE=64 → AppMsg::CallFrame → Session::encrypt → relay
 path (live only)

 relay → Transport → Session::decrypt → CallFrame → jitter buffer
 (target 120ms, max 240ms, seq-ordered) → opus decode (PLC on gap) →
 cpal playback
```

- **Backpressure**: outbound channel full → drop oldest frame, increment
  `drops_out`. The encoder runs at a fixed 50 fps cadence on a timer — it
  cannot outrun the channel; a stalled session drains instead of backing
  up.
- **Jitter buffer**: sorted insert by `seq`; playout starts after
  `CALL_JITTER_TARGET` buffered; a gap at playout time feeds
  `opus_decode(NULL)` PLC; frames arriving past the 240 ms horizon →
  `drops_late`. Buffer holds ≤12 frames ≈ ≤5.4 KB — nothing unbounded.
- **Interleave with chat**: `CallFrame` and chat `AppMsg`s share the
  session's nonce stream (ordered — required). Chat `pending_out` takes
  priority over the media queue at the send chokepoint: text latency beats
  audio smoothness. Pacing math: 50 fps audio + rekey + chat + acks ≪ 120
  fps relay cap.
- **CPU**: encode+decode ≈ single-digit % of one core on any desktop; no
  SIMD tuning in v0.

---

## 7. TUI deltas

- `tb::Backend` gains `call_ring(chat_id)`, `call_accept(call_id)`,
  `call_decline`, `call_hangup`, `call_mute(muted)`; `Event` gains
  `CallIncoming{…}`, `CallState{…}`.
- `/call [chat]`, `/answer`, `/decline`, `/hangup`, `/mute` — `mute` is
  **local-only**: it gates mic capture and emits **no wire signal at all**.
  Nothing about mute is transmitted or verifiable; the peer sees silence,
  which is the truth. A `muted`-flag frame would leak state for zero
  security benefit — deliberately absent in v0.
- Status line shows `☎ active 04:32 · degraded` — same sanitize rules; call
  panel replaces input hint during `active`.

---

## 8. Phase A security notes

- **Signaling confidentiality** — `CallRing`/`Accept`/`End` are inside
  Noise AEAD; the relay sees call timing/shape only (a ~50 fps regular
  frame stream is unmistakable). **The relay learns THAT a call is
  happening between two peer-ids, its duration, and its volume** — same
  class as existing metadata residuals, stated not mitigated.
- **No mailbox ringing** — `CallRing` to an offline peer → `peer_offline`
  at `call.ring` time (client-side presence+session check first, else the
  send fails on `deliver_now`). Design reason, not accident: an offer
  delivered minutes later rings a phone whose context may be gone, and a
  queued call attempt is a metadata record in the mailbox. Calls are
  synchronous by nature.
- **Media inherits session crypto** — same AEAD, same decrypt-fatal rule,
  same rekey window (rekey continues mid-call; `CallFrame` rides the same
  ordering as the rekey control unit, §11 ordering preserved).
- **Call pinning to a session** — a call started under `session_id` S is
  bound to S; frames under any other session_id for the same `call_id`
  drop without decrypt (the existing dead-sid/bind machinery does this
  free).
- **Deniability/authorship** — call audio carries no signatures; peer-id
  pinning means both parties know exactly who they're talking to, same as
  messages (THREAT-MODEL §4 row: no formal deniability claimed).
- **Mute is local truth** — nothing verifies a peer's mute claim; v0 sends
  no mute wire signal at all (can't leak what isn't sent).
- **call_id is not secret** — it's a correlation id, not a capability;
  call control requires the session (capability IS the Noise channel).

**What is NOT protected (Phase A):** call existence, duration, and
approximate bitrate leak to the relay and to any passive observer of the
TCP flow; call audio confidentiality/integrity/authenticity are protected
to the same bar as messages. IP addresses stay hidden — that is the
decision (a) bought, and the single biggest privacy argument for it.

---

## 9. Phase B — UDP media upgrade (design, not for this impl)

Goal: media on UDP with Phase A's signaling layer unchanged. `call.*` RPC
and the call state machine carry over; the `CallFrame` carrier changes.

- **Key schedule** (SRTP-equivalent, no DTLS, no handshake on the media
  path):
  ```
  prk  = HKDF-SHA256-Extract(salt="cinder-call-v1",
                            ikm = session.handshake_hash()
                                  ‖ min(static_a, static_b)
                                  ‖ max(static_a, static_b))
  k_a→b = HKDF-Expand(prk, "media-a-to-b", 32)
  k_b→a = HKDF-Expand(prk, "media-b-to-a", 32)
  ```
  `a` = call initiator's static key (sorted ordering like SAS — both sides
  derive the same map). `Session::handshake_hash()` already exists
  (session.rs:501). The call dies with the session — a mid-call session
  death ends the call; a NEW handshake produces new media keys, so there
  is no resume ambiguity.
- **Datagram:** `[u8 ver=1 ‖ u8 flags ‖ u64 seq ‖ u32 ts ‖ u16 pt_len ‖
  opus ‖ 16B Poly1305 tag]`, ChaChaPoly-128 keyed per direction, nonce =
  `4B zero ‖ seq`. Anti-replay: sliding bitmap window over `seq`, width
  128, per direction, per call.
- **Transport options, honestly:**
  1. *Relay UDP forwarder* — relay gains a datagram socket and routes by a
     session-scoped cookie issued inside the E2E channel. Preserves
     IP-hiding + blindness; largest relay delta; the dumb hub grows a
     second socket type. Preferred for privacy.
  2. *Direct UDP + ICE-lite* — candidates exchanged inside the E2E
     `CallRing`/`CallAccept` (add `addrs: Vec<String>` field reserved in
     Phase A's `cap` bitmask — `cap` bit2 = "udp-capable"). Works on
     LANs/Tailscale/open NATs; fails most symmetric NATs without a STUN
     service cinder doesn't operate. **Leaks IPs to the callee** —
     opt-in per call (`call.ring{media:"audio_udp"}`), documented trade.
  3. *Fallback*: TCP-over-Noise, always available, Phase A code path kept.
- Path selection: prefer (1) when relay advertises UDP; else (2) when both
  opted in; else (3). Selection result visible in `call.info.media`.
- Exit criteria for B: p95 glass-to-glass < 200 ms on a 2%-loss link where
  TCP path shows > 400 ms; zero plaintext leakage (same needle test).

---

## 10. Phase C — video (exploratory only)

Marked **do not implement** in this plan. The decision that matters later:

| Option | What it buys | Cost |
|---|---|---|
| `webrtc` crate (full WebRTC stack) | ICE/DTLS/SRTP/jitter/NACK/FEC done; proven interop | Huge dep tree; DTLS handshake duplicates Noise; drags in SDP semantics we don't want; the crate's maturity has been uneven |
| `str0m` | RTP/ICE library sans SDP ceremony — closer to our model (signaling is already E2E) | Still need jitter/NACK/FEC plumbing; SRTP keying is its own — or reuse §9 HKDF export |
| Custom RTP-ish over Phase B datagram | Minimal dep | You reinvent jitter/NACK/FEC — the three things that make video work at all |

Working recommendation (to revisit): **str0m + §9 handshake-hash keying**
for the SRTP layer, codec **AV1** (`rav1e` encode is slow — realistically
decode-first via `dav1d`, or H.264 `openh264` with the Cisco-license flag
noted: BSD code, but patent royalties are a distribution question, not a
code question). Video needs the Phase B UDP path to be real first; on TCP
it is a slideshow. Estimated scope when picked up: ≥ 3× Phase A.

---

## 11. Files touched (Phase A map)

| File | Delta |
|---|---|
| `cinder/src/core.rs` | `AppMsg` call variants, call engine (state machine, timeouts, media pump threads, jitter buffer), `call.*` impls, `call.incoming`/`call.state` emit |
| `cinder/src/rpc/methods.rs` | `call.*` handlers, `call:` prefix, dedup for `client_call_id` |
| `cinder/src/rpc/core.rs` | `CinderCore` call methods, `CORE_EVENT_KINDS += call.incoming, call.state` |
| `cinder/src/rpc/describe.rs` | schemas, `call:` prefix, limits fields |
| `cinder/src/tui/{backend,app,ui,widgets}.rs` | backend trait + `/call /answer /hangup /mute` + status |
| `cinder/Cargo.toml` | `+cpal`, `+audiopus` (flag: libopus native dep) |
| `docs/AGENT-API.md` | §2.6 `call:` prefix, methods, §5 events, §8.4 limits |
| `docs/keybindings.md` | call commands |

PROTOCOL.md: **zero changes**. THREAT-MODEL: add call-metadata residual row
when Phase A ships.

---

## 12. Test plan (lands before impl)

| # | Test | Location | Pass criteria |
|---|---|---|---|
| CA1 | Ring→accept→hangup e2e | `cinder/tests/e2e_call.rs` | two `cinder rpc` procs + relay: `call.ring`→`call.incoming`→`call.accept`→`call.state:active` both sides <2 s; `call.hangup`→`ended(hangup)` exactly once each side |
| CA2 | Ring timeout | `cinder/tests/call.rs` | unanswered ring → `ended(no_answer)` at 60 s ±1 s (injected 50 ms clock: wall <1 s) |
| CA3 | Deliver timeout | `cinder/tests/call.rs` | callee never emits `CallRinging` → caller `ended(failed,timeout)` at 10 s |
| CA4 | Busy | `cinder/tests/call.rs` | inbound ring while `CALL_MAX_ACTIVE` → auto `CallEnd{busy}`; caller sees `ended(busy)`; local call unaffected |
| CA5 | Decline | `cinder/tests/call.rs` | `call.decline` → caller `ended(decline)`; no media frames flow (assert 0 CallFrame on wire) |
| CA6 | Media loopback | `cinder/tests/call.rs` | encode→jitter→decode 1 000 frames, 0 injected loss → output seq monotonic, PLC invocations == 0; inject 10% drops → PLC invoked exactly per-gap count, no panic |
| CA7 | Jitter bounds | `cinder/tests/call.rs` | burst deliver 12 frames at once → buffer ≤ 12; frames > 240 ms late → `drops_late` increments, 0 played |
| CA8 | Backpressure | `cinder/tests/call.rs` | fill `CALL_FRAME_QUEUE` → oldest dropped not newest, `drops_out` counts, channel never > 64 |
| CA9 | Session death mid-call | `cinder/tests/e2e_call.rs` | kill TCP during active → `ended(failed)` ≤ 2 s noticing side, ≤ `CALL_MEDIA_DEAD` peer; frames under new session dropped (dead-sid path), 0 decrypt-fatal on wrong-sid frames |
| CA10 | Call+chat interleave | `cinder/tests/e2e_call.rs` | during active call, `msg.send` → `message.received` < 2 s; call stays `active`; frame loss < 5% on healthy link |
| CA11 | Blindness | `cinder/tests/e2e_call.rs` | relay byte-capture: known Opus needle (silence-encoded signature frame) appears 0 times; all captured frames parse as `ClientMsg`/`ServerMsg` |
| CA12 | One-call invariant | `cinder/tests/call.rs` | second outbound `call.ring` while active → `-32018` `conflict`; proptest state machine: legal transitions only, `ended` terminal-absorbing 500 cases |
| CA13 | Key-changed mid-call | `cinder/tests/call.rs` | force pin mismatch on re-handshake → call `ended(failed)` + `security.identity_changed` ordering before any post-change frame |
| CA14 | Rate cap conformance | `cinder/tests/call.rs` | sustained call: transport frames/s to relay ≤ 120 measured over 60 s window; relay never emits `RateLimited` |
| CA15 | Event ordering | `cinder/tests/rpc_events.rs` | `call.incoming` < `call.state:ringing` < `active` < `ended` per call_id, seq monotonic; exactly one `ended` per call across 200 runs |
| CA16 | Mute locality | `cinder/tests/call.rs` | `call.mute(true)` produces 0 call-related `AppMsg` on the wire (assert pump sees none) — mute MUST NOT transmit |
| CA17 | `rpc.describe` | `cinder/tests/rpc_methods.rs` | ≥ 5 `call.*` methods + `call.incoming`,`call.state` events + `call:` prefix present |
| CA18 | Offline ring | `cinder/tests/call.rs` | `call.ring` to peer with mailbox grant but no live session → `-32011` `peer_offline`, 0 `CallRing` deposits anywhere (mailbox byte-capture assert) |

---

## 13. Open flags

1. **`audiopus` vs `opus` crate** — `audiopus` is the maintained fork;
   `opus-sys` needs `libopus` present (pkg-config/cmake). If hermetic CI
   matters, gate media behind `--features calls` and mock `AudioDuplex` for
   tests; CA6–CA8 run without hardware regardless.
2. **DTX off in v0** — silence costs ~13 KB/s instead of ~0. Simpler
   (gap==loss unambiguous); revisit if metered links complain.
3. **`call.ring` scope** — `send` chosen so agents can dial; if deployments
   want agents-that-can't-call, the `calls` scope lands with v2 tokens —
   the scope string is reserved here deliberately.
4. **Phase B `cap` bit2 reservation** — `udp-capable` bit reserved now so a
   Phase A client answers a Phase B offer with a defined fallback instead
   of unknown-capability silence. Cheap forward-compat, costs one bit.
