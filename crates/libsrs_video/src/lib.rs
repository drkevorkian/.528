mod bitstream;
mod codec;
mod error;

pub use bitstream::{
    parse_video_frame_packet_header, FramePacketMetadata, VideoStreamHeader, VideoStreamReader,
    VideoStreamWriter,
};
pub use codec::{
    decode_frame, encode_frame, FrameType, VideoFrame, BLOCK_SIZE, PACKET_SYNC, STREAM_MAGIC,
    STREAM_VERSION,
};
pub use error::VideoCodecError;
