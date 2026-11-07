//! Scale-invariant signal-to-distortion ratio (SI-SDR).
//!
//! J. Le Roux, S. Wisdom, H. Erdogan, J. R. Hershey, "SDR - half-baked or well done?",
//! ICASSP 2019, eq. (1)-(3), with both signals made zero-mean first (as in the paper's
//! reference code):
//!
//! `alpha = <s_hat, s> / ||s||^2`, `e_target = alpha s`, `e_res = s_hat - e_target`,
//! `SI-SDR = 10 log10(||e_target||^2 / ||e_res||^2)` dB.

/// Mean-removed energy per sample at or below which an estimate counts as silent.
const SILENT_ENERGY: f64 = 1e-20;

/// SI-SDR in dB of `estimate` against `reference`.
///
/// Returns `None` for empty or different-length inputs or a reference with no energy
/// after mean removal; `Some(f64::INFINITY)` for a residual of exactly zero on a
/// non-silent estimate.
///
/// A silent estimate (mean-removed energy per sample at or below [`SILENT_ENERGY`], i.e.
/// all-zero, DC-only or numerically negligible) returns `Some(f64::NEG_INFINITY)`, so a
/// canceller that mutes its output can never score well.
pub fn si_sdr(estimate: &[f32], reference: &[f32]) -> Option<f64> {
    if estimate.is_empty() || estimate.len() != reference.len() {
        return None;
    }
    let n = estimate.len() as f64;
    let mean = |x: &[f32]| x.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let (me, mr) = (mean(estimate), mean(reference));
    let e = estimate.iter().map(|&v| f64::from(v) - me);
    let r: Vec<f64> = reference.iter().map(|&v| f64::from(v) - mr).collect();
    let ref_energy: f64 = r.iter().map(|v| v * v).sum();
    if ref_energy <= 0.0 {
        return None;
    }
    let est_energy: f64 = e.clone().map(|v| v * v).sum();
    if est_energy / n <= SILENT_ENERGY {
        return Some(f64::NEG_INFINITY);
    }
    let dot: f64 = e.clone().zip(&r).map(|(a, b)| a * b).sum();
    let alpha = dot / ref_energy;
    let (mut target, mut residual) = (0.0f64, 0.0f64);
    for (ev, rv) in e.zip(&r) {
        let t = alpha * rv;
        target += t * t;
        let d = ev - t;
        residual += d * d;
    }
    if residual == 0.0 {
        return Some(f64::INFINITY);
    }
    Some(10.0 * (target / residual).log10())
}
