//! Frame buffers with stride-aware planes and allocation guards.

use super::error::SrsV2Error;
use super::limits::{MAX_DIMENSION, MAX_LUMA_SAMPLES};
use super::model::PixelFormat;

#[derive(Debug, Clone)]
pub struct VideoPlane<T> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub samples: Vec<T>,
}

impl VideoPlane<u8> {
    pub fn try_new(width: u32, height: u32, stride: usize) -> Result<Self, SrsV2Error> {
        validate_plane_geometry(width, height, stride, 1)?;
        let rows = height as usize;
        let need = stride
            .checked_mul(rows)
            .ok_or(SrsV2Error::Overflow("plane byte length"))?;
        if need > MAX_LUMA_SAMPLES as usize {
            return Err(SrsV2Error::AllocationLimit {
                context: "plane exceeds MAX_LUMA_SAMPLES",
            });
        }
        Ok(Self {
            width,
            height,
            stride,
            samples: vec![0_u8; need],
        })
    }

    pub fn row(&self, y: usize) -> &[u8] {
        let start = y * self.stride;
        &self.samples[start..start + self.width as usize]
    }

    pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let start = y * self.stride;
        let w = self.width as usize;
        &mut self.samples[start..start + w]
    }
}

#[derive(Debug, Clone)]
pub struct YuvFrame {
    pub format: PixelFormat,
    pub y: VideoPlane<u8>,
    pub u: VideoPlane<u8>,
    pub v: VideoPlane<u8>,
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrameV2 {
    pub frame_index: u32,
    pub width: u32,
    pub height: u32,
    pub yuv: YuvFrame,
}

impl DecodedVideoFrameV2 {
    /// Single-plane grayscale preview (Y only), contiguous `width*height`.
    pub fn luma_gray_bytes(&self) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let mut out = Vec::with_capacity(w.saturating_mul(h));
        for row in 0..h {
            out.extend_from_slice(self.yuv.y.row(row));
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct EncodedVideoPacketV2 {
    pub frame_index: u32,
    pub payload: Vec<u8>,
}

pub(crate) fn validate_plane_geometry(
    width: u32,
    height: u32,
    stride: usize,
    bpp: usize,
) -> Result<(), SrsV2Error> {
    if width == 0 || height == 0 {
        return Err(SrsV2Error::Dimensions { width, height });
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(SrsV2Error::LimitExceeded("dimension cap"));
    }
    let samples = (width as u64).saturating_mul(height as u64);
    if samples > MAX_LUMA_SAMPLES {
        return Err(SrsV2Error::LimitExceeded("luma sample cap"));
    }
    let min_stride = (width as usize)
        .checked_mul(bpp)
        .ok_or(SrsV2Error::Overflow("stride"))?;
    if stride < min_stride {
        return Err(SrsV2Error::syntax("stride smaller than width*bpp"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_huge_dim_product() {
        assert!(VideoPlane::<u8>::try_new(
            MAX_DIMENSION,
            MAX_DIMENSION + 1,
            MAX_DIMENSION as usize
        )
        .is_err());
    }
}
