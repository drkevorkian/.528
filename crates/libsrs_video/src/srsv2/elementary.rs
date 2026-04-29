//! `.srsv2` elementary stream: `SRS2` sequence header + CRC-checked framed payloads.

use std::io::{Read, Write};

use crc32fast::Hasher;

use super::error::SrsV2Error;
use super::limits::MAX_FRAME_PAYLOAD_BYTES;
use super::model::{decode_sequence_header_v2, encode_sequence_header_v2, VideoSequenceHeaderV2};
use crate::codec::PACKET_SYNC;

pub const SRSV2_STREAM_PACKET_VERSION: u8 = 2;

pub struct VideoStreamWriterV2<W: Write> {
    inner: W,
}

impl<W: Write> VideoStreamWriterV2<W> {
    pub fn new(mut inner: W, seq: &VideoSequenceHeaderV2) -> Result<Self, SrsV2Error> {
        let hdr = encode_sequence_header_v2(seq);
        inner.write_all(&hdr)?;
        Ok(Self { inner })
    }

    pub fn write_frame_payload(
        &mut self,
        frame_index: u32,
        payload: &[u8],
    ) -> Result<(), SrsV2Error> {
        if payload.len() > MAX_FRAME_PAYLOAD_BYTES {
            return Err(SrsV2Error::AllocationLimit {
                context: "elementary frame payload",
            });
        }
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| SrsV2Error::syntax("payload length"))?;

        let mut crc_hasher = Hasher::new();
        crc_hasher.update(&[SRSV2_STREAM_PACKET_VERSION, 0]); // frame type placeholder I only
        crc_hasher.update(&frame_index.to_le_bytes());
        crc_hasher.update(&payload_len.to_le_bytes());
        crc_hasher.update(payload);
        let crc = crc_hasher.finalize();

        self.inner.write_all(&PACKET_SYNC)?;
        self.inner.write_all(&[SRSV2_STREAM_PACKET_VERSION, 0])?;
        self.inner.write_all(&frame_index.to_le_bytes())?;
        self.inner.write_all(&payload_len.to_le_bytes())?;
        self.inner.write_all(&crc.to_le_bytes())?;
        self.inner.write_all(payload)?;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

pub struct VideoStreamReaderV2<R: Read> {
    inner: R,
    pub seq: VideoSequenceHeaderV2,
}

impl<R: Read> VideoStreamReaderV2<R> {
    pub fn new(mut inner: R) -> Result<Self, SrsV2Error> {
        let mut hdr = [0_u8; super::model::SEQUENCE_HEADER_BYTES];
        inner.read_exact(&mut hdr)?;
        let seq = decode_sequence_header_v2(&hdr)?;
        Ok(Self { inner, seq })
    }

    pub fn read_next_payload(&mut self) -> Result<Option<(u32, Vec<u8>)>, SrsV2Error> {
        let mut sync = [0_u8; 2];
        match self.inner.read_exact(&mut sync) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        if sync != PACKET_SYNC {
            return Err(SrsV2Error::syntax("bad VP sync in srsv2"));
        }
        let mut vt = [0_u8; 2];
        self.inner.read_exact(&mut vt)?;
        if vt[0] != SRSV2_STREAM_PACKET_VERSION {
            return Err(SrsV2Error::UnsupportedVersion(vt[0]));
        }
        let mut idx = [0_u8; 4];
        self.inner.read_exact(&mut idx)?;
        let frame_index = u32::from_le_bytes(idx);
        let mut lenb = [0_u8; 4];
        self.inner.read_exact(&mut lenb)?;
        let payload_len = u32::from_le_bytes(lenb) as usize;
        if payload_len > MAX_FRAME_PAYLOAD_BYTES {
            return Err(SrsV2Error::LimitExceeded("elementary payload"));
        }
        let mut crc_expected = [0_u8; 4];
        self.inner.read_exact(&mut crc_expected)?;
        let mut payload = vec![0_u8; payload_len];
        self.inner.read_exact(&mut payload)?;

        let mut crc_hasher = Hasher::new();
        crc_hasher.update(&vt);
        crc_hasher.update(&idx);
        crc_hasher.update(&lenb);
        crc_hasher.update(&payload);
        let actual = crc_hasher.finalize();
        let expected = u32::from_le_bytes(crc_expected);
        if actual != expected {
            return Err(SrsV2Error::syntax("crc mismatch srsv2"));
        }
        Ok(Some((frame_index, payload)))
    }
}

/// Detect SRSV2 elementary stream from first 4 bytes (`SRS2`).
pub fn peek_is_srsv2(first_four: &[u8]) -> bool {
    first_four.len() >= 4 && first_four[0..4] == super::model::SEQUENCE_MAGIC
}
