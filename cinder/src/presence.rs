//! macOS user-presence gate for keychain access. Unsafe island like
//! `hardening.rs` — `LAContext` bindings are `unsafe fn`; permitted via
//! `#[allow(unsafe_code)]` on the module declaration in main.rs.
//!
//! The stronger design — `kSecAttrAccessControl` with
//! `kSecAccessControlUserPresence` on the keychain item itself — is not
//! possible for this binary: on macOS the attribute requires the
//! `keychain-access-groups` entitlement and real codesigning; unsigned
//! `cargo install` builds get errSecMissingEntitlement (-34018) from
//! `SecItemAdd` (verified on-device). So enforcement is app-level:
//! `LAContext::evaluatePolicy(DeviceOwnerAuthentication)` — Touch ID,
//! falling back to the login password — before the identity is read from
//! the OS keyring. Once per process start.
//!
//! Honest limit: this binds cinder's own code paths only. The keychain
//! item is still readable by other tools under its normal keychain ACL,
//! and a same-uid attacker could patch the check out of the binary —
//! endpoint-compromise territory (docs/THREAT-MODEL.md §3.5). The gate
//! stops casual interactive misuse of an unlocked session, not malware.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_local_authentication::{LAContext, LAError, LAPolicy};

// Long enough to type a login password on the fallback path, short
// enough that a prompt nobody sees can't leave the gate open — a
// timeout denies rather than silently degrading to an ungated read.
const LA_EVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Result of a user-presence check.
pub enum Presence {
    /// Touch ID or login password accepted.
    Authenticated,
    /// LocalAuthentication cannot prompt in this context (headless, CI,
    /// SSH) or no auth mechanism exists. The gate is inert — callers fall
    /// through to the normal keyring→age-file chain.
    Unavailable,
    /// Authentication was offered and failed, was cancelled, or timed out.
    Denied,
}

/// Evaluate `LAPolicyDeviceOwnerAuthentication` once, synchronously.
pub fn verify_user_presence() -> Presence {
    // SAFETY: the LAContext is created, used, and dropped within this
    // call on the current thread and stays alive for the whole
    // evaluation (the OS cancels with AppCancel on drop, which would land
    // as Denied — safe). The reply block only sends over a channel.
    unsafe {
        let ctx = LAContext::new();
        if ctx
            .canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthentication)
            .is_err()
        {
            return Presence::Unavailable;
        }
        let (tx, rx) = mpsc::channel();
        let block = RcBlock::new(move |success: Bool, err: *mut NSError| {
            let code = err.as_ref().map(|e| e.code()).unwrap_or_default();
            let _ = tx.send((success.as_bool(), code));
        });
        ctx.evaluatePolicy_localizedReason_reply(
            LAPolicy::DeviceOwnerAuthentication,
            &NSString::from_str("unlock the cinder identity"),
            &block,
        );
        match rx.recv_timeout(LA_EVAL_TIMEOUT) {
            Ok((true, _)) => Presence::Authenticated,
            // NotInteractive = UI impossible (headless/SSH): same class as
            // "no API" — the keyring read then succeeds or hits its own
            // 8s bounded fallback, matching non-gated behavior.
            Ok((false, code)) if code == LAError::NotInteractive.0 => Presence::Unavailable,
            Ok((false, _)) | Err(_) => Presence::Denied,
        }
    }
}
