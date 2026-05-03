//! Experimental SRSV2 P-frame payload (`FR2` revision **2** legacy tuples, **4** adaptive entropy): 16×16 integer-pel MC + 8×8 residuals.
//!
//! Half-pel luma (`FR2` rev **5** explicit / **6** entropy) stores MVs in **quarter-pel units** (`±2` == half-pel).
//!
//! Chroma is predicted by copying reference U/V with integer MV (`mv/2` integer path; `mv_q/8` half-pel path).

use super::adaptive_quant::{accumulate_block_aq_wire_plane, SrsV2BlockAqWireStats};
use super::block_aq::{
    apply_qp_delta_clamped, choose_block_qp_delta, collect_p_subblock_variances,
    validate_qp_clip_range, validate_wire_qp_delta,
};
use super::dct::fdct_8x8;
use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::inter_mv::{
    decode_mv_grid_compact, encode_mv_grid_compact, mv_compact_grid_delta_statistics,
    predict_mv_qpel, rans_decode_mv_bytes, rans_encode_mv_bytes, signed_varint_wire_bytes,
    MV_PREDICTION_MODE_LABEL,
};
use super::intra_codec::{decode_residual_block_8x8, encode_residual_block_8x8, quantize};
use super::limits::{MAX_FRAME_PAYLOAD_BYTES, MAX_MOTION_VECTOR_PELS};
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::motion_search::{pick_mv, sad_16x16, sample_u8_plane, SrsV2MotionEncodeStats};
use super::rate_control::{
    rdo_lambda_effective, ResidualEncodeStats, ResidualEntropy, SrsV2BlockAqMode,
    SrsV2EncodeSettings, SrsV2InterSyntaxMode, SrsV2RdoMode, SrsV2SubpelMode,
};
use super::residual_entropy::{
    decode_p_residual_chunk, encode_p_residual_chunk, BlockResidualCoding, PResidualChunkKind,
};
use super::residual_tokens::residual_token_model;
use super::subpel::{
    refine_half_pel_center, sad_16x16_qpel, sample_luma_bilinear_qpel, validate_mv_qpel_halfgrid,
};
use libsrs_bitio::RansModel;

pub const FRAME_PAYLOAD_MAGIC_P: [u8; 4] = [b'F', b'R', b'2', 2];
pub const FRAME_PAYLOAD_MAGIC_P_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 4];
/// Half-pel P-frame, tuple residuals (same layout as rev **2** after MV width).
pub const FRAME_PAYLOAD_MAGIC_P_SUBPEL: [u8; 4] = [b'F', b'R', b'2', 5];
/// Half-pel P-frame, adaptive residual chunks (same layout as rev **4** after MV width).
pub const FRAME_PAYLOAD_MAGIC_P_SUBPEL_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 6];
/// Integer MV P-frame + adaptive residuals + per-chunk **`qp_delta`** (clip range after base QP).
pub const FRAME_PAYLOAD_MAGIC_P_BLOCK_AQ: [u8; 4] = [b'F', b'R', b'2', 8];
/// Half-pel P-frame + adaptive residuals + per-chunk **`qp_delta`**.
pub const FRAME_PAYLOAD_MAGIC_P_SUBPEL_BLOCK_AQ: [u8; 4] = [b'F', b'R', b'2', 9];
/// P-frame **compact** MV deltas (`FR2` rev **15**).
pub const FRAME_PAYLOAD_MAGIC_P_COMPACT: [u8; 4] = [b'F', b'R', b'2', 15];
/// P-frame **entropy-coded** MV section (`FR2` rev **17**).
pub const FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 17];

/// One macroblock worth of P residuals (MV stored separately for compact modes).
#[derive(Clone)]
pub(crate) struct PMacroblockEncoded {
    pub mvx_q: i32,
    pub mvy_q: i32,
    pub pattern: u8,
    pub residual_entries: Vec<(i8, Vec<u8>)>,
}

impl PMacroblockEncoded {
    #[allow(dead_code)] // Used by tooling / benchmarks for residual vs MV accounting.
    pub(crate) fn residual_payload_bytes(&self) -> usize {
        self.residual_entries.iter().map(|(_, c)| c.len()).sum()
    }
}

/// Default skip threshold when [`SrsV2EncodeSettings::enable_skip_blocks`] is true (Y only).
const SKIP_ABS_THRESH: i16 = 6;

/// [`FR2` rev **15**/**17**] flags byte after `base_qp` (only low 3 bits defined).
const P_INTER_FLAG_SUBPEL: u8 = 1;
const P_INTER_FLAG_BLOCK_AQ: u8 = 2;
const P_INTER_FLAG_ENTROPY_RESIDUAL: u8 = 4;

/// λ·bits score helper: `distortion + lam * wire_bytes`.
#[inline]
fn rdo_score_p_mb(sad: u32, lam: i64, wire_bytes: i64) -> i128 {
    sad as i128 + (lam as i128 * wire_bytes.max(0) as i128) / 256
}

fn mb_luma_sad_with_mv_q(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mvx_q: i32,
    mvy_q: i32,
    use_subpel: bool,
) -> u32 {
    if use_subpel {
        sad_16x16_qpel(cur, refp, mb_x, mb_y, mvx_q, mvy_q)
    } else {
        sad_16x16(cur, refp, mb_x, mb_y, mvx_q / 4, mvy_q / 4)
    }
}

fn mv_wire_cost_bytes_estimate(
    mode: SrsV2InterSyntaxMode,
    use_subpel: bool,
    mbx: u32,
    mby: u32,
    mb_cols: u32,
    grid_so_far: &[(i32, i32)],
    mv: (i32, i32),
) -> i64 {
    match mode {
        SrsV2InterSyntaxMode::RawLegacy => {
            if use_subpel {
                8
            } else {
                4
            }
        }
        SrsV2InterSyntaxMode::CompactV1 | SrsV2InterSyntaxMode::EntropyV1 => {
            let (px, py) = predict_mv_qpel(mbx, mby, mb_cols, grid_so_far);
            let dx = mv.0 - px;
            let dy = mv.1 - py;
            (signed_varint_wire_bytes(dx) + signed_varint_wire_bytes(dy)) as i64
        }
    }
}

fn p_mb_residual_wire_bytes(mb: &PMacroblockEncoded, block_aq_wire: bool) -> u64 {
    let mut n = 1_u64;
    for (_qp_delta, chunk) in &mb.residual_entries {
        if block_aq_wire {
            n += 1;
        }
        n += 4 + chunk.len() as u64;
    }
    n
}

fn validate_mv(mvx: i16, mvy: i16) -> Result<(), SrsV2Error> {
    if mvx.abs() > MAX_MOTION_VECTOR_PELS || mvy.abs() > MAX_MOTION_VECTOR_PELS {
        return Err(SrsV2Error::CorruptedMotionVector);
    }
    Ok(())
}

pub(crate) fn copy_chroma_mb8_qpel(
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

pub(crate) fn copy_chroma_mb8(
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

#[allow(clippy::too_many_arguments)]
fn encode_p_macroblock(
    cur: &YuvFrame,
    reference: &YuvFrame,
    mbx: u32,
    mby: u32,
    mb_cols: u32,
    mvx_q: i32,
    mvy_q: i32,
    use_subpel: bool,
    allow_skip: bool,
    qp: u8,
    block_aq_wire: bool,
    sub_vars: &[u32],
    median_var: u64,
    settings: &SrsV2EncodeSettings,
    rans_model: &RansModel,
    stats: &mut Option<&mut ResidualEncodeStats>,
    ms: &mut SrsV2MotionEncodeStats,
    block_wire_acc: &mut Option<&mut SrsV2BlockAqWireStats>,
) -> Result<PMacroblockEncoded, SrsV2Error> {
    let mvx_i = (mvx_q / 4) as i16;
    let mvy_i = (mvy_q / 4) as i16;
    let mut pattern = 0_u8;
    let mut residual_entries: Vec<(i8, Vec<u8>)> = Vec::new();

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
                    sample_luma_bilinear_qpel(&reference.y, lx as i32, ly as i32, mvx_q, mvy_q)
                        as i16
                } else {
                    let rx = lx as i32 + mvx_i as i32;
                    let ry = ly as i32 + mvy_i as i32;
                    sample_u8_plane(&reference.y, rx, ry) as i16
                };
                let d = cx - pred;
                blk[row as usize][col as usize] = d;
                max_abs = max_abs.max(d.abs());
            }
        }
        let idx = ((mby * mb_cols + mbx) * 4 + si as u32) as usize;
        let qp_delta = if block_aq_wire {
            let bv = sub_vars[idx];
            choose_block_qp_delta(
                bv,
                median_var,
                settings.aq_strength,
                settings.min_block_qp_delta,
                settings.max_block_qp_delta,
            )
        } else {
            0_i8
        };
        if block_aq_wire {
            validate_wire_qp_delta(qp_delta)?;
        }
        let eff_qp_u8 = if block_aq_wire {
            apply_qp_delta_clamped(qp, qp_delta, settings.min_qp, settings.max_qp)
        } else {
            qp
        };
        let eff_i = eff_qp_u8.max(1) as i16;

        if allow_skip && max_abs <= SKIP_ABS_THRESH {
            if matches!(settings.rdo_mode, SrsV2RdoMode::Fast) {
                let (chunk, kind) =
                    if matches!(settings.residual_entropy, ResidualEntropy::Explicit) {
                        let mut c = Vec::new();
                        encode_residual_block_8x8(&blk, eff_i, &mut c)?;
                        (c, None)
                    } else {
                        let mut linear = [0_i16; 64];
                        for r in 0..8 {
                            for c in 0..8 {
                                linear[r * 8 + c] = blk[r][c];
                            }
                        }
                        let f = fdct_8x8(&linear);
                        let qf = quantize(&f, eff_i);
                        let (c, k) =
                            encode_p_residual_chunk(&qf, settings.residual_entropy, rans_model)?;
                        (c, Some(k))
                    };
                let wire_res = (4 + chunk.len()) as i64 + if block_aq_wire { 1 } else { 0 };
                let lam = rdo_lambda_effective(settings, qp);
                ms.rdo.candidates_tested += 2;
                ms.rdo.estimated_bits_used_for_decision = ms
                    .rdo
                    .estimated_bits_used_for_decision
                    .saturating_add(wire_res.max(0) as u64);
                let lhs = i128::from(max_abs as i32) * 256;
                let rhs = i128::from(lam) * i128::from(wire_res.max(1));
                if lhs <= rhs {
                    pattern |= 1 << si;
                    ms.skip_subblocks += 1;
                    ms.rdo.skip_decisions += 1;
                    ms.rdo.no_residual_decisions += 1;
                    continue;
                }
                ms.rdo.residual_decisions += 1;
                if matches!(settings.residual_entropy, ResidualEntropy::Explicit) {
                    if let Some(s) = stats.as_mut() {
                        s.p_explicit_chunks += 1;
                    }
                } else if let Some(k) = kind {
                    if let Some(s) = stats.as_mut() {
                        match k {
                            PResidualChunkKind::LegacyTuple => s.p_explicit_chunks += 1,
                            PResidualChunkKind::Adaptive(BlockResidualCoding::ExplicitTuples) => {
                                s.p_explicit_chunks += 1;
                            }
                            PResidualChunkKind::Adaptive(BlockResidualCoding::RansV1) => {
                                s.p_rans_chunks += 1;
                            }
                        }
                    }
                }
                if block_aq_wire {
                    if let Some(acc) = block_wire_acc.as_mut() {
                        let pos = u32::from(qp_delta > 0);
                        let neg = u32::from(qp_delta < 0);
                        let zero = u32::from(qp_delta == 0);
                        accumulate_block_aq_wire_plane(
                            acc,
                            1,
                            u64::from(eff_qp_u8),
                            eff_qp_u8,
                            eff_qp_u8,
                            pos,
                            neg,
                            zero,
                        );
                    }
                    residual_entries.push((qp_delta, chunk));
                } else {
                    residual_entries.push((0, chunk));
                }
                continue;
            }
            pattern |= 1 << si;
            ms.skip_subblocks += 1;
            continue;
        }

        let chunk = if matches!(settings.residual_entropy, ResidualEntropy::Explicit) {
            let mut c = Vec::new();
            encode_residual_block_8x8(&blk, eff_i, &mut c)?;
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
            let qf = quantize(&f, eff_i);
            let (c, kind) = encode_p_residual_chunk(&qf, settings.residual_entropy, rans_model)?;
            if let Some(s) = stats.as_mut() {
                match kind {
                    PResidualChunkKind::LegacyTuple => s.p_explicit_chunks += 1,
                    PResidualChunkKind::Adaptive(BlockResidualCoding::ExplicitTuples) => {
                        s.p_explicit_chunks += 1;
                    }
                    PResidualChunkKind::Adaptive(BlockResidualCoding::RansV1) => {
                        s.p_rans_chunks += 1;
                    }
                }
            }
            c
        };
        if block_aq_wire {
            if let Some(acc) = block_wire_acc.as_mut() {
                let pos = u32::from(qp_delta > 0);
                let neg = u32::from(qp_delta < 0);
                let zero = u32::from(qp_delta == 0);
                accumulate_block_aq_wire_plane(
                    acc,
                    1,
                    u64::from(eff_qp_u8),
                    eff_qp_u8,
                    eff_qp_u8,
                    pos,
                    neg,
                    zero,
                );
            }
            residual_entries.push((qp_delta, chunk));
        } else {
            residual_entries.push((0, chunk));
        }
    }

    Ok(PMacroblockEncoded {
        mvx_q,
        mvy_q,
        pattern,
        residual_entries,
    })
}

fn append_p_mb_residual_body(
    out: &mut Vec<u8>,
    mb: &PMacroblockEncoded,
    block_aq_wire: bool,
) -> Result<(), SrsV2Error> {
    out.push(mb.pattern);
    for (qp_delta, chunk) in &mb.residual_entries {
        if block_aq_wire {
            out.push(*qp_delta as u8);
        }
        let len = u32::try_from(chunk.len()).map_err(|_| SrsV2Error::Overflow("chunk"))?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(chunk);
    }
    Ok(())
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
    mut block_wire_acc: Option<&mut SrsV2BlockAqWireStats>,
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

    let block_aq_wire = matches!(settings.block_aq_mode, SrsV2BlockAqMode::BlockDelta)
        && matches!(
            settings.residual_entropy,
            ResidualEntropy::Auto | ResidualEntropy::Rans
        );
    if matches!(settings.block_aq_mode, SrsV2BlockAqMode::BlockDelta)
        && matches!(settings.residual_entropy, ResidualEntropy::Explicit)
    {
        return Err(SrsV2Error::syntax(
            "block AQ (FR2 rev 8/9) requires adaptive residual entropy (Auto or Rans)",
        ));
    }

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
    ms.inter_mv = Default::default();
    ms.rdo = Default::default();
    let use_subpel = matches!(settings.subpel_mode, SrsV2SubpelMode::HalfPel);
    ms.subpel_enabled = use_subpel;
    ms.subpel_blocks_tested = 0;
    ms.subpel_blocks_selected = 0;
    ms.additional_subpel_evaluations = 0;
    ms.sum_abs_frac_qpel = 0;

    let sub_vars = if block_aq_wire {
        collect_p_subblock_variances(&cur.y, mb_cols, mb_rows)
    } else {
        Vec::new()
    };
    let median_var = if block_aq_wire {
        let mut s = sub_vars.clone();
        s.sort_unstable();
        let mid = s.len() / 2;
        if s.is_empty() {
            0_u64
        } else if s.len().is_multiple_of(2) {
            (s[mid - 1] as u64 + s[mid] as u64) / 2
        } else {
            s[mid] as u64
        }
    } else {
        0_u64
    };

    let rans_model = residual_token_model();

    let n_mb = (mb_cols as usize).saturating_mul(mb_rows as usize);
    let mut me_mvs: Vec<(i32, i32)> = Vec::with_capacity(n_mb);
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let mut mb_evals = 0_u64;
            let mv_me = if use_subpel {
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
                (qx, qy)
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
                (mvx as i32 * 4, mvy as i32 * 4)
            };
            me_mvs.push(mv_me);
        }
    }

    let lam_mv = if matches!(settings.rdo_mode, SrsV2RdoMode::Fast) {
        rdo_lambda_effective(settings, qp)
    } else {
        0
    };
    let mut grid_pred: Vec<(i32, i32)> = vec![(0, 0); n_mb];
    let mut mbs: Vec<PMacroblockEncoded> = Vec::with_capacity(n_mb);
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let mv_me = me_mvs[idx];
            let mv_use = if matches!(settings.rdo_mode, SrsV2RdoMode::Fast) {
                let sad_me = mb_luma_sad_with_mv_q(
                    &cur.y,
                    &reference.y,
                    mbx,
                    mby,
                    mv_me.0,
                    mv_me.1,
                    use_subpel,
                );
                let sad_z = mb_luma_sad_with_mv_q(&cur.y, &reference.y, mbx, mby, 0, 0, use_subpel);
                let cost_me = mv_wire_cost_bytes_estimate(
                    settings.inter_syntax_mode,
                    use_subpel,
                    mbx,
                    mby,
                    mb_cols,
                    &grid_pred,
                    mv_me,
                );
                let cost_z = mv_wire_cost_bytes_estimate(
                    settings.inter_syntax_mode,
                    use_subpel,
                    mbx,
                    mby,
                    mb_cols,
                    &grid_pred,
                    (0, 0),
                );
                ms.rdo.candidates_tested += 2;
                ms.rdo.estimated_bits_used_for_decision = ms
                    .rdo
                    .estimated_bits_used_for_decision
                    .saturating_add(cost_me.max(0) as u64);
                ms.rdo.estimated_bits_used_for_decision = ms
                    .rdo
                    .estimated_bits_used_for_decision
                    .saturating_add(cost_z.max(0) as u64);
                let score_me = rdo_score_p_mb(sad_me, lam_mv, cost_me);
                let score_z = rdo_score_p_mb(sad_z, lam_mv, cost_z);
                if score_z <= score_me {
                    (0, 0)
                } else {
                    mv_me
                }
            } else {
                mv_me
            };
            grid_pred[idx] = mv_use;

            let enc = encode_p_macroblock(
                cur,
                reference,
                mbx,
                mby,
                mb_cols,
                mv_use.0,
                mv_use.1,
                use_subpel,
                allow_skip,
                qp,
                block_aq_wire,
                &sub_vars,
                median_var,
                settings,
                &rans_model,
                &mut stats,
                ms,
                &mut block_wire_acc,
            )?;
            mbs.push(enc);
        }
    }

    ms.nonzero_motion_macroblocks =
        mbs.iter().filter(|m| m.mvx_q != 0 || m.mvy_q != 0).count() as u32;
    ms.sum_mv_l1 = mbs
        .iter()
        .map(|m| ((m.mvx_q.unsigned_abs() + m.mvy_q.unsigned_abs()) / 4) as u64)
        .sum();

    let entropy_residual = matches!(
        settings.residual_entropy,
        ResidualEntropy::Auto | ResidualEntropy::Rans
    );

    let mut out = Vec::new();
    match settings.inter_syntax_mode {
        SrsV2InterSyntaxMode::RawLegacy => {
            let magic = match (use_subpel, block_aq_wire) {
                (true, true) => FRAME_PAYLOAD_MAGIC_P_SUBPEL_BLOCK_AQ,
                (false, true) => FRAME_PAYLOAD_MAGIC_P_BLOCK_AQ,
                (true, false) => match settings.residual_entropy {
                    ResidualEntropy::Explicit => FRAME_PAYLOAD_MAGIC_P_SUBPEL,
                    ResidualEntropy::Auto | ResidualEntropy::Rans => {
                        FRAME_PAYLOAD_MAGIC_P_SUBPEL_ENTROPY
                    }
                },
                (false, false) => match settings.residual_entropy {
                    ResidualEntropy::Explicit => FRAME_PAYLOAD_MAGIC_P,
                    ResidualEntropy::Auto | ResidualEntropy::Rans => FRAME_PAYLOAD_MAGIC_P_ENTROPY,
                },
            };
            out.extend_from_slice(&magic);
            out.extend_from_slice(&frame_index.to_le_bytes());
            out.push(qp);
            if block_aq_wire {
                validate_qp_clip_range(settings.min_qp, settings.max_qp)?;
                out.push(settings.min_qp);
                out.push(settings.max_qp);
            }
            for mb in &mbs {
                if use_subpel {
                    out.extend_from_slice(&mb.mvx_q.to_le_bytes());
                    out.extend_from_slice(&mb.mvy_q.to_le_bytes());
                } else {
                    let mvx = (mb.mvx_q / 4) as i16;
                    let mvy = (mb.mvy_q / 4) as i16;
                    out.extend_from_slice(&mvx.to_le_bytes());
                    out.extend_from_slice(&mvy.to_le_bytes());
                }
                append_p_mb_residual_body(&mut out, mb, block_aq_wire)?;
            }
        }
        SrsV2InterSyntaxMode::CompactV1 => {
            out.extend_from_slice(&FRAME_PAYLOAD_MAGIC_P_COMPACT);
            out.extend_from_slice(&frame_index.to_le_bytes());
            out.push(qp);
            let mut flags = 0_u8;
            if use_subpel {
                flags |= P_INTER_FLAG_SUBPEL;
            }
            if block_aq_wire {
                flags |= P_INTER_FLAG_BLOCK_AQ;
            }
            if entropy_residual {
                flags |= P_INTER_FLAG_ENTROPY_RESIDUAL;
            }
            out.push(flags);
            if block_aq_wire {
                validate_qp_clip_range(settings.min_qp, settings.max_qp)?;
                out.push(settings.min_qp);
                out.push(settings.max_qp);
            }
            let mvs: Vec<(i32, i32)> = mbs.iter().map(|m| (m.mvx_q, m.mvy_q)).collect();
            let mv_compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
            out.extend_from_slice(&mv_compact);
            for mb in &mbs {
                append_p_mb_residual_body(&mut out, mb, block_aq_wire)?;
            }
        }
        SrsV2InterSyntaxMode::EntropyV1 => {
            out.extend_from_slice(&FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY);
            out.extend_from_slice(&frame_index.to_le_bytes());
            out.push(qp);
            let mut flags = 0_u8;
            if use_subpel {
                flags |= P_INTER_FLAG_SUBPEL;
            }
            if block_aq_wire {
                flags |= P_INTER_FLAG_BLOCK_AQ;
            }
            if entropy_residual {
                flags |= P_INTER_FLAG_ENTROPY_RESIDUAL;
            }
            out.push(flags);
            if block_aq_wire {
                validate_qp_clip_range(settings.min_qp, settings.max_qp)?;
                out.push(settings.min_qp);
                out.push(settings.max_qp);
            }
            let mvs: Vec<(i32, i32)> = mbs.iter().map(|m| (m.mvx_q, m.mvy_q)).collect();
            let mv_compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
            let mv_blob = rans_encode_mv_bytes(&mv_compact)?;
            let sym_count = u32::try_from(mv_compact.len())
                .map_err(|_| SrsV2Error::Overflow("mv compact symbol count"))?;
            let blob_len = u32::try_from(mv_blob.len())
                .map_err(|_| SrsV2Error::Overflow("mv rans blob length"))?;
            out.extend_from_slice(&sym_count.to_le_bytes());
            out.extend_from_slice(&blob_len.to_le_bytes());
            out.extend_from_slice(&mv_blob);
            for mb in &mbs {
                append_p_mb_residual_body(&mut out, mb, block_aq_wire)?;
            }
        }
    }

    let mvs_final: Vec<(i32, i32)> = mbs.iter().map(|m| (m.mvx_q, m.mvy_q)).collect();
    let compact_tmp = encode_mv_grid_compact(&mvs_final, mb_cols, mb_rows);
    let (z0, zn, sum_abs, avg_abs) = mv_compact_grid_delta_statistics(&mvs_final, mb_cols, mb_rows);
    let residual_total: u64 = mbs
        .iter()
        .map(|mb| p_mb_residual_wire_bytes(mb, block_aq_wire))
        .sum();
    let mv_entropy_section =
        if matches!(settings.inter_syntax_mode, SrsV2InterSyntaxMode::EntropyV1) {
            let blob = rans_encode_mv_bytes(&compact_tmp)?;
            8_u64.saturating_add(blob.len() as u64)
        } else {
            0
        };
    ms.inter_mv.mv_prediction_mode = MV_PREDICTION_MODE_LABEL;
    ms.inter_mv.mv_raw_bytes_estimate = n_mb as u64 * if use_subpel { 8 } else { 4 };
    ms.inter_mv.mv_compact_bytes = compact_tmp.len() as u64;
    ms.inter_mv.mv_entropy_section_bytes = mv_entropy_section;
    ms.inter_mv.mv_delta_zero_varints = z0;
    ms.inter_mv.mv_delta_nonzero_varints = zn;
    ms.inter_mv.mv_delta_sum_abs_components = sum_abs;
    ms.inter_mv.mv_delta_avg_abs = avg_abs;
    ms.inter_mv.residual_payload_bytes = residual_total;
    ms.inter_mv.inter_header_bytes = out.len() as u64 - residual_total;

    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded P-frame",
        });
    }
    Ok(out)
}

/// Decode `FR2` rev **2**, **4**, **5**, **6**, **8**, **9**, **15**, or **17** P-frame into **reconstructed** YUV420p8 (no loop filter).
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
    let rev = payload[3];
    if payload.len() < 4
        || &payload[0..3] != b"FR2"
        || !matches!(rev, 2 | 4 | 5 | 6 | 8 | 9 | 15 | 17)
    {
        return Err(SrsV2Error::BadMagic);
    }
    let inter_compact = matches!(rev, 15 | 17);
    let inter_entropy = rev == 17;

    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let qp = read_u8(payload, &mut cur)?;

    let (use_qpel, block_aq_wire, entropy_chunks) = if inter_compact {
        let flags = read_u8(payload, &mut cur)?;
        if flags & !7 != 0 {
            return Err(SrsV2Error::syntax("unknown P inter compact flags"));
        }
        let use_qpel = flags & P_INTER_FLAG_SUBPEL != 0;
        let block_aq_wire = flags & P_INTER_FLAG_BLOCK_AQ != 0;
        let entropy_chunks = flags & P_INTER_FLAG_ENTROPY_RESIDUAL != 0;
        (use_qpel, block_aq_wire, entropy_chunks)
    } else {
        (
            matches!(rev, 5 | 6 | 9),
            matches!(rev, 8 | 9),
            matches!(rev, 4 | 6 | 8 | 9),
        )
    };

    let qp_i = qp.max(1) as i16;
    let (clip_min, clip_max) = if block_aq_wire {
        let a = read_u8(payload, &mut cur)?;
        let b = read_u8(payload, &mut cur)?;
        validate_qp_clip_range(a, b)?;
        (a, b)
    } else {
        (1_u8, 51_u8)
    };

    let mb_cols = w / 16;
    let mb_rows = h / 16;

    let mv_grid: Option<Vec<(i32, i32)>> = if inter_compact {
        let validate_mv_wire = |mx: i32, my: i32| -> Result<(), SrsV2Error> {
            if use_qpel {
                validate_mv_qpel_halfgrid(mx, my)
            } else if mx % 4 != 0 || my % 4 != 0 {
                Err(SrsV2Error::syntax("P MV not on integer pel grid"))
            } else {
                validate_mv((mx / 4) as i16, (my / 4) as i16)
            }
        };
        let grid = if inter_entropy {
            let sym_count = read_u32(payload, &mut cur)? as usize;
            let blob_len = read_u32(payload, &mut cur)? as usize;
            let max_compact = (mb_cols as usize)
                .saturating_mul(mb_rows as usize)
                .saturating_mul(16)
                .min(MAX_FRAME_PAYLOAD_BYTES);
            if sym_count > max_compact {
                return Err(SrsV2Error::syntax("P MV compact length out of range"));
            }
            let blob_end = cur
                .checked_add(blob_len)
                .ok_or(SrsV2Error::Overflow("mv rans blob"))?;
            if blob_end > payload.len() {
                return Err(SrsV2Error::Truncated);
            }
            let blob = &payload[cur..blob_end];
            cur = blob_end;
            let budget = blob_len.saturating_mul(64).min(512_000);
            let compact = rans_decode_mv_bytes(blob, sym_count, budget)?;
            if compact.len() != sym_count {
                return Err(SrsV2Error::syntax("P MV rANS output length mismatch"));
            }
            let mut cc = 0usize;
            let g = decode_mv_grid_compact(&compact, &mut cc, mb_cols, mb_rows, validate_mv_wire)?;
            if cc != compact.len() {
                return Err(SrsV2Error::syntax("P MV compact trailing bytes"));
            }
            g
        } else {
            let mut cc = cur;
            let g = decode_mv_grid_compact(payload, &mut cc, mb_cols, mb_rows, validate_mv_wire)?;
            cur = cc;
            g
        };
        Some(grid)
    } else {
        None
    };

    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx_mb = (mby * mb_cols + mbx) as usize;
            let (mvx_q, mvy_q) = if let Some(ref grid) = mv_grid {
                grid[idx_mb]
            } else if use_qpel {
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
                    let eff_i = if block_aq_wire {
                        let d = read_u8(payload, &mut cur)? as i8;
                        validate_wire_qp_delta(d)?;
                        let eff = apply_qp_delta_clamped(qp, d, clip_min, clip_max);
                        eff.max(1) as i16
                    } else {
                        qp_i
                    };
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
                        decode_p_residual_chunk(chunk, eff_i)?
                    } else {
                        let mut c = 0usize;
                        let r = decode_residual_block_8x8(chunk, &mut c, eff_i)?;
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
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, qp, &settings, None, None, None).unwrap();
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
            encode_yuv420_p_payload(&seq, &y1, &y0, 1, qp, &settings, None, None, None).unwrap();
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
        encode_yuv420_p_payload(
            &seq,
            &yuv,
            &yuv,
            1,
            28,
            &settings,
            None,
            Some(&mut ms),
            None,
        )
        .unwrap();
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
        encode_yuv420_p_payload(
            &seq,
            &yuv,
            &yuv,
            1,
            28,
            &settings,
            None,
            Some(&mut ms),
            None,
        )
        .unwrap();
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
        let p_on =
            encode_yuv420_p_payload(&seq, &y_cur, &y_ref, 1, qp, &on, None, None, None).unwrap();
        let p_off =
            encode_yuv420_p_payload(&seq, &y_cur, &y_ref, 1, qp, &off, None, None, None).unwrap();
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
            encode_yuv420_p_payload(&seq, &y1, &y0, 1, 26, &settings, None, None, None).unwrap();
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
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
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
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
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
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
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
        let p =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
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
        let p =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
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
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
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
            encode_yuv420_p_payload(&seq, &y1, ref_y, 1, qp, &settings, None, None, None).unwrap();
        assert_eq!(p_payload[3], 5);
        let mut slot2 = Some(ref_y.clone());
        let via_disp = decode_yuv420_srsv2_payload(&seq, &p_payload, &mut slot2).unwrap();
        let dec_raw = decode_yuv420_p_payload(&seq, &p_payload, ref_y).unwrap();
        assert_eq!(via_disp.yuv.y.samples, dec_raw.yuv.y.samples);
    }

    #[test]
    fn p_compact_rev15_roundtrip_matches_raw_legacy_luma() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let base = SrsV2EncodeSettings {
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            ..Default::default()
        };
        let raw_settings = SrsV2EncodeSettings {
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::RawLegacy,
            ..base.clone()
        };
        let compact_settings = SrsV2EncodeSettings {
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::CompactV1,
            ..base
        };
        let p_raw =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &raw_settings, None, None, None)
                .unwrap();
        let p_co =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &compact_settings, None, None, None)
                .unwrap();
        assert_eq!(p_co[3], 15);
        let d_raw = decode_yuv420_p_payload(&seq, &p_raw, &yuv).unwrap();
        let d_co = decode_yuv420_p_payload(&seq, &p_co, &yuv).unwrap();
        assert_eq!(d_raw.yuv.y.samples, d_co.yuv.y.samples);
    }

    #[test]
    fn p_entropy_rev17_roundtrip_matches_compact_luma() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let compact_settings = SrsV2EncodeSettings {
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::CompactV1,
            ..Default::default()
        };
        let entropy_settings = SrsV2EncodeSettings {
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::EntropyV1,
            ..compact_settings.clone()
        };
        let p_co =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &compact_settings, None, None, None)
                .unwrap();
        let p_en =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &entropy_settings, None, None, None)
                .unwrap();
        assert_eq!(p_en[3], 17);
        let d_co = decode_yuv420_p_payload(&seq, &p_co, &yuv).unwrap();
        let d_en = decode_yuv420_p_payload(&seq, &p_en, &yuv).unwrap();
        assert_eq!(d_co.yuv.y.samples, d_en.yuv.y.samples);
    }

    #[test]
    fn p_compact_reserved_flags_rejected() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::CompactV1,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
        payload[9] |= 0x08;
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn p_compact_rev15_truncated_mv_stream_fails() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::CompactV1,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
        assert_eq!(payload[3], 15);
        payload.truncate(10);
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(err, SrsV2Error::Truncated));
    }

    #[test]
    fn p_compact_rev15_mv_varint_truncated_fails() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::CompactV1,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
        payload.truncate(11);
        payload.push(0x80);
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(err, SrsV2Error::Truncated));
    }

    #[test]
    fn p_entropy_rev17_mv_sym_count_out_of_range_fails() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::EntropyV1,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
        assert_eq!(payload[3], 17);
        payload[10..14].copy_from_slice(&5000u32.to_le_bytes());
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn p_entropy_rev17_mv_blob_truncated_fails() {
        let seq = seq_inter(64, 64);
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            motion_search_radius: 16,
            residual_entropy: crate::srsv2::rate_control::ResidualEntropy::Explicit,
            inter_syntax_mode: crate::srsv2::rate_control::SrsV2InterSyntaxMode::EntropyV1,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_p_payload(&seq, &yuv, &yuv, 1, 28, &settings, None, None, None).unwrap();
        let blob_len_idx = 14;
        let declared =
            u32::from_le_bytes(payload[blob_len_idx..blob_len_idx + 4].try_into().unwrap());
        payload[blob_len_idx..blob_len_idx + 4]
            .copy_from_slice(&(declared.saturating_add(4096)).to_le_bytes());
        let err = decode_yuv420_p_payload(&seq, &payload, &yuv).unwrap_err();
        assert!(matches!(err, SrsV2Error::Truncated));
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
        encode_yuv420_p_payload(
            &seq,
            &yuv,
            &yuv,
            1,
            28,
            &settings,
            None,
            Some(&mut ms),
            None,
        )
        .unwrap();
        assert!(ms.subpel_enabled);
        assert!(ms.subpel_blocks_tested > 0);
        assert!(ms.additional_subpel_evaluations > 0);
    }
}
