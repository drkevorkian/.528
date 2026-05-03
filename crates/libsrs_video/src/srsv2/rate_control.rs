//! Encoder-side rate control: QP selection for benchmarks and future encode loops.

use super::deblock::SrsV2LoopFilterMode;
use super::limits::{
    MAX_MOTION_SEARCH_RADIUS, MAX_MOTION_VECTOR_PELS, MAX_SUBPEL_REFINEMENT_RADIUS,
};

/// How 8×8 residual blocks choose explicit tuples vs static rANS (`FR2` rev 3/4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResidualEntropy {
    /// Pick the smaller on-wire representation per block (never larger than explicit tuples).
    #[default]
    Auto,
    /// Legacy tuple stream only (`FR2` rev 1 / rev 2 layout).
    Explicit,
    /// Prefer rANS where coefficients fit the static alphabet (fails encode if not).
    Rans,
}

#[derive(Debug, Clone, Default)]
pub struct ResidualEncodeStats {
    pub intra_explicit_blocks: u64,
    pub intra_rans_blocks: u64,
    pub p_explicit_chunks: u64,
    pub p_rans_chunks: u64,
}

/// High-level rate-control strategy (bench / encoder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrsV2RateControlMode {
    /// Use [`SrsV2EncodeSettings::quantizer`] for every frame (clamped to min/max QP).
    #[default]
    FixedQp,
    /// Use [`SrsV2EncodeSettings::quality`] as the QP index directly (CRF-like: **lower number ⇒ lower QP ⇒ higher quality**), clamped.
    ConstantQuality,
    /// Adapt QP toward [`SrsV2EncodeSettings::target_bitrate_kbps`] using per-frame payload sizes.
    TargetBitrate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrsV2RateControlError {
    InvalidQpRange { min_qp: u8, max_qp: u8 },
    ZeroTargetBitrate,
    QualityRequiredForConstantQuality,
    TargetBitrateRequiredForTargetBitrateMode,
    InvalidFps { fps_num: u32, fps_den: u32 },
}

impl std::fmt::Display for SrsV2RateControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidQpRange { min_qp, max_qp } => {
                write!(f, "min_qp ({min_qp}) must be <= max_qp ({max_qp})")
            }
            Self::ZeroTargetBitrate => write!(f, "target_bitrate_kbps must be non-zero"),
            Self::QualityRequiredForConstantQuality => {
                write!(f, "constant-quality mode requires settings.quality")
            }
            Self::TargetBitrateRequiredForTargetBitrateMode => {
                write!(
                    f,
                    "target-bitrate mode requires settings.target_bitrate_kbps"
                )
            }
            Self::InvalidFps { fps_num, fps_den } => {
                write!(f, "invalid fps {fps_num}/{fps_den} (fps_num must be > 0)")
            }
        }
    }
}

impl std::error::Error for SrsV2RateControlError {}

/// Experimental adaptive quantization mode (frame-level effective QP in this slice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrsV2AdaptiveQuantizationMode {
    #[default]
    Off,
    Activity,
    EdgeAware,
    ScreenAware,
}

/// Per-**8×8** QP delta on wire (`FR2` rev **7**/**8**/**9**). Legacy payloads omit deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrsV2BlockAqMode {
    #[default]
    Off,
    /// Same bitstream as **`Off`**; label for “frame QP only” tooling.
    FrameOnly,
    /// Emit versioned block **`qp_delta`** syntax (requires adaptive residual path where defined).
    BlockDelta,
}

/// Sub-pixel motion refinement (luma). **`Off`** keeps legacy `FR2` rev **2**/**4** integer MVs only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrsV2SubpelMode {
    #[default]
    Off,
    /// Half-pel on the quarter-pel grid (`FR2` rev **5**/**6**); chroma uses integer `mv_q/8`.
    HalfPel,
}

/// Integer-pel motion search strategy for experimental P-frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrsV2MotionSearchMode {
    /// Full search within [`SrsV2EncodeSettings::motion_search_radius`] (legacy behavior).
    #[default]
    ExhaustiveSmall,
    None,
    Diamond,
    Hex,
    Hierarchical,
}

/// Experimental **B-frame** motion / blend search (`FR2` rev **13** integer MVs, rev **14** half-pel MVs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrsV2BMotionSearchMode {
    /// Zero MVs; per-MB blend chosen by SAD among forward / backward / average (`FR2` rev **13**).
    #[default]
    Off,
    /// Reserved alias for tooling; currently identical to [`Off`].
    ReuseP,
    /// Independent integer search on backward ref **A** and forward ref **B**, then pick blend by SAD.
    IndependentForwardBackward,
    /// Like [`IndependentForwardBackward`], then half-pel refinement on the quarter-pel grid (`FR2` rev **14**).
    IndependentForwardBackwardHalfPel,
}

#[derive(Debug, Clone)]
pub struct SrsV2EncodeSettings {
    pub quantizer: u8,
    pub rate_control_mode: SrsV2RateControlMode,
    /// **ConstantQuality**: treated as QP index 1..=51 — lower value ⇒ lower QP ⇒ higher quality (CRF-like).
    pub quality: Option<u8>,
    pub target_bitrate_kbps: Option<u32>,
    pub max_bitrate_kbps: Option<u32>,
    pub min_qp: u8,
    pub max_qp: u8,
    /// Maximum QP delta magnitude applied in one frame for [`SrsV2RateControlMode::TargetBitrate`].
    pub qp_step_limit_per_frame: u8,
    /// Force an I-frame every N frames (1 = all-intra). Frame 0 is always I.
    pub keyframe_interval: u32,
    /// Half-side of integer-pel ME window for experimental P-frames (clamped in encoder).
    pub motion_search_radius: i16,
    pub tune_quality_vs_speed: u8,
    pub residual_entropy: ResidualEntropy,

    pub adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode,
    pub aq_strength: u8,
    pub min_block_qp_delta: i8,
    pub max_block_qp_delta: i8,

    pub motion_search_mode: SrsV2MotionSearchMode,
    /// Experimental B-frame motion search (`FR2` rev **13**/**14**).
    pub b_motion_search_mode: SrsV2BMotionSearchMode,
    /// Per-MB weighted prediction candidates (`FR2` rev **14** only on wire); **`false`** preserves rev **13** behavior.
    pub b_weighted_prediction: bool,
    /// When non-zero, motion search may stop early once best SAD ≤ threshold.
    pub early_exit_sad_threshold: u32,
    pub enable_skip_blocks: bool,

    pub subpel_mode: SrsV2SubpelMode,
    /// Half-pel refinement radius: **`0`** skips the eight half-pel probes; **`1`** runs one ring (default).
    pub subpel_refinement_radius: u8,

    pub block_aq_mode: SrsV2BlockAqMode,

    pub loop_filter_mode: SrsV2LoopFilterMode,
    /// Written to the sequence header when [`SrsV2LoopFilterMode::SimpleDeblock`] is selected; **`0`** uses codec default strength.
    pub deblock_strength: u8,
}

impl Default for SrsV2EncodeSettings {
    fn default() -> Self {
        Self {
            quantizer: 24,
            rate_control_mode: SrsV2RateControlMode::FixedQp,
            quality: None,
            target_bitrate_kbps: None,
            max_bitrate_kbps: None,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit_per_frame: 2,
            keyframe_interval: 60,
            motion_search_radius: 16,
            tune_quality_vs_speed: 50,
            residual_entropy: ResidualEntropy::Auto,

            adaptive_quantization_mode: SrsV2AdaptiveQuantizationMode::Off,
            aq_strength: 4,
            min_block_qp_delta: -6,
            max_block_qp_delta: 6,

            motion_search_mode: SrsV2MotionSearchMode::ExhaustiveSmall,
            b_motion_search_mode: SrsV2BMotionSearchMode::Off,
            b_weighted_prediction: false,
            early_exit_sad_threshold: 0,
            enable_skip_blocks: true,

            subpel_mode: SrsV2SubpelMode::Off,
            subpel_refinement_radius: 1,

            block_aq_mode: SrsV2BlockAqMode::Off,

            loop_filter_mode: SrsV2LoopFilterMode::Off,
            deblock_strength: 0,
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

    pub fn clamped_subpel_refinement_radius(&self) -> u8 {
        self.subpel_refinement_radius
            .min(MAX_SUBPEL_REFINEMENT_RADIUS)
    }

    pub fn clamp_qp(&self, qp: u8) -> u8 {
        qp.clamp(self.min_qp, self.max_qp)
    }

    /// Validate rate-control-related fields (deterministic checks only).
    pub fn validate_rate_control(&self) -> Result<(), SrsV2RateControlError> {
        if self.min_qp > self.max_qp {
            return Err(SrsV2RateControlError::InvalidQpRange {
                min_qp: self.min_qp,
                max_qp: self.max_qp,
            });
        }
        match self.rate_control_mode {
            SrsV2RateControlMode::FixedQp | SrsV2RateControlMode::ConstantQuality => {}
            SrsV2RateControlMode::TargetBitrate => {
                let Some(tb) = self.target_bitrate_kbps else {
                    return Err(SrsV2RateControlError::TargetBitrateRequiredForTargetBitrateMode);
                };
                if tb == 0 {
                    return Err(SrsV2RateControlError::ZeroTargetBitrate);
                }
            }
        }
        if self.rate_control_mode == SrsV2RateControlMode::ConstantQuality && self.quality.is_none()
        {
            return Err(SrsV2RateControlError::QualityRequiredForConstantQuality);
        }
        Ok(())
    }
}

/// Stats for the frame **just encoded**, supplied when asking QP for the **next** frame.
#[derive(Debug, Clone, Copy)]
pub struct PreviousFrameRcStats {
    pub encoded_bytes: u32,
    pub is_keyframe: bool,
}

/// Encoder-side QP controller (deterministic first pass).
pub struct SrsV2RateController {
    settings: SrsV2EncodeSettings,
    fps_num: u32,
    fps_den: u32,
    current_qp: u8,
}

impl SrsV2RateController {
    pub fn new(
        settings: &SrsV2EncodeSettings,
        fps_num: u32,
        fps_den: u32,
    ) -> Result<Self, SrsV2RateControlError> {
        settings.validate_rate_control()?;
        if fps_num == 0 || fps_den == 0 {
            return Err(SrsV2RateControlError::InvalidFps { fps_num, fps_den });
        }

        let current_qp = match settings.rate_control_mode {
            SrsV2RateControlMode::FixedQp => settings.clamp_qp(settings.quantizer),
            SrsV2RateControlMode::ConstantQuality => {
                let q = settings.quality.unwrap_or(settings.quantizer);
                settings.clamp_qp(q)
            }
            SrsV2RateControlMode::TargetBitrate => settings.clamp_qp(settings.quantizer),
        };

        Ok(Self {
            settings: settings.clone(),
            fps_num,
            fps_den,
            current_qp,
        })
    }

    /// QP to use when encoding `frame_index`.
    /// For [`SrsV2RateControlMode::TargetBitrate`], call [`observe_frame`] after each encode so the next frame picks an updated QP; `previous` is ignored in that mode.
    pub fn qp_for_frame(
        &mut self,
        _frame_index: u32,
        previous: Option<PreviousFrameRcStats>,
    ) -> u8 {
        match self.settings.rate_control_mode {
            SrsV2RateControlMode::FixedQp => self.settings.clamp_qp(self.settings.quantizer),
            SrsV2RateControlMode::ConstantQuality => {
                let q = self.settings.quality.unwrap_or(self.settings.quantizer);
                self.settings.clamp_qp(q)
            }
            SrsV2RateControlMode::TargetBitrate => {
                // Caller updates state via [`observe_frame`] after each encode; `previous` is unused here.
                let _ = previous;
                self.settings.clamp_qp(self.current_qp)
            }
        }
    }

    /// After encoding a frame, feed payload size and keyframe flag so [`SrsV2RateControlMode::TargetBitrate`] can adjust QP for the next frame.
    pub fn observe_frame(&mut self, _frame_index: u32, encoded_bytes: usize, is_keyframe: bool) {
        if self.settings.rate_control_mode != SrsV2RateControlMode::TargetBitrate {
            return;
        }
        self.adjust_qp_target_bitrate(PreviousFrameRcStats {
            encoded_bytes: encoded_bytes.try_into().unwrap_or(u32::MAX),
            is_keyframe,
        });
    }

    fn adjust_qp_target_bitrate(&mut self, prev: PreviousFrameRcStats) {
        let Some(tb) = self.settings.target_bitrate_kbps else {
            return;
        };
        let target = target_payload_bytes(tb, self.fps_num, self.fps_den, prev.is_keyframe);
        let actual = u64::from(prev.encoded_bytes);
        let lim = self.settings.qp_step_limit_per_frame.max(1) as i16;

        let delta: i16 = if actual > target {
            lim
        } else if actual < target {
            -lim
        } else {
            0
        };

        let next = (self.current_qp as i16).saturating_add(delta);
        self.current_qp = next.clamp(
            i16::from(self.settings.min_qp),
            i16::from(self.settings.max_qp),
        ) as u8;
    }
}

/// Expected payload bytes for one frame at `target_bitrate_kbps`. I-frames get a simple 3× allowance vs P.
pub fn target_payload_bytes(kbps: u32, fps_num: u32, fps_den: u32, is_keyframe: bool) -> u64 {
    let num = u64::from(kbps)
        .saturating_mul(1000)
        .saturating_mul(u64::from(fps_den));
    let den = 8u64.saturating_mul(u64::from(fps_num.max(1)));
    let base = (num / den.max(1)).max(1);
    if is_keyframe {
        base.saturating_mul(3)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tb_settings(kbps: u32, q0: u8) -> SrsV2EncodeSettings {
        SrsV2EncodeSettings {
            rate_control_mode: SrsV2RateControlMode::TargetBitrate,
            target_bitrate_kbps: Some(kbps),
            quantizer: q0,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit_per_frame: 2,
            ..Default::default()
        }
    }

    #[test]
    fn fixed_qp_returns_same_qp() {
        let s = SrsV2EncodeSettings {
            rate_control_mode: SrsV2RateControlMode::FixedQp,
            quantizer: 28,
            min_qp: 10,
            max_qp: 40,
            ..Default::default()
        };
        let mut rc = SrsV2RateController::new(&s, 30, 1).unwrap();
        assert_eq!(rc.qp_for_frame(0, None), 28);
        assert_eq!(
            rc.qp_for_frame(
                1,
                Some(PreviousFrameRcStats {
                    encoded_bytes: 9999,
                    is_keyframe: false,
                })
            ),
            28
        );
    }

    #[test]
    fn constant_quality_maps_deterministically() {
        let s = SrsV2EncodeSettings {
            rate_control_mode: SrsV2RateControlMode::ConstantQuality,
            quality: Some(22),
            min_qp: 10,
            max_qp: 40,
            ..Default::default()
        };
        let mut rc = SrsV2RateController::new(&s, 30, 1).unwrap();
        assert_eq!(rc.qp_for_frame(0, None), 22);
    }

    #[test]
    fn target_bitrate_increases_qp_after_oversized_frame() {
        // 1 kbps @ 30 fps => ~4 byte P-frame budget; pretend we emitted far more => raise QP.
        let s = tb_settings(1, 10);
        let mut rc = SrsV2RateController::new(&s, 30, 1).unwrap();
        assert_eq!(rc.current_qp, 10);
        let _ = rc.qp_for_frame(0, None);
        let tiny_target = target_payload_bytes(1, 30, 1, false);
        let big = tiny_target.saturating_mul(100).min(u32::MAX as u64) as usize;
        rc.observe_frame(0, big, false);
        let qp1 = rc.qp_for_frame(1, None);
        assert!(qp1 > 10);
    }

    #[test]
    fn target_bitrate_decreases_qp_after_undersized_frame() {
        let s = tb_settings(10_000, 40);
        let mut rc = SrsV2RateController::new(&s, 30, 1).unwrap();
        let _ = rc.qp_for_frame(0, None);
        rc.observe_frame(0, 1, false);
        let qp1 = rc.qp_for_frame(1, None);
        assert!(qp1 < 40);
    }

    #[test]
    fn qp_stays_inside_min_max() {
        let s = SrsV2EncodeSettings {
            rate_control_mode: SrsV2RateControlMode::FixedQp,
            quantizer: 99,
            min_qp: 10,
            max_qp: 20,
            ..Default::default()
        };
        let mut rc = SrsV2RateController::new(&s, 30, 1).unwrap();
        assert_eq!(rc.qp_for_frame(0, None), 20);
    }

    #[test]
    fn qp_step_limit_respected_magnitude() {
        let s = SrsV2EncodeSettings {
            rate_control_mode: SrsV2RateControlMode::TargetBitrate,
            target_bitrate_kbps: Some(1),
            quantizer: 20,
            min_qp: 4,
            max_qp: 51,
            qp_step_limit_per_frame: 3,
            ..Default::default()
        };
        let mut rc = SrsV2RateController::new(&s, 30, 1).unwrap();
        let _ = rc.qp_for_frame(0, None);
        let before = rc.current_qp;
        let tiny = target_payload_bytes(1, 30, 1, false);
        let big = tiny.saturating_mul(50).min(u32::MAX as u64) as usize;
        rc.observe_frame(0, big, false);
        let after = rc.qp_for_frame(1, None);
        assert!(after <= before + 3);
    }

    #[test]
    fn invalid_settings_errors() {
        let s = SrsV2EncodeSettings {
            min_qp: 40,
            max_qp: 10,
            ..Default::default()
        };
        assert!(s.validate_rate_control().is_err());

        let s = SrsV2EncodeSettings {
            rate_control_mode: SrsV2RateControlMode::TargetBitrate,
            target_bitrate_kbps: Some(0),
            ..Default::default()
        };
        assert!(s.validate_rate_control().is_err());

        let s = SrsV2EncodeSettings {
            rate_control_mode: SrsV2RateControlMode::ConstantQuality,
            quality: None,
            ..Default::default()
        };
        assert!(s.validate_rate_control().is_err());
    }

    #[test]
    fn invalid_fps_rejected() {
        let s = SrsV2EncodeSettings::default();
        assert!(SrsV2RateController::new(&s, 0, 1).is_err());
        assert!(SrsV2RateController::new(&s, 30, 0).is_err());
    }

    #[test]
    fn motion_radius_tests_negative_clamps() {
        let s = SrsV2EncodeSettings {
            motion_search_radius: -40,
            ..Default::default()
        };
        assert_eq!(s.clamped_motion_search_radius(), 0);
    }

    #[test]
    fn motion_radius_tests_oversized_clamps() {
        let s = SrsV2EncodeSettings {
            motion_search_radius: i16::MAX,
            ..Default::default()
        };
        assert_eq!(s.clamped_motion_search_radius(), MAX_MOTION_SEARCH_RADIUS);
    }

    #[test]
    fn motion_radius_tests_default_stable() {
        let s = SrsV2EncodeSettings::default();
        assert_eq!(s.motion_search_radius, 16);
        assert_eq!(s.clamped_motion_search_radius(), 16);
    }

    #[test]
    fn subpel_refinement_radius_clamps_to_max() {
        use super::super::limits::MAX_SUBPEL_REFINEMENT_RADIUS;
        let s = SrsV2EncodeSettings {
            subpel_refinement_radius: 255,
            ..Default::default()
        };
        assert_eq!(
            s.clamped_subpel_refinement_radius(),
            MAX_SUBPEL_REFINEMENT_RADIUS
        );
    }
}
