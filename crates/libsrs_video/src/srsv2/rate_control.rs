//! Encoder-side knobs (CPU rate control loop planned — QP wired today).

use super::limits::{MAX_MOTION_SEARCH_RADIUS, MAX_MOTION_VECTOR_PELS};

#[derive(Debug, Clone)]
pub struct SrsV2EncodeSettings {
    pub quantizer: u8,
    pub target_bitrate_kbps: Option<u32>,
    pub max_bitrate_kbps: Option<u32>,
    /// Force an I-frame every N frames (1 = all-intra). Frame 0 is always I.
    pub keyframe_interval: u32,
    /// Half-side of integer-pel ME window for experimental P-frames (clamped in encoder).
    pub motion_search_radius: i16,
    pub tune_quality_vs_speed: u8,
}

impl Default for SrsV2EncodeSettings {
    fn default() -> Self {
        Self {
            quantizer: 24,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            keyframe_interval: 60,
            motion_search_radius: 16,
            tune_quality_vs_speed: 50,
        }
    }
}

impl SrsV2EncodeSettings {
    /// Search radius bounded for hostile-input-safe encode (`≤ MAX_MOTION_SEARCH_RADIUS`).
    pub fn clamped_motion_search_radius(&self) -> i16 {
        self.motion_search_radius
            .clamp(0, MAX_MOTION_SEARCH_RADIUS)
            .min(MAX_MOTION_VECTOR_PELS)
    }
}
