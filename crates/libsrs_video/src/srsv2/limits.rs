//! Hard caps for hostile-input safety (decoder-enforced).

/// Maximum picture width or height (pixels).
pub const MAX_DIMENSION: u32 = 8192;
/// Maximum luma samples per frame (1080p60 baseline fits comfortably).
pub const MAX_LUMA_SAMPLES: u64 = 33_177_600; // 7680×4320
/// Maximum coded packet payload bytes accepted by SRSV2 frame decoder.
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
/// Maximum tiles per frame (reserved for future tiling).
pub const MAX_TILES: u32 = 4096;
/// Maximum reference frames (future inter).
pub const MAX_REF_FRAMES: u8 = 16;
/// Maximum sequence metadata extension bytes.
pub const MAX_METADATA_BYTES: usize = 16 * 1024;
/// Superblock size (baseline intra path uses recursive splits down to 8×8).
pub const SUPERBLOCK_SIZE: u32 = 64;
/// Minimum coding unit (after splits).
pub const MIN_CU_SIZE: u32 = 8;
