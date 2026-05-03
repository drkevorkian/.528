//! Experimental **B-frame** payload (`FR2` rev **10** integer MV, **11** half-pel, **13** per-MB blend +
//! integer MV, **14** per-MB blend + half-pel MV grid + optional weighted prediction, **16**/**18**
//! compact / entropy MV grids).

use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::inter_mv::{
    decode_mv_grid_compact, encode_mv_grid_compact, mv_compact_grid_delta_statistics,
    rans_decode_mv_bytes, rans_encode_mv_bytes, MV_PREDICTION_MODE_LABEL,
};
use super::limits::{MAX_FRAME_PAYLOAD_BYTES, MAX_MOTION_VECTOR_PELS};
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::motion_search::{
    pick_mv, sad_16x16, sample_u8_plane, SrsV2InterMvBenchStats, SrsV2RdoBenchStats,
};
use super::p_frame_codec::{copy_chroma_mb8, copy_chroma_mb8_qpel};
use super::rate_control::{
    rdo_lambda_effective, ResidualEntropy, SrsV2BMotionSearchMode, SrsV2EncodeSettings,
    SrsV2InterSyntaxMode, SrsV2RdoMode,
};
use super::reference_manager::SrsV2ReferenceManager;
use super::residual_entropy::{decode_p_residual_chunk, encode_p_residual_chunk};
use super::residual_tokens::residual_token_model;
use super::subpel::{
    refine_half_pel_center, sad_16x16_qpel, sample_luma_bilinear_qpel, validate_mv_qpel_halfgrid,
};

pub const FRAME_PAYLOAD_MAGIC_B: [u8; 4] = [b'F', b'R', b'2', 10];
pub const FRAME_PAYLOAD_MAGIC_B_SUBPEL: [u8; 4] = [b'F', b'R', b'2', 11];
/// Per-macroblock blend selection, integer MV only (`FR2` rev **13**).
pub const FRAME_PAYLOAD_MAGIC_B_MB_BLEND: [u8; 4] = [b'F', b'R', b'2', 13];
/// Per-macroblock blend + quarter-pel MVs (half-pel grid only) + optional weighted prediction (`FR2` rev **14**).
pub const FRAME_PAYLOAD_MAGIC_B_MB_BLEND_QP: [u8; 4] = [b'F', b'R', b'2', 14];
/// **B** compact MV deltas — dual median-predicted grids (`FR2` rev **16**).
pub const FRAME_PAYLOAD_MAGIC_B_COMPACT: [u8; 4] = [b'F', b'R', b'2', 16];
/// **B** entropy-coded MV sections (`FR2` rev **18**).
pub const FRAME_PAYLOAD_MAGIC_B_INTER_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 18];

/// [`FR2` rev **16**/**18**] flags after reference slots: bit **0** qpel/half-pel MV grid, bit **1** weighted blend allowed.
const B_INTER_FLAG_SUBPEL: u8 = 1;
const B_INTER_FLAG_WEIGHTED_OK: u8 = 2;

type MvGridCell = (i32, i32);
type MvGridVec = Vec<MvGridCell>;
type DualMvGrids = (MvGridVec, MvGridVec);

/// Fixed weighted-prediction coefficient pairs \(`weight_a`, `weight_b`\); **`sum == 256`** (denominator \(2^8\)).
pub const B_WEIGHTED_PRED_CANDIDATES: [(u8, u8); 5] =
    [(64, 192), (95, 161), (128, 128), (161, 95), (192, 64)];

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BBlendModeWire {
    ForwardOnly = 0,
    BackwardOnly = 1,
    Average = 2,
    /// Weighted luma/chroma blend (`FR2` rev **14** only). **`weight_a + weight_b == 256`** on wire.
    Weighted = 3,
}

impl BBlendModeWire {
    /// Frame-level blend byte (`FR2` rev **10**/**11**): weighted is unsupported.
    pub fn from_u8(v: u8) -> Result<Self, SrsV2Error> {
        match v {
            0 => Ok(Self::ForwardOnly),
            1 => Ok(Self::BackwardOnly),
            2 => Ok(Self::Average),
            3 => Err(SrsV2Error::Unsupported(
                "B-frame weighted blend is not implemented for legacy FR2 rev10/11",
            )),
            _ => Err(SrsV2Error::syntax("unknown B blend mode")),
        }
    }

    /// Per-MB blend (`FR2` rev **13**/**14**). Rev **13** rejects weighted ([`Self::Weighted`]).
    pub fn from_u8_per_mb(fr2_rev14: bool, v: u8) -> Result<Self, SrsV2Error> {
        match v {
            0 => Ok(Self::ForwardOnly),
            1 => Ok(Self::BackwardOnly),
            2 => Ok(Self::Average),
            3 if fr2_rev14 => Ok(Self::Weighted),
            3 => Err(SrsV2Error::Unsupported(
                "B-frame weighted blend requires FR2 rev14 wire",
            )),
            _ => Err(SrsV2Error::syntax("unknown B blend mode")),
        }
    }
}

/// Validate **`weight_a + weight_b == 256`** (integer blend denominator \(2^8\)).
pub fn validate_b_prediction_weights(weight_a: u8, weight_b: u8) -> Result<(), SrsV2Error> {
    let sum = weight_a as u16 + weight_b as u16;
    if sum != 256 {
        return Err(SrsV2Error::syntax(
            "B weighted prediction weights must sum to 256",
        ));
    }
    Ok(())
}

#[inline]
pub fn blend_weighted_pixels(pred_a: i32, pred_b: i32, weight_a: u8, weight_b: u8) -> i32 {
    (pred_a * weight_a as i32 + pred_b * weight_b as i32 + 128) >> 8
}

fn uses_fr2_rev14_wire(settings: &SrsV2EncodeSettings) -> bool {
    matches!(
        settings.b_motion_search_mode,
        SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel
    ) || settings.b_weighted_prediction
}

fn read_u32(data: &[u8], cur: &mut usize) -> Result<u32, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let v = u32::from_le_bytes([data[*cur], data[*cur + 1], data[*cur + 2], data[*cur + 3]]);
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

fn validate_mv_i16(mvx: i16, mvy: i16) -> Result<(), SrsV2Error> {
    if mvx.abs() > MAX_MOTION_VECTOR_PELS || mvy.abs() > MAX_MOTION_VECTOR_PELS {
        return Err(SrsV2Error::CorruptedMotionVector);
    }
    Ok(())
}

fn validate_b_compact_mv_unit(use_qpel: bool, mx: i32, my: i32) -> Result<(), SrsV2Error> {
    if use_qpel {
        validate_mv_qpel_halfgrid(mx, my)
    } else if mx % 4 != 0 || my % 4 != 0 {
        Err(SrsV2Error::syntax("B MV not on integer pel grid"))
    } else {
        validate_mv_i16(mx.div_euclid(4) as i16, my.div_euclid(4) as i16)
    }
}

fn push_chunk(out: &mut Vec<u8>, chunk: &[u8]) -> Result<(), SrsV2Error> {
    let len = u32::try_from(chunk.len()).map_err(|_| SrsV2Error::syntax("b chunk length"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(chunk);
    Ok(())
}

#[derive(Clone)]
struct BMacroblockEncoded {
    blend: BBlendModeWire,
    weight_a: u8,
    weight_b: u8,
    mv_aqx: i32,
    mv_aqy: i32,
    mv_bqx: i32,
    mv_bqy: i32,
    pattern: u8,
    chunks: Vec<Vec<u8>>,
}

#[allow(clippy::too_many_arguments)]
fn build_b_macroblock_encoded(
    ch: &BMbEncodeChoice,
    wire_rev14: bool,
    cur: &YuvFrame,
    ref_a: &YuvFrame,
    ref_b: &YuvFrame,
    mbx: u32,
    mby: u32,
    qp_i: i16,
    model: &libsrs_bitio::RansModel,
) -> Result<BMacroblockEncoded, SrsV2Error> {
    let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];
    let mut pattern = 0_u8;
    let mut chunks: Vec<Vec<u8>> = Vec::new();

    for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
        let mut blk = [[0_i16; 8]; 8];
        let mut max_abs = 0_i16;
        for row in 0..8 {
            for col in 0..8 {
                let lx = mbx * 16 + dx + col;
                let ly = mby * 16 + dy + row;
                let cx = cur.y.samples[ly as usize * cur.y.stride + lx as usize] as i16;
                let pred = match ch.blend {
                    BBlendModeWire::ForwardOnly => {
                        if wire_rev14 {
                            sample_luma_bilinear_qpel(
                                &ref_a.y, lx as i32, ly as i32, ch.mv_aqx, ch.mv_aqy,
                            ) as i16
                        } else {
                            let rx = lx as i32 + ch.mv_ax as i32;
                            let ry = ly as i32 + ch.mv_ay as i32;
                            sample_u8_plane(&ref_a.y, rx, ry) as i16
                        }
                    }
                    BBlendModeWire::BackwardOnly => {
                        if wire_rev14 {
                            sample_luma_bilinear_qpel(
                                &ref_b.y, lx as i32, ly as i32, ch.mv_bqx, ch.mv_bqy,
                            ) as i16
                        } else {
                            let rx = lx as i32 + ch.mv_bx as i32;
                            let ry = ly as i32 + ch.mv_by as i32;
                            sample_u8_plane(&ref_b.y, rx, ry) as i16
                        }
                    }
                    BBlendModeWire::Average => {
                        let pa = if wire_rev14 {
                            sample_luma_bilinear_qpel(
                                &ref_a.y, lx as i32, ly as i32, ch.mv_aqx, ch.mv_aqy,
                            ) as i32
                        } else {
                            let rx = lx as i32 + ch.mv_ax as i32;
                            let ry = ly as i32 + ch.mv_ay as i32;
                            sample_u8_plane(&ref_a.y, rx, ry) as i32
                        };
                        let pb = if wire_rev14 {
                            sample_luma_bilinear_qpel(
                                &ref_b.y, lx as i32, ly as i32, ch.mv_bqx, ch.mv_bqy,
                            ) as i32
                        } else {
                            let rx = lx as i32 + ch.mv_bx as i32;
                            let ry = ly as i32 + ch.mv_by as i32;
                            sample_u8_plane(&ref_b.y, rx, ry) as i32
                        };
                        ((pa + pb + 1) >> 1) as i16
                    }
                    BBlendModeWire::Weighted => {
                        let pa = sample_luma_bilinear_qpel(
                            &ref_a.y, lx as i32, ly as i32, ch.mv_aqx, ch.mv_aqy,
                        ) as i32;
                        let pb = sample_luma_bilinear_qpel(
                            &ref_b.y, lx as i32, ly as i32, ch.mv_bqx, ch.mv_bqy,
                        ) as i32;
                        blend_weighted_pixels(pa, pb, ch.weight_a, ch.weight_b).clamp(0, 255) as i16
                    }
                };
                let d = cx - pred;
                blk[row as usize][col as usize] = d;
                max_abs = max_abs.max(d.abs());
            }
        }
        const SKIP_ABS_THRESH: i16 = 6;
        if max_abs <= SKIP_ABS_THRESH {
            pattern |= 1 << si;
        } else {
            let mut linear = [0_i16; 64];
            for r in 0..8 {
                for c in 0..8 {
                    linear[r * 8 + c] = blk[r][c];
                }
            }
            let f = super::dct::fdct_8x8(&linear);
            let qf = super::intra_codec::quantize(&f, qp_i);
            let (chunk, _) = encode_p_residual_chunk(&qf, ResidualEntropy::Auto, model)?;
            chunks.push(chunk);
        }
    }

    Ok(BMacroblockEncoded {
        blend: ch.blend,
        weight_a: ch.weight_a,
        weight_b: ch.weight_b,
        mv_aqx: ch.mv_aqx,
        mv_aqy: ch.mv_aqy,
        mv_bqx: ch.mv_bqx,
        mv_bqy: ch.mv_bqy,
        pattern,
        chunks,
    })
}

fn append_b_mb_legacy_wire(
    out: &mut Vec<u8>,
    mb: &BMacroblockEncoded,
    wire_rev14: bool,
) -> Result<(), SrsV2Error> {
    out.push(mb.blend as u8);
    if wire_rev14 {
        if mb.blend == BBlendModeWire::Weighted {
            validate_b_prediction_weights(mb.weight_a, mb.weight_b)?;
            out.push(mb.weight_a);
            out.push(mb.weight_b);
        }
        validate_mv_qpel_halfgrid(mb.mv_aqx, mb.mv_aqy)?;
        validate_mv_qpel_halfgrid(mb.mv_bqx, mb.mv_bqy)?;
        out.extend_from_slice(&mb.mv_aqx.to_le_bytes());
        out.extend_from_slice(&mb.mv_aqy.to_le_bytes());
        out.extend_from_slice(&mb.mv_bqx.to_le_bytes());
        out.extend_from_slice(&mb.mv_bqy.to_le_bytes());
    } else {
        let mv_ax = mb.mv_aqx.div_euclid(4) as i16;
        let mv_ay = mb.mv_aqy.div_euclid(4) as i16;
        let mv_bx = mb.mv_bqx.div_euclid(4) as i16;
        let mv_by = mb.mv_bqy.div_euclid(4) as i16;
        validate_mv_i16(mv_ax, mv_ay)?;
        validate_mv_i16(mv_bx, mv_by)?;
        out.extend_from_slice(&mv_ax.to_le_bytes());
        out.extend_from_slice(&mv_ay.to_le_bytes());
        out.extend_from_slice(&mv_bx.to_le_bytes());
        out.extend_from_slice(&mv_by.to_le_bytes());
    }
    out.push(mb.pattern);
    for c in &mb.chunks {
        push_chunk(out, c)?;
    }
    Ok(())
}

fn append_b_mb_compact_residual_wire(
    out: &mut Vec<u8>,
    mb: &BMacroblockEncoded,
    weighted_allowed: bool,
) -> Result<(), SrsV2Error> {
    let tag = mb.blend as u8;
    if tag > 3 {
        return Err(SrsV2Error::syntax("bad B blend tag"));
    }
    out.push(tag);
    if weighted_allowed && mb.blend == BBlendModeWire::Weighted {
        validate_b_prediction_weights(mb.weight_a, mb.weight_b)?;
        out.push(mb.weight_a);
        out.push(mb.weight_b);
    }
    out.push(mb.pattern);
    for c in &mb.chunks {
        push_chunk(out, c)?;
    }
    Ok(())
}

/// Encode minimal **B** picture (`FR2` rev **10** or **11**): adaptive residuals, blend **average** or single-ref modes.
#[allow(clippy::too_many_arguments)]
pub fn encode_yuv420_b_payload(
    seq: &VideoSequenceHeaderV2,
    cur: &YuvFrame,
    ref_a: &YuvFrame,
    ref_b: &YuvFrame,
    frame_index: u32,
    qp: u8,
    slot_a: u8,
    slot_b: u8,
    blend: BBlendModeWire,
    half_pel: bool,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.max_ref_frames < 2 {
        return Err(SrsV2Error::syntax("B-frame requires max_ref_frames >= 2"));
    }
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("B encode YUV420p8 only"));
    }
    if !seq.width.is_multiple_of(16) || !seq.height.is_multiple_of(16) {
        return Err(SrsV2Error::syntax("B-frame requires 16-aligned dimensions"));
    }
    let w = seq.width;
    let h = seq.height;
    let qp_i = qp.max(1) as i16;
    let mb_cols = w / 16;
    let mb_rows = h / 16;
    let model = residual_token_model();
    let magic = if half_pel {
        FRAME_PAYLOAD_MAGIC_B_SUBPEL
    } else {
        FRAME_PAYLOAD_MAGIC_B
    };
    let mut out = Vec::new();
    out.extend_from_slice(&magic);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(qp);
    out.push(slot_a);
    out.push(slot_b);
    out.push(blend as u8);

    let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let mv_ax = 0_i16;
            let mv_ay = 0_i16;
            let mv_bx = 0_i16;
            let mv_by = 0_i16;
            let (mv_aqx, mv_aqy, mv_bqx, mv_bqy) = (0_i32, 0_i32, 0_i32, 0_i32);

            if half_pel {
                out.extend_from_slice(&mv_aqx.to_le_bytes());
                out.extend_from_slice(&mv_aqy.to_le_bytes());
                out.extend_from_slice(&mv_bqx.to_le_bytes());
                out.extend_from_slice(&mv_bqy.to_le_bytes());
            } else {
                out.extend_from_slice(&mv_ax.to_le_bytes());
                out.extend_from_slice(&mv_ay.to_le_bytes());
                out.extend_from_slice(&mv_bx.to_le_bytes());
                out.extend_from_slice(&mv_by.to_le_bytes());
            }

            let mut pattern = 0_u8;
            let mut chunks: Vec<Vec<u8>> = Vec::new();

            for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
                let mut blk = [[0_i16; 8]; 8];
                let mut max_abs = 0_i16;
                for row in 0..8 {
                    for col in 0..8 {
                        let lx = mbx * 16 + dx + col;
                        let ly = mby * 16 + dy + row;
                        let cx = cur.y.samples[ly as usize * cur.y.stride + lx as usize] as i16;
                        let pred = match blend {
                            BBlendModeWire::ForwardOnly => {
                                if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                    ) as i16
                                } else {
                                    let rx = lx as i32 + mv_ax as i32;
                                    let ry = ly as i32 + mv_ay as i32;
                                    sample_u8_plane(&ref_a.y, rx, ry) as i16
                                }
                            }
                            BBlendModeWire::BackwardOnly => {
                                if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                    ) as i16
                                } else {
                                    let rx = lx as i32 + mv_bx as i32;
                                    let ry = ly as i32 + mv_by as i32;
                                    sample_u8_plane(&ref_b.y, rx, ry) as i16
                                }
                            }
                            BBlendModeWire::Average => {
                                let pa = if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                    ) as i32
                                } else {
                                    let rx = lx as i32 + mv_ax as i32;
                                    let ry = ly as i32 + mv_ay as i32;
                                    sample_u8_plane(&ref_a.y, rx, ry) as i32
                                };
                                let pb = if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                    ) as i32
                                } else {
                                    let rx = lx as i32 + mv_bx as i32;
                                    let ry = ly as i32 + mv_by as i32;
                                    sample_u8_plane(&ref_b.y, rx, ry) as i32
                                };
                                ((pa + pb + 1) >> 1) as i16
                            }
                            BBlendModeWire::Weighted => unreachable!(),
                        };
                        let d = cx - pred;
                        blk[row as usize][col as usize] = d;
                        max_abs = max_abs.max(d.abs());
                    }
                }
                const SKIP_ABS_THRESH: i16 = 6;
                if max_abs <= SKIP_ABS_THRESH {
                    pattern |= 1 << si;
                } else {
                    let mut linear = [0_i16; 64];
                    for r in 0..8 {
                        for c in 0..8 {
                            linear[r * 8 + c] = blk[r][c];
                        }
                    }
                    let f = super::dct::fdct_8x8(&linear);
                    let qf = super::intra_codec::quantize(&f, qp_i);
                    let (chunk, _) = encode_p_residual_chunk(
                        &qf,
                        super::rate_control::ResidualEntropy::Auto,
                        &model,
                    )?;
                    chunks.push(chunk);
                }
            }

            out.push(pattern);
            for c in chunks {
                push_chunk(&mut out, &c)?;
            }
        }
    }

    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded B-frame",
        });
    }
    Ok(out)
}

/// Aggregate stats from [`encode_yuv420_b_payload_mb_blend`] (one encoded B picture).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BFrameEncodeStats {
    pub b_forward_macroblocks: u32,
    pub b_backward_macroblocks: u32,
    pub b_average_macroblocks: u32,
    pub b_weighted_macroblocks: u32,
    pub inter_mv: SrsV2InterMvBenchStats,
    pub rdo: SrsV2RdoBenchStats,
    pub b_sad_evaluations: u64,
    pub b_subpel_blocks_tested: u64,
    pub b_subpel_blocks_selected: u64,
    pub b_additional_subpel_evaluations: u64,
    pub b_sum_abs_frac_qpel: u64,
    pub b_forward_halfpel_blocks: u32,
    pub b_backward_halfpel_blocks: u32,
    pub b_weighted_candidates_tested: u64,
    pub b_weighted_sum_weight_a: u64,
    pub b_weighted_sum_weight_b: u64,
}

/// Per-MB encoder decision (integer MVs always kept for chroma approximation / rev13 wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BMbEncodeChoice {
    pub blend: BBlendModeWire,
    pub weight_a: u8,
    pub weight_b: u8,
    pub mv_ax: i16,
    pub mv_ay: i16,
    pub mv_bx: i16,
    pub mv_by: i16,
    pub mv_aqx: i32,
    pub mv_aqy: i32,
    pub mv_bqx: i32,
    pub mv_bqy: i32,
}

fn sad_mb_average_plane(
    cur: &VideoPlane<u8>,
    ref_a: &VideoPlane<u8>,
    ref_b: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mva: (i16, i16),
    mvb: (i16, i16),
) -> u32 {
    let (rax, ray) = (mva.0 as i32, mva.1 as i32);
    let (rbx, rby) = (mvb.0 as i32, mvb.1 as i32);
    let mut acc = 0_u32;
    for row in 0..16 {
        for col in 0..16 {
            let lx = mb_x * 16 + col;
            let ly = mb_y * 16 + row;
            let cx = cur.samples[ly as usize * cur.stride + lx as usize] as i32;
            let pa = sample_u8_plane(ref_a, lx as i32 + rax, ly as i32 + ray) as i32;
            let pb = sample_u8_plane(ref_b, lx as i32 + rbx, ly as i32 + rby) as i32;
            let pred = (pa + pb + 1) >> 1;
            acc += (cx - pred).unsigned_abs() as u32;
        }
    }
    acc
}

fn sad_mb_average_qpel(
    cur: &VideoPlane<u8>,
    ref_a: &VideoPlane<u8>,
    ref_b: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mva_q: (i32, i32),
    mvb_q: (i32, i32),
) -> u32 {
    let mut acc = 0_u32;
    for row in 0..16 {
        for col in 0..16 {
            let lx = mb_x * 16 + col;
            let ly = mb_y * 16 + row;
            let cx = cur.samples[ly as usize * cur.stride + lx as usize] as i32;
            let pa =
                sample_luma_bilinear_qpel(ref_a, lx as i32, ly as i32, mva_q.0, mva_q.1) as i32;
            let pb =
                sample_luma_bilinear_qpel(ref_b, lx as i32, ly as i32, mvb_q.0, mvb_q.1) as i32;
            let pred = (pa + pb + 1) >> 1;
            acc += (cx - pred).unsigned_abs() as u32;
        }
    }
    acc
}

#[allow(clippy::too_many_arguments)]
fn sad_mb_weighted_plane(
    cur: &VideoPlane<u8>,
    ref_a: &VideoPlane<u8>,
    ref_b: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mva: (i16, i16),
    mvb: (i16, i16),
    weight_a: u8,
    weight_b: u8,
) -> u32 {
    let (rax, ray) = (mva.0 as i32, mva.1 as i32);
    let (rbx, rby) = (mvb.0 as i32, mvb.1 as i32);
    let mut acc = 0_u32;
    for row in 0..16 {
        for col in 0..16 {
            let lx = mb_x * 16 + col;
            let ly = mb_y * 16 + row;
            let cx = cur.samples[ly as usize * cur.stride + lx as usize] as i32;
            let pa = sample_u8_plane(ref_a, lx as i32 + rax, ly as i32 + ray) as i32;
            let pb = sample_u8_plane(ref_b, lx as i32 + rbx, ly as i32 + rby) as i32;
            let pred = blend_weighted_pixels(pa, pb, weight_a, weight_b);
            acc += (cx - pred).unsigned_abs() as u32;
        }
    }
    acc
}

#[allow(clippy::too_many_arguments)]
fn sad_mb_weighted_qpel(
    cur: &VideoPlane<u8>,
    ref_a: &VideoPlane<u8>,
    ref_b: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mva_q: (i32, i32),
    mvb_q: (i32, i32),
    weight_a: u8,
    weight_b: u8,
) -> u32 {
    let mut acc = 0_u32;
    for row in 0..16 {
        for col in 0..16 {
            let lx = mb_x * 16 + col;
            let ly = mb_y * 16 + row;
            let cx = cur.samples[ly as usize * cur.stride + lx as usize] as i32;
            let pa =
                sample_luma_bilinear_qpel(ref_a, lx as i32, ly as i32, mva_q.0, mva_q.1) as i32;
            let pb =
                sample_luma_bilinear_qpel(ref_b, lx as i32, ly as i32, mvb_q.0, mvb_q.1) as i32;
            let pred = blend_weighted_pixels(pa, pb, weight_a, weight_b);
            acc += (cx - pred).unsigned_abs() as u32;
        }
    }
    acc
}

/// Choose per-MB blend, optional weights, and MVs for [`encode_yuv420_b_payload_mb_blend`].
#[allow(clippy::too_many_arguments)]
pub fn choose_b_macroblock(
    settings: &SrsV2EncodeSettings,
    cur: &YuvFrame,
    ref_a: &YuvFrame,
    ref_b: &YuvFrame,
    mbx: u32,
    mby: u32,
    frame_qp: u8,
    stats: &mut BFrameEncodeStats,
) -> Result<BMbEncodeChoice, SrsV2Error> {
    let use_halfpel_b = matches!(
        settings.b_motion_search_mode,
        SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel
    );
    let use_weighted = settings.b_weighted_prediction;
    let radius = settings.clamped_motion_search_radius();
    let mode = settings.motion_search_mode;
    let early = settings.early_exit_sad_threshold;

    let ((mv_ax, mv_ay), (mv_bx, mv_by)) = match settings.b_motion_search_mode {
        SrsV2BMotionSearchMode::Off | SrsV2BMotionSearchMode::ReuseP => {
            ((0_i16, 0_i16), (0_i16, 0_i16))
        }
        SrsV2BMotionSearchMode::IndependentForwardBackward
        | SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel => {
            let mut ev_a = 0_u64;
            let mut ev_b = 0_u64;
            let ma = pick_mv(
                mode,
                &cur.y,
                &ref_a.y,
                mbx,
                mby,
                radius,
                early,
                Some(&mut ev_a),
            );
            let mb = pick_mv(
                mode,
                &cur.y,
                &ref_b.y,
                mbx,
                mby,
                radius,
                early,
                Some(&mut ev_b),
            );
            stats.b_sad_evaluations += ev_a + ev_b;
            (ma, mb)
        }
    };

    let (mv_aqx, mv_aqy, mv_bqx, mv_bqy) = if use_halfpel_b {
        let sub_r = settings.clamped_subpel_refinement_radius();
        let mut ev_a = 0_u64;
        let mut tested_a = 0_u64;
        let (aqx, aqy) = refine_half_pel_center(
            &cur.y,
            &ref_a.y,
            mbx,
            mby,
            mv_ax,
            mv_ay,
            sub_r,
            &mut ev_a,
            &mut tested_a,
        );
        stats.b_additional_subpel_evaluations += ev_a;
        stats.b_subpel_blocks_tested += tested_a;
        let mut ev_b = 0_u64;
        let mut tested_b = 0_u64;
        let (bqx, bqy) = refine_half_pel_center(
            &cur.y,
            &ref_b.y,
            mbx,
            mby,
            mv_bx,
            mv_by,
            sub_r,
            &mut ev_b,
            &mut tested_b,
        );
        stats.b_additional_subpel_evaluations += ev_b;
        stats.b_subpel_blocks_tested += tested_b;
        validate_mv_qpel_halfgrid(aqx, aqy)?;
        validate_mv_qpel_halfgrid(bqx, bqy)?;
        if aqx != mv_ax as i32 * 4 || aqy != mv_ay as i32 * 4 {
            stats.b_forward_halfpel_blocks += 1;
        }
        if bqx != mv_bx as i32 * 4 || bqy != mv_by as i32 * 4 {
            stats.b_backward_halfpel_blocks += 1;
        }
        if aqx != mv_ax as i32 * 4
            || aqy != mv_ay as i32 * 4
            || bqx != mv_bx as i32 * 4
            || bqy != mv_by as i32 * 4
        {
            stats.b_subpel_blocks_selected += 1;
        }
        let fx_a = aqx.rem_euclid(4) as u32;
        let fy_a = aqy.rem_euclid(4) as u32;
        let fx_b = bqx.rem_euclid(4) as u32;
        let fy_b = bqy.rem_euclid(4) as u32;
        stats.b_sum_abs_frac_qpel += (fx_a + fy_a + fx_b + fy_b) as u64;
        (aqx, aqy, bqx, bqy)
    } else {
        (
            mv_ax as i32 * 4,
            mv_ay as i32 * 4,
            mv_bx as i32 * 4,
            mv_by as i32 * 4,
        )
    };

    let (sf, sb, sa) = if use_halfpel_b {
        let sf = sad_16x16_qpel(&cur.y, &ref_a.y, mbx, mby, mv_aqx, mv_aqy);
        let sb = sad_16x16_qpel(&cur.y, &ref_b.y, mbx, mby, mv_bqx, mv_bqy);
        let sa = sad_mb_average_qpel(
            &cur.y,
            &ref_a.y,
            &ref_b.y,
            mbx,
            mby,
            (mv_aqx, mv_aqy),
            (mv_bqx, mv_bqy),
        );
        (sf, sb, sa)
    } else {
        let sf = sad_16x16(&cur.y, &ref_a.y, mbx, mby, mv_ax as i32, mv_ay as i32);
        let sb = sad_16x16(&cur.y, &ref_b.y, mbx, mby, mv_bx as i32, mv_by as i32);
        let sa = sad_mb_average_plane(
            &cur.y,
            &ref_a.y,
            &ref_b.y,
            mbx,
            mby,
            (mv_ax, mv_ay),
            (mv_bx, mv_by),
        );
        (sf, sb, sa)
    };
    stats.b_sad_evaluations += 3;

    let hp_pen_bytes = if use_halfpel_b
        && (mv_aqx != mv_ax as i32 * 4
            || mv_aqy != mv_ay as i32 * 4
            || mv_bqx != mv_bx as i32 * 4
            || mv_bqy != mv_by as i32 * 4)
    {
        40i128
    } else {
        0
    };

    let (blend, weight_a, weight_b) = if matches!(settings.rdo_mode, SrsV2RdoMode::Fast) {
        let lam = rdo_lambda_effective(settings, frame_qp) as i128;
        let base_bytes = 8i128;
        let weighted_extra = 72i128;
        let mut cands: Vec<(i128, BBlendModeWire, u8, u8)> = Vec::with_capacity(8);
        let sf_s = sf as i128 + lam * (base_bytes + hp_pen_bytes) / 256;
        let sb_s = sb as i128 + lam * (base_bytes + hp_pen_bytes) / 256;
        let sa_s = sa as i128 + lam * (base_bytes + hp_pen_bytes) / 256;
        cands.push((sf_s, BBlendModeWire::ForwardOnly, 0, 0));
        cands.push((sb_s, BBlendModeWire::BackwardOnly, 0, 0));
        cands.push((sa_s, BBlendModeWire::Average, 0, 0));
        stats.rdo.candidates_tested += 3;
        if use_weighted {
            for &(wa, wb) in &B_WEIGHTED_PRED_CANDIDATES {
                stats.b_weighted_candidates_tested += 1;
                let sw = if use_halfpel_b {
                    sad_mb_weighted_qpel(
                        &cur.y,
                        &ref_a.y,
                        &ref_b.y,
                        mbx,
                        mby,
                        (mv_aqx, mv_aqy),
                        (mv_bqx, mv_bqy),
                        wa,
                        wb,
                    )
                } else {
                    sad_mb_weighted_plane(
                        &cur.y,
                        &ref_a.y,
                        &ref_b.y,
                        mbx,
                        mby,
                        (mv_ax, mv_ay),
                        (mv_bx, mv_by),
                        wa,
                        wb,
                    )
                };
                stats.b_sad_evaluations += 1;
                let sw_s = sw as i128 + lam * (base_bytes + weighted_extra + hp_pen_bytes) / 256;
                stats.rdo.candidates_tested += 1;
                cands.push((sw_s, BBlendModeWire::Weighted, wa, wb));
            }
        }
        fn blend_pri(b: BBlendModeWire) -> u8 {
            match b {
                BBlendModeWire::ForwardOnly => 0,
                BBlendModeWire::BackwardOnly => 1,
                BBlendModeWire::Average => 2,
                BBlendModeWire::Weighted => 3,
            }
        }
        cands.sort_by(|a, b| a.0.cmp(&b.0).then(blend_pri(a.1).cmp(&blend_pri(b.1))));
        let (_, bl, wa, wb) = cands[0];
        stats.rdo.estimated_bits_used_for_decision = stats
            .rdo
            .estimated_bits_used_for_decision
            .saturating_add(base_bytes.max(0) as u64);
        if matches!(bl, BBlendModeWire::Weighted) {
            stats.rdo.estimated_bits_used_for_decision = stats
                .rdo
                .estimated_bits_used_for_decision
                .saturating_add(weighted_extra.max(0) as u64);
        }
        if hp_pen_bytes > 0 {
            stats.rdo.halfpel_decisions += 1;
        }
        (bl, wa, wb)
    } else {
        let mut min_v = sf.min(sb).min(sa);
        let mut best_w: Option<(u8, u8)> = None;
        if use_weighted {
            for &(wa, wb) in &B_WEIGHTED_PRED_CANDIDATES {
                stats.b_weighted_candidates_tested += 1;
                let sw = if use_halfpel_b {
                    sad_mb_weighted_qpel(
                        &cur.y,
                        &ref_a.y,
                        &ref_b.y,
                        mbx,
                        mby,
                        (mv_aqx, mv_aqy),
                        (mv_bqx, mv_bqy),
                        wa,
                        wb,
                    )
                } else {
                    sad_mb_weighted_plane(
                        &cur.y,
                        &ref_a.y,
                        &ref_b.y,
                        mbx,
                        mby,
                        (mv_ax, mv_ay),
                        (mv_bx, mv_by),
                        wa,
                        wb,
                    )
                };
                stats.b_sad_evaluations += 1;
                if sw < min_v {
                    min_v = sw;
                    best_w = Some((wa, wb));
                }
            }
        }

        if sa == min_v {
            (BBlendModeWire::Average, 0_u8, 0_u8)
        } else if sf == min_v {
            (BBlendModeWire::ForwardOnly, 0_u8, 0_u8)
        } else if sb == min_v {
            (BBlendModeWire::BackwardOnly, 0_u8, 0_u8)
        } else {
            let (wa, wb) = best_w.ok_or_else(|| {
                SrsV2Error::syntax("internal: weighted B blend missing after candidate search")
            })?;
            (BBlendModeWire::Weighted, wa, wb)
        }
    };

    match blend {
        BBlendModeWire::ForwardOnly => {
            stats.b_forward_macroblocks += 1;
            stats.rdo.forward_decisions += 1;
        }
        BBlendModeWire::BackwardOnly => {
            stats.b_backward_macroblocks += 1;
            stats.rdo.backward_decisions += 1;
        }
        BBlendModeWire::Average => {
            stats.b_average_macroblocks += 1;
            stats.rdo.average_decisions += 1;
        }
        BBlendModeWire::Weighted => {
            stats.b_weighted_macroblocks += 1;
            stats.b_weighted_sum_weight_a += weight_a as u64;
            stats.b_weighted_sum_weight_b += weight_b as u64;
            stats.rdo.weighted_decisions += 1;
        }
    }

    Ok(BMbEncodeChoice {
        blend,
        weight_a,
        weight_b,
        mv_ax,
        mv_ay,
        mv_bx,
        mv_by,
        mv_aqx,
        mv_aqy,
        mv_bqx,
        mv_bqy,
    })
}

/// Choose per-MB blend and integer MVs for [`encode_yuv420_b_payload_mb_blend`] (compat shim).
#[allow(clippy::too_many_arguments)]
pub fn choose_b_macroblock_blend_and_mv(
    settings: &SrsV2EncodeSettings,
    cur: &YuvFrame,
    ref_a: &YuvFrame,
    ref_b: &YuvFrame,
    mbx: u32,
    mby: u32,
    frame_qp: u8,
    stats: &mut BFrameEncodeStats,
) -> Result<(BBlendModeWire, i16, i16, i16, i16), SrsV2Error> {
    let c = choose_b_macroblock(settings, cur, ref_a, ref_b, mbx, mby, frame_qp, stats)?;
    Ok((c.blend, c.mv_ax, c.mv_ay, c.mv_bx, c.mv_by))
}

/// Encode experimental **B** picture (`FR2` rev **13** or **14**): per-MB blend (+ optional weights) and MVs.
#[allow(clippy::too_many_arguments)]
pub fn encode_yuv420_b_payload_mb_blend(
    seq: &VideoSequenceHeaderV2,
    cur: &YuvFrame,
    ref_a: &YuvFrame,
    ref_b: &YuvFrame,
    frame_index: u32,
    qp: u8,
    slot_a: u8,
    slot_b: u8,
    settings: &SrsV2EncodeSettings,
    stats_out: &mut BFrameEncodeStats,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.max_ref_frames < 2 {
        return Err(SrsV2Error::syntax("B-frame requires max_ref_frames >= 2"));
    }
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("B encode YUV420p8 only"));
    }
    if !seq.width.is_multiple_of(16) || !seq.height.is_multiple_of(16) {
        return Err(SrsV2Error::syntax("B-frame requires 16-aligned dimensions"));
    }
    let w = seq.width;
    let h = seq.height;
    let qp_i = qp.max(1) as i16;
    let mb_cols = w / 16;
    let mb_rows = h / 16;
    let model = residual_token_model();
    let wire_rev14 = uses_fr2_rev14_wire(settings);

    let mut mbs: Vec<BMacroblockEncoded> =
        Vec::with_capacity((mb_cols as usize).saturating_mul(mb_rows as usize));
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let ch = choose_b_macroblock(settings, cur, ref_a, ref_b, mbx, mby, qp, stats_out)?;
            let mb = build_b_macroblock_encoded(
                &ch, wire_rev14, cur, ref_a, ref_b, mbx, mby, qp_i, &model,
            )?;
            mbs.push(mb);
        }
    }

    let mut flags_compact = 0_u8;
    if wire_rev14 {
        flags_compact |= B_INTER_FLAG_SUBPEL;
    }
    if settings.b_weighted_prediction {
        flags_compact |= B_INTER_FLAG_WEIGHTED_OK;
    }

    let mva: Vec<(i32, i32)> = mbs.iter().map(|m| (m.mv_aqx, m.mv_aqy)).collect();
    let mvb: Vec<(i32, i32)> = mbs.iter().map(|m| (m.mv_bqx, m.mv_bqy)).collect();
    let ca = encode_mv_grid_compact(&mva, mb_cols, mb_rows);
    let cb = encode_mv_grid_compact(&mvb, mb_cols, mb_rows);

    let entropy_blobs: Option<(Vec<u8>, Vec<u8>)> =
        if matches!(settings.inter_syntax_mode, SrsV2InterSyntaxMode::EntropyV1) {
            Some((rans_encode_mv_bytes(&ca)?, rans_encode_mv_bytes(&cb)?))
        } else {
            None
        };

    let weighted_allowed = settings.b_weighted_prediction;
    let mut out = Vec::new();
    let inter_prefix_end: usize;
    match settings.inter_syntax_mode {
        SrsV2InterSyntaxMode::RawLegacy => {
            out.extend_from_slice(if wire_rev14 {
                &FRAME_PAYLOAD_MAGIC_B_MB_BLEND_QP
            } else {
                &FRAME_PAYLOAD_MAGIC_B_MB_BLEND
            });
            out.extend_from_slice(&frame_index.to_le_bytes());
            out.push(qp);
            out.push(slot_a);
            out.push(slot_b);
            inter_prefix_end = out.len();
            for mb in &mbs {
                append_b_mb_legacy_wire(&mut out, mb, wire_rev14)?;
            }
        }
        SrsV2InterSyntaxMode::CompactV1 => {
            out.extend_from_slice(&FRAME_PAYLOAD_MAGIC_B_COMPACT);
            out.extend_from_slice(&frame_index.to_le_bytes());
            out.push(qp);
            out.push(slot_a);
            out.push(slot_b);
            out.push(flags_compact);
            out.extend_from_slice(&ca);
            out.extend_from_slice(&cb);
            inter_prefix_end = out.len();
            for mb in &mbs {
                append_b_mb_compact_residual_wire(&mut out, mb, weighted_allowed)?;
            }
        }
        SrsV2InterSyntaxMode::EntropyV1 => {
            let (ba, bb) = entropy_blobs
                .as_ref()
                .ok_or_else(|| SrsV2Error::syntax("internal: B entropy inter blobs missing"))?;
            out.extend_from_slice(&FRAME_PAYLOAD_MAGIC_B_INTER_ENTROPY);
            out.extend_from_slice(&frame_index.to_le_bytes());
            out.push(qp);
            out.push(slot_a);
            out.push(slot_b);
            out.push(flags_compact);
            let sa = u32::try_from(ca.len()).map_err(|_| SrsV2Error::Overflow("b mv a sym"))?;
            let la = u32::try_from(ba.len()).map_err(|_| SrsV2Error::Overflow("b mv a blob"))?;
            let sb = u32::try_from(cb.len()).map_err(|_| SrsV2Error::Overflow("b mv b sym"))?;
            let lb = u32::try_from(bb.len()).map_err(|_| SrsV2Error::Overflow("b mv b blob"))?;
            out.extend_from_slice(&sa.to_le_bytes());
            out.extend_from_slice(&la.to_le_bytes());
            out.extend_from_slice(ba);
            out.extend_from_slice(&sb.to_le_bytes());
            out.extend_from_slice(&lb.to_le_bytes());
            out.extend_from_slice(bb);
            inter_prefix_end = out.len();
            for mb in &mbs {
                append_b_mb_compact_residual_wire(&mut out, mb, weighted_allowed)?;
            }
        }
    }

    let n_mb_u64 = (mb_cols as u64).saturating_mul(mb_rows as u64);
    let (z0a, zna, sum_a, _) = mv_compact_grid_delta_statistics(&mva, mb_cols, mb_rows);
    let (z0b, znb, sum_b, _) = mv_compact_grid_delta_statistics(&mvb, mb_cols, mb_rows);
    let zero_c = z0a + z0b;
    let nonzero_c = zna + znb;
    let sum_abs_c = sum_a + sum_b;
    let denom_c = (zero_c + nonzero_c).max(1);
    let mv_entropy_section_bytes = entropy_blobs
        .as_ref()
        .map(|(a, b)| {
            16_u64
                .saturating_add(a.len() as u64)
                .saturating_add(b.len() as u64)
        })
        .unwrap_or(0);

    let inter_header_bytes = inter_prefix_end as u64;
    let residual_payload_bytes = out.len() as u64 - inter_header_bytes;

    stats_out.inter_mv = SrsV2InterMvBenchStats {
        mv_prediction_mode: MV_PREDICTION_MODE_LABEL,
        mv_raw_bytes_estimate: n_mb_u64.saturating_mul(2).saturating_mul(if wire_rev14 {
            8
        } else {
            4
        }),
        mv_compact_bytes: (ca.len() + cb.len()) as u64,
        mv_entropy_section_bytes,
        mv_delta_zero_varints: zero_c,
        mv_delta_nonzero_varints: nonzero_c,
        mv_delta_sum_abs_components: sum_abs_c,
        mv_delta_avg_abs: sum_abs_c as f64 / denom_c as f64,
        inter_header_bytes,
        residual_payload_bytes,
    };

    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded B-frame",
        });
    }
    Ok(out)
}

/// Decode **B** payload; **`manager`** supplies **`slot_a`** / **`slot_b`** pictures (by linear index).
pub fn decode_yuv420_b_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    manager: &SrsV2ReferenceManager,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if seq.max_ref_frames < 2 {
        return Err(SrsV2Error::syntax("B-frame requires max_ref_frames >= 2"));
    }
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("B decode YUV420p8 only"));
    }
    if payload.len() < 4 + 4 + 1 + 2 {
        return Err(SrsV2Error::Truncated);
    }
    if &payload[0..3] != b"FR2" || !matches!(payload[3], 10 | 11 | 13 | 14 | 16 | 18) {
        return Err(SrsV2Error::BadMagic);
    }
    let rev_byte = payload[3];
    let half_pel_legacy = rev_byte == 11;
    let mb_blend_rev14 = rev_byte == 14;
    let compact_b = matches!(rev_byte, 16 | 18);
    let entropy_b = rev_byte == 18;
    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let qp = read_u8(payload, &mut cur)?;
    let slot_a = read_u8(payload, &mut cur)?;
    let slot_b = read_u8(payload, &mut cur)?;
    let frame_blend = if matches!(rev_byte, 10 | 11) {
        Some(BBlendModeWire::from_u8(read_u8(payload, &mut cur)?)?)
    } else {
        None
    };
    let compact_flags_opt: Option<u8> = if compact_b {
        let fl = read_u8(payload, &mut cur)?;
        if fl & !3 != 0 {
            return Err(SrsV2Error::syntax("unknown B compact inter flags"));
        }
        Some(fl)
    } else {
        None
    };

    let ref_a = manager.frame_at_slot_index(slot_a)?;
    let ref_b = manager.frame_at_slot_index(slot_b)?;
    let fi_a = manager.slot_frame_index(slot_a)?;
    let fi_b = manager.slot_frame_index(slot_b)?;
    if fi_a >= frame_index {
        return Err(SrsV2Error::syntax(
            "B-frame backward reference must use an earlier frame_index",
        ));
    }
    if fi_b <= frame_index {
        return Err(SrsV2Error::syntax(
            "B-frame forward reference must use a later frame_index",
        ));
    }
    if ref_a.format != PixelFormat::Yuv420p8 || ref_b.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("B refs must be YUV420p8"));
    }

    let w = seq.width;
    let h = seq.height;
    if !w.is_multiple_of(16) || !h.is_multiple_of(16) {
        return Err(SrsV2Error::syntax("bad dimensions for B-frame"));
    }
    if ref_a.y.width != w || ref_b.y.width != w {
        return Err(SrsV2Error::syntax("reference geometry mismatch"));
    }

    let mb_cols = w / 16;
    let mb_rows = h / 16;

    let compact_subpel = compact_flags_opt
        .map(|f| f & B_INTER_FLAG_SUBPEL != 0)
        .unwrap_or(false);
    let weighted_allowed_compact = compact_flags_opt
        .map(|f| f & B_INTER_FLAG_WEIGHTED_OK != 0)
        .unwrap_or(false);

    let mv_grids: Option<DualMvGrids> = if compact_b {
        let (ga, gb) = if entropy_b {
            let mut decode_one = || -> Result<MvGridVec, SrsV2Error> {
                let sym_count = read_u32(payload, &mut cur)? as usize;
                let blob_len = read_u32(payload, &mut cur)? as usize;
                let max_compact = (mb_cols as usize)
                    .saturating_mul(mb_rows as usize)
                    .saturating_mul(16)
                    .min(MAX_FRAME_PAYLOAD_BYTES);
                if sym_count > max_compact {
                    return Err(SrsV2Error::syntax("B MV compact length out of range"));
                }
                let blob_end = cur
                    .checked_add(blob_len)
                    .ok_or(SrsV2Error::Overflow("b mv rans blob"))?;
                if blob_end > payload.len() {
                    return Err(SrsV2Error::Truncated);
                }
                let blob = &payload[cur..blob_end];
                cur = blob_end;
                let budget = blob_len.saturating_mul(64).min(512_000);
                let compact = rans_decode_mv_bytes(blob, sym_count, budget)?;
                if compact.len() != sym_count {
                    return Err(SrsV2Error::syntax("B MV rANS output length mismatch"));
                }
                let mut cc = 0usize;
                let g = decode_mv_grid_compact(&compact, &mut cc, mb_cols, mb_rows, |mx, my| {
                    validate_b_compact_mv_unit(compact_subpel, mx, my)
                })?;
                if cc != compact.len() {
                    return Err(SrsV2Error::syntax("B MV compact trailing bytes"));
                }
                Ok(g)
            };
            let ga = decode_one()?;
            let gb = decode_one()?;
            (ga, gb)
        } else {
            let mut cc = cur;
            let ga = decode_mv_grid_compact(payload, &mut cc, mb_cols, mb_rows, |mx, my| {
                validate_b_compact_mv_unit(compact_subpel, mx, my)
            })?;
            let gb = decode_mv_grid_compact(payload, &mut cc, mb_cols, mb_rows, |mx, my| {
                validate_b_compact_mv_unit(compact_subpel, mx, my)
            })?;
            cur = cc;
            (ga, gb)
        };
        Some((ga, gb))
    } else {
        None
    };

    let weighted_allowed = mb_blend_rev14 || weighted_allowed_compact;
    let use_qpel_luma = half_pel_legacy || mb_blend_rev14 || (compact_b && compact_subpel);
    let chroma_qpel = use_qpel_luma;

    let qp_i = qp.max(1) as i16;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let blend = if let Some(b) = frame_blend {
                b
            } else {
                let tag = read_u8(payload, &mut cur)?;
                BBlendModeWire::from_u8_per_mb(weighted_allowed, tag)?
            };
            let (weight_a, weight_b) = if weighted_allowed {
                if blend == BBlendModeWire::Weighted {
                    let wa = read_u8(payload, &mut cur)?;
                    let wb = read_u8(payload, &mut cur)?;
                    validate_b_prediction_weights(wa, wb)?;
                    (wa, wb)
                } else {
                    (0_u8, 0_u8)
                }
            } else {
                (0_u8, 0_u8)
            };
            let (mv_aqx, mv_aqy, mv_bqx, mv_bqy, mv_ax, mv_ay, mv_bx, mv_by) =
                if let Some((ref ga, ref gb)) = mv_grids.as_ref() {
                    let idx = (mby * mb_cols + mbx) as usize;
                    let (ax, ay) = ga[idx];
                    let (bx, by) = gb[idx];
                    if compact_subpel {
                        validate_mv_qpel_halfgrid(ax, ay)?;
                        validate_mv_qpel_halfgrid(bx, by)?;
                        (
                            ax,
                            ay,
                            bx,
                            by,
                            ax.div_euclid(4) as i16,
                            ay.div_euclid(4) as i16,
                            bx.div_euclid(4) as i16,
                            by.div_euclid(4) as i16,
                        )
                    } else if ax % 4 != 0 || ay % 4 != 0 || bx % 4 != 0 || by % 4 != 0 {
                        return Err(SrsV2Error::syntax("B MV not on integer pel grid"));
                    } else {
                        let iax = ax.div_euclid(4) as i16;
                        let iay = ay.div_euclid(4) as i16;
                        let ibx = bx.div_euclid(4) as i16;
                        let iby = by.div_euclid(4) as i16;
                        validate_mv_i16(iax, iay)?;
                        validate_mv_i16(ibx, iby)?;
                        (ax, ay, bx, by, iax, iay, ibx, iby)
                    }
                } else if half_pel_legacy {
                    let ax = read_i32(payload, &mut cur)?;
                    let ay = read_i32(payload, &mut cur)?;
                    let bx = read_i32(payload, &mut cur)?;
                    let by = read_i32(payload, &mut cur)?;
                    validate_mv_qpel_halfgrid(ax, ay)?;
                    validate_mv_qpel_halfgrid(bx, by)?;
                    (ax, ay, bx, by, 0_i16, 0_i16, 0_i16, 0_i16)
                } else if mb_blend_rev14 {
                    let ax = read_i32(payload, &mut cur)?;
                    let ay = read_i32(payload, &mut cur)?;
                    let bx = read_i32(payload, &mut cur)?;
                    let by = read_i32(payload, &mut cur)?;
                    validate_mv_qpel_halfgrid(ax, ay)?;
                    validate_mv_qpel_halfgrid(bx, by)?;
                    (
                        ax,
                        ay,
                        bx,
                        by,
                        ax.div_euclid(4) as i16,
                        ay.div_euclid(4) as i16,
                        bx.div_euclid(4) as i16,
                        by.div_euclid(4) as i16,
                    )
                } else {
                    let ax = read_i16(payload, &mut cur)?;
                    let ay = read_i16(payload, &mut cur)?;
                    let bx = read_i16(payload, &mut cur)?;
                    let by = read_i16(payload, &mut cur)?;
                    validate_mv_i16(ax, ay)?;
                    validate_mv_i16(bx, by)?;
                    (
                        ax as i32 * 4,
                        ay as i32 * 4,
                        bx as i32 * 4,
                        by as i32 * 4,
                        ax,
                        ay,
                        bx,
                        by,
                    )
                };

            let pattern = read_u8(payload, &mut cur)?;

            for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
                let skip = (pattern & (1 << si)) != 0;
                if skip {
                    for row in 0..8 {
                        for col in 0..8 {
                            let lx = mbx * 16 + dx + col;
                            let ly = mby * 16 + dy + row;
                            let pv = match blend {
                                BBlendModeWire::ForwardOnly => {
                                    if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        )
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry)
                                    }
                                }
                                BBlendModeWire::BackwardOnly => {
                                    if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        )
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry)
                                    }
                                }
                                BBlendModeWire::Average => {
                                    let pa = if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry) as i32
                                    };
                                    let pb = if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry) as i32
                                    };
                                    ((pa + pb + 1) >> 1) as u8
                                }
                                BBlendModeWire::Weighted => {
                                    let pa = sample_luma_bilinear_qpel(
                                        &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                    ) as i32;
                                    let pb = sample_luma_bilinear_qpel(
                                        &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                    ) as i32;
                                    blend_weighted_pixels(pa, pb, weight_a, weight_b).clamp(0, 255)
                                        as u8
                                }
                            };
                            y_plane.samples[ly as usize * y_plane.stride + lx as usize] = pv;
                        }
                    }
                } else {
                    let chunk_len = read_u32(payload, &mut cur)? as usize;
                    let end = cur
                        .checked_add(chunk_len)
                        .ok_or(SrsV2Error::Overflow("b residual chunk"))?;
                    if end > payload.len() {
                        return Err(SrsV2Error::Truncated);
                    }
                    let chunk = &payload[cur..end];
                    cur = end;
                    let res = decode_p_residual_chunk(chunk, qp_i)?;
                    for row in 0..8 {
                        for col in 0..8 {
                            let lx = mbx * 16 + dx + col;
                            let ly = mby * 16 + dy + row;
                            let pred = match blend {
                                BBlendModeWire::ForwardOnly => {
                                    if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry) as i32
                                    }
                                }
                                BBlendModeWire::BackwardOnly => {
                                    if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry) as i32
                                    }
                                }
                                BBlendModeWire::Average => {
                                    let pa = if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry) as i32
                                    };
                                    let pb = if use_qpel_luma {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry) as i32
                                    };
                                    (pa + pb + 1) >> 1
                                }
                                BBlendModeWire::Weighted => {
                                    let pa = sample_luma_bilinear_qpel(
                                        &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                    ) as i32;
                                    let pb = sample_luma_bilinear_qpel(
                                        &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                    ) as i32;
                                    blend_weighted_pixels(pa, pb, weight_a, weight_b)
                                }
                            };
                            let pv = (pred + res[row as usize][col as usize] as i32).clamp(0, 255);
                            y_plane.samples[ly as usize * y_plane.stride + lx as usize] = pv as u8;
                        }
                    }
                }
            }

            match blend {
                BBlendModeWire::ForwardOnly => {
                    if chroma_qpel {
                        copy_chroma_mb8_qpel(&ref_a.u, &mut u_plane, mbx, mby, mv_aqx, mv_aqy);
                        copy_chroma_mb8_qpel(&ref_a.v, &mut v_plane, mbx, mby, mv_aqx, mv_aqy);
                    } else {
                        copy_chroma_mb8(&ref_a.u, &mut u_plane, mbx, mby, mv_ax, mv_ay);
                        copy_chroma_mb8(&ref_a.v, &mut v_plane, mbx, mby, mv_ax, mv_ay);
                    }
                }
                BBlendModeWire::BackwardOnly => {
                    if chroma_qpel {
                        copy_chroma_mb8_qpel(&ref_b.u, &mut u_plane, mbx, mby, mv_bqx, mv_bqy);
                        copy_chroma_mb8_qpel(&ref_b.v, &mut v_plane, mbx, mby, mv_bqx, mv_bqy);
                    } else {
                        copy_chroma_mb8(&ref_b.u, &mut u_plane, mbx, mby, mv_bx, mv_by);
                        copy_chroma_mb8(&ref_b.v, &mut v_plane, mbx, mby, mv_bx, mv_by);
                    }
                }
                BBlendModeWire::Average => {
                    for ry in 0..8u32 {
                        for rx in 0..8u32 {
                            let ox = (mbx * 8 + rx) as usize;
                            let oy = (mby * 8 + ry) as usize;
                            if ox >= u_plane.width as usize || oy >= u_plane.height as usize {
                                continue;
                            }
                            let base_x = (mbx * 8) as i32 + rx as i32;
                            let base_y = (mby * 8) as i32 + ry as i32;
                            let ua = if chroma_qpel {
                                sample_u8_plane(&ref_a.u, base_x + mv_aqx / 8, base_y + mv_aqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_a.u,
                                    base_x + (mv_ax as i32) / 2,
                                    base_y + (mv_ay as i32) / 2,
                                ) as i32
                            };
                            let ub = if chroma_qpel {
                                sample_u8_plane(&ref_b.u, base_x + mv_bqx / 8, base_y + mv_bqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_b.u,
                                    base_x + (mv_bx as i32) / 2,
                                    base_y + (mv_by as i32) / 2,
                                ) as i32
                            };
                            u_plane.samples[oy * u_plane.stride + ox] = ((ua + ub + 1) >> 1) as u8;
                            let va = if chroma_qpel {
                                sample_u8_plane(&ref_a.v, base_x + mv_aqx / 8, base_y + mv_aqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_a.v,
                                    base_x + (mv_ax as i32) / 2,
                                    base_y + (mv_ay as i32) / 2,
                                ) as i32
                            };
                            let vb = if chroma_qpel {
                                sample_u8_plane(&ref_b.v, base_x + mv_bqx / 8, base_y + mv_bqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_b.v,
                                    base_x + (mv_bx as i32) / 2,
                                    base_y + (mv_by as i32) / 2,
                                ) as i32
                            };
                            v_plane.samples[oy * v_plane.stride + ox] = ((va + vb + 1) >> 1) as u8;
                        }
                    }
                }
                BBlendModeWire::Weighted => {
                    for ry in 0..8u32 {
                        for rx in 0..8u32 {
                            let ox = (mbx * 8 + rx) as usize;
                            let oy = (mby * 8 + ry) as usize;
                            if ox >= u_plane.width as usize || oy >= u_plane.height as usize {
                                continue;
                            }
                            let base_x = (mbx * 8) as i32 + rx as i32;
                            let base_y = (mby * 8) as i32 + ry as i32;
                            let ua = if chroma_qpel {
                                sample_u8_plane(&ref_a.u, base_x + mv_aqx / 8, base_y + mv_aqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_a.u,
                                    base_x + (mv_ax as i32) / 2,
                                    base_y + (mv_ay as i32) / 2,
                                ) as i32
                            };
                            let ub = if chroma_qpel {
                                sample_u8_plane(&ref_b.u, base_x + mv_bqx / 8, base_y + mv_bqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_b.u,
                                    base_x + (mv_bx as i32) / 2,
                                    base_y + (mv_by as i32) / 2,
                                ) as i32
                            };
                            let pu = blend_weighted_pixels(ua, ub, weight_a, weight_b).clamp(0, 255)
                                as u8;
                            u_plane.samples[oy * u_plane.stride + ox] = pu;
                            let va = if chroma_qpel {
                                sample_u8_plane(&ref_a.v, base_x + mv_aqx / 8, base_y + mv_aqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_a.v,
                                    base_x + (mv_ax as i32) / 2,
                                    base_y + (mv_ay as i32) / 2,
                                ) as i32
                            };
                            let vb = if chroma_qpel {
                                sample_u8_plane(&ref_b.v, base_x + mv_bqx / 8, base_y + mv_bqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_b.v,
                                    base_x + (mv_bx as i32) / 2,
                                    base_y + (mv_by as i32) / 2,
                                ) as i32
                            };
                            let pv = blend_weighted_pixels(va, vb, weight_a, weight_b).clamp(0, 255)
                                as u8;
                            v_plane.samples[oy * v_plane.stride + ox] = pv;
                        }
                    }
                }
            }
        }
    }

    if cur != payload.len() {
        return Err(SrsV2Error::syntax("trailing B-frame bytes"));
    }

    Ok(DecodedVideoFrameV2 {
        frame_index,
        width: w,
        height: h,
        is_displayable: true,
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
    use crate::srsv2::color::gray8_packed_to_yuv420p8_neutral;
    use crate::srsv2::frame_codec::{
        decode_yuv420_srsv2_payload, encode_yuv420_inter_payload, encode_yuv420_intra_payload,
    };
    use crate::srsv2::model::{
        ChromaSiting, ColorPrimaries, ColorRange, MatrixCoefficients, SrsVideoProfile,
        TransferFunction,
    };
    use crate::srsv2::rate_control::{
        ResidualEntropy, SrsV2BMotionSearchMode, SrsV2EncodeSettings, SrsV2InterSyntaxMode,
        SrsV2MotionSearchMode,
    };

    fn seq_b(w: u32, h: u32) -> VideoSequenceHeaderV2 {
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
            max_ref_frames: 2,
        }
    }

    fn flat_yuv(w: u32, h: u32, yv: u8) -> YuvFrame {
        let g = vec![yv; (w * h) as usize];
        gray8_packed_to_yuv420p8_neutral(&g, w, h).unwrap()
    }

    #[test]
    fn b_identical_refs_small_payload() {
        let seq = seq_b(64, 64);
        let gray = vec![99_u8; 64 * 64];
        let yuv = gray8_packed_to_yuv420p8_neutral(&gray, 64, 64).unwrap();
        let pay = encode_yuv420_b_payload(
            &seq,
            &yuv,
            &yuv,
            &yuv,
            2,
            28,
            0,
            1,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        assert!(pay.len() < 8000, "payload {}", pay.len());
    }

    #[test]
    fn weighted_blend_wire_mode_is_reserved() {
        assert!(BBlendModeWire::from_u8(3).is_err());
    }

    #[test]
    fn b_average_blend_decode_near_lossless_flat_scene() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let pay = encode_yuv420_b_payload(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            18,
            1,
            0,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let dec = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap();
        let mut max_abs = 0_u8;
        for i in 0..dec.yuv.y.samples.len() {
            let d = dec.yuv.y.samples[i].abs_diff(cur.y.samples[i]);
            max_abs = max_abs.max(d);
        }
        assert!(
            max_abs <= 8,
            "expected small quantization error on flat average-blend B, got max_abs={max_abs}"
        );
    }

    #[test]
    fn decode_b_missing_backward_slot_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 90);
        let ref_a = flat_yuv(16, 16, 70);
        let ref_b = flat_yuv(16, 16, 110);
        let pay = encode_yuv420_b_payload(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(6, ref_b);
        assert!(decode_yuv420_b_payload(&seq, &pay, &mgr).is_err());
    }

    #[test]
    fn decode_b_missing_forward_slot_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 90);
        let ref_a = flat_yuv(16, 16, 70);
        let ref_b = flat_yuv(16, 16, 110);
        let pay = encode_yuv420_b_payload(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        assert!(decode_yuv420_b_payload(&seq, &pay, &mgr).is_err());
    }

    #[test]
    fn decode_b_invalid_slot_index_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 90);
        let ref_a = flat_yuv(16, 16, 70);
        let ref_b = flat_yuv(16, 16, 110);
        let mut pay = encode_yuv420_b_payload(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        pay[9] = 7;
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        assert!(decode_yuv420_b_payload(&seq, &pay, &mgr).is_err());
    }

    #[test]
    fn decode_b_weighted_blend_wire_rejected() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 90);
        let ref_a = flat_yuv(16, 16, 70);
        let ref_b = flat_yuv(16, 16, 110);
        let mut pay = encode_yuv420_b_payload(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        pay[11] = 3;
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let err = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap_err();
        assert!(matches!(err, SrsV2Error::Unsupported(_)));
    }

    #[test]
    fn decode_b_truncated_payload_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 90);
        let ref_a = flat_yuv(16, 16, 70);
        let ref_b = flat_yuv(16, 16, 110);
        let mut pay = encode_yuv420_b_payload(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        pay.truncate(12);
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        assert!(decode_yuv420_b_payload(&seq, &pay, &mgr).is_err());
    }

    #[test]
    fn decode_b_rev11_odd_qpel_mv_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 90);
        let ref_b = flat_yuv(16, 16, 110);
        let mut pay = encode_yuv420_b_payload(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            BBlendModeWire::Average,
            true,
        )
        .unwrap();
        assert_eq!(pay[3], 11);
        pay[12..16].copy_from_slice(&1_i32.to_le_bytes());
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        assert!(decode_yuv420_b_payload(&seq, &pay, &mgr).is_err());
    }

    #[test]
    fn b_rev13_roundtrip_managed_refs() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            ..Default::default()
        };
        let pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(pay[3], 13);
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let dec = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap();
        assert_eq!(dec.frame_index, 5);
        assert!(dec.is_displayable);
    }

    #[test]
    fn b_compact_rev16_roundtrip_matches_legacy_luma() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let base = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            ..Default::default()
        };
        let raw_settings = SrsV2EncodeSettings {
            inter_syntax_mode: SrsV2InterSyntaxMode::RawLegacy,
            ..base.clone()
        };
        let compact_settings = SrsV2EncodeSettings {
            inter_syntax_mode: SrsV2InterSyntaxMode::CompactV1,
            ..base
        };
        let mut st = BFrameEncodeStats::default();
        let mut st2 = BFrameEncodeStats::default();
        let pay_raw = encode_yuv420_b_payload_mb_blend(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            &raw_settings,
            &mut st,
        )
        .unwrap();
        let pay_co = encode_yuv420_b_payload_mb_blend(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            &compact_settings,
            &mut st2,
        )
        .unwrap();
        assert_eq!(pay_co[3], 16);
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let d_raw = decode_yuv420_b_payload(&seq, &pay_raw, &mgr).unwrap();
        let d_co = decode_yuv420_b_payload(&seq, &pay_co, &mgr).unwrap();
        assert_eq!(d_raw.yuv.y.samples, d_co.yuv.y.samples);
    }

    #[test]
    fn b_entropy_rev18_roundtrip_matches_compact_luma() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let compact_settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            inter_syntax_mode: SrsV2InterSyntaxMode::CompactV1,
            ..Default::default()
        };
        let entropy_settings = SrsV2EncodeSettings {
            inter_syntax_mode: SrsV2InterSyntaxMode::EntropyV1,
            ..compact_settings.clone()
        };
        let mut st = BFrameEncodeStats::default();
        let mut st2 = BFrameEncodeStats::default();
        let pay_co = encode_yuv420_b_payload_mb_blend(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            &compact_settings,
            &mut st,
        )
        .unwrap();
        let pay_en = encode_yuv420_b_payload_mb_blend(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            &entropy_settings,
            &mut st2,
        )
        .unwrap();
        assert_eq!(pay_en[3], 18);
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let d_co = decode_yuv420_b_payload(&seq, &pay_co, &mgr).unwrap();
        let d_en = decode_yuv420_b_payload(&seq, &pay_en, &mgr).unwrap();
        assert_eq!(d_co.yuv.y.samples, d_en.yuv.y.samples);
    }

    #[test]
    fn b_compact_rev16_truncated_mv_grid_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            inter_syntax_mode: SrsV2InterSyntaxMode::CompactV1,
            ..Default::default()
        };
        let mut st = BFrameEncodeStats::default();
        let mut pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(pay[3], 16);
        pay.truncate(12);
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let err = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap_err();
        assert!(matches!(err, SrsV2Error::Truncated));
    }

    #[test]
    fn b_compact_rev16_mv_varint_truncated_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            inter_syntax_mode: SrsV2InterSyntaxMode::CompactV1,
            ..Default::default()
        };
        let mut st = BFrameEncodeStats::default();
        let mut pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        pay.truncate(13);
        pay.push(0x80);
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let err = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap_err();
        assert!(matches!(err, SrsV2Error::Truncated));
    }

    #[test]
    fn b_entropy_rev18_first_mv_sym_count_out_of_range_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            inter_syntax_mode: SrsV2InterSyntaxMode::EntropyV1,
            ..Default::default()
        };
        let mut st = BFrameEncodeStats::default();
        let mut pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(pay[3], 18);
        pay[12..16].copy_from_slice(&99999u32.to_le_bytes());
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let err = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn b_rev13_first_mb_weighted_blend_wire_rejected() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 90);
        let ref_a = flat_yuv(16, 16, 70);
        let ref_b = flat_yuv(16, 16, 110);
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings::default();
        let mut pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        // Header ends at index 10 (0-based): magic4 + fi4 + qp + slot_a + slot_b → next is first MB blend.
        pay[11] = BBlendModeWire::Weighted as u8;
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let err = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap_err();
        assert!(matches!(err, SrsV2Error::Unsupported(_)));
    }

    #[test]
    fn b_rev13_cur_matches_forward_ref_prefers_forward_blend() {
        let seq = seq_b(16, 16);
        let ref_a = flat_yuv(16, 16, 50);
        let ref_b = flat_yuv(16, 16, 200);
        let cur = ref_a.clone();
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            ..Default::default()
        };
        let _ = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(st.b_forward_macroblocks, 1);
        assert_eq!(st.b_weighted_macroblocks, 0);
    }

    #[test]
    fn b_rev13_cur_matches_backward_ref_prefers_backward_blend() {
        let seq = seq_b(16, 16);
        let ref_a = flat_yuv(16, 16, 40);
        let ref_b = flat_yuv(16, 16, 90);
        let cur = ref_b.clone();
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            ..Default::default()
        };
        let _ = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(st.b_backward_macroblocks, 1);
        assert_eq!(st.b_weighted_macroblocks, 0);
    }

    #[test]
    fn b_rev13_flat_midpoint_prefers_average_blend() {
        let seq = seq_b(16, 16);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let cur = flat_yuv(16, 16, 100);
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            ..Default::default()
        };
        let _ = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(st.b_average_macroblocks, 1);
    }

    #[test]
    fn b_rev13_independent_motion_search_collects_extra_sad_evaluations() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let mut st_off = BFrameEncodeStats::default();
        let s_off = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            motion_search_mode: SrsV2MotionSearchMode::Diamond,
            motion_search_radius: 4,
            ..Default::default()
        };
        let _ = encode_yuv420_b_payload_mb_blend(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            &s_off,
            &mut st_off,
        )
        .unwrap();
        let mut st_ind = BFrameEncodeStats::default();
        let s_ind = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::IndependentForwardBackward,
            motion_search_mode: SrsV2MotionSearchMode::Diamond,
            motion_search_radius: 4,
            ..Default::default()
        };
        let _ = encode_yuv420_b_payload_mb_blend(
            &seq,
            &cur,
            &ref_a,
            &ref_b,
            5,
            28,
            1,
            0,
            &s_ind,
            &mut st_ind,
        )
        .unwrap();
        assert!(st_ind.b_sad_evaluations > st_off.b_sad_evaluations);
    }

    #[test]
    fn weighted_prediction_blend_math_deterministic() {
        assert_eq!(blend_weighted_pixels(100, 200, 128, 128), 150);
        assert_eq!(blend_weighted_pixels(100, 200, 64, 192), 175);
        assert_eq!(blend_weighted_pixels(40, 80, 192, 64), 50);
    }

    #[test]
    fn validate_b_prediction_weights_rejects_bad_sum() {
        assert!(validate_b_prediction_weights(100, 100).is_err());
        assert!(validate_b_prediction_weights(1, 254).is_err());
        assert!(validate_b_prediction_weights(128, 128).is_ok());
    }

    #[test]
    fn b_rev13_magic_with_independent_integer_me() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::IndependentForwardBackward,
            motion_search_mode: SrsV2MotionSearchMode::Diamond,
            motion_search_radius: 4,
            ..Default::default()
        };
        let pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(pay[3], 13);
    }

    #[test]
    fn b_rev14_magic_with_half_pel_b_motion_mode() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel,
            motion_search_mode: SrsV2MotionSearchMode::Diamond,
            motion_search_radius: 4,
            ..Default::default()
        };
        let pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(pay[3], 14);
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        let dec = decode_yuv420_b_payload(&seq, &pay, &mgr).unwrap();
        assert_eq!(dec.frame_index, 5);
    }

    #[test]
    fn decode_b_rev14_odd_qpel_mv_fails() {
        let seq = seq_b(16, 16);
        let cur = flat_yuv(16, 16, 100);
        let ref_a = flat_yuv(16, 16, 80);
        let ref_b = flat_yuv(16, 16, 120);
        let mut st = BFrameEncodeStats::default();
        let settings = SrsV2EncodeSettings {
            b_motion_search_mode: SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel,
            motion_search_mode: SrsV2MotionSearchMode::Diamond,
            motion_search_radius: 4,
            ..Default::default()
        };
        let mut pay = encode_yuv420_b_payload_mb_blend(
            &seq, &cur, &ref_a, &ref_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap();
        assert_eq!(pay[3], 14);
        // First MB: blend @ 11, then four i32 MVs @ 12..28
        pay[12] = 1;
        pay[13] = 0;
        pay[14] = 0;
        pay[15] = 0;
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        mgr.push_displayable_last(4, ref_a);
        mgr.push_displayable_last(6, ref_b);
        assert!(decode_yuv420_b_payload(&seq, &pay, &mgr).is_err());
    }

    #[test]
    fn legacy_intra_then_inter_rev2_still_decodes() {
        let seq = VideoSequenceHeaderV2::intra_main_yuv420_bt709_limited_one_ref(16, 16);
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Explicit,
            ..Default::default()
        };
        let yuv0 = flat_yuv(16, 16, 0x33);
        let yuv1 = flat_yuv(16, 16, 0xCC);
        let enc0 = encode_yuv420_intra_payload(&seq, &yuv0, 0, 28, &st, None, None).unwrap();
        let mut slot = None;
        decode_yuv420_srsv2_payload(&seq, &enc0, &mut slot).unwrap();
        let enc1 =
            encode_yuv420_inter_payload(&seq, &yuv1, slot.as_ref(), 1, 28, &st, None, None, None)
                .unwrap();
        assert_eq!(enc1[3], 2);
        decode_yuv420_srsv2_payload(&seq, &enc1, &mut slot).unwrap();
    }
}
