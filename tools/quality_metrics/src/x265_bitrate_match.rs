//! Bitrate-aligned **libx265** reference measurements vs an explicit kbps target or SRSV2’s achieved bitrate.
//!
//! Engineering tool only — no claims that any encoder “wins”; CRF‑only rows remain an incomplete fairness story.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::hevc_compare::{
    ffmpeg_cli_available, ffmpeg_reports_libx265_encoder, reference_ffmpeg_command,
    run_libx265_yuv_roundtrip, X265CompareReport, X265YuvParams,
};
use crate::{psnr_u8, ssim_u8_simple};

/// When MSE is zero, [`psnr_u8`] returns ∞. JSON uses this finite sentinel (matches `hevc_compare`).
const PSNR_Y_JSON_SAFE_IDENTICAL_DB: f64 = 100.0;

/// One attempted x265 encode (CRF sweep cell or fixed-bitrate encode).
#[derive(Debug, Clone, Serialize)]
pub struct X265SweepRow {
    /// `None` when the attempt used average bitrate constraints instead of CRF.
    pub crf: Option<u8>,
    pub encode_command: String,
    pub ok: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub psnr_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssim_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encode_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_seconds: Option<f64>,
    /// `100 × (achieved − target) / target` using the report target (`target_bps`); `None` if undefined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_error_percent: Option<f64>,
}

/// JSON-facing aggregate for [`crate::bench_srsv2`] (`--match-x265-bitrate`).
#[derive(Debug, Clone, Serialize)]
pub struct X265BitrateMatchReport {
    pub x265_match_status: String,
    /// Nominal matched target (**kbps**) — either `--x265-target-bitrate-kbps` or SRSV2 bps÷1000.
    pub x265_target_bitrate_kbps: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_best_crf: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_best_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_best_bitrate_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_best_psnr_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_best_ssim_y: Option<f64>,
    /// For the selected best row: `100 × (achieved − target_bps) / target_bps`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x265_bitrate_error_percent: Option<f64>,
    pub x265_sweep_rows: Vec<X265SweepRow>,
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

/// Human-readable FFmpeg template for bitrate-style libx265 (container MP4 fragment).
pub fn reference_ffmpeg_bitrate_command(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    input_display: &Path,
    preset: &str,
    target_bitrate_kbps: u32,
) -> String {
    format!(
        "ffmpeg -y -f rawvideo -pix_fmt yuv420p -s {w}x{h} -r {fps} -i \"{inp}\" \
-frames:v {frames} -c:v libx265 -preset {preset} -b:v {tb}k -maxrate {tb}k \
-bufsize {buf}k -an <output.mp4>",
        w = width,
        h = height,
        fps = fps.max(1),
        inp = input_display.display(),
        frames = frames,
        preset = preset,
        tb = target_bitrate_kbps,
        buf = target_bitrate_kbps.saturating_mul(2),
    )
}

fn bitrate_error_percent(achieved_bps: f64, target_bps: f64) -> Option<f64> {
    if !achieved_bps.is_finite() || !target_bps.is_finite() {
        return None;
    }
    if target_bps.abs() <= f64::EPSILON {
        return None;
    }
    Some(100.0 * (achieved_bps - target_bps) / target_bps)
}

/// Deterministic closest match: smallest absolute bitrate distance, then **lower CRF**, then fewer bytes,
/// then earlier sweep index (stable insertion order).
pub fn pick_closest_bitrate_row_index(rows: &[X265SweepRow], target_bps: f64) -> Option<usize> {
    #[derive(Clone, Copy, PartialEq)]
    struct OrdKey {
        dist_bits: OrderedF64,
        crf_ord: u8,
        bytes_ord: u64,
        idx: usize,
    }
    impl Eq for OrdKey {}
    impl PartialOrd for OrdKey {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for OrdKey {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.dist_bits
                .cmp(&other.dist_bits)
                .then_with(|| self.crf_ord.cmp(&other.crf_ord))
                .then_with(|| self.bytes_ord.cmp(&other.bytes_ord))
                .then_with(|| self.idx.cmp(&other.idx))
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    struct OrderedF64(f64);
    impl OrderedF64 {
        fn key(self) -> u64 {
            f64_to_sort_bits(self.0)
        }
    }
    impl Eq for OrderedF64 {}
    impl PartialOrd for OrderedF64 {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for OrderedF64 {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.key().cmp(&other.key())
        }
    }

    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.ok && r.bitrate_bps.is_some_and(|b| b.is_finite()))
        .filter_map(|(idx, r)| {
            let b = r.bitrate_bps?;
            let dist_bits = OrderedF64((b - target_bps).abs());
            let crf_ord = r.crf.unwrap_or(u8::MAX);
            let bytes_ord = r.bytes.unwrap_or(u64::MAX);
            Some((
                OrdKey {
                    dist_bits,
                    crf_ord,
                    bytes_ord,
                    idx,
                },
                idx,
            ))
        })
        .min_by_key(|(k, _)| *k)
        .map(|(_, idx)| idx)
}

#[inline]
fn f64_to_sort_bits(x: f64) -> u64 {
    let bits = x.to_bits();
    if (bits >> 63) != 0 {
        !bits
    } else {
        bits ^ (1_u64 << 63)
    }
}

fn report_from_compare(
    crf_opt: Option<u8>,
    cmd: String,
    target_bps: f64,
    r: X265CompareReport,
) -> X265SweepRow {
    let ok = r.x265_status == "ok";
    let br = r.x265_bitrate_bps;
    let err_pct = br.and_then(|b| bitrate_error_percent(b, target_bps));
    X265SweepRow {
        crf: crf_opt,
        encode_command: cmd,
        ok,
        status: r.x265_status.clone(),
        bytes: r.x265_bytes,
        bitrate_bps: br,
        psnr_y: r.x265_psnr_y,
        ssim_y: r.x265_ssim_y,
        encode_seconds: r.x265_encode_seconds,
        decode_seconds: r.x265_decode_seconds,
        bitrate_error_percent: err_pct,
    }
}

fn run_libx265_yuv_average_bitrate(
    p: X265YuvParams<'_>,
    preset: &str,
    target_kbps: u32,
) -> Result<(X265CompareReport, String)> {
    let cmd = reference_ffmpeg_bitrate_command(
        p.width,
        p.height,
        p.fps.max(1),
        p.frames,
        p.input_path,
        preset,
        target_kbps,
    );

    if !ffmpeg_cli_available() {
        return Ok((
            X265CompareReport {
                x265_status: "skipped: ffmpeg unavailable".to_string(),
                x265_command: Some(cmd.clone()),
                x265_bytes: None,
                x265_bitrate_bps: None,
                x265_psnr_y: None,
                x265_ssim_y: None,
                x265_encode_seconds: None,
                x265_decode_seconds: None,
            },
            cmd,
        ));
    }

    if !ffmpeg_reports_libx265_encoder() {
        return Ok((
            X265CompareReport {
                x265_status: "skipped: ffmpeg has no libx265 encoder".to_string(),
                x265_command: Some(cmd.clone()),
                x265_bytes: None,
                x265_bitrate_bps: None,
                x265_psnr_y: None,
                x265_ssim_y: None,
                x265_encode_seconds: None,
                x265_decode_seconds: None,
            },
            cmd,
        ));
    }

    let pid = std::process::id();
    let tmp_mp4 = std::env::temp_dir().join(format!("bench-x265-br-{pid}.mp4"));
    let tmp_dec = std::env::temp_dir().join(format!("bench-x265-br-dec-{pid}.yuv"));
    let bufsize_k = target_kbps.saturating_mul(2).max(target_kbps);

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
        .arg("-b:v")
        .arg(format!("{target_kbps}k"))
        .arg("-maxrate")
        .arg(format!("{target_kbps}k"))
        .arg("-bufsize")
        .arg(format!("{bufsize_k}k"))
        .arg("-an")
        .arg(tmp_mp4.as_os_str())
        .status()
        .context("ffmpeg libx265 average-bitrate encode")?;
    let enc_secs = t0.elapsed().as_secs_f64();

    if !st.success() {
        let _ = fs::remove_file(&tmp_mp4);
        let _ = fs::remove_file(&tmp_dec);
        return Ok((
            X265CompareReport {
                x265_status: "ffmpeg libx265 encode failed".to_string(),
                x265_command: Some(cmd.clone()),
                x265_bytes: None,
                x265_bitrate_bps: None,
                x265_psnr_y: None,
                x265_ssim_y: None,
                x265_encode_seconds: Some(enc_secs),
                x265_decode_seconds: None,
            },
            cmd,
        ));
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
        return Ok((
            X265CompareReport {
                x265_status: "ffmpeg libx265 decode failed".to_string(),
                x265_command: Some(cmd.clone()),
                x265_bytes: Some(bytes),
                x265_bitrate_bps: None,
                x265_psnr_y: None,
                x265_ssim_y: None,
                x265_encode_seconds: Some(enc_secs),
                x265_decode_seconds: Some(dec_secs),
            },
            cmd,
        ));
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
            p.src_luma, &dec_luma, p.width, p.height, p.frames,
        )?)
    } else {
        None
    };

    let fps = p.fps.max(1) as f64;
    let bitrate_bps = (bytes as f64 * 8.0) * fps / (p.frames.max(1) as f64);

    Ok((
        X265CompareReport {
            x265_status: "ok".to_string(),
            x265_command: Some(cmd.clone()),
            x265_bytes: Some(bytes),
            x265_bitrate_bps: Some(bitrate_bps),
            x265_psnr_y: psnr_y,
            x265_ssim_y: ssim_y,
            x265_encode_seconds: Some(enc_secs),
            x265_decode_seconds: Some(dec_secs),
        },
        cmd,
    ))
}

/// Run bitrate matching: either a **CRF sweep** (`sweep_crf`) or a **single average-bitrate** encode.
///
/// `target_bitrate_kbps_cli` overrides `srsv2_bitrate_bps` when `Some`.
#[allow(clippy::too_many_arguments)]
pub fn run_x265_bitrate_match(
    p: X265YuvParams<'_>,
    preset: &str,
    srsv2_bitrate_bps: f64,
    target_bitrate_kbps_cli: Option<u32>,
    sweep_crf: bool,
    crf_min: u8,
    crf_max: u8,
    crf_step: u8,
) -> Result<X265BitrateMatchReport> {
    let target_bps = if let Some(k) = target_bitrate_kbps_cli {
        (k as f64) * 1000.0
    } else {
        srsv2_bitrate_bps
    };
    let target_kbps_f = target_bps / 1000.0;

    if !target_bps.is_finite() || target_bps <= 0.0 {
        return Ok(X265BitrateMatchReport {
            x265_match_status: "skipped: invalid target bitrate (need positive SRSV2 bitrate or --x265-target-bitrate-kbps)".to_string(),
            x265_target_bitrate_kbps: target_kbps_f.max(0.0),
            x265_best_crf: None,
            x265_best_bytes: None,
            x265_best_bitrate_bps: None,
            x265_best_psnr_y: None,
            x265_best_ssim_y: None,
            x265_bitrate_error_percent: None,
            x265_sweep_rows: vec![],
        });
    }

    if !ffmpeg_cli_available() {
        let doc = reference_ffmpeg_command(
            p.width,
            p.height,
            p.fps.max(1),
            p.frames,
            p.input_path,
            preset,
            crf_min,
        );
        return Ok(X265BitrateMatchReport {
            x265_match_status: "skipped: ffmpeg unavailable".to_string(),
            x265_target_bitrate_kbps: target_kbps_f,
            x265_best_crf: None,
            x265_best_bytes: None,
            x265_best_bitrate_bps: None,
            x265_best_psnr_y: None,
            x265_best_ssim_y: None,
            x265_bitrate_error_percent: None,
            x265_sweep_rows: vec![X265SweepRow {
                crf: None,
                encode_command: doc,
                ok: false,
                status: "skipped: ffmpeg unavailable".into(),
                bytes: None,
                bitrate_bps: None,
                psnr_y: None,
                ssim_y: None,
                encode_seconds: None,
                decode_seconds: None,
                bitrate_error_percent: None,
            }],
        });
    }

    if !ffmpeg_reports_libx265_encoder() {
        let doc = reference_ffmpeg_command(
            p.width,
            p.height,
            p.fps.max(1),
            p.frames,
            p.input_path,
            preset,
            crf_min,
        );
        return Ok(X265BitrateMatchReport {
            x265_match_status: "skipped: ffmpeg has no libx265 encoder".to_string(),
            x265_target_bitrate_kbps: target_kbps_f,
            x265_best_crf: None,
            x265_best_bytes: None,
            x265_best_bitrate_bps: None,
            x265_best_psnr_y: None,
            x265_best_ssim_y: None,
            x265_bitrate_error_percent: None,
            x265_sweep_rows: vec![X265SweepRow {
                crf: None,
                encode_command: doc,
                ok: false,
                status: "skipped: ffmpeg has no libx265 encoder".into(),
                bytes: None,
                bitrate_bps: None,
                psnr_y: None,
                ssim_y: None,
                encode_seconds: None,
                decode_seconds: None,
                bitrate_error_percent: None,
            }],
        });
    }

    let mut rows: Vec<X265SweepRow> = Vec::new();

    if sweep_crf {
        let mut c = crf_min as u32;
        let c_hi = crf_max as u32;
        let step = crf_step as u32;
        while c <= c_hi {
            let crf = c as u8;
            let cmd = reference_ffmpeg_command(
                p.width,
                p.height,
                p.fps.max(1),
                p.frames,
                p.input_path,
                preset,
                crf,
            );
            let rep = run_libx265_yuv_roundtrip(p, crf, preset)?;
            rows.push(report_from_compare(Some(crf), cmd, target_bps, rep));
            let Some(nc) = c.checked_add(step) else {
                break;
            };
            if nc <= c {
                break;
            }
            c = nc;
        }
    } else {
        let tk = target_bitrate_kbps_cli.unwrap_or_else(|| {
            // Round to integer kbps for ffmpeg -b:v; clamp to at least 1.
            (target_bps / 1000.0).round().max(1.0) as u32
        });
        let (rep, cmd) = run_libx265_yuv_average_bitrate(p, preset, tk.max(1))?;
        rows.push(report_from_compare(None, cmd, target_bps, rep));
    }

    let best_idx = pick_closest_bitrate_row_index(&rows, target_bps);
    let best = best_idx.and_then(|i| rows.get(i));

    let status = if best.is_some() {
        "ok"
    } else if rows.iter().any(|r| r.status.contains("skipped")) {
        rows.iter()
            .find(|r| r.status.contains("skipped"))
            .map(|r| r.status.as_str())
            .unwrap_or("no successful encode in sweep")
    } else {
        "no successful encode in sweep"
    };

    Ok(X265BitrateMatchReport {
        x265_match_status: status.to_string(),
        x265_target_bitrate_kbps: target_kbps_f,
        x265_best_crf: best.and_then(|b| b.crf),
        x265_best_bytes: best.and_then(|b| b.bytes),
        x265_best_bitrate_bps: best.and_then(|b| b.bitrate_bps),
        x265_best_psnr_y: best.and_then(|b| b.psnr_y),
        x265_best_ssim_y: best.and_then(|b| b.ssim_y),
        x265_bitrate_error_percent: best.and_then(|b| b.bitrate_error_percent),
        x265_sweep_rows: rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn reference_bitrate_command_deterministic() {
        let a = reference_ffmpeg_bitrate_command(
            64,
            64,
            24,
            5,
            Path::new("C:\\x\\y.yuv"),
            "faster",
            500,
        );
        let b = reference_ffmpeg_bitrate_command(
            64,
            64,
            24,
            5,
            Path::new("C:\\x\\y.yuv"),
            "faster",
            500,
        );
        assert_eq!(a, b);
        assert!(a.contains("libx265"));
        assert!(a.contains("-b:v 500k"));
    }

    #[test]
    fn closest_row_prefers_absolute_error_then_lower_crf() {
        let rows = vec![
            X265SweepRow {
                crf: Some(30),
                encode_command: String::new(),
                ok: true,
                status: "ok".into(),
                bytes: Some(1000),
                bitrate_bps: Some(1_000_000.0),
                psnr_y: Some(30.0),
                ssim_y: Some(0.9),
                encode_seconds: Some(1.0),
                decode_seconds: Some(1.0),
                bitrate_error_percent: None,
            },
            X265SweepRow {
                crf: Some(28),
                encode_command: String::new(),
                ok: true,
                status: "ok".into(),
                bytes: Some(1000),
                bitrate_bps: Some(1_000_000.0),
                psnr_y: Some(30.0),
                ssim_y: Some(0.9),
                encode_seconds: Some(1.0),
                decode_seconds: Some(1.0),
                bitrate_error_percent: None,
            },
        ];
        assert_eq!(pick_closest_bitrate_row_index(&rows, 1_000_000.0), Some(1));
    }

    #[test]
    fn closest_row_ignores_failed_rows() {
        let rows = vec![
            X265SweepRow {
                crf: Some(20),
                encode_command: String::new(),
                ok: false,
                status: "fail".into(),
                bytes: None,
                bitrate_bps: Some(1_000_000.0),
                psnr_y: None,
                ssim_y: None,
                encode_seconds: None,
                decode_seconds: None,
                bitrate_error_percent: None,
            },
            X265SweepRow {
                crf: Some(30),
                encode_command: String::new(),
                ok: true,
                status: "ok".into(),
                bytes: Some(10),
                bitrate_bps: Some(2_000_000.0),
                psnr_y: Some(20.0),
                ssim_y: Some(0.8),
                encode_seconds: Some(1.0),
                decode_seconds: Some(1.0),
                bitrate_error_percent: None,
            },
        ];
        assert_eq!(pick_closest_bitrate_row_index(&rows, 1_000_000.0), Some(1));
    }

    #[test]
    fn match_report_serializes() {
        let r = X265BitrateMatchReport {
            x265_match_status: "ok".into(),
            x265_target_bitrate_kbps: 123.4,
            x265_best_crf: Some(27),
            x265_best_bytes: Some(999),
            x265_best_bitrate_bps: Some(987654.0),
            x265_best_psnr_y: Some(31.2),
            x265_best_ssim_y: Some(0.95),
            x265_bitrate_error_percent: Some(-1.2),
            x265_sweep_rows: vec![X265SweepRow {
                crf: Some(27),
                encode_command: "ffmpeg ...".into(),
                ok: true,
                status: "ok".into(),
                bytes: Some(999),
                bitrate_bps: Some(987654.0),
                psnr_y: Some(31.2),
                ssim_y: Some(0.95),
                encode_seconds: Some(1.0),
                decode_seconds: Some(0.9),
                bitrate_error_percent: Some(-1.2),
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("x265_match_status"));
        assert!(s.contains("x265_sweep_rows"));
    }

    #[test]
    fn ffmpeg_missing_skips_with_empty_or_placeholder_row() {
        if ffmpeg_cli_available() {
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
        let r = run_x265_bitrate_match(p, "medium", 500_000.0, None, true, 28, 30, 2).unwrap();
        assert!(r.x265_match_status.contains("skipped"));
        assert!(!r.x265_sweep_rows.is_empty());
    }

    #[test]
    fn ffmpeg_present_no_libx265_skips() {
        if !ffmpeg_cli_available() || ffmpeg_reports_libx265_encoder() {
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
        let r = run_x265_bitrate_match(p, "medium", 500_000.0, None, true, 28, 28, 1).unwrap();
        assert!(
            r.x265_match_status.contains("libx265")
                || r.x265_sweep_rows
                    .iter()
                    .any(|x| x.status.contains("libx265"))
        );
    }
}
