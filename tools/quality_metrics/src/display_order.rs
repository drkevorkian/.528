//! Bounded helpers to reorder decoded luma rows by **`frame_index`** (presentation order).

use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DisplayOrderError {
    #[error("display reorder buffer exceeded max entries ({max})")]
    BufferExceeded { max: usize },
    #[error("duplicate frame_index {0}")]
    DuplicateFrameIndex(u32),
    #[error("missing decoded frames for indices {0:?}")]
    MissingFrames(Vec<u32>),
    #[error("plane size mismatch at frame_index {frame_index}: expected {expected}, got {got}")]
    PlaneLengthMismatch {
        frame_index: u32,
        expected: usize,
        got: usize,
    },
}

/// Collect displayable decoded luma planes keyed by **`frame_index`**, then flatten in **`expected_indices`** order.
#[derive(Debug)]
pub struct DisplayReorderBuffer {
    max_entries: usize,
    map: BTreeMap<u32, Vec<u8>>,
}

impl DisplayReorderBuffer {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            map: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn insert(&mut self, frame_index: u32, luma: Vec<u8>) -> Result<(), DisplayOrderError> {
        if self.map.contains_key(&frame_index) {
            return Err(DisplayOrderError::DuplicateFrameIndex(frame_index));
        }
        if self.map.len() >= self.max_entries {
            return Err(DisplayOrderError::BufferExceeded {
                max: self.max_entries,
            });
        }
        self.map.insert(frame_index, luma);
        Ok(())
    }

    /// Concatenate planes in the order given by **`expected_indices`** (typically `0..N`).
    pub fn flatten_expected(
        self,
        expected_indices: &[u32],
        plane_bytes: usize,
    ) -> Result<Vec<u8>, DisplayOrderError> {
        let mut missing = Vec::new();
        let mut out = Vec::with_capacity(expected_indices.len().saturating_mul(plane_bytes));
        for &fi in expected_indices {
            match self.map.get(&fi) {
                Some(row) => {
                    if row.len() != plane_bytes {
                        return Err(DisplayOrderError::PlaneLengthMismatch {
                            frame_index: fi,
                            expected: plane_bytes,
                            got: row.len(),
                        });
                    }
                    out.extend_from_slice(row);
                }
                None => missing.push(fi),
            }
        }
        if !missing.is_empty() {
            return Err(DisplayOrderError::MissingFrames(missing));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_order_i0_p2_b1_flattens_display_i0_b1_p2() {
        let plane = 4usize;
        let mut buf = DisplayReorderBuffer::new(16);
        buf.insert(0, vec![10, 11, 12, 13]).unwrap();
        buf.insert(2, vec![20, 21, 22, 23]).unwrap();
        buf.insert(1, vec![30, 31, 32, 33]).unwrap();
        let flat = buf.flatten_expected(&[0, 1, 2], plane).unwrap();
        assert_eq!(flat.len(), 12);
        assert_eq!(&flat[..4], &[10, 11, 12, 13]);
        assert_eq!(&flat[4..8], &[30, 31, 32, 33]);
        assert_eq!(&flat[8..12], &[20, 21, 22, 23]);
    }

    #[test]
    fn buffer_cap_enforced() {
        let mut buf = DisplayReorderBuffer::new(2);
        buf.insert(0, vec![0]).unwrap();
        buf.insert(1, vec![1]).unwrap();
        assert_eq!(
            buf.insert(2, vec![2]),
            Err(DisplayOrderError::BufferExceeded { max: 2 })
        );
    }

    #[test]
    fn duplicate_frame_index_rejected() {
        let mut buf = DisplayReorderBuffer::new(8);
        buf.insert(1, vec![1]).unwrap();
        assert_eq!(
            buf.insert(1, vec![9]),
            Err(DisplayOrderError::DuplicateFrameIndex(1))
        );
    }

    #[test]
    fn missing_index_errors() {
        let mut buf = DisplayReorderBuffer::new(8);
        buf.insert(0, vec![0; 2]).unwrap();
        let err = buf.flatten_expected(&[0, 1], 2).unwrap_err();
        assert!(matches!(err, DisplayOrderError::MissingFrames(ref v) if v == &[1]));
    }
}
