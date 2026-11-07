//! Small signal-processing helpers shared by the simulator and the echo canceller.

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Causal FIR filtering of `x` by `h` via one FFT: returns `y[n] = sum_k h[k] x[n - k]`
/// for `n` in `0..x.len()` (the convolution tail past the end of `x` is dropped).
#[must_use]
pub fn fft_filter(x: &[f32], h: &[f32]) -> Vec<f32> {
    if x.is_empty() || h.is_empty() {
        return vec![0.0; x.len()];
    }
    let n = (x.len() + h.len() - 1).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let mut a: Vec<Complex<f64>> = (0..n)
        .map(|i| Complex::new(x.get(i).map_or(0.0, |&v| f64::from(v)), 0.0))
        .collect();
    let mut b: Vec<Complex<f64>> = (0..n)
        .map(|i| Complex::new(h.get(i).map_or(0.0, |&v| f64::from(v)), 0.0))
        .collect();
    fwd.process(&mut a);
    fwd.process(&mut b);
    for (p, q) in a.iter_mut().zip(&b) {
        *p *= q;
    }
    inv.process(&mut a);
    let scale = 1.0 / n as f64;
    a[..x.len()].iter().map(|c| (c.re * scale) as f32).collect()
}

/// Mean power of `x` (0 for an empty slice).
#[must_use]
pub fn power(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64
}

/// `10 log10(p)` with a floor of -300 dB for `p <= 0`.
#[must_use]
pub fn db(p: f64) -> f64 {
    if p > 0.0 { 10.0 * p.log10() } else { -300.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_filter_matches_direct_convolution() {
        let x: Vec<f32> = (0..100).map(|i| ((i * 37) % 11) as f32 - 5.0).collect();
        let h = [0.5f32, -0.25, 0.0, 0.125];
        let y = fft_filter(&x, &h);
        for n in 0..x.len() {
            let mut d = 0.0f32;
            for (k, &hk) in h.iter().enumerate() {
                if n >= k {
                    d += hk * x[n - k];
                }
            }
            assert!((y[n] - d).abs() < 1e-4, "{n}: {} vs {d}", y[n]);
        }
    }
}
