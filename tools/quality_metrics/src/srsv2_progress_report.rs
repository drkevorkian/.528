//! Engineering progress summary from existing **`bench_srsv2`** JSON artifacts (no FFmpeg required).
//!
//! - [`build_progress_report`] reads the three primary JSON inputs with [`read_json`] (missing paths or
//!   parse errors fail with [`ProgressReportError`]); optional x264 / B-mode paths add [`ProgressReport::warnings`]
//!   when provided but unreadable.
//! - [`build_progress_report_strict`] also enforces minimal schema (`compare_entropy_models`, `compare_partition_costs`,
//!   `rows`) on those three files. **`bench_srsv2 --h264-progress-summary`** uses [`write_progress_summary_files`]
//!   (strict + writes JSON/Markdown).
//!
//! Consumes:
//! - `--compare-entropy-models` report (`compare_entropy_models[]`)
//! - `--compare-partition-costs` report (`compare_partition_costs[]`)
//! - `--sweep-quality-bitrate` report (`quality_metrics::srsv2_sweep::SweepReport` JSON)
//! - Optional: a primary **`bench_srsv2`** JSON with **`compare-x264`** (`table[]`, `x264`)
//! - Optional: `--compare-b-modes` report for B-half / weighted telemetry (`compare_b_modes[]`)

use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// CLI-aligned input paths for [`build_progress_report`].
#[derive(Debug, Clone)]
pub struct ProgressReportInputs<'a> {
    pub entropy_models_json: &'a Path,
    pub partition_costs_json: &'a Path,
    pub sweep_quality_bitrate_json: &'a Path,
    pub compare_x264_bench_json: Option<&'a Path>,
    pub compare_b_modes_json: Option<&'a Path>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProgressReport {
    pub note: &'static str,
    pub inputs_read: Vec<String>,
    pub warnings: Vec<String>,
    pub questions: ProgressQuestions,
    pub byte_cost_breakdown: ByteCostBreakdown,
    /// Named dominant bucket for remaining compressed bytes (engineering label).
    pub next_bottleneck: String,
    /// Sentence tying dominant bucket to follow-up work (no competitive claims).
    pub next_bottleneck_rationale: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProgressQuestions {
    /// 1. Did ContextV1 reduce total bytes vs StaticV1 (entropy compare rows)?
    pub context_v1_vs_static_v1_bytes: QuestionEntropyModels,
    /// 2. Did RDO / partition telemetry show rejections or byte wins vs sad-only auto-fast?
    pub rdo_partition_behavior: QuestionRdoPartitions,
    /// 3. Did auto-fast beat fixed16x16 in the quality/bitrate sweep matrix?
    pub auto_fast_vs_fixed16_in_sweep: QuestionSweepAutoFast,
    /// 4. B-half / weighted vs integer B (optional compare-b-modes JSON).
    pub b_half_and_weighted: QuestionBModes,
    /// 5. SRSV2 vs x264 at reported bitrates/quality (optional bench JSON).
    pub srsv2_vs_x264: QuestionX264,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuestionEntropyModels {
    pub answered: bool,
    pub static_total_bytes: Option<u64>,
    pub context_total_bytes: Option<u64>,
    pub delta_context_minus_static: Option<i128>,
    pub summary_sentence: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuestionRdoPartitions {
    pub answered: bool,
    pub partition_rejected_by_rdo_total: u64,
    pub partition_rejected_by_header_cost_total: u64,
    pub auto_fast_rdo_bytes: Option<u64>,
    pub auto_fast_sad_bytes: Option<u64>,
    pub rdo_same_or_smaller_bytes_than_sad: Option<bool>,
    pub summary_sentence: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuestionSweepAutoFast {
    pub answered: bool,
    pub comparable_pairs: u32,
    pub auto_fast_smaller_bytes_count: u32,
    pub summary_sentence: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuestionBModes {
    pub answered: bool,
    pub half_smaller_than_int_count: u32,
    pub weighted_smaller_than_int_count: u32,
    pub summary_sentence: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuestionX264 {
    pub answered: bool,
    pub srsv2_bytes: Option<u64>,
    pub x264_bytes: Option<u64>,
    pub srsv2_psnr_y: Option<f64>,
    pub x264_psnr_y: Option<f64>,
    pub srsv2_ssim_y: Option<f64>,
    pub x264_ssim_y: Option<f64>,
    pub bitrate_ratio_srsv2_over_x264: Option<f64>,
    pub summary_sentence: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ByteCostBreakdown {
    pub source_label: String,
    pub total_payload_bytes: Option<u64>,
    pub mv_header_bytes: u64,
    pub inter_residual_bytes: u64,
    pub partition_map_bytes: u64,
    pub transform_syntax_bytes: u64,
    pub poor_prediction_proxy_bytes: u64,
    pub shares: ByteCostShares,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ByteCostShares {
    pub mv_header: f64,
    pub inter_residual: f64,
    pub partition_map: f64,
    pub transform_syntax: f64,
    pub poor_prediction_proxy: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum ProgressReportError {
    #[error("read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("parse JSON {0}: {1}")]
    Json(String, serde_json::Error),
    #[error("required benchmark JSON schema invalid ({path}): {detail}")]
    SchemaInvalid { path: String, detail: String },
}

fn read_json(path: &Path) -> Result<Value, ProgressReportError> {
    let s = fs::read_to_string(path)
        .map_err(|e| ProgressReportError::Io(path.display().to_string(), e))?;
    serde_json::from_str(&s).map_err(|e| ProgressReportError::Json(path.display().to_string(), e))
}

fn validate_entropy_models_schema(v: &Value) -> Result<(), ProgressReportError> {
    match v.get("compare_entropy_models") {
        Some(a) if a.is_array() => Ok(()),
        _ => Err(ProgressReportError::SchemaInvalid {
            path: "entropy_models_json".into(),
            detail: "expected top-level compare_entropy_models array".into(),
        }),
    }
}

fn validate_partition_costs_schema(v: &Value) -> Result<(), ProgressReportError> {
    match v.get("compare_partition_costs") {
        Some(a) if a.is_array() => Ok(()),
        _ => Err(ProgressReportError::SchemaInvalid {
            path: "partition_costs_json".into(),
            detail: "expected top-level compare_partition_costs array".into(),
        }),
    }
}

fn validate_sweep_schema(v: &Value) -> Result<(), ProgressReportError> {
    match v.get("rows") {
        Some(a) if a.is_array() => Ok(()),
        _ => Err(ProgressReportError::SchemaInvalid {
            path: "sweep_quality_bitrate_json".into(),
            detail: "expected top-level rows array".into(),
        }),
    }
}

/// Like [`build_progress_report`], but **requires** the three benchmark JSON inputs to exist, parse,
/// and match minimal **`bench_srsv2`** shapes. Use from CLI (`--h264-progress-summary`) so missing or
/// malformed artifacts **fail fast** instead of emitting a partial report.
pub fn build_progress_report_strict(
    inputs: &ProgressReportInputs<'_>,
) -> Result<ProgressReport, ProgressReportError> {
    let entropy_v = read_json(inputs.entropy_models_json)?;
    validate_entropy_models_schema(&entropy_v)?;
    let part_v = read_json(inputs.partition_costs_json)?;
    validate_partition_costs_schema(&part_v)?;
    let sweep_v = read_json(inputs.sweep_quality_bitrate_json)?;
    validate_sweep_schema(&sweep_v)?;

    let mut warnings = Vec::new();
    let mut inputs_read = vec![
        inputs.entropy_models_json.display().to_string(),
        inputs.partition_costs_json.display().to_string(),
        inputs.sweep_quality_bitrate_json.display().to_string(),
    ];

    let x264_v = if let Some(p) = inputs.compare_x264_bench_json {
        match read_json(p) {
            Ok(v) => {
                inputs_read.push(p.display().to_string());
                Some(v)
            }
            Err(e) => {
                warnings.push(format!(
                    "optional x264 bench JSON unreadable ({}): {e}",
                    p.display()
                ));
                None
            }
        }
    } else {
        None
    };

    let b_v = if let Some(p) = inputs.compare_b_modes_json {
        match read_json(p) {
            Ok(v) => {
                inputs_read.push(p.display().to_string());
                Some(v)
            }
            Err(e) => {
                warnings.push(format!(
                    "optional compare-b-modes JSON unreadable ({}): {e}",
                    p.display()
                ));
                None
            }
        }
    } else {
        None
    };

    let q1 = answer_entropy(Some(&entropy_v));
    let q2 = answer_rdo(Some(&part_v));
    let q3 = answer_sweep_auto_fast(Some(&sweep_v));
    let q4 = answer_b_modes(b_v.as_ref());
    let q5 = answer_x264(x264_v.as_ref());

    let breakdown = byte_breakdown_from_partition_report(Some(&part_v));
    let (next_bottleneck, next_rationale) = select_next_bottleneck(&breakdown);

    Ok(ProgressReport {
        note: "Engineering measurement only; not a marketing claim.",
        inputs_read,
        warnings,
        questions: ProgressQuestions {
            context_v1_vs_static_v1_bytes: q1,
            rdo_partition_behavior: q2,
            auto_fast_vs_fixed16_in_sweep: q3,
            b_half_and_weighted: q4,
            srsv2_vs_x264: q5,
        },
        byte_cost_breakdown: breakdown,
        next_bottleneck,
        next_bottleneck_rationale: next_rationale,
    })
}

fn val_u64(v: &Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for p in path {
        cur = match cur.get(*p) {
            Some(x) => x,
            None => return 0,
        };
    }
    cur.as_u64().unwrap_or(0)
}

/// Build the engineering summary.
///
/// The entropy-model, partition-cost, and quality/bitrate sweep reports are required inputs:
/// missing or malformed files fail clearly. Optional x264 and B-mode inputs only add warnings.
pub fn build_progress_report(
    inputs: &ProgressReportInputs<'_>,
) -> Result<ProgressReport, ProgressReportError> {
    let mut warnings = Vec::new();
    let mut inputs_read: Vec<String> = Vec::new();

    let entropy_v = read_json(inputs.entropy_models_json)?;
    inputs_read.push(inputs.entropy_models_json.display().to_string());

    let part_v = read_json(inputs.partition_costs_json)?;
    inputs_read.push(inputs.partition_costs_json.display().to_string());

    let sweep_v = read_json(inputs.sweep_quality_bitrate_json)?;
    inputs_read.push(inputs.sweep_quality_bitrate_json.display().to_string());

    let x264_v = if let Some(p) = inputs.compare_x264_bench_json {
        match read_json(p) {
            Ok(v) => {
                inputs_read.push(p.display().to_string());
                Some(v)
            }
            Err(e) => {
                warnings.push(format!(
                    "optional x264 bench JSON unreadable ({}): {e}",
                    p.display()
                ));
                None
            }
        }
    } else {
        None
    };

    let b_v = if let Some(p) = inputs.compare_b_modes_json {
        match read_json(p) {
            Ok(v) => {
                inputs_read.push(p.display().to_string());
                Some(v)
            }
            Err(e) => {
                warnings.push(format!(
                    "optional compare-b-modes JSON unreadable ({}): {e}",
                    p.display()
                ));
                None
            }
        }
    } else {
        None
    };

    let q1 = answer_entropy(Some(&entropy_v));
    let q2 = answer_rdo(Some(&part_v));
    let q3 = answer_sweep_auto_fast(Some(&sweep_v));
    let q4 = answer_b_modes(b_v.as_ref());
    let q5 = answer_x264(x264_v.as_ref());

    let breakdown = byte_breakdown_from_partition_report(Some(&part_v));
    let (next_bottleneck, next_rationale) = select_next_bottleneck(&breakdown);

    Ok(ProgressReport {
        note: "Engineering measurement only; not a marketing claim.",
        inputs_read,
        warnings,
        questions: ProgressQuestions {
            context_v1_vs_static_v1_bytes: q1,
            rdo_partition_behavior: q2,
            auto_fast_vs_fixed16_in_sweep: q3,
            b_half_and_weighted: q4,
            srsv2_vs_x264: q5,
        },
        byte_cost_breakdown: breakdown,
        next_bottleneck,
        next_bottleneck_rationale: next_rationale,
    })
}

fn answer_entropy(report: Option<&Value>) -> QuestionEntropyModels {
    let Some(report) = report else {
        return QuestionEntropyModels {
            answered: false,
            static_total_bytes: None,
            context_total_bytes: None,
            delta_context_minus_static: None,
            summary_sentence: "No entropy-model compare JSON; skipped.".to_string(),
        };
    };
    let Some(arr) = report
        .get("compare_entropy_models")
        .and_then(|x| x.as_array())
    else {
        return QuestionEntropyModels {
            answered: false,
            static_total_bytes: None,
            context_total_bytes: None,
            delta_context_minus_static: None,
            summary_sentence: "JSON missing compare_entropy_models[].".to_string(),
        };
    };
    let mut static_b: Option<u64> = None;
    let mut context_b: Option<u64> = None;
    for e in arr {
        if !e.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let mode = e
            .get("entropy_model_mode")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let b = e
            .get("row")
            .and_then(|r| r.get("bytes"))
            .and_then(|x| x.as_u64());
        let Some(b) = b else { continue };
        match mode {
            "static" => static_b = Some(b),
            "context" => context_b = Some(b),
            _ => {}
        }
    }
    let (Some(s), Some(c)) = (static_b, context_b) else {
        return QuestionEntropyModels {
            answered: false,
            static_total_bytes: static_b,
            context_total_bytes: context_b,
            delta_context_minus_static: None,
            summary_sentence: "Could not find ok rows for both static and context entropy models."
                .to_string(),
        };
    };
    let delta = i128::from(c) - i128::from(s);
    let summary = if delta < 0 {
        format!(
            "ContextV1 total payload bytes ({c}) are lower than StaticV1 ({s}) by {} bytes on this compare.",
            -delta
        )
    } else if delta > 0 {
        format!(
            "ContextV1 total payload bytes ({c}) exceed StaticV1 ({s}) by {delta} bytes on this compare."
        )
    } else {
        format!("ContextV1 and StaticV1 total payload bytes tie ({s}) on this compare.")
    };
    QuestionEntropyModels {
        answered: true,
        static_total_bytes: Some(s),
        context_total_bytes: Some(c),
        delta_context_minus_static: Some(delta),
        summary_sentence: summary,
    }
}

fn answer_rdo(report: Option<&Value>) -> QuestionRdoPartitions {
    let Some(report) = report else {
        return QuestionRdoPartitions {
            answered: false,
            partition_rejected_by_rdo_total: 0,
            partition_rejected_by_header_cost_total: 0,
            auto_fast_rdo_bytes: None,
            auto_fast_sad_bytes: None,
            rdo_same_or_smaller_bytes_than_sad: None,
            summary_sentence: "No partition-cost compare JSON; skipped.".to_string(),
        };
    };
    let Some(arr) = report
        .get("compare_partition_costs")
        .and_then(|x| x.as_array())
    else {
        return QuestionRdoPartitions {
            answered: false,
            partition_rejected_by_rdo_total: 0,
            partition_rejected_by_header_cost_total: 0,
            auto_fast_rdo_bytes: None,
            auto_fast_sad_bytes: None,
            rdo_same_or_smaller_bytes_than_sad: None,
            summary_sentence: "JSON missing compare_partition_costs[].".to_string(),
        };
    };
    let mut rdo_rej = 0u64;
    let mut hdr_rej = 0u64;
    let mut rdo_b: Option<u64> = None;
    let mut sad_b: Option<u64> = None;
    for e in arr {
        if !e.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let label = e.get("label").and_then(|x| x.as_str()).unwrap_or("");
        let det = e.get("details").unwrap_or(&Value::Null);
        if label.contains("auto-fast-rdo") {
            rdo_rej += val_u64(det, &["partition", "partition_rejected_by_rdo"]);
            hdr_rej += val_u64(det, &["partition", "partition_rejected_by_header_cost"]);
            rdo_b = e
                .get("row")
                .and_then(|r| r.get("bytes"))
                .and_then(|x| x.as_u64());
        }
        if label.contains("auto-fast-sad") {
            sad_b = e
                .get("row")
                .and_then(|r| r.get("bytes"))
                .and_then(|x| x.as_u64());
        }
    }
    let cmp = match (rdo_b, sad_b) {
        (Some(r), Some(s)) => Some(r <= s),
        _ => None,
    };
    let summary = format!(
        "RDO rejected split/partition candidates {} times (header-cost rejects {}). Auto-fast RDO total bytes: {:?}; auto-fast sad-only: {:?}; RDO same or smaller than sad-only: {:?}.",
        rdo_rej, hdr_rej, rdo_b, sad_b, cmp
    );
    QuestionRdoPartitions {
        answered: rdo_b.is_some() || rdo_rej > 0,
        partition_rejected_by_rdo_total: rdo_rej,
        partition_rejected_by_header_cost_total: hdr_rej,
        auto_fast_rdo_bytes: rdo_b,
        auto_fast_sad_bytes: sad_b,
        rdo_same_or_smaller_bytes_than_sad: cmp,
        summary_sentence: summary,
    }
}

fn answer_sweep_auto_fast(report: Option<&Value>) -> QuestionSweepAutoFast {
    let Some(report) = report else {
        return QuestionSweepAutoFast {
            answered: false,
            comparable_pairs: 0,
            auto_fast_smaller_bytes_count: 0,
            summary_sentence: "No sweep-quality-bitrate JSON; skipped.".to_string(),
        };
    };
    let Some(rows) = report.get("rows").and_then(|x| x.as_array()) else {
        return QuestionSweepAutoFast {
            answered: false,
            comparable_pairs: 0,
            auto_fast_smaller_bytes_count: 0,
            summary_sentence: "Sweep JSON missing rows[].".to_string(),
        };
    };
    use std::collections::BTreeMap;
    type SliceKey = (String, u8, String, String);
    let mut fixed: BTreeMap<SliceKey, u64> = BTreeMap::new();
    let mut auto: BTreeMap<SliceKey, u64> = BTreeMap::new();
    for r in rows {
        if !r.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let qp = r.get("qp").and_then(|x| x.as_u64()).unwrap_or(0) as u8;
        let inter = r
            .get("inter_syntax")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let em = r
            .get("entropy_model")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let pcm = r
            .get("partition_cost_model")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let part = r
            .get("inter_partition")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let tb = r.get("total_bytes").and_then(|x| x.as_u64());
        let Some(tb) = tb else { continue };
        let k: SliceKey = (inter, qp, em, pcm);
        if part == "fixed16x16" {
            fixed.insert(k, tb);
        } else if part == "auto-fast" {
            auto.insert(k, tb);
        }
    }
    let mut pairs = 0u32;
    let mut wins = 0u32;
    for (k, ab) in &auto {
        if let Some(fb) = fixed.get(k) {
            pairs += 1;
            if ab < fb {
                wins += 1;
            }
        }
    }
    let summary = if pairs == 0 {
        "Sweep did not yield comparable fixed16x16 vs auto-fast rows.".to_string()
    } else if wins > 0 {
        format!(
            "In {pairs} comparable sweep slices (QP/inter/entropy/part-cost), auto-fast total_bytes was smaller than fixed16x16 in {wins} slices."
        )
    } else {
        format!(
            "In {pairs} comparable sweep slices, auto-fast never beat fixed16x16 on total_bytes (ties possible)."
        )
    };
    QuestionSweepAutoFast {
        answered: pairs > 0,
        comparable_pairs: pairs,
        auto_fast_smaller_bytes_count: wins,
        summary_sentence: summary,
    }
}

fn answer_b_modes(report: Option<&Value>) -> QuestionBModes {
    let Some(report) = report else {
        return QuestionBModes {
            answered: false,
            half_smaller_than_int_count: 0,
            weighted_smaller_than_int_count: 0,
            summary_sentence: "Optional compare-b-modes JSON not provided or unreadable; skipped."
                .to_string(),
        };
    };
    let Some(arr) = report.get("compare_b_modes").and_then(|x| x.as_array()) else {
        return QuestionBModes {
            answered: false,
            half_smaller_than_int_count: 0,
            weighted_smaller_than_int_count: 0,
            summary_sentence: "JSON missing compare_b_modes[].".to_string(),
        };
    };
    let mut int_b: Option<u64> = None;
    let mut half_b: Option<u64> = None;
    let mut wgt_b: Option<u64> = None;
    for e in arr {
        if e.get("error").and_then(|x| x.as_str()).is_some() {
            continue;
        }
        let mode = e.get("mode").and_then(|x| x.as_str()).unwrap_or("");
        let b = e
            .get("row")
            .and_then(|r| r.get("bytes"))
            .and_then(|x| x.as_u64());
        let Some(b) = b else { continue };
        match mode {
            "SRSV2-B-int" => int_b = Some(b),
            "SRSV2-B-half" => half_b = Some(b),
            "SRSV2-B-weighted" => wgt_b = Some(b),
            _ => {}
        }
    }
    let half_win = matches!((half_b, int_b), (Some(h), Some(i)) if h < i);
    let wgt_win = matches!((wgt_b, int_b), (Some(w), Some(i)) if w < i);
    let summary = format!(
        "B-int bytes {:?}; B-half {:?}; B-weighted {:?}. Half lower than int: {}; weighted lower than int: {}.",
        int_b, half_b, wgt_b, half_win, wgt_win
    );
    QuestionBModes {
        answered: int_b.is_some() && (half_b.is_some() || wgt_b.is_some()),
        half_smaller_than_int_count: u32::from(half_win),
        weighted_smaller_than_int_count: u32::from(wgt_win),
        summary_sentence: summary,
    }
}

/// Prefer the primary **`SRSV2`** row when present; otherwise the smallest-byte **SRSV2\*** row
/// without an error field (handles `--compare-b-modes` tables that list multiple SRSV2 variants).
fn pick_primary_srsv2_table_row(tab: &[Value]) -> Option<&Value> {
    let mut exact: Option<&Value> = None;
    let mut best: Option<&Value> = None;
    let mut best_bytes: u64 = u64::MAX;
    for row in tab {
        if row.get("error").and_then(|x| x.as_str()).is_some() {
            continue;
        }
        let codec = row.get("codec").and_then(|x| x.as_str()).unwrap_or("");
        let c_lower = codec.to_ascii_lowercase();
        if c_lower.contains("x264") {
            continue;
        }
        if !codec.starts_with("SRSV2") {
            continue;
        }
        let b = row.get("bytes").and_then(|x| x.as_u64());
        let Some(b) = b else { continue };
        if codec == "SRSV2" {
            exact = Some(row);
            break;
        }
        if b < best_bytes {
            best_bytes = b;
            best = Some(row);
        }
    }
    exact.or(best)
}

fn pick_x264_table_row(tab: &[Value]) -> Option<&Value> {
    let mut best: Option<&Value> = None;
    let mut best_bytes: u64 = u64::MAX;
    for row in tab {
        if row.get("error").and_then(|x| x.as_str()).is_some() {
            continue;
        }
        let codec = row.get("codec").and_then(|x| x.as_str()).unwrap_or("");
        let c_lower = codec.to_ascii_lowercase();
        if !(c_lower.contains("x264") || codec == "libx264") {
            continue;
        }
        let Some(b) = row.get("bytes").and_then(|x| x.as_u64()) else {
            continue;
        };
        if b < best_bytes {
            best_bytes = b;
            best = Some(row);
        }
    }
    best
}

fn answer_x264(report: Option<&Value>) -> QuestionX264 {
    let Some(report) = report else {
        return QuestionX264 {
            answered: false,
            srsv2_bytes: None,
            x264_bytes: None,
            srsv2_psnr_y: None,
            x264_psnr_y: None,
            srsv2_ssim_y: None,
            x264_ssim_y: None,
            bitrate_ratio_srsv2_over_x264: None,
            summary_sentence: "No primary bench JSON with x264 compare; skipped.".to_string(),
        };
    };
    let mut srs_b: Option<u64> = None;
    let mut srs_psnr: Option<f64> = None;
    let mut srs_ssim: Option<f64> = None;
    let mut x264_b: Option<u64> = None;
    let mut x264_psnr: Option<f64> = None;
    let mut x264_ssim: Option<f64> = None;
    if let Some(tab) = report.get("table").and_then(|x| x.as_array()) {
        if let Some(row) = pick_primary_srsv2_table_row(tab) {
            srs_b = row.get("bytes").and_then(|x| x.as_u64());
            srs_psnr = row.get("psnr_y").and_then(|x| x.as_f64());
            srs_ssim = row.get("ssim_y").and_then(|x| x.as_f64());
        }
        if let Some(row) = pick_x264_table_row(tab) {
            x264_b = row.get("bytes").and_then(|x| x.as_u64());
            x264_psnr = row.get("psnr_y").and_then(|x| x.as_f64());
            x264_ssim = row.get("ssim_y").and_then(|x| x.as_f64());
        }
    }
    let ratio = match (srs_b, x264_b) {
        (Some(s), Some(x)) if x > 0 => Some(s as f64 / x as f64),
        _ => None,
    };
    let summary = match (srs_b, x264_b, srs_psnr, x264_psnr, srs_ssim, x264_ssim) {
        (Some(s), Some(x), sp, xp, ss, xs) => format!(
            "SRSV2 total bytes {s}, x264 total bytes {x}; PSNR-Y {:.2} vs {:.2}; SSIM-Y {:.4} vs {:.4}. Bitrate ratio SRSV2/x264 payload bytes: {:?}.",
            sp.unwrap_or(f64::NAN),
            xp.unwrap_or(f64::NAN),
            ss.unwrap_or(f64::NAN),
            xs.unwrap_or(f64::NAN),
            ratio
        ),
        _ => "Could not extract SRSV2 and x264 rows from table[].".to_string(),
    };
    QuestionX264 {
        answered: srs_b.is_some() && x264_b.is_some(),
        srsv2_bytes: srs_b,
        x264_bytes: x264_b,
        srsv2_psnr_y: srs_psnr,
        x264_psnr_y: x264_psnr,
        srsv2_ssim_y: srs_ssim,
        x264_ssim_y: x264_ssim,
        bitrate_ratio_srsv2_over_x264: ratio,
        summary_sentence: summary,
    }
}

fn byte_breakdown_from_partition_report(report: Option<&Value>) -> ByteCostBreakdown {
    let default = ByteCostBreakdown {
        source_label: String::new(),
        total_payload_bytes: None,
        mv_header_bytes: 0,
        inter_residual_bytes: 0,
        partition_map_bytes: 0,
        transform_syntax_bytes: 0,
        poor_prediction_proxy_bytes: 0,
        shares: ByteCostShares {
            mv_header: 0.0,
            inter_residual: 0.0,
            partition_map: 0.0,
            transform_syntax: 0.0,
            poor_prediction_proxy: 0.0,
        },
    };
    let Some(report) = report else {
        return default;
    };
    let Some(arr) = report
        .get("compare_partition_costs")
        .and_then(|x| x.as_array())
    else {
        return default;
    };
    for e in arr {
        let label = e.get("label").and_then(|x| x.as_str()).unwrap_or("");
        if !label.contains("auto-fast-rdo")
            || !e.get("ok").and_then(|x| x.as_bool()).unwrap_or(false)
        {
            continue;
        }
        let det = match e.get("details") {
            Some(d) => d,
            None => continue,
        };
        let mv_e = val_u64(det, &["mv_entropy_bytes"]);
        let mv_c = val_u64(det, &["mv_compact_bytes"]);
        let hdr = val_u64(det, &["inter_header_bytes"]);
        let res = val_u64(det, &["inter_residual_bytes"]);
        let pm = val_u64(det, &["partition", "partition_map_bytes"]);
        let tx = val_u64(det, &["partition", "partition_header_bytes"]);
        let total_row = e
            .get("row")
            .and_then(|r| r.get("bytes"))
            .and_then(|x| x.as_u64());
        let mv_header = mv_e.saturating_add(mv_c).saturating_add(hdr);
        let accounted = mv_header
            .saturating_add(res)
            .saturating_add(pm)
            .saturating_add(tx);
        let total_guess = total_row.unwrap_or(accounted);
        let poor = total_guess.saturating_sub(accounted);
        let denom = total_guess.max(1) as f64;
        return ByteCostBreakdown {
            source_label: label.to_string(),
            total_payload_bytes: total_row,
            mv_header_bytes: mv_header,
            inter_residual_bytes: res,
            partition_map_bytes: pm,
            transform_syntax_bytes: tx,
            poor_prediction_proxy_bytes: poor,
            shares: ByteCostShares {
                mv_header: mv_header as f64 / denom,
                inter_residual: res as f64 / denom,
                partition_map: pm as f64 / denom,
                transform_syntax: tx as f64 / denom,
                poor_prediction_proxy: poor as f64 / denom,
            },
        };
    }
    default
}

/// Tie-break order when shares are equal: fixed lexicographic id order (deterministic).
const BOTTLENECK_IDS: [&str; 5] = [
    "mv_header",
    "inter_residual",
    "partition_map",
    "transform_syntax",
    "poor_prediction_proxy",
];

fn select_next_bottleneck(b: &ByteCostBreakdown) -> (String, String) {
    let mut candidates: Vec<(usize, &str, f64)> = vec![
        (0, BOTTLENECK_IDS[0], b.shares.mv_header),
        (1, BOTTLENECK_IDS[1], b.shares.inter_residual),
        (2, BOTTLENECK_IDS[2], b.shares.partition_map),
        (3, BOTTLENECK_IDS[3], b.shares.transform_syntax),
        (4, BOTTLENECK_IDS[4], b.shares.poor_prediction_proxy),
    ];
    candidates.sort_by(|a, c| {
        c.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&c.0))
    });
    let win = candidates[0];
    let name = win.1.to_string();
    let rationale = match win.1 {
        "mv_header" => {
            "Largest share of measured payload is MV packing plus inter header bytes; next investigation targets MV entropy/coding efficiency.".to_string()
        }
        "inter_residual" => {
            "Largest share is frame-level residual bytes; prediction/error signal or quant tuning likely dominates.".to_string()
        }
        "partition_map" => {
            "Largest share is partition map bytes; partition decision wiring or map coding may dominate.".to_string()
        }
        "transform_syntax" => {
            "Largest share is partition/transform header bytes; transform decision syntax cost is prominent.".to_string()
        }
        _ => {
            "Largest share is unallocated payload vs summed buckets (containers, slice headers, other syntax); treat as miscellaneous overhead until instrumented.".to_string()
        }
    };
    (name, rationale)
}

/// Write JSON + Markdown summary files (creates parent directories).
///
/// Uses [`build_progress_report_strict`] so invalid or missing required bench JSON fails before writing.
pub fn write_progress_summary_files(
    inputs: &ProgressReportInputs<'_>,
    out_json: &Path,
    out_md: &Path,
) -> Result<ProgressReport, ProgressReportError> {
    let rep = build_progress_report_strict(inputs)?;
    if let Some(p) = out_json.parent() {
        fs::create_dir_all(p).map_err(|e| ProgressReportError::Io(p.display().to_string(), e))?;
    }
    if let Some(p) = out_md.parent() {
        fs::create_dir_all(p).map_err(|e| ProgressReportError::Io(p.display().to_string(), e))?;
    }
    let js = serde_json::to_string_pretty(&rep)
        .map_err(|e| ProgressReportError::Json("progress report serialize".into(), e))?;
    fs::write(out_json, js)
        .map_err(|e| ProgressReportError::Io(out_json.display().to_string(), e))?;
    fs::write(out_md, progress_report_markdown(&rep))
        .map_err(|e| ProgressReportError::Io(out_md.display().to_string(), e))?;
    Ok(rep)
}

fn progress_report_markdown(rep: &ProgressReport) -> String {
    let mut out = String::new();
    out.push_str("# SRSV2 engineering progress summary\n\n");
    out.push_str("_Engineering facts only; not a competitive marketing claim._\n\n");
    out.push_str("## Inputs\n\n");
    for p in &rep.inputs_read {
        out.push_str(&format!("- `{p}`\n"));
    }
    if !rep.warnings.is_empty() {
        out.push_str("\n### Warnings\n\n");
        for w in &rep.warnings {
            out.push_str(&format!("- {w}\n"));
        }
    }
    out.push_str("\n## Answers\n\n");
    out.push_str("### 1. ContextV1 vs StaticV1 (total bytes)\n\n");
    out.push_str(&rep.questions.context_v1_vs_static_v1_bytes.summary_sentence);
    out.push_str("\n\n### 2. RDO vs partition choices\n\n");
    out.push_str(&rep.questions.rdo_partition_behavior.summary_sentence);
    out.push_str("\n\n### 3. Auto-fast vs fixed16×16 (sweep)\n\n");
    out.push_str(&rep.questions.auto_fast_vs_fixed16_in_sweep.summary_sentence);
    out.push_str("\n\n### 4. B-half / weighted B\n\n");
    out.push_str(&rep.questions.b_half_and_weighted.summary_sentence);
    out.push_str("\n\n### 5. SRSV2 vs x264 (same bench JSON)\n\n");
    out.push_str(&rep.questions.srsv2_vs_x264.summary_sentence);
    out.push_str(
        "\n\n### 6. Largest remaining byte cost (bottleneck label)\n\n\
Engineering bucket with the largest share of the instrumented partition-cost row (see snapshot below). \
Labels match `next_bottleneck` in JSON (`mv_header`, `inter_residual`, `partition_map`, `transform_syntax`, `poor_prediction_proxy`).\n\n",
    );
    out.push_str(&format!(
        "**{}** — {}\n",
        rep.next_bottleneck, rep.next_bottleneck_rationale
    ));
    out.push_str("\n## Byte-cost snapshot (auto-fast RDO row when available)\n\n");
    let b = &rep.byte_cost_breakdown;
    out.push_str(&format!(
        "- Source row: `{}`\n- Total payload bytes (row): {:?}\n",
        if b.source_label.is_empty() {
            "(none)"
        } else {
            &b.source_label
        },
        b.total_payload_bytes
    ));
    out.push_str(&format!(
        "- MV/header (mv_entropy+mv_compact+inter_header): {}\n",
        b.mv_header_bytes
    ));
    out.push_str(&format!(
        "- Inter residual: {}\n- Partition map: {}\n- Transform/partition header syntax: {}\n- Other / unbucketed vs row total: {}\n",
        b.inter_residual_bytes,
        b.partition_map_bytes,
        b.transform_syntax_bytes,
        b.poor_prediction_proxy_bytes
    ));
    out.push_str("\n### Shares (of row total bytes)\n\n");
    out.push_str(&format!(
        "| Bucket | Share |\n|---|---:|\n| mv_header | {:.4} |\n| inter_residual | {:.4} |\n| partition_map | {:.4} |\n| transform_syntax | {:.4} |\n| other_overhead | {:.4} |\n",
        b.shares.mv_header,
        b.shares.inter_residual,
        b.shares.partition_map,
        b.shares.transform_syntax,
        b.shares.poor_prediction_proxy
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_partition_json() -> Value {
        serde_json::json!({
            "compare_partition_costs": [
                {
                    "label": "SRSV2-pc-auto-fast-rdo",
                    "ok": true,
                    "row": { "bytes": 10000u64 },
                    "details": {
                        "mv_entropy_bytes": 100u64,
                        "mv_compact_bytes": 200u64,
                        "inter_header_bytes": 50u64,
                        "inter_residual_bytes": 5000u64,
                        "partition": {
                            "partition_map_bytes": 80u64,
                            "partition_header_bytes": 60u64,
                            "partition_rejected_by_rdo": 5u64,
                            "partition_rejected_by_header_cost": 2u64
                        }
                    }
                },
                {
                    "label": "SRSV2-pc-auto-fast-sad",
                    "ok": true,
                    "row": { "bytes": 10200u64 }
                }
            ]
        })
    }

    fn sample_entropy_json() -> Value {
        serde_json::json!({
            "compare_entropy_models": [
                {
                    "entropy_model_mode": "static",
                    "ok": true,
                    "row": { "bytes": 1200u64 }
                },
                {
                    "entropy_model_mode": "context",
                    "ok": true,
                    "row": { "bytes": 1100u64 }
                }
            ]
        })
    }

    fn sample_sweep_json() -> Value {
        serde_json::json!({
            "rows": [
                {
                    "ok": true,
                    "qp": 28u64,
                    "inter_syntax": "compact",
                    "entropy_model": "static",
                    "partition_cost_model": "rdo-fast",
                    "inter_partition": "fixed16x16",
                    "total_bytes": 1300u64
                },
                {
                    "ok": true,
                    "qp": 28u64,
                    "inter_syntax": "compact",
                    "entropy_model": "static",
                    "partition_cost_model": "rdo-fast",
                    "inter_partition": "auto-fast",
                    "total_bytes": 1250u64
                }
            ]
        })
    }

    fn write_json(path: &Path, v: &Value) {
        std::fs::write(path, serde_json::to_string(v).unwrap()).unwrap();
    }

    #[test]
    fn report_serializes() {
        let p = sample_partition_json();
        let b = byte_breakdown_from_partition_report(Some(&p));
        let (name, _) = select_next_bottleneck(&b);
        assert_eq!(name, "inter_residual");
        let r = ProgressReport {
            note: "test",
            inputs_read: vec![],
            warnings: vec![],
            questions: ProgressQuestions {
                context_v1_vs_static_v1_bytes: QuestionEntropyModels {
                    answered: false,
                    static_total_bytes: None,
                    context_total_bytes: None,
                    delta_context_minus_static: None,
                    summary_sentence: String::new(),
                },
                rdo_partition_behavior: QuestionRdoPartitions {
                    answered: true,
                    partition_rejected_by_rdo_total: 5,
                    partition_rejected_by_header_cost_total: 2,
                    auto_fast_rdo_bytes: Some(10000),
                    auto_fast_sad_bytes: Some(10200),
                    rdo_same_or_smaller_bytes_than_sad: Some(true),
                    summary_sentence: "ok".into(),
                },
                auto_fast_vs_fixed16_in_sweep: QuestionSweepAutoFast {
                    answered: false,
                    comparable_pairs: 0,
                    auto_fast_smaller_bytes_count: 0,
                    summary_sentence: String::new(),
                },
                b_half_and_weighted: QuestionBModes {
                    answered: false,
                    half_smaller_than_int_count: 0,
                    weighted_smaller_than_int_count: 0,
                    summary_sentence: String::new(),
                },
                srsv2_vs_x264: QuestionX264 {
                    answered: false,
                    srsv2_bytes: None,
                    x264_bytes: None,
                    srsv2_psnr_y: None,
                    x264_psnr_y: None,
                    srsv2_ssim_y: None,
                    x264_ssim_y: None,
                    bitrate_ratio_srsv2_over_x264: None,
                    summary_sentence: String::new(),
                },
            },
            byte_cost_breakdown: b.clone(),
            next_bottleneck: name.clone(),
            next_bottleneck_rationale: "r".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("inter_residual"));
    }

    #[test]
    fn missing_required_entropy_report_fails() {
        let inputs = ProgressReportInputs {
            entropy_models_json: Path::new("/nonexistent/entropy.json"),
            partition_costs_json: Path::new("/nonexistent/part.json"),
            sweep_quality_bitrate_json: Path::new("/nonexistent/sweep.json"),
            compare_x264_bench_json: None,
            compare_b_modes_json: None,
        };
        let err = build_progress_report(&inputs).unwrap_err().to_string();
        assert!(err.contains("entropy.json"), "{err}");
        let err_s = build_progress_report_strict(&inputs)
            .unwrap_err()
            .to_string();
        assert!(err_s.contains("entropy.json"), "{err_s}");
    }

    #[test]
    fn strict_rejects_bad_entropy_shape() {
        let dir = std::env::temp_dir();
        let id = std::process::id();
        let e = dir.join(format!("qm_strict_e_{id}.json"));
        let p = dir.join(format!("qm_strict_p_{id}.json"));
        let sw = dir.join(format!("qm_strict_s_{id}.json"));
        std::fs::write(&e, "{}").unwrap();
        std::fs::write(&p, r#"{"compare_partition_costs":[]}"#).unwrap();
        std::fs::write(&sw, r#"{"rows":[]}"#).unwrap();
        let inputs = ProgressReportInputs {
            entropy_models_json: &e,
            partition_costs_json: &p,
            sweep_quality_bitrate_json: &sw,
            compare_x264_bench_json: None,
            compare_b_modes_json: None,
        };
        assert!(matches!(
            build_progress_report_strict(&inputs),
            Err(ProgressReportError::SchemaInvalid { .. })
        ));
        let _ = std::fs::remove_file(e);
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(sw);
    }

    #[test]
    fn strict_accepts_minimal_valid_shapes() {
        let dir = std::env::temp_dir();
        let id = std::process::id();
        let e = dir.join(format!("qm_strict_ok_e_{id}.json"));
        let p = dir.join(format!("qm_strict_ok_p_{id}.json"));
        let sw = dir.join(format!("qm_strict_ok_s_{id}.json"));
        std::fs::write(&e, r#"{"compare_entropy_models":[]}"#).unwrap();
        std::fs::write(&p, r#"{"compare_partition_costs":[]}"#).unwrap();
        std::fs::write(&sw, r#"{"rows":[]}"#).unwrap();
        let inputs = ProgressReportInputs {
            entropy_models_json: &e,
            partition_costs_json: &p,
            sweep_quality_bitrate_json: &sw,
            compare_x264_bench_json: None,
            compare_b_modes_json: None,
        };
        build_progress_report_strict(&inputs).unwrap();
        let _ = std::fs::remove_file(e);
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(sw);
    }

    #[test]
    fn missing_x264_optional_handled() {
        let dir =
            std::env::temp_dir().join(format!("srsv2-progress-no-x264-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entropy = dir.join("entropy.json");
        let part = dir.join("partition.json");
        let sweep = dir.join("sweep.json");
        write_json(&entropy, &sample_entropy_json());
        write_json(&part, &sample_partition_json());
        write_json(&sweep, &sample_sweep_json());
        let inputs = ProgressReportInputs {
            entropy_models_json: &entropy,
            partition_costs_json: &part,
            sweep_quality_bitrate_json: &sweep,
            compare_x264_bench_json: None,
            compare_b_modes_json: None,
        };
        let r = build_progress_report(&inputs).unwrap();
        assert!(
            !r.warnings
                .iter()
                .any(|w| w.contains("compare-x264 bench JSON not provided")),
            "omitted optional x264 path should not spam warnings"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn valid_required_reports_serialize_summary() {
        let dir =
            std::env::temp_dir().join(format!("srsv2-progress-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entropy = dir.join("entropy.json");
        let part = dir.join("partition.json");
        let sweep = dir.join("sweep.json");
        write_json(&entropy, &sample_entropy_json());
        write_json(&part, &sample_partition_json());
        write_json(&sweep, &sample_sweep_json());

        let inputs = ProgressReportInputs {
            entropy_models_json: &entropy,
            partition_costs_json: &part,
            sweep_quality_bitrate_json: &sweep,
            compare_x264_bench_json: None,
            compare_b_modes_json: None,
        };
        let out_json = dir.join("summary_out.json");
        let out_md = dir.join("summary_out.md");
        let r = write_progress_summary_files(&inputs, &out_json, &out_md).unwrap();
        assert_eq!(r.next_bottleneck, "inter_residual");
        assert!(r.questions.context_v1_vs_static_v1_bytes.answered);
        assert!(r.questions.auto_fast_vs_fixed16_in_sweep.answered);
        let disk = std::fs::read_to_string(&out_json).unwrap();
        assert!(disk.contains("next_bottleneck"));
        assert!(std::fs::read_to_string(&out_md)
            .unwrap()
            .contains("### 6. Largest remaining byte cost"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_optional_b_modes_report_warns() {
        let dir =
            std::env::temp_dir().join(format!("srsv2-progress-report-bm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entropy = dir.join("entropy.json");
        let part = dir.join("partition.json");
        let sweep = dir.join("sweep.json");
        let bm = dir.join("missing-b-modes.json");
        write_json(&entropy, &sample_entropy_json());
        write_json(&part, &sample_partition_json());
        write_json(&sweep, &sample_sweep_json());

        let inputs = ProgressReportInputs {
            entropy_models_json: &entropy,
            partition_costs_json: &part,
            sweep_quality_bitrate_json: &sweep,
            compare_x264_bench_json: None,
            compare_b_modes_json: Some(&bm),
        };
        let r = build_progress_report(&inputs).unwrap();
        assert!(!r.questions.b_half_and_weighted.answered);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("compare-b-modes") || w.contains("b-modes")),
            "{:?}",
            r.warnings
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_optional_x264_report_warns() {
        let dir =
            std::env::temp_dir().join(format!("srsv2-progress-report-opt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entropy = dir.join("entropy.json");
        let part = dir.join("partition.json");
        let sweep = dir.join("sweep.json");
        let x264 = dir.join("missing-x264.json");
        write_json(&entropy, &sample_entropy_json());
        write_json(&part, &sample_partition_json());
        write_json(&sweep, &sample_sweep_json());

        let inputs = ProgressReportInputs {
            entropy_models_json: &entropy,
            partition_costs_json: &part,
            sweep_quality_bitrate_json: &sweep,
            compare_x264_bench_json: Some(&x264),
            compare_b_modes_json: None,
        };
        let r = build_progress_report(&inputs).unwrap();
        assert!(!r.questions.srsv2_vs_x264.answered);
        assert!(r.warnings.iter().any(|w| w.contains("x264")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn answer_entropy_static_vs_context_delta() {
        let v = serde_json::json!({
            "compare_entropy_models": [
                {"ok": true, "entropy_model_mode": "static", "row": {"bytes": 1000u64}},
                {"ok": true, "entropy_model_mode": "context", "row": {"bytes": 950u64}}
            ]
        });
        let q = answer_entropy(Some(&v));
        assert!(q.answered);
        assert_eq!(q.static_total_bytes, Some(1000));
        assert_eq!(q.context_total_bytes, Some(950));
        assert_eq!(q.delta_context_minus_static, Some(-50));
        assert!(q.summary_sentence.contains("lower"));
    }

    #[test]
    fn answer_x264_prefers_exact_srsv2_row() {
        let v = serde_json::json!({
            "table": [
                {"codec": "SRSV2-B-int", "bytes": 100u64, "psnr_y": 30.0, "ssim_y": 0.9},
                {"codec": "SRSV2", "bytes": 200u64, "psnr_y": 29.0, "ssim_y": 0.89},
                {"codec": "x264 ref", "bytes": 150u64, "psnr_y": 31.0, "ssim_y": 0.91}
            ]
        });
        let q = answer_x264(Some(&v));
        assert!(q.answered);
        assert_eq!(q.srsv2_bytes, Some(200));
        assert_eq!(q.x264_bytes, Some(150));
    }

    #[test]
    fn answer_x264_falls_back_to_smallest_srsv2_variant() {
        let v = serde_json::json!({
            "table": [
                {"codec": "SRSV2-B-int", "bytes": 400u64},
                {"codec": "SRSV2-P-only", "bytes": 300u64},
                {"codec": "libx264", "bytes": 350u64}
            ]
        });
        let q = answer_x264(Some(&v));
        assert!(q.answered);
        assert_eq!(q.srsv2_bytes, Some(300));
        assert_eq!(q.x264_bytes, Some(350));
    }

    #[test]
    fn bottleneck_tie_break_deterministic() {
        let b = ByteCostBreakdown {
            source_label: "x".into(),
            total_payload_bytes: Some(100),
            mv_header_bytes: 25,
            inter_residual_bytes: 25,
            partition_map_bytes: 25,
            transform_syntax_bytes: 25,
            poor_prediction_proxy_bytes: 0,
            shares: ByteCostShares {
                mv_header: 0.25,
                inter_residual: 0.25,
                partition_map: 0.25,
                transform_syntax: 0.25,
                poor_prediction_proxy: 0.0,
            },
        };
        let (n1, _) = select_next_bottleneck(&b);
        let (n2, _) = select_next_bottleneck(&b);
        assert_eq!(n1, n2);
        assert_eq!(n1, "mv_header");
    }
}
