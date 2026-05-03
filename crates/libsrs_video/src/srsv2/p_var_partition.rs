//! Experimental **`FR2` rev 19 / 20** — P-frame variable inter partitions + transform tagging (`FR2` rev **19** compact MV, **20** entropy MV).
//!
//! **`FR2` rev 21 / 22** (**B**) are reserved; see [`crate::srsv2::b_frame_codec`].

use libsrs_bitio::RansModel;

use super::adaptive_quant::{accumulate_block_aq_wire_plane, SrsV2BlockAqWireStats};
use super::block_aq::{
    apply_qp_delta_clamped, choose_block_qp_delta, collect_p_subblock_variances,
    validate_qp_clip_range, validate_wire_qp_delta,
};
use super::dct::{fdct_4x4, fdct_8x8};
use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::inter_mv::{
    decode_mv_stream_partitioned, encode_mv_stream_partitioned, rans_decode_mv_bytes,
    rans_encode_mv_bytes, validate_partition_reserved_bits, P_PART_WIRE_16X16, P_PART_WIRE_16X8,
    P_PART_WIRE_8X16, P_PART_WIRE_8X8,
};
use super::intra_codec::{quantize, quantize_4x4};
use super::limits::{MAX_FRAME_PAYLOAD_BYTES, MAX_MOTION_VECTOR_PELS};
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::motion_search::{
    pick_mv_rect, sad_rect_integer, sample_u8_plane, SrsV2MotionEncodeStats,
    SrsV2PartitionEncodeStats,
};
use super::rate_control::{
    rdo_lambda_effective, ResidualEncodeStats, ResidualEntropy, SrsV2EncodeSettings,
    SrsV2InterPartitionMode, SrsV2InterSyntaxMode, SrsV2PartitionCostModel,
    SrsV2PartitionMapEncoding, SrsV2SubpelMode, SrsV2TransformSizeMode,
};
use super::residual_entropy::{
    decode_p_residual_chunk, decode_p_residual_chunk_4x4, encode_p_residual_chunk,
    encode_p_residual_chunk_4x4, BlockResidualCoding, PResidualChunkKind,
};
use super::residual_tokens::residual_token_model;
use super::subpel::{sample_luma_bilinear_qpel, validate_mv_qpel_halfgrid};

/// P-frame variable partitions — compact MV (`FR2` rev **19**).
pub const FRAME_PAYLOAD_MAGIC_P_VAR_PARTITION: [u8; 4] = [b'F', b'R', b'2', 19];
/// P-frame variable partitions — entropy-coded MV section (`FR2` rev **20**).
pub const FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR: [u8; 4] = [b'F', b'R', b'2', 20];

fn copy_chroma_mb8_qpel_local(
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

const P_INTER_FLAG_SUBPEL: u8 = 1;
const P_INTER_FLAG_BLOCK_AQ: u8 = 2;
const P_INTER_FLAG_ENTROPY_RESIDUAL: u8 = 4;
/// Run-length partition map (`FR2` rev **19**/**20**): **flags** bit **3**.
const P_INTER_FLAG_PACKED_PART_MAP: u8 = 8;

const MAX_MB_RESIDUAL_RD_BYTES: usize = 65_536;

const CTRL_SKIP: u8 = 1;
const TX_SHIFT: u8 = 1;
const TX_MASK: u8 = 3;
const TX_WIRE_8X8: u8 = 0;
const TX_WIRE_4X4: u8 = 1;
const TX_WIRE_16: u8 = 2;

const SKIP_ABS_THRESH: i16 = 6;

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

fn read_u16_le(data: &[u8], cur: &mut usize) -> Result<u16, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 2 {
        return Err(SrsV2Error::Truncated);
    }
    let v = u16::from_le_bytes([data[*cur], data[*cur + 1]]);
    *cur += 2;
    Ok(v)
}

/// Run-length partition map: **`n_runs`** (**u16** LE), then **`(partition_byte, run_len_u16)`** pairs.
fn rle_partition_payload(partitions: &[u8]) -> Result<Vec<u8>, SrsV2Error> {
    if partitions.is_empty() {
        return Err(SrsV2Error::syntax("empty partition map"));
    }
    for &b in partitions {
        validate_partition_reserved_bits(b)?;
    }
    let mut runs: Vec<(u8, u16)> = Vec::new();
    let mut cur_b = partitions[0];
    let mut cnt: u16 = 1;
    for &b in &partitions[1..] {
        if b == cur_b && cnt < u16::MAX {
            cnt += 1;
        } else {
            runs.push((cur_b, cnt));
            cur_b = b;
            cnt = 1;
        }
    }
    runs.push((cur_b, cnt));
    let n_runs =
        u16::try_from(runs.len()).map_err(|_| SrsV2Error::Overflow("partition rle run count"))?;
    let mut out = Vec::new();
    out.extend_from_slice(&n_runs.to_le_bytes());
    for (w, c) in runs {
        out.push(w);
        out.extend_from_slice(&c.to_le_bytes());
    }
    Ok(out)
}

fn decode_rle_partition_map(
    data: &[u8],
    cur: &mut usize,
    n_mb: usize,
) -> Result<Vec<u8>, SrsV2Error> {
    if n_mb == 0 {
        return Ok(Vec::new());
    }
    let n_runs = read_u16_le(data, cur)? as usize;
    if n_runs == 0 || n_runs > n_mb {
        return Err(SrsV2Error::syntax("partition rle run count out of range"));
    }
    let mut out = Vec::with_capacity(n_mb);
    let mut total = 0usize;
    for _ in 0..n_runs {
        let w = read_u8(data, cur)?;
        validate_partition_reserved_bits(w)?;
        let c = read_u16_le(data, cur)? as usize;
        if c == 0 {
            return Err(SrsV2Error::syntax("partition rle zero-length run"));
        }
        total = total
            .checked_add(c)
            .ok_or(SrsV2Error::Overflow("partition rle expand"))?;
        if total > n_mb {
            return Err(SrsV2Error::syntax(
                "partition rle overflow macroblock count",
            ));
        }
        out.extend(std::iter::repeat_n(w, c));
    }
    if total != n_mb {
        return Err(SrsV2Error::syntax("partition rle length mismatch"));
    }
    Ok(out)
}

fn validate_mv_int(mvx: i16, mvy: i16) -> Result<(), SrsV2Error> {
    if mvx.abs() > MAX_MOTION_VECTOR_PELS || mvy.abs() > MAX_MOTION_VECTOR_PELS {
        return Err(SrsV2Error::CorruptedMotionVector);
    }
    Ok(())
}

fn validate_mv_wire(settings_use_qpel: bool, mx: i32, my: i32) -> Result<(), SrsV2Error> {
    if settings_use_qpel {
        validate_mv_qpel_halfgrid(mx, my)
    } else if mx % 4 != 0 || my % 4 != 0 {
        Err(SrsV2Error::syntax("P MV not on integer pel grid"))
    } else {
        validate_mv_int((mx / 4) as i16, (my / 4) as i16)
    }
}

struct PartitionMbCtx<'a> {
    cur: &'a VideoPlane<u8>,
    refp: &'a VideoPlane<u8>,
    mbx: u32,
    mby: u32,
    radius: i16,
    early: u32,
    search: super::rate_control::SrsV2MotionSearchMode,
}

#[inline]
fn partition_cost_model_label(m: SrsV2PartitionCostModel) -> &'static str {
    match m {
        SrsV2PartitionCostModel::SadOnly => "sad-only",
        SrsV2PartitionCostModel::HeaderAware => "header-aware",
        SrsV2PartitionCostModel::RdoFast => "rdo-fast",
    }
}

/// **SAD** sum and **wire-order** MVs for one macroblock and partition type (`pt`).
fn candidate_sad_and_mvs(
    ctx: &PartitionMbCtx<'_>,
    pt: u8,
    ms: &mut SrsV2MotionEncodeStats,
) -> Result<(u32, Vec<(i32, i32)>), SrsV2Error> {
    let PartitionMbCtx {
        cur,
        refp,
        mbx,
        mby,
        radius,
        early,
        search,
    } = ctx;
    let ox = mbx * 16;
    let oy = mby * 16;
    let mut out = Vec::new();
    let mut sad_acc = 0_u32;
    match pt {
        P_PART_WIRE_16X16 => {
            let mut ev = 0_u64;
            let (vx, vy) = pick_mv_rect(
                *search,
                cur,
                refp,
                ox,
                oy,
                16,
                16,
                *radius,
                *early,
                Some(&mut ev),
            );
            ms.sad_evaluations += ev;
            sad_acc = sad_acc.saturating_add(sad_rect_integer(
                cur, refp, ox, oy, 16, 16, vx as i32, vy as i32,
            ));
            out.push((vx as i32 * 4, vy as i32 * 4));
        }
        P_PART_WIRE_16X8 => {
            for dy in [0u32, 8u32] {
                let mut ev = 0_u64;
                let (vx, vy) = pick_mv_rect(
                    *search,
                    cur,
                    refp,
                    ox,
                    oy + dy,
                    16,
                    8,
                    *radius,
                    *early,
                    Some(&mut ev),
                );
                ms.sad_evaluations += ev;
                sad_acc = sad_acc.saturating_add(sad_rect_integer(
                    cur,
                    refp,
                    ox,
                    oy + dy,
                    16,
                    8,
                    vx as i32,
                    vy as i32,
                ));
                out.push((vx as i32 * 4, vy as i32 * 4));
            }
        }
        P_PART_WIRE_8X16 => {
            for (dx, wt) in [(0u32, 8u32), (8, 8)] {
                let mut ev = 0_u64;
                let (vx, vy) = pick_mv_rect(
                    *search,
                    cur,
                    refp,
                    ox + dx,
                    oy,
                    wt,
                    16,
                    *radius,
                    *early,
                    Some(&mut ev),
                );
                ms.sad_evaluations += ev;
                sad_acc = sad_acc.saturating_add(sad_rect_integer(
                    cur,
                    refp,
                    ox + dx,
                    oy,
                    wt,
                    16,
                    vx as i32,
                    vy as i32,
                ));
                out.push((vx as i32 * 4, vy as i32 * 4));
            }
        }
        P_PART_WIRE_8X8 => {
            for (dx, dy) in [(0u32, 0u32), (8, 0), (0, 8), (8, 8)] {
                let mut ev = 0_u64;
                let (vx, vy) = pick_mv_rect(
                    *search,
                    cur,
                    refp,
                    ox + dx,
                    oy + dy,
                    8,
                    8,
                    *radius,
                    *early,
                    Some(&mut ev),
                );
                ms.sad_evaluations += ev;
                sad_acc = sad_acc.saturating_add(sad_rect_integer(
                    cur,
                    refp,
                    ox + dx,
                    oy + dy,
                    8,
                    8,
                    vx as i32,
                    vy as i32,
                ));
                out.push((vx as i32 * 4, vy as i32 * 4));
            }
        }
        _ => return Err(SrsV2Error::syntax("bad candidate partition")),
    }
    Ok((sad_acc, out))
}

/// Legacy **SAD**-only **AutoFast** winner (**no** [`SrsV2PartitionEncodeStats::rdo_partition_candidates_tested`] bump).
fn autofast_compute_sad_only_choice(
    ctx: &PartitionMbCtx<'_>,
    ms: &mut SrsV2MotionEncodeStats,
    lam_p: i64,
) -> u8 {
    let ox = ctx.mbx * 16;
    let oy = ctx.mby * 16;
    let mut ev = 0_u64;
    let (mv16x, mv16y) = pick_mv_rect(
        ctx.search,
        ctx.cur,
        ctx.refp,
        ox,
        oy,
        16,
        16,
        ctx.radius,
        ctx.early,
        Some(&mut ev),
    );
    ms.sad_evaluations += ev;
    let sad16 = sad_rect_integer(
        ctx.cur,
        ctx.refp,
        ox,
        oy,
        16,
        16,
        mv16x as i32,
        mv16y as i32,
    );
    let mut sad8 = 0_u32;
    for (dx, dy) in [(0u32, 0u32), (8, 0), (0, 8), (8, 8)] {
        let mut e2 = 0_u64;
        let (vx, vy) = pick_mv_rect(
            ctx.search,
            ctx.cur,
            ctx.refp,
            ox + dx,
            oy + dy,
            8,
            8,
            ctx.radius,
            ctx.early,
            Some(&mut e2),
        );
        ms.sad_evaluations += e2;
        sad8 = sad8.saturating_add(sad_rect_integer(
            ctx.cur,
            ctx.refp,
            ox + dx,
            oy + dy,
            8,
            8,
            vx as i32,
            vy as i32,
        ));
    }
    let mut e3 = 0_u64;
    let (mvt_x, mvt_y) = pick_mv_rect(
        ctx.search,
        ctx.cur,
        ctx.refp,
        ox,
        oy,
        16,
        8,
        ctx.radius,
        ctx.early,
        Some(&mut e3),
    );
    ms.sad_evaluations += e3;
    let sad16x8_top =
        sad_rect_integer(ctx.cur, ctx.refp, ox, oy, 16, 8, mvt_x as i32, mvt_y as i32);
    let mut e4 = 0_u64;
    let (mvb_x, mvb_y) = pick_mv_rect(
        ctx.search,
        ctx.cur,
        ctx.refp,
        ox,
        oy + 8,
        16,
        8,
        ctx.radius,
        ctx.early,
        Some(&mut e4),
    );
    ms.sad_evaluations += e4;
    let sad16x8_bot = sad_rect_integer(
        ctx.cur,
        ctx.refp,
        ox,
        oy + 8,
        16,
        8,
        mvb_x as i32,
        mvb_y as i32,
    );
    let sad16x8 = sad16x8_top.saturating_add(sad16x8_bot);

    let split_penalty = (12_i64 * lam_p) / 256;
    let rect_penalty = (8_i64 * lam_p) / 256;
    let s16 = sad16 as i128;
    let s8 = sad8 as i128 + split_penalty as i128;
    let sr = sad16x8 as i128 + rect_penalty as i128;
    if s8 + 4 < s16 && s8 <= sr {
        P_PART_WIRE_8X8
    } else if sr + 4 < s16 {
        P_PART_WIRE_16X8
    } else {
        P_PART_WIRE_16X16
    }
}

fn autofast_pick_sad_only_legacy(
    ctx: &PartitionMbCtx<'_>,
    ms: &mut SrsV2MotionEncodeStats,
    lam_p: i64,
) -> u8 {
    ms.partition.rdo_partition_candidates_tested += 3;
    autofast_compute_sad_only_choice(ctx, ms, lam_p)
}

#[allow(clippy::too_many_arguments)]
fn encode_one_mb_residual_body_len(
    cur: &YuvFrame,
    reference: &YuvFrame,
    mbx: u32,
    mby: u32,
    mb_cols: u32,
    pt: u8,
    pu_mvs: &[(i32, i32)],
    qp: u8,
    block_aq_wire: bool,
    sub_vars: &[u32],
    median_var: u64,
    settings: &SrsV2EncodeSettings,
    rans_model: &RansModel,
) -> Result<usize, SrsV2Error> {
    let use_subpel = matches!(settings.subpel_mode, SrsV2SubpelMode::HalfPel);
    let mut buf = Vec::new();
    let mut dry_residual_stats: Option<&mut ResidualEncodeStats> = None;
    let mut dry_ms = SrsV2MotionEncodeStats::default();
    let mut dry_block: Option<&mut SrsV2BlockAqWireStats> = None;
    let npu = pu_index_layout(pt);
    if pu_mvs.len() != npu {
        return Err(SrsV2Error::syntax("PU MV len mismatch residual RD"));
    }
    for (pu_idx, mv) in pu_mvs.iter().copied().enumerate() {
        encode_residual_subblocks_for_pu(
            &mut buf,
            cur,
            reference,
            mbx,
            mby,
            mb_cols,
            mv.0,
            mv.1,
            use_subpel,
            pt,
            pu_idx,
            qp,
            block_aq_wire,
            sub_vars,
            median_var,
            settings,
            rans_model,
            &mut dry_residual_stats,
            &mut dry_ms,
            &mut dry_block,
        )?;
        if buf.len() > MAX_MB_RESIDUAL_RD_BYTES {
            return Err(SrsV2Error::AllocationLimit {
                context: "var-partition mb residual RD estimate",
            });
        }
    }
    Ok(buf.len())
}

#[allow(clippy::too_many_arguments)]
fn partition_choice_for_mb(
    mode: SrsV2InterPartitionMode,
    settings: &SrsV2EncodeSettings,
    qp: u8,
    ctx: &PartitionMbCtx<'_>,
    ms: &mut SrsV2MotionEncodeStats,
    lam_p: i64,
    cur_frame: &YuvFrame,
    reference: &YuvFrame,
    mb_cols: u32,
    block_aq_wire: bool,
    sub_vars: &[u32],
    median_var: u64,
    rans_model: &RansModel,
) -> Result<u8, SrsV2Error> {
    match mode {
        SrsV2InterPartitionMode::Fixed16x16 => Ok(P_PART_WIRE_16X16),
        SrsV2InterPartitionMode::Split8x8 => Ok(P_PART_WIRE_8X8),
        SrsV2InterPartitionMode::Rect16x8 => Ok(P_PART_WIRE_16X8),
        SrsV2InterPartitionMode::Rect8x16 => Ok(P_PART_WIRE_8X16),
        SrsV2InterPartitionMode::AutoFast => {
            let cm = settings.partition_cost_model;
            if matches!(cm, SrsV2PartitionCostModel::SadOnly) {
                return Ok(autofast_pick_sad_only_legacy(ctx, ms, lam_p));
            }

            let cands = [
                P_PART_WIRE_16X16,
                P_PART_WIRE_8X8,
                P_PART_WIRE_16X8,
                P_PART_WIRE_8X16,
            ];
            let sad_only_choice = autofast_compute_sad_only_choice(ctx, ms, lam_p);
            ms.partition.rdo_partition_candidates_tested += cands.len() as u64;

            let mut best_pt = P_PART_WIRE_16X16;
            let mut best_score = i128::MAX;
            let mut sad_by_pt = [(P_PART_WIRE_16X16, 0u32); 4];

            for (i, &pt) in cands.iter().enumerate() {
                let (sad, mvs) = candidate_sad_and_mvs(ctx, pt, ms)?;
                sad_by_pt[i] = (pt, sad);
                let mv_b = encode_mv_stream_partitioned(1, 1, &[pt], &mvs)?.len();
                let extra_pu = pu_index_layout(pt).saturating_sub(1);
                let score = match cm {
                    SrsV2PartitionCostModel::SadOnly => unreachable!(),
                    SrsV2PartitionCostModel::HeaderAware => {
                        let spl = if pt == P_PART_WIRE_8X8 {
                            i128::from(settings.partition_split_penalty)
                        } else {
                            0
                        };
                        sad as i128
                            + (lam_p as i128
                                * i128::from(settings.partition_mv_penalty)
                                * mv_b as i128)
                                / (256 * 256)
                            + (lam_p as i128
                                * i128::from(settings.partition_header_penalty)
                                * extra_pu as i128)
                                / (256 * 256)
                            + (lam_p as i128 * spl) / (256 * 256)
                    }
                    SrsV2PartitionCostModel::RdoFast => {
                        let res_b = encode_one_mb_residual_body_len(
                            cur_frame,
                            reference,
                            ctx.mbx,
                            ctx.mby,
                            mb_cols,
                            pt,
                            &mvs,
                            qp,
                            block_aq_wire,
                            sub_vars,
                            median_var,
                            settings,
                            rans_model,
                        )?;
                        let qb = i128::from(settings.partition_quality_bias);
                        sad as i128 + (lam_p as i128 * qb * (mv_b + res_b) as i128) / (256 * 256)
                    }
                };
                if score < best_score {
                    best_score = score;
                    best_pt = pt;
                }
            }

            if sad_only_choice != best_pt {
                match cm {
                    SrsV2PartitionCostModel::HeaderAware => {
                        ms.partition.partition_rejected_by_header_cost += 1;
                    }
                    SrsV2PartitionCostModel::RdoFast => {
                        ms.partition.partition_rejected_by_rdo += 1;
                    }
                    _ => {}
                }
                let sad_so = sad_by_pt
                    .iter()
                    .find(|(p, _)| *p == sad_only_choice)
                    .map(|(_, s)| *s)
                    .unwrap_or(0);
                let sad_best = sad_by_pt
                    .iter()
                    .find(|(p, _)| *p == best_pt)
                    .map(|(_, s)| *s)
                    .unwrap_or(0);
                ms.partition.partition_sad_override_events += 1;
                ms.partition.partition_sad_override_accum += sad_so.saturating_sub(sad_best) as u64;
            }

            Ok(best_pt)
        }
    }
}

fn collect_pu_mvs(
    partitions: &[u8],
    mb_cols: u32,
    mb_rows: u32,
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    settings: &SrsV2EncodeSettings,
    ms: &mut SrsV2MotionEncodeStats,
) -> Result<Vec<(i32, i32)>, SrsV2Error> {
    let radius = settings.clamped_motion_search_radius();
    let early = settings.early_exit_sad_threshold;
    let search = settings.motion_search_mode;
    let use_subpel = matches!(settings.subpel_mode, SrsV2SubpelMode::HalfPel);
    let mut out = Vec::new();
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let pt = validate_partition_reserved_bits(partitions[idx])?;
            match pt {
                P_PART_WIRE_16X16 => {
                    let mut ev = 0_u64;
                    let (vx, vy) = pick_mv_rect(
                        search,
                        cur,
                        refp,
                        mbx * 16,
                        mby * 16,
                        16,
                        16,
                        radius,
                        early,
                        Some(&mut ev),
                    );
                    ms.sad_evaluations += ev;
                    let qx = vx as i32 * 4;
                    let qy = vy as i32 * 4;
                    validate_mv_wire(use_subpel, qx, qy)?;
                    out.push((qx, qy));
                }
                P_PART_WIRE_16X8 => {
                    for dy in [0u32, 8u32] {
                        let mut ev = 0_u64;
                        let (vx, vy) = pick_mv_rect(
                            search,
                            cur,
                            refp,
                            mbx * 16,
                            mby * 16 + dy,
                            16,
                            8,
                            radius,
                            early,
                            Some(&mut ev),
                        );
                        ms.sad_evaluations += ev;
                        let qx = vx as i32 * 4;
                        let qy = vy as i32 * 4;
                        validate_mv_wire(use_subpel, qx, qy)?;
                        out.push((qx, qy));
                    }
                }
                P_PART_WIRE_8X16 => {
                    for (dx, w) in [(0u32, 8u32), (8, 8)] {
                        let mut ev = 0_u64;
                        let (vx, vy) = pick_mv_rect(
                            search,
                            cur,
                            refp,
                            mbx * 16 + dx,
                            mby * 16,
                            w,
                            16,
                            radius,
                            early,
                            Some(&mut ev),
                        );
                        ms.sad_evaluations += ev;
                        let qx = vx as i32 * 4;
                        let qy = vy as i32 * 4;
                        validate_mv_wire(use_subpel, qx, qy)?;
                        out.push((qx, qy));
                    }
                }
                P_PART_WIRE_8X8 => {
                    for (dx, dy) in [(0u32, 0u32), (8, 0), (0, 8), (8, 8)] {
                        let mut ev = 0_u64;
                        let (vx, vy) = pick_mv_rect(
                            search,
                            cur,
                            refp,
                            mbx * 16 + dx,
                            mby * 16 + dy,
                            8,
                            8,
                            radius,
                            early,
                            Some(&mut ev),
                        );
                        ms.sad_evaluations += ev;
                        let qx = vx as i32 * 4;
                        let qy = vy as i32 * 4;
                        validate_mv_wire(use_subpel, qx, qy)?;
                        out.push((qx, qy));
                    }
                }
                _ => return Err(SrsV2Error::syntax("bad P partition type")),
            }
        }
    }
    Ok(out)
}

fn subblocks_for_pu(pt: u8, pu_index: usize) -> &'static [(u32, u32)] {
    match (pt, pu_index) {
        (P_PART_WIRE_16X16, 0) => &[(0, 0), (8, 0), (0, 8), (8, 8)],
        (P_PART_WIRE_16X8, 0) => &[(0, 0), (8, 0)],
        (P_PART_WIRE_16X8, 1) => &[(0, 8), (8, 8)],
        (P_PART_WIRE_8X16, 0) => &[(0, 0), (0, 8)],
        (P_PART_WIRE_8X16, 1) => &[(8, 0), (8, 8)],
        (P_PART_WIRE_8X8, 0) => &[(0, 0)],
        (P_PART_WIRE_8X8, 1) => &[(8, 0)],
        (P_PART_WIRE_8X8, 2) => &[(0, 8)],
        (P_PART_WIRE_8X8, 3) => &[(8, 8)],
        _ => &[],
    }
}

fn pu_index_layout(pt: u8) -> usize {
    match pt {
        P_PART_WIRE_16X16 => 1,
        P_PART_WIRE_16X8 | P_PART_WIRE_8X16 => 2,
        P_PART_WIRE_8X8 => 4,
        _ => 0,
    }
}

fn detail_prefers_4x4(blk: &[[i16; 8]; 8]) -> bool {
    let mut m = 0_i16;
    for row in blk {
        for &v in row.iter() {
            m = m.max(v.abs());
        }
    }
    m > 22
}

#[allow(clippy::too_many_arguments)]
fn encode_residual_subblocks_for_pu(
    out: &mut Vec<u8>,
    cur: &YuvFrame,
    reference: &YuvFrame,
    mbx: u32,
    mby: u32,
    mb_cols: u32,
    mvx_q: i32,
    mvy_q: i32,
    use_subpel: bool,
    pt: u8,
    pu_idx: usize,
    qp: u8,
    block_aq_wire: bool,
    sub_vars: &[u32],
    median_var: u64,
    settings: &SrsV2EncodeSettings,
    rans_model: &RansModel,
    stats: &mut Option<&mut ResidualEncodeStats>,
    ms: &mut SrsV2MotionEncodeStats,
    block_wire_acc: &mut Option<&mut SrsV2BlockAqWireStats>,
) -> Result<(), SrsV2Error> {
    let mvx_i = (mvx_q / 4) as i16;
    let mvy_i = (mvy_q / 4) as i16;
    let subs = subblocks_for_pu(pt, pu_idx);
    let allow_skip = settings.enable_skip_blocks;

    for &(dx, dy) in subs {
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
        let si = (((mby * mb_cols + mbx) * 4) as usize)
            + match (dx, dy) {
                (0, 0) => 0usize,
                (8, 0) => 1,
                (0, 8) => 2,
                _ => 3,
            };
        let qp_delta = if block_aq_wire {
            let bv = sub_vars[si];
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

        let mut ctrl = 0_u8;
        if allow_skip && max_abs <= SKIP_ABS_THRESH {
            ctrl |= CTRL_SKIP;
            out.push(ctrl);
            ms.skip_subblocks += 1;
            continue;
        }

        let use_tx4 = match settings.transform_size_mode {
            SrsV2TransformSizeMode::Auto => detail_prefers_4x4(&blk),
            SrsV2TransformSizeMode::Force4x4 => true,
            SrsV2TransformSizeMode::Force8x8 => false,
        };
        let txw = if use_tx4 {
            ms.partition.transform_4x4_count += 1;
            TX_WIRE_4X4
        } else {
            ms.partition.transform_8x8_count += 1;
            TX_WIRE_8X8
        };
        ctrl |= (txw & TX_MASK) << TX_SHIFT;
        out.push(ctrl);

        let mut push_chunk =
            |chunk: Vec<u8>,
             _stats: &mut Option<&mut ResidualEncodeStats>,
             ms: &mut SrsV2MotionEncodeStats,
             block_wire_acc: &mut Option<&mut SrsV2BlockAqWireStats>| {
                let mut wire = Vec::with_capacity(chunk.len() + 1);
                if block_aq_wire {
                    wire.push(qp_delta as u8);
                }
                wire.extend_from_slice(&chunk);
                let len =
                    u32::try_from(wire.len()).map_err(|_| SrsV2Error::Overflow("var p chunk"))?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&wire);
                if block_aq_wire {
                    if let Some(acc) = block_wire_acc.as_mut() {
                        accumulate_block_aq_wire_plane(
                            acc,
                            1,
                            u64::from(eff_qp_u8),
                            eff_qp_u8,
                            eff_qp_u8,
                            u32::from(qp_delta > 0),
                            u32::from(qp_delta < 0),
                            u32::from(qp_delta == 0),
                        );
                    }
                }
                ms.partition.partition_residual_bytes = ms
                    .partition
                    .partition_residual_bytes
                    .saturating_add(4 + wire.len() as u64);
                Ok::<(), SrsV2Error>(())
            };

        if txw == TX_WIRE_8X8 {
            let mut linear = [0_i16; 64];
            for r in 0..8 {
                for c in 0..8 {
                    linear[r * 8 + c] = blk[r][c];
                }
            }
            let f = fdct_8x8(&linear);
            let qf = quantize(&f, eff_i);
            let (chunk, kind) =
                encode_p_residual_chunk(&qf, settings.residual_entropy, rans_model)?;
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
            push_chunk(chunk, stats, ms, block_wire_acc)?;
        } else {
            for ry in (0u32..8).step_by(4) {
                for rx in (0u32..8).step_by(4) {
                    let mut b4 = [[0_i16; 4]; 4];
                    for r in 0..4 {
                        for c in 0..4 {
                            b4[r as usize][c as usize] = blk[(ry + r) as usize][(rx + c) as usize];
                        }
                    }
                    let mut linear = [0_i16; 16];
                    for r in 0..4 {
                        for c in 0..4 {
                            linear[r * 4 + c] = b4[r][c];
                        }
                    }
                    let f = fdct_4x4(&linear);
                    let qf = quantize_4x4(&f, eff_i);
                    let chunk = encode_p_residual_chunk_4x4(&qf)?;
                    if let Some(s) = stats.as_mut() {
                        s.p_explicit_chunks += 1;
                    }
                    push_chunk(chunk, stats, ms, block_wire_acc)?;
                }
            }
        }
    }
    Ok(())
}

/// Encode **`FR2` rev 19** or **20** P-frame.
#[allow(clippy::too_many_arguments)]
pub fn encode_yuv420_p_payload_var_partition(
    seq: &VideoSequenceHeaderV2,
    cur: &YuvFrame,
    reference: &YuvFrame,
    frame_index: u32,
    qp: u8,
    settings: &SrsV2EncodeSettings,
    stats: Option<&mut ResidualEncodeStats>,
    motion_stats: Option<&mut SrsV2MotionEncodeStats>,
    block_wire_acc: Option<&mut SrsV2BlockAqWireStats>,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8
        || cur.format != PixelFormat::Yuv420p8
        || reference.format != PixelFormat::Yuv420p8
    {
        return Err(SrsV2Error::Unsupported("P-frame encode requires YUV420p8"));
    }
    let w = seq.width;
    let h = seq.height;
    let mb_cols = w / 16;
    let mb_rows = h / 16;
    let n_mb = (mb_cols * mb_rows) as usize;

    let block_aq_wire = matches!(
        settings.block_aq_mode,
        super::rate_control::SrsV2BlockAqMode::BlockDelta
    ) && matches!(
        settings.residual_entropy,
        ResidualEntropy::Auto | ResidualEntropy::Rans
    );

    let mut motion_discard = SrsV2MotionEncodeStats::default();
    let ms = motion_stats.unwrap_or(&mut motion_discard);
    ms.partition = SrsV2PartitionEncodeStats {
        inter_partition_mode_label: match settings.inter_partition_mode {
            SrsV2InterPartitionMode::Fixed16x16 => "fixed16x16",
            SrsV2InterPartitionMode::Split8x8 => "split8x8",
            SrsV2InterPartitionMode::Rect16x8 => "rect16x8",
            SrsV2InterPartitionMode::Rect8x16 => "rect8x16",
            SrsV2InterPartitionMode::AutoFast => "auto-fast",
        },
        partition_cost_model_label: if matches!(
            settings.inter_partition_mode,
            SrsV2InterPartitionMode::AutoFast
        ) {
            partition_cost_model_label(settings.partition_cost_model)
        } else {
            ""
        },
        ..Default::default()
    };

    let lam_p = rdo_lambda_effective(settings, qp)
        .saturating_mul(i64::from(settings.partition_rdo_lambda_scale))
        / 256;

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

    let mut partitions = Vec::with_capacity(n_mb);
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let mb_ctx = PartitionMbCtx {
                cur: &cur.y,
                refp: &reference.y,
                mbx,
                mby,
                radius: settings.clamped_motion_search_radius(),
                early: settings.early_exit_sad_threshold,
                search: settings.motion_search_mode,
            };
            let choice = partition_choice_for_mb(
                settings.inter_partition_mode,
                settings,
                qp,
                &mb_ctx,
                ms,
                lam_p,
                cur,
                reference,
                mb_cols,
                block_aq_wire,
                &sub_vars,
                median_var,
                &rans_model,
            )?;
            partitions.push(choice);
            match choice {
                P_PART_WIRE_16X16 => ms.partition.partition_16x16_count += 1,
                P_PART_WIRE_16X8 => ms.partition.partition_16x8_count += 1,
                P_PART_WIRE_8X16 => ms.partition.partition_8x16_count += 1,
                P_PART_WIRE_8X8 => ms.partition.partition_8x8_count += 1,
                _ => {}
            }
        }
    }

    let pu_mvs = collect_pu_mvs(
        &partitions,
        mb_cols,
        mb_rows,
        &cur.y,
        &reference.y,
        settings,
        ms,
    )?;
    let mv_compact = encode_mv_stream_partitioned(mb_cols, mb_rows, &partitions, &pu_mvs)?;
    let use_subpel = matches!(settings.subpel_mode, SrsV2SubpelMode::HalfPel);

    let entropy_residual = matches!(
        settings.residual_entropy,
        ResidualEntropy::Auto | ResidualEntropy::Rans
    );

    let mut body_residual = Vec::new();
    let mut stats_mut = stats;
    let mut block_acc_mut = block_wire_acc;
    let mut pu_cursor = 0usize;
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let pt = partitions[idx];
            let npu = pu_index_layout(pt);
            for pu_idx in 0..npu {
                let mv = pu_mvs[pu_cursor];
                pu_cursor += 1;
                encode_residual_subblocks_for_pu(
                    &mut body_residual,
                    cur,
                    reference,
                    mbx,
                    mby,
                    mb_cols,
                    mv.0,
                    mv.1,
                    use_subpel,
                    pt,
                    pu_idx,
                    qp,
                    block_aq_wire,
                    &sub_vars,
                    median_var,
                    settings,
                    &rans_model,
                    &mut stats_mut,
                    ms,
                    &mut block_acc_mut,
                )?;
            }
        }
    }

    let mut out = Vec::new();
    let inter_entropy_mv = matches!(settings.inter_syntax_mode, SrsV2InterSyntaxMode::EntropyV1);
    let magic = if inter_entropy_mv {
        FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR
    } else {
        FRAME_PAYLOAD_MAGIC_P_VAR_PARTITION
    };
    out.extend_from_slice(&magic);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(qp);

    let rle_blob = if matches!(
        settings.partition_map_encoding,
        SrsV2PartitionMapEncoding::RleRuns
    ) {
        Some(rle_partition_payload(&partitions)?)
    } else {
        None
    };
    let use_packed_part_map = rle_blob
        .as_ref()
        .map(|r| r.len() < partitions.len())
        .unwrap_or(false);

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
    if use_packed_part_map {
        flags |= P_INTER_FLAG_PACKED_PART_MAP;
    }
    out.push(flags);
    if block_aq_wire {
        validate_qp_clip_range(settings.min_qp, settings.max_qp)?;
        out.push(settings.min_qp);
        out.push(settings.max_qp);
    }
    if use_packed_part_map {
        let r = rle_blob
            .as_ref()
            .ok_or_else(|| SrsV2Error::syntax("missing partition RLE payload"))?;
        out.extend_from_slice(r);
        let map_len = r.len() as u64;
        ms.partition.partition_map_bytes = map_len;
        ms.partition.partition_header_bytes = map_len;
    } else {
        out.extend_from_slice(&partitions);
        let map_len = partitions.len() as u64;
        ms.partition.partition_map_bytes = map_len;
        ms.partition.partition_header_bytes = map_len;
    }

    if inter_entropy_mv {
        let mv_blob = rans_encode_mv_bytes(&mv_compact)?;
        let sym_count =
            u32::try_from(mv_compact.len()).map_err(|_| SrsV2Error::Overflow("mv sym"))?;
        let blob_len = u32::try_from(mv_blob.len()).map_err(|_| SrsV2Error::Overflow("mv blob"))?;
        out.extend_from_slice(&sym_count.to_le_bytes());
        out.extend_from_slice(&blob_len.to_le_bytes());
        out.extend_from_slice(&mv_blob);
        ms.partition.partition_mv_bytes = 8 + mv_blob.len() as u64;
        ms.inter_mv.mv_entropy_section_bytes = ms.partition.partition_mv_bytes;
    } else {
        out.extend_from_slice(&mv_compact);
        ms.partition.partition_mv_bytes = mv_compact.len() as u64;
    }

    let res_start = out.len();
    out.extend_from_slice(&body_residual);
    ms.partition.partition_residual_bytes = (out.len() - res_start) as u64;

    ms.nonzero_motion_macroblocks = pu_mvs.iter().filter(|m| m.0 != 0 || m.1 != 0).count() as u32;
    ms.sum_mv_l1 = pu_mvs
        .iter()
        .map(|m| ((m.0.unsigned_abs() + m.1.unsigned_abs()) / 4) as u64)
        .sum();
    ms.inter_mv.mv_prediction_mode = super::inter_mv::MV_PREDICTION_MODE_LABEL;
    ms.inter_mv.mv_compact_bytes = mv_compact.len() as u64;

    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded P-frame var partition",
        });
    }
    Ok(out)
}

fn mc_skip_fill(
    y_plane: &mut VideoPlane<u8>,
    reference: &VideoPlane<u8>,
    lx: u32,
    ly: u32,
    mvx_q: i32,
    mvy_q: i32,
    use_subpel: bool,
) -> Result<(), SrsV2Error> {
    let mvx_i = (mvx_q / 4) as i16;
    let mvy_i = (mvy_q / 4) as i16;
    for row in 0..8 {
        for col in 0..8 {
            let x = lx + col;
            let y = ly + row;
            let pv = if use_subpel {
                sample_luma_bilinear_qpel(reference, x as i32, y as i32, mvx_q, mvy_q)
            } else {
                sample_u8_plane(reference, x as i32 + mvx_i as i32, y as i32 + mvy_i as i32)
            };
            y_plane.samples[y as usize * y_plane.stride + x as usize] = pv;
        }
    }
    Ok(())
}

fn apply_residual_8x8(y_plane: &mut VideoPlane<u8>, lx: u32, ly: u32, pix: &[[i16; 8]; 8]) {
    for row in 0..8 {
        for col in 0..8 {
            let x = lx + col;
            let y = ly + row;
            let base = y_plane.samples[y as usize * y_plane.stride + x as usize] as i32;
            let v = (base + pix[row as usize][col as usize] as i32).clamp(0, 255);
            y_plane.samples[y as usize * y_plane.stride + x as usize] = v as u8;
        }
    }
}

/// Decode **`FR2` rev 19** / **20** P-frame.
pub fn decode_yuv420_p_payload_var_partition(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    reference: &YuvFrame,
    rev_byte: u8,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 || reference.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("P-frame decode requires YUV420p8"));
    }
    let w = seq.width;
    let h = seq.height;
    let mb_cols = w / 16;
    let mb_rows = h / 16;
    let n_mb = (mb_cols * mb_rows) as usize;

    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let qp = read_u8(payload, &mut cur)?;
    let flags = read_u8(payload, &mut cur)?;
    if flags & !0x0F != 0 {
        return Err(SrsV2Error::syntax("unknown P var-partition flags"));
    }
    let use_qpel = flags & P_INTER_FLAG_SUBPEL != 0;
    let block_aq_wire = flags & P_INTER_FLAG_BLOCK_AQ != 0;
    let entropy_chunks = flags & P_INTER_FLAG_ENTROPY_RESIDUAL != 0;
    let packed_part_map = flags & P_INTER_FLAG_PACKED_PART_MAP != 0;
    let _ = entropy_chunks;

    let (clip_min, clip_max) = if block_aq_wire {
        let a = read_u8(payload, &mut cur)?;
        let b = read_u8(payload, &mut cur)?;
        validate_qp_clip_range(a, b)?;
        (a, b)
    } else {
        (1_u8, 51_u8)
    };

    let partitions_owned = if packed_part_map {
        decode_rle_partition_map(payload, &mut cur, n_mb)?
    } else {
        if payload.len() < cur + n_mb {
            return Err(SrsV2Error::Truncated);
        }
        let sl = payload[cur..cur + n_mb].to_vec();
        cur += n_mb;
        for &b in &sl {
            validate_partition_reserved_bits(b)?;
        }
        sl
    };
    let partitions = partitions_owned.as_slice();
    let inter_entropy_mv = rev_byte == FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR[3];

    let pu_mvs = if inter_entropy_mv {
        let sym_count = read_u32(payload, &mut cur)? as usize;
        let blob_len = read_u32(payload, &mut cur)? as usize;
        let max_compact = n_mb.saturating_mul(64).min(MAX_FRAME_PAYLOAD_BYTES);
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
        let mut val = |mx: i32, my: i32| validate_mv_wire(use_qpel, mx, my);
        decode_mv_stream_partitioned(&compact, &mut cc, mb_cols, mb_rows, partitions, &mut val)?
    } else {
        let mut val = |mx: i32, my: i32| validate_mv_wire(use_qpel, mx, my);
        decode_mv_stream_partitioned(payload, &mut cur, mb_cols, mb_rows, partitions, &mut val)?
    };

    let qp_i = qp.max(1) as i16;

    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    let mut pu_cursor = 0usize;
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let pt = validate_partition_reserved_bits(partitions[idx])?;
            let mb_pu_start = pu_cursor;
            let npu = pu_index_layout(pt);
            for pu_idx in 0..npu {
                let mv = pu_mvs[pu_cursor];
                pu_cursor += 1;
                let subs = subblocks_for_pu(pt, pu_idx);
                for &(dx, dy) in subs {
                    let lx = mbx * 16 + dx;
                    let ly = mby * 16 + dy;
                    let ctrl = read_u8(payload, &mut cur)?;
                    if ctrl & !7 != 0 {
                        return Err(SrsV2Error::syntax("bad var-partition ctrl reserved"));
                    }
                    let skip = (ctrl & CTRL_SKIP) != 0;
                    let tx = (ctrl >> TX_SHIFT) & TX_MASK;
                    if skip {
                        mc_skip_fill(&mut y_plane, &reference.y, lx, ly, mv.0, mv.1, use_qpel)?;
                        continue;
                    }
                    if tx == TX_WIRE_16 {
                        return Err(SrsV2Error::syntax("unsupported P transform size"));
                    }
                    if tx == TX_WIRE_8X8 {
                        let chunk_len = read_u32(payload, &mut cur)? as usize;
                        let end = cur.checked_add(chunk_len).ok_or(SrsV2Error::Truncated)?;
                        if end > payload.len() {
                            return Err(SrsV2Error::Truncated);
                        }
                        let chunk = &payload[cur..end];
                        cur = end;
                        mc_skip_fill(&mut y_plane, &reference.y, lx, ly, mv.0, mv.1, use_qpel)?;
                        let qp_delta = if block_aq_wire {
                            let d = chunk[0] as i8;
                            validate_wire_qp_delta(d)?;
                            d
                        } else {
                            0_i8
                        };
                        let eff_qp = if block_aq_wire {
                            apply_qp_delta_clamped(qp, qp_delta, clip_min, clip_max).max(1) as i16
                        } else {
                            qp_i
                        };
                        let pix = if block_aq_wire {
                            decode_p_residual_chunk(&chunk[1..], eff_qp)?
                        } else {
                            decode_p_residual_chunk(chunk, eff_qp)?
                        };
                        apply_residual_8x8(&mut y_plane, lx, ly, &pix);
                    } else if tx == TX_WIRE_4X4 {
                        mc_skip_fill(&mut y_plane, &reference.y, lx, ly, mv.0, mv.1, use_qpel)?;
                        for ry in (0u32..8).step_by(4) {
                            for rx in (0u32..8).step_by(4) {
                                let chunk_len = read_u32(payload, &mut cur)? as usize;
                                let end =
                                    cur.checked_add(chunk_len).ok_or(SrsV2Error::Truncated)?;
                                if end > payload.len() {
                                    return Err(SrsV2Error::Truncated);
                                }
                                let chunk = &payload[cur..end];
                                cur = end;
                                let qp_delta = if block_aq_wire {
                                    let d = chunk[0] as i8;
                                    validate_wire_qp_delta(d)?;
                                    d
                                } else {
                                    0_i8
                                };
                                let eff_qp = if block_aq_wire {
                                    apply_qp_delta_clamped(qp, qp_delta, clip_min, clip_max).max(1)
                                        as i16
                                } else {
                                    qp_i
                                };
                                let p4 = if block_aq_wire {
                                    decode_p_residual_chunk_4x4(&chunk[1..], eff_qp)?
                                } else {
                                    decode_p_residual_chunk_4x4(chunk, eff_qp)?
                                };
                                for r in 0..4 {
                                    for c in 0..4 {
                                        let x = lx + rx + c;
                                        let y = ly + ry + r;
                                        let base = y_plane.samples
                                            [y as usize * y_plane.stride + x as usize]
                                            as i32;
                                        let v = (base + p4[r as usize][c as usize] as i32)
                                            .clamp(0, 255);
                                        y_plane.samples[y as usize * y_plane.stride + x as usize] =
                                            v as u8;
                                    }
                                }
                            }
                        }
                    } else {
                        return Err(SrsV2Error::syntax("bad P var-partition transform wire"));
                    }
                }
            }
            let chroma_mv = pu_mvs[mb_pu_start];
            copy_chroma_mb8_qpel_local(
                &reference.u,
                &mut u_plane,
                mbx,
                mby,
                chroma_mv.0,
                chroma_mv.1,
            );
            copy_chroma_mb8_qpel_local(
                &reference.v,
                &mut v_plane,
                mbx,
                mby,
                chroma_mv.0,
                chroma_mv.1,
            );
        }
    }

    if pu_cursor != pu_mvs.len() {
        return Err(SrsV2Error::syntax("P var-partition MV cursor"));
    }
    if cur != payload.len() {
        return Err(SrsV2Error::syntax("trailing P var-partition bytes"));
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
    use crate::srsv2::error::SrsV2Error;
    use crate::srsv2::inter_mv::decode_mv_stream_partitioned;
    use crate::srsv2::model::{ColorRange, VideoSequenceHeaderV2};
    use crate::srsv2::rate_control::{
        ResidualEntropy, SrsV2EncodeSettings, SrsV2InterPartitionMode, SrsV2InterSyntaxMode,
        SrsV2MotionSearchMode, SrsV2TransformSizeMode,
    };

    fn seq_inter16() -> VideoSequenceHeaderV2 {
        VideoSequenceHeaderV2 {
            width: 16,
            height: 16,
            profile: crate::srsv2::model::SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: crate::srsv2::model::ColorPrimaries::Bt709,
            transfer: crate::srsv2::model::TransferFunction::Sdr,
            matrix: crate::srsv2::model::MatrixCoefficients::Bt709,
            chroma_siting: crate::srsv2::model::ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        }
    }

    /// Byte index of the first residual control byte (matches [`decode_yuv420_p_payload_var_partition`] MV parse).
    fn residual_cursor_after_mv(payload: &[u8], w: u32, h: u32) -> usize {
        let mb_cols = w / 16;
        let mb_rows = h / 16;
        let n_mb = (mb_cols * mb_rows) as usize;
        let mut cur = 4usize;
        let _frame_index = read_u32(payload, &mut cur).unwrap();
        let _qp = read_u8(payload, &mut cur).unwrap();
        let flags = read_u8(payload, &mut cur).unwrap();
        if flags & P_INTER_FLAG_BLOCK_AQ != 0 {
            cur += 2;
        }
        assert!(payload.len() >= cur + n_mb);
        let partitions = &payload[cur..cur + n_mb];
        cur += n_mb;
        if payload[3] == FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR[3] {
            let _sym_count = read_u32(payload, &mut cur).unwrap() as usize;
            let blob_len = read_u32(payload, &mut cur).unwrap() as usize;
            cur += blob_len;
        } else {
            let mut val = |_mx: i32, _my: i32| Ok(());
            decode_mv_stream_partitioned(payload, &mut cur, mb_cols, mb_rows, partitions, &mut val)
                .unwrap();
        }
        cur
    }

    #[test]
    fn decode_rejects_unsupported_tx16_wire_value() {
        let seq = seq_inter16();
        let rgb = vec![90_u8; 16 * 16 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 16, 16, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            motion_search_mode: SrsV2MotionSearchMode::None,
            inter_syntax_mode: SrsV2InterSyntaxMode::CompactV1,
            inter_partition_mode: SrsV2InterPartitionMode::Fixed16x16,
            residual_entropy: ResidualEntropy::Explicit,
            enable_skip_blocks: false,
            transform_size_mode: SrsV2TransformSizeMode::Force8x8,
            ..Default::default()
        };
        let mut payload = encode_yuv420_p_payload_var_partition(
            &seq, &yuv, &yuv, 1, 28, &settings, None, None, None,
        )
        .unwrap();
        let res = residual_cursor_after_mv(&payload, 16, 16);
        // First 8×8 subblock ctrl: force tx wire = TX_WIRE_16 (value 2) → bits (tx<<1) = 4.
        payload[res] = 4;
        let err = decode_yuv420_p_payload_var_partition(&seq, &payload, &yuv, 19).unwrap_err();
        match err {
            SrsV2Error::Syntax(s) => assert!(s.contains("unsupported P transform")),
            e => panic!("expected Syntax unsupported transform: {e:?}"),
        }
    }

    #[test]
    fn decode_rejects_reserved_transform_wire_symbol() {
        let seq = seq_inter16();
        let rgb = vec![90_u8; 16 * 16 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 16, 16, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            motion_search_mode: SrsV2MotionSearchMode::None,
            inter_syntax_mode: SrsV2InterSyntaxMode::CompactV1,
            inter_partition_mode: SrsV2InterPartitionMode::Fixed16x16,
            residual_entropy: ResidualEntropy::Explicit,
            enable_skip_blocks: false,
            transform_size_mode: SrsV2TransformSizeMode::Force8x8,
            ..Default::default()
        };
        let mut payload = encode_yuv420_p_payload_var_partition(
            &seq, &yuv, &yuv, 1, 28, &settings, None, None, None,
        )
        .unwrap();
        let res = residual_cursor_after_mv(&payload, 16, 16);
        // Illegal tx nibble 3 → ctrl byte (3<<1)=6 (still passes ctrl & !7 check).
        payload[res] = 6;
        let err = decode_yuv420_p_payload_var_partition(&seq, &payload, &yuv, 19).unwrap_err();
        match err {
            SrsV2Error::Syntax(s) => assert!(s.contains("bad P var-partition transform wire")),
            e => panic!("expected Syntax bad transform wire: {e:?}"),
        }
    }

    #[test]
    fn partition_map_rle_roundtrip_all_same() {
        let p = vec![P_PART_WIRE_16X16; 64];
        let r = rle_partition_payload(&p).unwrap();
        assert!(r.len() < p.len(), "RLE should beat raw for uniform map");
        let mut c = 0usize;
        let out = decode_rle_partition_map(&r, &mut c, 64).unwrap();
        assert_eq!(out, p);
        assert_eq!(c, r.len());
    }

    #[test]
    fn partition_map_rle_rejects_run_overflow_vs_mb_count() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&1u16.to_le_bytes());
        blob.push(P_PART_WIRE_16X16);
        blob.extend_from_slice(&100u16.to_le_bytes());
        let mut c = 0usize;
        let err = decode_rle_partition_map(&blob, &mut c, 64).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn partition_map_rle_rejects_truncated_header() {
        let blob = 1u16.to_le_bytes();
        let mut c = 0usize;
        assert!(decode_rle_partition_map(&blob, &mut c, 64).is_err());
    }
}
