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
//! plane bounds. The current codec paths still use their existing 16×16
//! macroblock and experimental partition syntax. Treat every API here as
//! planning scaffolding until a later change explicitly designs bitstream
//! syntax and compatibility rules.

use thiserror::Error;

use super::limits::{MAX_DIMENSION, MAX_LUMA_SAMPLES};

/// Hard cap on CTUs per frame for planning data structures.
///
/// 8K UHD (7680×4320) with 16×16 CTUs is 129_600 CTUs. This cap leaves room for
/// square 8192×8192 pictures with 16×16 CTUs while still rejecting accidental or
/// hostile over-allocation.
pub const MAX_CTU_COUNT: u64 = 262_144;

/// Candidate CTU edge sizes for the future superblock layer.
///
/// `Ctu16` represents compatibility with today's macroblock-scale thinking,
/// while `Ctu32` and `Ctu64` are architecture planning targets. None of these
/// values imply new wire syntax yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CtuAddress {
    pub x_ctu: u32,
    pub y_ctu: u32,
    pub raster_index: u32,
}

/// Half-open `[x0, x1) × [y0, y1)` bounds on a single plane (luma or chroma samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CtuPlaneBounds {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl CtuPlaneBounds {
    #[inline]
    pub fn width(self) -> u32 {
        self.x1.saturating_sub(self.x0)
    }

    #[inline]
    pub fn height(self) -> u32 {
        self.y1.saturating_sub(self.y0)
    }
}

/// Combined luma and chroma ([`CtuPlaneBounds`]) for one CTU in YUV420.
///
/// Chroma coordinates use half-base origins with **ceil** on the exclusive end
/// derived from luma so odd frame edges and partial edge CTUs stay inside the
/// chroma plane `(ceil(width/2), ceil(height/2))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CtuBounds {
    pub luma: CtuPlaneBounds,
    pub chroma: CtuPlaneBounds,
}

impl CtuBounds {
    #[inline]
    pub fn luma_width(self) -> u32 {
        self.luma.width()
    }

    #[inline]
    pub fn luma_height(self) -> u32 {
        self.luma.height()
    }

    #[inline]
    pub fn chroma_width(self) -> u32 {
        self.chroma.width()
    }

    #[inline]
    pub fn chroma_height(self) -> u32 {
        self.chroma.height()
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
    pub avg_ctu_luma_area: f64,
    /// Largest single-CTU luma sample count in this grid (partial edge CTUs can be smaller).
    pub max_ctu_luma_area: u64,
}

impl CtuGrid {
    /// Build a hostile-input-checked CTU grid (same checks as [`validate_ctu_grid`]).
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

    /// Number of CTUs in this grid (`cols * rows`).
    #[inline]
    pub fn ctu_count(&self) -> u32 {
        self.count
    }

    /// Half-open luma + YUV420 chroma bounds for CTU `raster_index` (row-major).
    #[inline]
    pub fn ctu_bounds(&self, raster_index: u32) -> Result<CtuBounds, CtuError> {
        self.bounds_for_index(raster_index)
    }

    /// Iterate [`CtuAddress`] in raster order (same order as [`CtuGrid::addresses`]).
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = CtuAddress> + '_ {
        self.addresses()
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
            luma: CtuPlaneBounds { x0, y0, x1, y1 },
            chroma: CtuPlaneBounds {
                x0: chroma_x0_yuv420(x0),
                y0: chroma_y0_yuv420(y0),
                x1: chroma_x1_yuv420(x1)?,
                y1: chroma_y1_yuv420(y1)?,
            },
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
        let mut max_ctu_luma_area = 0u64;
        for addr in self.addresses() {
            let b = self.bounds_for_address(addr)?;
            if b.luma_width() < full_edge || b.luma_height() < full_edge {
                edge_ctu_count = edge_ctu_count.saturating_add(1);
            }
            let cell = u64::from(b.luma_width())
                .checked_mul(u64::from(b.luma_height()))
                .ok_or(CtuError::GridTooLarge)?;
            total_area = total_area.checked_add(cell).ok_or(CtuError::GridTooLarge)?;
            max_ctu_luma_area = max_ctu_luma_area.max(cell);
        }
        Ok(CtuGridStats {
            ctu_size: full_edge,
            ctu_cols: self.cols,
            ctu_rows: self.rows,
            ctu_count: self.count,
            edge_ctu_count,
            avg_ctu_luma_area: total_area as f64 / self.count as f64,
            max_ctu_luma_area,
        })
    }
}

#[inline]
fn chroma_x0_yuv420(luma_x0: u32) -> u32 {
    luma_x0 / 2
}

#[inline]
fn chroma_y0_yuv420(luma_y0: u32) -> u32 {
    luma_y0 / 2
}

#[inline]
fn chroma_x1_yuv420(luma_x1: u32) -> Result<u32, CtuError> {
    div_ceil_u32(luma_x1, 2)
}

#[inline]
fn chroma_y1_yuv420(luma_y1: u32) -> Result<u32, CtuError> {
    div_ceil_u32(luma_y1, 2)
}

/// Luma [`CtuPlaneBounds`] for CTU `raster_index`.
pub fn ctu_luma_bounds(grid: &CtuGrid, raster_index: u32) -> Result<CtuPlaneBounds, CtuError> {
    Ok(grid.ctu_bounds(raster_index)?.luma)
}

/// Chroma [`CtuPlaneBounds`] for CTU `raster_index` (YUV420 semantics).
pub fn ctu_chroma_bounds_yuv420(
    grid: &CtuGrid,
    raster_index: u32,
) -> Result<CtuPlaneBounds, CtuError> {
    Ok(grid.ctu_bounds(raster_index)?.chroma)
}

/// Validate frame dimensions and that the implied CTU grid fits caps (`[`MAX_CTU_COUNT`]`, etc.).
pub fn validate_ctu_grid(width: u32, height: u32, ctu_size: CtuSize) -> Result<(), CtuError> {
    CtuGrid::new(width, height, ctu_size).map(|_| ())
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

    fn chroma_plane_extent(width: u32, height: u32) -> (u32, u32) {
        (
            div_ceil_u32(width, 2).unwrap(),
            div_ceil_u32(height, 2).unwrap(),
        )
    }

    fn assert_chroma_inside_plane(grid: &CtuGrid, bounds: &CtuBounds) {
        let (cw, ch) = chroma_plane_extent(grid.width, grid.height);
        assert!(bounds.chroma.x0 <= bounds.chroma.x1);
        assert!(bounds.chroma.y0 <= bounds.chroma.y1);
        assert!(bounds.chroma.x1 <= cw);
        assert!(bounds.chroma.y1 <= ch);
        assert!(bounds.chroma.width() <= bounds.luma.width().div_ceil(2));
        assert!(bounds.chroma.height() <= bounds.luma.height().div_ceil(2));
    }

    #[test]
    fn one_twenty_eight_square_splits_to_four_ctu64s() {
        let g = CtuGrid::new(128, 128, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 2);
        assert_eq!(g.rows, 2);
        assert_eq!(g.ctu_count(), 4);
        assert_eq!(g.count, g.ctu_count());
        let bounds: Vec<_> = g.iter().map(|a| g.bounds_for_address(a).unwrap()).collect();
        assert_eq!(bounds[0].luma_width(), 64);
        assert_eq!(bounds[0].luma_height(), 64);
        assert_eq!(bounds[3].luma.x0, 64);
        assert_eq!(bounds[3].luma.y0, 64);
        assert_eq!(bounds[3].luma.x1, 128);
        assert_eq!(bounds[3].luma.y1, 128);
    }

    #[test]
    fn nineteen_twenty_by_ten_eighty_has_safe_partial_edges() {
        let g = CtuGrid::new(1920, 1080, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 30);
        assert_eq!(g.rows, 17);
        assert_eq!(g.ctu_count(), 510);

        let last = g.ctu_bounds(g.ctu_count() - 1).unwrap();
        assert_eq!(last.luma.x0, 1856);
        assert_eq!(last.luma.x1, 1920);
        assert_eq!(last.luma.y0, 1024);
        assert_eq!(last.luma.y1, 1080);
        assert_eq!(last.luma_width(), 64);
        assert_eq!(last.luma_height(), 56);
        assert_eq!(last.chroma.x1, 960);
        assert_eq!(last.chroma.y1, 540);
        assert_chroma_inside_plane(&g, &last);
    }

    #[test]
    fn eight_k_grid_accepted_and_bounded() {
        let g = CtuGrid::new(7680, 4320, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 120);
        assert_eq!(g.rows, 68);
        assert_eq!(g.ctu_count(), 8160);
        assert!((g.ctu_count() as u64) < MAX_CTU_COUNT);

        let dense = CtuGrid::new(7680, 4320, CtuSize::Ctu16).unwrap();
        assert_eq!(dense.cols, 480);
        assert_eq!(dense.rows, 270);
        assert_eq!(dense.ctu_count(), 129_600);
        assert!((dense.ctu_count() as u64) < MAX_CTU_COUNT);
    }

    #[test]
    fn zero_dimensions_rejected() {
        assert_eq!(
            validate_ctu_grid(0, 64, CtuSize::Ctu64),
            Err(CtuError::ZeroDimensions)
        );
        assert_eq!(
            validate_ctu_grid(64, 0, CtuSize::Ctu64),
            Err(CtuError::ZeroDimensions)
        );
    }

    #[test]
    fn oversized_dimensions_rejected() {
        assert_eq!(
            CtuGrid::new(u32::MAX, 64, CtuSize::Ctu64),
            Err(CtuError::DimensionTooLarge {
                width: u32::MAX,
                height: 64
            })
        );
        assert_eq!(
            CtuGrid::new(8192, 8192, CtuSize::Ctu16),
            Err(CtuError::LumaSamplesTooLarge {
                samples: 67_108_864,
                max: MAX_LUMA_SAMPLES
            })
        );
    }

    #[test]
    fn chroma_bounds_valid_for_yuv420() {
        let g = CtuGrid::new(65, 65, CtuSize::Ctu64).unwrap();
        assert_eq!(g.cols, 2);
        assert_eq!(g.rows, 2);

        let top_left = map_ctu_to_yuv420_bounds(&g, 0).unwrap();
        assert_eq!(top_left.luma.x0, 0);
        assert_eq!(top_left.luma.x1, 64);
        assert_eq!(top_left.chroma.x0, 0);
        assert_eq!(top_left.chroma.x1, 32);
        assert_eq!(top_left.chroma.y1, 32);
        assert_chroma_inside_plane(&g, &top_left);

        let bottom_right = map_ctu_to_yuv420_bounds(&g, 3).unwrap();
        assert_eq!(bottom_right.luma.x0, 64);
        assert_eq!(bottom_right.luma.x1, 65);
        assert_eq!(bottom_right.luma.y0, 64);
        assert_eq!(bottom_right.luma.y1, 65);
        assert_eq!(bottom_right.chroma.x0, 32);
        assert_eq!(bottom_right.chroma.x1, 33);
        assert_eq!(bottom_right.chroma.y0, 32);
        assert_eq!(bottom_right.chroma.y1, 33);
        assert!(bottom_right.chroma.x1 <= 65_u32.div_ceil(2));
        assert!(bottom_right.chroma.y1 <= 65_u32.div_ceil(2));
        assert_chroma_inside_plane(&g, &bottom_right);

        assert_eq!(ctu_luma_bounds(&g, 3).unwrap(), bottom_right.luma);
        assert_eq!(
            ctu_chroma_bounds_yuv420(&g, 3).unwrap(),
            bottom_right.chroma
        );
    }

    #[test]
    fn iter_covers_every_ctu_once() {
        let g = CtuGrid::new(1920, 1080, CtuSize::Ctu64).unwrap();
        let n = g.ctu_count() as usize;
        let mut seen = vec![false; n];
        for addr in g.iter() {
            assert!(addr.raster_index < g.ctu_count());
            assert!(
                !seen[addr.raster_index as usize],
                "duplicate raster_index {}",
                addr.raster_index
            );
            seen[addr.raster_index as usize] = true;
            let b = g.ctu_bounds(addr.raster_index).unwrap();
            assert_chroma_inside_plane(&g, &b);
        }
        assert!(
            seen.iter().all(|&x| x),
            "iterator missed {} CTUs",
            seen.iter().filter(|&&x| !x).count()
        );
    }

    #[test]
    fn reporting_stats_count_partial_edge_ctus() {
        let s = ctu_grid_stats(1920, 1080, CtuSize::Ctu64).unwrap();
        assert_eq!(s.ctu_size, 64);
        assert_eq!(s.ctu_cols, 30);
        assert_eq!(s.ctu_rows, 17);
        assert_eq!(s.ctu_count, 510);
        assert_eq!(s.edge_ctu_count, 30);
        assert_eq!(s.avg_ctu_luma_area, (1920 * 1080) as f64 / 510.0);
        assert_eq!(s.max_ctu_luma_area, 64 * 64);
    }

    #[test]
    fn stats_max_reflects_partial_edge_cell() {
        let s = ctu_grid_stats(96, 80, CtuSize::Ctu64).unwrap();
        assert_eq!(s.max_ctu_luma_area, 64 * 64);
        assert!(s.avg_ctu_luma_area < s.max_ctu_luma_area as f64);
    }
}
