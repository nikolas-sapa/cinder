# cinder — demo tape (charmbracelet/vhs)
#
# Regenerate:  vhs demo.tape   →   docs/demo.gif
# CI-regenerable target: docs/demo.gif is produced from this file by
# .github/workflows/demo.yml — never hand-edit the gif.
#
# Requires on PATH: vhs, tmux, cinder, cinder-relay. vhs additionally
# shells out to ttyd + ffmpeg — the workflow installs all of it.
# (`vhs` is not installed on every dev machine; the workflow is the
# canonical regenerator.)
#
# Verified against the built binaries — things that are easy to get
# wrong and why this tape looks the way it does:
#
#   * Two pane-local identities: without per-pane CINDER_DATA_DIR both
#     panes load the SAME default identity — you can't add yourself.
#   * CINDER_KEYSTORE=age pins the passphrase-file keystore: OS keychain
#     access can block on an unseen permission dialog (unsigned binaries
#     prompt per-access on macOS) — never in CI. First run prompts twice
#     (create + confirm), later runs once; prompts read /dev/tty, never
#     stdin. The tape types `demo-pass` wherever a real run prompts.
#     CINDER_IDENTITY=ephemeral is NOT used: `cinder id` and `cinder tui`
#     are separate processes and would get different peer-ids.
#   * The relay slot survives `cinder id` exiting, but the OWNER must be
#     online when the Msg1 arrives — the left side runs `cinder tui`
#     before the right runs `cinder add`.
#   * Inbound adds land pending_in and emit `contact.request`, which the
#     TUI cannot see or answer (no /accept exists). The tape accepts via
#     a one-shot `cinder rpc` — the honest path, and it demos the agent
#     interface.
#   * `/verify` with no argument only OPENS the fingerprint overlay; it
#     does not verify. `/verify <sas>` does — and since the tape can't
#     type emoji, it uses the digit SAS (ident.verify accepts either
#     form, whitespace-normalized). The digits are lifted out of the
#     replayed security.verification_needed event into a tmux buffer and
#     pasted into the input bar with `C-b ]`.

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

# ---------- setup (hidden): scratch dirs, local relay, attached tmux ----------
Hide
Type "pkill -f 'cinder-relay --listen' 2>/dev/null; tmux kill-session -t cinder 2>/dev/null; rm -rf /tmp/cinder-demo; mkdir -p /tmp/cinder-demo"
Enter
Type "cinder-relay --listen 127.0.0.1:7000 >/tmp/cinder-demo/relay.log 2>&1 &"
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
# focus returns to the left pane for the invite side
Ctrl+B
Type "o"
Sleep 400ms

# ---------- left pane: identity + one-time invite link ----------
Type "export CINDER_DATA_DIR=/tmp/cinder-demo/left CINDER_RELAY=127.0.0.1:7000 CINDER_KEYSTORE=age"
Enter
Type "cinder id --qr"
Enter
Sleep 1500ms
Type "demo-pass"
Enter
Sleep 800ms
Type "demo-pass"
Enter
Sleep 4s

# hidden: capture the minted URI before the TUI takes the screen
# (-J joins soft-wrapped lines so the full cinder:// URI stays one line)
Hide
Type "tmux capture-pane -p -J -S - > /tmp/cinder-demo/left-pane.txt"
Enter
Type "grep -o 'cinder://[^ ]*' /tmp/cinder-demo/left-pane.txt | head -n1 > /tmp/cinder-demo/uri.txt"
Enter
Show

# the slot owner must be online to answer the forwarded Msg1
Type "cinder tui"
Enter
Sleep 1500ms
Type "demo-pass"
Enter
Sleep 3s

# ---------- right pane: consume the link ----------
Ctrl+B
Type "o"
Sleep 400ms
Type "export CINDER_DATA_DIR=/tmp/cinder-demo/right CINDER_RELAY=127.0.0.1:7000 CINDER_KEYSTORE=age"
Enter
Hide
Type "URI=$(cat /tmp/cinder-demo/uri.txt)"
Enter
Show
Type "cinder add $URI"
Enter
Sleep 1500ms
Type "demo-pass"
Enter
Sleep 800ms
Type "demo-pass"
Enter
Sleep 8s
Type "cinder tui"
Enter
Sleep 1500ms
Type "demo-pass"
Enter
Sleep 3s

# ---------- left pane: the request is pending_in; accept over rpc ----------
# (contact.request has no TUI event and there is no /accept — this is
# the designed path, and it shows the same core agents use. Requests are
# piped — the passphrase prompt reads /dev/tty, never stdin, so frames
# on the pipe stay clean. stdout is tee'd so the pane shows the frames
# AND we can parse them.)
Ctrl+B
Type "o"
Sleep 400ms
Type "/quit"
Enter
Sleep 1s
Type "bash demo/rpc_list.sh"
Enter
Sleep 1500ms
Type "demo-pass"
Enter
Sleep 5s
# accept emits security.verification_needed with sas_hint {emoji,digits};
# subscribe with since:0 replays it into stdout → digits land in the file
Type "bash demo/rpc_accept.sh"
Enter
Sleep 1500ms
Type "demo-pass"
Enter
Sleep 8s
Hide
# accept emitted the verification_needed event; replay wrote it to
# accept.json — lift the digits into the tmux paste buffer for /verify
Type "bash demo/load_sas.sh"
Enter
Sleep 500ms
Show
Type "cinder tui"
Enter
Sleep 1500ms
Type "demo-pass"
Enter
Sleep 3s

# ---------- left pane: fingerprint overlay, verify with the digit SAS ----------
# /finger shows the 7-emoji SAS; /verify accepts the digit fallback too.
# The buffer holds the digits — paste with tmux paste-buffer (C-b ]).
Type "/finger"
Enter
Sleep 2s
Escape
Sleep 500ms
Type "/verify "
Ctrl+B
Type "]"
Sleep 500ms
Enter
Sleep 2s
Escape
Sleep 500ms

# ---------- right pane: same digits, same verdict ----------
Ctrl+B
Type "o"
Sleep 400ms
Type "/finger"
Enter
Sleep 2s
Escape
Sleep 500ms
Type "/verify "
Ctrl+B
Type "]"
Sleep 500ms
Enter
Sleep 2s
Escape
Sleep 500ms

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
