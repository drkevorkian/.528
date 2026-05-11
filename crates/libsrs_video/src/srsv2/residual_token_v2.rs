//! Experimental **residual token layout v2** — standalone AC coefficient serialization for benchmarking.
//!
//! Intended as the **Block 6 residual redesign** path when the Block 4 transform-grouping gate fails
//! (see project SRSV2 docs); compare telemetry via **`bench_srsv2 --compare-residual-token-v2`**.
//!
//! Not wired into **`FR2`** payloads. Compared against legacy [`super::residual_tokens`] static **rANS**
//! AC bodies when coefficients fit **±[`MAX_ABS_AC_COEFF`]** (same as v1).
//!
//! Design:
//! - **Four zigzag AC bands** (low → high frequency).
//! - Per band: **skip** (all zero), else **last-nonzero index** (EOB placement within the band).
//! - **Unsigned varints** for zero-runs (with per-plane bias for minor context steering) and **zigzag-mapped**
//!   coefficient values (single number carries magnitude + sign).
//! - **Plane id** (`Y`/`U`/`V`) in the header biases the first-pass zero-run Golomb offset.

use super::dct::ZIGZAG;
use super::error::SrsV2Error;

/// Same ceiling as [`super::residual_tokens::MAX_RANS_ABS_COEFF`].
pub const MAX_ABS_AC_COEFF: i16 = 127;

const MAGIC: u8 = 0x52;
const VERSION: u8 = 2;

/// Zigzag slot indices (`1..64`) — first AC index per band.
const BAND_FIRST_ZZ: [usize; 4] = [1, 17, 33, 49];
const BAND_LEN: [usize; 4] = [16, 16, 16, 15];

#[inline]
fn plane_zrun_bias(plane: u8) -> u64 {
    match plane & 3 {
        0 => 0,
        1 => 2,
        2 => 5,
        _ => 0,
    }
}

fn validate_coeffs(freq: &[i16; 64]) -> Result<(), SrsV2Error> {
    for &zi in ZIGZAG.iter().skip(1) {
        let v = freq[zi];
        if v != 0 && v.unsigned_abs() > MAX_ABS_AC_COEFF as u16 {
            return Err(SrsV2Error::syntax("residual_token_v2: coeff out of range"));
        }
    }
    Ok(())
}

fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(((v & 0x7F) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_uvarint(data: &[u8], cur: &mut usize) -> Result<u64, SrsV2Error> {
    let mut shift = 0_u32;
    let mut val = 0_u64;
    loop {
        let b = *data.get(*cur).ok_or(SrsV2Error::Truncated)?;
        *cur += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(SrsV2Error::syntax("residual_token_v2: varint overflow"));
        }
    }
    Ok(val)
}

#[inline]
fn zigzag_i16(n: i16) -> u32 {
    ((n as i32) << 1 ^ ((n as i32) >> 15)) as u32
}

#[inline]
fn unzigzag_u32(z: u32) -> i16 {
    let z = z as i32;
    ((z >> 1) ^ -(z & 1)) as i16
}

/// Encode AC coefficients (**`freq[0]` DC ignored**). `plane` is `0=Y`, `1=U`, `2=V`.
pub fn encode_ac_payload(freq: &[i16; 64], plane: u8) -> Result<Vec<u8>, SrsV2Error> {
    validate_coeffs(freq)?;
    let mut out = Vec::new();
    out.push(MAGIC);
    out.push(VERSION);
    out.push(plane & 3);
    let bias = plane_zrun_bias(plane);

    for band in 0..4 {
        let len = BAND_LEN[band];
        let first = BAND_FIRST_ZZ[band];
        let mut local = vec![0_i16; len];
        for i in 0..len {
            let zi = first + i;
            local[i] = freq[ZIGZAG[zi]];
        }

        let mut last_nz = None::<usize>;
        for i in (0..len).rev() {
            if local[i] != 0 {
                last_nz = Some(i);
                break;
            }
        }

        if last_nz.is_none() {
            out.push(0x00);
            continue;
        }
        let last = last_nz.unwrap();
        let hl = u8::try_from(last + 1)
            .map_err(|_| SrsV2Error::syntax("residual_token_v2: band hdr"))?;
        // 0x00 = skip; 0x01..=0x10 => last_local = hl - 1
        out.push(hl);

        let mut pos = 0usize;
        while pos <= last {
            let mut zrun = 0_u64;
            while pos <= last && local[pos] == 0 {
                zrun += 1;
                pos += 1;
            }
            write_uvarint(&mut out, zrun.saturating_add(bias));

            if pos > last {
                break;
            }

            let v = local[pos];
            write_uvarint(&mut out, u64::from(zigzag_i16(v)));
            pos += 1;
        }
    }

    Ok(out)
}

/// Decode AC into `freq`, preserving caller's **`freq[0]`**. Returns bytes consumed.
pub fn decode_ac_payload(data: &[u8], freq: &mut [i16; 64]) -> Result<usize, SrsV2Error> {
    if data.len() < 3 {
        return Err(SrsV2Error::Truncated);
    }
    if data[0] != MAGIC {
        return Err(SrsV2Error::syntax("residual_token_v2: bad magic"));
    }
    if data[1] != VERSION {
        return Err(SrsV2Error::syntax("residual_token_v2: bad version"));
    }
    let plane = data[2] & 3;
    let bias = plane_zrun_bias(plane);
    let mut cur = 3usize;

    for k in ZIGZAG.iter().skip(1) {
        freq[*k] = 0;
    }

    for band in 0..4 {
        let len = BAND_LEN[band];
        let first = BAND_FIRST_ZZ[band];
        let ctrl = *data.get(cur).ok_or(SrsV2Error::Truncated)?;
        cur += 1;
        if ctrl == 0x00 {
            continue;
        }
        let last = (ctrl as usize)
            .checked_sub(1)
            .ok_or_else(|| SrsV2Error::syntax("residual_token_v2: band hdr"))?;
        if last >= len {
            return Err(SrsV2Error::syntax("residual_token_v2: last_local"));
        }

        let mut pos = 0usize;
        while pos <= last {
            let z_enc = read_uvarint(data, &mut cur)?;
            let zrun = z_enc.saturating_sub(bias);
            let zr = usize::try_from(zrun)
                .map_err(|_| SrsV2Error::syntax("residual_token_v2: zrun range"))?;
            pos = pos
                .checked_add(zr)
                .ok_or_else(|| SrsV2Error::syntax("residual_token_v2: zrun overflow"))?;
            if pos > last {
                return Err(SrsV2Error::syntax("residual_token_v2: zrun overflow"));
            }

            let zz = read_uvarint(data, &mut cur)?;
            if zz > u64::from(u32::MAX) {
                return Err(SrsV2Error::syntax("residual_token_v2: coeff zz"));
            }
            let v = unzigzag_u32(zz as u32);
            if v == 0 || v.unsigned_abs() > MAX_ABS_AC_COEFF as u16 {
                return Err(SrsV2Error::syntax("residual_token_v2: bad coeff"));
            }
            let idx = ZIGZAG[first + pos];
            freq[idx] = v;
            pos += 1;
        }
    }

    Ok(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(freq: &[i16; 64], plane: u8) {
        let enc = encode_ac_payload(freq, plane).unwrap();
        let mut out = *freq;
        let n = decode_ac_payload(&enc, &mut out).unwrap();
        assert_eq!(n, enc.len());
        assert_eq!(out, *freq);
    }

    #[test]
    fn all_zero_ac() {
        let mut f = [0_i16; 64];
        f[0] = 42;
        roundtrip(&f, 0);
    }

    #[test]
    fn single_ac_low_band() {
        let mut f = [0_i16; 64];
        f[0] = 1;
        f[ZIGZAG[3]] = -5;
        roundtrip(&f, 0);
    }

    #[test]
    fn bands_sparse() {
        let mut f = [0_i16; 64];
        f[0] = 9;
        f[ZIGZAG[10]] = 3;
        f[ZIGZAG[40]] = -7;
        f[ZIGZAG[63]] = 1;
        roundtrip(&f, 2);
    }

    #[test]
    fn dense_random_roundtrip() {
        let mut f = [0_i16; 64];
        f[0] = -33;
        let mut zi = 1;
        while zi < 64 {
            f[ZIGZAG[zi]] = (((zi * 17) % 31) as i16)
                .saturating_sub(15)
                .clamp(-127, 127);
            zi += 1;
        }
        roundtrip(&f, 1);
    }

    #[test]
    fn rejects_out_of_range() {
        let mut f = [0_i16; 64];
        f[ZIGZAG[5]] = 128;
        assert!(encode_ac_payload(&f, 0).is_err());
    }
}
