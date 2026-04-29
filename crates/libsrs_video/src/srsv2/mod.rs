//! SRSV2 modern video path — intra baseline with hostile-input-safe parsers.
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
pub mod rate_control;

pub use color::{rgb888_full_to_yuv420_bt709, yuv420_bt709_to_rgb888_limited};
pub use error::SrsV2Error;
pub use frame::{DecodedVideoFrameV2, EncodedVideoPacketV2, VideoPlane, YuvFrame};
pub use frame_codec::{decode_yuv420_intra_payload, encode_yuv420_intra_payload};
pub use model::{
    decode_sequence_header_v2, encode_sequence_header_v2, ChromaSiting, ColorPrimaries, ColorRange,
    FrameHeaderV2, FrameTypeV2, MatrixCoefficients, PixelFormat, SrsVideoCodecId, SrsVideoProfile,
    TileHeaderV2, TransferFunction, VideoSequenceHeaderV2, SEQUENCE_HEADER_BYTES,
};
pub use rate_control::SrsV2EncodeSettings;

pub use gpu_traits::{
    ColorConvertBackend, CpuVideoAccelerator, GpuVideoAccelerator, MotionSearchBackend,
    QuantBackend, TransformBackend,
};

/// Elementary `.srsv2` file writer/reader (fixed 64-byte sequence header + framed payloads).
pub mod elementary;
