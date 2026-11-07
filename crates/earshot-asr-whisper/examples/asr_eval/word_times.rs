//! `word_times` (informational, T2.2d; runs only when named with `--metric word_times`,
//! never as part of `accept m2`): whisper word start times from token-level timestamps
//! against DTW over the alignment heads (`earshot_asr_whisper::dtw`), per whisper model.
//!
//! Measurement rule (fixed 2026-10-25, before the first measurement):
//!
//! - Models: `--word-time-models a,b,...`, default every model of
//!   `earshot_asr_whisper::dtw::ALIGNMENT_HEADS` (the manifest's whisper models). Each model
//!   is loaded twice through `evidence::load_verified` on `--backend`, once per
//!   `WordTiming`, with the same threads; every other setting is identical. Decoding:
//!   `TranscribeOptions::live()` (greedy with whisper's temperature fallback; what the
//!   default live-stretch final pass decodes with), one call per item, the state kept as
//!   in production.
//! - `ami` (the error measurement; `--word-time-corpora ami,smoke` selects corpora, default
//!   both): the AMI IHM set of `asr.wer_ami_ihm` (`refs::load_ami_ihm`, IS1009a + ES2004a),
//!   every oracle segment decoded by one call on the speaker's headset. Hypothesis words:
//!   `refs::kept_words` of each decode, start = segment start (ms) + word start. Reference:
//!   the speaker's words inside a segment (`AmiHeadset::segment_words`), start times from
//!   the AMI manual annotations v1.6.2 words files (forced alignments). Words are matched
//!   per headset by a longest common subsequence of `normalize_token` texts (empty tokens
//!   never match), the same rule as `model_eval`'s `ami` timing. Reported: matched words,
//!   median, 90th percentile (sorted value at index round((n - 1) p)) and share within
//!   250 ms of the absolute start error, the median signed error (hypothesis minus
//!   reference), words with start == end (untimed for LocalAgreement), decode time and
//!   real-time factor, and segments whose kept text differs between the two timings.
//! - `smoke` (speed, and a relative comparison only: the smoke set has no word
//!   alignments): the 20 committed clips, one call each. Reported: decode time and RTF per
//!   timing and, for clips whose kept text is identical, the median and 90th percentile of
//!   |DTW start - token start| over words at the same position.
//!
//! Decision rule (fixed with the measurement rule): DTW becomes the default word timing of
//! a model only if, on `ami`, both its median and its 90th percentile absolute start error
//! are at least one DTW frame (20 ms) below token timestamps', and its decode time is at
//! most 10% above token timestamps' on the CUDA `ami` run and on the CPU `smoke` run.

use super::*;
use earshot_asr_whisper::WordTiming;
use earshot_asr_whisper::dtw::ALIGNMENT_HEADS;

/// Metric id.
pub const ID: &str = "word_times";
/// A start within this of its reference counts as well timed (as `model_eval`).
const TIMING_OK_MS: u64 = 250;
/// One DTW frame (whisper.cpp encoder frame), the decision rule's margin.
const DECISION_MARGIN_MS: u64 = 20;
/// The decision rule's speed allowance (DTW decode time / token decode time).
const DECISION_MAX_SLOWDOWN: f64 = 1.10;

/// Corpora of this measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corpus {
    Ami,
    Smoke,
}

/// `--word-time-models` / `--word-time-corpora` values.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub models: Vec<String>,
    pub corpora: Vec<Corpus>,
}

impl Options {
    /// Parses a `--word-time-models` value.
    pub fn parse_models(v: &str) -> Vec<String> {
        let models: Vec<String> = v.split(',').map(|m| m.trim().to_owned()).collect();
        for m in &models {
            if !ALIGNMENT_HEADS.iter().any(|h| h.model == m) {
                fail(format!(
                    "--word-time-models: {m} has no pinned alignment heads"
                ));
            }
        }
        models
    }

    /// Parses a `--word-time-corpora` value.
    pub fn parse_corpora(v: &str) -> Vec<Corpus> {
        v.split(',')
            .map(|c| match c.trim() {
                "ami" => Corpus::Ami,
                "smoke" => Corpus::Smoke,
                other => fail(format!("--word-time-corpora: unknown corpus {other}")),
            })
            .collect()
    }

    fn models(&self) -> Vec<String> {
        if self.models.is_empty() {
            ALIGNMENT_HEADS.iter().map(|h| h.model.to_owned()).collect()
        } else {
            self.models.clone()
        }
    }

    fn wants(&self, c: Corpus) -> bool {
        self.corpora.is_empty() || self.corpora.contains(&c)
    }
}

/// Index pairs of a longest common subsequence of `a` and `b` under `same` (the
/// `model_eval` alignment).
pub fn lcs_pairs(a: usize, b: usize, same: impl Fn(usize, usize) -> bool) -> Vec<(usize, usize)> {
    // Directions: 0 diagonal (match), 1 up (skip a), 2 left (skip b).
    let mut dir = vec![0u8; (a + 1) * (b + 1)];
    let mut prev = vec![0u32; b + 1];
    let mut cur = vec![0u32; b + 1];
    for i in 1..=a {
        cur[0] = 0;
        for j in 1..=b {
            let (v, d) = if same(i - 1, j - 1) {
                (prev[j - 1] + 1, 0)
            } else if prev[j] >= cur[j - 1] {
                (prev[j], 1)
            } else {
                (cur[j - 1], 2)
            };
            cur[j] = v;
            dir[i * (b + 1) + j] = d;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let (mut i, mut j, mut out) = (a, b, Vec::new());
    while i > 0 && j > 0 {
        match dir[i * (b + 1) + j] {
            0 => {
                out.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
            1 => i -= 1,
            _ => j -= 1,
        }
    }
    out.reverse();
    out
}

/// Signed start errors (hypothesis minus reference, ms) of hypothesis words matched in
/// order to reference words by normalised text.
pub fn start_errors(hyp: &[(String, u64)], reference: &[(String, u64)]) -> Vec<i64> {
    lcs_pairs(hyp.len(), reference.len(), |i, j| {
        !hyp[i].0.is_empty() && hyp[i].0 == reference[j].0
    })
    .into_iter()
    .map(|(i, j)| hyp[i].1 as i64 - reference[j].1 as i64)
    .collect()
}

/// The sorted value at index round((n - 1) p); `None` when empty.
pub fn percentile<T: Copy + Ord>(values: &[T], p: f64) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let k = ((v.len() - 1) as f64 * p).round() as usize;
    Some(v[k])
}

/// Statistics of signed start errors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErrorStats {
    pub matched: usize,
    pub median_ms: u64,
    pub p90_ms: u64,
    pub within_250_percent: f64,
    pub signed_median_ms: i64,
}

impl ErrorStats {
    pub fn of(errors: &[i64]) -> Option<Self> {
        let abs: Vec<u64> = errors.iter().map(|e| e.unsigned_abs()).collect();
        Some(ErrorStats {
            matched: errors.len(),
            median_ms: percentile(&abs, 0.5)?,
            p90_ms: percentile(&abs, 0.9)?,
            within_250_percent: 100.0 * abs.iter().filter(|&&e| e <= TIMING_OK_MS).count() as f64
                / abs.len() as f64,
            signed_median_ms: percentile(errors, 0.5)?,
        })
    }

    fn json(self) -> Value {
        json!({
            "matched_words": self.matched,
            "start_error_median_ms": self.median_ms,
            "start_error_p90_ms": self.p90_ms,
            "within_250ms_percent": self.within_250_percent,
            "signed_error_median_ms": self.signed_median_ms,
        })
    }
}

/// The accuracy part of the decision rule: DTW's median and p90 absolute start error both
/// at least [`DECISION_MARGIN_MS`] below token timestamps'.
pub fn accuracy_met(token: ErrorStats, dtw: ErrorStats) -> bool {
    dtw.median_ms + DECISION_MARGIN_MS <= token.median_ms
        && dtw.p90_ms + DECISION_MARGIN_MS <= token.p90_ms
}

/// One speed check of the decision rule, or `None` when this run is not one the rule names
/// (only CUDA `ami` and CPU `smoke` count) or the corpus did not run. The two checks come
/// from two reports (one backend per run), so no single report decides the whole rule.
pub fn speed_check(backend: BackendKind, corpus: Corpus, slowdown: Option<f64>) -> Option<bool> {
    let named = matches!(
        (backend, corpus),
        (BackendKind::Cuda, Corpus::Ami) | (BackendKind::Cpu, Corpus::Smoke)
    );
    slowdown
        .filter(|_| named)
        .map(|s| s <= DECISION_MAX_SLOWDOWN)
}

/// Normalised word texts with their start (ms).
type TimedWords = Vec<(String, u64)>;

/// Decodes of one model with one timing.
struct Run {
    /// Per AMI headset: timed hypothesis words and the reference.
    ami: Vec<(TimedWords, TimedWords)>,
    /// Kept text of every AMI segment, in order.
    ami_texts: Vec<String>,
    ami_words: usize,
    ami_untimed: usize,
    ami_audio: usize,
    ami_decode: Duration,
    /// Per smoke clip: kept words (normalised text, start ms).
    smoke: Vec<TimedWords>,
    smoke_audio: usize,
    smoke_decode: Duration,
    load: Value,
}

fn run(
    args: &Args,
    name: &str,
    timing: WordTiming,
    ami: &[refs::AmiHeadset],
    smoke: &[Utterance],
) -> Run {
    let mut model = load_timed(args, name, args.backend, timing);
    if model.transcriber.word_timing() != timing {
        fail(format!(
            "{name}: loaded with {:?}",
            model.transcriber.word_timing()
        ));
    }
    let options = TranscribeOptions::live();
    let mut out = Run {
        ami: Vec::new(),
        ami_texts: Vec::new(),
        ami_words: 0,
        ami_untimed: 0,
        ami_audio: 0,
        ami_decode: Duration::ZERO,
        smoke: Vec::new(),
        smoke_audio: 0,
        smoke_decode: Duration::ZERO,
        load: Value::Null,
    };
    // Every call's backend is checked against its own log by `transcribe_full`.
    for h in ami {
        let mut timed = Vec::new();
        for (range, audio) in h.ranges.iter().zip(h.segment_audio()) {
            let (t, dt) = transcribe_full(&mut model, audio, &options);
            out.ami_decode += dt;
            out.ami_audio += audio.len();
            let words = refs::kept_words(&t);
            let offset =
                (range.start as f64 * 1000.0 / f64::from(PIPELINE_RATE.hz())).round() as u64;
            for w in &words {
                out.ami_words += 1;
                out.ami_untimed += usize::from(w.start == w.end);
                timed.push((normalize_token(&w.text), offset + w.start.as_millis()));
            }
            out.ami_texts.push(refs::hypothesis_text(&words));
        }
        let reference = h
            .segment_words()
            .iter()
            .map(|w| {
                (
                    normalize_token(&w.text),
                    (w.start * 1000.0).round().max(0.0) as u64,
                )
            })
            .collect();
        eprintln!(
            "asr_eval: {ID}: {name} {timing:?} {} {} ({} segments)",
            h.meeting,
            h.channel,
            h.ranges.len()
        );
        out.ami.push((timed, reference));
    }
    for u in smoke {
        let (t, dt) = transcribe_full(&mut model, &u.audio, &options);
        out.smoke_decode += dt;
        out.smoke_audio += u.audio.len();
        out.smoke.push(
            refs::kept_words(&t)
                .iter()
                .map(|w| (normalize_token(&w.text), w.start.as_millis()))
                .collect(),
        );
    }
    out.load = model.report;
    out
}

fn rtf(decode: Duration, samples: usize) -> Option<f64> {
    (samples > 0).then(|| decode.as_secs_f64() * f64::from(PIPELINE_RATE.hz()) / samples as f64)
}

fn ami_stats(r: &Run) -> Option<ErrorStats> {
    let errors: Vec<i64> = r
        .ami
        .iter()
        .flat_map(|(hyp, reference)| start_errors(hyp, reference))
        .collect();
    ErrorStats::of(&errors)
}

fn run_json(r: &Run, stats: Option<ErrorStats>) -> Value {
    json!({
        "load": r.load,
        "ami": stats.map(ErrorStats::json),
        "ami_words": r.ami_words,
        "ami_untimed_words": r.ami_untimed,
        "ami_decode_s": r.ami_decode.as_secs_f64(),
        "ami_rtf": rtf(r.ami_decode, r.ami_audio),
        "smoke_decode_s": r.smoke_decode.as_secs_f64(),
        "smoke_rtf": rtf(r.smoke_decode, r.smoke_audio),
    })
}

/// |DTW start - token start| over words at the same position of clips with identical text.
fn smoke_shift(token: &Run, dtw: &Run) -> Value {
    let (mut shifts, mut same, mut differ) = (Vec::new(), 0usize, 0usize);
    for (a, b) in token.smoke.iter().zip(&dtw.smoke) {
        let text = |w: &[(String, u64)]| w.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>();
        if text(a) != text(b) {
            differ += 1;
            continue;
        }
        same += 1;
        shifts.extend(a.iter().zip(b).map(|(x, y)| x.1.abs_diff(y.1)));
    }
    json!({
        "note": "relative only: the smoke set has no word alignments",
        "clips_same_text": same,
        "clips_different_text": differ,
        "words": shifts.len(),
        "shift_median_ms": percentile(&shifts, 0.5),
        "shift_p90_ms": percentile(&shifts, 0.9),
    })
}

/// The `word_times` informational result.
pub fn word_times(args: &Args, opts: &Options) -> Value {
    let ami = if opts.wants(Corpus::Ami) {
        refs::load_ami_ihm(&args.fixtures, &refs::AMI_MEETINGS).unwrap_or_else(|e| fail(e))
    } else {
        Vec::new()
    };
    let smoke = if opts.wants(Corpus::Smoke) {
        smoke_set(&args.root)
    } else {
        Vec::new()
    };
    let mut models = Vec::new();
    for name in opts.models() {
        let token = run(args, &name, WordTiming::TokenTimestamps, &ami, &smoke);
        let dtw = run(args, &name, WordTiming::Dtw, &ami, &smoke);
        let (ts, ds) = (ami_stats(&token), ami_stats(&dtw));
        let text_diff = token
            .ami_texts
            .iter()
            .zip(&dtw.ami_texts)
            .filter(|(a, b)| a != b)
            .count();
        let slowdown = |a: Duration, b: Duration| {
            (a > Duration::ZERO).then(|| b.as_secs_f64() / a.as_secs_f64())
        };
        let ami_slowdown = slowdown(token.ami_decode, dtw.ami_decode);
        let smoke_slowdown = slowdown(token.smoke_decode, dtw.smoke_decode);
        let accuracy = match (ts, ds) {
            (Some(t), Some(d)) => Some(accuracy_met(t, d)),
            _ => None,
        };
        let ami_speed = speed_check(args.backend, Corpus::Ami, ami_slowdown);
        let smoke_speed = speed_check(args.backend, Corpus::Smoke, smoke_slowdown);
        eprintln!(
            "asr_eval: {ID}: RESULT {name} on {}: token {:?} | dtw {:?} | ami slowdown {:?} smoke \
             slowdown {:?} | ami segments with different text {text_diff} | accuracy rule met \
             {accuracy:?}, ami speed check {ami_speed:?}, smoke speed check {smoke_speed:?}",
            backend_name(args.backend),
            ts,
            ds,
            ami_slowdown,
            smoke_slowdown
        );
        models.push(json!({
            "model": name,
            "token_timestamps": run_json(&token, ts),
            "dtw": run_json(&dtw, ds),
            "ami_segments": token.ami_texts.len(),
            "ami_segments_with_different_text": text_diff,
            "ami_dtw_slowdown": ami_slowdown,
            "smoke_dtw_slowdown": smoke_slowdown,
            "smoke_shift": smoke_shift(&token, &dtw),
            // The whole rule needs a CUDA ami report and a CPU smoke report.
            "accuracy_rule_met": accuracy,
            "ami_speed_check_met": ami_speed,
            "smoke_speed_check_met": smoke_speed,
        }));
    }
    json!({
        "id": ID,
        "backend": backend_name(args.backend),
        "decoding": TranscribeOptions::live(),
        "rule": "see examples/asr_eval/word_times.rs (fixed 2026-10-25 before measuring): AMI IHM \
                 oracle segments (IS1009a, ES2004a), kept words, LCS on normalised tokens per \
                 headset against AMI manual word starts; DTW is a model's default only if median \
                 and p90 absolute start error are both >= 20 ms lower and DTW decode time is <= \
                 1.10 x token timestamps' on the CUDA ami run and the CPU smoke run",
        "models": models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(v: &[(&str, u64)]) -> Vec<(String, u64)> {
        v.iter().map(|&(t, s)| (t.to_owned(), s)).collect()
    }

    #[test]
    fn asr_eval_word_times_match_in_order_by_text() {
        let hyp = words(&[
            ("the", 100),
            ("cat", 400),
            ("", 450),
            ("sat", 900),
            ("x", 950),
        ]);
        let reference = words(&[("the", 120), ("cat", 300), ("sat", 1000), ("down", 1200)]);
        assert_eq!(start_errors(&hyp, &reference), [-20, 100, -100]);
        // An empty token never matches, even an empty reference token.
        assert!(start_errors(&words(&[("", 0)]), &words(&[("", 0)])).is_empty());
        assert!(start_errors(&[], &reference).is_empty());
    }

    #[test]
    fn asr_eval_word_times_stats_use_the_model_eval_percentile() {
        assert_eq!(percentile::<u64>(&[], 0.5), None);
        assert_eq!(percentile(&[5u64, 1, 3], 0.5), Some(3));
        // index round(9 * 0.9) = 8 of 0..=9
        let ten: Vec<u64> = (0..10).collect();
        assert_eq!(percentile(&ten, 0.9), Some(8));
        let s = ErrorStats::of(&[-300, 10, 20, -40, 250]).unwrap();
        assert_eq!(s.matched, 5);
        assert_eq!(s.median_ms, 40);
        assert_eq!(s.p90_ms, 300);
        assert_eq!(s.signed_median_ms, 10);
        assert!((s.within_250_percent - 80.0).abs() < 1e-9);
        assert!(ErrorStats::of(&[]).is_none());
    }

    #[test]
    fn asr_eval_word_times_decision_needs_both_errors_lower_and_little_slowdown() {
        let s = |median_ms, p90_ms| ErrorStats {
            matched: 1,
            median_ms,
            p90_ms,
            within_250_percent: 0.0,
            signed_median_ms: 0,
        };
        let token = s(130, 650);
        assert!(accuracy_met(token, s(110, 630)));
        assert!(!accuracy_met(token, s(111, 300)), "median not 20 ms lower");
        assert!(!accuracy_met(token, s(60, 631)), "p90 not 20 ms lower");
        use BackendKind::{Cpu, Cuda};
        assert_eq!(speed_check(Cuda, Corpus::Ami, Some(1.10)), Some(true));
        assert_eq!(speed_check(Cuda, Corpus::Ami, Some(1.11)), Some(false));
        assert_eq!(speed_check(Cpu, Corpus::Smoke, Some(0.9)), Some(true));
        assert_eq!(speed_check(Cpu, Corpus::Smoke, Some(1.2)), Some(false));
    }

    #[test]
    fn asr_eval_word_times_speed_checks_only_count_the_runs_the_rule_names() {
        use BackendKind::{Cpu, Cuda};
        // A CUDA smoke run and a CPU ami run are not part of the rule, and a corpus that
        // did not run has no slowdown: neither reports a check.
        assert_eq!(speed_check(Cuda, Corpus::Smoke, Some(0.5)), None);
        assert_eq!(speed_check(Cpu, Corpus::Ami, Some(0.5)), None);
        assert_eq!(speed_check(Cuda, Corpus::Ami, None), None);
        assert_eq!(speed_check(Cpu, Corpus::Smoke, None), None);
    }
}
