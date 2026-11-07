//! How loud stretches of a FLAC track are (T9.1i: is a quiet mic voice a voice at all?). Prints
//! levels only, never audio or text. `spans` holds one stretch per line, `<label> <start_ms>
//! <end_ms>`; `--track` prints the whole track's level per 30 ms frame percentiles too.
//!
//! cargo run --release -p earshot-runtime --example span_levels -- <track.flac> <spans.txt>

use std::collections::BTreeMap;

/// dBFS of a mean square; -120 for silence.
fn db(mean_square: f64) -> f64 {
    if mean_square <= 1e-12 {
        -120.0
    } else {
        10.0 * mean_square.log10()
    }
}

/// `sorted`'s value at fraction `p`.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [flac, spans] = args.as_slice() else {
        panic!("usage: span_levels <track.flac> <spans.txt>");
    };
    let mut reader = claxon::FlacReader::open(flac).expect("a FLAC file");
    let info = reader.streaminfo();
    assert_eq!(info.channels, 1, "mono tracks only");
    let rate = u64::from(info.sample_rate);
    let scale = f64::from(1u32 << (info.bits_per_sample - 1));
    let samples: Vec<f32> = reader
        .samples()
        .map(|s| (f64::from(s.expect("a sample")) / scale) as f32)
        .collect();

    let frame = usize::try_from(rate * 30 / 1000).expect("frame length");
    let mut frames: Vec<f64> = samples
        .chunks_exact(frame)
        .map(|c| db(c.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / c.len() as f64))
        .collect();
    frames.sort_by(f64::total_cmp);
    println!(
        "track: {:.0} s, 30 ms frames dBFS p10 {:.1} p50 {:.1} p90 {:.1} p99 {:.1}",
        samples.len() as f64 / rate as f64,
        percentile(&frames, 0.10),
        percentile(&frames, 0.50),
        percentile(&frames, 0.90),
        percentile(&frames, 0.99),
    );

    let mut by_label: BTreeMap<String, Vec<(u64, u64, f64, f64)>> = BTreeMap::new();
    for line in std::fs::read_to_string(spans)
        .expect("the spans file")
        .lines()
    {
        let mut parts = line.split_whitespace();
        let (Some(label), Some(start), Some(end)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (start, end): (u64, u64) = (start.parse().expect("ms"), end.parse().expect("ms"));
        let at = |ms: u64| {
            usize::try_from(ms * rate / 1000)
                .unwrap_or(usize::MAX)
                .min(samples.len())
        };
        let part = &samples[at(start)..at(end)];
        if part.is_empty() {
            continue;
        }
        // The loudest 30 ms inside the span and its RMS: a word in a span of mostly silence
        // reads by its loudest frames.
        let loudest = part
            .chunks(frame)
            .map(|c| {
                db(c.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / c.len() as f64)
            })
            .fold(-120.0, f64::max);
        let rms = db(part
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / part.len() as f64);
        by_label
            .entry(label.to_owned())
            .or_default()
            .push((start, end, rms, loudest));
    }
    for (label, rows) in &by_label {
        let mut loud: Vec<f64> = rows.iter().map(|r| r.3).collect();
        loud.sort_by(f64::total_cmp);
        let mut rms: Vec<f64> = rows.iter().map(|r| r.2).collect();
        rms.sort_by(f64::total_cmp);
        let seconds: u64 = rows.iter().map(|r| r.1 - r.0).sum();
        println!(
            "{label}: {} spans, {:.1} s; span rms dBFS p10 {:.1} p50 {:.1} p90 {:.1}; loudest frame p10 {:.1} p50 {:.1} p90 {:.1}",
            rows.len(),
            seconds as f64 / 1000.0,
            percentile(&rms, 0.10),
            percentile(&rms, 0.50),
            percentile(&rms, 0.90),
            percentile(&loud, 0.10),
            percentile(&loud, 0.50),
            percentile(&loud, 0.90),
        );
        if rows.len() <= 30 {
            for r in rows {
                println!(
                    "  {:.1} s: rms {:.1}, loudest frame {:.1}",
                    r.0 as f64 / 1000.0,
                    r.2,
                    r.3
                );
            }
        }
    }
}
