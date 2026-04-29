use crate::error::VideoCodecError;

pub const STREAM_MAGIC: [u8; 4] = *b"SRSV";
pub const STREAM_VERSION: u8 = 1;
pub const PACKET_SYNC: [u8; 2] = *b"VP";
pub const BLOCK_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    I = 0,
    PReserved = 1,
}

impl FrameType {
    pub fn from_u8(value: u8) -> Result<Self, VideoCodecError> {
        match value {
            0 => Ok(Self::I),
            1 => Ok(Self::PReserved),
            other => Err(VideoCodecError::UnsupportedFrameType(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub frame_index: u32,
    pub frame_type: FrameType,
    pub data: Vec<u8>,
}

impl VideoFrame {
    pub fn validate_dimensions(&self) -> Result<(), VideoCodecError> {
        let expected = (self.width as usize).saturating_mul(self.height as usize);
        if expected != self.data.len() {
            return Err(VideoCodecError::InvalidData(
                "frame pixel buffer length does not match dimensions",
            ));
        }
        Ok(())
    }
}

pub fn encode_frame(frame: &VideoFrame) -> Result<Vec<u8>, VideoCodecError> {
    frame.validate_dimensions()?;
    if frame.frame_type != FrameType::I {
        return Err(VideoCodecError::InvalidData(
            "v1 encoder only supports intra (I) frames",
        ));
    }

    let mut payload = Vec::new();
    payload.push(BLOCK_SIZE as u8);
    let sample_count = u32::try_from((frame.width as usize).saturating_mul(frame.height as usize))
        .map_err(|_| VideoCodecError::InvalidData("frame too large"))?;
    payload.extend_from_slice(&sample_count.to_le_bytes());
    encode_residual_blocks(frame.width, frame.height, &frame.data, &mut payload)?;

    Ok(payload)
}

pub fn decode_frame(
    width: u32,
    height: u32,
    frame_index: u32,
    frame_type: FrameType,
    payload: &[u8],
) -> Result<VideoFrame, VideoCodecError> {
    if frame_type != FrameType::I {
        return Err(VideoCodecError::InvalidData(
            "v1 decoder only supports intra (I) frames",
        ));
    }
    if payload.len() < 5 {
        return Err(VideoCodecError::InvalidData("payload too small"));
    }
    let block_size = payload[0] as usize;
    if block_size != BLOCK_SIZE {
        return Err(VideoCodecError::InvalidData("unsupported block size"));
    }
    let mut len_bytes = [0_u8; 4];
    len_bytes.copy_from_slice(&payload[1..5]);
    let expected_len = u32::from_le_bytes(len_bytes) as usize;
    let exact_len = (width as usize).saturating_mul(height as usize);
    if expected_len != exact_len {
        return Err(VideoCodecError::InvalidData(
            "payload length metadata does not match frame dimensions",
        ));
    }
    let mut cursor = 5;
    let mut pixels = vec![0_u8; expected_len];
    decode_residual_blocks(width, height, payload, &mut cursor, &mut pixels)?;
    if cursor != payload.len() {
        return Err(VideoCodecError::InvalidData(
            "unexpected bytes after payload",
        ));
    }

    Ok(VideoFrame {
        width,
        height,
        frame_index,
        frame_type,
        data: pixels,
    })
}

fn encode_residual_blocks(
    width: u32,
    height: u32,
    pixels: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), VideoCodecError> {
    let width_usize = width as usize;
    let height_usize = height as usize;
    if pixels.len() != width_usize.saturating_mul(height_usize) {
        return Err(VideoCodecError::InvalidData(
            "pixel buffer length does not match dimensions",
        ));
    }

    for by in (0..height_usize).step_by(BLOCK_SIZE) {
        for bx in (0..width_usize).step_by(BLOCK_SIZE) {
            let mut predictor = 128_i16;
            let mut zero_run = 0_u8;
            for y in by..(by + BLOCK_SIZE).min(height_usize) {
                for x in bx..(bx + BLOCK_SIZE).min(width_usize) {
                    let idx = y * width_usize + x;
                    let sample = pixels[idx] as i16;
                    let delta = sample - predictor;
                    predictor = sample;
                    if delta == 0 {
                        if zero_run == u8::MAX {
                            flush_zero_run(zero_run, out);
                            zero_run = 0;
                        }
                        zero_run = zero_run.saturating_add(1);
                    } else {
                        flush_zero_run(zero_run, out);
                        zero_run = 0;
                        write_delta_token(delta, out);
                    }
                }
            }
            flush_zero_run(zero_run, out);
        }
    }
    Ok(())
}

fn decode_residual_blocks(
    width: u32,
    height: u32,
    payload: &[u8],
    cursor: &mut usize,
    out: &mut [u8],
) -> Result<(), VideoCodecError> {
    let width_usize = width as usize;
    let height_usize = height as usize;
    for by in (0..height_usize).step_by(BLOCK_SIZE) {
        for bx in (0..width_usize).step_by(BLOCK_SIZE) {
            let mut predictor = 128_i16;
            let block_positions = block_positions(width_usize, height_usize, bx, by);
            let mut i = 0_usize;
            while i < block_positions.len() {
                let tag = read_u8(payload, cursor)?;
                if tag <= 127 {
                    let run = (tag as usize) + 1;
                    for _ in 0..run {
                        if i >= block_positions.len() {
                            return Err(VideoCodecError::InvalidData("zero run overflows block"));
                        }
                        let value = predictor.clamp(0, 255) as u8;
                        let pos = block_positions[i];
                        out[pos] = value;
                        i += 1;
                    }
                } else {
                    let delta = if tag == 128 {
                        read_i16(payload, cursor)?
                    } else {
                        (tag as i16) - 192
                    };
                    let reconstructed = predictor + delta;
                    if !(0..=255).contains(&reconstructed) {
                        return Err(VideoCodecError::InvalidData("decoded sample out of range"));
                    }
                    predictor = reconstructed;
                    let pos = block_positions[i];
                    out[pos] = reconstructed as u8;
                    i += 1;
                }
            }
        }
    }
    Ok(())
}

fn block_positions(width: usize, height: usize, bx: usize, by: usize) -> Vec<usize> {
    let mut pos = Vec::with_capacity(BLOCK_SIZE * BLOCK_SIZE);
    for y in by..(by + BLOCK_SIZE).min(height) {
        for x in bx..(bx + BLOCK_SIZE).min(width) {
            pos.push(y * width + x);
        }
    }
    pos
}

fn write_delta_token(delta: i16, out: &mut Vec<u8>) {
    if (-63..=63).contains(&delta) && delta != 0 {
        let tag = (delta + 192) as u8;
        out.push(tag);
    } else {
        out.push(128);
        out.extend_from_slice(&delta.to_le_bytes());
    }
}

fn flush_zero_run(mut run: u8, out: &mut Vec<u8>) {
    while run > 0 {
        let chunk = run.min(128);
        out.push(chunk - 1);
        run -= chunk;
    }
}

fn read_u8(data: &[u8], cursor: &mut usize) -> Result<u8, VideoCodecError> {
    if *cursor >= data.len() {
        return Err(VideoCodecError::InvalidData("truncated payload"));
    }
    let value = data[*cursor];
    *cursor += 1;
    Ok(value)
}

fn read_i16(data: &[u8], cursor: &mut usize) -> Result<i16, VideoCodecError> {
    if data.len().saturating_sub(*cursor) < 2 {
        return Err(VideoCodecError::InvalidData("truncated i16 literal"));
    }
    let mut bytes = [0_u8; 2];
    bytes.copy_from_slice(&data[*cursor..*cursor + 2]);
    *cursor += 2;
    Ok(i16::from_le_bytes(bytes))
}
