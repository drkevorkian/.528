use std::io::{self, ErrorKind, Read, Write};

use crate::crc::{crc32, crc32c};
use crate::format::{
    BlockHeader, BlockType, CueBlock, FileHeader, IndexBlock, IndexEntry, Packet, PacketHeader,
    ReadError, TrackDescriptor, TrackKind, BLOCK_HEADER_LEN, BLOCK_MAGIC, CONTAINER_MAGIC,
    CONTAINER_MAGIC_LEGACY, CONTAINER_VERSION, CONTAINER_VERSION_LEGACY,
    FILE_HEADER_V1_ON_DISK_LEN, FILE_HEADER_V2_ON_DISK_LEN, MAX_BLOCK_BODY_BYTES,
    MAX_INDEX_ENTRIES_PER_BLOCK, MAX_PACKET_PAYLOAD_BYTES, MAX_TRACKS, MAX_TRACK_CONFIG_BYTES,
};

fn io_err(err: ReadError) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, err)
}

fn read_exact_array<const N: usize, R: Read>(reader: &mut R) -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

fn checksum_body(body: &[u8], crc32c_mode: bool) -> u32 {
    if crc32c_mode {
        crc32c(body)
    } else {
        crc32(body)
    }
}

fn checksum_prelude(prelude: &[u8], crc32c_mode: bool) -> u32 {
    checksum_body(prelude, crc32c_mode)
}

/// Writes a v1 (SRSM) or v2 (`SRS528\0\0`) file preamble from `header`.
pub fn encode_file_header(header: &FileHeader) -> io::Result<Vec<u8>> {
    if header.track_count > MAX_TRACKS {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "track_count exceeds MAX_TRACKS",
        ));
    }
    match header.version {
        CONTAINER_VERSION_LEGACY => {
            if header.header_len != FILE_HEADER_V1_ON_DISK_LEN {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "v1 header_len must be FILE_HEADER_V1_ON_DISK_LEN",
                ));
            }
            let mut out = Vec::with_capacity(FILE_HEADER_V1_ON_DISK_LEN as usize);
            out.extend_from_slice(&CONTAINER_MAGIC_LEGACY);
            append_header_fields(&mut out, header);
            Ok(out)
        }
        CONTAINER_VERSION => {
            if header.header_len != FILE_HEADER_V2_ON_DISK_LEN {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "v2 header_len must be FILE_HEADER_V2_ON_DISK_LEN",
                ));
            }
            let mut out = Vec::with_capacity(FILE_HEADER_V2_ON_DISK_LEN as usize);
            out.extend_from_slice(&CONTAINER_MAGIC);
            append_header_fields(&mut out, header);
            Ok(out)
        }
        other => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsupported container version for encode: {other}"),
        )),
    }
}

fn append_header_fields(out: &mut Vec<u8>, header: &FileHeader) {
    out.extend_from_slice(&header.version.to_le_bytes());
    out.extend_from_slice(&header.flags.to_le_bytes());
    out.extend_from_slice(&header.header_len.to_le_bytes());
    out.extend_from_slice(&header.track_count.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&header.cue_interval_packets.to_le_bytes());
}

/// Decodes file preamble. Supports legacy `SRSM` (v1) and `SRS528\0\0` (v2+).
pub fn decode_file_header<R: Read>(reader: &mut R) -> io::Result<FileHeader> {
    let prefix4 = read_exact_array::<4, _>(reader)?;
    let (magic_is_v2, header) = if prefix4 == CONTAINER_MAGIC_LEGACY {
        let rest = read_exact_array::<16, _>(reader)?;
        let header = parse_header_fields(&rest)?;
        (false, header)
    } else {
        let mut magic8 = [0u8; 8];
        magic8[0..4].copy_from_slice(&prefix4);
        reader.read_exact(&mut magic8[4..8])?;
        if magic8 != CONTAINER_MAGIC {
            return Err(io_err(ReadError::InvalidMagic([
                magic8[0], magic8[1], magic8[2], magic8[3],
            ])));
        }
        let rest = read_exact_array::<16, _>(reader)?;
        let header = parse_header_fields(&rest)?;
        (true, header)
    };

    validate_file_header_layout(&header, magic_is_v2).map_err(io_err)?;

    if header.track_count > MAX_TRACKS {
        return Err(io_err(ReadError::LimitExceeded("track_count > MAX_TRACKS")));
    }

    Ok(header)
}

fn parse_header_fields(rest: &[u8; 16]) -> io::Result<FileHeader> {
    let version = u16::from_le_bytes([rest[0], rest[1]]);
    if version != CONTAINER_VERSION_LEGACY && version != CONTAINER_VERSION {
        return Err(io_err(ReadError::UnsupportedVersion(version)));
    }
    let flags = u16::from_le_bytes([rest[2], rest[3]]);
    let header_len = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
    let track_count = u16::from_le_bytes([rest[8], rest[9]]);
    let _reserved = u16::from_le_bytes([rest[10], rest[11]]);
    let cue_interval_packets = u32::from_le_bytes([rest[12], rest[13], rest[14], rest[15]]);
    Ok(FileHeader {
        version,
        flags,
        header_len,
        track_count,
        cue_interval_packets,
    })
}

fn validate_file_header_layout(header: &FileHeader, magic_is_v2: bool) -> Result<(), ReadError> {
    let expected = if magic_is_v2 {
        FILE_HEADER_V2_ON_DISK_LEN
    } else {
        FILE_HEADER_V1_ON_DISK_LEN
    };
    if header.header_len != expected {
        return Err(ReadError::InvalidHeaderLayout {
            expected_len: expected,
            actual: header.header_len,
        });
    }
    if magic_is_v2 && header.version < CONTAINER_VERSION {
        return Err(ReadError::UnsupportedVersion(header.version));
    }
    if !magic_is_v2 && header.version != CONTAINER_VERSION_LEGACY {
        return Err(ReadError::UnsupportedVersion(header.version));
    }
    Ok(())
}

pub fn encode_track_descriptor(track: &TrackDescriptor) -> io::Result<Vec<u8>> {
    if track.config.len() > MAX_TRACK_CONFIG_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "track config exceeds MAX_TRACK_CONFIG_BYTES",
        ));
    }
    let config_len = u32::try_from(track.config.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "track config too large"))?;
    let mut out = Vec::with_capacity(16 + track.config.len());
    out.extend_from_slice(&track.track_id.to_le_bytes());
    out.push(track.kind as u8);
    out.push(0);
    out.extend_from_slice(&track.codec_id.to_le_bytes());
    out.extend_from_slice(&track.flags.to_le_bytes());
    out.extend_from_slice(&track.timescale.to_le_bytes());
    out.extend_from_slice(&config_len.to_le_bytes());
    out.extend_from_slice(&track.config);
    Ok(out)
}

pub fn decode_track_descriptor<R: Read>(reader: &mut R) -> io::Result<TrackDescriptor> {
    let track_id = u16::from_le_bytes(read_exact_array::<2, _>(reader)?);
    let kind_raw = read_exact_array::<1, _>(reader)?[0];
    let kind = TrackKind::try_from(kind_raw).map_err(io_err)?;
    let _reserved = read_exact_array::<1, _>(reader)?;
    let codec_id = u16::from_le_bytes(read_exact_array::<2, _>(reader)?);
    let flags = u16::from_le_bytes(read_exact_array::<2, _>(reader)?);
    let timescale = u32::from_le_bytes(read_exact_array::<4, _>(reader)?);
    let config_len_u32 = u32::from_le_bytes(read_exact_array::<4, _>(reader)?);
    let config_len = usize::try_from(config_len_u32).map_err(|_| {
        io_err(ReadError::InvalidLength(
            "track config length does not fit usize",
        ))
    })?;
    if config_len > MAX_TRACK_CONFIG_BYTES {
        return Err(io_err(ReadError::LimitExceeded(
            "track config > MAX_TRACK_CONFIG_BYTES",
        )));
    }
    let mut config = vec![0u8; config_len];
    reader.read_exact(&mut config)?;
    Ok(TrackDescriptor {
        track_id,
        kind,
        codec_id,
        flags,
        timescale,
        config,
    })
}

pub fn encode_block(header: &BlockHeader, body: &[u8], use_crc32c: bool) -> Vec<u8> {
    let mut prelude = Vec::with_capacity(16);
    prelude.extend_from_slice(&BLOCK_MAGIC);
    prelude.push(header.block_type as u8);
    prelude.push(header.flags);
    prelude.extend_from_slice(&0u16.to_le_bytes());
    prelude.extend_from_slice(&header.body_len.to_le_bytes());
    prelude.extend_from_slice(&header.body_crc32.to_le_bytes());
    let header_crc = checksum_prelude(&prelude, use_crc32c);

    let mut out = Vec::with_capacity(BLOCK_HEADER_LEN + body.len());
    out.extend_from_slice(&prelude);
    out.extend_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(body);
    out
}

pub fn decode_block_header<R: Read>(reader: &mut R, use_crc32c: bool) -> io::Result<BlockHeader> {
    let mut prelude = [0u8; 16];
    reader.read_exact(&mut prelude)?;
    if prelude[0..4] != BLOCK_MAGIC {
        return Err(io_err(ReadError::InvalidMagic([
            prelude[0], prelude[1], prelude[2], prelude[3],
        ])));
    }
    let expected_crc = u32::from_le_bytes(read_exact_array::<4, _>(reader)?);
    let actual_crc = checksum_prelude(&prelude, use_crc32c);
    if expected_crc != actual_crc {
        return Err(io_err(ReadError::InvalidHeaderCrc {
            expected: expected_crc,
            actual: actual_crc,
        }));
    }
    let block_type = BlockType::try_from(prelude[4]).map_err(io_err)?;
    let flags = prelude[5];
    let body_len = u32::from_le_bytes([prelude[8], prelude[9], prelude[10], prelude[11]]);
    let body_crc32 = u32::from_le_bytes([prelude[12], prelude[13], prelude[14], prelude[15]]);
    if body_len > MAX_BLOCK_BODY_BYTES {
        return Err(io_err(ReadError::LimitExceeded(
            "block body_len > MAX_BLOCK_BODY_BYTES",
        )));
    }
    Ok(BlockHeader {
        block_type,
        flags,
        body_len,
        body_crc32,
    })
}

pub fn encode_packet_block(packet: &Packet, use_crc32c: bool) -> io::Result<Vec<u8>> {
    if packet.payload.len() > MAX_PACKET_PAYLOAD_BYTES as usize {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "packet payload exceeds MAX_PACKET_PAYLOAD_BYTES",
        ));
    }
    let payload_len = u32::try_from(packet.payload.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "packet payload too large"))?;
    let mut body = Vec::with_capacity(32 + packet.payload.len());
    body.extend_from_slice(&packet.header.track_id.to_le_bytes());
    body.extend_from_slice(&packet.header.flags.to_le_bytes());
    body.extend_from_slice(&packet.header.sequence.to_le_bytes());
    body.extend_from_slice(&packet.header.pts.to_le_bytes());
    body.extend_from_slice(&packet.header.dts.to_le_bytes());
    body.extend_from_slice(&payload_len.to_le_bytes());
    body.extend_from_slice(&packet.payload);
    let header = BlockHeader {
        block_type: BlockType::Packet,
        flags: 0,
        body_len: u32::try_from(body.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "packet block body too large"))?,
        body_crc32: checksum_body(&body, use_crc32c),
    };
    Ok(encode_block(&header, &body, use_crc32c))
}

pub fn decode_packet_block(body: &[u8]) -> io::Result<Packet> {
    if body.len() < 32 {
        return Err(io_err(ReadError::InvalidLength("packet block")));
    }
    let track_id = u16::from_le_bytes([body[0], body[1]]);
    let flags = u16::from_le_bytes([body[2], body[3]]);
    let sequence = u64::from_le_bytes([
        body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
    ]);
    let pts = u64::from_le_bytes([
        body[12], body[13], body[14], body[15], body[16], body[17], body[18], body[19],
    ]);
    let dts = u64::from_le_bytes([
        body[20], body[21], body[22], body[23], body[24], body[25], body[26], body[27],
    ]);
    let payload_len = u32::from_le_bytes([body[28], body[29], body[30], body[31]]);
    if payload_len > MAX_PACKET_PAYLOAD_BYTES {
        return Err(io_err(ReadError::LimitExceeded(
            "packet payload_len > MAX_PACKET_PAYLOAD_BYTES",
        )));
    }
    let payload_len_usize = usize::try_from(payload_len).map_err(|_| {
        io_err(ReadError::InvalidLength(
            "packet payload length does not fit usize",
        ))
    })?;
    if body.len() != 32 + payload_len_usize {
        return Err(io_err(ReadError::InvalidLength("packet payload")));
    }
    let payload = body[32..].to_vec();
    Ok(Packet {
        header: PacketHeader {
            track_id,
            flags,
            sequence,
            pts,
            dts,
            payload_len,
        },
        payload,
    })
}

fn encode_index_entry(entry: &IndexEntry, out: &mut Vec<u8>) {
    out.extend_from_slice(&entry.packet_number.to_le_bytes());
    out.extend_from_slice(&entry.file_offset.to_le_bytes());
    out.extend_from_slice(&entry.track_id.to_le_bytes());
    out.extend_from_slice(&entry.flags.to_le_bytes());
    out.extend_from_slice(&entry.pts.to_le_bytes());
}

fn decode_index_entry(chunk: &[u8]) -> IndexEntry {
    IndexEntry {
        packet_number: u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]),
        file_offset: u64::from_le_bytes([
            chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15],
        ]),
        track_id: u16::from_le_bytes([chunk[16], chunk[17]]),
        flags: u16::from_le_bytes([chunk[18], chunk[19]]),
        pts: u64::from_le_bytes([
            chunk[20], chunk[21], chunk[22], chunk[23], chunk[24], chunk[25], chunk[26], chunk[27],
        ]),
    }
}

fn cap_index_count(payload_len: usize) -> Result<u32, ReadError> {
    if !payload_len.is_multiple_of(28) {
        return Err(ReadError::InvalidLength("index entries size"));
    }
    let n = payload_len / 28;
    let n_u32 = u32::try_from(n).map_err(|_| ReadError::InvalidLength("index entry count"))?;
    if n_u32 > MAX_INDEX_ENTRIES_PER_BLOCK {
        return Err(ReadError::LimitExceeded(
            "index entry count > MAX_INDEX_ENTRIES_PER_BLOCK",
        ));
    }
    Ok(n_u32)
}

pub fn encode_cue_block(cue: &CueBlock, use_crc32c: bool) -> io::Result<Vec<u8>> {
    if cue.entries.len() > MAX_INDEX_ENTRIES_PER_BLOCK as usize {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "cue entries > MAX_INDEX_ENTRIES_PER_BLOCK",
        ));
    }
    let count = u32::try_from(cue.entries.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "cue entry count too large"))?;
    let mut body = Vec::with_capacity(20 + cue.entries.len() * 28);
    body.extend_from_slice(&cue.cue_id.to_le_bytes());
    body.extend_from_slice(&cue.first_packet_number.to_le_bytes());
    body.extend_from_slice(&count.to_le_bytes());
    for entry in &cue.entries {
        encode_index_entry(entry, &mut body);
    }
    let header = BlockHeader {
        block_type: BlockType::Cue,
        flags: 0,
        body_len: u32::try_from(body.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "cue body too large"))?,
        body_crc32: checksum_body(&body, use_crc32c),
    };
    Ok(encode_block(&header, &body, use_crc32c))
}

pub fn decode_cue_block(body: &[u8]) -> io::Result<CueBlock> {
    if body.len() < 20 {
        return Err(io_err(ReadError::InvalidLength("cue block")));
    }
    let cue_id = u64::from_le_bytes([
        body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
    ]);
    let first_packet_number = u64::from_le_bytes([
        body[8], body[9], body[10], body[11], body[12], body[13], body[14], body[15],
    ]);
    let count = u32::from_le_bytes([body[16], body[17], body[18], body[19]]);
    let payload = &body[20..];
    let expected_len = usize::try_from(count)
        .map(|c| c.saturating_mul(28))
        .map_err(|_| io_err(ReadError::InvalidLength("cue entry count")))?;
    if payload.len() != expected_len {
        return Err(io_err(ReadError::InvalidLength("cue index entries")));
    }
    let _ = cap_index_count(payload.len()).map_err(io_err)?;
    let entries = payload.chunks_exact(28).map(decode_index_entry).collect();
    Ok(CueBlock {
        cue_id,
        first_packet_number,
        entries,
    })
}

pub fn encode_index_block(index: &IndexBlock, use_crc32c: bool) -> io::Result<Vec<u8>> {
    if index.entries.len() > MAX_INDEX_ENTRIES_PER_BLOCK as usize {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "index entries > MAX_INDEX_ENTRIES_PER_BLOCK",
        ));
    }
    let count = u32::try_from(index.entries.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "index entry count too large"))?;
    let mut body = Vec::with_capacity(4 + index.entries.len() * 28);
    body.extend_from_slice(&count.to_le_bytes());
    for entry in &index.entries {
        encode_index_entry(entry, &mut body);
    }
    let header = BlockHeader {
        block_type: BlockType::Index,
        flags: 0,
        body_len: u32::try_from(body.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "index body too large"))?,
        body_crc32: checksum_body(&body, use_crc32c),
    };
    Ok(encode_block(&header, &body, use_crc32c))
}

pub fn decode_index_block(body: &[u8]) -> io::Result<IndexBlock> {
    if body.len() < 4 {
        return Err(io_err(ReadError::InvalidLength("index block")));
    }
    let count = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let payload = &body[4..];
    let expected_len = usize::try_from(count)
        .map(|c| c.saturating_mul(28))
        .map_err(|_| io_err(ReadError::InvalidLength("index entry count")))?;
    if payload.len() != expected_len {
        return Err(io_err(ReadError::InvalidLength("index entries")));
    }
    let _ = cap_index_count(payload.len()).map_err(io_err)?;
    let entries = payload.chunks_exact(28).map(decode_index_entry).collect();
    Ok(IndexBlock { entries })
}

pub fn read_block_body<R: Read>(
    reader: &mut R,
    header: &BlockHeader,
    use_crc32c: bool,
) -> io::Result<Vec<u8>> {
    if header.body_len > MAX_BLOCK_BODY_BYTES {
        return Err(io_err(ReadError::LimitExceeded(
            "block body_len > MAX_BLOCK_BODY_BYTES",
        )));
    }
    let len = usize::try_from(header.body_len)
        .map_err(|_| io_err(ReadError::InvalidLength("block body length")))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let actual_crc = checksum_body(&body, use_crc32c);
    if actual_crc != header.body_crc32 {
        return Err(io_err(ReadError::InvalidBodyCrc {
            expected: header.body_crc32,
            actual: actual_crc,
        }));
    }
    Ok(body)
}

pub fn write_all<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)
}
