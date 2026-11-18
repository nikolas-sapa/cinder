#!/usr/bin/env bash
# check-unsafe.sh — unsafe-code gate.
#
# Policy (BRIEF.md "UNSAFE EXCEPTION", PLAN.md section 4 gate 3):
#   * cinder-core + cinder-relay: crate root MUST declare #![forbid(unsafe_code)].
#   * cinder (bin):               crate root MUST declare #![deny(unsafe_code)].
#                                 deny (not forbid) so the ONE vetted unsafe
#                                 island can be allow-listed.
#   * `unsafe` tokens and #[allow(unsafe_code)] may live ONLY inside:
#         cinder/src/hardening.rs      libc hardening: RLIMIT_CORE / mlockall /
#                                      PR_SET_DUMPABLE
#         cinder/src/**/peercred.rs    SO_PEERCRED / getpeereid socket auth
#     At most ONE #[allow(unsafe_code)] per allow-listed file.
#
# Fails loudly on: missing lint attribute, `unsafe` token outside the
# allow-list, or an allow attribute in any other file.
# Exit 0 = compliant, exit 1 = violation (each printed to stderr).
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
err() { echo "check-unsafe: FAIL: $*" >&2; fail=1; }

# --- 1) crate-root lint attributes -------------------------------------------
check_root_attr() {
    crate="$1"; lint="$2"; root=""
    for f in "$crate/src/lib.rs" "$crate/src/main.rs"; do
        if [ -f "$f" ]; then root="$f"; break; fi
    done
    if [ -z "$root" ]; then
        err "$crate: no crate root (src/lib.rs or src/main.rs) found"
        return
    fi
    if ! grep -qE "#!\[${lint}\(unsafe_code\)\]" "$root"; then
        err "$root: missing #![${lint}(unsafe_code)] — see BRIEF.md UNSAFE EXCEPTION"
    fi
}
check_root_attr cinder-core  forbid
check_root_attr cinder-relay forbid
check_root_attr cinder       deny

# --- 2) `unsafe` tokens outside allow-listed files ----------------------------
# Word-boundary pattern avoids matching `unsafe_code` inside lint attributes.
# Comment-only lines are filtered to reduce noise; a bare `unsafe` in a string
# literal would still trip — that is intentional (fail loud, allow-list it).
unsafe_hits=$(grep -rnE '(^|[^[:alnum:]_])unsafe([^[:alnum:]_]|$)' \
                --include='*.rs' \
                cinder/src cinder-core/src cinder-relay/src 2>/dev/null \
              | grep -vE ':[0-9]+:[[:space:]]*(//|///|/\*|\*)' \
              || true)
if [ -n "$unsafe_hits" ]; then
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        case "$line" in
            cinder/src/hardening.rs:*|cinder/src/*peercred.rs:*) : ;;
            *) err "unsafe token outside allow-list: $line" ;;
        esac
    done <<EOF
$unsafe_hits
EOF
fi

# --- 3) #[allow(unsafe_code)] confined to the allow-list ----------------------
# Permitted forms:
#   a) inside cinder/src/hardening.rs or cinder/src/*peercred.rs (any position)
#   b) an outer #[allow(unsafe_code)] directly attached to the module
#      declaration `mod hardening;` / `(pub )?mod peercred;` — the only legal
#      way to permit unsafe inside a #![deny]-rooted crate.
allow_hits=$(grep -rnE 'allow\(unsafe_code\)' \
               --include='*.rs' \
               cinder/src cinder-core/src cinder-relay/src 2>/dev/null \
             || true)
if [ -n "$allow_hits" ]; then
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        f="${line%%:*}"; rest="${line#*:}"; ln="${rest%%:*}"
        case "$f" in
            cinder/src/hardening.rs|cinder/src/*peercred.rs) continue ;;
        esac
        # is the very next source line a hardening/peercred module decl?
        next=$(sed -n "$((ln + 1))p" "$f" 2>/dev/null | tr -d '[:space:]')
        case "$next" in
            modhardening\;*|modpeercred\;*|pubmodpeercred\;*) : ;;
            *) err "#[allow(unsafe_code)] outside allow-list: $line" ;;
        esac
    done <<EOF
$allow_hits
EOF
    # at most one allow attribute per file
    for f in $(printf '%s\n' "$allow_hits" | cut -d: -f1 | sort -u); do
        n=$(printf '%s\n' "$allow_hits" | grep -c "^${f}:" || true)
        if [ "$n" -gt 1 ]; then
            err "$f has $n #[allow(unsafe_code)] attributes — max 1 per file"
        fi
    done
else
    echo "check-unsafe: note: no #[allow(unsafe_code)] found yet —" \
         "hardening.rs/peercred.rs not landed"
fi

if [ "$fail" -ne 0 ]; then
    echo "check-unsafe: FAILED — see violations above" >&2
    exit 1
fi
echo "check-unsafe: OK — forbid on libs, unsafe confined to hardening.rs/peercred.rs"
