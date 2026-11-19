# Phone Build Status

Snapshot of how close cinder is to building for Android/iOS targets. Verified
on main (macOS arm64 host, stable toolchain) — `cargo check` results, not
linked binaries.

## Verdict matrix

| Crate | `aarch64-linux-android` | `aarch64-apple-ios` |
|---|---|---|
| `cinder-core` | **checks clean** | blocked: `apple-native-keyring-store` requires its `protected` feature on iOS (compile_error, `apple-native-keyring-store/src/lib.rs:50`) |
| `cinder-relay` | **checks clean** (pure-Rust deps) | same `keyring` block via `cinder-core` |
| `cinder` (bin/TUI) | blocked: `libsqlite3-sys` build script can't find `aarch64-linux-android-clang` (no NDK; `bundled-sqlcipher` compiles C) | blocked: same `keyring` issue first, then `libsqlite3-sys` C build would need Xcode toolchain |

`cargo check` on cinder-core/android succeeds because `check` never invokes a
linker and cinder-core has zero C dependencies (snow is pure-Rust crypto).
A real `cargo build` for android additionally needs the NDK for the linker
(`aarch64-linux-androidXX-clang`) even for cinder-core.

## Toolchain gotcha

`rust-toolchain.toml` pins `channel = "stable"` but the rustup default here is
`1.88.0`. Plain `rustup target add <t>` installs into the *default* toolchain
and cargo then fails with `can't find crate for 'core'`. Use:

```sh
rustup target add --toolchain stable aarch64-linux-android aarch64-apple-ios
```

## Android via Termux (works today, native compile)

On-device builds sidestep cross-compile entirely: Termux's `rust` package
targets `aarch64-linux-android` natively and Termux provides clang, so
`libsqlite3-sys` and the linker just work.

```sh
pkg install rust clang
git clone <repo> && cd cinder
cargo build --release --bin cinder
```

## Keystore on mobile

- `cinder` checks `CINDER_KEYSTORE` in `Identity::load_or_create`
  (`cinder-core/src/identity.rs:75`). `CINDER_KEYSTORE=age` skips the OS
  keyring and uses a passphrase-encrypted age file (`AgeFileStore`,
  `cinder-core/src/keystore.rs:69`). Recommended on Termux:
  `CINDER_KEYSTORE=age cinder ...`
- Even without the env var it degrades gracefully: on Android, keyring 4.x
  selects no store backend, `keyring::Entry::new` returns
  `Error::NoDefaultStore`, and `load_or_create` falls through to the age file
  (`identity.rs:87-98`). The env var just avoids the dead-end probe and the
  8s `KEYRING_OP_TIMEOUT` path.
- On iOS the keyring code doesn't even compile today (above). A real app would
  not use the `keyring` crate on mobile anyway: Android Keystore /
  iOS Secure Enclave need platform APIs, reached via JNI/Swift FFI (likely
  through the uniffi layer in `docs/plans/phone.md`).

## iOS path (blocked, by design)

- Termux equivalent doesn't exist; iSH emulates x86 and is far too slow.
- Real route: an Xcode app embedding cinder-core as a library via uniffi —
  tracked in `docs/plans/phone.md`.
- First source-level blocker to fix when that lands: the unconditional
  `keyring` dep in `cinder-core/Cargo.toml`. Either cfg-gate
  `KeyringStore`/the dep to non-iOS targets (the age fallback already covers
  it), or enable `keyring`'s `protected` store feature
  (`keyring` 4.2 only wires `apple-native-keyring-store/protected` into its
  `cli` feature, not the `v1` default). No source changes were made for this
  report.

## Exact failure points

| Target | Command | Fails at | Why |
|---|---|---|---|
| android | `cargo check -p cinder-core` | — (passes, ~11s) | no C deps, no linker needed for `check` |
| android | `cargo check -p cinder` | `libsqlite3-sys` build script | cc-rs can't find `aarch64-linux-android-clang`; NDK absent |
| ios | `cargo check -p cinder-core` | `apple-native-keyring-store` compile | `compile_error!` requiring `protected` feature |

Any real (non-check) mobile build also needs the platform linker/SDKs: Android
NDK + `ANDROID_NDK_HOME`, or Xcode command line tools for iOS.
