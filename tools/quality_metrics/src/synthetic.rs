//! Deterministic YUV420p8 synthetic clips for benchmarks and regression tests.
//!
//! Frames are concatenated as planar `Y` then `U` then `V` per frame.

use serde::{Deserialize, Serialize};
use std::io;

/// Hard cap for default generator output unless `allow_large=true`.
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyntheticPattern {
    Flat,
    GrayRamp,
    MovingSquare,
    ScrollingBars,
    Noise,
    Checker,
    SceneCut,
}

impl SyntheticPattern {
    pub fn parse_cli(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "flat" => Some(Self::Flat),
            "gray-ramp" | "gray_ramp" | "gradient" => Some(Self::GrayRamp),
            "moving-square" | "moving_square" => Some(Self::MovingSquare),
            "scrolling-bars" | "scrolling_bars" => Some(Self::ScrollingBars),
            "noise" => Some(Self::Noise),
            "checker" => Some(Self::Checker),
            "scene-cut" | "scene_cut" => Some(Self::SceneCut),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyntheticClipSpec {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub frames: u32,
    pub pattern: SyntheticPattern,
    pub seed: u64,
    pub allow_large: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntheticClipMetadata {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// Back-compat convenience field (integer FPS).
    #[serde(default)]
    pub fps: u32,
    pub frames: u32,
    pub pix_fmt: String,
    pub pattern: SyntheticPattern,
    pub seed: u64,
    pub yuv_bytes: u64,
    pub raw_size_bytes: u64,
}

/// Back-compat name used by older tools.
pub type SyntheticMeta = SyntheticClipMetadata;

/// Back-compat helper: byte length of one planar YUV420p8 frame.
///
/// Prefer [`yuv420p8_frame_bytes_even`] when validating user input.
pub fn yuv420p8_frame_bytes(width: u32, height: u32) -> usize {
    let w = width as usize;
    let h = height as usize;
    w.checked_mul(h)
        .and_then(|y| y.checked_add(y / 2))
        .unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub enum SyntheticError {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    #[error("invalid dimensions: {0}x{1}")]
    Dimensions(u32, u32),
    #[error("YUV420p8 requires even width/height, got {0}x{1}")]
    OddDimensions(u32, u32),
    #[error("clip would be too large ({bytes} bytes); pass --allow-large")]
    TooLarge { bytes: u64 },
    #[error("integer overflow computing clip size")]
    Overflow,
    #[error("serde json: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("unknown pattern: {0}")]
    UnknownPattern(String),
    #[error("--preset-corpus requires --out-dir")]
    PresetCorpusRequiresOutDir,
}

fn checked_mul_u64(a: u64, b: u64) -> Result<u64, SyntheticError> {
    a.checked_mul(b).ok_or(SyntheticError::Overflow)
}

/// Bytes per frame for planar YUV420p8, requiring **even** dimensions.
pub fn yuv420p8_frame_bytes_even(width: u32, height: u32) -> Result<u64, SyntheticError> {
    if width == 0 || height == 0 {
        return Err(SyntheticError::Dimensions(width, height));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(SyntheticError::OddDimensions(width, height));
    }
    let w = u64::from(width);
    let h = u64::from(height);
    // Y: w*h; U/V: (w/2)*(h/2) each.
    let y = checked_mul_u64(w, h)?;
    let c = checked_mul_u64(w / 2, h / 2)?;
    y.checked_add(c.checked_mul(2).ok_or(SyntheticError::Overflow)?)
        .ok_or(SyntheticError::Overflow)
}

/// XOR-shift PRNG (deterministic from `seed`).
fn xorshift64(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn fill_yuv420_frame(
    spec: &SyntheticClipSpec,
    frame_idx: u32,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let w = spec.width;
    let h = spec.height;
    let cw = w / 2;
    let ch = h / 2;
    debug_assert_eq!(y.len(), (w * h) as usize);
    debug_assert_eq!(u.len(), (cw * ch) as usize);
    debug_assert_eq!(v.len(), (cw * ch) as usize);

    match spec.pattern {
        SyntheticPattern::Flat => {
            y.fill(128);
            u.fill(128);
            v.fill(128);
        }
        SyntheticPattern::GrayRamp => {
            for yy in 0..h {
                for xx in 0..w {
                    let v0 = ((xx.wrapping_add(frame_idx.wrapping_mul(3))) & 0xFF) as u8;
                    y[(yy * w + xx) as usize] = v0;
                }
            }
            u.fill(128);
            v.fill(128);
        }
        SyntheticPattern::MovingSquare => {
            y.fill(16);
            let sq = 32u32;
            let ox = (frame_idx.wrapping_mul(8) % w.saturating_sub(sq).max(1)) as i32;
            let oy = (frame_idx.wrapping_mul(4) % h.saturating_sub(sq).max(1)) as i32;
            for yy in 0..sq {
                for xx in 0..sq {
                    let px = ox + xx as i32;
                    let py = oy + yy as i32;
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        y[(py as u32 * w + px as u32) as usize] = 220;
                    }
                }
            }
            u.fill(128);
            v.fill(128);
        }
        SyntheticPattern::ScrollingBars => {
            let shift = (frame_idx.wrapping_mul(7)) % h.max(1);
            for yy in 0..h {
                let band = ((yy + shift) % 64) < 32;
                let fill = if band { 200_u8 } else { 40_u8 };
                for xx in 0..w {
                    y[(yy * w + xx) as usize] = fill.wrapping_add((xx % 16) as u8);
                }
            }
            u.fill(128);
            v.fill(128);
        }
        SyntheticPattern::Noise => {
            let mut s = spec.seed ^ u64::from(frame_idx).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for b in y.iter_mut() {
                s = xorshift64(s);
                *b = (s & 0xFF) as u8;
            }
            for b in u.iter_mut().chain(v.iter_mut()) {
                s = xorshift64(s);
                *b = (s & 0xFF) as u8;
            }
        }
        SyntheticPattern::Checker => {
            for yy in 0..h {
                for xx in 0..w {
                    let c = if ((xx / 8) ^ (yy / 8) ^ frame_idx) & 1 == 0 {
                        240
                    } else {
                        20
                    };
                    y[(yy * w + xx) as usize] = c as u8;
                }
            }
            u.fill(128);
            v.fill(128);
        }
        SyntheticPattern::SceneCut => {
            let hi = frame_idx < (spec.frames / 2).max(1);
            y.fill(if hi { 210 } else { 45 });
            u.fill(128);
            v.fill(128);
        }
    }
}

/// Generate a planar YUV420p8 clip: frames concatenated as `Y` + `U` + `V` per frame.
pub fn generate_yuv420p8_clip(spec: &SyntheticClipSpec) -> Result<Vec<u8>, SyntheticError> {
    let frame_bytes = yuv420p8_frame_bytes_even(spec.width, spec.height)?;
    let total_bytes = checked_mul_u64(frame_bytes, u64::from(spec.frames))?;
    if !spec.allow_large && total_bytes > DEFAULT_MAX_OUTPUT_BYTES {
        return Err(SyntheticError::TooLarge { bytes: total_bytes });
    }

    let mut out = vec![0_u8; total_bytes as usize];
    let y_len = (spec.width * spec.height) as usize;
    let c_len = ((spec.width / 2) * (spec.height / 2)) as usize;
    let per = frame_bytes as usize;

    for fi in 0..spec.frames {
        let frame = &mut out[fi as usize * per..][..per];
        let (y, rest) = frame.split_at_mut(y_len);
        let (u, v) = rest.split_at_mut(c_len);
        fill_yuv420_frame(spec, fi, y, u, v);
    }
    Ok(out)
}

pub fn metadata_for_clip(
    spec: &SyntheticClipSpec,
) -> Result<SyntheticClipMetadata, SyntheticError> {
    let frame_bytes = yuv420p8_frame_bytes_even(spec.width, spec.height)?;
    let yuv_bytes = checked_mul_u64(frame_bytes, u64::from(spec.frames))?;
    Ok(SyntheticClipMetadata {
        width: spec.width,
        height: spec.height,
        fps_num: spec.fps_num.max(1),
        fps_den: spec.fps_den.max(1),
        fps: spec.fps_num.max(1) / spec.fps_den.max(1),
        frames: spec.frames,
        pix_fmt: "yuv420p".to_string(),
        pattern: spec.pattern,
        seed: spec.seed,
        yuv_bytes,
        raw_size_bytes: yuv_bytes,
    })
}

pub fn write_yuv420p8_clip(
    spec: &SyntheticClipSpec,
    yuv_path: &std::path::Path,
    metadata_path: &std::path::Path,
) -> Result<SyntheticClipMetadata, SyntheticError> {
    let yuv = generate_yuv420p8_clip(spec)?;
    if let Some(p) = yuv_path.parent() {
        std::fs::create_dir_all(p)?;
    }
    if let Some(p) = metadata_path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(yuv_path, &yuv)?;
    let meta = metadata_for_clip(spec)?;
    let json = serde_json::to_string_pretty(&meta)?;
    std::fs::write(metadata_path, json)?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_spec(pattern: SyntheticPattern) -> SyntheticClipSpec {
        SyntheticClipSpec {
            width: 64,
            height: 64,
            fps_num: 30,
            fps_den: 1,
            frames: 3,
            pattern,
            seed: 528,
            allow_large: false,
        }
    }

    #[test]
    fn deterministic_for_same_seed() {
        let spec = base_spec(SyntheticPattern::Noise);
        let a = generate_yuv420p8_clip(&spec).unwrap();
        let b = generate_yuv420p8_clip(&spec).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn all_patterns_have_expected_byte_length() {
        for p in [
            SyntheticPattern::Flat,
            SyntheticPattern::GrayRamp,
            SyntheticPattern::MovingSquare,
            SyntheticPattern::ScrollingBars,
            SyntheticPattern::Noise,
            SyntheticPattern::Checker,
            SyntheticPattern::SceneCut,
        ] {
            let spec = base_spec(p);
            let buf = generate_yuv420p8_clip(&spec).unwrap();
            let fb = yuv420p8_frame_bytes_even(spec.width, spec.height).unwrap() as usize;
            assert_eq!(buf.len(), fb * spec.frames as usize);
        }
    }

    #[test]
    fn odd_and_zero_dimensions_rejected() {
        let mut s = base_spec(SyntheticPattern::Flat);
        s.width = 0;
        assert!(generate_yuv420p8_clip(&s).is_err());
        s.width = 63;
        s.height = 64;
        assert!(matches!(
            generate_yuv420p8_clip(&s),
            Err(SyntheticError::OddDimensions(..))
        ));
    }

    #[test]
    fn large_output_blocked_unless_allowed() {
        let mut s = base_spec(SyntheticPattern::Flat);
        s.width = 4096;
        s.height = 4096;
        s.frames = 10;
        assert!(matches!(
            generate_yuv420p8_clip(&s),
            Err(SyntheticError::TooLarge { .. }) | Err(SyntheticError::Overflow)
        ));
        s.allow_large = true;
        let _ = generate_yuv420p8_clip(&s).unwrap();
    }

    #[test]
    fn metadata_serializes() {
        let s = base_spec(SyntheticPattern::Checker);
        let m = metadata_for_clip(&s).unwrap();
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"pix_fmt\""));
    }

    #[test]
    fn gradient_cli_alias_is_gray_ramp() {
        assert_eq!(
            SyntheticPattern::parse_cli("gradient"),
            Some(SyntheticPattern::GrayRamp)
        );
    }

    #[test]
    fn preset_tiny_flat_clip_deterministic_seed() {
        let dir = std::env::temp_dir().join("qm_preset_tiny_flat_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spec = SyntheticClipSpec {
            width: 64,
            height: 64,
            fps_num: 30,
            fps_den: 1,
            frames: 16,
            pattern: SyntheticPattern::Flat,
            seed: 528,
            allow_large: false,
        };
        let out = dir.join("tiny_flat_64x64.yuv");
        let meta = dir.join("tiny_flat_64x64.json");
        let m1 = write_yuv420p8_clip(&spec, &out, &meta).unwrap();
        let m2 = write_yuv420p8_clip(&spec, &out, &meta).unwrap();
        assert_eq!(m1.yuv_bytes, m2.yuv_bytes);
        let raw = std::fs::read(&out).unwrap();
        assert_eq!(raw.len() as u64, m1.yuv_bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
