//! CPU color conversion — BT.709-first, full/limited hooks.

use super::error::SrsV2Error;
use super::frame::{VideoPlane, YuvFrame};
use super::model::{ColorRange, PixelFormat};

fn clamp_u8(x: f64) -> u8 {
    x.round().clamp(0.0, 255.0) as u8
}

fn rgb_to_ycbcr_bt709(r: f64, g: f64, b: f64, range: ColorRange) -> (u8, u8, u8) {
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556 + 128.0;
    let cr = (r - y) / 1.5748 + 128.0;
    match range {
        ColorRange::Full => (clamp_u8(y), clamp_u8(cb), clamp_u8(cr)),
        ColorRange::Limited => {
            let yn = y * (219.0 / 255.0) + 16.0;
            let cbn = (cb - 128.0) * (224.0 / 255.0) + 128.0;
            let crn = (cr - 128.0) * (224.0 / 255.0) + 128.0;
            (clamp_u8(yn), clamp_u8(cbn), clamp_u8(crn))
        }
    }
}

/// BT.709 — pack RGB888 row-major into YUV420p8 with **simple 2×2 chroma averaging**.
pub fn rgb888_full_to_yuv420_bt709(
    rgb: &[u8],
    width: u32,
    height: u32,
    range: ColorRange,
) -> Result<YuvFrame, SrsV2Error> {
    let w = width as usize;
    let h = height as usize;
    let expected = w
        .checked_mul(h)
        .and_then(|x| x.checked_mul(3))
        .ok_or(SrsV2Error::Overflow("rgb buffer"))?;
    if rgb.len() < expected {
        return Err(SrsV2Error::syntax("rgb buffer too small"));
    }

    let cw = (width + 1) / 2;
    let ch = (height + 1) / 2;
    let mut ypl = VideoPlane::<u8>::try_new(width, height, w)?;
    let mut u = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    for row in 0..h {
        for col in 0..w {
            let i = (row * w + col) * 3;
            let r = rgb[i] as f64;
            let g = rgb[i + 1] as f64;
            let b = rgb[i + 2] as f64;
            let (yc, _, _) = rgb_to_ycbcr_bt709(r, g, b, range);
            ypl.samples[row * ypl.stride + col] = yc;
        }
    }
    for cy in 0..ch as usize {
        for cx in 0..cw as usize {
            let mut sum_cb = 0_f64;
            let mut sum_cr = 0_f64;
            let mut n = 0_f64;
            for dy in 0..2 {
                for dx in 0..2 {
                    let x = cx * 2 + dx;
                    let yrow = cy * 2 + dy;
                    if x < w && yrow < h {
                        let i = (yrow * w + x) * 3;
                        let r = rgb[i] as f64;
                        let g = rgb[i + 1] as f64;
                        let b = rgb[i + 2] as f64;
                        let (_, cb, cr) = rgb_to_ycbcr_bt709(r, g, b, range);
                        sum_cb += cb as f64;
                        sum_cr += cr as f64;
                        n += 1.0;
                    }
                }
            }
            let ui = cy * u.stride + cx;
            u.samples[ui] = clamp_u8(sum_cb / n.max(1.0));
            v.samples[ui] = clamp_u8(sum_cr / n.max(1.0));
        }
    }

    Ok(YuvFrame {
        format: PixelFormat::Yuv420p8,
        y: ypl,
        u,
        v,
    })
}

/// BT.709 limited-range RGB888 preview from YUV420 (nearest chroma).
pub fn yuv420_bt709_to_rgb888_limited(yuv: &YuvFrame) -> Result<Vec<u8>, SrsV2Error> {
    if yuv.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("preview only YUV420p8"));
    }
    let w = yuv.y.width as usize;
    let h = yuv.y.height as usize;
    let mut out = vec![0_u8; w.saturating_mul(h).saturating_mul(3)];
    for row in 0..h {
        for col in 0..w {
            let cy = yuv.y.samples[row * yuv.y.stride + col] as f64;
            let cx = col / 2;
            let cyy = row / 2;
            let ci = cyy * yuv.u.stride + cx;
            let cb = yuv.u.samples[ci] as f64 - 128.0;
            let cr = yuv.v.samples[ci] as f64 - 128.0;
            let r = cy + 1.5748 * cr;
            let g = cy - 0.1873 * cb - 0.4681 * cr;
            let b = cy + 1.8556 * cb;
            let i = (row * w + col) * 3;
            out[i] = clamp_u8(r);
            out[i + 1] = clamp_u8(g);
            out[i + 2] = clamp_u8(b);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::model::MatrixCoefficients;
    use super::*;

    #[test]
    fn black_white_rgb_roundtrip_limited_close() {
        let w = 16u32;
        let h = 16u32;
        let mut rgb = vec![0_u8; (w * h * 3) as usize];
        for px in rgb.chunks_exact_mut(3) {
            px[0] = 255;
            px[1] = 255;
            px[2] = 255;
        }
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, w, h, ColorRange::Limited).unwrap();
        let back = yuv420_bt709_to_rgb888_limited(&yuv).unwrap();
        // Limited-range YUV pegs white primary channels around ~235 after round-trip.
        assert!(back.iter().step_by(3).take(4).all(|&r| r > 220));

        let black = vec![0_u8; (w * h * 3) as usize];
        let yuv_b = rgb888_full_to_yuv420_bt709(&black, w, h, ColorRange::Limited).unwrap();
        let bb = yuv420_bt709_to_rgb888_limited(&yuv_b).unwrap();
        assert!(bb.iter().take(12).all(|&x| x < 40));
        let _ = MatrixCoefficients::Bt709;
    }

    #[test]
    fn primaries_red_green_blue_patch() {
        let w = 8u32;
        let h = 8u32;
        for (name, r, g, b) in [
            ("r", 255u8, 0u8, 0u8),
            ("g", 0u8, 255u8, 0u8),
            ("b", 0u8, 0u8, 255u8),
        ] {
            let mut rgb = vec![0_u8; (w * h * 3) as usize];
            for px in rgb.chunks_exact_mut(3) {
                px[0] = r;
                px[1] = g;
                px[2] = b;
            }
            let yuv = rgb888_full_to_yuv420_bt709(&rgb, w, h, ColorRange::Full).unwrap();
            let back = yuv420_bt709_to_rgb888_limited(&yuv).unwrap();
            let mid = (h / 2 * w + w / 2) as usize * 3;
            match name {
                "r" => assert!(back[mid] > back[mid + 1] && back[mid] > back[mid + 2]),
                "g" => assert!(back[mid + 1] > back[mid] && back[mid + 1] > back[mid + 2]),
                "b" => assert!(back[mid + 2] > back[mid] && back[mid + 2] > back[mid + 1]),
                _ => {}
            }
        }
    }

    #[test]
    fn gray_ramp_monotonic() {
        let w = 256u32;
        let h = 1u32;
        let mut rgb = vec![0_u8; (w * h * 3) as usize];
        for x in 0..256usize {
            let v = x as u8;
            rgb[x * 3] = v;
            rgb[x * 3 + 1] = v;
            rgb[x * 3 + 2] = v;
        }
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, w, h, ColorRange::Full).unwrap();
        let mut prev = 0u8;
        for x in 0..256usize {
            let yy = yuv.y.samples[x];
            assert!(yy >= prev);
            prev = yy;
        }
    }
}
