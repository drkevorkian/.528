//! Frame-level adaptive quantization (experimental): derives one effective QP from per-MB activity.
//! Optional **block-level** `qp_delta` bytes use versioned `FR2` rev **7**/**8**/**9**.

use super::activity::{mb_activity_y16, screen_activity_score, BlockActivity};
use super::error::SrsV2Error;
use super::frame::YuvFrame;
use super::limits::{QP_DELTA_WIRE_MAX, QP_DELTA_WIRE_MIN};
use super::rate_control::{SrsV2AdaptiveQuantizationMode, SrsV2BlockAqMode, SrsV2EncodeSettings};

/// Statistics for **on-wire** per-**8×8** `qp_delta` when using [`SrsV2BlockAqMode::BlockDelta`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SrsV2BlockAqWireStats {
    pub block_aq_enabled: bool,
    pub min_block_qp_used: u8,
    pub max_block_qp_used: u8,
    pub avg_block_qp: f64,
    pub positive_qp_delta_blocks: u32,
    pub negative_qp_delta_blocks: u32,
    pub unchanged_qp_blocks: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SrsV2AqEncodeStats {
    pub aq_enabled: bool,
    pub base_qp: u8,
    pub effective_qp: u8,
    /// When frame-level AQ is on: min per-**macroblock** hinted QP (not per 8×8 wire delta).
    pub min_block_qp_used: u8,
    pub max_block_qp_used: u8,
    pub avg_block_qp: f64,
    /// Frame-level AQ: counts of MBs with QP hint above / below / equal to `base_qp`.
    pub positive_qp_delta_blocks: u32,
    pub negative_qp_delta_blocks: u32,
    pub unchanged_qp_blocks: u32,
    /// On-wire **8×8** block `qp_delta` stats when [`SrsV2BlockAqMode::BlockDelta`] is active.
    pub block_wire: SrsV2BlockAqWireStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrsV2AqError {
    InvalidDeltaRange { min_d: i8, max_d: i8 },
    BlockAqWireDeltaRange { min_d: i8, max_d: i8 },
}

impl std::fmt::Display for SrsV2AqError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDeltaRange { min_d, max_d } => {
                write!(
                    f,
                    "min_block_qp_delta ({min_d}) must be <= max_block_qp_delta ({max_d})"
                )
            }
            Self::BlockAqWireDeltaRange { min_d, max_d } => write!(
                f,
                "block qp_delta clamp [{min_d},{max_d}] must stay within wire [{},{}]",
                QP_DELTA_WIRE_MIN, QP_DELTA_WIRE_MAX
            ),
        }
    }
}

impl std::error::Error for SrsV2AqError {}

pub fn validate_adaptive_quant_settings(
    settings: &SrsV2EncodeSettings,
) -> Result<(), SrsV2AqError> {
    if settings.min_block_qp_delta > settings.max_block_qp_delta {
        return Err(SrsV2AqError::InvalidDeltaRange {
            min_d: settings.min_block_qp_delta,
            max_d: settings.max_block_qp_delta,
        });
    }
    if settings.block_aq_mode == SrsV2BlockAqMode::BlockDelta
        && (settings.min_block_qp_delta < QP_DELTA_WIRE_MIN
            || settings.max_block_qp_delta > QP_DELTA_WIRE_MAX)
    {
        return Err(SrsV2AqError::BlockAqWireDeltaRange {
            min_d: settings.min_block_qp_delta,
            max_d: settings.max_block_qp_delta,
        });
    }
    Ok(())
}

/// Fold plane-level block-AQ counters into frame stats (`min`/`max`/`avg` re-derived from totals).
#[allow(clippy::too_many_arguments)]
pub fn accumulate_block_aq_wire_plane(
    acc: &mut SrsV2BlockAqWireStats,
    plane_blocks: u32,
    sum_eff_qp: u64,
    min_qp: u8,
    max_qp: u8,
    pos_d: u32,
    neg_d: u32,
    zero_d: u32,
) {
    if plane_blocks == 0 {
        return;
    }
    if !acc.block_aq_enabled {
        acc.block_aq_enabled = true;
        acc.min_block_qp_used = min_qp;
        acc.max_block_qp_used = max_qp;
        acc.avg_block_qp = 0.0;
    } else {
        acc.min_block_qp_used = acc.min_block_qp_used.min(min_qp);
        acc.max_block_qp_used = acc.max_block_qp_used.max(max_qp);
    }
    let prev_total =
        acc.positive_qp_delta_blocks + acc.negative_qp_delta_blocks + acc.unchanged_qp_blocks;
    let new_total = prev_total + plane_blocks;
    let prev_sum = (acc.avg_block_qp * prev_total as f64).round() as u64;
    let sum_all = prev_sum + sum_eff_qp;
    acc.avg_block_qp = sum_all as f64 / new_total as f64;
    acc.positive_qp_delta_blocks += pos_d;
    acc.negative_qp_delta_blocks += neg_d;
    acc.unchanged_qp_blocks += zero_d;
}

fn score_for_mode(mode: SrsV2AdaptiveQuantizationMode, act: &BlockActivity) -> u64 {
    match mode {
        SrsV2AdaptiveQuantizationMode::Off => 0,
        SrsV2AdaptiveQuantizationMode::Activity => act
            .variance_sum
            .saturating_div(256)
            .saturating_add(act.edge_sum / 16),
        SrsV2AdaptiveQuantizationMode::EdgeAware => act
            .edge_sum
            .saturating_mul(2)
            .saturating_add(act.variance_sum / 512),
        SrsV2AdaptiveQuantizationMode::ScreenAware => screen_activity_score(act),
    }
}

fn qp_delta_from_score(score: u64, strength: u8, min_d: i8, max_d: i8, median_score: u64) -> i8 {
    let st = strength.max(1) as i64;
    let sc = score as i64;
    let med = median_score as i64;
    // Avoid division by zero when median is 0; offset uses the true median for `(score - median)`.
    let med_denom = median_score.max(1) as i64;
    // Positive delta => higher QP on complex regions vs median (more compression).
    let raw = ((sc - med) * st) / (med_denom * 4 + 1);
    let d = raw.clamp(i64::from(min_d), i64::from(max_d)) as i8;
    d.clamp(min_d, max_d)
}

fn median_u64_slice(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2
    } else {
        values[mid]
    }
}

/// Returns effective QP for the whole frame and statistics (per-block suggestions aggregated).
pub fn resolve_frame_adaptive_qp(
    base_qp: u8,
    yuv: &YuvFrame,
    settings: &SrsV2EncodeSettings,
) -> Result<(u8, SrsV2AqEncodeStats), SrsV2Error> {
    validate_adaptive_quant_settings(settings)
        .map_err(|_| SrsV2Error::syntax("invalid aq qp delta range"))?;

    let base = settings.clamp_qp(base_qp);
    if settings.adaptive_quantization_mode == SrsV2AdaptiveQuantizationMode::Off {
        return Ok((
            base,
            SrsV2AqEncodeStats {
                aq_enabled: false,
                base_qp: base,
                effective_qp: base,
                min_block_qp_used: base,
                max_block_qp_used: base,
                avg_block_qp: base as f64,
                positive_qp_delta_blocks: 0,
                negative_qp_delta_blocks: 0,
                unchanged_qp_blocks: 0,
                block_wire: SrsV2BlockAqWireStats::default(),
            },
        ));
    }

    let plane = &yuv.y;
    let mb_cols = plane.width / 16;
    let mb_rows = plane.height / 16;
    let total = mb_cols.saturating_mul(mb_rows);
    if total == 0 {
        return Ok((
            base,
            SrsV2AqEncodeStats {
                aq_enabled: true,
                base_qp: base,
                effective_qp: base,
                min_block_qp_used: base,
                max_block_qp_used: base,
                avg_block_qp: base as f64,
                positive_qp_delta_blocks: 0,
                negative_qp_delta_blocks: 0,
                unchanged_qp_blocks: 1,
                block_wire: SrsV2BlockAqWireStats::default(),
            },
        ));
    }

    let mut scores = Vec::with_capacity(total as usize);
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let act = mb_activity_y16(plane, mbx, mby);
            let sc = score_for_mode(settings.adaptive_quantization_mode, &act);
            scores.push(sc);
        }
    }
    let mut sorted_scores = scores.clone();
    let median_score = median_u64_slice(&mut sorted_scores);

    let mut block_qps = Vec::with_capacity(total as usize);
    let mut pos = 0u32;
    let mut neg = 0u32;
    let mut zero = 0u32;

    for sc in &scores {
        let d = qp_delta_from_score(
            *sc,
            settings.aq_strength,
            settings.min_block_qp_delta,
            settings.max_block_qp_delta,
            median_score,
        );
        let iq = base.saturating_add_signed(d);
        let iq = settings.clamp_qp(iq);
        block_qps.push(iq);
        let db = iq as i16 - base as i16;
        if db > 0 {
            pos += 1;
        } else if db < 0 {
            neg += 1;
        } else {
            zero += 1;
        }
    }

    let sum: u64 = block_qps.iter().map(|&q| q as u64).sum();
    let avg = sum as f64 / block_qps.len() as f64;
    let eff = (avg.round() as u8).clamp(settings.min_qp, settings.max_qp);

    let min_b = *block_qps.iter().min().unwrap_or(&base);
    let max_b = *block_qps.iter().max().unwrap_or(&base);

    Ok((
        eff,
        SrsV2AqEncodeStats {
            aq_enabled: true,
            base_qp: base,
            effective_qp: eff,
            min_block_qp_used: min_b,
            max_block_qp_used: max_b,
            avg_block_qp: avg,
            positive_qp_delta_blocks: pos,
            negative_qp_delta_blocks: neg,
            unchanged_qp_blocks: zero,
            block_wire: SrsV2BlockAqWireStats::default(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::frame::VideoPlane;
    use crate::srsv2::model::PixelFormat;

    fn gray_frame(v: u8, w: u32, h: u32) -> YuvFrame {
        let y = VideoPlane::try_new(w, h, w as usize).unwrap();
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        let mut u = VideoPlane::try_new(cw, ch, cw as usize).unwrap();
        let mut vpl = VideoPlane::try_new(cw, ch, cw as usize).unwrap();
        u.samples.fill(128);
        vpl.samples.fill(128);
        let mut yy = y;
        yy.samples.fill(v);
        YuvFrame {
            format: PixelFormat::Yuv420p8,
            y: yy,
            u,
            v: vpl,
        }
    }

    #[test]
    fn aq_off_keeps_effective_qp_equal_base() {
        let yuv = gray_frame(100, 64, 64);
        let s = SrsV2EncodeSettings {
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
            ..Default::default()
        };
        let (eff, st) = resolve_frame_adaptive_qp(33, &yuv, &s).unwrap();
        assert_eq!(eff, 33);
        assert_eq!(st.base_qp, 33);
        assert_eq!(st.effective_qp, 33);
        assert!(!st.aq_enabled);
    }

    #[test]
    fn aq_respects_min_max_qp_after_resolve() {
        let mut yuv = gray_frame(80, 64, 64);
        for y in 0..64usize {
            for x in 32..64usize {
                let v = if (x / 4 + y / 4) % 2 == 0 {
                    40_u8
                } else {
                    220_u8
                };
                yuv.y.samples[y * 64 + x] = v;
            }
        }
        let s = SrsV2EncodeSettings {
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Activity,
            aq_strength: 24,
            min_qp: 12,
            max_qp: 18,
            min_block_qp_delta: -8,
            max_block_qp_delta: 8,
            ..Default::default()
        };
        let (eff, st) = resolve_frame_adaptive_qp(40, &yuv, &s).unwrap();
        assert!((12..=18).contains(&eff));
        assert!((12..=18).contains(&st.min_block_qp_used));
        assert!((12..=18).contains(&st.max_block_qp_used));
    }

    #[test]
    fn validation_rejects_inverted_delta_range() {
        let s = SrsV2EncodeSettings {
            min_block_qp_delta: 4,
            max_block_qp_delta: -4,
            ..Default::default()
        };
        assert!(validate_adaptive_quant_settings(&s).is_err());
    }

    #[test]
    fn block_aq_wire_rejects_delta_clamp_outside_of_plus_minus_24() {
        use crate::srsv2::rate_control::SrsV2BlockAqMode;
        let s = SrsV2EncodeSettings {
            block_aq_mode: SrsV2BlockAqMode::BlockDelta,
            min_block_qp_delta: -30,
            max_block_qp_delta: 6,
            ..Default::default()
        };
        assert!(matches!(
            validate_adaptive_quant_settings(&s),
            Err(SrsV2AqError::BlockAqWireDeltaRange { .. })
        ));
    }

    #[test]
    fn checker_pattern_aq_changes_effective_qp() {
        // Uniform checker repeats per MB → per-MB scores match → deltas vs median are zero.
        // Use left-flat / right-checker so MB scores differ and AQ produces nonzero ± deltas.
        let mut yuv = gray_frame(120, 64, 64);
        for y in 0..64usize {
            for x in 32..64usize {
                let v = if (x / 4 + y / 4) % 2 == 0 {
                    60_u8
                } else {
                    200_u8
                };
                yuv.y.samples[y * 64 + x] = v;
            }
        }
        let s = SrsV2EncodeSettings {
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Activity,
            aq_strength: 8,
            min_block_qp_delta: -4,
            max_block_qp_delta: 6,
            ..Default::default()
        };
        let (_eff, st) = resolve_frame_adaptive_qp(22, &yuv, &s).unwrap();
        assert!(st.aq_enabled);
        assert!(st.positive_qp_delta_blocks > 0 || st.negative_qp_delta_blocks > 0);
        assert!(st.min_block_qp_used <= st.max_block_qp_used);
    }

    #[test]
    fn edge_detail_triggers_aq_deltas_vs_uniform_flat() {
        let mut yuv = gray_frame(80, 64, 64);
        for y in 8..56usize {
            for x in 28..36usize {
                yuv.y.samples[y * 64 + x] = 240;
            }
        }
        let flat = gray_frame(80, 64, 64);
        let s = SrsV2EncodeSettings {
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::EdgeAware,
            aq_strength: 6,
            min_block_qp_delta: -4,
            max_block_qp_delta: 4,
            ..Default::default()
        };
        let (_, st_edge) = resolve_frame_adaptive_qp(24, &yuv, &s).unwrap();
        let (_, st_flat) = resolve_frame_adaptive_qp(24, &flat, &s).unwrap();
        assert_eq!(
            st_flat.positive_qp_delta_blocks + st_flat.negative_qp_delta_blocks,
            0
        );
        assert!(st_edge.positive_qp_delta_blocks + st_edge.negative_qp_delta_blocks > 0);
    }

    #[test]
    fn aq_activity_mixed_detail_changes_effective_qp_from_base() {
        let mut yuv = gray_frame(120, 64, 64);
        for y in 0..64usize {
            for x in 32..64usize {
                let v = if (x / 4 + y / 4) % 2 == 0 {
                    60_u8
                } else {
                    200_u8
                };
                yuv.y.samples[y * 64 + x] = v;
            }
        }
        let s = SrsV2EncodeSettings {
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Activity,
            aq_strength: 12,
            min_qp: 8,
            max_qp: 48,
            min_block_qp_delta: -6,
            max_block_qp_delta: 8,
            ..Default::default()
        };
        let (eff, st) = resolve_frame_adaptive_qp(22, &yuv, &s).unwrap();
        assert!(st.aq_enabled);
        assert_eq!(st.effective_qp, eff);
        assert!(
            st.positive_qp_delta_blocks > 0 || st.negative_qp_delta_blocks > 0,
            "mixed-detail activity should propose ± block QP deltas"
        );
        assert_ne!(st.min_block_qp_used, st.max_block_qp_used);
    }
}
