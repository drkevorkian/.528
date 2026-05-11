//! Transform size and coefficient **layout** decisions for SRSV2 (experimental).
//!
//! This module owns **coefficient order and layout** choices. It sits **above**
//! [`crate::srsv2::dct`] (DCT math only — no transforms implemented here) and **before**
//! [`crate::srsv2::residual_entropy`] (entropy coding). Encoder wiring may call these helpers later;
//! this block does not change emitted bitstreams by itself.
//!
//! ## Layout vs entropy
//! - **Natural order** depends on [`SrsV2TransformKind`] (8×8 raster vs four 4×4 quadrants).
//! - **Scan order** permutes coefficients for analysis or future serialization.
//! - [`SrsV2CoeffLayoutMode::CompactV1`] is an experimental `S2TL` container for sizing / tooling.
//!
//! ## Transform grouping (spatial residual → transform choice)
//! - [`SrsV2TransformGrouping`] selects **single 8×8**, **four 4×4**, or **auto** from **spatial**
//!   residual samples only (Laplacian / quadrant-variance proxies — **no DCT** in this module).
//! - Quantized spectral coefficients, once produced elsewhere, pair with [`choose_transform_and_scan`]
//!   and [`estimate_transform_grouping_bytes`] for scan + sizing telemetry.
//! - Rev34 MB framing estimates: [`estimate_single8x8_cost`], [`estimate_four4x4_cost`],
//!   [`compare_single8x8_vs_four4x4`] (dual-quant wire proxies); coefficient-only footprint compare:
//!   [`compare_coeff_layout_single_vs_four4x4`].

use super::dct::{ZIGZAG, ZIGZAG_4X4};

/// **COEFFICIENTS_PER_LAYOUT_BLOCK** — one 8×8 macroblock region as a flat `[i16; 64]`.
pub const COEFFICIENTS_PER_LAYOUT_BLOCK: usize = 64;

const COMPACT_MAGIC: &[u8; 4] = b"S2TL";
const COMPACT_VERSION: u8 = 1;

/// Transform granularity for one 8×8 luma region (`64` coefficients).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrsV2TransformKind {
    /// Four separate 4×4 transforms; coefficients stored quadrant-by-quadrant (row-major 4×4 each).
    Tx4x4,
    /// Single 8×8 transform; coefficients stored row-major 8×8.
    Tx8x8,
}

/// Scan pattern over coefficients after choosing [`SrsV2TransformKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrsV2CoeffScan {
    /// Classical MPEG-style zigzag within the transform partition.
    ZigZag,
    /// Low spatial frequency first (Manhattan distance from DC on the 8×8 grid).
    GroupedLowFirst,
    /// Nonzero coefficients first (zigzag order among nonzeros), then zeros — invertible given values.
    RunOptimized,
}

/// Wire / sizing mode for [`estimate_coeff_layout_bytes`] and compact helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrsV2CoeffLayoutMode {
    /// Naïve per-coefficient footprint model (telemetry only).
    Legacy,
    /// Sparse-aware bitmap + explicit values (`encode_coeff_layout_compact_v1`).
    CompactV1,
}

/// Lightweight counters for telemetry / summaries.
#[derive(Debug, Clone, Default)]
pub struct TransformLayoutStats {
    pub coeffs_total: usize,
    pub nonzero_count: usize,
    pub estimated_legacy_bytes: usize,
    pub estimated_compact_v1_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformLayoutError {
    /// Wrong magic bytes for compact layout.
    BadMagic,
    /// Unsupported container version.
    UnsupportedVersion(u8),
    /// Coefficient slice length mismatch for transform kind.
    InvalidCoefficientCount { expected: usize, got: usize },
    /// Unexpected end of buffer.
    Truncated,
    /// Enum tag out of range.
    InvalidDiscriminant(&'static str, u8),
    /// Declared nonzero count inconsistent with bitmap.
    InconsistentNonzeroCount {
        bitmap_popcount: u32,
        payload_pairs: usize,
    },
}

/// Resolved transform + scan for a block (no bitstream binding yet).
#[derive(Debug, Clone, PartialEq)]
pub struct TransformDecision {
    pub kind: SrsV2TransformKind,
    pub scan: SrsV2CoeffScan,
}

/// Pixel / spectral hints for [`choose_transform_kind`] (caller-supplied).
#[derive(Debug, Clone)]
pub struct TransformDecisionInput {
    /// Spatial variance of the residual or pixel block (larger ⇒ more detail).
    pub spatial_variance: f64,
    /// High-frequency energy proxy (e.g. sum of |AC| after a coarse transform); optional.
    pub hf_energy: f64,
    pub max_abs_coeff: i16,
    pub nonzero_count: usize,
}

/// Thresholds for [`choose_transform_kind`].
#[derive(Debug, Clone)]
pub struct TransformDecisionConfig {
    /// If `spatial_variance` **≥** this, prefer [`SrsV2TransformKind::Tx4x4`] for “high detail”.
    pub variance_select_tx4x4: f64,
    /// If `hf_energy` **≥** this, also steer toward [`SrsV2TransformKind::Tx4x4`].
    pub hf_energy_select_tx4x4: f64,
}

impl Default for TransformDecisionConfig {
    fn default() -> Self {
        Self {
            variance_select_tx4x4: 400.0,
            hf_energy_select_tx4x4: 5_000.0,
        }
    }
}

/// Policy for one **8×8** luma residual region: single spectral shape vs four quadrants (no DCT here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrsV2TransformGrouping {
    /// Force [`SrsV2TransformKind::Tx8x8`] coefficient layout.
    Single8x8,
    /// Force [`SrsV2TransformKind::Tx4x4`] quadrant coefficient layout.
    Four4x4,
    /// Resolve [`SrsV2TransformKind`] from spatial residual proxies ([`spatial_residual_to_transform_input`]).
    AutoByResidual,
}

/// Spatial-domain residual shape bucket (telemetry / thresholds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResidualBlockClass {
    Zero,
    DcOnly,
    Sparse,
    Dense,
}

/// Per-block or aggregated byte-estimate accounting for transform grouping experiments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransformGroupingStats {
    pub single_8x8_blocks: u64,
    pub four_4x4_blocks: u64,
    pub zero_blocks: u64,
    pub dc_only_blocks: u64,
    pub sparse_blocks: u64,
    pub dense_blocks: u64,
    pub estimated_legacy_bytes: u64,
    pub estimated_grouped_bytes: u64,
    pub estimated_savings_bytes: i64,
    pub estimated_savings_percent: f64,
}

/// Legacy-model byte estimates for the **same** natural `64` coefficients under **Tx8×8** vs **Tx4×4** layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SingleVsFourByteEstimate {
    pub bytes_single_8x8: usize,
    pub bytes_four_4x4: usize,
    /// [`SrsV2TransformGrouping::Single8x8`] or [`SrsV2TransformGrouping::Four4x4`] only; ties prefer **Single8x8**.
    pub prefers: SrsV2TransformGrouping,
}

/// Fixed **`FR2` rev34** intra macroblock prefix before the compact coefficient body:
/// **`grouping` + `scan` + `pred` + `u16` body length**.
pub const REV34_INTRA_MB_PREFIX_BYTES: usize = 5;

/// Nominal extra side bytes modeled per **4×4** quadrant for [`SrsV2TransformKind::Tx4x4`] so Four4×4 must pay for itself.
pub const FOUR4X4_PER_SUBBLOCK_SIDE_BYTES: usize = 1;

/// Byte accounting for one transform-grouping candidate on rev34-style intra MB framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupingWireCostBreakdown {
    pub grouping_tag_bytes: usize,
    pub scan_tag_bytes: usize,
    pub pred_byte_bytes: usize,
    pub body_length_field_bytes: usize,
    pub compact_coeff_body_bytes: usize,
    pub legacy_residual_entropy_estimate_bytes: usize,
    pub per_subblock_overhead_bytes: usize,
}

impl GroupingWireCostBreakdown {
    /// Sum of all buckets for λ·rate (compact wire + legacy entropy proxy + explicit subblock overhead).
    #[must_use]
    pub fn total_rdo_rate_bytes(&self) -> i64 {
        let sum = self
            .grouping_tag_bytes
            .saturating_add(self.scan_tag_bytes)
            .saturating_add(self.pred_byte_bytes)
            .saturating_add(self.body_length_field_bytes)
            .saturating_add(self.compact_coeff_body_bytes)
            .saturating_add(self.per_subblock_overhead_bytes)
            .saturating_add(self.legacy_residual_entropy_estimate_bytes);
        i64::try_from(sum).unwrap_or(i64::MAX)
    }
}

/// Rev34-style wire comparison between **already quantized** Tx8×8 vs Tx4×4 candidates (distinct scans allowed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleVsFourRev34Compare {
    pub breakdown_tx8: GroupingWireCostBreakdown,
    pub breakdown_tx4: GroupingWireCostBreakdown,
    pub rate_tx8: i64,
    pub rate_tx4: i64,
    /// Rate ties prefer [`SrsV2TransformGrouping::Single8x8`].
    pub prefers: SrsV2TransformGrouping,
}

impl TransformGroupingStats {
    /// Recompute [`Self::estimated_savings_percent`] from accumulated byte totals.
    pub fn finalize_estimated_savings_percent(&mut self) {
        self.estimated_savings_percent = if self.estimated_legacy_bytes > 0 {
            100.0 * self.estimated_savings_bytes as f64 / self.estimated_legacy_bytes as f64
        } else {
            0.0
        };
    }
}

/// **`FR2` rev34** intra per-MB grouping tag: four **4×4** transforms ([`SrsV2TransformKind::Tx4x4`] layout).
pub const FR2_REV34_INTRA_GROUPING_FOUR4X4: u8 = 0;
/// **`FR2` rev34** intra per-MB grouping tag: single **8×8** transform ([`SrsV2TransformKind::Tx8x8`] layout).
pub const FR2_REV34_INTRA_GROUPING_SINGLE8X8: u8 = 1;

#[inline]
pub fn fr2_rev34_grouping_wire_from_transform_kind(kind: SrsV2TransformKind) -> u8 {
    match kind {
        SrsV2TransformKind::Tx4x4 => FR2_REV34_INTRA_GROUPING_FOUR4X4,
        SrsV2TransformKind::Tx8x8 => FR2_REV34_INTRA_GROUPING_SINGLE8X8,
    }
}

/// Decode rev34 grouping tag → [`SrsV2TransformKind`] (same numeric mapping as rev32 mixed transform byte).
pub fn fr2_rev34_transform_kind_from_grouping_wire(
    b: u8,
) -> Result<SrsV2TransformKind, TransformLayoutError> {
    match b {
        FR2_REV34_INTRA_GROUPING_FOUR4X4 => Ok(SrsV2TransformKind::Tx4x4),
        FR2_REV34_INTRA_GROUPING_SINGLE8X8 => Ok(SrsV2TransformKind::Tx8x8),
        _ => Err(TransformLayoutError::InvalidDiscriminant(
            "fr2_rev34_intra_grouping",
            b,
        )),
    }
}

/// Wire grouping byte for **`FR2` rev35** **P** residuals — same values as [`fr2_rev34_grouping_wire_from_transform_kind`].
#[inline]
pub fn fr2_rev35_p_grouping_wire_from_transform_kind(kind: SrsV2TransformKind) -> u8 {
    fr2_rev34_grouping_wire_from_transform_kind(kind)
}

/// Decode rev35 **P** grouping tag → [`SrsV2TransformKind`] (distinct error field from intra rev34 for clearer diagnostics).
pub fn fr2_rev35_p_transform_kind_from_grouping_wire(
    b: u8,
) -> Result<SrsV2TransformKind, TransformLayoutError> {
    match b {
        FR2_REV34_INTRA_GROUPING_FOUR4X4 => Ok(SrsV2TransformKind::Tx4x4),
        FR2_REV34_INTRA_GROUPING_SINGLE8X8 => Ok(SrsV2TransformKind::Tx8x8),
        _ => Err(TransformLayoutError::InvalidDiscriminant(
            "fr2_rev35_p_grouping",
            b,
        )),
    }
}

impl std::fmt::Display for TransformLayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransformLayoutError::BadMagic => write!(f, "bad S2TL magic"),
            TransformLayoutError::UnsupportedVersion(v) => {
                write!(f, "unsupported transform-layout version {v}")
            }
            TransformLayoutError::InvalidCoefficientCount { expected, got } => {
                write!(f, "expected {expected} coefficients, got {got}")
            }
            TransformLayoutError::Truncated => write!(f, "truncated layout payload"),
            TransformLayoutError::InvalidDiscriminant(field, v) => {
                write!(f, "invalid {field} discriminant {v}")
            }
            TransformLayoutError::InconsistentNonzeroCount {
                bitmap_popcount,
                payload_pairs,
            } => write!(
                f,
                "bitmap popcount {bitmap_popcount} != payload coefficient count {payload_pairs}"
            ),
        }
    }
}

impl std::error::Error for TransformLayoutError {}

fn expect_coeff_len(coeffs: &[i16], kind: SrsV2TransformKind) -> Result<(), TransformLayoutError> {
    let n = coeffs.len();
    if n != COEFFICIENTS_PER_LAYOUT_BLOCK {
        return Err(TransformLayoutError::InvalidCoefficientCount {
            expected: COEFFICIENTS_PER_LAYOUT_BLOCK,
            got: n,
        });
    }
    let _ = kind;
    Ok(())
}

/// Zigzag permutation in **natural index** space for [`SrsV2TransformKind`].
fn zigzag_perm(kind: SrsV2TransformKind) -> [usize; COEFFICIENTS_PER_LAYOUT_BLOCK] {
    match kind {
        SrsV2TransformKind::Tx8x8 => ZIGZAG,
        SrsV2TransformKind::Tx4x4 => {
            let mut p = [0usize; COEFFICIENTS_PER_LAYOUT_BLOCK];
            let mut s = 0usize;
            for q in 0..4 {
                let base = q * 16;
                for &zz in ZIGZAG_4X4.iter() {
                    p[s] = base + zz;
                    s += 1;
                }
            }
            p
        }
    }
}

/// `(row, col)` for natural 8×8 raster index.
#[inline]
fn raster_rc(idx: usize) -> (usize, usize) {
    (idx / 8, idx % 8)
}

/// Manhattan AC frequency proxy from DC (excluding DC cell itself).
#[inline]
fn manhattan_ac(idx: usize) -> usize {
    let (r, c) = raster_rc(idx);
    r + c
}

fn grouped_low_first_perm(kind: SrsV2TransformKind) -> [usize; COEFFICIENTS_PER_LAYOUT_BLOCK] {
    match kind {
        SrsV2TransformKind::Tx8x8 => {
            let mut idxs: Vec<usize> = (0..64).collect();
            idxs.sort_by(|&a, &b| {
                let da = if a == 0 {
                    (0, 0, 0)
                } else {
                    (1, manhattan_ac(a), a)
                };
                let db = if b == 0 {
                    (0, 0, 0)
                } else {
                    (1, manhattan_ac(b), b)
                };
                da.cmp(&db)
            });
            let mut p = [0usize; 64];
            for (s, &nat) in idxs.iter().enumerate() {
                p[s] = nat;
            }
            p
        }
        SrsV2TransformKind::Tx4x4 => {
            let mut p = [0usize; 64];
            let mut s = 0usize;
            for q in 0..4 {
                let base = q * 16;
                let mut local: Vec<usize> = (0..16).map(|k| base + k).collect();
                local.sort_by(|&a, &b| {
                    let la = a - base;
                    let lb = b - base;
                    let (ra, ca) = (la / 4, la % 4);
                    let (rb, cb) = (lb / 4, lb % 4);
                    let da = if la == 0 { (0, 0, 0) } else { (1, ra + ca, la) };
                    let db = if lb == 0 { (0, 0, 0) } else { (1, rb + cb, lb) };
                    da.cmp(&db)
                });
                for nat in local {
                    p[s] = nat;
                    s += 1;
                }
            }
            p
        }
    }
}

/// Zigzag index order for RunOptimized tie-breaking within nonzero / zero groups.
fn zigzag_order_indices(kind: SrsV2TransformKind) -> [usize; COEFFICIENTS_PER_LAYOUT_BLOCK] {
    zigzag_perm(kind)
}

fn run_optimized_perm(
    coeffs: &[i16],
    kind: SrsV2TransformKind,
) -> Result<[usize; COEFFICIENTS_PER_LAYOUT_BLOCK], TransformLayoutError> {
    expect_coeff_len(coeffs, kind)?;
    let zz = zigzag_order_indices(kind);
    let mut nonzero: Vec<usize> = Vec::new();
    let mut zeros: Vec<usize> = Vec::new();
    for &zi in zz.iter() {
        if coeffs[zi] != 0 {
            nonzero.push(zi);
        } else {
            zeros.push(zi);
        }
    }
    let mut p = [0usize; COEFFICIENTS_PER_LAYOUT_BLOCK];
    for (s, nat) in nonzero.into_iter().chain(zeros.into_iter()).enumerate() {
        p[s] = nat;
    }
    Ok(p)
}

fn scan_permutation(
    coeffs: &[i16],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<[usize; COEFFICIENTS_PER_LAYOUT_BLOCK], TransformLayoutError> {
    match scan {
        SrsV2CoeffScan::ZigZag => Ok(zigzag_perm(kind)),
        SrsV2CoeffScan::GroupedLowFirst => Ok(grouped_low_first_perm(kind)),
        SrsV2CoeffScan::RunOptimized => run_optimized_perm(coeffs, kind),
    }
}

/// Pick transform size from spatial / spectral heuristics.
pub fn choose_transform_kind(
    input: &TransformDecisionInput,
    cfg: &TransformDecisionConfig,
) -> SrsV2TransformKind {
    let detail = input.spatial_variance >= cfg.variance_select_tx4x4
        || input.hf_energy >= cfg.hf_energy_select_tx4x4;
    if detail {
        SrsV2TransformKind::Tx4x4
    } else {
        SrsV2TransformKind::Tx8x8
    }
}

/// Pick a scan using simple coefficient statistics.
pub fn choose_coeff_scan(
    kind: SrsV2TransformKind,
    coeffs: &[i16],
) -> Result<SrsV2CoeffScan, TransformLayoutError> {
    expect_coeff_len(coeffs, kind)?;
    let zz = zigzag_perm(kind);
    let mut nz = 0usize;
    let mut trailing_z = 0usize;
    for &zi in zz.iter().rev() {
        if coeffs[zi] != 0 {
            break;
        }
        trailing_z += 1;
    }
    for &zi in zz.iter() {
        if coeffs[zi] != 0 {
            nz += 1;
        }
    }
    if trailing_z >= 48 {
        Ok(SrsV2CoeffScan::RunOptimized)
    } else if nz <= 8 {
        Ok(SrsV2CoeffScan::GroupedLowFirst)
    } else {
        Ok(SrsV2CoeffScan::ZigZag)
    }
}

// --- Spatial residual proxies (no DCT) and transform-grouping API -------------------------------

const SPATIAL_SPARSE_NZ_MAX: usize = 10;
const SPATIAL_SPARSE_VAR_MAX: f64 = 120.0;
const QUADRANT_HF_SCALE: f64 = 64.0;

fn spatial_variance_8x8(block: &[[i16; 8]; 8]) -> f64 {
    let mut sum = 0.0f64;
    let mut sum2 = 0.0f64;
    for row in block.iter() {
        for &cell in row.iter() {
            let x = cell as f64;
            sum += x;
            sum2 += x * x;
        }
    }
    let n = 64.0;
    let mean = sum / n;
    (sum2 / n - mean * mean).max(0.0)
}

fn spatial_laplacian_energy(block: &[[i16; 8]; 8]) -> f64 {
    let mut acc = 0.0f64;
    for r in 0..8 {
        for c in 0..8 {
            let v = block[r][c] as f64;
            let mut edge = 0.0f64;
            let mut k = 0u32;
            if r > 0 {
                edge += (v - block[r - 1][c] as f64).abs();
                k += 1;
            }
            if r < 7 {
                edge += (v - block[r + 1][c] as f64).abs();
                k += 1;
            }
            if c > 0 {
                edge += (v - block[r][c - 1] as f64).abs();
                k += 1;
            }
            if c < 7 {
                edge += (v - block[r][c + 1] as f64).abs();
                k += 1;
            }
            if k > 0 {
                acc += edge / k as f64;
            }
        }
    }
    acc
}

fn max_quadrant_variance(block: &[[i16; 8]; 8]) -> f64 {
    let mut maxv = 0.0f64;
    for q in 0..4 {
        let r0 = (q / 2) * 4;
        let c0 = (q % 2) * 4;
        let mut sum = 0.0f64;
        let mut sum2 = 0.0f64;
        for dr in 0..4 {
            for dc in 0..4 {
                let x = block[r0 + dr][c0 + dc] as f64;
                sum += x;
                sum2 += x * x;
            }
        }
        let n = 16.0f64;
        let mean = sum / n;
        let var = (sum2 / n - mean * mean).max(0.0);
        maxv = maxv.max(var);
    }
    maxv
}

/// Build [`TransformDecisionInput`] from an **8×8** spatial residual (integer samples).
///
/// HF proxy blends Laplacian edge energy with **`max quadrant variance × scale`** so localized
/// detail can steer toward [`SrsV2TransformKind::Tx4x4`] without running a transform in-module.
pub fn spatial_residual_to_transform_input(block: &[[i16; 8]; 8]) -> TransformDecisionInput {
    let spatial_variance = spatial_variance_8x8(block);
    let lap = spatial_laplacian_energy(block);
    let qmax = max_quadrant_variance(block);
    let hf_energy = lap.max(qmax * QUADRANT_HF_SCALE);
    let mut max_abs = 0_i16;
    let mut nonzero_count = 0usize;
    for row in block.iter() {
        for &v in row.iter() {
            max_abs = max_abs.max(v.abs());
            if v != 0 {
                nonzero_count += 1;
            }
        }
    }
    TransformDecisionInput {
        spatial_variance,
        hf_energy,
        max_abs_coeff: max_abs,
        nonzero_count,
    }
}

/// Classify spatial residual shape (deterministic thresholds).
pub fn classify_residual_block(spatial: &[[i16; 8]; 8]) -> ResidualBlockClass {
    let nz = spatial.iter().flatten().filter(|&&v| v != 0).count();
    if nz == 0 {
        return ResidualBlockClass::Zero;
    }
    let v0 = spatial[0][0];
    let constant = spatial.iter().all(|row| row.iter().all(|&v| v == v0));
    if constant {
        return ResidualBlockClass::DcOnly;
    }
    let var = spatial_variance_8x8(spatial);
    if nz <= SPATIAL_SPARSE_NZ_MAX && var < SPATIAL_SPARSE_VAR_MAX {
        ResidualBlockClass::Sparse
    } else {
        ResidualBlockClass::Dense
    }
}

/// Resolve [`SrsV2TransformKind`] from grouping policy and spatial residual (no coefficients required).
pub fn choose_transform_grouping(
    grouping: SrsV2TransformGrouping,
    spatial: &[[i16; 8]; 8],
    cfg: &TransformDecisionConfig,
) -> SrsV2TransformKind {
    match grouping {
        SrsV2TransformGrouping::Single8x8 => SrsV2TransformKind::Tx8x8,
        SrsV2TransformGrouping::Four4x4 => SrsV2TransformKind::Tx4x4,
        SrsV2TransformGrouping::AutoByResidual => {
            let inp = spatial_residual_to_transform_input(spatial);
            choose_transform_kind(&inp, cfg)
        }
    }
}

/// Resolve [`TransformDecision`] using spatial grouping policy + **caller-supplied natural coefficients**
/// for the resolved transform kind (must match encoder-side quantization layout).
pub fn choose_transform_and_scan(
    grouping: SrsV2TransformGrouping,
    spatial: &[[i16; 8]; 8],
    coeffs_natural: &[i16],
    cfg: &TransformDecisionConfig,
) -> Result<TransformDecision, TransformLayoutError> {
    let kind = choose_transform_grouping(grouping, spatial, cfg);
    expect_coeff_len(coeffs_natural, kind)?;
    let scan = choose_coeff_scan(kind, coeffs_natural)?;
    Ok(TransformDecision { kind, scan })
}

/// Byte estimates for one block: [`SrsV2CoeffLayoutMode::Legacy`] vs [`SrsV2CoeffLayoutMode::CompactV1`]
/// (“grouped” sparse packaging), plus population counters for [`classify_residual_block`] / transform kind.
pub fn estimate_transform_grouping_bytes(
    spatial: &[[i16; 8]; 8],
    coeffs_natural: &[i16],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<TransformGroupingStats, TransformLayoutError> {
    expect_coeff_len(coeffs_natural, kind)?;
    let leg = estimate_coeff_layout_bytes(coeffs_natural, kind, scan, SrsV2CoeffLayoutMode::Legacy)?
        as u64;
    let grp =
        estimate_coeff_layout_bytes(coeffs_natural, kind, scan, SrsV2CoeffLayoutMode::CompactV1)?
            as u64;
    let savings = leg as i64 - grp as i64;
    let pct = if leg > 0 {
        100.0 * savings as f64 / leg as f64
    } else {
        0.0
    };
    let mut stats = TransformGroupingStats {
        estimated_legacy_bytes: leg,
        estimated_grouped_bytes: grp,
        estimated_savings_bytes: savings,
        estimated_savings_percent: pct,
        ..Default::default()
    };
    match classify_residual_block(spatial) {
        ResidualBlockClass::Zero => stats.zero_blocks = 1,
        ResidualBlockClass::DcOnly => stats.dc_only_blocks = 1,
        ResidualBlockClass::Sparse => stats.sparse_blocks = 1,
        ResidualBlockClass::Dense => stats.dense_blocks = 1,
    }
    match kind {
        SrsV2TransformKind::Tx8x8 => stats.single_8x8_blocks = 1,
        SrsV2TransformKind::Tx4x4 => stats.four_4x4_blocks = 1,
    }
    Ok(stats)
}

/// Compare heuristic byte footprint for **Tx8×8** vs **Tx4×4** interpretations of the **same** natural `64` coeffs.
pub fn compare_coeff_layout_single_vs_four4x4(
    coeffs_natural: &[i16],
    scan: SrsV2CoeffScan,
    mode: SrsV2CoeffLayoutMode,
) -> Result<SingleVsFourByteEstimate, TransformLayoutError> {
    expect_coeff_len(coeffs_natural, SrsV2TransformKind::Tx8x8)?;
    let bytes_single_8x8 =
        estimate_coeff_layout_bytes(coeffs_natural, SrsV2TransformKind::Tx8x8, scan, mode)?;
    let bytes_four_4x4 =
        estimate_coeff_layout_bytes(coeffs_natural, SrsV2TransformKind::Tx4x4, scan, mode)?;
    let prefers = if bytes_four_4x4 < bytes_single_8x8 {
        SrsV2TransformGrouping::Four4x4
    } else {
        SrsV2TransformGrouping::Single8x8
    };
    Ok(SingleVsFourByteEstimate {
        bytes_single_8x8,
        bytes_four_4x4,
        prefers,
    })
}

fn estimate_rev34_mb_grouping_breakdown(
    qfreq: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<GroupingWireCostBreakdown, TransformLayoutError> {
    let body = encode_coeff_compact_rev32_intra_block(qfreq, kind, scan)?;
    let legacy =
        estimate_coeff_layout_bytes(qfreq.as_slice(), kind, scan, SrsV2CoeffLayoutMode::Legacy)?;
    Ok(GroupingWireCostBreakdown {
        grouping_tag_bytes: 1,
        scan_tag_bytes: 1,
        pred_byte_bytes: 1,
        body_length_field_bytes: 2,
        compact_coeff_body_bytes: body.len(),
        legacy_residual_entropy_estimate_bytes: legacy,
        per_subblock_overhead_bytes: 0,
    })
}

/// Rev34-style rate estimate for **Single8×8** (`grouping`/`scan`/`pred`/length + compact body + legacy proxy).
pub fn estimate_single8x8_cost(
    qfreq: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
    scan: SrsV2CoeffScan,
) -> Result<GroupingWireCostBreakdown, TransformLayoutError> {
    estimate_rev34_mb_grouping_breakdown(qfreq, SrsV2TransformKind::Tx8x8, scan)
}

/// Same as [`estimate_single8x8_cost`] for **Four4×4**, including [`FOUR4X4_PER_SUBBLOCK_SIDE_BYTES`] × 4 overhead.
pub fn estimate_four4x4_cost(
    qfreq: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
    scan: SrsV2CoeffScan,
) -> Result<GroupingWireCostBreakdown, TransformLayoutError> {
    let mut b = estimate_rev34_mb_grouping_breakdown(qfreq, SrsV2TransformKind::Tx4x4, scan)?;
    b.per_subblock_overhead_bytes = FOUR4X4_PER_SUBBLOCK_SIDE_BYTES.saturating_mul(4);
    Ok(b)
}

/// Compare rev34 intra MB **rate proxies** for dual-quant candidates (distinct quantized spectra + scans).
pub fn compare_single8x8_vs_four4x4(
    q_tx8: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
    scan8: SrsV2CoeffScan,
    q_tx4: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
    scan4: SrsV2CoeffScan,
) -> Result<SingleVsFourRev34Compare, TransformLayoutError> {
    let breakdown_tx8 = estimate_single8x8_cost(q_tx8, scan8)?;
    let breakdown_tx4 = estimate_four4x4_cost(q_tx4, scan4)?;
    let rate_tx8 = breakdown_tx8.total_rdo_rate_bytes();
    let rate_tx4 = breakdown_tx4.total_rdo_rate_bytes();
    let prefers = if rate_tx4 < rate_tx8 {
        SrsV2TransformGrouping::Four4x4
    } else {
        SrsV2TransformGrouping::Single8x8
    };
    Ok(SingleVsFourRev34Compare {
        breakdown_tx8,
        breakdown_tx4,
        rate_tx8,
        rate_tx4,
        prefers,
    })
}

/// Single-line summary for logs / benches.
pub fn transform_grouping_summary(stats: &TransformGroupingStats) -> String {
    format!(
        "single8x8={} four4x4={} zero={} dc_only={} sparse={} dense={} legacy_B={} grouped_B={} savings_B={} savings_pct={:.2}",
        stats.single_8x8_blocks,
        stats.four_4x4_blocks,
        stats.zero_blocks,
        stats.dc_only_blocks,
        stats.sparse_blocks,
        stats.dense_blocks,
        stats.estimated_legacy_bytes,
        stats.estimated_grouped_bytes,
        stats.estimated_savings_bytes,
        stats.estimated_savings_percent
    )
}

/// Map natural-order coefficients to scan order (`out[s] = coeffs[perm[s]]`).
pub fn reorder_coefficients_for_scan(
    coeffs: &[i16],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<[i16; COEFFICIENTS_PER_LAYOUT_BLOCK], TransformLayoutError> {
    expect_coeff_len(coeffs, kind)?;
    let perm = scan_permutation(coeffs, kind, scan)?;
    let mut out = [0_i16; COEFFICIENTS_PER_LAYOUT_BLOCK];
    for s in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
        out[s] = coeffs[perm[s]];
    }
    Ok(out)
}

/// Inverse of [`reorder_coefficients_for_scan`].
pub fn restore_coefficients_from_scan(
    scanned: &[i16],
    coeffs_same_natural: &[i16],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<[i16; COEFFICIENTS_PER_LAYOUT_BLOCK], TransformLayoutError> {
    expect_coeff_len(scanned, kind)?;
    expect_coeff_len(coeffs_same_natural, kind)?;
    let perm = scan_permutation(coeffs_same_natural, kind, scan)?;
    let mut natural = [0_i16; COEFFICIENTS_PER_LAYOUT_BLOCK];
    for s in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
        natural[perm[s]] = scanned[s];
    }
    Ok(natural)
}

fn legacy_byte_estimate(coeffs: &[i16]) -> usize {
    // Nominal syntax/framing overhead so legacy compares fairly to compact’s fixed header (telemetry only).
    const LEGACY_NOTIONAL_CONTAINER_BYTES: usize = 10;
    let mut bits = 0usize;
    for &c in coeffs {
        if c == 0 {
            bits += 2;
        } else {
            let a = c.unsigned_abs() as u32;
            let mag = 32 - a.leading_zeros();
            bits += 4 + mag as usize;
        }
    }
    bits.div_ceil(8) + LEGACY_NOTIONAL_CONTAINER_BYTES
}

fn compact_v1_byte_estimate_natural(coeffs_natural: &[i16]) -> usize {
    let nz = coeffs_natural.iter().filter(|&&c| c != 0).count();
    let header = COMPACT_MAGIC.len() + 1 + 1 + 1 + 1 + 2;
    let bitmap = 8usize;
    header + bitmap + nz.saturating_mul(2)
}

/// Heuristic byte estimates for sizing / telemetry (not entropy-accurate).
pub fn estimate_coeff_layout_bytes(
    coeffs_natural: &[i16],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
    mode: SrsV2CoeffLayoutMode,
) -> Result<usize, TransformLayoutError> {
    let scanned = reorder_coefficients_for_scan(coeffs_natural, kind, scan)?;
    Ok(match mode {
        SrsV2CoeffLayoutMode::Legacy => legacy_byte_estimate(&scanned),
        SrsV2CoeffLayoutMode::CompactV1 => compact_v1_byte_estimate_natural(coeffs_natural),
    })
}

fn kind_tag(k: SrsV2TransformKind) -> u8 {
    match k {
        SrsV2TransformKind::Tx4x4 => 0,
        SrsV2TransformKind::Tx8x8 => 1,
    }
}

fn kind_from_tag(b: u8) -> Result<SrsV2TransformKind, TransformLayoutError> {
    match b {
        0 => Ok(SrsV2TransformKind::Tx4x4),
        1 => Ok(SrsV2TransformKind::Tx8x8),
        _ => Err(TransformLayoutError::InvalidDiscriminant(
            "transform_kind",
            b,
        )),
    }
}

fn scan_tag(s: SrsV2CoeffScan) -> u8 {
    match s {
        SrsV2CoeffScan::ZigZag => 0,
        SrsV2CoeffScan::GroupedLowFirst => 1,
        SrsV2CoeffScan::RunOptimized => 2,
    }
}

fn scan_from_tag(b: u8) -> Result<SrsV2CoeffScan, TransformLayoutError> {
    match b {
        0 => Ok(SrsV2CoeffScan::ZigZag),
        1 => Ok(SrsV2CoeffScan::GroupedLowFirst),
        2 => Ok(SrsV2CoeffScan::RunOptimized),
        _ => Err(TransformLayoutError::InvalidDiscriminant("coeff_scan", b)),
    }
}

/// Experimental compact blob: `S2TL` + version + tags + u16 `64` + 8-byte **natural-index**
/// nonzero bitmap + `i16` values in ascending natural index (`scan`/`kind` stored for telemetry).
pub fn encode_coeff_layout_compact_v1(
    coeffs_natural: &[i16],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<Vec<u8>, TransformLayoutError> {
    expect_coeff_len(coeffs_natural, kind)?;
    let mut bitmap: u64 = 0;
    for (i, &coeff) in coeffs_natural
        .iter()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if coeff != 0 {
            bitmap |= 1u64 << i;
        }
    }
    let pop = bitmap.count_ones() as usize;
    let mut out = Vec::with_capacity(10 + 8 + pop * 2);
    out.extend_from_slice(COMPACT_MAGIC);
    out.push(COMPACT_VERSION);
    out.push(kind_tag(kind));
    out.push(scan_tag(scan));
    out.push(1u8); // layout_mode tag = CompactV1
    out.extend_from_slice(&(COEFFICIENTS_PER_LAYOUT_BLOCK as u16).to_le_bytes());
    out.extend_from_slice(&bitmap.to_le_bytes());
    for (i, &coeff) in coeffs_natural
        .iter()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if (bitmap >> i) & 1 != 0 {
            out.extend_from_slice(&coeff.to_le_bytes());
        }
    }
    Ok(out)
}

/// Decode [`encode_coeff_layout_compact_v1`]; returns `(kind, scan, natural_coeffs)`.
pub fn decode_coeff_layout_compact_v1(
    buf: &[u8],
) -> Result<(SrsV2TransformKind, SrsV2CoeffScan, [i16; 64]), TransformLayoutError> {
    const HDR: usize = 10;
    if buf.len() < HDR + 8 {
        return Err(TransformLayoutError::Truncated);
    }
    if &buf[0..4] != COMPACT_MAGIC.as_slice() {
        return Err(TransformLayoutError::BadMagic);
    }
    let ver = buf[4];
    if ver != COMPACT_VERSION {
        return Err(TransformLayoutError::UnsupportedVersion(ver));
    }
    let kind = kind_from_tag(buf[5])?;
    let scan = scan_from_tag(buf[6])?;
    let layout_tag = buf[7];
    if layout_tag != 1 {
        return Err(TransformLayoutError::InvalidDiscriminant(
            "coeff_layout_mode",
            layout_tag,
        ));
    }
    let n = u16::from_le_bytes([buf[8], buf[9]]) as usize;
    if n != COEFFICIENTS_PER_LAYOUT_BLOCK {
        return Err(TransformLayoutError::InvalidCoefficientCount {
            expected: COEFFICIENTS_PER_LAYOUT_BLOCK,
            got: n,
        });
    }
    let bitmap = u64::from_le_bytes(buf[HDR..HDR + 8].try_into().unwrap());
    let pop = bitmap.count_ones() as usize;
    let need = HDR + 8 + pop * 2;
    if buf.len() < need {
        return Err(TransformLayoutError::Truncated);
    }
    let mut natural = [0_i16; COEFFICIENTS_PER_LAYOUT_BLOCK];
    let mut cur = HDR + 8;
    for (i, slot) in natural
        .iter_mut()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if (bitmap >> i) & 1 != 0 {
            *slot = i16::from_le_bytes([buf[cur], buf[cur + 1]]);
            cur += 2;
        }
    }
    if cur != need {
        return Err(TransformLayoutError::InconsistentNonzeroCount {
            bitmap_popcount: bitmap.count_ones(),
            payload_pairs: (cur - (HDR + 8)) / 2,
        });
    }
    Ok((kind, scan, natural))
}

/// Compact coefficient body only (**no** `S2TL` header): `u64` LE natural-index bitmap + nonzero `i16`
/// values in ascending natural index order. Callers may embed this blob in their own framing.
pub fn encode_coeff_compact_v1_natural_body(
    coeffs: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
) -> Vec<u8> {
    let mut bitmap: u64 = 0;
    for (i, &coeff) in coeffs
        .iter()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if coeff != 0 {
            bitmap |= 1u64 << i;
        }
    }
    let pop = bitmap.count_ones() as usize;
    let mut out = Vec::with_capacity(8 + pop * 2);
    out.extend_from_slice(&bitmap.to_le_bytes());
    for (i, &coeff) in coeffs
        .iter()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if (bitmap >> i) & 1 != 0 {
            out.extend_from_slice(&coeff.to_le_bytes());
        }
    }
    out
}

/// Decode [`encode_coeff_compact_v1_natural_body`].
pub fn decode_coeff_compact_v1_natural_body(
    buf: &[u8],
) -> Result<[i16; COEFFICIENTS_PER_LAYOUT_BLOCK], TransformLayoutError> {
    if buf.len() < 8 {
        return Err(TransformLayoutError::Truncated);
    }
    let bitmap = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let pop = bitmap.count_ones() as usize;
    let need = 8usize.saturating_add(pop.saturating_mul(2));
    if buf.len() != need {
        return Err(TransformLayoutError::InconsistentNonzeroCount {
            bitmap_popcount: bitmap.count_ones(),
            payload_pairs: buf.len().saturating_sub(8) / 2,
        });
    }
    let mut natural = [0_i16; COEFFICIENTS_PER_LAYOUT_BLOCK];
    let mut cur = 8usize;
    for (i, slot) in natural
        .iter_mut()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if (bitmap >> i) & 1 != 0 {
            *slot = i16::from_le_bytes([buf[cur], buf[cur + 1]]);
            cur += 2;
        }
    }
    debug_assert_eq!(cur, buf.len());
    Ok(natural)
}

/// Natural-index permutation for one block: `perm[s]` is the raster index visited at scan position `s`.
///
/// Used by **`FR2` rev32** intra to serialize nonzero coefficient **`i16`** values in scan traversal order
/// (after the leading natural `u64` bitmap).
pub fn block_natural_index_permutation_for_scan(
    coeffs_natural: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<[usize; COEFFICIENTS_PER_LAYOUT_BLOCK], TransformLayoutError> {
    scan_permutation(coeffs_natural.as_slice(), kind, scan)
}

/// **`FR2` rev32** intra and **rev33** fixed-grid **P** 8×8 residual payload: `u64` LE bitmap (natural nonzero
/// positions) then nonzero coefficients in **scan order** (iterate `s = 0..64`, index `perm[s]`, emit when the bitmap bit is set).
///
/// [`decode_coeff_compact_rev32_intra_block`] recovers natural coefficients before inverse transform.
pub fn encode_coeff_compact_rev32_intra_block(
    coeffs_natural: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<Vec<u8>, TransformLayoutError> {
    let perm = block_natural_index_permutation_for_scan(coeffs_natural, kind, scan)?;
    let mut bitmap: u64 = 0;
    for (i, &coeff) in coeffs_natural
        .iter()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if coeff != 0 {
            bitmap |= 1u64 << i;
        }
    }
    let pop = bitmap.count_ones() as usize;
    let mut out = Vec::with_capacity(8 + pop * 2);
    out.extend_from_slice(&bitmap.to_le_bytes());
    for &i in perm.iter().take(COEFFICIENTS_PER_LAYOUT_BLOCK) {
        if (bitmap >> i) & 1 != 0 {
            out.extend_from_slice(&coeffs_natural[i].to_le_bytes());
        }
    }
    Ok(out)
}

/// Decode [`encode_coeff_compact_rev32_intra_block`].
pub fn decode_coeff_compact_rev32_intra_block(
    buf: &[u8],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<[i16; COEFFICIENTS_PER_LAYOUT_BLOCK], TransformLayoutError> {
    if buf.len() < 8 {
        return Err(TransformLayoutError::Truncated);
    }
    let bitmap = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let pop = bitmap.count_ones() as usize;
    let need = 8usize.saturating_add(pop.saturating_mul(2));
    if buf.len() != need {
        return Err(TransformLayoutError::InconsistentNonzeroCount {
            bitmap_popcount: bitmap.count_ones(),
            payload_pairs: buf.len().saturating_sub(8) / 2,
        });
    }
    let mut pattern = [0_i16; COEFFICIENTS_PER_LAYOUT_BLOCK];
    for (i, slot) in pattern
        .iter_mut()
        .enumerate()
        .take(COEFFICIENTS_PER_LAYOUT_BLOCK)
    {
        if (bitmap >> i) & 1 != 0 {
            *slot = 1;
        }
    }
    let perm = scan_permutation(&pattern, kind, scan)?;
    let mut natural = [0_i16; COEFFICIENTS_PER_LAYOUT_BLOCK];
    let mut cur = 8usize;
    for &i in perm.iter().take(COEFFICIENTS_PER_LAYOUT_BLOCK) {
        if (bitmap >> i) & 1 != 0 {
            natural[i] = i16::from_le_bytes([buf[cur], buf[cur + 1]]);
            cur += 2;
        }
    }
    debug_assert_eq!(cur, buf.len());
    Ok(natural)
}

/// Short human-readable summary for logs / benches.
pub fn transform_layout_summary(
    coeffs_natural: &[i16],
    kind: SrsV2TransformKind,
    scan: SrsV2CoeffScan,
) -> Result<String, TransformLayoutError> {
    expect_coeff_len(coeffs_natural, kind)?;
    let nz = coeffs_natural.iter().filter(|&&c| c != 0).count();
    let leg =
        estimate_coeff_layout_bytes(coeffs_natural, kind, scan, SrsV2CoeffLayoutMode::Legacy)?;
    let cmp =
        estimate_coeff_layout_bytes(coeffs_natural, kind, scan, SrsV2CoeffLayoutMode::CompactV1)?;
    Ok(format!(
        "kind={kind:?} scan={scan:?} nonzero={nz}/{} legacy_est_B={leg} compact_v1_est_B={cmp}",
        COEFFICIENTS_PER_LAYOUT_BLOCK
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::dct::ZIGZAG;

    fn assert_roundtrip(coeffs: &[i16], kind: SrsV2TransformKind, scan: SrsV2CoeffScan) {
        let shuffled = reorder_coefficients_for_scan(coeffs, kind, scan).unwrap();
        let back = restore_coefficients_from_scan(&shuffled, coeffs, kind, scan).unwrap();
        assert_eq!(back.as_slice(), coeffs);
    }

    #[test]
    fn zigzag_roundtrip_tx8x8() {
        let mut c = [0_i16; 64];
        for (i, slot) in c.iter_mut().enumerate() {
            *slot = (i as i16).wrapping_mul(3).wrapping_sub(17);
        }
        assert_roundtrip(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag);
    }

    #[test]
    fn zigzag_roundtrip_tx4x4() {
        let mut c = [0_i16; 64];
        for (i, slot) in c.iter_mut().enumerate() {
            *slot = (i as i16).wrapping_rem(31).wrapping_sub(11);
        }
        assert_roundtrip(&c, SrsV2TransformKind::Tx4x4, SrsV2CoeffScan::ZigZag);
    }

    #[test]
    fn grouped_low_first_roundtrip_tx8x8_and_tx4x4() {
        let mut c = [0_i16; 64];
        for (i, slot) in c.iter_mut().enumerate() {
            *slot = ((i * 13 + 7) % 101) as i16 - 50;
        }
        assert_roundtrip(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::GroupedLowFirst,
        );
        assert_roundtrip(
            &c,
            SrsV2TransformKind::Tx4x4,
            SrsV2CoeffScan::GroupedLowFirst,
        );
    }

    #[test]
    fn run_optimized_roundtrip() {
        let mut c = [0_i16; 64];
        c[0] = 100;
        c[ZIGZAG[10]] = -7;
        c[45] = 3;
        assert_roundtrip(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::RunOptimized);
        let mut c4 = [0_i16; 64];
        for q in 0..4 {
            c4[q * 16] = (q as i16 + 1) * 10;
            c4[q * 16 + 5] = -3;
        }
        assert_roundtrip(&c4, SrsV2TransformKind::Tx4x4, SrsV2CoeffScan::RunOptimized);
    }

    #[test]
    fn all_zero_estimates_tiny() {
        let z = [0_i16; 64];
        let e = estimate_coeff_layout_bytes(
            &z,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::Legacy,
        )
        .unwrap();
        let c = estimate_coeff_layout_bytes(
            &z,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::CompactV1,
        )
        .unwrap();
        assert!(e <= 32, "legacy est {e}");
        assert!(c <= 32, "compact est {c}");
    }

    #[test]
    fn dc_only_estimates_tiny() {
        let mut c = [0_i16; 64];
        c[0] = -42;
        let leg = estimate_coeff_layout_bytes(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::Legacy,
        )
        .unwrap();
        let cmp = estimate_coeff_layout_bytes(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::CompactV1,
        )
        .unwrap();
        assert!(leg < 200);
        assert!(cmp < 40);
    }

    #[test]
    fn sparse_ac_compact_smaller_or_equal_legacy() {
        let mut c = [0_i16; 64];
        c[0] = 10;
        for &zi in ZIGZAG.iter().skip(3).take(5) {
            c[zi] = ((zi as i16) % 9) - 4;
        }
        let leg = estimate_coeff_layout_bytes(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::Legacy,
        )
        .unwrap();
        let cmp = estimate_coeff_layout_bytes(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::CompactV1,
        )
        .unwrap();
        assert!(cmp <= leg, "compact {cmp} legacy {leg}");
    }

    #[test]
    fn dense_noisy_no_panic() {
        let mut c = [0_i16; 64];
        for (i, slot) in c.iter_mut().enumerate() {
            let x = (i as i64).wrapping_mul(1103515245).wrapping_add(12345);
            *slot = (x.rem_euclid(2001) - 1000) as i16;
        }
        let _ = reorder_coefficients_for_scan(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::RunOptimized,
        )
        .unwrap();
        let _ = estimate_coeff_layout_bytes(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::Legacy,
        )
        .unwrap();
    }

    #[test]
    fn tx4x4_for_high_detail_when_configured() {
        let cfg = TransformDecisionConfig {
            variance_select_tx4x4: 100.0,
            hf_energy_select_tx4x4: 10_000.0,
        };
        let hi = TransformDecisionInput {
            spatial_variance: 500.0,
            hf_energy: 0.0,
            max_abs_coeff: 100,
            nonzero_count: 40,
        };
        assert_eq!(choose_transform_kind(&hi, &cfg), SrsV2TransformKind::Tx4x4);
    }

    #[test]
    fn tx8x8_for_smooth_block() {
        let cfg = TransformDecisionConfig::default();
        let lo = TransformDecisionInput {
            spatial_variance: 10.0,
            hf_energy: 100.0,
            max_abs_coeff: 5,
            nonzero_count: 4,
        };
        assert_eq!(choose_transform_kind(&lo, &cfg), SrsV2TransformKind::Tx8x8);
    }

    #[test]
    fn malformed_compact_rejected() {
        assert!(matches!(
            decode_coeff_layout_compact_v1(b"XXXX"),
            Err(TransformLayoutError::Truncated | TransformLayoutError::BadMagic)
        ));
        let mut buf = encode_coeff_layout_compact_v1(
            &[7_i16; 64],
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        buf.truncate(buf.len().saturating_sub(3));
        assert!(matches!(
            decode_coeff_layout_compact_v1(&buf),
            Err(TransformLayoutError::Truncated)
        ));
        let mut badver = encode_coeff_layout_compact_v1(
            &[7_i16; 64],
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        badver[4] = 99;
        assert!(matches!(
            decode_coeff_layout_compact_v1(&badver),
            Err(TransformLayoutError::UnsupportedVersion(99))
        ));
        let mut bad_layout = encode_coeff_layout_compact_v1(
            &[7_i16; 64],
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        bad_layout[7] = 0;
        assert!(matches!(
            decode_coeff_layout_compact_v1(&bad_layout),
            Err(TransformLayoutError::InvalidDiscriminant(
                "coeff_layout_mode",
                0
            ))
        ));
    }

    #[test]
    fn deterministic_compact_encode() {
        let c = [13_i16; 64];
        let a =
            encode_coeff_layout_compact_v1(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag)
                .unwrap();
        let b =
            encode_coeff_layout_compact_v1(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag)
                .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn transform_layout_summary_deterministic_for_same_input() {
        let mut c = [0_i16; 64];
        c[0] = 5;
        c[3] = -2;
        c[ZIGZAG[15]] = 17;
        let s1 = transform_layout_summary(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag)
            .unwrap();
        let s2 = transform_layout_summary(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag)
            .unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn compact_roundtrip_natural() {
        let mut c = [0_i16; 64];
        c[0] = -900;
        c[11] = 3;
        c[63] = 1;
        let blob = encode_coeff_layout_compact_v1(
            &c,
            SrsV2TransformKind::Tx4x4,
            SrsV2CoeffScan::GroupedLowFirst,
        )
        .unwrap();
        let (k, s, out) = decode_coeff_layout_compact_v1(&blob).unwrap();
        assert_eq!(k, SrsV2TransformKind::Tx4x4);
        assert_eq!(s, SrsV2CoeffScan::GroupedLowFirst);
        assert_eq!(out, c);
    }

    #[test]
    fn compact_v1_natural_body_roundtrip() {
        let mut c = [0_i16; 64];
        for (i, slot) in c.iter_mut().enumerate() {
            *slot = ((i as i16 * 17).wrapping_rem(91)).wrapping_sub(40);
        }
        let b = encode_coeff_compact_v1_natural_body(&c);
        assert_eq!(decode_coeff_compact_v1_natural_body(&b).unwrap(), c);
    }

    #[test]
    fn rev32_intra_compact_scan_body_roundtrip_zigzag() {
        let mut c = [0_i16; 64];
        c[0] = 11;
        c[ZIGZAG[5]] = -3;
        c[40] = 7;
        let blob = encode_coeff_compact_rev32_intra_block(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        let out = decode_coeff_compact_rev32_intra_block(
            &blob,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        assert_eq!(out, c);
    }

    #[test]
    fn rev32_intra_compact_scan_body_roundtrip_run_optimized() {
        let mut c = [0_i16; 64];
        c[0] = 100;
        c[ZIGZAG[10]] = -7;
        c[45] = 3;
        let blob = encode_coeff_compact_rev32_intra_block(
            &c,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::RunOptimized,
        )
        .unwrap();
        let out = decode_coeff_compact_rev32_intra_block(
            &blob,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::RunOptimized,
        )
        .unwrap();
        assert_eq!(out, c);
    }

    fn spatial_flat_from_raster(raster: &[i16; 64]) -> [[i16; 8]; 8] {
        let mut b = [[0_i16; 8]; 8];
        for i in 0..64 {
            b[i / 8][i % 8] = raster[i];
        }
        b
    }

    fn spatial_smooth_uniform() -> [[i16; 8]; 8] {
        [[3_i16; 8]; 8]
    }

    /// Localized checkerboard in quadrant 0 only → high quadrant variance × scale drives Tx4×4.
    fn spatial_localized_high_detail() -> [[i16; 8]; 8] {
        let mut b = [[0_i16; 8]; 8];
        for (r, row) in b.iter_mut().enumerate().take(4) {
            for (c, cell) in row.iter_mut().enumerate().take(4) {
                *cell = if (r + c) % 2 == 0 { 40 } else { -40 };
            }
        }
        b
    }

    #[test]
    fn transform_grouping_auto_smooth_chooses_single8x8() {
        let cfg = TransformDecisionConfig::default();
        let spatial = spatial_smooth_uniform();
        assert_eq!(
            choose_transform_grouping(SrsV2TransformGrouping::AutoByResidual, &spatial, &cfg),
            SrsV2TransformKind::Tx8x8
        );
    }

    #[test]
    fn transform_grouping_auto_localized_detail_chooses_four4x4() {
        let cfg = TransformDecisionConfig::default();
        let spatial = spatial_localized_high_detail();
        assert_eq!(
            choose_transform_grouping(SrsV2TransformGrouping::AutoByResidual, &spatial, &cfg),
            SrsV2TransformKind::Tx4x4
        );
    }

    #[test]
    fn transform_grouping_decision_deterministic() {
        let cfg = TransformDecisionConfig::default();
        let spatial = spatial_localized_high_detail();
        let a = choose_transform_grouping(SrsV2TransformGrouping::AutoByResidual, &spatial, &cfg);
        let b = choose_transform_grouping(SrsV2TransformGrouping::AutoByResidual, &spatial, &cfg);
        assert_eq!(a, b);
    }

    #[test]
    fn estimate_grouping_all_zero_tiny() {
        let spatial = [[0_i16; 8]; 8];
        let coeffs = [0_i16; 64];
        let st = estimate_transform_grouping_bytes(
            &spatial,
            &coeffs,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        assert_eq!(st.zero_blocks, 1);
        assert!(st.estimated_legacy_bytes <= 32);
        assert!(st.estimated_grouped_bytes <= 32);
    }

    #[test]
    fn estimate_grouping_dc_only_tiny() {
        let spatial = [[11_i16; 8]; 8];
        assert_eq!(
            classify_residual_block(&spatial),
            ResidualBlockClass::DcOnly
        );
        let mut coeffs = [0_i16; 64];
        coeffs[0] = -77;
        let st = estimate_transform_grouping_bytes(
            &spatial,
            &coeffs,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        assert_eq!(st.dc_only_blocks, 1);
        assert!(st.estimated_legacy_bytes < 200);
        assert!(st.estimated_grouped_bytes < 40);
    }

    #[test]
    fn estimate_grouping_sparse_improves_or_ties_legacy() {
        let mut raster = [0_i16; 64];
        raster[0] = 1;
        raster[10] = 2;
        raster[20] = -1;
        let spatial = spatial_flat_from_raster(&raster);
        assert_eq!(
            classify_residual_block(&spatial),
            ResidualBlockClass::Sparse
        );
        let mut coeffs = [0_i16; 64];
        coeffs[0] = 10;
        for &zi in ZIGZAG.iter().skip(3).take(5) {
            coeffs[zi] = ((zi as i16) % 9) - 4;
        }
        let st = estimate_transform_grouping_bytes(
            &spatial,
            &coeffs,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        assert_eq!(st.sparse_blocks, 1);
        assert!(st.estimated_grouped_bytes <= st.estimated_legacy_bytes);
    }

    #[test]
    fn estimate_grouping_dense_noisy_safe_even_if_larger() {
        let mut raster = [0_i16; 64];
        for (i, slot) in raster.iter_mut().enumerate() {
            let x = (i as i64).wrapping_mul(1103515245).wrapping_add(12345);
            *slot = (x.rem_euclid(2001) - 1000) as i16;
        }
        let spatial = spatial_flat_from_raster(&raster);
        assert_eq!(classify_residual_block(&spatial), ResidualBlockClass::Dense);
        let coeffs = raster;
        let st = estimate_transform_grouping_bytes(
            &spatial,
            &coeffs,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        assert_eq!(st.dense_blocks, 1);
        let _ = transform_grouping_summary(&st);
        // Grouped may cost more than legacy on dense blocks; must still return finite estimates.
        assert!(st.estimated_legacy_bytes > 0);
        assert!(st.estimated_grouped_bytes > 0);
    }

    #[test]
    fn choose_transform_and_scan_wires_grouping_and_scan() {
        let cfg = TransformDecisionConfig::default();
        let spatial = spatial_smooth_uniform();
        let coeffs = [0_i16; 64];
        let d = choose_transform_and_scan(
            SrsV2TransformGrouping::AutoByResidual,
            &spatial,
            &coeffs,
            &cfg,
        )
        .unwrap();
        assert_eq!(d.kind, SrsV2TransformKind::Tx8x8);
        assert_eq!(d.scan, SrsV2CoeffScan::RunOptimized);
    }

    #[test]
    fn compare_single_vs_four_tie_prefers_single8x8() {
        let z = [0_i16; 64];
        let est = compare_coeff_layout_single_vs_four4x4(
            &z,
            SrsV2CoeffScan::ZigZag,
            SrsV2CoeffLayoutMode::Legacy,
        )
        .unwrap();
        assert_eq!(est.bytes_single_8x8, est.bytes_four_4x4);
        assert_eq!(est.prefers, SrsV2TransformGrouping::Single8x8);
    }

    #[test]
    fn rev34_wire_estimates_sum_tags_body_legacy_and_four_overhead() {
        let z = [0_i16; 64];
        let s = estimate_single8x8_cost(&z, SrsV2CoeffScan::ZigZag).unwrap();
        assert_eq!(
            s.grouping_tag_bytes + s.scan_tag_bytes + s.pred_byte_bytes + s.body_length_field_bytes,
            REV34_INTRA_MB_PREFIX_BYTES
        );
        let f = estimate_four4x4_cost(&z, SrsV2CoeffScan::ZigZag).unwrap();
        assert_eq!(
            f.per_subblock_overhead_bytes,
            FOUR4X4_PER_SUBBLOCK_SIDE_BYTES * 4
        );
        let q8 = z;
        let q4 = z;
        let cmp =
            compare_single8x8_vs_four4x4(&q8, SrsV2CoeffScan::ZigZag, &q4, SrsV2CoeffScan::ZigZag)
                .unwrap();
        assert_eq!(cmp.prefers, SrsV2TransformGrouping::Single8x8);
        assert!(
            cmp.rate_tx4 > cmp.rate_tx8,
            "Four4×4 includes per-subblock overhead even when coefficients match"
        );
    }

    #[test]
    fn transform_grouping_summary_non_empty() {
        let spatial = [[0_i16; 8]; 8];
        let coeffs = [0_i16; 64];
        let st = estimate_transform_grouping_bytes(
            &spatial,
            &coeffs,
            SrsV2TransformKind::Tx8x8,
            SrsV2CoeffScan::ZigZag,
        )
        .unwrap();
        let s = transform_grouping_summary(&st);
        assert!(s.contains("single8x8="));
        assert!(s.contains("savings_pct="));
    }

    #[test]
    fn fr2_rev34_grouping_wire_roundtrips_transform_kind() {
        assert_eq!(
            fr2_rev34_grouping_wire_from_transform_kind(SrsV2TransformKind::Tx4x4),
            FR2_REV34_INTRA_GROUPING_FOUR4X4
        );
        assert_eq!(
            fr2_rev34_grouping_wire_from_transform_kind(SrsV2TransformKind::Tx8x8),
            FR2_REV34_INTRA_GROUPING_SINGLE8X8
        );
        assert_eq!(
            fr2_rev34_transform_kind_from_grouping_wire(FR2_REV34_INTRA_GROUPING_FOUR4X4).unwrap(),
            SrsV2TransformKind::Tx4x4
        );
        assert_eq!(
            fr2_rev34_transform_kind_from_grouping_wire(FR2_REV34_INTRA_GROUPING_SINGLE8X8)
                .unwrap(),
            SrsV2TransformKind::Tx8x8
        );
        assert!(fr2_rev34_transform_kind_from_grouping_wire(2).is_err());
    }

    #[test]
    fn fr2_rev35_p_grouping_wire_matches_rev34_numeric_mapping() {
        assert_eq!(
            fr2_rev35_p_grouping_wire_from_transform_kind(SrsV2TransformKind::Tx4x4),
            FR2_REV34_INTRA_GROUPING_FOUR4X4
        );
        assert_eq!(
            fr2_rev35_p_transform_kind_from_grouping_wire(FR2_REV34_INTRA_GROUPING_SINGLE8X8)
                .unwrap(),
            SrsV2TransformKind::Tx8x8
        );
        assert!(fr2_rev35_p_transform_kind_from_grouping_wire(7).is_err());
    }
}
