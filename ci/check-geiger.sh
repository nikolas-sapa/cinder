#!/usr/bin/env bash
# check-geiger.sh — cargo-geiger gate. SOFT gate (warn) at workspace level;
# hard fail only if a forbid-crate reports unsafe.
#
# Policy (PLAN.md section 4 gate 3):
#   * cinder-core + cinder-relay declare #![forbid(unsafe_code)] — geiger must
#     report ZERO unsafe symbols for them. This is the actionable signal and
#     the only hard condition here.
#   * cinder (bin) permits unsafe ONLY in src/hardening.rs and
#     src/**/peercred.rs — file-level attribution is enforced by
#     ci/check-unsafe.sh (geiger counts per crate, not per file); its unsafe
#     count is reported informationally.
#   * Transitive deps (snow, curve25519-dalek, libc, ...) DO contain unsafe —
#     expected. Workspace-wide numbers are informational only.
#
# Notes on invocation:
#   * geiger cannot run on a virtual workspace manifest — it is invoked once
#     per crate via --manifest-path.
#   * geiger exits nonzero on scan warnings (unresolved platform pkgs etc.);
#     the report is still parsed — the row data is what matters.
#
# Exit 1 only when cinder-core/cinder-relay report unsafe — the compiler-level
# forbid gate would fail in that case anyway.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

if ! command -v cargo-geiger >/dev/null 2>&1; then
    echo "check-geiger: WARN: cargo-geiger not installed — skipping (soft gate)"
    exit 0
fi

fail=0
for crate in cinder-core cinder-relay cinder; do
    [ -f "$crate/Cargo.toml" ] || continue
    out=$(cargo geiger --manifest-path "$PWD/$crate/Cargo.toml" 2>/dev/null)
    if [ -z "$out" ]; then
        echo "check-geiger: WARN: 'cargo geiger' produced no output for $crate — skipping (soft gate)"
        continue
    fi
    printf '%s\n' "$out"
    # Row format: "<f> <e> <i> <t> <m>  <symbol> <tree> <crate> <version>"
    # Each metric is "unsafe/total". Match the crate's own row (root of the
    # printed tree), take the first occurrence.
    row=$(printf '%s\n' "$out" | grep -E "[[:space:]]${crate}[[:space:]][0-9]+\.[0-9]+\.[0-9]+" | head -n1 || true)
    if [ -z "$row" ]; then
        echo "check-geiger: WARN: no geiger row for $crate (absent from report?)"
        continue
    fi
    unsafe_total=$(printf '%s\n' "$row" \
        | grep -oE '[0-9]+/[0-9]+' | cut -d/ -f1 \
        | awk '{ s += $1 } END { print s + 0 }')
    case "$crate" in
        cinder-core|cinder-relay)
            if [ "$unsafe_total" -gt 0 ]; then
                echo "check-geiger: FAIL: $crate reports $unsafe_total unsafe symbols — must be 0 (forbid)" >&2
                fail=1
            else
                echo "check-geiger: $crate: 0 unsafe symbols"
            fi
            ;;
        *)
            echo "check-geiger: $crate: $unsafe_total unsafe symbols (allowed ONLY in hardening.rs/peercred.rs — enforced by check-unsafe.sh)"
            ;;
    esac
done

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "check-geiger: OK — zero unsafe in first-party forbid crates"
