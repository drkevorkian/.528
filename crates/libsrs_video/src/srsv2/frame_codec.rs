//! Serialized SRSV2 **frame** payload (inside mux packet or elementary stream).

use super::adaptive_quant::{
    accumulate_block_aq_wire_plane, resolve_frame_adaptive_qp, SrsV2AqEncodeStats,
};
use super::b_frame_codec;
use super::block_aq::validate_qp_clip_range;
use super::deblock::apply_loop_filter_y;
use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::intra_codec::{decode_plane_intra, encode_plane_intra};
use super::limits::MAX_FRAME_PAYLOAD_BYTES;
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::motion_search::SrsV2MotionEncodeStats;
use super::p_frame_codec;
use super::rate_control::{
    ResidualEncodeStats, ResidualEntropy, SrsV2BlockAqMode, SrsV2CoeffLayoutMode,
    SrsV2EncodeSettings, SrsV2ResidualContextMode,
};
use super::reference_manager::SrsV2ReferenceManager;
use super::residual_entropy::{
    decode_plane_intra_compact_v32, decode_plane_intra_entropy,
    decode_plane_intra_entropy_block_aq, encode_plane_intra_compact_v32,
    encode_plane_intra_entropy, encode_plane_intra_entropy_block_aq, ResidualPlane,
};

pub const FRAME_PAYLOAD_MAGIC: [u8; 4] = [b'F', b'R', b'2', 1];
pub const FRAME_PAYLOAD_MAGIC_INTRA_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 3];
pub const FRAME_PAYLOAD_MAGIC_INTRA_BLOCK_AQ: [u8; 4] = [b'F', b'R', b'2', 7];
/// Intra adaptive residuals **strict** multi-context rANS (`FR2` rev **29**).
pub const FRAME_PAYLOAD_MAGIC_INTRA_RESIDUAL_CTX_V1: [u8; 4] = [b'F', b'R', b'2', 29];
/// Intra with [`SrsV2CoeffLayoutMode::CompactV1`] coefficient bodies (`FR2` rev **32**).
pub const FRAME_PAYLOAD_MAGIC_INTRA_COMPACT_V1: [u8; 4] = [b'F', b'R', b'2', 32];
/// Non-displayable reference refresh (`FR2` rev **12**, experimental).
pub const FRAME_PAYLOAD_MAGIC_ALT_REF: [u8; 4] = [b'F', b'R', b'2', 12];

fn intra_magic_matches(payload: &[u8]) -> bool {
    payload.len() >= 4
        && &payload[0..3] == b"FR2"
        && matches!(payload[3], 1 | 3 | 7 | 29 | 32)
}

fn intra_use_block_aq_wire(settings: &SrsV2EncodeSettings) -> bool {
    matches!(settings.block_aq_mode, SrsV2BlockAqMode::BlockDelta)
        && matches!(
            settings.residual_entropy,
            ResidualEntropy::Auto | ResidualEntropy::Rans
        )
}

pub fn encode_yuv420_intra_payload(
    seq: &VideoSequenceHeaderV2,
    yuv: &YuvFrame,
    frame_index: u32,
    qp: u8,
    settings: &SrsV2EncodeSettings,
    stats: Option<&mut ResidualEncodeStats>,
    aq_out: Option<&mut SrsV2AqEncodeStats>,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 || yuv.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported(
            "encode path only supports YUV420p8 in this slice",
        ));
    }
    settings.validate_residual_context_mode()?;
    settings.validate_coeff_layout_settings()?;
    if matches!(settings.coeff_layout_mode, SrsV2CoeffLayoutMode::CompactV1) {
        if matches!(settings.residual_entropy, ResidualEntropy::Explicit) {
            return Err(SrsV2Error::syntax(
                "FR2 rev32 intra CompactV1 requires residual_entropy Auto or Rans",
            ));
        }
        if intra_use_block_aq_wire(settings) {
            return Err(SrsV2Error::syntax(
                "FR2 rev32 intra CompactV1 is incompatible with block AQ (FR2 rev7)",
            ));
        }
        if matches!(
            settings.residual_context_mode,
            SrsV2ResidualContextMode::ContextV1,
        ) {
            return Err(SrsV2Error::syntax(
                "FR2 rev32 intra CompactV1 is incompatible with residual_context_mode ContextV1",
            ));
        }
    }
    if matches!(settings.block_aq_mode, SrsV2BlockAqMode::BlockDelta)
        && matches!(settings.residual_entropy, ResidualEntropy::Explicit)
    {
        return Err(SrsV2Error::syntax(
            "block AQ (FR2 rev 7) requires adaptive residual entropy (Auto or Rans)",
        ));
    }
    let (eff_qp, mut aq_st) = resolve_frame_adaptive_qp(qp, yuv, settings)?;
    let mut out = Vec::new();
    let magic = if matches!(settings.coeff_layout_mode, SrsV2CoeffLayoutMode::CompactV1) {
        FRAME_PAYLOAD_MAGIC_INTRA_COMPACT_V1
    } else if intra_use_block_aq_wire(settings) {
        FRAME_PAYLOAD_MAGIC_INTRA_BLOCK_AQ
    } else if matches!(
        settings.residual_context_mode,
        SrsV2ResidualContextMode::ContextV1,
    ) && matches!(
        settings.residual_entropy,
        ResidualEntropy::Auto | ResidualEntropy::Rans,
    ) {
        FRAME_PAYLOAD_MAGIC_INTRA_RESIDUAL_CTX_V1
    } else {
        match settings.residual_entropy {
            ResidualEntropy::Explicit => FRAME_PAYLOAD_MAGIC,
            ResidualEntropy::Auto | ResidualEntropy::Rans => FRAME_PAYLOAD_MAGIC_INTRA_ENTROPY,
        }
    };
    out.extend_from_slice(&magic);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(eff_qp);

    let clip_min = settings.min_qp;
    let clip_max = settings.max_qp;
    if intra_use_block_aq_wire(settings) {
        validate_qp_clip_range(clip_min, clip_max)?;
        out.push(clip_min);
        out.push(clip_max);
    }

    let qp_i = eff_qp.max(1) as i16;
    let mut yb = Vec::new();
    let mut ub = Vec::new();
    let mut vb = Vec::new();
    let strict_rev29 = magic == FRAME_PAYLOAD_MAGIC_INTRA_RESIDUAL_CTX_V1;

    match settings.residual_entropy {
        ResidualEntropy::Explicit => {
            encode_plane_intra(&yuv.y, qp_i, &mut yb)?;
            encode_plane_intra(&yuv.u, qp_i, &mut ub)?;
            encode_plane_intra(&yuv.v, qp_i, &mut vb)?;
        }
        ResidualEntropy::Auto | ResidualEntropy::Rans => {
            let mut noop = ResidualEncodeStats::default();
            let acc: &mut ResidualEncodeStats = stats.unwrap_or(&mut noop);
            if matches!(
                settings.residual_context_mode,
                SrsV2ResidualContextMode::ContextV1
            ) {
                acc.residual_context_enabled = true;
            }
            if magic == FRAME_PAYLOAD_MAGIC_INTRA_COMPACT_V1 {
                encode_plane_intra_compact_v32(&yuv.y, qp_i, settings, acc, &mut yb)?;
                encode_plane_intra_compact_v32(&yuv.u, qp_i, settings, acc, &mut ub)?;
                encode_plane_intra_compact_v32(&yuv.v, qp_i, settings, acc, &mut vb)?;
                acc.finalize_coeff_layout_derived();
            } else if intra_use_block_aq_wire(settings) {
                let sy = encode_plane_intra_entropy_block_aq(
                    &yuv.y,
                    eff_qp,
                    clip_min,
                    clip_max,
                    ResidualPlane::Y,
                    acc,
                    settings,
                    &mut yb,
                )?;
                let su = encode_plane_intra_entropy_block_aq(
                    &yuv.u,
                    eff_qp,
                    clip_min,
                    clip_max,
                    ResidualPlane::U,
                    acc,
                    settings,
                    &mut ub,
                )?;
                let sv = encode_plane_intra_entropy_block_aq(
                    &yuv.v,
                    eff_qp,
                    clip_min,
                    clip_max,
                    ResidualPlane::V,
                    acc,
                    settings,
                    &mut vb,
                )?;
                accumulate_block_aq_wire_plane(
                    &mut aq_st.block_wire,
                    sy.blocks,
                    sy.sum_eff_qp,
                    sy.min_eff_qp,
                    sy.max_eff_qp,
                    sy.pos_delta,
                    sy.neg_delta,
                    sy.zero_delta,
                );
                accumulate_block_aq_wire_plane(
                    &mut aq_st.block_wire,
                    su.blocks,
                    su.sum_eff_qp,
                    su.min_eff_qp,
                    su.max_eff_qp,
                    su.pos_delta,
                    su.neg_delta,
                    su.zero_delta,
                );
                accumulate_block_aq_wire_plane(
                    &mut aq_st.block_wire,
                    sv.blocks,
                    sv.sum_eff_qp,
                    sv.min_eff_qp,
                    sv.max_eff_qp,
                    sv.pos_delta,
                    sv.neg_delta,
                    sv.zero_delta,
                );
                acc.finalize_residual_context_derived();
            } else {
                encode_plane_intra_entropy(
                    &yuv.y,
                    qp_i,
                    settings,
                    ResidualPlane::Y,
                    acc,
                    strict_rev29,
                    &mut yb,
                )?;
                encode_plane_intra_entropy(
                    &yuv.u,
                    qp_i,
                    settings,
                    ResidualPlane::U,
                    acc,
                    strict_rev29,
                    &mut ub,
                )?;
                encode_plane_intra_entropy(
                    &yuv.v,
                    qp_i,
                    settings,
                    ResidualPlane::V,
                    acc,
                    strict_rev29,
                    &mut vb,
                )?;
                acc.finalize_residual_context_derived();
            }
        }
    }

    if let Some(a) = aq_out {
        *a = aq_st;
    }

    push_chunk(&mut out, &yb)?;
    push_chunk(&mut out, &ub)?;
    push_chunk(&mut out, &vb)?;

    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded frame",
        });
    }
    Ok(out)
}

/// Apply sequence-signaled **in-loop** / reconstruction luma filter exactly once (Y plane only).
///
/// Call after intra or P **reconstruction** and before display or copying into a reference slot.
pub fn apply_reconstruction_filter_if_enabled(
    seq: &VideoSequenceHeaderV2,
    frame: &mut DecodedVideoFrameV2,
) {
    apply_loop_filter_y(
        seq.loop_filter_mode(),
        seq.effective_deblock_strength_for_filter(),
        &mut frame.yuv.y,
    );
}

fn push_chunk(out: &mut Vec<u8>, chunk: &[u8]) -> Result<(), SrsV2Error> {
    let len = u32::try_from(chunk.len()).map_err(|_| SrsV2Error::syntax("chunk length"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(chunk);
    Ok(())
}

/// Decode intra payload into **reconstructed** samples **without** loop filtering.
///
/// Use [`decode_yuv420_srsv2_payload`] for mux playback (filter applied once there), or call
/// [`apply_reconstruction_filter_if_enabled`] after decode when displaying or refreshing references.
pub fn decode_yuv420_intra_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported(
            "decode path only supports YUV420p8 in this slice",
        ));
    }
    let min_hdr = match payload.get(3).copied() {
        Some(7) => 4 + 4 + 1 + 2 + 4 * 3,
        _ => 4 + 4 + 1 + 4 * 3,
    };
    if payload.len() < min_hdr {
        return Err(SrsV2Error::Truncated);
    }
    if !intra_magic_matches(payload) {
        return Err(SrsV2Error::BadMagic);
    }
    let rev = payload[3];
    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let base_qp = read_u8_intra(payload, &mut cur)?;
    let (clip_min, clip_max, qp_i) = if rev == 7 {
        let clip_min = read_u8_intra(payload, &mut cur)?;
        let clip_max = read_u8_intra(payload, &mut cur)?;
        validate_qp_clip_range(clip_min, clip_max)?;
        (clip_min, clip_max, base_qp.max(1) as i16)
    } else {
        (1_u8, 51_u8, base_qp.max(1) as i16)
    };

    let y_len = read_u32(payload, &mut cur)? as usize;
    let y_end = cur
        .checked_add(y_len)
        .ok_or(SrsV2Error::Overflow("y chunk"))?;
    if y_end > payload.len() {
        return Err(SrsV2Error::Truncated);
    }
    let y_data = &payload[cur..y_end];
    cur = y_end;

    let u_len = read_u32(payload, &mut cur)? as usize;
    let u_end = cur
        .checked_add(u_len)
        .ok_or(SrsV2Error::Overflow("u chunk"))?;
    if u_end > payload.len() {
        return Err(SrsV2Error::Truncated);
    }
    let u_data = &payload[cur..u_end];
    cur = u_end;

    let v_len = read_u32(payload, &mut cur)? as usize;
    let v_end = cur
        .checked_add(v_len)
        .ok_or(SrsV2Error::Overflow("v chunk"))?;
    if v_end > payload.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v_data = &payload[cur..v_end];
    cur = v_end;
    if cur != payload.len() {
        return Err(SrsV2Error::syntax("trailing frame bytes"));
    }

    let w = seq.width;
    let h = seq.height;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);

    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    match rev {
        1 => {
            let mut c = 0usize;
            decode_plane_intra(y_data, &mut c, &mut y_plane, qp_i)?;
            if c != y_data.len() {
                return Err(SrsV2Error::syntax("y plane trailing bits"));
            }
            c = 0;
            decode_plane_intra(u_data, &mut c, &mut u_plane, qp_i)?;
            if c != u_data.len() {
                return Err(SrsV2Error::syntax("u plane trailing bits"));
            }
            c = 0;
            decode_plane_intra(v_data, &mut c, &mut v_plane, qp_i)?;
            if c != v_data.len() {
                return Err(SrsV2Error::syntax("v plane trailing bits"));
            }
        }
        3 => {
            let mut c = 0usize;
            decode_plane_intra_entropy(
                y_data,
                &mut c,
                &mut y_plane,
                qp_i,
                ResidualPlane::Y,
                false,
            )?;
            if c != y_data.len() {
                return Err(SrsV2Error::syntax("y plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy(
                u_data,
                &mut c,
                &mut u_plane,
                qp_i,
                ResidualPlane::U,
                false,
            )?;
            if c != u_data.len() {
                return Err(SrsV2Error::syntax("u plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy(
                v_data,
                &mut c,
                &mut v_plane,
                qp_i,
                ResidualPlane::V,
                false,
            )?;
            if c != v_data.len() {
                return Err(SrsV2Error::syntax("v plane trailing bits"));
            }
        }
        29 => {
            let mut c = 0usize;
            decode_plane_intra_entropy(y_data, &mut c, &mut y_plane, qp_i, ResidualPlane::Y, true)?;
            if c != y_data.len() {
                return Err(SrsV2Error::syntax("y plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy(u_data, &mut c, &mut u_plane, qp_i, ResidualPlane::U, true)?;
            if c != u_data.len() {
                return Err(SrsV2Error::syntax("u plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy(v_data, &mut c, &mut v_plane, qp_i, ResidualPlane::V, true)?;
            if c != v_data.len() {
                return Err(SrsV2Error::syntax("v plane trailing bits"));
            }
        }
        7 => {
            let mut c = 0usize;
            decode_plane_intra_entropy_block_aq(
                y_data,
                &mut c,
                &mut y_plane,
                base_qp,
                clip_min,
                clip_max,
                ResidualPlane::Y,
            )?;
            if c != y_data.len() {
                return Err(SrsV2Error::syntax("y plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy_block_aq(
                u_data,
                &mut c,
                &mut u_plane,
                base_qp,
                clip_min,
                clip_max,
                ResidualPlane::U,
            )?;
            if c != u_data.len() {
                return Err(SrsV2Error::syntax("u plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy_block_aq(
                v_data,
                &mut c,
                &mut v_plane,
                base_qp,
                clip_min,
                clip_max,
                ResidualPlane::V,
            )?;
            if c != v_data.len() {
                return Err(SrsV2Error::syntax("v plane trailing bits"));
            }
        }
        32 => {
            let mut c = 0usize;
            decode_plane_intra_compact_v32(y_data, &mut c, &mut y_plane, qp_i)?;
            if c != y_data.len() {
                return Err(SrsV2Error::syntax("y plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_compact_v32(u_data, &mut c, &mut u_plane, qp_i)?;
            if c != u_data.len() {
                return Err(SrsV2Error::syntax("u plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_compact_v32(v_data, &mut c, &mut v_plane, qp_i)?;
            if c != v_data.len() {
                return Err(SrsV2Error::syntax("v plane trailing bits"));
            }
        }
        _ => return Err(SrsV2Error::BadMagic),
    }

    Ok(DecodedVideoFrameV2 {
        frame_index,
        width: w,
        height: h,
        is_displayable: true,
        yuv: YuvFrame {
            format: PixelFormat::Yuv420p8,
            y: y_plane,
            u: u_plane,
            v: v_plane,
        },
    })
}

/// Encode SRSV2 video payload: intra (`FR2` rev 1 or 3) or experimental P (`FR2` rev 2 or 4).
#[allow(clippy::too_many_arguments)]
pub fn encode_yuv420_inter_payload(
    seq: &VideoSequenceHeaderV2,
    cur: &YuvFrame,
    reference: Option<&YuvFrame>,
    frame_index: u32,
    qp: u8,
    settings: &SrsV2EncodeSettings,
    stats: Option<&mut ResidualEncodeStats>,
    aq_out: Option<&mut SrsV2AqEncodeStats>,
    motion_out: Option<&mut SrsV2MotionEncodeStats>,
) -> Result<Vec<u8>, SrsV2Error> {
    let interval = settings.keyframe_interval.max(1);
    let force_intra = frame_index == 0 || frame_index.is_multiple_of(interval);
    if force_intra || seq.max_ref_frames == 0 {
        return encode_yuv420_intra_payload(seq, cur, frame_index, qp, settings, stats, aq_out);
    }
    let Some(reference) = reference else {
        return encode_yuv420_intra_payload(seq, cur, frame_index, qp, settings, stats, aq_out);
    };
    if !seq.width.is_multiple_of(16) || !seq.height.is_multiple_of(16) {
        return encode_yuv420_intra_payload(seq, cur, frame_index, qp, settings, stats, aq_out);
    }
    settings.validate_residual_context_inter_residual()?;
    settings.validate_coeff_layout_settings()?;
    let (eff_qp, aq_st) = resolve_frame_adaptive_qp(qp, cur, settings)?;
    let block_wire = if let Some(a) = aq_out {
        *a = aq_st;
        Some(&mut a.block_wire)
    } else {
        None
    };
    p_frame_codec::encode_yuv420_p_payload(
        seq,
        cur,
        reference,
        frame_index,
        eff_qp,
        settings,
        stats,
        motion_out,
        block_wire,
    )
}

/// Decode **alt-ref** (`FR2` rev **12**): intra-coded planes stored into `manager` at `target_slot`; not displayable.
pub fn decode_yuv420_alt_ref_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    manager: &mut SrsV2ReferenceManager,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported(
            "decode path only supports YUV420p8 in this slice",
        ));
    }
    if seq.max_ref_frames == 0 {
        return Err(SrsV2Error::syntax("alt-ref requires max_ref_frames >= 1"));
    }
    if payload.len() < 4 + 4 + 1 + 1 + 1 + 12 {
        return Err(SrsV2Error::Truncated);
    }
    if payload.len() < 4 || payload[0..4] != FRAME_PAYLOAD_MAGIC_ALT_REF {
        return Err(SrsV2Error::BadMagic);
    }
    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let base_qp = read_u8_intra(payload, &mut cur)?;
    let target_slot = read_u8_intra(payload, &mut cur)?;
    let reserved = read_u8_intra(payload, &mut cur)?;
    if reserved != 0 {
        return Err(SrsV2Error::syntax("alt-ref header reserved"));
    }
    manager.validate_slot_index(target_slot)?;
    let qp_i = base_qp.max(1) as i16;

    let y_len = read_u32(payload, &mut cur)? as usize;
    let y_end = cur
        .checked_add(y_len)
        .ok_or(SrsV2Error::Overflow("alt-ref y chunk"))?;
    if y_end > payload.len() {
        return Err(SrsV2Error::Truncated);
    }
    let y_data = &payload[cur..y_end];
    cur = y_end;

    let u_len = read_u32(payload, &mut cur)? as usize;
    let u_end = cur
        .checked_add(u_len)
        .ok_or(SrsV2Error::Overflow("alt-ref u chunk"))?;
    if u_end > payload.len() {
        return Err(SrsV2Error::Truncated);
    }
    let u_data = &payload[cur..u_end];
    cur = u_end;

    let v_len = read_u32(payload, &mut cur)? as usize;
    let v_end = cur
        .checked_add(v_len)
        .ok_or(SrsV2Error::Overflow("alt-ref v chunk"))?;
    if v_end > payload.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v_data = &payload[cur..v_end];
    cur = v_end;
    if cur != payload.len() {
        return Err(SrsV2Error::syntax("trailing alt-ref bytes"));
    }

    let w = seq.width;
    let h = seq.height;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);

    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    let mut c = 0usize;
    decode_plane_intra_entropy(y_data, &mut c, &mut y_plane, qp_i, ResidualPlane::Y, false)?;
    if c != y_data.len() {
        return Err(SrsV2Error::syntax("alt-ref y plane trailing bits"));
    }
    c = 0;
    decode_plane_intra_entropy(u_data, &mut c, &mut u_plane, qp_i, ResidualPlane::U, false)?;
    if c != u_data.len() {
        return Err(SrsV2Error::syntax("alt-ref u plane trailing bits"));
    }
    c = 0;
    decode_plane_intra_entropy(v_data, &mut c, &mut v_plane, qp_i, ResidualPlane::V, false)?;
    if c != v_data.len() {
        return Err(SrsV2Error::syntax("alt-ref v plane trailing bits"));
    }

    let yuv = YuvFrame {
        format: PixelFormat::Yuv420p8,
        y: y_plane,
        u: u_plane,
        v: v_plane,
    };
    manager.store_alt_ref_at(target_slot, frame_index, yuv.clone())?;

    Ok(DecodedVideoFrameV2 {
        frame_index,
        width: w,
        height: h,
        is_displayable: false,
        yuv,
    })
}

/// Encode experimental **alt-ref** (`FR2` rev **12**) using rev **3**-style entropy planes.
pub fn encode_yuv420_alt_ref_payload(
    seq: &VideoSequenceHeaderV2,
    yuv: &YuvFrame,
    frame_index: u32,
    qp: u8,
    target_slot: u8,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 || yuv.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported(
            "encode path only supports YUV420p8 in this slice",
        ));
    }
    if seq.max_ref_frames == 0 {
        return Err(SrsV2Error::syntax("alt-ref requires max_ref_frames >= 1"));
    }
    let probe = SrsV2ReferenceManager::new(seq.max_ref_frames)?;
    probe.validate_slot_index(target_slot)?;
    let qp_i = qp.max(1) as i16;
    let mut out = Vec::new();
    out.extend_from_slice(&FRAME_PAYLOAD_MAGIC_ALT_REF);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(qp);
    out.push(target_slot);
    out.push(0_u8);

    let mut acc = ResidualEncodeStats::default();
    let mut yb = Vec::new();
    let mut ub = Vec::new();
    let mut vb = Vec::new();
    let intra_settings = SrsV2EncodeSettings::default();
    encode_plane_intra_entropy(
        &yuv.y,
        qp_i,
        &intra_settings,
        ResidualPlane::Y,
        &mut acc,
        false,
        &mut yb,
    )?;
    encode_plane_intra_entropy(
        &yuv.u,
        qp_i,
        &intra_settings,
        ResidualPlane::U,
        &mut acc,
        false,
        &mut ub,
    )?;
    encode_plane_intra_entropy(
        &yuv.v,
        qp_i,
        &intra_settings,
        ResidualPlane::V,
        &mut acc,
        false,
        &mut vb,
    )?;
    push_chunk(&mut out, &yb)?;
    push_chunk(&mut out, &ub)?;
    push_chunk(&mut out, &vb)?;
    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded alt-ref frame",
        });
    }
    Ok(out)
}

/// Multi-reference decode entry point for mux / playback (`FR2` rev **1**–**14** plus experimental **P** **15**/**17**/**23**/**25**/**27**/**28** and **B** **24** where wired).
///
/// Updates `manager` for intra, **P**, and **alt-ref**; **B** frames (**10**/**11**/**13**/**14**) do not advance the last-displayed slot.
pub fn decode_yuv420_srsv2_payload_managed(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    manager: &mut SrsV2ReferenceManager,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if payload.len() < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let rev = payload[3];
    let mut dec = match rev {
        1 | 3 | 7 | 29 | 32 => {
            let d = decode_yuv420_intra_payload(seq, payload)?;
            if seq.max_ref_frames > 0 {
                manager.replace_after_keyframe(d.frame_index, d.yuv.clone());
            }
            d
        }
        2 | 4 | 5 | 6 | 8 | 9 | 15 | 17 | 19 | 20 | 23 | 25 | 27 | 28 | 30 | 33 => {
            let reference = manager
                .primary_ref()
                .ok_or(SrsV2Error::PFrameWithoutReference)?;
            let d = p_frame_codec::decode_yuv420_p_payload(seq, payload, reference)?;
            if seq.max_ref_frames > 0 {
                manager.push_displayable_last(d.frame_index, d.yuv.clone());
            }
            d
        }
        10 | 11 | 13 | 14 | 16 | 18 | 21 | 22 | 24 | 31 => {
            if seq.max_ref_frames < 2 {
                return Err(SrsV2Error::syntax(
                    "B-frame requires max_ref_frames >= 2 in sequence header",
                ));
            }
            b_frame_codec::decode_yuv420_b_payload(seq, payload, manager)?
        }
        12 => decode_yuv420_alt_ref_payload(seq, payload, manager)?,
        _ => {
            return Err(SrsV2Error::Unsupported(
                "unknown SRSV2 frame payload revision",
            ));
        }
    };
    apply_reconstruction_filter_if_enabled(seq, &mut dec);
    Ok(dec)
}

/// Decode intra or P SRSV2 payload; updates `ref_slot` when `max_ref_frames > 0` after a successful decode.
///
/// **`FR2` revision 10–14** (`B` / **alt-ref**) require [`decode_yuv420_srsv2_payload_managed`].
pub fn decode_yuv420_srsv2_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    ref_slot: &mut Option<YuvFrame>,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if payload.len() < 4 {
        return Err(SrsV2Error::Truncated);
    }
    if matches!(payload[3], 10 | 11 | 13 | 14 | 16 | 18 | 21 | 22 | 24 | 31) {
        return Err(SrsV2Error::Unsupported(
            "multi-reference SRSV2 payloads require decode_yuv420_srsv2_payload_managed",
        ));
    }
    let mut dec = match payload[3] {
        1 | 3 | 7 | 29 | 32 => decode_yuv420_intra_payload(seq, payload)?,
        2 | 4 | 5 | 6 | 8 | 9 | 15 | 17 | 19 | 20 | 23 | 25 | 27 | 28 | 30 | 33 => {
            let reference = ref_slot
                .as_ref()
                .ok_or(SrsV2Error::PFrameWithoutReference)?;
            p_frame_codec::decode_yuv420_p_payload(seq, payload, reference)?
        }
        _ => {
            return Err(SrsV2Error::Unsupported(
                "unknown SRSV2 frame payload revision",
            ));
        }
    };
    apply_reconstruction_filter_if_enabled(seq, &mut dec);
    if seq.max_ref_frames > 0 {
        ref_slot.replace(dec.yuv.clone());
    }
    Ok(dec)
}

fn read_u8_intra(data: &[u8], cur: &mut usize) -> Result<u8, SrsV2Error> {
    if *cur >= data.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v = data[*cur];
    *cur += 1;
    Ok(v)
}

fn read_u32(data: &[u8], cur: &mut usize) -> Result<u32, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let v = u32::from_le_bytes([data[*cur], data[*cur + 1], data[*cur + 2], data[*cur + 3]]);
    *cur += 4;
    Ok(v)
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    use crate::srsv2::adaptive_quant::SrsV2AqEncodeStats;
    use crate::srsv2::color::rgb888_full_to_yuv420_bt709;
    use crate::srsv2::model::{
        ChromaSiting, ColorPrimaries, ColorRange, MatrixCoefficients, PixelFormat, SrsVideoProfile,
        TransferFunction, VideoSequenceHeaderV2,
    };
    use crate::srsv2::rate_control::{
        ResidualEncodeStats, ResidualEntropy, SrsV2AdaptiveQuantizationMode, SrsV2BlockAqMode,
        SrsV2CoeffLayoutMode, SrsV2CoeffScanMode, SrsV2EncodeSettings, SrsV2EntropyModelMode,
        SrsV2InterSyntaxMode, SrsV2ResidualContextMode, SrsV2TransformDecisionMode,
    };
    use crate::srsv2::reference_manager::SrsV2ReferenceManager;

    fn explicit_only_settings() -> SrsV2EncodeSettings {
        SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Explicit,
            ..Default::default()
        }
    }

    #[test]
    fn srsv2_dispatcher_p_requires_reference_then_decodes() {
        let w = 64u32;
        let h = 64u32;
        let mut seq = VideoSequenceHeaderV2 {
            width: w,
            height: h,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let rgb = vec![128_u8; (w * h * 3) as usize];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, w, h, ColorRange::Limited).unwrap();
        let st = explicit_only_settings();
        let pbytes =
            encode_yuv420_inter_payload(&seq, &yuv, Some(&yuv), 1, 28, &st, None, None, None)
                .unwrap();
        let mut slot = None::<crate::srsv2::frame::YuvFrame>;
        assert!(matches!(
            decode_yuv420_srsv2_payload(&seq, &pbytes, &mut slot),
            Err(crate::srsv2::error::SrsV2Error::PFrameWithoutReference)
        ));
        slot = Some(yuv.clone());
        decode_yuv420_srsv2_payload(&seq, &pbytes, &mut slot).unwrap();
        seq.max_ref_frames = 0;
        let intra_only =
            encode_yuv420_inter_payload(&seq, &yuv, None, 5, 28, &st, None, None, None).unwrap();
        assert_eq!(intra_only[3], 1);
    }

    #[test]
    fn yuv420_intra_payload_encode_decode_roundtrip() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).expect("yuv");
        let qp = 10_u8;
        let st = explicit_only_settings();
        let payload = encode_yuv420_intra_payload(&seq, &yuv, 1, qp, &st, None, None).expect("enc");
        let dec = decode_yuv420_intra_payload(&seq, &payload).expect("dec");
        assert_eq!(dec.frame_index, 1);
        assert_eq!(dec.width, 64);
        assert_eq!(dec.height, 64);
        assert_eq!(dec.yuv.y.samples.len(), yuv.y.samples.len());
    }

    /// Legacy **`FR2` rev 1** explicit intra still decodes after coeff-layout settings exist (defaults unchanged).
    #[test]
    fn fr2_rev1_explicit_intra_decodes_with_default_coeff_layout_fields() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).expect("yuv");
        let st = explicit_only_settings();
        assert_eq!(st.coeff_layout_mode, SrsV2CoeffLayoutMode::Legacy);
        let payload = encode_yuv420_intra_payload(&seq, &yuv, 7, 22, &st, None, None).expect("enc");
        assert_eq!(payload[3], 1, "explicit residual uses FR2 revision byte 1");
        decode_yuv420_intra_payload(&seq, &payload).expect("dec");
    }

    #[test]
    fn intra_encode_rejects_transform_decision_tx16x16() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).expect("yuv");
        let st = SrsV2EncodeSettings {
            transform_decision_mode: SrsV2TransformDecisionMode::Tx16x16,
            residual_entropy: ResidualEntropy::Explicit,
            ..Default::default()
        };
        assert!(encode_yuv420_intra_payload(&seq, &yuv, 0, 22, &st, None, None).is_err());
    }

    #[test]
    fn intra_decode_deblock_changes_y_when_loop_filter_disabled() {
        let seq_off = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let mut rgb = vec![30_u8; 64 * 64 * 3];
        for y in 0..64usize {
            for x in 0..64usize {
                let v = if (x / 16 + y / 16) % 2 == 0 {
                    240_u8
                } else {
                    40_u8
                };
                let i = (y * 64 + x) * 3;
                rgb[i] = v;
                rgb[i + 1] = v;
                rgb[i + 2] = v;
            }
        }
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Explicit,
            ..Default::default()
        };
        let payload = encode_yuv420_intra_payload(&seq_off, &yuv, 0, 22, &st, None, None).unwrap();
        let mut dec_off = decode_yuv420_intra_payload(&seq_off, &payload).unwrap();
        apply_reconstruction_filter_if_enabled(&seq_off, &mut dec_off);
        let mut seq_on = seq_off.clone();
        seq_on.disable_loop_filter = false;
        let mut dec_on = decode_yuv420_intra_payload(&seq_on, &payload).unwrap();
        apply_reconstruction_filter_if_enabled(&seq_on, &mut dec_on);
        let mut diff = 0usize;
        for i in 0..dec_off.yuv.y.samples.len() {
            if dec_off.yuv.y.samples[i] != dec_on.yuv.y.samples[i] {
                diff += 1;
            }
        }
        assert!(diff > 0, "deblocking should alter some luma samples");
    }

    #[test]
    fn identical_frames_p_payload_smaller_than_intra() {
        let w = 64u32;
        let h = 64u32;
        let seq = VideoSequenceHeaderV2 {
            width: w,
            height: h,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let rgb = vec![200_u8; (w * h * 3) as usize];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, w, h, ColorRange::Limited).unwrap();
        let qp = 28_u8;
        let st = explicit_only_settings();
        let intra = encode_yuv420_intra_payload(&seq, &yuv, 0, qp, &st, None, None).unwrap();
        let mut slot = None;
        decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        let p =
            encode_yuv420_inter_payload(&seq, &yuv, slot.as_ref(), 1, qp, &st, None, None, None)
                .unwrap();
        assert_eq!(p[3], 2);
        assert!(
            p.len() < intra.len(),
            "expected P payload smaller than intra for identical texture (p={} intra={})",
            p.len(),
            intra.len()
        );
    }

    #[test]
    fn intra_entropy_matches_explicit_decode() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![90_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st_exp = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Explicit,
            ..Default::default()
        };
        let payload_exp =
            encode_yuv420_intra_payload(&seq, &yuv, 0, 22, &st_exp, None, None).unwrap();
        assert_eq!(payload_exp[3], 1);
        let st_auto = SrsV2EncodeSettings::default();
        let payload_auto =
            encode_yuv420_intra_payload(&seq, &yuv, 0, 22, &st_auto, None, None).unwrap();
        assert_eq!(payload_auto[3], 3);
        let dec_exp = decode_yuv420_intra_payload(&seq, &payload_exp).unwrap();
        let dec_auto = decode_yuv420_intra_payload(&seq, &payload_auto).unwrap();
        assert_eq!(dec_exp.yuv.y.samples, dec_auto.yuv.y.samples);
        assert_eq!(dec_exp.yuv.u.samples, dec_auto.yuv.u.samples);
        assert_eq!(dec_exp.yuv.v.samples, dec_auto.yuv.v.samples);
    }

    #[test]
    fn aq_activity_qp_byte_matches_effective_qp_stats() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let mut yuv =
            rgb888_full_to_yuv420_bt709(&vec![128_u8; 64 * 64 * 3], 64, 64, ColorRange::Limited)
                .unwrap();
        for y in 0..64usize {
            for x in 32..64usize {
                let v = if (x / 4 + y / 4) % 2 == 0 {
                    60_u8
                } else {
                    200_u8
                };
                yuv.y.samples[y * 64 + x] = v;
            }
        }
        let st = SrsV2EncodeSettings {
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Activity,
            aq_strength: 12,
            min_qp: 10,
            max_qp: 45,
            min_block_qp_delta: -6,
            max_block_qp_delta: 8,
            residual_entropy: ResidualEntropy::Explicit,
            ..Default::default()
        };
        let mut aq = SrsV2AqEncodeStats::default();
        let payload =
            encode_yuv420_intra_payload(&seq, &yuv, 0, 22, &st, None, Some(&mut aq)).unwrap();
        assert!(aq.aq_enabled);
        assert_eq!(payload[8], aq.effective_qp);
        assert!(
            aq.positive_qp_delta_blocks > 0 || aq.negative_qp_delta_blocks > 0,
            "encode path should record AQ deltas on mixed-detail Y"
        );
    }

    #[test]
    fn p_decode_chain_is_deterministic_with_loop_filter_enabled() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: false,
            deblock_strength: 41,
            max_ref_frames: 1,
        };
        let rgb = vec![140_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let mut rgb2 = rgb.clone();
        for y in 16..48usize {
            for x in 16..48usize {
                let i = (y * 64 + x) * 3;
                rgb2[i] = 40;
                rgb2[i + 1] = 40;
                rgb2[i + 2] = 40;
            }
        }
        let yuv2 = rgb888_full_to_yuv420_bt709(&rgb2, 64, 64, ColorRange::Limited).unwrap();
        let st = explicit_only_settings();
        let intra = encode_yuv420_intra_payload(&seq, &yuv, 0, 26, &st, None, None).unwrap();
        let mut slot = None;
        decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        let p =
            encode_yuv420_inter_payload(&seq, &yuv2, slot.as_ref(), 1, 26, &st, None, None, None)
                .unwrap();

        fn decode_twice(seq: &VideoSequenceHeaderV2, intra: &[u8], p: &[u8]) -> (Vec<u8>, Vec<u8>) {
            let mut slot = None;
            let d0 = decode_yuv420_srsv2_payload(seq, intra, &mut slot).unwrap();
            let d1 = decode_yuv420_srsv2_payload(seq, p, &mut slot).unwrap();
            (d0.yuv.y.samples.clone(), d1.yuv.y.samples.clone())
        }

        let (a0, a1) = decode_twice(&seq, &intra, &p);
        let (b0, b1) = decode_twice(&seq, &intra, &p);
        assert_eq!(a0, b0);
        assert_eq!(a1, b1);
    }

    #[test]
    fn dispatcher_matches_raw_intra_plus_single_reconstruction_filter() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: false,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = explicit_only_settings();
        let payload = encode_yuv420_intra_payload(&seq, &yuv, 2, 22, &st, None, None).unwrap();
        let via_dispatcher = decode_yuv420_srsv2_payload(&seq, &payload, &mut None).unwrap();
        let mut via_raw = decode_yuv420_intra_payload(&seq, &payload).unwrap();
        apply_reconstruction_filter_if_enabled(&seq, &mut via_raw);
        assert_eq!(
            via_dispatcher.yuv.y.samples, via_raw.yuv.y.samples,
            "dispatcher must apply reconstruction filter exactly once"
        );
        assert_eq!(via_dispatcher.frame_index, via_raw.frame_index);
    }

    #[test]
    fn dispatcher_intra_twice_from_clean_state_is_identical() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: false,
            deblock_strength: 41,
            max_ref_frames: 1,
        };
        let rgb = vec![90_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = explicit_only_settings();
        let intra = encode_yuv420_intra_payload(&seq, &yuv, 0, 24, &st, None, None).unwrap();
        let mut slot_a = None;
        let mut slot_b = None;
        let da = decode_yuv420_srsv2_payload(&seq, &intra, &mut slot_a).unwrap();
        let db = decode_yuv420_srsv2_payload(&seq, &intra, &mut slot_b).unwrap();
        assert_eq!(da.yuv.y.samples, db.yuv.y.samples);
    }

    #[test]
    fn reference_slot_y_matches_decoded_after_filtered_intra() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: false,
            deblock_strength: 22,
            max_ref_frames: 1,
        };
        let rgb = vec![111_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = explicit_only_settings();
        let intra = encode_yuv420_intra_payload(&seq, &yuv, 0, 21, &st, None, None).unwrap();
        let mut slot = None;
        let dec = decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        let got = slot.expect("ref slot");
        assert_eq!(got.y.samples, dec.yuv.y.samples);
    }

    #[test]
    fn intra_residual_context_v1_matches_static_rans_reconstruction() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![77_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st_ctx = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
            keyframe_interval: 1,
            ..Default::default()
        };
        let st_ref = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::Off,
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
            keyframe_interval: 1,
            ..Default::default()
        };
        let payload_ctx =
            encode_yuv420_intra_payload(&seq, &yuv, 0, 21, &st_ctx, None, None).unwrap();
        let payload_ref =
            encode_yuv420_intra_payload(&seq, &yuv, 0, 21, &st_ref, None, None).unwrap();
        assert_eq!(payload_ctx[3], 29);
        assert_eq!(payload_ref[3], 3);
        assert_ne!(
            payload_ctx, payload_ref,
            "expected ContextV1 to emit a distinct bitstream vs static rANS"
        );
        let dec_ctx = decode_yuv420_intra_payload(&seq, &payload_ctx).unwrap();
        let dec_ref = decode_yuv420_intra_payload(&seq, &payload_ref).unwrap();
        assert_eq!(dec_ctx.yuv.y.samples, dec_ref.yuv.y.samples);
        assert_eq!(dec_ctx.yuv.u.samples, dec_ref.yuv.u.samples);
        assert_eq!(dec_ctx.yuv.v.samples, dec_ref.yuv.v.samples);
    }

    #[test]
    fn intra_context_v1_populates_residual_encode_stats() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![77_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st_ctx = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
            keyframe_interval: 1,
            ..Default::default()
        };
        let mut stats = ResidualEncodeStats::default();
        let _payload =
            encode_yuv420_intra_payload(&seq, &yuv, 0, 21, &st_ctx, Some(&mut stats), None)
                .unwrap();
        assert!(stats.residual_context_enabled);
        assert!(stats.residual_context_blocks > 0);
        assert!(stats.residual_context_bytes > 0);
        assert!(stats.residual_static_bytes_estimate > 0);
        assert!(stats.residual_context_savings_percent >= 0.0);
    }

    #[test]
    fn fr2_rev30_p_populates_residual_context_stats_when_enabled() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            inter_syntax_mode: SrsV2InterSyntaxMode::EntropyV1,
            entropy_model_mode: SrsV2EntropyModelMode::ContextV1,
            keyframe_interval: 30,
            ..Default::default()
        };
        let rgb = vec![101_u8; 64 * 64 * 3];
        let yuv_ref = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let mut rgb_cur = rgb.clone();
        for y in 16..48usize {
            for x in 16..48usize {
                let i = (y * 64 + x) * 3;
                rgb_cur[i] = 40;
                rgb_cur[i + 1] = 40;
                rgb_cur[i + 2] = 40;
            }
        }
        let yuv_cur = rgb888_full_to_yuv420_bt709(&rgb_cur, 64, 64, ColorRange::Limited).unwrap();
        let slot = Some(yuv_ref.clone());
        let mut stats = ResidualEncodeStats::default();
        let p = encode_yuv420_inter_payload(
            &seq,
            &yuv_cur,
            slot.as_ref(),
            1,
            26,
            &st,
            Some(&mut stats),
            None,
            None,
        )
        .unwrap();
        assert_eq!(p[3], 30);
        assert!(stats.residual_context_enabled);
        assert!(
            stats.residual_context_blocks > 0,
            "expected non-skipped luma subblocks to emit context residuals"
        );
    }

    #[test]
    fn b_mb_blend_encode_rejects_residual_context_v1() {
        use crate::srsv2::b_frame_codec::{encode_yuv420_b_payload_mb_blend, BFrameEncodeStats};
        let seq = VideoSequenceHeaderV2 {
            width: 16,
            height: 16,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 2,
        };
        let rgb_a = vec![80_u8; 16 * 16 * 3];
        let yuv_a = rgb888_full_to_yuv420_bt709(&rgb_a, 16, 16, ColorRange::Limited).unwrap();
        let rgb_b = vec![120_u8; 16 * 16 * 3];
        let yuv_b = rgb888_full_to_yuv420_bt709(&rgb_b, 16, 16, ColorRange::Limited).unwrap();
        let rgb_c = vec![100_u8; 16 * 16 * 3];
        let yuv_c = rgb888_full_to_yuv420_bt709(&rgb_c, 16, 16, ColorRange::Limited).unwrap();
        let settings = SrsV2EncodeSettings {
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            residual_entropy: ResidualEntropy::Rans,
            b_motion_search_mode: crate::srsv2::rate_control::SrsV2BMotionSearchMode::Off,
            ..Default::default()
        };
        let mut st = BFrameEncodeStats::default();
        let err = encode_yuv420_b_payload_mb_blend(
            &seq, &yuv_c, &yuv_a, &yuv_b, 5, 28, 1, 0, &settings, &mut st,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            crate::srsv2::error::SrsV2Error::Unsupported(_)
        ));
    }

    #[test]
    fn fr2_rev29_intra_roundtrip_managed_matches_direct_decode() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let rgb = vec![88_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
            keyframe_interval: 1,
            ..Default::default()
        };
        let payload = encode_yuv420_intra_payload(&seq, &yuv, 2, 22, &st, None, None).unwrap();
        assert_eq!(payload[3], 29);
        let dec_intra = decode_yuv420_intra_payload(&seq, &payload).unwrap();
        assert_eq!(dec_intra.frame_index, 2);
        let mut mgr = SrsV2ReferenceManager::new(1).unwrap();
        let dec_mgr = decode_yuv420_srsv2_payload_managed(&seq, &payload, &mut mgr).unwrap();
        assert_eq!(dec_intra.yuv.y.samples, dec_mgr.yuv.y.samples);
    }

    #[test]
    fn fr2_rev29_malformed_truncated_payload_fails() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![90_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
            keyframe_interval: 1,
            ..Default::default()
        };
        let payload = encode_yuv420_intra_payload(&seq, &yuv, 0, 20, &st, None, None).unwrap();
        assert_eq!(payload[3], 29);
        let mut bad = payload.clone();
        bad.truncate(bad.len().saturating_sub(12));
        assert!(decode_yuv420_intra_payload(&seq, &bad).is_err());
    }

    #[test]
    fn fr2_rev30_p_roundtrip_dispatcher_decodes() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            inter_syntax_mode: SrsV2InterSyntaxMode::EntropyV1,
            entropy_model_mode: SrsV2EntropyModelMode::ContextV1,
            keyframe_interval: 30,
            ..Default::default()
        };
        let rgb = vec![101_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let mut slot = Some(yuv.clone());
        let p =
            encode_yuv420_inter_payload(&seq, &yuv, slot.as_ref(), 1, 26, &st, None, None, None)
                .unwrap();
        assert_eq!(p[3], 30);
        decode_yuv420_srsv2_payload(&seq, &p, &mut slot).unwrap();
    }

    #[test]
    fn fr2_rev30_malformed_truncated_payload_fails() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Rans,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            inter_syntax_mode: SrsV2InterSyntaxMode::EntropyV1,
            entropy_model_mode: SrsV2EntropyModelMode::ContextV1,
            keyframe_interval: 30,
            ..Default::default()
        };
        let rgb = vec![102_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let mut slot = Some(yuv.clone());
        let p =
            encode_yuv420_inter_payload(&seq, &yuv, slot.as_ref(), 1, 26, &st, None, None, None)
                .unwrap();
        assert_eq!(p[3], 30);
        let mut bad = p.clone();
        bad.truncate(bad.len().saturating_sub(20));
        assert!(decode_yuv420_srsv2_payload(&seq, &bad, &mut slot).is_err());
    }

    #[test]
    fn fr2_rev31_b_reserved_managed_decode_unsupported() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 2,
        };
        let payload = vec![b'F', b'R', b'2', 31, 0, 0, 0, 0, 28, 0, 1];
        let mut mgr = SrsV2ReferenceManager::new(2).unwrap();
        let err = decode_yuv420_srsv2_payload_managed(&seq, &payload, &mut mgr).unwrap_err();
        assert!(matches!(
            err,
            crate::srsv2::error::SrsV2Error::Unsupported(_)
        ));
    }

    #[test]
    fn inter_encode_rejects_residual_context_v1_until_wired() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let rgb = vec![80_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let mut slot = None;
        let intra = encode_yuv420_intra_payload(
            &seq,
            &yuv,
            0,
            24,
            &SrsV2EncodeSettings::default(),
            None,
            None,
        )
        .unwrap();
        decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        let st_bad = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Auto,
            residual_context_mode: SrsV2ResidualContextMode::ContextV1,
            keyframe_interval: 30,
            ..Default::default()
        };
        assert!(encode_yuv420_inter_payload(
            &seq,
            &yuv,
            slot.as_ref(),
            1,
            24,
            &st_bad,
            None,
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn intra_rev7_block_aq_roundtrips() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![90_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Auto,
            block_aq_mode: SrsV2BlockAqMode::BlockDelta,
            keyframe_interval: 1,
            ..Default::default()
        };
        let mut aq = SrsV2AqEncodeStats::default();
        let payload =
            encode_yuv420_intra_payload(&seq, &yuv, 3, 24, &st, None, Some(&mut aq)).unwrap();
        assert_eq!(payload[3], 7);
        assert!(aq.block_wire.block_aq_enabled);
        let dec = decode_yuv420_intra_payload(&seq, &payload).unwrap();
        assert_eq!(dec.frame_index, 3);
    }

    #[test]
    fn p_rev8_block_aq_roundtrips() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let rgb = vec![120_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Auto,
            block_aq_mode: SrsV2BlockAqMode::BlockDelta,
            keyframe_interval: 30,
            ..Default::default()
        };
        let intra = encode_yuv420_intra_payload(&seq, &yuv, 0, 26, &st, None, None).unwrap();
        let mut slot = None;
        decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        let p =
            encode_yuv420_inter_payload(&seq, &yuv, slot.as_ref(), 1, 26, &st, None, None, None)
                .unwrap();
        assert_eq!(p[3], 8);
        slot = None;
        decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        decode_yuv420_srsv2_payload(&seq, &p, &mut slot).unwrap();
    }

    #[test]
    fn p_rev9_block_aq_half_pel_roundtrips() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 1,
        };
        let rgb = vec![130_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Auto,
            block_aq_mode: SrsV2BlockAqMode::BlockDelta,
            subpel_mode: crate::srsv2::rate_control::SrsV2SubpelMode::HalfPel,
            keyframe_interval: 30,
            ..Default::default()
        };
        let intra = encode_yuv420_intra_payload(&seq, &yuv, 0, 26, &st, None, None).unwrap();
        let mut slot = None;
        decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        let p =
            encode_yuv420_inter_payload(&seq, &yuv, slot.as_ref(), 1, 26, &st, None, None, None)
                .unwrap();
        assert_eq!(p[3], 9);
        slot = None;
        decode_yuv420_srsv2_payload(&seq, &intra, &mut slot).unwrap();
        decode_yuv420_srsv2_payload(&seq, &p, &mut slot).unwrap();
    }

    #[test]
    fn rev7_decode_rejects_wire_qp_delta_out_of_range() {
        let seq = VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        };
        let rgb = vec![90_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            residual_entropy: ResidualEntropy::Auto,
            block_aq_mode: SrsV2BlockAqMode::BlockDelta,
            keyframe_interval: 1,
            ..Default::default()
        };
        let mut payload = encode_yuv420_intra_payload(&seq, &yuv, 0, 24, &st, None, None).unwrap();
        assert_eq!(payload[3], 7);
        let y_off = 4 + 4 + 1 + 2 + 4;
        let qp_delta_off = y_off + 1 + 2 + 1;
        payload[qp_delta_off] = 25_u8;
        assert!(decode_yuv420_intra_payload(&seq, &payload).is_err());
    }

    fn seq64_intra() -> VideoSequenceHeaderV2 {
        VideoSequenceHeaderV2 {
            width: 64,
            height: 64,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 0,
        }
    }

    #[test]
    fn fr2_rev32_intra_roundtrip_and_stats() {
        let seq = seq64_intra();
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            coeff_scan_mode: SrsV2CoeffScanMode::ZigZag,
            ..Default::default()
        };
        let mut stats = ResidualEncodeStats::default();
        let payload =
            encode_yuv420_intra_payload(&seq, &yuv, 3, 20, &st, Some(&mut stats), None).unwrap();
        assert_eq!(payload[3], 32);
        assert_eq!(
            stats.intra_coeff_layout_mode,
            Some(SrsV2CoeffLayoutMode::CompactV1)
        );
        assert_eq!(stats.intra_coeff_scan_mode, Some(SrsV2CoeffScanMode::ZigZag));
        assert!(stats.intra_tx8x8_blocks > 0);
        assert!(stats.coeff_layout_bytes > 0);
        assert!(stats.coeff_legacy_estimated_bytes > 0);
        let dec = decode_yuv420_intra_payload(&seq, &payload).unwrap();
        assert_eq!(dec.frame_index, 3);
        assert_eq!(dec.yuv.y.samples.len(), yuv.y.samples.len());
    }

    #[test]
    fn fr2_rev32_malformed_compact_body_fails_decode() {
        let seq = seq64_intra();
        let mut rgb = vec![0_u8; 64 * 64 * 3];
        for y in 0..64usize {
            for x in 0..64usize {
                let i = (y * 64 + x) * 3;
                let v = ((x.wrapping_mul(17)) ^ (y.wrapping_mul(31))) as u8;
                rgb[i] = v;
                rgb[i + 1] = v;
                rgb[i + 2] = v;
            }
        }
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            ..Default::default()
        };
        let mut payload =
            encode_yuv420_intra_payload(&seq, &yuv, 0, 18, &st, None, None).unwrap();
        assert_eq!(payload[3], 32);
        let corrupt_at = payload.len().saturating_sub(20).max(12);
        payload[corrupt_at] ^= 0xFF;
        assert!(decode_yuv420_intra_payload(&seq, &payload).is_err());
    }

    #[test]
    fn fr2_rev32_decoded_pixels_match_legacy_auto_intra_path() {
        let seq = seq64_intra();
        let mut rgb = vec![90_u8; 64 * 64 * 3];
        for y in 0..64usize {
            for x in 0..64usize {
                let i = (y * 64 + x) * 3;
                let v = (30 + (x + y) % 40) as u8;
                rgb[i] = v;
                rgb[i + 1] = v;
                rgb[i + 2] = v;
            }
        }
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let qp = 21_u8;
        let st_legacy = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::Legacy,
            ..Default::default()
        };
        let st_compact = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            coeff_scan_mode: SrsV2CoeffScanMode::ZigZag,
            ..Default::default()
        };
        let pl_legacy =
            encode_yuv420_intra_payload(&seq, &yuv, 0, qp, &st_legacy, None, None).unwrap();
        assert_eq!(pl_legacy[3], 3);
        let pl_compact =
            encode_yuv420_intra_payload(&seq, &yuv, 0, qp, &st_compact, None, None).unwrap();
        assert_eq!(pl_compact[3], 32);
        let dec_l = decode_yuv420_intra_payload(&seq, &pl_legacy).unwrap();
        let dec_c = decode_yuv420_intra_payload(&seq, &pl_compact).unwrap();
        assert_eq!(dec_l.yuv.y.samples, dec_c.yuv.y.samples);
        assert_eq!(dec_l.yuv.u.samples, dec_c.yuv.u.samples);
        assert_eq!(dec_l.yuv.v.samples, dec_c.yuv.v.samples);
    }

    #[test]
    fn fr2_intra_revisions_1_3_7_29_still_decode_via_dispatcher() {
        let seq = seq64_intra();
        let rgb = vec![110_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let qp = 23_u8;
        let payloads = [
            encode_yuv420_intra_payload(
                &seq,
                &yuv,
                1,
                qp,
                &explicit_only_settings(),
                None,
                None,
            )
            .unwrap(),
            encode_yuv420_intra_payload(&seq, &yuv, 2, qp, &SrsV2EncodeSettings::default(), None, None)
                .unwrap(),
            encode_yuv420_intra_payload(
                &seq,
                &yuv,
                3,
                qp,
                &SrsV2EncodeSettings {
                    block_aq_mode: SrsV2BlockAqMode::BlockDelta,
                    ..Default::default()
                },
                None,
                None,
            )
            .unwrap(),
            encode_yuv420_intra_payload(
                &seq,
                &yuv,
                4,
                qp,
                &SrsV2EncodeSettings {
                    residual_context_mode: SrsV2ResidualContextMode::ContextV1,
                    ..Default::default()
                },
                None,
                None,
            )
            .unwrap(),
        ];
        assert_eq!(payloads[0][3], 1);
        assert_eq!(payloads[1][3], 3);
        assert_eq!(payloads[2][3], 7);
        assert_eq!(payloads[3][3], 29);
        let mut slot = None::<YuvFrame>;
        for pl in &payloads {
            decode_yuv420_srsv2_payload(&seq, pl, &mut slot).unwrap();
        }
    }

    #[test]
    fn fr2_rev32_flat_frame_compact_wire_not_above_legacy_estimate() {
        let seq = seq64_intra();
        let rgb = vec![128_u8; 64 * 64 * 3];
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            coeff_scan_mode: SrsV2CoeffScanMode::ZigZag,
            ..Default::default()
        };
        let mut stats = ResidualEncodeStats::default();
        encode_yuv420_intra_payload(&seq, &yuv, 0, 26, &st, Some(&mut stats), None).unwrap();
        assert!(
            stats.coeff_layout_bytes <= stats.coeff_legacy_estimated_bytes,
            "flat gray: compact wire ({}) should not exceed legacy estimate ({})",
            stats.coeff_layout_bytes,
            stats.coeff_legacy_estimated_bytes
        );
        assert!(stats.coeff_layout_savings_bytes >= 0);
    }

    #[test]
    fn fr2_rev32_dense_noise_can_exceed_legacy_estimate_and_reports_negative_savings() {
        let seq = seq64_intra();
        let mut rgb = vec![0_u8; 64 * 64 * 3];
        for y in 0..64usize {
            for x in 0..64usize {
                let i = (y * 64 + x) * 3;
                let v = ((x.wrapping_mul(131)) ^ (y.wrapping_mul(97))) as u8;
                rgb[i] = v;
                rgb[i + 1] = v;
                rgb[i + 2] = v;
            }
        }
        let yuv = rgb888_full_to_yuv420_bt709(&rgb, 64, 64, ColorRange::Limited).unwrap();
        let st = SrsV2EncodeSettings {
            coeff_layout_mode: SrsV2CoeffLayoutMode::CompactV1,
            coeff_scan_mode: SrsV2CoeffScanMode::ZigZag,
            ..Default::default()
        };
        let mut stats = ResidualEncodeStats::default();
        encode_yuv420_intra_payload(&seq, &yuv, 0, 14, &st, Some(&mut stats), None).unwrap();
        assert!(
            stats.coeff_layout_bytes > stats.coeff_legacy_estimated_bytes,
            "expected noisy pattern to inflate compact wire vs legacy estimate (layout {} est {})",
            stats.coeff_layout_bytes,
            stats.coeff_legacy_estimated_bytes
        );
        assert!(stats.coeff_layout_savings_bytes < 0);
        assert!(stats.coeff_layout_savings_percent < 0.0);
    }

    #[test]
    fn legacy_dispatcher_rejects_fr2_rev10_through_12() {
        let seq = VideoSequenceHeaderV2 {
            width: 16,
            height: 16,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            deblock_strength: 0,
            max_ref_frames: 2,
        };
        let mut slot = None::<YuvFrame>;
        for rev in [10_u8, 11, 12] {
            let pl = vec![b'F', b'R', b'2', rev];
            assert!(matches!(
                decode_yuv420_srsv2_payload(&seq, &pl, &mut slot),
                Err(crate::srsv2::error::SrsV2Error::Unsupported(_))
            ));
        }
    }
}
