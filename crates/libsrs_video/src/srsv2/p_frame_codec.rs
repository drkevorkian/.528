//! Experimental SRSV2 P-frame payload (`FR2` revision **2** legacy tuples, **4** adaptive entropy): 16×16 integer-pel MC + 8×8 residuals.
//!
//! Half-pel luma (`FR2` rev **5** explicit / **6** entropy) stores MVs in **quarter-pel units** (`±2` == half-pel).
//!
//! Chroma is predicted by copying reference U/V with integer MV (`mv/2` integer path; `mv_q/8` half-pel path).

use super::dct::fdct_8x8;
use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::intra_codec::{decode_residual_block_8x8, encode_residual_block_8x8, quantize};
use super::limits::{MAX_FRAME_PAYLOAD_BYTES, MAX_MOTION_VECTOR_PELS};
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::motion_search::{pick_mv, sample_u8_plane, SrsV2MotionEncodeStats};
use super::rate_control::{
    ResidualEncodeStats, ResidualEntropy, SrsV2EncodeSettings, SrsV2SubpelMode,
};
use super::residual_entropy::{
    decode_p_residual_chunk, encode_p_residual_chunk, BlockResidualCoding, PResidualChunkKind,
};
use super::residual_tokens::residual_token_model;
use super::subpel::{refine_half_pel_center, sample_luma_bilinear_qpel, validate_mv_qpel_halfgrid};

pub const FRAME_PAYLOAD_MAGIC_P: [u8; 4] = [b'F', b'R', b'2', 2];
pub const FRAME_PAYLOAD_MAGIC_P_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 4];
/// Half-pel P-frame, tuple residuals (same layout as rev **2** after MV width).
pub const FRAME_PAYLOAD_MAGIC_P_SUBPEL: [u8; 4] = [b'F', b'R', b'2', 5];
/// Half-pel P-frame, adaptive residual chunks (same layout as rev **4** after MV width).
pub const FRAME_PAYLOAD_MAGIC_P_SUBPEL_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 6];

/// Default skip threshold when [`SrsV2EncodeSettings::enable_skip_blocks`] is true (Y only).
const SKIP_ABS_THRESH: i16 = 6;

fn validate_mv(mvx: i16, mvy: i16) -> Result<(), SrsV2Error> {
    if mvx.abs() > MAX_MOTION_VECTOR_PELS || mvy.abs() > MAX_MOTION_VECTOR_PELS {
        return Err(SrsV2Error::CorruptedMotionVector);
    }
    Ok(())
}

fn copy_chroma_mb8_qpel(
    ref_plane: &VideoPlane<u8>,
    out: &mut VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mvx_q: i32,
    mvy_q: i32,
) {
    let mvxc = mvx_q / 8;
    let mvyc = mvy_q / 8;
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

fn read_i32(data: &[u8], cur: &mut usize) -> Result<i32, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let v = i32::from_le_bytes([data[*cur], data[*cur + 1], data[*cur + 2], data[*cur + 3]]);
    *cur += 4;
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
#[allow(clippy::too_many_arguments)]
pub fn encode_yuv420_p_payload(
    seq: &VideoSequenceHeaderV2,
    cur: &YuvFrame,
    reference: &YuvFrame,
    frame_index: u32,
    qp: u8,
    settings: &SrsV2EncodeSettings,
    mut stats: Option<&mut ResidualEncodeStats>,
    motion_stats: Option<&mut SrsV2MotionEncodeStats>,
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
    let allow_skip = settings.enable_skip_blocks;

    let mut motion_discard = SrsV2MotionEncodeStats::default();
    let ms = match motion_stats {
        Some(r) => r,
        None => &mut motion_discard,
    };
    ms.motion_search_mode = settings.motion_search_mode;
    ms.sad_evaluations = 0;
    ms.skip_subblocks = 0;
    ms.nonzero_motion_macroblocks = 0;
    ms.sum_mv_l1 = 0;
    let use_subpel = matches!(settings.subpel_mode, SrsV2SubpelMode::HalfPel);
    ms.subpel_enabled = use_subpel;
    ms.subpel_blocks_tested = 0;
    ms.subpel_blocks_selected = 0;
    ms.additional_subpel_evaluations = 0;
    ms.sum_abs_frac_qpel = 0;

    let mut out = Vec::new();
    let magic = if use_subpel {
        match settings.residual_entropy {
            ResidualEntropy::Explicit => FRAME_PAYLOAD_MAGIC_P_SUBPEL,
            ResidualEntropy::Auto | ResidualEntropy::Rans => FRAME_PAYLOAD_MAGIC_P_SUBPEL_ENTROPY,
        }
    } else {
        match settings.residual_entropy {
            ResidualEntropy::Explicit => FRAME_PAYLOAD_MAGIC_P,
            ResidualEntropy::Auto | ResidualEntropy::Rans => FRAME_PAYLOAD_MAGIC_P_ENTROPY,
        }
    };
    out.extend_from_slice(&magic);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(qp);

    let rans_model = residual_token_model();

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let mut mb_evals = 0_u64;
            let (mvx, mvy, mvx_q, mvy_q) = if use_subpel {
                let (ix, iy) = pick_mv(
                    settings.motion_search_mode,
                    &cur.y,
                    &reference.y,
                    mbx,
                    mby,
                    radius,
                    settings.early_exit_sad_threshold,
                    Some(&mut mb_evals),
                );
                ms.sad_evaluations += mb_evals;
                let mut ev = 0_u64;
                let mut tested = 0_u64;
                let sub_r = settings.clamped_subpel_refinement_radius();
                let (qx, qy) = refine_half_pel_center(
                    &cur.y,
                    &reference.y,
                    mbx,
                    mby,
                    ix,
                    iy,
                    sub_r,
                    &mut ev,
                    &mut tested,
                );
                validate_mv_qpel_halfgrid(qx, qy)?;
                ms.additional_subpel_evaluations += ev;
                ms.subpel_blocks_tested += tested;
                if qx != ix as i32 * 4 || qy != iy as i32 * 4 {
                    ms.subpel_blocks_selected += 1;
                }
                let fx = qx.rem_euclid(4) as u32;
                let fy = qy.rem_euclid(4) as u32;
                ms.sum_abs_frac_qpel += (fx + fy) as u64;
                if qx != 0 || qy != 0 {
                    ms.nonzero_motion_macroblocks += 1;
                }
                ms.sum_mv_l1 += ((qx.unsigned_abs() + qy.unsigned_abs()) / 4) as u64;
                (ix, iy, qx, qy)
            } else {
                let (mvx, mvy) = pick_mv(
                    settings.motion_search_mode,
                    &cur.y,
                    &reference.y,
                    mbx,
                    mby,
                    radius,
                    settings.early_exit_sad_threshold,
                    Some(&mut mb_evals),
                );
                ms.sad_evaluations += mb_evals;
                if mvx != 0 || mvy != 0 {
                    ms.nonzero_motion_macroblocks += 1;
                }
                ms.sum_mv_l1 += mvx.unsigned_abs() as u64 + mvy.unsigned_abs() as u64;
                (mvx, mvy, mvx as i32 * 4, mvy as i32 * 4)
            };

            if use_subpel {
                out.extend_from_slice(&mvx_q.to_le_bytes());
                out.extend_from_slice(&mvy_q.to_le_bytes());
            } else {
                out.extend_from_slice(&mvx.to_le_bytes());
                out.extend_from_slice(&mvy.to_le_bytes());
            }

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
                        let pred = if use_subpel {
                            sample_luma_bilinear_qpel(
                                &reference.y,
                                lx as i32,
                                ly as i32,
                                mvx_q,
                                mvy_q,
                            ) as i16
                        } else {
                            let rx = lx as i32 + mvx as i32;
                            let ry = ly as i32 + mvy as i32;
                            sample_u8_plane(&reference.y, rx, ry) as i16
                        };
                        let d = cx - pred;
                        blk[row as usize][col as usize] = d;
                        max_abs = max_abs.max(d.abs());
                    }
                }
                if allow_skip && max_abs <= SKIP_ABS_THRESH {
                    pattern |= 1 << si;
                    ms.skip_subblocks += 1;
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

/// Decode `FR2` rev **2**, **4**, **5**, or **6** P-frame into **reconstructed** YUV420p8 (no loop filter).
///
/// Caller applies [`crate::srsv2::frame_codec::apply_reconstruction_filter_if_enabled`] before display/reference refresh when using raw decode entry points.
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
    if payload.len() < 4 || &payload[0..3] != b"FR2" || !matches!(payload[3], 2 | 4 | 5 | 6) {
        return Err(SrsV2Error::BadMagic);
    }
    let entropy_chunks = matches!(payload[3], 4 | 6);
    let use_qpel = matches!(payload[3], 5 | 6);
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
            let (mvx_q, mvy_q) = if use_qpel {
                let mvx_q = read_i32(payload, &mut cur)?;
                let mvy_q = read_i32(payload, &mut cur)?;
                validate_mv_qpel_halfgrid(mvx_q, mvy_q)?;
                (mvx_q, mvy_q)
            } else {
                let mvx = read_i16(payload, &mut cur)?;
                let mvy = read_i16(payload, &mut cur)?;
                validate_mv(mvx, mvy)?;
                (mvx as i32 * 4, mvy as i32 * 4)
            };
            let pattern = read_u8(payload, &mut cur)?;

            let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];
            for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
                let skip = (pattern & (1 << si)) != 0;
                if skip {
                    for row in 0..8 {
                        for col in 0..8 {
                            let lx = mbx * 16 + dx + col;
                            let ly = mby * 16 + dy + row;
                            let pv = sample_luma_bilinear_qpel(
                                &reference.y,
                                lx as i32,
                                ly as i32,
                                mvx_q,
                                mvy_q,
                            );
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
                            let pred = sample_luma_bilinear_qpel(
                                &reference.y,
                                lx as i32,
                                ly as i32,
                                mvx_q,
                                mvy_q,
                            ) as i32;
                            let pv = (pred + res[row as usize][col as usize] as i32).clamp(0, 255);
                            y_plane.samples[ly as usize * y_plane.stride + lx as usize] = pv as u8;
                        }
                    }
                }
            }

            if use_qpel {
                copy_chroma_mb8_qpel(&reference.u, &mut u_plane, mbx, mby, mvx_q, mvy_q);
                copy_chroma_mb8_qpel(&reference.v, &mut v_plane, mbx, mby, mvx_q, mvy_q);
            } else {
                let mvx = (mvx_q / 4) as i16;
                let mvy = (mvy_q / 4) as i16;
                copy_chroma_mb8(&reference.u, &mut u_plane, mbx, mby, mvx, mvy);
                copy_chroma_mb8(&reference.v, &mut v_plane, mbx, mby, mvx, mvy);
            }
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
    use crate::srsv2::frame_codec::{decode_yuv420_srsv2_payload, encode_yuv420_intra_payload};
    use crate::srsv2::model::{
        ChromaSiting, ColorPrimaries, ColorRange, MatrixCoefficients, SrsVideoProfile,
        TransferFunction,
    };
    use crate::srsv2::rate_control::SrsV2SubpelMode;

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
            deblock_strength: 0,
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
        let i_payload =
            encode_yuv420_intra_payload(&seq, &yuv, 0, qp, &settings, None, None).unwrap();
        let p_payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, qp, &settings, None, None).unwrap();
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
        let p_payload =
            encode_yuv420_p_payload(&seq, &y1, &y0, 1, qp, &settings, None, None).unwrap();
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

    fn compare_luma_mae(a: &VideoPlane<u8>, b: &VideoPlane<u8>) -> f64 {
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
        let mut sum = 0u64;
        let mut n = 0u64;
        for y in 0..a.height {
            for x in 0..a.width {
                sum += a.samples[y as usize * a.stride + x as usize]
                    .abs_diff(b.samples[y as usize * b.stride + x as usize])
                    as u64;
                n += 1;
            }
        }
        sum as f64 / n.max(1) as f64
    }

    fn compare_luma_max_abs(a: &VideoPlane<u8>, b: &VideoPlane<u8>) -> u8 {
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
        let mut m = 0_u8;
        for y in 0..a.height {
            for x in 0..a.width {
                let d = a.samples[y as usize * a.stride + x as usize]
                    .abs_diff(b.samples[y as usize * b.stride + x as usize]);
                m = m.max(d);
            }
        }
        m
    }

    #[test]
    fn identical_frames_skip_enabled_emits_skipped_subblocks() {
        let seq = seq_inter(64, 64);
        let rgb = vec![200_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            enable_skip_blocks: true,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let mut ms = SrsV2MotionEncodeStats::default();
        encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, Some(&mut ms)).unwrap();
        assert!(ms.skip_subblocks > 0);
    }

    #[test]
    fn identical_frames_skip_disabled_emits_zero_skipped_subblocks() {
        let seq = seq_inter(64, 64);
        let rgb = vec![200_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            enable_skip_blocks: false,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let mut ms = SrsV2MotionEncodeStats::default();
        encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, Some(&mut ms)).unwrap();
        assert_eq!(ms.skip_subblocks, 0);
    }

    #[test]
    fn skip_disabled_tracks_small_dc_bias_closer_than_skip_enabled() {
        // Same geometry with MV (0,0): tiny uniform residual (≤ SKIP_ABS_THRESH) is skipped when
        // enable_skip_blocks, wiping the bias; disabled path keeps quantized residuals.
        let seq = seq_inter(64, 64);
        let rgb = vec![120_u8; 64 * 64 * 3];
        let y_ref = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let mut y_cur = y_ref.clone();
        for y in 0..64 {
            for x in 0..64 {
                let ix = y as usize * y_ref.y.stride + x as usize;
                y_cur.y.samples[ix] = y_ref.y.samples[ix].saturating_add(4);
            }
        }
        let qp = 26_u8;
        let on = SrsV2EncodeSettings {
            enable_skip_blocks: true,
            motion_search_mode: crate::srsv2::rate_control::SrsV2MotionSearchMode::None,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let off = SrsV2EncodeSettings {
            enable_skip_blocks: false,
            motion_search_mode: crate::srsv2::rate_control::SrsV2MotionSearchMode::None,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let p_on = encode_yuv420_p_payload(&seq, &y_cur, &y_ref, 1, qp, &on, None, None).unwrap();
        let p_off = encode_yuv420_p_payload(&seq, &y_cur, &y_ref, 1, qp, &off, None, None).unwrap();
        let dec_on = decode_yuv420_p_payload(&seq, &p_on, &y_ref).unwrap();
        let dec_off = decode_yuv420_p_payload(&seq, &p_off, &y_ref).unwrap();
        let mae_on = compare_luma_mae(&y_cur.y, &dec_on.yuv.y);
        let mae_off = compare_luma_mae(&y_cur.y, &dec_off.yuv.y);
        assert!(
            mae_off < mae_on - 0.25,
            "skip disabled should preserve small residuals: off={mae_off} on={mae_on}"
        );
    }

    #[test]
    fn skip_disabled_decode_differs_from_reference_plane() {
        let seq = seq_inter(64, 64);
        let rgb0 = vec![40_u8; 64 * 64 * 3];
        let mut rgb1 = vec![40_u8; 64 * 64 * 3];
        for y in 10..54 {
            for x in 10..54 {
                let i = ((y * 64 + x) * 3) as usize;
                rgb1[i] = 220;
                rgb1[i + 1] = 180;
                rgb1[i + 2] = 140;
            }
        }
        let y0 = rgb888_full_to_yuv420_bt709(&rgb0, 64, 64, ColorRange::Limited).unwrap();
        let y1 = rgb888_full_to_yuv420_bt709(&rgb1, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            enable_skip_blocks: false,
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let payload =
            encode_yuv420_p_payload(&seq, &y1, &y0, 1, 26, &settings, None, None).unwrap();
        let dec = decode_yuv420_p_payload(&seq, &payload, &y0).unwrap();
        assert!(
            compare_luma_max_abs(&dec.yuv.y, &y0.y) > 8,
            "decoded P should diverge from raw reference when current differs"
        );
    }

    #[test]
    fn corrupted_skip_pattern_errors_as_syntax_or_truncated() {
        let seq = seq_inter(16, 16);
        let rgb = vec![100_u8; 16 * 16 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 16, 16, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            enable_skip_blocks: false,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None).unwrap();
        let pat_idx = 9 + 4;
        payload[pat_idx] = 0x0F;
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(
            matches!(err, SrsV2Error::Truncated | SrsV2Error::Syntax(_)),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn corrupted_residual_chunk_length_errors_truncated() {
        let seq = seq_inter(16, 16);
        let rgb = vec![100_u8; 16 * 16 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 16, 16, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            enable_skip_blocks: false,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None).unwrap();
        let chunk_len_idx = 9 + 4 + 1;
        payload[chunk_len_idx..chunk_len_idx + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(
            err,
            SrsV2Error::Truncated | SrsV2Error::Overflow(_)
        ));
    }

    #[test]
    fn identical_p_decode_near_source_with_quant_tolerance() {
        let seq = seq_inter(64, 64);
        let rgb = vec![180_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            enable_skip_blocks: true,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None).unwrap();
        let dec = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap();
        assert!(
            compare_luma_max_abs(&yuv.y, &dec.yuv.y) <= 8,
            "identical MC + tiny residuals should reconstruct within small tolerance"
        );
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

    #[test]
    fn subpel_off_keeps_fr2_rev2_explicit() {
        let seq = seq_inter(64, 64);
        let rgb = vec![200_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            subpel_mode: SrsV2SubpelMode::Off,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let p = encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None).unwrap();
        assert_eq!(p[3], 2);
    }

    #[test]
    fn subpel_half_emits_fr2_rev5_explicit_residual() {
        let seq = seq_inter(64, 64);
        let rgb = vec![200_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            subpel_mode: SrsV2SubpelMode::HalfPel,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let p = encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None).unwrap();
        assert_eq!(p[3], 5);
    }

    #[test]
    fn rev5_odd_qpel_mv_rejected_as_syntax() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            subpel_mode: SrsV2SubpelMode::HalfPel,
            motion_search_mode: crate::srsv2::rate_control::SrsV2MotionSearchMode::None,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None).unwrap();
        let mv_off = 9;
        payload[mv_off..mv_off + 4].copy_from_slice(&1i32.to_le_bytes());
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn half_pel_dispatcher_matches_raw_p_decode_when_loop_filter_off() {
        let mut seq = seq_inter(64, 64);
        seq.disable_loop_filter = true;
        let mut rgb0 = vec![20_u8; 64 * 64 * 3];
        for y in 20..44 {
            for x in 20..44 {
                let i = (y * 64 + x) * 3;
                rgb0[i] = 240;
                rgb0[i + 1] = 240;
                rgb0[i + 2] = 240;
            }
        }
        let mut rgb1 = vec![20_u8; 64 * 64 * 3];
        for y in 20..44 {
            for x in 24..48 {
                let i = (y * 64 + x) * 3;
                rgb1[i] = 240;
                rgb1[i + 1] = 240;
                rgb1[i + 2] = 240;
            }
        }
        let y0 = rgb888_full_to_yuv420_bt709(&rgb0, 64, 64, ColorRange::Limited).unwrap();
        let y1 = rgb888_full_to_yuv420_bt709(&rgb1, 64, 64, ColorRange::Limited).unwrap();
        let qp = 24_u8;
        let settings = SrsV2EncodeSettings {
            subpel_mode: SrsV2SubpelMode::HalfPel,
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let i_payload =
            encode_yuv420_intra_payload(&seq, &y0, 0, qp, &settings, None, None).unwrap();
        let mut slot = None;
        decode_yuv420_srsv2_payload(&seq, &i_payload, &mut slot).unwrap();
        let ref_y = slot.as_ref().unwrap();
        let p_payload =
            encode_yuv420_p_payload(&seq, &y1, ref_y, 1, qp, &settings, None, None).unwrap();
        assert_eq!(p_payload[3], 5);
        let mut slot2 = Some(ref_y.clone());
        let via_disp = decode_yuv420_srsv2_payload(&seq, &p_payload, &mut slot2).unwrap();
        let dec_raw = decode_yuv420_p_payload(&seq, &p_payload, ref_y).unwrap();
        assert_eq!(via_disp.yuv.y.samples, dec_raw.yuv.y.samples);
    }

    #[test]
    fn subpel_encode_populates_motion_stats() {
        let seq = seq_inter(64, 64);
        let rgb = vec![130_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            subpel_mode: SrsV2SubpelMode::HalfPel,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let mut ms = SrsV2MotionEncodeStats::default();
        encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, Some(&mut ms)).unwrap();
        assert!(ms.subpel_enabled);
        assert!(ms.subpel_blocks_tested > 0);
        assert!(ms.additional_subpel_evaluations > 0);
    }
}
