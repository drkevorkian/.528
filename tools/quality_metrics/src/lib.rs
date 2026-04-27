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
    use super::{psnr_u8, snr_i16};

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
}
