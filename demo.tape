# cinder — demo tape (charmbracelet/vhs)
#
# Regenerate:  vhs demo.tape   →   docs/demo.gif
# CI-regenerable target: docs/demo.gif is produced from this file in CI.
# Never hand-edit the gif.
#
# Requires on PATH: tmux, cinder, cinder-relay.
#
# Written against the spec (BRIEF.md / AGENT-API.md / PROTOCOL.md) before
# the binaries exist. Pending details, marked TBD-*:
#   TBD-M3: how the cinder client selects its relay (flag/config/env) —
#           this tape assumes a configured default of 127.0.0.1:7000.
#   TBD-M4: contact-request accept affordance and the /verify confirm
#           flow — Enter is used as a placeholder confirm.
#
# Layout: left pane mints + shares a one-time rendezvous link; right pane
# consumes it; both verify SAS and chat. The left side stays connected in
# `cinder tui` while the right adds — the rendezvous slot is relay-side
# but the owner must be online to receive the forwarded handshake
# (PROTOCOL §7: offline slot owner = dropped Msg1).

Output docs/demo.gif

Set Shell bash
Set FontSize 14
Set Width 1920
Set Height 1080
Set TypingSpeed 60ms
Set PlaybackSpeed 1.0

Require tmux
Require cinder
Require cinder-relay

# ---------- setup (hidden): local relay + attached tmux ----------
Hide
Type "cinder-relay --listen 127.0.0.1:7000 >/tmp/cinder-relay.log 2>&1 &"
Enter
Sleep 1s
Type "clear"
Enter
Type "tmux new-session -s cinder"
Enter
Sleep 2s
Show

# ---------- split into two panes ----------
Ctrl+B
Type "%"
Sleep 1s

# ---------- left pane: identity + one-time invite link ----------
Type "cinder id --qr"
Enter
Sleep 3s

# hidden: capture the URI off this pane's scrollback before the TUI
# takes the screen (-J joins soft-wrapped lines), then open the TUI
Hide
Type "tmux capture-pane -p -J -S - > /tmp/cinder-left-pane.txt"
Enter
Type "cinder tui"
Enter
Sleep 2s
Show

# ---------- right pane: consume the link ----------
Ctrl+B
Type "o"
Sleep 400ms
Hide
Type "URI=$(grep -o 'cinder://[^ ]*' /tmp/cinder-left-pane.txt | head -n1)"
Enter
Show
Type "cinder add \"$URI\""
Enter
Sleep 4s
Type "cinder tui"
Enter
Sleep 2s

# ---------- left pane: inbound contact request ----------
Ctrl+B
Type "o"
Sleep 400ms
# TBD-M4: the pending contact.request is on screen; Enter stands in for
# the accept affordance (final binding lands with the TUI milestone)
Enter
Sleep 2s

# ---------- both panes: SAS verification ----------
# same 7-emoji SAS rendered on both sides — the visual compare IS the
# out-of-band check this demo demonstrates
Type "/verify"
Enter
Sleep 2s
# TBD-M4: confirm the SAS match (placeholder Enter)
Enter
Sleep 1s
Ctrl+B
Type "o"
Sleep 400ms
Type "/verify"
Enter
Sleep 2s
# TBD-M4: confirm the SAS match (placeholder Enter)
Enter
Sleep 1s

# ---------- exchange messages, 2 each way ----------
Type "noise XX done, relay saw only ciphertext"
Enter
Sleep 2s
Type "every inbound body arrives trusted:false"
Enter
Sleep 2s
Ctrl+B
Type "o"
Sleep 400ms
Type "sas matches, verified"
Enter
Sleep 2s
Type "same rpc drives the tui and your agents"
Enter
Sleep 2s

# ---------- hold on both verified chats ----------
Sleep 4s
