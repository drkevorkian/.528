//! Half-pel (quarter-pel–grid) luma sampling and refinement for experimental SRSV2 P-frames.
//!
//! Motion is stored in **quarter-pel units** on the wire (`±2` == half-pel). Chroma MC uses an
//! integer approximation (`mv_q / 8`) until a fuller chroma sub-pel path exists.

use super::frame::VideoPlane;
use super::limits::MAX_MOTION_VECTOR_PELS;
use super::motion_search::sample_u8_plane;

/// Maximum absolute quarter-pel motion (includes ±2 half-pel slack beyond integer extremes).
pub(crate) const MAX_MV_QPEL_ABS: i32 = MAX_MOTION_VECTOR_PELS as i32 * 4 + 2;

/// Half-pel offsets in quarter-pel units around the integer center `(ix*4, iy*4)`.
pub(crate) const HALFPEL_REFINE_OFFSETS_QPEL: [(i32, i32); 8] = [
    (-2, -2),
    (0, -2),
    (2, -2),
    (-2, 0),
    (2, 0),
    (-2, 2),
    (0, 2),
    (2, 2),
];

/// Bilinear sample at luma integer pixel `(lx, ly)` with displacement `(mvx_q, mvy_q)` in **quarter-pels**.
///
/// Rounding: `(weighted_sum + 8) >> 4` (nearest on 1/16 grid). Edge samples use [`sample_u8_plane`]
/// clamp semantics (neutral **128** outside the plane).
#[inline]
pub fn sample_luma_bilinear_qpel(
    plane: &VideoPlane<u8>,
    lx: i32,
    ly: i32,
    mvx_q: i32,
    mvy_q: i32,
) -> u8 {
    let x_q = lx * 4 + mvx_q;
    let y_q = ly * 4 + mvy_q;
    let x0 = x_q.div_euclid(4);
    let y0 = y_q.div_euclid(4);
    let fx = x_q.rem_euclid(4);
    let fy = y_q.rem_euclid(4);
    if fx == 0 && fy == 0 {
        return sample_u8_plane(plane, x0, y0);
    }
    let p00 = sample_u8_plane(plane, x0, y0) as i32;
    let p10 = sample_u8_plane(plane, x0 + 1, y0) as i32;
    let p01 = sample_u8_plane(plane, x0, y0 + 1) as i32;
    let p11 = sample_u8_plane(plane, x0 + 1, y0 + 1) as i32;
    let wx0 = 4 - fx;
    let wx1 = fx;
    let wy0 = 4 - fy;
    let wy1 = fy;
    let sum = p00 * wx0 * wy0 + p10 * wx1 * wy0 + p01 * wx0 * wy1 + p11 * wx1 * wy1;
    ((sum + 8) >> 4).clamp(0, 255) as u8
}

#[inline]
pub fn sad_16x16_qpel(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mvx_q: i32,
    mvy_q: i32,
) -> u32 {
    let mut acc = 0_u32;
    for row in 0..16 {
        for col in 0..16 {
            let lx = mb_x * 16 + col;
            let ly = mb_y * 16 + row;
            let cx = cur.samples[ly as usize * cur.stride + lx as usize];
            let pred = sample_luma_bilinear_qpel(refp, lx as i32, ly as i32, mvx_q, mvy_q);
            acc += cx.abs_diff(pred) as u32;
        }
    }
    acc
}

/// After integer-pel `pick_mv`, optionally refine to the best half-pel among fixed offsets.
///
/// `radius == 0` skips refinement (returns `4*ix`, `4*iy`). `radius >= 1` evaluates the eight half-pel
/// neighbours of the integer candidate.
#[allow(clippy::too_many_arguments)]
pub fn refine_half_pel_center(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    ix: i16,
    iy: i16,
    radius: u8,
    evals_out: &mut u64,
    tested_out: &mut u64,
) -> (i32, i32) {
    let base_x = ix as i32 * 4;
    let base_y = iy as i32 * 4;
    if radius == 0 {
        return (base_x, base_y);
    }
    let mut best_qx = base_x;
    let mut best_qy = base_y;
    *evals_out += 1;
    let mut best_sad = sad_16x16_qpel(cur, refp, mb_x, mb_y, base_x, base_y);
    *tested_out += 1;
    for &(ox, oy) in &HALFPEL_REFINE_OFFSETS_QPEL {
        let mvx_q = base_x + ox;
        let mvy_q = base_y + oy;
        if mvx_q.abs() > MAX_MV_QPEL_ABS || mvy_q.abs() > MAX_MV_QPEL_ABS {
            continue;
        }
        *evals_out += 1;
        *tested_out += 1;
        let s = sad_16x16_qpel(cur, refp, mb_x, mb_y, mvx_q, mvy_q);
        if s < best_sad {
            best_sad = s;
            best_qx = mvx_q;
            best_qy = mvy_q;
        }
    }
    (best_qx, best_qy)
}

/// Quarter-pel vectors must stay within [`MAX_MV_QPEL_ABS`] and be **even** (half-pel on the ¼ grid).
pub fn validate_mv_qpel_halfgrid(mvx_q: i32, mvy_q: i32) -> Result<(), super::error::SrsV2Error> {
    use super::error::SrsV2Error;
    if mvx_q.abs() > MAX_MV_QPEL_ABS || mvy_q.abs() > MAX_MV_QPEL_ABS {
        return Err(SrsV2Error::CorruptedMotionVector);
    }
    if (mvx_q & 1) != 0 || (mvy_q & 1) != 0 {
        return Err(SrsV2Error::syntax("fractional MV not on half-pel grid"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::frame::VideoPlane;
    use crate::srsv2::motion_search::sad_16x16;

    fn plane_3x3() -> VideoPlane<u8> {
        let mut p = VideoPlane::<u8>::try_new(3, 3, 3).unwrap();
        // Interior test uses (1,1) neighbourhood
        p.samples.copy_from_slice(&[
            10, 20, 30, //
            40, 50, 60, //
            70, 80, 90,
        ]);
        p
    }

    #[test]
    fn bilinear_integer_mv_matches_integer_sample() {
        let p = plane_3x3();
        assert_eq!(sample_luma_bilinear_qpel(&p, 1, 1, 0, 0), p.samples[4]);
    }

    #[test]
    fn bilinear_horizontal_half_pel_averages_neighbors() {
        let p = plane_3x3();
        let got = sample_luma_bilinear_qpel(&p, 1, 1, 2, 0);
        // Between x=1 and x=2 at same y: (50+60)/2 = 55
        assert_eq!(got, 55);
    }

    #[test]
    fn bilinear_vertical_half_pel_averages_neighbors() {
        let p = plane_3x3();
        let got = sample_luma_bilinear_qpel(&p, 1, 1, 0, 2);
        // Between y=1 and y=2: (50+80)/2 = 65
        assert_eq!(got, 65);
    }

    #[test]
    fn bilinear_diagonal_half_pel() {
        let p = plane_3x3();
        let got = sample_luma_bilinear_qpel(&p, 1, 1, 2, 2);
        // fx=fy=2 → equal weights on four neighbors
        let sum = (50 + 60 + 80 + 90) * 4;
        assert_eq!(got, ((sum + 8) >> 4) as u8);
        assert_eq!(got, 70);
    }

    #[test]
    fn bilinear_negative_corner_uses_oob_padding_without_panic() {
        let p = plane_3x3();
        let v = sample_luma_bilinear_qpel(&p, 0, 0, -12, -12);
        let v2 = sample_luma_bilinear_qpel(&p, 0, 0, -12, -12);
        assert_eq!(v, v2);
    }

    #[test]
    fn bilinear_deterministic_repeatable() {
        let p = plane_3x3();
        let a = sample_luma_bilinear_qpel(&p, 1, 1, -2, 2);
        let b = sample_luma_bilinear_qpel(&p, 1, 1, -2, 2);
        assert_eq!(a, b);
    }

    #[test]
    fn validate_rejects_odd_qpel() {
        assert!(validate_mv_qpel_halfgrid(1, 0).is_err());
        assert!(validate_mv_qpel_halfgrid(0, 3).is_err());
        assert!(validate_mv_qpel_halfgrid(2, -4).is_ok());
    }

    /// Synthetic current MB equals reference sampled at half-pel left; qpel `(-2,0)` should beat integer `(0,0)`.
    #[test]
    fn half_pel_mv_can_lower_sad_vs_integer_for_half_shifted_ramp() {
        let w = 32u32;
        let mut refp = VideoPlane::<u8>::try_new(w, w, w as usize).unwrap();
        for y in 0..w {
            for x in 0..w {
                refp.samples[y as usize * refp.stride + x as usize] =
                    ((x * 13 + y * 3) % 251) as u8;
            }
        }
        let mut cur = VideoPlane::<u8>::try_new(w, w, w as usize).unwrap();
        for y in 0..16 {
            for x in 0..16 {
                let v = sample_luma_bilinear_qpel(&refp, x, y, -2, 0);
                cur.samples[y as usize * cur.stride + x as usize] = v;
            }
        }
        let s_int = sad_16x16(&cur, &refp, 0, 0, 0, 0);
        let s_hp = sad_16x16_qpel(&cur, &refp, 0, 0, -2, 0);
        assert!(
            s_hp < s_int,
            "expected half-pel mv (-2,0) qpel to improve SAD vs integer (0,0); int={s_int} hp={s_hp}"
        );
    }
}
