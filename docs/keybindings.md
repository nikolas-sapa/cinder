# cinder — Keybindings

Status: bindings marked **spec'd** are contractual from `BRIEF.md` §TUI /
`PLAN.md` M4. Bindings marked **inferred** are conventional defaults —
verify against the implementation when M4 lands; this file is updated to
match.

Notation: `M-` = Alt/Meta, `C-` = Ctrl, `Alt-N` = Alt + digit.

## Input model

**Input is always focused.** There is no normal/insert mode split: typed
text goes straight to the input bar, and all navigation happens through
chords and slash commands.

## Keys

| Key | Action | Context | Status |
|---|---|---|---|
| _(printable text)_ | Goes to the input bar | everywhere | spec'd (input-always-focused) |
| `Enter` | Send the composed message | input bar | inferred — verify at M4 |
| `Alt-1` … `Alt-9` | Jump to conversation buffer 1–9 | global | spec'd |
| `M-a` | Jump to next conversation with unread messages | global | spec'd |
| `C-g` | Open fuzzy conversation switcher | global | spec'd |
| `M-k` | Compose in `$EDITOR` (long / multi-line messages) | input bar | spec'd |
| `PgUp` / `PgDn` | Scroll chat history (windowed fetch — never load-all) | chat pane | spec'd |
| `Esc` | Dismiss the current overlay/switcher; clear input when no overlay is open | global | inferred — verify at M4 |
| `C-c` | Quit cinder | global | inferred — verify at M4 |

## Slash commands

Typed at the start of the input bar.

| Command | Action | Status |
|---|---|---|
| `/help` | List commands and bindings | spec'd |
| `/add <uri>` | Add a contact from a `cinder://` link (rendezvous or static address) | spec'd |
| `/verify` | Show the contact's SAS (7 emoji / digit fallback) and confirm out-of-band verification | spec'd |
| `/finger` | Show the contact's key fingerprint / peer-id | spec'd |
| `/clear` | Clear the current buffer's visible history | spec'd |
| `/quit` | Exit cinder | spec'd |

## UI conventions

| Glyph | Meaning |
|---|---|
| `…` | queued — accepted into the outbound pipeline |
| `✓` | sent — live write to the peer, or deposited to the peer's relay mailbox |
| `✓✓` | delivered — the peer's client acked durable receipt in-band |

There is deliberately **no read state**: no cinder peer ever transmits a
read receipt (`read` is reserved in the protocol schema and never emitted
in v1).

Trust states shown per contact: `unverified` (neutral), `verified`
(badge), `key_changed` (loud — sending is blocked until re-verified via
`/verify` with the new SAS).

Layout for orientation: conversation list (activity-sorted, unread
counts, verified glyph) | chat pane (timestamps, per-peer color,
read-marker line, delivery ticks) | status bar | input bar.
