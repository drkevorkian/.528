//! Benchmark SRSV2 core encoder/decoder on raw YUV420p8 input.
//!
//! Optional external comparison: `--compare-x264` uses `ffmpeg` + `libx264` when available.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use libsrs_video::srsv2::frame::VideoPlane;
use libsrs_video::{
    decode_yuv420_srsv2_payload, encode_yuv420_inter_payload, PixelFormat, ResidualEncodeStats,
    ResidualEntropy, SrsV2EncodeSettings, VideoSequenceHeaderV2, YuvFrame,
};
use quality_metrics::{compression_ratio, psnr_u8, ssim_u8_simple};
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(name = "bench_srsv2")]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    width: u32,
    #[arg(long)]
    height: u32,
    #[arg(long)]
    frames: u32,
    #[arg(long)]
    fps: u32,
    #[arg(long, default_value_t = 28)]
    qp: u8,
    #[arg(long, default_value_t = 30)]
    keyint: u32,
    #[arg(long, default_value_t = 16)]
    motion_radius: i16,
    #[arg(long)]
    report_json: PathBuf,
    #[arg(long)]
    report_md: PathBuf,
    #[arg(long, default_value_t = false)]
    compare_x264: bool,
    #[arg(long, default_value_t = 23)]
    x264_crf: u8,
    #[arg(long, default_value = "medium")]
    x264_preset: String,
    /// Residual coding: `auto` picks smaller per block, `explicit` legacy tuples only, `rans` prefers entropy.
    #[arg(long, default_value = "auto")]
    residual_entropy: String,
}

#[derive(Debug, Clone, Serialize)]
struct CodecRow {
    codec: String,
    bytes: u64,
    ratio: f64,
    bitrate_bps: f64,
    psnr_y: f64,
    ssim_y: f64,
    enc_fps: f64,
    dec_fps: f64,
}

#[derive(Debug, Clone, Serialize)]
struct Srsv2Details {
    frames: u32,
    keyframes: u32,
    pframes: u32,
    avg_i_bytes: f64,
    avg_p_bytes: f64,
    encode_seconds: f64,
    decode_seconds: f64,
    residual_entropy: String,
    intra_explicit_blocks: u64,
    intra_rans_blocks: u64,
    p_explicit_chunks: u64,
    p_rans_chunks: u64,
    /// Total SRSV2 payload bytes if every frame were encoded with `ResidualEntropy::Explicit` (same QP/keyint).
    legacy_explicit_total_payload_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct X264Details {
    status: String,
    bytes: Option<u64>,
    encode_seconds: Option<f64>,
    decode_seconds: Option<f64>,
    psnr_y: Option<f64>,
    ssim_y: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct BenchReport {
    note: &'static str,
    residual_note: &'static str,
    command: String,
    raw_bytes: u64,
    width: u32,
    height: u32,
    frames: u32,
    fps: u32,
    srsv2: Srsv2Details,
    x264: Option<X264Details>,
    table: Vec<CodecRow>,
    git_commit: Option<String>,
    os: String,
}

fn parse_residual_entropy(s: &str) -> Result<ResidualEntropy> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(ResidualEntropy::Auto),
        "explicit" => Ok(ResidualEntropy::Explicit),
        "rans" => Ok(ResidualEntropy::Rans),
        _ => Err(anyhow!(
            "--residual-entropy must be auto, explicit, or rans (got {s})"
        )),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cmd_str = std::env::args().collect::<Vec<_>>().join(" ");
    let os = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    let raw = fs::read(&args.input).with_context(|| format!("read {}", args.input.display()))?;
    let expected_frame = yuv420_frame_bytes(args.width, args.height)?;
    let expected = expected_frame
        .checked_mul(args.frames as usize)
        .ok_or_else(|| anyhow!("input size overflow"))?;
    if raw.len() != expected {
        return Err(anyhow!(
            "input size {} does not match expected {} ({} bytes/frame × {} frames)",
            raw.len(),
            expected,
            expected_frame,
            args.frames
        ));
    }

    let seq = VideoSequenceHeaderV2 {
        width: args.width,
        height: args.height,
        profile: libsrs_video::SrsVideoProfile::Main,
        pixel_format: PixelFormat::Yuv420p8,
        color_primaries: libsrs_video::ColorPrimaries::Bt709,
        transfer: libsrs_video::TransferFunction::Sdr,
        matrix: libsrs_video::MatrixCoefficients::Bt709,
        chroma_siting: libsrs_video::ChromaSiting::Center,
        range: libsrs_video::ColorRange::Limited,
        disable_loop_filter: true,
        max_ref_frames: 1,
    };
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let settings = SrsV2EncodeSettings {
        keyframe_interval: args.keyint.max(1),
        motion_search_radius: args.motion_radius,
        residual_entropy: re,
        ..Default::default()
    };

    let t0 = Instant::now();
    let mut ref_slot: Option<YuvFrame> = None;
    let mut payloads = Vec::with_capacity(args.frames as usize);
    let mut i_bytes = Vec::<u64>::new();
    let mut p_bytes = Vec::<u64>::new();
    let mut enc_stats = ResidualEncodeStats::default();

    for fi in 0..args.frames {
        let frame = load_yuv420_frame(&raw, expected_frame, fi, args.width, args.height)?;
        let payload = encode_yuv420_inter_payload(
            &seq,
            &frame,
            ref_slot.as_ref(),
            fi,
            args.qp,
            &settings,
            Some(&mut enc_stats),
        )
        .map_err(|e| anyhow!("SRSV2 encode: {e}"))?;
        match payload.get(3).copied() {
            Some(1 | 3) => i_bytes.push(payload.len() as u64),
            Some(2 | 4) => p_bytes.push(payload.len() as u64),
            _ => {}
        }
        // Maintain reference exactly like playback/import: decode updates the slot.
        let _ = decode_yuv420_srsv2_payload(&seq, &payload, &mut ref_slot)
            .map_err(|e| anyhow!("SRSV2 reference refresh: {e}"))?;
        payloads.push(payload);
    }
    let enc_secs = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let mut dec_luma = Vec::with_capacity((args.width * args.height * args.frames) as usize);
    let mut src_luma = Vec::with_capacity(dec_luma.capacity());
    let mut decode_slot = None::<YuvFrame>;
    for fi in 0..args.frames {
        let dec = decode_yuv420_srsv2_payload(&seq, &payloads[fi as usize], &mut decode_slot)
            .map_err(|e| anyhow!("SRSV2 decode: {e}"))?;
        src_luma.extend_from_slice(frame_luma_slice(
            &raw,
            expected_frame,
            fi,
            args.width,
            args.height,
        ));
        dec_luma.extend_from_slice(&dec.yuv.y.samples);
    }
    let dec_secs = t1.elapsed().as_secs_f64();

    let psnr_y = psnr_u8(&src_luma, &dec_luma, 255.0)?;
    let ssim_y = avg_ssim_per_frame(&src_luma, &dec_luma, args.width, args.height, args.frames)?;

    let enc_bytes: u64 = payloads.iter().map(|p| p.len() as u64).sum();
    let raw_bytes = raw.len() as u64;
    let fps = args.fps.max(1) as f64;
    let bitrate_bps = (enc_bytes as f64 * 8.0) * fps / (args.frames.max(1) as f64);

    let srsv2_row = CodecRow {
        codec: "SRSV2".to_string(),
        bytes: enc_bytes,
        ratio: compression_ratio(raw_bytes, enc_bytes),
        bitrate_bps,
        psnr_y,
        ssim_y,
        enc_fps: args.frames as f64 / enc_secs.max(1e-9),
        dec_fps: args.frames as f64 / dec_secs.max(1e-9),
    };

    let mut table = vec![srsv2_row.clone()];

    let legacy_explicit_total_payload_bytes = if matches!(re, ResidualEntropy::Explicit) {
        None
    } else {
        let mut settings_leg = settings.clone();
        settings_leg.residual_entropy = ResidualEntropy::Explicit;
        let mut sum = 0_u64;
        let mut slot = None::<YuvFrame>;
        for fi in 0..args.frames {
            let frame = load_yuv420_frame(&raw, expected_frame, fi, args.width, args.height)?;
            let pl = encode_yuv420_inter_payload(
                &seq,
                &frame,
                slot.as_ref(),
                fi,
                args.qp,
                &settings_leg,
                None,
            )
            .map_err(|e| anyhow!("SRSV2 legacy explicit encode: {e}"))?;
            let _ = decode_yuv420_srsv2_payload(&seq, &pl, &mut slot)
                .map_err(|e| anyhow!("SRSV2 legacy reference refresh: {e}"))?;
            sum += pl.len() as u64;
        }
        Some(sum)
    };

    let x264 = if args.compare_x264 {
        let (row, details) = run_x264_compare(&args, &raw, expected_frame, &src_luma)?;
        if let Some(r) = row {
            table.push(r);
        }
        Some(details)
    } else {
        None
    };

    let report = BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "Residual entropy (FR2 rev 3 intra / rev 4 P) is experimental; auto mode never chooses a larger representation than explicit tuples per block.",
        command: cmd_str,
        raw_bytes,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2: Srsv2Details {
            frames: args.frames,
            keyframes: i_bytes.len() as u32,
            pframes: p_bytes.len() as u32,
            avg_i_bytes: avg_u64(&i_bytes),
            avg_p_bytes: avg_u64(&p_bytes),
            encode_seconds: enc_secs,
            decode_seconds: dec_secs,
            residual_entropy: args.residual_entropy.clone(),
            intra_explicit_blocks: enc_stats.intra_explicit_blocks,
            intra_rans_blocks: enc_stats.intra_rans_blocks,
            p_explicit_chunks: enc_stats.p_explicit_chunks,
            p_rans_chunks: enc_stats.p_rans_chunks,
            legacy_explicit_total_payload_bytes,
        },
        x264,
        table,
        git_commit: git_short_hash(),
        os,
    };

    if let Some(p) = args.report_json.parent() {
        fs::create_dir_all(p).ok();
    }
    if let Some(p) = args.report_md.parent() {
        fs::create_dir_all(p).ok();
    }
    fs::write(&args.report_json, serde_json::to_string_pretty(&report)?)?;
    fs::write(&args.report_md, to_markdown(&report))?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn yuv420_frame_bytes(w: u32, h: u32) -> Result<usize> {
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(anyhow!("YUV420p8 requires non-zero even width/height"));
    }
    let y = (w as usize)
        .checked_mul(h as usize)
        .ok_or_else(|| anyhow!("overflow"))?;
    Ok(y + (y / 2))
}

fn frame_luma_slice(raw: &[u8], frame_bytes: usize, fi: u32, w: u32, h: u32) -> &[u8] {
    let ylen = (w * h) as usize;
    let start = fi as usize * frame_bytes;
    &raw[start..start + ylen]
}

fn load_yuv420_frame(raw: &[u8], frame_bytes: usize, fi: u32, w: u32, h: u32) -> Result<YuvFrame> {
    let ylen = (w * h) as usize;
    let clen = ((w / 2) * (h / 2)) as usize;
    let start = fi as usize * frame_bytes;
    let frame = &raw[start..start + frame_bytes];
    let yb = &frame[..ylen];
    let ub = &frame[ylen..ylen + clen];
    let vb = &frame[ylen + clen..ylen + 2 * clen];

    let mut y = VideoPlane::<u8>::try_new(w, h, w as usize).map_err(|e| anyhow!("{e}"))?;
    y.samples.copy_from_slice(yb);
    let mut u =
        VideoPlane::<u8>::try_new(w / 2, h / 2, (w / 2) as usize).map_err(|e| anyhow!("{e}"))?;
    u.samples.copy_from_slice(ub);
    let mut v =
        VideoPlane::<u8>::try_new(w / 2, h / 2, (w / 2) as usize).map_err(|e| anyhow!("{e}"))?;
    v.samples.copy_from_slice(vb);

    Ok(YuvFrame {
        format: PixelFormat::Yuv420p8,
        y,
        u,
        v,
    })
}

fn avg_ssim_per_frame(
    src_luma: &[u8],
    dec_luma: &[u8],
    w: u32,
    h: u32,
    frames: u32,
) -> Result<f64> {
    let ylen = (w * h) as usize;
    let mut acc = 0.0;
    for fi in 0..frames {
        let s = &src_luma[fi as usize * ylen..][..ylen];
        let d = &dec_luma[fi as usize * ylen..][..ylen];
        acc += ssim_u8_simple(s, d, w as usize, h as usize)?;
    }
    Ok(acc / frames.max(1) as f64)
}

fn avg_u64(v: &[u64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().copied().sum::<u64>() as f64 / v.len() as f64
}

fn git_short_hash() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            o.status
                .success()
                .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_x264_compare(
    args: &Args,
    raw: &[u8],
    frame_bytes: usize,
    src_luma: &[u8],
) -> Result<(Option<CodecRow>, X264Details)> {
    if !ffmpeg_available() {
        return Ok((
            None,
            X264Details {
                status: "ffmpeg unavailable".to_string(),
                bytes: None,
                encode_seconds: None,
                decode_seconds: None,
                psnr_y: None,
                ssim_y: None,
            },
        ));
    }

    let tmp_mp4 = std::env::temp_dir().join("bench-x264.mp4");
    let tmp_dec = std::env::temp_dir().join("bench-x264-dec.yuv");

    let t0 = Instant::now();
    let st = Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-s")
        .arg(format!("{}x{}", args.width, args.height))
        .arg("-r")
        .arg(args.fps.max(1).to_string())
        .arg("-i")
        .arg(args.input.as_os_str())
        .arg("-frames:v")
        .arg(args.frames.to_string())
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg(&args.x264_preset)
        .arg("-crf")
        .arg(args.x264_crf.to_string())
        .arg("-an")
        .arg(tmp_mp4.as_os_str())
        .status()
        .context("ffmpeg x264 encode")?;
    let enc_secs = t0.elapsed().as_secs_f64();
    if !st.success() {
        return Ok((
            None,
            X264Details {
                status: "ffmpeg libx264 encode failed".to_string(),
                bytes: None,
                encode_seconds: Some(enc_secs),
                decode_seconds: None,
                psnr_y: None,
                ssim_y: None,
            },
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
        .arg(args.frames.to_string())
        .arg(tmp_dec.as_os_str())
        .status()
        .context("ffmpeg x264 decode")?;
    let dec_secs = t1.elapsed().as_secs_f64();

    let dec = if st2.success() {
        fs::read(&tmp_dec).unwrap_or_default()
    } else {
        vec![]
    };

    let mut dec_luma = Vec::with_capacity(src_luma.len());
    if dec.len() >= frame_bytes * args.frames as usize {
        let ylen = (args.width * args.height) as usize;
        for fi in 0..args.frames {
            let start = fi as usize * frame_bytes;
            dec_luma.extend_from_slice(&dec[start..start + ylen]);
        }
    }

    let psnr_y = if dec_luma.len() == src_luma.len() {
        Some(psnr_u8(src_luma, &dec_luma, 255.0)?)
    } else {
        None
    };
    let ssim_y = if dec_luma.len() == src_luma.len() {
        Some(avg_ssim_per_frame(
            src_luma,
            &dec_luma,
            args.width,
            args.height,
            args.frames,
        )?)
    } else {
        None
    };

    let _ = fs::remove_file(&tmp_mp4);
    let _ = fs::remove_file(&tmp_dec);

    let raw_bytes = raw.len() as u64;
    let fps = args.fps.max(1) as f64;
    let bitrate_bps = (bytes as f64 * 8.0) * fps / (args.frames.max(1) as f64);
    let row = if let (Some(p), Some(s)) = (psnr_y, ssim_y) {
        Some(CodecRow {
            codec: "x264".to_string(),
            bytes,
            ratio: compression_ratio(raw_bytes, bytes.max(1)),
            bitrate_bps,
            psnr_y: p,
            ssim_y: s,
            enc_fps: args.frames as f64 / enc_secs.max(1e-9),
            dec_fps: args.frames as f64 / dec_secs.max(1e-9),
        })
    } else {
        None
    };

    Ok((
        row,
        X264Details {
            status: "ok".to_string(),
            bytes: Some(bytes),
            encode_seconds: Some(enc_secs),
            decode_seconds: Some(dec_secs),
            psnr_y,
            ssim_y,
        },
    ))
}

fn to_markdown(r: &BenchReport) -> String {
    let mut out = String::new();
    out.push_str("# SRSV2 benchmark report\n\n");
    out.push_str(&format!("**OS:** `{}`\n\n", r.os));
    if let Some(h) = &r.git_commit {
        out.push_str(&format!("**Commit:** `{h}`\n\n"));
    }
    out.push_str(&format!("**Command:** `{}`\n\n", r.command));
    out.push_str("_Engineering measurement only; not a marketing claim._\n\n");
    out.push_str(&format!("_{}_\n\n", r.residual_note));
    out.push_str(
        "| Codec | Bytes | Ratio | Bitrate (bps) | PSNR-Y | SSIM-Y | Enc FPS | Dec FPS |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for row in &r.table {
        out.push_str(&format!(
            "| {} | {} | {:.3} | {:.0} | {:.2} | {:.4} | {:.2} | {:.2} |\n",
            row.codec,
            row.bytes,
            row.ratio,
            row.bitrate_bps,
            row.psnr_y,
            row.ssim_y,
            row.enc_fps,
            row.dec_fps
        ));
    }
    out.push_str("\n## SRSV2 details\n\n");
    out.push_str(&format!(
        "- frames: {}\n- keyframes: {}\n- pframes: {}\n- avg I bytes: {:.1}\n- avg P bytes: {:.1}\n",
        r.srsv2.frames, r.srsv2.keyframes, r.srsv2.pframes, r.srsv2.avg_i_bytes, r.srsv2.avg_p_bytes
    ));
    out.push_str(&format!(
        "- residual_entropy setting: {}\n- intra explicit blocks: {}\n- intra rANS blocks: {}\n- P explicit chunks: {}\n- P rANS chunks: {}\n",
        r.srsv2.residual_entropy,
        r.srsv2.intra_explicit_blocks,
        r.srsv2.intra_rans_blocks,
        r.srsv2.p_explicit_chunks,
        r.srsv2.p_rans_chunks
    ));
    if let Some(lb) = r.srsv2.legacy_explicit_total_payload_bytes {
        out.push_str(&format!(
            "- legacy explicit total payload bytes (same QP/keyint): {}\n",
            lb
        ));
    }
    if let Some(x) = &r.x264 {
        out.push_str("\n## x264 details\n\n");
        out.push_str(&format!("- status: {}\n", x.status));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_input_size() {
        let tmp = std::env::temp_dir().join("bench-wrong.yuv");
        fs::write(&tmp, vec![0_u8; 10]).unwrap();
        let a = Args {
            input: tmp.clone(),
            width: 16,
            height: 16,
            frames: 2,
            fps: 30,
            qp: 28,
            keyint: 30,
            motion_radius: 16,
            report_json: std::env::temp_dir().join("x.json"),
            report_md: std::env::temp_dir().join("x.md"),
            compare_x264: false,
            x264_crf: 23,
            x264_preset: "medium".to_string(),
            residual_entropy: "auto".to_string(),
        };
        let raw = fs::read(&a.input).unwrap();
        let fb = yuv420_frame_bytes(a.width, a.height).unwrap();
        assert!(raw.len() != fb * a.frames as usize);
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn ffmpeg_probe_is_safe() {
        let _ = ffmpeg_available();
    }

    #[test]
    fn report_serializes() {
        let r = BenchReport {
            note: "x",
            residual_note: "n",
            command: "cmd".to_string(),
            raw_bytes: 1,
            width: 2,
            height: 2,
            frames: 1,
            fps: 30,
            srsv2: Srsv2Details {
                frames: 1,
                keyframes: 1,
                pframes: 0,
                avg_i_bytes: 10.0,
                avg_p_bytes: 0.0,
                encode_seconds: 0.1,
                decode_seconds: 0.1,
                residual_entropy: "auto".to_string(),
                intra_explicit_blocks: 0,
                intra_rans_blocks: 0,
                p_explicit_chunks: 0,
                p_rans_chunks: 0,
                legacy_explicit_total_payload_bytes: None,
            },
            x264: None,
            table: vec![CodecRow {
                codec: "SRSV2".to_string(),
                bytes: 10,
                ratio: 1.0,
                bitrate_bps: 1.0,
                psnr_y: 99.0,
                ssim_y: 1.0,
                enc_fps: 1.0,
                dec_fps: 1.0,
            }],
            git_commit: None,
            os: "os".to_string(),
        };
        let _ = serde_json::to_string(&r).unwrap();
    }
}
