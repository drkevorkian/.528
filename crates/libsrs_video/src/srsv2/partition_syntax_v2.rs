//! Standalone **partition map v2** and **MV share group** serializers (experimental).
//!
//! This module **does not** perform motion estimation, residual transforms, or RDO. It owns only:
//! - compact **macroblock partition mode** maps ([`PartitionMapV2`]),
//! - optional **PU index sharing** descriptors ([`MvShareGroupV2`]).
//!
//! Wire blobs are embedded in **FR2** rev **27**/**28** variable-**P** payloads when
//! [`crate::srsv2::rate_control::SrsV2EncodeSettings::partition_syntax_mode`] is [`crate::srsv2::rate_control::SrsV2PartitionSyntaxMode::V2RleMvShare`].
//! Legacy **v1** map layouts ([`crate::srsv2::p_var_partition`], [`crate::srsv2::rate_control::SrsV2PartitionMapEncoding`]) remain the default.
//!
//! Full byte-level specification: `docs/partition_syntax_v2.md` (repository root).

use std::collections::HashSet;

use thiserror::Error;

use super::inter_mv::{pu_count_partition_wire, validate_partition_reserved_bits};

/// Magic + format version for partition maps (`S2P` + `0x01`).
pub const PARTITION_MAP_V2_MAGIC: [u8; 4] = [0x53, 0x32, 0x50, 0x01];

/// Magic + format version for MV-share groups (`S2G` + `0x01`).
pub const MV_SHARE_GROUPS_V2_MAGIC: [u8; 4] = [0x53, 0x32, 0x47, 0x01];

const MAP_KIND_UNIFORM: u8 = 0;
const MAP_KIND_RLE: u8 = 1;
const MAP_KIND_RAW_SMALL: u8 = 2;

/// Maximum macroblocks per frame side accepted by this module (hostile-input cap).
pub const PARTITION_V2_MAX_MB_DIM: u32 = 4096;

/// One inter MB partition mode (matches `FR2` **P** wire low two bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartitionModeV2 {
    Int16x16,
    Int16x8,
    Int8x16,
    Int8x8,
}

impl PartitionModeV2 {
    /// Must be a legal `inter_mv` partition wire byte (`0..=3`, no reserved bits).
    pub fn from_wire(b: u8) -> Result<Self, PartitionSyntaxV2Error> {
        let v = validate_partition_reserved_bits(b)
            .map_err(|_| PartitionSyntaxV2Error::InvalidMode(b))?;
        match v {
            0 => Ok(Self::Int16x16),
            1 => Ok(Self::Int16x8),
            2 => Ok(Self::Int8x16),
            3 => Ok(Self::Int8x8),
            _ => Err(PartitionSyntaxV2Error::InvalidMode(b)),
        }
    }

    #[inline]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Int16x16 => 0,
            Self::Int16x8 => 1,
            Self::Int8x16 => 2,
            Self::Int8x8 => 3,
        }
    }
}

/// Raster-order **one mode per macroblock** (width-major `mbx`, then `mby`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionMapV2 {
    pub mb_cols: u32,
    pub mb_rows: u32,
    pub modes: Vec<PartitionModeV2>,
}

impl PartitionMapV2 {
    pub fn new(
        mb_cols: u32,
        mb_rows: u32,
        modes: Vec<PartitionModeV2>,
    ) -> Result<Self, PartitionSyntaxV2Error> {
        if mb_cols == 0 || mb_rows == 0 {
            return Err(PartitionSyntaxV2Error::ZeroGrid);
        }
        if mb_cols > PARTITION_V2_MAX_MB_DIM || mb_rows > PARTITION_V2_MAX_MB_DIM {
            return Err(PartitionSyntaxV2Error::GridTooLarge);
        }
        let n = (mb_cols as u64).saturating_mul(mb_rows as u64);
        if n > usize::MAX as u64 {
            return Err(PartitionSyntaxV2Error::GridTooLarge);
        }
        let n_mb = n as usize;
        if modes.len() != n_mb {
            return Err(PartitionSyntaxV2Error::ModeCountMismatch {
                expected: n_mb,
                got: modes.len(),
            });
        }
        Ok(Self {
            mb_cols,
            mb_rows,
            modes,
        })
    }

    #[inline]
    pub fn n_mb(&self) -> usize {
        self.modes.len()
    }
}

/// One run of identical MB modes in raster order (RLE primitive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionRunV2 {
    pub mode: PartitionModeV2,
    pub count: u32,
}

/// MV-sharing equivalence class: **`members[0]`** is the representative PU index (carries coded delta upstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvShareGroupV2 {
    pub members: Vec<u32>,
}

impl MvShareGroupV2 {
    pub fn new(members: Vec<u32>) -> Result<Self, PartitionSyntaxV2Error> {
        if members.len() < 2 {
            return Err(PartitionSyntaxV2Error::MvGroupTooSmall);
        }
        let mut seen = HashSet::new();
        for &m in &members {
            if !seen.insert(m) {
                return Err(PartitionSyntaxV2Error::DuplicatePuIndexInGroup);
            }
        }
        Ok(Self { members })
    }
}

/// Aggregated byte accounting from [`estimate_partition_syntax_v2_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionSyntaxV2Stats {
    pub map_wire_bytes: usize,
    pub mv_groups_wire_bytes: usize,
    pub map_kind: u8,
    pub n_mb: u32,
    pub n_runs: u32,
    /// Legacy **one-byte-per-MB** v1 macroblock-mode storage size (`n_mb`).
    pub v1_legacy_map_bytes: usize,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PartitionSyntaxV2Error {
    #[error("truncated payload")]
    Truncated,
    #[error("bad partition map magic or version")]
    BadMapHeader,
    #[error("bad MV-share blob magic or version")]
    BadMvShareHeader,
    #[error("unknown map encoding kind {0}")]
    UnknownMapKind(u8),
    #[error("invalid partition mode wire value {0}")]
    InvalidMode(u8),
    #[error("zero run length in RLE")]
    ZeroRunLength,
    #[error("RLE run count out of range (n_runs={got}, n_mb={n_mb})")]
    RleRunCountOutOfRange { got: u32, n_mb: u32 },
    #[error("RLE expanded length mismatch (expected {expected}, got {got})")]
    RleLengthMismatch { expected: usize, got: usize },
    #[error("trailing bytes after partition map")]
    TrailingMapBytes,
    #[error("trailing bytes after MV-share blob")]
    TrailingMvShareBytes,
    #[error("RAW_SMALL map illegal for n_mb > 5")]
    RawSmallMapTooLarge,
    #[error("zero grid dimension")]
    ZeroGrid,
    #[error("macroblock grid too large for this module")]
    GridTooLarge,
    #[error("mode vector length mismatch (expected {expected}, got {got})")]
    ModeCountMismatch { expected: usize, got: usize },
    #[error("MV-share group needs at least two members")]
    MvGroupTooSmall,
    #[error("duplicate PU index inside a share group")]
    DuplicatePuIndexInGroup,
    #[error("PU index {index} out of range (total_pu_slots={total})")]
    PuIndexOutOfRange { index: u32, total: usize },
    #[error("PU index {0} duplicated across MV-share groups")]
    DuplicatePuAcrossGroups(u32),
    #[error("missing PU leaf: group references {index} but PU slots only count {total}")]
    MissingPuLeaf { index: u32, total: usize },
}

/// Legacy v1 storage size for the same mode sequence (one `u8` per macroblock).
#[inline]
pub fn v1_legacy_partition_map_bytes(n_mb: usize) -> usize {
    n_mb
}

/// Count total PU slots for MV indexing / validation.
pub fn total_pu_slots_for_modes(modes: &[PartitionModeV2]) -> Result<usize, PartitionSyntaxV2Error> {
    let mut t = 0usize;
    for m in modes {
        let w = m.to_wire();
        let n = pu_count_partition_wire(w).map_err(|_| PartitionSyntaxV2Error::InvalidMode(w))?;
        t = t
            .checked_add(n)
            .ok_or(PartitionSyntaxV2Error::GridTooLarge)?;
    }
    Ok(t)
}

/// Validate logical map dimensions and per-mode wire values.
pub fn validate_partition_map_v2(map: &PartitionMapV2) -> Result<(), PartitionSyntaxV2Error> {
    let n = (map.mb_cols as u64).saturating_mul(map.mb_rows as u64);
    if map.mb_cols == 0 || map.mb_rows == 0 {
        return Err(PartitionSyntaxV2Error::ZeroGrid);
    }
    if map.mb_cols > PARTITION_V2_MAX_MB_DIM || map.mb_rows > PARTITION_V2_MAX_MB_DIM {
        return Err(PartitionSyntaxV2Error::GridTooLarge);
    }
    if n > usize::MAX as u64 {
        return Err(PartitionSyntaxV2Error::GridTooLarge);
    }
    let n_mb = n as usize;
    if map.modes.len() != n_mb {
        return Err(PartitionSyntaxV2Error::ModeCountMismatch {
            expected: n_mb,
            got: map.modes.len(),
        });
    }
    for m in &map.modes {
        let _ = pu_count_partition_wire(m.to_wire())
            .map_err(|_| PartitionSyntaxV2Error::InvalidMode(m.to_wire()))?;
    }
    Ok(())
}

fn modes_to_rle_runs(modes: &[PartitionModeV2]) -> Vec<PartitionRunV2> {
    if modes.is_empty() {
        return Vec::new();
    }
    let mut runs = Vec::new();
    let mut cur = modes[0];
    let mut cnt: u32 = 1;
    for &m in &modes[1..] {
        if m == cur && cnt < u32::from(u16::MAX) {
            cnt += 1;
        } else {
            runs.push(PartitionRunV2 { mode: cur, count: cnt });
            cur = m;
            cnt = 1;
        }
    }
    runs.push(PartitionRunV2 { mode: cur, count: cnt });
    runs
}

fn encode_map_inner(map: &PartitionMapV2) -> Result<(Vec<u8>, u8, u32), PartitionSyntaxV2Error> {
    validate_partition_map_v2(map)?;
    let n_mb = map.n_mb();
    let n_mb_u32 = u32::try_from(n_mb).map_err(|_| PartitionSyntaxV2Error::GridTooLarge)?;

    let all_same = map.modes.iter().all(|&m| m == map.modes[0]);

    let (body, kind, n_runs) = if all_same {
        let mut v = Vec::with_capacity(6);
        v.extend_from_slice(&PARTITION_MAP_V2_MAGIC);
        v.push(MAP_KIND_UNIFORM);
        v.push(map.modes[0].to_wire());
        (v, MAP_KIND_UNIFORM, 1)
    } else if n_mb <= 5 {
        let mut v = Vec::with_capacity(5 + n_mb);
        v.extend_from_slice(&PARTITION_MAP_V2_MAGIC);
        v.push(MAP_KIND_RAW_SMALL);
        for mo in &map.modes {
            v.push(mo.to_wire());
        }
        (v, MAP_KIND_RAW_SMALL, n_mb as u32)
    } else {
        let runs = modes_to_rle_runs(&map.modes);
        let n_runs = runs.len();
        if n_runs > n_mb {
            return Err(PartitionSyntaxV2Error::RleRunCountOutOfRange {
                got: n_runs as u32,
                n_mb: n_mb_u32,
            });
        }
        let n_runs_u16 = u16::try_from(n_runs).map_err(|_| PartitionSyntaxV2Error::GridTooLarge)?;
        let mut v = Vec::with_capacity(7 + n_runs * 3);
        v.extend_from_slice(&PARTITION_MAP_V2_MAGIC);
        v.push(MAP_KIND_RLE);
        v.extend_from_slice(&n_runs_u16.to_le_bytes());
        for r in &runs {
            if r.count == 0 {
                return Err(PartitionSyntaxV2Error::ZeroRunLength);
            }
            v.push(r.mode.to_wire());
            let c = u16::try_from(r.count).map_err(|_| PartitionSyntaxV2Error::GridTooLarge)?;
            v.extend_from_slice(&c.to_le_bytes());
        }
        (v, MAP_KIND_RLE, n_runs as u32)
    };

    Ok((body, kind, n_runs))
}

/// Encode [`PartitionMapV2`] to a self-contained blob (see `docs/partition_syntax_v2.md`).
pub fn encode_partition_map_v2(map: &PartitionMapV2) -> Result<Vec<u8>, PartitionSyntaxV2Error> {
    encode_map_inner(map).map(|(v, _, _)| v)
}

/// Decode partition map bytes for a known `mb_cols × mb_rows` grid.
pub fn decode_partition_map_v2(
    data: &[u8],
    mb_cols: u32,
    mb_rows: u32,
) -> Result<PartitionMapV2, PartitionSyntaxV2Error> {
    if mb_cols == 0 || mb_rows == 0 {
        return Err(PartitionSyntaxV2Error::ZeroGrid);
    }
    let n = (mb_cols as u64).saturating_mul(mb_rows as u64);
    if n > usize::MAX as u64 {
        return Err(PartitionSyntaxV2Error::GridTooLarge);
    }
    let n_mb = n as usize;
    if data.len() < 5 {
        return Err(PartitionSyntaxV2Error::Truncated);
    }
    if data.get(..4) != Some(&PARTITION_MAP_V2_MAGIC) {
        return Err(PartitionSyntaxV2Error::BadMapHeader);
    }
    let mut cur = 4usize;
    let kind = *data.get(cur).ok_or(PartitionSyntaxV2Error::Truncated)?;
    cur += 1;

    let modes: Vec<PartitionModeV2> = match kind {
        MAP_KIND_UNIFORM => {
            let m = *data.get(cur).ok_or(PartitionSyntaxV2Error::Truncated)?;
            cur += 1;
            let mode = PartitionModeV2::from_wire(m)?;
            if cur != data.len() {
                return Err(PartitionSyntaxV2Error::TrailingMapBytes);
            }
            vec![mode; n_mb]
        }
        MAP_KIND_RAW_SMALL => {
            if n_mb > 5 {
                return Err(PartitionSyntaxV2Error::RawSmallMapTooLarge);
            }
            if data.len().saturating_sub(cur) < n_mb {
                return Err(PartitionSyntaxV2Error::Truncated);
            }
            let mut out = Vec::with_capacity(n_mb);
            for _ in 0..n_mb {
                let b = data[cur];
                cur += 1;
                out.push(PartitionModeV2::from_wire(b)?);
            }
            if cur != data.len() {
                return Err(PartitionSyntaxV2Error::TrailingMapBytes);
            }
            out
        }
        MAP_KIND_RLE => {
            if data.len().saturating_sub(cur) < 2 {
                return Err(PartitionSyntaxV2Error::Truncated);
            }
            let n_runs = u16::from_le_bytes([data[cur], data[cur + 1]]) as u32;
            cur += 2;
            if n_runs == 0 || (n_runs as usize) > n_mb {
                return Err(PartitionSyntaxV2Error::RleRunCountOutOfRange {
                    got: n_runs,
                    n_mb: n_mb as u32,
                });
            }
            let mut out = Vec::with_capacity(n_mb);
            let mut total = 0usize;
            for _ in 0..n_runs {
                if data.len().saturating_sub(cur) < 3 {
                    return Err(PartitionSyntaxV2Error::Truncated);
                }
                let m = data[cur];
                cur += 1;
                let mode = PartitionModeV2::from_wire(m)?;
                let count = u16::from_le_bytes([data[cur], data[cur + 1]]) as usize;
                cur += 2;
                if count == 0 {
                    return Err(PartitionSyntaxV2Error::ZeroRunLength);
                }
                total = total
                    .checked_add(count)
                    .ok_or(PartitionSyntaxV2Error::GridTooLarge)?;
                if total > n_mb {
                    return Err(PartitionSyntaxV2Error::RleLengthMismatch {
                        expected: n_mb,
                        got: total,
                    });
                }
                out.extend(std::iter::repeat_n(mode, count));
            }
            if total != n_mb {
                return Err(PartitionSyntaxV2Error::RleLengthMismatch {
                    expected: n_mb,
                    got: total,
                });
            }
            if cur != data.len() {
                return Err(PartitionSyntaxV2Error::TrailingMapBytes);
            }
            out
        }
        k => return Err(PartitionSyntaxV2Error::UnknownMapKind(k)),
    };

    PartitionMapV2::new(mb_cols, mb_rows, modes)
}

fn validate_mv_groups_indices(
    groups: &[MvShareGroupV2],
    total_pu_slots: usize,
) -> Result<(), PartitionSyntaxV2Error> {
    let mut global_seen = HashSet::new();
    for g in groups {
        if g.members.len() < 2 {
            return Err(PartitionSyntaxV2Error::MvGroupTooSmall);
        }
        let mut local = HashSet::new();
        for &idx in &g.members {
            let ii = usize::try_from(idx).map_err(|_| PartitionSyntaxV2Error::PuIndexOutOfRange {
                index: idx,
                total: total_pu_slots,
            })?;
            if ii >= total_pu_slots {
                return Err(PartitionSyntaxV2Error::MissingPuLeaf {
                    index: idx,
                    total: total_pu_slots,
                });
            }
            if !local.insert(idx) {
                return Err(PartitionSyntaxV2Error::DuplicatePuIndexInGroup);
            }
            if !global_seen.insert(idx) {
                return Err(PartitionSyntaxV2Error::DuplicatePuAcrossGroups(idx));
            }
        }
    }
    Ok(())
}

/// Encode MV-share groups (`S2G1` wire). **`total_pu_slots`** must match [`total_pu_slots_for_modes`] for the paired map.
pub fn encode_mv_share_groups_v2(
    groups: &[MvShareGroupV2],
    total_pu_slots: usize,
) -> Result<Vec<u8>, PartitionSyntaxV2Error> {
    validate_mv_groups_indices(groups, total_pu_slots)?;
    let n_g = u16::try_from(groups.len()).map_err(|_| PartitionSyntaxV2Error::GridTooLarge)?;
    let mut v = Vec::new();
    v.extend_from_slice(&MV_SHARE_GROUPS_V2_MAGIC);
    v.extend_from_slice(&n_g.to_le_bytes());
    for g in groups {
        let n_m =
            u16::try_from(g.members.len()).map_err(|_| PartitionSyntaxV2Error::GridTooLarge)?;
        v.extend_from_slice(&n_m.to_le_bytes());
        for &mem in &g.members {
            let x = u16::try_from(mem).map_err(|_| PartitionSyntaxV2Error::PuIndexOutOfRange {
                index: mem,
                total: total_pu_slots,
            })?;
            v.extend_from_slice(&x.to_le_bytes());
        }
    }
    Ok(v)
}

/// Decode MV-share groups; enforces no trailing bytes.
pub fn decode_mv_share_groups_v2(
    data: &[u8],
    total_pu_slots: usize,
) -> Result<Vec<MvShareGroupV2>, PartitionSyntaxV2Error> {
    if data.len() < 6 {
        return Err(PartitionSyntaxV2Error::Truncated);
    }
    if data.get(..4) != Some(&MV_SHARE_GROUPS_V2_MAGIC) {
        return Err(PartitionSyntaxV2Error::BadMvShareHeader);
    }
    let mut cur = 4usize;
    let n_groups = u16::from_le_bytes([data[cur], data[cur + 1]]) as usize;
    cur += 2;
    let mut groups = Vec::with_capacity(n_groups);
    for _ in 0..n_groups {
        if data.len().saturating_sub(cur) < 2 {
            return Err(PartitionSyntaxV2Error::Truncated);
        }
        let n_mem = u16::from_le_bytes([data[cur], data[cur + 1]]) as usize;
        cur += 2;
        if n_mem < 2 {
            return Err(PartitionSyntaxV2Error::MvGroupTooSmall);
        }
        if data.len().saturating_sub(cur) < n_mem.saturating_mul(2) {
            return Err(PartitionSyntaxV2Error::Truncated);
        }
        let mut members = Vec::with_capacity(n_mem);
        for _ in 0..n_mem {
            let idx = u16::from_le_bytes([data[cur], data[cur + 1]]) as u32;
            cur += 2;
            members.push(idx);
        }
        groups.push(MvShareGroupV2 { members });
    }
    if cur != data.len() {
        return Err(PartitionSyntaxV2Error::TrailingMvShareBytes);
    }
    validate_mv_groups_indices(&groups, total_pu_slots)?;
    Ok(groups)
}

/// Combined byte-length estimate for map + optional MV-share blob.
pub fn estimate_partition_syntax_v2_bytes(
    map: &PartitionMapV2,
    mv_groups: Option<&[MvShareGroupV2]>,
    total_pu_slots: usize,
) -> Result<PartitionSyntaxV2Stats, PartitionSyntaxV2Error> {
    let (map_wire, kind, n_runs) = encode_map_inner(map)?;
    let mv_b = if let Some(gs) = mv_groups {
        validate_mv_groups_indices(gs, total_pu_slots)?;
        encode_mv_share_groups_v2(gs, total_pu_slots)?.len()
    } else {
        0
    };
    let n_mb = map.n_mb();
    Ok(PartitionSyntaxV2Stats {
        map_wire_bytes: map_wire.len(),
        mv_groups_wire_bytes: mv_b,
        map_kind: kind,
        n_mb: u32::try_from(n_mb).unwrap_or(u32::MAX),
        n_runs,
        v1_legacy_map_bytes: v1_legacy_partition_map_bytes(n_mb),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_uniform(cols: u32, rows: u32, m: PartitionModeV2) -> PartitionMapV2 {
        let n = (cols * rows) as usize;
        PartitionMapV2::new(cols, rows, vec![m; n]).unwrap()
    }

    #[test]
    fn all_16x16_roundtrip() {
        let m = map_uniform(8, 8, PartitionModeV2::Int16x16);
        let enc = encode_partition_map_v2(&m).unwrap();
        let dec = decode_partition_map_v2(&enc, 8, 8).unwrap();
        assert_eq!(dec, m);
    }

    #[test]
    fn all_8x8_roundtrip() {
        let m = map_uniform(4, 4, PartitionModeV2::Int8x8);
        let enc = encode_partition_map_v2(&m).unwrap();
        let dec = decode_partition_map_v2(&enc, 4, 4).unwrap();
        assert_eq!(dec, m);
    }

    #[test]
    fn mixed_moving_square_like_roundtrip() {
        // 8×8 MBs: mostly 16×16, one 8×8 split in the center.
        let mut modes = vec![PartitionModeV2::Int16x16; 64];
        modes[27] = PartitionModeV2::Int8x8;
        let m = PartitionMapV2::new(8, 8, modes).unwrap();
        let enc = encode_partition_map_v2(&m).unwrap();
        let dec = decode_partition_map_v2(&enc, 8, 8).unwrap();
        assert_eq!(dec, m);
    }

    #[test]
    fn rle_malformed_zero_run_rejected() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&PARTITION_MAP_V2_MAGIC);
        blob.push(MAP_KIND_RLE);
        blob.extend_from_slice(&1u16.to_le_bytes()); // n_runs
        blob.push(PartitionModeV2::Int16x16.to_wire());
        blob.extend_from_slice(&0u16.to_le_bytes()); // zero count — illegal
        assert!(decode_partition_map_v2(&blob, 4, 1).is_err());
    }

    #[test]
    fn truncated_map_rejected() {
        let blob = PARTITION_MAP_V2_MAGIC.to_vec();
        assert_eq!(
            decode_partition_map_v2(&blob, 2, 2),
            Err(PartitionSyntaxV2Error::Truncated)
        );
    }

    #[test]
    fn invalid_mode_rejected() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&PARTITION_MAP_V2_MAGIC);
        blob.push(MAP_KIND_UNIFORM);
        blob.push(0xFF);
        assert!(decode_partition_map_v2(&blob, 1, 1).is_err());
    }

    #[test]
    fn mv_share_missing_leaf_rejected() {
        let map = map_uniform(2, 2, PartitionModeV2::Int16x16);
        let total = total_pu_slots_for_modes(&map.modes).unwrap();
        assert_eq!(total, 4);
        let g = MvShareGroupV2::new(vec![0, 99]).unwrap();
        assert!(encode_mv_share_groups_v2(&[g], total).is_err());
    }

    #[test]
    fn all_16x16_byte_length_le_v1_for_mid_grid() {
        let cols = 16u32;
        let rows = 16u32;
        let m = map_uniform(cols, rows, PartitionModeV2::Int16x16);
        let enc = encode_partition_map_v2(&m).unwrap();
        let v1 = v1_legacy_partition_map_bytes(m.n_mb());
        assert!(
            enc.len() <= v1,
            "v2 {} bytes vs v1 {} bytes",
            enc.len(),
            v1
        );
    }

    #[test]
    fn mostly_16x16_sparse_split_smaller_than_v1() {
        let cols = 10u32;
        let rows = 10u32;
        let mut modes = vec![PartitionModeV2::Int16x16; 100];
        modes[42] = PartitionModeV2::Int8x8;
        let m = PartitionMapV2::new(cols, rows, modes).unwrap();
        let enc = encode_partition_map_v2(&m).unwrap();
        let v1 = v1_legacy_partition_map_bytes(m.n_mb());
        assert!(enc.len() < v1, "v2 {} v1 {}", enc.len(), v1);
    }

    #[test]
    fn mv_share_roundtrip() {
        let map = map_uniform(2, 2, PartitionModeV2::Int8x8);
        let total = total_pu_slots_for_modes(&map.modes).unwrap();
        let g1 = MvShareGroupV2::new(vec![0, 1]).unwrap();
        let g2 = MvShareGroupV2::new(vec![2, 3]).unwrap();
        let enc = encode_mv_share_groups_v2(&[g1.clone(), g2.clone()], total).unwrap();
        let dec = decode_mv_share_groups_v2(&enc, total).unwrap();
        assert_eq!(dec, vec![g1, g2]);
    }

    #[test]
    fn mv_share_duplicate_across_groups_rejected() {
        let map = map_uniform(2, 2, PartitionModeV2::Int8x8);
        let total = total_pu_slots_for_modes(&map.modes).unwrap();
        let g1 = MvShareGroupV2::new(vec![0, 1]).unwrap();
        let g2 = MvShareGroupV2::new(vec![1, 2]).unwrap();
        assert!(encode_mv_share_groups_v2(&[g1, g2], total).is_err());
    }

    #[test]
    fn partition_map_trailing_byte_rejected() {
        let m = map_uniform(4, 4, PartitionModeV2::Int16x16);
        let mut enc = encode_partition_map_v2(&m).unwrap();
        enc.push(0);
        assert_eq!(
            decode_partition_map_v2(&enc, 4, 4),
            Err(PartitionSyntaxV2Error::TrailingMapBytes)
        );
    }

    #[test]
    fn estimate_stats_matches_encode() {
        let m = map_uniform(8, 8, PartitionModeV2::Int16x16);
        let stats = estimate_partition_syntax_v2_bytes(&m, None, 0).unwrap();
        assert_eq!(stats.map_wire_bytes, encode_partition_map_v2(&m).unwrap().len());
        assert_eq!(stats.map_kind, MAP_KIND_UNIFORM);
    }
}
