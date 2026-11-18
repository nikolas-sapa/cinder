//! Peer-credential check for the unix-socket daemon. All libc calls live
//! here behind a safe API — the only unsafe block in the rpc module tree,
//! gated like mod hardening.

#![allow(unsafe_code)]

use std::io;

pub type Uid = u32;

#[cfg(target_os = "macos")]
pub fn peer_uid(stream: &tokio::net::UnixStream) -> io::Result<Uid> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &tokio::net::UnixStream) -> io::Result<Uid> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        Ok(cred.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn peer_uid(_stream: &tokio::net::UnixStream) -> io::Result<Uid> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "peer credentials unavailable on this platform"))
}

#[cfg(unix)]
pub fn own_uid() -> Uid {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
pub fn own_uid() -> Uid {
    0
}
