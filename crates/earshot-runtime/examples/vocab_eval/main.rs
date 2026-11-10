//! Your words in the transcript (T12.15b, a spike, evaluation only): the rule, the numbers and
//! the commands are in docs/vocabulary.md, which was committed before anything was measured.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example vocab_eval --
//!     --models <dir> --fixtures <dir> --out <dir> --decode parakeet|whisper
//!     [--workers <n>] [--threads <n>]
//! cargo run --release -p earshot-runtime --example vocab_eval --
//!     --fixtures <dir> --out <dir> --report <dir> --score
//! ```
//!
//! `--decode parakeet` decodes every LibriSpeech dev-clean utterance with
//! `parakeet-tdt-0.6b-v2` on the CPU, unbiased and with each boosting variant, `--decode whisper`
//! every fourth utterance with `small.en` without and with the list as its prompt; each variant's
//! hypotheses go to `<out>/<engine>-<variant>.jsonl` (id, hypothesis text, decode time) and the
//! run's settings and backend evidence to `<out>/<engine>-meta.json`. `--score` rebuilds the lists,
//! runs the correction pass on both baselines (`<out>/correction-<engine>-<variant>.jsonl`, every
//! replacement with the original kept) and writes the word-free report to `<report>`:
//! `results.json`, the meta files and per decode `counts-<name>.jsonl` (id, SHA-256 of the
//! hypothesis, counts). `<out>` holds text and stays out of the repo (`target/`): LibriSpeech text
//! can hold any name. Nothing here is shipped code: the one hook it needs,
//! `ParakeetTdt::transcribe_biased`, is unused by every shipped path.

#[allow(dead_code)] // shared with asr_eval; only the AMI words parser and kept words are used
#[path = "../../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

mod boost;
mod correct;
mod lists;
mod score;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use earshot_core::asr::{BackendKind, CancelToken, TranscribeOptions, Transcriber, Vocab};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_onnx::SessionConfig;
use earshot_onnx::parakeet::{ParakeetTdt, TokenBias};
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, WhisperLoader, init_onnx, whisper_model,
};
use serde_json::{Value, json};

use boost::{Boost, ContextGraph, Gate};
use correct::Variant;
use lists::{List, Utterance};
use score::{Alignment, Counts};

const PARAKEET: &str = "parakeet-tdt-0.6b-v2";
const PARAKEET_DIR: &str = "parakeet/tdt-0.6b-v2";
const WHISPER: &str = "small.en";
/// Whisper decodes every this many-th utterance.
const WHISPER_STRIDE: usize = 4;
/// Boosting bonuses, in logit units.
const BETAS: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
/// Guards (docs/vocabulary.md): G1 distractor hits per 1,000 words above the baseline's, G2
/// U-WER points above the baseline's.
const G1_DISTRACTORS: f64 = 0.5;
const G2_UWER: f64 = 0.1;
/// A follow-up is justified by at least this relative B-WER reduction.
const JUSTIFY_REDUCTION: f64 = 0.20;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("vocab_eval: {msg}");
    std::process::exit(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Parakeet,
    Whisper,
    Score,
}

struct Args {
    models: Option<ModelDir>,
    fixtures: PathBuf,
    out: PathBuf,
    /// Where `--score` writes its word-free report (`results.json`, per-utterance counts).
    report: Option<PathBuf>,
    mode: Mode,
    workers: usize,
    threads: usize,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let (mut models, mut fixtures, mut out, mut mode) = (None, None, None, None);
    let mut report = None;
    let (mut workers, mut threads) = (4usize, 4usize);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| fail(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--models" => models = Some(ModelDir::new(value())),
            "--fixtures" => fixtures = Some(PathBuf::from(value())),
            "--out" => out = Some(PathBuf::from(value())),
            "--report" => report = Some(PathBuf::from(value())),
            "--decode" => {
                mode = Some(match value().as_str() {
                    "parakeet" => Mode::Parakeet,
                    "whisper" => Mode::Whisper,
                    other => fail(format!("--decode parakeet|whisper, not {other}")),
                });
            }
            "--score" => mode = Some(Mode::Score),
            "--workers" => workers = value().parse().unwrap_or_else(|e| fail(e)),
            "--threads" => threads = value().parse().unwrap_or_else(|e| fail(e)),
            other => fail(format!("unknown argument {other}")),
        }
    }
    let mode = mode.unwrap_or_else(|| fail("--decode <engine> or --score"));
    if mode != Mode::Score && models.is_none() {
        fail("--models is needed to decode");
    }
    Args {
        models,
        fixtures: fixtures.unwrap_or_else(|| fail("--fixtures")),
        out: out.unwrap_or_else(|| fail("--out")),
        report,
        mode,
        workers: workers.max(1),
        threads: threads.max(1),
    }
}

const FETCH_LIBRISPEECH: &str = "run `cargo xtask fetch-fixtures --set librispeech`";

fn dev_clean_dir(fixtures: &Path) -> PathBuf {
    fixtures.join("librispeech/dev-clean/LibriSpeech/dev-clean")
}

/// Every dev-clean utterance, in `(speaker, chapter, utterance)` order.
fn dev_clean(fixtures: &Path) -> Vec<Utterance> {
    let dir = dev_clean_dir(fixtures);
    let mut out = Vec::new();
    let read = |p: &Path| {
        std::fs::read_dir(p)
            .unwrap_or_else(|e| fail(format!("{}: {e}; {FETCH_LIBRISPEECH}", p.display())))
    };
    for speaker in read(&dir).filter_map(Result::ok) {
        for chapter in read(&speaker.path()).filter_map(Result::ok) {
            for f in read(&chapter.path()).filter_map(Result::ok) {
                let name = f.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".trans.txt") {
                    continue;
                }
                let text = std::fs::read_to_string(f.path())
                    .unwrap_or_else(|e| fail(format!("{}: {e}", f.path().display())));
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    let (id, reference) = line
                        .split_once(' ')
                        .unwrap_or_else(|| fail(format!("bad transcript line {line:?}")));
                    out.push(Utterance {
                        id: id.to_owned(),
                        reference: reference.to_owned(),
                    });
                }
            }
        }
    }
    out.sort_by_key(Utterance::key);
    if out.len() != 2_703 {
        fail(format!(
            "dev-clean has {} utterances, expected 2,703; {FETCH_LIBRISPEECH}",
            out.len()
        ));
    }
    out
}

fn audio_of(fixtures: &Path, u: &Utterance) -> MonoBuffer {
    let mut parts = u.id.split('-');
    let (speaker, chapter) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let path = dev_clean_dir(fixtures)
        .join(speaker)
        .join(chapter)
        .join(format!("{}.flac", u.id));
    let (rate, samples) = earshot_store::read_flac(&path)
        .unwrap_or_else(|e| fail(format!("{}: {e}; {FETCH_LIBRISPEECH}", path.display())));
    if rate != PIPELINE_RATE.hz() {
        fail(format!("{}: expected 16 kHz", path.display()));
    }
    MonoBuffer::new(PIPELINE_RATE, samples)
}

/// The utterances, their lists and the common words (docs/vocabulary.md).
struct Setup {
    utterances: Vec<Utterance>,
    lists: Vec<List>,
    common: HashSet<String>,
    pool_len: usize,
}

fn setup(fixtures: &Path) -> Setup {
    let common =
        lists::ami_common(&fixtures.join("ami/annotations/words")).unwrap_or_else(|e| fail(e));
    let utterances = dev_clean(fixtures);
    let rare = lists::rare_tokens(&utterances, &common);
    let pool: Vec<String> = rare.iter().cloned().collect();
    let lists = utterances
        .iter()
        .enumerate()
        .map(|(i, u)| lists::list_for(i, &u.reference, &rare, &pool))
        .collect();
    Setup {
        utterances,
        lists,
        common,
        pool_len: pool.len(),
    }
}

/// One decode of one utterance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Line {
    id: String,
    hyp: String,
    ms: u64,
}

fn hypothesis(t: &earshot_core::asr::Transcription) -> String {
    refs::hypothesis_text(&refs::kept_words(t))
}

fn write_lines(out: &Path, name: &str, lines: &[Line]) {
    let path = out.join(format!("{name}.jsonl"));
    let mut f =
        std::fs::File::create(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    for l in lines {
        let s = serde_json::to_string(l).unwrap_or_else(|e| fail(e));
        writeln!(f, "{s}").unwrap_or_else(|e| fail(e));
    }
}

fn read_lines(out: &Path, name: &str) -> Option<Vec<Line>> {
    let path = out.join(format!("{name}.jsonl"));
    let text = std::fs::read_to_string(&path).ok()?;
    Some(
        text.lines()
            .map(|l| {
                serde_json::from_str(l).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())))
            })
            .collect(),
    )
}

fn write_json(out: &Path, name: &str, v: &Value) {
    let path = out.join(name);
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|e| fail(e));
    std::fs::write(&path, text + "\n").unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
}

fn boost_variants() -> Vec<(String, f32, Gate)> {
    let mut v = Vec::new();
    for gate in [Gate::Open, Gate::Gated] {
        for beta in BETAS {
            let g = if gate == Gate::Open { "open" } else { "gated" };
            v.push((format!("boost-{g}-{beta}"), beta, gate));
        }
    }
    v
}

/// The boosting graph of one list: each written form and its lower-case form in word pieces.
fn graph_of(list: &List, pieces: &HashMap<String, usize>) -> (ContextGraph, usize) {
    let mut seqs = Vec::new();
    let mut untokenised = 0;
    for form in list.forms() {
        let lower = form.to_lowercase();
        for f in [form.as_str(), lower.as_str()] {
            match boost::tokenise(f, pieces) {
                Some(s) if !seqs.contains(&s) => seqs.push(s),
                Some(_) => {}
                None => untokenised += 1,
            }
        }
    }
    (ContextGraph::new(&seqs), untokenised)
}

fn host_note() -> Value {
    json!({
        "logical_cores": std::thread::available_parallelism().map(std::num::NonZeroUsize::get).ok(),
        "load": "shared build host: other lanes were compiling and testing during the run; decode times decide nothing",
    })
}

fn decode_parakeet(args: &Args, s: &Setup) {
    let models = args.models.as_ref().unwrap_or_else(|| fail("--models"));
    init_onnx(models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let dir = models.root().join(PARAKEET_DIR);
    let tokens_txt = std::fs::read_to_string(dir.join("tokens.txt")).unwrap_or_else(|e| {
        fail(format!(
            "{}: {e}; run `cargo xtask fetch-models --set ci`",
            dir.display()
        ))
    });
    let vocab = Vocab::parse(&tokens_txt).unwrap_or_else(|e| fail(e));
    let pieces = boost::pieces(&vocab);
    let variants = boost_variants();
    let mut names = vec!["baseline".to_owned()];
    names.extend(variants.iter().map(|v| v.0.clone()));
    let next = AtomicUsize::new(0);
    let results: Mutex<BTreeMap<(usize, usize), Line>> = Mutex::new(BTreeMap::new());
    let evidence: Mutex<Option<Value>> = Mutex::new(None);
    let untokenised = AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..args.workers {
            scope.spawn(|| {
                let mut asr =
                    ParakeetTdt::load(&dir, &SessionConfig::cpu(args.threads), PARAKEET, true)
                        .unwrap_or_else(|e| {
                            fail(format!(
                                "{PARAKEET}: {e}; run `cargo xtask fetch-models --set ci`"
                            ))
                        });
                let backends = asr.backends();
                if backends.iter().any(|b| b.kind() != Some(BackendKind::Cpu)) {
                    fail(format!("{PARAKEET} is not wholly on the CPU"));
                }
                evidence
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_or_insert_with(|| {
                        json!({
                            "source": "ONNX Runtime node placement",
                            "sessions": backends.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
                        })
                    });
                let opts = TranscribeOptions::live();
                let cancel = CancelToken::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(u) = s.utterances.get(i) else { break };
                    let audio = audio_of(&args.fixtures, u);
                    let (graph, missed) = graph_of(&s.lists[i], &pieces);
                    untokenised.fetch_add(missed, Ordering::Relaxed);
                    let mut lines = Vec::new();
                    let t0 = Instant::now();
                    let t = asr
                        .transcribe(&audio, &opts, &cancel)
                        .unwrap_or_else(|e| fail(format!("{}: {e}", u.id)));
                    lines.push(Line {
                        id: u.id.clone(),
                        hyp: hypothesis(&t),
                        ms: millis(t0),
                    });
                    // Every boosting variant from one encoding of each piece: their decode
                    // time is the whole call shared out evenly.
                    let mut boosts: Vec<Boost> = variants
                        .iter()
                        .map(|(_, beta, gate)| Boost {
                            graph: &graph,
                            beta: *beta,
                            gate: *gate,
                            blank: vocab.blank(),
                            node: 0,
                        })
                        .collect();
                    let mut biases: Vec<&mut dyn TokenBias> =
                        boosts.iter_mut().map(|b| b as &mut dyn TokenBias).collect();
                    let t0 = Instant::now();
                    let ts = asr
                        .transcribe_biased(&audio, &opts, &cancel, &mut biases)
                        .unwrap_or_else(|e| fail(format!("{}: {e}", u.id)));
                    let ms = millis(t0) / variants.len().max(1) as u64;
                    for t in &ts {
                        lines.push(Line {
                            id: u.id.clone(),
                            hyp: hypothesis(t),
                            ms,
                        });
                    }
                    let mut r = results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for (k, l) in lines.into_iter().enumerate() {
                        r.insert((k, i), l);
                    }
                    let done = r.len() / names.len();
                    if done.is_multiple_of(100) {
                        eprintln!(
                            "vocab_eval: parakeet {done}/{} after {:.0} s",
                            s.utterances.len(),
                            started.elapsed().as_secs_f64()
                        );
                    }
                }
            });
        }
    });
    let results = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut totals = serde_json::Map::new();
    for (k, name) in names.iter().enumerate() {
        let lines: Vec<Line> = results
            .range((k, 0)..(k + 1, 0))
            .map(|(_, l)| l.clone())
            .collect();
        if lines.len() != s.utterances.len() {
            fail(format!(
                "{name}: {} of {} utterances",
                lines.len(),
                s.utterances.len()
            ));
        }
        totals.insert(
            name.clone(),
            json!(lines.iter().map(|l| l.ms).sum::<u64>() as f64 / 1000.0),
        );
        write_lines(&args.out, &format!("parakeet-{name}"), &lines);
    }
    write_json(
        &args.out,
        "parakeet-meta.json",
        &json!({
            "model": PARAKEET,
            "backend": "cpu",
            "evidence": evidence.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner),
            "decoding": "greedy TDT (TranscribeOptions::live), one direct call per utterance; the baseline through Transcriber::transcribe, every boosting variant from one transcribe_biased call (one encoding per piece, the call's time shared out evenly)",
            "workers": args.workers,
            "onnx_threads_per_worker": args.threads,
            "betas": BETAS,
            "listed_forms_not_tokenised": untokenised.into_inner(),
            "decode_seconds": totals,
            "wall_seconds": started.elapsed().as_secs_f64(),
            "host": host_note(),
        }),
    );
}

fn millis(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The whisper prompt of a list: its forms in list order, joined by ", ", ending with ".".
fn prompt_of(list: &List) -> String {
    format!("{}.", list.forms().join(", "))
}

fn whisper_indices(n: usize) -> Vec<usize> {
    (0..n).step_by(WHISPER_STRIDE).collect()
}

fn decode_whisper(args: &Args, s: &Setup) {
    let models = args.models.as_ref().unwrap_or_else(|| fail("--models"));
    let rel = whisper_model(WHISPER).unwrap_or_else(|e| fail(e));
    let path = models.require(&rel).unwrap_or_else(|e| fail(e));
    let loader = EvidenceLoader {
        threads: std::num::NonZeroUsize::new(args.threads),
        live_tracks: None,
    };
    let mut asr = loader
        .load_verified(&path, BackendKind::Cpu)
        .unwrap_or_else(|e| fail(format!("{WHISPER}: {e}")));
    let evidence = json!({
        "source": "whisper.cpp log (EvidenceLoader, re-checked after every call)",
        "backend_info": asr.backend_info(),
        "whisper_log": asr.evidence().log.name(),
    });
    let cancel = CancelToken::new();
    let (mut plain, mut prompted) = (Vec::new(), Vec::new());
    let started = Instant::now();
    let indices = whisper_indices(s.utterances.len());
    for (n, &i) in indices.iter().enumerate() {
        let u = &s.utterances[i];
        let audio = audio_of(&args.fixtures, u);
        for (prompt, out) in [
            (None, &mut plain),
            (Some(prompt_of(&s.lists[i])), &mut prompted),
        ] {
            let opts = TranscribeOptions {
                prompt,
                ..TranscribeOptions::live()
            };
            let t0 = Instant::now();
            let t = asr
                .transcribe(&audio, &opts, &cancel)
                .unwrap_or_else(|e| fail(format!("{}: {e}", u.id)));
            out.push(Line {
                id: u.id.clone(),
                hyp: hypothesis(&t),
                ms: millis(t0),
            });
        }
        if n.is_multiple_of(10) {
            let (a, b) = (&plain[plain.len() - 1], &prompted[prompted.len() - 1]);
            eprintln!(
                "vocab_eval: whisper {n}/{} ({}: {} ms plain, {} ms prompted) after {:.0} s",
                indices.len(),
                u.id,
                a.ms,
                b.ms,
                started.elapsed().as_secs_f64()
            );
        }
        if n.is_multiple_of(50) {
            eprintln!(
                "vocab_eval: whisper {n}/{} after {:.0} s",
                indices.len(),
                started.elapsed().as_secs_f64()
            );
        }
    }
    let secs = |l: &[Line]| l.iter().map(|l| l.ms).sum::<u64>() as f64 / 1000.0;
    write_lines(&args.out, "whisper-baseline", &plain);
    write_lines(&args.out, "whisper-prompt", &prompted);
    write_json(
        &args.out,
        "whisper-meta.json",
        &json!({
            "model": WHISPER,
            "backend": "cpu",
            "evidence": evidence,
            "decoding": "greedy (TranscribeOptions::live), one direct call per utterance",
            "utterances": "every fourth dev-clean utterance in id order (index 0, 4, 8, ...)",
            "threads": args.threads,
            "decode_seconds": { "baseline": secs(&plain), "prompt": secs(&prompted) },
            "wall_seconds": started.elapsed().as_secs_f64(),
            "host": host_note(),
        }),
    );
}

/// Scores of one variant against its baseline (`None` for a baseline).
fn score_lines(
    s: &Setup,
    lines: &[Line],
    baseline: Option<&[Alignment]>,
) -> (Counts, Vec<Alignment>, Vec<Counts>) {
    let by_id: HashMap<&str, usize> = s
        .utterances
        .iter()
        .enumerate()
        .map(|(i, u)| (u.id.as_str(), i))
        .collect();
    if baseline.is_some_and(|b| b.len() != lines.len()) {
        fail("a variant does not cover the baseline's utterances");
    }
    let mut total = Counts::default();
    let mut aligns = Vec::with_capacity(lines.len());
    let mut each = Vec::with_capacity(lines.len());
    for (k, l) in lines.iter().enumerate() {
        let i = *by_id
            .get(l.id.as_str())
            .unwrap_or_else(|| fail(format!("unknown utterance {}", l.id)));
        let r = lists::tokens(&s.utterances[i].reference);
        let h = lists::tokens(&l.hyp);
        let listed: HashSet<&str> = s.lists[i].tokens.iter().map(String::as_str).collect();
        let r: Vec<&str> = r.iter().map(String::as_str).collect();
        let h: Vec<&str> = h.iter().map(String::as_str).collect();
        let (c, a) = score::score(&r, &h, &listed, baseline.map(|b| &b[k]));
        total += c;
        each.push(c);
        aligns.push(a);
    }
    (total, aligns, each)
}

/// The word-free record of one scored decode: per utterance its id, the SHA-256 of the
/// hypothesis text and the counts in [`COUNT_FIELDS`] order (hypothesis text stays out of the
/// repo: LibriSpeech text can hold any name).
fn write_counts(report: &Path, name: &str, lines: &[Line], each: &[Counts]) {
    use sha2::{Digest, Sha256};
    let path = report.join(format!("counts-{name}.jsonl"));
    let mut f =
        std::fs::File::create(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    for (l, c) in lines.iter().zip(each) {
        let sha: String = Sha256::digest(l.hyp.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let row = json!({
            "id": l.id,
            "hyp_sha256": sha,
            "c": [
                c.ref_words, c.errors, c.listed_words, c.listed_errors, c.listed_correct,
                c.unlisted_words, c.unlisted_errors, c.distractor_hits, c.listed_lost,
                c.listed_gained, c.unlisted_lost, c.unlisted_gained,
            ],
        });
        writeln!(f, "{row}").unwrap_or_else(|e| fail(e));
    }
}

/// The order of the counts in a `counts-*.jsonl` row.
const COUNT_FIELDS: [&str; 12] = [
    "ref_words",
    "errors",
    "listed_words",
    "listed_errors",
    "listed_correct",
    "unlisted_words",
    "unlisted_errors",
    "distractor_hits",
    "listed_lost",
    "listed_gained",
    "unlisted_lost",
    "unlisted_gained",
];

fn counts_json(c: &Counts) -> Value {
    json!({
        "counts": c,
        "wer_percent": c.wer(),
        "b_wer_percent": c.b_wer(),
        "u_wer_percent": c.u_wer(),
        "listed_recall_percent": c.listed_recall(),
        "distractor_hits_per_1000_words": c.distractors_per_1000(),
    })
}

/// Guards G1-G3 of a variant against its baseline.
fn guards(c: &Counts, base: &Counts) -> (bool, bool, bool) {
    let g1 = c.distractors_per_1000().unwrap_or(0.0)
        <= base.distractors_per_1000().unwrap_or(0.0) + G1_DISTRACTORS;
    let g2 = c.u_wer().unwrap_or(0.0) <= base.u_wer().unwrap_or(0.0) + G2_UWER;
    let g3 = c.errors * base.ref_words <= base.errors * c.ref_words;
    (g1, g2, g3)
}

/// One method's variants judged by the rule; returns its JSON and the chosen variant's
/// relative B-WER reduction when it passes every guard.
fn judge(
    method: &str,
    base: &Counts,
    variants: &[(String, Counts, Value)],
) -> (Value, Option<(String, f64)>) {
    let mut rows = Vec::new();
    let mut best: Option<(String, f64)> = None;
    for (name, c, extra) in variants {
        let (g1, g2, g3) = guards(c, base);
        let reduction = match (base.b_wer(), c.b_wer()) {
            (Some(b), Some(v)) if b > 0.0 => Some((b - v) / b),
            _ => None,
        };
        // Variants come in the rule's order, so a tie keeps the earlier one.
        if let (true, Some(r)) = (g1 && g2 && g3, reduction)
            && best.as_ref().is_none_or(|(_, b)| r > *b)
        {
            best = Some((name.clone(), r));
        }
        rows.push(json!({
            "variant": name,
            "metrics": counts_json(c),
            "g1_distractors": g1,
            "g2_unlisted": g2,
            "g3_wer": g3,
            "b_wer_reduction": reduction,
            "extra": extra,
        }));
    }
    let justified = best.as_ref().is_some_and(|(_, r)| *r >= JUSTIFY_REDUCTION);
    (
        json!({
            "method": method,
            "baseline": counts_json(base),
            "variants": rows,
            "chosen": best.as_ref().map(|b| &b.0),
            "chosen_b_wer_reduction": best.as_ref().map(|b| b.1),
            "justifies_follow_up": justified,
        }),
        best.filter(|(_, r)| *r >= JUSTIFY_REDUCTION),
    )
}

#[derive(serde::Serialize)]
struct CorrectedLine {
    id: String,
    hyp: String,
    changes: Vec<correct::Change>,
}

fn run_corrections(
    args: &Args,
    report: &Path,
    s: &Setup,
    engine: &str,
    base_lines: &[Line],
    base_align: &[Alignment],
) -> Vec<(String, Counts, Value)> {
    let by_id: HashMap<&str, usize> = s
        .utterances
        .iter()
        .enumerate()
        .map(|(i, u)| (u.id.as_str(), i))
        .collect();
    let mut out = Vec::new();
    for variant in Variant::ALL {
        let mut lines = Vec::new();
        let mut corrected = Vec::new();
        let (mut changes, mut to_listed_spoken) = (0usize, 0usize);
        for l in base_lines {
            let i = by_id[l.id.as_str()];
            let list = &s.lists[i];
            let words: Vec<String> = l.hyp.split_whitespace().map(str::to_owned).collect();
            let (fixed, ch) = correct::correct(&words, &list.forms(), variant, &s.common);
            let said: HashSet<String> = lists::tokens(&s.utterances[i].reference)
                .into_iter()
                .collect();
            to_listed_spoken += ch
                .iter()
                .filter(|c| said.contains(&c.listed.to_lowercase()))
                .count();
            changes += ch.len();
            let hyp = fixed.join(" ");
            lines.push(Line {
                id: l.id.clone(),
                hyp: hyp.clone(),
                ms: 0,
            });
            corrected.push(CorrectedLine {
                id: l.id.clone(),
                hyp,
                changes: ch,
            });
        }
        let path = args
            .out
            .join(format!("correction-{engine}-{}.jsonl", variant.name()));
        let mut f = std::fs::File::create(&path)
            .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
        for c in &corrected {
            writeln!(
                f,
                "{}",
                serde_json::to_string(c).unwrap_or_else(|e| fail(e))
            )
            .unwrap_or_else(|e| fail(e));
        }
        let (c, _, each) = score_lines(s, &lines, Some(base_align));
        write_counts(
            report,
            &format!("correction-{engine}-{}", variant.name()),
            &lines,
            &each,
        );
        out.push((
            variant.name().to_owned(),
            c,
            json!({
                "replacements": changes,
                "replacements_with_a_word_said_in_the_utterance": to_listed_spoken,
            }),
        ));
    }
    out
}

fn run_score(args: &Args, s: &Setup) {
    let report = args
        .report
        .as_deref()
        .unwrap_or_else(|| fail("--score needs --report <dir>"));
    std::fs::create_dir_all(report).unwrap_or_else(|e| fail(format!("{}: {e}", report.display())));
    let scored = |name: &str, lines: &[Line], base: Option<&[Alignment]>| {
        let (c, a, each) = score_lines(s, lines, base);
        write_counts(report, name, lines, &each);
        (c, a)
    };
    let mut methods = Vec::new();
    let mut justified: Vec<(String, String, f64)> = Vec::new();
    let mut push = |m: (Value, Option<(String, f64)>), name: &str| {
        if let Some((v, r)) = m.1 {
            justified.push((name.to_owned(), v, r));
        }
        methods.push(m.0);
    };
    if let Some(base) = read_lines(&args.out, "parakeet-baseline") {
        let (bc, ba) = scored("parakeet-baseline", &base, None);
        let mut boosted = Vec::new();
        for (name, _, _) in boost_variants() {
            let lines = read_lines(&args.out, &format!("parakeet-{name}"))
                .unwrap_or_else(|| fail(format!("parakeet-{name}.jsonl is missing")));
            let (c, _) = scored(&format!("parakeet-{name}"), &lines, Some(&ba));
            boosted.push((name, c, Value::Null));
        }
        push(judge("b: boosting (parakeet)", &bc, &boosted), "boosting");
        let corrections = run_corrections(args, report, s, "parakeet", &base, &ba);
        push(
            judge("c: correction (parakeet)", &bc, &corrections),
            "correction (parakeet)",
        );
    }
    if let Some(base) = read_lines(&args.out, "whisper-baseline") {
        let (bc, ba) = scored("whisper-baseline", &base, None);
        let lines = read_lines(&args.out, "whisper-prompt")
            .unwrap_or_else(|| fail("whisper-prompt.jsonl is missing"));
        let (c, _) = scored("whisper-prompt", &lines, Some(&ba));
        push(
            judge(
                "a: whisper prompt (small.en)",
                &bc,
                &[("prompt".to_owned(), c, Value::Null)],
            ),
            "whisper prompt",
        );
        let corrections = run_corrections(args, report, s, "whisper", &base, &ba);
        push(
            judge("c: correction (small.en)", &bc, &corrections),
            "correction (whisper)",
        );
    }
    if methods.is_empty() {
        fail("no decodes to score: run --decode parakeet and --decode whisper first");
    }
    justified.sort_by(|a, b| b.2.total_cmp(&a.2));
    let spoken: usize = s.lists.iter().map(|l| l.spoken).sum();
    let listed_ref: usize = s
        .utterances
        .iter()
        .zip(&s.lists)
        .map(|(u, l)| {
            let set: BTreeSet<&str> = l.tokens.iter().map(String::as_str).collect();
            lists::tokens(&u.reference)
                .iter()
                .filter(|t| set.contains(t.as_str()))
                .count()
        })
        .sum();
    for meta in ["parakeet-meta.json", "whisper-meta.json"] {
        let from = args.out.join(meta);
        if from.is_file() {
            std::fs::copy(&from, report.join(meta))
                .unwrap_or_else(|e| fail(format!("{}: {e}", from.display())));
        }
    }
    write_json(
        report,
        "results.json",
        &json!({
            "rule": "docs/vocabulary.md, committed before measuring",
            "data": {
                "utterances": s.utterances.len(),
                "whisper_utterances": whisper_indices(s.utterances.len()).len(),
                "common_words": s.common.len(),
                "rare_pool": s.pool_len,
                "list_length": lists::LIST_LEN,
                "spoken_listed_types": spoken,
                "spoken_listed_positions": listed_ref,
            },
            "count_fields": COUNT_FIELDS,
            "guards": { "g1_distractors_per_1000": G1_DISTRACTORS, "g2_u_wer_points": G2_UWER, "justify_reduction": JUSTIFY_REDUCTION },
            "methods": methods,
            "justified": justified.iter().map(|(m, v, r)| json!({"method": m, "variant": v, "b_wer_reduction": r})).collect::<Vec<_>>(),
        }),
    );
    for m in &methods {
        eprintln!("{}", serde_json::to_string(m).unwrap_or_default());
    }
}

fn main() {
    let args = parse_args();
    std::fs::create_dir_all(&args.out)
        .unwrap_or_else(|e| fail(format!("{}: {e}", args.out.display())));
    let s = setup(&args.fixtures);
    eprintln!(
        "vocab_eval: {} utterances, {} common words, {} rare tokens in the pool, {} spoken listed types",
        s.utterances.len(),
        s.common.len(),
        s.pool_len,
        s.lists.iter().map(|l| l.spoken).sum::<usize>()
    );
    match args.mode {
        Mode::Parakeet => decode_parakeet(&args, &s),
        Mode::Whisper => decode_whisper(&args, &s),
        Mode::Score => run_score(&args, &s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_whisper_takes_every_fourth_utterance_and_the_prompt_is_the_list() {
        assert_eq!(whisper_indices(9), [0, 4, 8]);
        assert_eq!(whisper_indices(2_703).len(), 676);
        let list = List {
            tokens: vec!["bozzle".into(), "quixote".into()],
            spoken: 1,
        };
        assert_eq!(prompt_of(&list), "Bozzle, Quixote.");
    }

    #[test]
    fn vocab_boost_variants_follow_the_rule_order() {
        let names: Vec<String> = boost_variants().into_iter().map(|v| v.0).collect();
        assert_eq!(names.len(), 8);
        assert_eq!(names[0], "boost-open-1");
        assert_eq!(names[7], "boost-gated-4");
    }

    fn counts(
        ref_words: usize,
        errors: usize,
        distractor_hits: usize,
        uw: (usize, usize),
    ) -> Counts {
        Counts {
            ref_words,
            errors,
            distractor_hits,
            unlisted_words: uw.0,
            unlisted_errors: uw.1,
            ..Counts::default()
        }
    }

    #[test]
    fn vocab_guards_hold_the_rule() {
        let base = counts(10_000, 300, 2, (9_000, 250));
        // 0.2 + 0.5 per 1,000 allowed: 6 hits pass, 8 fail.
        assert!(guards(&counts(10_000, 300, 6, (9_000, 250)), &base).0);
        assert!(!guards(&counts(10_000, 300, 8, (9_000, 250)), &base).0);
        // U-WER 2.78% + 0.1: 258 of 9,000 passes (2.867%), 260 fails (2.889%).
        assert!(guards(&counts(10_000, 300, 2, (9_000, 258)), &base).1);
        assert!(!guards(&counts(10_000, 300, 2, (9_000, 260)), &base).1);
        // WER not above the baseline's.
        assert!(guards(&counts(10_000, 300, 2, (9_000, 250)), &base).2);
        assert!(!guards(&counts(10_000, 301, 2, (9_000, 250)), &base).2);
    }
}
