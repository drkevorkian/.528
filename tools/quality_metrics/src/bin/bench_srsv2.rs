//! Benchmark SRSV2 core encoder/decoder on raw YUV420p8 input.
//!
//! Optional external comparison: `--compare-x264` uses `ffmpeg` + `libx264` when available.

use std::collections::{BTreeMap, BTreeSet};
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
    encode_yuv420_b_payload_mb_blend, encode_yuv420_inter_payload, BFrameEncodeStats, PixelFormat,
    PreviousFrameRcStats, ResidualEncodeStats, ResidualEntropy, SrsV2AdaptiveQuantizationMode,
    SrsV2AqEncodeStats, SrsV2BMotionSearchMode, SrsV2BlockAqMode, SrsV2EncodeSettings,
    SrsV2EntropyModelMode, SrsV2InterPartitionMode, SrsV2InterSyntaxMode, SrsV2LoopFilterMode,
    SrsV2MotionEncodeStats, SrsV2MotionSearchMode, SrsV2PartitionCostModel,
    SrsV2PartitionMapEncoding, SrsV2RateControlMode, SrsV2RateController, SrsV2RdoMode,
    SrsV2ReferenceManager, SrsV2SubpelMode, SrsV2TransformSizeMode, VideoSequenceHeaderV2,
    YuvFrame,
};
use quality_metrics::{compression_ratio, psnr_u8, ssim_u8_simple};
use serde::Serialize;

#[derive(Parser, Debug, Clone)]
#[command(name = "bench_srsv2")]
struct Args {
    #[arg(long, default_value = "__progress_input_required__.yuv")]
    input: PathBuf,
    #[arg(long)]
    #[arg(default_value_t = 0)]
    width: u32,
    #[arg(long)]
    #[arg(default_value_t = 0)]
    height: u32,
    #[arg(long)]
    #[arg(default_value_t = 0)]
    frames: u32,
    #[arg(long)]
    #[arg(default_value_t = 0)]
    fps: u32,
    #[arg(long, default_value_t = 28)]
    qp: u8,
    #[arg(long, default_value_t = 30)]
    keyint: u32,
    #[arg(long, default_value_t = 16)]
    motion_radius: i16,
    #[arg(long, default_value = "__progress_report_required__.json")]
    report_json: PathBuf,
    #[arg(long, default_value = "__progress_report_required__.md")]
    report_md: PathBuf,
    #[arg(long, default_value_t = false)]
    compare_x264: bool,
    /// Target-bitrate matching vs x264 (**not implemented** — benchmark exits with an error when set).
    #[arg(long, default_value_t = false)]
    match_x264_bitrate: bool,
    #[arg(long, default_value_t = false)]
    compare_b_modes: bool,
    /// Experimental weighted B prediction (`FR2` rev **14** wire when combined with motion that selects weighted MBs).
    #[arg(long, default_value_t = false)]
    b_weighted_prediction: bool,
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

    /// Full SRSV2 quality/bitrate matrix (QP × inter × entropy × partition cost × partition mode); writes `--report-json` and `--report-md` (in-process encoder only).
    #[arg(long, default_value_t = false)]
    sweep_quality_bitrate: bool,

    /// SSIM-Y threshold in (0, 1] for Pareto “smallest bytes with SSIM ≥ threshold”.
    #[arg(long, default_value = "0.95")]
    sweep_ssim_threshold: f64,

    /// Total compressed-byte budget for Pareto “best SSIM / PSNR under budget”.
    #[arg(long, default_value = "10000000")]
    sweep_byte_budget: u64,

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

    /// Experimental B frames per GOP interior: **`0`** (default) = legacy I/P-only bench; **`1`** = *I₀,P₂,B₁,…* decode-order GOP (requires **`--reference-frames ≥ 2`**, **`--frames ≥ 3`**, 16-aligned size). **`> 1`** is rejected in this slice.
    #[arg(long, default_value_t = 0)]
    bframes: u32,

    /// Experimental alt-ref refresh after keyframes: `off` (default) or `on`.
    #[arg(long, default_value = "off")]
    alt_ref: String,

    /// B-frame integer motion (`FR2` rev **13**): `off` (default), `reuse-p`, or `independent-forward-backward`.
    #[arg(long, default_value = "off")]
    b_motion_search: String,

    /// Reserved GOP hint for future bench modes (currently unused).
    #[arg(long, default_value_t = 0)]
    gop: u32,

    /// SRSV2 sequence `max_ref_frames` (**default `1`** — unchanged vs historical bench).
    #[arg(long, default_value_t = 1)]
    reference_frames: u8,

    /// Experimental inter MV/header syntax for **P** (`FR2` 15/17) and **B** mb-blend (`FR2` 16/18): `raw`, `compact`, `entropy`.
    #[arg(long, default_value = "raw")]
    inter_syntax: String,

    /// Experimental fast RDO (heuristic λ×estimated-bits mode selection; `off` preserves legacy decisions): `off` or `fast`.
    #[arg(long, default_value = "off")]
    rdo: String,

    /// Fixed-point λ scale for `--rdo fast` (**256 ≈ 1.0**).
    #[arg(long, default_value_t = 256)]
    rdo_lambda_scale: u16,

    /// Run **raw**, **compact**, and **entropy** inter-syntax passes; failed modes get error rows (entropy does not abort raw/compact).
    #[arg(long, default_value_t = false)]
    compare_inter_syntax: bool,

    /// Run **`--rdo off`** vs **`--rdo fast`** with other settings unchanged.
    #[arg(long, default_value_t = false)]
    compare_rdo: bool,

    /// Compare **fixed16x16**, **split8x8**, and **auto-fast** inter partitions (uses **`--inter-syntax compact`** for each row).
    #[arg(long, default_value_t = false)]
    compare_partitions: bool,

    /// Compare partition cost models: **fixed16x16**, **split8x8**, **auto-fast** × (**sad-only**, **header-aware**, **rdo-fast**).
    #[arg(long, default_value_t = false)]
    compare_partition_costs: bool,

    /// AutoFast partition RD: **`sad-only`** (legacy default), **`header-aware`**, **`rdo-fast`**.
    #[arg(long, default_value = "sad-only")]
    partition_cost_model: String,

    /// Partition map on wire: **`legacy`** (one byte/MB) or **`rle`** when smaller.
    #[arg(long, default_value = "legacy")]
    partition_map_encoding: String,

    /// Inter macroblock partition (**default** fixed16x16). Non-default modes require **`--inter-syntax compact`** or **`entropy`**.
    #[arg(long, default_value = "fixed16x16")]
    inter_partition: String,

    /// Transform size for partitioned **P** payloads: **`auto`**, **`tx4x4`**, **`tx8x8`**.
    #[arg(long, default_value = "auto")]
    transform_size: String,

    /// MV rANS entropy model when **`--inter-syntax entropy`**: **`static`** (**StaticV1**, default) or **`context`** (**ContextV1**). **`context`** without **`--inter-syntax entropy`** is rejected at validation.
    #[arg(long, default_value = "static")]
    entropy_model: String,

    /// Run **StaticV1** then **ContextV1** in one report (**requires** **`--inter-syntax entropy`**). Emits two JSON rows + `entropy_model_compare_summary` (Δ total bytes / Δ MV section). If **ContextV1** fails, its row records `entropy_failure_reason`; **StaticV1** still runs. No FFmpeg.
    #[arg(long, default_value_t = false)]
    compare_entropy_models: bool,

    /// Build `var/bench/srsv2_h264_progress_summary.{json,md}` from existing bench JSON artifacts (**no encode**; other compare flags must be off).
    #[arg(long, default_value_t = false)]
    h264_progress_summary: bool,

    /// **Required** with `--h264-progress-summary`: `--compare-entropy-models` JSON (`compare_entropy_models[]`).
    #[arg(
        long = "entropy-models-json",
        alias = "progress-entropy-json",
        value_name = "PATH",
        default_value = "var/bench/compare_entropy_models.json"
    )]
    progress_entropy_json: PathBuf,

    /// **Required** with `--h264-progress-summary`: `--compare-partition-costs` JSON (`compare_partition_costs[]`).
    #[arg(
        long = "partition-costs-json",
        alias = "progress-partition-costs-json",
        value_name = "PATH",
        default_value = "var/bench/compare_partition_costs.json"
    )]
    progress_partition_costs_json: PathBuf,

    /// **Required** with `--h264-progress-summary`: `--sweep-quality-bitrate` JSON (`rows[]`, `pareto`).
    #[arg(
        long = "sweep-quality-bitrate-json",
        alias = "progress-sweep-json",
        value_name = "PATH",
        default_value = "var/bench/sweep_quality_bitrate.json"
    )]
    progress_sweep_json: PathBuf,

    /// Optional with `--h264-progress-summary`: **`bench_srsv2`** JSON that included **`--compare-x264`** (`table[]`).
    /// If missing or unreadable, the summary adds a warning and question 5 stays unanswered.
    #[arg(
        long = "compare-x264-json",
        alias = "progress-x264-json",
        value_name = "PATH"
    )]
    progress_x264_json: Option<PathBuf>,

    /// Optional with `--h264-progress-summary`: `--compare-b-modes` JSON (`compare_b_modes[]`).
    /// If missing or unreadable, the summary adds a warning; question 4 may be partial.
    #[arg(
        long = "compare-b-modes-json",
        alias = "progress-b-modes-json",
        value_name = "PATH"
    )]
    progress_b_modes_json: Option<PathBuf>,

    #[arg(
        long = "progress-summary-json",
        alias = "h264-progress-summary-out-json",
        value_name = "PATH",
        default_value = "var/bench/srsv2_h264_progress_summary.json"
    )]
    h264_progress_summary_out_json: PathBuf,

    #[arg(
        long = "progress-summary-md",
        alias = "h264-progress-summary-out-md",
        value_name = "PATH",
        default_value = "var/bench/srsv2_h264_progress_summary.md"
    )]
    h264_progress_summary_out_md: PathBuf,
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
    rev13: u32,
    rev14: u32,
    rev15: u32,
    rev16: u32,
    rev17: u32,
    rev18: u32,
    rev19: u32,
    rev20: u32,
    rev21: u32,
    rev22: u32,
    rev23: u32,
    rev24: u32,
    rev25: u32,
    rev26: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
struct BBlendBenchReport {
    b_forward_macroblocks: u64,
    b_backward_macroblocks: u64,
    b_average_macroblocks: u64,
    b_weighted_macroblocks: u64,
    b_sad_evaluations: u64,
    b_subpel_blocks_tested: u64,
    b_subpel_blocks_selected: u64,
    b_additional_subpel_evaluations: u64,
    b_avg_fractional_mv_qpel: f64,
    b_forward_halfpel_blocks: u64,
    b_backward_halfpel_blocks: u64,
    b_weighted_candidates_tested: u64,
    b_weighted_avg_weight_a: f64,
    b_weighted_avg_weight_b: f64,
}

#[derive(Debug, Clone, Serialize)]
struct CompareBModesEntry {
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    row: CodecRow,
    fr2_revision_counts: Fr2RevisionCounts,
    b_blend: BBlendBenchReport,
    keyframes: u32,
    pframes: u32,
    bframe_packets: u32,
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
    /// Experimental B-frame integer ME mode (`FR2` rev **13** bench).
    b_motion_search_mode: String,
    /// Sum over **P** frames: macroblocks where **zero MV** beat ME under **RdoFast** λ·MV-bytes.
    #[serde(default)]
    rdo_inter_zero_mv_wins: u64,
    /// Sum over **P** frames: macroblocks where **ME MV** was kept after **RdoFast**.
    #[serde(default)]
    rdo_inter_me_mv_wins: u64,
}

/// Aggregated partition / transform counters from **`SrsV2MotionEncodeStats::partition`** over **P** frames.
#[derive(Debug, Clone, Serialize, Default)]
struct PartitionBenchSummary {
    inter_partition_mode: String,
    transform_size_mode: String,
    /// [`SrsV2PartitionCostModel`] CLI label when relevant (**AutoFast**); empty otherwise.
    #[serde(default)]
    partition_cost_model: String,
    partition_16x16_count: u64,
    partition_16x8_count: u64,
    partition_8x16_count: u64,
    partition_8x8_count: u64,
    transform_4x4_count: u64,
    transform_8x8_count: u64,
    transform_16x16_count: u64,
    /// Same as **`transform_4x4_count`** (explicit telemetry label).
    #[serde(default)]
    transform_decision_tx4x4: u64,
    /// Same as **`transform_8x8_count`**.
    #[serde(default)]
    transform_decision_tx8x8: u64,
    partition_header_bytes: u64,
    #[serde(default)]
    partition_map_bytes: u64,
    partition_mv_bytes: u64,
    partition_residual_bytes: u64,
    /// Sum of **`rdo_partition_candidates_tested`** over **P** frames (same as candidates tested).
    rdo_partition_candidates_tested: u64,
    #[serde(default)]
    partition_rejected_by_header_cost: u64,
    #[serde(default)]
    partition_rejected_by_rdo: u64,
    #[serde(default)]
    avg_partition_sad_gain: f64,
    /// Reserved until byte deltas are tracked end-to-end per MB.
    #[serde(default)]
    avg_partition_byte_delta: f64,
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
    /// CLI / settings mirror (`raw` / `compact` / `entropy`).
    inter_syntax_mode: String,
    /// CLI / settings mirror (`off` / `fast`).
    rdo_mode: String,
    rdo_lambda_scale: u16,
    #[serde(default)]
    mv_prediction_mode: String,
    #[serde(default)]
    mv_raw_bytes_estimate: u64,
    #[serde(default)]
    mv_compact_bytes: u64,
    #[serde(default)]
    mv_entropy_bytes: u64,
    #[serde(default)]
    mv_delta_zero_count: u64,
    #[serde(default)]
    mv_delta_nonzero_count: u64,
    #[serde(default)]
    mv_delta_avg_abs: f64,
    #[serde(default)]
    inter_header_bytes: u64,
    #[serde(default)]
    inter_residual_bytes: u64,
    #[serde(default)]
    rdo_candidates_tested: u64,
    #[serde(default)]
    rdo_skip_decisions: u64,
    #[serde(default)]
    rdo_forward_decisions: u64,
    #[serde(default)]
    rdo_backward_decisions: u64,
    #[serde(default)]
    rdo_average_decisions: u64,
    #[serde(default)]
    rdo_weighted_decisions: u64,
    #[serde(default)]
    rdo_halfpel_decisions: u64,
    #[serde(default)]
    rdo_residual_decisions: u64,
    #[serde(default)]
    rdo_no_residual_decisions: u64,
    /// **RdoFast** P-frame MV: macroblocks where **zero MV** beat ME (λ·MV side cost).
    #[serde(default)]
    rdo_inter_zero_mv_wins: u64,
    /// **RdoFast** P-frame MV: macroblocks where **ME MV** was chosen.
    #[serde(default)]
    rdo_inter_me_mv_wins: u64,
    #[serde(default)]
    estimated_bits_used_for_decision: u64,
    legacy_explicit_total_payload_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rc: Option<RcBenchReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aq: Option<AqBenchReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    motion: Option<MotionBenchReport>,
    deblock: DeblockBenchReport,
    bframes_requested: u32,
    bframes_used: u32,
    decode_order_frame_indices: Vec<u32>,
    display_order_frame_indices: Vec<u32>,
    p_anchor_count: u32,
    avg_p_anchor_bytes: f64,
    #[serde(default)]
    b_blend: BBlendBenchReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    unsupported_bframe_reason: Option<String>,
    bframe_mode_note: String,
    bframe_psnr_y: f64,
    bframe_ssim_y: f64,
    /// Displayable packets decoded, in bitstream/decode order (`decode_order_frame_indices.len()`).
    decode_order_count: u32,
    #[serde(default)]
    partition: PartitionBenchSummary,
    /// CLI / settings mirror: **`static`** or **`context`** when inter entropy is used; else **`static`**.
    #[serde(default)]
    entropy_model_mode: String,
    /// Aggregated on-wire MV entropy section bytes (**sym_count + blob_len + blob**) for **P**/**B** when **`StaticV1`** + **`EntropyV1`**; else **0**.
    #[serde(default)]
    static_mv_bytes: u64,
    /// Same metric when **`ContextV1`** + **`EntropyV1`**; else **0**.
    #[serde(default)]
    context_mv_bytes: u64,
    /// Sum of per-frame **`SrsV2InterMvBenchStats::entropy_context_count`** (ContextV1: one label per MV byte).
    #[serde(default)]
    entropy_context_count: u64,
    /// Sum of per-frame **`SrsV2InterMvBenchStats::entropy_symbol_count`** (MV compact bytes under **EntropyV1**).
    #[serde(default)]
    entropy_symbol_count: u64,
    /// Populated when a benchmark pass fails before **`Srsv2Details`** is complete (reserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entropy_failure_reason: Option<String>,
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
            inter_syntax_mode: "raw".to_string(),
            rdo_mode: "off".to_string(),
            rdo_lambda_scale: 256,
            mv_prediction_mode: String::new(),
            mv_raw_bytes_estimate: 0,
            mv_compact_bytes: 0,
            mv_entropy_bytes: 0,
            mv_delta_zero_count: 0,
            mv_delta_nonzero_count: 0,
            mv_delta_avg_abs: 0.0,
            inter_header_bytes: 0,
            inter_residual_bytes: 0,
            rdo_candidates_tested: 0,
            rdo_skip_decisions: 0,
            rdo_forward_decisions: 0,
            rdo_backward_decisions: 0,
            rdo_average_decisions: 0,
            rdo_weighted_decisions: 0,
            rdo_halfpel_decisions: 0,
            rdo_residual_decisions: 0,
            rdo_no_residual_decisions: 0,
            rdo_inter_zero_mv_wins: 0,
            rdo_inter_me_mv_wins: 0,
            estimated_bits_used_for_decision: 0,
            legacy_explicit_total_payload_bytes: None,
            rc: None,
            aq: None,
            motion: None,
            deblock: DeblockBenchReport::default(),
            bframes_requested: 0,
            bframes_used: 0,
            decode_order_frame_indices: Vec::new(),
            display_order_frame_indices: Vec::new(),
            p_anchor_count: 0,
            avg_p_anchor_bytes: 0.0,
            b_blend: BBlendBenchReport::default(),
            unsupported_bframe_reason: None,
            bframe_mode_note: String::new(),
            bframe_psnr_y: 0.0,
            bframe_ssim_y: 0.0,
            decode_order_count: 0,
            partition: PartitionBenchSummary::default(),
            entropy_model_mode: "static".to_string(),
            static_mv_bytes: 0,
            context_mv_bytes: 0,
            entropy_context_count: 0,
            entropy_symbol_count: 0,
            entropy_failure_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct EntropyModelCompareEntry {
    entropy_model_mode: String,
    context_mv_bytes: u64,
    static_mv_bytes: u64,
    mv_delta_zero_count: u64,
    mv_delta_nonzero_count: u64,
    mv_delta_avg_abs: f64,
    entropy_context_count: u64,
    entropy_symbol_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    entropy_failure_reason: Option<String>,
    fr2_revision_counts: Fr2RevisionCounts,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    row: CodecRow,
    details: Srsv2Details,
}

#[derive(Debug, Clone, Serialize)]
struct X264Details {
    status: String,
    bytes: Option<u64>,
    encode_seconds: Option<f64>,
    decode_seconds: Option<f64>,
    psnr_y: Option<f64>,
    ssim_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ffmpeg_command: Option<String>,
    x264_preset: String,
    x264_crf: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    achieved_bitrate_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    srsv2_bitrate_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_x264_bitrate_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct VariantBenchEntry {
    label: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    row: CodecRow,
    details: Srsv2Details,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_b_modes: Option<Vec<CompareBModesEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_inter_syntax: Option<Vec<VariantBenchEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_rdo: Option<Vec<VariantBenchEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_partitions: Option<Vec<VariantBenchEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_partition_costs: Option<Vec<VariantBenchEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_entropy_models: Option<Vec<EntropyModelCompareEntry>>,
    /// When `--compare-entropy-models`: Δ-bytes verdict (StaticV1 vs ContextV1 total and MV section).
    #[serde(skip_serializing_if = "Option::is_none")]
    entropy_model_compare_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_x264_bitrate_note: Option<String>,
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
    if args.h264_progress_summary {
        let conflict = args.sweep
            || args.sweep_quality_bitrate
            || args.compare_residual_modes
            || args.compare_b_modes
            || args.compare_inter_syntax
            || args.compare_rdo
            || args.compare_partitions
            || args.compare_partition_costs
            || args.compare_entropy_models
            || args.compare_x264
            || args.match_x264_bitrate;
        if conflict {
            bail!(
                "--h264-progress-summary only reads JSON inputs; disable sweep/compare/x264 encode flags."
            );
        }
        for (flag, path) in [
            (
                "--entropy-models-json",
                args.progress_entropy_json.as_path(),
            ),
            (
                "--partition-costs-json",
                args.progress_partition_costs_json.as_path(),
            ),
            (
                "--sweep-quality-bitrate-json",
                args.progress_sweep_json.as_path(),
            ),
        ] {
            if !path.is_file() {
                bail!(
                    "{flag}: required progress-summary input is missing or not a file ({}). \
Provide paths from a prior `bench_srsv2` compare/sweep run.",
                    path.display()
                );
            }
        }
        return Ok(());
    }
    if args.input.as_os_str() == "__progress_input_required__.yuv" {
        bail!("--input is required unless --h264-progress-summary is set");
    }
    if args.report_json.as_os_str() == "__progress_report_required__.json" {
        bail!("--report-json is required unless --h264-progress-summary is set");
    }
    if args.report_md.as_os_str() == "__progress_report_required__.md" {
        bail!("--report-md is required unless --h264-progress-summary is set");
    }
    if args.match_x264_bitrate {
        bail!("bitrate matching is not implemented; use sweeps or target bitrate mode.");
    }
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
    if args.compare_residual_modes && (args.sweep || args.sweep_quality_bitrate) {
        bail!(
            "--compare-residual-modes cannot be combined with --sweep or --sweep-quality-bitrate"
        );
    }
    if args.sweep && args.sweep_quality_bitrate {
        bail!("--sweep and --sweep-quality-bitrate are mutually exclusive");
    }
    let compare_pass_count = args.compare_inter_syntax as u8
        + args.compare_rdo as u8
        + args.compare_partitions as u8
        + args.compare_partition_costs as u8
        + args.compare_entropy_models as u8;
    if compare_pass_count > 1 {
        bail!(
            "--compare-inter-syntax, --compare-rdo, --compare-partitions, --compare-partition-costs, and --compare-entropy-models are mutually exclusive"
        );
    }
    if (args.compare_inter_syntax
        || args.compare_rdo
        || args.compare_partitions
        || args.compare_partition_costs
        || args.compare_entropy_models)
        && (args.sweep
            || args.sweep_quality_bitrate
            || args.compare_residual_modes
            || args.compare_b_modes)
    {
        bail!("comparison modes cannot be combined with --sweep, --sweep-quality-bitrate, --compare-residual-modes, or --compare-b-modes");
    }
    if args.compare_b_modes
        && (args.sweep || args.compare_residual_modes || args.sweep_quality_bitrate)
    {
        bail!("--compare-b-modes cannot be combined with --sweep, --sweep-quality-bitrate, or --compare-residual-modes");
    }
    if args.compare_b_modes {
        if args.reference_frames < 2 {
            bail!("--compare-b-modes requires --reference-frames >= 2");
        }
        if args.frames < 3 {
            bail!("--compare-b-modes requires --frames >= 3");
        }
        if !args.width.is_multiple_of(16) || !args.height.is_multiple_of(16) {
            bail!("--compare-b-modes requires width and height divisible by 16");
        }
    }
    parse_aq_mode(&args.aq)?;
    parse_motion_search_mode(&args.motion_search)?;
    parse_loop_filter(&args.loop_filter)?;
    parse_subpel_mode(&args.subpel)?;
    parse_block_aq_mode(&args.block_aq)?;
    if parse_alt_ref_flag(&args.alt_ref)? {
        bail!("alt-ref benchmark encode is not wired yet.");
    }
    parse_b_motion_search_mode(&args.b_motion_search)?;
    let inter_syn = parse_inter_syntax_mode(&args.inter_syntax)?;
    let entropy_model = parse_entropy_model(&args.entropy_model)?;
    if entropy_model == SrsV2EntropyModelMode::ContextV1
        && inter_syn != SrsV2InterSyntaxMode::EntropyV1
    {
        bail!(
            "--entropy-model context requires --inter-syntax entropy (MV rANS on compact median-predicted deltas); got `--inter-syntax {}`",
            args.inter_syntax
        );
    }
    if args.compare_entropy_models && inter_syn != SrsV2InterSyntaxMode::EntropyV1 {
        bail!(
            "--compare-entropy-models requires --inter-syntax entropy (runs StaticV1 then ContextV1 on MV rANS paths); got `--inter-syntax {}`",
            args.inter_syntax
        );
    }
    parse_rdo_mode(&args.rdo)?;
    let inter_partition_mode = parse_inter_partition_mode(&args.inter_partition)?;
    parse_transform_size_mode(&args.transform_size)?;
    if inter_partition_mode != SrsV2InterPartitionMode::Fixed16x16
        && inter_syn == SrsV2InterSyntaxMode::RawLegacy
    {
        bail!("non-default --inter-partition requires --inter-syntax compact or entropy (not raw)");
    }
    if inter_partition_mode != SrsV2InterPartitionMode::Fixed16x16 && args.bframes > 0 {
        bail!("non-default --inter-partition requires --bframes 0 in this slice");
    }
    if args.bframes > 1 {
        bail!(
            "only --bframes 0 or 1 is supported in this experimental slice (got {})",
            args.bframes
        );
    }
    if args.bframes == 1 {
        if args.reference_frames < 2 {
            bail!("--bframes 1 requires --reference-frames >= 2");
        }
        if args.frames < 3 {
            bail!("--bframes 1 requires --frames >= 3");
        }
        if !args.width.is_multiple_of(16) || !args.height.is_multiple_of(16) {
            bail!("--bframes 1 requires width and height divisible by 16");
        }
        if args.sweep || args.compare_residual_modes || args.sweep_quality_bitrate {
            bail!("--bframes 1 cannot be combined with --sweep, --sweep-quality-bitrate, or --compare-residual-modes");
        }
    }
    if args.sweep_quality_bitrate {
        quality_metrics::srsv2_sweep::validate_sweep_pareto(
            args.sweep_ssim_threshold,
            args.sweep_byte_budget,
        )
        .map_err(|e| anyhow!("{e}"))?;
    }
    parse_partition_cost_model(&args.partition_cost_model)?;
    parse_partition_map_encoding(&args.partition_map_encoding)?;
    Ok(())
}

fn parse_subpel_mode(s: &str) -> Result<SrsV2SubpelMode> {
    match s.to_ascii_lowercase().as_str() {
        "off" => Ok(SrsV2SubpelMode::Off),
        "half" => Ok(SrsV2SubpelMode::HalfPel),
        _ => Err(anyhow!("--subpel must be off or half (got {s})")),
    }
}

fn parse_b_motion_search_mode(s: &str) -> Result<SrsV2BMotionSearchMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "off" => Ok(SrsV2BMotionSearchMode::Off),
        "reuse-p" | "reusep" => Ok(SrsV2BMotionSearchMode::ReuseP),
        "independent-forward-backward" | "independent" => {
            Ok(SrsV2BMotionSearchMode::IndependentForwardBackward)
        }
        "independent-forward-backward-half" | "independent-forward-backward-half-pel" => Ok(
            SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel,
        ),
        _ => Err(anyhow!(
            "--b-motion-search must be off, reuse-p, independent-forward-backward, or independent-forward-backward-half (got {s})"
        )),
    }
}

fn b_motion_mode_cli_label(m: SrsV2BMotionSearchMode) -> &'static str {
    match m {
        SrsV2BMotionSearchMode::Off => "off",
        SrsV2BMotionSearchMode::ReuseP => "reuse-p",
        SrsV2BMotionSearchMode::IndependentForwardBackward => "independent-forward-backward",
        SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel => {
            "independent-forward-backward-half"
        }
    }
}

fn parse_inter_syntax_mode(s: &str) -> Result<SrsV2InterSyntaxMode> {
    match s.to_ascii_lowercase().as_str() {
        "raw" | "legacy" => Ok(SrsV2InterSyntaxMode::RawLegacy),
        "compact" => Ok(SrsV2InterSyntaxMode::CompactV1),
        "entropy" => Ok(SrsV2InterSyntaxMode::EntropyV1),
        _ => Err(anyhow!(
            "--inter-syntax must be raw, compact, or entropy (got {s})"
        )),
    }
}

fn parse_entropy_model(s: &str) -> Result<SrsV2EntropyModelMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "static" | "static-v1" | "staticv1" => Ok(SrsV2EntropyModelMode::StaticV1),
        "context" | "context-v1" | "contextv1" => Ok(SrsV2EntropyModelMode::ContextV1),
        _ => Err(anyhow!(
            "--entropy-model must be static or context (got {s})"
        )),
    }
}

fn entropy_model_cli_label(m: SrsV2EntropyModelMode) -> &'static str {
    match m {
        SrsV2EntropyModelMode::StaticV1 => "static",
        SrsV2EntropyModelMode::ContextV1 => "context",
    }
}

fn parse_rdo_mode(s: &str) -> Result<SrsV2RdoMode> {
    match s.to_ascii_lowercase().as_str() {
        "off" => Ok(SrsV2RdoMode::Off),
        "fast" => Ok(SrsV2RdoMode::Fast),
        _ => Err(anyhow!("--rdo must be off or fast (got {s})")),
    }
}

fn parse_inter_partition_mode(s: &str) -> Result<SrsV2InterPartitionMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "fixed16x16" | "fixed-16x16" => Ok(SrsV2InterPartitionMode::Fixed16x16),
        "split8x8" | "split-8x8" => Ok(SrsV2InterPartitionMode::Split8x8),
        "rect16x8" | "rect-16x8" => Ok(SrsV2InterPartitionMode::Rect16x8),
        "rect8x16" | "rect-8x16" => Ok(SrsV2InterPartitionMode::Rect8x16),
        "auto-fast" | "autofast" => Ok(SrsV2InterPartitionMode::AutoFast),
        _ => Err(anyhow!(
            "--inter-partition must be fixed16x16, split8x8, rect16x8, rect8x16, or auto-fast (got {s})"
        )),
    }
}

fn parse_transform_size_mode(s: &str) -> Result<SrsV2TransformSizeMode> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "auto" => Ok(SrsV2TransformSizeMode::Auto),
        "tx4x4" | "4x4" => Ok(SrsV2TransformSizeMode::Force4x4),
        "tx8x8" | "8x8" => Ok(SrsV2TransformSizeMode::Force8x8),
        _ => Err(anyhow!(
            "--transform-size must be auto, tx4x4, or tx8x8 (got {s})"
        )),
    }
}

fn parse_partition_cost_model(s: &str) -> Result<SrsV2PartitionCostModel> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "sad-only" | "sadonly" => Ok(SrsV2PartitionCostModel::SadOnly),
        "header-aware" | "headeraware" => Ok(SrsV2PartitionCostModel::HeaderAware),
        "rdo-fast" | "rdofast" => Ok(SrsV2PartitionCostModel::RdoFast),
        _ => Err(anyhow!(
            "--partition-cost-model must be sad-only, header-aware, or rdo-fast (got {s})"
        )),
    }
}

fn parse_partition_map_encoding(s: &str) -> Result<SrsV2PartitionMapEncoding> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "legacy" | "per-mb" | "legacy-per-mb" => Ok(SrsV2PartitionMapEncoding::LegacyPerMb),
        "rle" | "rle-runs" => Ok(SrsV2PartitionMapEncoding::RleRuns),
        _ => Err(anyhow!(
            "--partition-map-encoding must be legacy or rle (got {s})"
        )),
    }
}

fn partition_cost_model_cli_label(m: SrsV2PartitionCostModel) -> &'static str {
    match m {
        SrsV2PartitionCostModel::SadOnly => "sad-only",
        SrsV2PartitionCostModel::HeaderAware => "header-aware",
        SrsV2PartitionCostModel::RdoFast => "rdo-fast",
    }
}

fn inter_partition_cli_label(m: SrsV2InterPartitionMode) -> &'static str {
    match m {
        SrsV2InterPartitionMode::Fixed16x16 => "fixed16x16",
        SrsV2InterPartitionMode::Split8x8 => "split8x8",
        SrsV2InterPartitionMode::Rect16x8 => "rect16x8",
        SrsV2InterPartitionMode::Rect8x16 => "rect8x16",
        SrsV2InterPartitionMode::AutoFast => "auto-fast",
    }
}

fn transform_size_cli_label(m: SrsV2TransformSizeMode) -> &'static str {
    match m {
        SrsV2TransformSizeMode::Auto => "auto",
        SrsV2TransformSizeMode::Force4x4 => "tx4x4",
        SrsV2TransformSizeMode::Force8x8 => "tx8x8",
    }
}

fn motion_agg_add_partition(motion_agg: &mut MotionAgg, motion_frame: &SrsV2MotionEncodeStats) {
    let p = &motion_frame.partition;
    motion_agg.partition_16x16_count += p.partition_16x16_count;
    motion_agg.partition_16x8_count += p.partition_16x8_count;
    motion_agg.partition_8x16_count += p.partition_8x16_count;
    motion_agg.partition_8x8_count += p.partition_8x8_count;
    motion_agg.transform_4x4_count += p.transform_4x4_count;
    motion_agg.transform_8x8_count += p.transform_8x8_count;
    motion_agg.transform_16x16_count += p.transform_16x16_count;
    motion_agg.partition_header_bytes += p.partition_header_bytes;
    motion_agg.partition_map_bytes += p.partition_map_bytes;
    motion_agg.partition_mv_bytes += p.partition_mv_bytes;
    motion_agg.partition_residual_bytes += p.partition_residual_bytes;
    motion_agg.rdo_partition_candidates_tested += p.rdo_partition_candidates_tested;
    motion_agg.partition_rejected_by_header_cost += p.partition_rejected_by_header_cost;
    motion_agg.partition_rejected_by_rdo += p.partition_rejected_by_rdo;
    motion_agg.partition_sad_override_accum += p.partition_sad_override_accum;
    motion_agg.partition_sad_override_events += p.partition_sad_override_events;
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
    let b_motion_search_mode = parse_b_motion_search_mode(&args.b_motion_search)?;
    let inter_syntax_mode = parse_inter_syntax_mode(&args.inter_syntax)?;
    let rdo_mode = parse_rdo_mode(&args.rdo)?;
    let inter_partition_mode = parse_inter_partition_mode(&args.inter_partition)?;
    let transform_size_mode = parse_transform_size_mode(&args.transform_size)?;
    let partition_cost_model = parse_partition_cost_model(&args.partition_cost_model)?;
    let partition_map_encoding = parse_partition_map_encoding(&args.partition_map_encoding)?;
    let entropy_model_mode = parse_entropy_model(&args.entropy_model)?;
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
        b_motion_search_mode,
        b_weighted_prediction: args.b_weighted_prediction,
        inter_syntax_mode,
        rdo_mode,
        rdo_lambda_scale: args.rdo_lambda_scale,
        inter_partition_mode,
        transform_size_mode,
        partition_cost_model,
        partition_map_encoding,
        entropy_model_mode,
        ..Default::default()
    };
    s.validate_rate_control()
        .map_err(|e| anyhow!("rate-control settings: {e}"))?;
    validate_adaptive_quant_settings(&s).map_err(|e| anyhow!("adaptive quant settings: {e}"))?;
    s.validate_entropy_model_inter()
        .map_err(|e| anyhow!("MV entropy configuration: {e}"))?;
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
    mv_raw_bytes_estimate: u64,
    mv_compact_bytes: u64,
    mv_entropy_section_bytes: u64,
    entropy_context_count: u64,
    entropy_symbol_count: u64,
    mv_delta_zero_varints: u64,
    mv_delta_nonzero_varints: u64,
    mv_delta_sum_abs_components: u64,
    inter_header_bytes_p: u64,
    residual_payload_bytes_p: u64,
    rdo_candidates_tested: u64,
    rdo_skip_decisions: u64,
    rdo_forward_decisions: u64,
    rdo_backward_decisions: u64,
    rdo_average_decisions: u64,
    rdo_weighted_decisions: u64,
    rdo_halfpel_decisions: u64,
    rdo_residual_decisions: u64,
    rdo_no_residual_decisions: u64,
    rdo_inter_zero_mv_wins: u64,
    rdo_inter_me_mv_wins: u64,
    rdo_estimated_bits: u64,
    partition_16x16_count: u64,
    partition_16x8_count: u64,
    partition_8x16_count: u64,
    partition_8x8_count: u64,
    transform_4x4_count: u64,
    transform_8x8_count: u64,
    transform_16x16_count: u64,
    partition_header_bytes: u64,
    partition_mv_bytes: u64,
    partition_residual_bytes: u64,
    rdo_partition_candidates_tested: u64,
    partition_rejected_by_header_cost: u64,
    partition_rejected_by_rdo: u64,
    partition_sad_override_accum: u64,
    partition_sad_override_events: u64,
    partition_map_bytes: u64,
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
    /// `frame_index` per encoded payload (decode order).
    decode_order_frame_indices: Vec<u32>,
    /// Display order `0 .. frames-1`.
    display_order_frame_indices: Vec<u32>,
    bframes_requested: u32,
    bframes_used: u32,
    decode_order_count: u32,
    p_anchor_count: u32,
    avg_p_anchor_bytes: f64,
    b_blend_forward_macroblocks: u64,
    b_blend_backward_macroblocks: u64,
    b_blend_average_macroblocks: u64,
    b_blend_weighted_macroblocks: u64,
    b_blend_sad_evaluations: u64,
    b_blend_subpel_blocks_tested: u64,
    b_blend_subpel_blocks_selected: u64,
    b_blend_additional_subpel_evaluations: u64,
    b_blend_sum_abs_frac_qpel: u64,
    b_blend_forward_halfpel_blocks: u64,
    b_blend_backward_halfpel_blocks: u64,
    b_blend_weighted_candidates_tested: u64,
    b_blend_weighted_sum_weight_a: u64,
    b_blend_weighted_sum_weight_b: u64,
    b_blend_macroblocks_total: u64,
    bframe_psnr_y: f64,
    bframe_ssim_y: f64,
    unsupported_bframe_reason: Option<String>,
    bframe_mode_note: String,
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
            13 => c.rev13 += 1,
            14 => c.rev14 += 1,
            15 => c.rev15 += 1,
            16 => c.rev16 += 1,
            17 => c.rev17 += 1,
            18 => c.rev18 += 1,
            19 => c.rev19 += 1,
            20 => c.rev20 += 1,
            21 => c.rev21 += 1,
            22 => c.rev22 += 1,
            23 => c.rev23 += 1,
            24 => c.rev24 += 1,
            25 => c.rev25 += 1,
            26 => c.rev26 += 1,
            _ => {}
        }
    }
    c
}

fn b_blend_report_from_pass(p: &PassNumbers) -> BBlendBenchReport {
    let denom = p.b_blend_macroblocks_total.max(1);
    let avg_frac = p.b_blend_sum_abs_frac_qpel as f64 / denom as f64;
    let (w_avg_a, w_avg_b) = if p.b_blend_weighted_macroblocks > 0 {
        (
            p.b_blend_weighted_sum_weight_a as f64 / p.b_blend_weighted_macroblocks as f64,
            p.b_blend_weighted_sum_weight_b as f64 / p.b_blend_weighted_macroblocks as f64,
        )
    } else {
        (0.0, 0.0)
    };
    BBlendBenchReport {
        b_forward_macroblocks: p.b_blend_forward_macroblocks,
        b_backward_macroblocks: p.b_blend_backward_macroblocks,
        b_average_macroblocks: p.b_blend_average_macroblocks,
        b_weighted_macroblocks: p.b_blend_weighted_macroblocks,
        b_sad_evaluations: p.b_blend_sad_evaluations,
        b_subpel_blocks_tested: p.b_blend_subpel_blocks_tested,
        b_subpel_blocks_selected: p.b_blend_subpel_blocks_selected,
        b_additional_subpel_evaluations: p.b_blend_additional_subpel_evaluations,
        b_avg_fractional_mv_qpel: avg_frac,
        b_forward_halfpel_blocks: p.b_blend_forward_halfpel_blocks,
        b_backward_halfpel_blocks: p.b_blend_backward_halfpel_blocks,
        b_weighted_candidates_tested: p.b_blend_weighted_candidates_tested,
        b_weighted_avg_weight_a: w_avg_a,
        b_weighted_avg_weight_b: w_avg_b,
    }
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
        b_motion_search_mode: b_motion_mode_cli_label(settings.b_motion_search_mode).to_string(),
        rdo_inter_zero_mv_wins: m.rdo_inter_zero_mv_wins,
        rdo_inter_me_mv_wins: m.rdo_inter_me_mv_wins,
    }
}

#[derive(Clone, Copy)]
enum BenchEmitKind {
    I,
    P,
    B,
}

/// Display-order classification: **I** at `0` and every **`keyint`**; **P** on even non‑I frames;
/// **B** on odd frames when a future **I/P** anchor exists; tail odds without a future anchor become **P**.
fn classify_bench_emit_kind(fi: u32, n: u32, keyint: u32) -> BenchEmitKind {
    let ki = keyint.max(1);
    if fi == 0 || fi.is_multiple_of(ki) {
        return BenchEmitKind::I;
    }
    if fi.is_multiple_of(2) {
        return BenchEmitKind::P;
    }
    let mut has_future_anchor = false;
    for j in (fi + 1)..n {
        if j.is_multiple_of(ki) || j.is_multiple_of(2) {
            has_future_anchor = true;
            break;
        }
    }
    if has_future_anchor {
        BenchEmitKind::B
    } else {
        BenchEmitKind::P
    }
}

fn bench_kind_slice(n: u32, keyint: u32) -> Vec<BenchEmitKind> {
    (0..n)
        .map(|fi| classify_bench_emit_kind(fi, n, keyint))
        .collect()
}

fn prev_ip_anchor(kinds: &[BenchEmitKind], fi: u32) -> Option<u32> {
    (0..fi)
        .rev()
        .find(|&j| matches!(kinds[j as usize], BenchEmitKind::I | BenchEmitKind::P))
}

fn next_ip_anchor(kinds: &[BenchEmitKind], fi: u32, n: u32) -> Option<u32> {
    ((fi + 1)..n).find(|&j| matches!(kinds[j as usize], BenchEmitKind::I | BenchEmitKind::P))
}

/// Topological encode order (decode order): smallest ready **`frame_index`** first when multiple frames are ready.
fn b_gop_encode_order(n: u32, keyint: u32) -> Result<Vec<(u32, BenchEmitKind)>> {
    let kinds = bench_kind_slice(n, keyint);
    let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n as usize];
    let mut indeg = vec![0_u32; n as usize];
    for fi in 0..n {
        match kinds[fi as usize] {
            BenchEmitKind::I => {}
            BenchEmitKind::P => {
                let Some(pa) = prev_ip_anchor(&kinds, fi) else {
                    bail!("internal: P frame {fi} has no previous I/P anchor");
                };
                adj[pa as usize].push(fi);
                indeg[fi as usize] += 1;
            }
            BenchEmitKind::B => {
                let Some(ab) = prev_ip_anchor(&kinds, fi) else {
                    bail!("internal: B frame {fi} has no backward anchor");
                };
                let Some(af) = next_ip_anchor(&kinds, fi, n) else {
                    bail!("internal: B frame {fi} has no forward anchor");
                };
                adj[ab as usize].push(fi);
                adj[af as usize].push(fi);
                indeg[fi as usize] += 2;
            }
        }
    }
    let mut ready: BTreeSet<u32> = (0..n).filter(|&fi| indeg[fi as usize] == 0).collect();
    let mut out = Vec::with_capacity(n as usize);
    while let Some(u) = ready.pop_first() {
        out.push((u, kinds[u as usize]));
        for &v in &adj[u as usize] {
            indeg[v as usize] = indeg[v as usize].saturating_sub(1);
            if indeg[v as usize] == 0 {
                ready.insert(v);
            }
        }
    }
    if out.len() != n as usize {
        bail!("internal: cyclic GOP dependency (broken anchor schedule)");
    }
    Ok(out)
}

fn run_srsv2_pass(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    settings: &SrsV2EncodeSettings,
    expected_frame: usize,
) -> Result<PassNumbers> {
    if args.bframes == 1 {
        run_srsv2_numbers_b_gop(args, seq, raw, settings, expected_frame)
    } else {
        run_srsv2_numbers(args, seq, raw, settings, expected_frame)
    }
}

fn run_srsv2_numbers_b_gop(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    settings: &SrsV2EncodeSettings,
    expected_frame: usize,
) -> Result<PassNumbers> {
    let n = args.frames;
    let nu = n as usize;
    let keyint = args.keyint.max(1);
    let kinds = bench_kind_slice(n, keyint);
    let schedule = b_gop_encode_order(n, keyint)?;
    let b_fis: Vec<u32> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| matches!(k, BenchEmitKind::B))
        .map(|(i, _)| i as u32)
        .collect();

    let mut b_fwd_mb = 0_u64;
    let mut b_bwd_mb = 0_u64;
    let mut b_avg_mb = 0_u64;
    let mut b_wgt_mb = 0_u64;
    let mut b_sad_ev = 0_u64;
    let mut b_subpel_tested = 0_u64;
    let mut b_subpel_sel = 0_u64;
    let mut b_subpel_extra_ev = 0_u64;
    let mut b_sum_frac = 0_u64;
    let mut b_fwd_hp = 0_u64;
    let mut b_bwd_hp = 0_u64;
    let mut b_wgt_cand = 0_u64;
    let mut b_wgt_sa = 0_u64;
    let mut b_wgt_sb = 0_u64;
    let mut b_mb_total = 0_u64;

    let mut rc =
        SrsV2RateController::new(settings, args.fps.max(1), 1).map_err(|e| anyhow!("{e}"))?;
    let t0 = Instant::now();
    let mut mgr = SrsV2ReferenceManager::new(seq.max_ref_frames).map_err(|e| anyhow!("{e}"))?;
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut byte_hist: Vec<u64> = Vec::new();
    let mut qp_hist: Vec<u8> = Vec::new();
    let mut psnr_src_frame: Vec<Option<u32>> = Vec::new();
    let mut enc_stats = ResidualEncodeStats::default();
    let mut prev: Option<PreviousFrameRcStats> = None;
    let mut aq_last = SrsV2AqEncodeStats::default();
    let mut motion_agg = MotionAgg::default();
    let mut anchor_p_payload_bytes: Vec<u64> = Vec::new();

    for &(fi, kind) in &schedule {
        let qp = rc.qp_for_frame(fi, prev);
        let frame = load_yuv420_frame(raw, expected_frame, fi, args.width, args.height)?;
        let mut aq_frame = SrsV2AqEncodeStats::default();
        let mut motion_frame = SrsV2MotionEncodeStats::default();
        let payload = match kind {
            BenchEmitKind::I => encode_yuv420_inter_payload(
                seq,
                &frame,
                None,
                fi,
                qp,
                settings,
                Some(&mut enc_stats),
                Some(&mut aq_frame),
                Some(&mut motion_frame),
            )
            .map_err(|e| anyhow!("SRSV2 encode I {fi}: {e}"))?,
            BenchEmitKind::P => {
                let pl = encode_yuv420_inter_payload(
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
                .map_err(|e| anyhow!("SRSV2 encode P {fi}: {e}"))?;
                anchor_p_payload_bytes.push(pl.len() as u64);
                let is_p_wire = matches!(
                    pl.get(3).copied(),
                    Some(2 | 4 | 5 | 6 | 8 | 9 | 15 | 17 | 19 | 20 | 23 | 25)
                );
                if is_p_wire {
                    motion_agg.sad_evaluations += motion_frame.sad_evaluations;
                    motion_agg.skip_subblocks += motion_frame.skip_subblocks;
                    motion_agg.nonzero_motion_macroblocks +=
                        motion_frame.nonzero_motion_macroblocks as u64;
                    motion_agg.sum_mv_l1 += motion_frame.sum_mv_l1;
                    motion_agg.p_frames += 1;
                    let mb = (seq.width / 16) as u64 * (seq.height / 16) as u64;
                    motion_agg.p_macroblocks_total += mb;
                    motion_agg.subpel_blocks_tested += motion_frame.subpel_blocks_tested;
                    motion_agg.subpel_blocks_selected += motion_frame.subpel_blocks_selected;
                    motion_agg.additional_subpel_evaluations +=
                        motion_frame.additional_subpel_evaluations;
                    motion_agg.sum_abs_frac_qpel += motion_frame.sum_abs_frac_qpel;
                    let im = &motion_frame.inter_mv;
                    motion_agg.mv_raw_bytes_estimate += im.mv_raw_bytes_estimate;
                    motion_agg.mv_compact_bytes += im.mv_compact_bytes;
                    motion_agg.mv_entropy_section_bytes += im.mv_entropy_section_bytes;
                    motion_agg.entropy_context_count += im.entropy_context_count;
                    motion_agg.entropy_symbol_count += im.entropy_symbol_count;
                    motion_agg.mv_delta_zero_varints += im.mv_delta_zero_varints;
                    motion_agg.mv_delta_nonzero_varints += im.mv_delta_nonzero_varints;
                    motion_agg.mv_delta_sum_abs_components += im.mv_delta_sum_abs_components;
                    motion_agg.inter_header_bytes_p += im.inter_header_bytes;
                    motion_agg.residual_payload_bytes_p += im.residual_payload_bytes;
                    let rd = &motion_frame.rdo;
                    motion_agg.rdo_candidates_tested += rd.candidates_tested;
                    motion_agg.rdo_skip_decisions += rd.skip_decisions;
                    motion_agg.rdo_residual_decisions += rd.residual_decisions;
                    motion_agg.rdo_no_residual_decisions += rd.no_residual_decisions;
                    motion_agg.rdo_inter_zero_mv_wins += rd.inter_zero_mv_wins;
                    motion_agg.rdo_inter_me_mv_wins += rd.inter_me_mv_wins;
                    motion_agg.rdo_estimated_bits += rd.estimated_bits_used_for_decision;
                    motion_agg_add_partition(&mut motion_agg, &motion_frame);
                }
                pl
            }
            BenchEmitKind::B => {
                let ref_a = mgr
                    .frame_at_slot_index(1)
                    .map_err(|e| anyhow!("B backward ref: {e}"))?;
                let ref_b = mgr
                    .frame_at_slot_index(0)
                    .map_err(|e| anyhow!("B forward ref: {e}"))?;
                let mut st = BFrameEncodeStats::default();
                let pl = encode_yuv420_b_payload_mb_blend(
                    seq, &frame, ref_a, ref_b, fi, qp, 1, 0, settings, &mut st,
                )
                .map_err(|e| anyhow!("SRSV2 encode B {fi}: {e}"))?;
                b_fwd_mb += st.b_forward_macroblocks as u64;
                b_bwd_mb += st.b_backward_macroblocks as u64;
                b_avg_mb += st.b_average_macroblocks as u64;
                b_wgt_mb += st.b_weighted_macroblocks as u64;
                b_sad_ev += st.b_sad_evaluations;
                b_subpel_tested += st.b_subpel_blocks_tested;
                b_subpel_sel += st.b_subpel_blocks_selected;
                b_subpel_extra_ev += st.b_additional_subpel_evaluations;
                b_sum_frac += st.b_sum_abs_frac_qpel;
                b_fwd_hp += st.b_forward_halfpel_blocks as u64;
                b_bwd_hp += st.b_backward_halfpel_blocks as u64;
                b_wgt_cand += st.b_weighted_candidates_tested;
                b_wgt_sa += st.b_weighted_sum_weight_a;
                b_wgt_sb += st.b_weighted_sum_weight_b;
                b_mb_total += (seq.width / 16) as u64 * (seq.height / 16) as u64;
                let rd = &st.rdo;
                motion_agg.rdo_candidates_tested += rd.candidates_tested;
                motion_agg.rdo_forward_decisions += rd.forward_decisions;
                motion_agg.rdo_backward_decisions += rd.backward_decisions;
                motion_agg.rdo_average_decisions += rd.average_decisions;
                motion_agg.rdo_weighted_decisions += rd.weighted_decisions;
                motion_agg.rdo_halfpel_decisions += rd.halfpel_decisions;
                motion_agg.rdo_estimated_bits += rd.estimated_bits_used_for_decision;
                let im = &st.inter_mv;
                motion_agg.mv_raw_bytes_estimate += im.mv_raw_bytes_estimate;
                motion_agg.mv_compact_bytes += im.mv_compact_bytes;
                motion_agg.mv_entropy_section_bytes += im.mv_entropy_section_bytes;
                motion_agg.entropy_context_count += im.entropy_context_count;
                motion_agg.entropy_symbol_count += im.entropy_symbol_count;
                motion_agg.mv_delta_zero_varints += im.mv_delta_zero_varints;
                motion_agg.mv_delta_nonzero_varints += im.mv_delta_nonzero_varints;
                motion_agg.mv_delta_sum_abs_components += im.mv_delta_sum_abs_components;
                motion_agg.inter_header_bytes_p += im.inter_header_bytes;
                motion_agg.residual_payload_bytes_p += im.residual_payload_bytes;
                pl
            }
        };

        aq_last = aq_frame;
        let is_i = matches!(payload.get(3).copied(), Some(1 | 3 | 7));
        qp_hist.push(qp);
        byte_hist.push(payload.len() as u64);
        rc.observe_frame(fi, payload.len(), is_i);
        decode_yuv420_srsv2_payload_managed(seq, &payload, &mut mgr)
            .map_err(|e| anyhow!("SRSV2 reference refresh {fi}: {e}"))?;
        payloads.push(payload);
        psnr_src_frame.push(Some(fi));
        prev = Some(PreviousFrameRcStats {
            encoded_bytes: byte_hist.last().copied().unwrap_or(0) as u32,
            is_keyframe: is_i,
        });
    }

    let enc_secs = t0.elapsed().as_secs_f64();
    let ylen = (args.width * args.height) as usize;
    let mut dec_by_frame = vec![vec![0_u8; ylen]; nu];
    let mut written = vec![false; nu];
    let mut decode_order_frame_indices: Vec<u32> = Vec::new();
    let t1 = Instant::now();
    let mut mgr_dec = SrsV2ReferenceManager::new(seq.max_ref_frames).map_err(|e| anyhow!("{e}"))?;
    for pl in &payloads {
        let dec = decode_yuv420_srsv2_payload_managed(seq, pl, &mut mgr_dec)
            .map_err(|e| anyhow!("SRSV2 decode: {e}"))?;
        if !dec.is_displayable {
            continue;
        }
        decode_order_frame_indices.push(dec.frame_index);
        let idx = dec.frame_index as usize;
        if idx >= nu {
            bail!(
                "decoded display frame_index {} out of range for --frames {}",
                dec.frame_index,
                n
            );
        }
        if written[idx] {
            bail!(
                "duplicate decoded display frame_index {} (benchmark requires unique indices)",
                dec.frame_index
            );
        }
        written[idx] = true;
        dec_by_frame[idx] = dec.yuv.y.samples.clone();
    }
    for (idx, filled) in written.iter().enumerate() {
        if !filled {
            bail!(
                "missing decoded display frame index {idx}; cannot compute display-order metrics"
            );
        }
    }

    let mut src_luma = Vec::with_capacity(ylen * nu);
    let mut dec_luma = Vec::with_capacity(ylen * nu);
    for fi in 0..n {
        src_luma.extend_from_slice(frame_luma_slice(
            raw,
            expected_frame,
            fi,
            args.width,
            args.height,
        ));
        dec_luma.extend_from_slice(&dec_by_frame[fi as usize]);
    }
    let psnr_y = psnr_u8(&src_luma, &dec_luma, 255.0)?;
    let ssim_y = avg_ssim_per_frame(&src_luma, &dec_luma, args.width, args.height, n)?;
    let dec_secs = t1.elapsed().as_secs_f64();

    let mut b_psnr_acc = 0.0_f64;
    for fi in &b_fis {
        let s = frame_luma_slice(raw, expected_frame, *fi, args.width, args.height);
        b_psnr_acc += psnr_u8(s, &dec_by_frame[*fi as usize], 255.0).map_err(|e| anyhow!("{e}"))?;
    }
    let mut b_ssim_acc = 0.0_f64;
    for fi in &b_fis {
        let s = frame_luma_slice(raw, expected_frame, *fi, args.width, args.height);
        b_ssim_acc += ssim_u8_simple(
            s,
            &dec_by_frame[*fi as usize],
            args.width as usize,
            args.height as usize,
        )
        .map_err(|e| anyhow!("{e}"))?;
    }

    let bframe_psnr_y = if b_fis.is_empty() {
        0.0
    } else {
        b_psnr_acc / b_fis.len() as f64
    };
    let bframe_ssim_y = if b_fis.is_empty() {
        0.0
    } else {
        b_ssim_acc / b_fis.len() as f64
    };

    let display_order_frame_indices: Vec<u32> = (0..n).collect();
    let p_anchor_count = anchor_p_payload_bytes.len() as u32;
    let avg_p_anchor_bytes = avg_u64(&anchor_p_payload_bytes);
    let decode_order_count = decode_order_frame_indices.len() as u32;

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
        legacy_explicit_total_payload_bytes: None,
        aq_last,
        motion_agg,
        decode_order_frame_indices,
        display_order_frame_indices,
        bframes_requested: args.bframes,
        bframes_used: 1,
        decode_order_count,
        p_anchor_count,
        avg_p_anchor_bytes,
        b_blend_forward_macroblocks: b_fwd_mb,
        b_blend_backward_macroblocks: b_bwd_mb,
        b_blend_average_macroblocks: b_avg_mb,
        b_blend_weighted_macroblocks: b_wgt_mb,
        b_blend_sad_evaluations: b_sad_ev,
        b_blend_subpel_blocks_tested: b_subpel_tested,
        b_blend_subpel_blocks_selected: b_subpel_sel,
        b_blend_additional_subpel_evaluations: b_subpel_extra_ev,
        b_blend_sum_abs_frac_qpel: b_sum_frac,
        b_blend_forward_halfpel_blocks: b_fwd_hp,
        b_blend_backward_halfpel_blocks: b_bwd_hp,
        b_blend_weighted_candidates_tested: b_wgt_cand,
        b_blend_weighted_sum_weight_a: b_wgt_sa,
        b_blend_weighted_sum_weight_b: b_wgt_sb,
        b_blend_macroblocks_total: b_mb_total,
        bframe_psnr_y,
        bframe_ssim_y,
        unsupported_bframe_reason: None,
        bframe_mode_note: "experimental --bframes 1: keyint-aware I/B/P placement; encode order is decode order (anchors before sandwiched B); FR2 rev13/14 per-MB blend/SAD + optional B ME (integer or half-pel) and optional weighted prediction; metrics computed in display order".to_string(),
    })
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
        let is_p = matches!(
            payload.get(3).copied(),
            Some(2 | 4 | 5 | 6 | 8 | 9 | 15 | 17 | 19 | 20 | 23 | 25)
        );
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
            let im = &motion_frame.inter_mv;
            motion_agg.mv_raw_bytes_estimate += im.mv_raw_bytes_estimate;
            motion_agg.mv_compact_bytes += im.mv_compact_bytes;
            motion_agg.mv_entropy_section_bytes += im.mv_entropy_section_bytes;
            motion_agg.entropy_context_count += im.entropy_context_count;
            motion_agg.entropy_symbol_count += im.entropy_symbol_count;
            motion_agg.mv_delta_zero_varints += im.mv_delta_zero_varints;
            motion_agg.mv_delta_nonzero_varints += im.mv_delta_nonzero_varints;
            motion_agg.mv_delta_sum_abs_components += im.mv_delta_sum_abs_components;
            motion_agg.inter_header_bytes_p += im.inter_header_bytes;
            motion_agg.residual_payload_bytes_p += im.residual_payload_bytes;
            let rd = &motion_frame.rdo;
            motion_agg.rdo_candidates_tested += rd.candidates_tested;
            motion_agg.rdo_skip_decisions += rd.skip_decisions;
            motion_agg.rdo_residual_decisions += rd.residual_decisions;
            motion_agg.rdo_no_residual_decisions += rd.no_residual_decisions;
            motion_agg.rdo_inter_zero_mv_wins += rd.inter_zero_mv_wins;
            motion_agg.rdo_inter_me_mv_wins += rd.inter_me_mv_wins;
            motion_agg.rdo_estimated_bits += rd.estimated_bits_used_for_decision;
            motion_agg_add_partition(&mut motion_agg, &motion_frame);
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
    }
    let enc_secs = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let ylen = (args.width * args.height) as usize;
    let nf = args.frames as usize;
    let mut dec_by_frame = vec![vec![0u8; ylen]; nf];
    let mut written = vec![false; nf];
    let mut decode_order_frame_indices: Vec<u32> = Vec::new();
    let mut mgr_dec = SrsV2ReferenceManager::new(seq.max_ref_frames)
        .map_err(|e| anyhow!("decode manager: {e}"))?;
    for pl in &payloads {
        let dec = decode_yuv420_srsv2_payload_managed(seq, pl, &mut mgr_dec)
            .map_err(|e| anyhow!("SRSV2 decode: {e}"))?;
        if !dec.is_displayable {
            continue;
        }
        decode_order_frame_indices.push(dec.frame_index);
        let idx = dec.frame_index as usize;
        if idx >= nf {
            bail!(
                "decoded frame_index {} out of range for --frames {}",
                dec.frame_index,
                args.frames
            );
        }
        if written[idx] {
            bail!("duplicate decoded display frame_index {}", dec.frame_index);
        }
        written[idx] = true;
        dec_by_frame[idx] = dec.yuv.y.samples.clone();
    }
    for (idx, filled) in written.iter().enumerate() {
        if !filled {
            bail!(
                "missing decoded display frame index {idx}; cannot compute display-order metrics"
            );
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

    let mut p_wire_bytes: Vec<u64> = Vec::new();
    for pl in &payloads {
        if matches!(
            pl.get(3).copied(),
            Some(2 | 4 | 5 | 6 | 8 | 9 | 15 | 17 | 19 | 20)
        ) {
            p_wire_bytes.push(pl.len() as u64);
        }
    }
    let display_order_frame_indices: Vec<u32> = (0..args.frames).collect();
    let decode_order_count = decode_order_frame_indices.len() as u32;

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
        decode_order_frame_indices,
        display_order_frame_indices,
        bframes_requested: args.bframes,
        bframes_used: 0,
        decode_order_count,
        p_anchor_count: p_wire_bytes.len() as u32,
        avg_p_anchor_bytes: avg_u64(&p_wire_bytes),
        b_blend_forward_macroblocks: 0,
        b_blend_backward_macroblocks: 0,
        b_blend_average_macroblocks: 0,
        b_blend_weighted_macroblocks: 0,
        b_blend_sad_evaluations: 0,
        b_blend_subpel_blocks_tested: 0,
        b_blend_subpel_blocks_selected: 0,
        b_blend_additional_subpel_evaluations: 0,
        b_blend_sum_abs_frac_qpel: 0,
        b_blend_forward_halfpel_blocks: 0,
        b_blend_backward_halfpel_blocks: 0,
        b_blend_weighted_candidates_tested: 0,
        b_blend_weighted_sum_weight_a: 0,
        b_blend_weighted_sum_weight_b: 0,
        b_blend_macroblocks_total: 0,
        bframe_psnr_y: 0.0,
        bframe_ssim_y: 0.0,
        unsupported_bframe_reason: None,
        bframe_mode_note: String::new(),
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
        let numbers_off = run_srsv2_pass(args, &seq_off, raw, settings, expected_frame)?;
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
            Some(2 | 4 | 5 | 6 | 8 | 9 | 15 | 17 | 19 | 20 | 23 | 25) => {
                p_bytes.push(pl.len() as u64)
            }
            Some(10 | 11 | 13 | 14 | 16 | 18 | 21 | 22 | 24 | 26) => b_bytes.push(pl.len() as u64),
            Some(12) => alt_bytes.push(pl.len() as u64),
            _ => {}
        }
    }
    let fr2 = fr2_revision_counts(&p.payloads);
    let bframe_count =
        fr2.rev10 + fr2.rev11 + fr2.rev13 + fr2.rev14 + fr2.rev16 + fr2.rev18 + fr2.rev24;
    let alt_ref_count = fr2.rev12;
    let enc_bytes: u64 = p.byte_hist.iter().sum();
    let raw_bytes = raw_len_for_bitrate(args);
    let m = &p.motion_agg;
    let mv_denom = (m.mv_delta_zero_varints + m.mv_delta_nonzero_varints).max(1);
    let mv_delta_avg_abs = m.mv_delta_sum_abs_components as f64 / mv_denom as f64;
    let (static_mv_b, context_mv_b) =
        if matches!(settings.inter_syntax_mode, SrsV2InterSyntaxMode::EntropyV1) {
            match settings.entropy_model_mode {
                SrsV2EntropyModelMode::StaticV1 => (m.mv_entropy_section_bytes, 0),
                SrsV2EntropyModelMode::ContextV1 => (0, m.mv_entropy_section_bytes),
            }
        } else {
            (0, 0)
        };
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
        inter_syntax_mode: args.inter_syntax.clone(),
        rdo_mode: args.rdo.clone(),
        rdo_lambda_scale: args.rdo_lambda_scale,
        mv_prediction_mode: libsrs_video::srsv2::inter_mv::MV_PREDICTION_MODE_LABEL.to_string(),
        mv_raw_bytes_estimate: m.mv_raw_bytes_estimate,
        mv_compact_bytes: m.mv_compact_bytes,
        mv_entropy_bytes: m.mv_entropy_section_bytes,
        mv_delta_zero_count: m.mv_delta_zero_varints,
        mv_delta_nonzero_count: m.mv_delta_nonzero_varints,
        mv_delta_avg_abs,
        inter_header_bytes: m.inter_header_bytes_p,
        inter_residual_bytes: m.residual_payload_bytes_p,
        rdo_candidates_tested: m.rdo_candidates_tested,
        rdo_skip_decisions: m.rdo_skip_decisions,
        rdo_forward_decisions: m.rdo_forward_decisions,
        rdo_backward_decisions: m.rdo_backward_decisions,
        rdo_average_decisions: m.rdo_average_decisions,
        rdo_weighted_decisions: m.rdo_weighted_decisions,
        rdo_halfpel_decisions: m.rdo_halfpel_decisions,
        rdo_residual_decisions: m.rdo_residual_decisions,
        rdo_no_residual_decisions: m.rdo_no_residual_decisions,
        rdo_inter_zero_mv_wins: m.rdo_inter_zero_mv_wins,
        rdo_inter_me_mv_wins: m.rdo_inter_me_mv_wins,
        estimated_bits_used_for_decision: m.rdo_estimated_bits,
        legacy_explicit_total_payload_bytes: p.legacy_explicit_total_payload_bytes,
        rc: Some(rc_report_from_pass(args, settings, p)),
        aq: Some(aq_report_from_pass(settings, &p.aq_last)),
        motion: Some(motion_report_from_pass(settings, &p.motion_agg)),
        deblock,
        bframes_requested: p.bframes_requested,
        bframes_used: p.bframes_used,
        decode_order_frame_indices: p.decode_order_frame_indices.clone(),
        display_order_frame_indices: p.display_order_frame_indices.clone(),
        decode_order_count: p.decode_order_count,
        p_anchor_count: p.p_anchor_count,
        avg_p_anchor_bytes: p.avg_p_anchor_bytes,
        b_blend: b_blend_report_from_pass(p),
        unsupported_bframe_reason: p.unsupported_bframe_reason.clone(),
        bframe_mode_note: p.bframe_mode_note.clone(),
        bframe_psnr_y: p.bframe_psnr_y,
        bframe_ssim_y: p.bframe_ssim_y,
        entropy_model_mode: entropy_model_cli_label(settings.entropy_model_mode).to_string(),
        static_mv_bytes: static_mv_b,
        context_mv_bytes: context_mv_b,
        entropy_context_count: m.entropy_context_count,
        entropy_symbol_count: m.entropy_symbol_count,
        entropy_failure_reason: None,
        partition: {
            let pcm = if matches!(
                settings.inter_partition_mode,
                SrsV2InterPartitionMode::AutoFast
            ) {
                partition_cost_model_cli_label(settings.partition_cost_model).to_string()
            } else {
                String::new()
            };
            let avg_sad_gain = if m.partition_sad_override_events > 0 {
                m.partition_sad_override_accum as f64 / m.partition_sad_override_events as f64
            } else {
                0.0
            };
            PartitionBenchSummary {
                inter_partition_mode: inter_partition_cli_label(settings.inter_partition_mode)
                    .to_string(),
                transform_size_mode: transform_size_cli_label(settings.transform_size_mode)
                    .to_string(),
                partition_cost_model: pcm,
                partition_16x16_count: m.partition_16x16_count,
                partition_16x8_count: m.partition_16x8_count,
                partition_8x16_count: m.partition_8x16_count,
                partition_8x8_count: m.partition_8x8_count,
                transform_4x4_count: m.transform_4x4_count,
                transform_8x8_count: m.transform_8x8_count,
                transform_16x16_count: m.transform_16x16_count,
                transform_decision_tx4x4: m.transform_4x4_count,
                transform_decision_tx8x8: m.transform_8x8_count,
                partition_header_bytes: m.partition_header_bytes,
                partition_map_bytes: m.partition_map_bytes,
                partition_mv_bytes: m.partition_mv_bytes,
                partition_residual_bytes: m.partition_residual_bytes,
                rdo_partition_candidates_tested: m.rdo_partition_candidates_tested,
                partition_rejected_by_header_cost: m.partition_rejected_by_header_cost,
                partition_rejected_by_rdo: m.partition_rejected_by_rdo,
                avg_partition_sad_gain: avg_sad_gain,
                avg_partition_byte_delta: 0.0,
            }
        },
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

fn compare_b_modes_err_row(label: &str, err: &str) -> CompareBModesEntry {
    CompareBModesEntry {
        mode: label.to_string(),
        error: Some(err.to_string()),
        row: CodecRow {
            codec: label.to_string(),
            error: Some(err.to_string()),
            bytes: 0,
            ratio: 0.0,
            bitrate_bps: 0.0,
            psnr_y: 0.0,
            ssim_y: 0.0,
            enc_fps: 0.0,
            dec_fps: 0.0,
        },
        fr2_revision_counts: Fr2RevisionCounts::default(),
        b_blend: BBlendBenchReport::default(),
        keyframes: 0,
        pframes: 0,
        bframe_packets: 0,
    }
}

fn compare_b_modes_try_entry(
    ef: usize,
    raw: &[u8],
    label: &str,
    row_args: Args,
) -> CompareBModesEntry {
    let re = match parse_residual_entropy(&row_args.residual_entropy) {
        Ok(r) => r,
        Err(e) => return compare_b_modes_err_row(label, &format!("{e:#}")),
    };
    let settings = match build_settings(&row_args, re) {
        Ok(s) => s,
        Err(e) => return compare_b_modes_err_row(label, &format!("{e:#}")),
    };
    let seq_r = build_seq_header(&row_args, &settings);
    let numbers = match run_srsv2_pass(&row_args, &seq_r, raw, &settings, ef) {
        Ok(n) => n,
        Err(e) => return compare_b_modes_err_row(label, &format!("{e:#}")),
    };
    let details = pass_to_details(
        &row_args,
        &seq_r,
        &settings,
        &row_args.residual_entropy,
        &numbers,
        DeblockBenchReport::default(),
    );
    let fr2 = fr2_revision_counts(&numbers.payloads);
    let b_pkts = fr2.rev10 + fr2.rev11 + fr2.rev13 + fr2.rev14 + fr2.rev16 + fr2.rev18;
    CompareBModesEntry {
        mode: label.to_string(),
        error: None,
        row: pass_to_row(&row_args, label, &numbers),
        fr2_revision_counts: fr2,
        b_blend: details.b_blend.clone(),
        keyframes: details.keyframes,
        pframes: details.pframes,
        bframe_packets: b_pkts,
    }
}

fn run_compare_b_modes_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    ef: usize,
) -> Result<BenchReport> {
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let settings = build_settings(args, re)?;
    let numbers = run_srsv2_pass(args, seq, raw, &settings, ef)?;
    let deblock = build_deblock_bench_report(args, seq, &settings, raw, ef, &numbers)?;
    let details = pass_to_details(
        args,
        seq,
        &settings,
        &args.residual_entropy,
        &numbers,
        deblock,
    );
    let srsv2_row = pass_to_row(args, "SRSV2", &numbers);
    let src_luma = flatten_src_luma(raw, ef, args)?;

    let mut rows: Vec<CompareBModesEntry> = Vec::new();

    let mut a = args.clone();
    a.bframes = 0;
    a.b_motion_search = "off".to_string();
    a.b_weighted_prediction = false;
    a.reference_frames = a.reference_frames.max(1);
    rows.push(compare_b_modes_try_entry(ef, raw, "SRSV2-P-only", a));

    let mut a = args.clone();
    a.bframes = 1;
    a.reference_frames = a.reference_frames.max(2);
    a.b_motion_search = "independent-forward-backward".to_string();
    a.b_weighted_prediction = false;
    rows.push(compare_b_modes_try_entry(ef, raw, "SRSV2-B-int", a));

    let mut a = args.clone();
    a.bframes = 1;
    a.reference_frames = a.reference_frames.max(2);
    a.b_motion_search = "independent-forward-backward-half".to_string();
    a.b_weighted_prediction = false;
    rows.push(compare_b_modes_try_entry(ef, raw, "SRSV2-B-half", a));

    let mut a = args.clone();
    a.bframes = 1;
    a.reference_frames = a.reference_frames.max(2);
    a.b_motion_search = "independent-forward-backward".to_string();
    a.b_weighted_prediction = true;
    rows.push(compare_b_modes_try_entry(ef, raw, "SRSV2-B-weighted", a));

    let mut table: Vec<CodecRow> = rows.iter().map(|r| r.row.clone()).collect();

    let x264 = if args.compare_x264 {
        let (row, det) = run_x264_compare(args, raw, ef, &src_luma, Some(srsv2_row.bitrate_bps))?;
        if let Some(r) = row {
            table.push(r);
        }
        Some(det)
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
        compare_b_modes: Some(rows),
        compare_inter_syntax: None,
        compare_rdo: None,
        compare_partitions: None,
        compare_partition_costs: None,
        compare_entropy_models: None,
        entropy_model_compare_summary: None,
        match_x264_bitrate_note: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
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
    let numbers = run_srsv2_pass(args, seq, raw, &settings, expected_frame)?;
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
        let (row, details_x264) = run_x264_compare(
            args,
            raw,
            expected_frame,
            &src_luma,
            Some(srsv2_row.bitrate_bps),
        )?;
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
        compare_b_modes: None,
        compare_inter_syntax: None,
        compare_rdo: None,
        compare_partitions: None,
        compare_partition_costs: None,
        compare_entropy_models: None,
        entropy_model_compare_summary: None,
        match_x264_bitrate_note: None,
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

        match run_srsv2_pass(args, seq, raw, &st, expected_frame) {
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
        compare_b_modes: None,
        compare_inter_syntax: None,
        compare_rdo: None,
        compare_partitions: None,
        compare_partition_costs: None,
        compare_entropy_models: None,
        entropy_model_compare_summary: None,
        match_x264_bitrate_note: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn run_compare_inter_syntax_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<BenchReport> {
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let modes = [
        ("SRSV2-raw", "raw"),
        ("SRSV2-compact", "compact"),
        ("SRSV2-entropy", "entropy"),
    ];
    let mut entries = Vec::new();
    let mut table = Vec::new();
    let mut primary: Option<Srsv2Details> = None;

    for (label, syn) in modes {
        let mut a = args.clone();
        a.inter_syntax = syn.to_string();
        let settings = match build_settings(&a, re) {
            Ok(s) => s,
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        inter_syntax_mode: syn.to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
                continue;
            }
        };

        match run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
            Ok(numbers) => {
                let row = pass_to_row(&a, label, &numbers);
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(
                    &a,
                    seq,
                    &settings,
                    &args.residual_entropy,
                    &numbers,
                    deblock,
                );
                if primary.is_none() {
                    primary = Some(details.clone());
                }
                entries.push(VariantBenchEntry {
                    label: label.to_string(),
                    ok: true,
                    error: None,
                    row: row.clone(),
                    details,
                });
                table.push(row);
            }
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        inter_syntax_mode: syn.to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
            }
        }
    }

    let srsv2 = primary.unwrap_or_else(|| entries[0].details.clone());

    Ok(BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "`--compare-inter-syntax` runs raw, compact, and entropy separately; failed variants keep error rows without aborting siblings.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        raw_bytes: raw.len() as u64,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2,
        x264: None,
        table,
        compare_residual_modes: None,
        sweep: None,
        compare_b_modes: None,
        compare_inter_syntax: Some(entries),
        compare_rdo: None,
        compare_partitions: None,
        compare_partition_costs: None,
        compare_entropy_models: None,
        entropy_model_compare_summary: None,
        match_x264_bitrate_note: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn run_compare_rdo_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<BenchReport> {
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let modes = [("SRSV2-rdo-off", "off"), ("SRSV2-rdo-fast", "fast")];
    let mut entries = Vec::new();
    let mut table = Vec::new();
    let mut primary: Option<Srsv2Details> = None;

    for (label, rdo_s) in modes {
        let mut a = args.clone();
        a.rdo = rdo_s.to_string();
        let settings = match build_settings(&a, re) {
            Ok(s) => s,
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        rdo_mode: rdo_s.to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
                continue;
            }
        };

        match run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
            Ok(numbers) => {
                let row = pass_to_row(&a, label, &numbers);
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(
                    &a,
                    seq,
                    &settings,
                    &args.residual_entropy,
                    &numbers,
                    deblock,
                );
                if primary.is_none() {
                    primary = Some(details.clone());
                }
                entries.push(VariantBenchEntry {
                    label: label.to_string(),
                    ok: true,
                    error: None,
                    row: row.clone(),
                    details,
                });
                table.push(row);
            }
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        rdo_mode: rdo_s.to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
            }
        }
    }

    let srsv2 = primary.unwrap_or_else(|| entries[0].details.clone());

    Ok(BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "`--compare-rdo` runs RDO off vs fast; counters are display-order aggregates in each row's details.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        raw_bytes: raw.len() as u64,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2,
        x264: None,
        table,
        compare_residual_modes: None,
        sweep: None,
        compare_b_modes: None,
        compare_inter_syntax: None,
        compare_rdo: Some(entries),
        compare_partitions: None,
        compare_partition_costs: None,
        compare_entropy_models: None,
        entropy_model_compare_summary: None,
        match_x264_bitrate_note: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn run_compare_partitions_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<BenchReport> {
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let modes = [
        ("SRSV2-part-fixed16x16", "fixed16x16"),
        ("SRSV2-part-split8x8", "split8x8"),
        ("SRSV2-part-auto-fast", "auto-fast"),
    ];
    let mut entries = Vec::new();
    let mut table = Vec::new();
    let mut primary: Option<Srsv2Details> = None;

    for (label, ip_s) in modes {
        let mut a = args.clone();
        a.inter_partition = ip_s.to_string();
        a.inter_syntax = "compact".to_string();
        let settings = match build_settings(&a, re) {
            Ok(s) => s,
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        inter_syntax_mode: "compact".to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
                continue;
            }
        };

        match run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
            Ok(numbers) => {
                let row = pass_to_row(&a, label, &numbers);
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(
                    &a,
                    seq,
                    &settings,
                    &args.residual_entropy,
                    &numbers,
                    deblock,
                );
                if primary.is_none() {
                    primary = Some(details.clone());
                }
                entries.push(VariantBenchEntry {
                    label: label.to_string(),
                    ok: true,
                    error: None,
                    row: row.clone(),
                    details,
                });
                table.push(row);
            }
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        inter_syntax_mode: "compact".to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
            }
        }
    }

    let srsv2 = primary.unwrap_or_else(|| entries[0].details.clone());

    Ok(BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "`--compare-partitions` runs fixed16x16, split8x8, and auto-fast with `--inter-syntax compact` for comparable MV packing; see each row's `partition` stats.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        raw_bytes: raw.len() as u64,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2,
        x264: None,
        table,
        compare_residual_modes: None,
        sweep: None,
        compare_b_modes: None,
        compare_inter_syntax: None,
        compare_rdo: None,
        compare_partitions: Some(entries),
        compare_partition_costs: None,
        compare_entropy_models: None,
        entropy_model_compare_summary: None,
        match_x264_bitrate_note: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn run_compare_partition_costs_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<BenchReport> {
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let modes = [
        (
            "SRSV2-pc-fixed16x16",
            "fixed16x16",
            SrsV2PartitionCostModel::SadOnly,
        ),
        (
            "SRSV2-pc-split8x8",
            "split8x8",
            SrsV2PartitionCostModel::SadOnly,
        ),
        (
            "SRSV2-pc-auto-fast-sad",
            "auto-fast",
            SrsV2PartitionCostModel::SadOnly,
        ),
        (
            "SRSV2-pc-auto-fast-header-aware",
            "auto-fast",
            SrsV2PartitionCostModel::HeaderAware,
        ),
        (
            "SRSV2-pc-auto-fast-rdo",
            "auto-fast",
            SrsV2PartitionCostModel::RdoFast,
        ),
    ];
    let mut entries = Vec::new();
    let mut table = Vec::new();
    let mut primary: Option<Srsv2Details> = None;

    for (label, ip_s, pcm) in modes {
        let mut a = args.clone();
        a.inter_partition = ip_s.to_string();
        a.inter_syntax = "compact".to_string();
        a.partition_cost_model = partition_cost_model_cli_label(pcm).to_string();
        let settings = match build_settings(&a, re) {
            Ok(s) => s,
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        inter_syntax_mode: "compact".to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
                continue;
            }
        };

        match run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
            Ok(numbers) => {
                let row = pass_to_row(&a, label, &numbers);
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let details = pass_to_details(
                    &a,
                    seq,
                    &settings,
                    &args.residual_entropy,
                    &numbers,
                    deblock,
                );
                if primary.is_none() {
                    primary = Some(details.clone());
                }
                entries.push(VariantBenchEntry {
                    label: label.to_string(),
                    ok: true,
                    error: None,
                    row: row.clone(),
                    details,
                });
                table.push(row);
            }
            Err(e) => {
                entries.push(VariantBenchEntry {
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
                        inter_syntax_mode: "compact".to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
            }
        }
    }

    let srsv2 = primary.unwrap_or_else(|| entries[0].details.clone());

    Ok(BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "`--compare-partition-costs` runs fixed16x16, split8x8, and three AutoFast cost models (`sad-only`, `header-aware`, `rdo-fast`) with `--inter-syntax compact`.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        raw_bytes: raw.len() as u64,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2,
        x264: None,
        table,
        compare_residual_modes: None,
        sweep: None,
        compare_b_modes: None,
        compare_inter_syntax: None,
        compare_rdo: None,
        compare_partitions: None,
        compare_partition_costs: Some(entries),
        compare_entropy_models: None,
        entropy_model_compare_summary: None,
        match_x264_bitrate_note: None,
        git_commit: git_short_hash(),
        os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

/// One-line engineering summary for JSON/Markdown when `--compare-entropy-models` produces two rows.
fn summarize_entropy_model_compare(entries: &[EntropyModelCompareEntry]) -> Option<String> {
    let st = entries.iter().find(|e| e.entropy_model_mode == "static")?;
    let ctx = entries.iter().find(|e| e.entropy_model_mode == "context")?;
    if st.ok && ctx.ok {
        let dt = ctx.row.bytes as i64 - st.row.bytes as i64;
        let d_mv = ctx.context_mv_bytes as i64 - st.static_mv_bytes as i64;
        let total_cmp = if dt < 0 {
            "On this clip, ContextV1 used fewer total SRSV2 payload bytes than StaticV1 (Δ negative)."
        } else if dt > 0 {
            "On this clip, ContextV1 used more total SRSV2 payload bytes than StaticV1 (Δ positive)."
        } else {
            "On this clip, ContextV1 and StaticV1 tied on total SRSV2 payload bytes."
        };
        let mv_cmp = if d_mv < 0 {
            "MV entropy section (sym+blob) is smaller under ContextV1 than StaticV1."
        } else if d_mv > 0 {
            "MV entropy section (sym+blob) is larger under ContextV1 than StaticV1."
        } else {
            "MV entropy section size matches between modes on this run (one mode carries all bytes)."
        };
        Some(format!(
            "Total compressed bytes: StaticV1={}, ContextV1={}, Δ(context−static)={:+}. MV entropy section bytes: StaticV1={}, ContextV1={}, Δ={:+}. {} {}",
            st.row.bytes,
            ctx.row.bytes,
            dt,
            st.static_mv_bytes,
            ctx.context_mv_bytes,
            d_mv,
            total_cmp,
            mv_cmp
        ))
    } else if st.ok && !ctx.ok {
        Some(format!(
            "StaticV1 ok (total_bytes={} static_mv_bytes={}). ContextV1 failed: {}",
            st.row.bytes,
            st.static_mv_bytes,
            ctx.error.as_deref().unwrap_or("unknown")
        ))
    } else if !st.ok && ctx.ok {
        Some(format!(
            "StaticV1 failed: {}. ContextV1 ok (total_bytes={} context_mv_bytes={}).",
            st.error.as_deref().unwrap_or("unknown"),
            ctx.row.bytes,
            ctx.context_mv_bytes
        ))
    } else {
        Some(format!(
            "Both rows failed: static_err={:?} context_err={:?}",
            st.error, ctx.error
        ))
    }
}

fn entropy_model_compare_row(
    label: &str,
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    settings: &SrsV2EncodeSettings,
    p: &PassNumbers,
    deblock: DeblockBenchReport,
) -> EntropyModelCompareEntry {
    let m = &p.motion_agg;
    let mv_denom = (m.mv_delta_zero_varints + m.mv_delta_nonzero_varints).max(1);
    let mv_delta_avg_abs = m.mv_delta_sum_abs_components as f64 / mv_denom as f64;
    let (static_mv, ctx_mv) =
        if matches!(settings.inter_syntax_mode, SrsV2InterSyntaxMode::EntropyV1) {
            match settings.entropy_model_mode {
                SrsV2EntropyModelMode::StaticV1 => (m.mv_entropy_section_bytes, 0),
                SrsV2EntropyModelMode::ContextV1 => (0, m.mv_entropy_section_bytes),
            }
        } else {
            (0, 0)
        };
    EntropyModelCompareEntry {
        entropy_model_mode: entropy_model_cli_label(settings.entropy_model_mode).to_string(),
        context_mv_bytes: ctx_mv,
        static_mv_bytes: static_mv,
        mv_delta_zero_count: m.mv_delta_zero_varints,
        mv_delta_nonzero_count: m.mv_delta_nonzero_varints,
        mv_delta_avg_abs,
        entropy_context_count: m.entropy_context_count,
        entropy_symbol_count: m.entropy_symbol_count,
        entropy_failure_reason: None,
        fr2_revision_counts: fr2_revision_counts(&p.payloads),
        ok: true,
        error: None,
        row: pass_to_row(args, label, p),
        details: pass_to_details(args, seq, settings, &args.residual_entropy, p, deblock),
    }
}

fn run_compare_entropy_models_report(
    args: &Args,
    seq: &VideoSequenceHeaderV2,
    raw: &[u8],
    expected_frame: usize,
) -> Result<BenchReport> {
    let re = parse_residual_entropy(&args.residual_entropy)?;
    let modes = [
        (
            "SRSV2-entropy-StaticV1",
            SrsV2EntropyModelMode::StaticV1,
            "static",
        ),
        (
            "SRSV2-entropy-ContextV1",
            SrsV2EntropyModelMode::ContextV1,
            "context",
        ),
    ];
    let mut entries = Vec::new();
    let mut table = Vec::new();
    let mut primary: Option<Srsv2Details> = None;

    for (label, mode, entropy_cli) in modes {
        let mut a = args.clone();
        a.inter_syntax = "entropy".to_string();
        a.entropy_model = entropy_cli.to_string();
        let settings = match build_settings(&a, re) {
            Ok(s) => s,
            Err(e) => {
                let err = format!("settings: {e:#}");
                entries.push(EntropyModelCompareEntry {
                    entropy_model_mode: entropy_cli.to_string(),
                    context_mv_bytes: 0,
                    static_mv_bytes: 0,
                    mv_delta_zero_count: 0,
                    mv_delta_nonzero_count: 0,
                    mv_delta_avg_abs: 0.0,
                    entropy_context_count: 0,
                    entropy_symbol_count: 0,
                    entropy_failure_reason: Some(err.clone()),
                    fr2_revision_counts: Fr2RevisionCounts::default(),
                    ok: false,
                    error: Some(err.clone()),
                    row: CodecRow {
                        codec: label.to_string(),
                        error: Some(err),
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
                        inter_syntax_mode: "entropy".to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        entropy_model_mode: entropy_cli.to_string(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
                continue;
            }
        };
        debug_assert_eq!(settings.entropy_model_mode, mode);
        debug_assert_eq!(settings.inter_syntax_mode, SrsV2InterSyntaxMode::EntropyV1);

        match run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
            Ok(numbers) => {
                let deblock =
                    build_deblock_bench_report(&a, seq, &settings, raw, expected_frame, &numbers)
                        .unwrap_or_else(|_| DeblockBenchReport::default());
                let row_entry =
                    entropy_model_compare_row(label, &a, seq, &settings, &numbers, deblock);
                if primary.is_none() {
                    primary = Some(row_entry.details.clone());
                }
                table.push(row_entry.row.clone());
                entries.push(row_entry);
            }
            Err(e) => {
                let err = format!("{e:#}");
                entries.push(EntropyModelCompareEntry {
                    entropy_model_mode: entropy_cli.to_string(),
                    context_mv_bytes: 0,
                    static_mv_bytes: 0,
                    mv_delta_zero_count: 0,
                    mv_delta_nonzero_count: 0,
                    mv_delta_avg_abs: 0.0,
                    entropy_context_count: 0,
                    entropy_symbol_count: 0,
                    entropy_failure_reason: Some(err.clone()),
                    fr2_revision_counts: Fr2RevisionCounts::default(),
                    ok: false,
                    error: Some(err.clone()),
                    row: CodecRow {
                        codec: label.to_string(),
                        error: Some(err),
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
                        inter_syntax_mode: "entropy".to_string(),
                        residual_entropy: args.residual_entropy.clone(),
                        entropy_model_mode: entropy_cli.to_string(),
                        ..Default::default()
                    },
                });
                table.push(entries.last().unwrap().row.clone());
            }
        }
    }

    let srsv2 = primary.unwrap_or_else(|| entries[0].details.clone());
    let entropy_model_compare_summary = summarize_entropy_model_compare(&entries);

    Ok(BenchReport {
        note: "Engineering measurement only; not a marketing claim.",
        residual_note: "`--compare-entropy-models` runs **StaticV1** then **ContextV1** with `--inter-syntax entropy`; a failed **ContextV1** pass keeps an error row without discarding **StaticV1**.",
        command: std::env::args().collect::<Vec<_>>().join(" "),
        raw_bytes: raw.len() as u64,
        width: args.width,
        height: args.height,
        frames: args.frames,
        fps: args.fps.max(1),
        srsv2,
        x264: None,
        table,
        compare_residual_modes: None,
        sweep: None,
        compare_b_modes: None,
        compare_inter_syntax: None,
        compare_rdo: None,
        compare_partitions: None,
        compare_partition_costs: None,
        compare_entropy_models: Some(entries),
        entropy_model_compare_summary,
        match_x264_bitrate_note: None,
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
                if let Ok(numbers) = run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
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
            if let Ok(numbers) = run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
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
            if let Ok(numbers) = run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
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
            if let Ok(numbers) = run_srsv2_pass(&a, seq, raw, &settings, expected_frame) {
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

fn run_h264_progress_summary(args: &Args) -> Result<()> {
    let inputs = quality_metrics::srsv2_progress_report::ProgressReportInputs {
        entropy_models_json: &args.progress_entropy_json,
        partition_costs_json: &args.progress_partition_costs_json,
        sweep_quality_bitrate_json: &args.progress_sweep_json,
        compare_x264_bench_json: args.progress_x264_json.as_deref(),
        compare_b_modes_json: args.progress_b_modes_json.as_deref(),
    };
    let rep = quality_metrics::srsv2_progress_report::write_progress_summary_files(
        &inputs,
        &args.h264_progress_summary_out_json,
        &args.h264_progress_summary_out_md,
    )
    .map_err(|e| anyhow!("progress summary: {e}"))?;
    println!("{}", serde_json::to_string_pretty(&rep)?);
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;

    if args.h264_progress_summary {
        return run_h264_progress_summary(&args);
    }

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

    if args.sweep_quality_bitrate {
        let sweep_cfg = quality_metrics::srsv2_sweep::SweepConfig::from_bench_cli(
            args.width,
            args.height,
            args.frames,
            args.fps,
            args.keyint,
            args.motion_radius,
            args.reference_frames,
            args.residual_entropy.clone(),
            args.sweep_ssim_threshold,
            args.sweep_byte_budget,
            None,
        );
        let report = quality_metrics::srsv2_sweep::run_quality_bitrate_sweep(&sweep_cfg, &raw)
            .map_err(|e| anyhow!("quality/bitrate sweep: {e}"))?;
        if let Some(p) = args.report_json.parent() {
            fs::create_dir_all(p).ok();
        }
        if let Some(p) = args.report_md.parent() {
            fs::create_dir_all(p).ok();
        }
        quality_metrics::srsv2_sweep::write_sweep_json(&args.report_json, &report)
            .map_err(|e| anyhow!("write sweep json: {e}"))?;
        quality_metrics::srsv2_sweep::write_sweep_markdown(&args.report_md, &report)
            .map_err(|e| anyhow!("write sweep markdown: {e}"))?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let re = parse_residual_entropy(&args.residual_entropy)?;
    let settings = build_settings(&args, re)?;
    let seq = build_seq_header(&args, &settings);

    if args.sweep {
        return run_sweep_file(&args, &seq, &raw, expected_frame);
    }

    let report = if args.compare_b_modes {
        run_compare_b_modes_report(&args, &seq, &raw, expected_frame)?
    } else if args.compare_inter_syntax {
        run_compare_inter_syntax_report(&args, &seq, &raw, expected_frame)?
    } else if args.compare_rdo {
        run_compare_rdo_report(&args, &seq, &raw, expected_frame)?
    } else if args.compare_partitions {
        run_compare_partitions_report(&args, &seq, &raw, expected_frame)?
    } else if args.compare_partition_costs {
        run_compare_partition_costs_report(&args, &seq, &raw, expected_frame)?
    } else if args.compare_entropy_models {
        run_compare_entropy_models_report(&args, &seq, &raw, expected_frame)?
    } else if args.compare_residual_modes {
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

/// When MSE is zero, [`psnr_u8`] returns positive infinity. `serde_json` serializes non-finite `f64`
/// as JSON `null`, which broke `--compare-x264` reports for near-lossless encodes. Use this finite
/// sentinel in JSON/table output only; it means “indistinguishable from source on luma for this clip.”
const PSNR_Y_JSON_SAFE_IDENTICAL_DB: f64 = 100.0;

fn psnr_y_json_safe_from_buffers(reference: &[u8], measured: &[u8]) -> Result<f64> {
    let p = psnr_u8(reference, measured, 255.0).map_err(|e| anyhow!("psnr: {e}"))?;
    if p.is_finite() {
        Ok(p)
    } else if p == f64::INFINITY {
        Ok(PSNR_Y_JSON_SAFE_IDENTICAL_DB)
    } else {
        Err(anyhow!("psnr non-finite (NaN)"))
    }
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
    srsv2_bitrate_bps: Option<f64>,
) -> Result<(Option<CodecRow>, X264Details)> {
    let crf = args.x264_crf;
    let ffmpeg_command = format!(
        "ffmpeg -y -f rawvideo -pix_fmt yuv420p -s {w}x{h} -r {fps} -i \"{inp}\" -frames:v {frames} -c:v libx264 -preset {preset} -crf {crf} -an <output.mp4>",
        w = args.width,
        h = args.height,
        fps = args.fps.max(1),
        inp = args.input.display(),
        frames = args.frames,
        preset = args.x264_preset,
        crf = crf,
    );

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
                ffmpeg_command: Some(ffmpeg_command),
                x264_preset: args.x264_preset.clone(),
                x264_crf: crf,
                achieved_bitrate_bps: None,
                srsv2_bitrate_bps,
                match_x264_bitrate_note: None,
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
                ffmpeg_command: Some(ffmpeg_command.clone()),
                x264_preset: args.x264_preset.clone(),
                x264_crf: crf,
                achieved_bitrate_bps: None,
                srsv2_bitrate_bps,
                match_x264_bitrate_note: None,
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
        Some(psnr_y_json_safe_from_buffers(src_luma, &dec_luma)?)
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
            ffmpeg_command: Some(ffmpeg_command),
            x264_preset: args.x264_preset.clone(),
            x264_crf: crf,
            achieved_bitrate_bps: Some(bitrate_bps),
            srsv2_bitrate_bps,
            match_x264_bitrate_note: None,
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

    if let Some(rows) = &r.compare_b_modes {
        out.push_str("\n## B-mode comparison (`--compare-b-modes`)\n\n");
        out.push_str("| Mode | Bytes | Bitrate (bps) | PSNR-Y | SSIM-Y | I | P | B pkts | B blend (f/b/avg/w) |\n");
        out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---|\n");
        for e in rows {
            let err = e
                .error
                .as_ref()
                .map(|s| format!(" **skipped:** {s}"))
                .unwrap_or_default();
            let bb = &e.b_blend;
            out.push_str(&format!(
                "| {}{} | {} | {:.0} | {:.2} | {:.4} | {} | {} | {} | {}/{}/{}/{} |\n",
                e.mode,
                err,
                e.row.bytes,
                e.row.bitrate_bps,
                e.row.psnr_y,
                e.row.ssim_y,
                e.keyframes,
                e.pframes,
                e.bframe_packets,
                bb.b_forward_macroblocks,
                bb.b_backward_macroblocks,
                bb.b_average_macroblocks,
                bb.b_weighted_macroblocks,
            ));
        }
    }

    if let Some(rows) = &r.compare_inter_syntax {
        out.push_str("\n## Inter-syntax comparison (`--compare-inter-syntax`)\n\n");
        for e in rows {
            out.push_str(&format!(
                "- **{}**: ok={} bytes={} PSNR-Y={:.2} err={:?}\n",
                e.label, e.ok, e.row.bytes, e.row.psnr_y, e.error
            ));
        }
    }

    if let Some(rows) = &r.compare_rdo {
        out.push_str("\n## RDO comparison (`--compare-rdo`)\n\n");
        for e in rows {
            out.push_str(&format!(
                "- **{}**: ok={} bytes={} RDO tested={} err={:?}\n",
                e.label, e.ok, e.row.bytes, e.details.rdo_candidates_tested, e.error
            ));
        }
    }

    if let Some(rows) = &r.compare_partitions {
        out.push_str("\n## Partition comparison (`--compare-partitions`)\n\n");
        for e in rows {
            let p = &e.details.partition;
            out.push_str(&format!(
                "- **{}**: ok={} bytes={} PSNR-Y={:.2} part={}/{}/{}/{} tx={}/{}/{} rdo_part_cand={} err={:?}\n",
                e.label,
                e.ok,
                e.row.bytes,
                e.row.psnr_y,
                p.partition_16x16_count,
                p.partition_16x8_count,
                p.partition_8x16_count,
                p.partition_8x8_count,
                p.transform_4x4_count,
                p.transform_8x8_count,
                p.transform_16x16_count,
                p.rdo_partition_candidates_tested,
                e.error
            ));
        }
    }

    if let Some(rows) = &r.compare_partition_costs {
        out.push_str("\n## Partition cost comparison (`--compare-partition-costs`)\n\n");
        for e in rows {
            let p = &e.details.partition;
            out.push_str(&format!(
                "- **{}**: ok={} bytes={} PSNR-Y={:.2} cost_model={} part={}/{}/{}/{} map_bytes={} mv_bytes={} res_bytes={} err={:?}\n",
                e.label,
                e.ok,
                e.row.bytes,
                e.row.psnr_y,
                p.partition_cost_model,
                p.partition_16x16_count,
                p.partition_16x8_count,
                p.partition_8x16_count,
                p.partition_8x8_count,
                p.partition_map_bytes,
                p.partition_mv_bytes,
                p.partition_residual_bytes,
                e.error
            ));
        }
    }

    if let Some(rows) = &r.compare_entropy_models {
        out.push_str("\n## MV entropy model comparison (`--compare-entropy-models`)\n\n");
        out.push_str(
            "| Model | Bytes | MV bytes | PSNR-Y | SSIM-Y | Enc FPS | Dec FPS | Status |\n",
        );
        out.push_str("|---|---:|---:|---:|---:|---:|---:|---|\n");
        for e in rows {
            let mv_wire = e.static_mv_bytes.saturating_add(e.context_mv_bytes);
            let status = if e.ok {
                "ok".to_string()
            } else {
                e.error
                    .as_ref()
                    .map(|s| {
                        let one_line = s.replace('\n', " ");
                        let short: String = one_line.chars().take(72).collect();
                        format!("failed ({short})")
                    })
                    .unwrap_or_else(|| "failed".to_string())
            };
            out.push_str(&format!(
                "| {} | {} | {} | {:.2} | {:.4} | {:.2} | {:.2} | {} |\n",
                e.entropy_model_mode,
                e.row.bytes,
                mv_wire,
                e.row.psnr_y,
                e.row.ssim_y,
                e.row.enc_fps,
                e.row.dec_fps,
                status
            ));
        }
        if let Some(sum) = &r.entropy_model_compare_summary {
            out.push('\n');
            out.push_str("**Summary:** ");
            out.push_str(sum);
            out.push('\n');
        }
        out.push_str("\n### Telemetry (JSON columns; per entropy-model pass)\n\n");
        for e in rows {
            out.push_str(&format!(
                "- **`{}`** (ok={}): `static_mv_bytes`={} `context_mv_bytes`={} `mv_delta_zero_count`={} `mv_delta_nonzero_count`={} `mv_delta_avg_abs`={:.4} `entropy_context_count`={} `entropy_symbol_count`={}\n",
                e.entropy_model_mode,
                e.ok,
                e.static_mv_bytes,
                e.context_mv_bytes,
                e.mv_delta_zero_count,
                e.mv_delta_nonzero_count,
                e.mv_delta_avg_abs,
                e.entropy_context_count,
                e.entropy_symbol_count
            ));
        }
        out.push_str("\n_JSON: `compare_entropy_models[]` includes `entropy_model_mode`, `static_mv_bytes`, `context_mv_bytes`, `mv_delta_zero_count`, `mv_delta_nonzero_count`, `mv_delta_avg_abs`, `entropy_context_count`, `entropy_symbol_count`, `entropy_failure_reason`, `fr2_revision_counts`, `entropy_model_compare_summary` (top-level), nested `details`._\n");
    }

    if let Some(note) = &r.match_x264_bitrate_note {
        out.push_str("\n## Bitrate matching note\n\n");
        out.push_str(note);
        out.push_str("\n\n");
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
    let sv = &r.srsv2;
    out.push_str(&format!(
        "- bframes_requested: {}\n- bframes_used: {}\n- decode_order_count: {}\n- decode_order_frame_indices: {:?}\n- display_order_frame_indices: {:?}\n- display_frame_count: {}\n- reference_frames_used: {}\n- p_anchor_count: {}\n- avg_p_anchor_bytes: {:.1}\n- bframe_count (wire): {}\n- avg_B_bytes: {:.1}\n- alt_ref_count: {}\n- avg_altref_bytes: {:.1}\n- bframe_psnr_y (B-only aggregate): {:.2}\n- bframe_ssim_y (B-only aggregate): {:.4}\n- b_blend: forward_mb={} backward_mb={} average_mb={} weighted_mb={} sad_eval={}\n",
        sv.bframes_requested,
        sv.bframes_used,
        sv.decode_order_count,
        sv.decode_order_frame_indices,
        sv.display_order_frame_indices,
        sv.display_frame_count,
        sv.reference_frames_used,
        sv.p_anchor_count,
        sv.avg_p_anchor_bytes,
        sv.bframe_count,
        sv.avg_bframe_bytes,
        sv.alt_ref_count,
        sv.avg_altref_bytes,
        sv.bframe_psnr_y,
        sv.bframe_ssim_y,
        sv.b_blend.b_forward_macroblocks,
        sv.b_blend.b_backward_macroblocks,
        sv.b_blend.b_average_macroblocks,
        sv.b_blend.b_weighted_macroblocks,
        sv.b_blend.b_sad_evaluations,
    ));
    if let Some(reason) = &sv.unsupported_bframe_reason {
        out.push_str(&format!("- unsupported_bframe_reason: {reason}\n"));
    }
    if !sv.bframe_mode_note.is_empty() {
        out.push_str(&format!("- bframe_mode_note: {}\n", sv.bframe_mode_note));
    }
    {
        let pt = &sv.partition;
        out.push_str("\n## Partition decision telemetry\n\n");
        out.push_str("| Metric | Value |\n|---|---:|\n");
        out.push_str(&format!(
            "| Partition cost model (AutoFast) | {} |\n",
            if pt.partition_cost_model.is_empty() {
                "(n/a)"
            } else {
                pt.partition_cost_model.as_str()
            }
        ));
        out.push_str(&format!(
            "| Candidates tested | {} |\n",
            pt.rdo_partition_candidates_tested
        ));
        out.push_str(&format!(
            "| Header-cost rejections | {} |\n",
            pt.partition_rejected_by_header_cost
        ));
        out.push_str(&format!(
            "| RDO rejections | {} |\n",
            pt.partition_rejected_by_rdo
        ));
        out.push_str(&format!(
            "| 16×16 chosen | {} |\n",
            pt.partition_16x16_count
        ));
        out.push_str(&format!(
            "| Split 8×8 chosen | {} |\n",
            pt.partition_8x8_count
        ));
        out.push_str(&format!(
            "| Rect 16×8 chosen | {} |\n",
            pt.partition_16x8_count
        ));
        out.push_str(&format!(
            "| Rect 8×16 chosen | {} |\n",
            pt.partition_8x16_count
        ));
        out.push_str(&format!(
            "| Avg SAD gain (override events) | {:.4} |\n",
            pt.avg_partition_sad_gain
        ));
        out.push_str(&format!(
            "| Partition map bytes | {} |\n",
            pt.partition_map_bytes
        ));
        out.push_str(&format!("| MV bytes | {} |\n", pt.partition_mv_bytes));
        out.push_str(&format!(
            "| Residual bytes | {} |\n",
            pt.partition_residual_bytes
        ));
        out.push_str(&format!(
            "| Transform decisions Tx4×4 | {} |\n",
            pt.transform_decision_tx4x4
        ));
        out.push_str(&format!(
            "| Transform decisions Tx8×8 | {} |\n",
            pt.transform_decision_tx8x8
        ));
    }
    out.push_str("\n## Frame-kind payloads\n\n");
    out.push_str("| Kind | Count | Avg bytes |\n");
    out.push_str("|---|---:|---:|\n");
    out.push_str(&format!(
        "| I | {} | {:.1} |\n",
        sv.keyframes, sv.avg_i_bytes
    ));
    out.push_str(&format!(
        "| P anchor | {} | {:.1} |\n",
        sv.p_anchor_count, sv.avg_p_anchor_bytes
    ));
    out.push_str(&format!(
        "| B | {} | {:.1} |\n",
        sv.bframe_count, sv.avg_bframe_bytes
    ));
    out.push_str(&format!(
        "| AltRef | {} | {:.1} |\n",
        sv.alt_ref_count, sv.avg_altref_bytes
    ));
    out.push_str(&format!(
        "- residual_entropy setting: {}\n- intra explicit blocks: {}\n- intra rANS blocks: {}\n- P explicit chunks: {}\n- P rANS chunks: {}\n",
        r.srsv2.residual_entropy,
        r.srsv2.intra_explicit_blocks,
        r.srsv2.intra_rans_blocks,
        r.srsv2.p_explicit_chunks,
        r.srsv2.p_rans_chunks
    ));
    out.push_str(&format!(
        "- inter_syntax_mode: {}\n- rdo_mode: {}\n- rdo_lambda_scale: {}\n",
        sv.inter_syntax_mode, sv.rdo_mode, sv.rdo_lambda_scale
    ));
    out.push_str(&format!(
        "- MV aggregate: prediction=`{}` mv_raw_bytes_estimate={} mv_compact_bytes={} mv_entropy_bytes={} mv_delta_zero_count={} mv_delta_nonzero_count={} mv_delta_avg_abs={:.4} inter_header_bytes={} inter_residual_bytes={}\n",
        sv.mv_prediction_mode,
        sv.mv_raw_bytes_estimate,
        sv.mv_compact_bytes,
        sv.mv_entropy_bytes,
        sv.mv_delta_zero_count,
        sv.mv_delta_nonzero_count,
        sv.mv_delta_avg_abs,
        sv.inter_header_bytes,
        sv.inter_residual_bytes,
    ));
    out.push_str(&format!(
        "- MV entropy telemetry: entropy_model_mode={} static_mv_bytes={} context_mv_bytes={} entropy_context_count={} entropy_symbol_count={}\n",
        sv.entropy_model_mode,
        sv.static_mv_bytes,
        sv.context_mv_bytes,
        sv.entropy_context_count,
        sv.entropy_symbol_count,
    ));
    out.push_str(&format!(
        "- RDO aggregate: candidates_tested={} skip={} forward={} backward={} average={} weighted={} halfpel={} residual={} no_residual={} inter_zero_mv_wins={} inter_me_mv_wins={} estimated_bits_used_for_decision={}\n",
        sv.rdo_candidates_tested,
        sv.rdo_skip_decisions,
        sv.rdo_forward_decisions,
        sv.rdo_backward_decisions,
        sv.rdo_average_decisions,
        sv.rdo_weighted_decisions,
        sv.rdo_halfpel_decisions,
        sv.rdo_residual_decisions,
        sv.rdo_no_residual_decisions,
        sv.rdo_inter_zero_mv_wins,
        sv.rdo_inter_me_mv_wins,
        sv.estimated_bits_used_for_decision,
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
        out.push_str(&format!(
            "- B-frame motion search (experimental, FR2 rev13/14 path): {}\n",
            m.b_motion_search_mode
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
        out.push_str("\n## x264 / FFmpeg comparison (`--compare-x264`)\n\n");
        out.push_str("_Fair comparison requires **achieved bitrate** and quality metrics for **both** encoders (CRF-only labels are not sufficient by themselves)._\n\n");
        out.push_str(&format!(
            "- status: {}\n- preset: `{}`\n- CRF: {}\n",
            x.status, x.x264_preset, x.x264_crf
        ));
        if let Some(b) = x.achieved_bitrate_bps {
            out.push_str(&format!("- x264 achieved bitrate (bps): {:.2}\n", b));
        } else {
            out.push_str("- x264 achieved bitrate (bps): *(unavailable)*\n");
        }
        if let Some(b) = x.srsv2_bitrate_bps {
            out.push_str(&format!(
                "- SRSV2 achieved bitrate (bps) at compare time: {:.2}\n",
                b
            ));
        }
        if let Some(py) = x.psnr_y {
            out.push_str(&format!("- x264 PSNR-Y (vs source): {:.2}\n", py));
            if (py - PSNR_Y_JSON_SAFE_IDENTICAL_DB).abs() <= 1e-6 {
                out.push_str("  - Identical luma → raw PSNR is ∞; **100.0 dB** here is a JSON-safe sentinel, not a codec ceiling.\n");
            }
        }
        if let Some(sy) = x.ssim_y {
            out.push_str(&format!("- x264 SSIM-Y (vs source): {:.4}\n", sy));
        }
        if let Some(cmd) = &x.ffmpeg_command {
            out.push_str(&format!(
                "- documented FFmpeg template / command string:\n\n```text\n{cmd}\n```\n"
            ));
        }
        if let Some(n) = &x.match_x264_bitrate_note {
            out.push_str(&format!("- match-bitrate placeholder: {n}\n"));
        }
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        assert!(validate_args(&a).is_err());
        a.bframes = 0;
        assert!(validate_args(&a).is_ok());
    }

    #[test]
    fn validate_rejects_compare_inter_syntax_with_compare_rdo_together() {
        let a = Args {
            input: PathBuf::from("nope"),
            width: 64,
            height: 64,
            frames: 3,
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: true,
            compare_rdo: true,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(
            err.contains("mutually exclusive"),
            "unexpected validate message: {err}"
        );
    }

    #[test]
    fn validate_rejects_compare_partitions_with_compare_inter_syntax() {
        let a = Args {
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "compact".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: true,
            compare_rdo: false,
            compare_partitions: true,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(
            err.contains("mutually exclusive"),
            "unexpected validate message: {err}"
        );
    }

    #[test]
    fn match_x264_bitrate_error_message_is_explicit() {
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: true,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(
            err.contains("bitrate matching is not implemented"),
            "unexpected: {err}"
        );
        a.match_x264_bitrate = false;
        assert!(validate_args(&a).is_ok());
    }

    #[test]
    fn bframes_one_requires_reference_frames_at_least_two() {
        let mut a = Args {
            input: PathBuf::from("nope"),
            width: 16,
            height: 16,
            frames: 3,
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            bframes: 1,
            alt_ref: "off".to_string(),
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        assert!(validate_args(&a).is_err());
        a.reference_frames = 2;
        assert!(validate_args(&a).is_ok());
    }

    #[test]
    fn b_gop_encode_order_three_frames_is_decode_order_i0_p2_b1() {
        let s = b_gop_encode_order(3, 30).unwrap();
        assert_eq!(
            s.iter().map(|(fi, _)| *fi).collect::<Vec<_>>(),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn b_gop_encode_order_five_frames_is_i0_p2_b1_p4_b3() {
        let s = b_gop_encode_order(5, 30).unwrap();
        assert_eq!(
            s.iter().map(|(fi, _)| *fi).collect::<Vec<_>>(),
            vec![0, 2, 1, 4, 3]
        );
    }

    #[test]
    fn alt_ref_on_is_unsupported_in_benchmark_validate() {
        let a = Args {
            input: PathBuf::from("nope"),
            width: 16,
            height: 16,
            frames: 3,
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            alt_ref: "on".to_string(),
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 2,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(
            err.contains("alt-ref benchmark encode is not wired yet"),
            "{err}"
        );
    }

    #[test]
    fn run_srsv2_pass_b_gop_three_frames_decode_vs_display_indices() {
        use quality_metrics::DisplayReorderBuffer;

        let w = 16u32;
        let h = 16u32;
        let frames = 3u32;
        let fb = yuv420_frame_bytes(w, h).unwrap();
        let raw: Vec<u8> = (0..fb * frames as usize).map(|i| (i % 251) as u8).collect();
        let args = Args {
            input: PathBuf::from("nope"),
            width: w,
            height: h,
            frames,
            fps: 30,
            qp: 28,
            keyint: 30,
            motion_radius: 8,
            report_json: PathBuf::from("j"),
            report_md: PathBuf::from("m"),
            compare_x264: false,
            x264_crf: 23,
            x264_preset: "medium".to_string(),
            residual_entropy: "auto".to_string(),
            compare_residual_modes: false,
            sweep: false,
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            bframes: 1,
            alt_ref: "off".to_string(),
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 2,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        validate_args(&args).unwrap();
        let settings = build_settings(&args, ResidualEntropy::Auto).unwrap();
        let seq = build_seq_header(&args, &settings);
        let numbers = run_srsv2_pass(&args, &seq, &raw, &settings, fb).unwrap();
        assert_eq!(numbers.display_order_frame_indices, vec![0, 1, 2]);
        assert_eq!(numbers.decode_order_frame_indices, vec![0, 2, 1]);
        assert_eq!(numbers.bframes_used, 1);
        assert_eq!(numbers.bframes_requested, 1);
        assert!(numbers.psnr_y.is_finite());
        assert!(numbers.ssim_y.is_finite());
        assert!(numbers.bframe_psnr_y.is_finite());

        let ylen = (w * h) as usize;
        let mut buf = DisplayReorderBuffer::new(8);
        for fi in [0u32, 2, 1] {
            let src = frame_luma_slice(&raw, fb, fi, w, h).to_vec();
            buf.insert(fi, src).unwrap();
        }
        let flat = buf.flatten_expected(&[0, 1, 2], ylen).unwrap();
        let mut exp = Vec::new();
        for fi in 0..frames {
            exp.extend_from_slice(frame_luma_slice(&raw, fb, fi, w, h));
        }
        assert_eq!(flat, exp);
    }

    #[test]
    fn run_srsv2_pass_b_gop_three_frames_32x32_decodes() {
        let w = 32u32;
        let h = 32u32;
        let frames = 3u32;
        let fb = yuv420_frame_bytes(w, h).unwrap();
        let raw: Vec<u8> = (0..fb * frames as usize).map(|i| (i % 251) as u8).collect();
        let args = Args {
            input: PathBuf::from("nope"),
            width: w,
            height: h,
            frames,
            fps: 30,
            qp: 28,
            keyint: 30,
            motion_radius: 8,
            report_json: PathBuf::from("j"),
            report_md: PathBuf::from("m"),
            compare_x264: false,
            x264_crf: 23,
            x264_preset: "medium".to_string(),
            residual_entropy: "auto".to_string(),
            compare_residual_modes: false,
            sweep: false,
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            bframes: 1,
            alt_ref: "off".to_string(),
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 2,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        validate_args(&args).unwrap();
        let settings = build_settings(&args, ResidualEntropy::Auto).unwrap();
        let seq = build_seq_header(&args, &settings);
        let numbers = run_srsv2_pass(&args, &seq, &raw, &settings, fb).unwrap();
        assert_eq!(numbers.display_order_frame_indices.len(), frames as usize);
        assert_eq!(numbers.decode_order_frame_indices, vec![0, 2, 1]);
        assert!(numbers.payloads.iter().any(|p| p.get(3) == Some(&13)));
    }

    #[test]
    fn compare_b_modes_report_runs_four_rows_without_errors_and_serializes() {
        let w = 64u32;
        let h = 64u32;
        let frames = 3u32;
        let fb = yuv420_frame_bytes(w, h).unwrap();
        let raw: Vec<u8> = vec![120u8; fb * frames as usize];
        let args = Args {
            input: PathBuf::from("nope"),
            width: w,
            height: h,
            frames,
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
            rc: "fixed-qp".to_string(),
            quality: None,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit: 2,
            aq: "off".to_string(),
            aq_strength: 4,
            motion_search: "diamond".to_string(),
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
            bframes: 1,
            alt_ref: "off".to_string(),
            b_motion_search: "independent-forward-backward".to_string(),
            gop: 0,
            reference_frames: 2,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        validate_args(&args).unwrap();
        let settings = build_settings(&args, ResidualEntropy::Auto).unwrap();
        let seq = build_seq_header(&args, &settings);
        let rep = run_compare_b_modes_report(&args, &seq, &raw, fb).unwrap();
        let rows = rep.compare_b_modes.as_ref().unwrap();
        assert_eq!(rows.len(), 4);
        for e in rows {
            assert!(e.error.is_none(), "mode {} failed: {:?}", e.mode, e.error);
        }
        assert_eq!(rep.srsv2.display_frame_count, frames);
        assert_eq!(rep.frames, frames);
        let js = serde_json::to_string(&rep).unwrap();
        assert!(js.contains("SRSV2-P-only"));
        assert!(js.contains("SRSV2-B-int"));
        assert!(js.contains("SRSV2-B-half"));
        assert!(js.contains("SRSV2-B-weighted"));
        assert!(js.contains("\"compare_b_modes\""));
    }

    #[test]
    fn b_motion_half_pel_cli_maps_in_build_settings() {
        let mut a = Args {
            input: PathBuf::from("nope"),
            width: 64,
            height: 64,
            frames: 3,
            fps: 30,
            qp: 28,
            keyint: 30,
            motion_radius: 8,
            report_json: PathBuf::from("j"),
            report_md: PathBuf::from("m"),
            compare_x264: false,
            x264_crf: 23,
            x264_preset: "medium".to_string(),
            residual_entropy: "auto".to_string(),
            compare_residual_modes: false,
            sweep: false,
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
            rc: "fixed-qp".to_string(),
            quality: None,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit: 2,
            aq: "off".to_string(),
            aq_strength: 4,
            motion_search: "diamond".to_string(),
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
            bframes: 1,
            alt_ref: "off".to_string(),
            b_motion_search: "independent-forward-backward-half".to_string(),
            gop: 0,
            reference_frames: 2,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        validate_args(&a).unwrap();
        let s = build_settings(&a, ResidualEntropy::Auto).unwrap();
        assert_eq!(
            s.b_motion_search_mode,
            SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel
        );
        a.b_motion_search = "independent_forward_backward_half".to_string();
        let s2 = build_settings(&a, ResidualEntropy::Auto).unwrap();
        assert_eq!(
            s2.b_motion_search_mode,
            SrsV2BMotionSearchMode::IndependentForwardBackwardHalfPel
        );
    }

    #[test]
    fn psnr_json_safe_maps_perfect_luma_match_to_finite_db() {
        let b = vec![42_u8; 256];
        let raw = psnr_u8(&b, &b, 255.0).unwrap();
        assert!(raw.is_infinite(), "expected +inf for identical buffers");
        let safe = psnr_y_json_safe_from_buffers(&b, &b).unwrap();
        assert_eq!(safe, PSNR_Y_JSON_SAFE_IDENTICAL_DB);
        assert!(safe.is_finite());
    }

    #[test]
    fn x264_compare_details_always_include_documented_ffmpeg_command_string() {
        let x = X264Details {
            status: "ffmpeg unavailable".to_string(),
            bytes: None,
            encode_seconds: None,
            decode_seconds: None,
            psnr_y: None,
            ssim_y: None,
            ffmpeg_command: Some("ffmpeg -y -f rawvideo ...".to_string()),
            x264_preset: "medium".to_string(),
            x264_crf: 23,
            achieved_bitrate_bps: None,
            srsv2_bitrate_bps: Some(123_456.0),
            match_x264_bitrate_note: None,
        };
        let j = serde_json::to_string(&x).unwrap();
        assert!(j.contains("ffmpeg_command"));
        assert!(j.contains("ffmpeg -y -f rawvideo"));
        assert!(j.contains("\"x264_preset\":\"medium\""));
        assert!(j.contains("\"x264_crf\":23"));
        assert!(j.contains("srsv2_bitrate_bps"));
    }

    #[test]
    fn run_srsv2_pass_ip_only_keeps_bench_aggregate_fields_zero() {
        let w = 16u32;
        let h = 16u32;
        let frames = 2u32;
        let fb = yuv420_frame_bytes(w, h).unwrap();
        let raw: Vec<u8> = vec![128u8; fb * frames as usize];
        let args = Args {
            input: PathBuf::from("nope"),
            width: w,
            height: h,
            frames,
            fps: 30,
            qp: 28,
            keyint: 1,
            motion_radius: 8,
            report_json: PathBuf::from("j"),
            report_md: PathBuf::from("m"),
            compare_x264: false,
            x264_crf: 23,
            x264_preset: "medium".to_string(),
            residual_entropy: "auto".to_string(),
            compare_residual_modes: false,
            sweep: false,
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let settings = build_settings(&args, ResidualEntropy::Auto).unwrap();
        let seq = build_seq_header(&args, &settings);
        let numbers = run_srsv2_pass(&args, &seq, &raw, &settings, fb).unwrap();
        assert_eq!(numbers.bframes_used, 0);
        assert_eq!(numbers.bframe_psnr_y, 0.0);
        assert_eq!(numbers.bframe_ssim_y, 0.0);
        assert_eq!(numbers.decode_order_frame_indices, vec![0, 1]);
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
                inter_syntax_mode: "raw".to_string(),
                rdo_mode: "off".to_string(),
                rdo_lambda_scale: 256,
                mv_prediction_mode: String::new(),
                mv_raw_bytes_estimate: 0,
                mv_compact_bytes: 0,
                mv_entropy_bytes: 0,
                mv_delta_zero_count: 0,
                mv_delta_nonzero_count: 0,
                mv_delta_avg_abs: 0.0,
                inter_header_bytes: 0,
                inter_residual_bytes: 0,
                rdo_candidates_tested: 0,
                rdo_skip_decisions: 0,
                rdo_forward_decisions: 0,
                rdo_backward_decisions: 0,
                rdo_average_decisions: 0,
                rdo_weighted_decisions: 0,
                rdo_halfpel_decisions: 0,
                rdo_residual_decisions: 0,
                rdo_no_residual_decisions: 0,
                rdo_inter_zero_mv_wins: 0,
                rdo_inter_me_mv_wins: 0,
                estimated_bits_used_for_decision: 0,
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
                    b_motion_search_mode: "off".to_string(),
                    rdo_inter_zero_mv_wins: 0,
                    rdo_inter_me_mv_wins: 0,
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
                bframes_requested: 0,
                bframes_used: 0,
                decode_order_frame_indices: vec![0],
                display_order_frame_indices: vec![0],
                p_anchor_count: 0,
                avg_p_anchor_bytes: 0.0,
                b_blend: BBlendBenchReport::default(),
                unsupported_bframe_reason: None,
                bframe_mode_note: String::new(),
                bframe_psnr_y: 0.0,
                bframe_ssim_y: 0.0,
                decode_order_count: 1,
                partition: PartitionBenchSummary::default(),
                entropy_model_mode: "static".to_string(),
                static_mv_bytes: 0,
                context_mv_bytes: 0,
                entropy_context_count: 0,
                entropy_symbol_count: 0,
                entropy_failure_reason: None,
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
            compare_b_modes: None,
            compare_inter_syntax: None,
            compare_rdo: None,
            compare_partitions: None,
            compare_partition_costs: None,
            compare_entropy_models: None,
            entropy_model_compare_summary: None,
            match_x264_bitrate_note: None,
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
        assert!(js.contains("\"b_motion_search_mode\":\"off\""));
        assert!(js.contains("\"avg_p_anchor_bytes\":0"));
        assert!(js.contains("\"b_blend\""));
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
                inter_syntax_mode: "raw".to_string(),
                rdo_mode: "off".to_string(),
                rdo_lambda_scale: 256,
                mv_prediction_mode: String::new(),
                mv_raw_bytes_estimate: 0,
                mv_compact_bytes: 0,
                mv_entropy_bytes: 0,
                mv_delta_zero_count: 0,
                mv_delta_nonzero_count: 0,
                mv_delta_avg_abs: 0.0,
                inter_header_bytes: 0,
                inter_residual_bytes: 0,
                rdo_candidates_tested: 0,
                rdo_skip_decisions: 0,
                rdo_forward_decisions: 0,
                rdo_backward_decisions: 0,
                rdo_average_decisions: 0,
                rdo_weighted_decisions: 0,
                rdo_halfpel_decisions: 0,
                rdo_residual_decisions: 0,
                rdo_no_residual_decisions: 0,
                rdo_inter_zero_mv_wins: 0,
                rdo_inter_me_mv_wins: 0,
                estimated_bits_used_for_decision: 0,
                legacy_explicit_total_payload_bytes: None,
                rc: None,
                aq: None,
                motion: None,
                deblock: DeblockBenchReport::default(),
                bframes_requested: 0,
                bframes_used: 0,
                decode_order_frame_indices: Vec::new(),
                display_order_frame_indices: Vec::new(),
                p_anchor_count: 0,
                avg_p_anchor_bytes: 0.0,
                b_blend: BBlendBenchReport::default(),
                unsupported_bframe_reason: None,
                bframe_mode_note: String::new(),
                bframe_psnr_y: 0.0,
                bframe_ssim_y: 0.0,
                decode_order_count: 0,
                partition: PartitionBenchSummary::default(),
                entropy_model_mode: "static".to_string(),
                static_mv_bytes: 0,
                context_mv_bytes: 0,
                entropy_context_count: 0,
                entropy_symbol_count: 0,
                entropy_failure_reason: None,
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
                    inter_syntax_mode: "raw".to_string(),
                    rdo_mode: "off".to_string(),
                    rdo_lambda_scale: 256,
                    mv_prediction_mode: String::new(),
                    mv_raw_bytes_estimate: 0,
                    mv_compact_bytes: 0,
                    mv_entropy_bytes: 0,
                    mv_delta_zero_count: 0,
                    mv_delta_nonzero_count: 0,
                    mv_delta_avg_abs: 0.0,
                    inter_header_bytes: 0,
                    inter_residual_bytes: 0,
                    rdo_candidates_tested: 0,
                    rdo_skip_decisions: 0,
                    rdo_forward_decisions: 0,
                    rdo_backward_decisions: 0,
                    rdo_average_decisions: 0,
                    rdo_weighted_decisions: 0,
                    rdo_halfpel_decisions: 0,
                    rdo_residual_decisions: 0,
                    rdo_no_residual_decisions: 0,
                    rdo_inter_zero_mv_wins: 0,
                    rdo_inter_me_mv_wins: 0,
                    estimated_bits_used_for_decision: 0,
                    legacy_explicit_total_payload_bytes: None,
                    rc: None,
                    aq: None,
                    motion: None,
                    deblock: DeblockBenchReport::default(),
                    bframes_requested: 0,
                    bframes_used: 0,
                    decode_order_frame_indices: Vec::new(),
                    display_order_frame_indices: Vec::new(),
                    p_anchor_count: 0,
                    avg_p_anchor_bytes: 0.0,
                    b_blend: BBlendBenchReport::default(),
                    unsupported_bframe_reason: None,
                    bframe_mode_note: String::new(),
                    bframe_psnr_y: 0.0,
                    bframe_ssim_y: 0.0,
                    decode_order_count: 0,
                    partition: PartitionBenchSummary::default(),
                    entropy_model_mode: "static".to_string(),
                    static_mv_bytes: 0,
                    context_mv_bytes: 0,
                    entropy_context_count: 0,
                    entropy_symbol_count: 0,
                    entropy_failure_reason: None,
                },
            }]),
            sweep: None,
            compare_b_modes: None,
            compare_inter_syntax: None,
            compare_rdo: None,
            compare_partitions: None,
            compare_partition_costs: None,
            compare_entropy_models: None,
            entropy_model_compare_summary: None,
            match_x264_bitrate_note: None,
            git_commit: None,
            os: "os".to_string(),
        };
        let _ = serde_json::to_string(&r).unwrap();
    }

    #[test]
    fn compare_partitions_report_serializes() {
        let r = BenchReport {
            note: "x",
            residual_note: "n",
            command: "cmd".to_string(),
            raw_bytes: 1,
            width: 128,
            height: 128,
            frames: 2,
            fps: 30,
            srsv2: Srsv2Details::default(),
            x264: None,
            table: vec![],
            compare_residual_modes: None,
            sweep: None,
            compare_b_modes: None,
            compare_inter_syntax: None,
            compare_rdo: None,
            compare_partitions: Some(vec![]),
            compare_partition_costs: None,
            compare_entropy_models: None,
            entropy_model_compare_summary: None,
            match_x264_bitrate_note: None,
            git_commit: None,
            os: "os".to_string(),
        };
        let js = serde_json::to_string(&r).unwrap();
        assert!(js.contains("\"compare_partitions\":[]"));
    }

    #[test]
    fn compare_partition_costs_report_serializes() {
        let r = BenchReport {
            note: "x",
            residual_note: "n",
            command: "cmd".to_string(),
            raw_bytes: 1,
            width: 128,
            height: 128,
            frames: 2,
            fps: 30,
            srsv2: Srsv2Details::default(),
            x264: None,
            table: vec![],
            compare_residual_modes: None,
            sweep: None,
            compare_b_modes: None,
            compare_inter_syntax: None,
            compare_rdo: None,
            compare_partitions: None,
            compare_partition_costs: Some(vec![]),
            compare_entropy_models: None,
            entropy_model_compare_summary: None,
            match_x264_bitrate_note: None,
            git_commit: None,
            os: "os".to_string(),
        };
        let js = serde_json::to_string(&r).unwrap();
        assert!(js.contains("\"compare_partition_costs\":[]"));
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
                    inter_syntax_mode: "raw".to_string(),
                    rdo_mode: "off".to_string(),
                    rdo_lambda_scale: 256,
                    mv_prediction_mode: String::new(),
                    mv_raw_bytes_estimate: 0,
                    mv_compact_bytes: 0,
                    mv_entropy_bytes: 0,
                    mv_delta_zero_count: 0,
                    mv_delta_nonzero_count: 0,
                    mv_delta_avg_abs: 0.0,
                    inter_header_bytes: 0,
                    inter_residual_bytes: 0,
                    rdo_candidates_tested: 0,
                    rdo_skip_decisions: 0,
                    rdo_forward_decisions: 0,
                    rdo_backward_decisions: 0,
                    rdo_average_decisions: 0,
                    rdo_weighted_decisions: 0,
                    rdo_halfpel_decisions: 0,
                    rdo_residual_decisions: 0,
                    rdo_no_residual_decisions: 0,
                    rdo_inter_zero_mv_wins: 0,
                    rdo_inter_me_mv_wins: 0,
                    estimated_bits_used_for_decision: 0,
                    legacy_explicit_total_payload_bytes: None,
                    rc: None,
                    aq: None,
                    motion: None,
                    deblock: DeblockBenchReport::default(),
                    bframes_requested: 0,
                    bframes_used: 0,
                    decode_order_frame_indices: Vec::new(),
                    display_order_frame_indices: Vec::new(),
                    p_anchor_count: 0,
                    avg_p_anchor_bytes: 0.0,
                    b_blend: BBlendBenchReport::default(),
                    unsupported_bframe_reason: None,
                    bframe_mode_note: String::new(),
                    bframe_psnr_y: 0.0,
                    bframe_ssim_y: 0.0,
                    decode_order_count: 0,
                    partition: PartitionBenchSummary::default(),
                    entropy_model_mode: "static".to_string(),
                    static_mv_bytes: 0,
                    context_mv_bytes: 0,
                    entropy_context_count: 0,
                    entropy_symbol_count: 0,
                    entropy_failure_reason: None,
                },
            }],
            git_commit: None,
            os: "os".to_string(),
        };
        let _ = serde_json::to_string(&s).unwrap();
    }

    #[test]
    fn validate_rejects_entropy_model_context_without_entropy_inter_syntax() {
        let a = Args {
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "compact".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "context".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(err.contains("entropy-model context"), "{err}");
        assert!(err.contains("inter-syntax entropy"), "{err}");
    }

    #[test]
    fn build_settings_entropy_model_static_and_context_with_entropy_inter_ok() {
        let mut base = Args {
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
            residual_entropy: "explicit".to_string(),
            compare_residual_modes: false,
            sweep: false,
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "entropy".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let s = build_settings(&base, ResidualEntropy::Explicit).unwrap();
        assert_eq!(s.entropy_model_mode, SrsV2EntropyModelMode::StaticV1);
        base.entropy_model = "context".to_string();
        let s2 = build_settings(&base, ResidualEntropy::Explicit).unwrap();
        assert_eq!(s2.entropy_model_mode, SrsV2EntropyModelMode::ContextV1);
    }

    #[test]
    fn validate_rejects_h264_progress_summary_with_sweep() {
        let a = Args {
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
            sweep: true,
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: true,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(err.contains("h264-progress-summary"), "{err}");
    }

    #[test]
    fn h264_progress_summary_requires_existing_required_json_files() {
        let a = Args::try_parse_from([
            "bench_srsv2",
            "--h264-progress-summary",
            "--entropy-models-json",
            "definitely_missing_entropy_progress_528.json",
            "--partition-costs-json",
            "definitely_missing_partition_progress_528.json",
            "--sweep-quality-bitrate-json",
            "definitely_missing_sweep_progress_528.json",
        ])
        .expect("clap parse");
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(
            err.contains("--entropy-models-json")
                && err.contains("required progress-summary input"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_compare_entropy_models_with_compare_rdo() {
        let a = Args {
            input: PathBuf::from("nope"),
            width: 64,
            height: 64,
            frames: 3,
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "entropy".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: true,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: true,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
        };
        let err = validate_args(&a).unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn summarize_entropy_model_compare_spells_byte_verdict_when_both_ok() {
        let row =
            |mode: &str, total: u64, static_mv: u64, context_mv: u64| EntropyModelCompareEntry {
                entropy_model_mode: mode.to_string(),
                context_mv_bytes: context_mv,
                static_mv_bytes: static_mv,
                mv_delta_zero_count: 1,
                mv_delta_nonzero_count: 1,
                mv_delta_avg_abs: 1.0,
                entropy_context_count: u64::from(mode == "context") * 10,
                entropy_symbol_count: 8,
                entropy_failure_reason: None,
                fr2_revision_counts: Fr2RevisionCounts::default(),
                ok: true,
                error: None,
                row: CodecRow {
                    codec: format!("SRSV2-{mode}"),
                    error: None,
                    bytes: total,
                    ratio: 0.1,
                    bitrate_bps: 1.0,
                    psnr_y: 30.0,
                    ssim_y: 0.9,
                    enc_fps: 10.0,
                    dec_fps: 10.0,
                },
                details: Srsv2Details::default(),
            };
        let lower_ctx = vec![row("static", 1000, 100, 0), row("context", 950, 0, 85)];
        let s = summarize_entropy_model_compare(&lower_ctx).unwrap();
        assert!(s.contains("fewer total SRSV2 payload bytes"), "{s}");
        assert!(s.contains("smaller under ContextV1"), "{s}");

        let higher_ctx = vec![row("static", 1000, 100, 0), row("context", 1050, 0, 110)];
        let s2 = summarize_entropy_model_compare(&higher_ctx).unwrap();
        assert!(s2.contains("more total SRSV2 payload bytes"), "{s2}");
        assert!(s2.contains("larger under ContextV1"), "{s2}");
    }

    #[test]
    fn compare_entropy_models_serializes_two_rows() {
        let rows = vec![
            EntropyModelCompareEntry {
                entropy_model_mode: "static".to_string(),
                context_mv_bytes: 0,
                static_mv_bytes: 120,
                mv_delta_zero_count: 1,
                mv_delta_nonzero_count: 2,
                mv_delta_avg_abs: 0.5,
                entropy_context_count: 0,
                entropy_symbol_count: 10,
                entropy_failure_reason: None,
                fr2_revision_counts: Fr2RevisionCounts::default(),
                ok: true,
                error: None,
                row: CodecRow {
                    codec: "SRSV2-entropy-StaticV1".to_string(),
                    error: None,
                    bytes: 1000,
                    ratio: 0.1,
                    bitrate_bps: 1.0,
                    psnr_y: 30.0,
                    ssim_y: 0.9,
                    enc_fps: 10.0,
                    dec_fps: 10.0,
                },
                details: Srsv2Details::default(),
            },
            EntropyModelCompareEntry {
                entropy_model_mode: "context".to_string(),
                context_mv_bytes: 0,
                static_mv_bytes: 0,
                mv_delta_zero_count: 0,
                mv_delta_nonzero_count: 0,
                mv_delta_avg_abs: 0.0,
                entropy_context_count: 0,
                entropy_symbol_count: 0,
                entropy_failure_reason: Some("simulated".into()),
                fr2_revision_counts: Fr2RevisionCounts::default(),
                ok: false,
                error: Some("simulated".into()),
                row: CodecRow {
                    codec: "SRSV2-entropy-ContextV1".to_string(),
                    error: Some("simulated".into()),
                    bytes: 0,
                    ratio: 0.0,
                    bitrate_bps: 0.0,
                    psnr_y: 0.0,
                    ssim_y: 0.0,
                    enc_fps: 0.0,
                    dec_fps: 0.0,
                },
                details: Srsv2Details::default(),
            },
        ];
        let summary = summarize_entropy_model_compare(&rows);
        let r = BenchReport {
            note: "n",
            residual_note: "n",
            command: "bench".into(),
            raw_bytes: 1,
            width: 16,
            height: 16,
            frames: 2,
            fps: 30,
            srsv2: Srsv2Details::default(),
            x264: None,
            table: vec![],
            compare_residual_modes: None,
            sweep: None,
            compare_b_modes: None,
            compare_inter_syntax: None,
            compare_rdo: None,
            compare_partitions: None,
            compare_partition_costs: None,
            compare_entropy_models: Some(rows),
            entropy_model_compare_summary: summary,
            match_x264_bitrate_note: None,
            git_commit: None,
            os: "x".into(),
        };
        let js = serde_json::to_string(&r).unwrap();
        assert!(js.contains("\"entropy_model_mode\":\"static\""), "{js}");
        assert!(js.contains("\"entropy_model_mode\":\"context\""), "{js}");
        assert!(js.contains("compare_entropy_models"));
        assert!(js.contains("entropy_failure_reason"));
        assert!(js.contains("fr2_revision_counts"));
        assert!(js.contains("entropy_model_compare_summary"), "{js}");
        let md = to_markdown(&r);
        assert!(md.contains("MV entropy model comparison"));
        assert!(md.contains("| static |"));
        assert!(md.contains("| Model |"));
        assert!(md.contains("**Summary:**"));
        assert!(md.contains("static_mv_bytes"));
        assert!(md.contains("context_mv_bytes"));
        assert!(md.contains("mv_delta_zero_count"));
        assert!(md.contains("entropy_symbol_count"));
        assert!(md.contains("Telemetry"));
        for key in [
            "static_mv_bytes",
            "context_mv_bytes",
            "mv_delta_zero_count",
            "mv_delta_nonzero_count",
            "mv_delta_avg_abs",
            "entropy_context_count",
            "entropy_symbol_count",
        ] {
            assert!(js.contains(key), "missing {key} in {js}");
        }
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
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
            sweep_quality_bitrate: false,
            sweep_ssim_threshold: 0.95,
            sweep_byte_budget: 10_000_000,
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
            b_motion_search: "off".to_string(),
            gop: 0,
            reference_frames: 1,
            inter_syntax: "raw".to_string(),
            rdo: "off".to_string(),
            rdo_lambda_scale: 256,
            match_x264_bitrate: false,
            compare_b_modes: false,
            b_weighted_prediction: false,
            compare_inter_syntax: false,
            compare_rdo: false,
            compare_partitions: false,
            compare_partition_costs: false,
            partition_cost_model: "sad-only".to_string(),
            partition_map_encoding: "legacy".to_string(),
            inter_partition: "fixed16x16".to_string(),
            transform_size: "auto".to_string(),
            entropy_model: "static".to_string(),
            compare_entropy_models: false,
            h264_progress_summary: false,
            progress_entropy_json: PathBuf::from("var/bench/compare_entropy_models.json"),
            progress_partition_costs_json: PathBuf::from("var/bench/compare_partition_costs.json"),
            progress_sweep_json: PathBuf::from("var/bench/sweep_quality_bitrate.json"),
            progress_x264_json: None,
            progress_b_modes_json: None,
            h264_progress_summary_out_json: PathBuf::from(
                "var/bench/srsv2_h264_progress_summary.json",
            ),
            h264_progress_summary_out_md: PathBuf::from("var/bench/srsv2_h264_progress_summary.md"),
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
