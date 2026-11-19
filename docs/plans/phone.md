# cinder — Plan: paths to phone

Status: scoped spec. Three routes, ordered by effort-to-value: **(a) Termux
today** — the existing binary, mostly builds as-is; **(b) daemon-remote** —
phone as a client of `cinder daemon` running on a desktop; **(c) native
app** — `cinder-core`/`cinder-app` as a library + `uniffi` → Swift/Kotlin
shell. Each path lists what works, what breaks, and what is explicitly NOT
protected. Lifts the v1 non-goal "mobile platforms" (PLAN §1, THREAT-MODEL
§7) only to the extent each path specifies.

---

## 1. Goals / non-goals

### Goals

- A working, honest account of how to run cinder from a phone *today*
  (Termux) with exact build steps and real caveats.
- A remote-access model that needs **no new crypto and no new protocol**:
  the phone speaks the existing JSON-RPC to a desktop daemon over a
  transport shim.
- A library-shaped native path: what the FFI surface must expose (mirrored
  from the `rpc` surface), what the keystore becomes on iOS/Android, and
  the extraction it requires.

### Non-goals (all paths, v1)

- **No background push** (APNs/FCM). cinder has no server-side identity —
  there is nothing to register a device token *to*. Phone delivery is
  foreground-or-deferred: messages land in the relay mailbox and surface on
  next open, same as any offline client.
- **No multi-device.** One identity keypair per install, per THREAT-MODEL;
  a phone is either *the* device or a view onto the device (path b). Native
  app = new identity = re-add contacts (no key import/export in v1 — that's
  a deliberate gap, not an oversight: key sync is where mobile messengers
  die).
- No battery/lifecycle heroics, no CallKit/PushKit, no iOS share
  extensions, no Android Auto.
- No promises about Android backgrounding: the OS kills background
  sockets; path (a) needs `termux-wake-lock` and still won't survive Doze
  reliably — stated plainly.

---

## 2. Path A — Termux (works today, with flags)

### 2.1 Build inside Termux (recommended path)

```sh
pkg update && pkg upgrade -y
pkg install -y rust clang binutils sqlite git
git clone <cinder-repo> && cd cinder
cargo build --release -p cinder -p cinder-relay
# binary lands at target/release/cinder
```

- Termux's `rust` package compiles natively for `aarch64-linux-android`
  — **no cross toolchain needed**; `rustc --print target-list` inside
  Termux already lists it as host.
- `rusqlite` `bundled-sqlcipher` compiles its own C: `clang` + `binutils`
  cover it. No OpenSSL anywhere (crypto guardrail), which is exactly why
  Android cross-compile is sane.
- `dirs::data_dir()`/`home_dir()` resolve under
  `/data/data/com.termux/files/home` — private by construction, so
  `data_dir()`'s `0700` chmod is a no-op on already-private storage.
  Explicit `--data-dir ~/.local/share/cinder` if in doubt.

### 2.2 Cross-compile (if you want binaries from a laptop)

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk          # needs ANDROID_NDK_HOME
cargo ndk -t aarch64-linux-android build --release -p cinder
```

The NDK is only needed for the bundled-sqlcipher C compile; the Rust side
is clean. `cargo-ndk` handles the linker/cc env.

### 2.3 Keystore — the part that actually works

`Identity::load_or_create` (identity.rs:72) tries `KeyringStore` first and
falls through to `AgeFileStore` on any failure. On Android:

- `keyring = "4.2.0"` has **no Android backend**: its linux-native path
  uses kernel `keyutils`/`libkeyutils`, which Android neither ships nor
  permits. Runtime consequence: keyring calls fail → automatic fallback to
  the age file — **the age keystore path is the Termux keystore**, and it
  needs no keyring at all. Set `CINDER_KEYSTORE=age` to skip the dead
  probe (also silences the latency of a doomed lookup).
- Compile consequence to flag honestly: if `keyring`'s transitive deps
  fail to build on `aarch64-linux-android` (they may — verify on the first
  build), gate it: `keyring` becomes
  `[target.'cfg(not(target_os = "android"))'.dependencies]` and
  `KeyringStore` behind `#[cfg]` in keystore.rs. One Cargo.toml line +
  one cfg — spec'd, not yet needed.
- `rpassword` (age passphrase prompt) reads `/dev/tty` — works in Termux.

### 2.4 Real Android/Termux caveats (found in code, not guessed)

| Issue | Location | Reality |
|---|---|---|
| `peercred.rs` has `#[cfg(target_os = "linux")]` for `SO_PEERCRED` — `target_os` on Android is `"android"`, NOT `"linux"` | `cinder/src/rpc/peercred.rs:23` | The `not(macos,linux)` stub returns `Unsupported` → `same_uid()` is always false → **`cinder daemon` rejects every client on Termux today**. Fix is one word: add `"android"` to the cfg arm (bionic provides `ucred`/`SO_PEERCRED`). Stdio `cinder rpc` is unaffected — daemon only |
| `PR_SET_DUMPABLE` behind same `target_os = "linux"` cfg | `cinder/src/hardening.rs:24` | Skipped silently on Android. `prctl`/`PR_SET_DUMPABLE` exists in bionic — same one-word cfg fix; best-effort either way |
| `mlockall` | `hardening.rs:20` | Fails on Android (low `RLIMIT_MEMLOCK`) — best-effort warn already; no action |
| Background life | Android OS | Foreground process is fine; backgrounded Termux gets killed/suspended — `termux-wake-lock` helps, Doze still wins sometimes. A `cinder daemon` in Termux is a best-effort resident, not a service |
| TUI | `crossterm`/ratatui | ANSI works in the Termux terminal; touch keyboard ≠ real keyboard — keybindings still work but Alt-key chords depend on the keyboard app. Non-blocking |
| Daemon socket dir | `rpc/server.rs:28` | `XDG_RUNTIME_DIR` unset on Termux → falls back to `~/.local/state/cinder/daemon.sock`, `0700`/`0600` perms preserved — correct by default |

**Honest verdict:** `cinder rpc`, `cinder` TUI, `cinder id`, `cinder add`,
`cinder relay` work on Termux once built (plus the keyring cfg if the dep
fails to compile). `cinder daemon` is **broken on Android until the
peercred cfg lands** — that's the one real code change Path A needs, and
it's in §5.

## 3. Path B — daemon-remote (phone as a view onto a desktop)

The phone runs a thin client; the identity, keys, store, and relay link
stay on a desktop running `cinder daemon`.

**Honest constraint up front:** the daemon socket is **local-only by
construction** — unix socket, `SO_PEERCRED`/`getpeereid` same-uid check
(AGENT-API §1.2, server.rs:78–111). There is no TCP listener, no token
layer, no remote auth. Any remote path is a **transport shim in front of
the socket**, not a daemon feature — and the shim is where all the
security lives or dies.

### 3.1 Recommended shim: SSH unix-socket forwarding

OpenSSH forwards unix sockets natively (`StreamLocal`). On the phone
(Termux `openssh`, or any SSH-capable client app):

```sh
# on phone: forward local socket → desktop daemon socket
ssh -N -L "$HOME/.local/state/cinder/daemon.sock:/run/user/1000/cinder/daemon.sock" user@desktop
# now $HOME/.../daemon.sock on the phone IS the desktop daemon
```

Why this is the right answer and not a hack:

- **Auth stays intact.** The forwarded connection arrives at the daemon
  socket from the *desktop's own sshd*, running as your uid →
  `SO_PEERCRED`/`getpeereid` sees daemon-uid == sshd-uid == yours. The
  same-uid check passes **without weakening it**: the phone never gets a
  unix peer credential at all — it got SSH auth instead, which is stronger
  (real key auth + your sshd policy).
- **Crypto is SSH's, which is fine** — the no-TLS rule is about the
  cinder↔relay path; a localhost-bound SSH forward is a standard admin
  channel, and the RPC stream is localhost-plaintext only *inside* the
  tunnel endpoint.
- Nothing in cinder changes except: the phone needs a **client that speaks
  the daemon protocol** — which doesn't exist as a shipped mode yet. The
  gap (§5): `cinder` gains a remote binding — a TUI/rpc-client mode that
  dials a unix socket instead of spawning `Core` in-process. Options:
  `cinder --remote <sock>` for TUI-over-RPC, or run the phone-side JSON
  from agents/scripts directly. The protocol is already the full surface
  (AGENT-API is explicitly designed so the TUI is just a client).

### 3.2 Alternative shims — evaluated, with the warnings they deserve

| Shim | Story | Verdict |
|---|---|---|
| `socat TCP-LISTEN:7071,bind=127.0.0.1,fork UNIX-CONNECT:daemon.sock` on desktop + `ssh -L 7071:…` | Same SSH answer with TCP on the desktop side | Works; no benefit over StreamLocal; keep `bind=127.0.0.1` always |
| `socat TCP-LISTEN:7071 UNIX-CONNECT:…` **exposed on LAN/WAN** | Direct TCP bridge | **DO NOT.** The daemon's only auth is uid equality, which a TCP peer cannot present — a bare TCP bridge gives any network peer **full user capability** (send as you, `ident.verify`, read history). This is the THREAT-MODEL §3.6 surface thrown open |
| TLS'd TCP bridge (`socat OPENSSL`/nginx stream + client certs, or a future `cinder daemon --tcp --token`) | Proper remote binding | Legitimate but a new auth domain (cert pinning or bearer token = the v2 `session.hello{token}` surface). Spec'd as future work alongside v2 capability tokens, not v1 — reusing SSH avoids inventing auth |
| Tailscale/WireGuard + TCP bridge | Private network + bridge | The VPN authenticates *the machine*, not *the process*: anything on the tailnet node hits the socket with no further check. Acceptable on a single-user tailnet; still weaker than SSH — flag, don't forbid |

### 3.3 What the phone-side client needs (spec delta)

- `cinder rpc`/`daemon` are servers; the remote client is new:
  `cinder --remote <unix-path>` (TUI speaks JSON-RPC to the socket) or a
  documented `nc -U`/wedged-in agent. TUI-over-RPC requires the backend
  trait (`tui/backend.rs::Backend`) to grow an `RpcBackend` impl — the
  trait is already shaped like the RPC surface (list_chats/history/send/
  subscribe ≈ chat.list/chat.history/msg.send/events.subscribe); delta is
  real but bounded.
- Events come through `events.subscribe` with `since` resume — reconnects
  on a phone are frequent; the replay log + `event.gap` machinery is
  exactly what it's for. Phone clients SHOULD subscribe `since:` their
  last seq every reconnect.
- `file.save`/`file.send` path params (files spec) are **daemon-local
  paths** — remote file transfer needs explicit upload/download plumbing;
  out of scope for Path B v0, flagged.

**What is NOT protected (Path B):** whoever holds the SSH key to the
desktop account holds full agent capability — the shim moves the trust
boundary from "same-uid process" to "anyone with your ssh credentials";
SSH hardening (key-only auth, `PermitRootLogin no`) is the entire game.
Traffic between phone and desktop is SSH-protected; traffic on each side
of the forward is localhost. The relay↔desktop leg is unchanged.

## 4. Path C — native app (cinder-core as library + uniffi)

### 4.1 What exists vs. what's needed

- `cinder-core` is already a clean library: `proto`, `session`, `identity`,
  `contacts`, `keystore`, `frame` — crypto + wire, `forbid(unsafe_code)`.
- **The domain core is not a library.** `cinder/src/core.rs` (3 729 lines —
  sessions, mailbox, AppMsg, store, TuiBackend) lives in the bin crate. A
  native app needs that logic, so Path C's first move is mechanical:
  **extract `cinder-app` as a lib crate** (core.rs + store.rs + rpc core
  types), leaving `cinder` bin as TUI+CLI glue. This is also what a
  `RemoteBackend` for Path B would reuse — the extraction pays for both.
- `uniffi` (Mozilla) generates Swift + Kotlin scaffolding from a Rust
  interface (UDL or proc-macros), supports async → maps onto the tokio
  runtime cinder-app already owns. Alternative `flutter_rust_bridge` —
  noted, not spec'd (uniffi covers both target shells in one interface
  definition and has the better long-term API).

### 4.2 The FFI surface — mirrored from the RPC catalog

The FFI layer exposes the `CinderCore` surface, not `cinder-core` internals.
Functions (Swift/Kotlin names auto-generated):

| FFI call | Maps to RPC | Returns |
|---|---|---|
| `identity()` | `session.hello` account | `{peer_id, display_name}` |
| `invite_create(label?, ttl?)` | `invite.create` | `{invite_id, link, expires_at}` |
| `contact_add(link, name?)` / `contact_accept(id)` / `contact_decline(id)` / `contact_list()` | `contact.*` | contact objects |
| `chat_list(limit?, cursor?)` / `chat_history(chat_id, before?, limit?)` | `chat.*` | paged models |
| `msg_send(to, body, client_msg_id)` / `msg_edit` / `msg_delete` / `msg_react` | `msg.*` | `{msg_id, state, …}` |
| `ident_verify(contact_id, sas)` / `sas_hint(contact_id)` | `ident.verify` + `security.verification_needed` payload | trust state |
| `file_send(to, path, …)` / `file_save(file_id, dir)` | `file.*` (files spec) | transfer objects |
| `call_ring(to)` / `call_accept` / `call_hangup` | `call.*` (calls spec) | call objects |
| `set_event_listener(cb)` | `events.subscribe` | **uniffi callback interface**: `on_event(kind: String, seq: u64, payload_json: String)` — the push model for `message.received`, `msg_state`, `file_state`, `call.*`, `security.*` |
| `shutdown()` | `shutdown.request` | drain |

Deliberately NOT in the FFI surface: `events.unsubscribe` (the callback
registration IS the subscription), `rpc.describe` (FFI *is* the schema),
relay administration, `cinder id --qr` (the shell renders its own QR from
`identity().peer_id`+link).

### 4.3 Mobile keystore & store

- `KeyStore` is already a trait (keystore.rs:10). Mobile impls:
  iOS `Security.framework` (kSecClassGenericPassword, accessible-when-
  unlocked-this-device-only — **no iCloud sync**, sync would be key export
  by another name) and Android `Keystore` (non-exportable AES key wrapping
  the X25519 private key — Keystore can't store raw EC keys for us, so:
  wrap the 32 B secret in a Keystore-generated AES key, store the wrapped
  blob in app-private files; unwrap on unlock).
- sqlcipher store key comes from the same keystore; `contacts.json` HMAC
  is keyed by `HKDF(static_secret)` — unchanged, already device-local.
- Data dir = app sandbox (`Application Support` / `filesDir`) — already
  `0700`-equivalent by OS construction.

### 4.4 The push problem, honestly

iOS kills background network activity; Android aggressively dozes. The
only v1-consistent answers:

- **Foreground app** — works, is what it is. Push notifications *about
  pending mail* can't exist without a server that knows you're waiting —
  the relay knows your mailbox but is intentionally untrusted; a
  notification-only push service that learns "mailbox X may have mail" is
  buildable (relay → APNs webhook on the user's own relay) and is its own
  spec, flagged NOT here.
- **Path B hybrid** — desktop daemon stays online; phone is a remote view.
  No push needed; the desktop does the being-online.

### 4.5 What stays out of scope

- Background push delivery (above), multi-device, key sync/migration (the
  phone's native identity is NEW — re-add contacts or use Path B),
  CallKit/PushKit integration, `Siri`/`Intent` surface, WearOS/watchOS.

**What is NOT protected (Path C):** same endpoint limits as desktop —
a compromised phone OS (root/jailbreak, pegasus-class) reads app memory
and keystores; mobile keystore raises the bar for *offline theft* (key
extraction resistance) but not for live-device compromise. Additionally:
the FFI boundary is a new same-process trust surface — any library linked
into the app shares it; `trusted:false` discipline must survive the JSON
hop to the shell (the FFI marks inbound content untrusted in generated
models — it MUST, or the prompt-injection hygiene is lost in translation).

---

## 5. Implementation deltas (all paths)

| File | Change | Path |
|---|---|---|
| `cinder/src/rpc/peercred.rs:23` | `#[cfg(target_os = "linux")]` → `#[cfg(any(target_os = "linux", target_os = "android"))]` | A (unblocks Termux daemon) |
| `cinder/src/hardening.rs:24` | same cfg widening for `PR_SET_DUMPABLE` | A |
| `cinder-core/Cargo.toml` | `keyring` → `cfg(not(target_os = "android"))` dep + `#[cfg]` on `KeyringStore` | A (only if build fails) |
| `cinder/src/tui/backend.rs` + new `rpc_backend.rs` | `RpcBackend` impl of `Backend` over a unix-socket JSON-RPC client | B |
| `cinder/src/main.rs` | `cinder --remote <sock>` flag → TUI on `RpcBackend` | B |
| new `cinder-app/` crate | extract `core.rs` + `store.rs` from bin → lib; `cinder` depends on it | B + C |
| new `cinder-ffi/` crate | `uniffi` scaffolding over `cinder-app` (§4.2 surface) | C |
| `cinder/src/rpc/server.rs` | optional `--tcp` listener **rejected** for v1 (§3.2) — no change | B |

---

## 6. Test plan

| # | Test | Where | Pass criteria |
|---|---|---|---|
| PH1 | Termux build | manual + CI cross-check | `cargo ndk -t aarch64-linux-android build` exits 0; `cinder --version` runs on-device; `CINDER_KEYSTORE=age` identity load creates `identity.age` 0600 |
| PH2 | Termux peercred | `cinder/tests/rpc_daemon.rs` (android cfg) | after cfg fix, same-uid client accepted on Android; stub path still rejects on unknown platforms |
| PH3 | Termux e2e | manual on-device | two cinder instances (termux sessions) round-trip a message via a LAN relay < 5 s |
| PH4 | SSH forward path | `cinder/tests/` + manual | StreamLocal-forwarded socket passes `same_uid` on the desktop side; rpc client through forward completes `session.hello`→`msg.send`→`message.received` loop |
| PH5 | TCP-bridge refusal doc | docs check | spec test: `grep` the phone doc + AGENT-API for the DO-NOT on bare TCP bridging — the warning exists (this doc §3.2) |
| PH6 | RpcBackend parity | `cinder/tests/` | `RpcBackend` implements all `Backend` trait methods; scripted session (add→send→history→events) passes the same assertions as `TuiBackend` path, reconnect-with-`since` resumes ≥ 95% of emitted events |
| PH7 | FFI smoke | `cinder-ffi/tests` | uniffi gen for `aarch64-apple-ios` + `aarch64-linux-android` compiles; in-process FFI call chain `identity()→contact_add→msg_send→on_event` completes < 5 s on loopback |
| PH8 | Keystore impls | `cinder-ffi` | wrapped-secret roundtrip under a mock mobile keystore: put→get byte-identical 32 B, 20/20; keystore-absent → `KeyUnavailable` not panic |
| PH9 | Event listener | `cinder-ffi` | 100 events fan to `on_event` callback in seq order, 0 dropped, 0 out-of-order |

---

## 7. Open flags

1. **Path B file transfer** — `file.send {path}`/`file.save {dir}` are
   daemon-local paths; remote file ops need upload/download verbs. Deferred
   deliberately: agents can already move small payloads via `file.send
   {data}`. Flagged, not solved.
2. **Identity per path** — Path A Termux can share `--data-dir` with a
   synced folder but **do not clone identity across devices** (concurrent
   same-peer-id connects → `PeerIdInUse` flapping; key duplication is also
   a theft-surface). Rule: one live device per identity.
3. **Push service** — the "mailbox-has-mail" webhook sketched in §4.4 is a
   real future design (relay-side, opt-in, learns only a mailbox id + a
   token — still a metadata leak vs pure v1). Needs its own spec + threat
   review before implementation.
4. **`dirs` crate on Android** — returns home-dir paths in Termux; inside a
   real Android app sandbox `dirs` is wrong — the FFI takes an explicit
   `data_dir` argument (shell provides `filesDir`), never `dirs::*`.
