//! Phase A voice-call machinery (docs/plans/voice-video-calls.md §3–§6): the
//! pure, unit-testable pieces the call engine in `core.rs` drives — caps,
//! wire enums, the seq-ordered jitter buffer, the bounded outbound media
//! queue, the Opus duplex (audiopus), and the `MicIo`/`SpeakerIo` device
//! seam (cpal behind the `calls` feature, Null/Loopback for tests/headless).
//! No Core/session dependencies — like `xfer.rs` for the file transfer.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Opus wire format: 48 kHz mono, 20 ms frames (§3 media format).
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_MS: u64 = 20;
pub const SAMPLE_RATE: u32 = 48_000;
/// §3: `CallFrame.payload` ceiling — enforced at encode and on receipt.
pub const MAX_OPUS_PACKET: usize = 400;

/// `cap` bitmask: bit0 = audio/opus48k (the only Phase A media).
pub const CAP_AUDIO_OPUS48K: u8 = 0b0000_0001;
/// bit2 reserved now: "udp-capable" (Phase B §9) — a Phase A callee answers
/// a Phase B offer with a defined TCP fallback instead of silence.
#[allow(dead_code)]
pub const CAP_UDP_CAPABLE: u8 = 0b0000_0100;
/// bit1 = video — unused in Phase A.
#[allow(dead_code)]
pub const CAP_VIDEO: u8 = 0b0000_0010;
/// The only media kind `CallRing.media` carries in Phase A.
pub const MEDIA_AUDIO: u8 = 1;

/// Normative call bounds (§4 table), injectable for tests.
#[derive(Debug, Clone)]
pub struct CallCaps {
    /// `CALL_RING_TIMEOUT` — ring TTL, sent → `no_answer`.
    pub ring_timeout: Duration,
    /// `CALL_DELIVER_TIMEOUT` — no `CallRinging` → `ended(timeout)`.
    pub deliver_timeout: Duration,
    /// `CALL_MAX_ACTIVE` — per account; inbound while busy → `CallEnd{busy}`.
    pub max_active: usize,
    /// `CALL_FRAME_QUEUE` — outbound media queue cap; full → drop oldest.
    pub frame_queue: usize,
    /// `CALL_JITTER_TARGET` — playout delay, frames (120 ms = 6).
    pub jitter_target: usize,
    /// `CALL_JITTER_MAX` — horizon, frames (240 ms = 12).
    pub jitter_max: usize,
    /// `CALL_MEDIA_KEEPALIVE` — no-frames grace before `quality:"degraded"`.
    pub media_keepalive: Duration,
    /// `CALL_MEDIA_DEAD` — no frames → `ended(failed)`.
    pub media_dead: Duration,
    /// `CALL_MAX_DURATION` — hard cap → `ended(timeout)`.
    pub max_duration: Duration,
    /// `CALL_RING_MIN_GAP` — per-peer re-ring throttle → `rate_limited`.
    pub ring_min_gap: Duration,
    /// Janitor tick (deadlines, session-pin sweeps, quality emits). ≤2 s
    /// keeps "session death mid-call" inside its spec bound; tests shrink.
    pub janitor_tick: Duration,
    /// Mic pump cadence; production = 20 ms frame clock, tests speed up.
    pub frame_pace: Duration,
    /// Opus bitrate, bits/s (spec default 48 000; floor 32 000, ceil 64 000).
    pub bitrate: i32,
}

impl Default for CallCaps {
    fn default() -> Self {
        Self {
            ring_timeout: Duration::from_secs(60),
            deliver_timeout: Duration::from_secs(10),
            max_active: 1,
            frame_queue: 64,
            jitter_target: 6,
            jitter_max: 12,
            media_keepalive: Duration::from_secs(1),
            media_dead: Duration::from_secs(10),
            max_duration: Duration::from_secs(4 * 3600),
            ring_min_gap: Duration::from_secs(5),
            janitor_tick: Duration::from_millis(200),
            frame_pace: Duration::from_millis(FRAME_MS),
            bitrate: 48_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDir {
    In,
    Out,
}

impl CallDir {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Outgoing,
    Ringing,
    Active,
    Ended,
}

impl CallState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outgoing => "outgoing",
            Self::Ringing => "ringing",
            Self::Active => "active",
            Self::Ended => "ended",
        }
    }
}

/// `CallEnd.reason` wire codes (§3) + the local-only `key_changed` reason
/// (§4: key change mid-setup aborts like sends are blocked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Hangup,
    Decline,
    Busy,
    NoAnswer,
    Failed,
    Timeout,
    KeyChanged,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hangup => "hangup",
            Self::Decline => "decline",
            Self::Busy => "busy",
            Self::NoAnswer => "no_answer",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::KeyChanged => "key_changed",
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Hangup => 0,
            Self::Decline => 1,
            Self::Busy => 2,
            Self::NoAnswer => 3,
            Self::Failed => 4,
            Self::Timeout => 5,
            Self::KeyChanged => 6,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Hangup,
            1 => Self::Decline,
            2 => Self::Busy,
            3 => Self::NoAnswer,
            4 => Self::Failed,
            5 => Self::Timeout,
            6 => Self::KeyChanged,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Good,
    Degraded,
}

impl Quality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Degraded => "degraded",
        }
    }
}

/// Signed modular distance `a − b` for per-direction u16 seq wraparound
/// (§3: modular compare).
fn seq_dist(a: u16, b: u16) -> i16 {
    a.wrapping_sub(b) as i16
}

/// §6 jitter buffer: seq-ordered, target-deep playout, bounded to the
/// 240 ms horizon. Over TCP arrivals are in-order; `seq` exists so the
/// receiver detects drops (sender backpressure, relay batch drop) and
/// feeds the decoder's PLC.
pub struct JitterBuf {
    target: usize,
    max: usize,
    /// Key = seq unwrapped against `anchor` (modular compare inside the
    /// ±32 768 window — the ≤12-frame spread is always unambiguous).
    map: BTreeMap<i64, Vec<u8>>,
    anchor: Option<(u16, i64)>,
    /// Next seq due at the playout slot; `None` until `target` buffered.
    next_play: Option<u16>,
    /// Frames that arrived behind the play position or past the horizon.
    pub drops_late: u64,
    /// PLC outputs produced (each tick with a gap at the play position).
    pub plc: u64,
}

/// What one 20 ms playout tick produced.
#[derive(Debug)]
pub enum Playout {
    /// Still filling to `jitter_target`; nothing due yet.
    Buffering,
    /// A gap at the play position — caller feeds the decoder PLC.
    Plc,
    /// The frame due now.
    Frame(Vec<u8>),
}

impl JitterBuf {
    pub fn new(target: usize, max: usize) -> Self {
        Self {
            target: target.max(1),
            max: max.max(1),
            map: BTreeMap::new(),
            anchor: None,
            next_play: None,
            drops_late: 0,
            plc: 0,
        }
    }

    fn unwrap(&mut self, seq: u16) -> i64 {
        let anchor = match self.anchor {
            Some(a) => a,
            None => {
                let a = (seq, seq as i64);
                self.anchor = Some(a);
                a
            }
        };
        anchor.1 + seq_dist(seq, anchor.0) as i64
    }

    /// `false` when the frame is dropped (late behind play position or
    /// buffer at the horizon cap) — `drops_late` increments.
    pub fn insert(&mut self, seq: u16, payload: Vec<u8>) -> bool {
        if let Some(np) = self.next_play {
            if seq_dist(seq, np) < 0 {
                self.drops_late += 1;
                return false;
            }
        }
        if self.map.len() >= self.max {
            self.drops_late += 1;
            return false;
        }
        let key = self.unwrap(seq);
        if self.map.contains_key(&key) {
            // Re-send dup: TCP can't produce it, but a dup is not late.
            return true;
        }
        self.map.insert(key, payload);
        true
    }

    /// One 20 ms playout slot.
    pub fn tick(&mut self) -> Playout {
        let np = match self.next_play {
            Some(np) => np,
            None => {
                if self.map.len() < self.target {
                    return Playout::Buffering;
                }
                let (&key, _) = self.map.iter().next().expect("nonempty");
                let seq = (key & 0xffff) as u16;
                self.next_play = Some(seq);
                seq
            }
        };
        let key = self.unwrap(np);
        self.next_play = Some(np.wrapping_add(1));
        match self.map.remove(&key) {
            Some(pkt) => Playout::Frame(pkt),
            None => {
                self.plc += 1;
                Playout::Plc
            }
        }
    }

    /// Test-facing inspection (#[path] test includes).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn buffered(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn playing(&self) -> bool {
        self.next_play.is_some()
    }
}

/// §6 outbound media queue: bounded `CALL_FRAME_QUEUE`; push drops the
/// OLDEST frame (isochronous — a late frame is worse than none) and the
/// caller counts `drops_out`.
pub struct FrameQueue {
    q: Mutex<VecDeque<Vec<u8>>>,
    cap: usize,
    wake: Notify,
}

impl FrameQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            q: Mutex::new(VecDeque::new()),
            cap: cap.max(1),
            wake: Notify::new(),
        }
    }

    /// Enqueue; when full the oldest is evicted first. Returns `true` when
    /// an oldest frame was dropped (caller counts `drops_out`).
    pub fn push(&self, pkt: Vec<u8>) -> bool {
        let mut dropped = false;
        {
            let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= self.cap {
                q.pop_front();
                dropped = true;
            }
            q.push_back(pkt);
        }
        self.wake.notify_one();
        dropped
    }

    pub fn pop(&self) -> Option<Vec<u8>> {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    pub fn len(&self) -> usize {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Wait for `push` to wake the sender (bounded by `dur` so the send
    /// loop also re-checks `stop`).
    pub async fn wait(&self, dur: Duration) {
        let _ = tokio::time::timeout(dur, self.wake.notified()).await;
    }
}

/// Shared runtime for one active call: the two pump tasks plus the inbound
/// `CallFrame` path all rendezvous here. Lives inside `Call` in core.rs.
pub struct CallRt {
    /// Mic → send-loop queue (bounded, drop-oldest).
    pub out_q: FrameQueue,
    /// Inbound `CallFrame` payload → playout ordering.
    pub jitter: Mutex<JitterBuf>,
    /// Local-only mute (§7): gates capture, emits no wire signal.
    pub muted: AtomicBool,
    /// Set by `end_call`; every pump exits on it.
    pub stop: AtomicBool,
    pub started: Instant,
    /// Outbound seq source (per-direction u16 wraparound).
    tx_seq: AtomicU64,
    /// Frames actually handed to the session (post-seal, on-wire).
    pub tx_frames: AtomicU64,
    /// Inbound frames accepted into the jitter buffer.
    pub rx_frames: AtomicU64,
    /// PLC invocations (jitter gaps at playout).
    pub plc: AtomicU64,
    /// Outbound drops (queue full / session couldn't take it).
    pub drops_out: AtomicU64,
    /// Inbound drops (behind play position / horizon cap).
    pub drops_late: AtomicU64,
    /// Last inbound CallFrame observed (media keepalive/dead clocks).
    pub last_rx: Mutex<Option<Instant>>,
    pub quality: Mutex<Quality>,
    /// Quality-update emit throttle (≤1/5 s per spec §5).
    pub last_quality_emit: Mutex<Instant>,
}

impl CallRt {
    pub fn new(caps: &CallCaps) -> Self {
        Self {
            out_q: FrameQueue::new(caps.frame_queue),
            jitter: Mutex::new(JitterBuf::new(caps.jitter_target, caps.jitter_max)),
            muted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            started: Instant::now(),
            tx_seq: AtomicU64::new(0),
            tx_frames: AtomicU64::new(0),
            rx_frames: AtomicU64::new(0),
            plc: AtomicU64::new(0),
            drops_out: AtomicU64::new(0),
            drops_late: AtomicU64::new(0),
            last_rx: Mutex::new(None),
            quality: Mutex::new(Quality::Good),
            last_quality_emit: Mutex::new(Instant::now()),
        }
    }

    pub fn next_seq(&self) -> u16 {
        (self.tx_seq.fetch_add(1, Ordering::Relaxed) & 0xffff) as u16
    }

    /// Sender call-relative ms — informational ordering aid (seq orders).
    pub fn ts_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    /// Inbound `CallFrame` after peer/pin/size checks: count + buffer.
    pub fn rx_frame(&self, seq: u16, payload: Vec<u8>) {
        *self.last_rx.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let ok = self
            .jitter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seq, payload);
        if ok {
            self.rx_frames.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Pull jitter-buffer counters the engine reports in `call.state`.
    pub fn sync_drops(&self) {
        let j = self.jitter.lock().unwrap_or_else(|e| e.into_inner());
        self.drops_late.store(j.drops_late, Ordering::Relaxed);
        self.plc.store(j.plc, Ordering::Relaxed);
    }
}

// ---------- Opus codec (audiopus; FFI lives inside the crate) -----------------

/// One encoder per direction per call. `Application::Voip`, VBR on, DTX
/// off (§3: DTX + silence suppression makes drop-vs-silence ambiguous),
/// complexity 5, bitrate from negotiated `CallCaps::bitrate`.
pub struct OpusEnc {
    inner: audiopus::coder::Encoder,
}

impl OpusEnc {
    pub fn new(bitrate: i32) -> Result<Self, audiopus::Error> {
        let mut enc = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Mono,
            audiopus::Application::Voip,
        )?;
        enc.set_bitrate(audiopus::Bitrate::BitsPerSecond(
            bitrate.clamp(32_000, 64_000),
        ))?;
        enc.set_vbr(true)?;
        enc.set_complexity(5)?;
        enc.set_dtx(false)?;
        Ok(Self { inner: enc })
    }

    /// 960 f32 samples → one Opus packet, ≤ `MAX_OPUS_PACKET` enforced
    /// (§3: over-cap payloads drop locally, never reach the wire).
    pub fn encode(&self, pcm: &[f32; FRAME_SAMPLES]) -> Result<Vec<u8>, audiopus::Error> {
        let mut out = [0u8; MAX_OPUS_PACKET];
        let n = self.inner.encode_float(pcm, &mut out)?;
        Ok(out[..n].to_vec())
    }
}

pub struct OpusDec {
    inner: audiopus::coder::Decoder,
}

impl OpusDec {
    pub fn new() -> Result<Self, audiopus::Error> {
        Ok(Self {
            inner: audiopus::coder::Decoder::new(
                audiopus::SampleRate::Hz48000,
                audiopus::Channels::Mono,
            )?,
        })
    }

    /// `None` = packet loss → PLC (§6). Fills `out` with 20 ms of audio.
    pub fn decode(
        &mut self,
        pkt: Option<&[u8]>,
        out: &mut [f32; FRAME_SAMPLES],
    ) -> Result<(), audiopus::Error> {
        let sig = audiopus::MutSignals::try_from(&mut out[..])?;
        let packet = pkt.map(audiopus::packet::Packet::try_from).transpose()?;
        self.inner.decode_float(packet, sig, false)?;
        Ok(())
    }
}

// ---------- device seam -------------------------------------------------------

/// Mic capture: fill 960 f32 samples per call. May block briefly waiting
/// for real hardware; must return within ~2× frame duration.
pub trait MicIo: Send {
    fn read(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> io::Result<()>;
}

/// Speaker playback: accept 960 f32 samples per call. Non-blocking (drop /
/// buffer internally; real-time deadlines are the device's own).
pub trait SpeakerIo: Send {
    fn write(&mut self, pcm: &[f32; FRAME_SAMPLES]) -> io::Result<()>;
}

/// One duplex pair per call.
pub struct Devices {
    pub mic: Box<dyn MicIo>,
    pub speaker: Box<dyn SpeakerIo>,
}

/// Headless device: silence in, sink out. The engine still runs the full
/// pump (encode → wire → jitter → decode) — remote side hears silence.
pub struct NullMic;
pub struct NullSpeaker;

impl MicIo for NullMic {
    fn read(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> io::Result<()> {
        out.fill(0.0);
        Ok(())
    }
}

impl SpeakerIo for NullSpeaker {
    fn write(&mut self, _pcm: &[f32; FRAME_SAMPLES]) -> io::Result<()> {
        Ok(())
    }
}

pub fn null_devices() -> Devices {
    Devices {
        mic: Box::new(NullMic),
        speaker: Box::new(NullSpeaker),
    }
}

/// Loopback pair for tests/headless: speaker output feeds the same
/// device's mic queue (bounded — drop-oldest past ~1 s). `played`
/// exposes what the decoder wrote for assertions.
#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone)]
pub struct Loopback {
    q: std::sync::Arc<Mutex<VecDeque<f32>>>,
}

#[cfg(test)]
#[allow(dead_code)]
impl Loopback {
    pub fn devices() -> (Devices, Loopback) {
        let lb = Loopback {
            q: std::sync::Arc::new(Mutex::new(VecDeque::new())),
        };
        (
            Devices {
                mic: Box::new(LoopMic { lb: lb.clone() }),
                speaker: Box::new(LoopSpeaker { lb: lb.clone() }),
            },
            lb,
        )
    }

    /// f32 samples written by the playback path (≤ ~1 s retained).
    pub fn played(&self) -> Vec<f32> {
        self.q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect()
    }

    /// Samples currently queued for the mic path to consume.
    pub fn pending(&self) -> usize {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
#[allow(dead_code)]
struct LoopMic {
    lb: Loopback,
}

#[cfg(test)]
#[allow(dead_code)]
struct LoopSpeaker {
    lb: Loopback,
}

#[cfg(test)]
#[allow(dead_code)]
impl MicIo for LoopMic {
    fn read(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> io::Result<()> {
        let mut q = self.lb.q.lock().unwrap_or_else(|e| e.into_inner());
        for s in out.iter_mut() {
            *s = q.pop_front().unwrap_or(0.0);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(dead_code)]
impl SpeakerIo for LoopSpeaker {
    fn write(&mut self, pcm: &[f32; FRAME_SAMPLES]) -> io::Result<()> {
        const CAP: usize = FRAME_SAMPLES * 50; // ~1 s
        let mut q = self.lb.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() + FRAME_SAMPLES > CAP {
            let drop_n = (q.len() + FRAME_SAMPLES - CAP).min(q.len());
            q.drain(..drop_n);
        }
        q.extend(pcm.iter().copied());
        Ok(())
    }
}

/// Default device factory: cpal under the `calls` feature (falling back to
/// Null on open failure — headless hosts still get a working call), Null
/// otherwise. Injectable on `Core` for tests.
#[cfg(feature = "calls")]
pub fn default_devices() -> Devices {
    match cpal_devices() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(err = %e, "cpal device open failed; calls run on NullMedia");
            null_devices()
        }
    }
}

#[cfg(not(feature = "calls"))]
pub fn default_devices() -> Devices {
    null_devices()
}

/// cpal-backed duplex. Default input/output devices; non-f32 configs,
/// multi-channel (first channel / mono-sum downmix), and non-48 kHz rates
/// are adapted with a linear resampler — no DSP beyond that in v0 (§1).
#[cfg(feature = "calls")]
pub fn cpal_devices() -> Result<Devices, anyhow::Error> {
    use anyhow::anyhow;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};
    use std::sync::{Arc, Condvar};

    const RING_CAP: usize = FRAME_SAMPLES * 24; // ~480 ms

    struct InRing {
        q: Mutex<VecDeque<f32>>,
        cv: Condvar,
    }

    let host = cpal::default_host();
    let in_dev = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    let out_dev = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let in_cfg = in_dev.default_input_config()?;
    let out_cfg = out_dev.default_output_config()?;
    if in_cfg.sample_format() != SampleFormat::F32 || out_cfg.sample_format() != SampleFormat::F32 {
        return Err(anyhow!("non-f32 device config; unsupported in v0"));
    }
    let in_rate = in_cfg.sample_rate();
    let out_rate = out_cfg.sample_rate();
    let in_ch = in_cfg.channels() as usize;
    let out_ch = out_cfg.channels() as usize;
    let in_stream_cfg: StreamConfig = in_cfg.config();
    let out_stream_cfg: StreamConfig = out_cfg.config();

    // Input callback: mono-sum → resample → ring. Read side blocks on the
    // condvar for a frame; starved reads yield silence (real-time-safe).
    let in_ring = Arc::new(InRing {
        q: Mutex::new(VecDeque::with_capacity(RING_CAP)),
        cv: Condvar::new(),
    });
    let in_cb_ring = in_ring.clone();
    let ratio_in = in_rate as f64 / SAMPLE_RATE as f64;
    let mut resample_pos = 0.0f64;
    let mut resample_carry: VecDeque<f32> = VecDeque::new();
    let in_stream = in_dev.build_input_stream(
        in_stream_cfg,
        move |data: &[f32], _| {
            let mut mono: Vec<f32> = Vec::with_capacity(data.len() / in_ch.max(1) + 1);
            for frame in data.chunks(in_ch.max(1)) {
                mono.push(frame.iter().copied().sum::<f32>() / frame.len() as f32);
            }
            // Linear resample in_rate → 48 kHz into `resample_carry`.
            let mut ring = in_cb_ring.q.lock().unwrap_or_else(|e| e.into_inner());
            let mut src: VecDeque<f32> = std::mem::take(&mut resample_carry);
            src.extend(mono.iter().copied());
            if src.len() >= 2 {
                while (resample_pos as usize) + 1 < src.len() {
                    let i = resample_pos as usize;
                    let t = (resample_pos - i as f64) as f32;
                    let s = src[i] + (src[i + 1] - src[i]) * t;
                    if ring.len() < RING_CAP {
                        ring.push_back(s);
                    }
                    resample_pos += ratio_in;
                }
                let consumed = resample_pos as usize;
                src.drain(..consumed);
                resample_pos -= consumed as f64;
            }
            resample_carry = src;
            drop(ring);
            in_cb_ring.cv.notify_all();
        },
        |e| tracing::warn!(err = %e, "cpal input stream error"),
        None,
    )?;
    in_stream.play()?;

    // Output ring: 48 kHz mono; the callback upsamples+spreads to channels.
    let out_ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)));
    let out_cb_ring = out_ring.clone();
    let ratio_out = SAMPLE_RATE as f64 / out_rate as f64;
    let mut out_src: VecDeque<f32> = VecDeque::new();
    let mut out_pos = 0.0f64;
    let out_stream = out_dev.build_output_stream(
        out_stream_cfg,
        move |data: &mut [f32], _| {
            let frames = data.len() / out_ch.max(1);
            let need = ((frames as f64) * ratio_out).ceil() as usize + 2;
            let mut ring = out_cb_ring.lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..need.saturating_sub(out_src.len()) {
                match ring.pop_front() {
                    Some(s) => out_src.push_back(s),
                    None => break,
                }
            }
            drop(ring);
            for f in 0..frames {
                let i = out_pos as usize;
                let s = if i + 1 < out_src.len() {
                    let t = (out_pos - i as f64) as f32;
                    out_src[i] + (out_src[i + 1] - out_src[i]) * t
                } else {
                    0.0
                };
                for ch in 0..out_ch.max(1) {
                    data[f * out_ch + ch] = s;
                }
                out_pos += ratio_out;
            }
            let consumed = (out_pos as usize).min(out_src.len());
            out_src.drain(..consumed);
            out_pos -= consumed as f64;
        },
        |e| tracing::warn!(err = %e, "cpal output stream error"),
        None,
    )?;
    out_stream.play()?;

    struct CpalMic {
        ring: Arc<InRing>,
        _stream: cpal::Stream,
    }
    impl MicIo for CpalMic {
        fn read(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> io::Result<()> {
            let mut q = self.ring.q.lock().unwrap_or_else(|e| e.into_inner());
            // Bounded wait (~2× frame): starved hardware yields silence,
            // never hangs the pump.
            let wait = Duration::from_millis(FRAME_MS * 2);
            let deadline = Instant::now() + wait;
            while q.len() < FRAME_SAMPLES {
                let remain = deadline.saturating_duration_since(Instant::now());
                if remain.is_zero() {
                    out.fill(0.0);
                    return Ok(());
                }
                let (g, _t) = self
                    .ring
                    .cv
                    .wait_timeout(q, remain)
                    .unwrap_or_else(|e| e.into_inner());
                q = g;
            }
            for s in out.iter_mut() {
                *s = q.pop_front().unwrap_or(0.0);
            }
            Ok(())
        }
    }

    struct CpalSpeaker {
        ring: Arc<Mutex<VecDeque<f32>>>,
        _stream: cpal::Stream,
    }
    impl SpeakerIo for CpalSpeaker {
        fn write(&mut self, pcm: &[f32; FRAME_SAMPLES]) -> io::Result<()> {
            let mut q = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() + FRAME_SAMPLES > RING_CAP {
                let drop_n = (q.len() + FRAME_SAMPLES - RING_CAP).min(q.len());
                q.drain(..drop_n);
            }
            q.extend(pcm.iter().copied());
            Ok(())
        }
    }

    Ok(Devices {
        mic: Box::new(CpalMic {
            ring: in_ring,
            _stream: in_stream,
        }),
        speaker: Box::new(CpalSpeaker {
            ring: out_ring,
            _stream: out_stream,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_in_order() {
        let mut j = JitterBuf::new(2, 4);
        for s in 0..3u16 {
            assert!(j.insert(s, vec![s as u8]));
        }
        assert!(matches!(j.tick(), Playout::Frame(_)));
        assert!(matches!(j.tick(), Playout::Frame(_)));
        assert!(matches!(j.tick(), Playout::Frame(_)));
        assert_eq!(j.drops_late, 0);
    }

    #[test]
    fn jitter_gap_plc() {
        let mut j = JitterBuf::new(2, 4);
        j.insert(0, vec![0]);
        j.insert(2, vec![2]);
        assert!(matches!(j.tick(), Playout::Frame(_)));
        assert!(matches!(j.tick(), Playout::Plc));
        assert!(matches!(j.tick(), Playout::Frame(_)));
        assert_eq!(j.plc, 1);
        // Late arrival of seq 1 → behind play position.
        assert!(!j.insert(1, vec![1]));
        assert_eq!(j.drops_late, 1);
    }

    #[test]
    fn queue_drop_oldest() {
        let q = FrameQueue::new(2);
        assert!(!q.push(vec![1]));
        assert!(!q.push(vec![2]));
        assert!(q.push(vec![3])); // dropped [1]
        assert_eq!(q.pop().unwrap(), vec![2]);
        assert_eq!(q.pop().unwrap(), vec![3]);
    }
}
