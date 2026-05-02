//! Integer-pel motion estimation modes for experimental P-frames.

use super::frame::VideoPlane;
use super::limits::MAX_MOTION_VECTOR_PELS;
use super::rate_control::SrsV2MotionSearchMode;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SrsV2MotionEncodeStats {
    pub motion_search_mode: SrsV2MotionSearchMode,
    pub sad_evaluations: u64,
    pub skip_subblocks: u64,
    pub nonzero_motion_macroblocks: u32,
    pub sum_mv_l1: u64,
    pub subpel_enabled: bool,
    pub subpel_blocks_tested: u64,
    pub subpel_blocks_selected: u64,
    pub additional_subpel_evaluations: u64,
    /// Sum of `|mvx_q.rem_euclid(4)| + |mvy_q.rem_euclid(4)|` over macroblocks (for averages).
    pub sum_abs_frac_qpel: u64,
}

pub(crate) fn sample_u8_plane(plane: &VideoPlane<u8>, x: i32, y: i32) -> u8 {
    let w = plane.width as i32;
    let h = plane.height as i32;
    if x < 0 || y < 0 || x >= w || y >= h {
        return 128;
    }
    plane.samples[y as usize * plane.stride + x as usize]
}

pub(crate) fn sad_16x16(
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

fn eval_and_track(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    mvx: i32,
    mvy: i32,
    evals: &mut u64,
) -> u32 {
    *evals += 1;
    sad_16x16(cur, refp, mb_x, mb_y, mvx, mvy)
}

/// Choose MV within `radius` using `mode`. `early_exit_sad_threshold` 0 = disabled.
#[allow(clippy::too_many_arguments)]
pub fn pick_mv(
    mode: SrsV2MotionSearchMode,
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    radius: i16,
    early_exit_sad_threshold: u32,
    stats_eval_only: Option<&mut u64>,
) -> (i16, i16) {
    let mut scratch = 0_u64;
    let evals: &mut u64 = match stats_eval_only {
        Some(r) => r,
        None => &mut scratch,
    };
    let r = radius as i32;
    let r = r.clamp(0, MAX_MOTION_VECTOR_PELS as i32);

    match mode {
        SrsV2MotionSearchMode::None => {
            *evals += 1;
            sad_16x16(cur, refp, mb_x, mb_y, 0, 0);
            (0, 0)
        }
        SrsV2MotionSearchMode::ExhaustiveSmall => {
            exhaustive(cur, refp, mb_x, mb_y, r, early_exit_sad_threshold, evals)
        }
        SrsV2MotionSearchMode::Diamond => {
            diamond_search(cur, refp, mb_x, mb_y, r, early_exit_sad_threshold, evals)
        }
        SrsV2MotionSearchMode::Hex => {
            hex_search(cur, refp, mb_x, mb_y, r, early_exit_sad_threshold, evals)
        }
        SrsV2MotionSearchMode::Hierarchical => {
            hierarchical_search(cur, refp, mb_x, mb_y, r, early_exit_sad_threshold, evals)
        }
    }
}

fn exhaustive(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    r: i32,
    early: u32,
    evals: &mut u64,
) -> (i16, i16) {
    let mut best_mvx = 0_i16;
    let mut best_mvy = 0_i16;
    let mut best_sad = u32::MAX;
    for mvx in -r..=r {
        for mvy in -r..=r {
            let s = eval_and_track(cur, refp, mb_x, mb_y, mvx, mvy, evals);
            if s < best_sad {
                best_sad = s;
                best_mvx = mvx as i16;
                best_mvy = mvy as i16;
            }
            if early > 0 && best_sad <= early {
                return (best_mvx, best_mvy);
            }
        }
    }
    (best_mvx, best_mvy)
}

fn diamond_search(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    r: i32,
    early: u32,
    evals: &mut u64,
) -> (i16, i16) {
    let mut cx = 0_i32;
    let mut cy = 0_i32;
    let mut best_sad = eval_and_track(cur, refp, mb_x, mb_y, cx, cy, evals);
    if early > 0 && best_sad <= early {
        return (0, 0);
    }
    let dirs = [(1_i32, 0_i32), (-1, 0), (0, 1), (0, -1)];
    let mut improved = true;
    while improved {
        improved = false;
        for &(dx, dy) in &dirs {
            let nx = cx + dx;
            let ny = cy + dy;
            if nx.abs() > r || ny.abs() > r {
                continue;
            }
            let s = eval_and_track(cur, refp, mb_x, mb_y, nx, ny, evals);
            if s < best_sad {
                best_sad = s;
                cx = nx;
                cy = ny;
                improved = true;
                if early > 0 && best_sad <= early {
                    return (cx as i16, cy as i16);
                }
            }
        }
    }
    (cx as i16, cy as i16)
}

fn hex_search(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    r: i32,
    early: u32,
    evals: &mut u64,
) -> (i16, i16) {
    let dirs = [
        (1_i32, 0_i32),
        (1, -1),
        (0, -1),
        (-1, -1),
        (-1, 0),
        (-1, 1),
        (0, 1),
        (1, 1),
    ];
    let mut cx = 0_i32;
    let mut cy = 0_i32;
    let mut best_sad = eval_and_track(cur, refp, mb_x, mb_y, cx, cy, evals);
    if early > 0 && best_sad <= early {
        return (0, 0);
    }
    let mut improved = true;
    while improved {
        improved = false;
        for &(dx, dy) in &dirs {
            let nx = cx + dx;
            let ny = cy + dy;
            if nx.abs() > r || ny.abs() > r {
                continue;
            }
            let s = eval_and_track(cur, refp, mb_x, mb_y, nx, ny, evals);
            if s < best_sad {
                best_sad = s;
                cx = nx;
                cy = ny;
                improved = true;
                if early > 0 && best_sad <= early {
                    return (cx as i16, cy as i16);
                }
            }
        }
    }
    (cx as i16, cy as i16)
}

fn hierarchical_search(
    cur: &VideoPlane<u8>,
    refp: &VideoPlane<u8>,
    mb_x: u32,
    mb_y: u32,
    r: i32,
    early: u32,
    evals: &mut u64,
) -> (i16, i16) {
    if r <= 2 {
        return exhaustive(cur, refp, mb_x, mb_y, r, early, evals);
    }
    let coarse_r = (r / 2).max(1);
    let step = 2_i32;
    let mut best_mvx = 0_i16;
    let mut best_mvy = 0_i16;
    let mut best_sad = u32::MAX;
    let mut mvx = -coarse_r;
    while mvx <= coarse_r {
        let mut mvy = -coarse_r;
        while mvy <= coarse_r {
            let s = eval_and_track(cur, refp, mb_x, mb_y, mvx * step, mvy * step, evals);
            if s < best_sad {
                best_sad = s;
                best_mvx = (mvx * step) as i16;
                best_mvy = (mvy * step) as i16;
            }
            if early > 0 && best_sad <= early {
                return (best_mvx, best_mvy);
            }
            mvy += 1;
        }
        mvx += 1;
    }
    let cx = best_mvx as i32;
    let cy = best_mvy as i32;
    let refine_r = 2.min(r);
    let mut best2_sad = u32::MAX;
    let mut best2_mv = (best_mvx, best_mvy);
    for ox in -refine_r..=refine_r {
        for oy in -refine_r..=refine_r {
            let nx = cx + ox;
            let ny = cy + oy;
            if nx.abs() > r || ny.abs() > r {
                continue;
            }
            let s = eval_and_track(cur, refp, mb_x, mb_y, nx, ny, evals);
            if s < best2_sad {
                best2_sad = s;
                best2_mv = (nx as i16, ny as i16);
            }
            if early > 0 && best2_sad <= early {
                return best2_mv;
            }
        }
    }
    best2_mv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::color::rgb888_full_to_yuv420_bt709;
    use crate::srsv2::model::{ColorRange, PixelFormat};

    fn square_seq(
        w: u32,
        h: u32,
    ) -> (
        crate::srsv2::model::VideoSequenceHeaderV2,
        crate::srsv2::frame::YuvFrame,
        crate::srsv2::frame::YuvFrame,
    ) {
        use crate::srsv2::model::{
            ChromaSiting, ColorPrimaries, MatrixCoefficients, SrsVideoProfile, TransferFunction,
        };
        let seq = crate::srsv2::model::VideoSequenceHeaderV2 {
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
        };
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
        (seq, y0, y1)
    }

    #[test]
    fn diamond_matches_exhaustive_small_radius() {
        let (_, y0, y1) = square_seq(64, 64);
        let cur = &y1.y;
        let refp = &y0.y;
        let mut e1 = 0_u64;
        let mut e2 = 0_u64;
        let (dx1, dy1) = pick_mv(
            SrsV2MotionSearchMode::ExhaustiveSmall,
            cur,
            refp,
            2,
            2,
            3,
            0,
            Some(&mut e1),
        );
        let (dx2, dy2) = pick_mv(
            SrsV2MotionSearchMode::Diamond,
            cur,
            refp,
            2,
            2,
            3,
            0,
            Some(&mut e2),
        );
        let s1 = sad_16x16(cur, refp, 2, 2, dx1 as i32, dy1 as i32);
        let s2 = sad_16x16(cur, refp, 2, 2, dx2 as i32, dy2 as i32);
        assert_eq!(s1, s2, "same minimum SAD at mb (2,2)");
    }

    #[test]
    fn exhaustive_deterministic() {
        let (_seq, _y0, y1) = square_seq(64, 64);
        let cur = &y1.y;
        let refp = &y1.y;
        let mut a = 0_u64;
        let mut b = 0_u64;
        let p1 = pick_mv(
            SrsV2MotionSearchMode::ExhaustiveSmall,
            cur,
            refp,
            0,
            0,
            4,
            0,
            Some(&mut a),
        );
        let p2 = pick_mv(
            SrsV2MotionSearchMode::ExhaustiveSmall,
            cur,
            refp,
            0,
            0,
            4,
            0,
            Some(&mut b),
        );
        assert_eq!(p1, p2);
        assert_eq!(a, b);
    }
}
