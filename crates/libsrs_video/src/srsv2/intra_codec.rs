//! Intra-only SRSV2 baseline — 8×8 blocks, integer DCT, explicit AC tuples.

#![allow(clippy::needless_range_loop)]

use super::dct::{fdct_8x8, idct_8x8, ZIGZAG};
use super::error::SrsV2Error;
use super::frame::VideoPlane;
use super::limits::MAX_FRAME_PAYLOAD_BYTES;

#[derive(Clone, Copy)]
pub(crate) enum PredMode {
    Dc = 0,
    Horizontal = 1,
    Vertical = 2,
    Planar = 3,
    Diagonal = 4,
}

impl PredMode {
    pub(crate) fn from_u8(v: u8) -> Result<Self, SrsV2Error> {
        match v {
            0 => Ok(Self::Dc),
            1 => Ok(Self::Horizontal),
            2 => Ok(Self::Vertical),
            3 => Ok(Self::Planar),
            4 => Ok(Self::Diagonal),
            _ => Err(SrsV2Error::syntax("bad intra mode")),
        }
    }
}

fn sample(rec: &[u8], stride: usize, pw: usize, ph: usize, x: isize, y: isize) -> i16 {
    if x < 0 || y < 0 || x >= pw as isize || y >= ph as isize {
        128
    } else {
        rec[y as usize * stride + x as usize] as i16
    }
}

pub(crate) fn predict_block(
    mode: PredMode,
    rec: &[u8],
    stride: usize,
    pw: usize,
    ph: usize,
    bx: usize,
    by: usize,
) -> [[i16; 8]; 8] {
    let sx = (bx * 8) as isize;
    let sy = (by * 8) as isize;
    let mut p = [[0_i16; 8]; 8];
    match mode {
        PredMode::Dc => {
            let mut sum = 0_i32;
            let mut n = 0_i32;
            for i in 0..8 {
                sum += sample(rec, stride, pw, ph, sx + i, sy - 1) as i32;
                sum += sample(rec, stride, pw, ph, sx - 1, sy + i) as i32;
                n += 2;
            }
            let dc = if n > 0 { (sum / n) as i16 } else { 128 };
            for r in 0..8 {
                for c in 0..8 {
                    p[r][c] = dc;
                }
            }
        }
        PredMode::Horizontal => {
            for r in 0..8 {
                let left = sample(rec, stride, pw, ph, sx - 1, sy + r as isize);
                for c in 0..8 {
                    p[r][c] = left;
                }
            }
        }
        PredMode::Vertical => {
            for c in 0..8 {
                let top = sample(rec, stride, pw, ph, sx + c as isize, sy - 1);
                for r in 0..8 {
                    p[r][c] = top;
                }
            }
        }
        PredMode::Planar => {
            for r in 0..8 {
                for c in 0..8 {
                    let top = sample(rec, stride, pw, ph, sx + c as isize, sy - 1);
                    let left = sample(rec, stride, pw, ph, sx - 1, sy + r as isize);
                    p[r][c] = ((top as i32 + left as i32) / 2) as i16;
                }
            }
        }
        PredMode::Diagonal => {
            for r in 0..8 {
                for c in 0..8 {
                    let top = sample(rec, stride, pw, ph, sx + c as isize, sy - 1);
                    let left = sample(rec, stride, pw, ph, sx - 1, sy + r as isize);
                    let tl = sample(rec, stride, pw, ph, sx - 1, sy - 1);
                    let x = top as i32 + left as i32 - tl as i32;
                    p[r][c] = x.clamp(0, 255) as i16;
                }
            }
        }
    }
    p
}

fn satd_mode(orig: &[[i16; 8]; 8], pred: &[[i16; 8]; 8]) -> i32 {
    let mut s = 0_i32;
    for r in 0..8 {
        for c in 0..8 {
            let d = orig[r][c] - pred[r][c];
            s += d.abs() as i32;
        }
    }
    s
}

pub(crate) fn pick_mode(
    rec: &[u8],
    stride: usize,
    pw: usize,
    ph: usize,
    bx: usize,
    by: usize,
    orig: &[[i16; 8]; 8],
) -> PredMode {
    let modes = [
        PredMode::Dc,
        PredMode::Horizontal,
        PredMode::Vertical,
        PredMode::Planar,
        PredMode::Diagonal,
    ];
    let mut best = PredMode::Dc;
    let mut best_satd = i32::MAX;
    for m in modes {
        let pred = predict_block(m, rec, stride, pw, ph, bx, by);
        let satd = satd_mode(orig, &pred);
        if satd < best_satd {
            best_satd = satd;
            best = m;
        }
    }
    best
}

pub(crate) fn quantize(block: &[i16; 64], qp: i16) -> [i16; 64] {
    let q = qp.max(1);
    let mut o = [0_i16; 64];
    for i in 0..64 {
        o[i] = ((block[i] as i32 + (q as i32 / 2) * block[i].signum() as i32) / q as i32)
            .clamp(-32768, 32767) as i16;
    }
    o
}

pub(crate) fn dequantize(block: &[i16; 64], qp: i16) -> [i16; 64] {
    let q = qp.max(1);
    let mut o = [0_i16; 64];
    for i in 0..64 {
        o[i] = (block[i] as i32 * q as i32).clamp(-32768, 32767) as i16;
    }
    o
}

pub(crate) fn quantize_4x4(block: &[i16; 16], qp: i16) -> [i16; 16] {
    let q = qp.max(1);
    let mut o = [0_i16; 16];
    for i in 0..16 {
        o[i] = ((block[i] as i32 + (q as i32 / 2) * block[i].signum() as i32) / q as i32)
            .clamp(-32768, 32767) as i16;
    }
    o
}

pub(crate) fn dequantize_4x4(block: &[i16; 16], qp: i16) -> [i16; 16] {
    let q = qp.max(1);
    let mut o = [0_i16; 16];
    for i in 0..16 {
        o[i] = (block[i] as i32 * q as i32).clamp(-32768, 32767) as i16;
    }
    o
}

pub(crate) fn encode_plane_intra(
    plane: &VideoPlane<u8>,
    qp: i16,
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let len = pw
        .checked_mul(ph)
        .ok_or(SrsV2Error::Overflow("pad plane"))?;
    let mut rec = vec![128_u8; len];
    let bw = pw / 8;
    let bh = ph / 8;
    for by in 0..bh {
        for bx in 0..bw {
            let mut orig = [[0_i16; 8]; 8];
            for r in 0..8 {
                for c in 0..8 {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let v = if x < w && y < h {
                        plane.samples[y * stride + x] as i16
                    } else {
                        128
                    };
                    orig[r][c] = v;
                }
            }
            let mode = pick_mode(&rec, pw, pw, ph, bx, by, &orig);
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let mut diff = [[0_i16; 8]; 8];
            for r in 0..8 {
                for c in 0..8 {
                    diff[r][c] = orig[r][c] - pred[r][c];
                }
            }
            let mut blk = [0_i16; 64];
            for r in 0..8 {
                for c in 0..8 {
                    blk[r * 8 + c] = diff[r][c];
                }
            }
            let freq = fdct_8x8(&blk);
            let qfreq = quantize(&freq, qp);
            write_block(mode, &qfreq, out)?;
            let recon_freq = dequantize(&qfreq, qp);
            let rpix = idct_8x8(&recon_freq);
            for r in 0..8 {
                for c in 0..8 {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                    if x < pw && y < ph {
                        rec[y * pw + x] = pv as u8;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Pack DCT coefficients for a pure 8×8 residual block (added to MC prediction outside).
pub(crate) fn encode_residual_block_8x8(
    block: &[[i16; 8]; 8],
    qp: i16,
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    let mut blk = [0_i16; 64];
    for r in 0..8 {
        for c in 0..8 {
            blk[r * 8 + c] = block[r][c];
        }
    }
    let freq = fdct_8x8(&blk);
    let qfreq = quantize(&freq, qp);
    write_block(PredMode::Dc, &qfreq, out)
}

pub(crate) fn decode_residual_block_8x8(
    data: &[u8],
    cursor: &mut usize,
    qp: i16,
) -> Result<[[i16; 8]; 8], SrsV2Error> {
    let (_mode, freq) = read_block(data, cursor)?;
    let recon_freq = dequantize(&freq, qp);
    let rpix = idct_8x8(&recon_freq);
    let mut out = [[0_i16; 8]; 8];
    for r in 0..8 {
        for c in 0..8 {
            out[r][c] = rpix[r * 8 + c];
        }
    }
    Ok(out)
}

fn write_block(mode: PredMode, freq: &[i16; 64], out: &mut Vec<u8>) -> Result<(), SrsV2Error> {
    out.push(mode as u8);
    out.extend_from_slice(&freq[0].to_le_bytes());
    let mut pairs = 0_usize;
    let mut tmp = Vec::new();
    for zi in 1..64 {
        let k = ZIGZAG[zi];
        let v = freq[k];
        if v != 0 {
            if pairs >= 63 {
                return Err(SrsV2Error::syntax("too many ac coeffs"));
            }
            tmp.push(k as u8);
            tmp.extend_from_slice(&v.to_le_bytes());
            pairs += 1;
        }
    }
    let pairs_u16 = u16::try_from(pairs).map_err(|_| SrsV2Error::syntax("ac pairs"))?;
    out.extend_from_slice(&pairs_u16.to_le_bytes());
    out.extend_from_slice(&tmp);
    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "plane bitstream",
        });
    }
    Ok(())
}

pub(crate) fn decode_plane_intra(
    data: &[u8],
    cursor: &mut usize,
    plane: &mut VideoPlane<u8>,
    qp: i16,
) -> Result<(), SrsV2Error> {
    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    for by in 0..bh {
        for bx in 0..bw {
            let (mode, freq) = read_block(data, cursor)?;
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let recon_freq = dequantize(&freq, qp);
            let rpix = idct_8x8(&recon_freq);
            for r in 0..8 {
                for c in 0..8 {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                    if x < pw && y < ph {
                        rec[y * pw + x] = pv as u8;
                    }
                }
            }
        }
    }
    // copy extracted region to plane output
    for y in 0..h {
        for x in 0..w {
            plane.samples[y * stride + x] = rec[y * pw + x];
        }
    }
    Ok(())
}

fn read_block(data: &[u8], cursor: &mut usize) -> Result<(PredMode, [i16; 64]), SrsV2Error> {
    let mode_b = read_u8(data, cursor)?;
    let mode = PredMode::from_u8(mode_b)?;
    let mut freq = [0_i16; 64];
    freq[0] = read_i16(data, cursor)?;
    let pairs = read_u16(data, cursor)? as usize;
    if pairs > 63 {
        return Err(SrsV2Error::syntax("ac pairs overflow"));
    }
    for _ in 0..pairs {
        let pos = read_u8(data, cursor)? as usize;
        if pos == 0 || pos > 63 {
            return Err(SrsV2Error::syntax("bad coeff index"));
        }
        let v = read_i16(data, cursor)?;
        freq[pos] = v;
    }
    Ok((mode, freq))
}

fn read_u8(data: &[u8], cursor: &mut usize) -> Result<u8, SrsV2Error> {
    if *cursor >= data.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v = data[*cursor];
    *cursor += 1;
    Ok(v)
}

fn read_u16(data: &[u8], cursor: &mut usize) -> Result<u16, SrsV2Error> {
    if data.len().saturating_sub(*cursor) < 2 {
        return Err(SrsV2Error::Truncated);
    }
    let v = u16::from_le_bytes([data[*cursor], data[*cursor + 1]]);
    *cursor += 2;
    Ok(v)
}

fn read_i16(data: &[u8], cursor: &mut usize) -> Result<i16, SrsV2Error> {
    if data.len().saturating_sub(*cursor) < 2 {
        return Err(SrsV2Error::Truncated);
    }
    let v = i16::from_le_bytes([data[*cursor], data[*cursor + 1]]);
    *cursor += 2;
    Ok(v)
}
