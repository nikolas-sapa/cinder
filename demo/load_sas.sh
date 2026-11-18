#!/usr/bin/env bash
# demo helper: lift the digit SAS out of the replayed
# security.verification_needed event into the tmux paste buffer.
set -u
grep -oE '"digits":"[0-9 ]+"' /tmp/cinder-demo/accept.json | head -n1 \
  | sed -E 's/"digits":"//; s/"$//' | tr -d '\n' > /tmp/cinder-demo/digits.txt
tmux load-buffer /tmp/cinder-demo/digits.txt
