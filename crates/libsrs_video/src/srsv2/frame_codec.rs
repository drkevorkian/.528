//! Serialized SRSV2 **frame** payload (inside mux packet or elementary stream).

use super::adaptive_quant::{resolve_frame_adaptive_qp, SrsV2AqEncodeStats};
use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::intra_codec::{decode_plane_intra, encode_plane_intra};
use super::limits::MAX_FRAME_PAYLOAD_BYTES;
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::motion_search::SrsV2MotionEncodeStats;
use super::p_frame_codec;
use super::rate_control::{ResidualEncodeStats, ResidualEntropy, SrsV2EncodeSettings};
use super::residual_entropy::{decode_plane_intra_entropy, encode_plane_intra_entropy};

pub const FRAME_PAYLOAD_MAGIC: [u8; 4] = [b'F', b'R', b'2', 1];
pub const FRAME_PAYLOAD_MAGIC_INTRA_ENTROPY: [u8; 4] = [b'F', b'R', b'2', 3];

fn intra_magic_matches(payload: &[u8]) -> bool {
    payload.len() >= 4 && &payload[0..3] == b"FR2" && matches!(payload[3], 1 | 3)
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
    let (eff_qp, aq_st) = resolve_frame_adaptive_qp(qp, yuv, settings)?;
    if let Some(a) = aq_out {
        *a = aq_st;
    }
    let mut out = Vec::new();
    let magic = match settings.residual_entropy {
        ResidualEntropy::Explicit => FRAME_PAYLOAD_MAGIC,
        ResidualEntropy::Auto | ResidualEntropy::Rans => FRAME_PAYLOAD_MAGIC_INTRA_ENTROPY,
    };
    out.extend_from_slice(&magic);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(eff_qp);

    let qp_i = eff_qp.max(1) as i16;
    let mut yb = Vec::new();
    let mut ub = Vec::new();
    let mut vb = Vec::new();

    match settings.residual_entropy {
        ResidualEntropy::Explicit => {
            encode_plane_intra(&yuv.y, qp_i, &mut yb)?;
            encode_plane_intra(&yuv.u, qp_i, &mut ub)?;
            encode_plane_intra(&yuv.v, qp_i, &mut vb)?;
        }
        ResidualEntropy::Auto | ResidualEntropy::Rans => {
            let mut noop = ResidualEncodeStats::default();
            let acc: &mut ResidualEncodeStats = stats.unwrap_or(&mut noop);
            encode_plane_intra_entropy(&yuv.y, qp_i, settings.residual_entropy, acc, &mut yb)?;
            encode_plane_intra_entropy(&yuv.u, qp_i, settings.residual_entropy, acc, &mut ub)?;
            encode_plane_intra_entropy(&yuv.v, qp_i, settings.residual_entropy, acc, &mut vb)?;
        }
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

fn push_chunk(out: &mut Vec<u8>, chunk: &[u8]) -> Result<(), SrsV2Error> {
    let len = u32::try_from(chunk.len()).map_err(|_| SrsV2Error::syntax("chunk length"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(chunk);
    Ok(())
}

pub fn decode_yuv420_intra_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported(
            "decode path only supports YUV420p8 in this slice",
        ));
    }
    if payload.len() < 4 + 4 + 1 + 4 * 3 {
        return Err(SrsV2Error::Truncated);
    }
    if !intra_magic_matches(payload) {
        return Err(SrsV2Error::BadMagic);
    }
    let rev = payload[3];
    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let qp = payload[cur];
    cur += 1;
    let qp_i = qp.max(1) as i16;

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
            decode_plane_intra_entropy(y_data, &mut c, &mut y_plane, qp_i)?;
            if c != y_data.len() {
                return Err(SrsV2Error::syntax("y plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy(u_data, &mut c, &mut u_plane, qp_i)?;
            if c != u_data.len() {
                return Err(SrsV2Error::syntax("u plane trailing bits"));
            }
            c = 0;
            decode_plane_intra_entropy(v_data, &mut c, &mut v_plane, qp_i)?;
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
    let (eff_qp, aq_st) = resolve_frame_adaptive_qp(qp, cur, settings)?;
    if let Some(a) = aq_out {
        *a = aq_st;
    }
    p_frame_codec::encode_yuv420_p_payload(
        seq,
        cur,
        reference,
        frame_index,
        eff_qp,
        settings,
        stats,
        motion_out,
    )
}

/// Decode intra or P SRSV2 payload; updates `ref_slot` when `max_ref_frames > 0` after a successful decode.
pub fn decode_yuv420_srsv2_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    ref_slot: &mut Option<YuvFrame>,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if payload.len() < 4 {
        return Err(SrsV2Error::Truncated);
    }
    match payload[3] {
        1 | 3 => {
            let dec = decode_yuv420_intra_payload(seq, payload)?;
            if seq.max_ref_frames > 0 {
                ref_slot.replace(dec.yuv.clone());
            }
            Ok(dec)
        }
        2 | 4 => {
            let reference = ref_slot
                .as_ref()
                .ok_or(SrsV2Error::PFrameWithoutReference)?;
            let dec = p_frame_codec::decode_yuv420_p_payload(seq, payload, reference)?;
            if seq.max_ref_frames > 0 {
                ref_slot.replace(dec.yuv.clone());
            }
            Ok(dec)
        }
        _ => Err(SrsV2Error::Unsupported(
            "unknown SRSV2 frame payload revision",
        )),
    }
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
    use crate::srsv2::color::rgb888_full_to_yuv420_bt709;
    use crate::srsv2::model::{
        ChromaSiting, ColorPrimaries, ColorRange, MatrixCoefficients, PixelFormat, SrsVideoProfile,
        TransferFunction, VideoSequenceHeaderV2,
    };
    use crate::srsv2::rate_control::SrsV2EncodeSettings;

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
}
