//! Shoebox room impulse responses by the image-source method.
//!
//! J. B. Allen, D. A. Berkley, "Image method for efficiently simulating small-room
//! acoustics", JASA 65(4), 1979. Every wall has the same frequency-independent pressure
//! reflection coefficient `beta = sqrt(1 - alpha)`. Each image contributes
//! `beta^(reflections) / (4 pi r)` at delay `r / c`, placed with a Hann-windowed sinc
//! fractional delay (T. I. Laakso et al., "Splitting the unit delay", IEEE SPM 1996) so
//! sub-sample arrival times are kept.

use serde::Deserialize;

use crate::audio::SampleRate;

/// Speed of sound in m/s (air at about 20 C).
pub const SPEED_OF_SOUND: f64 = 343.0;

/// Half-length in taps of the fractional-delay sinc.
const FRAC_HALF: isize = 16;

/// A shoebox room with uniform absorption.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Room {
    /// Length, width, height in metres.
    pub dims: [f64; 3],
    /// Energy absorption coefficient of every wall, in `(0, 1]`.
    pub absorption: f64,
}

impl Room {
    /// Sabine reverberation time `0.161 V / (S alpha)` in seconds.
    #[must_use]
    pub fn sabine_rt60(&self) -> f64 {
        let [x, y, z] = self.dims;
        let volume = x * y * z;
        let surface = 2.0 * (x * y + x * z + y * z);
        0.161 * volume / (surface * self.absorption)
    }

    fn contains(&self, p: [f64; 3]) -> bool {
        p.iter().zip(self.dims).all(|(&v, d)| v > 0.0 && v < d)
    }
}

/// Distance in metres between two points.
#[must_use]
pub fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f64>()
        .sqrt()
}

/// Direct-path propagation delay from `src` to `mic` in samples at `rate`.
#[must_use]
pub fn direct_delay_samples(src: [f64; 3], mic: [f64; 3], rate: SampleRate) -> f64 {
    distance(src, mic) / SPEED_OF_SOUND * f64::from(rate.hz())
}

/// Impulse response of `room` from `src` to `mic`, `len` taps at `rate`.
///
/// # Panics
/// If `src` or `mic` lies outside the room, or the absorption is not in `(0, 1]`.
#[must_use]
pub fn image_source_rir(
    room: &Room,
    src: [f64; 3],
    mic: [f64; 3],
    rate: SampleRate,
    len: usize,
) -> Vec<f32> {
    assert!(room.contains(src), "source outside the room");
    assert!(room.contains(mic), "microphone outside the room");
    assert!(
        room.absorption > 0.0 && room.absorption <= 1.0,
        "absorption must be in (0, 1]"
    );
    let fs = f64::from(rate.hz());
    let beta = (1.0 - room.absorption).sqrt();
    let max_dist = (len as f64 + FRAC_HALF as f64) / fs * SPEED_OF_SOUND;
    let mut h = vec![0.0f64; len];

    // Image positions along one axis: x = 2 n L + (1 - 2q) s, reflections |n - q| + |n|.
    let orders: Vec<i64> = room
        .dims
        .iter()
        .map(|&d| (max_dist / (2.0 * d)).ceil() as i64 + 1)
        .collect();
    let axis_images = |axis: usize| {
        let d = room.dims[axis];
        let n_max = orders[axis];
        let mut v = Vec::new();
        for n in -n_max..=n_max {
            for q in 0..=1i64 {
                let pos = 2.0 * n as f64 * d + (1 - 2 * q) as f64 * src[axis];
                let refl = (n - q).unsigned_abs() + n.unsigned_abs();
                let delta = pos - mic[axis];
                if delta.abs() <= max_dist {
                    v.push((delta, refl as i32));
                }
            }
        }
        v
    };
    let (ix, iy, iz) = (axis_images(0), axis_images(1), axis_images(2));
    for &(dx, rx) in &ix {
        for &(dy, ry) in &iy {
            let dxy2 = dx * dx + dy * dy;
            if dxy2 > max_dist * max_dist {
                continue;
            }
            for &(dz, rz) in &iz {
                let r = (dxy2 + dz * dz).sqrt();
                if r > max_dist || r == 0.0 {
                    continue;
                }
                let gain = beta.powi(rx + ry + rz) / (4.0 * std::f64::consts::PI * r);
                add_fractional_impulse(&mut h, r / SPEED_OF_SOUND * fs, gain);
            }
        }
    }
    h.into_iter().map(|v| v as f32).collect()
}

/// Adds `gain * delta(t - pos)` band-limited with a Hann-windowed sinc.
fn add_fractional_impulse(h: &mut [f64], pos: f64, gain: f64) {
    let centre = pos.round() as isize;
    for k in -FRAC_HALF..=FRAC_HALF {
        let idx = centre + k;
        if idx < 0 || idx as usize >= h.len() {
            continue;
        }
        let t = idx as f64 - pos;
        let sinc = if t.abs() < 1e-12 {
            1.0
        } else {
            (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t)
        };
        let w = 0.5 + 0.5 * (std::f64::consts::PI * t / (FRAC_HALF as f64 + 1.0)).cos();
        h[idx as usize] += gain * sinc * w;
    }
}
