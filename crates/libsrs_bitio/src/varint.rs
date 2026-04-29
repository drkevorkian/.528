//! Unsigned LEB128 and signed zigzag; decoder is **capped** at 10 bytes (`u64`) / reads until continuation clears.

use crate::bit_io::BitReader;
use crate::error::{BitIoError, BitIoResult};

pub const MAX_U64_VARINT_BYTES: usize = 10;

/// Encode unsigned varint to `out` (LEB128).
pub fn encode_u64_varint_into(out: &mut Vec<u8>, mut value: u64) -> BitIoResult<()> {
    let start = out.len();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if out.len().saturating_sub(start) > MAX_U64_VARINT_BYTES {
            return Err(BitIoError::VarintTooLong);
        }
        if value == 0 {
            break;
        }
    }
    Ok(())
}

pub fn decode_u64_varint(bytes: &[u8]) -> BitIoResult<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in bytes.iter().enumerate() {
        if i >= MAX_U64_VARINT_BYTES {
            return Err(BitIoError::InvalidVarint);
        }
        let digit = (byte & 0x7F) as u64;
        if shift >= 63 && digit > 1 {
            return Err(BitIoError::InvalidVarint);
        }
        result |= digit << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
    }
    Err(BitIoError::InvalidVarint)
}

#[inline]
fn zigzag_i64(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

#[inline]
fn unzigzag_i64(u: u64) -> i64 {
    ((u >> 1) as i64) ^ (-((u & 1) as i64))
}

pub fn encode_i64_varint_into(out: &mut Vec<u8>, value: i64) -> BitIoResult<()> {
    encode_u64_varint_into(out, zigzag_i64(value))
}

pub fn decode_i64_varint(bytes: &[u8]) -> BitIoResult<(i64, usize)> {
    let (u, n) = decode_u64_varint(bytes)?;
    Ok((unzigzag_i64(u), n))
}

/// Read one LEB128 `u64` from a **byte-aligned** `BitReader` cursor.
pub fn read_u64_varint_from_bits(r: &mut BitReader<'_>) -> BitIoResult<u64> {
    if r.position_bits() % 8 != 0 {
        return Err(BitIoError::InvalidVarint);
    }
    let start = r.position_bits() / 8;
    let slice = r
        .full_slice()
        .get(start..)
        .ok_or(BitIoError::UnexpectedEof)?;
    let (v, consumed) = decode_u64_varint(slice)?;
    r.advance_bytes(consumed)?;
    Ok(v)
}

/// Read one zigzag `i64` from a byte-aligned cursor.
pub fn read_i64_varint_from_bits(r: &mut BitReader<'_>) -> BitIoResult<i64> {
    let u = read_u64_varint_from_bits(r)?;
    Ok(unzigzag_i64(u))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_u64() {
        for &v in &[0u64, 1, 127, 128, 300, u64::MAX] {
            let mut buf = Vec::new();
            encode_u64_varint_into(&mut buf, v).unwrap();
            let (d, n) = decode_u64_varint(&buf).unwrap();
            assert_eq!(d, v);
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn varint_roundtrip_i64() {
        for &v in &[0i64, -1, i64::MIN, i64::MAX] {
            let mut buf = Vec::new();
            encode_i64_varint_into(&mut buf, v).unwrap();
            let (d, n) = decode_i64_varint(&buf).unwrap();
            assert_eq!(d, v);
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn varint_overflow_rejected() {
        let bad = [
            0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02,
        ];
        assert!(decode_u64_varint(&bad).is_err());
    }
}
