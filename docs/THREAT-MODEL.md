# cinder — Threat Model

Status: pre-1.0, **not externally audited**. This document describes the design as
specified in `BRIEF.md` and implemented across `cinder-core`, `cinder-relay`, and
the `cinder` binary. Every mitigation named here should be verifiable in code
(function and protocol-field names are cited deliberately — grep them). If code
and this document disagree, the document is wrong; file an issue.

The short version for skimmers: cinder gives you **confidential and
authenticated messages against everyone except your peer and anyone who
compromises your device or theirs**. It gives you **weak metadata protection**
— padding hides exact sizes, nothing hides timing, and the relay learns your
contact graph. It gives you **no forward secrecy once a live session's full
Noise state is stolen** and no post-compromise recovery yet. Those are the
real limits. Details below.

---

## 1. System summary

cinder is an E2E-encrypted terminal messenger for humans and AI agents, built
as three crates: `cinder-core` (wire protocol, Noise session wrapper,
identity/peer-id, contacts store), `cinder-relay` (a dumb TCP hub that routes
opaque ciphertext by peer-id plus offline mailbox queues), and `cinder` (the
client: TUI, `cinder rpc` for stdio JSON-RPC, `cinder daemon` for a
multi-client unix socket). Transport is length-prefixed bincode frames
(`[u32 BE len ‖ frame]`) over TCP. The relay sees only peer-ids, frame sizes,
and timing — it holds no keys, terminates no crypto, and is designed to be
blind; an integration test asserts this.

Identity is accountless: your identity is a static X25519 keypair, and your
peer-id is `SHA-256(static_pubkey)`. Peers meet out-of-band via one-time
`cinder://<base32-pubkey>@<relay-host>?rendezvous=<token>` links (SimpleX
model), then run `Noise_XX_25519_ChaChaPoly_BLAKE2s` via the `snow` crate.
After every handshake, `get_remote_static() → SHA-256 → constant-time
compare` against the pinned peer-id; a mismatch closes the session loudly.
Optional SAS (short authentication string) verification upgrades a contact
from `unverified` to `verified`; a key change produces a `key-changed` state
that blocks sending. All inbound messages are marked `trusted:false` in the
agent-facing schema because agent clients are a first-class prompt-injection
surface.

---

## 2. Assets

| Asset | Where it lives | Primary protection |
|---|---|---|
| Static identity private key | OS keyring (`keyring` crate); fallback: age-encrypted file, `0600`/`0700`, atomic write, passphrase via TTY only (never argv/env) | OS credential store or age passphrase; `secrecy`/`zeroize` in memory; `mlockall` + `RLIMIT_CORE=0` best-effort |
| Noise session keys (cipher states, chaining key, stored ephemerals) | Process memory only; `TransportState` is never serialized | In-memory only + `zeroize`; session death → resend over a *new* handshake |
| Message plaintext | 8KiB cap; padded to next power-of-2 ≥256B before encrypt; at rest in SQLite via `rusqlite` + bundled `sqlcipher` | ChaChaPoly AEAD in transit; sqlcipher at rest |
| Contact graph | Client-side `contacts.json` + relay-observable mailbox pairings | HMAC-SHA256 (keyed by `HKDF(static_secret)`) on the file; **only partially hidden from the relay — see §4** |
| `contacts.json` integrity | Local disk | HMAC fail-closed: mismatch → refuse to load, never auto-repair; atomic writes |
| Event-log contents | Local store, bounded replay log backing `events.subscribe` | sqlcipher at rest; monotonic seq + `event.gap` marker (never silently dropped) |
| Relay availability | `cinder-relay` | Per-IP handshake rate limits, pending-handshake cap, 10s handshake timeout, bounded queues/mailboxes — plus honest note: *your* relay can always deny service to *you* |

---

## 3. Adversary classes

### 3.1 Passive network observer

**Capabilities.** Sees your TCP flow to the relay: connection timing, session
durations, byte counts per frame, total volume. Sees Noise handshake and
transport frames on the wire — all ciphertext. If you transmit an invite link
over a channel they can watch, they see the pubkey, relay host, and
rendezvous token inside it.

**Cannot.**

- Read or forge messages — all payload is inside Noise AEAD.
- Learn static public keys from the XX handshake itself — XX encrypts both
  parties' static keys (`s` transmissions happen under DH-derived keys), so
  handshake identity-hiding holds against pure eavesdropping. (Your peer-id
  is still derivable from an observed invite link.)
- Reuse a stolen rendezvous token — one-time invite slot; consumed on use.
- Determine exact message sizes — plaintext is padded to power-of-2 buckets
  ≥256B before encryption, so wire size reveals only the bucket.

**Residual risk.** Sees *that you use cinder*, which relay, when, and how
much. Can correlate two users' flows to the same relay and infer who talks to
whom with timing analysis — padding does not address this at all. Bucketed
sizes still leak approximate length class and full conversation shape.
**Learns mailbox capability tokens** when they observe
`MailboxDeposit/Fetch/Ack/Rotate/Close` frames on your client↔relay link —
tokens transit in cleartext (there is no client↔relay crypto layer; Noise is
peer-to-peer only, PROTOCOL §8). Consequence: exercise of that one mailbox's
capability — deposit-token spam, or fetch-token mail deletion/withholding.
Availability loss on one mailbox, never plaintext; PROTOCOL §8/§13 document
it as accepted v1 residual.

### 3.2 Malicious relay operator

**Capabilities.** Sees connecting IP ↔ peer-id bindings (network location of
every user), all routing labels, mailbox deposit/fetch activity, timing,
sizes, and connection liveness. Can drop, delay, reorder, or duplicate
ciphertexts; withhold mailbox contents; selectively rate-limit or
partition users; log everything forever.

**Cannot.**

- Read message plaintext or recover keys — it only ever touches opaque
  ciphertext. Enforced by architecture (no keys on the relay), asserted by
  the relay-blindness integration test.
- MITM sessions between pinned contacts — peer-id pinning
  (`get_remote_static() → SHA-256 → constant-time compare`) means a forged
  or relay-substituted key fails the post-handshake check; the Noise AEAD
  additionally fails on any substituted handshake message. There is no
  opportunistic mode and no TLS layer to strip — Noise *is* the only crypto.
- Reorder *within* a live session without detection — a reordered or dropped
  frame desyncs the Noise nonce, decryption fails, and decrypt failure is
  fatal (close + zeroize, never retry-skip). The relay-side rule of dropping
  whole-session batches atomically exists so this failure mode stays clean.
- Spam open mailboxes — mailbox deposit/fetch requires capability tokens
  issued inside the authenticated channel at contact-add; the relay stores
  only `H(token)`. Mailbox ids are random 256-bit (NOT the peer-id), so they
  cannot be enumerated.
- Authenticate to you as a user — the relay has no authentication channel
  into Noise sessions at all.

**Residual risk (real, not theoretical).**

- **Contact-graph inference**: even though mailbox ids are random, the relay
  observes that peer-id A deposits into mailbox M and peer-id B fetches from
  M. Over time this reveals who talks to whom. Not mitigated in v1.
- **Availability**: can withhold queued mail (integrity holds — you cannot
  receive forged mail — but you may receive nothing, late). Partially
  mitigated: `msg_state` stays `queued`, `session.backlog`/`event.gap`
  events expose gaps rather than hiding them. The silence itself is the
  only signal; there is no independent delivery proof.
- **Metadata beyond padding**: flow timing, message frequency, online
  presence, IP↔peer-id binding. Not mitigated.
- **Forced re-handshakes**: killing connections costs you CPU/rate-limit
  budget. Bounded by per-IP limits on the relay side, but a *malicious*
  relay controls its own limits — the bound only helps against third-party
  abuse of a well-behaved relay.

### 3.3 Active MITM on the network path

**Capabilities.** Can modify, inject, replay, drop, or delay any frame
between client and relay or between the two TCP hops. Controls DNS/routing.

**Cannot.**

- Impersonate a pinned peer — XX requires the static private key matching
  the pinned peer-id. The exact check: after the handshake completes and
  before any transport message is accepted or sent,
  `HandshakeState::get_remote_static()` → `SHA-256` → constant-time compare
  against the stored peer-id. Mismatch = close + loud error, never silent
  fallback. (This is precisely the check whose absence produced Bitchat's
  MITM bug.)
- Downgrade the handshake — exactly one pattern exists
  (`Noise_XX_25519_ChaChaPoly_BLAKE2s`); there is no algorithm or pattern
  negotiation to downgrade.
- Replay handshake messages — the Noise handshake hash commits to every
  prior message; a replayed or spliced message fails AEAD/hh verification.
- Tamper with transport frames — every frame is AEAD-authenticated; a
  modified frame is a decrypt failure, which is fatal (fail-closed: session
  closes, keys zeroized — the session dies rather than accept corrupt state).

**Residual risk.**

- Pure denial of service — an on-path attacker can always kill the TCP
  connection. cinder's answer is detection (fatal close, `failed` states)
  and resend over a *new* handshake, not prevention.
- Same mailbox-token exposure as §3.1 — an on-path observer learns presented
  capability tokens and can race-use them (deposit spam / mail deletion).
- **First-contact gap**: for an `unverified` contact, the MITM protection
  is only as strong as the invite link's authenticity — if the *link itself*
  was substituted in transit, the pinned peer-id pins the attacker's key.
  This is the standard trust-on-first-use window, and SAS verification
  (`ident.verify`) is the designated mitigation: H(`handshake_hash ‖
  min(pk_a,pk_b) ‖ max(pk_a,pk_b)`) rendered as 7 emoji (+ digit fallback),
  compared out-of-band. A MITM'd link produces a SAS mismatch detectable on
  any out-of-band channel.

### 3.4 Malicious peer / contact

**Capabilities.** Sends arbitrary messages, spam, malformed frames, giant
(up-to-cap) payloads, rapid-fire handshakes and messages. Learns your
peer-id, your relay, your online timing, and padded message sizes. Controls
their own client entirely — all protocol-level honesty assumptions are void
at the peer boundary.

**Cannot.**

- Corrupt other sessions or your identity — per-session Noise state, no
  shared keys across contacts.
- Crash via frame-length abuse — `len` is validated *before* allocation
  (handshake ≤4KiB, transport ≤16KiB, plaintext cap 8KiB); over-cap frames
  are rejected pre-alloc.
- Silently downgrade your trust state — contact key changes surface as
  `key-changed` (loud, blocks sending until dismissed) and the
  `security.identity_changed` event. Never silent rebind.
- Execute anything in the TUI — ratatui renders text, not markup. Required
  property for review: message bodies must have C0/C1 control characters
  and ANSI escapes stripped on the render path (a raw escape sequence from
  a peer is otherwise a terminal-injection vector — this is a *requirement*,
  not a claim to be taken on faith).
- Inject into well-behaved agents *undetected* — inbound bodies are marked
  `trusted:false` in the `rpc.describe` schema, and `contact.request` /
  `contact.accept` are explicit user-gated steps. This is schema-level
  prompt-injection hygiene, not a sandbox: a hostile message can still
  *convince* a naive agent; `trusted:false` is a label, not a wall.

**Residual risk.**

- **Spam/flood** — contact-level spam is only bounded by declining the
  contact record (repeat requests from a declined key auto-decline,
  AGENT-API §4.12); there is no spam scoring in v1. Pre-contact spam via
  invite links is bounded by rendezvous tokens being one-time; the bare
  **static-address path** (no token, PROTOCOL §7) is bounded only by relay
  rate limits and the accept/decline gate — publishing a static address
  means accepting unsolicited contact requests.
- **Session-kill DoS** — a peer can send one bad ciphertext and force a
  fatal close (by design); recovery is a new handshake + resend of pending
  messages. Integrity-first choice, accepted liveness cost.
- **Social engineering of the human** — the strongest attack against a
  `verified` contact is compromising the peer or their device. cinder
  cannot help with that beyond making the event visible.

### 3.5 Device compromise / co-resident malware

**Capabilities considered:** a process running as your uid, or root, on the
machine running `cinder`/`cinder daemon`.

**What limits damage.**

- Static key at rest: OS keyring primary; age-encrypted file fallback
  (dir `0700`, file `0600`, atomic write). Passphrase entry is TTY-only — never
  argv, never env (both are scrapable via `/proc` and `ps`).
- In memory: `secrecy`/`zeroize` on key material; `TransportState` never
  serialized to disk; `RLIMIT_CORE=0` (no core dumps), `PR_SET_DUMPABLE`
  on Linux (blocks ptrace from non-parent processes), best-effort
  `mlockall` (limits key pages reaching swap). All in the single
  `#[allow(unsafe_code)]` island `cinder/src/hardening.rs`; everything else
  is `#![forbid(unsafe_code)]` / `#![deny(unsafe_code)]`.
- On-path tampering of `contacts.json`: HMAC-SHA256 keyed by
  `HKDF(static_secret)`, fail-closed on mismatch — malware with file
  write access but no keyring access cannot forge contacts.
- Message history at rest: sqlcipher — a stolen laptop with locked keyring
  does not yield history.

**Honest limit.** A same-uid process can connect to the daemon socket
(`SO_PEERCRED`/`getpeereid` only verifies same-uid, not *which* process),
can read your TTY on most platforms, and on Linux without yama-scope
protection can ptrace. **Root wins outright** — it can read process memory,
keylog, and bypass every mitigation above. This is the universal endpoint
story; cinder reduces the blast radius of *accidental* leakage (dumps,
swap, logs, argv) far better than it resists an active same-uid adversary.
Secrets are never emitted in logs, error payloads, or events.

### 3.6 Malicious agent client on the RPC socket

**Capabilities.** Full JSON-RPC surface: `msg.send` as you, `contact.add` /
`contact.accept`, `chat.history` (full readable history),
`events.subscribe` (live interception), `ident.verify` (can mark a SAS
verified — a write op with real security weight).

**What limits it.**

- `cinder rpc` (stdio): whoever spawns/attaches to the process *is* the
  trust boundary — same trust model as MCP stdio servers.
- `cinder daemon` (unix socket): socket dir `0700` + socket file `0600`,
  peer-cred auth via
  `SO_PEERCRED`/`getpeereid` — only same-uid processes connect. No TCP
  listener exists; the surface is local-only by construction.
- Protocol hygiene: frame cap 4MiB/line, flush per frame, per-client
  bounded send queues so a stuck client can't head-of-line-block others,
  an error frame is always emitted for line-parseable input (a parse-
  silent implementation would hang agents).
- `ident.verify` is namespaced and explicit; an agent marking a bogus key
  verified is visible afterward in the contact's trust state.

**Honest limit.** A malicious same-uid agent can do *anything you can do* —
send messages as you, read all history, accept contact requests, verify a
wrong SAS to silence future `security.identity_changed` alarms. That is the
design (agents are first-class principals, not sandboxed subordinates), and
it is also the most dangerous surface in the system: **an agent prompt-
injected via inbound message text inherits all of this**, which is why
`trusted:false` exists. Capability-scoped tokens and require-ack event
modes are designed for v2, not v1. Treat every connected agent as a second
you.

---

## 4. Protected / NOT protected

| Property | Status | Mechanism / honest reason |
|---|---|---|
| Message confidentiality in transit | ✅ | `Noise_XX_25519_ChaChaPoly_BLAKE2s`, peer-id pin |
| Message integrity & peer authentication | ✅ | AEAD + `get_remote_static→SHA-256→const-time compare`, `key-changed` state, `security.identity_changed` |
| Identity of peers (to each other) | ✅ | Static key + optional SAS |
| Message confidentiality on the relay | ✅ | Relay is blind by construction; tested |
| `contacts.json` tamper-resistance | ✅ | HMAC-SHA256 / `HKDF(static_secret)`, fail-closed |
| Store/history at rest (offline theft) | ✅ (conditional) | sqlcipher + keyring; falls with the keyring |
| Exact message size | ✅ | pow2 buckets ≥256B |
| Forward secrecy vs. *static key* theft (recorded sessions) | ✅ | XX ephemerals; see §5 for the within-session limit |
| Static-key identity hiding from passive handshake observers | ✅ | XX encrypts statics |
| **Exact message timing, frequency, presence** | ❌ | No cover traffic; padding is size-only |
| **Contact graph vs. relay operator** | ❌ | Mailbox deposit/fetch pairing is observable; random mailbox ids only stop enumeration |
| **IP ↔ peer-id binding vs. relay** | ❌ | Inherent to a direct TCP hub; no Tor/mix layer |
| **Global passive adversary flow correlation** | ❌ | Buckets + no constant-rate shaping; stated plainly, not mitigated |
| **Endpoint / root compromise** | ❌ | Keyring+age+hardening reduce leakage; root wins |
| **Post-compromise security (new keys heal a stolen session)** | ❌ | rekey-lite is not a ratchet — §5 |
| **Per-message forward secrecy within a key window** | ❌ | One cipher key covers the whole rekey window |
| **Formal deniability** | ❌ | No signatures (repudiation is plausible), but we make no deniability *claim* — peer-id pinning means both parties know exactly who they talked to |
| **Multi-device / key continuity** | ❌ | Single-device identity; key change = `key-changed`, not rotation |
| **Relay availability guarantees** | ❌ | Your relay can starve you; gaps are surfaced, not prevented |
| **Unverified-contact MITM** | ⚠️ partial | Pin works only if the link was authentic; SAS closes it |
| **Malicious-agent actions** | ⚠️ partial | Same-uid bound + schema hygiene only; full user capability by design |

---

## 5. Cryptographic guarantees & limits

**What Noise XX gives (and we use):**

- **Mutual authentication** — both parties prove static-key possession;
  enforced meaningfully only because the peer-id pin check exists.
- **Forward secrecy against static-key compromise** — recorded sessions
  stay sealed if a static key is stolen later (session keys derive from the
  ephemeral DHs, not from the static key alone).
- **Identity hiding vs. passive observers** — XX transmits both static
  keys encrypted; an eavesdropper sees the handshake but not the pubkeys.
  (Caveat: peer-ids are routing labels on the relay, and invite links
  contain pubkeys — hidden from the wire, not from the system.)
- **Zero negotiation** — one pattern, one suite. No downgrade surface.

**What rekey-lite buys — and does not:**

- `rekey_manually` every ~1024 messages or 10 minutes (normative policy:
  PROTOCOL §11 — transport-level only, performs no handshake and
  re-verifies nothing) bounds a transport cipher key's exposure: stealing
  one key decrypts only its window
  (≤~1024 msgs / ≤10 min), not the whole session.
- It is **not** a Double Ratchet: no per-message key deletion chain, no
  skipped-key recovery (unnecessary — TCP is ordered and decrypt failure
  is fatal anyway), and critically **no post-compromise security** — an
  attacker who steals the *full Noise state* (chaining key + stored
  ephemerals) derives all future rekeys. The only real healing path is a
  fresh handshake, which is exactly what session death triggers. Chosen
  because TCP's ordering removes the hard problems a ratchet solves;
  `vodozemac` is a documented future direction, not v1.
- **Decrypt-fatal discipline**: any AEAD failure = close session, zeroize,
  resend pending over a new handshake. Never retry, never skip nonces.
  Trades liveness for the guarantee that no session ever runs on corrupt
  or attacker-influenced state.
- **contacts HMAC**: `HMAC-SHA256(HKDF(static_secret), contacts.json)`,
  mismatch → refuse to load (fail-closed). No auto-repair, no "accept and
  warn" path.
- **No TLS anywhere**: the Noise session is the only security layer. Fewer
  layers, fewer places to misconfigure — and no PKI to trust or strip.

**Primitives:** everything above comes from `snow` + `age` + `sqlcipher` +
RustCrypto `sha2`/`hmac`/`hkdf` — **zero hand-rolled crypto** (guardrail,
enforced in review). `curve25519-dalek >= 4.1.3` pins past
RUSTSEC-2024-0344 (the dalek timing-signal advisory).

---

## 6. Known weaknesses accepted for v1

1. **Rendezvous links leak the pubkey at accept-time.** The
   `cinder://<pubkey>@<relay>?rendezvous=<token>` URI travels over whatever
   channel carries it; a network observer of that channel learns the
   pubkey→relay binding. Accepted: the token is one-time (no replay
   impersonation), and the alternative (opaque addresses) pushes complexity
   into v1 that the SimpleX-style slot already handles. A stolen link lets
   a thief consume your invite slot (invite DoS) and arrive as an
   *unverified stranger* contact — visible, deniable at the UI level,
   caught at SAS.
2. **SAS verification is optional.** An `unverified` contact is fully
   usable; pinning protects continuity but not first-contact authenticity
   if the link was tampered. Standard Signal/WhatsApp UX trade; mitigated
   by `security.verification_needed` nudges and `key-changed` being
   unmissable. Accepted because mandatory verification measurably doesn't
   happen.
3. **The relay can withhold/delay mail.** Mailbox delivery is an
   availability property cinder cannot enforce; integrity is unaffected
   (nothing forged arrives). Accepted: a blind relay is worth more than a
   deliverability proof in v1; gap events at least make suppression
   visible.
4. **Metadata is minimal, not zero.** Padding is pow2 buckets, not
   constant-rate cover traffic; the relay sees pairing, timing, presence,
   and IP↔peer-id. Fixing this requires mix-net/cover-traffic
   infrastructure that is explicitly out of scope.
5. **Same-uid agents and processes are trusted completely.** The daemon's
   peer-cred check answers "are you me", not "are you trustworthy". v2
   scopes this with capability tokens; v1 relies on socket dir `0700` /
   socket file `0600` and
   user discretion.
6. **No post-compromise recovery.** Steal the live Noise state and rekey
   won't save past or future traffic of that session — only a new
   handshake heals. Accepted pending a real ratchet decision (vodozemac)
   in a later version.
7. **Terminal-escape sanitization is a required property, not a proven
   one.** The threat model *requires* control-char stripping on render;
   code review must confirm it — a peer-controlled escape sequence is a
   real injection vector in terminal UIs.
8. **Mailbox capability tokens transit in cleartext.** The client↔relay
   link is unauthenticated TCP (Noise is peer-to-peer only); a passive
   observer of token presentation gains that mailbox's capability —
   deposit spam, or fetch+ack deletion of queued mail. Availability
   consequence only, scoped to one mailbox, mitigated by least-privilege
   tokens and rotation (PROTOCOL §8). Proof-of-possession tokens or a
   Noise layer to the relay are v2 candidates.
9. **Unsolicited contact requests on static addresses.** The bare
   `cinder://<pubkey>@<relay>` path carries no rendezvous token; anyone
   who learns your pubkey+relay can force a `pending_in` request. Gated
   by accept/decline and relay rate limits, nothing more (PROTOCOL §7).

---

## 7. Non-goals (explicitly out of scope for v1)

- Group messaging (multi-device identity and group key agreement are
  prerequisites we don't have)
- Multi-device / linked devices / key rotation
- Mobile platforms and their lifecycle constraints
- Federation between relays or servers
- Voice/video calls
- Metadata-resistant transport (Tor, mixnets, constant-rate cover traffic)
- Spam/abuse scoring beyond rate limits and contact blocking

---

## 8. Audit status

- **Not externally audited.** No formal security review has been performed.
- All cryptography comes from vetted crates: `snow >= 0.9.6` (the only
  Noise implementation used), `age`, `rusqlite` + bundled `sqlcipher`,
  `secrecy`/`zeroize`, RustCrypto `sha2`/`hmac`/`hkdf`. There is **no
  hand-rolled cryptographic code** — no custom AEAD, HKDF, or ratchet
  anywhere in the tree; this is a hard guardrail.
- `curve25519-dalek >= 4.1.3` is pinned to clear RUSTSEC-2024-0344;
  `cargo audit` clean is part of the definition of done.
- Unsafe code is confined to `cinder/src/hardening.rs` (libc calls for
  `RLIMIT_CORE`, `PR_SET_DUMPABLE`, `mlockall`); all other code is
  `#![forbid(unsafe_code)]` or `#![deny(unsafe_code)]`.
- To report a vulnerability, see `SECURITY.md` in the repository root.
  Please do not file public issues for undisclosed vulnerabilities.

---

## Security review — 2026-11-18

Pre-implementation spec audit: every mitigation claimed here was traced to a
normative mechanism in PROTOCOL.md / AGENT-API.md. Changed:

- Added: mailbox capability-token cleartext transit as a residual risk under
  §3.1 (passive observer) and §3.3 (MITM), plus §6 item 8 — the claim
  "relay stores only H(token)" was accurate at rest but silent on transit.
- Fixed: §3.4 "bounded by you blocking the contact" — v1 has no
  `contact.block` method; softened to decline + auto-decline of repeat
  requests (AGENT-API §4.12).
- Fixed: §3.4 pre-contact spam claim — invite-link spam is token-bounded,
  but the bare static-address path (PROTOCOL §7) has no token; now stated
  with its actual bounds (rate limits + accept/decline). Added §6 item 9.
- Fixed: §5 rekey claim had no normative spec — PROTOCOL §11 now carries
  the normative `rekey_manually` policy; citation added here.
- Fixed: socket/file permission strings harmonized with AGENT-API §1.2
  (dir 0700, socket file 0600); also age-fallback perms line.
- Verified traceability: peer-id pinning → PROTOCOL §6; padding → §2;
  invite slots → §7; mailbox tokens/drop-atomicity → §8/§9; decrypt-fatal →
  §11; rate limits → §12; contacts HMAC → PROTOCOL §6 pinning-root +
  PLAN C1–C4; terminal sanitization → now a PLAN M4 deliverable;
  daemon auth → AGENT-API §1.2; `trusted:false` → §6; gap events → §5.8.
