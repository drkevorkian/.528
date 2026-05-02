//! Experimental **B-frame** payload (`FR2` rev **10** integer MV, **11** half-pel) — parser-safe baseline.

use super::error::SrsV2Error;
use super::frame::{DecodedVideoFrameV2, VideoPlane, YuvFrame};
use super::limits::{MAX_FRAME_PAYLOAD_BYTES, MAX_MOTION_VECTOR_PELS};
use super::model::{PixelFormat, VideoSequenceHeaderV2};
use super::motion_search::sample_u8_plane;
use super::p_frame_codec::{copy_chroma_mb8, copy_chroma_mb8_qpel};
use super::reference_manager::SrsV2ReferenceManager;
use super::residual_entropy::{decode_p_residual_chunk, encode_p_residual_chunk};
use super::residual_tokens::residual_token_model;
use super::subpel::{sample_luma_bilinear_qpel, validate_mv_qpel_halfgrid};

pub const FRAME_PAYLOAD_MAGIC_B: [u8; 4] = [b'F', b'R', b'2', 10];
pub const FRAME_PAYLOAD_MAGIC_B_SUBPEL: [u8; 4] = [b'F', b'R', b'2', 11];

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BBlendModeWire {
    ForwardOnly = 0,
    BackwardOnly = 1,
    Average = 2,
    WeightedPlaceholder = 3,
}

impl BBlendModeWire {
    pub fn from_u8(v: u8) -> Result<Self, SrsV2Error> {
        match v {
            0 => Ok(Self::ForwardOnly),
            1 => Ok(Self::BackwardOnly),
            2 => Ok(Self::Average),
            3 => Err(SrsV2Error::Unsupported(
                "B-frame weighted blend is not implemented",
            )),
            _ => Err(SrsV2Error::syntax("unknown B blend mode")),
        }
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

fn read_u8(data: &[u8], cur: &mut usize) -> Result<u8, SrsV2Error> {
    if *cur >= data.len() {
        return Err(SrsV2Error::Truncated);
    }
    let v = data[*cur];
    *cur += 1;
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

fn read_i32(data: &[u8], cur: &mut usize) -> Result<i32, SrsV2Error> {
    if data.len().saturating_sub(*cur) < 4 {
        return Err(SrsV2Error::Truncated);
    }
    let v = i32::from_le_bytes([data[*cur], data[*cur + 1], data[*cur + 2], data[*cur + 3]]);
    *cur += 4;
    Ok(v)
}

fn validate_mv_i16(mvx: i16, mvy: i16) -> Result<(), SrsV2Error> {
    if mvx.abs() > MAX_MOTION_VECTOR_PELS || mvy.abs() > MAX_MOTION_VECTOR_PELS {
        return Err(SrsV2Error::CorruptedMotionVector);
    }
    Ok(())
}

fn push_chunk(out: &mut Vec<u8>, chunk: &[u8]) -> Result<(), SrsV2Error> {
    let len = u32::try_from(chunk.len()).map_err(|_| SrsV2Error::syntax("b chunk length"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(chunk);
    Ok(())
}

/// Encode minimal **B** picture (`FR2` rev **10** or **11**): adaptive residuals, blend **average** or single-ref modes.
#[allow(clippy::too_many_arguments)]
pub fn encode_yuv420_b_payload(
    seq: &VideoSequenceHeaderV2,
    cur: &YuvFrame,
    ref_a: &YuvFrame,
    ref_b: &YuvFrame,
    frame_index: u32,
    qp: u8,
    slot_a: u8,
    slot_b: u8,
    blend: BBlendModeWire,
    half_pel: bool,
) -> Result<Vec<u8>, SrsV2Error> {
    if seq.max_ref_frames < 2 {
        return Err(SrsV2Error::syntax("B-frame requires max_ref_frames >= 2"));
    }
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("B encode YUV420p8 only"));
    }
    if !seq.width.is_multiple_of(16) || !seq.height.is_multiple_of(16) {
        return Err(SrsV2Error::syntax("B-frame requires 16-aligned dimensions"));
    }
    let w = seq.width;
    let h = seq.height;
    let qp_i = qp.max(1) as i16;
    let mb_cols = w / 16;
    let mb_rows = h / 16;
    let model = residual_token_model();
    let magic = if half_pel {
        FRAME_PAYLOAD_MAGIC_B_SUBPEL
    } else {
        FRAME_PAYLOAD_MAGIC_B
    };
    let mut out = Vec::new();
    out.extend_from_slice(&magic);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.push(qp);
    out.push(slot_a);
    out.push(slot_b);
    out.push(blend as u8);

    let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let mv_ax = 0_i16;
            let mv_ay = 0_i16;
            let mv_bx = 0_i16;
            let mv_by = 0_i16;
            let (mv_aqx, mv_aqy, mv_bqx, mv_bqy) = (0_i32, 0_i32, 0_i32, 0_i32);

            if half_pel {
                out.extend_from_slice(&mv_aqx.to_le_bytes());
                out.extend_from_slice(&mv_aqy.to_le_bytes());
                out.extend_from_slice(&mv_bqx.to_le_bytes());
                out.extend_from_slice(&mv_bqy.to_le_bytes());
            } else {
                out.extend_from_slice(&mv_ax.to_le_bytes());
                out.extend_from_slice(&mv_ay.to_le_bytes());
                out.extend_from_slice(&mv_bx.to_le_bytes());
                out.extend_from_slice(&mv_by.to_le_bytes());
            }

            let mut pattern = 0_u8;
            let mut chunks: Vec<Vec<u8>> = Vec::new();

            for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
                let mut blk = [[0_i16; 8]; 8];
                let mut max_abs = 0_i16;
                for row in 0..8 {
                    for col in 0..8 {
                        let lx = mbx * 16 + dx + col;
                        let ly = mby * 16 + dy + row;
                        let cx = cur.y.samples[ly as usize * cur.y.stride + lx as usize] as i16;
                        let pred = match blend {
                            BBlendModeWire::ForwardOnly => {
                                if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                    ) as i16
                                } else {
                                    let rx = lx as i32 + mv_ax as i32;
                                    let ry = ly as i32 + mv_ay as i32;
                                    sample_u8_plane(&ref_a.y, rx, ry) as i16
                                }
                            }
                            BBlendModeWire::BackwardOnly => {
                                if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                    ) as i16
                                } else {
                                    let rx = lx as i32 + mv_bx as i32;
                                    let ry = ly as i32 + mv_by as i32;
                                    sample_u8_plane(&ref_b.y, rx, ry) as i16
                                }
                            }
                            BBlendModeWire::Average => {
                                let pa = if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                    ) as i32
                                } else {
                                    let rx = lx as i32 + mv_ax as i32;
                                    let ry = ly as i32 + mv_ay as i32;
                                    sample_u8_plane(&ref_a.y, rx, ry) as i32
                                };
                                let pb = if half_pel {
                                    sample_luma_bilinear_qpel(
                                        &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                    ) as i32
                                } else {
                                    let rx = lx as i32 + mv_bx as i32;
                                    let ry = ly as i32 + mv_by as i32;
                                    sample_u8_plane(&ref_b.y, rx, ry) as i32
                                };
                                ((pa + pb + 1) >> 1) as i16
                            }
                            BBlendModeWire::WeightedPlaceholder => unreachable!(),
                        };
                        let d = cx - pred;
                        blk[row as usize][col as usize] = d;
                        max_abs = max_abs.max(d.abs());
                    }
                }
                const SKIP_ABS_THRESH: i16 = 6;
                if max_abs <= SKIP_ABS_THRESH {
                    pattern |= 1 << si;
                } else {
                    let mut linear = [0_i16; 64];
                    for r in 0..8 {
                        for c in 0..8 {
                            linear[r * 8 + c] = blk[r][c];
                        }
                    }
                    let f = super::dct::fdct_8x8(&linear);
                    let qf = super::intra_codec::quantize(&f, qp_i);
                    let (chunk, _) = encode_p_residual_chunk(
                        &qf,
                        super::rate_control::ResidualEntropy::Auto,
                        &model,
                    )?;
                    chunks.push(chunk);
                }
            }

            out.push(pattern);
            for c in chunks {
                push_chunk(&mut out, &c)?;
            }
        }
    }

    if out.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(SrsV2Error::AllocationLimit {
            context: "encoded B-frame",
        });
    }
    Ok(out)
}

/// Decode **B** payload; **`manager`** supplies **`slot_a`** / **`slot_b`** pictures (by linear index).
pub fn decode_yuv420_b_payload(
    seq: &VideoSequenceHeaderV2,
    payload: &[u8],
    manager: &SrsV2ReferenceManager,
) -> Result<DecodedVideoFrameV2, SrsV2Error> {
    if seq.max_ref_frames < 2 {
        return Err(SrsV2Error::syntax("B-frame requires max_ref_frames >= 2"));
    }
    if seq.pixel_format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("B decode YUV420p8 only"));
    }
    if payload.len() < 4 + 4 + 1 + 3 {
        return Err(SrsV2Error::Truncated);
    }
    if &payload[0..3] != b"FR2" || !matches!(payload[3], 10 | 11) {
        return Err(SrsV2Error::BadMagic);
    }
    let half_pel = payload[3] == 11;
    let mut cur = 4usize;
    let frame_index = read_u32(payload, &mut cur)?;
    let qp = read_u8(payload, &mut cur)?;
    let slot_a = read_u8(payload, &mut cur)?;
    let slot_b = read_u8(payload, &mut cur)?;
    let blend_b = read_u8(payload, &mut cur)?;
    let blend = BBlendModeWire::from_u8(blend_b)?;

    let ref_a = manager.frame_at_slot_index(slot_a)?;
    let ref_b = manager.frame_at_slot_index(slot_b)?;
    let fi_a = manager.slot_frame_index(slot_a)?;
    let fi_b = manager.slot_frame_index(slot_b)?;
    if fi_a >= frame_index {
        return Err(SrsV2Error::syntax(
            "B-frame backward reference must use an earlier frame_index",
        ));
    }
    if fi_b <= frame_index {
        return Err(SrsV2Error::syntax(
            "B-frame forward reference must use a later frame_index",
        ));
    }
    if ref_a.format != PixelFormat::Yuv420p8 || ref_b.format != PixelFormat::Yuv420p8 {
        return Err(SrsV2Error::Unsupported("B refs must be YUV420p8"));
    }

    let w = seq.width;
    let h = seq.height;
    if !w.is_multiple_of(16) || !h.is_multiple_of(16) {
        return Err(SrsV2Error::syntax("bad dimensions for B-frame"));
    }
    if ref_a.y.width != w || ref_b.y.width != w {
        return Err(SrsV2Error::syntax("reference geometry mismatch"));
    }

    let qp_i = qp.max(1) as i16;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut y_plane = VideoPlane::<u8>::try_new(w, h, w as usize)?;
    let mut u_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;
    let mut v_plane = VideoPlane::<u8>::try_new(cw, ch, cw as usize)?;

    let mb_cols = w / 16;
    let mb_rows = h / 16;
    let sub_offsets = [(0_u32, 0_u32), (8, 0), (0, 8), (8, 8)];

    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let (mv_aqx, mv_aqy, mv_bqx, mv_bqy, mv_ax, mv_ay, mv_bx, mv_by) = if half_pel {
                let ax = read_i32(payload, &mut cur)?;
                let ay = read_i32(payload, &mut cur)?;
                let bx = read_i32(payload, &mut cur)?;
                let by = read_i32(payload, &mut cur)?;
                validate_mv_qpel_halfgrid(ax, ay)?;
                validate_mv_qpel_halfgrid(bx, by)?;
                (ax, ay, bx, by, 0_i16, 0_i16, 0_i16, 0_i16)
            } else {
                let ax = read_i16(payload, &mut cur)?;
                let ay = read_i16(payload, &mut cur)?;
                let bx = read_i16(payload, &mut cur)?;
                let by = read_i16(payload, &mut cur)?;
                validate_mv_i16(ax, ay)?;
                validate_mv_i16(bx, by)?;
                (0_i32, 0_i32, 0_i32, 0_i32, ax, ay, bx, by)
            };

            let pattern = read_u8(payload, &mut cur)?;

            for (si, &(dx, dy)) in sub_offsets.iter().enumerate() {
                let skip = (pattern & (1 << si)) != 0;
                if skip {
                    for row in 0..8 {
                        for col in 0..8 {
                            let lx = mbx * 16 + dx + col;
                            let ly = mby * 16 + dy + row;
                            let pv = match blend {
                                BBlendModeWire::ForwardOnly => {
                                    if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        )
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry)
                                    }
                                }
                                BBlendModeWire::BackwardOnly => {
                                    if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        )
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry)
                                    }
                                }
                                BBlendModeWire::Average => {
                                    let pa = if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry) as i32
                                    };
                                    let pb = if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry) as i32
                                    };
                                    ((pa + pb + 1) >> 1) as u8
                                }
                                BBlendModeWire::WeightedPlaceholder => unreachable!(),
                            };
                            y_plane.samples[ly as usize * y_plane.stride + lx as usize] = pv;
                        }
                    }
                } else {
                    let chunk_len = read_u32(payload, &mut cur)? as usize;
                    let end = cur
                        .checked_add(chunk_len)
                        .ok_or(SrsV2Error::Overflow("b residual chunk"))?;
                    if end > payload.len() {
                        return Err(SrsV2Error::Truncated);
                    }
                    let chunk = &payload[cur..end];
                    cur = end;
                    let res = decode_p_residual_chunk(chunk, qp_i)?;
                    for row in 0..8 {
                        for col in 0..8 {
                            let lx = mbx * 16 + dx + col;
                            let ly = mby * 16 + dy + row;
                            let pred = match blend {
                                BBlendModeWire::ForwardOnly => {
                                    if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry) as i32
                                    }
                                }
                                BBlendModeWire::BackwardOnly => {
                                    if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry) as i32
                                    }
                                }
                                BBlendModeWire::Average => {
                                    let pa = if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_a.y, lx as i32, ly as i32, mv_aqx, mv_aqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_ax as i32;
                                        let ry = ly as i32 + mv_ay as i32;
                                        sample_u8_plane(&ref_a.y, rx, ry) as i32
                                    };
                                    let pb = if half_pel {
                                        sample_luma_bilinear_qpel(
                                            &ref_b.y, lx as i32, ly as i32, mv_bqx, mv_bqy,
                                        ) as i32
                                    } else {
                                        let rx = lx as i32 + mv_bx as i32;
                                        let ry = ly as i32 + mv_by as i32;
                                        sample_u8_plane(&ref_b.y, rx, ry) as i32
                                    };
                                    (pa + pb + 1) >> 1
                                }
                                BBlendModeWire::WeightedPlaceholder => unreachable!(),
                            };
                            let pv = (pred + res[row as usize][col as usize] as i32).clamp(0, 255);
                            y_plane.samples[ly as usize * y_plane.stride + lx as usize] = pv as u8;
                        }
                    }
                }
            }

            match blend {
                BBlendModeWire::ForwardOnly => {
                    if half_pel {
                        copy_chroma_mb8_qpel(&ref_a.u, &mut u_plane, mbx, mby, mv_aqx, mv_aqy);
                        copy_chroma_mb8_qpel(&ref_a.v, &mut v_plane, mbx, mby, mv_aqx, mv_aqy);
                    } else {
                        copy_chroma_mb8(&ref_a.u, &mut u_plane, mbx, mby, mv_ax, mv_ay);
                        copy_chroma_mb8(&ref_a.v, &mut v_plane, mbx, mby, mv_ax, mv_ay);
                    }
                }
                BBlendModeWire::BackwardOnly => {
                    if half_pel {
                        copy_chroma_mb8_qpel(&ref_b.u, &mut u_plane, mbx, mby, mv_bqx, mv_bqy);
                        copy_chroma_mb8_qpel(&ref_b.v, &mut v_plane, mbx, mby, mv_bqx, mv_bqy);
                    } else {
                        copy_chroma_mb8(&ref_b.u, &mut u_plane, mbx, mby, mv_bx, mv_by);
                        copy_chroma_mb8(&ref_b.v, &mut v_plane, mbx, mby, mv_bx, mv_by);
                    }
                }
                BBlendModeWire::Average => {
                    for ry in 0..8u32 {
                        for rx in 0..8u32 {
                            let ox = (mbx * 8 + rx) as usize;
                            let oy = (mby * 8 + ry) as usize;
                            if ox >= u_plane.width as usize || oy >= u_plane.height as usize {
                                continue;
                            }
                            let base_x = (mbx * 8) as i32 + rx as i32;
                            let base_y = (mby * 8) as i32 + ry as i32;
                            let ua = if half_pel {
                                sample_u8_plane(&ref_a.u, base_x + mv_aqx / 8, base_y + mv_aqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_a.u,
                                    base_x + (mv_ax as i32) / 2,
                                    base_y + (mv_ay as i32) / 2,
                                ) as i32
                            };
                            let ub = if half_pel {
                                sample_u8_plane(&ref_b.u, base_x + mv_bqx / 8, base_y + mv_bqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_b.u,
                                    base_x + (mv_bx as i32) / 2,
                                    base_y + (mv_by as i32) / 2,
                                ) as i32
                            };
                            u_plane.samples[oy * u_plane.stride + ox] = ((ua + ub + 1) >> 1) as u8;
                            let va = if half_pel {
                                sample_u8_plane(&ref_a.v, base_x + mv_aqx / 8, base_y + mv_aqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_a.v,
                                    base_x + (mv_ax as i32) / 2,
                                    base_y + (mv_ay as i32) / 2,
                                ) as i32
                            };
                            let vb = if half_pel {
                                sample_u8_plane(&ref_b.v, base_x + mv_bqx / 8, base_y + mv_bqy / 8)
                                    as i32
                            } else {
                                sample_u8_plane(
                                    &ref_b.v,
                                    base_x + (mv_bx as i32) / 2,
                                    base_y + (mv_by as i32) / 2,
                                ) as i32
                            };
                            v_plane.samples[oy * v_plane.stride + ox] = ((va + vb + 1) >> 1) as u8;
                        }
                    }
                }
                BBlendModeWire::WeightedPlaceholder => unreachable!(),
            }
        }
    }

    if cur != payload.len() {
        return Err(SrsV2Error::syntax("trailing B-frame bytes"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::color::gray8_packed_to_yuv420p8_neutral;
    use crate::srsv2::model::{
        ChromaSiting, ColorPrimaries, ColorRange, MatrixCoefficients, SrsVideoProfile,
        TransferFunction,
    };

    fn seq_b(w: u32, h: u32) -> VideoSequenceHeaderV2 {
        VideoSequenceHeaderV2 {
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
            max_ref_frames: 2,
        }
    }

    #[test]
    fn b_identical_refs_small_payload() {
        let seq = seq_b(64, 64);
        let gray = vec![99_u8; 64 * 64];
        let yuv = gray8_packed_to_yuv420p8_neutral(&gray, 64, 64).unwrap();
        let pay = encode_yuv420_b_payload(
            &seq,
            &yuv,
            &yuv,
            &yuv,
            2,
            28,
            0,
            1,
            BBlendModeWire::Average,
            false,
        )
        .unwrap();
        assert!(pay.len() < 8000, "payload {}", pay.len());
    }

    #[test]
    fn weighted_blend_wire_mode_is_reserved() {
        assert!(BBlendModeWire::from_u8(3).is_err());
    }
}
