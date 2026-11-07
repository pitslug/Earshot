//! Unit tests for the audio primitives. Every test path contains `audio`, so
//! `cargo test -p earshot-core audio` selects them all.

use std::f64::consts::PI;
use std::io::Cursor;
use std::time::Duration;

use super::*;

fn sine(rate: SampleRate, freq: f64, amp: f64, frames: usize) -> Vec<f32> {
    let w = 2.0 * PI * freq / f64::from(rate.hz());
    (0..frames)
        .map(|n| (amp * (w * n as f64).sin()) as f32)
        .collect()
}

/// Least-squares fit of `a cos(wn) + b sin(wn) + c` at a known frequency. Returns
/// (amplitude, residual RMS).
fn fit_sine(x: &[f32], rate: SampleRate, freq: f64) -> (f64, f64) {
    let w = 2.0 * PI * freq / f64::from(rate.hz());
    // Normal equations for [cos, sin, 1].
    let mut ata = [[0.0f64; 3]; 3];
    let mut atb = [0.0f64; 3];
    for (n, &v) in x.iter().enumerate() {
        let row = [(w * n as f64).cos(), (w * n as f64).sin(), 1.0];
        for i in 0..3 {
            for j in 0..3 {
                ata[i][j] += row[i] * row[j];
            }
            atb[i] += row[i] * f64::from(v);
        }
    }
    let p = solve3(ata, atb);
    let mut sq = 0.0;
    for (n, &v) in x.iter().enumerate() {
        let model = p[0] * (w * n as f64).cos() + p[1] * (w * n as f64).sin() + p[2];
        sq += (f64::from(v) - model).powi(2);
    }
    (p[0].hypot(p[1]), (sq / x.len() as f64).sqrt())
}

fn solve3(mut a: [[f64; 3]; 3], mut b: [f64; 3]) -> [f64; 3] {
    for col in 0..3 {
        let piv = (col..3)
            .max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))
            .unwrap();
        a.swap(col, piv);
        b.swap(col, piv);
        for row in col + 1..3 {
            let f = a[row][col] / a[col][col];
            let pivot_row = a[col];
            for (v, p) in a[row].iter_mut().zip(pivot_row).skip(col) {
                *v -= f * p;
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = [0.0; 3];
    for row in (0..3).rev() {
        let s: f64 = (row + 1..3).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    x
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

// ---------------------------------------------------------------- types

#[test]
fn audio_sample_rate_rejects_zero_and_converts_durations() {
    assert_eq!(SampleRate::new(0), None);
    assert_eq!(SampleRate::new(48_000), Some(SampleRate::HZ_48000));
    assert_eq!(PIPELINE_RATE.hz(), 16_000);
    assert_eq!(
        SampleRate::HZ_48000.frames_for(Duration::from_millis(10)),
        480
    );
    assert_eq!(
        SampleRate::HZ_16000.duration_of(16_000),
        Duration::from_secs(1)
    );
    let b = MonoBuffer::new(SampleRate::HZ_16000, vec![0.0; 8_000]);
    assert_eq!(b.len(), 8_000);
    assert!(!b.is_empty());
    assert_eq!(b.duration(), Duration::from_millis(500));
}

// ---------------------------------------------------------------- conversion

#[test]
fn audio_i16_f32_round_trip_is_exact_for_every_value() {
    for v in i16::MIN..=i16::MAX {
        let f = i16_to_f32(v);
        assert!((-1.0..1.0).contains(&f), "{v} -> {f}");
        assert_eq!(f32_to_i16(f), v);
    }
    assert_eq!(i16_to_f32(i16::MIN), -1.0);
    assert_eq!(i16_to_f32(0), 0.0);
}

#[test]
fn audio_f32_to_i16_rounds_clamps_and_maps_nan_to_zero() {
    assert_eq!(f32_to_i16(1.0), i16::MAX);
    assert_eq!(f32_to_i16(2.5), i16::MAX);
    assert_eq!(f32_to_i16(-1.0), i16::MIN);
    assert_eq!(f32_to_i16(-7.0), i16::MIN);
    assert_eq!(f32_to_i16(f32::NAN), 0);
    assert_eq!(f32_to_i16(f32::INFINITY), i16::MAX);
    assert_eq!(f32_to_i16(f32::NEG_INFINITY), i16::MIN);
    // 0.6 LSB rounds up, 0.4 LSB rounds down.
    assert_eq!(f32_to_i16(0.6 / 32768.0), 1);
    assert_eq!(f32_to_i16(0.4 / 32768.0), 0);
    assert_eq!(f32_to_i16(-0.6 / 32768.0), -1);
}

#[test]
fn audio_slice_conversion_converts_the_common_prefix() {
    let src = [i16::MIN, -1, 0, 1, i16::MAX];
    let mut f = [9.0f32; 4];
    assert_eq!(i16_slice_to_f32(&src, &mut f), 4);
    assert_eq!(f, [-1.0, -1.0 / 32768.0, 0.0, 1.0 / 32768.0]);
    let mut back = [7i16; 6];
    assert_eq!(f32_slice_to_i16(&f, &mut back), 4);
    assert_eq!(back, [i16::MIN, -1, 0, 1, 7, 7]);
}

// ---------------------------------------------------------------- ring buffer

#[test]
fn audio_ring_push_pop_is_fifo_and_respects_capacity() {
    let mut r = RingBuffer::new(4);
    assert!(r.is_empty());
    assert_eq!(r.capacity(), 4);
    assert_eq!(r.push(&[1.0, 2.0, 3.0]), 3);
    assert_eq!(r.push(&[4.0, 5.0]), 1, "only one slot left");
    assert!(r.is_full());
    assert_eq!(r.free(), 0);
    let mut out = [0.0; 3];
    assert_eq!(r.pop(&mut out), 3);
    assert_eq!(out, [1.0, 2.0, 3.0]);
    assert_eq!(r.len(), 1);
    let mut rest = [0.0; 8];
    assert_eq!(r.pop(&mut rest), 1);
    assert_eq!(rest[0], 4.0);
    assert_eq!(r.pop(&mut rest), 0);
}

#[test]
fn audio_ring_wraparound_preserves_order_over_many_cycles() {
    let mut r = RingBuffer::new(7);
    let mut next_in = 0.0f32;
    let mut next_out = 0.0f32;
    // Push/pop sizes that are coprime with the capacity so every wrap offset is hit.
    for step in 0..500usize {
        let n_push = 1 + step % 5;
        let chunk: Vec<f32> = (0..n_push).map(|i| next_in + i as f32).collect();
        let pushed = r.push(&chunk);
        next_in += pushed as f32;
        let mut out = vec![0.0; 1 + (step * 3) % 4];
        let popped = r.pop(&mut out);
        for &v in &out[..popped] {
            assert_eq!(v, next_out);
            next_out += 1.0;
        }
        assert_eq!(r.len(), (next_in - next_out) as usize);
    }
    assert!(next_out > 500.0, "test must actually move data");
}

#[test]
fn audio_ring_peek_does_not_consume_and_peek_at_offsets() {
    let mut r = RingBuffer::new(5);
    r.push(&[0.0, 1.0, 2.0, 3.0]);
    r.discard(3); // head now at index 3; next push wraps
    r.push(&[4.0, 5.0, 6.0]);
    let mut out = [0.0; 4];
    assert_eq!(r.peek(&mut out), 4);
    assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    assert_eq!(r.len(), 4);
    let mut two = [0.0; 2];
    assert_eq!(r.peek_at(1, &mut two), 2);
    assert_eq!(two, [4.0, 5.0]);
    assert_eq!(r.peek_at(3, &mut two), 1);
    assert_eq!(two[0], 6.0);
    assert_eq!(r.peek_at(4, &mut two), 0);
    assert_eq!(r.discard(10), 4);
    assert!(r.is_empty());
}

#[test]
fn audio_ring_push_overwrite_keeps_the_newest_samples() {
    let mut r = RingBuffer::new(4);
    r.push(&[1.0, 2.0, 3.0]);
    assert_eq!(r.push_overwrite(&[4.0, 5.0]), 1);
    let mut out = [0.0; 4];
    r.peek(&mut out);
    assert_eq!(out, [2.0, 3.0, 4.0, 5.0]);
    // Longer than capacity: old content and the front of src are dropped.
    assert_eq!(r.push_overwrite(&[6.0, 7.0, 8.0, 9.0, 10.0, 11.0]), 4 + 2);
    r.peek(&mut out);
    assert_eq!(out, [8.0, 9.0, 10.0, 11.0]);
    r.clear();
    assert!(r.is_empty());
    assert_eq!(r.push_overwrite(&[1.0]), 0);
}

#[test]
#[should_panic(expected = "capacity")]
fn audio_ring_zero_capacity_panics() {
    let _ = RingBuffer::new(0);
}

// ---------------------------------------------------------------- resampler

#[test]
fn audio_resample_48k_to_16k_length_is_exact() {
    for len in [0usize, 1, 479, 480, 481, 48_000, 48_001, 100_003] {
        let x = vec![0.1f32; len];
        let y = resample_all(&x, SampleRate::HZ_48000, SampleRate::HZ_16000).unwrap();
        let expected = (len as f64 / 3.0).round() as usize;
        assert_eq!(y.len(), expected, "input {len}");
    }
    let x = vec![0.0f32; 16_000];
    assert_eq!(
        resample_all(&x, SampleRate::HZ_16000, SampleRate::HZ_48000)
            .unwrap()
            .len(),
        48_000
    );
    let r = SampleRate::new(44_100).unwrap();
    assert_eq!(
        resample_all(&vec![0.0; 44_100], r, SampleRate::HZ_16000)
            .unwrap()
            .len(),
        16_000
    );
}

#[test]
fn audio_resample_preserves_sine_frequency_and_amplitude() {
    // Tones inside the 16 kHz passband. Fit at the expected output frequency: a wrong
    // frequency or amplitude, distortion or aliasing all show up in the residual.
    for freq in [100.0, 1_000.0, 3_000.0, 6_000.0] {
        let x = sine(SampleRate::HZ_48000, freq, 0.5, 96_000);
        let y = resample_all(&x, SampleRate::HZ_48000, SampleRate::HZ_16000).unwrap();
        // Skip edges where the filter sees the clip boundary.
        let body = &y[500..y.len() - 500];
        let (amp, resid) = fit_sine(body, SampleRate::HZ_16000, freq);
        let gain_db = 20.0 * (amp / 0.5).log10();
        let snr_db = 20.0 * (amp / std::f64::consts::SQRT_2 / resid).log10();
        eprintln!("{freq} Hz: gain {gain_db:.4} dB, residual SNR {snr_db:.1} dB");
        assert!(gain_db.abs() < 0.1, "{freq} Hz gain {gain_db:.4} dB");
        assert!(snr_db > 70.0, "{freq} Hz residual SNR {snr_db:.1} dB");
    }
}

#[test]
fn audio_resample_upsampling_preserves_sine() {
    let x = sine(SampleRate::HZ_16000, 1_000.0, 0.5, 32_000);
    let y = resample_all(&x, SampleRate::HZ_16000, SampleRate::HZ_48000).unwrap();
    let body = &y[1_500..y.len() - 1_500];
    let (amp, resid) = fit_sine(body, SampleRate::HZ_48000, 1_000.0);
    assert!((20.0 * (amp / 0.5).log10()).abs() < 0.1);
    assert!(20.0 * (amp / std::f64::consts::SQRT_2 / resid).log10() > 70.0);
}

/// How many output frames a resampled Gaussian pulse lags the input (negative: leads),
/// from the output's peak with parabolic interpolation. Unambiguous (unlike a tone's
/// phase): the pulse (sigma 1 ms, centred at 0.5 s) is a single smooth peak, and its
/// spectrum is negligible above 4 kHz (below every Nyquist used here), so the resampler's
/// linear-phase filter only delays it.
fn resample_pulse_lag_frames(from: SampleRate, to: SampleRate) -> f64 {
    let (sigma, centre) = (0.001, 0.5);
    let x: Vec<f32> = (0..from.hz() as usize)
        .map(|n| {
            let t = n as f64 / f64::from(from.hz());
            (0.5 * (-((t - centre) / sigma).powi(2)).exp()) as f32
        })
        .collect();
    let y = resample_all(&x, from, to).unwrap();
    let k = (1..y.len() - 1)
        .max_by(|&a, &b| y[a].total_cmp(&y[b]))
        .unwrap();
    let (a, b, c) = (f64::from(y[k - 1]), f64::from(y[k]), f64::from(y[k + 1]));
    let peak = k as f64 + 0.5 * (a - c) / (a - 2.0 * b + c);
    peak - centre * f64::from(to.hz())
}

/// How many output frames the resampled 1 kHz tone lags the input (negative: leads), from
/// its phase: precise, but only modulo one tone period. A 1 kHz tone has a known phase at
/// t = 0; the body (1/16 .. 15/16 s) avoids the edges.
fn resample_lag_frames(from: SampleRate, to: SampleRate) -> f64 {
    let x = sine(from, 1_000.0, 0.5, from.hz() as usize);
    let y = resample_all(&x, from, to).unwrap();
    let w = 2.0 * PI * 1_000.0 / f64::from(to.hz());
    let (start, end) = (to.hz() as usize / 16, to.hz() as usize * 15 / 16);
    let (mut c, mut s) = (0.0, 0.0);
    for (n, &v) in y.iter().enumerate().take(end).skip(start) {
        c += f64::from(v) * (w * n as f64).cos();
        s += f64::from(v) * (w * n as f64).sin();
    }
    // y[n] = A sin(wn + phi) -> s ~ A/2 cos(phi), c ~ A/2 sin(phi); lag = -phi / w.
    -c.atan2(s) / w
}

#[test]
fn audio_resample_output_is_time_aligned_with_input() {
    // The bound the `Resampler` docs state: within half an output frame, downsampling and
    // upsampling, at integer and fractional rate ratios.
    for (a, b) in [
        (48_000, 16_000),
        (16_000, 48_000),
        (32_000, 16_000),
        (16_000, 32_000),
        (44_100, 16_000),
        (16_000, 44_100),
        (16_000, 22_050),
    ] {
        let (from, to) = (SampleRate::new(a).unwrap(), SampleRate::new(b).unwrap());
        // The pulse locates the delay unambiguously; the tone phase gives the sub-frame
        // residual, and both must agree (a trim wrong by a whole tone period would move
        // the pulse but not the phase).
        let coarse = resample_pulse_lag_frames(from, to);
        let lag = resample_lag_frames(from, to);
        assert!(
            coarse.abs() <= 0.5 + 0.05,
            "{a} -> {b} Hz: pulse lags by {coarse:.3} output frames"
        );
        assert!(
            (coarse - lag).abs() <= 0.05,
            "{a} -> {b} Hz: pulse {coarse:.3} and tone phase {lag:.3} disagree"
        );
        assert!(
            lag.abs() <= 0.5,
            "{a} -> {b} Hz: output lags input by {lag:.3} output frames"
        );
    }
}

#[test]
fn audio_resample_rejects_aliasing() {
    // 10 kHz is above the 8 kHz output Nyquist; it must be filtered, not folded to 6 kHz.
    let x = sine(SampleRate::HZ_48000, 10_000.0, 0.5, 48_000);
    let y = resample_all(&x, SampleRate::HZ_48000, SampleRate::HZ_16000).unwrap();
    let body = &y[500..y.len() - 500];
    let level_db = 20.0 * (rms(body) / (0.5 / std::f64::consts::SQRT_2)).log10();
    eprintln!("alias level {level_db:.1} dB");
    assert!(level_db < -60.0, "alias level {level_db:.1} dB");
}

#[test]
fn audio_resample_streaming_is_independent_of_input_split() {
    let x = sine(SampleRate::HZ_48000, 440.0, 0.3, 30_011);
    let whole = resample_all(&x, SampleRate::HZ_48000, SampleRate::HZ_16000).unwrap();
    let mut r = Resampler::pipeline_48k_to_16k();
    let mut out = Vec::new();
    let mut rest = &x[..];
    for size in [1usize, 7, 333, 480, 1_000, 2, 4_799].iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let n = (*size).min(rest.len());
        r.process(&rest[..n], &mut out);
        rest = &rest[n..];
    }
    r.flush(&mut out);
    assert_eq!(out, whole);
    // After flush the resampler starts a fresh stream.
    let mut again = Vec::new();
    r.process(&x, &mut again);
    r.flush(&mut again);
    assert_eq!(again, whole);
}

#[test]
fn audio_resample_process_does_not_reallocate_with_reserved_output() {
    let mut r = Resampler::pipeline_48k_to_16k();
    let x = vec![0.25f32; 4_800];
    let mut out: Vec<f32> = Vec::new();
    for _ in 0..20 {
        out.clear();
        let bound = r.max_output_len(x.len());
        out.reserve(bound);
        let ptr = out.as_ptr();
        let cap = out.capacity();
        let n = r.process(&x, &mut out);
        assert!(n <= bound, "{n} frames exceeds max_output_len {bound}");
        assert_eq!(out.as_ptr(), ptr);
        assert_eq!(out.capacity(), cap);
    }
}

#[test]
fn audio_resample_ratio_change_mid_stream_is_click_free() {
    // A pure sine's second difference is bounded by w^2 * A. A dropped or repeated
    // sample, or a phase jump at a ratio change, shows up as a spike far above it
    // (a one-sample slip at 1 kHz / 16 kHz gives about w * A, 2.5x the bound).
    // Stated bound: |x[n+1] - 2x[n] + x[n-1]| <= 1.05 * w^2 * A + 1e-3 everywhere.
    let freq = 1_000.0;
    let amp = 0.5;
    let x = sine(SampleRate::HZ_48000, freq, amp, 48_000 * 4);
    let mut r = Resampler::pipeline_48k_to_16k();
    let mut out = Vec::new();
    // Ratio schedule per 0.5 s block: nominal, +200 ppm, -200 ppm, +1 %, back to nominal.
    let schedule = [1.0, 1.0002, 0.9998, 1.01, 1.0, 1.0, 1.0, 1.0];
    for (block, &rel) in x.chunks(24_000).zip(schedule.iter()) {
        r.set_relative_ratio(rel).unwrap();
        for piece in block.chunks(480) {
            r.process(piece, &mut out);
        }
    }
    let w_max = 2.0 * PI * freq / (16_000.0 * 0.99);
    let bound = 1.05 * w_max * w_max * amp + 1e-3;
    let body = &out[200..];
    let mut worst = 0.0f64;
    for i in 1..body.len() - 1 {
        let d2 = f64::from(body[i + 1]) - 2.0 * f64::from(body[i]) + f64::from(body[i - 1]);
        worst = worst.max(d2.abs());
    }
    eprintln!("max second difference {worst:.5}, bound {bound:.5}");
    assert!(
        worst <= bound,
        "max second difference {worst:.5} exceeds {bound:.5}"
    );
}

#[test]
fn audio_resample_relative_ratio_changes_output_rate() {
    // 10 s at +200 ppm must yield exactly 32 extra output frames over 160 000; a 1 %
    // increase yields 1 600 extra.
    for (rel, extra) in [(1.0002, 32.0), (0.9998, -32.0), (1.01, 1_600.0)] {
        let x = vec![0.0f32; 480_000];
        let mut r = Resampler::pipeline_48k_to_16k();
        r.set_relative_ratio(rel).unwrap();
        assert_eq!(r.relative_ratio(), rel);
        let mut out = Vec::new();
        r.process(&x, &mut out);
        r.flush(&mut out);
        let got = out.len() as f64 - 160_000.0;
        eprintln!("rel {rel}: {got} extra frames (expected {extra})");
        // A ratio set before any input applies without a ramp: the length is exact.
        assert_eq!(got, extra, "rel {rel}: {got} extra frames");
    }
}

#[test]
fn audio_resample_rejects_out_of_bounds_ratios() {
    let mut r = Resampler::pipeline_48k_to_16k();
    for bad in [1.0101, 0.9899, f64::NAN, f64::INFINITY, 0.0, -1.0] {
        assert!(
            matches!(
                r.set_relative_ratio(bad),
                Err(ResamplerError::RatioOutOfBounds { .. })
            ),
            "{bad}"
        );
        assert_eq!(r.relative_ratio(), 1.0);
    }
    assert!(r.set_relative_ratio(1.0 + DEFAULT_MAX_DEVIATION).is_ok());
    assert!(r.set_relative_ratio(1.0 - DEFAULT_MAX_DEVIATION).is_ok());
    let mut tight =
        Resampler::with_max_deviation(SampleRate::HZ_48000, SampleRate::HZ_16000, 0.001).unwrap();
    assert!(tight.set_relative_ratio(1.0005).is_ok());
    assert!(tight.set_relative_ratio(1.002).is_err());
    for bad in [-0.1, 0.6, f64::NAN] {
        assert!(matches!(
            Resampler::with_max_deviation(SampleRate::HZ_48000, SampleRate::HZ_16000, bad),
            Err(ResamplerError::InvalidMaxDeviation(_))
        ));
    }
}

// ---------------------------------------------------------------- wav

#[test]
fn audio_wav_float32_round_trip_through_vec_is_lossless() {
    let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 1.0, -1.0, 1.5, 1e-7, -0.333_333];
    let mut buf = Vec::new();
    write_wav(
        &mut buf,
        SampleRate::HZ_48000,
        1,
        &samples,
        WavFormat::Float32,
    )
    .unwrap();
    assert_eq!(buf.len(), 44 + samples.len() * 4);
    let wav = read_wav(Cursor::new(&buf)).unwrap();
    assert_eq!(wav.rate(), SampleRate::HZ_48000);
    assert_eq!(wav.channels(), 1);
    assert_eq!(wav.samples(), samples);
}

#[test]
fn audio_wav_pcm16_round_trip_quantises_to_one_lsb() {
    let x = sine(SampleRate::HZ_16000, 440.0, 0.9, 1_600);
    let mut buf = Vec::new();
    write_wav(&mut buf, SampleRate::HZ_16000, 1, &x, WavFormat::Pcm16).unwrap();
    assert_eq!(buf.len(), 44 + x.len() * 2);
    let wav = read_wav(buf.as_slice()).unwrap();
    assert_eq!(wav.rate(), SampleRate::HZ_16000);
    assert_eq!(wav.samples().len(), x.len());
    for (a, b) in x.iter().zip(wav.samples()) {
        assert!((a - b).abs() <= 0.5 / 32768.0 + 1e-9, "{a} vs {b}");
        // Values already on the i16 grid are exact.
        assert_eq!(*b, i16_to_f32(f32_to_i16(*a)));
    }
}

#[test]
fn audio_wav_stereo_channels_and_mono_mix() {
    let inter = vec![0.5, -0.5, 0.25, 0.75, 0.0, 1.0];
    let mut buf = Vec::new();
    write_wav(
        &mut buf,
        SampleRate::HZ_48000,
        2,
        &inter,
        WavFormat::Float32,
    )
    .unwrap();
    let wav = read_wav(buf.as_slice()).unwrap();
    assert_eq!(wav.channels(), 2);
    assert_eq!(wav.frames(), 3);
    assert_eq!(wav.channel(0).unwrap(), vec![0.5, 0.25, 0.0]);
    assert_eq!(wav.channel(1).unwrap(), vec![-0.5, 0.75, 1.0]);
    assert_eq!(wav.channel(2), None);
    assert_eq!(wav.to_mono(), vec![0.0, 0.5, 0.5]);
}

#[test]
fn audio_wav_reads_hound_written_integer_formats() {
    for (bits, value, expected) in [
        (16u16, 16_384i32, 0.5f32),
        (24, -4_194_304, -0.5),
        (32, 1_073_741_824, 0.5),
        (8, -64, -0.5),
    ] {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 22_050,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cur = Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
            w.write_sample(value).unwrap();
            w.write_sample(0i32).unwrap();
            w.finalize().unwrap();
        }
        let wav = read_wav(cur.get_ref().as_slice()).unwrap();
        assert_eq!(wav.rate().hz(), 22_050);
        assert_eq!(wav.samples(), vec![expected, 0.0], "{bits} bit");
    }
}

#[test]
fn audio_wav_rejects_bad_input() {
    assert!(matches!(
        read_wav(&b"not a wav file at all"[..]),
        Err(WavError::Format(_) | WavError::Io(_))
    ));
    let mut buf = Vec::new();
    assert!(matches!(
        write_wav(
            &mut buf,
            SampleRate::HZ_16000,
            2,
            &[0.0; 3],
            WavFormat::Pcm16
        ),
        Err(WavError::InvalidInput(_))
    ));
    assert!(matches!(
        write_wav(&mut buf, SampleRate::HZ_16000, 0, &[], WavFormat::Pcm16),
        Err(WavError::InvalidInput(_))
    ));
    // Truncated data chunk.
    write_wav(
        &mut buf,
        SampleRate::HZ_16000,
        1,
        &[0.1; 100],
        WavFormat::Pcm16,
    )
    .unwrap();
    buf.truncate(44 + 51);
    assert!(read_wav(buf.as_slice()).is_err());
}

#[test]
fn audio_wav_write_propagates_io_errors() {
    struct Failing;
    impl std::io::Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(matches!(
        write_wav(Failing, SampleRate::HZ_16000, 1, &[0.0], WavFormat::Float32),
        Err(WavError::Io(_))
    ));
}

#[test]
fn audio_resample_non_unity_ratio_restreams_identically_with_exact_length() {
    // Both directions and both signs of deviation. A stream at a constant relative ratio
    // r yields exactly round(n * to / from * r) frames, and after flush (which resets)
    // an identical second stream is bit-identical to the first.
    let cases = [
        (SampleRate::HZ_48000, SampleRate::HZ_16000, 1.01),
        (SampleRate::HZ_48000, SampleRate::HZ_16000, 0.99),
        (SampleRate::HZ_16000, SampleRate::HZ_48000, 1.01),
        (SampleRate::HZ_16000, SampleRate::HZ_48000, 0.9998),
    ];
    for (from, to, rel) in cases {
        let x = sine(from, 440.0, 0.4, from.hz() as usize + 77);
        let mut r = Resampler::new(from, to).unwrap();
        let delay = r.output_delay();
        r.set_relative_ratio(rel).unwrap();
        let expected =
            (x.len() as f64 * f64::from(to.hz()) / f64::from(from.hz()) * rel).round() as usize;
        let mut first = Vec::new();
        r.process(&x, &mut first);
        r.flush(&mut first);
        assert_eq!(first.len(), expected, "{from:?}->{to:?} at {rel}");
        assert_eq!(r.output_delay(), delay, "trim must not follow the ratio");
        let mut second = Vec::new();
        r.process(&x, &mut second);
        r.flush(&mut second);
        assert_eq!(
            second, first,
            "{from:?}->{to:?} at {rel}: re-stream differs"
        );
        // An explicit reset behaves the same way.
        let mut third = Vec::new();
        r.process(&x[..1_000], &mut third);
        r.reset();
        third.clear();
        r.process(&x, &mut third);
        r.flush(&mut third);
        assert_eq!(
            third, first,
            "{from:?}->{to:?} at {rel}: reset stream differs"
        );
    }
}

#[test]
fn audio_resample_rejects_unsupported_rates() {
    let tiny = SampleRate::new(1).unwrap();
    let huge = SampleRate::new(u32::MAX).unwrap();
    for (from, to) in [
        (tiny, huge),
        (huge, tiny),
        (tiny, SampleRate::HZ_16000),
        (SampleRate::HZ_48000, huge),
    ] {
        assert!(
            matches!(
                Resampler::new(from, to),
                Err(ResamplerError::Construction(_))
            ),
            "{from:?} -> {to:?}"
        );
        assert!(resample_all(&[0.0; 10], from, to).is_err());
    }
    assert!(
        Resampler::new(
            SampleRate::new(8_000).unwrap(),
            SampleRate::new(192_000).unwrap()
        )
        .is_ok()
    );
}

#[test]
fn audio_wav_audio_new_rejects_zero_channels_and_partial_frames() {
    // A zero-channel value used to be constructible and panicked in frames()/to_mono().
    assert!(matches!(
        WavAudio::new(SampleRate::HZ_16000, 0, vec![]),
        Err(WavError::InvalidInput(_))
    ));
    assert!(matches!(
        WavAudio::new(SampleRate::HZ_16000, 2, vec![0.0; 3]),
        Err(WavError::InvalidInput(_))
    ));
    let w = WavAudio::new(SampleRate::HZ_16000, 2, vec![0.5, -0.5, 1.0, 0.0]).unwrap();
    assert_eq!(
        (w.frames(), w.channels(), w.channel_count().get()),
        (2, 2, 2)
    );
    assert_eq!(w.to_mono(), vec![0.0, 0.5]);
    assert_eq!(w.channel(1), Some(vec![-0.5, 0.0]));
    assert_eq!(w.into_samples().len(), 4);
}
