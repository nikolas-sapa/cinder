# cinder — Keybindings

Status: **verified** against `cinder/src/tui/keys.rs`, `cinder/src/tui/app.rs`
(`parse_command`), and `cinder/src/tui/mod.rs` (`handle_key`, `run_command`).
Anything marked **spec'd-only** is contractual in `BRIEF.md`/`PLAN.md` but has
no implementation yet.

Notation: `M-` = Alt/Meta, `C-` = Ctrl, `Alt-N` = Alt + digit.

## Input model

**Input is always focused.** There is no normal/insert mode split: typed
text goes straight to the input bar (`ratatui-textarea`), and all navigation
happens through chords and slash commands. `Enter` always submits — a draft
cannot contain a literal newline; for multi-line messages compose in
`$EDITOR` via `M-k`.

## Keys — main UI

| Key | Action | Context | Status |
|---|---|---|---|
| _(printable text)_ | Goes to the input bar | everywhere | verified |
| `Enter` | Submit: send the draft, or run it as a `/command` if it starts with `/` | input bar | verified |
| `Alt-1` … `Alt-9` | Jump to conversation buffer 1–9 | global | verified |
| `M-a` | Jump to next conversation with unread; "no unread chats" if none | global | verified |
| `C-g` | Open fuzzy conversation switcher | global | verified |
| `M-k` | Compose in `$EDITOR` (default `vi`); draft loads back into the input bar, empty result discarded | input bar | verified |
| `PgUp` | Scroll chat history up; near the top it fetches the next older page (windowed, 50 msgs — never load-all) | chat pane | verified |
| `PgDn` | Scroll chat history down | chat pane | verified |
| `Esc` | **No-op in the main UI** — it does *not* clear the draft (use `/clear`) | main UI | verified — differs from spec |
| `C-c` | Quit cinder | global, also inside every overlay | verified |

## Keys — fuzzy switcher (`C-g`)

| Key | Action | Status |
|---|---|---|
| _(printable text)_ | Append to the query | verified |
| `Backspace` | Delete last query char | verified |
| `Up` / `Down` | Move selection | verified |
| `Enter` | Activate selected chat | verified |
| `Esc` | Close the switcher | verified |
| `C-c` | Quit cinder | verified |

## Keys — overlays (`/help`, `/finger`)

| Key | Action | Status |
|---|---|---|
| `Esc` | Close the overlay | verified |
| `q` | Close the overlay | verified |
| `C-c` | Quit cinder | verified |
| _(anything else)_ | No-op | verified |

## Slash commands

Typed at the start of the input bar (`cinder/src/tui/app.rs:parse_command`).

| Command | Action | Status |
|---|---|---|
| `/help`, `/h`, `/?` | List commands and bindings | verified |
| `/add <uri>`, `/invite <uri>` | Add a contact from a `cinder://` link — rendezvous or static. `/invite` is an **alias for `/add`** (it consumes a link); minting a link is `cinder id` / `invite.create` only | verified |
| `/verify` | No argument: open the fingerprint overlay (same as `/finger`) — shows contact, peer-id, trust, and the current SAS | verified |
| `/verify <sas>` | Mark the active contact verified: `<sas>` is the 7-emoji string or the digit fallback, compared whitespace-normalized | verified |
| `/finger`, `/fp` | Show the active contact's fingerprint overlay | verified |
| `/clear` | Clear the input draft (not the history) | verified |
| `/status` | Connection state + peer-id + chat/unread counts in the status line | verified |
| `/next` | Same as `M-a` — next unread chat | verified |
| `/send <path>` | Send a file/image/voice note (≤ 64 MiB) to the active chat; encrypts inside the Noise session and queues an offer | verified |
| `/accept [file:…]` | Accept an inbound file offer (defaults to the most recent offered transfer in the chat) | verified |
| `/decline [file:…]` | Decline an inbound file offer | verified |
| `/cancel <file:…>` | Cancel an active or offered transfer, either direction | verified |
| `/save [dir]` | Save the selected/completed received file to `dir` (default: downloads dir); untrusted names are sanitized to a basename | verified |
| `/files` | List transfers in the active chat with state and progress | verified |
| `/call`, `/dial` | Ring the active chat's contact for an encrypted voice call (one call per account) | verified |
| `/answer [call:…]`, `/pickup` | Answer the inbound ringing call (`call:` id optional when exactly one rings) | verified |
| `/hangup [call:…]`, `/endcall` | Hang up / cancel / abort the current call | verified |
| `/mute [off\|0]` | Local mic gate on the active call — sends **nothing** on the wire; `off`/`0` un-mutes | verified |
| `/unmute` | Un-mute the local mic | verified |
| `/quit`, `/q` | Exit cinder | verified |
| _(anything else)_ | `unknown command <x> (/help)` on the status line | verified |

## File messages

Transfers render as placeholders in the chat pane, never inline bytes:

- `[file] name · 1.2 MiB` — generic attachment
- `[image] name · 340 KiB` — `kind:image` (thumbnail shown where supported)
- `[voice 0:12]` — `kind:audio` with duration from `audio.duration_ms`

Progress replaces the placeholder tail while `transferring`
(`· 42%`), and terminal states append the reason (`· failed: hash_mismatch`,
`· declined`, `· expired`). Inbound filenames are untrusted peer data and are
basename-sanitized before rendering — the wire can never inject a path.

## Known gap — no inbound-request affordance

There is **no key or slash command to accept or decline an inbound contact
request.** `contact.request` events are not mapped to any TUI event, and
`/accept` does not exist. Inbound requests land `pending_in` and are only
answerable via `contact.accept` / `contact.decline` on `cinder rpc` /
`cinder daemon`. The `pending_in` chat still appears in the list on the
next TUI start and messaging works while pending — only `declined` /
`blocked` peers are gated.

## UI conventions

| Glyph | Meaning |
|---|---|
| `…` | queued — accepted into the outbound pipeline |
| `✓` | sent — live write to the peer, or deposited to the peer's relay mailbox |
| `✓✓` | delivered — the peer's client acked durable receipt in-band |
| `!` | failed — terminal failure (`expired` renders as `!` too) |

There is deliberately **no read state**: no cinder peer ever transmits a
read receipt (`read` is reserved in the protocol schema and never emitted
in v1).

Trust states shown per contact: `unverified` (`○`), `verified` (`●`),
`key_changed` (`✕` — sending is blocked until re-verified via
`/verify <sas>` with the new SAS).

Status bar glyphs: `●` connected, `…` connecting, `○` offline.

Layout for orientation: conversation list (activity-sorted, unread
counts, trust glyph) | chat pane (timestamps, per-peer color,
read-marker line, delivery ticks) | status bar | input bar.
