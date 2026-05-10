//! Transform size and coefficient **layout** decisions for SRSV2 (experimental).
//!
//! This module sits **above** [`crate::srsv2::dct`] (pure DCT math) and **before**
//! [`crate::srsv2::residual_entropy`] (entropy coding). It does **not** define `FR2`
//! payload bytes or alter encoder behavior until explicitly wired.
//!
//! ## Layout vs entropy
//! - **Natural order** depends on [`SrsV2TransformKind`] (8×8 raster vs four 4×4 quadrants).
//! - **Scan order** permutes coefficients for analysis / future entropy-friendly serialization.
//! - [`SrsV2CoeffLayoutMode::CompactV1`] is an experimental container format (`S2TL` magic),
//!   not an `FR2` revision (but fixed-grid **P** frames may embed natural-order compact bodies in
//!   **`FR2` rev33** chunks via [`crate::srsv2::residual_entropy::encode_p_residual_chunk_compact_v33_wire`]).

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
    InconsistentNonzeroCount { bitmap_popcount: u32, payload_pairs: usize },
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
                for k in 0..16 {
                    p[s] = base + ZIGZAG_4X4[k];
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
                let da = if a == 0 { (0, 0, 0) } else { (1, manhattan_ac(a), a) };
                let db = if b == 0 { (0, 0, 0) } else { (1, manhattan_ac(b), b) };
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
                    let da = if la == 0 {
                        (0, 0, 0)
                    } else {
                        (1, ra + ca, la)
                    };
                    let db = if lb == 0 {
                        (0, 0, 0)
                    } else {
                        (1, rb + cb, lb)
                    };
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
    let mut s = 0usize;
    for nat in nonzero.into_iter().chain(zeros.into_iter()) {
        p[s] = nat;
        s += 1;
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
    (bits + 7) / 8 + LEGACY_NOTIONAL_CONTAINER_BYTES
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
        _ => Err(TransformLayoutError::InvalidDiscriminant("transform_kind", b)),
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
    for i in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
        if coeffs_natural[i] != 0 {
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
    for i in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
        if (bitmap >> i) & 1 != 0 {
            out.extend_from_slice(&coeffs_natural[i].to_le_bytes());
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
    for i in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
        if (bitmap >> i) & 1 != 0 {
            natural[i] = i16::from_le_bytes([buf[cur], buf[cur + 1]]);
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
/// values in ascending index order. Used by **`FR2` rev32** intra plane chunks.
pub fn encode_coeff_compact_v1_natural_body(
    coeffs: &[i16; COEFFICIENTS_PER_LAYOUT_BLOCK],
) -> Vec<u8> {
    let mut bitmap: u64 = 0;
    for i in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
        if coeffs[i] != 0 {
            bitmap |= 1u64 << i;
        }
    }
    let pop = bitmap.count_ones() as usize;
    let mut out = Vec::with_capacity(8 + pop * 2);
    out.extend_from_slice(&bitmap.to_le_bytes());
    for i in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
        if (bitmap >> i) & 1 != 0 {
            out.extend_from_slice(&coeffs[i].to_le_bytes());
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
    for i in 0..COEFFICIENTS_PER_LAYOUT_BLOCK {
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
    let leg = estimate_coeff_layout_bytes(coeffs_natural, kind, scan, SrsV2CoeffLayoutMode::Legacy)?;
    let cmp = estimate_coeff_layout_bytes(
        coeffs_natural,
        kind,
        scan,
        SrsV2CoeffLayoutMode::CompactV1,
    )?;
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
        for i in 0..64 {
            c[i] = (i as i16).wrapping_mul(3).wrapping_sub(17);
        }
        assert_roundtrip(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag);
    }

    #[test]
    fn grouped_low_first_roundtrip_tx8x8_and_tx4x4() {
        let mut c = [0_i16; 64];
        for i in 0..64 {
            c[i] = ((i * 13 + 7) % 101) as i16 - 50;
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
        for i in 0..64 {
            let x = (i as i64).wrapping_mul(1103515245).wrapping_add(12345);
            c[i] = (x.rem_euclid(2001) - 1000) as i16;
        }
        let _ = reorder_coefficients_for_scan(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::RunOptimized).unwrap();
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
        assert_eq!(
            choose_transform_kind(&hi, &cfg),
            SrsV2TransformKind::Tx4x4
        );
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
        assert_eq!(
            choose_transform_kind(&lo, &cfg),
            SrsV2TransformKind::Tx8x8
        );
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
    }

    #[test]
    fn deterministic_compact_encode() {
        let c = [13_i16; 64];
        let a = encode_coeff_layout_compact_v1(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag).unwrap();
        let b = encode_coeff_layout_compact_v1(&c, SrsV2TransformKind::Tx8x8, SrsV2CoeffScan::ZigZag).unwrap();
        assert_eq!(a, b);
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
        for i in 0..64 {
            c[i] = ((i as i16 * 17).wrapping_rem(91)).wrapping_sub(40);
        }
        let b = encode_coeff_compact_v1_natural_body(&c);
        assert_eq!(decode_coeff_compact_v1_natural_body(&b).unwrap(), c);
    }
}