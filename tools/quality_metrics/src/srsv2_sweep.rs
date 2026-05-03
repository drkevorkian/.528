//! SRSV2 **quality vs bitrate** matrix sweep (encoder-only; in-process `libsrs_video` only).
//!
//! ## Public API
//!
//! **Types:** [`SweepConfig`], [`SweepCase`], [`SweepRow`], [`SweepReport`], [`ParetoPoint`],
//! [`ParetoSummary`], [`SweepError`].
//!
//! **Functions:** [`enumerate_sweep_cases`], [`run_quality_bitrate_sweep`], [`compute_pareto_summary`],
//! [`write_sweep_json`], [`write_sweep_markdown`], [`validate_sweep_pareto`].
//!
//! ## Matrix (full enumeration)
//!
//! - **QP:** [`SWEEP_QPS`] — 18, 22, 26, 30, 34  
//! - **inter syntax:** `compact`, `entropy`  
//! - **entropy model:** `static`, `context` (context only with `entropy` inter syntax)  
//! - **partition cost:** `header-aware`, `rdo-fast`  
//! - **partition mode:** `fixed16x16`, `auto-fast`  
//!
//! Full row count: [`SWEEP_FULL_ROW_COUNT`] (**60**). Ordering is fixed: nested loops
//! QP → inter → entropy model → partition cost → partition mode.
//!
//! No FFmpeg or external processes.

use std::fs;
use std::path::Path;
use std::time::Instant;

use libsrs_video::srsv2::frame::VideoPlane;
use libsrs_video::srsv2::validate_adaptive_quant_settings;
use libsrs_video::{
    decode_yuv420_srsv2_payload_managed, encode_yuv420_inter_payload, PixelFormat,
    PreviousFrameRcStats, ResidualEncodeStats, ResidualEntropy, SrsV2AdaptiveQuantizationMode,
    SrsV2BMotionSearchMode, SrsV2BlockAqMode, SrsV2EncodeSettings, SrsV2EntropyModelMode,
    SrsV2Error, SrsV2InterPartitionMode, SrsV2InterSyntaxMode, SrsV2LoopFilterMode,
    SrsV2MotionSearchMode, SrsV2PartitionCostModel, SrsV2PartitionMapEncoding,
    SrsV2RateControlMode, SrsV2RateController, SrsV2RdoMode, SrsV2ReferenceManager,
    SrsV2SubpelMode, SrsV2TransformSizeMode, VideoSequenceHeaderV2, YuvFrame,
};
use serde::Serialize;
use thiserror::Error;

use crate::{psnr_u8, ssim_u8_simple};

/// Fixed QP values for the quality/bitrate matrix.
pub const SWEEP_QPS: [u8; 5] = [18, 22, 26, 30, 34];

/// Full matrix row count: **5 QPs** × **12** cases/QP  
/// (compact+static: 4; entropy+static/context: 8) × **2** costs × **2** partitions → **5 × 12 = 60**.
pub const SWEEP_FULL_ROW_COUNT: usize = {
    let per_qp = 2 * 2 * (1 + 2);
    SWEEP_QPS.len() * per_qp
};

const PSNR_JSON_SAFE_IDENTICAL_DB: f64 = 100.0;

#[derive(Debug, Error)]
pub enum SweepError {
    #[error("SSIM threshold must be finite and in (0, 1]; got {0}")]
    InvalidSsimThreshold(f64),
    #[error("byte budget must be > 0 for Pareto byte-constrained picks; got {0}")]
    InvalidByteBudget(u64),
    #[error("YUV420p8 requires non-zero even width/height")]
    InvalidDimensions,
    #[error("raw YUV byte length {actual} != expected {expected} for {w}x{h} x {frames} frames")]
    RawSizeMismatch {
        actual: usize,
        expected: usize,
        w: u32,
        h: u32,
        frames: u32,
    },
    #[error("encode sweep row {0}: {1}")]
    Encode(String, #[source] SrsV2Error),
    #[error("decode sweep row {0}: {1}")]
    Decode(String, #[source] SrsV2Error),
    #[error("metrics sweep row {0}: {1}")]
    Metrics(String, #[source] crate::MetricError),
    #[error("rate controller sweep: {0}")]
    RateControl(#[from] libsrs_video::SrsV2RateControlError),
    #[error("{0}")]
    Other(String),
}

/// One cell in the sweep matrix (after [`SweepConfig`] clip / Pareto knobs).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SweepCase {
    pub qp: u8,
    pub inter_syntax: String,
    pub entropy_model: String,
    pub partition_cost_model: String,
    pub inter_partition: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepConfig {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub fps: u32,
    pub keyint: u32,
    pub motion_radius: i16,
    pub reference_frames: u8,
    pub residual_entropy: String,
    /// Minimum SSIM-Y in `(0, 1]` for **`smallest_bytes_ssim_ge`**.
    pub pareto_ssim_threshold: f64,
    /// Maximum total compressed bytes for SSIM/PSNR-under-budget picks.
    pub pareto_byte_budget: u64,
    /// Cap rows for tests / safety (`None` = full matrix = [`SWEEP_FULL_ROW_COUNT`] rows).
    pub max_rows: Option<usize>,
}

impl SweepConfig {
    /// Fields aligned with `bench_srsv2 --sweep-quality-bitrate` (full matrix when `max_rows` is `None`).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_bench_cli(
        width: u32,
        height: u32,
        frames: u32,
        fps: u32,
        keyint: u32,
        motion_radius: i16,
        reference_frames: u8,
        residual_entropy: String,
        pareto_ssim_threshold: f64,
        pareto_byte_budget: u64,
        max_rows: Option<usize>,
    ) -> Self {
        Self {
            width,
            height,
            frames,
            fps,
            keyint,
            motion_radius,
            reference_frames,
            residual_entropy,
            pareto_ssim_threshold,
            pareto_byte_budget,
            max_rows,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepRow {
    pub row_index: u32,
    pub qp: u8,
    pub inter_syntax: String,
    pub entropy_model: String,
    pub partition_cost_model: String,
    pub inter_partition: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub total_bytes: u64,
    pub psnr_y: f64,
    pub ssim_y: f64,
    pub encode_seconds: f64,
    pub decode_seconds: f64,
    pub enc_fps: f64,
    pub dec_fps: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ParetoPoint {
    pub row_index: u32,
    pub label: String,
    pub qp: u8,
    pub inter_syntax: String,
    pub entropy_model: String,
    pub partition_cost_model: String,
    pub inter_partition: String,
    pub total_bytes: u64,
    pub psnr_y: f64,
    pub ssim_y: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ParetoSummary {
    /// Smallest `total_bytes` among successful rows with `ssim_y >= threshold`.
    pub smallest_bytes_ssim_ge_threshold: Option<ParetoPoint>,
    /// Highest `ssim_y` among successful rows with `total_bytes <= byte_budget`.
    pub best_ssim_under_byte_budget: Option<ParetoPoint>,
    /// Highest `psnr_y` among successful rows with `total_bytes <= byte_budget`.
    pub best_psnr_under_byte_budget: Option<ParetoPoint>,
    /// Lexicographic best: maximize SSIM-Y, then PSNR-Y, then minimize bytes (deterministic).
    pub best_bytes_quality_overall: Option<ParetoPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepReport {
    pub note: &'static str,
    pub expected_matrix_rows: usize,
    pub emitted_rows: usize,
    pub pareto_ssim_threshold: f64,
    pub pareto_byte_budget: u64,
    pub rows: Vec<SweepRow>,
    pub pareto: ParetoSummary,
}

/// Enumerate sweep cases in **deterministic** order  
/// (QP → inter syntax → entropy model → partition cost → partition mode).
///
/// `max_rows`: truncate to the first N cases after enumeration (`None` = full [`SWEEP_FULL_ROW_COUNT`] rows).
#[must_use]
pub fn enumerate_sweep_cases(max_rows: Option<usize>) -> Vec<SweepCase> {
    if max_rows == Some(0) {
        return Vec::new();
    }
    let cap = max_rows
        .unwrap_or(SWEEP_FULL_ROW_COUNT)
        .min(SWEEP_FULL_ROW_COUNT);
    let mut v = Vec::with_capacity(cap);
    'outer: for &qp in &SWEEP_QPS {
        for inter in ["compact", "entropy"] {
            for em in ["static", "context"] {
                if em == "context" && inter != "entropy" {
                    continue;
                }
                for pcm in ["header-aware", "rdo-fast"] {
                    for part in ["fixed16x16", "auto-fast"] {
                        v.push(SweepCase {
                            qp,
                            inter_syntax: inter.to_string(),
                            entropy_model: em.to_string(),
                            partition_cost_model: pcm.to_string(),
                            inter_partition: part.to_string(),
                        });
                        if max_rows.is_some_and(|m| v.len() >= m) {
                            break 'outer;
                        }
                    }
                }
            }
        }
    }
    v
}

fn yuv420_frame_bytes(w: u32, h: u32) -> Result<usize, SweepError> {
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(SweepError::InvalidDimensions);
    }
    let y = (w as usize)
        .checked_mul(h as usize)
        .ok_or_else(|| SweepError::Other("Y plane byte count overflow".into()))?;
    Ok(y + (y / 2))
}

fn load_yuv420_frame(
    raw: &[u8],
    frame_bytes: usize,
    fi: u32,
    w: u32,
    h: u32,
) -> Result<YuvFrame, SweepError> {
    let ylen = (w * h) as usize;
    let clen = ((w / 2) * (h / 2)) as usize;
    let start = fi as usize * frame_bytes;
    let frame = raw
        .get(start..start + frame_bytes)
        .ok_or_else(|| SweepError::Other("truncated raw YUV slice".into()))?;
    let yb = &frame[..ylen];
    let ub = &frame[ylen..ylen + clen];
    let vb = &frame[ylen + clen..ylen + 2 * clen];

    let mut y = VideoPlane::<u8>::try_new(w, h, w as usize)
        .map_err(|e| SweepError::Other(format!("Y plane: {e}")))?;
    y.samples.copy_from_slice(yb);
    let mut u = VideoPlane::<u8>::try_new(w / 2, h / 2, (w / 2) as usize)
        .map_err(|e| SweepError::Other(format!("U plane: {e}")))?;
    u.samples.copy_from_slice(ub);
    let mut v = VideoPlane::<u8>::try_new(w / 2, h / 2, (w / 2) as usize)
        .map_err(|e| SweepError::Other(format!("V plane: {e}")))?;

    v.samples.copy_from_slice(vb);

    Ok(YuvFrame {
        format: PixelFormat::Yuv420p8,
        y,
        u,
        v,
    })
}

fn frame_luma_slice(raw: &[u8], frame_bytes: usize, fi: u32, w: u32, h: u32) -> &[u8] {
    let ylen = (w * h) as usize;
    let start = fi as usize * frame_bytes;
    &raw[start..start + ylen]
}

fn avg_ssim_per_frame(
    src_luma: &[u8],
    dec_luma: &[u8],
    w: u32,
    h: u32,
    frames: u32,
) -> Result<f64, crate::MetricError> {
    let ylen = (w * h) as usize;
    let mut acc = 0.0;
    for fi in 0..frames {
        let s = &src_luma[fi as usize * ylen..][..ylen];
        let d = &dec_luma[fi as usize * ylen..][..ylen];
        acc += ssim_u8_simple(s, d, w as usize, h as usize)?;
    }
    Ok(acc / frames.max(1) as f64)
}

fn psnr_y_json_safe(reference: &[u8], measured: &[u8]) -> Result<f64, crate::MetricError> {
    let p = psnr_u8(reference, measured, 255.0)?;
    if p.is_finite() {
        Ok(p)
    } else if p == f64::INFINITY {
        Ok(PSNR_JSON_SAFE_IDENTICAL_DB)
    } else {
        Ok(f64::NAN) // treat as error upstream if needed
    }
}

fn parse_inter(s: &str) -> Result<SrsV2InterSyntaxMode, SweepError> {
    match s.to_ascii_lowercase().as_str() {
        "compact" => Ok(SrsV2InterSyntaxMode::CompactV1),
        "entropy" => Ok(SrsV2InterSyntaxMode::EntropyV1),
        _ => Err(SweepError::Other(format!("invalid inter_syntax {s}"))),
    }
}

fn parse_entropy_model(s: &str) -> Result<SrsV2EntropyModelMode, SweepError> {
    match s.to_ascii_lowercase().as_str() {
        "static" => Ok(SrsV2EntropyModelMode::StaticV1),
        "context" => Ok(SrsV2EntropyModelMode::ContextV1),
        _ => Err(SweepError::Other(format!("invalid entropy_model {s}"))),
    }
}

fn parse_partition(s: &str) -> Result<SrsV2InterPartitionMode, SweepError> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "fixed16x16" | "fixed-16x16" => Ok(SrsV2InterPartitionMode::Fixed16x16),
        "auto-fast" | "autofast" => Ok(SrsV2InterPartitionMode::AutoFast),
        _ => Err(SweepError::Other(format!("invalid inter_partition {s}"))),
    }
}

fn parse_partition_cost(s: &str) -> Result<SrsV2PartitionCostModel, SweepError> {
    match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "header-aware" | "headeraware" => Ok(SrsV2PartitionCostModel::HeaderAware),
        "rdo-fast" | "rdofast" => Ok(SrsV2PartitionCostModel::RdoFast),
        _ => Err(SweepError::Other(format!(
            "invalid partition_cost_model {s}"
        ))),
    }
}

fn parse_residual_entropy_label(s: &str) -> Result<ResidualEntropy, SweepError> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(ResidualEntropy::Auto),
        "explicit" => Ok(ResidualEntropy::Explicit),
        "rans" => Ok(ResidualEntropy::Rans),
        _ => Err(SweepError::Other(format!("invalid residual_entropy {s}"))),
    }
}

fn build_seq_header(cfg: &SweepConfig, settings: &SrsV2EncodeSettings) -> VideoSequenceHeaderV2 {
    let disable_loop_filter = matches!(settings.loop_filter_mode, SrsV2LoopFilterMode::Off);
    VideoSequenceHeaderV2 {
        width: cfg.width,
        height: cfg.height,
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
        max_ref_frames: cfg.reference_frames,
    }
}

fn build_settings_for_case(
    cfg: &SweepConfig,
    case: &SweepCase,
) -> Result<SrsV2EncodeSettings, SweepError> {
    let residual = parse_residual_entropy_label(&cfg.residual_entropy)?;
    let inter = parse_inter(&case.inter_syntax)?;
    let em = parse_entropy_model(&case.entropy_model)?;
    let part = parse_partition(&case.inter_partition)?;
    let pcm = parse_partition_cost(&case.partition_cost_model)?;
    let s = SrsV2EncodeSettings {
        quantizer: case.qp,
        rate_control_mode: SrsV2RateControlMode::FixedQp,
        keyframe_interval: cfg.keyint.max(1),
        motion_search_radius: cfg.motion_radius,
        residual_entropy: residual,
        adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
        aq_strength: 4,
        min_block_qp_delta: -6,
        max_block_qp_delta: 6,
        motion_search_mode: SrsV2MotionSearchMode::ExhaustiveSmall,
        early_exit_sad_threshold: 0,
        enable_skip_blocks: true,
        subpel_mode: SrsV2SubpelMode::Off,
        subpel_refinement_radius: 1,
        block_aq_mode: SrsV2BlockAqMode::Off,
        loop_filter_mode: SrsV2LoopFilterMode::Off,
        deblock_strength: 0,
        b_motion_search_mode: SrsV2BMotionSearchMode::Off,
        b_weighted_prediction: false,
        inter_syntax_mode: inter,
        rdo_mode: SrsV2RdoMode::Off,
        rdo_lambda_scale: 256,
        inter_partition_mode: part,
        transform_size_mode: SrsV2TransformSizeMode::Auto,
        partition_cost_model: pcm,
        partition_map_encoding: SrsV2PartitionMapEncoding::LegacyPerMb,
        entropy_model_mode: em,
        ..Default::default()
    };
    s.validate_rate_control()?;
    validate_adaptive_quant_settings(&s)
        .map_err(|e| SweepError::Other(format!("adaptive_quant: {e}")))?;
    s.validate_entropy_model_inter()
        .map_err(|e| SweepError::Other(format!("entropy_model_inter: {e}")))?;
    Ok(s)
}

fn sweep_row_from_case(
    row_index: u32,
    cfg: &SweepConfig,
    raw: &[u8],
    case: &SweepCase,
) -> Result<SweepRow, SweepError> {
    let fb = yuv420_frame_bytes(cfg.width, cfg.height)?;
    let expected = fb
        .checked_mul(cfg.frames as usize)
        .ok_or_else(|| SweepError::Other("raw YUV total byte count overflow".into()))?;
    if raw.len() != expected {
        return Err(SweepError::RawSizeMismatch {
            actual: raw.len(),
            expected,
            w: cfg.width,
            h: cfg.height,
            frames: cfg.frames,
        });
    }

    let settings = build_settings_for_case(cfg, case)?;
    let seq = build_seq_header(cfg, &settings);
    let mut rc = SrsV2RateController::new(&settings, cfg.fps.max(1), 1)?;
    let t_enc = Instant::now();
    let mut mgr = SrsV2ReferenceManager::new(seq.max_ref_frames)
        .map_err(|e| SweepError::Encode(format!("row {row_index} ref manager"), e))?;
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(cfg.frames as usize);
    let mut prev: Option<PreviousFrameRcStats> = None;
    let mut enc_stats = ResidualEncodeStats::default();

    for fi in 0..cfg.frames {
        let qp = rc.qp_for_frame(fi, prev);
        let frame = load_yuv420_frame(raw, fb, fi, cfg.width, cfg.height)?;
        let mut aq_frame = libsrs_video::SrsV2AqEncodeStats::default();
        let mut motion = libsrs_video::SrsV2MotionEncodeStats::default();
        let payload = encode_yuv420_inter_payload(
            &seq,
            &frame,
            mgr.primary_ref(),
            fi,
            qp,
            &settings,
            Some(&mut enc_stats),
            Some(&mut aq_frame),
            Some(&mut motion),
        )
        .map_err(|e| SweepError::Encode(format!("row {row_index} fi {fi}"), e))?;
        let is_i = matches!(payload.get(3).copied(), Some(1 | 3 | 7));
        rc.observe_frame(fi, payload.len(), is_i);
        decode_yuv420_srsv2_payload_managed(&seq, &payload, &mut mgr)
            .map_err(|e| SweepError::Decode(format!("row {row_index} fi {fi}"), e))?;
        prev = Some(PreviousFrameRcStats {
            encoded_bytes: payload.len() as u32,
            is_keyframe: is_i,
        });
        payloads.push(payload);
    }
    let enc_secs = t_enc.elapsed().as_secs_f64();

    let t_dec = Instant::now();
    let ylen = (cfg.width * cfg.height) as usize;
    let nf = cfg.frames as usize;
    let mut dec_by_frame = vec![vec![0u8; ylen]; nf];
    let mut written = vec![false; nf];
    let mut mgr_dec = SrsV2ReferenceManager::new(seq.max_ref_frames)
        .map_err(|e| SweepError::Decode(format!("row {row_index} decode ref manager"), e))?;
    for pl in &payloads {
        let dec = decode_yuv420_srsv2_payload_managed(&seq, pl, &mut mgr_dec)
            .map_err(|e| SweepError::Decode(format!("row {row_index}"), e))?;
        if !dec.is_displayable {
            continue;
        }
        let idx = dec.frame_index as usize;
        if idx >= nf {
            return Err(SweepError::Other(format!(
                "sweep row {row_index}: decoded frame_index {} out of range",
                dec.frame_index
            )));
        }
        if written[idx] {
            return Err(SweepError::Other(format!(
                "sweep row {row_index}: duplicate decoded frame_index {}",
                dec.frame_index
            )));
        }
        written[idx] = true;
        dec_by_frame[idx] = dec.yuv.y.samples.clone();
    }
    for (idx, filled) in written.iter().enumerate() {
        if !filled {
            return Err(SweepError::Other(format!(
                "sweep row {row_index}: missing decoded frame index {idx}"
            )));
        }
    }
    let mut src_luma = Vec::with_capacity(ylen * nf);
    let mut dec_luma = Vec::with_capacity(ylen * nf);
    for fi in 0..cfg.frames {
        src_luma.extend_from_slice(frame_luma_slice(raw, fb, fi, cfg.width, cfg.height));
        dec_luma.extend_from_slice(&dec_by_frame[fi as usize]);
    }
    let dec_secs = t_dec.elapsed().as_secs_f64();

    let psnr_y = psnr_y_json_safe(&src_luma, &dec_luma)
        .map_err(|e| SweepError::Metrics(format!("row {row_index}"), e))?;
    if !psnr_y.is_finite() {
        return Err(SweepError::Metrics(
            format!("row {row_index}"),
            crate::MetricError::EmptyInput,
        ));
    }
    let ssim_y = avg_ssim_per_frame(&src_luma, &dec_luma, cfg.width, cfg.height, cfg.frames)
        .map_err(|e| SweepError::Metrics(format!("row {row_index}"), e))?;

    let total_bytes: u64 = payloads.iter().map(|p| p.len() as u64).sum();
    let frames_f = cfg.frames.max(1) as f64;
    let enc_fps = frames_f / enc_secs.max(f64::EPSILON);
    let dec_fps = frames_f / dec_secs.max(f64::EPSILON);

    Ok(SweepRow {
        row_index,
        qp: case.qp,
        inter_syntax: case.inter_syntax.clone(),
        entropy_model: case.entropy_model.clone(),
        partition_cost_model: case.partition_cost_model.clone(),
        inter_partition: case.inter_partition.clone(),
        ok: true,
        error: None,
        total_bytes,
        psnr_y,
        ssim_y,
        encode_seconds: enc_secs,
        decode_seconds: dec_secs,
        enc_fps,
        dec_fps,
    })
}

fn row_to_point(r: &SweepRow, label: &str) -> ParetoPoint {
    ParetoPoint {
        row_index: r.row_index,
        label: label.to_string(),
        qp: r.qp,
        inter_syntax: r.inter_syntax.clone(),
        entropy_model: r.entropy_model.clone(),
        partition_cost_model: r.partition_cost_model.clone(),
        inter_partition: r.inter_partition.clone(),
        total_bytes: r.total_bytes,
        psnr_y: r.psnr_y,
        ssim_y: r.ssim_y,
    }
}

/// Compute Pareto-style summaries from successful rows only (deterministic tie-break by `row_index`).
#[must_use]
pub fn compute_pareto_summary(
    rows: &[SweepRow],
    ssim_threshold: f64,
    byte_budget: u64,
) -> ParetoSummary {
    let ok: Vec<&SweepRow> = rows.iter().filter(|r| r.ok).collect();

    let smallest_bytes_ssim_ge_threshold = ok
        .iter()
        .filter(|r| r.ssim_y >= ssim_threshold)
        .min_by_key(|r| (r.total_bytes, r.row_index))
        .map(|r| row_to_point(r, "smallest_bytes_ssim_ge_threshold"));

    let best_ssim_under_byte_budget = ok
        .iter()
        .filter(|r| r.total_bytes <= byte_budget)
        .max_by(|a, b| {
            a.ssim_y
                .partial_cmp(&b.ssim_y)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.total_bytes.cmp(&b.total_bytes).reverse())
                .then_with(|| a.row_index.cmp(&b.row_index))
        })
        .map(|r| row_to_point(r, "best_ssim_under_byte_budget"));

    let best_psnr_under_byte_budget = ok
        .iter()
        .filter(|r| r.total_bytes <= byte_budget)
        .max_by(|a, b| {
            a.psnr_y
                .partial_cmp(&b.psnr_y)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.total_bytes.cmp(&b.total_bytes).reverse())
                .then_with(|| a.row_index.cmp(&b.row_index))
        })
        .map(|r| row_to_point(r, "best_psnr_under_byte_budget"));

    let best_bytes_quality_overall = ok
        .iter()
        .max_by(|a, b| {
            a.ssim_y
                .partial_cmp(&b.ssim_y)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    a.psnr_y
                        .partial_cmp(&b.psnr_y)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.total_bytes.cmp(&b.total_bytes))
                .then_with(|| a.row_index.cmp(&b.row_index))
        })
        .map(|r| row_to_point(r, "best_bytes_quality_overall"));

    ParetoSummary {
        smallest_bytes_ssim_ge_threshold,
        best_ssim_under_byte_budget,
        best_psnr_under_byte_budget,
        best_bytes_quality_overall,
    }
}

/// Validate Pareto knobs (call before running the matrix).
pub fn validate_sweep_pareto(ssim_threshold: f64, byte_budget: u64) -> Result<(), SweepError> {
    if !ssim_threshold.is_finite() || ssim_threshold <= 0.0 || ssim_threshold > 1.0 {
        return Err(SweepError::InvalidSsimThreshold(ssim_threshold));
    }
    if byte_budget == 0 {
        return Err(SweepError::InvalidByteBudget(byte_budget));
    }
    Ok(())
}

/// Run the full quality/bitrate sweep over `raw_yuv420p8`.
pub fn run_quality_bitrate_sweep(
    cfg: &SweepConfig,
    raw_yuv420p8: &[u8],
) -> Result<SweepReport, SweepError> {
    validate_sweep_pareto(cfg.pareto_ssim_threshold, cfg.pareto_byte_budget)?;
    yuv420_frame_bytes(cfg.width, cfg.height)?;

    let cases = enumerate_sweep_cases(cfg.max_rows);

    let mut rows = Vec::with_capacity(cases.len());
    for (i, case) in cases.iter().enumerate() {
        let ri = i as u32;
        match sweep_row_from_case(ri, cfg, raw_yuv420p8, case) {
            Ok(r) => rows.push(r),
            Err(e) => {
                let msg = e.to_string();
                rows.push(SweepRow {
                    row_index: ri,
                    qp: case.qp,
                    inter_syntax: case.inter_syntax.clone(),
                    entropy_model: case.entropy_model.clone(),
                    partition_cost_model: case.partition_cost_model.clone(),
                    inter_partition: case.inter_partition.clone(),
                    ok: false,
                    error: Some(msg),
                    total_bytes: 0,
                    psnr_y: 0.0,
                    ssim_y: 0.0,
                    encode_seconds: 0.0,
                    decode_seconds: 0.0,
                    enc_fps: 0.0,
                    dec_fps: 0.0,
                });
            }
        }
    }

    let expected_matrix_rows = enumerate_sweep_cases(None).len();
    let pareto = compute_pareto_summary(&rows, cfg.pareto_ssim_threshold, cfg.pareto_byte_budget);

    Ok(SweepReport {
        note: "SRSV2 quality/bitrate matrix; engineering measurement only; in-process encoder/decoder only; no H.264 comparison.",
        expected_matrix_rows,
        emitted_rows: rows.len(),
        pareto_ssim_threshold: cfg.pareto_ssim_threshold,
        pareto_byte_budget: cfg.pareto_byte_budget,
        rows,
        pareto,
    })
}

/// Write the sweep report as JSON. The **`rows`** field is the sweep matrix array; **`pareto`** holds summaries.
pub fn write_sweep_json(path: &Path, report: &SweepReport) -> Result<(), SweepError> {
    let s = serde_json::to_string_pretty(report)
        .map_err(|e| SweepError::Other(format!("JSON serialize: {e}")))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| SweepError::Other(format!("mkdir: {e}")))?;
    }
    fs::write(path, s).map_err(|e| SweepError::Other(format!("write json: {e}")))?;
    Ok(())
}

/// Write Markdown: matrix table + Pareto section.
pub fn write_sweep_markdown(path: &Path, report: &SweepReport) -> Result<(), SweepError> {
    let mut out = String::new();
    out.push_str("# SRSV2 quality / bitrate sweep\n\n");
    out.push_str(&format!("_{}_\n\n", report.note));
    out.push_str(&format!(
        "- Expected full-matrix rows: **{}** (emitted **{}**)\n",
        report.expected_matrix_rows, report.emitted_rows
    ));
    out.push_str(&format!(
        "- Pareto SSIM floor: **{:.4}**; byte budget: **{}** bytes\n\n",
        report.pareto_ssim_threshold, report.pareto_byte_budget
    ));

    out.push_str("## Sweep matrix\n\n");
    out.push_str("| # | QP | inter | entropy | part cost | partition | bytes | PSNR-Y | SSIM-Y | enc FPS | dec FPS | ok |\n");
    out.push_str("|---:|---:|---|---|---|---|---:|---:|---:|---:|---:|---|\n");
    for r in &report.rows {
        let err = r
            .error
            .as_ref()
            .map(|e| format!(" `{e}`"))
            .unwrap_or_default();
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {:.2} | {:.4} | {:.2} | {:.2} | {}{} |\n",
            r.row_index,
            r.qp,
            r.inter_syntax,
            r.entropy_model,
            r.partition_cost_model,
            r.inter_partition,
            r.total_bytes,
            r.psnr_y,
            r.ssim_y,
            r.enc_fps,
            r.dec_fps,
            if r.ok { "yes" } else { "no" },
            err
        ));
    }

    out.push_str("\n## Pareto summary\n\n");
    let p = &report.pareto;
    fn fmt_point(title: &str, o: &Option<ParetoPoint>, out: &mut String) {
        match o {
            None => out.push_str(&format!("- **{title}:** *(none)*\n")),
            Some(pt) => {
                out.push_str(&format!(
                    "- **{title}:** row **{}** — QP **{}**, `{}` / `{}` / `{}` / `{}` — **{}** bytes, PSNR-Y **{:.2}**, SSIM-Y **{:.4}** (`{}`)\n",
                    pt.row_index,
                    pt.qp,
                    pt.inter_syntax,
                    pt.entropy_model,
                    pt.partition_cost_model,
                    pt.inter_partition,
                    pt.total_bytes,
                    pt.psnr_y,
                    pt.ssim_y,
                    pt.label
                ));
            }
        }
    }
    fmt_point(
        "Smallest bytes with SSIM ≥ threshold",
        &p.smallest_bytes_ssim_ge_threshold,
        &mut out,
    );
    fmt_point(
        "Best SSIM within byte budget",
        &p.best_ssim_under_byte_budget,
        &mut out,
    );
    fmt_point(
        "Best PSNR within byte budget",
        &p.best_psnr_under_byte_budget,
        &mut out,
    );
    fmt_point(
        "Best overall (SSIM, then PSNR, then fewer bytes)",
        &p.best_bytes_quality_overall,
        &mut out,
    );

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| SweepError::Other(format!("mkdir: {e}")))?;
    }
    fs::write(path, out).map_err(|e| SweepError::Other(format!("write md: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn sweep_row_count_is_deterministic() {
        let v = enumerate_sweep_cases(None);
        assert_eq!(v.len(), SWEEP_FULL_ROW_COUNT);
        assert_eq!(v.len(), 60);
        let v2 = enumerate_sweep_cases(None);
        assert_eq!(v, v2);
        assert_eq!(enumerate_sweep_cases(Some(4)).len(), 4);
        assert!(enumerate_sweep_cases(Some(0)).is_empty());
    }

    #[test]
    fn sweep_case_order_is_fixed() {
        let v = enumerate_sweep_cases(None);
        assert_eq!(v[0].qp, 18);
        assert_eq!(v[0].inter_syntax, "compact");
        assert_eq!(v[0].entropy_model, "static");
        assert_eq!(v[0].partition_cost_model, "header-aware");
        assert_eq!(v[0].inter_partition, "fixed16x16");
        assert_eq!(v[11].qp, 18);
        assert_eq!(v[12].qp, 22);
    }

    #[test]
    fn sweep_source_does_not_spawn_external_transcoder() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("srsv2_sweep.rs");
        let s = fs::read_to_string(&path).expect("read srsv2_sweep.rs");
        assert!(
            !s.contains("Command::new(\"ffmpeg\")"),
            "sweep must not spawn external transcode subprocesses"
        );
    }

    #[test]
    fn compute_pareto_summary_is_deterministic() {
        let rows = vec![
            SweepRow {
                row_index: 0,
                qp: 28,
                inter_syntax: "entropy".into(),
                entropy_model: "static".into(),
                partition_cost_model: "header-aware".into(),
                inter_partition: "fixed16x16".into(),
                ok: true,
                error: None,
                total_bytes: 1000,
                psnr_y: 30.0,
                ssim_y: 0.95,
                encode_seconds: 1.0,
                decode_seconds: 1.0,
                enc_fps: 10.0,
                dec_fps: 10.0,
            },
            SweepRow {
                row_index: 1,
                qp: 28,
                inter_syntax: "entropy".into(),
                entropy_model: "context".into(),
                partition_cost_model: "header-aware".into(),
                inter_partition: "fixed16x16".into(),
                ok: true,
                error: None,
                total_bytes: 900,
                psnr_y: 31.0,
                ssim_y: 0.96,
                encode_seconds: 1.0,
                decode_seconds: 1.0,
                enc_fps: 10.0,
                dec_fps: 10.0,
            },
        ];
        let p1 = compute_pareto_summary(&rows, 0.94, 950);
        let p2 = compute_pareto_summary(&rows, 0.94, 950);
        assert_eq!(
            serde_json::to_string(&p1).unwrap(),
            serde_json::to_string(&p2).unwrap()
        );
        assert_eq!(p1.best_bytes_quality_overall.as_ref().unwrap().row_index, 1);
    }

    #[test]
    fn invalid_ssim_threshold_rejected() {
        assert!(validate_sweep_pareto(0.0, 100).is_err());
        assert!(validate_sweep_pareto(1.5, 100).is_err());
        assert!(validate_sweep_pareto(f64::NAN, 100).is_err());
    }

    #[test]
    fn invalid_byte_budget_rejected() {
        assert!(validate_sweep_pareto(0.9, 0).is_err());
    }

    #[test]
    fn sweep_report_serializes() {
        let r = SweepReport {
            note: "n",
            expected_matrix_rows: 60,
            emitted_rows: 1,
            pareto_ssim_threshold: 0.9,
            pareto_byte_budget: 1,
            rows: vec![],
            pareto: ParetoSummary::default(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("pareto"));
    }

    #[test]
    fn tiny_clip_sweep_bounded_rows() {
        let w = 16u32;
        let h = 16u32;
        let frames = 2u32;
        let fb = yuv420_frame_bytes(w, h).unwrap();
        let raw = vec![128u8; fb * frames as usize];
        let cfg = SweepConfig {
            width: w,
            height: h,
            frames,
            fps: 30,
            keyint: 30,
            motion_radius: 8,
            reference_frames: 1,
            residual_entropy: "explicit".into(),
            pareto_ssim_threshold: 0.5,
            pareto_byte_budget: 1_000_000,
            max_rows: Some(4),
        };
        let rep = run_quality_bitrate_sweep(&cfg, &raw).unwrap();
        assert_eq!(rep.rows.len(), 4);
        assert!(rep.rows.iter().all(|r| r.ok));
    }
}
