//! PCM16 <-> `f32` conversion.
//!
//! Scale factor is 32768 in both directions, so every `i16` maps to a distinct `f32` in
//! `[-1.0, 1.0)` and converts back exactly.

/// Converts one PCM16 sample to `f32` in `[-1.0, 1.0)`.
#[must_use]
pub fn i16_to_f32(s: i16) -> f32 {
    f32::from(s) / 32768.0
}

/// Converts one `f32` sample to PCM16, rounding to nearest and clamping to the `i16`
/// range. NaN maps to 0.
#[must_use]
pub fn f32_to_i16(x: f32) -> i16 {
    if x.is_nan() {
        return 0;
    }
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// Converts `src` into `dst` element-wise; converts `min(src.len(), dst.len())` samples
/// and returns that count.
pub fn i16_slice_to_f32(src: &[i16], dst: &mut [f32]) -> usize {
    let n = src.len().min(dst.len());
    for (d, &s) in dst.iter_mut().zip(&src[..n]) {
        *d = i16_to_f32(s);
    }
    n
}

/// Converts `src` into `dst` element-wise with [`f32_to_i16`]; converts
/// `min(src.len(), dst.len())` samples and returns that count.
pub fn f32_slice_to_i16(src: &[f32], dst: &mut [i16]) -> usize {
    let n = src.len().min(dst.len());
    for (d, &s) in dst.iter_mut().zip(&src[..n]) {
        *d = f32_to_i16(s);
    }
    n
}

/// `samples` as a 16-bit recording gives them back: each through [`f32_to_i16`] and
/// [`i16_to_f32`], exactly as the FLAC track writer stores them and the final pass reads
/// them. Live work that the final pass reuses (speech stretches, speaker windows) is
/// computed from these samples, so it equals the same work done on the recording.
#[must_use]
pub fn quantise_pcm16(samples: &[f32]) -> Vec<f32> {
    samples.iter().map(|&x| i16_to_f32(f32_to_i16(x))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_quantise_pcm16_is_the_16_bit_round_trip_and_idempotent() {
        let x = [
            0.0,
            0.123_456_78,
            -0.999_99,
            1.5,
            -2.0,
            f32::NAN,
            1.0 / 32768.0,
        ];
        let q = quantise_pcm16(&x);
        let want: Vec<f32> = x.iter().map(|&v| i16_to_f32(f32_to_i16(v))).collect();
        assert_eq!(q, want);
        assert_eq!(q[3], 32767.0 / 32768.0, "clamped");
        assert_eq!(q[5], 0.0, "NaN is silence");
        assert_eq!(quantise_pcm16(&q), q, "a stored sample stays as it is");
        assert_ne!(q[1], x[1]);
    }
}
