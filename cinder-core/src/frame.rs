use crate::proto::{caps, ErrorCode};
use bincode::Options as _;
use bytes::{Buf, BufMut, BytesMut};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use tokio_util::codec::{Decoder, Encoder};

/// bincode 1.3 legacy configuration (§2): fixint, little-endian — plus
/// reject_trailing_bytes, which the spec promotes from allowed to fatal.
pub fn wire_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_little_endian()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .with_no_limit()
}

pub fn wire_serialize<T: Serialize>(msg: &T) -> Result<Vec<u8>, bincode::Error> {
    wire_options().serialize(msg)
}

pub fn wire_deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, bincode::Error> {
    wire_options().deserialize(bytes)
}

pub fn wire_serialized_size<T: Serialize>(value: &T) -> Result<u64, bincode::Error> {
    wire_options().serialized_size(value)
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("oversize frame: {len} > {cap}")]
    Oversize { len: u32, cap: u32 },
    #[error("protocol violation: {0}")]
    Violation(&'static str),
    #[error("malformed frame")]
    Malformed,
}

impl FrameError {
    pub fn code(&self) -> ErrorCode {
        match self {
            FrameError::Oversize { .. } => ErrorCode::OversizeFrame,
            FrameError::Violation(_) | FrameError::Malformed => ErrorCode::ProtocolViolation,
            FrameError::Io(_) => ErrorCode::ProtocolViolation,
        }
    }
}

/// Length-prefixed bincode codec: `[u32 BE len ‖ frame]`. The declared len is
/// validated against `max_frame` BEFORE any buffer growth (§2): `len == 0` and
/// `len > max_frame` are both fatal. `max_frame` is phase-aware — start at
/// `MAX_FRAME_HELLO` and call `set_max_frame(MAX_FRAME_TRANSPORT)` once the
/// peer's `Hello` has been accepted.
pub struct FrameCodec<T> {
    max_frame: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> FrameCodec<T> {
    pub fn new(max_frame: u32) -> Self {
        FrameCodec {
            max_frame,
            _marker: PhantomData,
        }
    }

    pub fn pre_hello() -> Self {
        Self::new(caps::MAX_FRAME_HELLO as u32)
    }

    pub fn transport() -> Self {
        Self::new(caps::MAX_FRAME_TRANSPORT as u32)
    }

    pub fn set_max_frame(&mut self, max_frame: u32) {
        self.max_frame = max_frame;
    }
}

impl<T: DeserializeOwned> Decoder for FrameCodec<T> {
    type Item = T;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<T>, FrameError> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
        if len == 0 {
            return Err(FrameError::Violation("zero-length frame"));
        }
        if len > self.max_frame {
            return Err(FrameError::Oversize {
                len,
                cap: self.max_frame,
            });
        }
        let len = len as usize;
        if src.len() < 4 + len {
            // len already validated ≤ cap, so this reservation is bounded.
            src.reserve(4 + len - src.len());
            return Ok(None);
        }
        src.advance(4);
        let payload = src.split_to(len);
        wire_deserialize(&payload)
            .map(Some)
            .map_err(|_| FrameError::Malformed)
    }
}

impl<T: Serialize> Encoder<T> for FrameCodec<T> {
    type Error = FrameError;

    fn encode(&mut self, item: T, dst: &mut BytesMut) -> Result<(), FrameError> {
        let payload = wire_serialize(&item).map_err(|_| FrameError::Malformed)?;
        if payload.is_empty() {
            return Err(FrameError::Violation("zero-length frame"));
        }
        if payload.len() > self.max_frame as usize {
            return Err(FrameError::Oversize {
                len: payload.len() as u32,
                cap: self.max_frame,
            });
        }
        dst.reserve(4 + payload.len());
        dst.put_u32(payload.len() as u32);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}
