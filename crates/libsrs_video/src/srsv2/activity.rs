//! Per-block luma activity metrics for adaptive quantization (deterministic, bounded work).

use super::frame::VideoPlane;

/// Activity metrics for one macroblock (16×16 luma).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockActivity {
    /// Sum of squared deviations from block mean (scaled down in aggregates).
    pub variance_sum: u64,
    /// Sum of absolute Sobel-like edge responses.
    pub edge_sum: u64,
    /// Mean absolute sample gradient (|dx|+|dy|) summed over interior pixels.
    pub mag_grad_sum: u64,
    /// High when variance is very low (flat regions).
    pub flatness: u32,
}

impl BlockActivity {
    pub const ZERO: Self = Self {
        variance_sum: 0,
        edge_sum: 0,
        mag_grad_sum: 0,
        flatness: 0,
    };
}

/// 16×16 luma MB at integer indices (must fit in plane).
pub fn mb_activity_y16(plane: &VideoPlane<u8>, mb_x: u32, mb_y: u32) -> BlockActivity {
    let w = plane.width as usize;
    let h = plane.height as usize;
    let base_x = mb_x as usize * 16;
    let base_y = mb_y as usize * 16;
    if base_x + 16 > w || base_y + 16 > h {
        return BlockActivity::ZERO;
    }

    let mut sum = 0_u64;
    for row in 0..16 {
        let row_off = (base_y + row) * plane.stride + base_x;
        for col in 0..16 {
            let v = plane.samples[row_off + col] as u64;
            sum += v;
        }
    }
    let n = 256_u64;
    let mean = sum / n;
    let mut var_acc = 0_u64;
    for row in 0..16 {
        let row_off = (base_y + row) * plane.stride + base_x;
        for col in 0..16 {
            let v = plane.samples[row_off + col] as i64;
            let d = v - mean as i64;
            var_acc += (d * d) as u64;
        }
    }

    let mut edge_acc = 0_u64;
    let mut grad_acc = 0_u64;
    for row in 1..15 {
        for col in 1..15 {
            let y = base_y + row;
            let x = base_x + col;
            let idx = y * plane.stride + x;
            let lx = plane.samples[idx - 1] as i32;
            let rx = plane.samples[idx + 1] as i32;
            let ty = plane.samples[idx - plane.stride] as i32;
            let by = plane.samples[idx + plane.stride] as i32;
            let gx = rx - lx;
            let gy = by - ty;
            let g = gx.unsigned_abs() + gy.unsigned_abs();
            grad_acc += g as u64;
            edge_acc += u64::from(gx.unsigned_abs()) + u64::from(gy.unsigned_abs());
        }
    }

    let flatness = if var_acc < (16 * 256) { 1024 } else { 0 };

    BlockActivity {
        variance_sum: var_acc,
        edge_sum: edge_acc,
        mag_grad_sum: grad_acc,
        flatness,
    }
}

/// Horizontal vs vertical gradient emphasis for screen-like content.
pub fn screen_activity_score(act: &BlockActivity) -> u64 {
    act.edge_sum
        .saturating_mul(2)
        .saturating_add(act.mag_grad_sum / 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::frame::VideoPlane;

    fn plane_constant(v: u8, w: u32, h: u32) -> VideoPlane<u8> {
        let mut p = VideoPlane::try_new(w, h, w as usize).unwrap();
        p.samples.fill(v);
        p
    }

    #[test]
    fn flat_block_low_activity() {
        let p = plane_constant(128, 32, 32);
        let a = mb_activity_y16(&p, 0, 0);
        assert!(a.variance_sum < 1000, "flat variance {:?}", a);
        assert!(a.edge_sum < 500, "flat edge {:?}", a);
    }

    #[test]
    fn checker_high_variance() {
        let mut p = VideoPlane::try_new(32, 32, 32).unwrap();
        for y in 0..32usize {
            for x in 0..32usize {
                let v = if (x / 4 + y / 4) % 2 == 0 {
                    40_u8
                } else {
                    220_u8
                };
                p.samples[y * 32 + x] = v;
            }
        }
        let a = mb_activity_y16(&p, 0, 0);
        assert!(a.variance_sum > 100_000, "checker variance {:?}", a);
        assert!(a.edge_sum > 1000, "checker edges {:?}", a);
    }

    #[test]
    fn edge_strip_higher_than_flat() {
        // MB (0,0): uniform; MB (1,0): same row band but adds a bright vertical bar wholly inside x∈16..32.
        let mut p = plane_constant(100, 32, 32);
        for y in 4..28usize {
            for x in 20..24usize {
                p.samples[y * 32 + x] = 240;
            }
        }
        let flat = mb_activity_y16(&p, 0, 0);
        let edge_mb = mb_activity_y16(&p, 1, 0);
        assert!(
            edge_mb.edge_sum > flat.edge_sum,
            "edge {:?} vs flat {:?}",
            edge_mb,
            flat
        );
    }

    #[test]
    fn deterministic_same_input() {
        let p = plane_constant(99, 64, 64);
        let a = mb_activity_y16(&p, 1, 1);
        let b = mb_activity_y16(&p, 1, 1);
        assert_eq!(a, b);
    }

    #[test]
    fn tiny_frame_no_panic() {
        let p = plane_constant(50, 8, 8);
        let _ = mb_activity_y16(&p, 0, 0);
    }
}
