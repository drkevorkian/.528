//! Single-slot reference picture buffer for experimental SRSV2 P-frames (multi-slot reserved).

use super::error::SrsV2Error;
use super::frame::YuvFrame;
use super::limits::MAX_REF_FRAMES;

/// Holds decoded reference frames up to `max_slots` (hostile-input bound).
#[derive(Debug, Clone)]
pub struct ReferenceFrameBuffer {
    max_slots: u8,
    slots: Vec<Option<YuvFrame>>,
}

impl ReferenceFrameBuffer {
    pub fn new(max_ref_frames: u8) -> Result<Self, SrsV2Error> {
        if max_ref_frames > MAX_REF_FRAMES {
            return Err(SrsV2Error::ExcessiveReferenceFrames(max_ref_frames));
        }
        let n = (max_ref_frames as usize).max(1);
        Ok(Self {
            max_slots: max_ref_frames,
            slots: vec![None; n],
        })
    }

    pub fn max_slots(&self) -> u8 {
        self.max_slots
    }

    pub fn clear(&mut self) {
        for s in &mut self.slots {
            *s = None;
        }
    }

    /// Baseline single-reference view (slot 0).
    pub fn primary_ref(&self) -> Option<&YuvFrame> {
        self.slots.first().and_then(|s| s.as_ref())
    }

    pub fn set_primary(&mut self, frame: YuvFrame) {
        if let Some(slot) = self.slots.first_mut() {
            *slot = Some(frame);
        }
    }
}
