//! File/image transfer helpers: pure, unit-testable machinery shared by the
//! transfer engine in `core.rs` — caps, chunk bitmaps, `.part` staging math,
//! save-path sanitization, and thumbnail generation. No Core/session deps.
//!
//! Normative values are spec §2 of docs/plans/files-images.md; everything is
//! injectable through `XferCaps` so tests can shrink the bounds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Normative caps (files-images §2), injectable for tests.
#[derive(Debug, Clone)]
pub struct XferCaps {
    /// `FILE_MAX_BYTES` — hard cap on FileOffer.size.
    pub max_bytes: u64,
    /// `FILE_CHUNK_BYTES` — default plaintext data per FileChunk.
    pub chunk_bytes: u32,
    /// `FILE_MAX_CHUNKS`.
    pub max_chunks: u32,
    /// `FILE_THUMB_MAX_BYTES`.
    pub thumb_max_bytes: usize,
    /// `FILE_THUMB_MAX_DIM` — long-edge cap for generated thumbnails.
    pub thumb_max_dim: u32,
    /// `FILE_MAX_INFLIGHT_PEER` — active transfers per peer, both dirs.
    pub max_inflight_peer: usize,
    /// `FILE_MAX_INFLIGHT` — active transfers per account.
    pub max_inflight: usize,
    /// `FILE_STAGE_MAX_BYTES` — aggregate inbound staging cap.
    pub stage_max_bytes: u64,
    /// `FILE_STAGE_TTL` — stale .part/have reap window.
    pub stage_ttl: Duration,
    /// `FILE_HAVE_INTERVAL` — FileHave emit cadence (verified chunks).
    pub have_interval: u32,
    /// `FILE_SEND_PACE` — per-chunk pacing; 80 fps ≈ 12.5 ms.
    pub send_pace: Duration,
    /// `FILE_RESUME_RETRY` — post-MailboxFull/stall repoll.
    pub resume_retry: Duration,
    /// Stale-staging reaper cadence (hourly in production).
    pub reap_every: Duration,
    /// `VOICE_MAX_MS` — advisory voice-note duration cap.
    pub voice_max_ms: u32,
    /// Inline `data` param decode cap (files-images §13.3).
    pub inline_data_max: usize,
    /// Serialized FileOffer budget: the whole AppMsg must fit the 8192 B
    /// plaintext bucket alongside the variant tag, so offer metadata+thumb
    /// gets a slightly tighter self-imposed cap (see DEVIATION note in
    /// core.rs — a 16 KiB thumb cannot fit the pad bucket).
    pub offer_budget: usize,
}

impl Default for XferCaps {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            chunk_bytes: 7168,
            max_chunks: 16_384,
            thumb_max_bytes: 16_384,
            thumb_max_dim: 320,
            max_inflight_peer: 4,
            max_inflight: 16,
            stage_max_bytes: 512 * 1024 * 1024,
            stage_ttl: Duration::from_secs(7 * 24 * 3600),
            have_interval: 64,
            send_pace: Duration::from_millis(12),
            resume_retry: Duration::from_secs(600),
            reap_every: Duration::from_secs(3600),
            voice_max_ms: 300_000,
            inline_data_max: 1024 * 1024,
            offer_budget: 7168,
        }
    }
}

/// `FileKind` on the wire (§3). Stored as lowercase strings in `files.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Image,
    Audio,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Image => "image",
            Self::Audio => "audio",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "image" => Self::Image,
            "audio" | "voice" => Self::Audio,
            _ => Self::File,
        }
    }

    /// Render tag for message fallbacks/TUI: `[file]`, `[image]`, `[voice]`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Image => "image",
            Self::Audio => "voice",
        }
    }
}

/// Voice-note metadata (§8); `audio_meta` column holds its JSON form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioMeta {
    pub duration_ms: u32,
    pub codec: String,
    #[serde(default)]
    pub sample_rate_hz: u32,
    #[serde(default)]
    pub channels: u8,
}

/// `FileCancel` reason enum (§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileCancelReason {
    User,
    Timeout,
    HashMismatch,
    StorageFull,
    Policy,
}

impl FileCancelReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Timeout => "timeout",
            Self::HashMismatch => "hash_mismatch",
            Self::StorageFull => "storage_full",
            Self::Policy => "policy",
        }
    }
}

/// Transfer lifecycle states (§4): `files.state` strings. `accepted` and
/// `verifying` are transient; the five on the right are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Offered,
    Accepted,
    Transferring,
    Verifying,
    Complete,
    Failed,
    Cancelled,
    Declined,
    Expired,
}

impl FileState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offered => "offered",
            Self::Accepted => "accepted",
            Self::Transferring => "transferring",
            Self::Verifying => "verifying",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Declined => "declined",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "offered" => Self::Offered,
            "accepted" => Self::Accepted,
            "transferring" => Self::Transferring,
            "verifying" => Self::Verifying,
            "complete" => Self::Complete,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "declined" => Self::Declined,
            "expired" => Self::Expired,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed | Self::Cancelled | Self::Declined | Self::Expired
        )
    }

    /// Non-terminal states that count toward inflight caps and resume scans.
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::Offered | Self::Accepted | Self::Transferring | Self::Verifying
        )
    }
}

// ---------- chunk bitmap ------------------------------------------------

/// `have` bitmap: bit i set = chunk i received-verified (inbound) or
/// FileHave/FileAccept-acked (outbound). ≤ 2 KiB at the chunks cap.
pub fn bitmap_new(chunks_total: u32) -> Vec<u8> {
    vec![0u8; chunks_total.div_ceil(8) as usize]
}

pub fn bit_get(map: &[u8], idx: u32) -> bool {
    map.get((idx / 8) as usize)
        .map(|b| b & (1 << (idx % 8)) != 0)
        .unwrap_or(false)
}

pub fn bit_set(map: &mut [u8], idx: u32) {
    if let Some(b) = map.get_mut((idx / 8) as usize) {
        *b |= 1 << (idx % 8);
    }
}

pub fn popcount(map: &[u8]) -> u32 {
    map.iter().map(|b| b.count_ones()).sum()
}

pub fn all_set(map: &[u8], chunks_total: u32) -> bool {
    (0..chunks_total).all(|i| bit_get(map, i))
}

/// Coalesced inclusive [start,end] ranges of set bits — the FileHave /
/// FileAccept wire shape.
pub fn ranges_of(map: &[u8], chunks_total: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut start: Option<u32> = None;
    for i in 0..chunks_total {
        if bit_get(map, i) {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s, i - 1));
        }
    }
    if let Some(s) = start {
        out.push((s, chunks_total - 1));
    }
    out
}

/// Merge a peer's ranges into `map`. Validates every range against
/// `chunks_total` — a malformed vector is rejected wholesale (no partial
/// merge). Returns the count of newly-set bits.
pub fn merge_ranges(
    map: &mut Vec<u8>,
    chunks_total: u32,
    ranges: &[(u32, u32)],
) -> std::result::Result<u32, ()> {
    if ranges.len() > chunks_total.div_ceil(2).max(1) as usize {
        return Err(());
    }
    for &(s, e) in ranges {
        if s > e || e >= chunks_total {
            return Err(());
        }
    }
    let mut added = 0u32;
    for &(s, e) in ranges {
        for i in s..=e {
            if !bit_get(map, i) {
                bit_set(map, i);
                added += 1;
            }
        }
    }
    Ok(added)
}

// ---------- staging ------------------------------------------------------

/// Staging dir: `<data_dir>/xfer`, created 0700 on demand.
pub fn stage_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("xfer")
}

/// Hash-named `.part` relative path: `xfer/<b32(sha256(file_id ‖ token))>.part`
/// — a directory listing cannot map blob → transfer without the token.
pub fn stage_rel(file_id: &[u8; 32], fetch_token: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(file_id);
    h.update(fetch_token);
    format!(
        "xfer/{}.part",
        data_encoding::BASE32_NOPAD
            .encode(&h.finalize())
            .to_lowercase()
    )
}

pub fn stage_abs(data_dir: &Path, rel: &str) -> PathBuf {
    data_dir.join(rel)
}

/// Create the staging dir (0700) and the `.part` file (0600) preallocated to
/// `size`. Sparse allocation — instant even at the 64 MiB cap.
pub fn stage_create(data_dir: &Path, rel: &str, size: u64) -> std::io::Result<PathBuf> {
    let dir = stage_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let abs = stage_abs(data_dir, rel);
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let f = opts.open(&abs)?;
        f.set_len(size)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(0o600));
    }
    Ok(abs)
}

/// Positioned write of one verified chunk into the `.part`.
pub fn stage_write_at(abs: &Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(abs)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(data)
}

/// Whole-file digest — the integrity root compared against FileOffer.sha256.
pub fn stage_sha256(abs: &Path) -> std::io::Result<[u8; 32]> {
    sha256_reader(&mut std::fs::File::open(abs)?)
}

pub fn sha256_reader(r: &mut impl std::io::Read) -> std::io::Result<[u8; 32]> {
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

/// Reap a `.part` quietly; missing is fine.
pub fn stage_unlink(abs: &Path) {
    let _ = std::fs::remove_file(abs);
}

// ---------- save-path sanitization (§5.4, normative) ----------------------

/// Candidate save name: basename semantics — split on both separators, drop
/// empty and `..` components, keep the last survivor; then strip NUL + all
/// C0/C1 controls and leading dots, restrict to the portable basename
/// charset, and fall back to `file.bin`. Never returns a separator.
pub fn sanitize_name(raw: &str) -> String {
    let last = raw
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .next_back()
        .unwrap_or("");
    let mut name: String = last
        .chars()
        .filter(|c| {
            // portable basename charset — drops controls AND anything that
            // could confuse a handler association (FT8 charset bound).
            matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | ' ' | '-')
        })
        .collect();
    while name.starts_with('.') {
        name.remove(0);
    }
    if name.len() > 255 {
        let mut end = 255;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    if name.is_empty() {
        "file.bin".to_string()
    } else {
        name
    }
}

/// Collision suffix: `name.ext` → `name-1.ext` … bounded at 64 attempts.
pub fn collision_name(base: &str, n: u32) -> String {
    if n == 0 {
        return base.to_string();
    }
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
        _ => format!("{base}-{n}"),
    }
}

/// `~`/`~user`-lite expansion: `~/x` → `$HOME/x`; absolute stays absolute.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

// ---------- thumbnail generation (§7) -------------------------------------

/// Sender-side thumbnail: header-probe dimension guards BEFORE decode
/// (≤ 8192×8192 and ≤ 100 Mpx), decode with explicit Limits, long-edge
/// resize to `thumb_max_dim`, JPEG q70, cap `thumb_max_bytes` — oversize
/// encode yields `None` rather than a quality-doom loop.
pub fn make_thumb(src: &Path, caps: &XferCaps) -> Option<Vec<u8>> {
    let (w, h) = image::ImageReader::open(src)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if w > 8192 || h > 8192 || w as u64 * h as u64 > 100_000_000 {
        return None;
    }
    let mut reader = image::ImageReader::open(src)
        .ok()?
        .with_guessed_format()
        .ok()?;
    reader.limits(image::Limits {
        max_image_width: Some(8192),
        max_image_height: Some(8192),
        max_alloc: Some(64 * 1024 * 1024),
    });
    let img = reader.decode().ok()?;
    let thumb = img.thumbnail(caps.thumb_max_dim, caps.thumb_max_dim);
    let mut buf = std::io::Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 70);
    thumb.write_with_encoder(enc).ok()?;
    let bytes = buf.into_inner();
    (bytes.len() <= caps.thumb_max_bytes).then_some(bytes)
}

// ---------- misc ---------------------------------------------------------

/// Constant-time 32-byte compare for `fetch_token` — resume offers are bound
/// by this check; never early-exit on a mismatch.
pub fn ct_eq32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut d = 0u8;
    for i in 0..32 {
        d |= a[i] ^ b[i];
    }
    d == 0
}

/// Human size for message fallbacks/TUI: `716 B`, `2.1 MiB`.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// `mm:ss` for voice-note rendering.
pub fn fmt_duration(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_roundtrip_ranges() {
        let mut m = bitmap_new(100);
        assert_eq!(m.len(), 13);
        for i in [0, 1, 2, 63, 64, 99] {
            bit_set(&mut m, i);
        }
        assert_eq!(popcount(&m), 6);
        assert_eq!(ranges_of(&m, 100), vec![(0, 2), (63, 64), (99, 99)]);
        assert!(!all_set(&m, 100));
        let mut full = bitmap_new(3);
        for i in 0..3 {
            bit_set(&mut full, i);
        }
        assert!(all_set(&full, 3));
    }

    #[test]
    fn merge_ranges_validates() {
        let mut m = bitmap_new(10);
        assert_eq!(merge_ranges(&mut m, 10, &[(2, 4), (7, 7)]).unwrap(), 4);
        assert_eq!(ranges_of(&m, 10), vec![(2, 4), (7, 7)]);
        assert!(merge_ranges(&mut m, 10, &[(5, 10)]).is_err());
        assert!(merge_ranges(&mut m, 10, &[(4, 2)]).is_err());
        // rejected wholesale: earlier bad range means no partial merge
        assert!(!bit_get(&m, 5));
        // idempotent re-merge adds nothing
        assert_eq!(merge_ranges(&mut m, 10, &[(2, 4)]).unwrap(), 0);
    }

    #[test]
    fn sanitize_name_traversal() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("/etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a\\b.exe"), "b.exe");
        assert_eq!(sanitize_name(".."), "file.bin");
        assert_eq!(sanitize_name("..."), "file.bin");
        assert_eq!(sanitize_name(".hidden"), "hidden");
        assert_eq!(sanitize_name("report.pdf"), "report.pdf");
        assert_eq!(sanitize_name("ev\x00il\x1b.txt"), "evil.txt");
        assert_eq!(sanitize_name("photo (2).png"), "photo 2.png");
        assert!(!sanitize_name("a/b/c").contains('/'));
        assert_eq!(sanitize_name(""), "file.bin");
    }

    #[test]
    fn collision_suffix() {
        assert_eq!(collision_name("a.txt", 0), "a.txt");
        assert_eq!(collision_name("a.txt", 1), "a-1.txt");
        assert_eq!(collision_name("noext", 3), "noext-3");
        assert_eq!(collision_name(".x", 1), ".x-1");
    }

    #[test]
    fn stage_name_is_hash_derived() {
        let a = stage_rel(&[1u8; 32], &[2u8; 32]);
        let b = stage_rel(&[1u8; 32], &[3u8; 32]);
        assert!(a.starts_with("xfer/") && a.ends_with(".part"));
        assert_ne!(a, b, "token binds the blob name");
    }

    #[test]
    fn fmt_helpers() {
        assert_eq!(fmt_size(716), "716 B");
        assert_eq!(fmt_size(2_202_009), "2.1 MiB");
        assert_eq!(fmt_duration(42_000), "0:42");
    }
}
