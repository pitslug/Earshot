//! Final-pass stage timings (T6.8): where the post-meeting time goes.
//!
//! Decodes a meeting's two FLAC tracks (as stored by a live session), optionally tiles them
//! to a longer meeting, then times each final-pass stage with the production models:
//! whisper batch decoding per track (and greedy for comparison), and diarisation per track
//! split into segmentation, embeddings and the rest (clustering, turns). `--pieces` also
//! decodes in Silero speech pieces (`asr::transcribe_in_pieces`, T6.16); `--text` prints
//! the transcripts.
//!
//! ```text
//! cargo run --release -p earshot-runtime --features cuda --example post_bench -- \
//!     --mic <mic.flac> --ref <loopback.flac> [--repeat N] [--model base.en] \
//!     [--whisper cuda|cpu] [--onnx cuda|cpu] [--greedy] [--pieces] [--text]
//! ```
//!
//! Models come from EARSHOT_MODELS (default %LOCALAPPDATA%\Earshot\models). Measurement
//! only: nothing is stored.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use earshot_core::asr::{
    CancelToken, DecodingOptions, PieceConfig, TranscribeOptions, Transcriber, join_words,
    transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample, SampleRate};
use earshot_core::diar::{
    DiarError, Diarizer, EmbedderInfo, Embedding, LocalActivity, SegmenterInfo, SpeakerEmbedder,
    SpeakerSegmenter,
};
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, init_onnx, load_diarisation, load_silero,
    select_whisper, whisper_model,
};
use earshot_runtime::post::PostConfig;
use earshot_store::{DataDir, read_flac};

/// Time and calls spent inside a wrapped model.
#[derive(Default)]
struct Meter {
    time: Cell<Duration>,
    calls: Cell<usize>,
    items: Cell<usize>,
}

impl Meter {
    fn add(&self, d: Duration, items: usize) {
        self.time.set(self.time.get() + d);
        self.calls.set(self.calls.get() + 1);
        self.items.set(self.items.get() + items);
    }

    fn take(&self) -> (Duration, usize, usize) {
        (
            self.time.replace(Duration::ZERO),
            self.calls.replace(0),
            self.items.replace(0),
        )
    }
}

struct TimedSegmenter<S> {
    inner: S,
    meter: Rc<Meter>,
}

impl<S: SpeakerSegmenter> SpeakerSegmenter for TimedSegmenter<S> {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }

    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError> {
        let t = Instant::now();
        let out = self.inner.segment(windows, cancel);
        self.meter.add(t.elapsed(), windows.len());
        out
    }
}

struct TimedEmbedder<E> {
    inner: E,
    meter: Rc<Meter>,
}

impl<E: SpeakerEmbedder> SpeakerEmbedder for TimedEmbedder<E> {
    fn info(&self) -> &EmbedderInfo {
        self.inner.info()
    }

    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        let t = Instant::now();
        let out = self.inner.embed(speech, cancel);
        self.meter.add(t.elapsed(), speech.len());
        out
    }
}

struct Args {
    mic: PathBuf,
    reference: PathBuf,
    repeat: usize,
    model: String,
    whisper: Accelerator,
    onnx: Accelerator,
    greedy: bool,
    pieces: bool,
    text: bool,
}

fn accel(s: &str) -> Accelerator {
    match s {
        "cuda" => Accelerator::Cuda,
        "cpu" => Accelerator::Cpu,
        other => panic!("unknown accelerator {other:?} (cuda|cpu)"),
    }
}

fn parse() -> Args {
    let mut args = Args {
        mic: PathBuf::new(),
        reference: PathBuf::new(),
        repeat: 1,
        model: "base.en".into(),
        whisper: Accelerator::Cuda,
        // The small ONNX models run on the CPU in production (T6.11); `--onnx cuda` is an
        // explicit request for comparison (needs cuDNN).
        onnx: Accelerator::Cpu,
        greedy: false,
        pieces: false,
        text: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--mic" => args.mic = val().into(),
            "--ref" => args.reference = val().into(),
            "--repeat" => args.repeat = val().parse().expect("--repeat N"),
            "--model" => args.model = val(),
            "--whisper" => args.whisper = accel(&val()),
            "--onnx" => args.onnx = accel(&val()),
            "--greedy" => args.greedy = true,
            "--pieces" => args.pieces = true,
            "--text" => args.text = true,
            other => panic!("unknown argument {other:?}"),
        }
    }
    assert!(
        !args.mic.as_os_str().is_empty() && !args.reference.as_os_str().is_empty(),
        "--mic and --ref are required"
    );
    args
}

fn secs(d: Duration) -> String {
    format!("{:8.1} s", d.as_secs_f64())
}

fn load(path: &Path, repeat: usize) -> (Duration, MonoBuffer) {
    let t = Instant::now();
    let (rate, samples) = read_flac(path).expect("read FLAC");
    let decode = t.elapsed();
    let rate = SampleRate::new(rate).expect("non-zero rate");
    assert_eq!(rate, PIPELINE_RATE, "the stored recordings are 16 kHz");
    (decode, MonoBuffer::new(rate, samples.repeat(repeat)))
}

fn main() {
    let args = parse();
    let models = ModelDir::from_env(&DataDir::from_local_app_data().expect("LOCALAPPDATA"));

    let (dec_mic, mic) = load(&args.mic, args.repeat);
    let (dec_ref, reference) = load(&args.reference, args.repeat);
    let minutes = mic.samples.len() as f64 / f64::from(PIPELINE_RATE.hz()) / 60.0;
    println!(
        "meeting: {minutes:.1} min ({}x), whisper {} on {:?}, onnx on {:?}",
        args.repeat, args.model, args.whisper, args.onnx
    );
    println!(
        "decode FLAC (once, before tiling): mic {}, ref {}",
        secs(dec_mic),
        secs(dec_ref)
    );

    let t = Instant::now();
    init_onnx(&models, args.onnx).expect("onnx runtime");
    let path = models
        .require(&whisper_model(&args.model).expect("model name"))
        .expect("model");
    let mut whisper = select_whisper(&EvidenceLoader::default(), &path, &[args.whisper])
        .expect("whisper")
        .transcriber;
    let (seg, emb) = load_diarisation(&models, args.onnx).expect("diarisation models");
    let mut vad = load_silero(&models, args.onnx).expect("silero");
    println!("load models: {}", secs(t.elapsed()));

    let seg_meter = Rc::new(Meter::default());
    let emb_meter = Rc::new(Meter::default());
    let model_id = emb.info().model.clone();
    let mut config = PostConfig::default();
    config.diarization.clustering.threshold = config
        .clustering_threshold(&model_id)
        .expect("calibrated threshold");
    let mut diarizer = Diarizer::new(
        TimedSegmenter {
            inner: seg,
            meter: Rc::clone(&seg_meter),
        },
        TimedEmbedder {
            inner: emb,
            meter: Rc::clone(&emb_meter),
        },
        config.diarization.clone(),
    )
    .expect("diarizer");

    let cancel = CancelToken::new();
    let mut total = Duration::ZERO;
    for (name, audio) in [("mic", &mic), ("ref", &reference)] {
        let t = Instant::now();
        let tr = whisper
            .transcribe(audio, &TranscribeOptions::batch(), &cancel)
            .expect("transcribe");
        let asr = t.elapsed();
        total += asr;
        println!(
            "{name}: whisper batch (beam 5)   {}  {} words, ran on {:?}",
            secs(asr),
            tr.words().len(),
            tr.backend.kind
        );
        if args.text {
            println!("{name}: whole-track text: {}", join_words(&tr.words()));
        }
        if args.pieces {
            let t = Instant::now();
            let (tr, report) = transcribe_in_pieces(
                &mut whisper,
                &mut vad,
                audio,
                &TranscribeOptions::batch(),
                &PieceConfig::default(),
                &cancel,
            )
            .expect("transcribe in pieces");
            println!(
                "{name}: whisper in pieces       {}  {} words, {} pieces, {} retried, {} dropped",
                secs(t.elapsed()),
                tr.words().len(),
                report.pieces,
                report.retried,
                report.dropped.len()
            );
            if args.text {
                println!("{name}: pieces text: {}", join_words(&tr.words()));
            }
        }
        if args.greedy {
            let opts = TranscribeOptions {
                decoding: DecodingOptions::LIVE,
                ..TranscribeOptions::batch()
            };
            let t = Instant::now();
            let tr = whisper
                .transcribe(audio, &opts, &cancel)
                .expect("transcribe");
            println!(
                "{name}: whisper greedy (compare) {}  {} words",
                secs(t.elapsed()),
                tr.words().len()
            );
        }

        let t = Instant::now();
        let d = diarizer
            .diarize(audio, earshot_core::asr::MeetingTime::ZERO, &cancel)
            .expect("diarize");
        let diar = t.elapsed();
        total += diar;
        let (seg_t, seg_calls, windows) = seg_meter.take();
        let (emb_t, emb_calls, spans) = emb_meter.take();
        println!(
            "{name}: diarisation total      {}  {} speakers",
            secs(diar),
            d.speakers.len()
        );
        println!(
            "{name}:   segmentation         {}  {windows} windows in {seg_calls} calls",
            secs(seg_t)
        );
        println!(
            "{name}:   embeddings           {}  {spans} spans in {emb_calls} calls",
            secs(emb_t)
        );
        println!(
            "{name}:   clustering and rest  {}",
            secs(diar.saturating_sub(seg_t + emb_t))
        );
    }
    println!(
        "stages total (excludes echo check and store): {}",
        secs(total)
    );
}
