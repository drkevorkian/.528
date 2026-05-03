//! SRSV2 modern video path — **intra baseline** plus **experimental P-frame** prototype (`FR2` revision 2),
//! with hostile-input-safe parsers.
//!
//! SRSV1 (`crate::codec`) remains the legacy grayscale prototype.

pub mod activity;
pub mod adaptive_quant;
pub mod b_frame_codec;
pub mod block_aq;
pub mod color;
mod dct;
pub mod deblock;
pub mod error;
pub mod frame;
pub mod frame_codec;
pub mod gpu_traits;
pub mod intra_codec;
pub mod limits;
pub mod model;
pub mod motion_search;
pub mod p_frame_codec;
pub mod payload_kind;
pub mod rate_control;
pub mod reference;
pub mod reference_manager;
pub mod residual_entropy;
pub mod residual_tokens;
pub mod subpel;

pub use adaptive_quant::{
    accumulate_block_aq_wire_plane, resolve_frame_adaptive_qp, validate_adaptive_quant_settings,
    SrsV2AqEncodeStats, SrsV2BlockAqWireStats,
};
pub use b_frame_codec::{
    blend_weighted_pixels, choose_b_macroblock, choose_b_macroblock_blend_and_mv,
    decode_yuv420_b_payload, encode_yuv420_b_payload, encode_yuv420_b_payload_mb_blend,
    validate_b_prediction_weights, BBlendModeWire, BFrameEncodeStats, BMbEncodeChoice,
    B_WEIGHTED_PRED_CANDIDATES, FRAME_PAYLOAD_MAGIC_B, FRAME_PAYLOAD_MAGIC_B_MB_BLEND,
    FRAME_PAYLOAD_MAGIC_B_MB_BLEND_QP, FRAME_PAYLOAD_MAGIC_B_SUBPEL,
};
pub use color::{
    gray8_packed_to_yuv420p8_neutral, rgb888_full_to_yuv420_bt709, yuv420_bt709_to_rgb888_limited,
};
pub use deblock::{
    apply_loop_filter_y, apply_simple_mb_boundary_deblock_y, resolve_deblock_strength,
    SrsV2LoopFilterMode, DEFAULT_DEBLOCK_STRENGTH,
};
pub use error::SrsV2Error;
pub use frame::{DecodedVideoFrameV2, EncodedVideoPacketV2, VideoPlane, YuvFrame};
pub use frame_codec::{
    apply_reconstruction_filter_if_enabled, decode_yuv420_alt_ref_payload,
    decode_yuv420_intra_payload, decode_yuv420_srsv2_payload, decode_yuv420_srsv2_payload_managed,
    encode_yuv420_alt_ref_payload, encode_yuv420_inter_payload, encode_yuv420_intra_payload,
    FRAME_PAYLOAD_MAGIC_ALT_REF,
};
pub use model::{
    decode_sequence_header_v2, encode_sequence_header_v2, frame_type_from_srsv2_revision,
    ChromaSiting, ColorPrimaries, ColorRange, FrameHeaderV2, FrameTypeV2, MatrixCoefficients,
    PixelFormat, SrsElementaryVideoCodecId, SrsVideoCodecId, SrsVideoProfile, TileHeaderV2,
    TransferFunction, VideoSequenceHeaderV2, SEQUENCE_HEADER_BYTES,
};
pub use motion_search::SrsV2MotionEncodeStats;
pub use payload_kind::{classify_srsv2_payload, Srsv2PayloadKind};
pub use rate_control::{
    target_payload_bytes, PreviousFrameRcStats, ResidualEncodeStats, ResidualEntropy,
    SrsV2AdaptiveQuantizationMode, SrsV2BMotionSearchMode, SrsV2BlockAqMode, SrsV2EncodeSettings,
    SrsV2MotionSearchMode, SrsV2RateControlError, SrsV2RateControlMode, SrsV2RateController,
    SrsV2SubpelMode,
};
pub use reference::ReferenceFrameBuffer;
pub use reference_manager::{SrsV2ReferenceKind, SrsV2ReferenceManager, SrsV2ReferenceSlot};

pub use gpu_traits::{
    ColorConvertBackend, CpuVideoAccelerator, GpuVideoAccelerator, MotionSearchBackend,
    QuantBackend, TransformBackend,
};

/// Elementary `.srsv2` file writer/reader (fixed 64-byte sequence header + framed payloads).
pub mod elementary;
