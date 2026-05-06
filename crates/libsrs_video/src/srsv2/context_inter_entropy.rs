//! **ContextV1** inter motion-vector entropy for SRSV2 (**FR2** rev **23+**).
//!
//! # Model
//!
//! ContextV1 uses **16 independent static rANS frequency tables** (one per context ID). Each table
//! is fixed for the session: **not** CABAC-class adaptive arithmetic coding. Conditioning selects
//! *which* static table encodes the next compact-MV byte; tables themselves do not update from
//! previously coded symbols in this revision.
//!
//! # Properties
//!
//! - **Deterministic:** same compact MV bytes + grid/partition layout ⇒ same context IDs and codeword.
//! - **Hostile-input bounded:** decode uses an explicit per-payload **symbol decode budget**
//!   (`decode_budget`) so degenerate bitstreams cannot force unbounded rANS renorm work.
//!
//! Callers build **compact** MV bytes via [`crate::srsv2::inter_mv`] (`encode_mv_grid_compact`,
//! `encode_mv_stream_partitioned`). This module owns labeling + multi-context rANS only.
//!
//! ## Which entry points to use
//!
//! | Scenario | Encode API | Decode API |
//! |----------|------------|------------|
//! | Fixed **16×16** MB grid (**FR2** rev **23** / **24**) | [`encode_mv_context_v1_fixed`] | [`decode_mv_context_v1_fixed`] |
//! | **Variable partitions** — one partition byte per MB, partitioned PU MV scan (**FR2** rev **25**) | [`encode_mv_context_v1_partitioned`] | [`decode_mv_context_v1_partitioned`] |
//!
//! Compact MV bytes must match [`crate::srsv2::inter_mv`] packing for the same `(mb_cols, mb_rows)` grid.
//! **`decode_budget`** counts **rANS normalize/decode steps** per payload (see decode helpers): exhausted budget ⇒ [`crate::srsv2::error::SrsV2Error::syntax`] — hostile streams cannot force unbounded decoder work.

use libsrs_bitio::{
    rans_decode_step_symbol, rans_encode_symbols_multi_context, RansModel, RANS_SCALE,
};

use super::error::SrsV2Error;
use super::inter_mv::{read_signed_varint, validate_partition_reserved_bits};

const MV_CONTEXT_COUNT: usize = 16;

/// Snapshot of the ContextV1 MV entropy design (for diagnostics / tooling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextV1ModelSummary {
    /// Distinct context IDs: **`0 .. context_count`**.
    pub context_count: usize,
    /// rANS frequency normalization scale (matches [`RANS_SCALE`]).
    pub rans_scale: u32,
    /// Short description of how per-context frequencies are derived (fixed, indexed tables).
    pub model_derivation: &'static str,
}

/// Returns a stable summary of the ContextV1 static multi-context rANS MV entropy model (table
/// count, rANS scale, derivation note). For benchmark JSON and diagnostics only — **no bitrate
/// claim** vs other codecs.
#[must_use]
pub fn context_model_summary() -> ContextV1ModelSummary {
    ContextV1ModelSummary {
        context_count: MV_CONTEXT_COUNT,
        rans_scale: RANS_SCALE,
        model_derivation: "per-context-id static 256-symbol frequency tables; deterministic tweak from context index",
    }
}

/// Encode **compact** MV bytes (**fixed 16×16 macroblock grid**) with ContextV1 multi-context rANS.
///
/// # Errors
///
/// MV compact length does not match the `(mb_cols, mb_rows)` grid implied by labeling, or rANS
/// encoder failure → [`SrsV2Error::Syntax`].
///
/// `mv_compact` must be exactly the wire produced by [`crate::srsv2::inter_mv::encode_mv_grid_compact`]
/// for `(mb_cols * mb_rows)` macroblocks.
pub fn encode_mv_context_v1_fixed(
    mv_compact: &[u8],
    mb_cols: u32,
    mb_rows: u32,
) -> Result<Vec<u8>, SrsV2Error> {
    let labels = fixed_grid_context_labels(mv_compact, mb_cols, mb_rows)?;
    encode_mv_compact_with_context_labels(mv_compact, &labels)
}

/// Decode ContextV1 MV rANS blob for a **fixed macroblock grid** back to compact MV bytes.
///
/// `sym_count` is the compact byte length stored beside the blob on the wire.
///
/// **`decode_budget`:** upper bound on internal **rANS step work** (renormalization + symbol pulls). If
/// the bitstream would require more steps than allowed (including malformed/corrupt blobs), decode fails
/// with a syntax error rather than looping indefinitely.
///
/// # Errors
///
/// Truncated header, `sym_count` mismatch vs bitstream, corrupt state, exhausted `decode_budget`,
/// MV grid vs compact mismatch, or trailing bytes → [`SrsV2Error::Syntax`] or [`SrsV2Error::Truncated`].
pub fn decode_mv_context_v1_fixed(
    blob: &[u8],
    sym_count: usize,
    mb_cols: u32,
    mb_rows: u32,
    decode_budget: usize,
) -> Result<Vec<u8>, SrsV2Error> {
    decode_mv_compact_streaming_fixed(blob, sym_count, mb_cols, mb_rows, decode_budget)
}

/// Encode **compact** MV bytes for **variable partition** layouts with ContextV1 multi-context rANS.
///
/// # Errors
///
/// `partition_types.len() != mb_cols * mb_rows`, illegal partition wire tags, MV stream / PU count
/// mismatch vs partitions, or rANS failure → [`SrsV2Error::Syntax`].
///
/// `partition_types` is one wire partition byte per macroblock (see [`crate::srsv2::inter_mv`]).
pub fn encode_mv_context_v1_partitioned(
    mv_compact: &[u8],
    mb_cols: u32,
    mb_rows: u32,
    partition_types: &[u8],
) -> Result<Vec<u8>, SrsV2Error> {
    let labels = partitioned_context_labels(mv_compact, mb_cols, mb_rows, partition_types)?;
    encode_mv_compact_with_context_labels(mv_compact, &labels)
}

/// Decode ContextV1 MV rANS for **partitioned** PU streams back to compact MV bytes.
///
/// **`decode_budget`:** same semantics as [`decode_mv_context_v1_fixed`] — bounds rANS normalize/decode
/// steps so hostile blobs cannot force unbounded work.
///
/// # Errors
///
/// Partition map length mismatch, malformed partition types, truncated/corrupt blob, symbol count
/// mismatch, exhausted `decode_budget`, or trailing bytes after decode → [`SrsV2Error::Syntax`] or
/// [`SrsV2Error::Truncated`].
pub fn decode_mv_context_v1_partitioned(
    blob: &[u8],
    sym_count: usize,
    mb_cols: u32,
    mb_rows: u32,
    partition_types: &[u8],
    decode_budget: usize,
) -> Result<Vec<u8>, SrsV2Error> {
    decode_mv_compact_streaming_partitioned(
        blob,
        sym_count,
        mb_cols,
        mb_rows,
        partition_types,
        decode_budget,
    )
}

// --- labeling (private): one context byte per compact MV byte ------------------------------------

#[inline]
fn delta_mag_bucket_qpel(d: i32) -> u8 {
    let a = d.unsigned_abs();
    match a {
        0 => 0,
        1..=8 => 1,
        9..=32 => 2,
        _ => 3,
    }
}

#[inline]
fn mv_context_id_from_prev_residual_deltas(prev_dx: i32, prev_dy: i32) -> u8 {
    let bx = delta_mag_bucket_qpel(prev_dx);
    let by = delta_mag_bucket_qpel(prev_dy);
    (bx << 2) | by
}

fn fixed_grid_context_labels(
    compact: &[u8],
    mb_cols: u32,
    mb_rows: u32,
) -> Result<Vec<u8>, SrsV2Error> {
    let mut v = Vec::with_capacity(compact.len());
    let mut cur = 0usize;
    let mut prev_dx = 0_i32;
    let mut prev_dy = 0_i32;
    let n_mb = (mb_cols * mb_rows) as usize;
    for _ in 0..n_mb {
        let ctx = mv_context_id_from_prev_residual_deltas(prev_dx, prev_dy);
        let s1 = cur;
        let dx = read_signed_varint(compact, &mut cur)?;
        for _ in s1..cur {
            v.push(ctx);
        }
        let s2 = cur;
        let dy = read_signed_varint(compact, &mut cur)?;
        for _ in s2..cur {
            v.push(ctx);
        }
        prev_dx = dx;
        prev_dy = dy;
    }
    if cur != compact.len() || v.len() != compact.len() {
        return Err(SrsV2Error::syntax("MV compact/context labeling mismatch"));
    }
    Ok(v)
}

fn partitioned_context_labels(
    compact: &[u8],
    mb_cols: u32,
    mb_rows: u32,
    partition_types: &[u8],
) -> Result<Vec<u8>, SrsV2Error> {
    let n_mb = (mb_cols * mb_rows) as usize;
    if partition_types.len() != n_mb {
        return Err(SrsV2Error::syntax("partition map length mismatch"));
    }
    let mut v = Vec::with_capacity(compact.len());
    let mut cur = 0usize;
    let mut prev_res = (0_i32, 0_i32);

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let pt = validate_partition_reserved_bits(partition_types[idx])?;
            let npu = super::inter_mv::pu_count_partition_wire(pt)?;
            for _ in 0..npu {
                let ctx = mv_context_id_from_prev_residual_deltas(prev_res.0, prev_res.1);
                let s1 = cur;
                let dx = read_signed_varint(compact, &mut cur)?;
                for _ in s1..cur {
                    v.push(ctx);
                }
                let s2 = cur;
                let dy = read_signed_varint(compact, &mut cur)?;
                for _ in s2..cur {
                    v.push(ctx);
                }
                prev_res = (dx, dy);
            }
        }
    }
    if cur != compact.len() || v.len() != compact.len() {
        return Err(SrsV2Error::syntax(
            "partition MV compact/context labeling mismatch",
        ));
    }
    Ok(v)
}

// --- rANS core (private) -----------------------------------------------------------------------

fn model_for_ctx(ctx: usize) -> Result<RansModel, SrsV2Error> {
    let mut freqs = vec![14_u32; 256];
    freqs[0] = RANS_SCALE - 255 * 14;
    let t = ((ctx as u32 * 11) % 64).min(freqs[0] / 4);
    freqs[1] = freqs[1].saturating_add(t);
    freqs[0] = freqs[0].saturating_sub(t);
    RansModel::try_from_freqs(freqs).map_err(|_| SrsV2Error::syntax("MV context rANS model"))
}

fn build_context_models_v1() -> Result<[RansModel; MV_CONTEXT_COUNT], SrsV2Error> {
    let v: Vec<RansModel> = (0..MV_CONTEXT_COUNT)
        .map(model_for_ctx)
        .collect::<Result<_, _>>()?;
    v.try_into()
        .map_err(|_| SrsV2Error::syntax("MV context model array"))
}

fn encode_mv_compact_with_context_labels(
    compact: &[u8],
    context_labels: &[u8],
) -> Result<Vec<u8>, SrsV2Error> {
    if compact.len() != context_labels.len() {
        return Err(SrsV2Error::syntax("MV context length mismatch"));
    }
    for &c in context_labels {
        if usize::from(c) >= MV_CONTEXT_COUNT {
            return Err(SrsV2Error::syntax("MV context id out of range"));
        }
    }
    let models_arr = build_context_models_v1()?;
    let models: Vec<RansModel> = models_arr.into_iter().collect();
    let symbols: Vec<usize> = compact.iter().map(|&b| usize::from(b)).collect();
    rans_encode_symbols_multi_context(&models, &symbols, context_labels)
        .map_err(|_| SrsV2Error::syntax("MV context rANS encode"))
}

fn try_complete_varint(buf: &[u8]) -> Result<Option<(i32, usize)>, SrsV2Error> {
    let mut c = 0usize;
    match read_signed_varint(buf, &mut c) {
        Ok(v) if c == buf.len() => Ok(Some((v, c))),
        Ok(_) => Err(SrsV2Error::syntax("MV varint parse trailing")),
        Err(_) => Ok(None),
    }
}

struct MvFixedCtxParse {
    mb_left: usize,
    phase_dx: bool,
    prev: (i32, i32),
    dx_hold: i32,
    buf: Vec<u8>,
}

impl MvFixedCtxParse {
    fn new(mb_cols: u32, mb_rows: u32) -> Self {
        Self {
            mb_left: (mb_cols * mb_rows) as usize,
            phase_dx: true,
            prev: (0, 0),
            dx_hold: 0,
            buf: Vec::new(),
        }
    }

    fn peek_context(&self) -> u8 {
        mv_context_id_from_prev_residual_deltas(self.prev.0, self.prev.1)
    }

    fn push_symbol_byte(&mut self, b: u8) -> Result<(), SrsV2Error> {
        self.buf.push(b);
        if let Some((v, consumed)) = try_complete_varint(&self.buf)? {
            if consumed != self.buf.len() {
                return Err(SrsV2Error::syntax("MV varint trailing"));
            }
            self.buf.clear();
            if self.phase_dx {
                self.dx_hold = v;
                self.phase_dx = false;
            } else {
                self.prev = (self.dx_hold, v);
                self.phase_dx = true;
                self.mb_left = self.mb_left.saturating_sub(1);
            }
        }
        Ok(())
    }
}

struct MvPartitionCtxParse<'a> {
    mb_cols: u32,
    mb_rows: u32,
    partition_types: &'a [u8],
    mbx: u32,
    mby: u32,
    pu_idx: usize,
    npu_this_mb: usize,
    prev_res: (i32, i32),
    phase_dx: bool,
    dx_hold: i32,
    buf: Vec<u8>,
}

impl<'a> MvPartitionCtxParse<'a> {
    fn new(mb_cols: u32, mb_rows: u32, partition_types: &'a [u8]) -> Result<Self, SrsV2Error> {
        let n = (mb_cols * mb_rows) as usize;
        if partition_types.len() != n {
            return Err(SrsV2Error::syntax("partition map"));
        }
        let pt0 = validate_partition_reserved_bits(partition_types[0])?;
        let npu = super::inter_mv::pu_count_partition_wire(pt0)?;
        Ok(Self {
            mb_cols,
            mb_rows,
            partition_types,
            mbx: 0,
            mby: 0,
            pu_idx: 0,
            npu_this_mb: npu,
            prev_res: (0, 0),
            phase_dx: true,
            dx_hold: 0,
            buf: Vec::new(),
        })
    }

    fn peek_context(&self) -> u8 {
        mv_context_id_from_prev_residual_deltas(self.prev_res.0, self.prev_res.1)
    }

    fn advance_mb(&mut self) -> Result<(), SrsV2Error> {
        self.pu_idx = 0;
        self.mbx += 1;
        if self.mbx >= self.mb_cols {
            self.mbx = 0;
            self.mby += 1;
        }
        if self.mby >= self.mb_rows {
            self.npu_this_mb = 0;
            return Ok(());
        }
        let idx = (self.mby * self.mb_cols + self.mbx) as usize;
        let pt = validate_partition_reserved_bits(self.partition_types[idx])?;
        self.npu_this_mb = super::inter_mv::pu_count_partition_wire(pt)?;
        Ok(())
    }

    fn push_symbol_byte(&mut self, b: u8) -> Result<(), SrsV2Error> {
        self.buf.push(b);
        if let Some((v, consumed)) = try_complete_varint(&self.buf)? {
            if consumed != self.buf.len() {
                return Err(SrsV2Error::syntax("MV varint trailing"));
            }
            self.buf.clear();
            if self.phase_dx {
                self.dx_hold = v;
                self.phase_dx = false;
            } else {
                self.prev_res = (self.dx_hold, v);
                self.phase_dx = true;
                self.pu_idx += 1;
                if self.pu_idx >= self.npu_this_mb {
                    self.advance_mb()?;
                }
            }
        }
        Ok(())
    }
}

fn decode_mv_compact_streaming_fixed(
    blob: &[u8],
    sym_count: usize,
    mb_cols: u32,
    mb_rows: u32,
    decode_budget: usize,
) -> Result<Vec<u8>, SrsV2Error> {
    let models_arr = build_context_models_v1()?;
    let models: Vec<RansModel> = models_arr.into_iter().collect();
    if blob.len() < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let mut state = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]);
    let mut idx = 4usize;
    let mut steps = 0usize;
    let mut sm = MvFixedCtxParse::new(mb_cols, mb_rows);
    let mut out = Vec::with_capacity(sym_count);
    for _ in 0..sym_count {
        let ctx = sm.peek_context();
        let model = models
            .get(ctx as usize)
            .ok_or_else(|| SrsV2Error::syntax("bad MV context"))?;
        let sym =
            rans_decode_step_symbol(model, &mut state, blob, &mut idx, &mut steps, decode_budget)
                .map_err(|_| SrsV2Error::syntax("MV context rANS decode"))?;
        sm.push_symbol_byte(sym as u8)?;
        out.push(sym as u8);
    }
    if idx != blob.len() {
        return Err(SrsV2Error::syntax("trailing rANS bytes"));
    }
    Ok(out)
}

fn decode_mv_compact_streaming_partitioned(
    blob: &[u8],
    sym_count: usize,
    mb_cols: u32,
    mb_rows: u32,
    partition_types: &[u8],
    decode_budget: usize,
) -> Result<Vec<u8>, SrsV2Error> {
    let models_arr = build_context_models_v1()?;
    let models: Vec<RansModel> = models_arr.into_iter().collect();
    if blob.len() < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let mut state = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]);
    let mut idx = 4usize;
    let mut steps = 0usize;
    let mut sm = MvPartitionCtxParse::new(mb_cols, mb_rows, partition_types)?;
    let mut out = Vec::with_capacity(sym_count);
    for _ in 0..sym_count {
        let ctx = sm.peek_context();
        let model = models
            .get(ctx as usize)
            .ok_or_else(|| SrsV2Error::syntax("bad MV context"))?;
        let sym =
            rans_decode_step_symbol(model, &mut state, blob, &mut idx, &mut steps, decode_budget)
                .map_err(|_| SrsV2Error::syntax("MV partition context rANS decode"))?;
        sm.push_symbol_byte(sym as u8)?;
        out.push(sym as u8);
    }
    if idx != blob.len() {
        return Err(SrsV2Error::syntax("trailing rANS bytes"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::inter_mv::{
        encode_mv_grid_compact, encode_mv_stream_partitioned, rans_decode_mv_bytes,
        rans_encode_mv_bytes, P_PART_WIRE_8X8,
    };
    use crate::srsv2::SrsV2Error;
    use libsrs_bitio::{
        rans_decode_symbols_multi_context, rans_encode_symbols_multi_context, RansModel,
    };

    #[test]
    fn context_v1_apis_reexported_from_srsv2_root() {
        let s = crate::srsv2::context_model_summary();
        assert_eq!(s.context_count, MV_CONTEXT_COUNT);
        let compact = encode_mv_grid_compact(&[(0_i32, 0_i32)], 1, 1);
        let blob = crate::srsv2::encode_mv_context_v1_fixed(&compact, 1, 1).unwrap();
        let dec =
            crate::srsv2::decode_mv_context_v1_fixed(&blob, compact.len(), 1, 1, 4096).unwrap();
        assert_eq!(dec, compact);
    }

    #[test]
    fn context_model_summary_matches_constants() {
        let s = context_model_summary();
        assert_eq!(s.context_count, MV_CONTEXT_COUNT);
        assert_eq!(s.rans_scale, RANS_SCALE);
        assert!(!s.model_derivation.is_empty());
    }

    /// StaticV1 single-model MV rANS remains unchanged (baseline for ContextV1 work).
    #[test]
    fn static_v1_mv_rans_roundtrip_unchanged() {
        let mb_cols = 1u32;
        let mb_rows = 1u32;
        let mvs = vec![(8_i32, -4)];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let blob = rans_encode_mv_bytes(&compact).unwrap();
        let dec = rans_decode_mv_bytes(&blob, compact.len(), 512_000).unwrap();
        assert_eq!(dec, compact);
    }

    #[test]
    fn zero_mv_field_roundtrips() {
        let mb_cols = 3u32;
        let mb_rows = 2u32;
        let mvs = vec![(0_i32, 0_i32); 6];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let blob = encode_mv_context_v1_fixed(&compact, mb_cols, mb_rows).unwrap();
        let dec =
            decode_mv_context_v1_fixed(&blob, compact.len(), mb_cols, mb_rows, 512_000).unwrap();
        assert_eq!(dec, compact);
    }

    #[test]
    fn smooth_pan_mv_field_roundtrips() {
        let mb_cols = 3u32;
        let mb_rows = 2u32;
        let mvs = vec![(4, 0), (8, 0), (12, 0), (16, 0), (20, 0), (24, 0)];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let blob = encode_mv_context_v1_fixed(&compact, mb_cols, mb_rows).unwrap();
        let dec =
            decode_mv_context_v1_fixed(&blob, compact.len(), mb_cols, mb_rows, 512_000).unwrap();
        assert_eq!(dec, compact);
    }

    #[test]
    fn encode_rejects_invalid_context_id_injected() {
        let compact = vec![0_u8, 1_u8];
        let bad_ctx = vec![16_u8, 0_u8];
        let err = encode_mv_compact_with_context_labels(&compact, &bad_ctx).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn decode_truncated_rans_blob_fails() {
        let blob = [0_u8; 4];
        let err = decode_mv_context_v1_fixed(&blob, 4, 1, 1, 10_000).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_) | SrsV2Error::Truncated));
    }

    #[test]
    fn decode_trailing_garbage_rejected() {
        let mb_cols = 1u32;
        let mb_rows = 1u32;
        let mvs = vec![(4_i32, 0)];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let mut blob = encode_mv_context_v1_fixed(&compact, mb_cols, mb_rows).unwrap();
        blob.push(0xAB);
        let err = decode_mv_context_v1_fixed(&blob, compact.len(), mb_cols, mb_rows, 512_000)
            .unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn decode_corrupt_mid_blob_rejected() {
        let mb_cols = 4u32;
        let mb_rows = 4u32;
        let mvs = vec![(4_i32, 0); 16];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let mut blob = encode_mv_context_v1_fixed(&compact, mb_cols, mb_rows).unwrap();
        assert!(blob.len() > 16);
        for b in blob.iter_mut().skip(8).take(8) {
            *b = !*b;
        }
        let err = decode_mv_context_v1_fixed(&blob, compact.len(), mb_cols, mb_rows, 512_000)
            .unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_) | SrsV2Error::Truncated));
    }

    #[test]
    fn decode_symbol_budget_enforced() {
        let mb_cols = 2u32;
        let mb_rows = 2u32;
        let mvs = vec![(4_i32, 0); 4];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let blob = encode_mv_context_v1_fixed(&compact, mb_cols, mb_rows).unwrap();
        let err =
            decode_mv_context_v1_fixed(&blob, compact.len(), mb_cols, mb_rows, 0).unwrap_err();
        assert!(matches!(err, SrsV2Error::Syntax(_)));
    }

    #[test]
    fn fixed_grid_context_roundtrip_rans() {
        let mb_cols = 2u32;
        let mb_rows = 2u32;
        let mvs = vec![(4, 0), (8, 0), (12, -4), (16, 0)];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let blob = encode_mv_context_v1_fixed(&compact, mb_cols, mb_rows).unwrap();
        let dec =
            decode_mv_context_v1_fixed(&blob, compact.len(), mb_cols, mb_rows, 512_000).unwrap();
        assert_eq!(dec, compact);
    }

    #[test]
    fn partitioned_mv_stream_roundtrips() {
        let mb_cols = 2u32;
        let mb_rows = 1u32;
        let parts = vec![P_PART_WIRE_8X8; 2];
        let mvs = vec![
            (4, 0),
            (8, 0),
            (12, 0),
            (16, 0),
            (4, 0),
            (8, 0),
            (12, 0),
            (16, 0),
        ];
        let compact = encode_mv_stream_partitioned(mb_cols, mb_rows, &parts, &mvs).unwrap();
        let blob = encode_mv_context_v1_partitioned(&compact, mb_cols, mb_rows, &parts).unwrap();
        let dec = decode_mv_context_v1_partitioned(
            &blob,
            compact.len(),
            mb_cols,
            mb_rows,
            &parts,
            512_000,
        )
        .unwrap();
        assert_eq!(dec, compact);
    }

    #[test]
    fn batch_multi_matches_streaming_fixed() {
        let mb_cols = 3u32;
        let mb_rows = 2u32;
        let mvs = vec![(4i32, 0); 6];
        let compact = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let labels = fixed_grid_context_labels(&compact, mb_cols, mb_rows).unwrap();
        let models_arr = build_context_models_v1().unwrap();
        let models: Vec<RansModel> = models_arr.into_iter().collect();
        let blob = rans_encode_symbols_multi_context(
            &models,
            &compact.iter().map(|&b| usize::from(b)).collect::<Vec<_>>(),
            &labels,
        )
        .unwrap();
        let sy = rans_decode_symbols_multi_context(&models, &blob, compact.len(), &labels, 512_000)
            .unwrap();
        let dec: Vec<u8> = sy.iter().map(|&s| s as u8).collect();
        assert_eq!(dec, compact);
        let blob2 = encode_mv_context_v1_fixed(&compact, mb_cols, mb_rows).unwrap();
        assert_eq!(blob, blob2);
    }
}
