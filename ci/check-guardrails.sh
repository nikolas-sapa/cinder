#!/usr/bin/env bash
# check-guardrails.sh — crypto/dependency guardrail greps (PLAN.md section 4
# gate 4, BRIEF.md GUARDRAILS).
#
#   * TLS never appears: Noise IS the crypto, the relay needs no TLS layer.
#     No openssl / native-tls / rustls / tokio-tls in first-party sources or
#     the lockfile.
#   * libsignal (AGPL) and vodozemac (post-v1 design only) are banned deps.
#   * Post-handshake peer-id binding MUST exist: get_remote_static() must be
#     called in cinder-core session code (missing check = MITM bug).
#   * Key material MUST be wrapped: Zeroizing/Secret in cinder-core sources.
#
# Exit 0 = clean, exit 1 = violation (each printed to stderr).
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
err() { echo "check-guardrails: FAIL: $*" >&2; fail=1; }

# 1) no TLS stack in first-party sources
hits=$(grep -rniE 'openssl|native[-_]?tls|rustls|tokio[-_]?tls|(^|[^[:alnum:]_])tls([^[:alnum:]_]|$)' \
        --include='*.rs' cinder/src cinder-core/src cinder-relay/src 2>/dev/null \
       || true)
if [ -n "$hits" ]; then
    err "TLS reference in sources — Noise only, no TLS layer:
$hits"
fi

# 2) banned crates in the lockfile
hits=$(grep -nE 'name = "(libsignal[^"]*|vodozemac|openssl[^"]*|native-tls|tokio-native-tls|rustls[^"]*|tokio-rustls|ring)"' \
        Cargo.lock || true)
if [ -n "$hits" ]; then
    err "banned crate in Cargo.lock (deny.toml should have blocked this):
$hits"
fi

# 3) peer-id pinning: post-handshake remote-static check
if [ -f cinder-core/src/session.rs ]; then
    grep -q 'get_remote_static' cinder-core/src/session.rs \
        || err "cinder-core/src/session.rs: no get_remote_static() — peer-id MITM check missing"
else
    err "cinder-core/src/session.rs missing"
fi

# 4) key material wrapped in Zeroizing/Secret
grep -rqE 'Zeroizing|Secret' --include='*.rs' cinder-core/src \
    || err "no Zeroizing/Secret usage in cinder-core/src — key material not wrapped"

if [ "$fail" -ne 0 ]; then
    echo "check-guardrails: FAILED — see violations above" >&2
    exit 1
fi
echo "check-guardrails: OK"
