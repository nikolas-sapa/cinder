# cinder — Spec Security Review (pre-implementation)

**Date:** 2026-11-18 · **Scope:** `BRIEF.md` (authoritative), `PROTOCOL.md`,
`AGENT-API.md`, `THREAT-MODEL.md`, `PLAN.md`. Spec-only audit — no code was
reviewed or written. Edits were made in place; each doc carries a dated
"Security review" appendix listing its changes.

## Checklist verdicts

| # | Item | Verdict |
|---|------|---------|
| 1 | `remote_static` ↔ peer-id binding both directions | **PASS** — already airtight (PROTOCOL §6 names Msg2/initiator, Msg3/responder, mismatch → close + no Msg3). Hardened: added "identity is derived, never claimed" (claimed `from` never feeds trust) and "contacts store = pinning root" (HMAC/fail-closed/atomic now normative in §6 + invariant 13). |
| 2 | SAS transcript binding | **PASS → FIXED** — formula matches brief. Added persistence semantics (§6): SAS is per-session (it binds `handshake_hash`), `verified` binds to the pinned key pair and survives reconnects, `ident.verify` uses the most recent handshake's SAS. Without this, implementers could have required re-verification on every reconnect OR silently accepted a stale SAS. |
| 3 | No unauthenticated trust anchors | **PASS → FIXED** — `Hello.peer_id`, `StoredEnvelope.from`, forwarded `from` all flagged. Added explicit flags: `cinder://` link authenticity is channel-dependent (TOFU gap → attacker-key pin; only SAS closes it), offline-slot-owner Msg1 drop, and the tokenless static-address path's spam surface (PROTOCOL §7). |
| 4 | Decrypt-fatal discipline | **PASS → FIXED** — transport rule was specced; extended explicitly to mailbox-fetched envelopes (§9, §11) and clarified dead/unknown `session_id` frames drop without decrypt attempt. |
| 5 | Frame caps / len-before-alloc | **FIXED — two real bugs:** (a) padding layout couldn't encode pt_len 8191–8192 (`u16` prefix inside an 8192 bucket); prefix moved outside the bucket, max ciphertext 8208→**8210**; (b) `MailboxMessages` could not fit 500 × ~8.2 KiB envelopes in one ≤16 KiB frame → multi-frame response with `more` flag, whole-envelope packing, `more:false` terminator. |
| 6 | Nonce never from wire | **PASS** — §11 already normative; clarified `Ping.nonce` is an opaque echo token, not a Noise nonce. |
| 7 | Capability tokens | **PASS → FIXED + NEW RESIDUAL** — deposit/fetch separation, `H(token)` at rest, rotation/revocation/expiry consistent. Documented why raw tokens transit (relay is the verifier) AND a previously undocumented exposure: tokens transit **cleartext** on client↔relay links — a passive observer gains the capability (deposit spam / fetch+ack mail deletion). Logged as accepted v1 residual (PROTOCOL §8/§13, THREAT-MODEL §3.1/§3.3/§6.8). |
| 8 | Queue drop atomicity | **PASS** — whole-`session_id` atomic drops in §9 and §12 already consistent. |
| 9 | contacts.json HMAC | **FIXED** — was spec'd only in BRIEF/THREAT-MODEL/PLAN; now a normative requirement at the pinning root (PROTOCOL §6, invariant 13). |
| 10 | Secret hygiene in schemas | **PASS → FIXED** — no error/event schema carries secrets; added an explicit MUST (AGENT-API §7.2) covering error data/hint/events/logs; fingerprints/peer-ids/SAS declared public-by-design. |

## Cross-doc inconsistencies reconciled

- **Rate limits:** PROTOCOL §12 is now the single authoritative relay table;
  PLAN's defaults table references it (and AGENT-API §8.4 for RPC limits).
  Conflicts fixed: handshake rate 10/min→30/60s+burst10, pending cap
  256→8/IP+512 global, replay 10k→16,384, RPC send queue 256→1024,
  keepalive none→jittered Ping (120 s idle timeout made "none" untenable).
- **`read` state:** kept in enums per recommendation; spec'd reserved and
  NEVER emitted in v1 (AGENT-API §4.8/§5.2). PLAN already agreed.
- **`peer_offline`:** single normative behavior — accept+queue to mailbox
  when a live grant exists, else `-32011` retryable reject (AGENT-API §4.3,
  §7.2, `rpc.describe` example).
- **PLAN FLAGS:** items 1–3 marked RESOLVED (ratatui 0.30/crossterm 0.29 in
  lockfile, workspace compiles, snow locked 0.9.6); rust-toolchain note
  corrected (`channel = "stable"`); M4 stale version-bump removed.
- **invite TTL:** AGENT-API `ttl_sec` ≤30 d contradicted `INVITE_MAX_TTL` =
  72 h → clamped to 60–259200.
- **socket perms:** dir 0700 / socket 0600 harmonized across AGENT-API,
  THREAT-MODEL, PLAN.
- **`len == 0`:** was `ProtocolViolation` in §2 text but `OversizeFrame` in
  the §10 table → unified to `ProtocolViolation`.

## THREAT-MODEL traceability

Every mitigation claim now traces to a normative mechanism. Three gaps found
and closed: **rekey-lite** (claimed in §5, spec'd nowhere → PROTOCOL §11 +
invariant 14), **terminal-escape sanitization** (required property, no owner
→ PLAN M4 deliverable + test), **contacts HMAC** (client note only →
pinning-root normative). Two claims softened: "blocking the contact" (no
`contact.block` method exists → decline/auto-decline) and pre-contact spam
(static-address path has no token).

## New mechanisms spec'd this review

- Multi-frame `MailboxMessages` response (`more` flag) — wire-visible change.
- In-band delivery-ack contract (PROTOCOL §11) giving `delivered` a real
  semantic distinct from `sent`; `expired` added to `msg_state` reasons.
- Normative rekey policy: `rekey_manually` at ≥1024 msgs / ≥600 s.

## Escalation list (gaps the spec cannot close alone)

1. **Capability-token transit exposure** — inherent to cleartext client↔relay
   links. Documented as residual; a real fix (POP-bound tokens or Noise to
   relay) is v2 design work.
2. **`snow = "0.9"` req string** in `cinder-core/Cargo.toml` permits <0.9.6 on
   an *unlocked* resolve; lockfile pins 0.9.6. One-word tighten flagged for
   M1 (PLAN FLAGS 3) — not a spec file, not edited per audit scope.
3. **`delivered` requires app-layer interop** — the ack contract is normative
   but the app payload enum is deliberately client-internal; two non-cinder
   clients would need a published app-layer spec (acceptable for v1).
4. **Same-uid / static-address spam** — documented honestly; v2 items
   (capability tokens, PoP at `Hello`) remain the designed exits.
