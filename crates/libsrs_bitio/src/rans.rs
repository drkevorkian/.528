//! 32-bit **rANS** with fixed `SCALE = 1 << 12` (4096). Frequency tables must sum to exactly `SCALE`.
//! Stream layout: **4-byte little-endian state** (post-encode flush), then
//! renormalization bytes in **decoder read order** (the encoder collects LSBs
//! while walking symbols in reverse and reverses that list once before layout).

use crate::error::{BitIoError, BitIoResult};

/// Renormalize threshold (ryg_rans-style `RANS_L`).
pub const RANS_L: u32 = 1 << 23;

pub const RANS_SCALE_BITS: u32 = 12;

pub const RANS_SCALE: u32 = 1 << RANS_SCALE_BITS;

pub const RANS_MAX_ALPHABET: usize = RANS_SCALE as usize;

#[derive(Debug, Clone)]
pub struct RansModel {
    freqs: Vec<u32>,
    cumulative: Vec<u32>,
}

impl RansModel {
    pub fn try_from_freqs(freqs: Vec<u32>) -> BitIoResult<Self> {
        if freqs.is_empty() {
            return Err(BitIoError::Rans("empty alphabet".into()));
        }
        if freqs.len() > RANS_MAX_ALPHABET {
            return Err(BitIoError::Rans("alphabet too large".into()));
        }
        let mut sum: u32 = 0;
        for &f in &freqs {
            if f == 0 {
                return Err(BitIoError::Rans("zero frequency".into()));
            }
            sum = sum
                .checked_add(f)
                .ok_or_else(|| BitIoError::Rans("freq sum overflow".into()))?;
        }
        if sum != RANS_SCALE {
            return Err(BitIoError::Rans(format!(
                "frequencies must sum to {} (got {})",
                RANS_SCALE, sum
            )));
        }
        let mut cumulative = Vec::with_capacity(freqs.len() + 1);
        cumulative.push(0);
        for &f in &freqs {
            let next = *cumulative.last().expect("built non-empty") + f;
            cumulative.push(next);
        }
        debug_assert_eq!(*cumulative.last().expect("len"), RANS_SCALE);
        Ok(Self { freqs, cumulative })
    }

    pub fn alphabet_size(&self) -> usize {
        self.freqs.len()
    }

    /// Uniform frequencies for `n` symbols (`n` must divide `RANS_SCALE`).
    pub fn uniform(n: usize) -> BitIoResult<Self> {
        if n == 0 || n > RANS_MAX_ALPHABET {
            return Err(BitIoError::Rans("bad alphabet size".into()));
        }
        let n_u32 = n as u32;
        if RANS_SCALE % n_u32 != 0 {
            return Err(BitIoError::Rans(
                "scale not divisible by alphabet size".into(),
            ));
        }
        let f = RANS_SCALE / n_u32;
        Self::try_from_freqs(vec![f; n])
    }

    fn symbol_for_slot(&self, slot: u32) -> BitIoResult<usize> {
        if slot >= RANS_SCALE {
            return Err(BitIoError::Rans("slot out of range".into()));
        }
        let idx = self
            .cumulative
            .partition_point(|&c| c <= slot)
            .checked_sub(1)
            .ok_or_else(|| BitIoError::Rans("invalid cumulative table".into()))?;
        if idx >= self.freqs.len() {
            return Err(BitIoError::Rans("symbol lookup failed".into()));
        }
        Ok(idx)
    }
}

pub fn rans_encode(model: &RansModel, symbols: &[usize]) -> BitIoResult<Vec<u8>> {
    let mut state = RANS_L;
    let mut renorm = Vec::new();

    for &sym in symbols.iter().rev() {
        if sym >= model.freqs.len() {
            return Err(BitIoError::Rans("symbol out of range".into()));
        }
        let freq = model.freqs[sym];
        let start = model.cumulative[sym];
        let max = ((RANS_L as u64 >> RANS_SCALE_BITS) << 8)
            .checked_mul(u64::from(freq))
            .ok_or_else(|| BitIoError::Rans("renorm bound overflow".into()))?;
        while u64::from(state) >= max {
            renorm.push(state as u8);
            state >>= 8;
        }
        let q = state / freq;
        let r = state % freq;
        state = q
            .checked_shl(RANS_SCALE_BITS)
            .and_then(|x| x.checked_add(r))
            .and_then(|x| x.checked_add(start))
            .ok_or_else(|| BitIoError::Rans("state overflow".into()))?;
    }

    // ryg rANS writes renormalization bytes while walking *backward* in the output
    // buffer; our Vec collected them in reverse of the order the decoder must read
    // after the 4-byte flushed state, so reverse once before layout.
    renorm.reverse();

    let mut out = Vec::with_capacity(4 + renorm.len());
    out.extend_from_slice(&state.to_le_bytes());
    out.extend_from_slice(&renorm);
    Ok(out)
}

/// Decode exactly `num_symbols` symbols. `decode_step_budget` caps total inner renorm loop iterations
/// (each absorbed byte counts as one) to bound hostile inputs.
pub fn rans_decode(
    model: &RansModel,
    data: &[u8],
    num_symbols: usize,
    decode_step_budget: usize,
) -> BitIoResult<Vec<usize>> {
    if data.len() < 4 {
        return Err(BitIoError::Rans("truncated stream".into()));
    }

    let mut state = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let mut idx = 4usize;
    let mut out = Vec::with_capacity(num_symbols);
    let mut steps = 0usize;

    for _ in 0..num_symbols {
        let slot = state & (RANS_SCALE - 1);
        let sym = model.symbol_for_slot(slot)?;
        let start = model.cumulative[sym];
        let freq = model.freqs[sym];
        state = freq
            .wrapping_mul(state >> RANS_SCALE_BITS)
            .wrapping_add(slot.wrapping_sub(start));

        while state < RANS_L {
            if idx >= data.len() {
                return Err(BitIoError::Rans("truncated renorm".into()));
            }
            steps += 1;
            if steps > decode_step_budget {
                return Err(BitIoError::RansDecodeBudget);
            }
            state = (state << 8) | u32::from(data[idx]);
            idx += 1;
        }
        out.push(sym);
    }

    if idx != data.len() {
        return Err(BitIoError::Rans("trailing bytes after rANS stream".into()));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rans_roundtrip_uniform() {
        let model = RansModel::uniform(4).unwrap();
        let symbols: Vec<usize> = vec![0, 2, 3, 1, 0, 2];
        let enc = rans_encode(&model, &symbols).unwrap();
        let dec = rans_decode(&model, &enc, symbols.len(), enc.len().saturating_mul(8)).unwrap();
        assert_eq!(dec, symbols);
    }

    #[test]
    fn rans_long_uniform_256_matches() {
        let model = RansModel::uniform(256).unwrap();
        let symbols: Vec<usize> = (0..240).map(|i| (i * 13 + 7) % 256).collect();
        let enc = rans_encode(&model, &symbols).unwrap();
        let dec = rans_decode(&model, &enc, symbols.len(), enc.len().saturating_mul(32)).unwrap();
        assert_eq!(dec, symbols);
    }

    #[test]
    fn rans_rejects_bad_table() {
        assert!(RansModel::try_from_freqs(vec![1, 2]).is_err());
    }

    #[test]
    fn rans_single_symbol_many() {
        let model = RansModel::uniform(1).unwrap();
        let symbols = vec![0_usize; 100];
        let enc = rans_encode(&model, &symbols).unwrap();
        let dec = rans_decode(&model, &enc, symbols.len(), enc.len().saturating_mul(8)).unwrap();
        assert_eq!(dec, symbols);
    }

    #[test]
    fn rans_rejects_trailing_garbage() {
        let model = RansModel::uniform(4).unwrap();
        let symbols: Vec<usize> = vec![0, 1, 2, 3];
        let mut enc = rans_encode(&model, &symbols).unwrap();
        enc.push(0xff);
        assert!(rans_decode(&model, &enc, symbols.len(), enc.len().saturating_mul(8)).is_err());
    }
}
