//! Benchmark SRSV2 core encoder/decoder on raw YUV420p8 input.
//!
//! Optional external comparison: `--compare-x264` uses `ffmpeg` + `libx264` when available.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use libsrs_video::srsv2::frame::VideoPlane;
use libsrs_video::{
    decode_yuv420_srsv2_payload, encode_yuv420_inter_payload, PixelFormat, PreviousFrameRcStats,
    ResidualEncodeStats, ResidualEntropy, SrsV2EncodeSettings, SrsV2RateControlMode,
    SrsV2RateController, VideoSequenceHeaderV2, YuvFrame,
};
use quality_metrics::{compression_ratio, psnr_u8, ssim_u8_simple};
use serde::Serialize;

#[derive(Parser, Debug, Clone)]
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

    #[arg(long, default_value_t = false)]
    compare_residual_modes: bool,

    #[arg(long, default_value_t = false)]
    sweep: bool,

    /// Rate control: `fixed-qp`, `quality`, `target-bitrate`.
    #[arg(long, default_value = "fixed-qp")]
    rc: String,

    #[arg(long)]
    quality: Option<u8>,

    #[arg(long)]
    target_bitrate_kbps: Option<u32>,

    #[arg(long)]
    max_bitrate_kbps: Option<u32>,

    #[arg(long, default_value_t = 4)]
    min_qp: u8,

    #[arg(long, default_value_t = 51)]
    max_qp: u8,

    #[arg(long, default_value_t = 2)]
    qp_step_limit: u8,
}

#[derive(Debug, Clone, Serialize)]
struct CodecRow {
    codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    bytes: u64,
    ratio: f64,
    bitrate_bps: f64,
    psnr_y: f64,
    ssim_y: f64,
    enc_fps: f64,
    dec_fps: f64,
}

#[derive(Debug, Clone, Serialize)]
struct RcBenchReport {
    mode: String,
    target_bitrate_kbps: Option<u32>,
    achieved_bitrate_bps: f64,
    bitrate_error_percent: f64,
    min_qp_used: u8,
    max_qp_used: u8,
    avg_qp: f64,
    /// Per-frame QP sequence (full detail).
    qp_per_frame: Vec<u8>,
    /// Count of frames at each QP (deterministic summary).
    qp_summary: String,
    frame_payload_bytes: Vec<u64>,
    frame_bytes_summary: String,
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
    legacy_explicit_total_payload_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rc: Option<RcBenchReport>,
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
struct ResidualCompareEntry {
    pub label: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub row: CodecRow,
    pub details: Srsv2Details,
}

#[derive(Debug, Clone, Serialize)]
struct SweepRunReport {
    qp: u8,
    residual_entropy: String,
    motion_radius: i16,
    row: CodecRow,
    details: Srsv2Details,
}

#[derive(Debug, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_residual_modes: Option<Vec<ResidualCompareEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sweep: Option<Vec<SweepRunReport>>,
    git_commit: Option<String>,
    os: String,
}

#[derive(Debug, Serialize)]
struct SweepFileReport {
    note: &'static str,
    command: String,
    sweep: Vec<SweepRunReport>,
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

fn parse_rc_mode(s: &str) -> Result<SrsV2RateControlMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "fixed-qp" => Ok(SrsV2RateControlMode::FixedQp),
        "quality" | "constant-quality" => Ok(SrsV2RateControlMode::ConstantQuality),
        "target-bitrate" => Ok(SrsV2RateControlMode::TargetBitrate),
        _ => Err(anyhow!(
            "--rc must be fixed-qp, quality, or target-bitrate (got {s})"
        )),
    }
}

fn rc_mode_cli_label(m: SrsV2RateControlMode) -> &'static str {
    match m {
        SrsV2RateControlMode::FixedQp => "fixed-qp",
        SrsV2RateControlMode::ConstantQuality => "quality",
        SrsV2RateControlMode::TargetBitrate => "target-bitrate",
    }
}

fn summarize_qp_counts(qp: &[u8]) -> String {
    let mut m: BTreeMap<u8, usize> = BTreeMap::new();
    for &q in qp {
        *m.entry(q).or_insert(0) += 1;
    }
    m.iter()
        .map(|(k, c)| format!("qp{k}:{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn summarize_frame_bytes_hist(v: &[u64]) -> String {
    if v.is_empty() {
        return String::new();
    }
    let sum: u64 = v.iter().sum();
    let mn = *v.iter().min().unwrap();
    let mx = *v.iter().max().unwrap();
    let avg = sum as f64 / v.len() as f64;
    format!("sum={sum} min={mn} max={mx} avg={avg:.1}")
}

fn validate_args(args: &Args) -> Result<()> {
    if args.frames == 0 {
        bail!("--frames must be > 0");
    }
    if args.fps == 0 {
        bail!("--fps must be > 0");
    }
    let rc = parse_rc_mode(&args.rc)?;
    match rc {
        SrsV2RateControlMode::ConstantQuality => {
            if args.quality.is_none() {
                bail!("--rc quality requires --quality N");
            }
        }
        SrsV2RateControlMode::TargetBitrate => {
            let Some(tb) = args.target_bitrate_kbps else {
                bail!("--rc target-bitrate requires --target-bitrate-kbps N");
            };
            if tb == 0 {
                bail!("--target-bitrate-kbps must be > 0");
            }
        }
        SrsV2RateControlMode::FixedQp => {}
    }
    if args.min_qp > args.max_qp {
        bail!("--min-qp must be <= --max-qp");
    }
    if args.compare_residual_modes && args.sweep {
        bail!("--compare-residual-modes and --sweep are mutually exclusive");
    }
    Ok(())
}

fn build_settings(args: &Args, residual: ResidualEntropy) -> Result<SrsV2EncodeSettings> {
    let rc = parse_rc_mode(&args.rc)?;
    let s = SrsV2EncodeSettings {
        quantizer: args.qp,
        rate_control_mode: rc,
        quality: args.quality,
        target_bitrate_kbps: args.target_bitrate_kbps,
        max_bitrate_kbps: args.max_bitrate_kbps,
        min_qp: args.min_qp,
        max_qp: args.max_qp,
        qp_step_limit_per_frame: args.qp_step_limit,
        keyframe_interval: args.keyint.max(1),
        motion_search_radius: args.motion_radius,
        residual_entropy: residual,
        ..Default::default()
    };
    s.validate_rate_control()
        .map_err(|e| anyhow!("rate-control settings: {e}"))?;
    Ok(s)
}

struct PassNumbers {
    qp_hist: Vec<u8>,
    byte_hist: Vec<u64>,
    enc_stats: ResidualEncodeStats,
    enc_secs: f64,
    dec_secs: f64,
    payloads: Vec<Vec<u8>>,
    psnr_y: f64,
    ssim_y: f64,
    legacy_explicit_total_payload_bytes: Option<u64>,
}

fn run_srsv2_numbers(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    settings: &SrsV2EncodeSettings,
    expected_frame: usize,
) -> Result<PassNumbers> {
    let mut rc = SrsV2RateController::new(settings, args.fps.max(1), 1)
        .map_err(|e| anyhow!("rate controller: {e}"))?;

    let t0 = Instant::now();
    let mut ref_slot: Option<YuvFrame> = None;
    let mut payloads = Vec::with_capacity(args.frames as usize);
    let mut enc_stats = ResidualEncodeStats::default();
    let mut qp_hist = Vec::with_capacity(args.frames as usize);
    let mut byte_hist = Vec::with_capacity(args.frames as usize);
    let mut prev: Option<PreviousFrameRcStats> = None;

    for fi in 0..args.frames {
        let qp = rc.qp_for_frame(fi, prev);

        let frame = load_yuv420_frame(raw, expected_frame, fi, args.width, args.height)?;
        let payload = encode_yuv420_inter_payload(
            seq,
            &frame,
            ref_slot.as_ref(),
            fi,
            qp,
            settings,
            Some(&mut enc_stats),
        )
        .map_err(|e| anyhow!("SRSV2 encode: {e}"))?;

        let is_i = matches!(payload.get(3).copied(), Some(1 | 3));
        qp_hist.push(qp);
        byte_hist.push(payload.len() as u64);
        rc.observe_frame(fi, payload.len(), is_i);

        let _ = decode_yuv420_srsv2_payload(seq, &payload, &mut ref_slot)
            .map_err(|e| anyhow!("SRSV2 reference refresh: {e}"))?;

        prev = Some(PreviousFrameRcStats {
            encoded_bytes: payload.len() as u32,
            is_keyframe: is_i,
        });
        payloads.push(payload);
    }
    let enc_secs = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let mut dec_luma = Vec::with_capacity((args.width * args.height * args.frames) as usize);
    let mut src_luma = Vec::with_capacity(dec_luma.capacity());
    let mut decode_slot = None::<YuvFrame>;
    for fi in 0..args.frames {
        let dec = decode_yuv420_srsv2_payload(seq, &payloads[fi as usize], &mut decode_slot)
            .map_err(|e| anyhow!("SRSV2 decode: {e}"))?;
        src_luma.extend_from_slice(frame_luma_slice(
            raw,
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

    let legacy_explicit_total_payload_bytes =
        if matches!(settings.residual_entropy, ResidualEntropy::Explicit) {
            None
        } else {
            let mut settings_leg = settings.clone();
            settings_leg.residual_entropy = ResidualEntropy::Explicit;
            let mut sum = 0_u64;
            let mut slot = None::<YuvFrame>;
            for fi in 0..args.frames {
                let frame = load_yuv420_frame(raw, expected_frame, fi, args.width, args.height)?;
                let qpi = *qp_hist.get(fi as usize).unwrap_or(&args.qp);
                let pl = encode_yuv420_inter_payload(
                    seq,
                    &frame,
                    slot.as_ref(),
                    fi,
                    qpi,
                    &settings_leg,
                    None,
                )
                .map_err(|e| anyhow!("SRSV2 legacy explicit encode: {e}"))?;
                let _ = decode_yuv420_srsv2_payload(seq, &pl, &mut slot)
                    .map_err(|e| anyhow!("SRSV2 legacy reference refresh: {e}"))?;
                sum += pl.len() as u64;
            }
            Some(sum)
        };

    Ok(PassNumbers {
        qp_hist,
        byte_hist,
        enc_stats,
        enc_secs,
        dec_secs,
        payloads,
        psnr_y,
        ssim_y,
        legacy_explicit_total_payload_bytes,
    })
}

fn rc_report_from_pass(
    args: &Args,
    settings: &SrsV2EncodeSettings,
    p: &PassNumbers,
) -> RcBenchReport {
    let enc_bytes: u64 = p.byte_hist.iter().sum();
    let fps = args.fps.max(1) as f64;
    let achieved = (enc_bytes as f64 * 8.0) * fps / (args.frames.max(1) as f64);
    let target_kbps = settings.target_bitrate_kbps;
    let err_pct = target_kbps.map(|tk| {
        let target_bps = tk as f64 * 1000.0;
        if target_bps <= 0.0 {
            0.0
        } else {
            (achieved - target_bps) / target_bps * 100.0
        }
    });

    let min_qp = *p.qp_hist.iter().min().unwrap_or(&args.qp);
    let max_qp = *p.qp_hist.iter().max().unwrap_or(&args.qp);
    let avg_qp = p.qp_hist.iter().map(|&x| x as f64).sum::<f64>() / (p.qp_hist.len().max(1) as f64);

    RcBenchReport {
        mode: rc_mode_cli_label(settings.rate_control_mode).to_string(),
        target_bitrate_kbps: target_kbps,
        achieved_bitrate_bps: achieved,
        bitrate_error_percent: err_pct.unwrap_or(0.0),
        min_qp_used: min_qp,
        max_qp_used: max_qp,
        avg_qp,
        qp_per_frame: p.qp_hist.clone(),
        qp_summary: summarize_qp_counts(&p.qp_hist),
        frame_payload_bytes: p.byte_hist.clone(),
        frame_bytes_summary: summarize_frame_bytes_hist(&p.byte_hist),
    }
}

fn pass_to_details(
    args: &Args,
    settings: &SrsV2EncodeSettings,
    residual_label: &str,
    p: &PassNumbers,
) -> Srsv2Details {
    let mut i_bytes = Vec::new();
    let mut p_bytes = Vec::new();
    for pl in &p.payloads {
        match pl.get(3).copied() {
            Some(1 | 3) => i_bytes.push(pl.len() as u64),
            Some(2 | 4) => p_bytes.push(pl.len() as u64),
            _ => {}
        }
    }
    Srsv2Details {
        frames: args.frames,
        keyframes: i_bytes.len() as u32,
        pframes: p_bytes.len() as u32,
        avg_i_bytes: avg_u64(&i_bytes),
        avg_p_bytes: avg_u64(&p_bytes),
        encode_seconds: p.enc_secs,
        decode_seconds: p.dec_secs,
        residual_entropy: residual_label.to_string(),
        intra_explicit_blocks: p.enc_stats.intra_explicit_blocks,
        intra_rans_blocks: p.enc_stats.intra_rans_blocks,
        p_explicit_chunks: p.enc_stats.p_explicit_chunks,
        p_rans_chunks: p.enc_stats.p_rans_chunks,
        legacy_explicit_total_payload_bytes: p.legacy_explicit_total_payload_bytes,
        rc: Some(rc_report_from_pass(args, settings, p)),
    }
}

fn pass_to_row(args: &Args, codec: &str, p: &PassNumbers) -> CodecRow {
    let enc_bytes: u64 = p.byte_hist.iter().sum();
    let raw_bytes = raw_len_for_bitrate(args);
    let fps = args.fps.max(1) as f64;
    let bitrate_bps = (enc_bytes as f64 * 8.0) * fps / (args.frames.max(1) as f64);
    CodecRow {
        codec: codec.to_string(),
        error: None,
        bytes: enc_bytes,
        ratio: compression_ratio(raw_bytes, enc_bytes),
        bitrate_bps,
        psnr_y: p.psnr_y,
        ssim_y: p.ssim_y,
        enc_fps: args.frames as f64 / p.enc_secs.max(1e-9),
        dec_fps: args.frames as f64 / p.dec_secs.max(1e-9),
    }
}

fn raw_len_for_bitrate(args: &Args) -> u64 {
    let fb = yuv420_frame_bytes(args.width, args.height).unwrap_or(0);
    (fb as u64).saturating_mul(args.frames as u64)
}

fn run_single_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<BenchReport> {
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let settings = build_settings(args, re)?;
    let numbers = run_srsv2_numbers(args, seq, raw, &settings, expected_frame)?;
    let details = pass_to_details(args, &settings, &args.residual_entropy, &numbers);
    let srsv2_row = pass_to_row(args, "SRSV2", &numbers);
    let mut table = vec![srsv2_row.clone()];

    let src_luma = flatten_src_luma(raw, expected_frame, args)?;

    let x264 = if args.compare_x264 {
        let (row, details_x264) = run_x264_compare(args, raw, expected_frame, &src_luma)?;
        if let Some(r) = row {
            table.push(r);
        }
        Some(details_x264)
    } else {
        None
    };

    Ok(BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "Residual entropy (FR2 rev 3 intra / rev 4 P) is experimental; auto mode never chooses a larger representation than explicit tuples per block.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        raw_bytes: raw.len() as u64,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2: details,
        x264,
        table,
        compare_residual_modes: None,
        sweep: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn flatten_src_luma(raw: &[u8], expected_frame: usize, args: &Args) -> Result<Vec<u8>> {
    let mut src_luma = Vec::with_capacity((args.width * args.height * args.frames) as usize);
    for fi in 0..args.frames {
        src_luma.extend_from_slice(frame_luma_slice(
            raw,
            expected_frame,
            fi,
            args.width,
            args.height,
        ));
    }
    Ok(src_luma)
}

fn run_compare_residual_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<BenchReport> {
    let modes = [
        ("SRSV2-explicit", ResidualEntropy::Explicit),
        ("SRSV2-auto", ResidualEntropy::Auto),
        ("SRSV2-rans", ResidualEntropy::Rans),
    ];
    let mut entries = Vec::new();
    let mut table = Vec::new();
    let mut primary_details: Option<Srsv2Details> = None;

    for (label, re) in modes {
        let st = match build_settings(args, re) {
            Ok(s) => s,
            Err(e) => {
                entries.push(ResidualCompareEntry {
                    label: label.to_string(),
                    ok: false,
                    error: Some(format!("settings: {e:#}")),
                    row: CodecRow {
                        codec: label.to_string(),
                        error: Some(format!("settings: {e:#}")),
                        bytes: 0,
                        ratio: 0.0,
                        bitrate_bps: 0.0,
                        psnr_y: 0.0,
                        ssim_y: 0.0,
                        enc_fps: 0.0,
                        dec_fps: 0.0,
                    },
                    details: Srsv2Details {
                        frames: args.frames,
                        keyframes: 0,
                        pframes: 0,
                        avg_i_bytes: 0.0,
                        avg_p_bytes: 0.0,
                        encode_seconds: 0.0,
                        decode_seconds: 0.0,
                        residual_entropy: format!("{re:?}"),
                        intra_explicit_blocks: 0,
                        intra_rans_blocks: 0,
                        p_explicit_chunks: 0,
                        p_rans_chunks: 0,
                        legacy_explicit_total_payload_bytes: None,
                        rc: None,
                    },
                });
                table.push(entries.last().unwrap().row.clone());
                continue;
            }
        };

        let res_entropy_str = match re {
            ResidualEntropy::Explicit => "explicit",
            ResidualEntropy::Auto => "auto",
            ResidualEntropy::Rans => "rans",
        };

        match run_srsv2_numbers(args, seq, raw, &st, expected_frame) {
            Ok(numbers) => {
                let row = pass_to_row(args, label, &numbers);
                let details = pass_to_details(args, &st, res_entropy_str, &numbers);
                if primary_details.is_none() || label == "SRSV2-auto" {
                    primary_details = Some(details.clone());
                }
                entries.push(ResidualCompareEntry {
                    label: label.to_string(),
                    ok: true,
                    error: None,
                    row: row.clone(),
                    details,
                });
                table.push(row);
            }
            Err(e) => {
                entries.push(ResidualCompareEntry {
                    label: label.to_string(),
                    ok: false,
                    error: Some(format!("{e:#}")),
                    row: CodecRow {
                        codec: label.to_string(),
                        error: Some(format!("{e:#}")),
                        bytes: 0,
                        ratio: 0.0,
                        bitrate_bps: 0.0,
                        psnr_y: 0.0,
                        ssim_y: 0.0,
                        enc_fps: 0.0,
                        dec_fps: 0.0,
                    },
                    details: Srsv2Details {
                        frames: args.frames,
                        keyframes: 0,
                        pframes: 0,
                        avg_i_bytes: 0.0,
                        avg_p_bytes: 0.0,
                        encode_seconds: 0.0,
                        decode_seconds: 0.0,
                        residual_entropy: res_entropy_str.to_string(),
                        intra_explicit_blocks: 0,
                        intra_rans_blocks: 0,
                        p_explicit_chunks: 0,
                        p_rans_chunks: 0,
                        legacy_explicit_total_payload_bytes: None,
                        rc: None,
                    },
                });
                table.push(entries.last().unwrap().row.clone());
            }
        }
    }

    let srsv2 = primary_details.unwrap_or_else(|| entries[0].details.clone());

    Ok(BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "Residual entropy (FR2 rev 3 intra / rev 4 P) is experimental; compare-residual-modes runs explicit, auto, and rans separately.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        raw_bytes: raw.len() as u64,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2,
        x264: None,
        table,
        compare_residual_modes: Some(entries),
        sweep: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn run_sweep_file(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<()> {
    let qps = [18u8, 22, 28, 34];
    let residuals = [
        ("explicit", ResidualEntropy::Explicit),
        ("auto", ResidualEntropy::Auto),
    ];
    let radii = [0i16, 8, 16];
    let mut sweep = Vec::new();

    for &qp in &qps {
        for &(re_str, re) in &residuals {
            for &mr in &radii {
                let mut a = args.clone();
                a.qp = qp;
                a.motion_radius = mr;
                a.residual_entropy = re_str.to_string();
                let settings = build_settings(&a, re)?;
                if let Ok(numbers) = run_srsv2_numbers(&a, seq, raw, &settings, expected_frame) {
                    let row = pass_to_row(&a, "SRSV2", &numbers);
                    let details = pass_to_details(&a, &settings, re_str, &numbers);
                    sweep.push(SweepRunReport {
                        qp,
                        residual_entropy: re_str.to_string(),
                        motion_radius: mr,
                        row,
                        details,
                    });
                }
            }
        }
    }

    let rep = SweepFileReport {
        note: "Sweep grid (QP × residual × motion radius). Engineering measurement only.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        sweep,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    };

    if let Some(p) = args.report_json.parent() {
        fs::create_dir_all(p).ok();
    }
    if let Some(p) = args.report_md.parent() {
        fs::create_dir_all(p).ok();
    }
    fs::write(&args.report_json, serde_json::to_string_pretty(&rep)?)?;
    fs::write(&args.report_md, sweep_to_markdown(&rep))?;
    println!("{}", serde_json::to_string_pretty(&rep)?);
    Ok(())
}

fn sweep_to_markdown(rep: &SweepFileReport) -> String {
    let mut out = String::new();
    out.push_str("# SRSV2 benchmark sweep\n\n");
    out.push_str("_Engineering measurement only; not a marketing claim._\n\n");
    out.push_str("| QP | residual | motion_r | bytes | ratio | bitrate | PSNR-Y | SSIM-Y |\n");
    out.push_str("|---:|---|---:|---:|---:|---:|---:|---:|\n");
    for r in &rep.sweep {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.3} | {:.0} | {:.2} | {:.4} |\n",
            r.qp,
            r.residual_entropy,
            r.motion_radius,
            r.row.bytes,
            r.row.ratio,
            r.row.bitrate_bps,
            r.row.psnr_y,
            r.row.ssim_y
        ));
    }
    out
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;

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

    if args.sweep {
        return run_sweep_file(&args, &seq, &raw, expected_frame);
    }

    let report = if args.compare_residual_modes {
        run_compare_residual_report(&args, &seq, &raw, expected_frame)?
    } else {
        run_single_report(&args, &seq, &raw, expected_frame)?
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
            error: None,
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
        let note = row
            .error
            .as_ref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "| {}{} | {} | {:.3} | {:.0} | {:.2} | {:.4} | {:.2} | {:.2} |\n",
            row.codec,
            note,
            row.bytes,
            row.ratio,
            row.bitrate_bps,
            row.psnr_y,
            row.ssim_y,
            row.enc_fps,
            row.dec_fps
        ));
    }

    if let Some(cr) = &r.compare_residual_modes {
        out.push_str("\n## Residual mode comparison\n\n");
        for e in cr {
            out.push_str(&format!(
                "- **{}**: ok={} error={:?}\n",
                e.label, e.ok, e.error
            ));
        }
    }

    out.push_str("\n## SRSV2 details\n\n");
    out.push_str(&format!(
        "- frames: {}\n- keyframes: {}\n- pframes: {}\n- avg I bytes: {:.1}\n- avg P bytes: {:.1}\n",
        r.srsv2.frames,
        r.srsv2.keyframes,
        r.srsv2.pframes,
        r.srsv2.avg_i_bytes,
        r.srsv2.avg_p_bytes
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
            "- legacy explicit total payload bytes (same QP path): {}\n",
            lb
        ));
    }
    if let Some(rc) = &r.srsv2.rc {
        out.push_str("\n### Rate control\n\n");
        out.push_str(&format!(
            "- mode: {}\n- target_bitrate_kbps: {:?}\n- achieved_bitrate_bps: {:.2}\n- bitrate_error_percent: {:.2}\n- min/max/avg QP: {}/{}/{:.2}\n- QP summary: {}\n- frame bytes summary: {}\n",
            rc.mode,
            rc.target_bitrate_kbps,
            rc.achieved_bitrate_bps,
            rc.bitrate_error_percent,
            rc.min_qp_used,
            rc.max_qp_used,
            rc.avg_qp,
            rc.qp_summary,
            rc.frame_bytes_summary
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
            compare_residual_modes: false,
            sweep: false,
            rc: "fixed-qp".to_string(),
            quality: None,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit: 2,
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
                rc: Some(RcBenchReport {
                    mode: "fixed-qp".to_string(),
                    target_bitrate_kbps: None,
                    achieved_bitrate_bps: 800.0,
                    bitrate_error_percent: 0.0,
                    min_qp_used: 28,
                    max_qp_used: 28,
                    avg_qp: 28.0,
                    qp_per_frame: vec![28],
                    qp_summary: "qp28:1".to_string(),
                    frame_payload_bytes: vec![100],
                    frame_bytes_summary: "sum=100 min=100 max=100 avg=100.0".to_string(),
                }),
            },
            x264: None,
            table: vec![CodecRow {
                codec: "SRSV2".to_string(),
                error: None,
                bytes: 10,
                ratio: 1.0,
                bitrate_bps: 1.0,
                psnr_y: 99.0,
                ssim_y: 1.0,
                enc_fps: 1.0,
                dec_fps: 1.0,
            }],
            compare_residual_modes: None,
            sweep: None,
            git_commit: None,
            os: "os".to_string(),
        };
        let _ = serde_json::to_string(&r).unwrap();
    }

    #[test]
    fn compare_residual_serializes() {
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
                rc: None,
            },
            x264: None,
            table: vec![],
            compare_residual_modes: Some(vec![ResidualCompareEntry {
                label: "SRSV2-explicit".to_string(),
                ok: true,
                error: None,
                row: CodecRow {
                    codec: "SRSV2-explicit".to_string(),
                    error: None,
                    bytes: 5,
                    ratio: 1.0,
                    bitrate_bps: 1.0,
                    psnr_y: 40.0,
                    ssim_y: 1.0,
                    enc_fps: 1.0,
                    dec_fps: 1.0,
                },
                details: Srsv2Details {
                    frames: 1,
                    keyframes: 1,
                    pframes: 0,
                    avg_i_bytes: 5.0,
                    avg_p_bytes: 0.0,
                    encode_seconds: 0.1,
                    decode_seconds: 0.1,
                    residual_entropy: "explicit".to_string(),
                    intra_explicit_blocks: 1,
                    intra_rans_blocks: 0,
                    p_explicit_chunks: 0,
                    p_rans_chunks: 0,
                    legacy_explicit_total_payload_bytes: None,
                    rc: None,
                },
            }]),
            sweep: None,
            git_commit: None,
            os: "os".to_string(),
        };
        let _ = serde_json::to_string(&r).unwrap();
    }

    #[test]
    fn sweep_report_serializes() {
        let s = SweepFileReport {
            note: "n",
            command: "c".to_string(),
            sweep: vec![SweepRunReport {
                qp: 28,
                residual_entropy: "auto".to_string(),
                motion_radius: 16,
                row: CodecRow {
                    codec: "SRSV2".to_string(),
                    error: None,
                    bytes: 9,
                    ratio: 2.0,
                    bitrate_bps: 100.0,
                    psnr_y: 30.0,
                    ssim_y: 0.9,
                    enc_fps: 10.0,
                    dec_fps: 10.0,
                },
                details: Srsv2Details {
                    frames: 2,
                    keyframes: 1,
                    pframes: 1,
                    avg_i_bytes: 5.0,
                    avg_p_bytes: 4.0,
                    encode_seconds: 0.2,
                    decode_seconds: 0.2,
                    residual_entropy: "auto".to_string(),
                    intra_explicit_blocks: 0,
                    intra_rans_blocks: 0,
                    p_explicit_chunks: 0,
                    p_rans_chunks: 0,
                    legacy_explicit_total_payload_bytes: None,
                    rc: None,
                },
            }],
            git_commit: None,
            os: "os".to_string(),
        };
        let _ = serde_json::to_string(&s).unwrap();
    }

    #[test]
    fn rc_validation_errors_in_bench_layer() {
        let mut a = Args {
            input: PathBuf::from("nope"),
            width: 64,
            height: 64,
            frames: 1,
            fps: 30,
            qp: 28,
            keyint: 30,
            motion_radius: 16,
            report_json: PathBuf::from("j"),
            report_md: PathBuf::from("m"),
            compare_x264: false,
            x264_crf: 23,
            x264_preset: "medium".to_string(),
            residual_entropy: "auto".to_string(),
            compare_residual_modes: false,
            sweep: false,
            rc: "quality".to_string(),
            quality: None,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit: 2,
        };
        assert!(validate_args(&a).is_err());
        a.quality = Some(22);
        assert!(validate_args(&a).is_ok());
    }
}
