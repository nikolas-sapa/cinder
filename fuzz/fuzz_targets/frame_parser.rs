#![no_main]

use bytes::BytesMut;
use cinder_core::frame::{wire_deserialize, FrameCodec};
use cinder_core::proto::{ClientMsg, HandshakeMsg, ServerMsg};
use libfuzzer_sys::fuzz_target;
use serde::de::DeserializeOwned;
use tokio_util::codec::Decoder;

// 0 forces every nonzero len down the Oversize path; 8210 is the transport
// ciphertext boundary between the 4 KiB and 16 KiB caps.
const CAPS: [u32; 4] = [0, 4096, 8210, 16384];

// Boundary declared-lens: cap edges ±1 plus absurd values. Caps ≤ 16384
// guarantee these error pre-allocation rather than reserve huge buffers.
const LENS: [u32; 12] = [
    0,
    1,
    31,
    32,
    4095,
    4096,
    4097,
    8209,
    8210,
    8211,
    16384,
    16385,
];

fn stream<T: DeserializeOwned>(max_frame: u32, data: &[u8]) {
    let mut codec = FrameCodec::<T>::new(max_frame);
    let mut buf = BytesMut::new();
    let mut i = 0usize;
    'outer: while i < data.len() {
        let chunk = 1 + usize::from(data[i]) % 512;
        let end = i.saturating_add(chunk).min(data.len());
        buf.extend_from_slice(&data[i..end]);
        i = end;
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => break 'outer,
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let _ = wire_deserialize::<ClientMsg>(data);
    let _ = wire_deserialize::<ServerMsg>(data);
    let _ = wire_deserialize::<HandshakeMsg>(data);

    for &cap in &CAPS {
        stream::<ClientMsg>(cap, data);
        stream::<ServerMsg>(cap, data);
    }

    let head = &data[..data.len().min(8192)];
    for &len in &LENS {
        for &cap in &CAPS {
            let mut codec = FrameCodec::<ClientMsg>::new(cap);
            let mut buf = BytesMut::with_capacity(4 + head.len());
            buf.extend_from_slice(&len.to_be_bytes());
            buf.extend_from_slice(head);
            let _ = codec.decode(&mut buf);
        }
    }

    for &len in &[u32::MAX, u32::MAX - 1, 0x8000_0000] {
        let mut codec = FrameCodec::<ClientMsg>::new(16384);
        let mut buf = BytesMut::from(&len.to_be_bytes()[..]);
        let _ = codec.decode(&mut buf);
    }
});
