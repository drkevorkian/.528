//! Centralized **rate–distortion** helpers for SRSV2 experimental encoders.
//!
//! ## Score model
//!
//! Effective score (fixed-point λ, **`lambda_fp ≈ λ × 256`** from [`crate::srsv2::rate_control::rdo_lambda_effective`]):
//!
//! ```text
//! score = distortion + (lambda_fp × estimated_wire_bytes) / 256
//!       ≈ distortion + λ × estimated_wire_bytes    (when lambda_fp = round(λ × 256))
//! ```
//!
//! [`RdoCost::total_wire_bytes`] sums **partition map**, **MV**, **residual**, **transform ID**,
//! **block AQ**, **skip flags**, and **B blend/weight** buckets for λ·rate terms.
//!
//! Callers pass [`crate::srsv2::rate_control::rdo_lambda_effective`] as `lambda_fp` unless a
//! partition-specific scaled λ is intentionally used (see [`partition_rdo_fast_score`]).
//!
//! **Safety:** candidate lists are **hard-capped** ([`MAX_RDO_CANDIDATES`]); [`bounded_candidate_push`]
//! and choosers return [`crate::srsv2::error::SrsV2Error`] when exceeded.

use super::error::SrsV2Error;
use super::inter_mv::{predict_mv_qpel, pu_count_partition_wire, signed_varint_wire_bytes};
use super::motion_search::SrsV2RdoBenchStats;
use super::rate_control::{SrsV2InterSyntaxMode, SrsV2RdoMode};

/// Maximum partition / inter-mode candidates evaluated in one RDO decision site.
pub const MAX_RDO_CANDIDATES: usize = 16;

/// Best-effort **on-wire byte** breakdown for λ·rate terms (informational + scoring).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RdoCost {
    pub partition_map_bytes: u64,
    pub mv_compact_or_entropy_bytes: u64,
    pub residual_bytes: u64,
    pub transform_id_bytes: u64,
    pub block_aq_bytes: u64,
    pub skip_flags_bytes: u64,
    pub blend_weight_bytes: u64,
}

impl RdoCost {
    #[inline]
    pub fn total_wire_bytes(&self) -> i64 {
        let s = self
            .partition_map_bytes
            .saturating_add(self.mv_compact_or_entropy_bytes);
        let s = s
            .saturating_add(self.residual_bytes)
            .saturating_add(self.transform_id_bytes);
        let s = s
            .saturating_add(self.block_aq_bytes)
            .saturating_add(self.skip_flags_bytes)
            .saturating_add(self.blend_weight_bytes);
        i64::try_from(s).unwrap_or(i64::MAX)
    }

    /// Single bucket (MV-only side cost, B blend base wire, etc.).
    pub fn from_total_bytes(n: u64) -> Self {
        Self {
            mv_compact_or_entropy_bytes: n,
            ..Default::default()
        }
    }
}

/// One scored alternative in an RDO search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdoCandidate {
    pub id: u8,
    pub distortion: u32,
    pub cost: RdoCost,
}

/// Outcome of [`choose_best_partition_candidate`] / [`choose_best_inter_mode_candidate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdoDecision {
    pub chosen_index: usize,
    pub chosen_id: u8,
    pub best_score: i128,
}

/// Lightweight counters for a single RDO site (optional merge into [`SrsV2RdoBenchStats`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RdoStats {
    pub candidates_compared: u32,
    pub estimated_side_bytes: u64,
}

impl RdoStats {
    pub fn merge_into_bench(&self, bench: &mut SrsV2RdoBenchStats) {
        bench.candidates_tested = bench
            .candidates_tested
            .saturating_add(u64::from(self.candidates_compared));
        bench.estimated_bits_used_for_decision = bench
            .estimated_bits_used_for_decision
            .saturating_add(self.estimated_side_bytes);
    }
}

/// Per-pass counters for high-level **mode** RDO decisions (merge into [`SrsV2RdoBenchStats`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RdoModeDecisionStats {
    pub inter_zero_mv_wins: u64,
    pub inter_me_mv_wins: u64,
}

impl RdoModeDecisionStats {
    #[inline]
    pub fn record_inter_mv_pick(&mut self, chose_zero_mv: bool) {
        if chose_zero_mv {
            self.inter_zero_mv_wins = self.inter_zero_mv_wins.saturating_add(1);
        } else {
            self.inter_me_mv_wins = self.inter_me_mv_wins.saturating_add(1);
        }
    }

    pub fn merge_into_bench(&self, bench: &mut SrsV2RdoBenchStats) {
        bench.inter_zero_mv_wins = bench
            .inter_zero_mv_wins
            .saturating_add(self.inter_zero_mv_wins);
        bench.inter_me_mv_wins = bench.inter_me_mv_wins.saturating_add(self.inter_me_mv_wins);
    }
}

/// λ·D + rate term (**fixed-point λ**, **256 ≈ 1.0**).
#[inline]
pub fn rdo_score(distortion: u32, lambda_fp: i64, wire_bytes: i64) -> i128 {
    distortion as i128 + (lambda_fp as i128 * wire_bytes.max(0) as i128) / 256
}

#[inline]
pub fn score_candidate(distortion: u32, lambda_fp: i64, cost: &RdoCost) -> i128 {
    rdo_score(distortion, lambda_fp, cost.total_wire_bytes())
}

/// Best-effort **fixed** P-frame prefix bytes through QP/flags (and optional block-AQ clip range).
/// Does **not** include MV grid, entropy blob, residuals, or transforms — use [`RdoCost`] fields for those.
#[must_use]
pub fn estimate_inter_header_bytes(inter_syntax: SrsV2InterSyntaxMode, block_aq_wire: bool) -> u64 {
    let aq = if block_aq_wire { 2u64 } else { 0 };
    match inter_syntax {
        SrsV2InterSyntaxMode::RawLegacy => 4 + 4 + 1 + aq,
        SrsV2InterSyntaxMode::CompactV1 | SrsV2InterSyntaxMode::EntropyV1 => 4 + 4 + 1 + 1 + aq,
    }
}

/// Fold explicit byte buckets into [`RdoCost`] (no magic — callers supply measured/estimated sizes).
pub fn estimate_partition_candidate_bytes(
    partition_map_bytes: u64,
    mv_compact_or_entropy_bytes: u64,
    residual_bytes: u64,
    transform_id_bytes: u64,
    block_aq_bytes: u64,
    skip_flags_bytes: u64,
    blend_weight_bytes: u64,
) -> RdoCost {
    RdoCost {
        partition_map_bytes,
        mv_compact_or_entropy_bytes,
        residual_bytes,
        transform_id_bytes,
        block_aq_bytes,
        skip_flags_bytes,
        blend_weight_bytes,
    }
}

/// **Partition AutoFast `RdoFast`** score (legacy): distortion plus λ·quality_bias·(MV+res) / 256².
/// Prefer [`autofast_partition_mb_rdo_score`] for new code — it folds **full** [`RdoCost`] buckets.
#[inline]
pub fn partition_rdo_fast_score(
    distortion: u32,
    lambda_partition_fp: i64,
    quality_bias_fp: u16,
    mv_b: usize,
    res_b: usize,
) -> i128 {
    let side = (mv_b.saturating_add(res_b)) as i128;
    distortion as i128
        + (lambda_partition_fp as i128 * i128::from(quality_bias_fp) * side) / (256 * 256)
}

/// Per-macroblock wire buckets for **AutoFast** + **`RdoFast`** (partition map, MV body, residual dry-run,
/// transform tags, optional block-AQ signaling). Aligns [`pu_count_partition_wire`] with the unit tests
/// in this module (`estimate_partition_candidate_bytes` proportions for **16×16** vs **8×8**).
#[must_use]
pub fn autofast_partition_mb_wire_cost(
    partition_wire_tag: u8,
    mv_body_bytes: usize,
    residual_body_bytes: usize,
    block_aq_wire: bool,
) -> Result<RdoCost, SrsV2Error> {
    let npu = pu_count_partition_wire(partition_wire_tag)?;
    let (partition_map_bytes, transform_id_bytes) = match npu {
        1 => (0u64, 0u64),
        2 => (1u64, 2u64),
        4 => (4u64, 4u64),
        _ => {
            let e = npu.saturating_sub(1) as u64;
            (e.saturating_mul(2), e.saturating_mul(2))
        }
    };
    let block_aq_bytes = if block_aq_wire {
        npu.saturating_sub(1) as u64
    } else {
        0
    };
    Ok(estimate_partition_candidate_bytes(
        partition_map_bytes,
        mv_body_bytes as u64,
        residual_body_bytes as u64,
        transform_id_bytes,
        block_aq_bytes,
        0,
        0,
    ))
}

/// AutoFast **RdoFast** score: [`score_candidate`] over [`autofast_partition_mb_wire_cost`], with
/// **`partition_quality_bias`** applied to λ the same way the legacy MV+res-only path scaled rate.
#[must_use]
pub fn autofast_partition_mb_rdo_score(
    partition_wire_tag: u8,
    distortion: u32,
    lambda_partition_fp: i64,
    quality_bias_fp: u16,
    mv_body_bytes: usize,
    residual_body_bytes: usize,
    block_aq_wire: bool,
) -> Result<i128, SrsV2Error> {
    let cost = autofast_partition_mb_wire_cost(
        partition_wire_tag,
        mv_body_bytes,
        residual_body_bytes,
        block_aq_wire,
    )?;
    let bias = i64::from(quality_bias_fp.max(1));
    let lam_eff = lambda_partition_fp.saturating_mul(bias) / 256;
    Ok(score_candidate(distortion, lam_eff, &cost))
}

/// **Partition `HeaderAware`** heuristic (legacy encoder behavior).
#[inline]
pub fn partition_header_aware_score(
    distortion: u32,
    lambda_partition_fp: i64,
    split_penalty_fp: i128,
    mv_wire_bytes: usize,
    mv_penalty_fp: u16,
    extra_pu: u32,
    header_penalty_fp: u16,
) -> i128 {
    let mv_b = mv_wire_bytes as i128;
    let extra = extra_pu as i128;
    distortion as i128
        + (lambda_partition_fp as i128 * i128::from(mv_penalty_fp) * mv_b) / (256 * 256)
        + (lambda_partition_fp as i128 * i128::from(header_penalty_fp) * extra) / (256 * 256)
        + (lambda_partition_fp as i128 * split_penalty_fp) / (256 * 256)
}

/// P subblock **skip vs residual** fast-path inequality (matches pre-centralization behavior).
#[inline]
pub fn p_subblock_skip_residual_is_rdo_cheaper(
    max_abs: i16,
    lambda_fp: i64,
    wire_residual_bytes: i64,
) -> bool {
    let lhs = i128::from(max_abs as i32) * 256;
    let rhs = i128::from(lambda_fp) * i128::from(wire_residual_bytes.max(1));
    lhs <= rhs
}

/// Push one [`RdoCandidate`] if under [`MAX_RDO_CANDIDATES`].
pub fn bounded_candidate_push(
    vec: &mut Vec<RdoCandidate>,
    c: RdoCandidate,
) -> Result<(), SrsV2Error> {
    if vec.len() >= MAX_RDO_CANDIDATES {
        return Err(SrsV2Error::syntax("RDO: candidate cap reached"));
    }
    vec.push(c);
    Ok(())
}

/// Compact / entropy MV **delta varint** byte estimate for one macroblock (median predictor).
pub fn estimate_mv_delta_wire_bytes(
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

/// Choose argmin score with deterministic tie-break: lower score, then lower `id`, then lower index.
pub fn choose_best_partition_candidate(
    lambda_fp: i64,
    items: &[RdoCandidate],
    stats: Option<&mut RdoStats>,
) -> Result<RdoDecision, SrsV2Error> {
    if items.is_empty() {
        return Err(SrsV2Error::syntax("RDO partition: empty candidate set"));
    }
    if items.len() > MAX_RDO_CANDIDATES {
        return Err(SrsV2Error::syntax("RDO partition: too many candidates"));
    }
    let mut best_i = 0usize;
    let mut best_s = score_candidate(items[0].distortion, lambda_fp, &items[0].cost);
    let mut best_id = items[0].id;
    let mut side_acc = items[0].cost.total_wire_bytes().max(0) as u64;
    for (i, c) in items.iter().enumerate().skip(1) {
        let s = score_candidate(c.distortion, lambda_fp, &c.cost);
        let better = s < best_s
            || (s == best_s && c.id < best_id)
            || (s == best_s && c.id == best_id && i < best_i);
        if better {
            best_i = i;
            best_s = s;
            best_id = c.id;
        }
        side_acc = side_acc.saturating_add(c.cost.total_wire_bytes().max(0) as u64);
    }
    if let Some(st) = stats {
        st.candidates_compared = items.len() as u32;
        st.estimated_side_bytes = side_acc;
    }
    Ok(RdoDecision {
        chosen_index: best_i,
        chosen_id: best_id,
        best_score: best_s,
    })
}

/// Choose best **inter-mode** row where each candidate is `(distortion, total_side_bytes)`.
/// Same scoring as [`rdo_score`]. Tie: lower distortion, lower bytes, lower index.
pub fn choose_best_inter_mode_candidate(
    lambda_fp: i64,
    rows: &[(u32, i64)],
    stats: Option<&mut RdoStats>,
) -> Result<RdoDecision, SrsV2Error> {
    if rows.is_empty() {
        return Err(SrsV2Error::syntax("RDO inter-mode: empty candidate set"));
    }
    if rows.len() > MAX_RDO_CANDIDATES {
        return Err(SrsV2Error::syntax("RDO inter-mode: too many candidates"));
    }
    let mut best_i = 0usize;
    let mut best_s = rdo_score(rows[0].0, lambda_fp, rows[0].1);
    let mut best_d = rows[0].0;
    let mut best_b = rows[0].1;
    let mut side_acc = rows[0].1.max(0) as u64;
    for (i, &(d, b)) in rows.iter().enumerate().skip(1) {
        let s = rdo_score(d, lambda_fp, b);
        let better = s < best_s
            || (s == best_s && d < best_d)
            || (s == best_s && d == best_d && b < best_b)
            || (s == best_s && d == best_d && b == best_b && i < best_i);
        if better {
            best_i = i;
            best_s = s;
            best_d = d;
            best_b = b;
        }
        side_acc = side_acc.saturating_add(b.max(0) as u64);
    }
    if let Some(st) = stats {
        st.candidates_compared = rows.len() as u32;
        st.estimated_side_bytes = side_acc;
    }
    Ok(RdoDecision {
        chosen_index: best_i,
        chosen_id: best_i as u8,
        best_score: best_s,
    })
}

/// Argmin over precomputed `(partition_wire_tag, score)` pairs (HeaderAware / RdoFast paths).
pub fn choose_min_partition_by_precomputed_scores(
    scored_tags: &[(u8, i128)],
) -> Result<RdoDecision, SrsV2Error> {
    if scored_tags.is_empty() {
        return Err(SrsV2Error::syntax("RDO partition: empty score list"));
    }
    if scored_tags.len() > MAX_RDO_CANDIDATES {
        return Err(SrsV2Error::syntax("RDO partition: too many score rows"));
    }
    let mut best_i = 0usize;
    let mut best_s = scored_tags[0].1;
    let mut best_tag = scored_tags[0].0;
    for (i, &(tag, s)) in scored_tags.iter().enumerate().skip(1) {
        let better = s < best_s
            || (s == best_s && tag < best_tag)
            || (s == best_s && tag == best_tag && i < best_i);
        if better {
            best_i = i;
            best_s = s;
            best_tag = tag;
        }
    }
    Ok(RdoDecision {
        chosen_index: best_i,
        chosen_id: best_tag,
        best_score: best_s,
    })
}

/// B-frame blend **RdoFast** scoring: `distortion + lam * (base + extra + hp) / 256`.
#[inline]
pub fn b_blend_rdo_score(
    distortion: u32,
    lambda_fp: i64,
    base_bytes: i128,
    extra_bytes: i128,
    halfpel_penalty_bytes: i128,
) -> i128 {
    let side_i128 = base_bytes
        .saturating_add(extra_bytes)
        .saturating_add(halfpel_penalty_bytes);
    let side_i64 = i64::try_from(side_i128.clamp(0, i64::MAX as i128)).unwrap_or(i64::MAX);
    rdo_score(distortion, lambda_fp, side_i64)
}

/// Whether fast RDO logic should run for this settings slice.
#[inline]
pub fn rdo_fast_enabled(mode: SrsV2RdoMode) -> bool {
    matches!(mode, SrsV2RdoMode::Fast)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdo_score_matches_lambda_scaling() {
        let s = rdo_score(1000, 256, 10);
        assert_eq!(s, 1000 + 10);
    }

    #[test]
    fn rdo_off_behavior_is_min_distortion_when_lambda_zero() {
        let d = choose_best_inter_mode_candidate(0, &[(500, 4), (400, 100)], None).unwrap();
        assert_eq!(d.chosen_index, 1);
    }

    #[test]
    fn rdo_fast_rejects_split_when_side_cost_dominates() {
        // 16×16: SAD 1000, bytes 10. 8×8: SAD 950 (better) but huge MV+map bytes -> pick 16×16.
        let lam = 256i64;
        let c16 = estimate_partition_candidate_bytes(0, 10, 200, 0, 0, 0, 0);
        let c8 = estimate_partition_candidate_bytes(4, 80, 220, 4, 0, 0, 0);
        let dec = choose_best_partition_candidate(
            lam,
            &[
                RdoCandidate {
                    id: 0,
                    distortion: 1000,
                    cost: c16,
                },
                RdoCandidate {
                    id: 1,
                    distortion: 950,
                    cost: c8,
                },
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            dec.chosen_index, 0,
            "flat/global-like side cost should keep 16×16"
        );
    }

    #[test]
    fn rdo_fast_accepts_split_when_quality_win_overcomes_bytes() {
        let lam = 256i64;
        let c16 = estimate_partition_candidate_bytes(0, 10, 400, 0, 0, 0, 0);
        let c8 = estimate_partition_candidate_bytes(2, 24, 380, 2, 0, 0, 0);
        let dec = choose_best_partition_candidate(
            lam,
            &[
                RdoCandidate {
                    id: 0,
                    distortion: 5000,
                    cost: c16,
                },
                RdoCandidate {
                    id: 1,
                    distortion: 800,
                    cost: c8,
                },
            ],
            None,
        )
        .unwrap();
        assert_eq!(dec.chosen_index, 1);
    }

    #[test]
    fn rdo_fast_halfpel_penalty_can_reject_subpel() {
        let lam = 512i64;
        let integ = b_blend_rdo_score(1000, lam, 8, 0, 0);
        let subp = b_blend_rdo_score(980, lam, 8, 0, 40);
        assert!(
            integ < subp,
            "tiny SAD gain must not pay half-pel side bytes"
        );
    }

    #[test]
    fn rdo_fast_weighted_extra_can_reject_weighted() {
        let lam = 512i64;
        let avg = b_blend_rdo_score(1000, lam, 8, 0, 0);
        let wgt = b_blend_rdo_score(980, lam, 8, 72, 0);
        assert!(avg < wgt);
    }

    #[test]
    fn candidate_cap_errors() {
        let mut v = Vec::new();
        for i in 0..=MAX_RDO_CANDIDATES {
            v.push(RdoCandidate {
                id: i as u8,
                distortion: i as u32,
                cost: RdoCost::default(),
            });
        }
        assert!(choose_best_partition_candidate(256, &v, None).is_err());
    }

    #[test]
    fn bounded_candidate_push_respects_cap() {
        let mut v = Vec::new();
        for i in 0..MAX_RDO_CANDIDATES {
            bounded_candidate_push(
                &mut v,
                RdoCandidate {
                    id: i as u8,
                    distortion: 0,
                    cost: RdoCost::default(),
                },
            )
            .unwrap();
        }
        assert_eq!(v.len(), MAX_RDO_CANDIDATES);
        assert!(bounded_candidate_push(
            &mut v,
            RdoCandidate {
                id: 0,
                distortion: 0,
                cost: RdoCost::default(),
            },
        )
        .is_err());
    }

    #[test]
    fn tie_break_deterministic() {
        let lam = 256i64;
        let c = RdoCost::from_total_bytes(5);
        let dec = choose_best_partition_candidate(
            lam,
            &[
                RdoCandidate {
                    id: 2,
                    distortion: 100,
                    cost: c.clone(),
                },
                RdoCandidate {
                    id: 1,
                    distortion: 100,
                    cost: c.clone(),
                },
            ],
            None,
        )
        .unwrap();
        assert_eq!(dec.chosen_id, 1);
    }

    #[test]
    fn partition_rdo_fast_score_deterministic() {
        let a = partition_rdo_fast_score(900, 4096, 256, 30, 200);
        let b = partition_rdo_fast_score(900, 4096, 256, 30, 200);
        assert_eq!(a, b);
    }

    #[test]
    fn autofast_wire_cost_8x8_matches_expected_buckets() {
        use crate::srsv2::inter_mv::P_PART_WIRE_8X8;
        let c = autofast_partition_mb_wire_cost(P_PART_WIRE_8X8, 80, 220, false).unwrap();
        assert_eq!(c.partition_map_bytes, 4);
        assert_eq!(c.mv_compact_or_entropy_bytes, 80);
        assert_eq!(c.residual_bytes, 220);
        assert_eq!(c.transform_id_bytes, 4);
    }
}
