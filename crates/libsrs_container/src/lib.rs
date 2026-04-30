pub mod codec_ids;

pub use codec_ids::{
    CONTAINER_AUDIO_CODEC_SRSA, CONTAINER_VIDEO_CODEC_SRSV1, CONTAINER_VIDEO_CODEC_SRSV2,
};
pub mod crc;
pub mod format;
pub mod io;
pub mod resync;

pub use crc::{crc32, crc32c};
pub use format::{
    BlockHeader, BlockType, CueBlock, FileHeader, FileProfile, IndexBlock, IndexEntry, Packet,
    PacketFlags, PacketHeader, ReadError, TrackDescriptor, TrackKind, BLOCK_HEADER_LEN,
    BLOCK_MAGIC, CONTAINER_MAGIC, CONTAINER_MAGIC_LEGACY, CONTAINER_VERSION,
    CONTAINER_VERSION_LEGACY, FILE_HEADER_V1_ON_DISK_LEN, FILE_HEADER_V2_ON_DISK_LEN,
    MAX_BLOCK_BODY_BYTES, MAX_INDEX_ENTRIES_PER_BLOCK, MAX_PACKET_PAYLOAD_BYTES, MAX_TRACKS,
    MAX_TRACK_CONFIG_BYTES,
};
pub use io::{
    decode_block_header, decode_cue_block, decode_file_header, decode_index_block,
    decode_packet_block, decode_track_descriptor, encode_block, encode_cue_block,
    encode_file_header, encode_index_block, encode_packet_block, encode_track_descriptor,
    read_block_body, write_all,
};
pub use resync::find_next_block_magic;
