//! Autocorrelation + Levinson–Durbin for short integer LPC (direct-form coefficients).

const MAX_ORDER: usize = 8;

/// Unnormalized autocorrelation r[lag] = sum_i x[i]*x[i-lag], lags in `0..=order`.
pub fn autocorr_i16(samples: &[i16], order: usize) -> Option<Vec<f64>> {
    if samples.is_empty() || order > MAX_ORDER {
        return None;
    }
    let n = samples.len();
    let mut r = vec![0.0f64; order + 1];
    for lag in 0..=order {
        let mut s = 0.0f64;
        for i in lag..n {
            s += f64::from(samples[i]) * f64::from(samples[i - lag]);
        }
        r[lag] = s;
    }
    Some(r)
}

/// Returns direct-form coefficients `a[1],..,a[p]` for predictor
/// `pred[n] = sum_{k=1..=p} round(a[k] * x[n-k])` (float a, applied via quantized taps).
pub fn levinson_durbin(r: &[f64], order: usize) -> Option<Vec<f64>> {
    if r.is_empty() || order == 0 || r[0] <= 0.0 {
        return None;
    }
    let p = order.min(r.len().saturating_sub(1)).min(MAX_ORDER);
    let mut a_prev = vec![0.0f64; p + 1];
    a_prev[0] = 1.0;
    let mut e = r[0];

    for k in 1..=p {
        let mut lambda_num = r[k];
        for j in 1..k {
            lambda_num -= a_prev[j] * r[k - j];
        }
        let lambda = lambda_num / e;
        if !lambda.is_finite() || lambda.abs() >= 1.0 - 1e-12 {
            return None;
        }
        let mut a_next = vec![0.0f64; p + 1];
        a_next[0] = 1.0;
        a_next[k] = lambda;
        for j in 1..k {
            a_next[j] = a_prev[j] - lambda * a_prev[k - j];
        }
        e *= 1.0 - lambda * lambda;
        if e <= 0.0 {
            return None;
        }
        a_prev = a_next;
    }

    Some((a_prev[1..=p]).to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldc_on_sin_like_sequence() {
        let x: Vec<i16> = (0..64)
            .map(|i| (16384.0 * (i as f64 * 0.1_f64).sin()) as i16)
            .collect();
        let r = autocorr_i16(&x, 4).unwrap();
        let a = levinson_durbin(&r, 4).unwrap();
        assert_eq!(a.len(), 4);
        assert!(a.iter().all(|c| c.is_finite()));
    }
}
