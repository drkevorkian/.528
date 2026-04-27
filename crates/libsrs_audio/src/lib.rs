mod bitstream;
mod codec;
mod error;

pub use bitstream::{
    parse_audio_frame_packet_header, AudioPacketMetadata, AudioStreamHeader, AudioStreamReader,
    AudioStreamWriter,
};
pub use codec::{decode_frame, encode_frame, AudioFrame, PACKET_SYNC, STREAM_MAGIC, STREAM_VERSION};
pub use error::AudioCodecError;
