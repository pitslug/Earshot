//! The first-run model check run (T6.10c2): the hardware read, the bundled test clip, the
//! timed two-track trial, the search driver over [`crate::model_check`] and the device-local
//! record of the answer.
//!
//! - **Hardware** ([`hardware_facts`]): logical cores, total memory
//!   ([`earshot_store::total_memory`]) and the accelerators this build has on this machine
//!   ([`crate::backends::accelerator_order`]: CUDA in a `cuda` build, Vulkan in a `vulkan`
//!   build when whisper.cpp sees a Vulkan device). Free video memory is not read: the
//!   hardware read only chooses where to start, and a GPU that cannot hold a model fails its
//!   trial. Each trial reports how its backend was proven (whisper.cpp's log or ONNX
//!   Runtime's placement). The GPU driver versions ([`gpu_drivers`]) are read for the
//!   fingerprint only (PLAN T6.10c: a driver change re-runs the check).
//! - **Test clip** ([`TestClip::bundled`]): the 20 committed LibriSpeech smoke utterances
//!   (CC-BY-4.0, [`CLIP_ATTRIBUTION`]), embedded in the binary. Speaker 121 is the far end on
//!   the reference track and speaker 61 is the user on the mic track; they take turns
//!   ([`TURN_GAP`] apart). The far end leaks into the mic as an echo ([`ECHO_GAIN`],
//!   [`ECHO_DELAY`]), and the mic is cleaned by the production echo canceller, as in a
//!   meeting.
//! - **Trial** ([`run_trial`]): both tracks are transcribed at the same time, each on its own
//!   copy of the model with the T6.10b thread budget for two tracks, in Silero speech pieces
//!   with live (greedy) decoding: the during-meeting pass of D20. The speed is the slower
//!   track's decode time over the clip length; the WER pools both tracks against the
//!   transcripts (Whisper normalisation). A model that cannot load, or that reports another
//!   backend than requested, fails as wrong output (its backend is then skipped).
//! - **Record** ([`record_check`], [`stored_check`]): the answer and a fingerprint of what it
//!   depends on (build, hardware, meeting languages, installed models) in the device-local
//!   state; a different fingerprint means the check must run again ([`StoredCheck::current`]).

use std::io::Cursor;
use std::time::{Duration, Instant};

use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig};
use earshot_core::asr::{
    BackendKind, CancelToken, PieceConfig, TranscribeOptions, transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::metrics::wer::{WerCounts, word_errors_normalized};
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::backends::{
    Accelerator as Accel, EvidenceLoader, ModelDir, accelerator_order, asr_model,
    live_whisper_threads, load_silero, select_asr,
};
use crate::local::DeviceRecord;
use crate::model_check::{
    Accelerator, HardwareFacts, MODELS, ModelSearch, Recommendation, Step, Trial, Verdict,
};

/// Attribution of the embedded test clip.
pub const CLIP_ATTRIBUTION: &str = "Test speech: LibriSpeech ASR corpus (Vassil Panayotov, \
Guoguo Chen, Daniel Povey, Sanjeev Khudanpur), https://www.openslr.org/12, CC-BY-4.0, derived \
from LibriVox public-domain audiobooks";
/// Silence between turns.
pub const TURN_GAP: Duration = Duration::from_millis(400);
/// Echo level of the far end in the mic (-10 dB, a laptop's speakers near its microphone).
pub const ECHO_GAIN: f32 = 0.316;
/// Echo delay (about what a desktop measures).
pub const ECHO_DELAY: Duration = Duration::from_millis(180);

/// The speaker on the mic track (the user).
const NEAR_SPEAKER: &str = "61-";

const TRANSCRIPTS: &str = include_str!("../../../fixtures/smoke/transcripts.txt");
const CLIPS: [(&str, &[u8]); 20] = [
    (
        "121-121726-0002",
        include_bytes!("../../../fixtures/smoke/121-121726-0002.flac"),
    ),
    (
        "121-121726-0004",
        include_bytes!("../../../fixtures/smoke/121-121726-0004.flac"),
    ),
    (
        "121-121726-0005",
        include_bytes!("../../../fixtures/smoke/121-121726-0005.flac"),
    ),
    (
        "121-121726-0006",
        include_bytes!("../../../fixtures/smoke/121-121726-0006.flac"),
    ),
    (
        "121-121726-0008",
        include_bytes!("../../../fixtures/smoke/121-121726-0008.flac"),
    ),
    (
        "121-121726-0011",
        include_bytes!("../../../fixtures/smoke/121-121726-0011.flac"),
    ),
    (
        "121-121726-0012",
        include_bytes!("../../../fixtures/smoke/121-121726-0012.flac"),
    ),
    (
        "121-121726-0013",
        include_bytes!("../../../fixtures/smoke/121-121726-0013.flac"),
    ),
    (
        "121-121726-0014",
        include_bytes!("../../../fixtures/smoke/121-121726-0014.flac"),
    ),
    (
        "121-127105-0004",
        include_bytes!("../../../fixtures/smoke/121-127105-0004.flac"),
    ),
    (
        "61-70968-0000",
        include_bytes!("../../../fixtures/smoke/61-70968-0000.flac"),
    ),
    (
        "61-70968-0001",
        include_bytes!("../../../fixtures/smoke/61-70968-0001.flac"),
    ),
    (
        "61-70968-0002",
        include_bytes!("../../../fixtures/smoke/61-70968-0002.flac"),
    ),
    (
        "61-70968-0003",
        include_bytes!("../../../fixtures/smoke/61-70968-0003.flac"),
    ),
    (
        "61-70968-0004",
        include_bytes!("../../../fixtures/smoke/61-70968-0004.flac"),
    ),
    (
        "61-70968-0006",
        include_bytes!("../../../fixtures/smoke/61-70968-0006.flac"),
    ),
    (
        "61-70968-0007",
        include_bytes!("../../../fixtures/smoke/61-70968-0007.flac"),
    ),
    (
        "61-70968-0008",
        include_bytes!("../../../fixtures/smoke/61-70968-0008.flac"),
    ),
    (
        "61-70968-0009",
        include_bytes!("../../../fixtures/smoke/61-70968-0009.flac"),
    ),
    (
        "61-70968-0012",
        include_bytes!("../../../fixtures/smoke/61-70968-0012.flac"),
    ),
];

fn decode(bytes: &[u8]) -> Result<Vec<f32>, RuntimeError> {
    let bad = |_| RuntimeError::Internal("the embedded test clip does not decode");
    let mut reader = claxon::FlacReader::new(Cursor::new(bytes)).map_err(bad)?;
    let info = reader.streaminfo();
    if info.sample_rate != PIPELINE_RATE.hz() || info.channels != 1 || info.bits_per_sample != 16 {
        return Err(RuntimeError::Internal(
            "the embedded test clip is not 16 kHz mono 16-bit",
        ));
    }
    reader
        .samples()
        .map(|s| s.map(|v| v as f32 / 32768.0).map_err(bad))
        .collect()
}

/// The two-track test clip.
#[derive(Debug, Clone)]
pub struct TestClip {
    /// The mic track after the echo canceller.
    pub mic: MonoBuffer,
    /// The mic track before the canceller (the user plus the echo).
    pub raw_mic: MonoBuffer,
    /// The far end.
    pub reference: MonoBuffer,
    /// What the user says, in order.
    pub mic_text: String,
    /// What the far end says, in order.
    pub reference_text: String,
}

impl TestClip {
    /// The bundled clip (module docs).
    ///
    /// # Errors
    /// [`RuntimeError::Internal`] if an embedded clip does not decode.
    pub fn bundled() -> Result<Self, RuntimeError> {
        let text = |id: &str| {
            TRANSCRIPTS
                .lines()
                .find_map(|l| l.strip_prefix(id).and_then(|r| r.strip_prefix(' ')))
                .unwrap_or_default()
                .to_owned()
        };
        let (mut near, mut far) = (Vec::new(), Vec::new());
        for (id, bytes) in CLIPS {
            let entry = (decode(bytes)?, text(id));
            if id.starts_with(NEAR_SPEAKER) {
                near.push(entry);
            } else {
                far.push(entry);
            }
        }
        let gap = PIPELINE_RATE.frames_for(TURN_GAP);
        let (mut mic, mut reference) = (Vec::new(), Vec::new());
        let (mut mic_text, mut reference_text) = (Vec::new(), Vec::new());
        // Turns: far end, then the user, alternating; the other track is silent meanwhile.
        let turn = |on: &mut Vec<f32>, off: &mut Vec<f32>, audio: &[f32]| {
            on.extend_from_slice(audio);
            on.resize(on.len() + gap, 0.0);
            off.resize(on.len(), 0.0);
        };
        for k in 0..near.len().max(far.len()) {
            if let Some((audio, words)) = far.get(k) {
                turn(&mut reference, &mut mic, audio);
                reference_text.push(words.clone());
            }
            if let Some((audio, words)) = near.get(k) {
                turn(&mut mic, &mut reference, audio);
                mic_text.push(words.clone());
            }
        }
        // The echo: the far end, delayed and attenuated, added to the mic.
        let delay = PIPELINE_RATE.frames_for(ECHO_DELAY);
        let mut raw_mic = mic;
        raw_mic.resize(reference.len() + delay, 0.0);
        reference.resize(raw_mic.len(), 0.0);
        for (i, r) in reference.iter().enumerate() {
            if let Some(m) = raw_mic.get_mut(i + delay) {
                *m += ECHO_GAIN * r;
            }
        }
        // The production canceller; its output latency is removed as the pipeline does.
        let n = raw_mic.len().div_ceil(BLOCK) * BLOCK;
        raw_mic.resize(n, 0.0);
        reference.resize(n, 0.0);
        let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
        let latency = aec.output_latency();
        let mut out = aec.process_vec(&raw_mic, &reference);
        out.drain(..latency.min(out.len()));
        out.resize(n, 0.0);
        Ok(Self {
            mic: MonoBuffer::new(PIPELINE_RATE, out),
            raw_mic: MonoBuffer::new(PIPELINE_RATE, raw_mic),
            reference: MonoBuffer::new(PIPELINE_RATE, reference),
            mic_text: mic_text.join(" "),
            reference_text: reference_text.join(" "),
        })
    }

    /// The clip's length.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.mic.duration()
    }
}

/// What this machine and build offer the check (module docs).
#[must_use]
pub fn hardware_facts() -> HardwareFacts {
    let accelerators = accelerator_order()
        .into_iter()
        .filter(|a| *a != Accel::Cpu)
        .map(|a| Accelerator {
            backend: a.backend(),
            free_memory: None,
        })
        .collect();
    HardwareFacts {
        accelerators,
        cores: std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        // Unknown memory is not held against a model: the trial decides.
        memory: earshot_store::total_memory().unwrap_or(u64::MAX),
        drivers: gpu_drivers(),
    }
}

/// The display adapters' driver versions, sorted (module docs, Hardware). On Windows the
/// display driver package carries both the CUDA driver (`nvcuda.dll`) and the Vulkan
/// driver (its ICD), so an NVIDIA, AMD, Intel or Qualcomm driver update changes this list
/// (NVIDIA's `32.0.16.1714` is `nvidia-smi`'s 617.14). Read from the display class key of
/// the registry with `reg query` (at most [`DRIVER_PROBE_TIMEOUT`]), once per process (the
/// first answer, also a failure, is kept); `None` when the probe failed or timed out, so
/// a stored check is kept rather than re-run ([`StoredCheck::current`]); empty on other
/// platforms (not read).
#[must_use]
pub fn gpu_drivers() -> Option<Vec<String>> {
    static DRIVERS: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    DRIVERS.get_or_init(read_gpu_drivers).clone()
}

fn read_gpu_drivers() -> Option<Vec<String>> {
    #[cfg(windows)]
    {
        let args = ["query", DISPLAY_CLASS_KEY, "/s", "/v", "DriverVersion"];
        probe_output("reg", &args, DRIVER_PROBE_TIMEOUT).map(|out| parse_driver_versions(&out))
    }
    #[cfg(not(windows))]
    {
        Some(Vec::new())
    }
}

/// The Windows device class of display adapters.
#[cfg(windows)]
const DISPLAY_CLASS_KEY: &str =
    r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

/// How long [`gpu_drivers`] waits for its probe: `reg query` answers in milliseconds, but
/// on a loaded machine (a ci run) it took over 5 s once, and a probe that gives up changes
/// the fingerprint, which re-runs the check for nothing.
pub const DRIVER_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// A probe's standard output as text: `reg query` writes in the console code page (the
/// localized "End of search" trailer is not UTF-8 on a non-English Windows), and only its
/// ASCII tokens are read, so invalid bytes are replaced rather than failing.
#[must_use]
pub fn probe_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The `DriverVersion` values of `reg query <class> /s /v DriverVersion`, each as
/// `<adapter subkey>=<version>`, sorted.
#[must_use]
pub fn parse_driver_versions(out: &str) -> Vec<String> {
    let mut key = "";
    let mut found = Vec::new();
    for line in out.lines() {
        if line.starts_with("HKEY_") {
            key = line.rsplit('\\').next().unwrap_or("").trim();
            continue;
        }
        let mut fields = line.split_whitespace();
        if let (Some("DriverVersion"), Some("REG_SZ"), Some(version)) =
            (fields.next(), fields.next(), fields.next())
        {
            found.push(format!("{key}={version}"));
        }
    }
    found.sort();
    found
}

/// The standard output of `program args` if it exits successfully within `timeout`
/// (killed otherwise).
#[cfg(windows)]
fn probe_output(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read while waiting, so a full pipe never stalls the child.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| probe_text(&bytes))
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok()?.ok()?;
    status
        .filter(std::process::ExitStatus::success)
        .map(|_| out)
}

/// Whether every file of model `name` is in `models`.
#[must_use]
pub fn installed(models: &ModelDir, name: &str) -> bool {
    asr_model(name).is_ok_and(|m| m.files().iter().all(|f| models.root().join(f).is_file()))
}

/// A trial and what happened in it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrialReport {
    /// The measurement.
    pub trial: Trial,
    /// Why it failed to run, when it did not (load error, wrong backend).
    pub failure: Option<String>,
    /// Model load time (both copies).
    pub load: Duration,
    /// How the backend was proven at load (whisper.cpp's log or ONNX Runtime's placement),
    /// when the model loaded.
    pub evidence: Option<String>,
}

fn failed(model: &'static str, backend: BackendKind, why: String, load: Duration) -> TrialReport {
    TrialReport {
        trial: Trial {
            model,
            backend,
            rtf: f64::NAN,
            wer_percent: f64::NAN,
        },
        failure: Some(why),
        load,
        evidence: None,
    }
}

/// Runs the clip through `model` on `backend` as a two-track load (module docs). ONNX
/// Runtime must be initialised with a library that has the backend's provider.
#[must_use]
pub fn run_trial(
    models: &ModelDir,
    model: &'static str,
    backend: BackendKind,
    clip: &TestClip,
) -> TrialReport {
    let started = Instant::now();
    let Some(accel) = Accel::of(backend) else {
        return failed(
            model,
            backend,
            format!("no {backend:?} support"),
            Duration::ZERO,
        );
    };
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let loader = EvidenceLoader::for_live(2);
    let threads = |b: BackendKind| Some(live_whisper_threads(b, cores, 2).get());
    let mut tracks = Vec::new();
    for _ in 0..2 {
        let t = select_asr(&loader, models, model, &[accel], &threads)
            .map(|c| c.transcriber)
            .map_err(|e| e.to_string());
        let vad = load_silero(models, Accel::Cpu).map_err(|e| e.to_string());
        match (t, vad) {
            (Ok(t), Ok(v)) => tracks.push((t, v)),
            (Err(e), _) | (_, Err(e)) => return failed(model, backend, e, started.elapsed()),
        }
    }
    let load = started.elapsed();
    let evidence = tracks.first().map(|(t, _)| t.backend_evidence());
    let cancel = CancelToken::new();
    let options = TranscribeOptions::live();
    let config = PieceConfig::default();
    let audio = [&clip.mic, &clip.reference];
    let results: Vec<Result<(String, Duration, BackendKind), String>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = tracks
                .iter_mut()
                .zip(audio)
                .map(|((t, vad), audio)| {
                    let (cancel, options, config) = (&cancel, &options, &config);
                    scope.spawn(move || {
                        let started = Instant::now();
                        let (out, _) = transcribe_in_pieces(t, vad, audio, options, config, cancel)
                            .map_err(|e| e.to_string())?;
                        let text: Vec<&str> =
                            out.segments.iter().map(|s| s.text.as_str()).collect();
                        Ok((text.join(" "), started.elapsed(), out.backend.kind))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err("a trial thread panicked".into()))
                })
                .collect()
        });
    let mut counts = WerCounts::default();
    let mut slowest = Duration::ZERO;
    for (r, reference) in results
        .into_iter()
        .zip([&clip.mic_text, &clip.reference_text])
    {
        match r {
            Ok((text, elapsed, ran_on)) if ran_on == backend => {
                counts += word_errors_normalized(reference, &text);
                slowest = slowest.max(elapsed);
            }
            Ok((_, _, ran_on)) => {
                return failed(model, backend, format!("ran on {ran_on:?}"), load);
            }
            Err(e) => return failed(model, backend, e, load),
        }
    }
    TrialReport {
        trial: Trial {
            model,
            backend,
            rtf: slowest.as_secs_f64() / clip.duration().as_secs_f64().max(1e-9),
            wer_percent: counts.wer().map_or(f64::NAN, |w| w * 100.0),
        },
        failure: None,
        load,
        evidence,
    }
}

/// The outcome of a model check.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckOutcome {
    /// What the hardware read found.
    pub facts: HardwareFacts,
    /// Every trial in order, with its verdict.
    pub trials: Vec<(TrialReport, Verdict)>,
    /// The answer (`None`: no installed model works for these languages here).
    pub recommendation: Option<Recommendation>,
}

/// Runs the check for meetings in `languages` (module docs), calling `progress` before each
/// trial and after it.
///
/// # Errors
/// [`RuntimeError::Internal`] if the embedded clip does not decode.
pub fn run_check(
    models: &ModelDir,
    languages: &[&str],
    progress: &mut dyn FnMut(&CheckProgress),
) -> Result<CheckOutcome, RuntimeError> {
    let clip = TestClip::bundled()?;
    let facts = hardware_facts();
    let mut search = ModelSearch::new(&facts, languages, &|m| installed(models, m));
    let mut trials = Vec::new();
    loop {
        match search.next() {
            Step::Test { model, backend } => {
                progress(&CheckProgress::Testing { model, backend });
                let report = run_trial(models, model, backend, &clip);
                search.record(report.trial.clone());
                let verdict = search.trials().last().map_or(Verdict::Wrong, |(_, v)| *v);
                progress(&CheckProgress::Tested {
                    report: report.clone(),
                    verdict,
                });
                trials.push((report, verdict));
            }
            Step::Done(recommendation) => {
                return Ok(CheckOutcome {
                    facts,
                    trials,
                    recommendation,
                });
            }
        }
    }
}

/// Progress of [`run_check`].
#[derive(Debug, Clone, PartialEq)]
pub enum CheckProgress {
    /// A trial starts.
    Testing {
        /// The model.
        model: &'static str,
        /// The backend.
        backend: BackendKind,
    },
    /// A trial ended.
    Tested {
        /// What it measured.
        report: TrialReport,
        /// What it means.
        verdict: Verdict,
    },
}

/// The recorded answer of a check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCheck {
    /// What the answer depends on ([`check_fingerprint`]).
    pub fingerprint: String,
    /// The model to use.
    pub model: String,
    /// The backend it passed on.
    pub backend: BackendKind,
    /// The fallback when a meeting falls behind.
    pub fallback: Option<String>,
    /// Every trial of the check and its verdict, in order (T7.4d: where "too slow here"
    /// comes from). Empty in a record written before T7.4d.
    #[serde(default)]
    pub trials: Vec<StoredTrial>,
}

/// One trial of a recorded check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTrial {
    /// The model.
    pub model: String,
    /// The backend it was tested on.
    pub backend: BackendKind,
    /// What the trial said.
    pub verdict: StoredVerdict,
}

/// A trial's [`Verdict`] as recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredVerdict {
    /// Correct and fast enough.
    Pass,
    /// Correct but too slow on this machine.
    TooSlow,
    /// Wrong output (or it did not run): the backend is broken for it.
    Wrong,
}

impl From<Verdict> for StoredVerdict {
    fn from(v: Verdict) -> Self {
        match v {
            Verdict::Pass { .. } => Self::Pass,
            Verdict::TooSlow => Self::TooSlow,
            Verdict::Wrong => Self::Wrong,
        }
    }
}

impl StoredCheck {
    /// The record of a finished check: its answer and every trial.
    #[must_use]
    pub fn new(
        fingerprint: String,
        recommendation: &Recommendation,
        trials: &[(TrialReport, Verdict)],
    ) -> Self {
        Self {
            fingerprint,
            model: recommendation.model.to_owned(),
            backend: recommendation.backend,
            fallback: recommendation.fallback.map(str::to_owned),
            trials: trials
                .iter()
                .map(|(r, v)| StoredTrial {
                    model: r.trial.model.to_owned(),
                    backend: r.trial.backend,
                    verdict: (*v).into(),
                })
                .collect(),
        }
    }

    /// What this check found for `model` on `backend`: the trial's verdict (the first
    /// trial of a pair tested twice, as the search judged it:
    /// `model_check::pair_result`), else [`StoredVerdict::Pass`] for the answer
    /// itself (a record written before T7.4d holds no trials), else `None` (not tested).
    #[must_use]
    pub fn verdict(&self, model: &str, backend: BackendKind) -> Option<StoredVerdict> {
        crate::model_check::pair_result(
            self.trials
                .iter()
                .map(|t| (t.model.as_str(), t.backend, t.verdict)),
            model,
            backend,
        )
        .or_else(|| (self.model == model && self.backend == backend).then_some(StoredVerdict::Pass))
    }

    /// Whether the answer still holds for `fingerprint`. A fingerprint whose driver
    /// versions could not be read ([`gpu_drivers`] is `None`) matches a stored one that
    /// differs only in its driver versions: a failed probe is not a driver change.
    #[must_use]
    pub fn current(&self, fingerprint: &str) -> bool {
        if self.fingerprint == fingerprint {
            return true;
        }
        let unknown = format!("{DRIVERS_FIELD}{UNKNOWN_DRIVERS}|");
        fingerprint.contains(&unknown)
            && without_drivers(&self.fingerprint) == without_drivers(fingerprint)
    }
}

/// The fingerprint field of the driver versions.
const DRIVERS_FIELD: &str = "|drivers ";
/// The driver versions in a fingerprint when they could not be read.
const UNKNOWN_DRIVERS: &str = "?";

/// `fingerprint` with its driver versions blanked.
fn without_drivers(fingerprint: &str) -> String {
    let Some(start) = fingerprint.find(DRIVERS_FIELD) else {
        return fingerprint.to_owned();
    };
    let from = start + DRIVERS_FIELD.len();
    let end = fingerprint[from..]
        .find('|')
        .map_or(fingerprint.len(), |i| from + i);
    format!("{}{}", &fingerprint[..from], &fingerprint[end..])
}

/// What a check's answer depends on: this build (version and accelerators), the hardware
/// (cores, memory, GPU driver versions), the meeting languages, the installed models and the
/// installed NPU versions (ARM64: whether a model can run on the NPU, TODO 77). Any change
/// means the check runs again.
#[must_use]
pub fn check_fingerprint(models: &ModelDir, facts: &HardwareFacts, languages: &[&str]) -> String {
    let npu = crate::catalogue::Catalogue::embedded()
        .map(|c| installed_npu_versions(c, models))
        .unwrap_or_default();
    check_fingerprint_with(models, facts, languages, &npu)
}

/// The NPU versions of `catalogue` installed in `models` (sizes only), in manifest order;
/// always empty where NPU versions are not offered.
fn installed_npu_versions(
    catalogue: &crate::catalogue::Catalogue,
    models: &ModelDir,
) -> Vec<String> {
    catalogue
        .models_of(crate::catalogue::ModelKind::NpuVersion)
        .filter(|m| catalogue.is_installed(models, &m.name))
        .map(|m| m.name.clone())
        .collect()
}

/// [`check_fingerprint`] with the installed NPU versions given: none adds no field, so a
/// fingerprint where no NPU version is installed is the one stored before they counted.
fn check_fingerprint_with(
    models: &ModelDir,
    facts: &HardwareFacts,
    languages: &[&str],
    npu: &[String],
) -> String {
    let installed: Vec<&str> = MODELS
        .iter()
        .map(|m| m.name)
        .filter(|m| installed(models, m))
        .collect();
    let accels: Vec<String> = facts
        .accelerators
        .iter()
        .map(|a| format!("{:?}", a.backend))
        .collect();
    let mut langs: Vec<String> = languages
        .iter()
        .map(|l| crate::model_check::language_code(l))
        .collect();
    langs.sort();
    langs.dedup();
    format!(
        "v{}|{}|cores {}|memory {}|drivers {}|languages {}|installed {}",
        env!("CARGO_PKG_VERSION"),
        accels.join(","),
        facts.cores,
        facts.memory,
        facts
            .drivers
            .as_ref()
            .map_or_else(|| UNKNOWN_DRIVERS.to_owned(), |d| d.join(",")),
        langs.join(","),
        installed.join(",")
    ) + &if npu.is_empty() {
        String::new()
    } else {
        format!("|npu {}", npu.join(","))
    }
}

/// Records `check` in the device-local state.
///
/// # Errors
/// Store errors.
pub fn record_check(store: &mut Store, check: &StoredCheck) -> Result<(), RuntimeError> {
    let value = serde_json::to_vec(check)
        .map_err(|_| RuntimeError::Internal("the model check does not serialise"))?;
    crate::local::set(store, &DeviceRecord::ModelCheck.key(), &value)?;
    Ok(())
}

/// The recorded answer, if any.
///
/// # Errors
/// Store errors; [`RuntimeError::Internal`] for a value that does not parse.
pub fn stored_check(store: &Store) -> Result<Option<StoredCheck>, RuntimeError> {
    store
        .local_value(DeviceRecord::ModelCheck.key().as_str())?
        .map(|v| {
            serde_json::from_slice(&v)
                .map_err(|_| RuntimeError::Internal("the stored model check does not parse"))
        })
        .transpose()
}

#[cfg(test)]
#[path = "model_bench_tests.rs"]
mod tests;
