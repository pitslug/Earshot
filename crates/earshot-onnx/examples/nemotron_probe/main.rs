//! Evaluation only (T12.23): checks `earshot_onnx::nemotron`, the streaming decoder of NVIDIA's
//! Nemotron Speech Streaming 0.6B (sherpa-onnx's 560 ms int8 export, manifest set `stream-eval`),
//! against sherpa-onnx's own Python package on the committed smoke clips.
//!
//! ```text
//! cargo run --release -p earshot-onnx --example nemotron_probe -- [--models <dir>]
//!     [--threads <n>] [--reference <file.jsonl>] [--dump <dir>] [--dump-features <dir>]
//!     [--decode-features <dir>]
//! cargo test -p earshot-onnx --example nemotron_probe -- --include-ignored
//! ```
//!
//! By default it decodes every clip with sherpa-onnx's protocol (the whole clip, 0.66 s of zeros,
//! input finished, every ready chunk) and requires, per clip, the same text and every token the
//! same as `reference.jsonl` beside this file; it also decodes each clip pushed in uneven pieces,
//! decoding after every piece as a live session would, and requires exactly the whole decode's
//! tokens and times. Token times against the reference are reported (how many moved and the
//! largest move in encoder frames of 80 ms), not required. It prints one line per clip and exits
//! 1 on any difference.
//!
//! Why times are reported, not required: the reference itself moves them with the version of
//! ONNX Runtime it runs on. `reference.jsonl` is sherpa-onnx 2.0.1 as published (its own ONNX
//! Runtime 1.28.2); `reference_ort131.jsonl` is the same package with its `onnxruntime.dll`
//! replaced by Earshot's pinned 1.31.0. The two differ by a comma on one clip and, on the other
//! 19, in 24 of 286 token times, by up to 3 frames. The exact check of the decoder is
//! `--decode-features`: given kaldi-native-fbank's own features (`knf_features.py`), this decoder
//! gives `reference_ort131.jsonl` exactly, every time included. This crate's front end differs
//! from kaldi-native-fbank's by float rounding (the FFT; at most 2.6e-4 in a log-mel value),
//! which moves some times in the same way.
//!
//! `--dump <dir>` writes each clip as `<id>.f32` (the samples `reference.py` reads) and
//! `--dump-features <dir>` this crate's features as `<id>.fbank`; neither decodes. The reference
//! files are produced only by `reference.py` (sherpa-onnx 2.0.1 from hash-pinned wheels,
//! `requirements.txt`), never by this crate.

use std::path::{Path, PathBuf};

use earshot_onnx::SessionConfig;
use earshot_onnx::nemotron::{NemotronStream, NemotronStreaming, SAMPLE_RATE};

/// The model's folder under the models directory.
const MODEL_DIR: &str = "nemotron/streaming-0.6b-560ms-int8";
/// sherpa-onnx's tail padding in its examples (`online-decode-files.py`): 0.66 s of zeros.
const TAIL_PADDING: usize = SAMPLE_RATE * 66 / 100;
/// Push sizes for the live-style decode (samples), cycled: uneven on purpose.
const PUSHES: [usize; 6] = [160, 1600, 37, 4000, 801, 10];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn models_dir() -> PathBuf {
    std::env::var_os("EARSHOT_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|b| PathBuf::from(b).join("Earshot").join("models"))
        })
        .expect("EARSHOT_MODELS or LOCALAPPDATA")
}

/// The model folder; a missing model is a failed precondition with the fetch command.
fn model_dir(models: &Path) -> PathBuf {
    let dir = models.join(MODEL_DIR);
    assert!(
        dir.join("encoder.int8.onnx").is_file(),
        "{} has no model; run `cargo xtask fetch-models --set stream-eval`",
        dir.display()
    );
    dir
}

fn decode_flac(path: &Path) -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, 16_000, "{}", path.display());
    assert_eq!(info.channels, 1, "{}", path.display());
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| {
            #[allow(clippy::cast_possible_truncation)]
            let v = (f64::from(s.expect("flac sample")) * scale) as f32;
            v
        })
        .collect()
}

/// The committed smoke clips, by id, in name order.
fn smoke_clips() -> Vec<(String, Vec<f32>)> {
    let dir = repo_root().join("fixtures/smoke");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "flac"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|p| {
            let id = p
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("clip name")
                .to_owned();
            let samples = decode_flac(&p);
            (id, samples)
        })
        .collect()
}

/// One clip's result as sherpa-onnx reports it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct Decoded {
    id: String,
    /// The tokens' text joined, U+2581 as a space.
    text: String,
    /// Each token's text (U+2581 as a space).
    tokens: Vec<String>,
    /// Each token's time in seconds (encoder frame times 0.08).
    timestamps: Vec<f64>,
}

/// A committed reference file beside this example.
fn committed(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/nemotron_probe")
        .join(name)
}

#[cfg(test)]
fn reference() -> Vec<Decoded> {
    reference_at(&committed("reference.jsonl"))
}

fn reference_at(path: &Path) -> Vec<Decoded> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("reference line"))
        .collect()
}

/// sherpa-onnx's result text from the stream's tokens (`Convert` in
/// `online-recognizer-transducer-impl.h`: `<unk>` skipped, U+2581 read as a space).
fn result_of(id: &str, model: &NemotronStreaming, stream: &NemotronStream) -> Decoded {
    let mut tokens = Vec::new();
    let mut timestamps = Vec::new();
    for t in stream.tokens() {
        // Every token keeps its time, `<unk>` included, as there.
        #[allow(clippy::cast_precision_loss)]
        timestamps.push(t.first_frame as f64 * 0.08);
        let sym = model.vocab().token(t.id).unwrap_or_default();
        if sym == "<unk>" {
            continue;
        }
        tokens.push(sym.replace('\u{2581}', " "));
    }
    Decoded {
        id: id.to_owned(),
        text: tokens.concat(),
        tokens,
        timestamps,
    }
}

/// sherpa-onnx's protocol: the whole clip, the tail padding, input finished, every ready chunk.
fn decode_whole(model: &mut NemotronStreaming, id: &str, samples: &[f32]) -> Decoded {
    let mut stream = model.stream();
    stream.accept(samples);
    stream.accept(&vec![0.0; TAIL_PADDING]);
    stream.input_finished();
    model.decode_ready(&mut stream).expect("decode");
    result_of(id, model, &stream)
}

/// The same audio pushed in uneven pieces, decoding after each as a live session would.
fn decode_pushed(model: &mut NemotronStreaming, id: &str, samples: &[f32]) -> Decoded {
    let mut stream = model.stream();
    let mut audio = samples.to_vec();
    audio.extend(std::iter::repeat_n(0.0, TAIL_PADDING));
    let mut at = 0;
    for n in PUSHES.iter().cycle() {
        if at >= audio.len() {
            break;
        }
        let end = (at + n).min(audio.len());
        stream.accept(&audio[at..end]);
        model.decode_ready(&mut stream).expect("decode");
        at = end;
    }
    stream.input_finished();
    model.decode_ready(&mut stream).expect("decode");
    result_of(id, model, &stream)
}

/// Whether two results have the same words: the same text and every token the same.
fn same_words(a: &Decoded, b: &Decoded) -> bool {
    a.id == b.id
        // sherpa-onnx's result text has no leading space (its own `Convert` gives one).
        && a.text.trim() == b.text.trim()
        && a.tokens == b.tokens
        && a.timestamps.len() == b.timestamps.len()
}

/// Tokens of `a` at another time than `b`'s, and the largest move in encoder frames (80 ms).
/// Only meaningful when [`same_words`].
fn moved(a: &Decoded, b: &Decoded) -> (usize, u32) {
    let diffs: Vec<f64> = a
        .timestamps
        .iter()
        .zip(&b.timestamps)
        .map(|(x, y)| (x - y).abs())
        .filter(|d| *d > 1e-6)
        .collect();
    let largest = diffs.iter().copied().fold(0.0f64, f64::max);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let frames = (largest / 0.08).round() as u32;
    (diffs.len(), frames)
}

/// Whether two results agree exactly: the same words and every token at the same time.
fn same(a: &Decoded, b: &Decoded) -> bool {
    same_words(a, b) && moved(a, b).0 == 0
}

fn load(models: &Path, threads: usize) -> NemotronStreaming {
    init_onnx(models);
    NemotronStreaming::load(&model_dir(models), &SessionConfig::cpu(threads)).expect("load")
}

fn init_onnx(models: &Path) {
    earshot_onnx::init(
        &models.join(earshot_onnx::ORT_CPU_DYLIB),
        earshot_onnx::ORT_CPU_DYLIB_SHA256,
    )
    .expect("ONNX Runtime");
}

/// What [`compare`] found over every clip.
#[derive(Debug, Default, PartialEq, Eq)]
struct Agreement {
    /// Clips whose text or tokens differ from the reference, or whose pushed decode differs from
    /// the whole one in anything (times included).
    differ: usize,
    /// Tokens in all; tokens at another time than the reference's (reported, module docs) and
    /// the largest such move in encoder frames.
    tokens: usize,
    moved: usize,
    largest_move: u32,
}

/// Compares every clip (module docs), printing one line per clip.
fn compare(models: &Path, threads: usize, want: &[Decoded]) -> Agreement {
    let mut model = load(models, threads);
    let clips = smoke_clips();
    assert_eq!(
        want.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        clips.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        "the reference must cover every smoke clip, in name order"
    );
    let mut found = Agreement::default();
    for ((id, samples), want) in clips.iter().zip(want) {
        let whole = decode_whole(&mut model, id, samples);
        let pushed = decode_pushed(&mut model, id, samples);
        let (moved_here, largest) = moved(&whole, want);
        let ok = same_words(&whole, want) && same(&pushed, &whole);
        found.tokens += want.tokens.len();
        found.moved += moved_here;
        found.largest_move = found.largest_move.max(largest);
        if !ok {
            found.differ += 1;
            eprintln!(
                "{id}: DIFFERS\n  sherpa-onnx: {want:?}\n  whole:  {whole:?}\n  pushed: {pushed:?}"
            );
        }
        println!(
            "{id}: {} ({} tokens, {moved_here} at another time, by up to {largest} frames)",
            if ok { "same words" } else { "DIFFERS" },
            want.tokens.len()
        );
    }
    found
}

/// The decoder alone on features written by another front end (`<dir>/<id>.fbank`, 128 bins,
/// little-endian f32, the tail padding included and flushed), against `want`.
fn compare_on_features(models: &Path, threads: usize, dir: &Path, want: &[Decoded]) -> usize {
    let mut model = load(models, threads);
    let mut differ = 0;
    for want in want {
        let bytes = std::fs::read(dir.join(format!("{}.fbank", want.id))).expect("features");
        let values: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        let frames: Vec<Vec<f32>> = values.chunks(128).map(<[f32]>::to_vec).collect();
        let fbank = earshot_onnx::nemotron::OnlineFbank::finished_with(frames);
        let mut stream = model.stream_on(fbank);
        model.decode_ready(&mut stream).expect("decode");
        let got = result_of(&want.id, &model, &stream);
        let ok = same(&got, want);
        if !ok {
            differ += 1;
            eprintln!("{}: DIFFERS\n  want: {want:?}\n  got:  {got:?}", want.id);
        }
        println!("{}: {}", want.id, if ok { "same" } else { "DIFFERS" });
    }
    differ
}

/// `--dump-features <dir>`: every clip's 128-bin features with the tail padding, flushed, as
/// `<id>.fbank` (little-endian f32, frame after frame), for comparison with kaldi-native-fbank.
fn dump_features(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create the dump folder");
    for (id, samples) in smoke_clips() {
        let mut f = earshot_onnx::nemotron::OnlineFbank::new(128);
        f.accept(&samples);
        f.accept(&vec![0.0; TAIL_PADDING]);
        f.input_finished();
        let n = f.frames_ready();
        let bytes: Vec<u8> = f
            .frames(0, n)
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        std::fs::write(dir.join(format!("{id}.fbank")), bytes).expect("write features");
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut models = models_dir();
    let mut threads = 2;
    let mut dump = None;
    let mut against = committed("reference.jsonl");
    while let Some(a) = args.next() {
        match a.as_str() {
            "--models" => models = PathBuf::from(args.next().expect("--models <dir>")),
            "--threads" => {
                threads = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--threads <n>");
            }
            "--reference" => against = PathBuf::from(args.next().expect("--reference <file>")),
            "--dump" => dump = Some(PathBuf::from(args.next().expect("--dump <dir>"))),
            "--decode-features" => {
                // Decoder only, on another front end's features: `<dir>/<id>.fbank`.
                let dir = PathBuf::from(args.next().expect("--decode-features <dir>"));
                let differ = compare_on_features(&models, threads, &dir, &reference_at(&against));
                println!("nemotron_probe: {differ} clips differ on the given features");
                std::process::exit(i32::from(differ > 0));
            }
            "--dump-features" => {
                let dir = PathBuf::from(args.next().expect("--dump-features <dir>"));
                dump_features(&dir);
                return;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    if let Some(dir) = dump {
        std::fs::create_dir_all(&dir).expect("create the dump folder");
        for (id, samples) in smoke_clips() {
            let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            std::fs::write(dir.join(format!("{id}.f32")), bytes).expect("write a clip");
        }
        return;
    }
    let found = compare(&models, threads, &reference_at(&against));
    println!(
        "nemotron_probe: {} clips differ; {} of {} token times moved, by up to {} frames",
        found.differ, found.moved, found.tokens, found.largest_move
    );
    if found.differ > 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nemotron_probe_reference_covers_every_smoke_clip() {
        let want = reference();
        let transcripts =
            std::fs::read_to_string(repo_root().join("fixtures/smoke/transcripts.txt"))
                .expect("transcripts");
        let mut ids: Vec<&str> = transcripts
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        ids.sort_unstable();
        assert_eq!(want.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ids);
        for d in &want {
            assert_eq!(d.tokens.concat().trim(), d.text, "{}", d.id);
            assert_eq!(d.tokens.len(), d.timestamps.len(), "{}", d.id);
        }
    }

    #[test]
    fn nemotron_probe_references_cover_the_same_clips() {
        let a = reference();
        let b = reference_at(&committed("reference_ort131.jsonl"));
        assert_eq!(
            a.iter().map(|d| &d.id).collect::<Vec<_>>(),
            b.iter().map(|d| &d.id).collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore = "needs the stream-eval model: cargo xtask fetch-models --set stream-eval, then \
                cargo test -p earshot-onnx --example nemotron_probe -- --include-ignored"]
    fn nemotron_probe_matches_sherpa_onnx_on_the_smoke_clips() {
        let found = compare(&models_dir(), 2, &reference());
        assert_eq!(found.differ, 0, "{found:?}");
    }
}
