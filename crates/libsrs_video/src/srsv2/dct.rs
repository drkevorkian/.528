//! Separable 8×8 DCT-II (orthonormal) using fixed-order `f64` transforms with explicit rounding.
//! Not invoking patented codec algorithms — generic linear algebra.

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
}
