//! CTU-style superblock planning/scaffolding for future SRSV2 work.
//!
//! This module is deliberately **geometry only**:
//! - no encoder decisions,
//! - no decoder syntax,
//! - no payload bytes,
//! - no `FR2` revision assignment.
//!
//! It gives future HEVC-class architecture work a hostile-input-safe way to
//! divide a YUV420 frame into CTU-sized regions and map each CTU to luma/chroma
//! plane bounds. The current codec paths still use their existing 16x16
//! macroblock and experimental partition syntax. Treat every API here as
//! planning scaffolding until a later change explicitly designs bitstream
//! syntax and compatibility rules.

use thiserror::Error;

use super::limits::{MAX_DIMENSION, MAX_LUMA_SAMPLES};

/// Hard cap on CTUs per frame for planning data structures.
///
/// 8K UHD (7680x4320) with 16x16 CTUs is 129_600 CTUs. This cap leaves room for
/// square 8192x8192 pictures with 16x16 CTUs while still rejecting accidental or
/// hostile over-allocation.
pub const MAX_CTU_COUNT: u64 = 262_144;

/// Candidate CTU edge sizes for the future superblock layer.
///
/// `Ctu16` represents compatibility with today's macroblock-scale thinking,
/// while `Ctu32` and `Ctu64` are architecture planning targets. None of these
/// values imply new wire syntax yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtuSize {
    Ctu16,
    Ctu32,
    Ctu64,
}

impl CtuSize {
    /// Edge length in luma pixels.
    pub const fn edge_luma(self) -> u32 {
        match self {
            Self::Ctu16 => 16,
            Self::Ctu32 => 32,
            Self::Ctu64 => 64,
        }
    }
}

/// Raster address of one CTU in a [`CtuGrid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtuAddress {
    pub x_ctu: u32,
    pub y_ctu: u32,
    pub raster_index: u32,
}

/// Half-open luma and chroma plane bounds for a CTU.
///
/// Bounds are `[start, end)` in each plane. For YUV420, chroma coordinates use
/// ceil-divided frame dimensions and ceil-divided CTU luma coordinates so odd
/// frame edges remain addressable without stepping outside allocated planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtuBounds {
    pub luma_x0: u32,
    pub luma_y0: u32,
    pub luma_x1: u32,
    pub luma_y1: u32,
    pub chroma_x0: u32,
    pub chroma_y0: u32,
    pub chroma_x1: u32,
    pub chroma_y1: u32,
}

impl CtuBounds {
    pub fn luma_width(self) -> u32 {
        self.luma_x1 - self.luma_x0
    }

    pub fn luma_height(self) -> u32 {
        self.luma_y1 - self.luma_y0
    }

    pub fn chroma_width(self) -> u32 {
        self.chroma_x1 - self.chroma_x0
    }

    pub fn chroma_height(self) -> u32 {
        self.chroma_y1 - self.chroma_y0
    }
}

/// Validated CTU grid for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtuGrid {
    pub width: u32,
    pub height: u32,
    pub ctu_size: CtuSize,
    pub cols: u32,
    pub rows: u32,
    pub count: u32,
}

/// Aggregate CTU grid report used by benchmark telemetry.
///
/// This is still reporting-only planning data. It is not a syntax summary and
/// must not be interpreted as proof that CTU encoding exists.
#[derive(Debug, Clone, PartialEq)]
pub struct CtuGridStats {
    pub ctu_size: u32,
    pub ctu_cols: u32,
    pub ctu_rows: u32,
    pub ctu_count: u32,
    /// CTUs whose luma area is clipped by the right or bottom frame edge.
    pub edge_ctu_count: u32,
    /// Average actual luma samples per CTU, including partial edge CTUs.
    pub average_ctu_luma_area: f64,
}

impl CtuGrid {
    /// Build a hostile-input-checked CTU grid.
    pub fn new(width: u32, height: u32, ctu_size: CtuSize) -> Result<Self, CtuError> {
        validate_frame_dimensions(width, height)?;
        let edge = ctu_size.edge_luma();
        let cols = div_ceil_u32(width, edge)?;
        let rows = div_ceil_u32(height, edge)?;
        let count_u64 = u64::from(cols)
            .checked_mul(u64::from(rows))
            .ok_or(CtuError::GridTooLarge)?;
        if count_u64 == 0 || count_u64 > MAX_CTU_COUNT || count_u64 > u64::from(u32::MAX) {
            return Err(CtuError::GridTooLarge);
        }
        Ok(Self {
            width,
            height,
            ctu_size,
            cols,
            rows,
            count: count_u64 as u32,
        })
    }

    /// Return the CTU address for a raster index.
    pub fn address_for_index(&self, raster_index: u32) -> Result<CtuAddress, CtuError> {
        if raster_index >= self.count {
            return Err(CtuError::CtuIndexOutOfRange {
                index: raster_index,
                count: self.count,
            });
        }
        let x_ctu = raster_index % self.cols;
        let y_ctu = raster_index / self.cols;
        Ok(CtuAddress {
            x_ctu,
            y_ctu,
            raster_index,
        })
    }

    /// Iterate CTU addresses in raster order.
    pub fn addresses(&self) -> impl Iterator<Item = CtuAddress> + '_ {
        (0..self.count).map(|i| CtuAddress {
            x_ctu: i % self.cols,
            y_ctu: i / self.cols,
            raster_index: i,
        })
    }

    /// Convert a CTU address to YUV420 luma/chroma bounds.
    pub fn bounds_for_address(&self, addr: CtuAddress) -> Result<CtuBounds, CtuError> {
        if addr.x_ctu >= self.cols || addr.y_ctu >= self.rows || addr.raster_index >= self.count {
            return Err(CtuError::CtuAddressOutOfRange);
        }
        let expected_index = addr
            .y_ctu
            .checked_mul(self.cols)
            .and_then(|v| v.checked_add(addr.x_ctu))
            .ok_or(CtuError::GridTooLarge)?;
        if expected_index != addr.raster_index {
            return Err(CtuError::CtuAddressOutOfRange);
        }

        let edge = self.ctu_size.edge_luma();
        let x0 = addr.x_ctu.checked_mul(edge).ok_or(CtuError::GridTooLarge)?;
        let y0 = addr.y_ctu.checked_mul(edge).ok_or(CtuError::GridTooLarge)?;
        let x1 = x0.saturating_add(edge).min(self.width);
        let y1 = y0.saturating_add(edge).min(self.height);

        Ok(CtuBounds {
            luma_x0: x0,
            luma_y0: y0,
            luma_x1: x1,
            luma_y1: y1,
            chroma_x0: x0 / 2,
            chroma_y0: y0 / 2,
            chroma_x1: div_ceil_u32(x1, 2)?,
            chroma_y1: div_ceil_u32(y1, 2)?,
        })
    }

    /// Convert a raster index directly to YUV420 luma/chroma bounds.
    pub fn bounds_for_index(&self, raster_index: u32) -> Result<CtuBounds, CtuError> {
        let addr = self.address_for_index(raster_index)?;
        self.bounds_for_address(addr)
    }

    /// Reporting-only aggregate stats for this grid.
    pub fn stats(&self) -> Result<CtuGridStats, CtuError> {
        let full_edge = self.ctu_size.edge_luma();
        let mut edge_ctu_count = 0u32;
        let mut total_area = 0u64;
        for addr in self.addresses() {
            let b = self.bounds_for_address(addr)?;
            if b.luma_width() < full_edge || b.luma_height() < full_edge {
                edge_ctu_count = edge_ctu_count.saturating_add(1);
            }
            total_area = total_area
                .checked_add(u64::from(b.luma_width()) * u64::from(b.luma_height()))
                .ok_or(CtuError::GridTooLarge)?;
        }
        Ok(CtuGridStats {
            ctu_size: full_edge,
            ctu_cols: self.cols,
            ctu_rows: self.rows,
            ctu_count: self.count,
            edge_ctu_count,
            average_ctu_luma_area: total_area as f64 / self.count as f64,
        })
    }
}

/// Split a frame into a validated CTU grid.
pub fn split_frame_into_ctu_grid(
    width: u32,
    height: u32,
    ctu_size: CtuSize,
) -> Result<CtuGrid, CtuError> {
    CtuGrid::new(width, height, ctu_size)
}

/// Map one CTU raster index to YUV420 luma/chroma bounds.
pub fn map_ctu_to_yuv420_bounds(grid: &CtuGrid, raster_index: u32) -> Result<CtuBounds, CtuError> {
    grid.bounds_for_index(raster_index)
}

/// Build reporting-only CTU grid stats in one call.
pub fn ctu_grid_stats(
    width: u32,
    height: u32,
    ctu_size: CtuSize,
) -> Result<CtuGridStats, CtuError> {
    split_frame_into_ctu_grid(width, height, ctu_size)?.stats()
}

/// Validate dimensions against existing SRSV2 hostile-input frame caps.
pub fn validate_frame_dimensions(width: u32, height: u32) -> Result<(), CtuError> {
    if width == 0 || height == 0 {
        return Err(CtuError::ZeroDimensions);
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(CtuError::DimensionTooLarge { width, height });
    }
    let samples = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(CtuError::DimensionOverflow)?;
    if samples > MAX_LUMA_SAMPLES {
        return Err(CtuError::LumaSamplesTooLarge {
            samples,
            max: MAX_LUMA_SAMPLES,
        });
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CtuError {
    #[error("frame dimensions must be non-zero")]
    ZeroDimensions,
    #[error("frame dimension too large (width={width}, height={height})")]
    DimensionTooLarge { width: u32, height: u32 },
    #[error("frame dimension multiplication overflow")]
    DimensionOverflow,
    #[error("luma sample count {samples} exceeds max {max}")]
    LumaSamplesTooLarge { samples: u64, max: u64 },
    #[error("CTU grid too large")]
    GridTooLarge,
    #[error("CTU index {index} out of range (count={count})")]
    CtuIndexOutOfRange { index: u32, count: u32 },
    #[error("CTU address is outside this grid or has the wrong raster index")]
    CtuAddressOutOfRange,
}

fn div_ceil_u32(n: u32, d: u32) -> Result<u32, CtuError> {
    if d == 0 {
        return Err(CtuError::GridTooLarge);
    }
    let adjusted = n.checked_add(d - 1).ok_or(CtuError::GridTooLarge)?;
    Ok(adjusted / d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_twenty_eight_square_splits_to_four_ctu64s() {
        let g = split_frame_into_ctu_grid(128, 128, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 2);
        assert_eq!(g.rows, 2);
        assert_eq!(g.count, 4);
        let bounds: Vec<_> = g
            .addresses()
            .map(|a| g.bounds_for_address(a).unwrap())
            .collect();
        assert_eq!(bounds[0].luma_width(), 64);
        assert_eq!(bounds[0].luma_height(), 64);
        assert_eq!(bounds[3].luma_x0, 64);
        assert_eq!(bounds[3].luma_y0, 64);
        assert_eq!(bounds[3].luma_x1, 128);
        assert_eq!(bounds[3].luma_y1, 128);
    }

    #[test]
    fn nineteen_twenty_by_ten_eighty_has_safe_partial_edges() {
        let g = split_frame_into_ctu_grid(1920, 1080, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 30);
        assert_eq!(g.rows, 17);
        assert_eq!(g.count, 510);

        let last = g.bounds_for_index(g.count - 1).unwrap();
        assert_eq!(last.luma_x0, 1856);
        assert_eq!(last.luma_x1, 1920);
        assert_eq!(last.luma_y0, 1024);
        assert_eq!(last.luma_y1, 1080);
        assert_eq!(last.luma_width(), 64);
        assert_eq!(last.luma_height(), 56);
        assert_eq!(last.chroma_x1, 960);
        assert_eq!(last.chroma_y1, 540);
    }

    #[test]
    fn eight_k_grid_count_is_bounded() {
        let g = split_frame_into_ctu_grid(7680, 4320, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 120);
        assert_eq!(g.rows, 68);
        assert_eq!(g.count, 8160);
        assert!((g.count as u64) < MAX_CTU_COUNT);

        let dense = split_frame_into_ctu_grid(7680, 4320, CtuSize::Ctu16).unwrap();
        assert_eq!(dense.cols, 480);
        assert_eq!(dense.rows, 270);
        assert_eq!(dense.count, 129_600);
        assert!((dense.count as u64) < MAX_CTU_COUNT);
    }

    #[test]
    fn zero_dimensions_rejected() {
        assert_eq!(
            split_frame_into_ctu_grid(0, 64, CtuSize::Ctu64),
            Err(CtuError::ZeroDimensions)
        );
        assert_eq!(
            split_frame_into_ctu_grid(64, 0, CtuSize::Ctu64),
            Err(CtuError::ZeroDimensions)
        );
    }

    #[test]
    fn overflow_dimensions_rejected() {
        assert_eq!(
            split_frame_into_ctu_grid(u32::MAX, 64, CtuSize::Ctu64),
            Err(CtuError::DimensionTooLarge {
                width: u32::MAX,
                height: 64
            })
        );
        assert_eq!(
            split_frame_into_ctu_grid(8192, 8192, CtuSize::Ctu16),
            Err(CtuError::LumaSamplesTooLarge {
                samples: 67_108_864,
                max: MAX_LUMA_SAMPLES
            })
        );
    }

    #[test]
    fn chroma_bounds_valid_for_yuv420() {
        let g = split_frame_into_ctu_grid(65, 65, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 2);
        assert_eq!(g.rows, 2);

        let top_left = map_ctu_to_yuv420_bounds(&g, 0).unwrap();
        assert_eq!(top_left.luma_x0, 0);
        assert_eq!(top_left.luma_x1, 64);
        assert_eq!(top_left.chroma_x0, 0);
        assert_eq!(top_left.chroma_x1, 32);
        assert_eq!(top_left.chroma_y1, 32);

        let bottom_right = map_ctu_to_yuv420_bounds(&g, 3).unwrap();
        assert_eq!(bottom_right.luma_x0, 64);
        assert_eq!(bottom_right.luma_x1, 65);
        assert_eq!(bottom_right.luma_y0, 64);
        assert_eq!(bottom_right.luma_y1, 65);
        assert_eq!(bottom_right.chroma_x0, 32);
        assert_eq!(bottom_right.chroma_x1, 33);
        assert_eq!(bottom_right.chroma_y0, 32);
        assert_eq!(bottom_right.chroma_y1, 33);
        assert!(bottom_right.chroma_x1 <= 65_u32.div_ceil(2));
        assert!(bottom_right.chroma_y1 <= 65_u32.div_ceil(2));
    }

    #[test]
    fn reporting_stats_count_partial_edge_ctus() {
        let s = ctu_grid_stats(1920, 1080, CtuSize::Ctu64).unwrap();
        assert_eq!(s.ctu_size, 64);
        assert_eq!(s.ctu_cols, 30);
        assert_eq!(s.ctu_rows, 17);
        assert_eq!(s.ctu_count, 510);
        assert_eq!(s.edge_ctu_count, 30);
        assert_eq!(s.average_ctu_luma_area, (1920 * 1080) as f64 / 510.0);
    }
}
