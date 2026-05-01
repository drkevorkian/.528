//! FR2 rev 3/4 residual packing — explicit AC tuples vs static rANS tokens per block.

use libsrs_bitio::RansModel;

use super::dct::ZIGZAG;
use super::dct::{fdct_8x8, idct_8x8};
use super::error::SrsV2Error;
use super::frame::VideoPlane;
use super::intra_codec::{dequantize, pick_mode, predict_block, quantize, PredMode};
use super::limits::MAX_FRAME_PAYLOAD_BYTES;
use super::rate_control::{ResidualEncodeStats, ResidualEntropy};
use super::residual_tokens::{
    detokenize_ac, rans_decode_tokens, rans_encode_tokens, residual_token_model, tokenize_ac,
    MAX_SYMBOLS_PER_BLOCK,
};

pub const TAG_EXPLICIT_AC: u8 = 0;
pub const TAG_RANS_AC: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockResidualCoding {
    ExplicitTuples,
    RansV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PResidualChunkKind {
    LegacyTuple,
    Adaptive(BlockResidualCoding),
}

pub(crate) fn write_explicit_ac_only(
    freq: &[i16; 64],
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    let mut pairs = 0_usize;
    let mut tmp = Vec::new();
    for &k in ZIGZAG.iter().skip(1) {
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

pub(crate) fn explicit_ac_only_len(freq: &[i16; 64]) -> usize {
    let mut pairs = 0usize;
    for &k in ZIGZAG.iter().skip(1) {
        if freq[k] != 0 {
            pairs += 1;
        }
    }
    2 + pairs * 3
}

pub(crate) fn read_explicit_ac_only(data: &[u8], cur: &mut usize) -> Result<[i16; 64], SrsV2Error> {
    let mut ac = [0_i16; 64];
    let pairs = read_u16(data, cur)? as usize;
    if pairs > 63 {
        return Err(SrsV2Error::syntax("ac pairs overflow"));
    }
    for _ in 0..pairs {
        let pos = read_u8(data, cur)? as usize;
        if pos == 0 || pos > 63 {
            return Err(SrsV2Error::syntax("bad coeff index"));
        }
        let v = read_i16(data, cur)?;
        ac[pos] = v;
    }
    Ok(ac)
}

fn read_u8(data: &[u8], cur: &mut usize) -> Result<u8, SrsV2Error> {
    if *cur >= data.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v = data[*cur];
    *cur += 1;
    Ok(v)
}

fn read_u16(data: &[u8], cur: &mut usize) -> Result<u16, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 2 {
        return Err(SrsV2Error::Truncated);
    }
    let v = u16::from_le_bytes([data[*cur], data[*cur + 1]]);
    *cur += 2;
    Ok(v)
}

fn read_i16(data: &[u8], cur: &mut usize) -> Result<i16, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 2 {
        return Err(SrsV2Error::Truncated);
    }
    let v = i16::from_le_bytes([data[*cur], data[*cur + 1]]);
    *cur += 2;
    Ok(v)
}

fn try_rans_payload(
    freq: &[i16; 64],
    model: &RansModel,
) -> Result<Option<(usize, Vec<u8>)>, SrsV2Error> {
    let tok = match tokenize_ac(freq) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let enc = rans_encode_tokens(model, &tok)?;
    Ok(Some((tok.len(), enc)))
}

/// Full serialized block size for explicit tuples (mode + dc + tag + AC blob).
fn explicit_wire_len(freq: &[i16; 64]) -> usize {
    1 + 2 + 1 + explicit_ac_only_len(freq)
}

fn rans_wire_len(enc_len: usize) -> usize {
    1 + 2 + 1 + 2 + 2 + enc_len
}

pub(crate) fn encode_intra_block_residual(
    mode: PredMode,
    freq: &[i16; 64],
    policy: ResidualEntropy,
    model: &RansModel,
    out: &mut Vec<u8>,
) -> Result<BlockResidualCoding, SrsV2Error> {
    let explicit_full = explicit_wire_len(freq);
    let rans_choice = try_rans_payload(freq, model)?;

    let coding = match policy {
        ResidualEntropy::Explicit => BlockResidualCoding::ExplicitTuples,
        ResidualEntropy::Rans => {
            if rans_choice.is_none() {
                return Err(SrsV2Error::syntax(
                    "forced rANS but coefficients out of range",
                ));
            }
            BlockResidualCoding::RansV1
        }
        ResidualEntropy::Auto => match &rans_choice {
            Some((_, enc)) => {
                let rfull = rans_wire_len(enc.len());
                if rfull < explicit_full {
                    BlockResidualCoding::RansV1
                } else {
                    BlockResidualCoding::ExplicitTuples
                }
            }
            None => BlockResidualCoding::ExplicitTuples,
        },
    };

    out.push(mode as u8);
    out.extend_from_slice(&freq[0].to_le_bytes());

    match coding {
        BlockResidualCoding::ExplicitTuples => {
            out.push(TAG_EXPLICIT_AC);
            write_explicit_ac_only(freq, out)?;
            Ok(BlockResidualCoding::ExplicitTuples)
        }
        BlockResidualCoding::RansV1 => {
            let (sym_ct, enc) = rans_choice.expect("rans payload");
            out.push(TAG_RANS_AC);
            let sc = u16::try_from(sym_ct).map_err(|_| SrsV2Error::syntax("rans sym count"))?;
            let bl =
                u16::try_from(enc.len()).map_err(|_| SrsV2Error::syntax("rans blob length"))?;
            out.extend_from_slice(&sc.to_le_bytes());
            out.extend_from_slice(&bl.to_le_bytes());
            out.extend_from_slice(&enc);
            Ok(BlockResidualCoding::RansV1)
        }
    }
}

pub(crate) fn decode_intra_block_residual(
    data: &[u8],
    cur: &mut usize,
) -> Result<(PredMode, [i16; 64]), SrsV2Error> {
    let mode_b = read_u8(data, cur)?;
    let mode = PredMode::from_u8(mode_b)?;
    let mut freq = [0_i16; 64];
    freq[0] = read_i16(data, cur)?;
    let tag = read_u8(data, cur)?;
    match tag {
        TAG_EXPLICIT_AC => {
            let ac = read_explicit_ac_only(data, cur)?;
            for &k in ZIGZAG.iter().skip(1) {
                freq[k] = ac[k];
            }
            Ok((mode, freq))
        }
        TAG_RANS_AC => {
            let sym_ct = read_u16(data, cur)? as usize;
            let bl = read_u16(data, cur)? as usize;
            if sym_ct > MAX_SYMBOLS_PER_BLOCK {
                return Err(SrsV2Error::syntax("rans symbol count"));
            }
            let end = cur
                .checked_add(bl)
                .ok_or(SrsV2Error::Overflow("rans blob"))?;
            if end > data.len() {
                return Err(SrsV2Error::Truncated);
            }
            let blob = &data[*cur..end];
            *cur = end;
            let model = residual_token_model();
            let syms = rans_decode_tokens(&model, blob, sym_ct)?;
            detokenize_ac(&syms, &mut freq)?;
            Ok((mode, freq))
        }
        _ => Err(SrsV2Error::syntax("bad residual coding tag")),
    }
}

pub(crate) fn encode_plane_intra_entropy(
    plane: &VideoPlane<u8>,
    qp: i16,
    policy: ResidualEntropy,
    stats: &mut ResidualEncodeStats,
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    let model = residual_token_model();
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
            let mut orig = [[0_i16; 8]; 8];
            for (r, row) in orig.iter_mut().enumerate() {
                for (c, cell) in row.iter_mut().enumerate() {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let v = if x < w && y < h {
                        plane.samples[y * stride + x] as i16
                    } else {
                        128
                    };
                    *cell = v;
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
            let kind = encode_intra_block_residual(mode, &qfreq, policy, &model, out)?;
            match kind {
                BlockResidualCoding::ExplicitTuples => stats.intra_explicit_blocks += 1,
                BlockResidualCoding::RansV1 => stats.intra_rans_blocks += 1,
            }
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

pub(crate) fn decode_plane_intra_entropy(
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
            let (mode, freq) = decode_intra_block_residual(data, cursor)?;
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
    for y in 0..h {
        for x in 0..w {
            plane.samples[y * stride + x] = rec[y * pw + x];
        }
    }
    Ok(())
}

/// P-frame 8×8 residual chunk (after outer `u32` length): `0` + legacy body, or `1` + adaptive body.
pub fn encode_p_residual_chunk(
    qfreq: &[i16; 64],
    policy: ResidualEntropy,
    model: &RansModel,
) -> Result<(Vec<u8>, PResidualChunkKind), SrsV2Error> {
    let mut legacy = Vec::new();
    legacy.push(PredMode::Dc as u8);
    legacy.extend_from_slice(&qfreq[0].to_le_bytes());
    write_explicit_ac_only(qfreq, &mut legacy)?;

    match policy {
        ResidualEntropy::Explicit => {
            let mut out = Vec::with_capacity(1 + legacy.len());
            out.push(0);
            out.extend_from_slice(&legacy);
            Ok((out, PResidualChunkKind::LegacyTuple))
        }
        ResidualEntropy::Rans => {
            let mut adaptive = Vec::new();
            let kind = encode_intra_block_residual(
                PredMode::Dc,
                qfreq,
                ResidualEntropy::Rans,
                model,
                &mut adaptive,
            )?;
            let mut out = Vec::with_capacity(1 + adaptive.len());
            out.push(1);
            out.extend_from_slice(&adaptive);
            Ok((out, PResidualChunkKind::Adaptive(kind)))
        }
        ResidualEntropy::Auto => {
            let mut adaptive = Vec::new();
            let kind = encode_intra_block_residual(
                PredMode::Dc,
                qfreq,
                ResidualEntropy::Auto,
                model,
                &mut adaptive,
            )?;
            if adaptive.len() < legacy.len() {
                let mut out = Vec::with_capacity(1 + adaptive.len());
                out.push(1);
                out.extend_from_slice(&adaptive);
                Ok((out, PResidualChunkKind::Adaptive(kind)))
            } else {
                let mut out = Vec::with_capacity(1 + legacy.len());
                out.push(0);
                out.extend_from_slice(&legacy);
                Ok((out, PResidualChunkKind::LegacyTuple))
            }
        }
    }
}

pub fn decode_p_residual_chunk(chunk: &[u8], qp: i16) -> Result<[[i16; 8]; 8], SrsV2Error> {
    if chunk.is_empty() {
        return Err(SrsV2Error::Truncated);
    }
    let layout = chunk[0];
    let body = &chunk[1..];
    let mut cur = 0usize;
    let freq = match layout {
        0 => {
            let mode_b = read_u8(body, &mut cur)?;
            PredMode::from_u8(mode_b)?;
            let mut freq = [0_i16; 64];
            freq[0] = read_i16(body, &mut cur)?;
            let pairs = read_u16(body, &mut cur)? as usize;
            if pairs > 63 {
                return Err(SrsV2Error::syntax("ac pairs overflow"));
            }
            for _ in 0..pairs {
                let pos = read_u8(body, &mut cur)? as usize;
                if pos == 0 || pos > 63 {
                    return Err(SrsV2Error::syntax("bad coeff index"));
                }
                let v = read_i16(body, &mut cur)?;
                freq[pos] = v;
            }
            if cur != body.len() {
                return Err(SrsV2Error::syntax("p residual trailing"));
            }
            freq
        }
        1 => {
            let (_m, f) = decode_intra_block_residual(body, &mut cur)?;
            if cur != body.len() {
                return Err(SrsV2Error::syntax("p rans residual trailing"));
            }
            f
        }
        _ => return Err(SrsV2Error::syntax("bad P residual layout")),
    };
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

#[cfg(test)]
mod residual_entropy_tests {
    use super::*;
    use crate::srsv2::dct::ZIGZAG;
    use crate::srsv2::frame::VideoPlane;
    use crate::srsv2::intra_codec::{decode_plane_intra, encode_plane_intra, PredMode};
    use crate::srsv2::rate_control::ResidualEncodeStats;

    fn sparse_one_ac_qfreq() -> [i16; 64] {
        let mut blk = [0_i16; 64];
        blk[0] = 5;
        blk[ZIGZAG[1]] = 1;
        blk
    }

    /// Dense AC=1 blocks blow up explicit tuples (63×3-byte pairs); static rANS usually wins.
    fn dense_ac_ones_qfreq() -> [i16; 64] {
        let mut f = [0_i16; 64];
        f[0] = 3;
        for &k in ZIGZAG.iter().skip(1) {
            f[k] = 1;
        }
        f
    }

    #[test]
    fn forced_rans_sparse_block_roundtrips() {
        let model = residual_token_model();
        let mut out = Vec::new();
        let qf = sparse_one_ac_qfreq();
        let k =
            encode_intra_block_residual(PredMode::Dc, &qf, ResidualEntropy::Rans, &model, &mut out)
                .unwrap();
        assert_eq!(k, BlockResidualCoding::RansV1);
        let mut c = 0usize;
        let (_m, f2) = decode_intra_block_residual(&out, &mut c).unwrap();
        assert_eq!(f2, qf);
    }

    #[test]
    fn auto_prefers_rans_when_explicit_bulkier() {
        let model = residual_token_model();
        let f = dense_ac_ones_qfreq();
        let explicit_full = explicit_wire_len(&f);
        let Some((_, enc)) = try_rans_payload(&f, &model).unwrap() else {
            panic!("tokenize failed");
        };
        let rfull = rans_wire_len(enc.len());
        assert!(
            rfull < explicit_full,
            "expected rANS smaller than explicit for dense AC=1 block (explicit={explicit_full} rans={rfull})"
        );
        let mut out = Vec::new();
        let k =
            encode_intra_block_residual(PredMode::Dc, &f, ResidualEntropy::Auto, &model, &mut out)
                .unwrap();
        assert_eq!(k, BlockResidualCoding::RansV1);
        let mut c = 0usize;
        let (_m, f2) = decode_intra_block_residual(&out, &mut c).unwrap();
        assert_eq!(f2, f);
    }

    #[test]
    fn auto_prefers_explicit_when_rans_larger() {
        let model = residual_token_model();
        let mut noisy = [0_i16; 64];
        noisy[0] = 1;
        for (i, slot) in noisy.iter_mut().enumerate().skip(1) {
            *slot = (((i * 7) % 15) as i16).saturating_sub(7).clamp(-127, 127);
        }
        let explicit_len = explicit_wire_len(&noisy);
        let tok = tokenize_ac(&noisy).expect("tok");
        let enc = rans_encode_tokens(&model, &tok).expect("enc");
        let rlen = rans_wire_len(enc.len());
        if rlen >= explicit_len {
            let mut out = Vec::new();
            let k = encode_intra_block_residual(
                PredMode::Dc,
                &noisy,
                ResidualEntropy::Auto,
                &model,
                &mut out,
            )
            .unwrap();
            assert_eq!(k, BlockResidualCoding::ExplicitTuples);
        }
    }

    #[test]
    fn intra_entropy_plane_matches_explicit_reconstruction() {
        let w = 64u32;
        let h = 64u32;
        let mut plane = VideoPlane::<u8>::try_new(w, h, w as usize).unwrap();
        for y in 0..h {
            for x in 0..w {
                plane.samples[y as usize * plane.stride + x as usize] =
                    ((x.wrapping_mul(13) ^ y.wrapping_mul(7)) & 0xff) as u8;
            }
        }
        let qp = 28_i16;
        let mut exp = Vec::new();
        encode_plane_intra(&plane, qp, &mut exp).unwrap();
        let mut ent = Vec::new();
        encode_plane_intra_entropy(
            &plane,
            qp,
            ResidualEntropy::Auto,
            &mut ResidualEncodeStats::default(),
            &mut ent,
        )
        .unwrap();
        let mut cur_e = 0usize;
        let mut dec_exp = VideoPlane::<u8>::try_new(w, h, w as usize).unwrap();
        decode_plane_intra(&exp, &mut cur_e, &mut dec_exp, qp).unwrap();
        let mut cur_n = 0usize;
        let mut dec_ent = VideoPlane::<u8>::try_new(w, h, w as usize).unwrap();
        decode_plane_intra_entropy(&ent, &mut cur_n, &mut dec_ent, qp).unwrap();
        assert_eq!(dec_exp.samples, dec_ent.samples);
    }

    #[test]
    fn p_chunk_roundtrip_matches_legacy_when_explicit() {
        let model = residual_token_model();
        let mut blk = [[0_i16; 8]; 8];
        blk[3][3] = 40;
        let mut linear = [0_i16; 64];
        for r in 0..8 {
            for c in 0..8 {
                linear[r * 8 + c] = blk[r][c];
            }
        }
        let f = fdct_8x8(&linear);
        let qf = quantize(&f, 28);
        let (chunk, _) = encode_p_residual_chunk(&qf, ResidualEntropy::Explicit, &model).unwrap();
        let dec = decode_p_residual_chunk(&chunk, 28).unwrap();
        let mut cur = 1usize;
        let legacy =
            super::super::intra_codec::decode_residual_block_8x8(&chunk, &mut cur, 28).unwrap();
        assert_eq!(dec, legacy);
    }
}
