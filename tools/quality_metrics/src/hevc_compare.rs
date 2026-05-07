//! Optional **HEVC (libx265)** round-trip via FFmpeg — engineering reference only; no quality claims vs other codecs.
//!
//! Skips cleanly when `ffmpeg` or the **libx265** encoder is unavailable.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::{compression_ratio, psnr_u8, ssim_u8_simple};

/// When MSE is zero, [`psnr_u8`] returns ∞. JSON tables use this finite sentinel (same convention as `--compare-x264`).
const PSNR_Y_JSON_SAFE_IDENTICAL_DB: f64 = 100.0;

/// Inputs for a raw **YUV420p8** clip benchmark (one `.yuv` file, packed frames).
#[derive(Clone, Copy)]
pub struct X265YuvParams<'a> {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u32,
    pub input_path: &'a Path,
    /// Full raw file size (all planes, all frames) — used for compression ratio on the primary table row.
    pub uncompressed_raw_bytes: u64,
    /// Concatenated luma only, `width * height * frames`, for objective metrics.
    pub src_luma: &'a [u8],
    pub yuv420_frame_bytes: usize,
}

/// JSON / Markdown-facing x265 subsection (`--compare-x265`, optional).
#[derive(Debug, Clone, Serialize)]
pub struct X265CompareReport {
    pub x265_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_bitrate_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_psnr_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_ssim_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_encode_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_decode_seconds: Option<f64>,
}

fn report_skipped_no_libx265(command: String) -> X265CompareReport {
    X265CompareReport {
        x265_status: "skipped: ffmpeg has no libx265 encoder".to_string(),
        x265_command: Some(command),
        x265_bytes: None,
        x265_bitrate_bps: None,
        x265_psnr_y: None,
        x265_ssim_y: None,
        x265_encode_seconds: None,
        x265_decode_seconds: None,
    }
}

/// `true` if `ffmpeg -version` succeeds (same probe style as `bench_srsv2`).
pub fn ffmpeg_cli_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reference command string for documentation (placeholder path; actual CLI uses `-i` with the real file).
pub fn reference_ffmpeg_command(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    input_display: &Path,
    preset: &str,
    crf: u8,
) -> String {
    format!(
        "ffmpeg -y -f rawvideo -pix_fmt yuv420p -s {w}x{h} -r {fps} -i \"{inp}\" -frames:v {frames} -c:v libx265 -preset {preset} -crf {crf} -an <output.mp4>",
        w = width,
        h = height,
        fps = fps.max(1),
        inp = input_display.display(),
        frames = frames,
        preset = preset,
        crf = crf,
    )
}

/// Detect **libx265** in the `ffmpeg -encoders` listing (hostile-input-safe substring probe).
pub fn encoders_text_lists_libx265(encoders_stdout_stderr: &str) -> bool {
    // Token match avoids accidental hits in unrelated messages; encoder list lines contain `libx265`.
    encoders_stdout_stderr
        .split_whitespace()
        .any(|w| w == "libx265")
}

fn ffmpeg_encoders_help_text() -> Option<String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s)
}

pub fn ffmpeg_reports_libx265_encoder() -> bool {
    let Some(t) = ffmpeg_encoders_help_text() else {
        return false;
    };
    encoders_text_lists_libx265(&t)
}

fn psnr_y_json_safe(reference: &[u8], measured: &[u8]) -> Result<f64, anyhow::Error> {
    let p = psnr_u8(reference, measured, 255.0).map_err(|e| anyhow!("psnr: {e}"))?;
    if p.is_finite() {
        Ok(p)
    } else if p == f64::INFINITY {
        Ok(PSNR_Y_JSON_SAFE_IDENTICAL_DB)
    } else {
        Err(anyhow!("psnr non-finite (NaN)"))
    }
}

fn avg_ssim_per_frame(
    src_luma: &[u8],
    dec_luma: &[u8],
    w: u32,
    h: u32,
    frames: u32,
) -> Result<f64, anyhow::Error> {
    let ylen = (w * h) as usize;
    let mut acc = 0.0_f64;
    for fi in 0..frames {
        let s = &src_luma[fi as usize * ylen..][..ylen];
        let d = &dec_luma[fi as usize * ylen..][..ylen];
        acc += ssim_u8_simple(s, d, w as usize, h as usize).map_err(|e| anyhow!("ssim: {e}"))?;
    }
    Ok(acc / frames.max(1) as f64)
}

/// Encode **YUV420p** with **libx265**, decode to raw **YUV420p**, compute PSNR-Y / SSIM-Y vs source luma.
///
/// Returns [`Ok`] with a **skipped** or **failed** status in [`X265CompareReport::x265_status`] when tools or encode/decode do not succeed — no panic, no hard error for missing FFmpeg.
pub fn run_libx265_yuv_roundtrip(
    p: X265YuvParams<'_>,
    crf: u8,
    preset: &str,
) -> Result<X265CompareReport> {
    let cmd = reference_ffmpeg_command(
        p.width,
        p.height,
        p.fps.max(1),
        p.frames,
        p.input_path,
        preset,
        crf,
    );

    if !ffmpeg_cli_available() {
        return Ok(X265CompareReport {
            x265_status: "skipped: ffmpeg unavailable".to_string(),
            x265_command: Some(cmd),
            x265_bytes: None,
            x265_bitrate_bps: None,
            x265_psnr_y: None,
            x265_ssim_y: None,
            x265_encode_seconds: None,
            x265_decode_seconds: None,
        });
    }

    if !ffmpeg_reports_libx265_encoder() {
        return Ok(report_skipped_no_libx265(cmd));
    }

    let pid = std::process::id();
    let tmp_mp4 = std::env::temp_dir().join(format!("bench-x265-{pid}.mp4"));
    let tmp_dec = std::env::temp_dir().join(format!("bench-x265-dec-{pid}.yuv"));

    let t0 = Instant::now();
    let st = Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-s")
        .arg(format!("{}x{}", p.width, p.height))
        .arg("-r")
        .arg(p.fps.max(1).to_string())
        .arg("-i")
        .arg(p.input_path.as_os_str())
        .arg("-frames:v")
        .arg(p.frames.to_string())
        .arg("-c:v")
        .arg("libx265")
        .arg("-preset")
        .arg(preset)
        .arg("-crf")
        .arg(crf.to_string())
        .arg("-an")
        .arg(tmp_mp4.as_os_str())
        .status()
        .context("ffmpeg libx265 encode")?;
    let enc_secs = t0.elapsed().as_secs_f64();

    if !st.success() {
        let _ = fs::remove_file(&tmp_mp4);
        let _ = fs::remove_file(&tmp_dec);
        return Ok(X265CompareReport {
            x265_status: "ffmpeg libx265 encode failed".to_string(),
            x265_command: Some(cmd),
            x265_bytes: None,
            x265_bitrate_bps: None,
            x265_psnr_y: None,
            x265_ssim_y: None,
            x265_encode_seconds: Some(enc_secs),
            x265_decode_seconds: None,
        });
    }

    let bytes = fs::metadata(&tmp_mp4).map(|m| m.len()).unwrap_or(0);
    let t1 = Instant::now();
    let st2 = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(tmp_mp4.as_os_str())
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-frames:v")
        .arg(p.frames.to_string())
        .arg(tmp_dec.as_os_str())
        .status()
        .context("ffmpeg libx265 decode")?;
    let dec_secs = t1.elapsed().as_secs_f64();

    let dec = if st2.success() {
        fs::read(&tmp_dec).unwrap_or_default()
    } else {
        vec![]
    };

    let _ = fs::remove_file(&tmp_mp4);
    let _ = fs::remove_file(&tmp_dec);

    if !st2.success() {
        return Ok(X265CompareReport {
            x265_status: "ffmpeg libx265 decode failed".to_string(),
            x265_command: Some(cmd),
            x265_bytes: Some(bytes),
            x265_bitrate_bps: None,
            x265_psnr_y: None,
            x265_ssim_y: None,
            x265_encode_seconds: Some(enc_secs),
            x265_decode_seconds: Some(dec_secs),
        });
    }

    let mut dec_luma = Vec::with_capacity(p.src_luma.len());
    let need = p.yuv420_frame_bytes.saturating_mul(p.frames as usize);
    if dec.len() >= need {
        let ylen = (p.width * p.height) as usize;
        for fi in 0..p.frames {
            let start = fi as usize * p.yuv420_frame_bytes;
            dec_luma.extend_from_slice(&dec[start..start + ylen]);
        }
    }

    let psnr_y = if dec_luma.len() == p.src_luma.len() {
        Some(psnr_y_json_safe(p.src_luma, &dec_luma)?)
    } else {
        None
    };
    let ssim_y = if dec_luma.len() == p.src_luma.len() {
        Some(avg_ssim_per_frame(
            p.src_luma,
            &dec_luma,
            p.width,
            p.height,
            p.frames,
        )?)
    } else {
        None
    };

    let fps = p.fps.max(1) as f64;
    let bitrate_bps = (bytes as f64 * 8.0) * fps / (p.frames.max(1) as f64);

    Ok(X265CompareReport {
        x265_status: "ok".to_string(),
        x265_command: Some(cmd),
        x265_bytes: Some(bytes),
        x265_bitrate_bps: Some(bitrate_bps),
        x265_psnr_y: psnr_y,
        x265_ssim_y: ssim_y,
        x265_encode_seconds: Some(enc_secs),
        x265_decode_seconds: Some(dec_secs),
    })
}

/// Metrics for a **libx265** table row (the `bench_srsv2` binary maps this to its `CodecRow`).
#[derive(Debug, Clone)]
pub struct X265TableMetrics {
    pub bytes: u64,
    pub ratio: f64,
    pub bitrate_bps: f64,
    pub psnr_y: f64,
    pub ssim_y: f64,
    pub enc_fps: f64,
    pub dec_fps: f64,
}

pub fn table_metrics_from_report(
    rep: &X265CompareReport,
    uncompressed_raw_bytes: u64,
    frames: u32,
    fps: u32,
) -> Option<X265TableMetrics> {
    let py = rep.x265_psnr_y?;
    let sy = rep.x265_ssim_y?;
    let bytes = rep.x265_bytes?;
    let enc = rep.x265_encode_seconds?;
    let dec = rep.x265_decode_seconds?;
    let bitrate = rep.x265_bitrate_bps.unwrap_or_else(|| {
        let f = fps.max(1) as f64;
        (bytes as f64 * 8.0) * f / (frames.max(1) as f64)
    });
    Some(X265TableMetrics {
        bytes,
        ratio: compression_ratio(uncompressed_raw_bytes, bytes.max(1)),
        bitrate_bps: bitrate,
        psnr_y: py,
        ssim_y: sy,
        enc_fps: frames as f64 / enc.max(1e-9),
        dec_fps: frames as f64 / dec.max(1e-9),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_command_is_deterministic() {
        let a = reference_ffmpeg_command(
            128,
            64,
            30,
            10,
            Path::new("C:\\a\\b.yuv"),
            "slow",
            24,
        );
        let b = reference_ffmpeg_command(
            128,
            64,
            30,
            10,
            Path::new("C:\\a\\b.yuv"),
            "slow",
            24,
        );
        assert_eq!(a, b);
        assert!(a.contains("libx265"));
        assert!(a.contains("128x64"));
    }

    #[test]
    fn encoder_list_probe_token() {
        assert!(!encoders_text_lists_libx265(""));
        assert!(!encoders_text_lists_libx265("libx264 only"));
        assert!(encoders_text_lists_libx265("V....D libx265      libx265 HEVC (codec hevc)"));
    }

    #[test]
    fn skipped_reports_serialize() {
        let r = X265CompareReport {
            x265_status: "skipped: ffmpeg unavailable".to_string(),
            x265_command: Some("ffmpeg ...".into()),
            x265_bytes: None,
            x265_bitrate_bps: None,
            x265_psnr_y: None,
            x265_ssim_y: None,
            x265_encode_seconds: None,
            x265_decode_seconds: None,
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("x265_status"));
        assert!(j.contains("skipped"));
        let r2 = X265CompareReport {
            x265_status: "skipped: ffmpeg has no libx265 encoder".to_string(),
            x265_command: Some("ffmpeg ...".into()),
            x265_bytes: None,
            x265_bitrate_bps: None,
            x265_psnr_y: None,
            x265_ssim_y: None,
            x265_encode_seconds: None,
            x265_decode_seconds: None,
        };
        let j2 = serde_json::to_string(&r2).unwrap();
        assert!(j2.contains("libx265"));
    }

    #[test]
    fn table_metrics_none_when_quality_missing() {
        let r = X265CompareReport {
            x265_status: "skipped: ffmpeg unavailable".to_string(),
            x265_command: None,
            x265_bytes: None,
            x265_bitrate_bps: None,
            x265_psnr_y: None,
            x265_ssim_y: None,
            x265_encode_seconds: None,
            x265_decode_seconds: None,
        };
        assert!(table_metrics_from_report(&r, 1000, 1, 30).is_none());
    }

    #[test]
    fn ffmpeg_missing_branch_matches_probe_contract() {
        let skip = !ffmpeg_cli_available();
        if skip {
            let p = X265YuvParams {
                width: 16,
                height: 16,
                fps: 30,
                frames: 1,
                input_path: Path::new("nope.yuv"),
                uncompressed_raw_bytes: 100,
                src_luma: &[0_u8; 16 * 16],
                yuv420_frame_bytes: 16 * 16 * 3 / 2,
            };
            let r = run_libx265_yuv_roundtrip(p, 28, "medium").unwrap();
            assert!(r.x265_status.contains("skipped"));
            assert!(r.x265_status.contains("ffmpeg"));
        }
    }

    #[test]
    fn ffmpeg_present_but_no_libx265_branch() {
        if !ffmpeg_cli_available() {
            return;
        }
        if ffmpeg_reports_libx265_encoder() {
            return;
        }
        let p = X265YuvParams {
            width: 16,
            height: 16,
            fps: 30,
            frames: 1,
            input_path: Path::new("nope.yuv"),
            uncompressed_raw_bytes: 100,
            src_luma: &[0_u8; 16 * 16],
            yuv420_frame_bytes: 16 * 16 * 3 / 2,
        };
        let r = run_libx265_yuv_roundtrip(p, 28, "medium").unwrap();
        assert!(
            r.x265_status.contains("libx265"),
            "status={}",
            r.x265_status
        );
    }
}
