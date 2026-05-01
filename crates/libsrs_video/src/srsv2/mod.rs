//! SRSV2 modern video path — **intra baseline** plus **experimental P-frame** prototype (`FR2` revision 2),
//! with hostile-input-safe parsers.
//!
//! SRSV1 (`crate::codec`) remains the legacy grayscale prototype.

pub mod color;
mod dct;
pub mod error;
pub mod frame;
pub mod frame_codec;
pub mod gpu_traits;
pub mod intra_codec;
pub mod limits;
pub mod model;
pub mod p_frame_codec;
pub mod payload_kind;
pub mod rate_control;
pub mod reference;
pub mod residual_entropy;
pub mod residual_tokens;

pub use color::{
    gray8_packed_to_yuv420p8_neutral, rgb888_full_to_yuv420_bt709, yuv420_bt709_to_rgb888_limited,
};
pub use error::SrsV2Error;
pub use frame::{DecodedVideoFrameV2, EncodedVideoPacketV2, VideoPlane, YuvFrame};
pub use frame_codec::{
    decode_yuv420_intra_payload, decode_yuv420_srsv2_payload, encode_yuv420_inter_payload,
    encode_yuv420_intra_payload,
};
pub use model::{
    decode_sequence_header_v2, encode_sequence_header_v2, ChromaSiting, ColorPrimaries, ColorRange,
    FrameHeaderV2, FrameTypeV2, MatrixCoefficients, PixelFormat, SrsElementaryVideoCodecId,
    SrsVideoCodecId, SrsVideoProfile, TileHeaderV2, TransferFunction, VideoSequenceHeaderV2,
    SEQUENCE_HEADER_BYTES,
};
pub use payload_kind::{classify_srsv2_payload, Srsv2PayloadKind};
pub use rate_control::{
    target_payload_bytes, PreviousFrameRcStats, ResidualEncodeStats, ResidualEntropy,
    SrsV2EncodeSettings, SrsV2RateControlError, SrsV2RateControlMode, SrsV2RateController,
};
pub use reference::ReferenceFrameBuffer;

pub use gpu_traits::{
    ColorConvertBackend, CpuVideoAccelerator, GpuVideoAccelerator, MotionSearchBackend,
    QuantBackend, TransformBackend,
};

/// Elementary `.srsv2` file writer/reader (fixed 64-byte sequence header + framed payloads).
pub mod elementary;
