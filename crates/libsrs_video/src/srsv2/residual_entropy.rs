//! FR2 rev 3/4/7 residual packing — explicit AC tuples vs static rANS tokens per block.
//! Rev **7** inserts a signed **`qp_delta`** byte after DC (before the residual coding tag).

use libsrs_bitio::RansModel;

use super::block_aq::{
    apply_qp_delta_clamped, choose_block_qp_delta, collect_plane_block_variances,
    validate_qp_clip_range, validate_wire_qp_delta,
};
use super::dct::{fdct_8x8, idct_4x4, idct_8x8, ZIGZAG, ZIGZAG_4X4};
use super::error::SrsV2Error;
use super::frame::VideoPlane;
use super::intra_codec::{
    dequantize, dequantize_4x4, idct_residual_tx4x4_from_qfreq, pick_mode, predict_block, quantize,
    quantize_residual_tx4x4_natural, PredMode,
};
use super::limits::MAX_FRAME_PAYLOAD_BYTES;
use super::rate_control::{
    rdo_lambda_effective, ResidualEncodeStats, ResidualEntropy, SrsV2CoeffScanMode,
    SrsV2EncodeSettings, SrsV2ResidualContextMode, SrsV2TransformDecisionMode,
    SrsV2TransformGroupingMode,
};
use super::rdo::choose_grouping_rdo_fast;
pub use super::residual_context_entropy::ResidualPlane;
use super::residual_context_entropy::{
    decode_residual_context_v1_plane, encode_residual_context_v1_plane, ResidualContextModel,
};
use super::residual_token_v2;
use super::residual_tokens::{
    detokenize_ac, rans_decode_tokens, rans_encode_tokens, residual_token_model, tokenize_ac,
    MAX_SYMBOLS_PER_BLOCK,
};
use super::transform_layout::{
    choose_coeff_scan, choose_transform_kind, decode_coeff_compact_rev32_intra_block,
    encode_coeff_compact_rev32_intra_block, estimate_coeff_layout_bytes,
    fr2_rev34_grouping_wire_from_transform_kind, fr2_rev34_transform_kind_from_grouping_wire,
    fr2_rev35_p_grouping_wire_from_transform_kind, fr2_rev35_p_transform_kind_from_grouping_wire,
    SrsV2CoeffLayoutMode as TlCoeffLayoutMode, SrsV2CoeffScan, SrsV2TransformKind,
    TransformDecisionConfig, TransformDecisionInput, TransformLayoutError,
};

pub const TAG_EXPLICIT_AC: u8 = 0;
pub const TAG_RANS_AC: u8 = 1;
/// Multi-context static rANS residual (`SrsV2ResidualContextMode::ContextV1`).
pub const TAG_CONTEXT_RANS_AC: u8 = 2;
/// **`FR2` rev33** fixed-grid **P** 8×8 residual: compact bitmap + scan-ordered `i16` values ([`encode_p_residual_chunk_compact_v33_wire`]).
pub const TAG_P_RESIDUAL_COMPACT_V1: u8 = 2;
/// **`FR2` rev35** fixed-grid **P** 8×8 residual: explicit transform grouping + scan + compact body ([`encode_p_residual_chunk_compact_v35_wire`]).
pub const TAG_P_RESIDUAL_TRANSFORM_GROUP_V35: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockResidualCoding {
    ExplicitTuples,
    RansV1,
    ContextRansV1,
}

/// Optional **`P`/`B`** 8×8 residual chunk controls for [`encode_p_residual_chunk_with_opts`].
#[derive(Debug, Clone, Copy)]
pub struct PResidualChunkEncodeOpts<'a> {
    pub residual_context_mode: SrsV2ResidualContextMode,
    pub context_model: Option<&'a ResidualContextModel>,
    pub plane: ResidualPlane,
    pub left_neighbor_nonzero: bool,
    pub above_neighbor_nonzero: bool,
    /// When **`true`** with [`SrsV2ResidualContextMode::ContextV1`], every adaptive block **must** use
    /// [`TAG_CONTEXT_RANS_AC`] (`FR2` **rev30** strict residual wire).
    pub strict_fr2_rev30_residual: bool,
}

impl Default for PResidualChunkEncodeOpts<'_> {
    fn default() -> Self {
        Self {
            residual_context_mode: SrsV2ResidualContextMode::Off,
            context_model: None,
            plane: ResidualPlane::Y,
            left_neighbor_nonzero: false,
            above_neighbor_nonzero: false,
            strict_fr2_rev30_residual: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PResidualChunkKind {
    LegacyTuple,
    Adaptive(BlockResidualCoding),
    /// Scan-ordered compact coefficient body (`TAG_P_RESIDUAL_COMPACT_V1`); **`FR2` rev33** fixed **P** grid ([`crate::srsv2::transform_layout::encode_coeff_compact_rev32_intra_block`]).
    CompactCoeffV1,
}

pub(crate) fn write_explicit_ac_only(
    freq: &[i16; 64],
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    let mut pairs = 0_usize;
    let mut tmp = Vec::new();
    for &k in ZIGZAG.iter().skip(1) {
        let v = freq[k];
        if v != 0 {
            if pairs >= 63 {
                return Err(SrsV2Error::syntax("too many ac coeffs"));
            }
            tmp.push(k as u8);
            tmp.extend_from_slice(&v.to_le_bytes());
            pairs += 1;
        }
    }
    let pairs_u16 = u16::try_from(pairs).map_err(|_| SrsV2Error::syntax("ac pairs"))?;
    out.extend_from_slice(&pairs_u16.to_le_bytes());
    out.extend_from_slice(&tmp);
    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "plane bitstream",
        });
    }
    Ok(())
}

pub(crate) fn explicit_ac_only_len(freq: &[i16; 64]) -> usize {
    let mut pairs = 0usize;
    for &k in ZIGZAG.iter().skip(1) {
        if freq[k] != 0 {
            pairs += 1;
        }
    }
    2 + pairs * 3
}

pub(crate) fn read_explicit_ac_only(data: &[u8], cur: &mut usize) -> Result<[i16; 64], SrsV2Error> {
    let mut ac = [0_i16; 64];
    let pairs = read_u16(data, cur)? as usize;
    if pairs > 63 {
        return Err(SrsV2Error::syntax("ac pairs overflow"));
    }
    for _ in 0..pairs {
        let pos = read_u8(data, cur)? as usize;
        if pos == 0 || pos > 63 {
            return Err(SrsV2Error::syntax("bad coeff index"));
        }
        let v = read_i16(data, cur)?;
        ac[pos] = v;
    }
    Ok(ac)
}

fn read_u8(data: &[u8], cur: &mut usize) -> Result<u8, SrsV2Error> {
    if *cur >= data.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v = data[*cur];
    *cur += 1;
    Ok(v)
}

fn read_u16(data: &[u8], cur: &mut usize) -> Result<u16, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 2 {
        return Err(SrsV2Error::Truncated);
    }
    let v = u16::from_le_bytes([data[*cur], data[*cur + 1]]);
    *cur += 2;
    Ok(v)
}

fn read_i16(data: &[u8], cur: &mut usize) -> Result<i16, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 2 {
        return Err(SrsV2Error::Truncated);
    }
    let v = i16::from_le_bytes([data[*cur], data[*cur + 1]]);
    *cur += 2;
    Ok(v)
}

fn try_rans_payload(
    freq: &[i16; 64],
    model: &RansModel,
) -> Result<Option<(usize, Vec<u8>)>, SrsV2Error> {
    let tok = match tokenize_ac(freq) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let enc = rans_encode_tokens(model, &tok)?;
    Ok(Some((tok.len(), enc)))
}

/// Full serialized block size for explicit tuples (mode + dc + tag + AC blob).
fn explicit_wire_len(freq: &[i16; 64]) -> usize {
    1 + 2 + 1 + explicit_ac_only_len(freq)
}

fn explicit_wire_len_rev7(freq: &[i16; 64]) -> usize {
    1 + 2 + 1 + 1 + explicit_ac_only_len(freq)
}

fn rans_wire_len(enc_len: usize) -> usize {
    1 + 2 + 1 + 2 + 2 + enc_len
}

fn rans_wire_len_rev7(enc_len: usize) -> usize {
    1 + 2 + 1 + 1 + 2 + 2 + enc_len
}

fn context_blob_wire_len(blob_len: usize) -> usize {
    1 + 2 + blob_len
}

fn record_residual_context_v1_tail_stats(
    stats: &mut ResidualEncodeStats,
    blob_len: usize,
    freq: &[i16; 64],
    model: &RansModel,
    rev7_block: bool,
) {
    stats.residual_context_blocks = stats.residual_context_blocks.saturating_add(1);
    let ctx_wire = context_blob_wire_len(blob_len);
    stats.residual_context_bytes = stats.residual_context_bytes.saturating_add(ctx_wire as u64);
    let explicit_tail = 1 + explicit_ac_only_len(freq);
    let rans_tail = match try_rans_payload(freq, model) {
        Ok(Some((_, enc))) => {
            let full = if rev7_block {
                rans_wire_len_rev7(enc.len())
            } else {
                rans_wire_len(enc.len())
            };
            let header_before_tag = if rev7_block { 4 } else { 3 };
            Some(full.saturating_sub(header_before_tag))
        }
        Ok(None) | Err(_) => None,
    };
    let static_est = match rans_tail {
        Some(rt) => explicit_tail.min(rt),
        None => explicit_tail,
    };
    stats.residual_static_bytes_estimate = stats
        .residual_static_bytes_estimate
        .saturating_add(static_est as u64);
    if ctx_wire > static_est {
        stats.residual_context_failed_blocks =
            stats.residual_context_failed_blocks.saturating_add(1);
    }
    let save = static_est.saturating_sub(ctx_wire);
    stats.residual_context_savings_bytes = stats
        .residual_context_savings_bytes
        .saturating_add(save as u64);
}

fn map_residual_context_entropy(
    e: super::residual_context_entropy::ResidualContextEntropyError,
) -> SrsV2Error {
    SrsV2Error::PartitionMapSyntax(format!("residual_context_entropy: {e}"))
}

fn encode_context_blob(
    freq: &[i16; 64],
    model: &ResidualContextModel,
    plane: ResidualPlane,
    left_nz: bool,
    above_nz: bool,
) -> Result<Vec<u8>, SrsV2Error> {
    encode_residual_context_v1_plane(model, plane, left_nz, above_nz, freq)
        .map(|(v, _)| v)
        .map_err(map_residual_context_entropy)
}

fn pick_auto_three_way_fixed(
    freq: &[i16; 64],
    explicit_full: usize,
    rans_choice: &Option<(usize, Vec<u8>)>,
    context_blob: Option<&Vec<u8>>,
) -> BlockResidualCoding {
    fn better(
        candidate: (usize, u8, BlockResidualCoding),
        best: (usize, u8, BlockResidualCoding),
    ) -> bool {
        candidate.0 < best.0 || (candidate.0 == best.0 && candidate.1 < best.1)
    }

    let mut best = (explicit_full, 0_u8, BlockResidualCoding::ExplicitTuples);
    if let Some((_, enc)) = rans_choice {
        let rfull = rans_wire_len(enc.len());
        let cand = (rfull, 1_u8, BlockResidualCoding::RansV1);
        if better(cand, best) {
            best = cand;
        }
    }
    if let Some(blob) = context_blob {
        let cfull =
            explicit_full - (1 + explicit_ac_only_len(freq)) + context_blob_wire_len(blob.len());
        let cand = (cfull, 2_u8, BlockResidualCoding::ContextRansV1);
        if better(cand, best) {
            best = cand;
        }
    }
    best.2
}

fn pick_auto_three_way_rev7(
    freq: &[i16; 64],
    explicit_full: usize,
    rans_choice: &Option<(usize, Vec<u8>)>,
    context_blob: Option<&Vec<u8>>,
) -> BlockResidualCoding {
    fn better(
        candidate: (usize, u8, BlockResidualCoding),
        best: (usize, u8, BlockResidualCoding),
    ) -> bool {
        candidate.0 < best.0 || (candidate.0 == best.0 && candidate.1 < best.1)
    }

    let mut best = (explicit_full, 0_u8, BlockResidualCoding::ExplicitTuples);
    if let Some((_, enc)) = rans_choice {
        let rfull = rans_wire_len_rev7(enc.len());
        let cand = (rfull, 1_u8, BlockResidualCoding::RansV1);
        if better(cand, best) {
            best = cand;
        }
    }
    if let Some(blob) = context_blob {
        let cfull =
            explicit_full - (1 + explicit_ac_only_len(freq)) + context_blob_wire_len(blob.len());
        let cand = (cfull, 2_u8, BlockResidualCoding::ContextRansV1);
        if better(cand, best) {
            best = cand;
        }
    }
    best.2
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_intra_block_residual(
    mode: PredMode,
    freq: &[i16; 64],
    policy: ResidualEntropy,
    residual_ctx_mode: SrsV2ResidualContextMode,
    model: &RansModel,
    ctx_model: Option<&ResidualContextModel>,
    plane: ResidualPlane,
    left_nz: bool,
    above_nz: bool,
    // When true with ContextV1: only TAG_CONTEXT_RANS_AC (`FR2` rev29 / rev30 strict residual wire).
    strict_fr2_context_v1_residual_only: bool,
    mut context_residual_stats: Option<&mut ResidualEncodeStats>,
    out: &mut Vec<u8>,
) -> Result<BlockResidualCoding, SrsV2Error> {
    let use_ctx = matches!(residual_ctx_mode, SrsV2ResidualContextMode::ContextV1);
    let explicit_full = explicit_wire_len(freq);
    let rans_choice = try_rans_payload(freq, model)?;
    if let (Some(st), Some((_, enc))) = (context_residual_stats.as_mut(), rans_choice.as_ref()) {
        st.residual_token_v1_rans_ac_body_bytes = st
            .residual_token_v1_rans_ac_body_bytes
            .saturating_add(enc.len() as u64);
        let plane_b = match plane {
            ResidualPlane::Y => 0u8,
            ResidualPlane::U => 1u8,
            ResidualPlane::V => 2u8,
        };
        if let Ok(v2) = residual_token_v2::encode_ac_payload(freq, plane_b) {
            st.residual_token_v2_ac_body_bytes = st
                .residual_token_v2_ac_body_bytes
                .saturating_add(v2.len() as u64);
            st.residual_token_compare_block_count =
                st.residual_token_compare_block_count.saturating_add(1);
        }
    }
    let context_blob: Option<Vec<u8>> = if use_ctx {
        let m = ctx_model.ok_or_else(|| {
            SrsV2Error::syntax("internal: ContextV1 residual requires ResidualContextModel")
        })?;
        Some(encode_context_blob(freq, m, plane, left_nz, above_nz)?)
    } else {
        None
    };

    let coding = match policy {
        ResidualEntropy::Explicit => {
            if use_ctx {
                return Err(SrsV2Error::syntax(
                    "internal: Explicit residual with ContextV1 (validate settings)",
                ));
            }
            BlockResidualCoding::ExplicitTuples
        }
        ResidualEntropy::Rans => {
            if use_ctx {
                BlockResidualCoding::ContextRansV1
            } else if rans_choice.is_none() {
                return Err(SrsV2Error::syntax(
                    "forced rANS but coefficients out of range",
                ));
            } else {
                BlockResidualCoding::RansV1
            }
        }
        ResidualEntropy::Auto => {
            if use_ctx {
                if strict_fr2_context_v1_residual_only {
                    BlockResidualCoding::ContextRansV1
                } else {
                    pick_auto_three_way_fixed(
                        freq,
                        explicit_full,
                        &rans_choice,
                        context_blob.as_ref(),
                    )
                }
            } else {
                match &rans_choice {
                    Some((_, enc)) => {
                        let rfull = rans_wire_len(enc.len());
                        if rfull < explicit_full {
                            BlockResidualCoding::RansV1
                        } else {
                            BlockResidualCoding::ExplicitTuples
                        }
                    }
                    None => BlockResidualCoding::ExplicitTuples,
                }
            }
        }
    };

    out.push(mode as u8);
    out.extend_from_slice(&freq[0].to_le_bytes());

    match coding {
        BlockResidualCoding::ExplicitTuples => {
            out.push(TAG_EXPLICIT_AC);
            write_explicit_ac_only(freq, out)?;
            Ok(BlockResidualCoding::ExplicitTuples)
        }
        BlockResidualCoding::RansV1 => {
            let (sym_ct, enc) = rans_choice.expect("rans payload");
            out.push(TAG_RANS_AC);
            let sc = u16::try_from(sym_ct).map_err(|_| SrsV2Error::syntax("rans sym count"))?;
            let bl =
                u16::try_from(enc.len()).map_err(|_| SrsV2Error::syntax("rans blob length"))?;
            out.extend_from_slice(&sc.to_le_bytes());
            out.extend_from_slice(&bl.to_le_bytes());
            out.extend_from_slice(&enc);
            Ok(BlockResidualCoding::RansV1)
        }
        BlockResidualCoding::ContextRansV1 => {
            let blob = context_blob.expect("context blob");
            out.push(TAG_CONTEXT_RANS_AC);
            let bl =
                u16::try_from(blob.len()).map_err(|_| SrsV2Error::syntax("context blob length"))?;
            out.extend_from_slice(&bl.to_le_bytes());
            out.extend_from_slice(&blob);
            if let Some(st) = context_residual_stats {
                record_residual_context_v1_tail_stats(st, blob.len(), freq, model, false);
            }
            Ok(BlockResidualCoding::ContextRansV1)
        }
    }
}

pub(crate) fn decode_intra_block_residual(
    data: &[u8],
    cur: &mut usize,
    ctx_model: &ResidualContextModel,
    plane: ResidualPlane,
    left_nz: bool,
    above_nz: bool,
) -> Result<(PredMode, [i16; 64]), SrsV2Error> {
    let mode_b = read_u8(data, cur)?;
    let mode = PredMode::from_u8(mode_b)?;
    let mut freq = [0_i16; 64];
    freq[0] = read_i16(data, cur)?;
    let tag = read_u8(data, cur)?;
    match tag {
        TAG_EXPLICIT_AC => {
            let ac = read_explicit_ac_only(data, cur)?;
            for &k in ZIGZAG.iter().skip(1) {
                freq[k] = ac[k];
            }
            Ok((mode, freq))
        }
        TAG_RANS_AC => {
            let sym_ct = read_u16(data, cur)? as usize;
            let bl = read_u16(data, cur)? as usize;
            if sym_ct > MAX_SYMBOLS_PER_BLOCK {
                return Err(SrsV2Error::syntax("rans symbol count"));
            }
            let end = cur
                .checked_add(bl)
                .ok_or(SrsV2Error::Overflow("rans blob"))?;
            if end > data.len() {
                return Err(SrsV2Error::Truncated);
            }
            let blob = &data[*cur..end];
            *cur = end;
            let model = residual_token_model();
            let syms = rans_decode_tokens(&model, blob, sym_ct)?;
            detokenize_ac(&syms, &mut freq)?;
            Ok((mode, freq))
        }
        TAG_CONTEXT_RANS_AC => {
            let bl = read_u16(data, cur)? as usize;
            let end = cur
                .checked_add(bl)
                .ok_or(SrsV2Error::Overflow("context rANS blob"))?;
            if end > data.len() {
                return Err(SrsV2Error::Truncated);
            }
            let blob = &data[*cur..end];
            *cur = end;
            decode_residual_context_v1_plane(ctx_model, plane, left_nz, above_nz, blob, &mut freq)
                .map_err(map_residual_context_entropy)?;
            Ok((mode, freq))
        }
        _ => Err(SrsV2Error::syntax("bad residual coding tag")),
    }
}

/// [`FR2` rev **29**] intra plane block: residual tag **must** be [`TAG_CONTEXT_RANS_AC`].
pub(crate) fn decode_intra_block_residual_strict_context_v1(
    data: &[u8],
    cur: &mut usize,
    ctx_model: &ResidualContextModel,
    plane: ResidualPlane,
    left_nz: bool,
    above_nz: bool,
) -> Result<(PredMode, [i16; 64]), SrsV2Error> {
    let mode_b = read_u8(data, cur)?;
    let mode = PredMode::from_u8(mode_b)?;
    let mut freq = [0_i16; 64];
    freq[0] = read_i16(data, cur)?;
    let tag = read_u8(data, cur)?;
    if tag != TAG_CONTEXT_RANS_AC {
        return Err(SrsV2Error::syntax(
            "FR2 rev29 intra residual requires context V1 (TAG_CONTEXT_RANS_AC)",
        ));
    }
    let bl = read_u16(data, cur)? as usize;
    let end = cur
        .checked_add(bl)
        .ok_or(SrsV2Error::Overflow("context rANS blob"))?;
    if end > data.len() {
        return Err(SrsV2Error::Truncated);
    }
    let blob = &data[*cur..end];
    *cur = end;
    decode_residual_context_v1_plane(ctx_model, plane, left_nz, above_nz, blob, &mut freq)
        .map_err(map_residual_context_entropy)?;
    Ok((mode, freq))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_intra_block_residual_rev7(
    mode: PredMode,
    freq: &[i16; 64],
    qp_delta: i8,
    policy: ResidualEntropy,
    residual_ctx_mode: SrsV2ResidualContextMode,
    model: &RansModel,
    ctx_model: Option<&ResidualContextModel>,
    plane: ResidualPlane,
    left_nz: bool,
    above_nz: bool,
    strict_fr2_context_v1_residual_only: bool,
    mut context_residual_stats: Option<&mut ResidualEncodeStats>,
    out: &mut Vec<u8>,
) -> Result<BlockResidualCoding, SrsV2Error> {
    validate_wire_qp_delta(qp_delta)?;
    let use_ctx = matches!(residual_ctx_mode, SrsV2ResidualContextMode::ContextV1);
    let explicit_full = explicit_wire_len_rev7(freq);
    let rans_choice = try_rans_payload(freq, model)?;
    if let (Some(st), Some((_, enc))) = (context_residual_stats.as_mut(), rans_choice.as_ref()) {
        st.residual_token_v1_rans_ac_body_bytes = st
            .residual_token_v1_rans_ac_body_bytes
            .saturating_add(enc.len() as u64);
        let plane_b = match plane {
            ResidualPlane::Y => 0u8,
            ResidualPlane::U => 1u8,
            ResidualPlane::V => 2u8,
        };
        if let Ok(v2) = residual_token_v2::encode_ac_payload(freq, plane_b) {
            st.residual_token_v2_ac_body_bytes = st
                .residual_token_v2_ac_body_bytes
                .saturating_add(v2.len() as u64);
            st.residual_token_compare_block_count =
                st.residual_token_compare_block_count.saturating_add(1);
        }
    }
    let context_blob: Option<Vec<u8>> = if use_ctx {
        let m = ctx_model.ok_or_else(|| {
            SrsV2Error::syntax("internal: ContextV1 residual requires ResidualContextModel")
        })?;
        Some(encode_context_blob(freq, m, plane, left_nz, above_nz)?)
    } else {
        None
    };

    let coding = match policy {
        ResidualEntropy::Explicit => {
            if use_ctx {
                return Err(SrsV2Error::syntax(
                    "internal: Explicit residual with ContextV1 (validate settings)",
                ));
            }
            BlockResidualCoding::ExplicitTuples
        }
        ResidualEntropy::Rans => {
            if use_ctx {
                BlockResidualCoding::ContextRansV1
            } else if rans_choice.is_none() {
                return Err(SrsV2Error::syntax(
                    "forced rANS but coefficients out of range",
                ));
            } else {
                BlockResidualCoding::RansV1
            }
        }
        ResidualEntropy::Auto => {
            if use_ctx {
                if strict_fr2_context_v1_residual_only {
                    BlockResidualCoding::ContextRansV1
                } else {
                    pick_auto_three_way_rev7(
                        freq,
                        explicit_full,
                        &rans_choice,
                        context_blob.as_ref(),
                    )
                }
            } else {
                match &rans_choice {
                    Some((_, enc)) => {
                        let rfull = rans_wire_len_rev7(enc.len());
                        if rfull < explicit_full {
                            BlockResidualCoding::RansV1
                        } else {
                            BlockResidualCoding::ExplicitTuples
                        }
                    }
                    None => BlockResidualCoding::ExplicitTuples,
                }
            }
        }
    };

    out.push(mode as u8);
    out.extend_from_slice(&freq[0].to_le_bytes());
    out.push(qp_delta as u8);

    match coding {
        BlockResidualCoding::ExplicitTuples => {
            out.push(TAG_EXPLICIT_AC);
            write_explicit_ac_only(freq, out)?;
            Ok(BlockResidualCoding::ExplicitTuples)
        }
        BlockResidualCoding::RansV1 => {
            let (sym_ct, enc) = rans_choice.expect("rans payload");
            out.push(TAG_RANS_AC);
            let sc = u16::try_from(sym_ct).map_err(|_| SrsV2Error::syntax("rans sym count"))?;
            let bl =
                u16::try_from(enc.len()).map_err(|_| SrsV2Error::syntax("rans blob length"))?;
            out.extend_from_slice(&sc.to_le_bytes());
            out.extend_from_slice(&bl.to_le_bytes());
            out.extend_from_slice(&enc);
            Ok(BlockResidualCoding::RansV1)
        }
        BlockResidualCoding::ContextRansV1 => {
            let blob = context_blob.expect("context blob");
            out.push(TAG_CONTEXT_RANS_AC);
            let bl =
                u16::try_from(blob.len()).map_err(|_| SrsV2Error::syntax("context blob length"))?;
            out.extend_from_slice(&bl.to_le_bytes());
            out.extend_from_slice(&blob);
            if let Some(st) = context_residual_stats {
                record_residual_context_v1_tail_stats(st, blob.len(), freq, model, true);
            }
            Ok(BlockResidualCoding::ContextRansV1)
        }
    }
}

pub(crate) fn decode_intra_block_residual_rev7(
    data: &[u8],
    cur: &mut usize,
    ctx_model: &ResidualContextModel,
    plane: ResidualPlane,
    left_nz: bool,
    above_nz: bool,
) -> Result<(PredMode, i8, [i16; 64]), SrsV2Error> {
    let mode_b = read_u8(data, cur)?;
    let mode = PredMode::from_u8(mode_b)?;
    let mut freq = [0_i16; 64];
    freq[0] = read_i16(data, cur)?;
    let qp_delta = read_u8(data, cur)? as i8;
    validate_wire_qp_delta(qp_delta)?;
    let tag = read_u8(data, cur)?;
    match tag {
        TAG_EXPLICIT_AC => {
            let ac = read_explicit_ac_only(data, cur)?;
            for &k in ZIGZAG.iter().skip(1) {
                freq[k] = ac[k];
            }
            Ok((mode, qp_delta, freq))
        }
        TAG_RANS_AC => {
            let sym_ct = read_u16(data, cur)? as usize;
            let bl = read_u16(data, cur)? as usize;
            if sym_ct > MAX_SYMBOLS_PER_BLOCK {
                return Err(SrsV2Error::syntax("rans symbol count"));
            }
            let end = cur
                .checked_add(bl)
                .ok_or(SrsV2Error::Overflow("rans blob"))?;
            if end > data.len() {
                return Err(SrsV2Error::Truncated);
            }
            let blob = &data[*cur..end];
            *cur = end;
            let model = residual_token_model();
            let syms = rans_decode_tokens(&model, blob, sym_ct)?;
            detokenize_ac(&syms, &mut freq)?;
            Ok((mode, qp_delta, freq))
        }
        TAG_CONTEXT_RANS_AC => {
            let bl = read_u16(data, cur)? as usize;
            let end = cur
                .checked_add(bl)
                .ok_or(SrsV2Error::Overflow("context rANS blob"))?;
            if end > data.len() {
                return Err(SrsV2Error::Truncated);
            }
            let blob = &data[*cur..end];
            *cur = end;
            decode_residual_context_v1_plane(ctx_model, plane, left_nz, above_nz, blob, &mut freq)
                .map_err(map_residual_context_entropy)?;
            Ok((mode, qp_delta, freq))
        }
        _ => Err(SrsV2Error::syntax("bad residual coding tag")),
    }
}

fn map_transform_layout_err(e: TransformLayoutError) -> SrsV2Error {
    match e {
        TransformLayoutError::Truncated => SrsV2Error::Truncated,
        TransformLayoutError::BadMagic => SrsV2Error::syntax("coeff layout: bad magic"),
        TransformLayoutError::UnsupportedVersion(v) => SrsV2Error::UnsupportedVersion(v),
        TransformLayoutError::InvalidCoefficientCount { .. } => {
            SrsV2Error::syntax("coeff layout: bad coefficient count")
        }
        TransformLayoutError::InvalidDiscriminant(field, _v) => {
            if field == "transform_kind" {
                SrsV2Error::syntax("coeff layout: bad transform_kind tag")
            } else if field == "coeff_scan" {
                SrsV2Error::syntax("coeff layout: bad coeff_scan tag")
            } else if field == "coeff_layout_mode" {
                SrsV2Error::syntax("coeff layout: bad coeff_layout_mode tag")
            } else if field == "fr2_rev34_intra_grouping" {
                SrsV2Error::syntax("FR2 rev34 intra: bad grouping tag")
            } else if field == "fr2_rev35_p_grouping" {
                SrsV2Error::syntax("FR2 rev35 P: bad grouping tag")
            } else {
                SrsV2Error::syntax("coeff layout: bad discriminant")
            }
        }
        TransformLayoutError::InconsistentNonzeroCount { .. } => {
            SrsV2Error::syntax("coeff compact: bitmap does not match payload")
        }
    }
}

/// Per **8×8** block resolved [`SrsV2CoeffScan`] on **`FR2` rev32** intra (`Auto` is never stored — encoder resolves per block).
#[inline]
fn rev32_intra_mb_scan_wire(scan: SrsV2CoeffScan) -> u8 {
    match scan {
        SrsV2CoeffScan::ZigZag => 0,
        SrsV2CoeffScan::GroupedLowFirst => 1,
        SrsV2CoeffScan::RunOptimized => 2,
    }
}

fn rev32_intra_mb_scan_from_wire(b: u8) -> Result<SrsV2CoeffScan, SrsV2Error> {
    match b {
        0 => Ok(SrsV2CoeffScan::ZigZag),
        1 => Ok(SrsV2CoeffScan::GroupedLowFirst),
        2 => Ok(SrsV2CoeffScan::RunOptimized),
        _ => Err(SrsV2Error::syntax(
            "FR2 rev32/rev34 intra: bad per-block coeff_scan byte",
        )),
    }
}

fn resolve_intra_compact_scan(
    settings: &SrsV2EncodeSettings,
    kind: SrsV2TransformKind,
    qfreq: &[i16; 64],
) -> Result<SrsV2CoeffScan, SrsV2Error> {
    Ok(match settings.coeff_scan_mode {
        SrsV2CoeffScanMode::ZigZag => SrsV2CoeffScan::ZigZag,
        SrsV2CoeffScanMode::GroupedLowFirst => SrsV2CoeffScan::GroupedLowFirst,
        SrsV2CoeffScanMode::RunOptimized => SrsV2CoeffScan::RunOptimized,
        SrsV2CoeffScanMode::Auto => {
            choose_coeff_scan(kind, qfreq).map_err(map_transform_layout_err)?
        }
    })
}

/// Plane tag **`1`**: legacy rev32 compact — every MB is **Tx8×8** (`[pred][scan][u16 len][body]`).
/// Plane tag **`2`**: per-MB **`transform_kind`** (`0` **Tx4×4**, `1` **Tx8×8**) precedes `[pred][scan]…`.
const REV32_INTRA_PLANE_ALL_TX8: u8 = 1;
const REV32_INTRA_PLANE_MIXED_TRANSFORM: u8 = 2;

/// Plane header for **`FR2` rev34** intra transform grouping (**`1`** = explicit per-MB grouping + scan + pred + compact body).
const REV34_INTRA_PLANE_V1: u8 = 1;

fn rev32_mb_transform_from_wire(b: u8) -> Result<SrsV2TransformKind, SrsV2Error> {
    match b {
        0 => Ok(SrsV2TransformKind::Tx4x4),
        1 => Ok(SrsV2TransformKind::Tx8x8),
        _ => Err(SrsV2Error::syntax(
            "FR2 rev32 intra: bad per-macroblock transform_kind byte",
        )),
    }
}

fn rev32_intra_use_mixed_mb_transform(settings: &SrsV2EncodeSettings) -> bool {
    match settings.transform_grouping_mode {
        SrsV2TransformGroupingMode::Legacy8x8 => false,
        SrsV2TransformGroupingMode::Four4x4 => true,
        SrsV2TransformGroupingMode::AutoByResidual => matches!(
            settings.transform_decision_mode,
            SrsV2TransformDecisionMode::ResidualAware | SrsV2TransformDecisionMode::RdoFast
        ),
    }
}

fn intra_compact_pick_transform_kind(
    settings: &SrsV2EncodeSettings,
    orig: &[[i16; 8]; 8],
    diff_flat: &[i16; 64],
    diff_spatial: &[[i16; 8]; 8],
    qp: i16,
) -> SrsV2TransformKind {
    match settings.transform_grouping_mode {
        SrsV2TransformGroupingMode::Legacy8x8 => SrsV2TransformKind::Tx8x8,
        SrsV2TransformGroupingMode::Four4x4 => SrsV2TransformKind::Tx4x4,
        SrsV2TransformGroupingMode::AutoByResidual => match settings.transform_decision_mode {
            SrsV2TransformDecisionMode::Legacy => SrsV2TransformKind::Tx8x8,
            SrsV2TransformDecisionMode::ResidualAware => {
                let mut sum = 0.0f64;
                let mut sum2 = 0.0f64;
                for row in orig.iter() {
                    for &cell in row.iter() {
                        let v = cell as f64;
                        sum += v;
                        sum2 += v * v;
                    }
                }
                let mean = sum / 64.0;
                let spatial_variance = (sum2 / 64.0 - mean * mean).max(0.0);
                let freq = fdct_8x8(diff_flat);
                let hf_energy: f64 = freq.iter().skip(1).map(|c| (*c as f64).abs()).sum();
                let max_abs = diff_flat.iter().map(|c| c.abs()).max().unwrap_or(0);
                let nz = diff_flat.iter().filter(|&&c| c != 0).count();
                let inp = TransformDecisionInput {
                    spatial_variance,
                    hf_energy,
                    max_abs_coeff: max_abs,
                    nonzero_count: nz,
                };
                choose_transform_kind(&inp, &TransformDecisionConfig::default())
            }
            SrsV2TransformDecisionMode::RdoFast => {
                let qp_u8 = qp.clamp(1, 255) as u8;
                let lambda_fp = rdo_lambda_effective(settings, qp_u8);
                choose_grouping_rdo_fast(settings, diff_spatial, qp, lambda_fp)
                    .map(|d| d.kind)
                    .unwrap_or(SrsV2TransformKind::Tx8x8)
            }
        },
    }
}

/// Encode one fixed-grid **`FR2` rev33** **P** 8×8 quantized coefficient block (`transform_layout` compact).
///
/// Wire: [`TAG_P_RESIDUAL_COMPACT_V1`] + [`PredMode::Dc`] + **resolved** scan byte (`0..=2`) + `u16` body length +
/// [`encode_coeff_compact_rev32_intra_block`] (bitmap + values in scan order; [`SrsV2CoeffScanMode::Auto`] resolved per chunk).
///
/// When `residual_encode_stats` is set, accumulates [`ResidualEncodeStats::coeff_layout_bytes`],
/// [`ResidualEncodeStats::coeff_legacy_estimated_bytes`], and [`ResidualEncodeStats::coeff_layout_savings_bytes`]
/// using the same header + [`estimate_coeff_layout_bytes`] (**Legacy**) accounting as rev32 intra macroblocks
/// (chunk overhead = **5** bytes before the compact body).
pub fn encode_p_residual_chunk_compact_v33_wire(
    qfreq: &[i16; 64],
    settings: &SrsV2EncodeSettings,
    residual_encode_stats: Option<&mut ResidualEncodeStats>,
) -> Result<Vec<u8>, SrsV2Error> {
    let scan = resolve_intra_compact_scan(settings, SrsV2TransformKind::Tx8x8, qfreq)?;
    let mut out = Vec::with_capacity(4 + 8 + 64 * 2);
    out.push(TAG_P_RESIDUAL_COMPACT_V1);
    out.push(PredMode::Dc as u8);
    out.push(rev32_intra_mb_scan_wire(scan));
    let body = encode_coeff_compact_rev32_intra_block(qfreq, SrsV2TransformKind::Tx8x8, scan)
        .map_err(map_transform_layout_err)?;
    let bl =
        u16::try_from(body.len()).map_err(|_| SrsV2Error::syntax("FR2 rev33 P compact length"))?;
    out.extend_from_slice(&bl.to_le_bytes());
    out.extend_from_slice(&body);
    if let Some(stats) = residual_encode_stats {
        let legacy_est = estimate_coeff_layout_bytes(
            qfreq,
            SrsV2TransformKind::Tx8x8,
            scan,
            TlCoeffLayoutMode::Legacy,
        )
        .map_err(map_transform_layout_err)? as u64;
        let block_wire = 5usize.saturating_add(body.len());
        stats.coeff_legacy_estimated_bytes = stats
            .coeff_legacy_estimated_bytes
            .saturating_add(legacy_est);
        stats.coeff_layout_bytes = stats.coeff_layout_bytes.saturating_add(block_wire as u64);
        stats.coeff_layout_savings_bytes = stats
            .coeff_layout_savings_bytes
            .saturating_add(legacy_est as i64 - block_wire as i64);
        stats.p_compact_coeff_chunks = stats.p_compact_coeff_chunks.saturating_add(1);
        stats.p_compact_coeff_payload_bytes = stats
            .p_compact_coeff_payload_bytes
            .saturating_add(out.len() as u64);
    }
    Ok(out)
}

/// Decode [`encode_p_residual_chunk_compact_v33_wire`] (full chunk including layout tag).
///
/// Returns spatial-domain 8×8 residual samples (after inverse quantize + **`8×8`** IDCT) and **nonzero-AC**
/// in the quantized spectrum (for optional neighbor bookkeeping).
pub fn decode_p_residual_chunk_compact_v33(
    chunk: &[u8],
    qp: i16,
) -> Result<([[i16; 8]; 8], bool), SrsV2Error> {
    if chunk.first().copied() != Some(TAG_P_RESIDUAL_COMPACT_V1) {
        return Err(SrsV2Error::syntax(
            "FR2 rev33 P residual requires compact layout tag",
        ));
    }
    let body = &chunk[1..];
    let mut cur = 0usize;
    let mode_b = read_u8(body, &mut cur)?;
    PredMode::from_u8(mode_b)?;
    let scan_b = read_u8(body, &mut cur)?;
    let scan = rev32_intra_mb_scan_from_wire(scan_b)?;
    let body_len = read_u16(body, &mut cur)? as usize;
    let end = cur
        .checked_add(body_len)
        .ok_or(SrsV2Error::Overflow("FR2 rev33 compact body"))?;
    if end > body.len() {
        return Err(SrsV2Error::Truncated);
    }
    let freq =
        decode_coeff_compact_rev32_intra_block(&body[cur..end], SrsV2TransformKind::Tx8x8, scan)
            .map_err(map_transform_layout_err)?;
    cur = end;
    if cur != body.len() {
        return Err(SrsV2Error::syntax("FR2 rev33 P residual trailing bytes"));
    }
    let nz_ac = freq.iter().skip(1).any(|&v| v != 0);
    let recon_freq = dequantize(&freq, qp);
    let rpix = idct_8x8(&recon_freq);
    let mut out = [[0_i16; 8]; 8];
    for r in 0..8 {
        for c in 0..8 {
            out[r][c] = rpix[r * 8 + c];
        }
    }
    Ok((out, nz_ac))
}

/// Encode one fixed-grid **`FR2` rev35** **P** 8×8 quantized residual block with explicit **transform grouping** + **scan**.
///
/// Wire: [`TAG_P_RESIDUAL_TRANSFORM_GROUP_V35`] + **`grouping`** (`0` Four4×4, `1` Single8×8) + **`scan`** (`0..=2`) +
/// [`PredMode::Dc`] placeholder + `u16` body length + [`encode_coeff_compact_rev32_intra_block`] payload.
///
/// Uses the same spatial **`AutoByResidual`** / **`Four4×4`** decision policy as intra compact ([`intra_compact_pick_transform_kind`]).
///
/// Returns the chunk bytes and **nonzero-AC** in the quantized spectrum (for luma neighbor bookkeeping).
pub fn encode_p_residual_chunk_compact_v35_wire(
    cur_pixels: &[[i16; 8]; 8],
    spatial_residual: &[[i16; 8]; 8],
    qp: i16,
    settings: &SrsV2EncodeSettings,
    residual_encode_stats: Option<&mut ResidualEncodeStats>,
) -> Result<(Vec<u8>, bool), SrsV2Error> {
    let mut diff_flat = [0_i16; 64];
    for r in 0..8 {
        for c in 0..8 {
            diff_flat[r * 8 + c] = spatial_residual[r][c];
        }
    }
    let kind =
        intra_compact_pick_transform_kind(settings, cur_pixels, &diff_flat, spatial_residual, qp);
    let qfreq = match kind {
        SrsV2TransformKind::Tx8x8 => {
            let freq = fdct_8x8(&diff_flat);
            quantize(&freq, qp)
        }
        SrsV2TransformKind::Tx4x4 => quantize_residual_tx4x4_natural(spatial_residual, qp),
    };
    let scan = resolve_intra_compact_scan(settings, kind, &qfreq)?;
    let mut out = Vec::with_capacity(8 + 64 * 2);
    out.push(TAG_P_RESIDUAL_TRANSFORM_GROUP_V35);
    out.push(fr2_rev35_p_grouping_wire_from_transform_kind(kind));
    out.push(rev32_intra_mb_scan_wire(scan));
    out.push(PredMode::Dc as u8);
    let body = encode_coeff_compact_rev32_intra_block(&qfreq, kind, scan)
        .map_err(map_transform_layout_err)?;
    let bl =
        u16::try_from(body.len()).map_err(|_| SrsV2Error::syntax("FR2 rev35 P compact length"))?;
    out.extend_from_slice(&bl.to_le_bytes());
    out.extend_from_slice(&body);

    if let Some(stats) = residual_encode_stats {
        if stats.intra_transform_grouping_mode.is_none() {
            stats.intra_transform_grouping_mode = Some(settings.transform_grouping_mode);
            stats.intra_transform_decision_mode = Some(settings.transform_decision_mode);
        }
        let legacy_est = estimate_coeff_layout_bytes(&qfreq, kind, scan, TlCoeffLayoutMode::Legacy)
            .map_err(map_transform_layout_err)? as u64;
        let block_wire = 6usize.saturating_add(body.len());
        stats.coeff_legacy_estimated_bytes = stats
            .coeff_legacy_estimated_bytes
            .saturating_add(legacy_est);
        stats.coeff_layout_bytes = stats.coeff_layout_bytes.saturating_add(block_wire as u64);
        stats.coeff_layout_savings_bytes = stats
            .coeff_layout_savings_bytes
            .saturating_add(legacy_est as i64 - block_wire as i64);
        stats.legacy_transform_estimated_bytes = stats
            .legacy_transform_estimated_bytes
            .saturating_add(legacy_est);
        stats.transform_grouping_bytes = stats
            .transform_grouping_bytes
            .saturating_add(block_wire as u64);
        stats.transform_grouping_savings_bytes = stats
            .transform_grouping_savings_bytes
            .saturating_add(legacy_est as i64 - block_wire as i64);
        match kind {
            SrsV2TransformKind::Tx8x8 => {
                stats.intra_tx8x8_blocks = stats.intra_tx8x8_blocks.saturating_add(1);
                stats.single_8x8_blocks = stats.single_8x8_blocks.saturating_add(1);
            }
            SrsV2TransformKind::Tx4x4 => {
                stats.intra_tx4x4_blocks = stats.intra_tx4x4_blocks.saturating_add(1);
                stats.four_4x4_blocks = stats.four_4x4_blocks.saturating_add(1);
            }
        }
        stats.p_compact_coeff_chunks = stats.p_compact_coeff_chunks.saturating_add(1);
        stats.p_compact_coeff_payload_bytes = stats
            .p_compact_coeff_payload_bytes
            .saturating_add(out.len() as u64);
    }
    let nz_ac = match kind {
        SrsV2TransformKind::Tx8x8 => qfreq.iter().skip(1).any(|&v| v != 0),
        SrsV2TransformKind::Tx4x4 => qfreq
            .iter()
            .enumerate()
            .any(|(i, &v)| v != 0 && (i % 16) != 0),
    };
    Ok((out, nz_ac))
}

/// Decode [`encode_p_residual_chunk_compact_v35_wire`] (full chunk including layout tag).
pub fn decode_p_residual_chunk_compact_v35(
    chunk: &[u8],
    qp: i16,
) -> Result<([[i16; 8]; 8], bool), SrsV2Error> {
    if chunk.first().copied() != Some(TAG_P_RESIDUAL_TRANSFORM_GROUP_V35) {
        return Err(SrsV2Error::syntax(
            "FR2 rev35 P residual requires transform-group compact layout tag",
        ));
    }
    let body = &chunk[1..];
    let mut cur = 0usize;
    let grouping_b = read_u8(body, &mut cur)?;
    let kind = fr2_rev35_p_transform_kind_from_grouping_wire(grouping_b)
        .map_err(map_transform_layout_err)?;
    let scan_b = read_u8(body, &mut cur)?;
    let scan = rev32_intra_mb_scan_from_wire(scan_b)?;
    let mode_b = read_u8(body, &mut cur)?;
    PredMode::from_u8(mode_b)?;
    let body_len = read_u16(body, &mut cur)? as usize;
    let end = cur
        .checked_add(body_len)
        .ok_or(SrsV2Error::Overflow("FR2 rev35 P compact body"))?;
    if end > body.len() {
        return Err(SrsV2Error::Truncated);
    }
    let freq = decode_coeff_compact_rev32_intra_block(&body[cur..end], kind, scan)
        .map_err(map_transform_layout_err)?;
    cur = end;
    if cur != body.len() {
        return Err(SrsV2Error::syntax("FR2 rev35 P residual trailing bytes"));
    }
    let nz_ac = match kind {
        SrsV2TransformKind::Tx8x8 => freq.iter().skip(1).any(|&v| v != 0),
        SrsV2TransformKind::Tx4x4 => freq
            .iter()
            .enumerate()
            .any(|(i, &v)| v != 0 && (i % 16) != 0),
    };
    let out = match kind {
        SrsV2TransformKind::Tx8x8 => {
            let recon_freq = dequantize(&freq, qp);
            let rpix = idct_8x8(&recon_freq);
            let mut o = [[0_i16; 8]; 8];
            for r in 0..8 {
                for c in 0..8 {
                    o[r][c] = rpix[r * 8 + c];
                }
            }
            o
        }
        SrsV2TransformKind::Tx4x4 => idct_residual_tx4x4_from_qfreq(&freq, qp),
    };
    Ok((out, nz_ac))
}

/// Encode one luma/chroma plane for **`FR2` rev32** intra (`CompactV1` bitmap + scan-ordered coefficients).
///
/// Plane header: **`1`** = uniform **Tx8×8** MBs (default [`SrsV2TransformGroupingMode::Legacy8x8`]).
/// **`2`** = per-MB **`transform_kind`** byte (`0` **Tx4×4**, `1` **Tx8×8**) when [`SrsV2EncodeSettings::transform_grouping_mode`]
/// is [`SrsV2TransformGroupingMode::Four4x4`] or [`SrsV2TransformGroupingMode::AutoByResidual`] with
/// [`SrsV2TransformDecisionMode::ResidualAware`] / [`SrsV2TransformDecisionMode::RdoFast`].
/// Each **8×8** macroblock: optional transform tag + prediction mode + **resolved**
/// scan tag + `u16` body length + [`encode_coeff_compact_rev32_intra_block`] payload.
pub(crate) fn encode_plane_intra_compact_v32(
    plane: &VideoPlane<u8>,
    qp: i16,
    settings: &SrsV2EncodeSettings,
    stats: &mut ResidualEncodeStats,
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    if stats.intra_coeff_layout_mode.is_none() {
        stats.intra_coeff_layout_mode = Some(settings.coeff_layout_mode);
        stats.intra_coeff_scan_mode = Some(settings.coeff_scan_mode);
    }
    let mixed_mb = rev32_intra_use_mixed_mb_transform(settings);
    let plane_tag = if mixed_mb {
        REV32_INTRA_PLANE_MIXED_TRANSFORM
    } else {
        REV32_INTRA_PLANE_ALL_TX8
    };
    out.push(plane_tag);
    stats.coeff_layout_bytes = stats.coeff_layout_bytes.saturating_add(1);
    stats.coeff_legacy_estimated_bytes = stats.coeff_legacy_estimated_bytes.saturating_add(1);

    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let _ = (left_nz, above_nz);

            let mut orig = [[0_i16; 8]; 8];
            for (r, row) in orig.iter_mut().enumerate() {
                for (c, cell) in row.iter_mut().enumerate() {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let v = if x < w && y < h {
                        plane.samples[y * stride + x] as i16
                    } else {
                        128
                    };
                    *cell = v;
                }
            }
            let mode = pick_mode(&rec, pw, pw, ph, bx, by, &orig);
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let mut diff = [[0_i16; 8]; 8];
            for r in 0..8 {
                for c in 0..8 {
                    diff[r][c] = orig[r][c] - pred[r][c];
                }
            }
            let mut blk = [0_i16; 64];
            for r in 0..8 {
                for c in 0..8 {
                    blk[r * 8 + c] = diff[r][c];
                }
            }
            let kind = if mixed_mb {
                intra_compact_pick_transform_kind(settings, &orig, &blk, &diff, qp)
            } else {
                SrsV2TransformKind::Tx8x8
            };
            if mixed_mb {
                out.push(kind_tag_wire(kind));
                stats.coeff_layout_bytes = stats.coeff_layout_bytes.saturating_add(1);
            }

            let qfreq = match kind {
                SrsV2TransformKind::Tx8x8 => {
                    let freq = fdct_8x8(&blk);
                    quantize(&freq, qp)
                }
                SrsV2TransformKind::Tx4x4 => quantize_residual_tx4x4_natural(&diff, qp),
            };

            let scan = resolve_intra_compact_scan(settings, kind, &qfreq)?;
            let legacy_est =
                estimate_coeff_layout_bytes(&qfreq, kind, scan, TlCoeffLayoutMode::Legacy)
                    .map_err(map_transform_layout_err)? as u64;
            let body = encode_coeff_compact_rev32_intra_block(&qfreq, kind, scan)
                .map_err(map_transform_layout_err)?;
            let mb_tx = usize::from(mixed_mb);
            let block_wire = mb_tx + 1 + 1 + 2 + body.len();
            stats.coeff_legacy_estimated_bytes = stats
                .coeff_legacy_estimated_bytes
                .saturating_add(legacy_est);
            stats.coeff_layout_bytes = stats.coeff_layout_bytes.saturating_add(block_wire as u64);
            stats.coeff_layout_savings_bytes = stats
                .coeff_layout_savings_bytes
                .saturating_add(legacy_est as i64 - block_wire as i64);
            match kind {
                SrsV2TransformKind::Tx8x8 => {
                    stats.intra_tx8x8_blocks = stats.intra_tx8x8_blocks.saturating_add(1);
                }
                SrsV2TransformKind::Tx4x4 => {
                    stats.intra_tx4x4_blocks = stats.intra_tx4x4_blocks.saturating_add(1);
                }
            }

            out.push(mode as u8);
            out.push(rev32_intra_mb_scan_wire(scan));
            let bl =
                u16::try_from(body.len()).map_err(|_| SrsV2Error::syntax("rev32 body length"))?;
            out.extend_from_slice(&bl.to_le_bytes());
            out.extend_from_slice(&body);

            let nz_ac = match kind {
                SrsV2TransformKind::Tx8x8 => qfreq.iter().skip(1).any(|&v| v != 0),
                SrsV2TransformKind::Tx4x4 => qfreq
                    .iter()
                    .enumerate()
                    .any(|(i, &v)| v != 0 && (i % 16) != 0),
            };
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            match kind {
                SrsV2TransformKind::Tx8x8 => {
                    let recon_freq = dequantize(&qfreq, qp);
                    let rpix = idct_8x8(&recon_freq);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
                SrsV2TransformKind::Tx4x4 => {
                    let residual = idct_residual_tx4x4_from_qfreq(&qfreq, qp);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + residual[r][c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Encode one luma/chroma plane for **`FR2` rev34** intra — explicit **transform grouping** + **scan** + prediction + compact coefficients ([`encode_coeff_compact_rev32_intra_block`]).
///
/// Plane: **`1`** byte schema **`1`**, then each **8×8** MB: **`grouping`** (`0` **Four4×4**, `1` **Single8×8**),
/// **`scan`** (`0..=2`), **`pred`**, **`u16`** body length, compact bitmap + scan-ordered **`i16`** values (natural indices via bitmap).
pub(crate) fn encode_plane_intra_compact_v34(
    plane: &VideoPlane<u8>,
    qp: i16,
    settings: &SrsV2EncodeSettings,
    stats: &mut ResidualEncodeStats,
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    if stats.intra_coeff_layout_mode.is_none() {
        stats.intra_coeff_layout_mode = Some(settings.coeff_layout_mode);
        stats.intra_coeff_scan_mode = Some(settings.coeff_scan_mode);
    }
    if stats.intra_transform_grouping_mode.is_none() {
        stats.intra_transform_grouping_mode = Some(settings.transform_grouping_mode);
        stats.intra_transform_decision_mode = Some(settings.transform_decision_mode);
    }

    out.push(REV34_INTRA_PLANE_V1);
    stats.coeff_layout_bytes = stats.coeff_layout_bytes.saturating_add(1);
    stats.coeff_legacy_estimated_bytes = stats.coeff_legacy_estimated_bytes.saturating_add(1);
    stats.transform_grouping_bytes = stats.transform_grouping_bytes.saturating_add(1);

    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let _ = (left_nz, above_nz);

            let mut orig = [[0_i16; 8]; 8];
            for (r, row) in orig.iter_mut().enumerate() {
                for (c, cell) in row.iter_mut().enumerate() {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let v = if x < w && y < h {
                        plane.samples[y * stride + x] as i16
                    } else {
                        128
                    };
                    *cell = v;
                }
            }
            let mode = pick_mode(&rec, pw, pw, ph, bx, by, &orig);
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let mut diff = [[0_i16; 8]; 8];
            for r in 0..8 {
                for c in 0..8 {
                    diff[r][c] = orig[r][c] - pred[r][c];
                }
            }
            let mut blk = [0_i16; 64];
            for r in 0..8 {
                for c in 0..8 {
                    blk[r * 8 + c] = diff[r][c];
                }
            }
            let kind = intra_compact_pick_transform_kind(settings, &orig, &blk, &diff, qp);

            let qfreq = match kind {
                SrsV2TransformKind::Tx8x8 => {
                    let freq = fdct_8x8(&blk);
                    quantize(&freq, qp)
                }
                SrsV2TransformKind::Tx4x4 => quantize_residual_tx4x4_natural(&diff, qp),
            };

            let scan = resolve_intra_compact_scan(settings, kind, &qfreq)?;
            let legacy_est =
                estimate_coeff_layout_bytes(&qfreq, kind, scan, TlCoeffLayoutMode::Legacy)
                    .map_err(map_transform_layout_err)? as u64;
            let body = encode_coeff_compact_rev32_intra_block(&qfreq, kind, scan)
                .map_err(map_transform_layout_err)?;
            let block_wire = 1usize + 1 + 1 + 2 + body.len();
            stats.coeff_legacy_estimated_bytes = stats
                .coeff_legacy_estimated_bytes
                .saturating_add(legacy_est);
            stats.coeff_layout_bytes = stats.coeff_layout_bytes.saturating_add(block_wire as u64);
            stats.coeff_layout_savings_bytes = stats
                .coeff_layout_savings_bytes
                .saturating_add(legacy_est as i64 - block_wire as i64);
            stats.legacy_transform_estimated_bytes = stats
                .legacy_transform_estimated_bytes
                .saturating_add(legacy_est);
            stats.transform_grouping_bytes = stats
                .transform_grouping_bytes
                .saturating_add(block_wire as u64);
            stats.transform_grouping_savings_bytes = stats
                .transform_grouping_savings_bytes
                .saturating_add(legacy_est as i64 - block_wire as i64);
            match kind {
                SrsV2TransformKind::Tx8x8 => {
                    stats.intra_tx8x8_blocks = stats.intra_tx8x8_blocks.saturating_add(1);
                    stats.single_8x8_blocks = stats.single_8x8_blocks.saturating_add(1);
                }
                SrsV2TransformKind::Tx4x4 => {
                    stats.intra_tx4x4_blocks = stats.intra_tx4x4_blocks.saturating_add(1);
                    stats.four_4x4_blocks = stats.four_4x4_blocks.saturating_add(1);
                }
            }

            out.push(fr2_rev34_grouping_wire_from_transform_kind(kind));
            out.push(rev32_intra_mb_scan_wire(scan));
            out.push(mode as u8);
            let bl =
                u16::try_from(body.len()).map_err(|_| SrsV2Error::syntax("rev34 body length"))?;
            out.extend_from_slice(&bl.to_le_bytes());
            out.extend_from_slice(&body);

            let nz_ac = match kind {
                SrsV2TransformKind::Tx8x8 => qfreq.iter().skip(1).any(|&v| v != 0),
                SrsV2TransformKind::Tx4x4 => qfreq
                    .iter()
                    .enumerate()
                    .any(|(i, &v)| v != 0 && (i % 16) != 0),
            };
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            match kind {
                SrsV2TransformKind::Tx8x8 => {
                    let recon_freq = dequantize(&qfreq, qp);
                    let rpix = idct_8x8(&recon_freq);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
                SrsV2TransformKind::Tx4x4 => {
                    let residual = idct_residual_tx4x4_from_qfreq(&qfreq, qp);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + residual[r][c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn kind_tag_wire(k: SrsV2TransformKind) -> u8 {
    match k {
        SrsV2TransformKind::Tx4x4 => 0,
        SrsV2TransformKind::Tx8x8 => 1,
    }
}

/// Decode [`encode_plane_intra_compact_v32`] plane blob.
pub(crate) fn decode_plane_intra_compact_v32(
    data: &[u8],
    cursor: &mut usize,
    plane: &mut VideoPlane<u8>,
    qp: i16,
) -> Result<(), SrsV2Error> {
    let plane_tag = read_u8(data, cursor)?;
    let mixed_mb = match plane_tag {
        REV32_INTRA_PLANE_ALL_TX8 => false,
        REV32_INTRA_PLANE_MIXED_TRANSFORM => true,
        _ => {
            return Err(SrsV2Error::syntax(
                "FR2 rev32 intra: bad plane transform layout tag",
            ));
        }
    };

    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let _ = (left_nz, above_nz);

            let kind = if mixed_mb {
                rev32_mb_transform_from_wire(read_u8(data, cursor)?)?
            } else {
                SrsV2TransformKind::Tx8x8
            };

            let mode_b = read_u8(data, cursor)?;
            let mode = PredMode::from_u8(mode_b)?;
            let scan_mb_b = read_u8(data, cursor)?;
            let scan = rev32_intra_mb_scan_from_wire(scan_mb_b)?;
            let bl = read_u16(data, cursor)? as usize;
            let end = cursor
                .checked_add(bl)
                .ok_or(SrsV2Error::Overflow("rev32 block body"))?;
            if end > data.len() {
                return Err(SrsV2Error::Truncated);
            }
            let body = &data[*cursor..end];
            *cursor = end;
            let qfreq = decode_coeff_compact_rev32_intra_block(body, kind, scan)
                .map_err(map_transform_layout_err)?;
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let nz_ac = match kind {
                SrsV2TransformKind::Tx8x8 => qfreq.iter().skip(1).any(|&v| v != 0),
                SrsV2TransformKind::Tx4x4 => qfreq
                    .iter()
                    .enumerate()
                    .any(|(i, &v)| v != 0 && (i % 16) != 0),
            };
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            match kind {
                SrsV2TransformKind::Tx8x8 => {
                    let recon_freq = dequantize(&qfreq, qp);
                    let rpix = idct_8x8(&recon_freq);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
                SrsV2TransformKind::Tx4x4 => {
                    let residual = idct_residual_tx4x4_from_qfreq(&qfreq, qp);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + residual[r][c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            plane.samples[y * stride + x] = rec[y * pw + x];
        }
    }
    Ok(())
}

/// Decode [`encode_plane_intra_compact_v34`] plane blob.
pub(crate) fn decode_plane_intra_compact_v34(
    data: &[u8],
    cursor: &mut usize,
    plane: &mut VideoPlane<u8>,
    qp: i16,
) -> Result<(), SrsV2Error> {
    let plane_ver = read_u8(data, cursor)?;
    if plane_ver != REV34_INTRA_PLANE_V1 {
        return Err(SrsV2Error::syntax(
            "FR2 rev34 intra: bad plane schema version",
        ));
    }

    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let _ = (left_nz, above_nz);

            let grouping_b = read_u8(data, cursor)?;
            let kind = fr2_rev34_transform_kind_from_grouping_wire(grouping_b)
                .map_err(map_transform_layout_err)?;
            let scan_b = read_u8(data, cursor)?;
            let scan = rev32_intra_mb_scan_from_wire(scan_b)?;
            let mode_b = read_u8(data, cursor)?;
            let mode = PredMode::from_u8(mode_b)?;
            let bl = read_u16(data, cursor)? as usize;
            let end = cursor
                .checked_add(bl)
                .ok_or(SrsV2Error::Overflow("rev34 block body"))?;
            if end > data.len() {
                return Err(SrsV2Error::Truncated);
            }
            let body = &data[*cursor..end];
            *cursor = end;
            let qfreq = decode_coeff_compact_rev32_intra_block(body, kind, scan)
                .map_err(map_transform_layout_err)?;
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let nz_ac = match kind {
                SrsV2TransformKind::Tx8x8 => qfreq.iter().skip(1).any(|&v| v != 0),
                SrsV2TransformKind::Tx4x4 => qfreq
                    .iter()
                    .enumerate()
                    .any(|(i, &v)| v != 0 && (i % 16) != 0),
            };
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            match kind {
                SrsV2TransformKind::Tx8x8 => {
                    let recon_freq = dequantize(&qfreq, qp);
                    let rpix = idct_8x8(&recon_freq);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
                SrsV2TransformKind::Tx4x4 => {
                    let residual = idct_residual_tx4x4_from_qfreq(&qfreq, qp);
                    for r in 0..8 {
                        for c in 0..8 {
                            let x = bx * 8 + c;
                            let y = by * 8 + r;
                            let pv = (pred[r][c] as i32 + residual[r][c] as i32).clamp(0, 255);
                            if x < pw && y < ph {
                                rec[y * pw + x] = pv as u8;
                            }
                        }
                    }
                }
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            plane.samples[y * stride + x] = rec[y * pw + x];
        }
    }
    Ok(())
}

/// Per-plane counters for folding into [`crate::srsv2::adaptive_quant::SrsV2BlockAqWireStats`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PlaneBlockAqSummary {
    pub blocks: u32,
    pub sum_eff_qp: u64,
    pub min_eff_qp: u8,
    pub max_eff_qp: u8,
    pub pos_delta: u32,
    pub neg_delta: u32,
    pub zero_delta: u32,
}

pub(crate) fn encode_plane_intra_entropy(
    plane: &VideoPlane<u8>,
    qp: i16,
    settings: &SrsV2EncodeSettings,
    plane_kind: ResidualPlane,
    stats: &mut ResidualEncodeStats,
    // When true: only TAG_CONTEXT_RANS_AC blocks (`FR2` rev29).
    strict_fr2_rev29_context_v1: bool,
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    settings.validate_residual_context_mode()?;
    if matches!(
        settings.residual_context_mode,
        SrsV2ResidualContextMode::ContextV1
    ) {
        stats.residual_context_enabled = true;
    }
    let policy = settings.residual_entropy;
    let residual_ctx_mode = settings.residual_context_mode;
    let model = residual_token_model();
    let ctx_model_storage = if matches!(residual_ctx_mode, SrsV2ResidualContextMode::ContextV1) {
        Some(ResidualContextModel::new())
    } else {
        None
    };
    let ctx_model_ref = ctx_model_storage.as_ref();
    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let mut orig = [[0_i16; 8]; 8];
            for (r, row) in orig.iter_mut().enumerate() {
                for (c, cell) in row.iter_mut().enumerate() {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let v = if x < w && y < h {
                        plane.samples[y * stride + x] as i16
                    } else {
                        128
                    };
                    *cell = v;
                }
            }
            let mode = pick_mode(&rec, pw, pw, ph, bx, by, &orig);
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let mut diff = [[0_i16; 8]; 8];
            for r in 0..8 {
                for c in 0..8 {
                    diff[r][c] = orig[r][c] - pred[r][c];
                }
            }
            let mut blk = [0_i16; 64];
            for r in 0..8 {
                for c in 0..8 {
                    blk[r * 8 + c] = diff[r][c];
                }
            }
            let freq = fdct_8x8(&blk);
            let qfreq = quantize(&freq, qp);
            let kind = encode_intra_block_residual(
                mode,
                &qfreq,
                policy,
                residual_ctx_mode,
                &model,
                ctx_model_ref,
                plane_kind,
                left_nz,
                above_nz,
                strict_fr2_rev29_context_v1,
                Some(stats),
                out,
            )?;
            match kind {
                BlockResidualCoding::ExplicitTuples => stats.intra_explicit_blocks += 1,
                BlockResidualCoding::RansV1 => stats.intra_rans_blocks += 1,
                BlockResidualCoding::ContextRansV1 => stats.intra_context_residual_blocks += 1,
            }
            let nz_ac = qfreq.iter().skip(1).any(|&v| v != 0);
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            let recon_freq = dequantize(&qfreq, qp);
            let rpix = idct_8x8(&recon_freq);
            for r in 0..8 {
                for c in 0..8 {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                    if x < pw && y < ph {
                        rec[y * pw + x] = pv as u8;
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_plane_intra_entropy_block_aq(
    plane: &VideoPlane<u8>,
    base_qp: u8,
    clip_min: u8,
    clip_max: u8,
    plane_kind: ResidualPlane,
    stats: &mut ResidualEncodeStats,
    settings: &SrsV2EncodeSettings,
    out: &mut Vec<u8>,
) -> Result<PlaneBlockAqSummary, SrsV2Error> {
    validate_qp_clip_range(clip_min, clip_max)?;
    settings.validate_residual_context_mode()?;
    if matches!(
        settings.residual_context_mode,
        SrsV2ResidualContextMode::ContextV1
    ) {
        stats.residual_context_enabled = true;
    }
    let policy = settings.residual_entropy;
    let residual_ctx_mode = settings.residual_context_mode;
    let model = residual_token_model();
    let ctx_model_storage = if matches!(residual_ctx_mode, SrsV2ResidualContextMode::ContextV1) {
        Some(ResidualContextModel::new())
    } else {
        None
    };
    let ctx_model_ref = ctx_model_storage.as_ref();
    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let (vars, median_var) = collect_plane_block_variances(plane);

    let mut acc = PlaneBlockAqSummary::default();
    if bw == 0 || bh == 0 {
        return Ok(acc);
    }
    acc.min_eff_qp = u8::MAX;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let idx = by * bw + bx;
            let block_var = vars[idx];
            let qp_delta = choose_block_qp_delta(
                block_var,
                median_var,
                settings.aq_strength,
                settings.min_block_qp_delta,
                settings.max_block_qp_delta,
            );
            validate_wire_qp_delta(qp_delta)?;
            let eff_qp = apply_qp_delta_clamped(base_qp, qp_delta, clip_min, clip_max);
            let qp_i = eff_qp.max(1) as i16;

            acc.blocks += 1;
            acc.sum_eff_qp += u64::from(eff_qp);
            acc.min_eff_qp = acc.min_eff_qp.min(eff_qp);
            acc.max_eff_qp = acc.max_eff_qp.max(eff_qp);
            if qp_delta > 0 {
                acc.pos_delta += 1;
            } else if qp_delta < 0 {
                acc.neg_delta += 1;
            } else {
                acc.zero_delta += 1;
            }

            let mut orig = [[0_i16; 8]; 8];
            for (r, row) in orig.iter_mut().enumerate() {
                for (c, cell) in row.iter_mut().enumerate() {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let v = if x < w && y < h {
                        plane.samples[y * stride + x] as i16
                    } else {
                        128
                    };
                    *cell = v;
                }
            }
            let mode = pick_mode(&rec, pw, pw, ph, bx, by, &orig);
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let mut diff = [[0_i16; 8]; 8];
            for r in 0..8 {
                for c in 0..8 {
                    diff[r][c] = orig[r][c] - pred[r][c];
                }
            }
            let mut blk = [0_i16; 64];
            for r in 0..8 {
                for c in 0..8 {
                    blk[r * 8 + c] = diff[r][c];
                }
            }
            let freq = fdct_8x8(&blk);
            let qfreq = quantize(&freq, qp_i);
            let kind = encode_intra_block_residual_rev7(
                mode,
                &qfreq,
                qp_delta,
                policy,
                residual_ctx_mode,
                &model,
                ctx_model_ref,
                plane_kind,
                left_nz,
                above_nz,
                false,
                Some(stats),
                out,
            )?;
            match kind {
                BlockResidualCoding::ExplicitTuples => stats.intra_explicit_blocks += 1,
                BlockResidualCoding::RansV1 => stats.intra_rans_blocks += 1,
                BlockResidualCoding::ContextRansV1 => stats.intra_context_residual_blocks += 1,
            }
            let nz_ac = qfreq.iter().skip(1).any(|&v| v != 0);
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            let recon_freq = dequantize(&qfreq, qp_i);
            let rpix = idct_8x8(&recon_freq);
            for r in 0..8 {
                for c in 0..8 {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                    if x < pw && y < ph {
                        rec[y * pw + x] = pv as u8;
                    }
                }
            }
        }
    }
    Ok(acc)
}

pub(crate) fn decode_plane_intra_entropy(
    data: &[u8],
    cursor: &mut usize,
    plane: &mut VideoPlane<u8>,
    qp: i16,
    plane_kind: ResidualPlane,
    strict_fr2_rev29_context_v1: bool,
) -> Result<(), SrsV2Error> {
    let ctx_model = ResidualContextModel::new();
    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let (mode, freq) = if strict_fr2_rev29_context_v1 {
                decode_intra_block_residual_strict_context_v1(
                    data, cursor, &ctx_model, plane_kind, left_nz, above_nz,
                )?
            } else {
                decode_intra_block_residual(
                    data, cursor, &ctx_model, plane_kind, left_nz, above_nz,
                )?
            };
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let recon_freq = dequantize(&freq, qp);
            let rpix = idct_8x8(&recon_freq);
            let nz_ac = freq.iter().skip(1).any(|&v| v != 0);
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            for r in 0..8 {
                for c in 0..8 {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                    if x < pw && y < ph {
                        rec[y * pw + x] = pv as u8;
                    }
                }
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            plane.samples[y * stride + x] = rec[y * pw + x];
        }
    }
    Ok(())
}

pub(crate) fn decode_plane_intra_entropy_block_aq(
    data: &[u8],
    cursor: &mut usize,
    plane: &mut VideoPlane<u8>,
    base_qp: u8,
    clip_min: u8,
    clip_max: u8,
    plane_kind: ResidualPlane,
) -> Result<(), SrsV2Error> {
    validate_qp_clip_range(clip_min, clip_max)?;
    let ctx_model = ResidualContextModel::new();
    let w = plane.width as usize;
    let h = plane.height as usize;
    let stride = plane.stride;
    let pw = (w + 7) & !7;
    let ph = (h + 7) & !7;
    let mut rec = vec![128_u8; pw.saturating_mul(ph)];
    let bw = pw / 8;
    let bh = ph / 8;
    let mut prev_row_nz = vec![false; bw.max(1)];
    for by in 0..bh {
        let mut left_nz = false;
        for (bx, prev_slot) in prev_row_nz.iter_mut().enumerate().take(bw) {
            let above_nz = if by == 0 { false } else { *prev_slot };
            let (mode, qp_delta, freq) = decode_intra_block_residual_rev7(
                data, cursor, &ctx_model, plane_kind, left_nz, above_nz,
            )?;
            let eff_qp = apply_qp_delta_clamped(base_qp, qp_delta, clip_min, clip_max);
            let qp_i = eff_qp.max(1) as i16;
            let pred = predict_block(mode, &rec, pw, pw, ph, bx, by);
            let recon_freq = dequantize(&freq, qp_i);
            let rpix = idct_8x8(&recon_freq);
            let nz_ac = freq.iter().skip(1).any(|&v| v != 0);
            left_nz = nz_ac;
            *prev_slot = nz_ac;
            for r in 0..8 {
                for c in 0..8 {
                    let x = bx * 8 + c;
                    let y = by * 8 + r;
                    let pv = (pred[r][c] as i32 + rpix[r * 8 + c] as i32).clamp(0, 255);
                    if x < pw && y < ph {
                        rec[y * pw + x] = pv as u8;
                    }
                }
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            plane.samples[y * stride + x] = rec[y * pw + x];
        }
    }
    Ok(())
}

/// P-frame 8×8 residual chunk (after outer `u32` length): `0` + legacy body, or `1` + adaptive body.
pub fn encode_p_residual_chunk(
    qfreq: &[i16; 64],
    policy: ResidualEntropy,
    model: &RansModel,
) -> Result<(Vec<u8>, PResidualChunkKind), SrsV2Error> {
    encode_p_residual_chunk_with_opts(
        qfreq,
        policy,
        model,
        &PResidualChunkEncodeOpts::default(),
        None,
    )
}

/// Like [`encode_p_residual_chunk`] with optional [`SrsV2ResidualContextMode::ContextV1`] (**requires** `context_model`).
pub fn encode_p_residual_chunk_with_opts<'a>(
    qfreq: &[i16; 64],
    policy: ResidualEntropy,
    model: &RansModel,
    opts: &PResidualChunkEncodeOpts<'a>,
    residual_encode_stats: Option<&mut ResidualEncodeStats>,
) -> Result<(Vec<u8>, PResidualChunkKind), SrsV2Error> {
    let mut legacy = Vec::new();
    legacy.push(PredMode::Dc as u8);
    legacy.extend_from_slice(&qfreq[0].to_le_bytes());
    write_explicit_ac_only(qfreq, &mut legacy)?;

    if matches!(
        opts.residual_context_mode,
        SrsV2ResidualContextMode::ContextV1
    ) {
        if opts.context_model.is_none() {
            return Err(SrsV2Error::syntax(
                "encode_p_residual_chunk_with_opts: ContextV1 requires context_model",
            ));
        }
        if matches!(policy, ResidualEntropy::Explicit) {
            return Err(SrsV2Error::syntax(
                "encode_p_residual_chunk_with_opts: ContextV1 incompatible with Explicit policy",
            ));
        }
    }

    match policy {
        ResidualEntropy::Explicit => {
            let mut out = Vec::with_capacity(1 + legacy.len());
            out.push(0);
            out.extend_from_slice(&legacy);
            Ok((out, PResidualChunkKind::LegacyTuple))
        }
        ResidualEntropy::Rans => {
            let mut adaptive = Vec::new();
            let kind = encode_intra_block_residual(
                PredMode::Dc,
                qfreq,
                ResidualEntropy::Rans,
                opts.residual_context_mode,
                model,
                opts.context_model,
                opts.plane,
                opts.left_neighbor_nonzero,
                opts.above_neighbor_nonzero,
                opts.strict_fr2_rev30_residual,
                residual_encode_stats,
                &mut adaptive,
            )?;
            let mut out = Vec::with_capacity(1 + adaptive.len());
            out.push(1);
            out.extend_from_slice(&adaptive);
            Ok((out, PResidualChunkKind::Adaptive(kind)))
        }
        ResidualEntropy::Auto => {
            let strict_rev30 = opts.strict_fr2_rev30_residual
                && matches!(
                    opts.residual_context_mode,
                    SrsV2ResidualContextMode::ContextV1
                );
            if strict_rev30 {
                let mut adaptive = Vec::new();
                let kind = encode_intra_block_residual(
                    PredMode::Dc,
                    qfreq,
                    ResidualEntropy::Auto,
                    opts.residual_context_mode,
                    model,
                    opts.context_model,
                    opts.plane,
                    opts.left_neighbor_nonzero,
                    opts.above_neighbor_nonzero,
                    true,
                    residual_encode_stats,
                    &mut adaptive,
                )?;
                let mut out = Vec::with_capacity(1 + adaptive.len());
                out.push(1);
                out.extend_from_slice(&adaptive);
                return Ok((out, PResidualChunkKind::Adaptive(kind)));
            }
            let mut adaptive = Vec::new();
            let kind = encode_intra_block_residual(
                PredMode::Dc,
                qfreq,
                ResidualEntropy::Auto,
                opts.residual_context_mode,
                model,
                opts.context_model,
                opts.plane,
                opts.left_neighbor_nonzero,
                opts.above_neighbor_nonzero,
                false,
                residual_encode_stats,
                &mut adaptive,
            )?;
            if adaptive.len() < legacy.len() {
                let mut out = Vec::with_capacity(1 + adaptive.len());
                out.push(1);
                out.extend_from_slice(&adaptive);
                Ok((out, PResidualChunkKind::Adaptive(kind)))
            } else {
                let mut out = Vec::with_capacity(1 + legacy.len());
                out.push(0);
                out.extend_from_slice(&legacy);
                Ok((out, PResidualChunkKind::LegacyTuple))
            }
        }
    }
}

pub(crate) fn write_explicit_ac_only_4x4(
    freq: &[i16; 16],
    out: &mut Vec<u8>,
) -> Result<(), SrsV2Error> {
    let mut pairs = 0_usize;
    let mut tmp = Vec::new();
    for &k in ZIGZAG_4X4.iter().skip(1) {
        let v = freq[k];
        if v != 0 {
            if pairs >= 15 {
                return Err(SrsV2Error::syntax("too many 4x4 ac coeffs"));
            }
            tmp.push(k as u8);
            tmp.extend_from_slice(&v.to_le_bytes());
            pairs += 1;
        }
    }
    let pairs_u16 = u16::try_from(pairs).map_err(|_| SrsV2Error::syntax("4x4 ac pairs"))?;
    out.extend_from_slice(&pairs_u16.to_le_bytes());
    out.extend_from_slice(&tmp);
    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "plane bitstream",
        });
    }
    Ok(())
}

/// Packed **`FR2` rev 19+** 4×4 residual chunk (explicit tuples only in this slice).
pub fn encode_p_residual_chunk_4x4(qfreq: &[i16; 16]) -> Result<Vec<u8>, SrsV2Error> {
    let mut legacy = Vec::new();
    legacy.push(PredMode::Dc as u8);
    legacy.extend_from_slice(&qfreq[0].to_le_bytes());
    write_explicit_ac_only_4x4(qfreq, &mut legacy)?;
    let mut out = Vec::with_capacity(1 + legacy.len());
    out.push(0);
    out.extend_from_slice(&legacy);
    Ok(out)
}

pub fn decode_p_residual_chunk_4x4(chunk: &[u8], qp: i16) -> Result<[[i16; 4]; 4], SrsV2Error> {
    if chunk.is_empty() {
        return Err(SrsV2Error::Truncated);
    }
    let layout = chunk[0];
    let body = &chunk[1..];
    let mut cur = 0usize;
    if layout != 0 {
        return Err(SrsV2Error::syntax("bad P 4x4 residual layout"));
    }
    let mode_b = read_u8(body, &mut cur)?;
    PredMode::from_u8(mode_b)?;
    let mut freq = [0_i16; 16];
    freq[0] = read_i16(body, &mut cur)?;
    let pairs = read_u16(body, &mut cur)? as usize;
    if pairs > 15 {
        return Err(SrsV2Error::syntax("4x4 ac pairs overflow"));
    }
    for _ in 0..pairs {
        let pos = read_u8(body, &mut cur)? as usize;
        if pos == 0 || pos > 15 {
            return Err(SrsV2Error::syntax("bad 4x4 coeff index"));
        }
        freq[pos] = read_i16(body, &mut cur)?;
    }
    if cur != body.len() {
        return Err(SrsV2Error::syntax("p 4x4 residual trailing"));
    }
    let recon_freq = dequantize_4x4(&freq, qp);
    let rpix = idct_4x4(&recon_freq);
    let mut out = [[0_i16; 4]; 4];
    for r in 0..4 {
        for c in 0..4 {
            out[r][c] = rpix[r * 4 + c];
        }
    }
    Ok(out)
}

pub fn decode_p_residual_chunk(chunk: &[u8], qp: i16) -> Result<[[i16; 8]; 8], SrsV2Error> {
    decode_p_residual_chunk_with_neighbors(chunk, qp, ResidualPlane::Y, false, false)
}

/// Decode adaptive **`P`** 8×8 residual chunk when [`TAG_CONTEXT_RANS_AC`] may be present (**neighbor hints**
/// must match encode order when context residuals are used).
pub fn decode_p_residual_chunk_with_neighbors(
    chunk: &[u8],
    qp: i16,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
) -> Result<[[i16; 8]; 8], SrsV2Error> {
    if chunk.is_empty() {
        return Err(SrsV2Error::Truncated);
    }
    let layout = chunk[0];
    if layout == TAG_P_RESIDUAL_COMPACT_V1 {
        let (spatial, _) = decode_p_residual_chunk_compact_v33(chunk, qp)?;
        return Ok(spatial);
    }
    if layout == TAG_P_RESIDUAL_TRANSFORM_GROUP_V35 {
        let (spatial, _) = decode_p_residual_chunk_compact_v35(chunk, qp)?;
        return Ok(spatial);
    }
    let body = &chunk[1..];
    let mut cur = 0usize;
    let ctx_model = ResidualContextModel::new();
    let freq = match layout {
        0 => {
            let mode_b = read_u8(body, &mut cur)?;
            PredMode::from_u8(mode_b)?;
            let mut freq = [0_i16; 64];
            freq[0] = read_i16(body, &mut cur)?;
            let pairs = read_u16(body, &mut cur)? as usize;
            if pairs > 63 {
                return Err(SrsV2Error::syntax("ac pairs overflow"));
            }
            for _ in 0..pairs {
                let pos = read_u8(body, &mut cur)? as usize;
                if pos == 0 || pos > 63 {
                    return Err(SrsV2Error::syntax("bad coeff index"));
                }
                let v = read_i16(body, &mut cur)?;
                freq[pos] = v;
            }
            if cur != body.len() {
                return Err(SrsV2Error::syntax("p residual trailing"));
            }
            freq
        }
        1 => {
            let (_m, f) = decode_intra_block_residual(
                body,
                &mut cur,
                &ctx_model,
                plane,
                left_neighbor_nonzero,
                above_neighbor_nonzero,
            )?;
            if cur != body.len() {
                return Err(SrsV2Error::syntax("p rans residual trailing"));
            }
            f
        }
        _ => return Err(SrsV2Error::syntax("bad P residual layout")),
    };
    let recon_freq = dequantize(&freq, qp);
    let rpix = idct_8x8(&recon_freq);
    let mut out = [[0_i16; 8]; 8];
    for r in 0..8 {
        for c in 0..8 {
            out[r][c] = rpix[r * 8 + c];
        }
    }
    Ok(out)
}

/// [`FR2` rev **30**] **`P`** luma 8×8 residual chunk: outer adaptive byte **`1`** and inner [`TAG_CONTEXT_RANS_AC`] only.
///
/// Returns reconstructed residual samples and **nonzero-AC** flag for context neighbor threading.
pub fn decode_p_residual_chunk_strict_rev30(
    chunk: &[u8],
    qp: i16,
    plane: ResidualPlane,
    left_neighbor_nonzero: bool,
    above_neighbor_nonzero: bool,
) -> Result<([[i16; 8]; 8], bool), SrsV2Error> {
    if chunk.is_empty() {
        return Err(SrsV2Error::Truncated);
    }
    if chunk[0] != 1 {
        return Err(SrsV2Error::syntax(
            "FR2 rev30 P residual requires adaptive chunk wrapper (0x01)",
        ));
    }
    let body = &chunk[1..];
    let mut cur = 0usize;
    let ctx_model = ResidualContextModel::new();
    let (_m, freq) = decode_intra_block_residual_strict_context_v1(
        body,
        &mut cur,
        &ctx_model,
        plane,
        left_neighbor_nonzero,
        above_neighbor_nonzero,
    )?;
    if cur != body.len() {
        return Err(SrsV2Error::syntax("FR2 rev30 P residual trailing bytes"));
    }
    let nz_ac = freq.iter().skip(1).any(|&v| v != 0);
    let recon_freq = dequantize(&freq, qp);
    let rpix = idct_8x8(&recon_freq);
    let mut out = [[0_i16; 8]; 8];
    for r in 0..8 {
        for c in 0..8 {
            out[r][c] = rpix[r * 8 + c];
        }
    }
    Ok((out, nz_ac))
}

#[cfg(test)]
mod residual_entropy_tests {
    use super::*;
    use crate::srsv2::dct::ZIGZAG;
    use crate::srsv2::frame::VideoPlane;
    use crate::srsv2::intra_codec::{decode_plane_intra, encode_plane_intra, PredMode};
    use crate::srsv2::rate_control::{
        ResidualEncodeStats, SrsV2CoeffLayoutMode, SrsV2CoeffScanMode, SrsV2EncodeSettings,
        SrsV2ResidualContextMode, SrsV2TransformDecisionMode, SrsV2TransformGroupingMode,
    };

    fn sparse_one_ac_qfreq() -> [i16; 64] {
        let mut blk = [0_i16; 64];
        blk[0] = 5;
        blk[ZIGZAG[1]] = 1;
        blk
    }

    #[test]
    fn p_residual_compact_rev33_wire_scan_changes_bytes_not_spatial_residual() {
        let qf = sparse_one_ac_qfreq();
        let st_zz = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            coeff_scan_mode: SrsV2CoeffScanMode::ZigZag,
            ..Default::default()
        };
        let st_gl = SrsV2EncodeSettings {
            coeff_scan_mode: SrsV2CoeffScanMode::GroupedLowFirst,
            ..st_zz.clone()
        };
        let wz = encode_p_residual_chunk_compact_v33_wire(&qf, &st_zz, None).unwrap();
        let wg = encode_p_residual_chunk_compact_v33_wire(&qf, &st_gl, None).unwrap();
        assert_ne!(
            wz, wg,
            "different scans should reorder compact payload bytes"
        );
        let qp = 19_i16;
        let (az, _) = decode_p_residual_chunk_compact_v33(&wz, qp).unwrap();
        let (ag, _) = decode_p_residual_chunk_compact_v33(&wg, qp).unwrap();
        assert_eq!(az, ag);
    }

    #[test]
    fn p_residual_compact_rev35_wire_roundtrip() {
        let mut cur = [[118_i16; 8]; 8];
        cur[3][4] = 200;
        let mut spatial = [[0_i16; 8]; 8];
        spatial[3][4] = 200_i16.saturating_sub(118);
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            transform_grouping_mode: SrsV2TransformGroupingMode::Four4x4,
            transform_decision_mode: SrsV2TransformDecisionMode::ResidualAware,
            coeff_scan_mode: SrsV2CoeffScanMode::ZigZag,
            ..Default::default()
        };
        let qp = 24_i16;
        let (wire, nz0) =
            encode_p_residual_chunk_compact_v35_wire(&cur, &spatial, qp, &st, None).unwrap();
        assert_eq!(wire[0], TAG_P_RESIDUAL_TRANSFORM_GROUP_V35);
        let (dec1, nz1) = decode_p_residual_chunk_compact_v35(&wire, qp).unwrap();
        let (dec2, nz2) = decode_p_residual_chunk_compact_v35(&wire, qp).unwrap();
        assert_eq!(nz0, nz1);
        assert_eq!(nz1, nz2);
        assert_eq!(dec1, dec2);
    }

    #[test]
    fn p_residual_compact_rev35_malformed_grouping_fails() {
        let cur = [[128_i16; 8]; 8];
        let mut spatial = [[0_i16; 8]; 8];
        spatial[2][2] = 33;
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            transform_grouping_mode: SrsV2TransformGroupingMode::AutoByResidual,
            transform_decision_mode: SrsV2TransformDecisionMode::ResidualAware,
            ..Default::default()
        };
        let (mut wire, _) =
            encode_p_residual_chunk_compact_v35_wire(&cur, &spatial, 22, &st, None).unwrap();
        wire[1] = 99;
        assert!(decode_p_residual_chunk_compact_v35(&wire, 22).is_err());
    }

    #[test]
    fn p_residual_compact_rev35_malformed_scan_fails() {
        let cur = [[128_i16; 8]; 8];
        let mut spatial = [[0_i16; 8]; 8];
        spatial[2][2] = 44;
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            transform_grouping_mode: SrsV2TransformGroupingMode::Four4x4,
            transform_decision_mode: SrsV2TransformDecisionMode::ResidualAware,
            ..Default::default()
        };
        let (mut wire, _) =
            encode_p_residual_chunk_compact_v35_wire(&cur, &spatial, 22, &st, None).unwrap();
        wire[2] = 99;
        assert!(decode_p_residual_chunk_compact_v35(&wire, 22).is_err());
    }

    #[test]
    fn p_residual_compact_rev35_wrong_chunk_layout_tag_fails() {
        let cur = [[128_i16; 8]; 8];
        let mut spatial = [[0_i16; 8]; 8];
        spatial[1][1] = 40;
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            transform_grouping_mode: SrsV2TransformGroupingMode::Four4x4,
            transform_decision_mode: SrsV2TransformDecisionMode::ResidualAware,
            ..Default::default()
        };
        let (mut wire, _) =
            encode_p_residual_chunk_compact_v35_wire(&cur, &spatial, 20, &st, None).unwrap();
        wire[0] = TAG_P_RESIDUAL_COMPACT_V1;
        assert!(decode_p_residual_chunk_compact_v35(&wire, 20).is_err());
    }

    /// Dense AC=1 blocks blow up explicit tuples (63×3-byte pairs); static rANS usually wins.
    fn dense_ac_ones_qfreq() -> [i16; 64] {
        let mut f = [0_i16; 64];
        f[0] = 3;
        for &k in ZIGZAG.iter().skip(1) {
            f[k] = 1;
        }
        f
    }

    #[test]
    fn p_residual_chunk_4x4_roundtrip_explicit() {
        let mut q = [0_i16; 16];
        q[0] = 5;
        q[1] = 2;
        let enc = encode_p_residual_chunk_4x4(&q).unwrap();
        let pix = decode_p_residual_chunk_4x4(&enc, 18).unwrap();
        assert!(pix.iter().flatten().any(|&v| v != 0));
    }

    #[test]
    fn forced_rans_sparse_block_roundtrips() {
        let model = residual_token_model();
        let mut out = Vec::new();
        let qf = sparse_one_ac_qfreq();
        let k = encode_intra_block_residual(
            PredMode::Dc,
            &qf,
            ResidualEntropy::Rans,
            SrsV2ResidualContextMode::Off,
            &model,
            None,
            ResidualPlane::Y,
            false,
            false,
            false,
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(k, BlockResidualCoding::RansV1);
        let mut c = 0usize;
        let cm = ResidualContextModel::new();
        let (_m, f2) =
            decode_intra_block_residual(&out, &mut c, &cm, ResidualPlane::Y, false, false).unwrap();
        assert_eq!(f2, qf);
    }

    #[test]
    fn auto_prefers_rans_when_explicit_bulkier() {
        let model = residual_token_model();
        let f = dense_ac_ones_qfreq();
        let explicit_full = explicit_wire_len(&f);
        let Some((_, enc)) = try_rans_payload(&f, &model).unwrap() else {
            panic!("tokenize failed");
        };
        let rfull = rans_wire_len(enc.len());
        assert!(
            rfull < explicit_full,
            "expected rANS smaller than explicit for dense AC=1 block (explicit={explicit_full} rans={rfull})"
        );
        let mut out = Vec::new();
        let k = encode_intra_block_residual(
            PredMode::Dc,
            &f,
            ResidualEntropy::Auto,
            SrsV2ResidualContextMode::Off,
            &model,
            None,
            ResidualPlane::Y,
            false,
            false,
            false,
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(k, BlockResidualCoding::RansV1);
        let mut c = 0usize;
        let cm = ResidualContextModel::new();
        let (_m, f2) =
            decode_intra_block_residual(&out, &mut c, &cm, ResidualPlane::Y, false, false).unwrap();
        assert_eq!(f2, f);
    }

    #[test]
    fn auto_prefers_explicit_when_rans_larger() {
        let model = residual_token_model();
        let mut noisy = [0_i16; 64];
        noisy[0] = 1;
        for (i, slot) in noisy.iter_mut().enumerate().skip(1) {
            *slot = (((i * 7) % 15) as i16).saturating_sub(7).clamp(-127, 127);
        }
        let explicit_len = explicit_wire_len(&noisy);
        let tok = tokenize_ac(&noisy).expect("tok");
        let enc = rans_encode_tokens(&model, &tok).expect("enc");
        let rlen = rans_wire_len(enc.len());
        if rlen >= explicit_len {
            let mut out = Vec::new();
            let k = encode_intra_block_residual(
                PredMode::Dc,
                &noisy,
                ResidualEntropy::Auto,
                SrsV2ResidualContextMode::Off,
                &model,
                None,
                ResidualPlane::Y,
                false,
                false,
                false,
                None,
                &mut out,
            )
            .unwrap();
            assert_eq!(k, BlockResidualCoding::ExplicitTuples);
        }
    }

    #[test]
    fn forced_context_v1_sparse_roundtrips() {
        let model = residual_token_model();
        let ctx = ResidualContextModel::new();
        let mut out = Vec::new();
        let qf = sparse_one_ac_qfreq();
        let k = encode_intra_block_residual(
            PredMode::Dc,
            &qf,
            ResidualEntropy::Rans,
            SrsV2ResidualContextMode::ContextV1,
            &model,
            Some(&ctx),
            ResidualPlane::Y,
            false,
            false,
            false,
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(k, BlockResidualCoding::ContextRansV1);
        let mut c = 0usize;
        let dec_m = ResidualContextModel::new();
        let (_m, f2) =
            decode_intra_block_residual(&out, &mut c, &dec_m, ResidualPlane::Y, false, false)
                .unwrap();
        assert_eq!(f2, qf);
    }

    #[test]
    fn context_v1_sparse_records_residual_stats_accounting() {
        let model = residual_token_model();
        let ctx = ResidualContextModel::new();
        let mut st = ResidualEncodeStats::default();
        let mut out = Vec::new();
        let qf = sparse_one_ac_qfreq();
        let k = encode_intra_block_residual(
            PredMode::Dc,
            &qf,
            ResidualEntropy::Rans,
            SrsV2ResidualContextMode::ContextV1,
            &model,
            Some(&ctx),
            ResidualPlane::Y,
            false,
            false,
            false,
            Some(&mut st),
            &mut out,
        )
        .unwrap();
        assert_eq!(k, BlockResidualCoding::ContextRansV1);
        st.finalize_residual_context_derived();
        assert_eq!(st.residual_context_blocks, 1);
        assert!(st.residual_context_bytes > 0);
        assert!(st.residual_static_bytes_estimate > 0);
        let oversize = st.residual_context_bytes > st.residual_static_bytes_estimate;
        assert_eq!(oversize, st.residual_context_failed_blocks > 0);
    }

    #[test]
    fn context_v1_quantized_zero_ac_records_without_panic() {
        let model = residual_token_model();
        let ctx = ResidualContextModel::new();
        let mut st = ResidualEncodeStats::default();
        let mut qf = [0_i16; 64];
        qf[0] = 3;
        let mut out = Vec::new();
        let k = encode_intra_block_residual(
            PredMode::Dc,
            &qf,
            ResidualEntropy::Rans,
            SrsV2ResidualContextMode::ContextV1,
            &model,
            Some(&ctx),
            ResidualPlane::Y,
            false,
            false,
            false,
            Some(&mut st),
            &mut out,
        )
        .unwrap();
        assert_eq!(k, BlockResidualCoding::ContextRansV1);
        st.finalize_residual_context_derived();
        assert_eq!(st.residual_context_blocks, 1);
        let oversize = st.residual_context_bytes > st.residual_static_bytes_estimate;
        assert_eq!(oversize, st.residual_context_failed_blocks > 0);
    }

    #[test]
    fn intra_entropy_plane_matches_explicit_reconstruction() {
        let w = 64u32;
        let h = 64u32;
        let mut plane = VideoPlane::<u8>::try_new(w, h, w as usize).unwrap();
        for y in 0..h {
            for x in 0..w {
                plane.samples[y as usize * plane.stride + x as usize] =
                    ((x.wrapping_mul(13) ^ y.wrapping_mul(7)) & 0xff) as u8;
            }
        }
        let qp = 28_i16;
        let mut exp = Vec::new();
        encode_plane_intra(&plane, qp, &mut exp).unwrap();
        let mut ent = Vec::new();
        let settings = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Auto,
            ..Default::default()
        };
        encode_plane_intra_entropy(
            &plane,
            qp,
            &settings,
            ResidualPlane::Y,
            &mut ResidualEncodeStats::default(),
            false,
            &mut ent,
        )
        .unwrap();
        let mut cur_e = 0usize;
        let mut dec_exp = VideoPlane::<u8>::try_new(w, h, w as usize).unwrap();
        decode_plane_intra(&exp, &mut cur_e, &mut dec_exp, qp).unwrap();
        let mut cur_n = 0usize;
        let mut dec_ent = VideoPlane::<u8>::try_new(w, h, w as usize).unwrap();
        decode_plane_intra_entropy(&ent, &mut cur_n, &mut dec_ent, qp, ResidualPlane::Y, false)
            .unwrap();
        assert_eq!(dec_exp.samples, dec_ent.samples);
    }

    #[test]
    fn p_chunk_roundtrip_matches_legacy_when_explicit() {
        let model = residual_token_model();
        let mut blk = [[0_i16; 8]; 8];
        blk[3][3] = 40;
        let mut linear = [0_i16; 64];
        for r in 0..8 {
            for c in 0..8 {
                linear[r * 8 + c] = blk[r][c];
            }
        }
        let f = fdct_8x8(&linear);
        let qf = quantize(&f, 28);
        let (chunk, _) = encode_p_residual_chunk(&qf, ResidualEntropy::Explicit, &model).unwrap();
        let dec = decode_p_residual_chunk(&chunk, 28).unwrap();
        let mut cur = 1usize;
        let legacy =
            super::super::intra_codec::decode_residual_block_8x8(&chunk, &mut cur, 28).unwrap();
        assert_eq!(dec, legacy);
    }
}
