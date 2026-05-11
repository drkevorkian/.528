//! SRSV2 modern video path — **intra baseline** plus **experimental P-frame** prototype (`FR2` revision 2),
//! with hostile-input-safe parsers.
//!
//! SRSV1 (`crate::codec`) remains the legacy grayscale prototype.

pub mod activity;
pub mod adaptive_quant;
pub mod b_frame_codec;
pub mod block_aq;
pub mod color;
pub mod context_inter_entropy;
/// CTU-style 16/32/64 superblock grid planning only; no bitstream syntax or `FR2` revision.
pub mod ctu64;
mod dct;
pub mod deblock;
pub mod error;
pub mod frame;
pub mod frame_codec;
pub mod gpu_traits;
pub mod inter_mv;
pub mod intra_codec;
pub mod limits;
pub mod model;
pub mod motion_search;
pub mod p_frame_codec;
pub mod p_var_partition;
/// Experimental compact partition maps + MV-share blobs (embedded in **FR2** rev **27**/**28** when [`SrsV2PartitionSyntaxMode`](crate::srsv2::rate_control::SrsV2PartitionSyntaxMode) is **V2RleMvShare**).
pub mod partition_syntax_v2;
pub mod payload_kind;
pub mod rate_control;
pub mod rdo;
pub mod reference;
pub mod reference_manager;
pub mod residual_context_entropy;
pub mod residual_entropy;
pub mod residual_token_v2;
pub mod residual_tokens;
pub mod subpel;
/// Superchunk-style **128×128** … **1024×1024** region grid for 4K+ planning; geometry only (no wire syntax).
pub mod superchunk;
pub mod transform_layout;

pub use adaptive_quant::{
    accumulate_block_aq_wire_plane, resolve_frame_adaptive_qp, validate_adaptive_quant_settings,
    SrsV2AqEncodeStats, SrsV2BlockAqWireStats,
};
pub use b_frame_codec::{
    blend_weighted_pixels, choose_b_macroblock, choose_b_macroblock_blend_and_mv,
    decode_yuv420_b_payload, encode_yuv420_b_payload, encode_yuv420_b_payload_mb_blend,
    validate_b_prediction_weights, BBlendModeWire, BFrameEncodeStats, BMbEncodeChoice,
    B_WEIGHTED_PRED_CANDIDATES, FRAME_PAYLOAD_MAGIC_B, FRAME_PAYLOAD_MAGIC_B_COMPACT,
    FRAME_PAYLOAD_MAGIC_B_INTER_ENTROPY, FRAME_PAYLOAD_MAGIC_B_INTER_ENTROPY_CTX_V1,
    FRAME_PAYLOAD_MAGIC_B_MB_BLEND, FRAME_PAYLOAD_MAGIC_B_MB_BLEND_QP,
    FRAME_PAYLOAD_MAGIC_B_SUBPEL,
};
pub use color::{
    gray8_packed_to_yuv420p8_neutral, rgb888_full_to_yuv420_bt709, yuv420_bt709_to_rgb888_limited,
};
pub use context_inter_entropy::{
    context_model_summary, decode_mv_context_v1_fixed, decode_mv_context_v1_partitioned,
    encode_mv_context_v1_fixed, encode_mv_context_v1_partitioned, ContextV1ModelSummary,
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
    FRAME_PAYLOAD_MAGIC_ALT_REF, FRAME_PAYLOAD_MAGIC_INTRA_COMPACT_V1,
    FRAME_PAYLOAD_MAGIC_INTRA_TRANSFORM_GROUP_V34,
};
pub use model::{
    decode_sequence_header_v2, encode_sequence_header_v2, frame_type_from_srsv2_revision,
    ChromaSiting, ColorPrimaries, ColorRange, FrameHeaderV2, FrameTypeV2, MatrixCoefficients,
    PixelFormat, SrsElementaryVideoCodecId, SrsVideoCodecId, SrsVideoProfile, TileHeaderV2,
    TransferFunction, VideoSequenceHeaderV2, SEQUENCE_HEADER_BYTES,
};
pub use motion_search::{
    SrsV2InterMvBenchStats, SrsV2MotionEncodeStats, SrsV2PartitionEncodeStats, SrsV2RdoBenchStats,
};
pub use p_frame_codec::{
    FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_CTX_V1, FRAME_PAYLOAD_MAGIC_P_RESIDUAL_COMPACT_V1,
    FRAME_PAYLOAD_MAGIC_P_RESIDUAL_TRANSFORM_GROUP_V35,
};
pub use p_var_partition::{
    FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR, FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR_CTX_V1,
    FRAME_PAYLOAD_MAGIC_P_INTER_ENTROPY_VAR_V2, FRAME_PAYLOAD_MAGIC_P_VAR_PARTITION,
    FRAME_PAYLOAD_MAGIC_P_VAR_PARTITION_V2,
};
pub use partition_syntax_v2::{
    decode_mv_share_groups_v2, decode_partition_map_v2, encode_mv_share_groups_v2,
    encode_partition_map_v2, estimate_partition_syntax_v2_bytes, total_pu_slots_for_modes,
    v1_legacy_partition_map_bytes, validate_partition_map_v2, MvShareGroupV2, PartitionMapV2,
    PartitionModeV2, PartitionRunV2, PartitionSyntaxV2Error, PartitionSyntaxV2Stats,
    MV_SHARE_GROUPS_V2_MAGIC, PARTITION_MAP_V2_MAGIC,
};
pub use payload_kind::{classify_srsv2_payload, Srsv2PayloadKind};
pub use rate_control::{
    target_payload_bytes, PreviousFrameRcStats, ResidualEncodeStats, ResidualEntropy,
    SrsV2AdaptiveQuantizationMode, SrsV2BMotionSearchMode, SrsV2BlockAqMode, SrsV2CoeffLayoutMode,
    SrsV2CoeffLayoutSettingsError, SrsV2CoeffScanMode, SrsV2EncodeSettings, SrsV2EntropyModelMode,
    SrsV2InterPartitionMode, SrsV2InterSyntaxMode, SrsV2MotionSearchMode, SrsV2PartitionCostModel,
    SrsV2PartitionMapEncoding, SrsV2PartitionSyntaxMode, SrsV2RateControlError,
    SrsV2RateControlMode, SrsV2RateController, SrsV2RdoMode, SrsV2ResidualContextMode,
    SrsV2SubpelMode, SrsV2TransformDecisionMode, SrsV2TransformGroupingMode, SrsV2TransformSize,
    SrsV2TransformSizeMode,
};
pub use rdo::{
    autofast_partition_mb_rdo_score, autofast_partition_mb_wire_cost, b_blend_rdo_score,
    bounded_candidate_push, choose_best_inter_mode_candidate, choose_best_partition_candidate,
    choose_grouping_rdo_fast, choose_min_partition_by_precomputed_scores,
    estimate_inter_header_bytes, estimate_mv_delta_wire_bytes, estimate_partition_candidate_bytes,
    p_subblock_skip_residual_is_rdo_cheaper, partition_header_aware_rdo_score,
    partition_header_aware_score, partition_rdo_fast_score, rdo_fast_enabled, rdo_score,
    score_candidate, GroupingRdoFastDecision, RdoCandidate, RdoCost, RdoDecision,
    RdoModeDecisionStats, RdoStats, GROUPING_RDO_FAST_CANDIDATES, MAX_RDO_CANDIDATES,
};
pub use reference::ReferenceFrameBuffer;
pub use reference_manager::{SrsV2ReferenceKind, SrsV2ReferenceManager, SrsV2ReferenceSlot};

pub use gpu_traits::{
    ColorConvertBackend, CpuVideoAccelerator, GpuVideoAccelerator, MotionSearchBackend,
    QuantBackend, TransformBackend,
};

pub use residual_token_v2::{
    decode_ac_payload as residual_token_v2_decode_ac_payload,
    encode_ac_payload as residual_token_v2_encode_ac_payload,
    MAX_ABS_AC_COEFF as RESIDUAL_TOKEN_V2_MAX_ABS_AC_COEFF,
};

/// Elementary `.srsv2` file writer/reader (fixed 64-byte sequence header + framed payloads).
pub mod elementary;
