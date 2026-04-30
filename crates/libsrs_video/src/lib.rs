mod bitstream;
mod codec;
mod error;
pub mod srsv2;

pub use bitstream::{
    parse_video_frame_packet_header, FramePacketMetadata, VideoStreamHeader, VideoStreamReader,
    VideoStreamWriter,
};
pub use codec::{
    decode_frame, encode_frame, FrameType, VideoFrame, BLOCK_SIZE, PACKET_SYNC, STREAM_MAGIC,
    STREAM_VERSION,
};
pub use error::VideoCodecError;

pub use srsv2::limits::MAX_LUMA_SAMPLES;
pub use srsv2::model::SEQUENCE_HEADER_BYTES;
pub use srsv2::{
    classify_srsv2_payload, decode_sequence_header_v2, decode_yuv420_intra_payload,
    decode_yuv420_srsv2_payload,
    elementary::{peek_is_srsv2, VideoStreamReaderV2, VideoStreamWriterV2},
    encode_sequence_header_v2, encode_yuv420_inter_payload, encode_yuv420_intra_payload,
    gray8_packed_to_yuv420p8_neutral, rgb888_full_to_yuv420_bt709, yuv420_bt709_to_rgb888_limited,
    ChromaSiting, ColorConvertBackend, ColorPrimaries, ColorRange, CpuVideoAccelerator,
    DecodedVideoFrameV2, GpuVideoAccelerator, MatrixCoefficients, MotionSearchBackend, PixelFormat,
    QuantBackend, ReferenceFrameBuffer, SrsElementaryVideoCodecId, SrsV2EncodeSettings, SrsV2Error,
    SrsVideoCodecId, SrsVideoProfile, Srsv2PayloadKind, TransferFunction, TransformBackend,
    VideoPlane, VideoSequenceHeaderV2, YuvFrame,
};

#[cfg(test)]
mod export_sanity_tests {
    #[test]
    fn max_luma_samples_export_matches_8k_uhd_plane() {
        assert_eq!(crate::MAX_LUMA_SAMPLES, 33_177_600);
    }
}
