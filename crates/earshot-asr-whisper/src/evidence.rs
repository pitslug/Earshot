//! Backend evidence from whisper.cpp's own log.
//!
//! whisper.cpp falls back to the CPU when a selected GPU cannot initialise and only logs
//! `whisper_backend_init_gpu: failed to initialize`, so the backend a report prints is
//! trusted only when [`WhisperTranscriber::backend_info`](earshot_core::asr::Transcriber::backend_info)
//! and whisper.cpp's log agree (AGENTS.md 11). This module is the one place that rule
//! lives: the CLI, this crate's tests and `cargo xtask accept m2` all use it.
//!
//! - [`capture_logs`] routes whisper.cpp and ggml logs (whisper-rs `log_backend`) into a
//!   process-wide buffer through the `log` facade. It installs the process logger, so a
//!   program that wants evidence must not install another one. Only whisper-rs targets
//!   are captured (other crates' records, for example network connection lines, are
//!   ignored), and the buffer keeps the last [`MAX_CAPTURED_LINES`] lines;
//!   [`logs_since_complete`] refuses a window that lost lines.
//! - [`classify_whisper_log`] reads the backend from the lines of one model load.
//! - [`check_backend`] fails unless the expected backend, `backend_info()` and the log
//!   agree; [`LogBackend::Unknown`] is always an error, never a CPU fallback.
//! - [`load_verified`] does all three for one model load.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use earshot_core::asr::{AsrError, BackendInfo, BackendKind, Transcriber};

use crate::{WhisperConfig, WhisperTranscriber};

/// Which backend whisper.cpp's own log says it initialised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogBackend {
    /// `whisper_backend_init_gpu: no GPU found`, and no GPU was picked.
    Cpu,
    /// `whisper_backend_init_gpu: using CUDA<n> backend` plus model weights in a CUDA
    /// buffer, and no failed initialisation.
    Cuda,
    /// `whisper_backend_init_gpu: using Vulkan<n> backend` plus model weights in a Vulkan
    /// buffer (`whisper_model_load: Vulkan<n> total size`), and no failed initialisation
    /// (T6.10d; lines as whisper.cpp 1.8.3 logs them on the desktop's RTX 5080).
    Vulkan,
    /// Anything else: no evidence (log not captured), contradictory lines, or a GPU that
    /// was picked and then failed to initialise.
    Unknown,
}

impl LogBackend {
    /// Lower-case name for reports (`cpu`, `cuda`, `vulkan`, `unknown`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            LogBackend::Cpu => "cpu",
            LogBackend::Cuda => "cuda",
            LogBackend::Vulkan => "vulkan",
            LogBackend::Unknown => "unknown",
        }
    }

    /// The log classification `kind` needs, `None` for a backend without a log rule.
    #[must_use]
    pub fn of(kind: BackendKind) -> Option<LogBackend> {
        match kind {
            BackendKind::Cpu => Some(LogBackend::Cpu),
            BackendKind::Cuda => Some(LogBackend::Cuda),
            BackendKind::Vulkan => Some(LogBackend::Vulkan),
            _ => None,
        }
    }
}

/// The ggml device-name prefix of each GPU backend with a log rule.
const GPU_PREFIXES: [(&str, LogBackend); 2] =
    [("CUDA", LogBackend::Cuda), ("Vulkan", LogBackend::Vulkan)];

/// The ggml device prefix `kind` selects in `whisper_backend_init_gpu`, if it is a GPU
/// backend with a log rule.
fn gpu_prefix(kind: BackendKind) -> Option<&'static str> {
    let log = LogBackend::of(kind)?;
    GPU_PREFIXES
        .iter()
        .find(|(_, l)| *l == log)
        .map(|(p, _)| *p)
}

/// Classifies whisper.cpp's log of one model load and returns the lines that decided it
/// (backend initialisation and model buffer placement).
#[must_use]
pub fn classify_whisper_log(lines: &[String]) -> (LogBackend, Vec<String>) {
    let relevant: Vec<String> = lines
        .iter()
        .filter(|l| {
            l.contains("whisper_backend_init_gpu")
                || (l.contains("whisper_model_load") && l.contains("total size"))
        })
        .cloned()
        .collect();
    let cpu = relevant.iter().any(|l| {
        l.contains("whisper_backend_init_gpu: no GPU found")
            || l.contains("whisper_backend_init_gpu: failed to initialize")
    });
    let picked_gpu = relevant
        .iter()
        .any(|l| l.contains("whisper_backend_init_gpu: using"));
    // Which GPU families were selected and which hold weights: exactly one family on both
    // sides is evidence; anything mixed (a build with CUDA and Vulkan) is contradictory.
    let families = |pred: &dyn Fn(&str, &str) -> bool| -> Vec<LogBackend> {
        GPU_PREFIXES
            .iter()
            .filter(|(p, _)| relevant.iter().any(|l| pred(l, p)))
            .map(|(_, k)| *k)
            .collect()
    };
    let init = families(&|l, p| l.contains(&format!("whisper_backend_init_gpu: using {p}")));
    let weights = families(&|l, p| l.contains("whisper_model_load") && l.contains(p));
    let kind = match (init.as_slice(), weights.as_slice()) {
        ([i], [w]) if i == w && !cpu => *i,
        ([], []) if cpu && !picked_gpu => LogBackend::Cpu,
        _ => LogBackend::Unknown,
    };
    (kind, relevant)
}

/// Fails unless `expected`, the transcriber's [`BackendInfo`] and whisper.cpp's log all
/// name the same backend. CPU, CUDA and Vulkan have a log rule; any other expected backend
/// is an error.
///
/// # Errors
/// [`AsrError::Backend`] describing what each source reported.
pub fn check_backend(
    expected: BackendKind,
    info: &BackendInfo,
    log: LogBackend,
) -> Result<(), AsrError> {
    if LogBackend::of(expected) == Some(log) && info.kind == expected {
        return Ok(());
    }
    Err(AsrError::Backend(format!(
        "expected whisper.cpp on {expected:?}, but backend_info() reports {:?} on `{}` and \
         whisper.cpp's log says {} (whisper.cpp falls back to the CPU when a GPU cannot \
         initialise; an unknown log is never taken as the CPU)",
        info.kind,
        info.device,
        log.name()
    )))
}

/// Checks the log of one transcription call (lines captured during the call). A
/// beam-search call creates a fresh whisper state, which re-runs
/// `whisper_backend_init_gpu`; a GPU that fails to initialise there falls back to the CPU
/// for that call only, so a per-call log must not contradict the load's backend. A call
/// that initialised nothing (greedy, same state) passes.
///
/// # Errors
/// [`AsrError::Backend`] naming the contradicting line.
pub fn check_call_log(expected: BackendKind, lines: &[String]) -> Result<(), AsrError> {
    let init: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("whisper_backend_init_gpu"))
        .collect();
    let bad = init.iter().find(|l| {
        l.contains("failed to initialize")
            || match expected {
                BackendKind::Cpu => l.contains("whisper_backend_init_gpu: using"),
                _ => l.contains("no GPU found"),
            }
    });
    if let Some(line) = bad {
        return Err(AsrError::Backend(format!(
            "a transcription call did not run on {expected:?}: `{line}`"
        )));
    }
    if LogBackend::of(expected).is_none() {
        return Err(AsrError::Backend(format!(
            "no log rule for {expected:?} yet"
        )));
    }
    if let Some(prefix) = gpu_prefix(expected) {
        let wanted = format!("whisper_backend_init_gpu: using {prefix}");
        if let Some(line) = init
            .iter()
            .find(|l| l.contains("whisper_backend_init_gpu: using") && !l.contains(&wanted))
        {
            return Err(AsrError::Backend(format!(
                "a transcription call did not run on {expected:?}: `{line}`"
            )));
        }
        if !init.is_empty() && !init.iter().any(|l| l.contains(&wanted)) {
            return Err(AsrError::Backend(format!(
                "a transcription call re-initialised its backend without selecting {expected:?}"
            )));
        }
    }
    Ok(())
}

/// What one model load proved about its backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendEvidence {
    /// The log classification.
    pub log: LogBackend,
    /// The log lines that decided it.
    pub lines: Vec<String>,
}

/// Most lines kept: older ones are dropped (a model load logs about a hundred).
pub const MAX_CAPTURED_LINES: usize = 4096;

/// The `log` targets captured: whisper-rs forwards whisper.cpp and ggml lines from its
/// `whisper_logging_hook` and `ggml_logging_hook` modules. Records of every other crate
/// (for example reqwest's connection lines) are neither captured nor echoed.
const CAPTURED_TARGET: &str = "whisper_rs";

/// A bounded ring of captured lines; `start` is the absolute index of `lines[0]`, so marks
/// stay valid while old lines are dropped.
struct Ring {
    lines: VecDeque<String>,
    start: usize,
}

struct LogCapture {
    ring: Mutex<Ring>,
    echo: AtomicBool,
}

impl log::Log for LogCapture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target().starts_with(CAPTURED_TARGET)
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("{}", record.args());
        let line = line.trim_end().to_owned();
        if self.echo.load(Ordering::Relaxed) {
            eprintln!("{line}");
        }
        if let Ok(mut ring) = self.ring.lock() {
            ring.lines.push_back(line);
            while ring.lines.len() > MAX_CAPTURED_LINES {
                ring.lines.pop_front();
                ring.start += 1;
            }
        }
    }

    fn flush(&self) {}
}

static CAPTURE: LogCapture = LogCapture {
    ring: Mutex::new(Ring {
        lines: VecDeque::new(),
        start: 0,
    }),
    echo: AtomicBool::new(false),
};
static INSTALLED: Mutex<bool> = Mutex::new(false);

fn install() -> Result<(), AsrError> {
    let mut installed = INSTALLED
        .lock()
        .map_err(|_| AsrError::Backend("whisper log capture lock poisoned".into()))?;
    if !*installed {
        log::set_logger(&CAPTURE).map_err(|e| {
            AsrError::Backend(format!(
                "cannot install the whisper.cpp log capture (another logger is set): {e}"
            ))
        })?;
        log::set_max_level(log::LevelFilter::Trace);
        whisper_rs::install_logging_hooks();
        *installed = true;
    }
    Ok(())
}

/// Routes whisper.cpp and ggml logs into the process-wide capture buffer, echoing each
/// line to stderr when `echo` is set. Safe to call more than once (later calls only change
/// `echo`).
///
/// # Errors
/// [`AsrError::Backend`] if another `log` logger is already installed.
pub fn capture_logs(echo: bool) -> Result<(), AsrError> {
    CAPTURE.echo.store(echo, Ordering::Relaxed);
    install()
}

/// Number of lines captured so far (dropped ones included): a mark for [`logs_since`].
#[must_use]
pub fn log_mark() -> usize {
    CAPTURE.ring.lock().map_or(0, |r| r.start + r.lines.len())
}

/// Lines captured after `mark` that are still kept (best effort; see
/// [`logs_since_complete`]).
#[must_use]
pub fn logs_since(mark: usize) -> Vec<String> {
    CAPTURE
        .ring
        .lock()
        .map(|r| {
            let skip = mark.saturating_sub(r.start);
            r.lines.iter().skip(skip).cloned().collect()
        })
        .unwrap_or_default()
}

/// Every line captured after `mark`, or `None` if some of them were already dropped
/// (more than [`MAX_CAPTURED_LINES`] since the mark): evidence must not be read from an
/// incomplete window.
#[must_use]
pub fn logs_since_complete(mark: usize) -> Option<Vec<String>> {
    let ring = CAPTURE.ring.lock().ok()?;
    if mark < ring.start {
        return None;
    }
    Some(ring.lines.iter().skip(mark - ring.start).cloned().collect())
}

/// Every line still kept.
#[must_use]
pub fn captured_logs() -> Vec<String> {
    logs_since(0)
}

/// Loads a model with the log captured and checks the backend with [`check_backend`].
/// Installs the capture if needed (without changing its echo setting). The evidence covers
/// the lines logged during this call, so loads that need evidence must not run
/// concurrently with other whisper.cpp loads in the process.
///
/// # Errors
/// Any [`WhisperTranscriber::load`] error; [`AsrError::Backend`] if the capture cannot be
/// installed or the backend check fails.
pub fn load_verified(
    config: &WhisperConfig,
    expected: BackendKind,
) -> Result<(WhisperTranscriber, BackendEvidence), AsrError> {
    install()?;
    let mark = log_mark();
    let transcriber = WhisperTranscriber::load(config)?;
    let window = logs_since_complete(mark).ok_or_else(|| {
        AsrError::Backend("the whisper log of this load overflowed the capture".into())
    })?;
    let (log, lines) = classify_whisper_log(&window);
    check_backend(expected, &transcriber.backend_info(), log)?;
    Ok((transcriber, BackendEvidence { log, lines }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn evidence_classifies_the_whisper_log() {
        let cpu = lines(&[
            "whisper_init_from_file_with_params_no_state: loading model",
            "whisper_backend_init_gpu: no GPU found",
            "whisper_model_load:      CPU total size =    77.11 MB",
        ]);
        let (kind, ev) = classify_whisper_log(&cpu);
        assert_eq!(kind, LogBackend::Cpu);
        assert_eq!(ev.len(), 2, "{ev:?}");

        let cuda = lines(&[
            "whisper_backend_init_gpu: using CUDA0 backend",
            "whisper_model_load:    CUDA0 total size =    77.11 MB",
        ]);
        assert_eq!(classify_whisper_log(&cuda).0, LogBackend::Cuda);

        // A GPU that was selected but failed to initialise ran on the CPU, but the log is
        // contradictory, so it is not accepted as evidence of either.
        let failed = lines(&[
            "whisper_backend_init_gpu: using CUDA0 backend",
            "whisper_backend_init_gpu: failed to initialize CUDA0 backend",
            "whisper_model_load:      CPU total size =    77.11 MB",
        ]);
        assert_eq!(classify_whisper_log(&failed).0, LogBackend::Unknown);

        // CUDA init without the weights on the GPU is not proof of a CUDA run.
        let no_weights = lines(&["whisper_backend_init_gpu: using CUDA0 backend"]);
        assert_eq!(classify_whisper_log(&no_weights).0, LogBackend::Unknown);
        assert_eq!(classify_whisper_log(&[]).0, LogBackend::Unknown);

        // A Vulkan run is Vulkan evidence, never CUDA.
        let vulkan = lines(&[
            "whisper_backend_init_gpu: using Vulkan0 backend",
            "whisper_model_load:  Vulkan0 total size =    77.11 MB",
        ]);
        assert_eq!(classify_whisper_log(&vulkan).0, LogBackend::Vulkan);
    }

    #[test]
    fn evidence_classifies_a_vulkan_log() {
        // whisper.cpp 1.8.3 on the RTX 5080 through Vulkan (T6.10d).
        let vulkan = lines(&[
            "whisper_backend_init_gpu: device 0: Vulkan0 (type: 1)",
            "whisper_backend_init_gpu: found GPU device 0: Vulkan0 (type: 1, cnt: 0)",
            "whisper_backend_init_gpu: using Vulkan0 backend",
            "whisper_model_load:      Vulkan0 total size =    77.11 MB",
        ]);
        assert_eq!(classify_whisper_log(&vulkan).0, LogBackend::Vulkan);
        assert_eq!(LogBackend::Vulkan.name(), "vulkan");

        // Vulkan init without the weights on the Vulkan device is not proof.
        let no_weights = lines(&["whisper_backend_init_gpu: using Vulkan0 backend"]);
        assert_eq!(classify_whisper_log(&no_weights).0, LogBackend::Unknown);
        let failed = lines(&[
            "whisper_backend_init_gpu: using Vulkan0 backend",
            "whisper_backend_init_gpu: failed to initialize Vulkan0 backend",
            "whisper_model_load:      CPU total size =    77.11 MB",
        ]);
        assert_eq!(classify_whisper_log(&failed).0, LogBackend::Unknown);
        // A build with CUDA and Vulkan: one device picked, the weights on another, or two
        // devices picked in one load, is contradictory.
        for mixed in [
            lines(&[
                "whisper_backend_init_gpu: using CUDA0 backend",
                "whisper_model_load:      Vulkan0 total size =    77.11 MB",
            ]),
            lines(&[
                "whisper_backend_init_gpu: using Vulkan0 backend",
                "whisper_model_load:        CUDA0 total size =    77.11 MB",
            ]),
            lines(&[
                "whisper_backend_init_gpu: using CUDA0 backend",
                "whisper_backend_init_gpu: using Vulkan0 backend",
                "whisper_model_load:        CUDA0 total size =    77.11 MB",
                "whisper_model_load:      Vulkan0 total size =    77.11 MB",
            ]),
        ] {
            assert_eq!(
                classify_whisper_log(&mixed).0,
                LogBackend::Unknown,
                "{mixed:?}"
            );
        }
    }

    fn info(kind: BackendKind) -> BackendInfo {
        BackendInfo {
            kind,
            device: "dev".into(),
            model: "ggml-tiny.en".into(),
        }
    }

    #[test]
    fn evidence_check_needs_expected_info_and_log_to_agree() {
        use BackendKind::{Cpu, Cuda, Vulkan};
        assert!(check_backend(Cpu, &info(Cpu), LogBackend::Cpu).is_ok());
        assert!(check_backend(Cuda, &info(Cuda), LogBackend::Cuda).is_ok());
        // Unknown is never a CPU fallback.
        assert!(check_backend(Cpu, &info(Cpu), LogBackend::Unknown).is_err());
        assert!(check_backend(Cuda, &info(Cuda), LogBackend::Unknown).is_err());
        // backend_info alone is not enough, nor is the log alone.
        assert!(check_backend(Cuda, &info(Cuda), LogBackend::Cpu).is_err());
        assert!(check_backend(Cuda, &info(Cpu), LogBackend::Cuda).is_err());
        assert!(check_backend(Cpu, &info(Cuda), LogBackend::Cpu).is_err());
        // Vulkan needs its own log rule (T6.10d); an unknown log is never Vulkan.
        assert!(check_backend(Vulkan, &info(Vulkan), LogBackend::Unknown).is_err());
        assert!(check_backend(Vulkan, &info(Vulkan), LogBackend::Vulkan).is_ok());
        assert!(check_backend(Vulkan, &info(Vulkan), LogBackend::Cuda).is_err());
        assert!(check_backend(Vulkan, &info(Cuda), LogBackend::Vulkan).is_err());
        assert!(check_backend(Cuda, &info(Vulkan), LogBackend::Vulkan).is_err());
        assert!(check_backend(Cuda, &info(Cuda), LogBackend::Vulkan).is_err());
        assert!(check_backend(Cpu, &info(Cpu), LogBackend::Vulkan).is_err());
        let e = check_backend(Cuda, &info(Cpu), LogBackend::Cpu)
            .unwrap_err()
            .to_string();
        assert!(e.contains("expected whisper.cpp on Cuda"), "{e}");
        assert!(e.contains("log says cpu"), "{e}");
    }

    #[test]
    fn evidence_call_log_must_not_contradict_the_backend() {
        use BackendKind::{Cpu, Cuda};
        let cuda_init = lines(&[
            "whisper_backend_init_gpu: device 0: CUDA0 (type: 1)",
            "whisper_backend_init_gpu: using CUDA0 backend",
        ]);
        assert!(check_call_log(Cuda, &cuda_init).is_ok());
        assert!(check_call_log(Cuda, &[]).is_ok(), "no re-initialisation");
        assert!(check_call_log(Cpu, &[]).is_ok());
        let failed = lines(&[
            "whisper_backend_init_gpu: using CUDA0 backend",
            "whisper_backend_init_gpu: failed to initialize CUDA0 backend",
        ]);
        assert!(check_call_log(Cuda, &failed).is_err());
        let none = lines(&["whisper_backend_init_gpu: no GPU found"]);
        assert!(check_call_log(Cuda, &none).is_err());
        assert!(check_call_log(Cpu, &none).is_ok());
        assert!(check_call_log(Cpu, &cuda_init).is_err());
        let other = lines(&["whisper_backend_init_gpu: device 0: CUDA0 (type: 1)"]);
        assert!(
            check_call_log(Cuda, &other).is_err(),
            "init without a selection"
        );
    }

    #[test]
    fn evidence_vulkan_call_log_must_not_contradict_the_backend() {
        use BackendKind::{Cpu, Cuda, Vulkan};
        let vulkan_init = lines(&[
            "whisper_backend_init_gpu: device 0: Vulkan0 (type: 1)",
            "whisper_backend_init_gpu: using Vulkan0 backend",
        ]);
        let cuda_init = lines(&["whisper_backend_init_gpu: using CUDA0 backend"]);
        assert!(check_call_log(Vulkan, &vulkan_init).is_ok());
        assert!(check_call_log(Vulkan, &[]).is_ok(), "no re-initialisation");
        assert!(check_call_log(Vulkan, &cuda_init).is_err(), "another GPU");
        assert!(check_call_log(Cuda, &vulkan_init).is_err(), "another GPU");
        assert!(check_call_log(Cpu, &vulkan_init).is_err());
        let failed = lines(&[
            "whisper_backend_init_gpu: using Vulkan0 backend",
            "whisper_backend_init_gpu: failed to initialize Vulkan0 backend",
        ]);
        assert!(check_call_log(Vulkan, &failed).is_err());
        let none = lines(&["whisper_backend_init_gpu: no GPU found"]);
        assert!(check_call_log(Vulkan, &none).is_err());
        let other = lines(&["whisper_backend_init_gpu: device 0: Vulkan0 (type: 1)"]);
        assert!(
            check_call_log(Vulkan, &other).is_err(),
            "init without a selection"
        );
        // Backends without a log rule stay errors.
        assert!(check_call_log(BackendKind::Qnn, &[]).is_err());
    }

    /// Tests that read or write the process-wide capture run one at a time.
    static CAPTURE_TESTS: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        CAPTURE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn evidence_logs_since_a_mark() {
        let _serial = serial();
        assert!(logs_since(usize::MAX).is_empty());
        let mark = log_mark();
        assert!(logs_since(mark).is_empty());
    }

    fn emit(target: &str, text: &str) {
        log::Log::log(
            &CAPTURE,
            &log::Record::builder()
                .target(target)
                .level(log::Level::Info)
                .args(format_args!("{text}"))
                .build(),
        );
    }

    #[test]
    fn evidence_capture_keeps_only_whisper_records() {
        let _serial = serial();
        let mark = log_mark();
        emit(
            "reqwest::connect",
            "starting new connection: https://llm.example.com/",
        );
        emit("rustls::client", "handshake");
        emit(
            "whisper_rs::whisper_logging_hook",
            "whisper_backend_init_gpu: no GPU found",
        );
        let got = logs_since(mark);
        assert!(got.iter().any(|l| l.contains("no GPU found")), "{got:?}");
        assert!(
            !got.iter()
                .any(|l| l.contains("llm.example.com") || l.contains("handshake")),
            "{got:?}"
        );
        assert!(!log::Log::enabled(
            &CAPTURE,
            &log::Metadata::builder().target("reqwest").build()
        ));
    }

    #[test]
    fn evidence_capture_is_bounded_and_refuses_incomplete_windows() {
        let _serial = serial();
        let mark = log_mark();
        for i in 0..MAX_CAPTURED_LINES + 100 {
            emit("whisper_rs::ggml_logging_hook", &format!("line {i}"));
        }
        let kept = CAPTURE.ring.lock().unwrap().lines.len();
        assert!(kept <= MAX_CAPTURED_LINES, "{kept}");
        assert!(
            log_mark() >= mark + MAX_CAPTURED_LINES + 100,
            "marks keep counting"
        );
        assert_eq!(
            logs_since_complete(mark),
            None,
            "lines of the window were dropped"
        );
        let recent = log_mark();
        emit("whisper_rs::ggml_logging_hook", "fresh");
        assert_eq!(logs_since_complete(recent), Some(vec!["fresh".to_owned()]));
    }
}
