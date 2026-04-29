//! MSB-first bit packing: the first bit emitted is the MSB of the first output byte.
//! `BitReader` interprets input the same way (`data[0] >> 7` first).

use crate::error::{BitIoError, BitIoResult};

/// Read bits from a byte slice (borrowed, no internal cursor beyond `bit_pos`).
#[derive(Debug, Clone, Copy)]
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    pub fn total_bits(&self) -> usize {
        self.data.len().saturating_mul(8)
    }

    pub fn bits_remaining(&self) -> usize {
        self.total_bits().saturating_sub(self.bit_pos)
    }

    pub fn position_bits(&self) -> usize {
        self.bit_pos
    }

    pub(crate) fn full_slice(&self) -> &'a [u8] {
        self.data
    }

    /// Advance the cursor by `bytes` whole bytes; errors if not currently byte-aligned.
    pub(crate) fn advance_bytes(&mut self, bytes: usize) -> BitIoResult<()> {
        if self.bit_pos % 8 != 0 {
            return Err(BitIoError::InvalidVarint);
        }
        let add = bytes.saturating_mul(8);
        let next = self.bit_pos.saturating_add(add);
        if next > self.total_bits() {
            return Err(BitIoError::UnexpectedEof);
        }
        self.bit_pos = next;
        Ok(())
    }

    /// Read `n` bits (1..=64) MSB-first; returned value uses only the low `n` bits.
    pub fn read(&mut self, n: u8) -> BitIoResult<u64> {
        if n == 0 || n > 64 {
            return Err(BitIoError::InvalidBitCount(n));
        }
        let need = usize::from(n);
        if self.bits_remaining() < need {
            return Err(BitIoError::UnexpectedEof);
        }
        let mut out: u64 = 0;
        for _ in 0..need {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = 7 - (self.bit_pos % 8);
            let bit = u64::from((self.data[byte_idx] >> bit_idx) & 1);
            out = (out << 1) | bit;
            self.bit_pos += 1;
        }
        Ok(out)
    }

    pub fn skip(&mut self, n: usize) -> BitIoResult<()> {
        if self.bits_remaining() < n {
            return Err(BitIoError::UnexpectedEof);
        }
        self.bit_pos += n;
        Ok(())
    }

    /// Consume zero padding bits until the next byte boundary.
    pub fn align_byte(&mut self) -> BitIoResult<()> {
        let mis = self.bit_pos % 8;
        if mis != 0 {
            self.skip(8 - mis)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            out: Vec::with_capacity(cap),
            acc: 0,
            nbits: 0,
        }
    }

    /// Write the low `n` bits of `value` (1..=64), MSB of this run first in the bitstream.
    pub fn write(&mut self, n: u8, value: u64) -> BitIoResult<()> {
        if n == 0 || n > 64 {
            return Err(BitIoError::InvalidBitCount(n));
        }
        let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
        if value & !mask != 0 {
            return Err(BitIoError::ValueOutOfRange { bits: n });
        }
        let mut bits_left = u32::from(n);
        while bits_left > 0 {
            let room = 8usize.saturating_sub(self.nbits as usize);
            let take = room.min(bits_left as usize);
            if take == 0 {
                return Err(BitIoError::InvalidBitCount(n));
            }
            let shift = bits_left - take as u32;
            let chunk = (value >> shift) & ((1u64 << take) - 1);
            self.acc = (self.acc << (take as u8)) | chunk;
            self.nbits += take as u8;
            bits_left -= take as u32;
            while self.nbits >= 8 {
                self.nbits -= 8;
                let byte = ((self.acc >> self.nbits) & 0xFF) as u8;
                self.out.push(byte);
                self.acc &= (1u64 << self.nbits) - 1;
            }
        }
        Ok(())
    }

    /// Pad with zeros to a byte boundary and return written bytes.
    pub fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            let _ = self.write(pad, 0);
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_reader_msb_first_within_byte() {
        let data = [0b1010_0100u8];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read(3).unwrap(), 0b001);
        assert_eq!(r.read(2).unwrap(), 0b00);
        assert_eq!(r.bits_remaining(), 0);
    }

    #[test]
    fn bit_writer_matches_reader() {
        let mut w = BitWriter::new();
        w.write(3, 0b101).unwrap();
        w.write(5, 0b00011).unwrap();
        let bytes = w.finish();
        assert_eq!(bytes, &[0b1010_0011]);
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(8).unwrap(), 0b1010_0011_u64);
    }

    #[test]
    fn read_past_end_errors() {
        let mut r = BitReader::new(&[0xff]);
        assert!(r.read(9).is_err());
    }
}
