//! Versioned **block-level QP delta** helpers (`FR2` rev **7**/**8**/**9**).
//!
//! Wire format stores **`qp_delta` as one signed byte** per **8×8** residual block (or P sub-block).
//! Hostile-input bounds: **[`crate::srsv2::limits::QP_DELTA_WIRE_MIN`],
//! [`crate::srsv2::limits::QP_DELTA_WIRE_MAX`]**. Effective QP clamps to a per-frame clip range carried
//! in the payload **after** `base_qp`.
//!
//! ## Semantics vs frame-level AQ
//!
//! Frame-level adaptive quantization (`crate::srsv2::adaptive_quant`) compares
//! each **16×16** macroblock activity score to the frame median: **above median ⇒ positive `qp_delta` ⇒
//! higher effective QP** on busier macroblocks (more compression tilt).
//!
//! Block-level AQ here compares each **8×8** sample-variance tile to the plane median: **above median ⇒
//! negative `qp_delta` ⇒ lower effective QP`** (detail preservation). Flat tiles skew **positive or zero**
//! vs the median. Tune with **`aq_strength`** and **`min_block_qp_delta` / `max_block_qp_delta`**.
//!
//! ## Chroma
//!
//! **Intra rev 7** emits **`qp_delta` per 8×8 block on Y, U, and V** independently (variance from each
//! plane). **P-frame rev 8/9** carries **`qp_delta` only on non-skipped luma 8×8 residuals**; chroma is
//! still reference-copy with **no chroma residual**, so there is **no** chroma block QP syntax on P.

use super::error::SrsV2Error;
use super::frame::VideoPlane;
use super::limits::{QP_DELTA_WIRE_MAX, QP_DELTA_WIRE_MIN};

#[inline]
pub fn validate_wire_qp_delta(d: i8) -> Result<(), SrsV2Error> {
    if !(QP_DELTA_WIRE_MIN..=QP_DELTA_WIRE_MAX).contains(&d) {
        return Err(SrsV2Error::syntax("qp_delta out of wire range"));
    }
    Ok(())
}

/// Clamp **`base_qp + delta`** into **`[clip_min, clip_max]`** then enforce **`≥ 1`** for transform use.
#[inline]
pub fn apply_qp_delta_clamped(base_qp: u8, delta: i8, clip_min: u8, clip_max: u8) -> u8 {
    let sum = (base_qp as i16).saturating_add(delta as i16);
    let clipped = sum.clamp(clip_min as i16, clip_max as i16);
    clipped.clamp(1, 51) as u8
}

pub fn validate_qp_clip_range(clip_min: u8, clip_max: u8) -> Result<(), SrsV2Error> {
    if clip_min > clip_max || clip_min == 0 || clip_max > 51 {
        return Err(SrsV2Error::syntax("invalid qp clip range"));
    }
    Ok(())
}

/// Sample variance of an **8×8** region (padding edge samples as **128**).
pub fn compute_block_variance_8x8(
    plane: &VideoPlane<u8>,
    bx: usize,
    by: usize,
    pw: usize,
    ph: usize,
) -> u32 {
    let mut sum = 0_u64;
    let mut sumsq = 0_u64;
    let n = 64_u64;
    for r in 0..8usize {
        for c in 0..8usize {
            let x = bx * 8 + c;
            let y = by * 8 + r;
            let v = if x < pw && y < ph {
                plane.samples[y * plane.stride + x] as u64
            } else {
                128
            };
            sum += v;
            sumsq += v * v;
        }
    }
    let mean = sum / n;
    let var = sumsq / n - mean * mean;
    var.min(u32::MAX as u64) as u32
}

fn median_u32(values: &mut [u32]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] as u64 + values[mid] as u64) / 2
    } else {
        values[mid] as u64
    }
}

/// Choose a bounded **`qp_delta`** from local sample variance vs median plane variance.
///
/// Formula: `raw = -((block_var - median_var) * strength) / (median_var * 4 + 1)`, then clamp to
/// `[delta_min, delta_max]`.
pub fn choose_block_qp_delta(
    block_var: u32,
    median_var: u64,
    aq_strength: u8,
    delta_min: i8,
    delta_max: i8,
) -> i8 {
    let st = aq_strength.max(1) as i64;
    let sc = block_var as i64;
    let med = median_var.max(1) as i64;
    // High variance vs median ⇒ negative delta ⇒ lower effective QP (preserve detail).
    let raw = -((sc - med) * st) / (med * 4 + 1);
    let lo = delta_min as i64;
    let hi = delta_max as i64;
    let d = raw.clamp(lo, hi) as i8;
    d.clamp(delta_min, delta_max)
}

/// Collect variances for every **8×8** tile in the padded grid (same covering as intra entropy).
pub fn collect_plane_block_variances(plane: &VideoPlane<u8>) -> (Vec<u32>, u64) {
    let w = plane.width as usize;
    let h = plane.height as usize;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let bw = pw / 8;
    let bh = ph / 8;
    let mut vars = Vec::with_capacity(bw * bh);
    for by in 0..bh {
        for bx in 0..bw {
            vars.push(compute_block_variance_8x8(plane, bx, by, w, h));
        }
    }
    let mut sorted = vars.clone();
    let med = median_u32(&mut sorted);
    (vars, med)
}

/// Precompute P-frame luma **8×8** sub-block variances in macroblock raster order (4 sub-blocks per MB).
pub fn collect_p_subblock_variances(
    plane: &VideoPlane<u8>,
    mb_cols: u32,
    mb_rows: u32,
) -> Vec<u32> {
    let w = plane.width as usize;
    let h = plane.height as usize;
    let mut out = Vec::with_capacity((mb_cols * mb_rows * 4) as usize);
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            for &(dx, dy) in &[(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)] {
                let bx = (mbx * 16 + dx) as usize / 8;
                let by = (mby * 16 + dy) as usize / 8;
                out.push(compute_block_variance_8x8(plane, bx, by, w, h));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_qp_delta_clamps_to_clip() {
        assert_eq!(apply_qp_delta_clamped(20, 10, 4, 51), 30);
        assert_eq!(apply_qp_delta_clamped(10, -20, 4, 51), 4);
        assert_eq!(apply_qp_delta_clamped(1, -5, 4, 51), 4);
    }

    #[test]
    fn wire_delta_out_of_range_errors() {
        assert!(validate_wire_qp_delta(25).is_err());
        assert!(validate_wire_qp_delta(-25).is_err());
        assert!(validate_wire_qp_delta(24).is_ok());
    }

    #[test]
    fn negative_delta_lowers_effective_qp_vs_positive() {
        let lo = apply_qp_delta_clamped(28, -4, 4, 51);
        let hi = apply_qp_delta_clamped(28, 4, 4, 51);
        assert!(lo < hi);
    }

    #[test]
    fn choose_block_delta_detail_prefers_negative_vs_flat() {
        let med = 100_u64;
        let flat_var = 80_u32;
        let edge_var = 800_u32;
        let d_flat = choose_block_qp_delta(flat_var, med, 8, -6, 6);
        let d_edge = choose_block_qp_delta(edge_var, med, 8, -6, 6);
        assert!(
            d_flat >= d_edge,
            "higher activity should steer toward lower QP (negative delta)"
        );
    }

    #[test]
    fn very_flat_block_vs_high_median_hits_positive_delta_cap() {
        // block_var << median_var ⇒ raw > 0 ⇒ compress flat regions harder when allowed.
        let d = choose_block_qp_delta(1, 5000, 25, -6, 6);
        assert_eq!(d, 6);
    }

    #[test]
    fn block_variance_near_median_yields_small_delta() {
        let d = choose_block_qp_delta(1000, 1000, 8, -6, 6);
        assert_eq!(d, 0);
    }
}
