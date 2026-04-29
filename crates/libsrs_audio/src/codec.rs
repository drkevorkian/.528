use libsrs_bitio::{rans_decode, rans_encode, BitIoError, RansModel};

use crate::error::AudioCodecError;
use crate::lpc::{autocorr_i16, levinson_durbin};

pub const STREAM_MAGIC: [u8; 4] = *b"SRSA";
pub const STREAM_VERSION_V1: u8 = 1;
pub const STREAM_VERSION_V2: u8 = 2;
/// Container + packet writers use this stream revision.
pub const STREAM_VERSION: u8 = STREAM_VERSION_V2;
pub const PACKET_SYNC: [u8; 2] = *b"AP";

/// Marks lossless LPC + rANS channel payloads (`"R2"`).
pub const PAYLOAD_V2_MAGIC: [u8; 2] = [0x52, 0x32];

const LPC_SHIFT: i32 = 12;
const LPC_SCALE: f64 = (1i32 << LPC_SHIFT) as f64;
const MAX_LPC_ORDER: usize = 8;

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

/// Encode a frame using **v2** lossless LPC + rANS channel format (magic `"R2"`).
pub fn encode_frame(frame: &AudioFrame) -> Result<Vec<u8>, AudioCodecError> {
    validate_channels(frame.channels)?;
    let sample_count = frame.sample_count_per_channel()?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&sample_count.to_le_bytes());
    payload.push(frame.channels);
    payload.extend_from_slice(&PAYLOAD_V2_MAGIC);

    for ch in 0..frame.channels as usize {
        let channel_samples = deinterleave_channel(&frame.samples, frame.channels as usize, ch);
        let encoded = encode_channel_v2(&channel_samples)?;
        let encoded_len = u32::try_from(encoded.len())
            .map_err(|_| AudioCodecError::InvalidData("channel payload too large"))?;
        payload.extend_from_slice(&encoded_len.to_le_bytes());
        payload.extend_from_slice(&encoded);
    }

    Ok(payload)
}

/// Decodes one frame from a **self-describing** payload (mux / unknown elementary context).
///
/// Uses magic [`PAYLOAD_V2_MAGIC`] at bytes 5–6 to detect v2. Legacy v1 elementary streams
/// can mis-detect if the first channel’s LE length has low 16 bits `0x3252`; prefer
/// [`decode_frame_with_stream_version`] when the 16-byte `SRSA` header revision is known.
pub fn decode_frame(
    sample_rate: u32,
    frame_index: u32,
    payload: &[u8],
) -> Result<AudioFrame, AudioCodecError> {
    decode_frame_inner(sample_rate, frame_index, payload, None)
}

/// Decodes using the elementary stream header’s `stream_version` byte (offset 4 in the
/// 16-byte `SRSA` header): v1 always parses legacy framing from `payload[5..]`; v2 requires
/// `R2` magic and uses LPC + rANS channel blobs.
pub fn decode_frame_with_stream_version(
    sample_rate: u32,
    frame_index: u32,
    payload: &[u8],
    elementary_stream_version: u8,
) -> Result<AudioFrame, AudioCodecError> {
    decode_frame_inner(
        sample_rate,
        frame_index,
        payload,
        Some(elementary_stream_version),
    )
}

fn decode_frame_inner(
    sample_rate: u32,
    frame_index: u32,
    payload: &[u8],
    elementary_stream_version: Option<u8>,
) -> Result<AudioFrame, AudioCodecError> {
    if payload.len() < 5 {
        return Err(AudioCodecError::InvalidData("audio payload too small"));
    }
    let mut count = [0_u8; 4];
    count.copy_from_slice(&payload[0..4]);
    let sample_count = u32::from_le_bytes(count);
    let channels = payload[4];
    validate_channels(channels)?;

    match elementary_stream_version {
        Some(STREAM_VERSION_V1) => decode_payload_v1_legacy(
            sample_rate,
            frame_index,
            channels,
            sample_count,
            &payload[5..],
        ),
        Some(STREAM_VERSION_V2) => {
            if payload.len() < 7 || payload[5..7] != PAYLOAD_V2_MAGIC {
                return Err(AudioCodecError::InvalidData(
                    "v2 audio payload missing R2 magic",
                ));
            }
            decode_payload_v2(
                sample_rate,
                frame_index,
                channels,
                sample_count,
                &payload[7..],
            )
        }
        Some(_) | None => {
            if payload.len() >= 7 && payload[5..7] == PAYLOAD_V2_MAGIC {
                decode_payload_v2(
                    sample_rate,
                    frame_index,
                    channels,
                    sample_count,
                    &payload[7..],
                )
            } else {
                decode_payload_v1_legacy(
                    sample_rate,
                    frame_index,
                    channels,
                    sample_count,
                    &payload[5..],
                )
            }
        }
    }
}

fn decode_payload_v2(
    sample_rate: u32,
    frame_index: u32,
    channels: u8,
    sample_count: u32,
    body: &[u8],
) -> Result<AudioFrame, AudioCodecError> {
    let mut cursor = 0usize;
    let mut channels_data: Vec<Vec<i16>> = Vec::with_capacity(channels as usize);
    for _ in 0..channels {
        if body.len().saturating_sub(cursor) < 4 {
            return Err(AudioCodecError::InvalidData("truncated channel length"));
        }
        let mut len = [0_u8; 4];
        len.copy_from_slice(&body[cursor..cursor + 4]);
        cursor += 4;
        let channel_len = u32::from_le_bytes(len) as usize;
        if body.len().saturating_sub(cursor) < channel_len {
            return Err(AudioCodecError::InvalidData("truncated channel payload"));
        }
        let channel =
            decode_channel_v2(&body[cursor..cursor + channel_len], sample_count as usize)?;
        cursor += channel_len;
        channels_data.push(channel);
    }
    if cursor != body.len() {
        return Err(AudioCodecError::InvalidData(
            "unexpected bytes in audio v2 payload",
        ));
    }

    let interleaved = interleave_channels(&channels_data, channels as usize)?;
    Ok(AudioFrame {
        sample_rate,
        channels,
        frame_index,
        samples: interleaved,
    })
}

fn decode_payload_v1_legacy(
    sample_rate: u32,
    frame_index: u32,
    channels: u8,
    sample_count: u32,
    body: &[u8],
) -> Result<AudioFrame, AudioCodecError> {
    let mut cursor = 0usize;
    let mut channels_data: Vec<Vec<i16>> = Vec::with_capacity(channels as usize);
    for _ in 0..channels {
        if body.len().saturating_sub(cursor) < 4 {
            return Err(AudioCodecError::InvalidData("truncated channel length"));
        }
        let mut len = [0_u8; 4];
        len.copy_from_slice(&body[cursor..cursor + 4]);
        cursor += 4;
        let channel_len = u32::from_le_bytes(len) as usize;
        if body.len().saturating_sub(cursor) < channel_len {
            return Err(AudioCodecError::InvalidData("truncated channel payload"));
        }
        let channel =
            decode_channel_legacy(&body[cursor..cursor + channel_len], sample_count as usize)?;
        cursor += channel_len;
        channels_data.push(channel);
    }
    if cursor != body.len() {
        return Err(AudioCodecError::InvalidData(
            "unexpected bytes in audio payload",
        ));
    }

    let interleaved = interleave_channels(&channels_data, channels as usize)?;
    Ok(AudioFrame {
        sample_rate,
        channels,
        frame_index,
        samples: interleaved,
    })
}

fn predict_i16(x: &[i16], n: usize, p: usize, coef: &[i16]) -> i32 {
    let mut acc: i32 = 0;
    for k in 0..p {
        let c = coef[k] as i32;
        let s = x[n - 1 - k] as i32;
        acc = acc.saturating_add(c.saturating_mul(s));
    }
    acc >> LPC_SHIFT
}

fn encode_channel_v2(samples: &[i16]) -> Result<Vec<u8>, AudioCodecError> {
    if samples.is_empty() {
        let mut out = vec![0u8];
        out.extend_from_slice(&0u32.to_le_bytes());
        return Ok(out);
    }

    let n = samples.len();
    if n > MAX_LPC_ORDER {
        for p in (1..=MAX_LPC_ORDER.min(n - 1)).rev() {
            if let Some(r) = autocorr_i16(samples, p) {
                if let Some(a_f) = levinson_durbin(&r, p) {
                    let coef: Vec<i16> = a_f
                        .iter()
                        .map(|&c| {
                            let q = (c * LPC_SCALE).round() as i32;
                            q.clamp(i16::MIN as i32, i16::MAX as i32) as i16
                        })
                        .collect();
                    let mut ok = true;
                    let mut residuals: Vec<i16> = Vec::with_capacity(n.saturating_sub(p));
                    for i in p..n {
                        let pred = predict_i16(samples, i, p, &coef);
                        let res = (samples[i] as i32).saturating_sub(pred);
                        if !(i16::MIN as i32..=i16::MAX as i32).contains(&res) {
                            ok = false;
                            break;
                        }
                        residuals.push(res as i16);
                    }
                    if ok {
                        let mut body = vec![1u8];
                        body.push(p as u8);
                        for c in &coef {
                            body.extend_from_slice(&c.to_le_bytes());
                        }
                        for s in samples.iter().take(p) {
                            body.extend_from_slice(&s.to_le_bytes());
                        }
                        let mut res_bytes = Vec::with_capacity(residuals.len().saturating_mul(2));
                        for r in &residuals {
                            res_bytes.extend_from_slice(&r.to_le_bytes());
                        }
                        let model = shared_rans_model()?;
                        let sym: Vec<usize> = res_bytes.iter().map(|&b| usize::from(b)).collect();
                        let blob = rans_encode(&model, &sym).map_err(map_bit_io)?;
                        body.extend_from_slice(
                            &u32::try_from(blob.len())
                                .map_err(|_| AudioCodecError::InvalidData("rans blob length"))?
                                .to_le_bytes(),
                        );
                        body.extend_from_slice(&blob);
                        return Ok(body);
                    }
                }
            }
        }
    }

    let mut body = vec![0u8];
    let raw_len =
        u32::try_from(n.saturating_mul(2)).map_err(|_| AudioCodecError::InvalidData("raw len"))?;
    body.extend_from_slice(&raw_len.to_le_bytes());
    for s in samples {
        body.extend_from_slice(&s.to_le_bytes());
    }
    Ok(body)
}

fn decode_channel_v2(payload: &[u8], sample_count: usize) -> Result<Vec<i16>, AudioCodecError> {
    if payload.is_empty() {
        if sample_count == 0 {
            return Ok(Vec::new());
        }
        return Err(AudioCodecError::InvalidData("empty v2 channel"));
    }
    let mode = payload[0];
    let mut c = 1usize;
    match mode {
        0 => {
            if payload.len().saturating_sub(c) < 4 {
                return Err(AudioCodecError::InvalidData("v2 raw len"));
            }
            let len =
                u32::from_le_bytes([payload[c], payload[c + 1], payload[c + 2], payload[c + 3]])
                    as usize;
            c += 4;
            if len != sample_count.saturating_mul(2) {
                return Err(AudioCodecError::InvalidData("v2 raw size mismatch"));
            }
            if payload.len().saturating_sub(c) < len {
                return Err(AudioCodecError::InvalidData("v2 raw truncated"));
            }
            let mut out = Vec::with_capacity(sample_count);
            for chunk in payload[c..c + len].chunks_exact(2) {
                out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
            if out.len() != sample_count {
                return Err(AudioCodecError::InvalidData("v2 raw sample count"));
            }
            if c + len != payload.len() {
                return Err(AudioCodecError::InvalidData("v2 raw trailing"));
            }
            Ok(out)
        }
        1 => {
            if sample_count == 0 {
                return Ok(Vec::new());
            }
            let p = payload
                .get(c)
                .copied()
                .ok_or(AudioCodecError::InvalidData("v2 lpc order byte"))?
                as usize;
            c += 1;
            if p == 0 || p > MAX_LPC_ORDER {
                return Err(AudioCodecError::InvalidData("v2 bad lpc order"));
            }
            let coef_bytes = p.saturating_mul(2);
            let warm_bytes = p.saturating_mul(2);
            if payload.len().saturating_sub(c)
                < coef_bytes.saturating_add(warm_bytes).saturating_add(4)
            {
                return Err(AudioCodecError::InvalidData("v2 lpc header"));
            }
            let mut coef = vec![0i16; p];
            for coef_i in coef.iter_mut().take(p) {
                *coef_i = i16::from_le_bytes([payload[c], payload[c + 1]]);
                c += 2;
            }
            let mut x = vec![0i16; sample_count];
            for xi in x.iter_mut().take(p) {
                *xi = i16::from_le_bytes([payload[c], payload[c + 1]]);
                c += 2;
            }
            if sample_count < p {
                return Err(AudioCodecError::InvalidData("v2 sample count vs order"));
            }
            let rlen =
                u32::from_le_bytes([payload[c], payload[c + 1], payload[c + 2], payload[c + 3]])
                    as usize;
            c += 4;
            if payload.len().saturating_sub(c) < rlen {
                return Err(AudioCodecError::InvalidData("v2 rans truncated"));
            }
            let blob = &payload[c..c + rlen];

            let expect_residuals = sample_count.saturating_sub(p);
            let expect_bytes = expect_residuals.saturating_mul(2);
            let num_syms = expect_bytes;
            let decode_budget = blob
                .len()
                .saturating_mul(32)
                .max(num_syms.saturating_mul(4));
            let model = shared_rans_model()?;
            let syms = rans_decode(&model, blob, num_syms, decode_budget).map_err(map_bit_io)?;
            if syms.len() != num_syms {
                return Err(AudioCodecError::InvalidData("v2 rans symbol count"));
            }
            let res_bytes: Vec<u8> = syms.iter().map(|&s| s as u8).collect();
            if res_bytes.len() != expect_bytes {
                return Err(AudioCodecError::InvalidData("v2 residual bytes"));
            }
            let mut residuals = Vec::with_capacity(expect_residuals);
            for chunk in res_bytes.chunks_exact(2) {
                residuals.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
            for i in p..sample_count {
                let pred = predict_i16(&x, i, p, &coef);
                let r = residuals
                    .get(i - p)
                    .ok_or(AudioCodecError::InvalidData("v2 residual idx"))?;
                let v = pred + i32::from(*r);
                if !(i16::MIN as i32..=i16::MAX as i32).contains(&v) {
                    return Err(AudioCodecError::InvalidData("v2 reconstruct range"));
                }
                x[i] = v as i16;
            }
            Ok(x)
        }
        _ => Err(AudioCodecError::InvalidData("v2 unknown channel mode")),
    }
}

fn shared_rans_model() -> Result<RansModel, AudioCodecError> {
    RansModel::uniform(256).map_err(map_bit_io)
}

fn decode_channel_legacy(payload: &[u8], sample_count: usize) -> Result<Vec<i16>, AudioCodecError> {
    decode_channel_legacy_inner(payload, sample_count)
}

fn decode_channel_legacy_inner(
    payload: &[u8],
    sample_count: usize,
) -> Result<Vec<i16>, AudioCodecError> {
    if sample_count == 0 {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        return Err(AudioCodecError::InvalidData(
            "unexpected channel payload for zero sample frame",
        ));
    }
    if payload.len() < 2 {
        return Err(AudioCodecError::InvalidData(
            "truncated channel first sample",
        ));
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
                i32::from(tag) - 192
            };
            let value = prev + delta;
            if !(-32768..=32767).contains(&value) {
                return Err(AudioCodecError::InvalidData(
                    "decoded sample out of i16 range",
                ));
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

fn map_bit_io(e: BitIoError) -> AudioCodecError {
    AudioCodecError::Entropy(e)
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

/// `true` if `v` is a known on-the-wire audio stream/packet version.
pub fn is_supported_stream_version(v: u8) -> bool {
    v == STREAM_VERSION_V1 || v == STREAM_VERSION_V2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sr: u32, ch: u8, idx: u32, samples: Vec<i16>) -> AudioFrame {
        AudioFrame {
            sample_rate: sr,
            channels: ch,
            frame_index: idx,
            samples,
        }
    }

    #[test]
    fn v2_elementary_stream_rejects_missing_magic() {
        let f = frame(48_000, 1, 0, vec![1_i16; 32]);
        let mut blob = encode_frame(&f).unwrap();
        blob[5] = 0;
        blob[6] = 0;
        assert!(decode_frame_with_stream_version(48_000, 0, &blob, STREAM_VERSION_V2).is_err());
    }

    #[test]
    fn round_trip_v2_mono_silence() {
        let f = frame(48_000, 1, 0, vec![0_i16; 64]);
        let blob = encode_frame(&f).unwrap();
        let got = decode_frame(48_000, 0, &blob).unwrap();
        assert_eq!(got, f);
    }

    #[test]
    fn round_trip_v2_stereo_sine() {
        let n = 256_usize;
        let mut interleaved = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = ((i as f64 * 0.17).sin() * 20000.0) as i16;
            interleaved.push(s);
            interleaved.push(s.wrapping_mul(3).wrapping_add(100));
        }
        let f = frame(44_100, 2, 7, interleaved);
        let blob = encode_frame(&f).unwrap();
        let got = decode_frame(44_100, 7, &blob).unwrap();
        assert_eq!(got, f);
    }

    #[test]
    fn round_trip_stream_one_frame() {
        use std::io::Cursor;

        use crate::bitstream::{AudioStreamReader, AudioStreamWriter};

        let f = frame(
            48_000,
            1,
            3,
            (0..128).map(|i| (i as i16).wrapping_mul(31)).collect(),
        );
        let mut buf = Vec::new();
        {
            let mut w = AudioStreamWriter::new(&mut buf, f.sample_rate, f.channels).unwrap();
            w.write_frame(&f).unwrap();
        }
        let mut r = AudioStreamReader::new(Cursor::new(&buf)).unwrap();
        assert_eq!(r.header.stream_version, STREAM_VERSION);
        let got = r.read_next_frame().unwrap().unwrap();
        assert_eq!(got, f);
        assert!(r.read_next_frame().unwrap().is_none());
    }

    #[test]
    fn lpc_walk_residuals_rans_roundtrip() {
        use libsrs_bitio::{rans_decode, rans_encode, RansModel};

        let mut samples = Vec::with_capacity(128);
        let mut left = 19_i16;
        for i in 0..128_usize {
            left = left.wrapping_add((i as i16 % 11) - 5);
            samples.push(left);
        }
        let n = samples.len();
        let p = 8_usize;
        let r = autocorr_i16(&samples, p).unwrap();
        let a_f = levinson_durbin(&r, p).unwrap();
        let coef: Vec<i16> = a_f
            .iter()
            .map(|&c| {
                let q = (c * LPC_SCALE).round() as i32;
                q.clamp(i16::MIN as i32, i16::MAX as i32) as i16
            })
            .collect();
        let mut residuals: Vec<i16> = Vec::with_capacity(n - p);
        for i in p..n {
            let pred = predict_i16(&samples, i, p, &coef);
            let res = samples[i] as i32 - pred;
            assert!((i16::MIN as i32..=i16::MAX as i32).contains(&res));
            residuals.push(res as i16);
        }
        let mut res_bytes = Vec::with_capacity(residuals.len() * 2);
        for r in &residuals {
            res_bytes.extend_from_slice(&r.to_le_bytes());
        }
        let model = RansModel::uniform(256).unwrap();
        let sym: Vec<usize> = res_bytes.iter().map(|&b| usize::from(b)).collect();
        let blob = rans_encode(&model, &sym).unwrap();
        let dec = rans_decode(&model, &blob, sym.len(), blob.len().saturating_mul(32)).unwrap();
        assert_eq!(dec, sym);
    }

    #[test]
    fn round_trip_v2_mono_walk_like_conformance() {
        let mut samples = Vec::with_capacity(128);
        let mut left = 19_i16;
        for i in 0..128_usize {
            left = left.wrapping_add((i as i16 % 11) - 5);
            samples.push(left);
        }
        let f = frame(48_000, 1, 0, samples);
        let blob = encode_frame(&f).unwrap();
        let got = decode_frame(48_000, 0, &blob).unwrap();
        assert_eq!(got, f);
    }

    #[test]
    fn round_trip_v2_conformance_like_walk() {
        let sample_rate = 48_000_u32;
        let channels = 2_u8;
        let mut samples = Vec::with_capacity(128 * channels as usize);
        let mut left = 19_i16;
        let mut right = -19_i16;
        for i in 0..128_usize {
            left = left.wrapping_add((i as i16 % 11) - 5);
            samples.push(left);
            right = right.wrapping_sub((i as i16 % 7) - 3);
            samples.push(right);
        }
        let f = frame(sample_rate, channels, 0, samples);
        let blob = encode_frame(&f).unwrap();
        let got = decode_frame(sample_rate, 0, &blob).unwrap();
        assert_eq!(got, f);
    }

    #[test]
    fn round_trip_v2_empty_frame() {
        let f = frame(48_000, 2, 1, Vec::new());
        let blob = encode_frame(&f).unwrap();
        let got = decode_frame(48_000, 1, &blob).unwrap();
        assert_eq!(got, f);
    }
}
