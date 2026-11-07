//! Why one far-end voice comes out as several speakers (T6.9). Counts only.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example far_speakers -- <repo-root> \
//!     --models <dir> [--recording <label>=<raw recording dir>]... \
//!     [--dev <librispeech dev-clean speakers dir> --dev-speakers <n>] [--out <file.json>]
//! ```
//!
//! No transcript text, display name or audio is printed or written. Word text is read only to
//! ask whether a word ends a sentence, so `recording` output is times, counts and similarities.
//!
//! `--recording`: a raw recording's reference track as the pipeline delivers it (real device
//! timestamps, `LiveConfig::default().pipeline`), quantised as the FLAC writer stores it,
//! diarised with the final pass's setup (`load_diarisation` on the CPU, `PostConfig` defaults,
//! the calibrated threshold) and decoded with the production final-pass model.
//!
//! `--dev`: LibriSpeech dev-clean only. Long scenes the T3.2b calibration never had (it used
//! 15 s of speech per speaker and no one-speaker scene):
//! - `one`: one speaker for [`DEV_SCENE_S`];
//! - `even`: two speakers in alternating turns, half each;
//! - `dominant`: two speakers, the second about a tenth of the speech.
//!
//! Sensitivity tables re-run the production `cluster_embeddings` over the embeddings the
//! diarisation computed, at other thresholds and minimum cluster sizes. On a private recording
//! they are a description only: a threshold is never chosen from them (AGENTS.md). The dev
//! scenes are the data a fix task may use, under a rule written before measuring. The dev run
//! of T6.9 is committed unmodified as `docs/far-speakers/dev-clean.json`.

#[allow(dead_code)] // shared with asr_eval; only `kept_words` is used here
#[path = "../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use earshot_core::asr::{
    BackendKind, CancelToken, MeetingTime, PieceConfig, TranscribeOptions, Word,
    transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample, quantise_pcm16, read_wav};
use earshot_core::capture::{
    AudioSource, RawTimeline, ReplayBuilder, SourceStatus, TrackBuffer, TrackId,
};
use earshot_core::diar::{
    ClusteringConfig, DiarError, Diarization, DiarizationConfig, Diarizer, EmbedderInfo, Embedding,
    LocalActivity, SegmenterInfo, SpeakerEmbedder, SpeakerSegmenter, SpeakerTurn, assign_words,
    cluster_embeddings,
};
use earshot_core::pipeline::{Pipeline, PipelineEvent};
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, final_pass_asr, init_onnx, load_diarisation,
    load_silero, onnx_library,
};
use earshot_runtime::live::LiveConfig;
use earshot_runtime::post::PostConfig;
use serde_json::{Value, json};

/// The production default English model on the desktop (gates/GATE-12.md, scoring).
const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v2";
/// Distances looked at in the sensitivity tables. 0.68 is the calibration; 0.68-0.85 is its
/// "valley" (`CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD` docs).
const THRESHOLDS: [f32; 8] = [0.60, 0.68, 0.75, 0.80, 0.85, 0.90, 1.00, 1.15];
/// Length of one dev scene.
const DEV_SCENE_S: usize = 360;
/// Earlier cuts of a dev scene, by share of its embeddings.
const DEV_CUTS_S: [usize; 4] = [30, 60, 180, 360];
/// Pause between the utterances of a dev stream (ms).
const DEV_GAP_MS: usize = 200;
/// Bin for "where in the track is this speaker" (s).
const PRESENCE_BIN_S: u64 = 30;
/// A speaker change between words with less than this pause is a cut inside speech (ms).
const NO_PAUSE_MS: u64 = 300;
/// Embedding length buckets (s).
const LENGTH_EDGES_S: [f32; 3] = [1.0, 3.0, 6.0];

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("far_speakers: {msg}");
    std::process::exit(1);
}

struct Args {
    models: ModelDir,
    recordings: Vec<(String, PathBuf)>,
    dev: Option<PathBuf>,
    dev_speakers: usize,
    out: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let _root = it
        .next()
        .unwrap_or_else(|| fail("usage: far_speakers <repo-root> --models <dir>"));
    let (mut models, mut dev, mut out) = (None, None, None);
    let mut recordings = Vec::new();
    let mut dev_speakers = 20;
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .unwrap_or_else(|| fail(format!("{flag} needs a value")));
        match flag.as_str() {
            "--models" => models = Some(ModelDir::new(value)),
            "--recording" => {
                let (label, dir) = value
                    .split_once('=')
                    .unwrap_or_else(|| fail("--recording takes <label>=<dir>"));
                recordings.push((label.to_owned(), PathBuf::from(dir)));
            }
            "--dev" => dev = Some(PathBuf::from(value)),
            "--dev-speakers" => {
                dev_speakers = value
                    .parse()
                    .unwrap_or_else(|e| fail(format!("--dev-speakers: {e}")));
            }
            "--out" => out = Some(PathBuf::from(value)),
            other => fail(format!("unknown argument {other}")),
        }
    }
    Args {
        models: models.unwrap_or_else(|| fail("--models is required")),
        recordings,
        dev,
        dev_speakers,
        out,
    }
}

// ---- recording wrappers ------------------------------------------------------------------

/// A segmenter that also counts active local speakers per window.
struct CountingSegmenter<S> {
    inner: S,
    onset: f32,
    /// Windows by number of active local speakers.
    locals_per_window: BTreeMap<usize, usize>,
}

impl<S: SpeakerSegmenter> SpeakerSegmenter for CountingSegmenter<S> {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError> {
        let out = self.inner.segment(windows, cancel)?;
        for a in &out {
            let active = (0..a.speakers())
                .filter(|&s| (0..a.frames()).any(|f| a.get(f, s) >= self.onset))
                .count();
            *self.locals_per_window.entry(active).or_insert(0) += 1;
        }
        Ok(out)
    }
}

/// An embedder that also keeps every valid embedding and its input length, in pipeline order.
struct RecordingEmbedder<E> {
    inner: E,
    /// (unit-free vector, seconds of speech embedded).
    seen: Vec<(Vec<f32>, f32)>,
}

impl<E: SpeakerEmbedder> SpeakerEmbedder for RecordingEmbedder<E> {
    fn info(&self) -> &EmbedderInfo {
        self.inner.info()
    }
    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        let out = self.inner.embed(speech, cancel)?;
        for (e, s) in out.iter().zip(speech) {
            if e.is_valid() {
                self.seen
                    .push((e.vector.clone(), s.len() as f32 / PIPELINE_RATE.hz() as f32));
            }
        }
        Ok(out)
    }
}

type Prod = Diarizer<
    CountingSegmenter<earshot_onnx::segment::PyannoteSegmenter>,
    RecordingEmbedder<earshot_onnx::embed::CamppEmbedder>,
>;

/// One diarisation with what the wrappers saw.
struct Run {
    diar: Diarization,
    locals_per_window: BTreeMap<usize, usize>,
    embeddings: Vec<(Vec<f32>, f32)>,
}

fn diarise(d: Prod, audio: &[f32], config: &DiarizationConfig) -> (Run, Prod) {
    let mut d = d;
    let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
    let diar = d
        .diarize(&buffer, MeetingTime::ZERO, &CancelToken::new())
        .unwrap_or_else(|e| fail(format!("diarize: {e}")));
    let (mut seg, mut emb) = d.into_parts();
    let run = Run {
        diar,
        locals_per_window: std::mem::take(&mut seg.locals_per_window),
        embeddings: std::mem::take(&mut emb.seen),
    };
    let d = Diarizer::new(seg, emb, config.clone()).unwrap_or_else(|e| fail(e));
    (run, d)
}

// ---- clustering views --------------------------------------------------------------------

/// Number of clusters at `threshold` and `min_cluster_size` over `embeddings`.
fn clusters_at(embeddings: &[(Vec<f32>, f32)], threshold: f32, min_size: usize) -> usize {
    let vectors: Vec<Vec<f32>> = embeddings.iter().map(|(v, _)| v.clone()).collect();
    let cfg = ClusteringConfig {
        threshold,
        min_cluster_size: min_size,
        ..ClusteringConfig::default()
    };
    cluster_embeddings(&vectors, &cfg)
        .unwrap_or_else(|e| fail(format!("cluster: {e}")))
        .centroids
        .len()
}

/// Clusters per threshold, with the production minimum size and with none.
fn sensitivity(embeddings: &[(Vec<f32>, f32)], min_size: usize) -> Value {
    let rows: Vec<Value> = THRESHOLDS
        .iter()
        .map(|&t| {
            json!({
                "threshold": t,
                "clusters": clusters_at(embeddings, t, min_size),
                "clusters_before_small_ones_are_absorbed": clusters_at(embeddings, t, 1),
            })
        })
        .collect();
    json!(rows)
}

/// Cosine similarity of two embeddings of one model.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm(a) * norm(b)).max(1e-12)
}

fn unit(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Per cluster at the production settings, and the linkage's own distance between each pair.
fn cluster_table(embeddings: &[(Vec<f32>, f32)], cfg: &ClusteringConfig) -> Value {
    let vectors: Vec<Vec<f32>> = embeddings.iter().map(|(v, _)| v.clone()).collect();
    let c = cluster_embeddings(&vectors, cfg).unwrap_or_else(|e| fail(format!("cluster: {e}")));
    let k = c.centroids.len();
    let units: Vec<Vec<f32>> = vectors.iter().map(|v| unit(v)).collect();
    let dim = units.first().map_or(0, Vec::len);
    let mut means = vec![vec![0f64; dim]; k];
    let mut sizes = vec![0usize; k];
    let mut lengths = vec![[0usize; LENGTH_EDGES_S.len() + 1]; k];
    let mut own = vec![Vec::new(); k];
    for (i, &l) in c.labels.iter().enumerate() {
        sizes[l] += 1;
        for (m, x) in means[l].iter_mut().zip(&units[i]) {
            *m += f64::from(*x);
        }
        let bucket = LENGTH_EDGES_S
            .iter()
            .position(|&e| embeddings[i].1 < e)
            .unwrap_or(LENGTH_EDGES_S.len());
        lengths[l][bucket] += 1;
        own[l].push(f64::from(cosine_similarity(&units[i], &c.centroids[l])));
    }
    for (m, &n) in means.iter_mut().zip(&sizes) {
        for x in m.iter_mut() {
            *x /= n.max(1) as f64;
        }
    }
    let clusters: Vec<Value> = (0..k)
        .map(|l| {
            let mut sims = own[l].clone();
            sims.sort_by(f64::total_cmp);
            json!({
                "cluster": l,
                "embeddings": sizes[l],
                "embedding_seconds_under_1_1_3_3_6_over_6": lengths[l],
                "cosine_to_own_centroid_p10": round3(sims.get(sims.len() / 10).copied().unwrap_or(0.0)),
                "cosine_to_own_centroid_median": round3(sims.get(sims.len() / 2).copied().unwrap_or(0.0)),
            })
        })
        .collect();
    let mut pairs = Vec::new();
    for a in 0..k {
        for b in a + 1..k {
            let euclid = means[a]
                .iter()
                .zip(&means[b])
                .map(|(x, y)| (x - y) * (x - y))
                .sum::<f64>()
                .sqrt();
            pairs.push(json!({
                "clusters": [a, b],
                "linkage_distance": round3(euclid),
                "centroid_cosine": round3(f64::from(cosine_similarity(&c.centroids[a], &c.centroids[b]))),
            }));
        }
    }
    json!({ "clusters": clusters, "pairs": pairs })
}

// ---- turns and words ---------------------------------------------------------------------

fn turn_table(diar: &Diarization, track_ms: u64) -> Value {
    let k = diar.speakers.len();
    let bins = (track_ms / (PRESENCE_BIN_S * 1000)) as usize + 1;
    let mut lengths = vec![Vec::new(); k];
    let mut present = vec![vec![false; bins]; k];
    for t in &diar.turns {
        let (a, b) = (t.start.as_millis(), t.end.as_millis());
        lengths[t.speaker].push(b.saturating_sub(a));
        let (first, last) = (
            (a / (PRESENCE_BIN_S * 1000)) as usize,
            (b / (PRESENCE_BIN_S * 1000)) as usize,
        );
        for slot in present[t.speaker].iter_mut().take(last + 1).skip(first) {
            *slot = true;
        }
    }
    let speakers: Vec<Value> = (0..k)
        .map(|s| {
            let mut l = lengths[s].clone();
            l.sort_unstable();
            json!({
                "speaker": s,
                "speech_s": round3(diar.speakers[s].speech.as_secs_f64()),
                "turns": l.len(),
                "median_turn_ms": l.get(l.len() / 2),
                "presence_30s_bins": present[s].iter().map(|&p| if p { '#' } else { '.' }).collect::<String>(),
            })
        })
        .collect();
    let mut sorted: Vec<&SpeakerTurn> = diar.turns.iter().collect();
    sorted.sort_by_key(|t| (t.start.as_millis(), t.speaker));
    let switches = sorted
        .windows(2)
        .filter(|w| w[0].speaker != w[1].speaker)
        .count();
    let overlap_ms: u64 = sorted
        .windows(2)
        .map(|w| {
            w[0].end
                .as_millis()
                .saturating_sub(w[1].start.as_millis())
                .min(w[1].end.as_millis().saturating_sub(w[1].start.as_millis()))
        })
        .sum();
    json!({
        "speakers": speakers,
        "turn_changes": switches,
        "turn_changes_per_minute": round3(switches as f64 * 60_000.0 / track_ms.max(1) as f64),
        "overlapped_s_between_neighbouring_turns": round3(overlap_ms as f64 / 1000.0),
    })
}

fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', ')'])
        .ends_with(['.', '?', '!'])
}

/// How words fall on speakers: never any word's text in the output.
fn word_table(words: &[Word], turns: &[SpeakerTurn], k: usize) -> Value {
    let labels = assign_words(words, turns);
    let mut per_speaker = vec![0usize; k];
    let mut unassigned = 0;
    for l in &labels {
        match l {
            Some(s) => per_speaker[*s] += 1,
            None => unassigned += 1,
        }
    }
    let (mut changes, mut no_pause, mut mid_sentence, mut mid_no_pause) = (0, 0, 0, 0);
    let mut gap_buckets = [0usize; 4];
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let Some(s) = labels[i] else { continue };
        match runs.last_mut() {
            Some((last, n)) if *last == s => *n += 1,
            _ => runs.push((s, 1)),
        }
        if i == 0 {
            continue;
        }
        let Some(p) = labels[i - 1] else { continue };
        if p == s {
            continue;
        }
        changes += 1;
        let gap = w
            .start
            .as_millis()
            .saturating_sub(words[i - 1].end.as_millis());
        let mid = !ends_sentence(&words[i - 1].text);
        no_pause += usize::from(gap < NO_PAUSE_MS);
        mid_sentence += usize::from(mid);
        mid_no_pause += usize::from(mid && gap < NO_PAUSE_MS);
        let b = match gap {
            0..100 => 0,
            100..300 => 1,
            300..1000 => 2,
            _ => 3,
        };
        gap_buckets[b] += 1;
    }
    // A run of at most 3 words with the same speaker on both sides: a flip, not a turn.
    let flips = runs
        .windows(3)
        .filter(|w| w[0].0 == w[2].0 && w[1].1 <= 3)
        .count();
    let straddling = words
        .iter()
        .filter(|w| {
            let (a, b) = (w.start.as_millis(), w.end.as_millis());
            let mut on: Vec<usize> = turns
                .iter()
                .filter(|t| t.start.as_millis() < b && a < t.end.as_millis())
                .map(|t| t.speaker)
                .collect();
            on.sort_unstable();
            on.dedup();
            on.len() > 1
        })
        .count();
    json!({
        "words": words.len(),
        "words_per_speaker": per_speaker,
        "words_without_a_speaker": unassigned,
        "sentence_ends": words.iter().filter(|w| ends_sentence(&w.text)).count(),
        "speaker_changes_between_words": changes,
        "changes_gap_under_100_100_300_300_1000_over_1000_ms": gap_buckets,
        "changes_with_no_pause": no_pause,
        "changes_inside_a_sentence": mid_sentence,
        "changes_inside_a_sentence_with_no_pause": mid_no_pause,
        "flips_of_at_most_3_words": flips,
        "words_touching_two_speakers": straddling,
    })
}

// ---- recordings --------------------------------------------------------------------------

fn read_track(path: &Path) -> TrackBuffer {
    let file = File::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let wav =
        read_wav(BufReader::new(file)).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    TrackBuffer::try_from(wav).unwrap_or_else(|e| fail(format!("{}: {e:?}", path.display())))
}

/// The reference track as a session delivers it.
fn reference_track(dir: &Path) -> Vec<f32> {
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
    let mut pipeline = Pipeline::new(source.session().clone(), LiveConfig::default().pipeline)
        .unwrap_or_else(|e| fail(format!("pipeline: {e:?}")));
    let mut reference = Vec::new();
    loop {
        let status = source
            .pump(&mut pipeline)
            .unwrap_or_else(|e| fail(format!("replay pump: {e:?}")));
        for event in pipeline.poll() {
            if let PipelineEvent::Audio(a) = event
                && a.track == TrackId::Reference
            {
                let start = usize::try_from(a.start).unwrap_or(usize::MAX);
                if reference.len() < start + a.samples.len() {
                    reference.resize(start + a.samples.len(), 0.0);
                }
                reference[start..start + a.samples.len()].copy_from_slice(&a.samples);
            }
        }
        if status == SourceStatus::Ended && pipeline.is_ended() {
            break;
        }
    }
    reference
}

// ---- dev scenes --------------------------------------------------------------------------

fn read_flac(path: &Path) -> Vec<f32> {
    let mut r =
        claxon::FlacReader::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let scale = 1.0 / f64::from(1u32 << (r.streaminfo().bits_per_sample - 1));
    r.samples()
        .map(|s| {
            (f64::from(s.unwrap_or_else(|e| fail(format!("{}: {e}", path.display())))) * scale)
                as f32
        })
        .collect()
}

/// A speaker's utterances in path order, joined with a short pause, up to `want` samples.
fn dev_stream(dir: &Path, want: usize) -> Vec<f32> {
    fn entries(d: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .unwrap_or_else(|e| fail(format!("{}: {e}", d.display())))
            .map(|e| e.unwrap_or_else(|e| fail(e)).path())
            .collect();
        v.sort();
        v
    }
    let gap = vec![0f32; PIPELINE_RATE.hz() as usize * DEV_GAP_MS / 1000];
    let mut out = Vec::new();
    for chapter in entries(dir).into_iter().filter(|p| p.is_dir()) {
        for flac in entries(&chapter)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        {
            out.extend(read_flac(&flac));
            out.extend_from_slice(&gap);
            if out.len() >= want {
                out.truncate(want);
                return out;
            }
        }
    }
    out
}

/// The next of a fixed sequence of turn lengths in `[lo, hi)` (s): deterministic and
/// independent of the audio.
fn next_turn(state: &mut u64, lo: usize, hi: usize) -> usize {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    lo + ((*state >> 33) as usize) % (hi - lo)
}

/// Two streams in turns of `a_turn` and `b_turn` seconds, with a short silence at each change.
fn two_speaker_scene(
    a: &[f32],
    b: &[f32],
    a_turn: (usize, usize),
    b_turn: (usize, usize),
    seed: u64,
) -> (Vec<f32>, usize) {
    let hz = PIPELINE_RATE.hz() as usize;
    let want = DEV_SCENE_S * hz;
    let gap = vec![0f32; hz * DEV_GAP_MS / 1000];
    let mut state = seed;
    let (mut ia, mut ib) = (0, 0);
    let mut out = Vec::with_capacity(want);
    let mut true_b = 0;
    while out.len() < want {
        let n = next_turn(&mut state, a_turn.0, a_turn.1) * hz;
        let take = n.min(a.len() - ia);
        out.extend_from_slice(&a[ia..ia + take]);
        ia += take;
        out.extend_from_slice(&gap);
        let n = next_turn(&mut state, b_turn.0, b_turn.1) * hz;
        let take_b = n.min(b.len() - ib);
        out.extend_from_slice(&b[ib..ib + take_b]);
        ib += take_b;
        true_b += take_b;
        out.extend_from_slice(&gap);
        if take == 0 && take_b == 0 {
            break;
        }
    }
    out.truncate(want);
    (out, true_b / hz)
}

struct DevRow {
    scene: String,
    speakers: usize,
    length_s: usize,
    found: usize,
    second_voice_s: usize,
    locals: BTreeMap<usize, usize>,
    /// (cut s, threshold, clusters with the production minimum size).
    grid: Vec<(usize, f32, usize)>,
}

fn dev_row(
    kind: &str,
    speakers: usize,
    audio: &[f32],
    second_voice_s: usize,
    d: Prod,
    config: &DiarizationConfig,
) -> (DevRow, Prod) {
    let audio = quantise_pcm16(audio);
    let (run, d) = diarise(d, &audio, config);
    let hz = PIPELINE_RATE.hz() as usize;
    let length_s = audio.len() / hz;
    let mut grid = Vec::new();
    for cut in DEV_CUTS_S {
        if cut > length_s {
            continue;
        }
        let n = run.embeddings.len() * cut / length_s;
        for &t in &THRESHOLDS {
            grid.push((
                cut,
                t,
                clusters_at(&run.embeddings[..n], t, config.clustering.min_cluster_size),
            ));
        }
    }
    (
        DevRow {
            scene: kind.to_owned(),
            speakers,
            length_s,
            found: run.diar.speakers.len(),
            second_voice_s,
            locals: run.locals_per_window,
            grid,
        },
        d,
    )
}

/// `one`, `even`, `dominant`: mean found count and how often it is right, per cut and threshold.
fn dev_summary(rows: &[DevRow]) -> Value {
    let mut out = Vec::new();
    for kind in ["one", "even", "dominant"] {
        let of: Vec<&DevRow> = rows.iter().filter(|r| r.scene == kind).collect();
        if of.is_empty() {
            continue;
        }
        let mut grid = Vec::new();
        for cut in DEV_CUTS_S {
            for &t in &THRESHOLDS {
                let values: Vec<(usize, usize)> = of
                    .iter()
                    .flat_map(|r| {
                        r.grid
                            .iter()
                            .filter(|g| g.0 == cut && g.1 == t)
                            .map(|g| (g.2, r.speakers))
                    })
                    .collect();
                if values.is_empty() {
                    continue;
                }
                let n = values.len() as f64;
                grid.push(json!({
                    "cut_s": cut,
                    "threshold": t,
                    "scenes": values.len(),
                    "mean_clusters": round3(values.iter().map(|v| v.0 as f64).sum::<f64>() / n),
                    "right_count": values.iter().filter(|v| v.0 == v.1).count(),
                    "too_many": values.iter().filter(|v| v.0 > v.1).count(),
                    "too_few": values.iter().filter(|v| v.0 < v.1).count(),
                }));
            }
        }
        let (mut windows, mut multi) = (0usize, 0usize);
        for r in &of {
            for (&a, &n) in &r.locals {
                windows += n;
                if a >= 2 {
                    multi += n;
                }
            }
        }
        out.push(json!({
            "scene": kind,
            "scenes": of.len(),
            "length_s_min": of.iter().map(|r| r.length_s).min(),
            "second_voice_s_mean": of.iter().map(|r| r.second_voice_s).sum::<usize>() / of.len(),
            "found_by_the_pipeline_at_the_calibrated_threshold": of.iter().map(|r| r.found).collect::<Vec<_>>(),
            "windows_with_2_or_more_local_speakers_percent": round3(100.0 * multi as f64 / windows.max(1) as f64),
            "grid": grid,
        }));
    }
    json!(out)
}

fn run_dev(dir: &Path, count: usize, mut d: Prod, config: &DiarizationConfig) -> Value {
    let hz = PIPELINE_RATE.hz() as usize;
    let mut speakers: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())))
        .map(|e| e.unwrap_or_else(|e| fail(e)).path())
        .filter(|p| p.is_dir())
        .collect();
    speakers.sort();
    speakers.truncate(count);
    let streams: Vec<Vec<f32>> = speakers
        .iter()
        .map(|s| dev_stream(s, DEV_SCENE_S * hz))
        .collect();
    let mut rows = Vec::new();
    for (i, s) in streams.iter().enumerate() {
        eprintln!("far_speakers: dev one {i}");
        let (row, next) = dev_row("one", 1, s, 0, d, config);
        rows.push(row);
        d = next;
    }
    for (p, pair) in streams.as_chunks::<2>().0.iter().enumerate() {
        eprintln!("far_speakers: dev pair {p}");
        let seed = 6_900 + p as u64;
        let (even, b_s) = two_speaker_scene(&pair[0], &pair[1], (5, 30), (5, 30), seed);
        let (row, next) = dev_row("even", 2, &even, b_s, d, config);
        rows.push(row);
        d = next;
        let (dom, b_s) = two_speaker_scene(&pair[0], &pair[1], (40, 90), (4, 12), seed);
        let (row, next) = dev_row("dominant", 2, &dom, b_s, d, config);
        rows.push(row);
        d = next;
    }
    dev_summary(&rows)
}

fn main() {
    let args = parse_args();
    init_onnx(
        &args.models,
        onnx_library(DEFAULT_MODEL, &[Accelerator::Cpu]),
    )
    .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let (segmenter, embedder) =
        load_diarisation(&args.models, Accelerator::Cpu).unwrap_or_else(|e| fail(e));
    let model = embedder.info().model.clone();
    let post = PostConfig::default();
    let threshold = post
        .clustering_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    let voiceprint = post
        .voiceprint_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    let mut config = post.diarization.clone();
    config.clustering.threshold = threshold;
    let onset = config.onset;
    let mut d: Prod = Diarizer::new(
        CountingSegmenter {
            inner: segmenter,
            onset,
            locals_per_window: BTreeMap::new(),
        },
        RecordingEmbedder {
            inner: embedder,
            seen: Vec::new(),
        },
        config.clone(),
    )
    .unwrap_or_else(|e| fail(e));

    let mut recordings = Vec::new();
    let mut all_centroids: Vec<(String, Vec<f32>)> = Vec::new();
    if !args.recordings.is_empty() {
        let choice = final_pass_asr(
            &EvidenceLoader::default(),
            &args.models,
            DEFAULT_MODEL,
            &[Accelerator::Cpu],
        )
        .unwrap_or_else(|e| fail(format!("final-pass model: {e}")));
        if choice.backend != BackendKind::Cpu {
            fail("the final-pass model is not on the CPU");
        }
        let mut transcriber = choice.transcriber;
        let mut vad = load_silero(&args.models, Accelerator::Cpu)
            .unwrap_or_else(|e| fail(format!("Silero: {e}")));
        for (label, dir) in &args.recordings {
            eprintln!("far_speakers: {label}");
            let audio = quantise_pcm16(&reference_track(dir));
            let track_ms = (audio.len() as u64 * 1000) / u64::from(PIPELINE_RATE.hz());
            let (run, next) = diarise(d, &audio, &config);
            d = next;
            let buffer = MonoBuffer::new(PIPELINE_RATE, audio);
            let (t, _) = transcribe_in_pieces(
                &mut transcriber,
                &mut vad,
                &buffer,
                &TranscribeOptions::live(),
                &PieceConfig::default(),
                &CancelToken::new(),
            )
            .unwrap_or_else(|e| fail(format!("pieces: {e}")));
            let words = refs::kept_words(&t);
            let k = run.diar.speakers.len();
            for (i, s) in run.diar.speakers.iter().enumerate() {
                all_centroids.push((format!("{label}.{i}"), s.centroid.vector.clone()));
            }
            let reclustered = clusters_at(
                &run.embeddings,
                threshold,
                config.clustering.min_cluster_size,
            );
            recordings.push(json!({
                "label": label,
                "track_s": round3(track_ms as f64 / 1000.0),
                "speakers": k,
                "reclustered_speakers_must_equal_speakers": reclustered,
                "active_local_speakers_per_window": run.locals_per_window,
                "embeddings": run.embeddings.len(),
                "turns": turn_table(&run.diar, track_ms),
                "clusters": cluster_table(&run.embeddings, &config.clustering),
                "words": word_table(&words, &run.diar.turns, k),
                "sensitivity_description_only": sensitivity(&run.embeddings, config.clustering.min_cluster_size),
            }));
        }
    }

    let mut across = Vec::new();
    for (i, (a, x)) in all_centroids.iter().enumerate() {
        for (b, y) in &all_centroids[i + 1..] {
            across.push(
                json!({"a": a, "b": b, "cosine": round3(f64::from(cosine_similarity(x, y)))}),
            );
        }
    }

    let dev = args
        .dev
        .as_ref()
        .map(|dir| run_dev(dir, args.dev_speakers, d, &config));

    let report = json!({
        "model": model.to_string(),
        "cluster_threshold": threshold,
        "voiceprint_threshold": voiceprint,
        "min_cluster_size": config.clustering.min_cluster_size,
        "recordings": recordings,
        "centroid_cosine_across_everything": across,
        "dev": dev,
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    if let Some(out) = &args.out {
        std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
    }
    println!("{text}");
}
