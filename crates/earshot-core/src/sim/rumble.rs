//! Low-frequency room rumble for the echo canceller's rumble dev set (T6.23b,
//! `acceptance/aec_rumble_dev.toml`, private notes).
//!
//! A real mic holds non-echo energy below about 150 Hz (a private recording, measured by
//! `xtask::aec_rumble`) that the simulator's scenarios lack. [`render_rumble`] makes
//! stationary noise with a given per-band power spectrum: every FFT bin of one transform
//! over the whole signal gets the band's power and a random phase (the DC bin none), so the
//! power spectrum is exact and the noise Gaussian-like.

use rand_core::Rng;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use serde::Deserialize;

use super::SimOutput;
use crate::aec::HighPassFilter;

/// Power spectrum of the rumble: relative levels of consecutive bands of `band_hz` from 0 Hz.
#[derive(Debug, Clone, PartialEq)]
pub struct RumbleSpectrum {
    /// Band width (Hz).
    pub band_hz: f64,
    /// Relative power per band (dB); bands past the last are silent.
    pub levels_db: Vec<f64>,
}

/// `n` samples at `rate_hz` of stationary noise with the shape of `spectrum` and mean
/// square `mean_square`.
///
/// # Panics
/// If `spectrum` has no band or a band width that is not positive.
pub fn render_rumble<R: Rng + ?Sized>(
    n: usize,
    rate_hz: u32,
    spectrum: &RumbleSpectrum,
    mean_square: f64,
    rng: &mut R,
) -> Vec<f32> {
    assert!(
        !spectrum.levels_db.is_empty() && spectrum.band_hz > 0.0,
        "rumble spectrum is empty"
    );
    if n == 0 {
        return Vec::new();
    }
    let len = n.next_power_of_two().max(2);
    let mut spec = vec![Complex::new(0.0f64, 0.0); len];
    let df = f64::from(rate_hz) / len as f64;
    for k in 1..len / 2 {
        let band = (k as f64 * df / spectrum.band_hz).floor() as usize;
        let Some(&level) = spectrum.levels_db.get(band) else {
            break;
        };
        let phase = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * std::f64::consts::TAU;
        let c = Complex::from_polar(10f64.powf(level / 20.0), phase);
        spec[k] = c;
        spec[len - k] = c.conj();
    }
    FftPlanner::<f64>::new()
        .plan_fft_inverse(len)
        .process(&mut spec);
    let x: Vec<f64> = spec[..n].iter().map(|c| c.re).collect();
    let ms = x.iter().map(|v| v * v).sum::<f64>() / n as f64;
    let g = if ms > 0.0 {
        (mean_square / ms).sqrt()
    } else {
        0.0
    };
    x.into_iter().map(|v| (v * g) as f32).collect()
}

/// The rumble dev set (`acceptance/aec_rumble_dev.toml`): measured spectra and the base
/// scenarios they are added to.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RumbleSet {
    /// Band width of both spectra (Hz).
    pub band_hz: f64,
    /// Non-echo power per band, dB relative to the echo power above 100 Hz (added to
    /// scenarios with echo).
    pub far_levels_db: Vec<f64>,
    /// Rumble power per band, dB relative to the near-end voice power above 100 Hz (added to
    /// scenarios without echo).
    pub near_levels_db: Vec<f64>,
    /// Added to every rendering's seed for the rumble's own random phases.
    pub seed_offset: u64,
    /// Base scenarios (names in `acceptance/aec_scenarios.toml`) of the rumble set.
    pub scenarios: Vec<String>,
}

/// Mean square of `x` high-passed at 100 Hz over `ranges`.
fn power_above_100(x: &[f32], ranges: &[std::ops::Range<usize>]) -> f64 {
    let hp = HighPassFilter::new(100.0, 16_000).filtered(x);
    let (mut sum, mut n) = (0.0f64, 0usize);
    for r in ranges {
        for &v in &hp[r.start.min(hp.len())..r.end.min(hp.len())] {
            sum += f64::from(v).powi(2);
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// Adds the rumble of `set` to `out`'s mic (and its `noise` track), its phases drawn from
/// `rng` (callers seed it with the rendering's seed plus [`RumbleSet::seed_offset`]): the far spectrum relative to the echo power above 100 Hz over the echo's
/// active spans when the scenario has echo, else the near spectrum relative to the near-end
/// voice above 100 Hz over its utterances. Returns the rumble's mean square (0 when the
/// scenario has neither).
pub fn add_rumble<R: Rng + ?Sized>(out: &mut SimOutput, set: &RumbleSet, rng: &mut R) -> f64 {
    let echo = power_above_100(&out.echo, &out.echo_active);
    let (levels, reference) = if echo > 0.0 {
        (&set.far_levels_db, echo)
    } else {
        (
            &set.near_levels_db,
            power_above_100(&out.near_clean, &out.near_regions),
        )
    };
    if reference <= 0.0 {
        return 0.0;
    }
    let total: f64 = levels.iter().map(|l| 10f64.powf(l / 10.0)).sum();
    let ms = total * reference;
    let spectrum = RumbleSpectrum {
        band_hz: set.band_hz,
        levels_db: levels.clone(),
    };
    let r = render_rumble(out.mic.len(), out.rate.hz(), &spectrum, ms, rng);
    for ((m, n), v) in out.mic.iter_mut().zip(out.noise.iter_mut()).zip(&r) {
        *m += v;
        *n += v;
    }
    ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;

    #[test]
    fn sim_rumble_has_the_requested_level_and_shape() {
        let spectrum = RumbleSpectrum {
            band_hz: 10.0,
            levels_db: vec![0.0, -10.0, -20.0],
        };
        let n = 16_000 * 20;
        let x = render_rumble(
            n,
            16_000,
            &spectrum,
            1e-3,
            &mut ChaCha8Rng::seed_from_u64(1),
        );
        let ms = x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / n as f64;
        assert!((ms / 1e-3 - 1.0).abs() < 1e-6);
        // Power per band from one FFT of the whole signal.
        let len = n.next_power_of_two();
        let mut b: Vec<Complex<f64>> = x.iter().map(|&v| Complex::new(f64::from(v), 0.0)).collect();
        b.resize(len, Complex::new(0.0, 0.0));
        FftPlanner::<f64>::new()
            .plan_fft_forward(len)
            .process(&mut b);
        let df = 16_000.0 / len as f64;
        let band = |lo: f64, hi: f64| {
            (1..len / 2)
                .filter(|&k| (lo..hi).contains(&(k as f64 * df)))
                .map(|k| b[k].norm_sqr())
                .sum::<f64>()
        };
        let (b0, b1, b2, above) = (
            band(0.0, 10.0),
            band(10.0, 20.0),
            band(20.0, 30.0),
            band(40.0, 8_000.0),
        );
        let rel = |p: f64| 10.0 * (p / b0).log10();
        assert!((rel(b1) + 10.0).abs() < 1.0, "{}", rel(b1));
        assert!((rel(b2) + 20.0).abs() < 1.0, "{}", rel(b2));
        assert!(rel(above) < -40.0, "{}", rel(above));
        let again = render_rumble(
            n,
            16_000,
            &spectrum,
            1e-3,
            &mut ChaCha8Rng::seed_from_u64(1),
        );
        assert_eq!(x, again);
    }
}

#[cfg(test)]
mod set_tests {
    use super::*;
    use crate::testutil::{repo_root, run_scenario};
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;

    fn dev_set() -> RumbleSet {
        let text = std::fs::read_to_string(repo_root().join("acceptance/aec_rumble_dev.toml"))
            .expect("acceptance/aec_rumble_dev.toml");
        toml::from_str(&text).expect("rumble set")
    }

    #[test]
    fn sim_rumble_dev_set_scales_to_the_echo_or_the_voice() {
        let set = dev_set();
        let file = crate::testutil::scenarios();
        for name in &set.scenarios {
            assert!(
                file.get(name).is_some(),
                "rumble set names unknown scenario {name}"
            );
        }
        let total = |l: &[f64]| 10.0 * l.iter().map(|x| 10f64.powf(x / 10.0)).sum::<f64>().log10();
        for (name, levels) in [
            ("far-only", &set.far_levels_db),
            ("near-only", &set.near_levels_db),
        ] {
            let mut o = run_scenario(name);
            let before = o.mic.clone();
            let ms = add_rumble(&mut o, &set, &mut ChaCha8Rng::seed_from_u64(1));
            let reference = if name == "far-only" {
                power_above_100(&o.echo, &o.echo_active)
            } else {
                power_above_100(&o.near_clean, &o.near_regions)
            };
            let got = 10.0 * (ms / reference).log10();
            assert!((got - total(levels)).abs() < 1e-6, "{name}: {got} dB");
            let added: Vec<f32> = o.mic.iter().zip(&before).map(|(a, b)| a - b).collect();
            let added_ms =
                added.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / added.len() as f64;
            assert!(
                (added_ms / ms - 1.0).abs() < 1e-3,
                "{name}: mic got the rumble"
            );
        }
    }
}
