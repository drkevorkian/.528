//! Serialized SRSV2 **frame** payload (inside mux packet or elementary stream).

use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::intra_codec::{decode_plane_intra, encode_plane_intra};
use super::limits::MAX_FRAME_PAYLOAD_BYTES;
use super::model::{PixelFormat, VideoSequenceHeaderV2};

pub const FRAME_PAYLOAD_MAGIC: [u8; 4] = [b'F', b'R', b'2', 1];

pub fn encode_yuv420_intra_payload(
    seq: &VideoSequenceHeaderV2,
    yuv: &YuvFrame,
    frame_index: u32,
    qp: u8,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.pixel_format != PixelFormat::Yuv420p8 || yuv.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported(
            "encode path only supports YUV420p8 in this slice",
        ));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&FRAME_PAYLOAD_MAGIC);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(qp);

    let mut yb = Vec::new();
    encode_plane_intra(&yuv.y, qp.max(1) as i16, &mut yb)?;
    let mut ub = Vec::new();
    encode_plane_intra(&yuv.u, qp.max(1) as i16, &mut ub)?;
    let mut vb = Vec::new();
    encode_plane_intra(&yuv.v, qp.max(1) as i16, &mut vb)?;

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
    if payload[0..4] != FRAME_PAYLOAD_MAGIC {
        return Err(SrsV2Error::BadMagic);
    }
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
    let cw = (w + 1) / 2;
    let ch = (h + 1) / 2;

    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

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
        let payload = encode_yuv420_intra_payload(&seq, &yuv, 1, qp).expect("enc");
        let dec = decode_yuv420_intra_payload(&seq, &payload).expect("dec");
        assert_eq!(dec.frame_index, 1);
        assert_eq!(dec.width, 64);
        assert_eq!(dec.height, 64);
        assert_eq!(dec.yuv.y.samples.len(), yuv.y.samples.len());
    }
}
