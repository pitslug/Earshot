//! Shorter speech stretches for Parakeet (T12.24, evaluation only): AMI dev meetings decoded in
//! pieces at several `PieceConfig::max_piece` values. Counts and times only, never text.
//!
//! ```text
//! cargo run --release -p earshot-runtime [--features qnn] --example stretch_eval -- <repo-root>
//!     [--backend cpu|qnn] [--models <dir>] [--fixtures <dir>] [--max-piece 30,20,15,10]
//!     [--meetings ES2003a,..] [--out <file.json>]
//! ```
//!
//! The rule, data and pass points are docs/live-previews.md ("Shorter speech stretches for
//! Parakeet"), committed before the first run. Setup: Parakeet TDT 0.6B v2 through the
//! final-pass loader (`backends::final_pass_asr`) on `--backend` alone, Silero on the CPU
//! (`backends::load_silero`), `asr::transcribe_in_pieces` with `PieceConfig::default()` and only
//! `max_piece` changed, on the whole Mix-Headset recording quantised to 16-bit PCM (as the final
//! pass reads the meeting's FLAC), with the options a live stretch track hands the final pass
//! (`TranscribeOptions::live()`, greedy, D20). The hypothesis is the words production keeps
//! (`refs::kept_words`). The reference is every participant's words over the whole recording in
//! start-time order (`refs::ami_reference`), so this WER compares `max_piece` values with each
//! other and is not `asr.wer_ami_ihm`.
//!
//! Cuts ([`cuts`]): boundaries between consecutive planned pieces (`asr::plan_pieces` over the
//! same VAD probabilities the decode used) with no gap between them. Around each cut the words
//! starting within [`CUT_WINDOW_MS`] either side are scored as one more `WerCounts`, and a word
//! is doubled ([`doubled_at`]) when the last kept word of the earlier piece and the first of the
//! later piece are equal after Whisper's English normalisation and both lie within
//! [`DOUBLED_WITHIN_MS`] of the cut. Decode time is the wall time of `transcribe_in_pieces`
//! (VAD included, as in production) after one untimed warm-up decode.
//!
//! Dev data only (fixture set `ami-dev`; words from the annotations archive, set `ami-sum`).

#[allow(dead_code)] // shared with asr_eval; this example uses the AMI word helpers
#[path = "../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use earshot_core::asr::{
    BackendKind, CancelToken, PieceConfig, Span, TranscribeOptions, Transcriber, Vad, Word,
    frame_probabilities, plan_pieces, speech_regions, transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16, read_wav};
use earshot_core::metrics::normalize::normalize_english;
use earshot_core::metrics::wer::{WerCounts, word_errors_normalized};
use earshot_runtime::backends::{
    Accelerator, AsrModel, EvidenceLoader, ModelDir, asr_model, final_pass_asr, init_onnx,
    load_silero, onnx_library,
};
use serde_json::{Value, json};

/// The model this task measures (T12.24: Parakeet 0.6B v2 only).
const MODEL: &str = "parakeet-tdt-0.6b-v2";
/// The first meeting of each AMI dev series (fixture set `ami-dev`).
const AMI_DEV: [&str; 5] = ["ES2003a", "ES2011a", "IS1008a", "TS3004a", "TS3006a"];
/// Letters a speaker's words file can have.
const AMI_LETTERS: [&str; 5] = ["A", "B", "C", "D", "E"];
/// The values of the rule, today's first.
const DEFAULT_VALUES: [u64; 4] = [30, 20, 15, 10];
/// Words starting this close to a cut, either side, are scored in its window (the rule: 2 s).
const CUT_WINDOW_MS: u64 = 2_000;
/// A doubled word lies this close to the cut on both sides (the rule: 1 s).
const DOUBLED_WITHIN_MS: u64 = 1_000;
/// Length of the untimed warm-up decode.
const WARM_UP_S: u64 = 30;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("stretch_eval: {msg}");
    std::process::exit(2);
}

struct Args {
    backend: BackendKind,
    models: Option<PathBuf>,
    fixtures: Option<PathBuf>,
    values: Vec<u64>,
    meetings: Vec<String>,
    out: Option<PathBuf>,
}

fn parse_from(mut raw: impl Iterator<Item = String>) -> Result<Args, String> {
    raw.next()
        .ok_or_else(|| "usage: stretch_eval <repo-root> [options]".to_owned())?;
    let mut args = Args {
        backend: BackendKind::Cpu,
        models: None,
        fixtures: None,
        values: DEFAULT_VALUES.to_vec(),
        meetings: AMI_DEV.iter().map(|m| (*m).to_owned()).collect(),
        out: None,
    };
    while let Some(flag) = raw.next() {
        let mut value = || raw.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--backend" => {
                args.backend = match value()?.as_str() {
                    "cpu" => BackendKind::Cpu,
                    "qnn" => BackendKind::Qnn,
                    other => return Err(format!("unsupported backend {other}")),
                }
            }
            "--models" => args.models = Some(PathBuf::from(value()?)),
            "--fixtures" => args.fixtures = Some(PathBuf::from(value()?)),
            "--max-piece" => {
                args.values = value()?
                    .split(',')
                    .map(|s| {
                        s.trim()
                            .parse::<u64>()
                            .map_err(|e| format!("--max-piece: {e}"))
                    })
                    .collect::<Result<_, _>>()?;
                for &s in &args.values {
                    pieces_with_max(s)?;
                }
            }
            "--meetings" => {
                args.meetings = value()?.split(',').map(|s| s.trim().to_owned()).collect();
                if let Some(m) = args
                    .meetings
                    .iter()
                    .find(|m| !AMI_DEV.contains(&m.as_str()))
                {
                    return Err(format!("{m} is not an AMI dev meeting of set ami-dev"));
                }
            }
            "--out" => args.out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.values.is_empty() || args.meetings.is_empty() {
        return Err("no values or no meetings".to_owned());
    }
    Ok(args)
}

/// `PieceConfig::default()` with only `max_piece` changed.
///
/// # Errors
/// A value the config refuses (1-30 s).
fn pieces_with_max(seconds: u64) -> Result<PieceConfig, String> {
    let pieces = PieceConfig {
        max_piece: Duration::from_secs(seconds),
        ..PieceConfig::default()
    };
    pieces
        .validate()
        .map_err(|e| format!("--max-piece {seconds}: {e}"))?;
    Ok(pieces)
}

/// The cuts of a piece plan (module docs): the sample where a piece ends and the next one
/// starts with no gap between them.
fn cuts(pieces: &[Span]) -> Vec<usize> {
    pieces
        .windows(2)
        .filter(|w| w[0].end == w[1].start)
        .map(|w| w[0].end)
        .collect()
}

fn ms_of_sample(sample: usize) -> u64 {
    (sample as u64 * 1000) / u64::from(PIPELINE_RATE.hz())
}

/// Texts of `words` whose start lies in `[from, to)` ms, in order.
fn texts_between<'a>(words: impl Iterator<Item = (u64, &'a str)>, from: u64, to: u64) -> String {
    words
        .filter(|(s, _)| *s >= from && *s < to)
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a word is doubled at the cut `cut_ms` (module docs): the last hypothesis word
/// starting before the cut and the first starting at or after it are equal after
/// normalisation, both within [`DOUBLED_WITHIN_MS`] of the cut. `words` are in start order.
fn doubled_at(words: &[Word], cut_ms: u64) -> bool {
    let split = words.partition_point(|w| w.start.as_millis() < cut_ms);
    let (Some(before), Some(after)) = (split.checked_sub(1).map(|i| &words[i]), words.get(split))
    else {
        return false;
    };
    let near = |t: u64| t.abs_diff(cut_ms) <= DOUBLED_WITHIN_MS;
    let (a, b) = (
        normalize_english(&before.text),
        normalize_english(&after.text),
    );
    near(before.end.as_millis().max(before.start.as_millis()))
        && near(after.start.as_millis())
        && !a.is_empty()
        && a == b
}

/// The words of one meeting's annotation files, every participant, as `refs::AmiWord`s.
fn meeting_words(annotations: &Path, meeting: &str) -> Vec<Vec<refs::AmiWord>> {
    let mut speakers = Vec::new();
    for letter in AMI_LETTERS {
        let p = annotations
            .join("words")
            .join(format!("{meeting}.{letter}.words.xml"));
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let xml = refs::latin1(&bytes);
        speakers.push(
            refs::parse_ami_words(&xml).unwrap_or_else(|e| fail(format!("{}: {e}", p.display()))),
        );
    }
    if speakers.is_empty() {
        fail(format!(
            "{meeting}: no words files; run `cargo xtask fetch-fixtures --set ami-sum`"
        ));
    }
    speakers
}

fn read_mono_wav(path: &Path) -> Vec<f32> {
    let file = File::open(path).unwrap_or_else(|e| {
        fail(format!(
            "{}: {e}; run `cargo xtask fetch-fixtures --set ami-dev`",
            path.display()
        ))
    });
    let wav =
        read_wav(BufReader::new(file)).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    if wav.rate() != PIPELINE_RATE || wav.channels() != 1 {
        fail(format!("{}: expected 16 kHz mono", path.display()));
    }
    wav.into_samples()
}

fn env_dir(var: &str, under_local: &[&str]) -> PathBuf {
    if let Some(d) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    let mut p =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| fail("LOCALAPPDATA")));
    for part in under_local {
        p.push(part);
    }
    p
}

/// Sums of one value's results.
#[derive(Default)]
struct Totals {
    wer: WerCounts,
    cut_wer: WerCounts,
    cuts: usize,
    doubled: usize,
    pieces: usize,
    audio_s: f64,
    decode_s: f64,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.wer += other.wer;
        self.cut_wer += other.cut_wer;
        self.cuts += other.cuts;
        self.doubled += other.doubled;
        self.pieces += other.pieces;
        self.audio_s += other.audio_s;
        self.decode_s += other.decode_s;
    }

    fn json(&self) -> Value {
        let pct = |c: &WerCounts| c.wer().map(|w| w * 100.0);
        json!({
            "wer_percent": pct(&self.wer),
            "wer": {"ref_words": self.wer.ref_words, "substitutions": self.wer.substitutions,
                    "deletions": self.wer.deletions, "insertions": self.wer.insertions},
            "cuts": self.cuts,
            "cut_window_wer_percent": pct(&self.cut_wer),
            "cut_window": {"ref_words": self.cut_wer.ref_words,
                           "substitutions": self.cut_wer.substitutions,
                           "deletions": self.cut_wer.deletions,
                           "insertions": self.cut_wer.insertions},
            "doubled": self.doubled,
            "doubled_per_10_min": (self.audio_s > 0.0).then(|| self.doubled as f64 * 600.0 / self.audio_s),
            "pieces": self.pieces,
            "audio_s": self.audio_s,
            "decode_s": self.decode_s,
            "rtf": (self.audio_s > 0.0).then(|| self.decode_s / self.audio_s),
        })
    }
}

fn main() {
    // NPU compiles in a helper, as in the app (T12.12b).
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(code) = earshot_runtime::npu_compiled::child_main(&raw) {
        std::process::exit(code);
    }
    earshot_runtime::npu_compiled::use_this_exe();
    let args = parse_from(std::env::args().skip(1)).unwrap_or_else(|e| fail(e));
    let models = ModelDir::new(
        args.models
            .clone()
            .unwrap_or_else(|| env_dir("EARSHOT_MODELS", &["Earshot", "models"])),
    );
    let fixtures = args
        .fixtures
        .clone()
        .unwrap_or_else(|| env_dir("EARSHOT_FIXTURES", &["earshot-dev", "fixtures"]));

    match asr_model(MODEL).unwrap_or_else(|e| fail(e)) {
        AsrModel::Parakeet(_) => {}
        AsrModel::Whisper(_) => fail(format!("{MODEL} is not a Parakeet model")),
    }
    let accel = Accelerator::of(args.backend)
        .unwrap_or_else(|| fail(format!("no accelerator for {:?}", args.backend)));
    let order = vec![accel];
    init_onnx(&models, onnx_library(MODEL, &order))
        .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let started = Instant::now();
    let choice = final_pass_asr(&EvidenceLoader::default(), &models, MODEL, &order)
        .unwrap_or_else(|e| fail(format!("final-pass model: {e}")));
    if choice.backend != args.backend || !choice.rejected.is_empty() {
        fail(format!(
            "final-pass model on {:?} (refused {:?})",
            choice.backend, choice.rejected
        ));
    }
    let mut asr = choice.transcriber;
    let mut vad =
        load_silero(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("Silero: {e}")));
    let load_s = started.elapsed().as_secs_f64();
    let backend_info = asr.backend_info();
    let evidence = asr.backend_evidence();
    eprintln!(
        "stretch_eval: {MODEL} on {:?} loaded in {load_s:.1} s",
        backend_info.kind
    );
    let options = TranscribeOptions::live();
    let cancel = CancelToken::new();
    let annotations = fixtures.join("ami").join("annotations");

    let mut per_meeting = Vec::new();
    let mut totals: Vec<Totals> = args.values.iter().map(|_| Totals::default()).collect();
    let mut warmed = false;
    for meeting in &args.meetings {
        let audio = quantise_pcm16(&read_mono_wav(
            &fixtures
                .join("ami")
                .join("audio")
                .join(format!("{meeting}.Mix-Headset.wav")),
        ));
        let speakers = meeting_words(&annotations, meeting);
        let reference = refs::ami_reference(&speakers, 0.0, f64::INFINITY);
        let ref_words: Vec<(u64, &str)> = {
            let mut all: Vec<&refs::AmiWord> = speakers.iter().flatten().collect();
            all.sort_by(|a, b| a.start.total_cmp(&b.start));
            all.iter()
                .map(|w| ((w.start * 1000.0).round() as u64, w.text.as_str()))
                .collect()
        };
        let audio_s = audio.len() as f64 / f64::from(PIPELINE_RATE.hz());
        let buffer = MonoBuffer::new(PIPELINE_RATE, audio.clone());
        if !warmed {
            let n = PIPELINE_RATE
                .frames_for(Duration::from_secs(WARM_UP_S))
                .min(audio.len());
            let warm = MonoBuffer::new(PIPELINE_RATE, audio[..n].to_vec());
            transcribe_in_pieces(
                &mut asr,
                &mut vad,
                &warm,
                &options,
                &PieceConfig::default(),
                &cancel,
            )
            .unwrap_or_else(|e| fail(format!("warm-up: {e}")));
            warmed = true;
        }
        let frame = vad.frame_len();
        let probs = frame_probabilities(&mut vad, &audio, &cancel)
            .unwrap_or_else(|e| fail(format!("VAD: {e}")));
        let mut values = Vec::new();
        for (i, &s) in args.values.iter().enumerate() {
            let config = pieces_with_max(s).unwrap_or_else(|e| fail(e));
            let regions = speech_regions(&probs, frame, audio.len(), &config);
            let plan = plan_pieces(&regions, &probs, frame, &config);
            let at = cuts(&plan);
            let t0 = Instant::now();
            let (t, report) =
                transcribe_in_pieces(&mut asr, &mut vad, &buffer, &options, &config, &cancel)
                    .unwrap_or_else(|e| fail(format!("{meeting} at {s} s: {e}")));
            let decode_s = t0.elapsed().as_secs_f64();
            if t.backend.kind != args.backend {
                fail(format!("{meeting}: decoded on {:?}", t.backend.kind));
            }
            if report.pieces != plan.len() {
                fail(format!(
                    "{meeting} at {s} s: decoded {} pieces, planned {}",
                    report.pieces,
                    plan.len()
                ));
            }
            let words = refs::kept_words(&t);
            let hyp = refs::hypothesis_text(&words);
            let wer = word_errors_normalized(&reference, &hyp);
            let mut cut_wer = WerCounts::default();
            let mut doubled = 0;
            for &c in &at {
                let ms = ms_of_sample(c);
                let (from, to) = (ms.saturating_sub(CUT_WINDOW_MS), ms + CUT_WINDOW_MS);
                let r = texts_between(ref_words.iter().copied(), from, to);
                let h = texts_between(
                    words.iter().map(|w| (w.start.as_millis(), w.text.as_str())),
                    from,
                    to,
                );
                cut_wer += word_errors_normalized(&r, &h);
                if doubled_at(&words, ms) {
                    doubled += 1;
                }
            }
            let here = Totals {
                wer,
                cut_wer,
                cuts: at.len(),
                doubled,
                pieces: plan.len(),
                audio_s,
                decode_s,
            };
            totals[i].add(&here);
            let mut j = here.json();
            j["max_piece_s"] = json!(s);
            j["retried"] = json!(report.retried);
            j["uncovered"] = json!(report.uncovered);
            j["dropped_pieces"] = json!(report.dropped.len());
            eprintln!(
                "stretch_eval: {meeting} ({audio_s:.0} s) max_piece {s} s: WER {:.2}% ({} pieces, {} \
                 cuts, {} doubled), rtf {:.4}",
                wer.wer().unwrap_or(0.0) * 100.0,
                plan.len(),
                at.len(),
                doubled,
                decode_s / audio_s
            );
            values.push(j);
        }
        per_meeting.push(json!({"meeting": meeting, "audio_s": audio_s, "values": values}));
    }
    let pooled: Vec<Value> = args
        .values
        .iter()
        .zip(&totals)
        .map(|(s, t)| {
            let mut j = t.json();
            j["max_piece_s"] = json!(s);
            j
        })
        .collect();
    let report = json!({
        "task": "T12.24",
        "model": MODEL,
        "requested_backend": format!("{:?}", args.backend),
        "backend_info": backend_info,
        "evidence": evidence,
        "load_s": load_s,
        "options": options,
        "base_pieces": format!("{:?}", PieceConfig::default()),
        "set": "AMI dev Mix-Headset, whole recordings, every participant's words (refs::ami_reference)",
        "cut_window_ms": CUT_WINDOW_MS,
        "doubled_within_ms": DOUBLED_WITHIN_MS,
        "pooled": pooled,
        "per_meeting": per_meeting,
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    println!("{text}");
    if let Some(out) = &args.out {
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| fail(e));
        }
        std::fs::write(out, &text).unwrap_or_else(|e| fail(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::asr::MeetingTime;

    fn word(text: &str, start: u64, end: u64) -> Word {
        Word {
            text: text.to_owned(),
            start: MeetingTime::from_millis(start),
            end: MeetingTime::from_millis(end),
            probability: 1.0,
        }
    }

    #[test]
    fn stretch_eval_cuts_are_only_boundaries_without_a_gap() {
        let p = [
            Span { start: 0, end: 100 },
            Span {
                start: 100,
                end: 200,
            },
            Span {
                start: 250,
                end: 300,
            },
            Span {
                start: 300,
                end: 400,
            },
        ];
        assert_eq!(cuts(&p), vec![100, 300]);
        assert!(cuts(&[]).is_empty());
        assert!(cuts(&p[..1]).is_empty());
    }

    #[test]
    fn stretch_eval_a_word_is_doubled_only_when_equal_and_near_the_cut_on_both_sides() {
        let w = [
            word("the", 9_000, 9_200),
            word("Budget.", 9_600, 9_900),
            word("budget", 10_100, 10_400),
            word("is", 10_500, 10_600),
        ];
        assert!(doubled_at(&w, 10_000));
        // Different words.
        let x = [word("budget", 9_600, 9_900), word("plan", 10_100, 10_400)];
        assert!(!doubled_at(&x, 10_000));
        // Too far from the cut on one side.
        let y = [word("budget", 7_000, 7_400), word("budget", 10_100, 10_400)];
        assert!(!doubled_at(&y, 10_000));
        // Nothing on one side.
        assert!(!doubled_at(&w[..2], 10_000));
        assert!(!doubled_at(&[], 10_000));
    }

    #[test]
    fn stretch_eval_values_change_only_max_piece_and_refuse_what_the_config_refuses() {
        let p = pieces_with_max(15).unwrap();
        assert_eq!(
            PieceConfig {
                max_piece: PieceConfig::default().max_piece,
                ..p
            },
            PieceConfig::default()
        );
        assert_eq!(pieces_with_max(30).unwrap(), PieceConfig::default());
        assert!(pieces_with_max(0).is_err());
        assert!(pieces_with_max(31).is_err());
    }

    #[test]
    fn stretch_eval_parses_values_and_refuses_meetings_outside_the_dev_set() {
        let parse = |a: &[&str]| {
            parse_from(
                std::iter::once("repo")
                    .chain(a.iter().copied())
                    .map(str::to_owned),
            )
        };
        let a = parse(&["--max-piece", "30,10", "--backend", "qnn"]).unwrap();
        assert_eq!(a.values, vec![30, 10]);
        assert_eq!(a.backend, BackendKind::Qnn);
        assert_eq!(parse(&[]).unwrap().values, DEFAULT_VALUES.to_vec());
        assert!(parse(&["--meetings", "ES2004a"]).is_err());
        assert!(parse(&["--max-piece", "40"]).is_err());
    }

    #[test]
    fn stretch_eval_window_text_keeps_words_starting_inside_it() {
        let w = [(500u64, "a"), (1_000, "b"), (2_999, "c"), (3_000, "d")];
        assert_eq!(texts_between(w.iter().copied(), 1_000, 3_000), "b c");
    }
}
