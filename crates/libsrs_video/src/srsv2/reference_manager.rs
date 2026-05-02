//! Multi-slot **reference picture** bookkeeping for SRSV2 (experimental B / alt-ref groundwork).
//!
//! Bounded by [`crate::srsv2::limits::MAX_REF_FRAMES`] and `VideoSequenceHeaderV2::max_ref_frames`.

use super::error::SrsV2Error;
use super::frame::YuvFrame;
use super::limits::MAX_REF_FRAMES;

/// Semantic role for diagnostics / future signaling (wire may use slot indices only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrsV2ReferenceKind {
    Last,
    Golden,
    AltRef,
    Future,
}

/// One occupied reference slot (hostile-input validated indices).
#[derive(Debug, Clone)]
pub struct SrsV2ReferenceSlot {
    pub slot_id: u8,
    pub frame_index: u32,
    pub order_index: u64,
    pub kind: SrsV2ReferenceKind,
    pub frame: YuvFrame,
    pub is_displayable: bool,
}

/// Bounded reference store for decode / playback (cleared on seek/stop).
#[derive(Debug, Clone)]
pub struct SrsV2ReferenceManager {
    max_ref_frames: u8,
    slots: Vec<Option<SrsV2ReferenceSlot>>,
    next_order: u64,
}

impl SrsV2ReferenceManager {
    pub fn new(max_ref_frames: u8) -> Result<Self, SrsV2Error> {
        if max_ref_frames > MAX_REF_FRAMES {
            return Err(SrsV2Error::ExcessiveReferenceFrames(max_ref_frames));
        }
        let n = max_ref_frames as usize;
        Ok(Self {
            max_ref_frames,
            slots: vec![None; n],
            next_order: 0,
        })
    }

    pub fn max_ref_frames(&self) -> u8 {
        self.max_ref_frames
    }

    pub fn clear(&mut self) {
        for s in &mut self.slots {
            *s = None;
        }
        self.next_order = 0;
    }

    pub fn validate_slot_index(&self, idx: u8) -> Result<(), SrsV2Error> {
        if self.slots.is_empty() {
            return Err(SrsV2Error::syntax(
                "no reference storage (max_ref_frames=0)",
            ));
        }
        if idx as usize >= self.slots.len() {
            return Err(SrsV2Error::syntax("reference slot id out of range"));
        }
        Ok(())
    }

    /// Legacy single-reference view: **slot 0** when populated.
    pub fn primary_ref(&self) -> Option<&YuvFrame> {
        self.slots
            .first()
            .and_then(|s| s.as_ref())
            .map(|x| &x.frame)
    }

    pub fn frame_at_slot_index(&self, idx: u8) -> Result<&YuvFrame, SrsV2Error> {
        self.validate_slot_index(idx)?;
        self.slots[idx as usize]
            .as_ref()
            .map(|s| &s.frame)
            .ok_or(SrsV2Error::PFrameWithoutReference)
    }

    pub fn slot_frame_index(&self, idx: u8) -> Result<u32, SrsV2Error> {
        self.validate_slot_index(idx)?;
        self.slots[idx as usize]
            .as_ref()
            .map(|s| s.frame_index)
            .ok_or(SrsV2Error::PFrameWithoutReference)
    }

    /// Largest stored `frame_index` strictly less than `target` (if any).
    pub fn nearest_previous_frame_index(&self, target: u32) -> Option<u32> {
        let mut best: Option<u32> = None;
        for slot in self.slots.iter().flatten() {
            if slot.frame_index < target {
                best = Some(match best {
                    None => slot.frame_index,
                    Some(b) => b.max(slot.frame_index),
                });
            }
        }
        best
    }

    /// Smallest stored `frame_index` strictly greater than `target` (if any).
    pub fn future_reference_if_any(&self, target: u32) -> Option<&YuvFrame> {
        let mut best_idx: Option<u32> = None;
        let mut best_frame: Option<&YuvFrame> = None;
        for slot in self.slots.iter().flatten() {
            if slot.frame_index > target {
                let replace = match best_idx {
                    None => true,
                    Some(b) => slot.frame_index < b,
                };
                if replace {
                    best_idx = Some(slot.frame_index);
                    best_frame = Some(&slot.frame);
                }
            }
        }
        best_frame
    }

    /// Replace buffer after an intra refresh (drops hidden refs).
    pub fn replace_after_keyframe(&mut self, frame_index: u32, frame: YuvFrame) {
        self.clear();
        if self.slots.is_empty() {
            return;
        }
        self.slots[0] = Some(SrsV2ReferenceSlot {
            slot_id: 0,
            frame_index,
            order_index: 0,
            kind: SrsV2ReferenceKind::Golden,
            frame,
            is_displayable: true,
        });
        self.next_order = 1;
    }

    /// Push a **displayable** picture as **slot 0**; shift older references toward higher indices.
    pub fn push_displayable_last(&mut self, frame_index: u32, frame: YuvFrame) {
        if self.slots.is_empty() {
            return;
        }
        let n = self.slots.len();
        if n >= 2 {
            for i in (1..n).rev() {
                self.slots[i] = self.slots[i - 1].take();
            }
        }
        let order = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        self.slots[0] = Some(SrsV2ReferenceSlot {
            slot_id: 0,
            frame_index,
            order_index: order,
            kind: SrsV2ReferenceKind::Last,
            frame,
            is_displayable: true,
        });
    }

    pub fn store_alt_ref_at(
        &mut self,
        slot_id: u8,
        frame_index: u32,
        frame: YuvFrame,
    ) -> Result<(), SrsV2Error> {
        self.validate_slot_index(slot_id)?;
        let order = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        self.slots[slot_id as usize] = Some(SrsV2ReferenceSlot {
            slot_id,
            frame_index,
            order_index: order,
            kind: SrsV2ReferenceKind::AltRef,
            frame,
            is_displayable: false,
        });
        Ok(())
    }

    /// Bootstrap **slot 0** from a legacy playback reference.
    pub fn bootstrap_legacy_primary(&mut self, frame_index: u32, frame: YuvFrame) {
        if self.slots.is_empty() {
            return;
        }
        let order = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        self.slots[0] = Some(SrsV2ReferenceSlot {
            slot_id: 0,
            frame_index,
            order_index: order,
            kind: SrsV2ReferenceKind::Last,
            frame,
            is_displayable: true,
        });
    }

    pub fn legacy_primary_clone(&self) -> Option<YuvFrame> {
        self.slots
            .first()
            .and_then(|s| s.as_ref())
            .map(|x| x.frame.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srsv2::frame::VideoPlane;
    use crate::srsv2::model::PixelFormat;

    fn tiny_gray_frame(v: u8) -> YuvFrame {
        let y = VideoPlane::try_new(16, 16, 16).unwrap();
        let mut u = VideoPlane::try_new(8, 8, 8).unwrap();
        let mut vv = VideoPlane::try_new(8, 8, 8).unwrap();
        let mut yy = y;
        yy.samples.fill(v);
        u.samples.fill(128);
        vv.samples.fill(128);
        YuvFrame {
            format: PixelFormat::Yuv420p8,
            y: yy,
            u,
            v: vv,
        }
    }

    #[test]
    fn max_ref_zero_empty_slots() {
        let m = SrsV2ReferenceManager::new(0).unwrap();
        assert!(m.slots.is_empty());
        assert!(m.primary_ref().is_none());
    }

    #[test]
    fn max_ref_one_keeps_last_in_slot_zero() {
        let mut m = SrsV2ReferenceManager::new(1).unwrap();
        m.push_displayable_last(0, tiny_gray_frame(40));
        assert!(m.primary_ref().is_some());
        m.push_displayable_last(1, tiny_gray_frame(50));
        assert_eq!(m.slots[0].as_ref().unwrap().frame_index, 1);
    }

    #[test]
    fn max_ref_two_stores_multiple() {
        let mut m = SrsV2ReferenceManager::new(2).unwrap();
        m.push_displayable_last(0, tiny_gray_frame(10));
        m.push_displayable_last(1, tiny_gray_frame(20));
        assert!(m.frame_at_slot_index(0).is_ok());
        assert!(m.frame_at_slot_index(1).is_ok());
    }

    #[test]
    fn invalid_slot_fails() {
        let m = SrsV2ReferenceManager::new(1).unwrap();
        assert!(m.validate_slot_index(1).is_err());
        assert!(m.frame_at_slot_index(2).is_err());
    }

    #[test]
    fn slot_frame_index_tracks_picture_order() {
        let mut m = SrsV2ReferenceManager::new(2).unwrap();
        m.push_displayable_last(10, tiny_gray_frame(1));
        m.push_displayable_last(20, tiny_gray_frame(2));
        assert_eq!(m.slot_frame_index(0).unwrap(), 20);
        assert_eq!(m.slot_frame_index(1).unwrap(), 10);
    }

    #[test]
    fn clear_removes_all() {
        let mut m = SrsV2ReferenceManager::new(2).unwrap();
        m.push_displayable_last(0, tiny_gray_frame(1));
        m.clear();
        assert!(m.primary_ref().is_none());
    }
}
