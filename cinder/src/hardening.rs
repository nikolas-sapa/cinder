//! Process hardening. The ONLY module in `cinder` permitted `unsafe`
//! (besides `rpc/peercred.rs`, allow-listed in BRIEF). Every call is
//! best-effort: a failure warns and startup continues.

/// Apply OS-level hardening before any key material exists in memory.
pub fn apply() {
    #[cfg(unix)]
    {
        // Core dumps can carry plaintext, session keys, and the identity
        // static secret. Disable them.
        let lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &lim) } != 0 {
            tracing::warn!("hardening: setrlimit(RLIMIT_CORE, 0) failed");
        }
        // Keep key material out of swap. Best-effort: RLIMIT_MEMLOCK is
        // often low; failure is not fatal.
        if unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) } != 0 {
            tracing::warn!("hardening: mlockall failed (insufficient limit?)");
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Belt-and-suspenders vs ptrace/dumpable dumps.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            tracing::warn!("hardening: prctl(PR_SET_DUMPABLE) failed");
        }
    }
}
