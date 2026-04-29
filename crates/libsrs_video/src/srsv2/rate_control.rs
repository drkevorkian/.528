//! Encoder-side knobs (CPU rate control loop planned — QP wired today).

#[derive(Debug, Clone)]
pub struct SrsV2EncodeSettings {
    pub quantizer: u8,
    pub target_bitrate_kbps: Option<u32>,
    pub max_bitrate_kbps: Option<u32>,
    pub keyframe_interval: u32,
    pub tune_quality_vs_speed: u8,
}

impl Default for SrsV2EncodeSettings {
    fn default() -> Self {
        Self {
            quantizer: 24,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            keyframe_interval: 60,
            tune_quality_vs_speed: 50,
        }
    }
}
