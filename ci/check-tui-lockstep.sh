#!/usr/bin/env bash
# check-tui-lockstep.sh — ratatui/crossterm version-pair gate.
#
# GUARDRAIL (BRIEF.md): ratatui + crossterm must stay a matched pair —
# mixed majors = lost-input-event bug. deny.toml additionally bans multiple
# resolved versions of each; this script asserts the resolved pair is a
# known-good one and fails loudly on drift.
#
# Compatibility map, ratatui 0.<minor> -> allowed crossterm 0.<minor>:
#   0.30.x -> 0.29.x        (current pair; Cargo.lock: ratatui 0.30.2 / crossterm 0.29.0)
#   0.29.x -> 0.28.x
#   0.28.x -> 0.27.x, 0.28.x
#   0.26.x -> 0.27.x
# An unmapped ratatui minor = FAIL — check upstream pairing, then update the map.
set -euo pipefail
cd "$(dirname "$0")/.."

die() { echo "check-tui-lockstep: FAIL: $*" >&2; exit 1; }

# Print every locked version of crate $1 from Cargo.lock.
lockvers() {
    awk -v want="$1" '
        /^name = /    { gsub(/"/, "", $3); n = $3 }
        /^version = / { if (n == want) { gsub(/"/, "", $3); print $3 } }
    ' Cargo.lock
}

rat_versions=$(lockvers ratatui)
ct_versions=$(lockvers crossterm)

[ -z "$rat_versions" ] && die "ratatui not in Cargo.lock — TUI dep missing or renamed"
[ -z "$ct_versions" ]  && die "crossterm not in Cargo.lock — TUI dep missing or renamed"

rat_count=$(printf '%s\n' "$rat_versions" | grep -c .)
ct_count=$(printf '%s\n' "$ct_versions"  | grep -c .)
[ "$rat_count" -gt 1 ] && die "multiple ratatui versions locked: $(echo "$rat_versions" | tr '\n' ' ')"
[ "$ct_count"  -gt 1 ] && die "multiple crossterm versions locked: $(echo "$ct_versions" | tr '\n' ' ')"

rat="$rat_versions"; ct="$ct_versions"
rat_major=${rat%%.*};  rat_minor=$(echo "$rat" | cut -d. -f2)
ct_major=${ct%%.*};    ct_minor=$(echo "$ct" | cut -d. -f2)
[ "$rat_major" = "0" ] || die "ratatui $rat is not a 0.x series — lockstep map needs a rewrite"
[ "$ct_major"  = "0" ] || die "crossterm $ct is not a 0.x series — lockstep map needs a rewrite"

allowed=""
case "$rat_minor" in
    30) allowed="29" ;;
    29) allowed="28" ;;
    28) allowed="27 28" ;;
    26) allowed="27" ;;
    *)  die "ratatui 0.$rat_minor.x has no known crossterm pairing — check upstream, update ci/check-tui-lockstep.sh" ;;
esac

match=0
for m in $allowed; do
    [ "$ct_minor" = "$m" ] && match=1
done
if [ "$match" -ne 1 ]; then
    die "version drift: ratatui $rat pairs with crossterm 0.{$(echo "$allowed" | tr ' ' ',')}.x, locked crossterm is $ct"
fi

echo "check-tui-lockstep: OK — ratatui $rat + crossterm $ct (matched pair)"
