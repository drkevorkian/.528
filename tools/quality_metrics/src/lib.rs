pub mod display_order;
pub use display_order::{DisplayOrderError, DisplayReorderBuffer};
pub mod hevc_compare;
pub mod srsv2_progress_report;
pub mod srsv2_sweep;
pub mod synthetic;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricError {
    LengthMismatch { reference: usize, measured: usize },
    EmptyInput,
    ZeroNoisePower,
}

impl std::fmt::Display for MetricError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LengthMismatch {
                reference,
                measured,
            } => write!(
                f,
                "input length mismatch: reference={reference}, measured={measured}"
            ),
            Self::EmptyInput => write!(f, "metric inputs cannot be empty"),
            Self::ZeroNoisePower => write!(f, "noise power is zero, ratio is infinite"),
        }
    }
}

impl std::error::Error for MetricError {}

/// Ratio of uncompressed size to compressed size (≥ 1.0 when compressed is smaller).
pub fn compression_ratio(uncompressed_bytes: u64, compressed_bytes: u64) -> f64 {
    uncompressed_bytes as f64 / compressed_bytes.max(1) as f64
}

/// Windowed SSIM on luminance (`u8`), 8×8 windows, simplified constants (not MS-SSIM).
pub fn ssim_u8_simple(
    reference: &[u8],
    measured: &[u8],
    width: usize,
    height: usize,
) -> Result<f64, MetricError> {
    validate_len(reference.len(), measured.len())?;
    let expected = width
        .checked_mul(height)
        .filter(|&n| n > 0)
        .ok_or(MetricError::EmptyInput)?;
    if reference.len() != expected {
        return Err(MetricError::LengthMismatch {
            reference: reference.len(),
            measured: expected,
        });
    }
    const WIN: usize = 8;
    let mut accum = 0.0_f64;
    let mut count = 0_u64;
    let c1 = (0.01_f64 * 255.0).powi(2);
    let c2 = (0.03_f64 * 255.0).powi(2);
    let mut wy = 0usize;
    while wy + WIN <= height {
        let mut wx = 0usize;
        while wx + WIN <= width {
            let mut sum_r = 0.0_f64;
            let mut sum_m = 0.0_f64;
            let mut sum_rr = 0.0_f64;
            let mut sum_mm = 0.0_f64;
            let mut sum_rm = 0.0_f64;
            let n = (WIN * WIN) as f64;
            for y in 0..WIN {
                let row = (wy + y) * width + wx;
                for x in 0..WIN {
                    let i = row + x;
                    let r = reference[i] as f64;
                    let m = measured[i] as f64;
                    sum_r += r;
                    sum_m += m;
                    sum_rr += r * r;
                    sum_mm += m * m;
                    sum_rm += r * m;
                }
            }
            let mean_r = sum_r / n;
            let mean_m = sum_m / n;
            let var_r = (sum_rr / n - mean_r * mean_r).max(0.0);
            let var_m = (sum_mm / n - mean_m * mean_m).max(0.0);
            let cov_rm = sum_rm / n - mean_r * mean_m;
            let num = (2.0 * mean_r * mean_m + c1) * (2.0 * cov_rm + c2);
            let den = (mean_r * mean_r + mean_m * mean_m + c1) * (var_r + var_m + c2);
            if den > 0.0 {
                accum += num / den;
            }
            count += 1;
            wx += WIN;
        }
        wy += WIN;
    }
    if count == 0 {
        return Err(MetricError::EmptyInput);
    }
    Ok(accum / count as f64)
}

/// Hook point for VMAF: this crate does not vendor FFmpeg; call `ffmpeg` + libvmaf externally.
pub const VMAF_EXTERNAL_NOTE: &str =
    "VMAF is not computed in-tree; invoke ffmpeg with libvmaf when available.";

#[derive(Debug, Clone)]
pub struct EncodeDecodeThroughput {
    pub frames: u64,
    pub compressed_bytes: u64,
    pub uncompressed_bytes: u64,
    pub encode_seconds: f64,
    pub decode_seconds: f64,
}

impl EncodeDecodeThroughput {
    pub fn compression_ratio(&self) -> f64 {
        compression_ratio(self.uncompressed_bytes, self.compressed_bytes)
    }

    pub fn encode_fps(&self) -> f64 {
        self.frames as f64 / self.encode_seconds.max(f64::EPSILON)
    }

    pub fn decode_fps(&self) -> f64 {
        self.frames as f64 / self.decode_seconds.max(f64::EPSILON)
    }
}

pub fn psnr_u8(reference: &[u8], measured: &[u8], peak: f64) -> Result<f64, MetricError> {
    validate_len(reference.len(), measured.len())?;
    if reference.is_empty() {
        return Err(MetricError::EmptyInput);
    }
    if peak <= 0.0 {
        return Err(MetricError::ZeroNoisePower);
    }
    let mse = mean_squared_error_u8(reference, measured);
    if mse == 0.0 {
        return Ok(f64::INFINITY);
    }
    Ok(10.0 * ((peak * peak) / mse).log10())
}

pub fn snr_i16(reference: &[i16], measured: &[i16]) -> Result<f64, MetricError> {
    validate_len(reference.len(), measured.len())?;
    if reference.is_empty() {
        return Err(MetricError::EmptyInput);
    }
    let signal_power: f64 = reference
        .iter()
        .map(|s| {
            let x = *s as f64;
            x * x
        })
        .sum::<f64>()
        / reference.len() as f64;
    let noise_power: f64 = reference
        .iter()
        .zip(measured.iter())
        .map(|(r, m)| {
            let d = (*r as f64) - (*m as f64);
            d * d
        })
        .sum::<f64>()
        / reference.len() as f64;
    if noise_power == 0.0 {
        return Ok(f64::INFINITY);
    }
    if signal_power == 0.0 {
        return Err(MetricError::ZeroNoisePower);
    }
    Ok(10.0 * (signal_power / noise_power).log10())
}

fn validate_len(reference: usize, measured: usize) -> Result<(), MetricError> {
    if reference != measured {
        return Err(MetricError::LengthMismatch {
            reference,
            measured,
        });
    }
    Ok(())
}

fn mean_squared_error_u8(reference: &[u8], measured: &[u8]) -> f64 {
    reference
        .iter()
        .zip(measured.iter())
        .map(|(r, m)| {
            let d = (*r as f64) - (*m as f64);
            d * d
        })
        .sum::<f64>()
        / reference.len() as f64
}

#[cfg(test)]
mod tests {
    use super::{compression_ratio, psnr_u8, snr_i16, ssim_u8_simple};

    #[test]
    fn psnr_is_infinite_on_identical_input() {
        let input = [1_u8, 2, 3, 4];
        let value = psnr_u8(&input, &input, 255.0).expect("psnr should compute");
        assert!(value.is_infinite());
    }

    #[test]
    fn psnr_is_finite_when_inputs_differ() {
        let reference = [10_u8, 20, 30, 40];
        let measured = [11_u8, 18, 31, 38];
        let value = psnr_u8(&reference, &measured, 255.0).expect("psnr should compute");
        assert!(value.is_finite());
        assert!(value > 30.0);
    }

    #[test]
    fn snr_is_infinite_on_identical_input() {
        let input = [10_i16, -8, 4, 0];
        let value = snr_i16(&input, &input).expect("snr should compute");
        assert!(value.is_infinite());
    }

    #[test]
    fn snr_is_finite_when_inputs_differ() {
        let reference = [100_i16, 200, -100, -200];
        let measured = [98_i16, 205, -102, -197];
        let value = snr_i16(&reference, &measured).expect("snr should compute");
        assert!(value.is_finite());
    }

    #[test]
    fn ssim_is_one_on_identical_8x8() {
        let buf = [99_u8; 8 * 8];
        let v = ssim_u8_simple(&buf, &buf, 8, 8).expect("ssim");
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn compression_ratio_basic() {
        let r = compression_ratio(1000, 100);
        assert!((r - 10.0).abs() < 1e-9);
    }
}
