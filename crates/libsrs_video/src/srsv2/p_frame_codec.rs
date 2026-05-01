//! Experimental SRSV2 P-frame payload (`FR2` revision **2** legacy tuples, **4** adaptive entropy): 16×16 integer-pel MC + 8×8 residuals.
//!
//! Chroma is predicted by copying reference U/V with half-resolution MV (no chroma residual in this slice).

use super::dct::fdct_8x8;
use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::intra_codec::{decode_residual_block_8x8, encode_residual_block_8x8, quantize};
use super::limits::{MAX_FRAME_PAYLOAD_BYTES, MAX_MOTION_VECTOR_PELS};
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::rate_control::{ResidualEncodeStats, ResidualEntropy, SrsV2EncodeSettings};
use super::residual_entropy::{
    decode_p_residual_chunk, encode_p_residual_chunk, BlockResidualCoding, PResidualChunkKind,
};
use super::residual_tokens::residual_token_model;

pub const FRAME_PAYLOAD_MAGIC_P: [u8; 4] = [b'F', b'R', b'2', 2];
pub const FRAME_PAYLOAD_MAGIC_P_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 4];

/// Residuals below this absolute threshold become skip sub-blocks (Y only).
const SKIP_ABS_THRESH: i16 = 6;

fn sample_u8_plane(plane: &VideoPlane<u8>, x: i32, y: i32) -> u8 {
    let w = plane.width as i32;
    let h = plane.height as i32;
    if x < 0 || y < 0 || x >= w || y >= h {
        return 128;
    }
    plane.samples[y as usize * plane.stride + x as usize]
}

fn sad_16x16(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mvx: i32,
    mvy: i32,
) -> u32 {
    let mut acc = 0_u32;
    for row in 0..16 {
        for col in 0..16 {
            let lx = mb_x * 16 + col;
            let ly = mb_y * 16 + row;
            let cx = cur.samples[ly as usize * cur.stride + lx as usize];
            let rx = lx as i32 + mvx;
            let ry = ly as i32 + mvy;
            let pv = sample_u8_plane(refp, rx, ry);
            acc += cx.abs_diff(pv) as u32;
        }
    }
    acc
}

fn pick_mv(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    radius: i16,
) -> (i16, i16) {
    let r = radius as i32;
    let mut best_mvx = 0_i16;
    let mut best_mvy = 0_i16;
    let mut best_sad = u32::MAX;
    for mvx in -r..=r {
        for mvy in -r..=r {
            let s = sad_16x16(cur, refp, mb_x, mb_y, mvx, mvy);
            if s < best_sad {
                best_sad = s;
                best_mvx = mvx as i16;
                best_mvy = mvy as i16;
            }
        }
    }
    (best_mvx, best_mvy)
}

fn validate_mv(mvx: i16, mvy: i16) -> Result<(), SrsV2Error> {
    if mvx.abs() > MAX_MOTION_VECTOR_PELS || mvy.abs() > MAX_MOTION_VECTOR_PELS {
        return Err(SrsV2Error::CorruptedMotionVector);
    }
    Ok(())
}

fn copy_chroma_mb8(
    ref_plane: &VideoPlane<u8>,
    out: &mut VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mvx: i16,
    mvy: i16,
) {
    let mvxc = (mvx as i32) / 2;
    let mvyc = (mvy as i32) / 2;
    let base_x = (mb_x * 8) as i32;
    let base_y = (mb_y * 8) as i32;
    for ry in 0..8 {
        for rx in 0..8 {
            let sx = base_x + rx as i32 + mvxc;
            let sy = base_y + ry as i32 + mvyc;
            let v = sample_u8_plane(ref_plane, sx, sy);
            let ox = (mb_x * 8) as usize + rx;
            let oy = (mb_y * 8) as usize + ry;
            if ox < out.width as usize && oy < out.height as usize {
                out.samples[oy * out.stride + ox] = v;
            }
        }
    }
}

fn read_u32(data: &[u8], cur: &mut usize) -> Result<u32, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let v = u32::from_le_bytes([data[*cur], data[*cur + 1], data[*cur + 2], data[*cur + 3]]);
    *cur += 4;
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

fn read_u8(data: &[u8], cur: &mut usize) -> Result<u8, SrsV2Error> {
    if *cur >= data.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v = data[*cur];
    *cur += 1;
    Ok(v)
}

/// Encode one P-frame (`FR2` rev 2 or 4). Caller must supply a valid reference of matching dimensions.
pub fn encode_yuv420_p_payload(
    seq: &VideoSequenceHeaderV2,
    cur: &YuvFrame,
    reference: &YuvFrame,
    frame_index: u32,
    qp: u8,
    settings: &SrsV2EncodeSettings,
    mut stats: Option<&mut ResidualEncodeStats>,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8
        || cur.format != PixelFormat::Yuv420p8
        || reference.format != PixelFormat::Yuv420p8
    {
        return Err(SrsV2Error::Unsupported("P-frame encode requires YUV420p8"));
    }
    if seq.max_ref_frames == 0 {
        return Err(SrsV2Error::Unsupported(
            "sequence max_ref_frames must be >= 1 for P-frame",
        ));
    }
    let w = seq.width;
    let h = seq.height;
    if !w.is_multiple_of(16) || !h.is_multiple_of(16) {
        return Err(SrsV2Error::syntax(
            "P-frame prototype requires width and height divisible by 16",
        ));
    }
    if cur.y.width != w || reference.y.width != w || cur.y.height != h || reference.y.height != h {
        return Err(SrsV2Error::syntax("plane geometry mismatch"));
    }

    let qp_i = qp.max(1) as i16;
    let radius = settings.clamped_motion_search_radius();
    let mb_cols = w / 16;
    let mb_rows = h / 16;

    let mut out = Vec::new();
    let magic = match settings.residual_entropy {
        ResidualEntropy::Explicit => FRAME_PAYLOAD_MAGIC_P,
        ResidualEntropy::Auto | ResidualEntropy::Rans => FRAME_PAYLOAD_MAGIC_P_ENTROPY,
    };
    out.extend_from_slice(&magic);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(qp);

    let rans_model = residual_token_model();

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let (mvx, mvy) = pick_mv(&cur.y, &reference.y, mbx, mby, radius);
            out.extend_from_slice(&mvx.to_le_bytes());
            out.extend_from_slice(&mvy.to_le_bytes());

            let mut pattern = 0_u8;
            let mut residual_chunks: Vec<Vec<u8>> = Vec::new();

            let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];
            for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
                let mut blk = [[0_i16; 8]; 8];
                let mut max_abs = 0_i16;
                for row in 0..8 {
                    for col in 0..8 {
                        let lx = mbx * 16 + dx + col;
                        let ly = mby * 16 + dy + row;
                        let cx = cur.y.samples[ly as usize * cur.y.stride + lx as usize] as i16;
                        let rx = lx as i32 + mvx as i32;
                        let ry = ly as i32 + mvy as i32;
                        let pred = sample_u8_plane(&reference.y, rx, ry) as i16;
                        let d = cx - pred;
                        blk[row as usize][col as usize] = d;
                        max_abs = max_abs.max(d.abs());
                    }
                }
                if max_abs <= SKIP_ABS_THRESH {
                    pattern |= 1 << si;
                } else {
                    let chunk = if matches!(settings.residual_entropy, ResidualEntropy::Explicit) {
                        let mut c = Vec::new();
                        encode_residual_block_8x8(&blk, qp_i, &mut c)?;
                        if let Some(s) = stats.as_mut() {
                            s.p_explicit_chunks += 1;
                        }
                        c
                    } else {
                        let mut linear = [0_i16; 64];
                        for r in 0..8 {
                            for c in 0..8 {
                                linear[r * 8 + c] = blk[r][c];
                            }
                        }
                        let f = fdct_8x8(&linear);
                        let qf = quantize(&f, qp_i);
                        let (c, kind) =
                            encode_p_residual_chunk(&qf, settings.residual_entropy, &rans_model)?;
                        if let Some(s) = stats.as_mut() {
                            match kind {
                                PResidualChunkKind::LegacyTuple => s.p_explicit_chunks += 1,
                                PResidualChunkKind::Adaptive(
                                    BlockResidualCoding::ExplicitTuples,
                                ) => {
                                    s.p_explicit_chunks += 1;
                                }
                                PResidualChunkKind::Adaptive(BlockResidualCoding::RansV1) => {
                                    s.p_rans_chunks += 1;
                                }
                            }
                        }
                        c
                    };
                    residual_chunks.push(chunk);
                }
            }
            out.push(pattern);
            for chunk in residual_chunks {
                let len = u32::try_from(chunk.len()).map_err(|_| SrsV2Error::Overflow("chunk"))?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&chunk);
            }
        }
    }

    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded P-frame",
        });
    }
    Ok(out)
}

/// Decode `FR2` rev **2** or **4** P-frame into a full YUV420p8 frame (chroma from reference MC).
pub fn decode_yuv420_p_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    reference: &YuvFrame,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 || reference.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("P-frame decode requires YUV420p8"));
    }
    if seq.max_ref_frames == 0 {
        return Err(SrsV2Error::Unsupported(
            "inter frame in sequence with max_ref_frames=0",
        ));
    }
    let w = seq.width;
    let h = seq.height;
    if !w.is_multiple_of(16) || !h.is_multiple_of(16) {
        return Err(SrsV2Error::syntax("bad dimensions for P-frame"));
    }
    if reference.y.width != w || reference.y.height != h {
        return Err(SrsV2Error::syntax("reference geometry mismatch"));
    }

    if payload.len() < 4 + 4 + 1 {
        return Err(SrsV2Error::Truncated);
    }
    if payload.len() < 4 || &payload[0..3] != b"FR2" || !matches!(payload[3], 2 | 4) {
        return Err(SrsV2Error::BadMagic);
    }
    let entropy_chunks = payload[3] == 4;
    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let qp = read_u8(payload, &mut cur)?;
    let qp_i = qp.max(1) as i16;

    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    let mb_cols = w / 16;
    let mb_rows = h / 16;

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let mvx = read_i16(payload, &mut cur)?;
            let mvy = read_i16(payload, &mut cur)?;
            validate_mv(mvx, mvy)?;
            let pattern = read_u8(payload, &mut cur)?;

            let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];
            for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
                let skip = (pattern & (1 << si)) != 0;
                if skip {
                    for row in 0..8 {
                        for col in 0..8 {
                            let lx = mbx * 16 + dx + col;
                            let ly = mby * 16 + dy + row;
                            let rx = lx as i32 + mvx as i32;
                            let ry = ly as i32 + mvy as i32;
                            let pv = sample_u8_plane(&reference.y, rx, ry);
                            y_plane.samples[ly as usize * y_plane.stride + lx as usize] = pv;
                        }
                    }
                } else {
                    let chunk_len = read_u32(payload, &mut cur)? as usize;
                    let chunk_start = cur;
                    let chunk_end = chunk_start
                        .checked_add(chunk_len)
                        .ok_or(SrsV2Error::Overflow("p residual chunk"))?;
                    if chunk_end > payload.len() {
                        return Err(SrsV2Error::Truncated);
                    }
                    let chunk = &payload[chunk_start..chunk_end];
                    let res = if entropy_chunks {
                        decode_p_residual_chunk(chunk, qp_i)?
                    } else {
                        let mut c = 0usize;
                        let r = decode_residual_block_8x8(chunk, &mut c, qp_i)?;
                        if c != chunk.len() {
                            return Err(SrsV2Error::syntax("residual chunk length mismatch"));
                        }
                        r
                    };
                    cur = chunk_end;
                    for row in 0..8 {
                        for col in 0..8 {
                            let lx = mbx * 16 + dx + col;
                            let ly = mby * 16 + dy + row;
                            let rx = lx as i32 + mvx as i32;
                            let ry = ly as i32 + mvy as i32;
                            let pred = sample_u8_plane(&reference.y, rx, ry) as i32;
                            let pv = (pred + res[row as usize][col as usize] as i32).clamp(0, 255);
                            y_plane.samples[ly as usize * y_plane.stride + lx as usize] = pv as u8;
                        }
                    }
                }
            }

            copy_chroma_mb8(&reference.u, &mut u_plane, mbx, mby, mvx, mvy);
            copy_chroma_mb8(&reference.v, &mut v_plane, mbx, mby, mvx, mvy);
        }
    }

    if cur != payload.len() {
        return Err(SrsV2Error::syntax("trailing P-frame bytes"));
    }

    Ok(DecodedVideoFrameV2 {
        frame_index,
        width: w,
        height: h,
        yuv: YuvFrame {
            format: PixelFormat::Yuv420p8,
            y: y_plane,
            u: u_plane,
            v: v_plane,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::color::rgb888_full_to_yuv420_bt709;
    use crate::srsv2::frame_codec::encode_yuv420_intra_payload;
    use crate::srsv2::model::{
        ChromaSiting, ColorPrimaries, ColorRange, MatrixCoefficients, SrsVideoProfile,
        TransferFunction,
    };

    fn seq_inter(w: u32, h: u32) -> VideoSequenceHeaderV2 {
        VideoSequenceHeaderV2 {
            width: w,
            height: h,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            max_ref_frames: 1,
        }
    }

    #[test]
    fn identical_frames_p_smaller_than_i() {
        let w = 64u32;
        let h = 64u32;
        let seq = seq_inter(w, h);
        let rgb = vec![200_u8; (w * h * 3) as usize];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, w, h, ColorRange::Limited).unwrap();
        let qp = 28_u8;
        let settings = SrsV2EncodeSettings {
            keyframe_interval: 30,
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let i_payload = encode_yuv420_intra_payload(&seq, &yuv, 0, qp, &settings, None).unwrap();
        let p_payload = encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, qp, &settings, None).unwrap();
        assert!(
            p_payload.len() < i_payload.len(),
            "expected P payload smaller than I for identical texture: p={} i={}",
            p_payload.len(),
            i_payload.len()
        );
    }

    #[test]
    fn moving_square_decodes_different_frames() {
        let w = 64u32;
        let h = 64u32;
        let seq = seq_inter(w, h);
        let mut rgb0 = vec![20_u8; (w * h * 3) as usize];
        for y in 20..44 {
            for x in 20..44 {
                let i = ((y * w + x) * 3) as usize;
                rgb0[i] = 240;
                rgb0[i + 1] = 240;
                rgb0[i + 2] = 240;
            }
        }
        let mut rgb1 = vec![20_u8; (w * h * 3) as usize];
        for y in 20..44 {
            for x in 24..48 {
                let i = ((y * w + x) * 3) as usize;
                rgb1[i] = 240;
                rgb1[i + 1] = 240;
                rgb1[i + 2] = 240;
            }
        }
        let y0 = rgb888_full_to_yuv420_bt709(&rgb0, w, h, ColorRange::Limited).unwrap();
        let y1 = rgb888_full_to_yuv420_bt709(&rgb1, w, h, ColorRange::Limited).unwrap();
        let qp = 24_u8;
        let settings = SrsV2EncodeSettings {
            keyframe_interval: 30,
            motion_search_radius: 16,
            ..Default::default()
        };
        let p_payload = encode_yuv420_p_payload(&seq, &y1, &y0, 1, qp, &settings, None).unwrap();
        let dec = decode_yuv420_p_payload(&seq, &p_payload, &y0).unwrap();
        let mean_dec: u32 = dec.yuv.y.samples.iter().map(|&x| x as u32).sum::<u32>()
            / dec.yuv.y.samples.len() as u32;
        let mean_tgt: u32 =
            y1.y.samples.iter().map(|&x| x as u32).sum::<u32>() / y1.y.samples.len() as u32;
        assert!(
            (mean_dec as i32 - mean_tgt as i32).abs() < 40,
            "mean luma should be loosely tracked"
        );
    }

    #[test]
    fn corrupted_mv_rejected() {
        let w = 64u32;
        let h = 64u32;
        let seq = seq_inter(w, h);
        let rgb = vec![128_u8; (w * h * 3) as usize];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, w, h, ColorRange::Limited).unwrap();
        let mut payload = encode_yuv420_p_payload(
            &seq,
            &yuv,
            &yuv,
            1,
            28,
            &SrsV2EncodeSettings::default(),
            None,
        )
        .unwrap();
        // Corrupt first MV after header (offset 4+4+1 = 9): i16 LE 300 > MAX_MOTION_VECTOR_PELS
        if payload.len() > 11 {
            payload[9] = 0x2c;
            payload[10] = 0x01;
        }
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(err, SrsV2Error::CorruptedMotionVector));
    }

    #[test]
    fn excessive_max_ref_in_header_rejected() {
        let mut hdr = [0_u8; 64];
        hdr[0..4].copy_from_slice(b"SRS2");
        hdr[4] = 1;
        hdr[8..12].copy_from_slice(&64u32.to_le_bytes());
        hdr[12..16].copy_from_slice(&64u32.to_le_bytes());
        hdr[24] = 99;
        let err = crate::srsv2::model::decode_sequence_header_v2(&hdr).unwrap_err();
        assert!(matches!(err, SrsV2Error::ExcessiveReferenceFrames(99)));
    }
}
