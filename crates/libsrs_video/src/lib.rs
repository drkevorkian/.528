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
    apply_loop_filter_y, apply_reconstruction_filter_if_enabled, blend_weighted_pixels,
    choose_b_macroblock, classify_srsv2_payload, decode_sequence_header_v2,
    decode_yuv420_alt_ref_payload, decode_yuv420_b_payload, decode_yuv420_intra_payload,
    decode_yuv420_srsv2_payload, decode_yuv420_srsv2_payload_managed,
    elementary::{peek_is_srsv2, VideoStreamReaderV2, VideoStreamWriterV2},
    encode_sequence_header_v2, encode_yuv420_alt_ref_payload, encode_yuv420_b_payload,
    encode_yuv420_b_payload_mb_blend, encode_yuv420_inter_payload, encode_yuv420_intra_payload,
    frame_type_from_srsv2_revision, gray8_packed_to_yuv420p8_neutral, resolve_deblock_strength,
    rgb888_full_to_yuv420_bt709, target_payload_bytes, validate_b_prediction_weights,
    yuv420_bt709_to_rgb888_limited, BBlendModeWire, BFrameEncodeStats, BMbEncodeChoice,
    ChromaSiting, ColorConvertBackend, ColorPrimaries, ColorRange, CpuVideoAccelerator,
    DecodedVideoFrameV2, FrameTypeV2, GpuVideoAccelerator, MatrixCoefficients, MotionSearchBackend,
    PixelFormat, PreviousFrameRcStats, QuantBackend, ReferenceFrameBuffer, ResidualEncodeStats,
    ResidualEntropy, SrsElementaryVideoCodecId, SrsV2AdaptiveQuantizationMode, SrsV2AqEncodeStats,
    SrsV2BMotionSearchMode, SrsV2BlockAqMode, SrsV2EncodeSettings, SrsV2EntropyModelMode,
    SrsV2Error, SrsV2InterPartitionMode, SrsV2InterSyntaxMode, SrsV2LoopFilterMode,
    SrsV2MotionEncodeStats, SrsV2MotionSearchMode, SrsV2PartitionCostModel,
    SrsV2PartitionEncodeStats, SrsV2PartitionMapEncoding, SrsV2RateControlError,
    SrsV2RateControlMode, SrsV2RateController, SrsV2RdoMode, SrsV2ReferenceKind,
    SrsV2ReferenceManager, SrsV2ReferenceSlot, SrsV2SubpelMode, SrsV2TransformSize,
    SrsV2TransformSizeMode, SrsVideoCodecId, SrsVideoProfile, Srsv2PayloadKind, TransferFunction,
    TransformBackend, VideoPlane, VideoSequenceHeaderV2, YuvFrame, B_WEIGHTED_PRED_CANDIDATES,
    DEFAULT_DEBLOCK_STRENGTH, FRAME_PAYLOAD_MAGIC_ALT_REF, FRAME_PAYLOAD_MAGIC_B,
    FRAME_PAYLOAD_MAGIC_B_COMPACT, FRAME_PAYLOAD_MAGIC_B_INTER_ENTROPY,
    FRAME_PAYLOAD_MAGIC_B_INTER_ENTROPY_CTX_V1, FRAME_PAYLOAD_MAGIC_B_MB_BLEND,
    FRAME_PAYLOAD_MAGIC_B_MB_BLEND_QP, FRAME_PAYLOAD_MAGIC_B_SUBPEL,
    FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_CTX_V1, FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR,
    FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR_CTX_V1, FRAME_PAYLOAD_MAGIC_P_VAR_PARTITION,
};

#[cfg(test)]
mod export_sanity_tests {
    #[test]
    fn max_luma_samples_export_matches_8k_uhd_plane() {
        assert_eq!(crate::MAX_LUMA_SAMPLES, 33_177_600);
    }
}
