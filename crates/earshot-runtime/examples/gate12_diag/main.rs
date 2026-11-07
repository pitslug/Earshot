//! Where near-end words are lost in talk-over on a GATE-12 raw recording (T6.23b).
//!
//! ```text
//! cargo run --release -p earshot-runtime --example gate12_diag -- <repo-root> \
//!     --dir <raw recording dir> --models <dir> [--backend cpu|cuda] [--model <name>] \
//!     [--out <file.json>] [--stored <replay data dir>]
//! ```
//!
//! Counts only: no transcript text is printed or written (AGENTS.md privacy rules). The
//! script lines are the committed near-end script of gates/GATE-12.md, so per-line counts
//! and the names of the script's own words may appear.
//!
//! The recording is replayed through `earshot_core::pipeline` with the real device
//! timestamps (`ReplayBuilder::from_raw_timeline`) and the live session's pipeline settings
//! (`LiveConfig::default().pipeline`), which gives the cleaned mic as the runtime stores it
//! (`quantise_pcm16`, as the FLAC writer). A second replay with `stats_interval` of one
//! canceller block (8 ms) reads the canceller's status per interval; its cleaned mic must
//! equal the first bit for bit (the stats period does not change processing), else the
//! status counts are reported as not representative.
//!
//! Sources:
//! - `raw`: mic.wav resampled to 16 kHz (`audio::resample_all`), quantised the same way;
//! - `cleaned`: the pipeline's cleaned mic.
//!
//! Decodes, each with the production final-pass model (`backends::final_pass_asr`), the
//! stretch track's options (`TranscribeOptions::live()`):
//! - `pieces`: `transcribe_in_pieces` over the whole track with Silero and
//!   `PieceConfig::default()` (the final pass; by D20 equal to the live stretches), words
//!   counted by start time inside each window;
//! - `direct`: plain `Transcriber::transcribe` calls over the window in 30 s chunks (one
//!   Parakeet window, no VAD), on two grids offset by 15 s; per line the better grid counts.
//!
//! Per script line, the words found are the longest in-order match (LCS) of the line's
//! normalised tokens against all of the window's words, each line on its own. It is an
//! upper bound on the words recognised from that line: a line read twice can match some
//! words in one reading and others in the other, and so exceed either reading
//! ([`LINE_COUNT_RULE`], written into the report).
//!
//! The "cleaned audio, raw VAD" variant replays the raw mic's VAD probabilities over the
//! cleaned mic; past the last raw frame it reads silence, so the report records both
//! lengths (`lengths`) and the cleaned audio a raw VAD frame does not cover.
//!
//! Also reported per 5 s bin of the first window: raw and cleaned mic level and their
//! difference, reference level, VAD speech seconds of each source, words of each decode, and
//! the canceller status counts (lock, double-talk calls, vetoed calls, bypass, converged,
//! suppression gain below 0.5 and mean gain).

mod stored;

#[path = "../../../earshot-asr-whisper/examples/asr_eval/gate12_private.rs"]
mod gate12_private;
#[allow(dead_code)] // shared with asr_eval; only `kept_words` is used here
#[path = "../../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::fs::File;
use std::io::BufReader;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use earshot_core::asr::{
    AsrError, BackendKind, CancelToken, MeetingTime, PieceConfig, TranscribeOptions, Transcriber,
    Vad, Word, frame_probabilities, normalize_token, speech_regions, transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16, read_wav, resample_all};
use earshot_core::capture::{
    AudioSource, RawTimeline, ReplayBuilder, SourceStatus, TrackBuffer, TrackId,
};
use earshot_core::pipeline::{AecStats, Pipeline, PipelineConfig, PipelineEvent};
use earshot_onnx::silero::SileroVad;
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, final_pass_asr, init_onnx, load_silero, onnx_library,
};
use earshot_runtime::echo::{EchoLock, text_echo_mask};
use earshot_runtime::live::LiveConfig;
use earshot_runtime::live::echo::LiveEchoFilter;
use serde_json::{Value, json};

/// The production default English model on the desktop (gates/GATE-12.md, scoring).
const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v2";
/// The windows, script lines, key words and residue stretches read off the recordings, kept
/// outside the repo (T9.1kb); set once at the start of `main`.
static PRIVATE: OnceLock<gate12_private::Gate12Private> = OnceLock::new();

fn private() -> &'static gate12_private::Gate12Private {
    PRIVATE
        .get()
        .expect("main reads the private settings first")
}

/// Bin width of the per-bin table (ms).
const BIN_MS: u64 = 5_000;
/// One Parakeet window (`earshot_onnx::parakeet::MAX_CHUNK`).
const DIRECT_CHUNK_MS: u64 = 30_000;
/// The second direct grid starts this much later.
const DIRECT_OFFSET_MS: u64 = 15_000;
/// Canceller status period of the diagnostic replay: one block (128 samples at 16 kHz).
const STATUS_INTERVAL: Duration = Duration::from_millis(8);

/// How every per-line count in the report is computed (module docs).
const LINE_COUNT_RULE: &str = "upper bound: LCS of the line's tokens against all of the \
    window's word tokens; a line read twice can exceed either reading";

/// A VAD that replays probabilities computed on other audio (the raw mic), frame by frame:
/// pieces planned on the raw mic's speech, decoded from the cleaned mic. Past the last
/// recorded probability it returns 0.0 (silence); `lengths` in the report shows how much
/// audio that covers.
struct ReplayedVad {
    probs: Vec<f32>,
    frame: usize,
    at: usize,
}

impl Vad for ReplayedVad {
    fn id(&self) -> String {
        "replayed raw-mic probabilities".to_owned()
    }
    fn frame_len(&self) -> usize {
        self.frame
    }
    fn speech_probability(&mut self, _frame: &[f32]) -> Result<f32, AsrError> {
        let p = self.probs.get(self.at).copied().unwrap_or(0.0);
        self.at += 1;
        Ok(p)
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("gate12_diag: {msg}");
    std::process::exit(1);
}

struct Args {
    root: PathBuf,
    dir: PathBuf,
    models: ModelDir,
    backend: BackendKind,
    model: String,
    out: Option<PathBuf>,
    /// `--stored <data-dir>`: score the stored final transcript of a replay (`stored`).
    stored: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let root = PathBuf::from(
        it.next()
            .unwrap_or_else(|| fail("usage: gate12_diag <repo-root> --dir <dir> --models <dir>")),
    );
    let (mut dir, mut models, mut out, mut stored) = (None, None, None, None);
    let mut backend = BackendKind::Cpu;
    let mut model = DEFAULT_MODEL.to_owned();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| fail(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--dir" => dir = Some(PathBuf::from(value())),
            "--models" => models = Some(ModelDir::new(value())),
            "--out" => out = Some(PathBuf::from(value())),
            "--stored" => stored = Some(PathBuf::from(value())),
            "--model" => model = value(),
            "--backend" => {
                backend = match value().as_str() {
                    "cpu" => BackendKind::Cpu,
                    "cuda" => BackendKind::Cuda,
                    other => fail(format!("unsupported backend {other}")),
                }
            }
            other => fail(format!("unknown argument {other}")),
        }
    }
    Args {
        root,
        dir: dir.unwrap_or_else(|| fail("--dir is required")),
        models: models.unwrap_or_else(|| fail("--models is required")),
        backend,
        model,
        out,
        stored,
    }
}

fn read_track(path: &Path) -> TrackBuffer {
    let file = File::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let wav =
        read_wav(BufReader::new(file)).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    TrackBuffer::try_from(wav).unwrap_or_else(|e| fail(format!("{}: {e:?}", path.display())))
}

/// The cleaned mic and the per-interval canceller status of one replay.
struct Replay {
    cleaned: Vec<f32>,
    reference: Vec<f32>,
    status: Vec<(MeetingTime, AecStats)>,
    /// Lock stretches as a live session records them (`LiveEchoFilter::stats`).
    locks: Vec<EchoLock>,
}

fn replay(dir: &Path, config: PipelineConfig) -> Replay {
    let bytes = std::fs::read(dir.join("timestamps.json"))
        .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())));
    let timeline: RawTimeline =
        serde_json::from_slice(&bytes).unwrap_or_else(|e| fail(format!("timestamps.json: {e}")));
    let mut source = ReplayBuilder::from_raw_timeline(
        &timeline,
        read_track(&dir.join("mic.wav")),
        read_track(&dir.join("loopback.wav")),
    )
    .and_then(ReplayBuilder::build)
    .unwrap_or_else(|e| fail(format!("replay: {e:?}")));
    let mut pipeline = Pipeline::new(source.session().clone(), config)
        .unwrap_or_else(|e| fail(format!("pipeline: {e:?}")));
    let mut out = Replay {
        cleaned: Vec::new(),
        reference: Vec::new(),
        status: Vec::new(),
        locks: Vec::new(),
    };
    let mut locks = LiveEchoFilter::new();
    loop {
        let status = source
            .pump(&mut pipeline)
            .unwrap_or_else(|e| fail(format!("replay pump: {e:?}")));
        for event in pipeline.poll() {
            match event {
                PipelineEvent::Stats(s) => {
                    locks.stats(&s);
                    if let Some(a) = s.aec {
                        out.status.push((s.end, a));
                    }
                }
                PipelineEvent::Audio(a) => {
                    let track = if a.track == TrackId::Mic {
                        &mut out.cleaned
                    } else {
                        &mut out.reference
                    };
                    let start = usize::try_from(a.start).unwrap_or(usize::MAX);
                    if track.len() < start + a.samples.len() {
                        track.resize(start + a.samples.len(), 0.0);
                    }
                    track[start..start + a.samples.len()].copy_from_slice(&a.samples);
                }
                _ => {}
            }
        }
        if status == SourceStatus::Ended && pipeline.is_ended() {
            break;
        }
    }
    out.locks = locks.locks().to_vec();
    out
}

/// The near-end script lines of gates/GATE-12.md, normalised tokens per line.
fn script_lines(root: &Path) -> Vec<Vec<String>> {
    let path = root.join("gates/GATE-12.md");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let section = text
        .split("## Near-end script")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .unwrap_or_else(|| fail("GATE-12.md has no near-end script"));
    let lines: Vec<Vec<String>> = section
        .lines()
        .filter_map(|l| l.trim().strip_prefix("- "))
        .map(tokens_of)
        .collect();
    if let Err(e) = private().check_lines(lines.len()) {
        fail(e);
    }
    if lines.len() != 10 {
        fail(format!(
            "GATE-12 script: {} lines, expected 10",
            lines.len()
        ));
    }
    lines
}

fn tokens_of(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_token)
        .filter(|t| !t.is_empty())
        .collect()
}

fn word_tokens(words: &[Word]) -> Vec<String> {
    words.iter().flat_map(|w| tokens_of(&w.text)).collect()
}

/// Length of the longest common subsequence of `a` and `b`.
fn lcs(a: &[String], b: &[String]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for x in a {
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                cur[j].max(prev[j + 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn frames(ms: u64) -> usize {
    usize::try_from(MeetingTime::from_millis(ms).to_pipeline_frames()).unwrap_or(usize::MAX)
}

fn words_in(words: &[Word], from_ms: u64, to_ms: u64) -> Vec<Word> {
    words
        .iter()
        .filter(|w| (from_ms..to_ms).contains(&w.start.as_millis()))
        .cloned()
        .collect()
}

/// Per reported line, words found in `tokens`.
fn line_counts(lines: &[Vec<String>], tokens: &[String]) -> Vec<usize> {
    private()
        .line_range()
        .map(|l| lcs(&lines[l - 1], tokens))
        .collect()
}

fn key_counts(tokens: &[String]) -> Value {
    let mut m = serde_json::Map::new();
    for k in &private().key_words {
        m.insert(k.clone(), json!(tokens.iter().filter(|t| *t == k).count()));
    }
    Value::Object(m)
}

struct Decoder {
    model: Box<dyn Transcriber>,
    vad: SileroVad,
    options: TranscribeOptions,
    backend: BackendKind,
}

impl Decoder {
    fn check(&self, kind: BackendKind) {
        if kind != self.backend || self.model.backend_info().kind != self.backend {
            fail(format!(
                "decode ran on {kind:?}, expected {:?}",
                self.backend
            ));
        }
    }

    /// The final pass over the whole track: words and the piece report.
    fn pieces(&mut self, audio: &[f32]) -> (Vec<Word>, Value) {
        self.pieces_with(audio, None)
    }

    /// As [`Decoder::pieces`], with speech found by `vad` instead of Silero on `audio`.
    fn pieces_with(&mut self, audio: &[f32], vad: Option<&mut dyn Vad>) -> (Vec<Word>, Value) {
        let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
        let vad: &mut dyn Vad = match vad {
            Some(v) => v,
            None => &mut self.vad,
        };
        let (t, report) = transcribe_in_pieces(
            self.model.as_mut(),
            vad,
            &buffer,
            &self.options,
            &PieceConfig::default(),
            &CancelToken::new(),
        )
        .unwrap_or_else(|e| fail(format!("pieces: {e}")));
        self.check(t.backend.kind);
        let report = json!({
            "pieces": report.pieces,
            "retried": report.retried,
            "dropped": report.dropped.iter().map(|(a, b)| [a.as_millis(), b.as_millis()]).collect::<Vec<_>>(),
            "uncovered": report.uncovered,
        });
        (refs::kept_words(&t), report)
    }

    /// Plain calls over `from_ms..to_ms` in [`DIRECT_CHUNK_MS`] chunks starting at `from_ms`,
    /// words in meeting time.
    fn direct(&mut self, audio: &[f32], from_ms: u64, to_ms: u64) -> Vec<Word> {
        let mut words = Vec::new();
        let mut at = from_ms;
        while at < to_ms {
            let end = (at + DIRECT_CHUNK_MS).min(to_ms);
            let (a, b) = (frames(at).min(audio.len()), frames(end).min(audio.len()));
            if b > a {
                let buffer = MonoBuffer::new(PIPELINE_RATE, audio[a..b].to_vec());
                let t = self
                    .model
                    .transcribe(&buffer, &self.options, &CancelToken::new())
                    .unwrap_or_else(|e| fail(format!("direct: {e}")));
                self.check(t.backend.kind);
                let shift = MeetingTime::from_millis(at);
                for mut w in refs::kept_words(&t) {
                    w.start = MeetingTime::from_millis(w.start.as_millis() + shift.as_millis());
                    w.end = MeetingTime::from_millis(w.end.as_millis() + shift.as_millis());
                    words.push(w);
                }
            }
            at = end;
        }
        words
    }
}

fn db(power: f64) -> Value {
    if power > 1e-12 {
        json!((10.0 * power.log10() * 10.0).round() / 10.0)
    } else {
        Value::Null
    }
}

fn mean_power(x: &[f32], from: usize, to: usize) -> f64 {
    let (a, b) = (from.min(x.len()), to.min(x.len()));
    if b <= a {
        return 0.0;
    }
    x[a..b]
        .iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum::<f64>()
        / (b - a) as f64
}

/// VAD speech seconds of each [`BIN_MS`] bin of `from_ms..to_ms` and in total.
fn speech_seconds(regions: &[Range<usize>], from_ms: u64, to_ms: u64) -> f64 {
    let (a, b) = (frames(from_ms), frames(to_ms));
    let n: usize = regions
        .iter()
        .map(|r| r.end.min(b).saturating_sub(r.start.max(a)))
        .sum();
    n as f64 / f64::from(PIPELINE_RATE.hz())
}

fn probabilities(vad: &mut SileroVad, audio: &[f32]) -> Vec<f32> {
    frame_probabilities(vad, audio, &CancelToken::new())
        .unwrap_or_else(|e| fail(format!("vad: {e}")))
}

fn regions(vad: &mut SileroVad, audio: &[f32]) -> Vec<Range<usize>> {
    let probs = probabilities(vad, audio);
    speech_regions(
        &probs,
        vad.frame_len(),
        audio.len(),
        &PieceConfig::default(),
    )
    .into_iter()
    .map(|s| s.start..s.end)
    .collect()
}

#[derive(Default)]
struct StatusCounts {
    intervals: usize,
    locked: usize,
    double_talk: usize,
    vetoed: usize,
    effective_double_talk: usize,
    bypass: usize,
    converged: usize,
    gain_below_half: usize,
    gain_sum: f64,
}

impl StatusCounts {
    fn of(status: &[(MeetingTime, AecStats)], from_ms: u64, to_ms: u64) -> Self {
        let mut c = StatusCounts::default();
        for (_, a) in status
            .iter()
            .filter(|(t, _)| (from_ms..to_ms).contains(&t.as_millis()))
        {
            c.intervals += 1;
            c.locked += usize::from(a.locked);
            c.double_talk += usize::from(a.double_talk);
            c.vetoed += usize::from(a.double_talk && a.vetoed);
            c.effective_double_talk += usize::from(a.double_talk && !a.vetoed);
            c.bypass += usize::from(a.bypass);
            c.converged += usize::from(a.converged);
            c.gain_below_half += usize::from(a.suppression_gain < 0.5);
            c.gain_sum += f64::from(a.suppression_gain);
        }
        c
    }

    fn json(&self) -> Value {
        json!({
            "intervals": self.intervals,
            "locked": self.locked,
            "double_talk": self.double_talk,
            "vetoed": self.vetoed,
            "effective_double_talk": self.effective_double_talk,
            "bypass": self.bypass,
            "converged": self.converged,
            "gain_below_half": self.gain_below_half,
            "mean_gain": if self.intervals > 0 {
                json!((self.gain_sum / self.intervals as f64 * 1000.0).round() / 1000.0)
            } else {
                Value::Null
            },
        })
    }
}

fn main() {
    let args = parse_args();
    let path = gate12_private::default_path();
    match gate12_private::load(path.as_deref()) {
        Ok(Some(settings)) => {
            let _ = PRIVATE.set(settings);
        }
        Ok(None) => {
            eprintln!(
                "gate12_diag: SKIPPED: this tool reports only on private recordings, and their settings (.earshot-private/gate12.toml in the home folder) are not here"
            );
            return;
        }
        Err(e) => fail(e),
    }
    let accel = Accelerator::of(args.backend)
        .unwrap_or_else(|| fail(format!("no accelerator for {:?}", args.backend)));
    let order = vec![accel];
    init_onnx(&args.models, onnx_library(&args.model, &order))
        .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let choice = final_pass_asr(
        &EvidenceLoader::default(),
        &args.models,
        &args.model,
        &order,
    )
    .unwrap_or_else(|e| fail(format!("final-pass model: {e}")));
    if choice.backend != args.backend || !choice.rejected.is_empty() {
        fail(format!(
            "final-pass model on {:?} (refused {:?})",
            choice.backend, choice.rejected
        ));
    }
    let vad = load_silero(&args.models, Accelerator::Cpu)
        .unwrap_or_else(|e| fail(format!("Silero: {e}")));
    let mut dec = Decoder {
        model: Box::new(choice.transcriber),
        vad,
        options: TranscribeOptions::live(),
        backend: args.backend,
    };
    let lines = script_lines(&args.root);
    if let Some(data) = &args.stored {
        stored::run(&args, &lines, data);
        return;
    }

    eprintln!("gate12_diag: replaying (live pipeline settings)");
    let production = replay(&args.dir, LiveConfig::default().pipeline);
    eprintln!("gate12_diag: replaying (8 ms canceller status)");
    let mut fine = LiveConfig::default().pipeline;
    fine.stats_interval = STATUS_INTERVAL;
    let diag = replay(&args.dir, fine);
    let same_output = diag.cleaned.len() == production.cleaned.len()
        && diag
            .cleaned
            .iter()
            .zip(&production.cleaned)
            .all(|(a, b)| a.to_bits() == b.to_bits());

    let loopback = read_track(&args.dir.join("loopback.wav"));
    let mic = read_track(&args.dir.join("mic.wav"));
    if mic.channels().get() != 1 {
        fail("mic.wav is not mono");
    }
    let raw16 = resample_all(mic.samples(), mic.rate(), PIPELINE_RATE)
        .unwrap_or_else(|e| fail(format!("resample: {e:?}")));
    let sources: [(&str, Vec<f32>); 2] = [
        ("raw", quantise_pcm16(&raw16)),
        ("cleaned", quantise_pcm16(&production.cleaned)),
    ];

    let mut per_source = Vec::new();
    for (name, audio) in &sources {
        eprintln!("gate12_diag: {name}: final-pass pieces over the whole track");
        let (piece_words, report) = dec.pieces(audio);
        let speech = regions(&mut dec.vad, audio);
        let mut windows = Vec::new();
        for w in &private().windows {
            let (label, from, to) = (w.label.as_str(), w.from_ms, w.to_ms);
            eprintln!("gate12_diag: {name}: direct decodes over {label}");
            let grids = [
                dec.direct(audio, from, to),
                dec.direct(audio, from + DIRECT_OFFSET_MS, to),
            ];
            // The offset grid misses the first DIRECT_OFFSET_MS: decode it on its own.
            let head = dec.direct(audio, from, from + DIRECT_OFFSET_MS);
            let offset: Vec<Word> = head.into_iter().chain(grids[1].clone()).collect();
            let pw = words_in(&piece_words, from, to);
            let pt = word_tokens(&pw);
            let d0 = word_tokens(&grids[0]);
            let d1 = word_tokens(&offset);
            let direct_lines: Vec<usize> = line_counts(&lines, &d0)
                .into_iter()
                .zip(line_counts(&lines, &d1))
                .map(|(a, b)| a.max(b))
                .collect();
            windows.push(json!({
                "window": label,
                "vad_speech_s": (speech_seconds(&speech, from, to) * 10.0).round() / 10.0,
                "pieces": {
                    "words": pw.len(),
                    "lines": line_counts(&lines, &pt),
                    "key_words": key_counts(&pt),
                },
                "direct": {
                    "words": [grids[0].len(), offset.len()],
                    "lines": direct_lines,
                    "key_words": key_counts(if lcs_total(&lines, &d0) >= lcs_total(&lines, &d1) { &d0 } else { &d1 }),
                },
            }));
        }
        per_source.push((name.to_string(), piece_words, speech, report, windows));
    }

    // Pieces planned on the raw mic's speech, decoded from the cleaned mic.
    eprintln!("gate12_diag: cleaned audio, pieces planned on the raw mic's VAD");
    let mut replayed = ReplayedVad {
        probs: probabilities(&mut dec.vad, &sources[0].1),
        frame: dec.vad.frame_len(),
        at: 0,
    };
    let lengths = lengths_json(
        sources[0].1.len(),
        sources[1].1.len(),
        replayed.probs.len(),
        replayed.frame,
    );
    let (raw_planned, raw_planned_report) = dec.pieces_with(&sources[1].1, Some(&mut replayed));
    // Production echo cleaning's text rule against the model's own far-end transcript, per
    // lock stretch (the speaker check is not run; it only removes more).
    eprintln!("gate12_diag: far-end transcript for the text rule");
    let (far_words, _) = dec.pieces(&quantise_pcm16(&production.reference));
    let text_clean = |words: &Vec<Word>| -> Vec<Word> {
        let mask = text_echo_mask(words, &far_words, &production.locks)
            .unwrap_or_else(|e| fail(format!("echo cleaning: {e}")));
        words
            .iter()
            .zip(mask)
            .filter(|(_, echo)| !echo)
            .map(|(w, _)| w.clone())
            .collect()
    };
    let variants: Vec<(&str, &Vec<Word>)> = vec![
        ("raw audio, raw VAD", &per_source[0].1),
        ("cleaned audio, cleaned VAD (production)", &per_source[1].1),
        ("cleaned audio, raw VAD", &raw_planned),
    ];
    let piece_variants: Vec<Value> = variants
        .iter()
        .map(|(label, words)| {
            let after = {
                    let kept = text_clean(words);
                    let t = word_tokens(&words_in(&kept, private().windows[0].from_ms, private().windows[0].to_ms));
                    json!({
                        "track_words": kept.len(),
                        "residue_words": private().residue_ms.iter().map(|&[a, b]| words_in(&kept, a, b).len()).sum::<usize>(),
                        "lines_first_window": line_counts(&lines, &t),
                        "script_lines_1_10_whole_track": (1..=10).map(|l| lcs(&lines[l - 1], &word_tokens(&kept))).collect::<Vec<_>>(),
                    })
            };
            json!({
                "variant": label,
                "windows": private().windows.iter().map(|win| {
                    let (w, a, b) = (win.label.as_str(), win.from_ms, win.to_ms);
                    let t = word_tokens(&words_in(words, a, b));
                    json!({"window": w, "words": words_in(words, a, b).len(),
                           "lines": line_counts(&lines, &t), "key_words": key_counts(&t)})
                }).collect::<Vec<_>>(),
                "script_lines_1_10_whole_track": (1..=10).map(|l| lcs(&lines[l - 1], &word_tokens(words))).collect::<Vec<_>>(),
                "residue_words": private().residue_ms.iter().map(|&[a, b]| words_in(words, a, b).len()).sum::<usize>(),
                "track_words": words.len(),
                "after_text_rule": after,
            })
        })
        .collect();

    // Per-bin table over the first window.
    let (from, to) = (private().windows[0].from_ms, private().windows[0].to_ms);
    let mut bins = Vec::new();
    let mut t = from;
    while t < to {
        let e = (t + BIN_MS).min(to);
        let (a, b) = (frames(t), frames(e));
        let raw_p = mean_power(&sources[0].1, a, b);
        let clean_p = mean_power(&sources[1].1, a, b);
        let mut row = serde_json::Map::new();
        row.insert("from_s".into(), json!(t / 1000));
        row.insert("raw_dbfs".into(), db(raw_p));
        row.insert("cleaned_dbfs".into(), db(clean_p));
        row.insert(
            "attenuation_db".into(),
            if raw_p > 1e-12 && clean_p > 1e-12 {
                json!((10.0 * (raw_p / clean_p).log10() * 10.0).round() / 10.0)
            } else {
                Value::Null
            },
        );
        row.insert(
            "reference_dbfs".into(),
            db(mean_power(&production.reference, a, b)),
        );
        row.insert(
            "reference_side_to_mid_db".into(),
            side_to_mid(&loopback, t, e),
        );
        for (name, words, speech, _, _) in &per_source {
            row.insert(
                format!("{name}_vad_speech_s"),
                json!((speech_seconds(speech, t, e) * 10.0).round() / 10.0),
            );
            row.insert(
                format!("{name}_piece_words"),
                json!(words_in(words, t, e).len()),
            );
        }
        row.insert(
            "canceller".into(),
            StatusCounts::of(&diag.status, t, e).json(),
        );
        bins.push(Value::Object(row));
        t = e;
    }

    let out = json!({
        "recording": args.dir.file_name().map(|n| n.to_string_lossy().into_owned()),
        "model": args.model,
        "backend": format!("{:?}", args.backend),
        "options": "TranscribeOptions::live() (the stretch track's final-pass options)",
        "line_count_rule": LINE_COUNT_RULE,
        "lines_reported": private().line_range().collect::<Vec<_>>(),
        "lengths": lengths,
        "line_words": private().line_range().map(|l| lines[l - 1].len()).collect::<Vec<_>>(),
        "status_replay_equals_production": same_output,
        "status_intervals": diag.status.len(),
        "lock_stretches": production.locks.len(),
        "far_words": far_words.len(),
        "canceller_windows": private().windows.iter().map(|w| json!({
            "window": w.label,
            "status": StatusCounts::of(&diag.status, w.from_ms, w.to_ms).json(),
        })).collect::<Vec<_>>(),
        "sources": per_source.iter().map(|(name, words, _, report, windows)| json!({
            "source": name,
            "track_words": words.len(),
            "piece_report": report,
            "windows": windows,
        })).collect::<Vec<_>>(),
        "piece_variants": piece_variants,
        "raw_planned_piece_report": raw_planned_report,
        "bins": bins,
    });
    let text = serde_json::to_string_pretty(&out).unwrap_or_else(|e| fail(e));
    if let Some(path) = &args.out {
        std::fs::write(path, &text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    }
    println!("{text}");
}

/// The raw and cleaned mic lengths (16 kHz samples), the raw VAD frames the
/// "cleaned audio, raw VAD" variant replays, and the cleaned audio past the last of them
/// (read as silence by [`ReplayedVad`]).
fn lengths_json(raw: usize, cleaned: usize, raw_vad_frames: usize, frame: usize) -> Value {
    let covered = raw_vad_frames.saturating_mul(frame);
    let uncovered = cleaned.saturating_sub(covered);
    json!({
        "raw_samples": raw,
        "cleaned_samples": cleaned,
        "raw_vad_frames": raw_vad_frames,
        "vad_frame_samples": frame,
        "cleaned_samples_past_raw_vad": uncovered,
        "cleaned_seconds_past_raw_vad": uncovered as f64 / f64::from(PIPELINE_RATE.hz()),
    })
}

fn lcs_total(lines: &[Vec<String>], tokens: &[String]) -> usize {
    line_counts(lines, tokens).iter().sum()
}

/// Side over mid power (dB) of a stereo track over `from_ms..to_ms` (frame 0 at the origin).
fn side_to_mid(track: &TrackBuffer, from_ms: u64, to_ms: u64) -> Value {
    if track.channels().get() != 2 {
        return Value::Null;
    }
    let rate = u64::from(track.rate().hz());
    let at = |ms: u64| usize::try_from(ms * rate / 1000).unwrap_or(usize::MAX);
    let frames = track.samples().len() / 2;
    let (a, b) = (at(from_ms).min(frames), at(to_ms).min(frames));
    let (mut mid, mut side) = (0.0f64, 0.0f64);
    for f in track.samples()[2 * a..2 * b].as_chunks::<2>().0 {
        let (l, r) = (f64::from(f[0]), f64::from(f[1]));
        mid += (l + r) * (l + r) / 4.0;
        side += (l - r) * (l - r) / 4.0;
    }
    if mid > 1e-12 && side > 0.0 {
        json!((10.0 * (side / mid).log10() * 10.0).round() / 10.0)
    } else if mid > 1e-12 {
        json!("identical channels")
    } else {
        Value::Null
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate12_diag_line_counts_are_an_upper_bound_on_one_reading() {
        let t = |s: &str| tokens_of(s);
        // A line read twice, each reading missing a different word: the LCS over both
        // readings finds the whole line, more than either reading.
        let line = t("the valves are late");
        let twice = t("the valves late the are late");
        assert_eq!(lcs(&line, &t("the valves late")), 3);
        assert_eq!(lcs(&line, &t("the are late")), 3);
        assert_eq!(lcs(&line, &twice), 4);
        assert!(LINE_COUNT_RULE.starts_with("upper bound"));
    }

    #[test]
    fn gate12_diag_lengths_report_the_cleaned_tail_past_the_raw_vad() {
        let v = lengths_json(16_000, 16_000 + 512 * 3 + 100, 31, 512);
        assert_eq!(v["raw_vad_frames"], 31);
        assert_eq!(
            v["cleaned_samples_past_raw_vad"],
            16_000 + 512 * 3 + 100 - 31 * 512
        );
        let none = lengths_json(16_000, 15_000, 32, 512);
        assert_eq!(none["cleaned_samples_past_raw_vad"], 0);
    }
}
