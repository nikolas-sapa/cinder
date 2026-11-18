# Security Policy

## Reporting a vulnerability

Report vulnerabilities **privately** via a GitHub security advisory on this
repository:

**https://github.com/nikolas-sapa/cinder/security/advisories/new**

(Or: repository → Security tab → "Report a vulnerability".)

**Do not file public issues for undisclosed vulnerabilities.**

Useful contents for a report: affected version/commit, reproduction
steps or PoC, the security property you believe is violated, and which
adversary class in [docs/THREAT-MODEL.md](docs/THREAT-MODEL.md) it maps
to.

## Response expectations

cinder is a community project maintained in spare time. Honest
expectations, not an SLA:

- **Acknowledgment:** we aim for ~7 days.
- **Assessment & fix:** best effort, prioritized by severity. There is no
  committed timeline.
- **Disclosure:** coordinated disclosure preferred — we will agree on a
  public timeline with you once a fix or mitigation exists. Reporter
  credit in release notes if you want it.

If the advisory gets no response after ~14 days, a public issue asking
for a security contact (without vulnerability details) is acceptable.

## Supported versions

**Latest release only.** cinder is pre-1.0; only the newest tag receives
fixes. Upgrading is the mitigation — there are no backport branches.

## Scope

In scope:

- Our use of `snow` (the only Noise implementation): peer-id pinning,
  handshake flow, session lifecycle, rekey policy, decrypt-fatal
  discipline.
- Key storage (OS keyring, age-encrypted file fallback), contacts-store
  integrity (HMAC-SHA256 keyed by `HKDF(static_secret)`, fail-closed).
- Daemon authentication (`SO_PEERCRED`/`getpeereid`), RPC framing and
  limits, always-error behavior.
- **Any path where plaintext or key material reaches the relay** — relay
  blindness is a design invariant; a violation is a bug.

Out of scope — known, documented v1 limits (see
[docs/THREAT-MODEL.md](docs/THREAT-MODEL.md) §4–§6 before reporting):

- Metadata resistance: timing, message frequency, presence, contact
  graph, IP↔peer-id binding. Padding covers sizes, nothing else.
- Endpoint/root compromise: hardening reduces accidental leakage
  (core dumps, swap, argv/env exposure); root and same-uid adversaries
  are out of the effective threat model.
- Post-compromise security: rekey-lite is not a ratchet; a stolen live
  Noise state is healed only by a new handshake.
- Cleartext transit of mailbox capability tokens on the client↔relay
  link (one-mailbox availability loss, never plaintext).
- Unsolicited contact requests on bare static addresses.
- Deniability, group chats, multi-device — v1 non-goals.
- Vulnerabilities in upstream dependencies themselves (`snow`, `age`,
  `sqlcipher`, RustCrypto crates) — please report those to the upstream
  project.

## Cryptographic supply chain

All cryptography comes from vetted crates — `snow >= 0.9.6`, `age`,
`rusqlite` + bundled `sqlcipher`, `secrecy`/`zeroize`, RustCrypto
`sha2`/`hmac`/`hkdf`. `curve25519-dalek >= 4.1.3` is enforced to clear
RUSTSEC-2024-0344. **There is no hand-rolled cryptographic code** — no
custom AEAD, HKDF, or ratchet anywhere in the tree (a hard guardrail,
enforced in review and CI). `cargo audit` clean is part of the
definition of done.

Unsafe code is confined to `cinder/src/hardening.rs` (libc calls for
`RLIMIT_CORE`, `PR_SET_DUMPABLE` on Linux, `mlockall`); everything else
is `#![forbid(unsafe_code)]` / `#![deny(unsafe_code)]`.

## Audit status

**Not externally audited.** No formal security review has been
performed. Assurance today is the design documentation
([docs/THREAT-MODEL.md](docs/THREAT-MODEL.md),
[docs/PROTOCOL.md](docs/PROTOCOL.md)) and grep-able invariants enforced
in CI — see PROTOCOL.md §14.

## PGP / signatures

No PGP signing key is published at this time. Release artifacts (once
releases begin) are signed with `minisign`; the public key ships in the
repository.
