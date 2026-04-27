pub mod crc;
pub mod format;
pub mod io;
pub mod resync;

pub use crc::crc32;
pub use format::{
    BlockHeader, BlockType, CueBlock, FileHeader, IndexBlock, IndexEntry, Packet, PacketFlags,
    PacketHeader, ReadError, TrackDescriptor, TrackKind, BLOCK_HEADER_LEN, BLOCK_MAGIC,
    CONTAINER_MAGIC,
};
pub use io::{
    decode_block_header, decode_cue_block, decode_file_header, decode_index_block,
    decode_packet_block, decode_track_descriptor, encode_block, encode_cue_block,
    encode_file_header, encode_index_block, encode_packet_block, encode_track_descriptor,
    read_block_body, write_all,
};
pub use resync::find_next_block_magic;
