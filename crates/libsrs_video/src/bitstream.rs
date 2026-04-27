use std::io::{self, Read, Write};

use crc32fast::Hasher;

use crate::codec::{decode_frame, encode_frame, FrameType, VideoFrame, PACKET_SYNC, STREAM_MAGIC, STREAM_VERSION};
use crate::error::VideoCodecError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoStreamHeader {
    pub width: u32,
    pub height: u32,
}

impl VideoStreamHeader {
    pub fn encode(&self) -> [u8; 16] {
        let mut out = [0_u8; 16];
        out[0..4].copy_from_slice(&STREAM_MAGIC);
        out[4] = STREAM_VERSION;
        out[8..12].copy_from_slice(&self.width.to_le_bytes());
        out[12..16].copy_from_slice(&self.height.to_le_bytes());
        out
    }

    pub fn decode(bytes: [u8; 16]) -> Result<Self, VideoCodecError> {
        if bytes[0..4] != STREAM_MAGIC {
            return Err(VideoCodecError::InvalidData("invalid video stream magic"));
        }
        if bytes[4] != STREAM_VERSION {
            return Err(VideoCodecError::InvalidData("unsupported video stream version"));
        }
        let mut w = [0_u8; 4];
        w.copy_from_slice(&bytes[8..12]);
        let mut h = [0_u8; 4];
        h.copy_from_slice(&bytes[12..16]);
        let width = u32::from_le_bytes(w);
        let height = u32::from_le_bytes(h);
        if width == 0 || height == 0 {
            return Err(VideoCodecError::InvalidData("invalid zero dimension stream"));
        }
        Ok(Self { width, height })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePacketMetadata {
    pub frame_index: u32,
    pub frame_type: FrameType,
    pub payload_len: u32,
    pub crc32: u32,
}

pub struct VideoStreamWriter<W: Write> {
    inner: W,
    header: VideoStreamHeader,
}

impl<W: Write> VideoStreamWriter<W> {
    pub fn new(mut inner: W, width: u32, height: u32) -> Result<Self, VideoCodecError> {
        let header = VideoStreamHeader { width, height };
        inner.write_all(&header.encode())?;
        Ok(Self { inner, header })
    }

    pub fn write_frame(&mut self, frame: &VideoFrame) -> Result<FramePacketMetadata, VideoCodecError> {
        if frame.width != self.header.width || frame.height != self.header.height {
            return Err(VideoCodecError::DimensionMismatch {
                expected_width: self.header.width,
                expected_height: self.header.height,
                actual_width: frame.width,
                actual_height: frame.height,
            });
        }
        let payload = encode_frame(frame)?;
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| VideoCodecError::InvalidData("payload too large"))?;

        let mut crc_hasher = Hasher::new();
        crc_hasher.update(&[STREAM_VERSION, frame.frame_type as u8]);
        crc_hasher.update(&frame.frame_index.to_le_bytes());
        crc_hasher.update(&payload_len.to_le_bytes());
        crc_hasher.update(&payload);
        let crc32 = crc_hasher.finalize();

        self.inner.write_all(&PACKET_SYNC)?;
        self.inner.write_all(&[STREAM_VERSION, frame.frame_type as u8])?;
        self.inner.write_all(&frame.frame_index.to_le_bytes())?;
        self.inner.write_all(&payload_len.to_le_bytes())?;
        self.inner.write_all(&crc32.to_le_bytes())?;
        self.inner.write_all(&payload)?;

        Ok(FramePacketMetadata {
            frame_index: frame.frame_index,
            frame_type: frame.frame_type,
            payload_len,
            crc32,
        })
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

pub struct VideoStreamReader<R: Read> {
    inner: R,
    pub header: VideoStreamHeader,
}

impl<R: Read> VideoStreamReader<R> {
    pub fn new(mut inner: R) -> Result<Self, VideoCodecError> {
        let mut bytes = [0_u8; 16];
        inner.read_exact(&mut bytes)?;
        let header = VideoStreamHeader::decode(bytes)?;
        Ok(Self { inner, header })
    }

    pub fn read_next_frame(&mut self) -> Result<Option<VideoFrame>, VideoCodecError> {
        let mut sync = [0_u8; 2];
        match self.inner.read_exact(&mut sync) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(VideoCodecError::Io(err)),
        }
        if sync != PACKET_SYNC {
            return Err(VideoCodecError::InvalidData("invalid video packet sync"));
        }
        let mut version_and_type = [0_u8; 2];
        self.inner.read_exact(&mut version_and_type)?;
        if version_and_type[0] != STREAM_VERSION {
            return Err(VideoCodecError::InvalidData("unsupported video packet version"));
        }
        let frame_type = FrameType::from_u8(version_and_type[1])?;

        let frame_index = read_u32(&mut self.inner)?;
        let payload_len = read_u32(&mut self.inner)?;
        let expected_crc = read_u32(&mut self.inner)?;
        let mut payload = vec![0_u8; payload_len as usize];
        self.inner.read_exact(&mut payload)?;

        let mut crc_hasher = Hasher::new();
        crc_hasher.update(&version_and_type);
        crc_hasher.update(&frame_index.to_le_bytes());
        crc_hasher.update(&payload_len.to_le_bytes());
        crc_hasher.update(&payload);
        let actual_crc = crc_hasher.finalize();
        if actual_crc != expected_crc {
            return Err(VideoCodecError::CrcMismatch {
                expected: expected_crc,
                actual: actual_crc,
            });
        }

        let frame = decode_frame(
            self.header.width,
            self.header.height,
            frame_index,
            frame_type,
            &payload,
        )?;
        Ok(Some(frame))
    }
}

pub fn parse_video_frame_packet_header(packet: &[u8]) -> Result<FramePacketMetadata, VideoCodecError> {
    if packet.len() < 2 + 2 + 4 + 4 + 4 {
        return Err(VideoCodecError::InvalidData(
            "packet too small for video frame header",
        ));
    }
    if packet[0..2] != PACKET_SYNC {
        return Err(VideoCodecError::InvalidData("invalid video packet sync"));
    }
    if packet[2] != STREAM_VERSION {
        return Err(VideoCodecError::InvalidData("unsupported video packet version"));
    }
    let frame_type = FrameType::from_u8(packet[3])?;
    let mut idx = [0_u8; 4];
    idx.copy_from_slice(&packet[4..8]);
    let mut len = [0_u8; 4];
    len.copy_from_slice(&packet[8..12]);
    let mut crc = [0_u8; 4];
    crc.copy_from_slice(&packet[12..16]);
    Ok(FramePacketMetadata {
        frame_index: u32::from_le_bytes(idx),
        frame_type,
        payload_len: u32::from_le_bytes(len),
        crc32: u32::from_le_bytes(crc),
    })
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, VideoCodecError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}
