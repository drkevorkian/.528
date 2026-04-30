//! Deterministic **planar YUV420p8** buffers for codec measurements (`Y` plane, then `U`, then `V`).

use serde::{Deserialize, Serialize};
use std::io::{self, Write};

/// Metadata written beside generated `.yuv` files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntheticMeta {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u32,
    pub pix_fmt: String,
    pub seed: u64,
    pub pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticPattern {
    Flat,
    GrayRamp,
    MovingSquare,
    ScrollingBars,
    Noise,
    Checker,
    SceneCut,
    Hd1080Short,
    Uhd4kShort,
    Uhd8kTiny,
}

impl SyntheticPattern {
    /// Parse CLI `--pattern` names (`flat`, `noise`, `1080p`, …).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "flat" => Some(Self::Flat),
            "gray_ramp" | "gray-ramp" => Some(Self::GrayRamp),
            "moving_square" | "moving-square" => Some(Self::MovingSquare),
            "scrolling_bars" | "scrolling-bars" => Some(Self::ScrollingBars),
            "noise" => Some(Self::Noise),
            "checker" => Some(Self::Checker),
            "scene_cut" | "scene-cut" => Some(Self::SceneCut),
            "1080p" | "hd1080" => Some(Self::Hd1080Short),
            "4k" | "uhd4k" => Some(Self::Uhd4kShort),
            "8k" | "uhd8k" => Some(Self::Uhd8kTiny),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum SyntheticError {
    Io(io::Error),
    SerdeJson(serde_json::Error),
    ResolutionNeedsAllowLarge {
        width: u32,
        height: u32,
    },
    /// Unknown `--pattern` name.
    UnknownPattern(String),
}

impl std::fmt::Display for SyntheticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::SerdeJson(e) => write!(f, "{e}"),
            Self::ResolutionNeedsAllowLarge { width, height } => write!(
                f,
                "resolution {width}x{height} requires --allow-large (hostile-input guard)"
            ),
            Self::UnknownPattern(s) => write!(f, "unknown pattern {s:?}"),
        }
    }
}

impl std::error::Error for SyntheticError {}

impl From<io::Error> for SyntheticError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct GenerateOptions {
    pub pattern: SyntheticPattern,
    pub seed: u64,
    pub frames: Option<u32>,
    pub fps: u32,
    pub allow_large: bool,
}

fn dims_for_pattern(pat: SyntheticPattern) -> (u32, u32, u32) {
    match pat {
        SyntheticPattern::Hd1080Short => (1920, 1080, 2),
        SyntheticPattern::Uhd4kShort => (3840, 2160, 1),
        SyntheticPattern::Uhd8kTiny => (7680, 4320, 1),
        _ => (64, 64, 10),
    }
}

/// Byte length of one YUV420p8 frame (planar) at `w`×`h`.
pub fn yuv420p8_frame_bytes(w: u32, h: u32) -> usize {
    pixel_count_yuv420(w, h)
}

fn pixel_count_yuv420(w: u32, h: u32) -> usize {
    let y = (w as usize) * (h as usize);
    let cu = w.div_ceil(2) as usize;
    let ch = h.div_ceil(2) as usize;
    y + 2 * cu * ch
}

/// XOR-shift PRNG (deterministic from `seed`).
fn xorshift64(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn fill_frame(pat: SyntheticPattern, seed: u64, frame_idx: u32, w: u32, h: u32, buf: &mut [u8]) {
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let y_len = (w * h) as usize;
    let u_len = (cw * ch) as usize;
    let (yplane, rest) = buf.split_at_mut(y_len);
    let (uplane, vplane) = rest.split_at_mut(u_len);

    match pat {
        SyntheticPattern::Flat => {
            yplane.fill(128);
            uplane.fill(128);
            vplane.fill(128);
        }
        SyntheticPattern::GrayRamp => {
            for y in 0..h {
                for x in 0..w {
                    let v = ((x.wrapping_add(frame_idx.wrapping_mul(3))) % 256) as u8;
                    yplane[(y * w + x) as usize] = v;
                }
            }
            uplane.fill(128);
            vplane.fill(128);
        }
        SyntheticPattern::MovingSquare => {
            yplane.fill(16);
            let sq = 16u32;
            let ox = (frame_idx.wrapping_mul(4) % w.saturating_sub(sq).max(1)) as i32;
            let oy = (frame_idx.wrapping_mul(2) % h.saturating_sub(sq).max(1)) as i32;
            for yy in 0..sq {
                for xx in 0..sq {
                    let px = ox + xx as i32;
                    let py = oy + yy as i32;
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        yplane[(py as u32 * w + px as u32) as usize] = 220;
                    }
                }
            }
            uplane.fill(128);
            vplane.fill(128);
        }
        SyntheticPattern::ScrollingBars => {
            let shift = (frame_idx.wrapping_mul(7)) % h.max(1);
            for y in 0..h {
                let band = ((y + shift) % 32) < 16;
                let fill = if band { 200_u8 } else { 40_u8 };
                for x in 0..w {
                    yplane[(y * w + x) as usize] = fill.wrapping_add((x % 16) as u8);
                }
            }
            uplane.fill(128);
            vplane.fill(128);
        }
        SyntheticPattern::Noise => {
            let mut s = seed ^ u64::from(frame_idx).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for b in yplane.iter_mut() {
                s = xorshift64(s);
                *b = (s & 0xFF) as u8;
            }
            for b in uplane.iter_mut().chain(vplane.iter_mut()) {
                s = xorshift64(s);
                *b = (s & 0xFF) as u8;
            }
        }
        SyntheticPattern::Checker => {
            for y in 0..h {
                for x in 0..w {
                    let c = if ((x / 8) ^ (y / 8) ^ frame_idx) & 1 == 0 {
                        240
                    } else {
                        20
                    };
                    yplane[(y * w + x) as usize] = c as u8;
                }
            }
            uplane.fill(128);
            vplane.fill(128);
        }
        SyntheticPattern::SceneCut => {
            let half = |buf: &mut [u8], hi: bool| {
                let v = if hi { 210_u8 } else { 45_u8 };
                buf.fill(v);
            };
            if frame_idx < 5 {
                half(yplane, true);
            } else {
                half(yplane, false);
            }
            uplane.fill(128);
            vplane.fill(128);
        }
        SyntheticPattern::Hd1080Short
        | SyntheticPattern::Uhd4kShort
        | SyntheticPattern::Uhd8kTiny => {
            fill_frame(SyntheticPattern::Noise, seed, frame_idx, w, h, buf);
        }
    }
}

/// Generate planar YUV420p8 bytes for all frames (concatenated).
pub fn generate(opts: &GenerateOptions) -> Result<(Vec<u8>, SyntheticMeta), SyntheticError> {
    let pat = opts.pattern;
    let (mut w, mut h, default_frames) = dims_for_pattern(pat);
    if !matches!(
        pat,
        SyntheticPattern::Hd1080Short | SyntheticPattern::Uhd4kShort | SyntheticPattern::Uhd8kTiny
    ) {
        w = 64;
        h = 64;
    }
    let frames = opts.frames.unwrap_or(default_frames);
    if matches!(pat, SyntheticPattern::Uhd8kTiny) && !opts.allow_large {
        return Err(SyntheticError::ResolutionNeedsAllowLarge {
            width: w,
            height: h,
        });
    }
    let px = (w as u64).saturating_mul(h as u64);
    if px > 33_177_600 && !opts.allow_large {
        return Err(SyntheticError::ResolutionNeedsAllowLarge {
            width: w,
            height: h,
        });
    }
    let frame_bytes = pixel_count_yuv420(w, h);
    let total = frame_bytes.saturating_mul(frames as usize);
    let mut buf = vec![0_u8; total];
    let pattern_name = format!("{pat:?}");
    for fi in 0..frames {
        let chunk = &mut buf[fi as usize * frame_bytes..][..frame_bytes];
        fill_frame(pat, opts.seed, fi, w, h, chunk);
    }
    let meta = SyntheticMeta {
        width: w,
        height: h,
        fps: opts.fps.max(1),
        frames,
        pix_fmt: "yuv420p".to_string(),
        seed: opts.seed,
        pattern: pattern_name,
    };
    Ok((buf, meta))
}

/// Write `.yuv` and sidecar `meta.json`.
pub fn write_yuv_with_meta(
    yuv_path: &std::path::Path,
    meta_path: &std::path::Path,
    opts: &GenerateOptions,
) -> Result<SyntheticMeta, SyntheticError> {
    let (bytes, meta) = generate(opts)?;
    let mut f = std::fs::File::create(yuv_path)?;
    f.write_all(&bytes)?;
    let json = serde_json::to_string_pretty(&meta).map_err(SyntheticError::SerdeJson)?;
    std::fs::write(meta_path, json)?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_same_seed_same_bytes() {
        let o = GenerateOptions {
            pattern: SyntheticPattern::Noise,
            seed: 42,
            frames: Some(2),
            fps: 30,
            allow_large: false,
        };
        let (a, _) = generate(&o).unwrap();
        let (b, _) = generate(&o).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn meta_json_roundtrip_shape() {
        let o = GenerateOptions {
            pattern: SyntheticPattern::Flat,
            seed: 1,
            frames: Some(1),
            fps: 30,
            allow_large: false,
        };
        let (_, m) = generate(&o).unwrap();
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"width\": 64") || j.contains("\"width\":64"));
    }

    #[test]
    fn uhd8k_blocked_without_flag() {
        let o = GenerateOptions {
            pattern: SyntheticPattern::Uhd8kTiny,
            seed: 1,
            frames: Some(1),
            fps: 30,
            allow_large: false,
        };
        assert!(matches!(
            generate(&o),
            Err(SyntheticError::ResolutionNeedsAllowLarge { .. })
        ));
    }

    #[test]
    fn uhd8k_allowed_with_flag() {
        let o = GenerateOptions {
            pattern: SyntheticPattern::Uhd8kTiny,
            seed: 1,
            frames: Some(1),
            fps: 30,
            allow_large: true,
        };
        let (buf, m) = generate(&o).unwrap();
        assert_eq!(m.width, 7680);
        assert_eq!(buf.len(), yuv420p8_frame_bytes(7680, 4320));
    }

    #[test]
    fn temp_write_matches_generate() {
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let full_yuv = dir.join(format!("syn-yuv-{nanos}.yuv"));
        let meta_p = dir.join(format!("syn-meta-{nanos}.json"));
        let o = GenerateOptions {
            pattern: SyntheticPattern::Checker,
            seed: 9,
            frames: Some(3),
            fps: 24,
            allow_large: false,
        };
        write_yuv_with_meta(&full_yuv, &meta_p, &o).unwrap();
        let disk = std::fs::read(&full_yuv).unwrap();
        let (gen, _) = generate(&o).unwrap();
        assert_eq!(disk, gen);
        let _ = std::fs::remove_file(&full_yuv);
        let _ = std::fs::remove_file(&meta_p);
    }
}
