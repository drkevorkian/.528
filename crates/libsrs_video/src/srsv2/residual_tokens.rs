//! Zigzag AC tokenization + static rANS symbolset for SRSV2 residual blocks (experimental).

use libsrs_bitio::{rans_decode, rans_encode, BitIoError, RansModel, RANS_SCALE};

use super::dct::ZIGZAG;
use super::error::SrsV2Error;

/// Maximum absolute quantized AC coefficient representable in the static rANS alphabet.
pub const MAX_RANS_ABS_COEFF: i16 = 127;

/// Zigzag-order AC positions only (`ZIGZAG[1..]`).
pub const AC_POSITIONS: usize = 63;

/// Hard cap on rANS symbols emitted per 8×8 block (hostile-input bound).
pub const MAX_SYMBOLS_PER_BLOCK: usize = 384;

pub fn zigzag_unsigned(signed: i16) -> Option<usize> {
    let v = signed;
    if v == 0 {
        return None;
    }
    if v.unsigned_abs() > MAX_RANS_ABS_COEFF as u16 {
        return None;
    }
    Some(if v > 0 {
        2 * v as usize - 1
    } else {
        (-2 * v as isize) as usize
    })
}

pub fn zigzag_signed(z: usize) -> Option<i16> {
    if z == 0 {
        return None;
    }
    if z > 254 {
        return None;
    }
    Some(if z.is_multiple_of(2) {
        -(z as i16 / 2)
    } else {
        z.div_ceil(2) as i16
    })
}

/// Symbol layout: `0`=EOB, `1..=62` = zero-run length, `63..=316` = AC value zigzag `z` in `1..=254`.
pub fn residual_symbol_count() -> usize {
    1 + 62 + 254
}

pub fn sym_eob() -> usize {
    0
}

pub fn sym_zrun(run_len: usize) -> Option<usize> {
    if !(1..=62).contains(&run_len) {
        return None;
    }
    Some(run_len)
}

pub fn sym_value(z: usize) -> Option<usize> {
    if !(1..=254).contains(&z) {
        return None;
    }
    Some(62 + z)
}

/// Deterministic static frequency table (sums to [`libsrs_bitio::RANS_SCALE`]).
pub fn residual_token_model() -> RansModel {
    let mut freqs = vec![1u32; residual_symbol_count()];
    freqs[0] = 200;
    for slot in freqs.iter_mut().take(63).skip(1) {
        *slot = 35;
    }
    let sum_head: u32 = freqs[..63].iter().sum();
    let rem = RANS_SCALE - sum_head;
    let n_val = 254usize;
    let base = rem / n_val as u32;
    let extra = rem % n_val as u32;
    for i in 0..n_val {
        freqs[63 + i] = base + if (i as u32) < extra { 1 } else { 0 };
    }
    RansModel::try_from_freqs(freqs).expect("static residual model")
}

fn coeff_rans_eligible(freq: &[i16; 64]) -> bool {
    for &k in ZIGZAG.iter().skip(1) {
        let v = freq[k];
        if v != 0 && v.unsigned_abs() > MAX_RANS_ABS_COEFF as u16 {
            return false;
        }
    }
    true
}

/// Tokenize quantized coefficients (DC stored separately; `freq[0]` ignored here).
pub fn tokenize_ac(freq: &[i16; 64]) -> Result<Vec<usize>, SrsV2Error> {
    if !coeff_rans_eligible(freq) {
        return Err(SrsV2Error::syntax("coeff out of rANS residual range"));
    }
    let mut out = Vec::new();
    let mut zi = 1usize;
    let mut run = 0usize;

    while zi < 64 {
        let k = ZIGZAG[zi];
        let v = freq[k];
        if v == 0 {
            run += 1;
            zi += 1;
            while run >= 62 {
                out.push(sym_zrun(62).unwrap());
                run -= 62;
                if out.len() > MAX_SYMBOLS_PER_BLOCK {
                    return Err(SrsV2Error::syntax("token overflow"));
                }
            }
        } else {
            while run > 0 {
                let take = run.min(62);
                out.push(sym_zrun(take).unwrap());
                run -= take;
                if out.len() > MAX_SYMBOLS_PER_BLOCK {
                    return Err(SrsV2Error::syntax("token overflow"));
                }
            }
            let z = zigzag_unsigned(v).ok_or(SrsV2Error::syntax("bad coeff zigzag"))?;
            out.push(sym_value(z).unwrap());
            if out.len() > MAX_SYMBOLS_PER_BLOCK {
                return Err(SrsV2Error::syntax("token overflow"));
            }
            zi += 1;
        }
    }

    while run > 0 {
        let take = run.min(62);
        out.push(sym_zrun(take).unwrap());
        run -= take;
        if out.len() > MAX_SYMBOLS_PER_BLOCK {
            return Err(SrsV2Error::syntax("token overflow"));
        }
    }

    out.push(sym_eob());
    Ok(out)
}

/// Reconstruct AC coefficients; `freq[0]` must already hold DC.
pub fn detokenize_ac(symbols: &[usize], freq: &mut [i16; 64]) -> Result<(), SrsV2Error> {
    for &k in ZIGZAG.iter().skip(1) {
        freq[k] = 0;
    }
    let mut ac_pos = 0usize;
    let mut i = 0usize;
    while i < symbols.len() {
        let s = *symbols
            .get(i)
            .ok_or_else(|| SrsV2Error::syntax("missing symbol"))?;
        i += 1;
        if s == sym_eob() {
            while ac_pos < AC_POSITIONS {
                let k = ZIGZAG[ac_pos + 1];
                freq[k] = 0;
                ac_pos += 1;
            }
            if i != symbols.len() {
                return Err(SrsV2Error::syntax("trailing after EOB"));
            }
            return Ok(());
        }
        if (1..63).contains(&s) {
            let run = s;
            for _ in 0..run {
                if ac_pos >= AC_POSITIONS {
                    return Err(SrsV2Error::syntax("zero run overflow"));
                }
                let k = ZIGZAG[ac_pos + 1];
                freq[k] = 0;
                ac_pos += 1;
            }
            continue;
        }
        if (63..317).contains(&s) {
            if ac_pos >= AC_POSITIONS {
                return Err(SrsV2Error::syntax("coeff overflow"));
            }
            let z = s - 62;
            let v = zigzag_signed(z).ok_or(SrsV2Error::syntax("bad value symbol"))?;
            let k = ZIGZAG[ac_pos + 1];
            freq[k] = v;
            ac_pos += 1;
            continue;
        }
        return Err(SrsV2Error::syntax("invalid residual symbol"));
    }
    Err(SrsV2Error::syntax("missing EOB"))
}

pub fn rans_encode_tokens(model: &RansModel, symbols: &[usize]) -> Result<Vec<u8>, SrsV2Error> {
    rans_encode(model, symbols).map_err(map_bitio)
}

pub fn rans_decode_tokens(
    model: &RansModel,
    data: &[u8],
    num_symbols: usize,
) -> Result<Vec<usize>, SrsV2Error> {
    if num_symbols > MAX_SYMBOLS_PER_BLOCK {
        return Err(SrsV2Error::syntax("rans symbol budget"));
    }
    let budget = data.len().saturating_mul(32).max(4096);
    rans_decode(model, data, num_symbols, budget).map_err(map_bitio)
}

fn map_bitio(e: BitIoError) -> SrsV2Error {
    match e {
        BitIoError::RansDecodeBudget => SrsV2Error::syntax("rans decode budget"),
        _ => SrsV2Error::syntax("rans bitstream"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_signed_mapping_roundtrip() {
        for v in -127_i16..=127 {
            if v == 0 {
                continue;
            }
            let z = zigzag_unsigned(v).unwrap();
            assert_eq!(zigzag_signed(z).unwrap(), v);
        }
    }

    #[test]
    fn all_zero_ac_roundtrip() {
        let mut f = [0_i16; 64];
        f[0] = 11;
        let t = tokenize_ac(&f).unwrap();
        assert_eq!(*t.last().unwrap(), sym_eob());
        let mut out = f;
        detokenize_ac(&t, &mut out).unwrap();
        assert_eq!(out, f);
    }

    #[test]
    fn dc_only_block() {
        let mut f = [0_i16; 64];
        f[0] = -50;
        let t = tokenize_ac(&f).unwrap();
        let mut out = [0_i16; 64];
        out[0] = f[0];
        detokenize_ac(&t, &mut out).unwrap();
        assert_eq!(out, f);
    }

    #[test]
    fn sparse_ac_block() {
        let mut f = [0_i16; 64];
        f[0] = 3;
        f[ZIGZAG[3]] = -1;
        f[ZIGZAG[10]] = 4;
        let t = tokenize_ac(&f).unwrap();
        let mut out = [0_i16; 64];
        out[0] = f[0];
        detokenize_ac(&t, &mut out).unwrap();
        assert_eq!(out, f);
    }

    #[test]
    fn dense_noisy_block_roundtrip_rans() {
        let mut f = [0_i16; 64];
        f[0] = 5;
        let mut zi = 1;
        while zi < 64 {
            f[ZIGZAG[zi]] = (((zi * 13) % 17) as i16).saturating_sub(8).clamp(-127, 127);
            zi += 1;
        }
        let model = residual_token_model();
        let tok = tokenize_ac(&f).unwrap();
        let enc = rans_encode_tokens(&model, &tok).unwrap();
        let dec = rans_decode_tokens(&model, &enc, tok.len()).unwrap();
        assert_eq!(dec, tok);
        let mut out = [0_i16; 64];
        out[0] = f[0];
        detokenize_ac(&dec, &mut out).unwrap();
        assert_eq!(out, f);
    }

    #[test]
    fn max_legal_coefficient() {
        let mut f = [0_i16; 64];
        f[0] = 1;
        f[ZIGZAG[1]] = MAX_RANS_ABS_COEFF;
        f[ZIGZAG[2]] = -MAX_RANS_ABS_COEFF;
        let t = tokenize_ac(&f).unwrap();
        let mut out = [0_i16; 64];
        out[0] = f[0];
        detokenize_ac(&t, &mut out).unwrap();
        assert_eq!(out, f);
    }

    #[test]
    fn coeff_over_max_rejected() {
        let mut f = [0_i16; 64];
        f[ZIGZAG[1]] = 128;
        assert!(tokenize_ac(&f).is_err());
    }

    #[test]
    fn malformed_token_stream_no_eob() {
        let mut f = [0_i16; 64];
        f[0] = 1;
        assert!(detokenize_ac(&[], &mut f).is_err());
        assert!(detokenize_ac(&[1], &mut f).is_err());
    }

    #[test]
    fn truncated_rans_stream_fails() {
        let model = residual_token_model();
        let mut f = [0_i16; 64];
        f[ZIGZAG[1]] = 3;
        let tok = tokenize_ac(&f).unwrap();
        let mut enc = rans_encode_tokens(&model, &tok).unwrap();
        if enc.len() > 4 {
            enc.truncate(enc.len() - 2);
        }
        assert!(rans_decode_tokens(&model, &enc, tok.len()).is_err());
    }

    #[test]
    fn invalid_symbol_fails_decode() {
        let mut f = [0_i16; 64];
        assert!(detokenize_ac(&[500, sym_eob()], &mut f).is_err());
    }

    #[test]
    fn auto_counts_symbols_match_roundtrip() {
        let model = residual_token_model();
        let mut f = [0_i16; 64];
        f[0] = 9;
        f[ZIGZAG[5]] = -20;
        let tok = tokenize_ac(&f).unwrap();
        let enc = rans_encode_tokens(&model, &tok).unwrap();
        let dec = rans_decode_tokens(&model, &enc, tok.len()).unwrap();
        assert_eq!(tok, dec);
    }
}
