use crate::error::Error;
use crate::proto::caps;

/// §2 padding: `plaintext unit = [u16 BE pt_len ‖ block]` where
/// `block = [pt ‖ 0x00…]` and `block_size = max(256, next_power_of_two(pt_len))`.
/// The u16 length sits OUTSIDE the power-of-2 bucket so pt_len = 8192 is
/// encodable inside the 8192 bucket; max unit size is 2 + 8192 = 8194 B
/// (max ciphertext 8210 B with the 16 B AEAD tag).
pub fn padded_len(pt_len: usize) -> usize {
    2 + pt_len.next_power_of_two().max(caps::PAD_MIN)
}

pub fn encode_plaintext(plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    if plaintext.len() > caps::MAX_PLAINTEXT {
        return Err(Error::Padding);
    }
    let total = padded_len(plaintext.len());
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(plaintext.len() as u16).to_be_bytes());
    out.extend_from_slice(plaintext);
    out.resize(total, 0);
    Ok(out)
}

/// Strip a padded unit. A pt_len whose implied block size does not match the
/// unit length, or any nonzero padding byte, is an integrity failure — fatal
/// to the session per §11.
pub fn decode_plaintext(unit: &[u8]) -> Result<&[u8], Error> {
    if unit.len() < 2 {
        return Err(Error::Padding);
    }
    let pt_len = u16::from_be_bytes([unit[0], unit[1]]) as usize;
    if pt_len > caps::MAX_PLAINTEXT || unit.len() != padded_len(pt_len) {
        return Err(Error::Padding);
    }
    let (body, pad) = unit[2..].split_at(pt_len);
    if pad.iter().any(|b| *b != 0) {
        return Err(Error::Padding);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // F4 — plaintext cap.
    #[test]
    fn f4_plaintext_cap() {
        assert!(encode_plaintext(&vec![0u8; caps::MAX_PLAINTEXT + 1]).is_err());
        assert!(encode_plaintext(&vec![0u8; caps::MAX_PLAINTEXT]).is_ok());
    }
}
