use crate::error::AudioCodecError;

pub const STREAM_MAGIC: [u8; 4] = *b"SRSA";
pub const STREAM_VERSION: u8 = 1;
pub const PACKET_SYNC: [u8; 2] = *b"AP";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_index: u32,
    pub samples: Vec<i16>,
}

impl AudioFrame {
    pub fn sample_count_per_channel(&self) -> Result<u32, AudioCodecError> {
        validate_channels(self.channels)?;
        if self.samples.is_empty() {
            return Ok(0);
        }
        let channels = self.channels as usize;
        if !self.samples.len().is_multiple_of(channels) {
            return Err(AudioCodecError::InvalidData(
                "interleaved sample length does not match channels",
            ));
        }
        Ok((self.samples.len() / channels) as u32)
    }
}

pub fn encode_frame(frame: &AudioFrame) -> Result<Vec<u8>, AudioCodecError> {
    validate_channels(frame.channels)?;
    let sample_count = frame.sample_count_per_channel()?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&sample_count.to_le_bytes());
    payload.push(frame.channels);

    for ch in 0..frame.channels as usize {
        let channel_samples = deinterleave_channel(&frame.samples, frame.channels as usize, ch);
        let encoded = encode_channel(&channel_samples);
        let encoded_len = u32::try_from(encoded.len())
            .map_err(|_| AudioCodecError::InvalidData("channel payload too large"))?;
        payload.extend_from_slice(&encoded_len.to_le_bytes());
        payload.extend_from_slice(&encoded);
    }

    Ok(payload)
}

pub fn decode_frame(
    sample_rate: u32,
    frame_index: u32,
    payload: &[u8],
) -> Result<AudioFrame, AudioCodecError> {
    if payload.len() < 5 {
        return Err(AudioCodecError::InvalidData("audio payload too small"));
    }
    let mut count = [0_u8; 4];
    count.copy_from_slice(&payload[0..4]);
    let sample_count = u32::from_le_bytes(count);
    let channels = payload[4];
    validate_channels(channels)?;

    let mut cursor = 5usize;
    let mut channels_data: Vec<Vec<i16>> = Vec::with_capacity(channels as usize);
    for _ in 0..channels {
        if payload.len().saturating_sub(cursor) < 4 {
            return Err(AudioCodecError::InvalidData("truncated channel length"));
        }
        let mut len = [0_u8; 4];
        len.copy_from_slice(&payload[cursor..cursor + 4]);
        cursor += 4;
        let channel_len = u32::from_le_bytes(len) as usize;
        if payload.len().saturating_sub(cursor) < channel_len {
            return Err(AudioCodecError::InvalidData("truncated channel payload"));
        }
        let channel = decode_channel(&payload[cursor..cursor + channel_len], sample_count as usize)?;
        cursor += channel_len;
        channels_data.push(channel);
    }
    if cursor != payload.len() {
        return Err(AudioCodecError::InvalidData("unexpected bytes in audio payload"));
    }

    let interleaved = interleave_channels(&channels_data, channels as usize)?;
    Ok(AudioFrame {
        sample_rate,
        channels,
        frame_index,
        samples: interleaved,
    })
}

fn encode_channel(samples: &[i16]) -> Vec<u8> {
    if samples.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    out.extend_from_slice(&samples[0].to_le_bytes());
    let mut zero_run: u8 = 0;
    let mut prev = samples[0] as i32;
    for sample in samples.iter().skip(1) {
        let current = *sample as i32;
        let delta = current - prev;
        prev = current;
        if delta == 0 {
            if zero_run == u8::MAX {
                flush_zero_run(zero_run, &mut out);
                zero_run = 0;
            }
            zero_run = zero_run.saturating_add(1);
        } else {
            flush_zero_run(zero_run, &mut out);
            zero_run = 0;
            write_delta_token(delta, &mut out);
        }
    }
    flush_zero_run(zero_run, &mut out);
    out
}

fn decode_channel(payload: &[u8], sample_count: usize) -> Result<Vec<i16>, AudioCodecError> {
    if sample_count == 0 {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        return Err(AudioCodecError::InvalidData(
            "unexpected channel payload for zero sample frame",
        ));
    }
    if payload.len() < 2 {
        return Err(AudioCodecError::InvalidData("truncated channel first sample"));
    }
    let mut cursor = 0usize;
    let first = read_i16(payload, &mut cursor)? as i32;
    let mut samples = Vec::with_capacity(sample_count);
    samples.push(first as i16);
    let mut prev = first;

    while samples.len() < sample_count {
        let tag = read_u8(payload, &mut cursor)?;
        if tag <= 127 {
            let run = (tag as usize) + 1;
            for _ in 0..run {
                if samples.len() >= sample_count {
                    return Err(AudioCodecError::InvalidData("audio zero run overflow"));
                }
                samples.push(prev as i16);
            }
        } else {
            let delta = if tag == 128 {
                read_i32(payload, &mut cursor)?
            } else {
                (tag as i32) - 192
            };
            let value = prev + delta;
            if !(-32768..=32767).contains(&value) {
                return Err(AudioCodecError::InvalidData("decoded sample out of i16 range"));
            }
            prev = value;
            samples.push(value as i16);
        }
    }
    if cursor != payload.len() {
        return Err(AudioCodecError::InvalidData(
            "unexpected bytes at end of audio channel payload",
        ));
    }
    Ok(samples)
}

fn deinterleave_channel(samples: &[i16], channels: usize, channel_idx: usize) -> Vec<i16> {
    samples
        .iter()
        .skip(channel_idx)
        .step_by(channels)
        .copied()
        .collect()
}

fn interleave_channels(
    channels_data: &[Vec<i16>],
    channels: usize,
) -> Result<Vec<i16>, AudioCodecError> {
    if channels_data.len() != channels {
        return Err(AudioCodecError::InvalidData(
            "channel metadata does not match payload channels",
        ));
    }
    if channels == 0 {
        return Ok(Vec::new());
    }
    let sample_count = channels_data
        .first()
        .ok_or(AudioCodecError::InvalidData("missing channel data"))?
        .len();
    if channels_data.iter().any(|ch| ch.len() != sample_count) {
        return Err(AudioCodecError::InvalidData(
            "channels are not equal sample length",
        ));
    }
    let mut out = Vec::with_capacity(sample_count * channels);
    for i in 0..sample_count {
        for ch in channels_data {
            out.push(ch[i]);
        }
    }
    Ok(out)
}

fn validate_channels(channels: u8) -> Result<(), AudioCodecError> {
    if channels == 1 || channels == 2 {
        Ok(())
    } else {
        Err(AudioCodecError::UnsupportedChannels(channels))
    }
}

fn write_delta_token(delta: i32, out: &mut Vec<u8>) {
    if (-63..=63).contains(&delta) && delta != 0 {
        out.push((delta + 192) as u8);
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

fn read_u8(data: &[u8], cursor: &mut usize) -> Result<u8, AudioCodecError> {
    if *cursor >= data.len() {
        return Err(AudioCodecError::InvalidData("truncated payload"));
    }
    let value = data[*cursor];
    *cursor += 1;
    Ok(value)
}

fn read_i16(data: &[u8], cursor: &mut usize) -> Result<i16, AudioCodecError> {
    if data.len().saturating_sub(*cursor) < 2 {
        return Err(AudioCodecError::InvalidData("truncated i16"));
    }
    let mut bytes = [0_u8; 2];
    bytes.copy_from_slice(&data[*cursor..*cursor + 2]);
    *cursor += 2;
    Ok(i16::from_le_bytes(bytes))
}

fn read_i32(data: &[u8], cursor: &mut usize) -> Result<i32, AudioCodecError> {
    if data.len().saturating_sub(*cursor) < 4 {
        return Err(AudioCodecError::InvalidData("truncated i32"));
    }
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&data[*cursor..*cursor + 4]);
    *cursor += 4;
    Ok(i32::from_le_bytes(bytes))
}
