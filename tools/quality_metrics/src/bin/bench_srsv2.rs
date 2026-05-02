//! Benchmark SRSV2 core encoder/decoder on raw YUV420p8 input.
//!
//! Optional external comparison: `--compare-x264` uses `ffmpeg` + `libx264` when available.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgAction, Parser};
use libsrs_video::srsv2::frame::VideoPlane;
use libsrs_video::srsv2::validate_adaptive_quant_settings;
use libsrs_video::{
    decode_yuv420_srsv2_payload, decode_yuv420_srsv2_payload_managed,
    encode_yuv420_alt_ref_payload, encode_yuv420_inter_payload, PixelFormat, PreviousFrameRcStats,
    ResidualEncodeStats, ResidualEntropy, SrsV2AdaptiveQuantizationMode, SrsV2AqEncodeStats,
    SrsV2BlockAqMode, SrsV2EncodeSettings, SrsV2LoopFilterMode, SrsV2MotionEncodeStats,
    SrsV2MotionSearchMode, SrsV2RateControlMode, SrsV2RateController, SrsV2ReferenceManager,
    SrsV2SubpelMode, VideoSequenceHeaderV2, YuvFrame,
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

    /// Adaptive quantization: `off`, `activity`, `edge-aware`, `screen-aware` (experimental; frame-level QP only).
    #[arg(long, default_value = "off")]
    aq: String,

    #[arg(long, default_value_t = 4)]
    aq_strength: u8,

    /// Motion search: `none`, `diamond`, `hex`, `hierarchical`, `exhaustive-small` (integer-pel only).
    #[arg(long, default_value = "exhaustive-small")]
    motion_search: String,

    #[arg(long, default_value_t = 0)]
    early_exit_sad_threshold: u32,

    /// P-frame Y sub-block skip (see `SrsV2EncodeSettings::enable_skip_blocks`). Use `false` to force all residuals on-wire.
    #[arg(
        long,
        default_value_t = true,
        num_args = 0..=1,
        default_missing_value = "true",
        action = ArgAction::Set
    )]
    enable_skip_blocks: bool,

    /// Append a small optional grid (AQ off vs activity × diamond vs exhaustive-small); not default.
    #[arg(long, default_value_t = false)]
    sweep_extended: bool,

    /// Experimental luma loop filter: `off` (default) or `simple` (maps to sequence `disable_loop_filter=false`).
    #[arg(long, default_value = "off")]
    loop_filter: String,

    /// Loop-filter strength byte in the sequence header (`0` = codec default when filter on); ignored when `--loop-filter off`.
    #[arg(long, default_value_t = 0)]
    deblock_strength: u8,

    /// Experimental luma half-pel refinement: `off` (default, integer MV rev 2/4) or `half`.
    #[arg(long, default_value = "off")]
    subpel: String,

    #[arg(long, default_value_t = 1)]
    subpel_refinement_radius: u8,

    /// Block-level adaptive QP on wire: `off` (default), `frame-only` (label; same as off), or `block-delta` (`FR2` rev 7/8/9 with adaptive residuals).
    #[arg(long, default_value = "off")]
    block_aq: String,

    /// Encoder clamp for per-block `qp_delta` (must stay within wire ±24 when `--block-aq block-delta`).
    #[arg(long, default_value_t = -6)]
    block_aq_delta_min: i8,

    #[arg(long, default_value_t = 6)]
    block_aq_delta_max: i8,

    /// Reserved for future bench B-GOP wiring; **`> 0` is rejected** (use `libsrs_video` B-frame tests).
    #[arg(long, default_value_t = 0)]
    bframes: u32,

    /// Experimental alt-ref refresh after keyframes: `off` (default) or `on`.
    #[arg(long, default_value = "off")]
    alt_ref: String,

    /// Reserved GOP hint for a future B-bench mode (ignored while `--bframes` stays unsupported).
    #[arg(long, default_value_t = 0)]
    gop: u32,

    /// SRSV2 sequence `max_ref_frames` (**default `1`** — unchanged vs historical bench).
    #[arg(long, default_value_t = 1)]
    reference_frames: u8,
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

#[derive(Debug, Clone, Serialize, Default)]
struct BlockAqWireBenchReport {
    block_aq_enabled: bool,
    min_block_qp_used: u8,
    max_block_qp_used: u8,
    avg_block_qp: f64,
    positive_qp_delta_blocks: u32,
    negative_qp_delta_blocks: u32,
    unchanged_qp_blocks: u32,
}

/// Per-frame adaptive QP derived from **16×16** luma macroblock activity (not written per MB on the wire).
#[derive(Debug, Clone, Serialize, Default)]
struct FrameAqBenchReport {
    enabled: bool,
    base_qp: u8,
    effective_qp: u8,
    mb_activity_min_qp: u8,
    mb_activity_max_qp: u8,
    mb_activity_avg_qp: f64,
    mb_activity_positive_delta_count: u32,
    mb_activity_negative_delta_count: u32,
    mb_activity_unchanged_count: u32,
}

#[derive(Debug, Clone, Serialize)]
struct AqBenchReport {
    mode: String,
    aq_strength: u8,
    min_block_qp_delta: i8,
    max_block_qp_delta: i8,
    block_aq_mode: String,
    frame_aq: FrameAqBenchReport,
    block_aq_wire: BlockAqWireBenchReport,
}

#[derive(Debug, Clone, Serialize, Default)]
struct Fr2RevisionCounts {
    rev1: u32,
    rev2: u32,
    rev3: u32,
    rev4: u32,
    rev5: u32,
    rev6: u32,
    rev7: u32,
    rev8: u32,
    rev9: u32,
    rev10: u32,
    rev11: u32,
    rev12: u32,
}

#[derive(Debug, Clone, Serialize)]
struct DeblockBenchReport {
    loop_filter_mode: String,
    deblock_strength_byte: u8,
    deblock_strength_effective: u8,
    psnr_y: f64,
    ssim_y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    psnr_y_filter_disabled_respin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssim_y_filter_disabled_respin: Option<f64>,
    note: String,
}

impl Default for DeblockBenchReport {
    fn default() -> Self {
        Self {
            loop_filter_mode: "off".to_string(),
            deblock_strength_byte: 0,
            deblock_strength_effective: 0,
            psnr_y: 0.0,
            ssim_y: 0.0,
            psnr_y_filter_disabled_respin: None,
            ssim_y_filter_disabled_respin: None,
            note: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct MotionBenchReport {
    motion_search_mode: String,
    motion_search_radius_effective: i16,
    early_exit_sad_threshold: u32,
    enable_skip_blocks: bool,
    sad_evaluations_total: u64,
    skip_subblocks_total: u64,
    nonzero_motion_macroblocks_total: u64,
    avg_mv_l1_per_nonzero_mb: f64,
    p_frames: u32,
    subpel_mode: String,
    subpel_refinement_radius_effective: u8,
    subpel_blocks_tested_total: u64,
    subpel_blocks_selected_total: u64,
    additional_subpel_evaluations_total: u64,
    /// Mean fractional magnitude in quarter-pel units per macroblock (0 = integer-aligned).
    avg_fractional_mv_qpel_per_mb: f64,
}

#[derive(Debug, Clone, Serialize)]
struct Srsv2Details {
    frames: u32,
    keyframes: u32,
    pframes: u32,
    avg_i_bytes: f64,
    avg_p_bytes: f64,
    /// Experimental multi-reference summary (defaults keep prior bench semantics).
    bframes_enabled: bool,
    bframe_count: u32,
    alt_ref_count: u32,
    display_frame_count: u32,
    reference_frames_used: u8,
    avg_bframe_bytes: f64,
    avg_altref_bytes: f64,
    compression_ratio_displayed_vs_raw: f64,
    psnr_y_displayed_frames: f64,
    ssim_y_displayed_frames: f64,
    encode_seconds: f64,
    decode_seconds: f64,
    residual_entropy: String,
    intra_explicit_blocks: u64,
    intra_rans_blocks: u64,
    p_explicit_chunks: u64,
    p_rans_chunks: u64,
    fr2_revision_counts: Fr2RevisionCounts,
    legacy_explicit_total_payload_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rc: Option<RcBenchReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aq: Option<AqBenchReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    motion: Option<MotionBenchReport>,
    deblock: DeblockBenchReport,
}

impl Default for Srsv2Details {
    fn default() -> Self {
        Self {
            frames: 0,
            keyframes: 0,
            pframes: 0,
            avg_i_bytes: 0.0,
            avg_p_bytes: 0.0,
            bframes_enabled: false,
            bframe_count: 0,
            alt_ref_count: 0,
            display_frame_count: 0,
            reference_frames_used: 1,
            avg_bframe_bytes: 0.0,
            avg_altref_bytes: 0.0,
            compression_ratio_displayed_vs_raw: 0.0,
            psnr_y_displayed_frames: 0.0,
            ssim_y_displayed_frames: 0.0,
            encode_seconds: 0.0,
            decode_seconds: 0.0,
            residual_entropy: String::new(),
            intra_explicit_blocks: 0,
            intra_rans_blocks: 0,
            p_explicit_chunks: 0,
            p_rans_chunks: 0,
            fr2_revision_counts: Fr2RevisionCounts::default(),
            legacy_explicit_total_payload_bytes: None,
            rc: None,
            aq: None,
            motion: None,
            deblock: DeblockBenchReport::default(),
        }
    }
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
    aq: String,
    motion_search: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sweep_variant: Option<String>,
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

fn parse_aq_mode(s: &str) -> Result<SrsV2AdaptiveQuantizationMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "off" => Ok(SrsV2AdaptiveQuantizationMode::Off),
        "activity" => Ok(SrsV2AdaptiveQuantizationMode::Activity),
        "edge-aware" => Ok(SrsV2AdaptiveQuantizationMode::EdgeAware),
        "screen-aware" => Ok(SrsV2AdaptiveQuantizationMode::ScreenAware),
        _ => Err(anyhow!(
            "--aq must be off, activity, edge-aware, or screen-aware (got {s})"
        )),
    }
}

fn parse_motion_search_mode(s: &str) -> Result<SrsV2MotionSearchMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "none" => Ok(SrsV2MotionSearchMode::None),
        "diamond" => Ok(SrsV2MotionSearchMode::Diamond),
        "hex" => Ok(SrsV2MotionSearchMode::Hex),
        "hierarchical" => Ok(SrsV2MotionSearchMode::Hierarchical),
        "exhaustive-small" | "exhaustive" => Ok(SrsV2MotionSearchMode::ExhaustiveSmall),
        _ => Err(anyhow!(
            "--motion-search must be none, diamond, hex, hierarchical, or exhaustive-small (got {s})"
        )),
    }
}

fn parse_loop_filter(s: &str) -> Result<SrsV2LoopFilterMode> {
    match s.to_ascii_lowercase().as_str() {
        "off" => Ok(SrsV2LoopFilterMode::Off),
        "simple" => Ok(SrsV2LoopFilterMode::SimpleDeblock),
        _ => Err(anyhow!("--loop-filter must be off or simple (got {s})")),
    }
}

fn loop_filter_cli_label(m: SrsV2LoopFilterMode) -> &'static str {
    match m {
        SrsV2LoopFilterMode::Off => "off",
        SrsV2LoopFilterMode::SimpleDeblock => "simple",
    }
}

fn parse_alt_ref_flag(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "off" | "false" | "0" => Ok(false),
        "on" | "true" | "1" => Ok(true),
        _ => Err(anyhow!("--alt-ref must be off or on (got {s})")),
    }
}

fn build_seq_header(args: &Args, settings: &SrsV2EncodeSettings) -> VideoSequenceHeaderV2 {
    let disable_loop_filter = matches!(settings.loop_filter_mode, SrsV2LoopFilterMode::Off);
    VideoSequenceHeaderV2 {
        width: args.width,
        height: args.height,
        profile: libsrs_video::SrsVideoProfile::Main,
        pixel_format: PixelFormat::Yuv420p8,
        color_primaries: libsrs_video::ColorPrimaries::Bt709,
        transfer: libsrs_video::TransferFunction::Sdr,
        matrix: libsrs_video::MatrixCoefficients::Bt709,
        chroma_siting: libsrs_video::ChromaSiting::Center,
        range: libsrs_video::ColorRange::Limited,
        disable_loop_filter,
        deblock_strength: if disable_loop_filter {
            0
        } else {
            settings.deblock_strength
        },
        max_ref_frames: args.reference_frames,
    }
}

fn aq_mode_cli_label(m: SrsV2AdaptiveQuantizationMode) -> &'static str {
    match m {
        SrsV2AdaptiveQuantizationMode::Off => "off",
        SrsV2AdaptiveQuantizationMode::Activity => "activity",
        SrsV2AdaptiveQuantizationMode::EdgeAware => "edge-aware",
        SrsV2AdaptiveQuantizationMode::ScreenAware => "screen-aware",
    }
}

fn motion_mode_cli_label(m: SrsV2MotionSearchMode) -> &'static str {
    match m {
        SrsV2MotionSearchMode::None => "none",
        SrsV2MotionSearchMode::Diamond => "diamond",
        SrsV2MotionSearchMode::Hex => "hex",
        SrsV2MotionSearchMode::Hierarchical => "hierarchical",
        SrsV2MotionSearchMode::ExhaustiveSmall => "exhaustive-small",
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
    parse_aq_mode(&args.aq)?;
    parse_motion_search_mode(&args.motion_search)?;
    parse_loop_filter(&args.loop_filter)?;
    parse_subpel_mode(&args.subpel)?;
    parse_block_aq_mode(&args.block_aq)?;
    let _ = parse_alt_ref_flag(&args.alt_ref)?;
    if args.bframes > 0 {
        bail!(
            "--bframes > 0 is not supported in bench_srsv2; keep --bframes 0 (B syntax is covered by libsrs_video unit tests)"
        );
    }
    Ok(())
}

fn parse_subpel_mode(s: &str) -> Result<SrsV2SubpelMode> {
    match s.to_ascii_lowercase().as_str() {
        "off" => Ok(SrsV2SubpelMode::Off),
        "half" => Ok(SrsV2SubpelMode::HalfPel),
        _ => Err(anyhow!("--subpel must be off or half (got {s})")),
    }
}

fn parse_block_aq_mode(s: &str) -> Result<SrsV2BlockAqMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "off" => Ok(SrsV2BlockAqMode::Off),
        "frame-only" => Ok(SrsV2BlockAqMode::FrameOnly),
        "block-delta" => Ok(SrsV2BlockAqMode::BlockDelta),
        _ => Err(anyhow!(
            "--block-aq must be off, frame-only, or block-delta (got {s})"
        )),
    }
}

fn block_aq_cli_label(m: SrsV2BlockAqMode) -> &'static str {
    match m {
        SrsV2BlockAqMode::Off => "off",
        SrsV2BlockAqMode::FrameOnly => "frame-only",
        SrsV2BlockAqMode::BlockDelta => "block-delta",
    }
}

fn subpel_mode_cli_label(m: SrsV2SubpelMode) -> &'static str {
    match m {
        SrsV2SubpelMode::Off => "off",
        SrsV2SubpelMode::HalfPel => "half",
    }
}

fn build_settings(args: &Args, residual: ResidualEntropy) -> Result<SrsV2EncodeSettings> {
    let rc = parse_rc_mode(&args.rc)?;
    let aq_mode = parse_aq_mode(&args.aq)?;
    let motion_mode = parse_motion_search_mode(&args.motion_search)?;
    let loop_filter_mode = parse_loop_filter(&args.loop_filter)?;
    let subpel_mode = parse_subpel_mode(&args.subpel)?;
    let block_aq_mode = parse_block_aq_mode(&args.block_aq)?;
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
        adaptive_quantization_mode: aq_mode,
        aq_strength: args.aq_strength,
        min_block_qp_delta: args.block_aq_delta_min,
        max_block_qp_delta: args.block_aq_delta_max,
        motion_search_mode: motion_mode,
        early_exit_sad_threshold: args.early_exit_sad_threshold,
        enable_skip_blocks: args.enable_skip_blocks,
        subpel_mode,
        subpel_refinement_radius: args.subpel_refinement_radius,
        block_aq_mode,
        loop_filter_mode,
        deblock_strength: args.deblock_strength,
        ..Default::default()
    };
    s.validate_rate_control()
        .map_err(|e| anyhow!("rate-control settings: {e}"))?;
    validate_adaptive_quant_settings(&s).map_err(|e| anyhow!("adaptive quant settings: {e}"))?;
    Ok(s)
}

#[derive(Default)]
struct MotionAgg {
    sad_evaluations: u64,
    skip_subblocks: u64,
    nonzero_motion_macroblocks: u64,
    sum_mv_l1: u64,
    p_frames: u32,
    p_macroblocks_total: u64,
    subpel_blocks_tested: u64,
    subpel_blocks_selected: u64,
    additional_subpel_evaluations: u64,
    sum_abs_frac_qpel: u64,
}

struct PassNumbers {
    qp_hist: Vec<u8>,
    byte_hist: Vec<u64>,
    enc_stats: ResidualEncodeStats,
    enc_secs: f64,
    dec_secs: f64,
    payloads: Vec<Vec<u8>>,
    /// Parallel to `payloads`: source frame index for PSNR when picture is displayable.
    #[allow(dead_code)]
    psnr_src_frame: Vec<Option<u32>>,
    psnr_y: f64,
    ssim_y: f64,
    legacy_explicit_total_payload_bytes: Option<u64>,
    aq_last: SrsV2AqEncodeStats,
    motion_agg: MotionAgg,
}

fn aq_report_from_pass(settings: &SrsV2EncodeSettings, aq: &SrsV2AqEncodeStats) -> AqBenchReport {
    let w = &aq.block_wire;
    AqBenchReport {
        mode: aq_mode_cli_label(settings.adaptive_quantization_mode).to_string(),
        aq_strength: settings.aq_strength,
        min_block_qp_delta: settings.min_block_qp_delta,
        max_block_qp_delta: settings.max_block_qp_delta,
        block_aq_mode: block_aq_cli_label(settings.block_aq_mode).to_string(),
        frame_aq: FrameAqBenchReport {
            enabled: aq.aq_enabled,
            base_qp: aq.base_qp,
            effective_qp: aq.effective_qp,
            mb_activity_min_qp: aq.min_block_qp_used,
            mb_activity_max_qp: aq.max_block_qp_used,
            mb_activity_avg_qp: aq.avg_block_qp,
            mb_activity_positive_delta_count: aq.positive_qp_delta_blocks,
            mb_activity_negative_delta_count: aq.negative_qp_delta_blocks,
            mb_activity_unchanged_count: aq.unchanged_qp_blocks,
        },
        block_aq_wire: BlockAqWireBenchReport {
            block_aq_enabled: w.block_aq_enabled,
            min_block_qp_used: w.min_block_qp_used,
            max_block_qp_used: w.max_block_qp_used,
            avg_block_qp: w.avg_block_qp,
            positive_qp_delta_blocks: w.positive_qp_delta_blocks,
            negative_qp_delta_blocks: w.negative_qp_delta_blocks,
            unchanged_qp_blocks: w.unchanged_qp_blocks,
        },
    }
}

fn fr2_revision_counts(payloads: &[Vec<u8>]) -> Fr2RevisionCounts {
    let mut c = Fr2RevisionCounts::default();
    for p in payloads {
        let Some(&b) = p.get(3) else { continue };
        match b {
            1 => c.rev1 += 1,
            2 => c.rev2 += 1,
            3 => c.rev3 += 1,
            4 => c.rev4 += 1,
            5 => c.rev5 += 1,
            6 => c.rev6 += 1,
            7 => c.rev7 += 1,
            8 => c.rev8 += 1,
            9 => c.rev9 += 1,
            10 => c.rev10 += 1,
            11 => c.rev11 += 1,
            12 => c.rev12 += 1,
            _ => {}
        }
    }
    c
}

fn motion_report_from_pass(settings: &SrsV2EncodeSettings, m: &MotionAgg) -> MotionBenchReport {
    let nz = m.nonzero_motion_macroblocks.max(1);
    let avg_mv = m.sum_mv_l1 as f64 / nz as f64;
    let avg_frac = if m.p_macroblocks_total > 0 {
        m.sum_abs_frac_qpel as f64 / m.p_macroblocks_total as f64
    } else {
        0.0
    };
    MotionBenchReport {
        motion_search_mode: motion_mode_cli_label(settings.motion_search_mode).to_string(),
        motion_search_radius_effective: settings.clamped_motion_search_radius(),
        early_exit_sad_threshold: settings.early_exit_sad_threshold,
        enable_skip_blocks: settings.enable_skip_blocks,
        sad_evaluations_total: m.sad_evaluations,
        skip_subblocks_total: m.skip_subblocks,
        nonzero_motion_macroblocks_total: m.nonzero_motion_macroblocks,
        avg_mv_l1_per_nonzero_mb: avg_mv,
        p_frames: m.p_frames,
        subpel_mode: subpel_mode_cli_label(settings.subpel_mode).to_string(),
        subpel_refinement_radius_effective: settings.clamped_subpel_refinement_radius(),
        subpel_blocks_tested_total: m.subpel_blocks_tested,
        subpel_blocks_selected_total: m.subpel_blocks_selected,
        additional_subpel_evaluations_total: m.additional_subpel_evaluations,
        avg_fractional_mv_qpel_per_mb: avg_frac,
    }
}

fn run_srsv2_numbers(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    settings: &SrsV2EncodeSettings,
    expected_frame: usize,
) -> Result<PassNumbers> {
    let alt_on = parse_alt_ref_flag(&args.alt_ref)?;
    if alt_on && seq.max_ref_frames < 2 {
        bail!("--alt-ref on requires --reference-frames >= 2");
    }

    let mut rc = SrsV2RateController::new(settings, args.fps.max(1), 1)
        .map_err(|e| anyhow!("rate controller: {e}"))?;

    let t0 = Instant::now();
    let mut mgr = SrsV2ReferenceManager::new(seq.max_ref_frames)
        .map_err(|e| anyhow!("reference manager: {e}"))?;
    let mut payloads = Vec::with_capacity(args.frames as usize);
    let mut psnr_src_frame = Vec::with_capacity(args.frames as usize);
    let mut enc_stats = ResidualEncodeStats::default();
    let mut qp_hist = Vec::with_capacity(args.frames as usize);
    let mut byte_hist = Vec::with_capacity(args.frames as usize);
    let mut prev: Option<PreviousFrameRcStats> = None;
    let mut aq_last = SrsV2AqEncodeStats::default();
    let mut motion_agg = MotionAgg::default();

    for fi in 0..args.frames {
        let qp = rc.qp_for_frame(fi, prev);

        let frame = load_yuv420_frame(raw, expected_frame, fi, args.width, args.height)?;
        let mut aq_frame = SrsV2AqEncodeStats::default();
        let mut motion_frame = SrsV2MotionEncodeStats::default();
        let payload = encode_yuv420_inter_payload(
            seq,
            &frame,
            mgr.primary_ref(),
            fi,
            qp,
            settings,
            Some(&mut enc_stats),
            Some(&mut aq_frame),
            Some(&mut motion_frame),
        )
        .map_err(|e| anyhow!("SRSV2 encode: {e}"))?;

        aq_last = aq_frame;
        let is_p = matches!(payload.get(3).copied(), Some(2 | 4 | 5 | 6 | 8 | 9));
        if is_p {
            motion_agg.sad_evaluations += motion_frame.sad_evaluations;
            motion_agg.skip_subblocks += motion_frame.skip_subblocks;
            motion_agg.nonzero_motion_macroblocks += motion_frame.nonzero_motion_macroblocks as u64;
            motion_agg.sum_mv_l1 += motion_frame.sum_mv_l1;
            motion_agg.p_frames += 1;
            let mb = (seq.width / 16) as u64 * (seq.height / 16) as u64;
            motion_agg.p_macroblocks_total += mb;
            motion_agg.subpel_blocks_tested += motion_frame.subpel_blocks_tested;
            motion_agg.subpel_blocks_selected += motion_frame.subpel_blocks_selected;
            motion_agg.additional_subpel_evaluations += motion_frame.additional_subpel_evaluations;
            motion_agg.sum_abs_frac_qpel += motion_frame.sum_abs_frac_qpel;
        }

        let is_i = matches!(payload.get(3).copied(), Some(1 | 3 | 7));
        qp_hist.push(qp);
        byte_hist.push(payload.len() as u64);
        rc.observe_frame(fi, payload.len(), is_i);

        decode_yuv420_srsv2_payload_managed(seq, &payload, &mut mgr)
            .map_err(|e| anyhow!("SRSV2 reference refresh: {e}"))?;

        prev = Some(PreviousFrameRcStats {
            encoded_bytes: payload.len() as u32,
            is_keyframe: is_i,
        });
        payloads.push(payload);
        psnr_src_frame.push(Some(fi));

        if alt_on && is_i {
            let alt = encode_yuv420_alt_ref_payload(seq, &frame, fi, qp, 1)
                .map_err(|e| anyhow!("alt-ref encode: {e}"))?;
            decode_yuv420_srsv2_payload_managed(seq, &alt, &mut mgr)
                .map_err(|e| anyhow!("alt-ref decode: {e}"))?;
            byte_hist.push(alt.len() as u64);
            payloads.push(alt);
            psnr_src_frame.push(None);
        }
    }
    let enc_secs = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let ylen = (args.width * args.height) as usize;
    let mut dec_by_frame = vec![vec![0u8; ylen]; args.frames as usize];
    let mut mgr_dec = SrsV2ReferenceManager::new(seq.max_ref_frames)
        .map_err(|e| anyhow!("decode manager: {e}"))?;
    for (pl, src_ix) in payloads.iter().zip(psnr_src_frame.iter()) {
        let dec = decode_yuv420_srsv2_payload_managed(seq, pl, &mut mgr_dec)
            .map_err(|e| anyhow!("SRSV2 decode: {e}"))?;
        if let Some(fi) = src_ix {
            dec_by_frame[*fi as usize] = dec.yuv.y.samples.clone();
        }
    }
    let mut src_luma = Vec::with_capacity(ylen * args.frames as usize);
    let mut dec_luma = Vec::with_capacity(src_luma.capacity());
    for fi in 0..args.frames {
        src_luma.extend_from_slice(frame_luma_slice(
            raw,
            expected_frame,
            fi,
            args.width,
            args.height,
        ));
        dec_luma.extend_from_slice(&dec_by_frame[fi as usize]);
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
            settings_leg.block_aq_mode = SrsV2BlockAqMode::Off;
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
                    None,
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
        psnr_src_frame,
        psnr_y,
        ssim_y,
        legacy_explicit_total_payload_bytes,
        aq_last,
        motion_agg,
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

fn build_deblock_bench_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    settings: &SrsV2EncodeSettings,
    raw: &[u8],
    expected_frame: usize,
    numbers: &PassNumbers,
) -> Result<DeblockBenchReport> {
    let mode_label = loop_filter_cli_label(settings.loop_filter_mode).to_string();
    let mut rep = DeblockBenchReport {
        loop_filter_mode: mode_label,
        deblock_strength_byte: seq.deblock_strength,
        deblock_strength_effective: seq.effective_deblock_strength_for_filter(),
        psnr_y: numbers.psnr_y,
        ssim_y: numbers.ssim_y,
        psnr_y_filter_disabled_respin: None,
        ssim_y_filter_disabled_respin: None,
        note: String::new(),
    };
    if matches!(
        settings.loop_filter_mode,
        SrsV2LoopFilterMode::SimpleDeblock
    ) {
        let mut seq_off = seq.clone();
        seq_off.disable_loop_filter = true;
        seq_off.deblock_strength = 0;
        let numbers_off = run_srsv2_numbers(args, &seq_off, raw, settings, expected_frame)?;
        rep.psnr_y_filter_disabled_respin = Some(numbers_off.psnr_y);
        rep.ssim_y_filter_disabled_respin = Some(numbers_off.ssim_y);
        rep.note = "Respin uses disable_loop_filter=true (different bitstream than primary). Deblocking can lower PSNR-Y while improving subjective block edges; compare numbers cautiously.".to_string();
    } else {
        rep.note = "Loop filter off; no respin.".to_string();
    }
    Ok(rep)
}

fn pass_to_details(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    settings: &SrsV2EncodeSettings,
    residual_label: &str,
    p: &PassNumbers,
    deblock: DeblockBenchReport,
) -> Srsv2Details {
    let mut i_bytes = Vec::new();
    let mut p_bytes = Vec::new();
    let mut b_bytes = Vec::new();
    let mut alt_bytes = Vec::new();
    for pl in &p.payloads {
        match pl.get(3).copied() {
            Some(1 | 3 | 7) => i_bytes.push(pl.len() as u64),
            Some(2 | 4 | 5 | 6 | 8 | 9) => p_bytes.push(pl.len() as u64),
            Some(10 | 11) => b_bytes.push(pl.len() as u64),
            Some(12) => alt_bytes.push(pl.len() as u64),
            _ => {}
        }
    }
    let fr2 = fr2_revision_counts(&p.payloads);
    let bframe_count = fr2.rev10 + fr2.rev11;
    let alt_ref_count = fr2.rev12;
    let enc_bytes: u64 = p.byte_hist.iter().sum();
    let raw_bytes = raw_len_for_bitrate(args);
    Srsv2Details {
        frames: args.frames,
        keyframes: i_bytes.len() as u32,
        pframes: p_bytes.len() as u32,
        avg_i_bytes: avg_u64(&i_bytes),
        avg_p_bytes: avg_u64(&p_bytes),
        bframes_enabled: args.bframes > 0,
        bframe_count,
        alt_ref_count,
        display_frame_count: args.frames,
        reference_frames_used: seq.max_ref_frames,
        avg_bframe_bytes: avg_u64(&b_bytes),
        avg_altref_bytes: avg_u64(&alt_bytes),
        compression_ratio_displayed_vs_raw: compression_ratio(raw_bytes, enc_bytes),
        psnr_y_displayed_frames: p.psnr_y,
        ssim_y_displayed_frames: p.ssim_y,
        encode_seconds: p.enc_secs,
        decode_seconds: p.dec_secs,
        residual_entropy: residual_label.to_string(),
        intra_explicit_blocks: p.enc_stats.intra_explicit_blocks,
        intra_rans_blocks: p.enc_stats.intra_rans_blocks,
        p_explicit_chunks: p.enc_stats.p_explicit_chunks,
        p_rans_chunks: p.enc_stats.p_rans_chunks,
        fr2_revision_counts: fr2,
        legacy_explicit_total_payload_bytes: p.legacy_explicit_total_payload_bytes,
        rc: Some(rc_report_from_pass(args, settings, p)),
        aq: Some(aq_report_from_pass(settings, &p.aq_last)),
        motion: Some(motion_report_from_pass(settings, &p.motion_agg)),
        deblock,
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
    let deblock = build_deblock_bench_report(args, seq, &settings, raw, expected_frame, &numbers)?;
    let details = pass_to_details(
        args,
        seq,
        &settings,
        &args.residual_entropy,
        &numbers,
        deblock,
    );
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
        residual_note: "Residual entropy (FR2 rev 3 intra / rev 4 P; rev 7–9 add optional block qp_delta with adaptive residuals) is experimental; auto mode never chooses a larger representation than explicit tuples per block.",
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
                        residual_entropy: format!("{re:?}"),
                        ..Default::default()
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
                let deblock =
                    build_deblock_bench_report(args, seq, &st, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(args, seq, &st, res_entropy_str, &numbers, deblock);
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
                        residual_entropy: res_entropy_str.to_string(),
                        ..Default::default()
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
                    let deblock = build_deblock_bench_report(
                        &a,
                        seq,
                        &settings,
                        raw,
                        expected_frame,
                        &numbers,
                    )
                    .unwrap_or_else(|_| DeblockBenchReport::default());
                    let details = pass_to_details(&a, seq, &settings, re_str, &numbers, deblock);
                    sweep.push(SweepRunReport {
                        qp,
                        residual_entropy: re_str.to_string(),
                        motion_radius: mr,
                        aq: a.aq.clone(),
                        motion_search: a.motion_search.clone(),
                        sweep_variant: None,
                        row,
                        details,
                    });
                }
            }
        }
    }

    if args.sweep_extended {
        let extras = [
            (28u8, "off", "exhaustive-small"),
            (28u8, "activity", "diamond"),
        ];
        for (qp, aq_s, ms_s) in extras {
            let mut a = args.clone();
            a.qp = qp;
            a.motion_radius = 8;
            a.residual_entropy = "auto".to_string();
            a.aq = aq_s.to_string();
            a.motion_search = ms_s.to_string();
            let settings = match build_settings(&a, ResidualEntropy::Auto) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Ok(numbers) = run_srsv2_numbers(&a, seq, raw, &settings, expected_frame) {
                let row = pass_to_row(&a, "SRSV2", &numbers);
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(&a, seq, &settings, "auto", &numbers, deblock);
                sweep.push(SweepRunReport {
                    qp,
                    residual_entropy: "auto".to_string(),
                    motion_radius: 8,
                    aq: a.aq.clone(),
                    motion_search: a.motion_search.clone(),
                    sweep_variant: Some("extended-aq-motion".to_string()),
                    row,
                    details,
                });
            }
        }

        let subpel_grid = [
            ("integer-diamond", "off", "diamond"),
            ("halfpel-diamond", "half", "diamond"),
            ("integer-exhaustive-small", "off", "exhaustive-small"),
            ("halfpel-exhaustive-small", "half", "exhaustive-small"),
        ];
        for (label, sub_s, ms_s) in subpel_grid {
            let mut a = args.clone();
            a.qp = 28;
            a.motion_radius = 8;
            a.residual_entropy = "auto".to_string();
            a.aq = "off".to_string();
            a.motion_search = ms_s.to_string();
            a.subpel = sub_s.to_string();
            a.subpel_refinement_radius = 1;
            let settings = match build_settings(&a, ResidualEntropy::Auto) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Ok(numbers) = run_srsv2_numbers(&a, seq, raw, &settings, expected_frame) {
                let row = pass_to_row(&a, "SRSV2", &numbers);
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(&a, seq, &settings, "auto", &numbers, deblock);
                sweep.push(SweepRunReport {
                    qp: 28,
                    residual_entropy: "auto".to_string(),
                    motion_radius: 8,
                    aq: a.aq.clone(),
                    motion_search: a.motion_search.clone(),
                    sweep_variant: Some(format!("subpel-{label}")),
                    row,
                    details,
                });
            }
        }

        for (label, baq) in [("blockaq-off", "off"), ("blockaq-delta", "block-delta")] {
            let mut a = args.clone();
            a.qp = 28;
            a.motion_radius = 8;
            a.residual_entropy = "auto".to_string();
            a.aq = "off".to_string();
            a.motion_search = "diamond".to_string();
            a.subpel = "off".to_string();
            a.block_aq = baq.to_string();
            let settings = match build_settings(&a, ResidualEntropy::Auto) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Ok(numbers) = run_srsv2_numbers(&a, seq, raw, &settings, expected_frame) {
                let row = pass_to_row(&a, "SRSV2", &numbers);
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(&a, seq, &settings, "auto", &numbers, deblock);
                sweep.push(SweepRunReport {
                    qp: 28,
                    residual_entropy: "auto".to_string(),
                    motion_radius: 8,
                    aq: a.aq.clone(),
                    motion_search: a.motion_search.clone(),
                    sweep_variant: Some(label.to_string()),
                    row,
                    details,
                });
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
    out.push_str("| QP | residual | motion_r | aq | motion | variant | bytes | ratio | bitrate | PSNR-Y | SSIM-Y |\n");
    out.push_str("|---:|---|---:|---|---|---:|---:|---:|---:|---:|---:|\n");
    for r in &rep.sweep {
        let var = r.sweep_variant.as_deref().unwrap_or("");
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {:.3} | {:.0} | {:.2} | {:.4} |\n",
            r.qp,
            r.residual_entropy,
            r.motion_radius,
            r.aq,
            r.motion_search,
            var,
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

    let re = parse_residual_entropy(&args.residual_entropy)?;
    let settings = build_settings(&args, re)?;
    let seq = build_seq_header(&args, &settings);

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
    if let Some(aq) = &r.srsv2.aq {
        let fa = &aq.frame_aq;
        let bw = &aq.block_aq_wire;
        out.push_str("\n### Adaptive quantization (experimental)\n\n");
        out.push_str(&format!(
            "- mode: {}\n- aq_strength: {}\n- encoder qp_delta clamp: {} … {}\n- block_aq_mode: {}\n",
            aq.mode, aq.aq_strength, aq.min_block_qp_delta, aq.max_block_qp_delta, aq.block_aq_mode
        ));
        out.push_str("\n**Frame-level AQ** (16×16 MB activity → one effective QP / picture):\n\n");
        out.push_str(&format!(
            "- enabled: {}\n- base_qp / effective_qp: {} / {}\n- MB hint QP min/max/avg: {}/{}/{:.2}\n- MB hints +/−/0 vs base: {}/{}/{}\n",
            fa.enabled,
            fa.base_qp,
            fa.effective_qp,
            fa.mb_activity_min_qp,
            fa.mb_activity_max_qp,
            fa.mb_activity_avg_qp,
            fa.mb_activity_positive_delta_count,
            fa.mb_activity_negative_delta_count,
            fa.mb_activity_unchanged_count
        ));
        out.push_str("\n**Block-level AQ on wire** (FR2 rev 7–9, per 8×8 `qp_delta`):\n\n");
        out.push_str(&format!(
            "- enabled: {}\n- effective QP min/max/avg (blocks): {}/{}/{:.2}\n- +/−/0 qp_delta blocks: {}/{}/{}\n",
            bw.block_aq_enabled,
            bw.min_block_qp_used,
            bw.max_block_qp_used,
            bw.avg_block_qp,
            bw.positive_qp_delta_blocks,
            bw.negative_qp_delta_blocks,
            bw.unchanged_qp_blocks
        ));
    }
    if let Some(m) = &r.srsv2.motion {
        out.push_str("\n### Motion search\n\n");
        out.push_str(&format!(
            "- mode: {}\n- radius (effective): {}\n- early_exit_sad_threshold: {}\n- enable_skip_blocks: {}\n- P-frames: {}\n- SAD evals (total): {}\n- skip subblocks (total): {}\n- nonzero-MV macroblocks (total): {}\n- avg |MV| L1 (nonzero MBs): {:.3}\n",
            m.motion_search_mode,
            m.motion_search_radius_effective,
            m.early_exit_sad_threshold,
            m.enable_skip_blocks,
            m.p_frames,
            m.sad_evaluations_total,
            m.skip_subblocks_total,
            m.nonzero_motion_macroblocks_total,
            m.avg_mv_l1_per_nonzero_mb
        ));
        out.push_str(&format!(
            "- subpel: {} (refinement radius effective: {})\n- subpel blocks tested (total): {}\n- subpel blocks selected (total): {}\n- additional subpel SAD evals (total): {}\n- avg fractional |MV| (qpel units per MB): {:.4}\n",
            m.subpel_mode,
            m.subpel_refinement_radius_effective,
            m.subpel_blocks_tested_total,
            m.subpel_blocks_selected_total,
            m.additional_subpel_evaluations_total,
            m.avg_fractional_mv_qpel_per_mb
        ));
    }
    {
        let d = &r.srsv2.deblock;
        out.push_str("\n### Loop filter (experimental)\n\n");
        out.push_str(&format!(
            "- mode: {}\n- deblock_strength_byte: {}\n- effective strength: {}\n- PSNR-Y / SSIM-Y (primary): {:.2} / {:.4}\n",
            d.loop_filter_mode,
            d.deblock_strength_byte,
            d.deblock_strength_effective,
            d.psnr_y,
            d.ssim_y,
        ));
        if let (Some(py), Some(sy)) = (
            d.psnr_y_filter_disabled_respin,
            d.ssim_y_filter_disabled_respin,
        ) {
            out.push_str(&format!(
                "- PSNR-Y / SSIM-Y (filter-disabled respin): {:.2} / {:.4}\n",
                py, sy,
            ));
        }
        if !d.note.is_empty() {
            out.push_str(&format!("- note: {}\n", d.note));
        }
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
            aq: "off".to_string(),
            aq_strength: 4,
            motion_search: "exhaustive-small".to_string(),
            early_exit_sad_threshold: 0,
            enable_skip_blocks: true,
            sweep_extended: false,
            loop_filter: "off".to_string(),
            deblock_strength: 0,
            subpel: "off".to_string(),
            subpel_refinement_radius: 1,
            block_aq: "off".to_string(),
            block_aq_delta_min: -6,
            block_aq_delta_max: 6,
            bframes: 0,
            alt_ref: "off".to_string(),
            gop: 0,
            reference_frames: 1,
        };
        let raw = fs::read(&a.input).unwrap();
        let fb = yuv420_frame_bytes(a.width, a.height).unwrap();
        assert!(raw.len() != fb * a.frames as usize);
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn bframes_positive_rejected_by_validate_args() {
        let mut a = Args {
            input: PathBuf::from("nope"),
            width: 64,
            height: 64,
            frames: 1,
            fps: 30,
            qp: 28,
            keyint: 30,
            motion_radius: 16,
            report_json: std::env::temp_dir().join("xb.json"),
            report_md: std::env::temp_dir().join("xb.md"),
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
            aq: "off".to_string(),
            aq_strength: 4,
            motion_search: "exhaustive-small".to_string(),
            early_exit_sad_threshold: 0,
            enable_skip_blocks: true,
            sweep_extended: false,
            loop_filter: "off".to_string(),
            deblock_strength: 0,
            subpel: "off".to_string(),
            subpel_refinement_radius: 1,
            block_aq: "off".to_string(),
            block_aq_delta_min: -6,
            block_aq_delta_max: 6,
            bframes: 2,
            alt_ref: "off".to_string(),
            gop: 0,
            reference_frames: 1,
        };
        assert!(validate_args(&a).is_err());
        a.bframes = 0;
        assert!(validate_args(&a).is_ok());
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
                bframes_enabled: false,
                bframe_count: 0,
                alt_ref_count: 0,
                display_frame_count: 1,
                reference_frames_used: 1,
                avg_bframe_bytes: 0.0,
                avg_altref_bytes: 0.0,
                compression_ratio_displayed_vs_raw: 0.0,
                psnr_y_displayed_frames: 99.0,
                ssim_y_displayed_frames: 1.0,
                encode_seconds: 0.1,
                decode_seconds: 0.1,
                residual_entropy: "auto".to_string(),
                intra_explicit_blocks: 0,
                intra_rans_blocks: 0,
                p_explicit_chunks: 0,
                p_rans_chunks: 0,
                fr2_revision_counts: Fr2RevisionCounts::default(),
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
                aq: Some(AqBenchReport {
                    mode: "off".to_string(),
                    aq_strength: 4,
                    min_block_qp_delta: -6,
                    max_block_qp_delta: 6,
                    block_aq_mode: "off".to_string(),
                    frame_aq: FrameAqBenchReport {
                        enabled: false,
                        base_qp: 28,
                        effective_qp: 28,
                        mb_activity_min_qp: 28,
                        mb_activity_max_qp: 28,
                        mb_activity_avg_qp: 28.0,
                        mb_activity_positive_delta_count: 0,
                        mb_activity_negative_delta_count: 0,
                        mb_activity_unchanged_count: 16,
                    },
                    block_aq_wire: BlockAqWireBenchReport::default(),
                }),
                motion: Some(MotionBenchReport {
                    motion_search_mode: "diamond".to_string(),
                    motion_search_radius_effective: 8,
                    early_exit_sad_threshold: 0,
                    enable_skip_blocks: true,
                    sad_evaluations_total: 100,
                    skip_subblocks_total: 2,
                    nonzero_motion_macroblocks_total: 4,
                    avg_mv_l1_per_nonzero_mb: 3.5,
                    p_frames: 3,
                    subpel_mode: "off".to_string(),
                    subpel_refinement_radius_effective: 1,
                    subpel_blocks_tested_total: 0,
                    subpel_blocks_selected_total: 0,
                    additional_subpel_evaluations_total: 0,
                    avg_fractional_mv_qpel_per_mb: 0.0,
                }),
                deblock: DeblockBenchReport {
                    loop_filter_mode: "off".to_string(),
                    deblock_strength_byte: 0,
                    deblock_strength_effective: 0,
                    psnr_y: 99.0,
                    ssim_y: 1.0,
                    psnr_y_filter_disabled_respin: None,
                    ssim_y_filter_disabled_respin: None,
                    note: String::new(),
                },
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
        let js = serde_json::to_string(&r).unwrap();
        assert!(js.contains("\"motion_search_mode\":\"diamond\""));
        assert!(js.contains("\"mode\":\"off\"") && js.contains("\"aq\""));
        assert!(js.contains("\"frame_aq\""));
        assert!(js.contains("\"block_aq_mode\":\"off\""));
        assert!(js.contains("\"loop_filter_mode\":\"off\""));
        assert!(js.contains("\"subpel_mode\":\"off\""));
        assert!(js.contains("\"bframes_enabled\":false"));
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
                bframes_enabled: false,
                bframe_count: 0,
                alt_ref_count: 0,
                display_frame_count: 1,
                reference_frames_used: 1,
                avg_bframe_bytes: 0.0,
                avg_altref_bytes: 0.0,
                compression_ratio_displayed_vs_raw: 0.0,
                psnr_y_displayed_frames: 0.0,
                ssim_y_displayed_frames: 0.0,
                encode_seconds: 0.1,
                decode_seconds: 0.1,
                residual_entropy: "auto".to_string(),
                intra_explicit_blocks: 0,
                intra_rans_blocks: 0,
                p_explicit_chunks: 0,
                p_rans_chunks: 0,
                fr2_revision_counts: Fr2RevisionCounts::default(),
                legacy_explicit_total_payload_bytes: None,
                rc: None,
                aq: None,
                motion: None,
                deblock: DeblockBenchReport::default(),
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
                    bframes_enabled: false,
                    bframe_count: 0,
                    alt_ref_count: 0,
                    display_frame_count: 1,
                    reference_frames_used: 1,
                    avg_bframe_bytes: 0.0,
                    avg_altref_bytes: 0.0,
                    compression_ratio_displayed_vs_raw: 0.0,
                    psnr_y_displayed_frames: 40.0,
                    ssim_y_displayed_frames: 1.0,
                    encode_seconds: 0.1,
                    decode_seconds: 0.1,
                    residual_entropy: "explicit".to_string(),
                    intra_explicit_blocks: 1,
                    intra_rans_blocks: 0,
                    p_explicit_chunks: 0,
                    p_rans_chunks: 0,
                    fr2_revision_counts: Fr2RevisionCounts::default(),
                    legacy_explicit_total_payload_bytes: None,
                    rc: None,
                    aq: None,
                    motion: None,
                    deblock: DeblockBenchReport::default(),
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
                aq: "off".to_string(),
                motion_search: "exhaustive-small".to_string(),
                sweep_variant: None,
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
                    bframes_enabled: false,
                    bframe_count: 0,
                    alt_ref_count: 0,
                    display_frame_count: 2,
                    reference_frames_used: 1,
                    avg_bframe_bytes: 0.0,
                    avg_altref_bytes: 0.0,
                    compression_ratio_displayed_vs_raw: 0.0,
                    psnr_y_displayed_frames: 30.0,
                    ssim_y_displayed_frames: 0.9,
                    encode_seconds: 0.2,
                    decode_seconds: 0.2,
                    residual_entropy: "auto".to_string(),
                    intra_explicit_blocks: 0,
                    intra_rans_blocks: 0,
                    p_explicit_chunks: 0,
                    p_rans_chunks: 0,
                    fr2_revision_counts: Fr2RevisionCounts::default(),
                    legacy_explicit_total_payload_bytes: None,
                    rc: None,
                    aq: None,
                    motion: None,
                    deblock: DeblockBenchReport::default(),
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
            aq: "off".to_string(),
            aq_strength: 4,
            motion_search: "exhaustive-small".to_string(),
            early_exit_sad_threshold: 0,
            enable_skip_blocks: true,
            sweep_extended: false,
            loop_filter: "off".to_string(),
            deblock_strength: 0,
            subpel: "off".to_string(),
            subpel_refinement_radius: 1,
            block_aq: "off".to_string(),
            block_aq_delta_min: -6,
            block_aq_delta_max: 6,
            bframes: 0,
            alt_ref: "off".to_string(),
            gop: 0,
            reference_frames: 1,
        };
        assert!(validate_args(&a).is_err());
        a.quality = Some(22);
        assert!(validate_args(&a).is_ok());
    }

    #[test]
    fn invalid_aq_motion_strings_fail_validate() {
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
            rc: "fixed-qp".to_string(),
            quality: None,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit: 2,
            aq: "not-a-mode".to_string(),
            aq_strength: 4,
            motion_search: "exhaustive-small".to_string(),
            early_exit_sad_threshold: 0,
            enable_skip_blocks: true,
            sweep_extended: false,
            loop_filter: "off".to_string(),
            deblock_strength: 0,
            subpel: "off".to_string(),
            subpel_refinement_radius: 1,
            block_aq: "off".to_string(),
            block_aq_delta_min: -6,
            block_aq_delta_max: 6,
            bframes: 0,
            alt_ref: "off".to_string(),
            gop: 0,
            reference_frames: 1,
        };
        assert!(validate_args(&a).is_err());
        a.aq = "off".to_string();
        a.motion_search = "turbo-fast".to_string();
        assert!(validate_args(&a).is_err());
        a.motion_search = "exhaustive-small".to_string();
        a.block_aq = "maybe-later".to_string();
        assert!(validate_args(&a).is_err());
    }
}
