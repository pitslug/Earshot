//! T12.89 (docs/quiet-voices.md, evaluation only): does more microphone gain, or a
//! compressor, help Earshot hear a quieter person in the room?
//!
//! Every condition of the rule (`off`, `gain12`, `comp4`, `comp10`) is applied to the whole
//! far-field recording (`Array1-01`) of each AMI dev meeting, quantised to 16-bit PCM as the
//! stored FLAC, and transcribed with the production final-pass constructors (as `m2_prod`):
//!
//! - `segments`: one batch call per oracle segment, scored by `refs::AmiHeadset::score`
//!   (mis-hearing alone, the `ami_sdm` protocol);
//! - `whole`: `asr::transcribe_in_pieces` over the meeting's UEM range, scored against every
//!   speaker's words in time order (missed plus mis-heard);
//! - `heard`: each speaker's share of reference words matched by a `whole` word with the same
//!   normalised text within [`MATCH_WINDOW_S`] ([`heard`]); the quietest speaker of each
//!   meeting by level over their solo segment time ([`speaker_levels`]).
//!
//! The report holds counts, levels and settings only, never transcript text.
//!
//! ```text
//! cargo run --release -p earshot-runtime --features cuda --example quiet_voices -- . \
//!     --models <dir> --fixtures <dir> --backend cuda --out docs/quiet-voices/cuda.json
//! ```

#[allow(dead_code)] // shared with asr_eval; this example uses the AMI loaders and scoring
#[path = "../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use earshot_core::asr::{
    BackendKind, CancelToken, PieceConfig, TranscribeOptions, Transcriber, transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16};
use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;
use earshot_core::metrics::normalize::normalize_english;
use earshot_core::metrics::wer::{WerCounts, word_errors_normalized};
use earshot_core::pipeline::LiveTrack;
use earshot_onnx::silero::SileroVad;
use earshot_runtime::backends::{
    Accelerator, AnyTranscriber, EvidenceLoader, ModelDir, VerifiedWhisper, final_pass_asr,
    init_onnx, live_asr_factory, load_silero, onnx_library,
};
use earshot_runtime::live::{LiveConfig, LiveTranscription};
use earshot_runtime::model_check;
use earshot_runtime::{SystemClock, os_seeded_rng};
use serde_json::{Value, json};

/// The AMI dev meetings of the rule (sets `ami-dev-sdm` and `ami-dev-ihm`).
const MEETINGS: [&str; 5] = ["ES2003a", "ES2011a", "IS1008a", "TS3004a", "TS3006a"];
/// A hypothesis word matches a reference word only within this many seconds of its start.
const MATCH_WINDOW_S: f64 = 1.0;
/// Compressor envelope attack and release (seconds).
const ATTACK_S: f64 = 0.005;
const RELEASE_S: f64 = 0.1;
/// The compressor threshold lies this far below the reference level (dB).
const THRESHOLD_BELOW_DB: f64 = 25.0;
/// The reference level is this percentile of 10 ms frame RMS inside the UEM.
const REFERENCE_PERCENTILE: f64 = 0.95;
/// 10 ms at 16 kHz.
const FRAME: usize = 160;
/// Decision thresholds of the rule (points).
const WHOLE_GAIN_POINTS: f64 = 1.0;
const QUIET_GAIN_POINTS: f64 = 2.0;
const SEGMENTS_LOSS_POINTS: f64 = 0.5;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("quiet_voices: {msg}");
    std::process::exit(2);
}

fn rate() -> f64 {
    f64::from(PIPELINE_RATE.hz())
}

/// One condition of the rule.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Condition {
    Off,
    /// A fixed gain in dB, clamped to [-1, 1].
    Gain(f64),
    /// The rule's compressor with this ratio.
    Compress(f64),
}

const CONDITIONS: [(&str, Condition); 4] = [
    ("off", Condition::Off),
    ("gain12", Condition::Gain(12.0)),
    ("comp4", Condition::Compress(4.0)),
    ("comp10", Condition::Compress(10.0)),
];

/// `audio` under `condition` (the reference level read inside `uem`), quantised to 16-bit.
fn apply(condition: Condition, audio: &[f32], uem: Range<usize>) -> Vec<f32> {
    let out = match condition {
        Condition::Off => audio.to_vec(),
        Condition::Gain(db) => gain(audio, db),
        Condition::Compress(ratio) => compress(audio, ratio, reference_level(&audio[uem])),
    };
    quantise_pcm16(&out)
}

fn gain(x: &[f32], db: f64) -> Vec<f32> {
    let g = 10f64.powf(db / 20.0);
    x.iter()
        .map(|&v| (f64::from(v) * g).clamp(-1.0, 1.0) as f32)
        .collect()
}

fn db(power: f64) -> f64 {
    10.0 * (power + 1e-12).log10()
}

/// Mean power of `x` (0 for an empty slice).
fn power(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64
}

/// The rule's reference level: the [`REFERENCE_PERCENTILE`] of 10 ms frame RMS (dBFS).
fn reference_level(x: &[f32]) -> f64 {
    let mut frames: Vec<f64> = x.chunks(FRAME).map(|f| db(power(f))).collect();
    if frames.is_empty() {
        fail("no audio inside the UEM");
    }
    frames.sort_by(f64::total_cmp);
    let at = ((frames.len() - 1) as f64 * REFERENCE_PERCENTILE).round() as usize;
    frames[at]
}

/// The rule's feed-forward compressor: one-pole power envelope (attack [`ATTACK_S`],
/// release [`RELEASE_S`]), threshold [`THRESHOLD_BELOW_DB`] below `level_ref`, `ratio` above
/// it, make-up gain so a level of `level_ref` leaves at `level_ref`, clamped to [-1, 1].
fn compress(x: &[f32], ratio: f64, level_ref: f64) -> Vec<f32> {
    let threshold = level_ref - THRESHOLD_BELOW_DB;
    let slope = 1.0 - 1.0 / ratio;
    let makeup = (level_ref - threshold) * slope;
    let attack = (-1.0 / (ATTACK_S * rate())).exp();
    let release = (-1.0 / (RELEASE_S * rate())).exp();
    let mut env = 0.0f64;
    x.iter()
        .map(|&v| {
            let v = f64::from(v);
            let p = v * v;
            let k = if p > env { attack } else { release };
            env = k * env + (1.0 - k) * p;
            let over = db(env) - threshold;
            let reduction = if over > 0.0 { -over * slope } else { 0.0 };
            (v * 10f64.powf((reduction + makeup) / 20.0)).clamp(-1.0, 1.0) as f32
        })
        .collect()
}

/// Each speaker's level (dBFS) over the samples of their segments that no other speaker's
/// segment covers; `None` for a speaker with no such samples.
fn speaker_levels(audio: &[f32], segments: &[Vec<Range<usize>>]) -> Vec<Option<f64>> {
    let mut cover = vec![0u8; audio.len()];
    for ranges in segments {
        for r in ranges {
            for c in &mut cover[r.clone()] {
                *c = c.saturating_add(1);
            }
        }
    }
    segments
        .iter()
        .map(|ranges| {
            let (mut sum, mut n) = (0.0f64, 0usize);
            for r in ranges {
                for i in r.clone() {
                    if cover[i] == 1 {
                        let v = f64::from(audio[i]);
                        sum += v * v;
                        n += 1;
                    }
                }
            }
            (n > 0).then(|| db(sum / n as f64))
        })
        .collect()
}

/// `(time, word)` pairs as normalised tokens (Whisper's English normaliser, per word; a word
/// that normalises to several tokens keeps its time for each, one that normalises to nothing
/// is dropped).
fn tokens<'a>(words: impl Iterator<Item = (f64, &'a str)>) -> Vec<(f64, String)> {
    words
        .flat_map(|(t, w)| {
            normalize_english(w)
                .split_whitespace()
                .map(|s| (t, s.to_owned()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// How many reference tokens of each speaker are matched one to one by a hypothesis token
/// with the same text starting within `window` seconds, matching all speakers' tokens
/// together in time order (each hypothesis token used once), the earliest unused candidate
/// first. `reference` holds `(speaker, time, token)`; returns matched counts per speaker.
fn heard(
    reference: &[(usize, f64, String)],
    hypothesis: &[(f64, String)],
    window: f64,
    speakers: usize,
) -> Vec<usize> {
    let mut refs: Vec<&(usize, f64, String)> = reference.iter().collect();
    refs.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut hyp: Vec<&(f64, String)> = hypothesis.iter().collect();
    hyp.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut used = vec![false; hyp.len()];
    let mut lo = 0;
    let mut out = vec![0; speakers];
    for (speaker, t, text) in refs {
        while lo < hyp.len() && hyp[lo].0 < t - window {
            lo += 1;
        }
        let found = (lo..hyp.len())
            .take_while(|&j| hyp[j].0 <= t + window)
            .find(|&j| !used[j] && &hyp[j].1 == text);
        if let Some(j) = found {
            used[j] = true;
            out[*speaker] += 1;
        }
    }
    out
}

/// One condition's results.
#[derive(Debug, Clone, Default, PartialEq)]
struct Measures {
    segments: WerCounts,
    whole: WerCounts,
    quiet_heard: usize,
    quiet_words: usize,
    all_heard: usize,
    all_words: usize,
}

fn wer_points(c: WerCounts) -> f64 {
    c.wer().unwrap_or_else(|| fail("no reference words")) * 100.0
}

fn share_points(heard: usize, words: usize) -> f64 {
    if words == 0 {
        fail("no reference words for a speaker share");
    }
    heard as f64 / words as f64 * 100.0
}

/// The rule's decision for `cond` against `off`: (whole, quiet, segments, all three).
fn decide(off: &Measures, cond: &Measures) -> (bool, bool, bool, bool) {
    let whole = wer_points(off.whole) - wer_points(cond.whole) >= WHOLE_GAIN_POINTS;
    let quiet = share_points(cond.quiet_heard, cond.quiet_words)
        - share_points(off.quiet_heard, off.quiet_words)
        >= QUIET_GAIN_POINTS;
    let segments = wer_points(cond.segments) - wer_points(off.segments) <= SEGMENTS_LOSS_POINTS;
    (whole, quiet, segments, whole && quiet && segments)
}

fn counts_json(c: WerCounts) -> Value {
    json!({
        "wer_percent": c.wer().map(|w| w * 100.0),
        "substitutions": c.substitutions,
        "deletions": c.deletions,
        "insertions": c.insertions,
        "ref_words": c.ref_words,
    })
}

struct Args {
    backend: BackendKind,
    models: ModelDir,
    fixtures: PathBuf,
    out: PathBuf,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let _root = it
        .next()
        .unwrap_or_else(|| fail("usage: quiet_voices <repo-root> ..."));
    let (mut backend, mut models, mut fixtures, mut out) = (None, None, None, None);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| fail(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--backend" => {
                backend = Some(match value().as_str() {
                    "cpu" => BackendKind::Cpu,
                    "cuda" => BackendKind::Cuda,
                    other => fail(format!("unsupported backend {other}")),
                });
            }
            "--models" => models = Some(ModelDir::new(value())),
            "--fixtures" => fixtures = Some(PathBuf::from(value())),
            "--out" => out = Some(PathBuf::from(value())),
            other => fail(format!("unknown argument {other}")),
        }
    }
    Args {
        backend: backend.unwrap_or_else(|| fail("--backend is required")),
        models: models.unwrap_or_else(|| fail("--models is required")),
        fixtures: fixtures.unwrap_or_else(|| fail("--fixtures is required")),
        out: out.unwrap_or_else(|| fail("--out is required")),
    }
}

/// The production final pass (as `m2_prod`): transcriber, Silero, pieces and options.
struct Prod {
    name: &'static str,
    backend: BackendKind,
    transcriber: AnyTranscriber<VerifiedWhisper>,
    vad: SileroVad,
    pieces: PieceConfig,
    options: TranscribeOptions,
}

fn load(args: &Args) -> Prod {
    let name = model_check::candidates(args.backend, &[])
        .first()
        .copied()
        .unwrap_or_else(|| fail(format!("no English ladder for {:?}", args.backend)));
    let accel = Accelerator::of(args.backend)
        .unwrap_or_else(|| fail(format!("no accelerator for {:?}", args.backend)));
    let order = vec![accel];
    init_onnx(&args.models, onnx_library(name, &order))
        .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    // One live track for the options a stretch track hands the final pass.
    let mut factory = live_asr_factory(
        Arc::new(EvidenceLoader::for_live(2)),
        args.models.clone(),
        name.to_owned(),
        order.clone(),
        Accelerator::Cpu,
        Arc::new(SystemClock),
        os_seeded_rng(),
    );
    let track = factory(TrackId::Mic, EntityId::from_bytes([1; 16]))
        .unwrap_or_else(|e| fail(format!("live ASR: {e}")))
        .unwrap_or_else(|| fail("the live factory gave no transcription"));
    let LiveTranscription::Stretches(stretches) = LiveConfig::default().transcription else {
        fail("the default live mode is no longer speech stretches");
    };
    let live = LiveTrack::with_stretches(TrackId::Mic, track.asr, &stretches)
        .unwrap_or_else(|e| fail(format!("live stretches: {e:?}")));
    let options = live.final_pass().options;
    drop(live);
    let choice = final_pass_asr(&EvidenceLoader::default(), &args.models, name, &order)
        .unwrap_or_else(|e| fail(format!("final-pass model: {e}")));
    if choice.backend != args.backend || !choice.rejected.is_empty() {
        fail(format!(
            "final-pass model on {:?} (refused {:?})",
            choice.backend, choice.rejected
        ));
    }
    let vad = load_silero(&args.models, Accelerator::Cpu)
        .unwrap_or_else(|e| fail(format!("Silero: {e}")));
    eprintln!("quiet_voices: {name} on {:?}", args.backend);
    Prod {
        name,
        backend: args.backend,
        transcriber: choice.transcriber,
        vad,
        pieces: PieceConfig::default(),
        options,
    }
}

impl Prod {
    fn check(&self, kind: BackendKind) {
        if kind != self.backend || self.transcriber.backend_info().kind != self.backend {
            fail(format!("transcription ran on {kind:?}"));
        }
    }

    /// One batch call (the `ami_sdm` protocol): the kept words' text.
    fn batch_text(&mut self, audio: &[f32]) -> String {
        let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
        let t = self
            .transcriber
            .transcribe(&buffer, &TranscribeOptions::batch(), &CancelToken::new())
            .unwrap_or_else(|e| fail(format!("transcribe: {e}")));
        self.check(t.backend.kind);
        refs::hypothesis_text(&refs::kept_words(&t))
    }

    /// The final pass over `audio` (starting `offset` s into the meeting): kept words as
    /// `(meeting time s, text)`.
    fn whole(&mut self, audio: &[f32], offset: f64) -> Vec<(f64, String)> {
        let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
        let (t, _) = transcribe_in_pieces(
            &mut self.transcriber,
            &mut self.vad,
            &buffer,
            &self.options,
            &self.pieces,
            &CancelToken::new(),
        )
        .unwrap_or_else(|e| fail(format!("final pass: {e}")));
        self.check(t.backend.kind);
        refs::kept_words(&t)
            .into_iter()
            .map(|w| (offset + w.start.as_millis() as f64 / 1000.0, w.text))
            .collect()
    }
}

fn main() {
    let args = parse_args();
    let mut prod = load(&args);
    let set = refs::load_ami_sdm(&args.fixtures, &MEETINGS).unwrap_or_else(|e| fail(e));
    let mut results: Vec<(&str, Measures, Value)> = Vec::new();
    let mut levels_out = Vec::new();
    for (ci, (cname, condition)) in CONDITIONS.iter().enumerate() {
        let started = Instant::now();
        let mut m = Measures::default();
        let mut meetings = Vec::new();
        for meeting in MEETINGS {
            let speakers: Vec<&refs::AmiHeadset> =
                set.iter().filter(|h| h.meeting == meeting).collect();
            let first = speakers
                .first()
                .unwrap_or_else(|| fail(format!("{meeting}: no speakers")));
            let original = Arc::clone(&first.audio);
            let uem = refs::span_samples(first.uem, rate(), original.len());
            let audio: Arc<[f32]> = apply(*condition, &original, uem.clone()).into();
            // Levels from the `off` recording, the same for every condition.
            let ranges: Vec<Vec<Range<usize>>> =
                speakers.iter().map(|h| h.ranges.clone()).collect();
            let levels = speaker_levels(&original, &ranges);
            let quietest = levels
                .iter()
                .enumerate()
                .filter_map(|(i, l)| l.map(|l| (i, l)))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
                .unwrap_or_else(|| fail(format!("{meeting}: no speaker has solo audio")));
            // Segments: mis-hearing alone.
            let mut seg = WerCounts::default();
            for h in &speakers {
                let mut h = (*h).clone();
                h.audio = Arc::clone(&audio);
                let hyps: Vec<String> = h
                    .segment_audio()
                    .map(|a| prod.batch_text(a))
                    .collect::<Vec<_>>();
                seg += h.score(&hyps).unwrap_or_else(|e| fail(e)).total();
            }
            // Whole meeting: the production in-room path.
            let offset = uem.start as f64 / rate();
            let hyp_words = prod.whole(&audio[uem.clone()], offset);
            let all_words: Vec<Vec<refs::AmiWord>> =
                speakers.iter().map(|h| h.words.clone()).collect();
            let reference = refs::ami_reference(&all_words, first.uem.0, first.uem.1);
            let hyp_text = hyp_words
                .iter()
                .map(|(_, w)| w.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let whole = word_errors_normalized(&reference, &hyp_text);
            // Heard words per speaker.
            let mut ref_tokens = Vec::new();
            let mut totals = vec![0usize; speakers.len()];
            for (i, h) in speakers.iter().enumerate() {
                let toks = tokens(
                    h.words
                        .iter()
                        .filter(|w| w.start >= h.uem.0 && w.start < h.uem.1)
                        .map(|w| (w.start, w.text.as_str())),
                );
                totals[i] = toks.len();
                ref_tokens.extend(toks.into_iter().map(|(t, s)| (i, t, s)));
            }
            let hyp_tokens = tokens(hyp_words.iter().map(|(t, w)| (*t, w.as_str())));
            let matched = heard(&ref_tokens, &hyp_tokens, MATCH_WINDOW_S, speakers.len());
            m.segments += seg;
            m.whole += whole;
            m.quiet_heard += matched[quietest];
            m.quiet_words += totals[quietest];
            m.all_heard += matched.iter().sum::<usize>();
            m.all_words += totals.iter().sum::<usize>();
            let speaker_rows: Vec<Value> = speakers
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    json!({
                        "speaker": h.speaker,
                        "level_dbfs": levels[i],
                        "quietest": i == quietest,
                        "heard": matched[i],
                        "words": totals[i],
                    })
                })
                .collect();
            if ci == 0 {
                levels_out.push(json!({
                    "meeting": meeting,
                    "reference_level_dbfs": reference_level(&original[uem.clone()]),
                }));
            }
            eprintln!(
                "quiet_voices: {cname} {meeting}: segments {:.2}%, whole {:.2}%, quietest \
                 heard {}/{}",
                wer_points(seg),
                wer_points(whole),
                matched[quietest],
                totals[quietest]
            );
            meetings.push(json!({
                "meeting": meeting,
                "segments": counts_json(seg),
                "whole": counts_json(whole),
                "speakers": speaker_rows,
            }));
        }
        let row = json!({
            "condition": cname,
            "segments": counts_json(m.segments),
            "whole": counts_json(m.whole),
            "heard_quiet": { "heard": m.quiet_heard, "words": m.quiet_words,
                             "percent": share_points(m.quiet_heard, m.quiet_words) },
            "heard_all": { "heard": m.all_heard, "words": m.all_words,
                           "percent": share_points(m.all_heard, m.all_words) },
            "meetings": meetings,
            "seconds": started.elapsed().as_secs_f64(),
        });
        results.push((cname, m, row));
    }
    let off = results[0].1.clone();
    let mut conditions = Vec::new();
    for (name, m, mut row) in results {
        if name != "off" {
            let (whole, quiet, segments, helps) = decide(&off, &m);
            row["decision"] = json!({
                "whole_at_least_1_point_lower": whole,
                "heard_quiet_at_least_2_points_higher": quiet,
                "segments_at_most_0_5_point_higher": segments,
                "helps": helps,
            });
            eprintln!("quiet_voices: {name} helps: {helps}");
        }
        conditions.push(row);
    }
    let report = json!({
        "task": "T12.89",
        "rule": "docs/quiet-voices.md",
        "model": prod.name,
        "backend": format!("{:?}", prod.backend),
        "meetings": MEETINGS,
        "settings": {
            "match_window_s": MATCH_WINDOW_S,
            "attack_s": ATTACK_S,
            "release_s": RELEASE_S,
            "threshold_below_reference_db": THRESHOLD_BELOW_DB,
            "reference_percentile": REFERENCE_PERCENTILE,
        },
        "reference_levels": levels_out,
        "conditions": conditions,
    });
    if let Some(dir) = args.out.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())));
    }
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    std::fs::write(&args.out, text + "\n")
        .unwrap_or_else(|e| fail(format!("{}: {e}", args.out.display())));
    eprintln!("quiet_voices: wrote {}", args.out.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16000.0).sin())
            .collect()
    }

    #[test]
    fn quiet_voices_gain_scales_and_clamps() {
        let y = gain(&[0.01, -0.5, 0.0], 12.0);
        assert!((y[0] - 0.039_810_7).abs() < 1e-6);
        assert_eq!(y[1], -1.0);
        assert_eq!(y[2], 0.0);
    }

    #[test]
    fn quiet_voices_compressor_keeps_the_reference_level_and_lifts_quieter_speech() {
        // A loud tone at the reference level, then the same tone 20 dB quieter.
        let loud = tone(0.5, 16000);
        let quiet = tone(0.05, 16000);
        let x: Vec<f32> = loud.iter().chain(&quiet).copied().collect();
        let level = db(power(&loud));
        let y = compress(&x, 4.0, level);
        let tail = |s: &[f32]| db(power(&s[8000..]));
        // The fast-attack power envelope sits above a tone's mean power (about 2.4 dB for a
        // sine), so a tone at the reference level leaves somewhat below it, never above.
        let l = tail(&y[..16000]);
        assert!(l <= level && l > level - 2.5, "{l} vs {level}");
        // Both tones lie above the threshold, so the 20 dB gap shrinks by the ratio to 5 dB.
        let gap = l - tail(&y[16000..]);
        assert!((gap - 5.0).abs() < 0.5, "{gap}");
        // Off changes nothing but the 16-bit quantisation.
        let off = apply(Condition::Off, &x, 0..x.len());
        assert_eq!(off, quantise_pcm16(&x));
    }

    #[test]
    fn quiet_voices_compressor_raises_audio_below_the_threshold_by_the_makeup() {
        let x = tone(0.001, 16000);
        let y = compress(&x, 10.0, -10.0); // threshold -35 dB; the tone is about -63 dB
        let lift = db(power(&y[8000..])) - db(power(&x[8000..]));
        assert!((lift - 22.5).abs() < 0.1, "{lift}");
    }

    #[test]
    fn quiet_voices_reference_level_is_the_95th_percentile_frame() {
        let mut x = vec![0.0f32; 160 * 100];
        for f in 94..100 {
            for v in &mut x[f * 160..(f + 1) * 160] {
                *v = 0.5;
            }
        }
        // 6 loud frames of 100: the 95th percentile (index 94 of the sorted frames) is the
        // first loud one; with 5 it is the last silent one.
        assert!((reference_level(&x) - db(0.25)).abs() < 1e-9);
        x[160 * 94..160 * 95].fill(0.0);
        assert!(reference_level(&x) < -100.0);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // one segment per speaker is the case here
    fn quiet_voices_speaker_levels_use_solo_time_only() {
        let audio: Vec<f32> = (0..300).map(|i| if i < 100 { 0.1 } else { 0.5 }).collect();
        // Speaker 0 alone on 0..100, both on 100..200, speaker 1 alone on 200..300.
        let levels = speaker_levels(&audio, &[vec![0..200], vec![100..300]]);
        // 0.1 is not exact in f32: compare at f32 precision.
        assert!((levels[0].unwrap() - db(f64::from(0.1f32).powi(2))).abs() < 1e-9);
        assert!((levels[1].unwrap() - db(0.25)).abs() < 1e-9);
        assert_eq!(
            speaker_levels(&audio, &[vec![0..100], vec![0..100]]),
            vec![None, None]
        );
    }

    #[test]
    fn quiet_voices_heard_matches_one_to_one_within_the_window() {
        let r = |s, t, w: &str| (s, t, w.to_owned());
        let h = |t, w: &str| (t, w.to_owned());
        let reference = vec![
            r(0, 1.0, "budget"),
            r(1, 1.2, "budget"),
            r(0, 5.0, "remote"),
            r(1, 9.0, "button"),
        ];
        let hypothesis = vec![h(1.1, "budget"), h(6.5, "remote"), h(9.4, "button")];
        // One hypothesis "budget" serves one reference word; "remote" is 1.5 s off.
        assert_eq!(heard(&reference, &hypothesis, 1.0, 2), vec![1, 1]);
        assert_eq!(heard(&reference, &hypothesis, 2.0, 2), vec![2, 1]);
    }

    #[test]
    fn quiet_voices_tokens_normalise_each_word() {
        let t = tokens([(1.0, "Don't"), (2.0, "um"), (3.0, "Budget,")].into_iter());
        assert_eq!(
            t,
            vec![
                (1.0, "do".to_owned()),
                (1.0, "not".to_owned()),
                (3.0, "budget".to_owned())
            ]
        );
    }

    #[test]
    fn quiet_voices_decision_needs_all_three() {
        let c = |sub, refs| WerCounts {
            substitutions: sub,
            deletions: 0,
            insertions: 0,
            ref_words: refs,
        };
        let off = Measures {
            segments: c(30, 100),
            whole: c(40, 100),
            quiet_heard: 50,
            quiet_words: 100,
            ..Measures::default()
        };
        let good = Measures {
            segments: c(30, 100),
            whole: c(39, 100),
            quiet_heard: 52,
            quiet_words: 100,
            ..Measures::default()
        };
        assert_eq!(decide(&off, &good), (true, true, true, true));
        let worse_segments = Measures {
            segments: c(31, 100),
            ..good.clone()
        };
        assert_eq!(decide(&off, &worse_segments), (true, true, false, false));
        let small = Measures {
            whole: c(40, 100),
            ..good
        };
        assert!(!decide(&off, &small).3);
    }
}
