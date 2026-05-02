//! Experimental **post-reconstruction luma loop filter** (CPU-only, deterministic).
//!
//! Applies a lightweight blend across **8-pixel** block boundaries on reconstructed **Y** only
//! (covers both **8×8** leaves and **16×16** macroblock edges). **Chroma is untouched.**
//!
//! When [`crate::srsv2::model::VideoSequenceHeaderV2::disable_loop_filter`] is **true** (default),
//! [`SrsV2LoopFilterMode::Off`] is used and the plane is unchanged. When **false**, encoders and
//! decoders must apply the same filter to reconstructed Y **before** updating the SRSV2 reference
//! slot so P-frame prediction matches (see `docs/deblock_filter.md`).
//!
//! This is **not** H.264/HEVC deblocking, **CDEF**, **restoration**, or **film grain**.

use super::frame::VideoPlane;

/// High-level loop-filter selection aligned with the sequence header flag
/// [`crate::srsv2::model::VideoSequenceHeaderV2::disable_loop_filter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrsV2LoopFilterMode {
    /// Loop filter disabled — reconstructed Y is unchanged (`disable_loop_filter == true`).
    #[default]
    Off,
    /// Simple boundary deblocking (`disable_loop_filter == false`).
    SimpleDeblock,
}

/// When the sequence header `deblock_strength` byte is **0** and the loop filter is on, this
/// default is used (deterministic, documented).
pub const DEFAULT_DEBLOCK_STRENGTH: u8 = 32;

#[inline]
fn strength_effective(deblock_strength: u8) -> u8 {
    if deblock_strength == 0 {
        DEFAULT_DEBLOCK_STRENGTH
    } else {
        deblock_strength
    }
}

/// Maps the sequence-header **`deblock_strength`** byte to the value used by the filter (**`0`** → [`DEFAULT_DEBLOCK_STRENGTH`]).
#[inline]
pub fn resolve_deblock_strength(byte: u8) -> u8 {
    strength_effective(byte)
}

/// Skip smoothing across a boundary when luma difference exceeds this (preserve sharp edges).
#[inline]
fn edge_threshold(deblock_strength: u8) -> u32 {
    let s = strength_effective(deblock_strength) as u32;
    // Stronger strength ⇒ slightly lower threshold ⇒ more boundaries softened; clamp keeps stable.
    let t = 96u32.saturating_sub(s.saturating_mul(3) / 2);
    t.clamp(4, 120)
}

#[inline]
fn blend_boundary_pair(a: &mut u8, b: &mut u8, deblock_strength: u8) {
    let au = *a as u32;
    let bu = *b as u32;
    if au.abs_diff(bu) > edge_threshold(deblock_strength) {
        return;
    }
    let se = strength_effective(deblock_strength) as u32;
    let k = se.clamp(1, 128);
    let mid = (au + bu).div_ceil(2);
    *a = ((au * (256 - k) + mid * k) / 256).min(255) as u8;
    *b = ((bu * (256 - k) + mid * k) / 256).min(255) as u8;
}

/// Apply the selected loop filter to reconstructed luma **in place**.
///
/// - [`SrsV2LoopFilterMode::Off`]: no-op (frame remains byte-identical).
/// - [`SrsV2LoopFilterMode::SimpleDeblock`]: vertical then horizontal passes on **8-pixel** grid.
///
/// Safe for small planes: dimensions `< 2`, or empty samples, return immediately without panic.
/// Odd widths/heights still run boundary passes where both neighbors exist (`bx < w`, `by < h`).
pub fn apply_loop_filter_y(
    mode: SrsV2LoopFilterMode,
    deblock_strength: u8,
    y: &mut VideoPlane<u8>,
) {
    if matches!(mode, SrsV2LoopFilterMode::Off) {
        return;
    }
    let w = y.width as usize;
    let h = y.height as usize;
    let stride = y.stride;
    if w < 2 || h < 2 || y.samples.len() < stride.saturating_mul(h) {
        return;
    }
    let s = &mut y.samples;

    for bx in (8..w).step_by(8) {
        for row in 0..h {
            let idx_r = row * stride + bx;
            if idx_r >= s.len() || idx_r == 0 {
                continue;
            }
            let idx_l = idx_r - 1;
            let (left, right) = s.split_at_mut(idx_r);
            if idx_l < left.len() {
                blend_boundary_pair(&mut left[idx_l], &mut right[0], deblock_strength);
            }
        }
    }

    for by in (8..h).step_by(8) {
        let split = by * stride;
        if split >= s.len() || split == 0 {
            continue;
        }
        for col in 0..w {
            let idx_a = (by - 1) * stride + col;
            if idx_a >= split || col >= stride {
                continue;
            }
            let (upper, lower) = s.split_at_mut(split);
            if col < lower.len() {
                blend_boundary_pair(&mut upper[idx_a], &mut lower[col], deblock_strength);
            }
        }
    }
}

/// Back-compat wrapper: [`SrsV2LoopFilterMode::SimpleDeblock`] with strength **0** (→ default 32).
#[inline]
pub fn apply_simple_mb_boundary_deblock_y(y: &mut VideoPlane<u8>) {
    apply_loop_filter_y(SrsV2LoopFilterMode::SimpleDeblock, 0, y);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::frame::VideoPlane;

    #[test]
    fn flat_frame_unchanged_under_simple_deblock() {
        let mut p = VideoPlane::try_new(64, 64, 64).unwrap();
        p.samples.fill(111);
        let copy = p.samples.clone();
        apply_loop_filter_y(
            SrsV2LoopFilterMode::SimpleDeblock,
            DEFAULT_DEBLOCK_STRENGTH,
            &mut p,
        );
        assert_eq!(p.samples, copy);
    }

    #[test]
    fn off_mode_byte_identical() {
        let mut p = VideoPlane::try_new(64, 64, 64).unwrap();
        p.samples.fill(40);
        for y in 0..64usize {
            for x in 32..64usize {
                p.samples[y * 64 + x] = 220;
            }
        }
        let copy = p.samples.clone();
        apply_loop_filter_y(SrsV2LoopFilterMode::Off, 99, &mut p);
        assert_eq!(p.samples, copy);
    }

    #[test]
    fn weak_block_boundary_softened() {
        let mut p = VideoPlane::try_new(64, 64, 64).unwrap();
        p.samples.fill(42);
        for y in 0..64usize {
            for x in 32..64usize {
                p.samples[y * 64 + x] = 58;
            }
        }
        let row = 16usize;
        let bx = 32usize;
        let before = (p.samples[row * 64 + bx - 1], p.samples[row * 64 + bx]);
        apply_loop_filter_y(
            SrsV2LoopFilterMode::SimpleDeblock,
            DEFAULT_DEBLOCK_STRENGTH,
            &mut p,
        );
        let after = (p.samples[row * 64 + bx - 1], p.samples[row * 64 + bx]);
        assert_ne!(
            before, after,
            "small step across MB edge should be smoothed"
        );
    }

    #[test]
    fn strong_edge_preserved_above_threshold() {
        let mut p = VideoPlane::try_new(64, 64, 64).unwrap();
        p.samples.fill(10);
        for y in 0..64usize {
            for x in 32..64usize {
                p.samples[y * 64 + x] = 250;
            }
        }
        let row = 20usize;
        let bx = 32usize;
        let before = (p.samples[row * 64 + bx - 1], p.samples[row * 64 + bx]);
        apply_loop_filter_y(
            SrsV2LoopFilterMode::SimpleDeblock,
            DEFAULT_DEBLOCK_STRENGTH,
            &mut p,
        );
        let after = (p.samples[row * 64 + bx - 1], p.samples[row * 64 + bx]);
        assert_eq!(
            before, after,
            "240-step edge should exceed preservation threshold"
        );
    }

    #[test]
    fn tiny_plane_no_panic() {
        let mut p = VideoPlane::try_new(1, 1, 1).unwrap();
        p.samples[0] = 5;
        apply_loop_filter_y(SrsV2LoopFilterMode::SimpleDeblock, 80, &mut p);
        assert_eq!(p.samples[0], 5);

        let mut p2 = VideoPlane::try_new(4, 4, 4).unwrap();
        p2.samples.fill(9);
        apply_loop_filter_y(SrsV2LoopFilterMode::SimpleDeblock, 10, &mut p2);
    }

    #[test]
    fn deterministic_two_passes() {
        let mut a = VideoPlane::try_new(32, 32, 32).unwrap();
        for y in 0..32usize {
            for x in 0..32usize {
                a.samples[y * 32 + x] = ((x.wrapping_mul(17) ^ y.wrapping_mul(31)) & 255) as u8;
            }
        }
        let mut b = a.clone();
        apply_loop_filter_y(SrsV2LoopFilterMode::SimpleDeblock, 44, &mut a);
        apply_loop_filter_y(SrsV2LoopFilterMode::SimpleDeblock, 44, &mut b);
        assert_eq!(a.samples, b.samples);
    }

    #[test]
    fn strength_zero_uses_default_constant() {
        assert_eq!(resolve_deblock_strength(0), DEFAULT_DEBLOCK_STRENGTH);
    }
}
