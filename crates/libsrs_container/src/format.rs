use std::fmt::{Display, Formatter};

pub const CONTAINER_MAGIC: [u8; 4] = *b"SRSM";
pub const BLOCK_MAGIC: [u8; 4] = *b"SBLK";
pub const BLOCK_HEADER_LEN: usize = 20;
pub const CONTAINER_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrackKind {
    Audio = 1,
    Video = 2,
    Data = 3,
}

impl TryFrom<u8> for TrackKind {
    type Error = ReadError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Audio),
            2 => Ok(Self::Video),
            3 => Ok(Self::Data),
            _ => Err(ReadError::InvalidTrackKind(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockType {
    Packet = 1,
    Cue = 2,
    Index = 3,
}

impl TryFrom<u8> for BlockType {
    type Error = ReadError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Packet),
            2 => Ok(Self::Cue),
            3 => Ok(Self::Index),
            _ => Err(ReadError::InvalidBlockType(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub version: u16,
    pub flags: u16,
    pub header_len: u32,
    pub track_count: u16,
    pub cue_interval_packets: u32,
}

impl FileHeader {
    pub fn new(track_count: u16, cue_interval_packets: u32) -> Self {
        Self {
            version: CONTAINER_VERSION,
            flags: 0,
            header_len: 20,
            track_count,
            cue_interval_packets,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDescriptor {
    pub track_id: u16,
    pub kind: TrackKind,
    pub codec_id: u16,
    pub flags: u16,
    pub timescale: u32,
    pub config: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
    pub block_type: BlockType,
    pub flags: u8,
    pub body_len: u32,
    pub body_crc32: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketFlags;

impl PacketFlags {
    pub const KEYFRAME: u16 = 1 << 0;
    pub const CONFIG: u16 = 1 << 1;
    pub const DISCONTINUITY: u16 = 1 << 2;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketHeader {
    pub track_id: u16,
    pub flags: u16,
    pub sequence: u64,
    pub pts: u64,
    pub dts: u64,
    pub payload_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: PacketHeader,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub packet_number: u64,
    pub file_offset: u64,
    pub track_id: u16,
    pub flags: u16,
    pub pts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueBlock {
    pub cue_id: u64,
    pub first_packet_number: u64,
    pub entries: Vec<IndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBlock {
    pub entries: Vec<IndexEntry>,
}

#[derive(Debug)]
pub enum ReadError {
    InvalidMagic([u8; 4]),
    UnsupportedVersion(u16),
    InvalidTrackKind(u8),
    InvalidBlockType(u8),
    InvalidLength(&'static str),
    InvalidHeaderCrc { expected: u32, actual: u32 },
    InvalidBodyCrc { expected: u32, actual: u32 },
}

impl Display for ReadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMagic(value) => write!(f, "invalid magic: {value:?}"),
            Self::UnsupportedVersion(value) => write!(f, "unsupported version: {value}"),
            Self::InvalidTrackKind(value) => write!(f, "invalid track kind: {value}"),
            Self::InvalidBlockType(value) => write!(f, "invalid block type: {value}"),
            Self::InvalidLength(ctx) => write!(f, "invalid length in {ctx}"),
            Self::InvalidHeaderCrc { expected, actual } => {
                write!(f, "header crc mismatch expected={expected} actual={actual}")
            }
            Self::InvalidBodyCrc { expected, actual } => {
                write!(f, "body crc mismatch expected={expected} actual={actual}")
            }
        }
    }
}

impl std::error::Error for ReadError {}
