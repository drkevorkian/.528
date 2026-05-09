//! Context-adaptive **coefficient** residual entropy (experimental).
//!
//! This path applies **per-symbol** static rANS frequency tables selected from a
//! bounded context tuple (plane, block activity, band, zero-run bucket, magnitude
//! bucket, neighbor occupancy). It is **not** MV entropy ([`super::context_inter_entropy`]),
//! **not** CABAC-class engine parity, and **not** a claim of HEVC/AOM competitiveness.
//!
//! Malformed or truncated blobs must be rejected via [`ResidualContextEntropyError`].
//!
//! # Standalone v1 payload layout (not `FR2`)
//!
//! ```text
//! [ u16 LE symbol_count ]
//! [ u8 context_slot[sym_i] for i in 0..symbol_count ]   // one byte per symbol, must be < NUM_CONTEXT_SLOTS
//! [ multi-context rANS payload from `rans_encode_symbols_multi_context` ]
//! ```
//!
//! Per-symbol context bytes are stored explicitly so the encoder may use **future-symbol**
//! information (for example magnitude classes) while the decoder remains well-defined.
//! Integration into frame payloads / `FR2` revisions is intentionally **out of scope**.

use libsrs_bitio::{
    rans_decode_symbols_multi_context, rans_encode_symbols_multi_context, BitIoError, RansModel,
    RANS_SCALE,
};
use thiserror::Error;

use super::dct::ZIGZAG;
use super::residual_tokens::{
    detokenize_ac, residual_symbol_count, sym_eob, tokenize_ac, zigzag_signed,
    AC_POSITIONS, MAX_SYMBOLS_PER_BLOCK,
};

// --- public stable API -------------------------------------------------------

/// Y / U / V plane for context selection (one 8×8 quantized block of that plane).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResidualPlane {
    Y,
    U,
    V,
}

/// Block-level texture heuristic for context mixing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockActivityContext {
    Flat,
    Edge,
    Textured,
}

/// Zigzag AC band bucket for the **current** coefficient or run position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoeffBandContext {
    Dc,
    LowAc,
    MidAc,
    HighAc,
}

/// Zero-run length class for run-length symbols (`sym_zrun`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZeroRunContext {
    Short,
    Medium,
    Long,
}

/// Absolute-coefficient magnitude class (after zigzag value decode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MagnitudeContext {
    Zero,
    One,
    TwoThree,
    FourSeven,
    Large,
}

/// Neighbor MB had significant residual energy (caller-provided hints).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeighborResidualContext {
    Neither,
    Left,
    Above,
    Both,
}

/// Fully expanded context key for one rANS symbol (debug / inspection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResidualContextId {
    pub plane: ResidualPlane,
    pub activity: BlockActivityContext,
    pub band: CoeffBandContext,
    pub zero_run: ZeroRunContext,
    pub magnitude: MagnitudeContext,
    pub neighbor: NeighborResidualContext,
}

impl ResidualContextId {
    /// Maps the semantic tuple into a **static** table slot in `0..NUM_CONTEXT_SLOTS`.
    #[must_use]
    pub fn to_slot(self) -> u8 {
        slot_from_parts(
            self.plane,
            self.activity,
            self.band,
            self.zero_run,
            self.magnitude,
            self.neighbor,
        )
    }

    /// Rejects reserved / impossible packed IDs (future-proof hook). Today only
    /// validates slot range when round-tripped from [`Self::to_slot`] is lossy — use
    /// [`Self::try_from_slot`] for inverse mapping used in tests.
    pub fn try_from_slot(slot: u8) -> Option<Self> {
        if slot >= NUM_CONTEXT_SLOTS as u8 {
            return None;
        }
        // Inverse is intentionally undefined for general slots (hash collision);
        // reserved-ID tests use explicit constructors instead.
        None
    }
}

/// Static multi-slot rANS library (one [`RansModel`] per context slot).
#[derive(Debug, Clone)]
pub struct ResidualContextModel {
    slots: Vec<RansModel>,
}

/// Encode/decode instrumentation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResidualContextStats {
    pub symbols_encoded: usize,
    pub output_bytes: usize,
    pub max_context_slot_used: u8,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ResidualContextEntropyError {
    #[error("coefficient tokenization failed: {0}")]
    Tokenize(String),
    #[error("rANS entropy error: {0}")]
    Entropy(String),
    #[error("symbol budget exceeded (max {max}, got {got})")]
    SymbolBudget { max: usize, got: usize },
    #[error("invalid residual symbol stream")]
    InvalidSymbols,
    #[error("invalid context slot {0} (must be < {1})")]
    InvalidContextSlot(u8, u32),
    #[error("detokenize failed: {0}")]
    Detokenize(String),
}

/// Number of static rANS tables (must fit in `u8` for multi-context rANS).
pub const NUM_CONTEXT_SLOTS: usize = 128;

/// Hostile-input cap for one plane encode path (single 8×8 block in this revision).
pub const MAX_SYMBOLS_PER_PLANE: usize = MAX_SYMBOLS_PER_BLOCK;

// --- encode / decode ---------------------------------------------------------

/// Encode one **8×8** quantized block (full `[i16;64]`; AC token path matches [`tokenize_ac`]).
///
/// `left_neighbor_nonzero` / `above_neighbor_nonzero` are **macroblock-level** hints:
/// `true` if the neighboring 8×8 block in that direction had any nonzero quantized AC
/// (typical P-frame neighbor signaling).
pub fn encode_residual_context_v1_block(
    model: &ResidualContextModel,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
    quantized: &[i16; 64],
) -> Result<(Vec<u8>, ResidualContextStats), ResidualContextEntropyError> {
    encode_plane_inner(
        model,
        plane,
        left_neighbor_nonzero,
        above_neighbor_nonzero,
        quantized,
    )
}

/// Same as [`encode_residual_context_v1_block`] — naming emphasizes chroma plane usage.
pub fn encode_residual_context_v1_plane(
    model: &ResidualContextModel,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
    quantized: &[i16; 64],
) -> Result<(Vec<u8>, ResidualContextStats), ResidualContextEntropyError> {
    encode_plane_inner(
        model,
        plane,
        left_neighbor_nonzero,
        above_neighbor_nonzero,
        quantized,
    )
}

/// Decode into `out`, preserving `out[0]` as DC (only AC coefficients are entropy-coded).
pub fn decode_residual_context_v1_block(
    model: &ResidualContextModel,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
    blob: &[u8],
    out: &mut [i16; 64],
) -> Result<ResidualContextStats, ResidualContextEntropyError> {
    decode_plane_inner(
        model,
        plane,
        left_neighbor_nonzero,
        above_neighbor_nonzero,
        blob,
        out,
    )
}

/// Same as [`decode_residual_context_v1_block`].
pub fn decode_residual_context_v1_plane(
    model: &ResidualContextModel,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
    blob: &[u8],
    out: &mut [i16; 64],
) -> Result<ResidualContextStats, ResidualContextEntropyError> {
    decode_plane_inner(
        model,
        plane,
        left_neighbor_nonzero,
        above_neighbor_nonzero,
        blob,
        out,
    )
}

/// Short human-readable summary for logs / bench JSON.
#[must_use]
pub fn residual_context_model_summary(model: &ResidualContextModel) -> String {
    format!(
        "ResidualContextModel: slots={} alphabet={}",
        model.slots.len(),
        residual_symbol_count()
    )
}

impl ResidualContextModel {
    /// Build **static** per-slot frequency tables (no training): perturbations of the
    /// baseline intra residual alphabet from [`super::residual_tokens::residual_token_model`].
    #[must_use]
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(NUM_CONTEXT_SLOTS);
        for s in 0..NUM_CONTEXT_SLOTS {
            slots.push(build_slot_model(s).expect("static model"));
        }
        Self { slots }
    }
}

impl Default for ResidualContextModel {
    fn default() -> Self {
        Self::new()
    }
}

// --- internals -------------------------------------------------------------

fn map_tokenize(e: super::error::SrsV2Error) -> ResidualContextEntropyError {
    ResidualContextEntropyError::Tokenize(format!("{e:?}"))
}

fn map_bitio(e: BitIoError) -> ResidualContextEntropyError {
    ResidualContextEntropyError::Entropy(format!("{e:?}"))
}

fn neighbor_ctx(left: bool, above: bool) -> NeighborResidualContext {
    match (left, above) {
        (false, false) => NeighborResidualContext::Neither,
        (true, false) => NeighborResidualContext::Left,
        (false, true) => NeighborResidualContext::Above,
        (true, true) => NeighborResidualContext::Both,
    }
}

fn classify_activity(freq: &[i16; 64]) -> BlockActivityContext {
    let mut nz = 0usize;
    let mut sum_abs = 0u32;
    for (_zi, &k) in ZIGZAG.iter().enumerate().skip(1) {
        let v = freq[k];
        if v != 0 {
            nz += 1;
            sum_abs += v.unsigned_abs() as u32;
        }
    }
    if nz <= 2 && sum_abs < 24 {
        BlockActivityContext::Flat
    } else if nz <= 18 {
        BlockActivityContext::Edge
    } else {
        BlockActivityContext::Textured
    }
}

fn band_for_ac_pos(ac_pos: usize) -> CoeffBandContext {
    match ac_pos {
        0 => CoeffBandContext::Dc,
        1..=8 => CoeffBandContext::LowAc,
        9..=32 => CoeffBandContext::MidAc,
        _ => CoeffBandContext::HighAc,
    }
}

fn zrun_bucket(run: usize) -> ZeroRunContext {
    match run {
        1..=5 => ZeroRunContext::Short,
        6..=20 => ZeroRunContext::Medium,
        _ => ZeroRunContext::Long,
    }
}

fn magnitude_bucket(v: i16) -> MagnitudeContext {
    let a = v.unsigned_abs();
    match a {
        0 => MagnitudeContext::Zero,
        1 => MagnitudeContext::One,
        2 | 3 => MagnitudeContext::TwoThree,
        4..=7 => MagnitudeContext::FourSeven,
        _ => MagnitudeContext::Large,
    }
}

fn slot_from_parts(
    plane: ResidualPlane,
    activity: BlockActivityContext,
    band: CoeffBandContext,
    zero_run: ZeroRunContext,
    magnitude: MagnitudeContext,
    neighbor: NeighborResidualContext,
) -> u8 {
    let p = plane as u8;
    let a = activity as u8;
    let b = band as u8;
    let z = zero_run as u8;
    let m = magnitude as u8;
    let n = neighbor as u8;
    let mix = (p as u32)
        .wrapping_mul(31)
        .wrapping_add(a as u32)
        .wrapping_mul(31)
        .wrapping_add(b as u32)
        .wrapping_mul(17)
        .wrapping_add(z as u32)
        .wrapping_mul(19)
        .wrapping_add(m as u32)
        .wrapping_mul(23)
        .wrapping_add(n as u32);
    (mix % NUM_CONTEXT_SLOTS as u32) as u8
}

fn build_contexts_for_symbols(
    symbols: &[usize],
    plane: ResidualPlane,
    activity: BlockActivityContext,
    neighbor: NeighborResidualContext,
) -> Result<Vec<u8>, ResidualContextEntropyError> {
    let mut ctxs = Vec::with_capacity(symbols.len().min(MAX_SYMBOLS_PER_BLOCK));
    let mut ac_pos = 0usize;
    let mut i = 0usize;
    while i < symbols.len() {
        let s = symbols[i];
        if s == sym_eob() {
            let id = ResidualContextId {
                plane,
                activity,
                band: CoeffBandContext::HighAc,
                zero_run: ZeroRunContext::Short,
                magnitude: MagnitudeContext::Zero,
                neighbor,
            };
            let slot = id.to_slot();
            validate_slot(slot)?;
            ctxs.push(slot);
            i += 1;
            continue;
        }
        if (1..63).contains(&s) {
            let run = s;
            let zb = zrun_bucket(run);
            let band = band_for_ac_pos(ac_pos);
            let id = ResidualContextId {
                plane,
                activity,
                band,
                zero_run: zb,
                magnitude: MagnitudeContext::Zero,
                neighbor,
            };
            let slot = id.to_slot();
            validate_slot(slot)?;
            ctxs.push(slot);
            ac_pos = ac_pos.saturating_add(run);
            if ac_pos > AC_POSITIONS {
                return Err(ResidualContextEntropyError::InvalidSymbols);
            }
            i += 1;
            continue;
        }
        if (63..317).contains(&s) {
            let z = s - 62;
            let v = zigzag_signed(z).ok_or(ResidualContextEntropyError::InvalidSymbols)?;
            let band = band_for_ac_pos(ac_pos);
            let mb = magnitude_bucket(v);
            let id = ResidualContextId {
                plane,
                activity,
                band,
                zero_run: ZeroRunContext::Short,
                magnitude: mb,
                neighbor,
            };
            let slot = id.to_slot();
            validate_slot(slot)?;
            ctxs.push(slot);
            ac_pos = ac_pos.saturating_add(1);
            if ac_pos > AC_POSITIONS {
                return Err(ResidualContextEntropyError::InvalidSymbols);
            }
            i += 1;
            continue;
        }
        return Err(ResidualContextEntropyError::InvalidSymbols);
    }
    if ctxs.len() != symbols.len() {
        return Err(ResidualContextEntropyError::InvalidSymbols);
    }
    Ok(ctxs)
}

fn validate_slot(slot: u8) -> Result<(), ResidualContextEntropyError> {
    if slot as usize >= NUM_CONTEXT_SLOTS {
        return Err(ResidualContextEntropyError::InvalidContextSlot(
            slot,
            NUM_CONTEXT_SLOTS as u32,
        ));
    }
    Ok(())
}

fn encode_plane_inner(
    model: &ResidualContextModel,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
    quantized: &[i16; 64],
) -> Result<(Vec<u8>, ResidualContextStats), ResidualContextEntropyError> {
    let syms = tokenize_ac(quantized).map_err(map_tokenize)?;
    if syms.len() > MAX_SYMBOLS_PER_BLOCK {
        return Err(ResidualContextEntropyError::SymbolBudget {
            max: MAX_SYMBOLS_PER_BLOCK,
            got: syms.len(),
        });
    }
    let activity = classify_activity(quantized);
    let neighbor = neighbor_ctx(left_neighbor_nonzero, above_neighbor_nonzero);
    let contexts = build_contexts_for_symbols(&syms, plane, activity, neighbor)?;
    debug_assert_eq!(contexts.len(), syms.len());

    let mut max_slot = 0u8;
    for &c in &contexts {
        max_slot = max_slot.max(c);
    }

    let rans_blob =
        rans_encode_symbols_multi_context(&model.slots, &syms, &contexts).map_err(map_bitio)?;

    let mut packed = Vec::with_capacity(2 + contexts.len() + rans_blob.len());
    packed.extend_from_slice(&(syms.len() as u16).to_le_bytes());
    packed.extend_from_slice(&contexts);
    packed.extend_from_slice(&rans_blob);
    let output_bytes = packed.len();

    Ok((
        packed,
        ResidualContextStats {
            symbols_encoded: syms.len(),
            output_bytes,
            max_context_slot_used: max_slot,
        },
    ))
}

fn decode_plane_inner(
    model: &ResidualContextModel,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
    blob: &[u8],
    out: &mut [i16; 64],
) -> Result<ResidualContextStats, ResidualContextEntropyError> {
    let dc_save = out[0];
    if blob.len() < 2 {
        return Err(ResidualContextEntropyError::Entropy(
            "truncated v1 header".into(),
        ));
    }
    let num_syms = u16::from_le_bytes([blob[0], blob[1]]) as usize;
    if num_syms == 0 {
        return Err(ResidualContextEntropyError::InvalidSymbols);
    }
    if num_syms > MAX_SYMBOLS_PER_BLOCK || num_syms > MAX_SYMBOLS_PER_PLANE {
        return Err(ResidualContextEntropyError::SymbolBudget {
            max: MAX_SYMBOLS_PER_BLOCK,
            got: num_syms,
        });
    }
    let header_ctx_len = 2usize.saturating_add(num_syms);
    if blob.len() < header_ctx_len {
        return Err(ResidualContextEntropyError::Entropy(
            "truncated context bytes".into(),
        ));
    }
    let contexts = &blob[2..header_ctx_len];
    let payload = &blob[header_ctx_len..];

    for &c in contexts {
        validate_slot(c)?;
    }

    let decode_budget = payload.len().saturating_mul(32).max(4096);
    let syms = rans_decode_symbols_multi_context(
        &model.slots,
        payload,
        num_syms,
        contexts,
        decode_budget,
    )
    .map_err(map_bitio)?;

    detokenize_ac(&syms, out).map_err(|e| ResidualContextEntropyError::Detokenize(format!("{e:?}")))?;
    out[0] = dc_save;

    let expected_tok = tokenize_ac(out).map_err(map_tokenize)?;
    if expected_tok != syms {
        return Err(ResidualContextEntropyError::InvalidSymbols);
    }

    let recomputed = build_contexts_for_symbols(
        &syms,
        plane,
        classify_activity(out),
        neighbor_ctx(left_neighbor_nonzero, above_neighbor_nonzero),
    )?;
    if recomputed.as_slice() != contexts {
        return Err(ResidualContextEntropyError::Entropy(
            "plane/neighbor/context metadata mismatch".into(),
        ));
    }

    let max_slot = contexts.iter().copied().max().unwrap_or(0);

    Ok(ResidualContextStats {
        symbols_encoded: syms.len(),
        output_bytes: blob.len(),
        max_context_slot_used: max_slot,
    })
}

fn baseline_residual_freqs() -> Vec<u32> {
    let mut freqs = vec![1u32; residual_symbol_count()];
    freqs[0] = 200;
    for slot in freqs.iter_mut().take(63).skip(1) {
        *slot = 35;
    }
    let sum_head: u32 = freqs[..63].iter().sum();
    let rem = RANS_SCALE - sum_head;
    let n_val = 254usize;
    let base = rem / n_val as u32;
    let extra = rem % n_val as u32;
    for i in 0..n_val {
        freqs[63 + i] = base + if (i as u32) < extra { 1 } else { 0 };
    }
    freqs
}

fn build_slot_model(slot: usize) -> Result<RansModel, BitIoError> {
    let mut freqs = baseline_residual_freqs();
    let shift = ((slot % 17) + 1) as u32;
    freqs[0] = freqs[0].saturating_sub(shift);
    let idx = 63 + (slot % 254);
    freqs[idx] = freqs[idx].saturating_add(shift);
    RansModel::try_from_freqs(freqs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::dct::ZIGZAG;

    fn rt_block(
        model: &ResidualContextModel,
        plane: ResidualPlane,
        left: bool,
        above: bool,
        q: &[i16; 64],
    ) -> [i16; 64] {
        let (blob, _) = encode_residual_context_v1_block(model, plane, left, above, q).unwrap();
        let mut out = *q;
        decode_residual_context_v1_block(model, plane, left, above, &blob, &mut out).unwrap();
        out
    }

    #[test]
    fn all_zero_block_roundtrip() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[0] = 11;
        let out = rt_block(&model, ResidualPlane::Y, false, false, &q);
        assert_eq!(out, q);
    }

    #[test]
    fn dc_only_block_roundtrip() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[0] = -50;
        let out = rt_block(&model, ResidualPlane::U, false, true, &q);
        assert_eq!(out, q);
    }

    #[test]
    fn sparse_ac_block_roundtrip() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[0] = 3;
        q[ZIGZAG[3]] = -1;
        q[ZIGZAG[10]] = 4;
        let out = rt_block(&model, ResidualPlane::V, true, false, &q);
        assert_eq!(out, q);
    }

    #[test]
    fn dense_noisy_block_roundtrip() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[0] = 5;
        let mut zi = 1;
        while zi < 64 {
            q[ZIGZAG[zi]] = (((zi * 13) % 17) as i16).saturating_sub(8).clamp(-127, 127);
            zi += 1;
        }
        let out = rt_block(&model, ResidualPlane::Y, true, true, &q);
        assert_eq!(out, q);
    }

    #[test]
    fn flat_plane_context_roundtrip() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[0] = 1;
        q[ZIGZAG[1]] = 2;
        assert_eq!(classify_activity(&q), BlockActivityContext::Flat);
        let out = rt_block(&model, ResidualPlane::Y, false, false, &q);
        assert_eq!(out, q);
    }

    #[test]
    fn edge_plane_context_roundtrip() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[0] = 0;
        for zi in 1..12 {
            q[ZIGZAG[zi]] = ((zi % 5) as i16).saturating_sub(2);
        }
        assert_eq!(classify_activity(&q), BlockActivityContext::Edge);
        let out = rt_block(&model, ResidualPlane::U, false, true, &q);
        assert_eq!(out, q);
    }

    #[test]
    fn invalid_context_slot_in_blob_fails() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[ZIGZAG[1]] = 3;
        let (mut blob, _) =
            encode_residual_context_v1_block(&model, ResidualPlane::Y, false, false, &q).unwrap();
        assert!(blob.len() > 2);
        blob[2] = NUM_CONTEXT_SLOTS as u8;
        let mut out = q;
        let err = decode_residual_context_v1_block(&model, ResidualPlane::Y, false, false, &blob, &mut out)
            .unwrap_err();
        match err {
            ResidualContextEntropyError::InvalidContextSlot(s, _) => assert_eq!(s, NUM_CONTEXT_SLOTS as u8),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn malformed_rans_truncated_fails() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[ZIGZAG[1]] = -7;
        let (mut blob, _) =
            encode_residual_context_v1_block(&model, ResidualPlane::Y, false, false, &q).unwrap();
        assert!(blob.len() > 8);
        blob.truncate(blob.len().saturating_sub(3));
        let mut out = q;
        assert!(
            decode_residual_context_v1_block(&model, ResidualPlane::Y, false, false, &blob, &mut out).is_err()
        );
    }

    #[test]
    fn symbol_budget_header_enforced() {
        let model = ResidualContextModel::new();
        let mut blob = vec![0xff_u8, 0x01];
        blob.resize(2 + 0x1ff + 8, 0);
        let mut out = [0_i16; 64];
        let err = decode_residual_context_v1_block(
            &model,
            ResidualPlane::Y,
            false,
            false,
            &blob,
            &mut out,
        )
        .unwrap_err();
        match err {
            ResidualContextEntropyError::SymbolBudget { .. } => {}
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn trailing_garbage_rejected() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[0] = 2;
        q[ZIGZAG[2]] = 1;
        let (mut blob, _) =
            encode_residual_context_v1_block(&model, ResidualPlane::Y, false, false, &q).unwrap();
        blob.push(0xAB);
        let mut out = q;
        assert!(
            decode_residual_context_v1_block(&model, ResidualPlane::Y, false, false, &blob, &mut out).is_err()
        );
    }

    #[test]
    fn deterministic_encode_same_input() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[ZIGZAG[5]] = -20;
        q[ZIGZAG[30]] = 11;
        let (a, _) = encode_residual_context_v1_block(&model, ResidualPlane::V, true, false, &q).unwrap();
        let (b, _) = encode_residual_context_v1_block(&model, ResidualPlane::V, true, false, &q).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn side_info_mismatch_rejected() {
        let model = ResidualContextModel::new();
        let mut q = [0_i16; 64];
        q[ZIGZAG[1]] = 4;
        let (blob, _) =
            encode_residual_context_v1_block(&model, ResidualPlane::Y, false, false, &q).unwrap();
        let mut out = q;
        let err = decode_residual_context_v1_block(&model, ResidualPlane::Y, true, false, &blob, &mut out)
            .unwrap_err();
        match err {
            ResidualContextEntropyError::Entropy(msg) => {
                assert!(msg.contains("mismatch"));
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn model_summary_smoke() {
        let model = ResidualContextModel::new();
        let s = residual_context_model_summary(&model);
        assert!(s.contains("128"));
        assert!(s.contains("317"));
    }
}
