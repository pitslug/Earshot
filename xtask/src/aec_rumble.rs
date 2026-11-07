//! Low-frequency non-echo content of a real recording (T6.23b): the measurement behind the
//! rumble dev set (`acceptance/aec_rumble_dev.toml`, private notes).
//!
//! Far-end-only stretches: the reference is aligned to the mic by the cross-correlation
//! peak, then Welch spectra (Hann, `NPERSEG` samples, half overlap) give the mic power
//! `Pmm`, the reference power and the cross spectrum; the magnitude-squared coherence
//! `g2 = |Smr|^2 / (Pmm Prr)` splits the mic into what a linear fit from the reference
//! explains (`Pmm g2`, the echo) and the rest (`Pmm (1 - g2)`, non-echo). Reported per
//! [`BAND_HZ`] band below [`TOP_HZ`] as dB relative to the echo power above 100 Hz.
//!
//! Near-end-only stretches (no reference): the frames whose power above [`TOP_HZ`] is in
//! the quietest fifth (speech pauses) give the rumble spectrum, relative to the mean power
//! above 100 Hz of the loudest half of the frames (the near-end voice).
//!
//! Counts and spectra only: no audio or transcript leaves the recording.

use std::ops::Range;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Sample rate of the inputs (Hz).
pub const RATE_HZ: f64 = 16_000.0;
/// Welch segment length (samples): 0.512 s, 1.95 Hz bins.
pub const NPERSEG: usize = 8_192;
/// Band width of the report (Hz).
pub const BAND_HZ: f64 = 10.0;
/// Upper edge of the reported low band (Hz).
pub const TOP_HZ: f64 = 150.0;
/// Largest echo delay searched (s).
const MAX_DELAY_S: f64 = 0.6;

/// Far-end-only measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct FarOnly {
    /// Reference-to-mic delay used (ms).
    pub delay_ms: f64,
    /// Seconds analysed.
    pub seconds: f64,
    /// Non-echo power per band (`[k * BAND_HZ, (k + 1) * BAND_HZ)`) below [`TOP_HZ`], dB
    /// relative to the echo power above 100 Hz.
    pub non_echo_db: Vec<f64>,
    /// Total non-echo power below [`TOP_HZ`], same reference.
    pub non_echo_total_db: f64,
    /// Echo power below 100 Hz, same reference.
    pub echo_below_100_db: f64,
    /// Non-echo power above [`TOP_HZ`], same reference.
    pub non_echo_above_db: f64,
}

/// Near-end-only measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct NearOnly {
    /// Seconds analysed.
    pub seconds: f64,
    /// Rumble power per band below [`TOP_HZ`] in the quietest fifth of the frames, dB
    /// relative to the voice power above 100 Hz (loudest half of the frames).
    pub rumble_db: Vec<f64>,
    /// Total of `rumble_db`.
    pub rumble_total_db: f64,
}

fn db(p: f64) -> f64 {
    10.0 * p.max(1e-30).log10()
}

fn hann() -> Vec<f64> {
    (0..NPERSEG)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / NPERSEG as f64).cos())
        .collect()
}

fn bin_hz(k: usize) -> f64 {
    k as f64 * RATE_HZ / NPERSEG as f64
}

fn bands() -> usize {
    (TOP_HZ / BAND_HZ).round() as usize
}

/// Sums `p` per band below [`TOP_HZ`].
fn per_band(p: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0; bands()];
    for (k, &v) in p.iter().enumerate() {
        let b = (bin_hz(k) / BAND_HZ).floor() as usize;
        if b < out.len() {
            out[b] += v;
        }
    }
    out
}

/// Lag (samples, `reference` earlier) of the cross-correlation peak within `MAX_DELAY_S`.
pub fn delay(mic: &[f32], reference: &[f32]) -> usize {
    let n = mic.len().min(reference.len());
    let len = (2 * n).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let (fwd, inv) = (planner.plan_fft_forward(len), planner.plan_fft_inverse(len));
    let spec = |x: &[f32]| {
        let mut b: Vec<Complex<f64>> = x[..n]
            .iter()
            .map(|&v| Complex::new(f64::from(v), 0.0))
            .collect();
        b.resize(len, Complex::new(0.0, 0.0));
        fwd.process(&mut b);
        b
    };
    let (m, r) = (spec(mic), spec(reference));
    // Phase transform: the peak of the whitened cross-correlation.
    let mut c: Vec<Complex<f64>> = m
        .iter()
        .zip(&r)
        .map(|(a, b)| {
            let x = a * b.conj();
            x / x.norm().max(1e-20)
        })
        .collect();
    inv.process(&mut c);
    let max_lag = (MAX_DELAY_S * RATE_HZ) as usize;
    (0..=max_lag.min(n - 1))
        .max_by(|&a, &b| c[a].re.total_cmp(&c[b].re))
        .unwrap_or(0)
}

/// Far-end-only measurement over `ranges` of the mic (samples), with the reference shifted by
/// the cross-correlation delay of the first range.
///
/// # Panics
/// If no range holds a whole Welch segment.
pub fn far_only(mic: &[f32], reference: &[f32], ranges: &[Range<usize>]) -> FarOnly {
    let first = &ranges[0];
    let d = delay(&mic[first.clone()], &reference[first.clone()]);
    let w = hann();
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(NPERSEG);
    let half = NPERSEG / 2 + 1;
    let (mut pmm, mut prr) = (vec![0.0f64; half], vec![0.0f64; half]);
    let mut smr = vec![Complex::new(0.0f64, 0.0); half];
    let mut segs = 0usize;
    let mut seconds = 0.0;
    for r in ranges {
        let start = r.start.max(d);
        let end = r.end.min(mic.len()).min(reference.len() + d);
        if end <= start {
            continue;
        }
        seconds += (end - start) as f64 / RATE_HZ;
        let mut s = start;
        while s + NPERSEG <= end {
            let seg = |x: &[f32], off: usize| {
                let mut b: Vec<Complex<f64>> = (0..NPERSEG)
                    .map(|i| Complex::new(f64::from(x[off + i]) * w[i], 0.0))
                    .collect();
                fft.process(&mut b);
                b
            };
            let (m, rr) = (seg(mic, s), seg(reference, s - d));
            for k in 0..half {
                pmm[k] += m[k].norm_sqr();
                prr[k] += rr[k].norm_sqr();
                smr[k] += m[k] * rr[k].conj();
            }
            segs += 1;
            s += NPERSEG / 2;
        }
    }
    assert!(segs > 0, "no whole Welch segment in the far-only ranges");
    let mut echo = vec![0.0; half];
    let mut non_echo = vec![0.0; half];
    for k in 0..half {
        let g2 = if pmm[k] > 0.0 && prr[k] > 0.0 {
            (smr[k].norm_sqr() / (pmm[k] * prr[k])).min(1.0)
        } else {
            0.0
        };
        echo[k] = pmm[k] * g2;
        non_echo[k] = pmm[k] * (1.0 - g2);
    }
    let sum_where = |p: &[f64], f: &dyn Fn(f64) -> bool| {
        p.iter()
            .enumerate()
            .filter(|(k, _)| f(bin_hz(*k)))
            .map(|(_, v)| v)
            .sum::<f64>()
    };
    let echo_hi = sum_where(&echo, &|f| f >= 100.0);
    let rel = |p: f64| db(p / echo_hi);
    let bands = per_band(&non_echo);
    FarOnly {
        delay_ms: d as f64 * 1000.0 / RATE_HZ,
        seconds,
        non_echo_total_db: rel(bands.iter().sum()),
        non_echo_db: bands.into_iter().map(rel).collect(),
        echo_below_100_db: rel(sum_where(&echo, &|f| f < 100.0)),
        non_echo_above_db: rel(sum_where(&non_echo, &|f| f >= TOP_HZ)),
    }
}

/// Near-end-only measurement over `range` of the mic.
///
/// # Panics
/// If the range holds fewer than ten frames.
pub fn near_only(mic: &[f32], range: Range<usize>) -> NearOnly {
    let w = hann();
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(NPERSEG);
    let half = NPERSEG / 2 + 1;
    let mut frames: Vec<Vec<f64>> = Vec::new();
    let mut s = range.start;
    while s + NPERSEG <= range.end.min(mic.len()) {
        let mut b: Vec<Complex<f64>> = (0..NPERSEG)
            .map(|i| Complex::new(f64::from(mic[s + i]) * w[i], 0.0))
            .collect();
        fft.process(&mut b);
        frames.push(b[..half].iter().map(Complex::norm_sqr).collect());
        s += NPERSEG / 2;
    }
    assert!(frames.len() >= 10, "near-only range too short");
    let above = |p: &[f64], lo: f64| {
        p.iter()
            .enumerate()
            .filter(|(k, _)| bin_hz(*k) >= lo)
            .map(|(_, v)| v)
            .sum::<f64>()
    };
    let mut order: Vec<usize> = (0..frames.len()).collect();
    order.sort_by(|&a, &b| above(&frames[a], TOP_HZ).total_cmp(&above(&frames[b], TOP_HZ)));
    let quiet = &order[..frames.len() / 5];
    let loud = &order[frames.len() / 2..];
    let mean = |idx: &[usize]| {
        let mut m = vec![0.0; half];
        for &i in idx {
            for (a, v) in m.iter_mut().zip(&frames[i]) {
                *a += v / idx.len() as f64;
            }
        }
        m
    };
    let voice = above(&mean(loud), 100.0);
    let bands = per_band(&mean(quiet));
    NearOnly {
        seconds: (range.end - range.start) as f64 / RATE_HZ,
        rumble_total_db: db(bands.iter().sum::<f64>() / voice),
        rumble_db: bands.into_iter().map(|p| db(p / voice)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32
            })
            .collect()
    }

    #[test]
    fn aec_rumble_far_only_separates_echo_from_low_frequency_noise() {
        // Echo: the reference delayed by 200 ms and halved. Non-echo: a 45 Hz tone whose
        // power is known relative to the echo above 100 Hz.
        let n = 16_000 * 30;
        let r = noise(n, 7);
        let d = 3_200;
        let tone = 0.05f32;
        let mic: Vec<f32> = (0..n)
            .map(|i| {
                let e = if i >= d { 0.5 * r[i - d] } else { 0.0 };
                e + tone * (std::f32::consts::TAU * 45.0 * i as f32 / 16_000.0).sin()
            })
            .collect();
        let all = 0..n;
        let m = far_only(&mic, &r, std::slice::from_ref(&all));
        assert!((m.delay_ms - 200.0).abs() < 0.1, "delay {}", m.delay_ms);
        // White echo of variance 0.25 / 12, of which (8000 - 100) / 8000 lies above 100 Hz; the tone
        // sits in band 4 (40-50 Hz) at (tone^2 / 2) / (0.25 / 12 * (8000 - 100) / 8000).
        let expect = db(f64::from(tone * tone) / 2.0 / (0.25 / 12.0 * 7_900.0 / 8_000.0));
        assert!(
            (m.non_echo_db[4] - expect).abs() < 0.5,
            "{} vs {expect}",
            m.non_echo_db[4]
        );
        assert!(m.non_echo_db[10] < expect - 20.0, "no rumble at 100 Hz");
        assert!(m.echo_below_100_db < -15.0);
    }

    #[test]
    #[ignore = "needs a private recording: take3_dir in .earshot-private/gate12.toml or EARSHOT_TAKE3_DIR"]
    fn aec_rumble_take3_low_frequency_non_echo() {
        let dir = crate::t613::gate12_private::recording_dir(
            "EARSHOT_TAKE3_DIR",
            &crate::repo_root(),
            |s| &s.take3_dir,
        );
        let rec = crate::echo_nl::load_recording(&dir)
            .unwrap_or_else(|e| panic!("take 3 at {}: {e:#}", dir.display()));
        let path = crate::t613::gate12_private::default_path();
        let settings = crate::t613::gate12_private::load(path.as_deref())
            .unwrap_or_else(|e| panic!("{e}"))
            .expect("the private settings file names this recording's stretches");
        let s = |ms: u64| (ms as f64 / 1000.0 * RATE_HZ) as usize;
        let far_stretches: Vec<_> = settings
            .take3_far_ms
            .iter()
            .map(|[from, to]| s(*from)..s(*to))
            .collect();
        let far = far_only(&rec.mic, &rec.loopback, &far_stretches);
        let [near_from, near_to] = settings.take3_near_ms;
        let near = near_only(&rec.mic, s(near_from)..s(near_to));
        // The mic's DC offset in the far-only stretches (part of the lowest band).
        let dc: Vec<f64> = far_stretches
            .iter()
            .map(|r| {
                rec.mic[r.clone()]
                    .iter()
                    .map(|&v| f64::from(v))
                    .sum::<f64>()
                    / r.len() as f64
            })
            .collect();
        println!("take3 mic DC offset in the far-only stretches: {dc:?}");
        let r1 = |v: &[f64]| {
            v.iter()
                .map(|x| (x * 10.0).round() / 10.0)
                .collect::<Vec<_>>()
        };
        println!(
            "take3 far-only: delay {:.1} ms, {:.0} s; non-echo below {TOP_HZ} Hz per {BAND_HZ} Hz band (dB re echo above 100 Hz) {:?}; total {:.1}; echo below 100 Hz {:.1}; non-echo above {TOP_HZ} Hz {:.1}",
            far.delay_ms,
            far.seconds,
            r1(&far.non_echo_db),
            far.non_echo_total_db,
            far.echo_below_100_db,
            far.non_echo_above_db
        );
        println!(
            "take3 near-only stretch: {:.0} s; quiet-frame rumble per band (dB re voice above 100 Hz) {:?}; total {:.1}",
            near.seconds,
            r1(&near.rumble_db),
            near.rumble_total_db
        );
    }
}
