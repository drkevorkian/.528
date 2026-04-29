use std::fmt::{Display, Formatter};

/// Legacy 4-byte file magic (v1 readers only).
pub const CONTAINER_MAGIC_LEGACY: [u8; 4] = *b"SRSM";

/// v2+ primary file magic: `SRS528` followed by two reserved NUL bytes (8 bytes, little-endian file id).
pub const CONTAINER_MAGIC: [u8; 8] = [b'S', b'R', b'S', b'5', b'2', b'8', 0, 0];

/// Block envelope magic (unchanged across v1/v2).
pub const BLOCK_MAGIC: [u8; 4] = *b"SBLK";
pub const BLOCK_HEADER_LEN: usize = 20;

/// On-disk format version written in the file header **after** the magic.
pub const CONTAINER_VERSION_LEGACY: u16 = 1;
pub const CONTAINER_VERSION: u16 = 2;

// --- Defensive maximums (hostile / malformed input) ---

/// Maximum tracks in one file (table size cap).
pub const MAX_TRACKS: u16 = 1024;

/// Codec private / track `config` blob size.
pub const MAX_TRACK_CONFIG_BYTES: usize = 1024 * 1024;

/// Maximum encoded packet payload bytes (application may choose a lower ceiling).
pub const MAX_PACKET_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Maximum block body size read from a bitstream (bounds allocations).
pub const MAX_BLOCK_BODY_BYTES: u32 = 16 * 1024 * 1024;

/// Maximum index / cue entries per block (additional cap beyond body size).
pub const MAX_INDEX_ENTRIES_PER_BLOCK: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrackKind {
    Audio = 1,
    Video = 2,
    /// Opaque data / subtitles / timed text (interpreted by codec_id).
    Data = 3,
    Subtitle = 4,
    Metadata = 5,
    Attachment = 6,
}

impl TryFrom<u8> for TrackKind {
    type Error = ReadError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Audio),
            2 => Ok(Self::Video),
            3 => Ok(Self::Data),
            4 => Ok(Self::Subtitle),
            5 => Ok(Self::Metadata),
            6 => Ok(Self::Attachment),
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

/// High-level profile hint stored in `FileHeader::flags` bits 0..3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum FileProfile {
    Unknown = 0,
    Lossless = 1,
    Visual = 2,
    AudioOnly = 3,
    VideoOnly = 4,
    Mixed = 5,
}

impl FileProfile {
    pub const FLAGS_SHIFT: u16 = 0;
    pub const FLAGS_MASK: u16 = 0x000F;

    pub fn from_flags(flags: u16) -> Self {
        let code = flags & Self::FLAGS_MASK;
        match code {
            1 => Self::Lossless,
            2 => Self::Visual,
            3 => Self::AudioOnly,
            4 => Self::VideoOnly,
            5 => Self::Mixed,
            _ => Self::Unknown,
        }
    }

    pub fn encode(self) -> u16 {
        (self as u16) & Self::FLAGS_MASK
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub version: u16,
    pub flags: u16,
    /// Byte length of this fixed file header on disk (magic + fields), not including track table.
    pub header_len: u32,
    pub track_count: u16,
    pub cue_interval_packets: u32,
}

impl FileHeader {
    /// New v2 `.528` header with default profile and cue cadence.
    pub fn new(track_count: u16, cue_interval_packets: u32) -> Self {
        Self {
            version: CONTAINER_VERSION,
            flags: FileProfile::Mixed.encode(),
            header_len: FILE_HEADER_V2_ON_DISK_LEN,
            track_count,
            cue_interval_packets,
        }
    }

    /// Legacy v1 SRSM header layout (for tests / compatibility helpers).
    pub fn new_legacy(track_count: u16, cue_interval_packets: u32) -> Self {
        Self {
            version: CONTAINER_VERSION_LEGACY,
            flags: 0,
            header_len: FILE_HEADER_V1_ON_DISK_LEN,
            track_count,
            cue_interval_packets,
        }
    }

    pub fn profile(&self) -> FileProfile {
        FileProfile::from_flags(self.flags)
    }

    /// Whether block bodies use CRC-32C (`true`) or legacy CRC-32 (`false`).
    pub fn block_checksum_is_crc32c(&self) -> bool {
        self.version >= CONTAINER_VERSION
    }
}

/// On-disk size: 4-byte legacy magic + 16 bytes fields.
pub const FILE_HEADER_V1_ON_DISK_LEN: u32 = 20;
/// On-disk size: 8-byte v2 magic + 16 bytes fields.
pub const FILE_HEADER_V2_ON_DISK_LEN: u32 = 24;

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
    pub const DISCARDABLE: u16 = 1 << 3;
    pub const CORRUPT: u16 = 1 << 4;
    /// Reserved for future encrypted payload signaling (do not interpret as security today).
    pub const ENCRYPTED_RESERVED: u16 = 1 << 5;
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
    LimitExceeded(&'static str),
    InvalidHeaderLayout { expected_len: u32, actual: u32 },
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
            Self::LimitExceeded(ctx) => write!(f, "limit exceeded: {ctx}"),
            Self::InvalidHeaderLayout {
                expected_len,
                actual,
            } => write!(
                f,
                "header length mismatch: on-disk header_len={actual}, expected={expected_len}"
            ),
        }
    }
}

impl std::error::Error for ReadError {}
