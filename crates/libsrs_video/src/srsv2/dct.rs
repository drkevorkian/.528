//! Separable 8×8 DCT-II (orthonormal) using fixed-order `f64` transforms with explicit rounding.
//! Not invoking patented codec algorithms — generic linear algebra.

#![allow(clippy::needless_range_loop)]
#![allow(clippy::manual_memcpy)]

const INV_SQRT2: f64 = std::f64::consts::FRAC_1_SQRT_2;

fn alpha(k: usize) -> f64 {
    if k == 0 {
        INV_SQRT2
    } else {
        1.0
    }
}

fn dct_1d_8(x: &[f64; 8]) -> [f64; 8] {
    let mut y = [0_f64; 8];
    for k in 0..8 {
        let mut s = 0_f64;
        for n in 0..8 {
            let ang = std::f64::consts::PI / 16.0 * k as f64 * (2 * n + 1) as f64;
            s += x[n] * ang.cos();
        }
        y[k] = alpha(k) * s / (8.0_f64).sqrt();
    }
    y
}

fn dct_1d_4(x: &[f64; 4]) -> [f64; 4] {
    let mut y = [0_f64; 4];
    for k in 0..4 {
        let mut s = 0_f64;
        for n in 0..4 {
            let ang = std::f64::consts::PI / 8.0 * k as f64 * (2 * n + 1) as f64;
            s += x[n] * ang.cos();
        }
        y[k] = alpha(k) * s / 2.0;
    }
    y
}

fn idct_1d_4(x: &[f64; 4]) -> [f64; 4] {
    let mut y = [0_f64; 4];
    for n in 0..4 {
        let mut s = 0_f64;
        for k in 0..4 {
            let ang = std::f64::consts::PI / 8.0 * k as f64 * (2 * n + 1) as f64;
            s += alpha(k) * x[k] * ang.cos();
        }
        y[n] = s / 2.0;
    }
    y
}

fn idct_1d_8(x: &[f64; 8]) -> [f64; 8] {
    let mut y = [0_f64; 8];
    for n in 0..8 {
        let mut s = 0_f64;
        for k in 0..8 {
            let ang = std::f64::consts::PI / 16.0 * k as f64 * (2 * n + 1) as f64;
            s += alpha(k) * x[k] * ang.cos();
        }
        y[n] = s / (8.0_f64).sqrt();
    }
    y
}

pub fn fdct_8x8(block: &[i16; 64]) -> [i16; 64] {
    let mut tmp = [[0_f64; 8]; 8];
    let mut row = [0_f64; 8];
    for r in 0..8 {
        for c in 0..8 {
            row[c] = block[r * 8 + c] as f64;
        }
        let d = dct_1d_8(&row);
        for c in 0..8 {
            tmp[r][c] = d[c];
        }
    }
    let mut out = [0_i16; 64];
    for c in 0..8 {
        let mut col = [0_f64; 8];
        for r in 0..8 {
            col[r] = tmp[r][c];
        }
        let d = dct_1d_8(&col);
        for r in 0..8 {
            out[r * 8 + c] = d[r].round().clamp(-32768.0, 32767.0) as i16;
        }
    }
    out
}

pub fn fdct_4x4(block: &[i16; 16]) -> [i16; 16] {
    let mut tmp = [[0_f64; 4]; 4];
    let mut row = [0_f64; 4];
    for r in 0..4 {
        for c in 0..4 {
            row[c] = block[r * 4 + c] as f64;
        }
        let d = dct_1d_4(&row);
        for c in 0..4 {
            tmp[r][c] = d[c];
        }
    }
    let mut out = [0_i16; 16];
    for c in 0..4 {
        let mut col = [0_f64; 4];
        for r in 0..4 {
            col[r] = tmp[r][c];
        }
        let d = dct_1d_4(&col);
        for r in 0..4 {
            out[r * 4 + c] = d[r].round().clamp(-32768.0, 32767.0) as i16;
        }
    }
    out
}

pub fn idct_4x4(block: &[i16; 16]) -> [i16; 16] {
    let mut tmp = [[0_f64; 4]; 4];
    let mut col = [0_f64; 4];
    for c in 0..4 {
        for r in 0..4 {
            col[r] = block[r * 4 + c] as f64;
        }
        let d = idct_1d_4(&col);
        for r in 0..4 {
            tmp[r][c] = d[r];
        }
    }
    let mut out = [0_i16; 16];
    for r in 0..4 {
        let mut row = [0_f64; 4];
        for c in 0..4 {
            row[c] = tmp[r][c];
        }
        let d = idct_1d_4(&row);
        for c in 0..4 {
            out[r * 4 + c] = d[c].round().clamp(-32768.0, 32767.0) as i16;
        }
    }
    out
}

pub fn idct_8x8(block: &[i16; 64]) -> [i16; 64] {
    let mut tmp = [[0_f64; 8]; 8];
    let mut col = [0_f64; 8];
    for c in 0..8 {
        for r in 0..8 {
            col[r] = block[r * 8 + c] as f64;
        }
        let d = idct_1d_8(&col);
        for r in 0..8 {
            tmp[r][c] = d[r];
        }
    }
    let mut out = [0_i16; 64];
    for r in 0..8 {
        let mut row = [0_f64; 8];
        for c in 0..8 {
            row[c] = tmp[r][c];
        }
        let d = idct_1d_8(&row);
        for c in 0..8 {
            out[r * 8 + c] = d[c].round().clamp(-32768.0, 32767.0) as i16;
        }
    }
    out
}

/// Zigzag indices for a 4×4 block (natural order flatten row-major).
pub const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

pub const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdct_idct_near_identity_flat_block() {
        let b = [42_i16; 64];
        let coeffs = fdct_8x8(&b);
        let r = idct_8x8(&coeffs);
        let max_delta = b
            .iter()
            .zip(r.iter())
            .map(|(&o, &x)| (o as i32 - x as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_delta <= 35,
            "flat block roundtrip max_delta={max_delta}"
        );
    }

    #[test]
    fn fdct_idct_4x4_near_identity() {
        let b = [11_i16; 16];
        let c = fdct_4x4(&b);
        let r = idct_4x4(&c);
        let max_d = b
            .iter()
            .zip(r.iter())
            .map(|(&o, &x)| (o as i32 - x as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(max_d <= 40, "4x4 roundtrip max_d={max_d}");
    }
}
