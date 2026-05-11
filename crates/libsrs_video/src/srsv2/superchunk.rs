//! Large **superchunk** grid scaffolding (**128×128** … **1024×1024** luma regions) for 4K+ planning.
//!
//! **Geometry only** — same contract as [`super::ctu64`]:
//! - no encoder / decoder syntax,
//! - no `FR2` revision,
//! - no transform over the whole superchunk (regions will contain CTUs / partitions later),
//! - **1024×1024** is allowed only as an indexing / tile-style unit (not a coding transform size).

use thiserror::Error;

use super::ctu64;

/// Hard cap on superchunks per frame (hostile-input safe; aligns with max picture size and minimum **128** edge).
pub const MAX_SUPERCHUNK_COUNT: u64 = 8192;

/// Superchunk edge sizes for coarse region indexing (**128** … **1024** luma pixels per step).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrsV2SuperchunkSize {
    Sc128,
    Sc256,
    Sc512,
    Sc1024,
}

impl SrsV2SuperchunkSize {
    /// Edge length in luma pixels.
    pub const fn edge_luma(self) -> u32 {
        match self {
            Self::Sc128 => 128,
            Self::Sc256 => 256,
            Self::Sc512 => 512,
            Self::Sc1024 => 1024,
        }
    }
}

/// Raster address of one superchunk in a [`SuperchunkGrid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SuperchunkAddress {
    pub x_sc: u32,
    pub y_sc: u32,
    pub raster_index: u32,
}

/// Half-open `[x0, x1) × [y0, y1)` bounds on one plane (luma or chroma samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SuperchunkPlaneBounds {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl SuperchunkPlaneBounds {
    #[inline]
    pub fn width(self) -> u32 {
        self.x1.saturating_sub(self.x0)
    }

    #[inline]
    pub fn height(self) -> u32 {
        self.y1.saturating_sub(self.y0)
    }
}

/// Luma + YUV420 chroma bounds for one superchunk (chroma uses half-resolution origins; exclusive end **ceil** from luma).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SuperchunkBounds {
    pub luma: SuperchunkPlaneBounds,
    pub chroma: SuperchunkPlaneBounds,
}

/// Validated superchunk grid for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperchunkGrid {
    pub width: u32,
    pub height: u32,
    pub size: SrsV2SuperchunkSize,
    pub cols: u32,
    pub rows: u32,
    count: u32,
}

impl SuperchunkGrid {
    /// Build a hostile-input-checked superchunk grid (same checks as [`validate_superchunk_grid`]).
    pub fn new(
        width: u32,
        height: u32,
        size: SrsV2SuperchunkSize,
    ) -> Result<Self, SuperchunkError> {
        validate_frame_dimensions(width, height)?;
        let edge = size.edge_luma();
        let cols = div_ceil_u32(width, edge)?;
        let rows = div_ceil_u32(height, edge)?;
        let count_u64 = u64::from(cols)
            .checked_mul(u64::from(rows))
            .ok_or(SuperchunkError::GridTooLarge)?;
        if count_u64 == 0 || count_u64 > MAX_SUPERCHUNK_COUNT || count_u64 > u64::from(u32::MAX) {
            return Err(SuperchunkError::GridTooLarge);
        }
        Ok(Self {
            width,
            height,
            size,
            cols,
            rows,
            count: count_u64 as u32,
        })
    }

    /// Number of superchunks (`cols * rows`).
    #[inline]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Half-open luma + YUV420 chroma bounds for superchunk `raster_index` (row-major).
    pub fn bounds(&self, raster_index: u32) -> Result<SuperchunkBounds, SuperchunkError> {
        self.bounds_for_index(raster_index)
    }

    /// Iterate [`SuperchunkAddress`] in raster order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = SuperchunkAddress> + '_ {
        self.addresses()
    }

    pub fn address_for_index(
        &self,
        raster_index: u32,
    ) -> Result<SuperchunkAddress, SuperchunkError> {
        if raster_index >= self.count {
            return Err(SuperchunkError::IndexOutOfRange {
                index: raster_index,
                count: self.count,
            });
        }
        let x_sc = raster_index % self.cols;
        let y_sc = raster_index / self.cols;
        Ok(SuperchunkAddress {
            x_sc,
            y_sc,
            raster_index,
        })
    }

    pub fn addresses(&self) -> impl Iterator<Item = SuperchunkAddress> + '_ {
        (0..self.count).map(|i| SuperchunkAddress {
            x_sc: i % self.cols,
            y_sc: i / self.cols,
            raster_index: i,
        })
    }

    pub fn bounds_for_address(
        &self,
        addr: SuperchunkAddress,
    ) -> Result<SuperchunkBounds, SuperchunkError> {
        if addr.x_sc >= self.cols || addr.y_sc >= self.rows || addr.raster_index >= self.count {
            return Err(SuperchunkError::AddressOutOfRange);
        }
        let expected_index = addr
            .y_sc
            .checked_mul(self.cols)
            .and_then(|v| v.checked_add(addr.x_sc))
            .ok_or(SuperchunkError::GridTooLarge)?;
        if expected_index != addr.raster_index {
            return Err(SuperchunkError::AddressOutOfRange);
        }

        let edge = self.size.edge_luma();
        let x0 = addr
            .x_sc
            .checked_mul(edge)
            .ok_or(SuperchunkError::ArithmeticOverflow)?;
        let y0 = addr
            .y_sc
            .checked_mul(edge)
            .ok_or(SuperchunkError::ArithmeticOverflow)?;
        let x1 = x0.saturating_add(edge).min(self.width);
        let y1 = y0.saturating_add(edge).min(self.height);

        Ok(SuperchunkBounds {
            luma: SuperchunkPlaneBounds { x0, y0, x1, y1 },
            chroma: SuperchunkPlaneBounds {
                x0: chroma_x0_yuv420(x0),
                y0: chroma_y0_yuv420(y0),
                x1: chroma_x1_yuv420(x1)?,
                y1: chroma_y1_yuv420(y1)?,
            },
        })
    }

    pub fn bounds_for_index(&self, raster_index: u32) -> Result<SuperchunkBounds, SuperchunkError> {
        let addr = self.address_for_index(raster_index)?;
        self.bounds_for_address(addr)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SuperchunkError {
    #[error("frame dimensions must be non-zero")]
    ZeroDimensions,
    #[error("frame dimension too large (width={width}, height={height})")]
    DimensionTooLarge { width: u32, height: u32 },
    #[error("frame dimension multiplication overflow")]
    DimensionOverflow,
    #[error("luma sample count {samples} exceeds max {max}")]
    LumaSamplesTooLarge { samples: u64, max: u64 },
    #[error("superchunk grid too large or arithmetic overflow while building grid")]
    GridTooLarge,
    #[error("integer overflow in superchunk geometry")]
    ArithmeticOverflow,
    #[error("superchunk index {index} out of range (count={count})")]
    IndexOutOfRange { index: u32, count: u32 },
    #[error("superchunk address is outside this grid or has the wrong raster index")]
    AddressOutOfRange,
}

fn map_ctu_err(e: ctu64::CtuError) -> SuperchunkError {
    match e {
        ctu64::CtuError::ZeroDimensions => SuperchunkError::ZeroDimensions,
        ctu64::CtuError::DimensionTooLarge { width, height } => {
            SuperchunkError::DimensionTooLarge { width, height }
        }
        ctu64::CtuError::DimensionOverflow => SuperchunkError::DimensionOverflow,
        ctu64::CtuError::LumaSamplesTooLarge { samples, max } => {
            SuperchunkError::LumaSamplesTooLarge { samples, max }
        }
        ctu64::CtuError::GridTooLarge => SuperchunkError::GridTooLarge,
        ctu64::CtuError::CtuIndexOutOfRange { .. } | ctu64::CtuError::CtuAddressOutOfRange => {
            SuperchunkError::GridTooLarge
        }
    }
}

fn validate_frame_dimensions(width: u32, height: u32) -> Result<(), SuperchunkError> {
    ctu64::validate_frame_dimensions(width, height).map_err(map_ctu_err)
}

fn div_ceil_u32(n: u32, d: u32) -> Result<u32, SuperchunkError> {
    if d == 0 {
        return Err(SuperchunkError::GridTooLarge);
    }
    let adjusted = n
        .checked_add(d - 1)
        .ok_or(SuperchunkError::ArithmeticOverflow)?;
    Ok(adjusted / d)
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
fn chroma_x1_yuv420(luma_x1: u32) -> Result<u32, SuperchunkError> {
    div_ceil_u32(luma_x1, 2)
}

#[inline]
fn chroma_y1_yuv420(luma_y1: u32) -> Result<u32, SuperchunkError> {
    div_ceil_u32(luma_y1, 2)
}

/// Luma [`SuperchunkPlaneBounds`] for superchunk `raster_index`.
pub fn superchunk_luma_bounds(
    grid: &SuperchunkGrid,
    raster_index: u32,
) -> Result<SuperchunkPlaneBounds, SuperchunkError> {
    Ok(grid.bounds(raster_index)?.luma)
}

/// Chroma [`SuperchunkPlaneBounds`] for superchunk `raster_index` (YUV420 semantics).
pub fn superchunk_chroma_bounds_yuv420(
    grid: &SuperchunkGrid,
    raster_index: u32,
) -> Result<SuperchunkPlaneBounds, SuperchunkError> {
    Ok(grid.bounds(raster_index)?.chroma)
}

/// Validate frame dimensions and implied superchunk grid caps.
pub fn validate_superchunk_grid(
    width: u32,
    height: u32,
    size: SrsV2SuperchunkSize,
) -> Result<(), SuperchunkError> {
    SuperchunkGrid::new(width, height, size).map(|_| ())
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

    fn assert_chroma_inside_plane(grid: &SuperchunkGrid, bounds: &SuperchunkBounds) {
        let (cw, ch) = chroma_plane_extent(grid.width, grid.height);
        assert!(bounds.chroma.x0 <= bounds.chroma.x1);
        assert!(bounds.chroma.y0 <= bounds.chroma.y1);
        assert!(bounds.chroma.x1 <= cw);
        assert!(bounds.chroma.y1 <= ch);
        assert!(bounds.chroma.width() <= bounds.luma.width().div_ceil(2));
        assert!(bounds.chroma.height() <= bounds.luma.height().div_ceil(2));
    }

    #[test]
    fn four_k_frame_sc128_grid_valid() {
        let g = SuperchunkGrid::new(3840, 2160, SrsV2SuperchunkSize::Sc128).unwrap();
        validate_superchunk_grid(3840, 2160, SrsV2SuperchunkSize::Sc128).unwrap();
        assert_eq!(g.cols, 30);
        assert_eq!(g.rows, 17);
        assert_eq!(g.count(), 510);
    }

    #[test]
    fn four_k_frame_sc256_grid_valid() {
        let g = SuperchunkGrid::new(3840, 2160, SrsV2SuperchunkSize::Sc256).unwrap();
        validate_superchunk_grid(3840, 2160, SrsV2SuperchunkSize::Sc256).unwrap();
        assert_eq!(g.cols, 15);
        assert_eq!(g.rows, 9);
        assert_eq!(g.count(), 135);
    }

    #[test]
    fn eight_k_frame_sc512_grid_valid() {
        let g = SuperchunkGrid::new(7680, 4320, SrsV2SuperchunkSize::Sc512).unwrap();
        validate_superchunk_grid(7680, 4320, SrsV2SuperchunkSize::Sc512).unwrap();
        assert_eq!(g.cols, 15);
        assert_eq!(g.rows, 9);
        assert_eq!(g.count(), 135);
    }

    #[test]
    fn eight_k_frame_sc1024_grid_valid() {
        let g = SuperchunkGrid::new(7680, 4320, SrsV2SuperchunkSize::Sc1024).unwrap();
        validate_superchunk_grid(7680, 4320, SrsV2SuperchunkSize::Sc1024).unwrap();
        assert_eq!(g.cols, 8);
        assert_eq!(g.rows, 5);
        assert_eq!(g.count(), 40);
    }

    #[test]
    fn edge_superchunks_safe_on_four_k_sc128() {
        let g = SuperchunkGrid::new(3840, 2160, SrsV2SuperchunkSize::Sc128).unwrap();
        let last = g.bounds(g.count() - 1).unwrap();
        assert_eq!(last.luma.x0, 3712);
        assert_eq!(last.luma.x1, 3840);
        assert_eq!(last.luma.y0, 2048);
        assert_eq!(last.luma.y1, 2160);
        assert!(last.luma.width() < g.size.edge_luma() || last.luma.height() < g.size.edge_luma());
        assert_chroma_inside_plane(&g, &last);
    }

    #[test]
    fn zero_dimensions_rejected() {
        assert_eq!(
            validate_superchunk_grid(0, 2160, SrsV2SuperchunkSize::Sc128),
            Err(SuperchunkError::ZeroDimensions)
        );
        assert_eq!(
            validate_superchunk_grid(3840, 0, SrsV2SuperchunkSize::Sc128),
            Err(SuperchunkError::ZeroDimensions)
        );
    }

    #[test]
    fn div_ceil_checked_overflow_rejected() {
        assert_eq!(
            div_ceil_u32(u32::MAX, u32::MAX),
            Err(SuperchunkError::ArithmeticOverflow)
        );
    }

    #[test]
    fn overflow_or_oversized_luma_rejected() {
        assert!(matches!(
            SuperchunkGrid::new(u32::MAX, 2160, SrsV2SuperchunkSize::Sc128),
            Err(SuperchunkError::DimensionTooLarge { .. })
        ));
        assert!(matches!(
            SuperchunkGrid::new(8192, 8192, SrsV2SuperchunkSize::Sc128),
            Err(SuperchunkError::LumaSamplesTooLarge { .. })
        ));
    }

    #[test]
    fn index_out_of_range_rejected() {
        let g = SuperchunkGrid::new(3840, 2160, SrsV2SuperchunkSize::Sc128).unwrap();
        assert_eq!(
            g.bounds(g.count()),
            Err(SuperchunkError::IndexOutOfRange {
                index: g.count(),
                count: g.count(),
            })
        );
    }

    #[test]
    fn yuv420_chroma_bounds_valid() {
        let g = SuperchunkGrid::new(3840, 2160, SrsV2SuperchunkSize::Sc256).unwrap();
        let i = g.count() - 1;
        let full = g.bounds(i).unwrap();
        assert_chroma_inside_plane(&g, &full);

        assert_eq!(superchunk_luma_bounds(&g, i).unwrap(), full.luma);
        assert_eq!(superchunk_chroma_bounds_yuv420(&g, i).unwrap(), full.chroma);

        for addr in g.iter() {
            let b = g.bounds(addr.raster_index).unwrap();
            assert_chroma_inside_plane(&g, &b);
        }
    }

    #[test]
    fn iter_covers_each_superchunk_once() {
        let g = SuperchunkGrid::new(7680, 4320, SrsV2SuperchunkSize::Sc1024).unwrap();
        let n = g.count() as usize;
        let mut seen = vec![false; n];
        for addr in g.iter() {
            assert!(!std::mem::replace(
                &mut seen[addr.raster_index as usize],
                true
            ));
        }
        assert!(seen.iter().all(|&x| x));
    }
}
