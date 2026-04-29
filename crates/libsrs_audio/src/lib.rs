mod bitstream;
mod codec;
mod error;
mod lpc;

pub use bitstream::{
    parse_audio_frame_packet_header, AudioPacketMetadata, AudioStreamHeader, AudioStreamReader,
    AudioStreamWriter,
};
pub use codec::{
    decode_frame, decode_frame_with_stream_version, encode_frame, is_supported_stream_version,
    AudioFrame, PACKET_SYNC, PAYLOAD_V2_MAGIC, STREAM_MAGIC, STREAM_VERSION, STREAM_VERSION_V1,
    STREAM_VERSION_V2,
};
pub use error::AudioCodecError;
