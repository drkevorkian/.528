use std::io::{self, Read, Write};

use crc32fast::Hasher;

use crate::codec::{decode_frame, encode_frame, AudioFrame, PACKET_SYNC, STREAM_MAGIC, STREAM_VERSION};
use crate::error::AudioCodecError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamHeader {
    pub sample_rate: u32,
    pub channels: u8,
}

impl AudioStreamHeader {
    pub fn encode(&self) -> [u8; 16] {
        let mut out = [0_u8; 16];
        out[0..4].copy_from_slice(&STREAM_MAGIC);
        out[4] = STREAM_VERSION;
        out[5] = self.channels;
        out[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        out
    }

    pub fn decode(bytes: [u8; 16]) -> Result<Self, AudioCodecError> {
        if bytes[0..4] != STREAM_MAGIC {
            return Err(AudioCodecError::InvalidData("invalid audio stream magic"));
        }
        if bytes[4] != STREAM_VERSION {
            return Err(AudioCodecError::InvalidData("unsupported audio stream version"));
        }
        let channels = bytes[5];
        if channels != 1 && channels != 2 {
            return Err(AudioCodecError::UnsupportedChannels(channels));
        }
        let mut sr = [0_u8; 4];
        sr.copy_from_slice(&bytes[8..12]);
        let sample_rate = u32::from_le_bytes(sr);
        if sample_rate == 0 {
            return Err(AudioCodecError::InvalidData("invalid zero sample rate"));
        }
        Ok(Self {
            sample_rate,
            channels,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacketMetadata {
    pub frame_index: u32,
    pub channels: u8,
    pub sample_count_per_channel: u32,
    pub payload_len: u32,
    pub crc32: u32,
}

pub struct AudioStreamWriter<W: Write> {
    inner: W,
    header: AudioStreamHeader,
}

impl<W: Write> AudioStreamWriter<W> {
    pub fn new(mut inner: W, sample_rate: u32, channels: u8) -> Result<Self, AudioCodecError> {
        if channels != 1 && channels != 2 {
            return Err(AudioCodecError::UnsupportedChannels(channels));
        }
        let header = AudioStreamHeader {
            sample_rate,
            channels,
        };
        inner.write_all(&header.encode())?;
        Ok(Self { inner, header })
    }

    pub fn write_frame(&mut self, frame: &AudioFrame) -> Result<AudioPacketMetadata, AudioCodecError> {
        if frame.sample_rate != self.header.sample_rate {
            return Err(AudioCodecError::InvalidData("sample rate mismatch for stream"));
        }
        if frame.channels != self.header.channels {
            return Err(AudioCodecError::InvalidData("channel count mismatch for stream"));
        }
        let sample_count_per_channel = frame.sample_count_per_channel()?;
        let payload = encode_frame(frame)?;
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| AudioCodecError::InvalidData("audio payload too large"))?;

        let mut crc_hasher = Hasher::new();
        crc_hasher.update(&[STREAM_VERSION, frame.channels]);
        crc_hasher.update(&frame.frame_index.to_le_bytes());
        crc_hasher.update(&sample_count_per_channel.to_le_bytes());
        crc_hasher.update(&payload_len.to_le_bytes());
        crc_hasher.update(&payload);
        let crc32 = crc_hasher.finalize();

        self.inner.write_all(&PACKET_SYNC)?;
        self.inner.write_all(&[STREAM_VERSION, frame.channels])?;
        self.inner.write_all(&frame.frame_index.to_le_bytes())?;
        self.inner.write_all(&sample_count_per_channel.to_le_bytes())?;
        self.inner.write_all(&payload_len.to_le_bytes())?;
        self.inner.write_all(&crc32.to_le_bytes())?;
        self.inner.write_all(&payload)?;

        Ok(AudioPacketMetadata {
            frame_index: frame.frame_index,
            channels: frame.channels,
            sample_count_per_channel,
            payload_len,
            crc32,
        })
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

pub struct AudioStreamReader<R: Read> {
    inner: R,
    pub header: AudioStreamHeader,
}

impl<R: Read> AudioStreamReader<R> {
    pub fn new(mut inner: R) -> Result<Self, AudioCodecError> {
        let mut bytes = [0_u8; 16];
        inner.read_exact(&mut bytes)?;
        let header = AudioStreamHeader::decode(bytes)?;
        Ok(Self { inner, header })
    }

    pub fn read_next_frame(&mut self) -> Result<Option<AudioFrame>, AudioCodecError> {
        let mut sync = [0_u8; 2];
        match self.inner.read_exact(&mut sync) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(AudioCodecError::Io(err)),
        }
        if sync != PACKET_SYNC {
            return Err(AudioCodecError::InvalidData("invalid audio packet sync"));
        }
        let version = read_u8(&mut self.inner)?;
        let channels = read_u8(&mut self.inner)?;
        if version != STREAM_VERSION {
            return Err(AudioCodecError::InvalidData("unsupported audio packet version"));
        }
        if channels != self.header.channels {
            return Err(AudioCodecError::InvalidData("packet channel mismatch"));
        }
        let frame_index = read_u32(&mut self.inner)?;
        let _sample_count = read_u32(&mut self.inner)?;
        let payload_len = read_u32(&mut self.inner)?;
        let expected_crc = read_u32(&mut self.inner)?;
        let mut payload = vec![0_u8; payload_len as usize];
        self.inner.read_exact(&mut payload)?;

        let mut crc_hasher = Hasher::new();
        crc_hasher.update(&[version, channels]);
        crc_hasher.update(&frame_index.to_le_bytes());
        crc_hasher.update(&_sample_count.to_le_bytes());
        crc_hasher.update(&payload_len.to_le_bytes());
        crc_hasher.update(&payload);
        let actual_crc = crc_hasher.finalize();
        if actual_crc != expected_crc {
            return Err(AudioCodecError::CrcMismatch {
                expected: expected_crc,
                actual: actual_crc,
            });
        }

        let frame = decode_frame(self.header.sample_rate, frame_index, &payload)?;
        Ok(Some(frame))
    }
}

pub fn parse_audio_frame_packet_header(packet: &[u8]) -> Result<AudioPacketMetadata, AudioCodecError> {
    if packet.len() < 2 + 2 + 4 + 4 + 4 + 4 {
        return Err(AudioCodecError::InvalidData(
            "packet too small for audio header",
        ));
    }
    if packet[0..2] != PACKET_SYNC {
        return Err(AudioCodecError::InvalidData("invalid audio packet sync"));
    }
    if packet[2] != STREAM_VERSION {
        return Err(AudioCodecError::InvalidData("unsupported audio packet version"));
    }
    let channels = packet[3];
    if channels != 1 && channels != 2 {
        return Err(AudioCodecError::UnsupportedChannels(channels));
    }
    let mut frame_index = [0_u8; 4];
    frame_index.copy_from_slice(&packet[4..8]);
    let mut sample_count = [0_u8; 4];
    sample_count.copy_from_slice(&packet[8..12]);
    let mut payload_len = [0_u8; 4];
    payload_len.copy_from_slice(&packet[12..16]);
    let mut crc = [0_u8; 4];
    crc.copy_from_slice(&packet[16..20]);
    Ok(AudioPacketMetadata {
        frame_index: u32::from_le_bytes(frame_index),
        channels,
        sample_count_per_channel: u32::from_le_bytes(sample_count),
        payload_len: u32::from_le_bytes(payload_len),
        crc32: u32::from_le_bytes(crc),
    })
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8, AudioCodecError> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, AudioCodecError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}
