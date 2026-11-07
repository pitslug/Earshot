//! Tests of GPU summaries in a child process. The fake child is this test binary run again on
//! [`summary_child_fake_child_for_the_tests`], which reads the job from its standard input as the
//! real child does and acts on the job's model id. Every name contains `summary`.

// One time chunk is what these jobs are about.
#![allow(clippy::single_range_in_vec_init)]

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use earshot_core::prompt::{ChatMessage, PromptVariant};
use earshot_llm_local::summarize::{Generate, StepTime};
use earshot_llm_local::{Generation, LlmError, Stop};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::RuntimeError;
use crate::Services;
use crate::summary::{
    LocalRunReport, LocalSummaryError, SummaryOutcome, SummaryRunReport, SummaryService,
};
use crate::testutil::{TempDir, meeting, services};

/// Set only for the fake child: run alone or with `--include-ignored`, the helper does nothing.
const FAKE_ENV: &str = "EARSHOT_TEST_SUMMARY_FAKE_CHILD";
/// The helper, as libtest names it.
const FAKE_TEST: &str = "summary_child::tests::summary_child_fake_child_for_the_tests";
/// The words of the meeting `testutil::meeting` writes that no error may hold.
const MEETING_WORDS: [&str; 5] = ["friday", "works", "release", "alice", "notes"];

fn fake_child() -> GpuChild {
    GpuChild {
        program: std::env::current_exe().unwrap(),
        args: ["--exact", FAKE_TEST, "--ignored", "--nocapture"]
            .map(OsString::from)
            .to_vec(),
        env: vec![(OsString::from(FAKE_ENV), OsString::from("1"))],
    }
}

/// A valid summary with this title.
fn titled(title: &str) -> SummaryV1 {
    SummaryV1 {
        title: title.to_owned(),
        tldr: "A short meeting.".to_owned(),
        topics: vec!["planning".to_owned()],
        ..SummaryV1::default()
    }
}

/// Runs in the child's process: never returns to libtest. It holds the "model file" open with no
/// sharing, so a test can see from outside when this process is gone.
#[test]
#[ignore = "helper: the summary child tests run it as their child process"]
fn summary_child_fake_child_for_the_tests() {
    use std::os::windows::fs::OpenOptionsExt;

    if std::env::var_os(FAKE_ENV).is_none() {
        return;
    }
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes).unwrap();
    let job: ChildJob = serde_json::from_slice(&bytes).unwrap();
    // The meeting's text is never on the command line.
    for a in std::env::args() {
        for word in MEETING_WORDS {
            assert!(!a.to_lowercase().contains(word), "{a}");
        }
    }
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&job.model_path)
        .unwrap();
    let out = Mutex::new(std::io::stdout());
    let loaded = ChildLine::Loaded {
        backend: BackendLine::Vulkan,
    };
    // The title proves the text arrived on standard input.
    let text = job
        .input
        .segments
        .first()
        .map_or(String::new(), |s| s.text.clone());
    let done = ChildLine::Done {
        summary: Box::new(titled(&format!("heard: {text}"))),
        backend: BackendLine::Vulkan,
        load_time: Duration::from_millis(7),
        report: Box::new(ReportLine {
            requests: 3,
            prompt_time: Duration::from_millis(11),
            steps: vec![StepLine {
                step: "single".to_owned(),
                wall: Duration::from_millis(13),
                requests: 3,
            }],
            ..ReportLine::default()
        }),
    };
    let forever = || -> ! {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    };
    match job.model_id.as_str() {
        "abort-at-once" => std::process::abort(),
        "abort" => {
            emit(&out, &loaded);
            std::process::abort();
        }
        "exit-silently" => {}
        "exit-after-loaded" => emit(&out, &loaded),
        "silent-load" => forever(),
        "stall" => {
            emit(&out, &loaded);
            forever();
        }
        "beating" => {
            emit(&out, &loaded);
            for _ in 0..200 {
                std::thread::sleep(Duration::from_millis(25));
                emit(&out, &ChildLine::Beat);
            }
            emit(&out, &done);
        }
        "done" => {
            emit(&out, &loaded);
            emit(&out, &done);
        }
        "report-folder" => {
            emit(&out, &loaded);
            emit(
                &out,
                &ChildLine::Done {
                    summary: Box::new(titled(&format!("folder: {:?}", job.llama_folder))),
                    backend: BackendLine::Cuda,
                    load_time: Duration::from_millis(7),
                    report: Box::default(),
                },
            );
        }
        "noisy" => {
            print_raw("ggml_vulkan: a native library writes here\n");
            emit(&out, &loaded);
            print_raw("no newline at the end of this one");
            emit(&out, &done);
            print_raw("and one more after the last line\n");
        }
        "garbage" => {
            emit(&out, &loaded);
            print_raw(&format!("\n{LINE_PREFIX}not json at all\n"));
            forever();
        }
        "two-finals" => {
            emit(&out, &done);
            emit(&out, &done);
        }
        "abort-after-done" => {
            emit(&out, &done);
            std::process::abort();
        }
        "stuck-after-done" => {
            emit(&out, &done);
            forever();
        }
        "fail-exit-after-done" => {
            emit(&out, &done);
            std::process::exit(7);
        }
        "refused" => emit(
            &out,
            &ChildLine::LoadRefused {
                message: "asked for the GPU, the load log shows the CPU".to_owned(),
            },
        ),
        "failed" => emit(
            &out,
            &ChildLine::Failed {
                message: "reduce step invalid after 3 requests".to_owned(),
            },
        ),
        other => panic!("no such fake child: {other}"),
    }
    std::process::exit(0);
}

fn print_raw(text: &str) {
    let mut out = std::io::stdout();
    out.write_all(text.as_bytes()).unwrap();
    out.flush().unwrap();
}

/// Answers on the processor; refuses to load on the GPU, which never happens in the parent.
struct CpuOnly {
    loads: Arc<AtomicUsize>,
    unloadable: bool,
}

struct CpuModel;

impl Generate for CpuModel {
    fn context_tokens(&self) -> usize {
        16_384
    }

    fn prompt_tokens(&self, m: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
        Ok(m.iter().map(|x| x.content.len() / 4).sum::<usize>() + prefix.len())
    }

    fn generate(
        &mut self,
        _: &[ChatMessage],
        _: &str,
        _: &str,
        _: usize,
        _: &CancelToken,
    ) -> Result<Generation, LlmError> {
        Ok(Generation {
            text: serde_json::to_string(&titled("on the processor")).unwrap(),
            stop: Stop::EndOfGeneration,
            prompt_tokens: 10,
            generated_tokens: 5,
            prompt_time: Duration::from_millis(1),
            generate_time: Duration::from_millis(1),
        })
    }
}

impl LoadLocal for CpuOnly {
    fn load(&self, config: &LocalSummaryConfig) -> Result<LoadedModel, RuntimeError> {
        assert!(!config.gpu, "the parent never loads the model on the GPU");
        self.loads.fetch_add(1, Ordering::SeqCst);
        if self.unloadable {
            return Err(RuntimeError::LocalSummary(LocalSummaryError::Load(
                LlmError::Load("a corrupt file".to_owned()),
            )));
        }
        Ok(LoadedModel {
            heartbeat: Heartbeat::default(),
            model: Box::new(CpuModel),
            backend: LlamaBackend::Cpu,
            load_time: Duration::from_millis(1),
        })
    }
}

/// One fake child's service: its session flag and the count of CPU loads.
struct Fixture {
    _dir: TempDir,
    svc: Services,
    id: earshot_core::identity::EntityId,
    service: SummaryService,
    cpu_loads: Arc<AtomicUsize>,
    flag: &'static AtomicBool,
    file: PathBuf,
}

fn config_for(dir: &TempDir, mode: &str) -> (LocalSummaryConfig, PathBuf) {
    let file = dir.0.join(format!("{mode}.gguf"));
    std::fs::write(&file, b"a stand-in for a model").unwrap();
    let mut cfg = LocalSummaryConfig::new(&file, mode);
    cfg.gpu = true;
    cfg.cpu_fallback = true;
    cfg.gpu_child = Some(fake_child());
    // Wide: a child starting on a busy machine is not what these tests judge.
    cfg.load_stall_limit = Duration::from_secs(60);
    cfg.stall_limit = Duration::from_secs(60);
    (cfg, file)
}

fn fixture_with(mode: &str, edit: impl FnOnce(&mut LocalSummaryConfig)) -> Fixture {
    let dir = TempDir::new("summary-child");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 300);
    let (mut cfg, file) = config_for(&dir, mode);
    edit(&mut cfg);
    let cpu_loads = Arc::new(AtomicUsize::new(0));
    let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let service = SummaryService::local_flagged(
        &cfg,
        Box::new(CpuOnly {
            loads: Arc::clone(&cpu_loads),
            unloadable: false,
        }),
        flag,
    )
    .unwrap();
    Fixture {
        _dir: dir,
        svc,
        id,
        service,
        cpu_loads,
        flag,
        file,
    }
}

fn fixture(mode: &str) -> Fixture {
    fixture_with(mode, |_| {})
}

impl Fixture {
    fn summarize(&self) -> Result<SummaryOutcome, RuntimeError> {
        self.summarize_cancelled_by(&CancelToken::new())
    }

    fn summarize_cancelled_by(&self, cancel: &CancelToken) -> Result<SummaryOutcome, RuntimeError> {
        self.service.summarize(
            &self.svc,
            self.id,
            &mut ChaCha8Rng::seed_from_u64(301),
            cancel,
        )
    }

    fn stored(&self) -> Vec<String> {
        use earshot_core::model::MeetingRepo;
        self.svc
            .store()
            .summaries(self.id)
            .unwrap()
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .map(|s| s.summary.title)
            .collect()
    }

    /// Whether the fake child holds its file now.
    fn held(&self) -> bool {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&self.file)
            .is_err()
    }
}

fn local(out: &SummaryOutcome) -> &LocalRunReport {
    match &out.report {
        SummaryRunReport::Local(r) => r,
        other => panic!("{other:?}"),
    }
}

/// A summary that moved to the processor: its note, what is stored, the transcript left alone.
fn assert_moved_to_the_cpu(f: &Fixture, out: &SummaryOutcome) -> LocalSummaryError {
    let r = local(out);
    assert_eq!(r.backend, LlamaBackend::Cpu);
    assert_eq!(out.summary.title, "on the processor");
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 1);
    assert_eq!(f.stored(), ["on the processor"]);
    let view = crate::view::MeetingView::load(&**f.svc.store(), f.id)
        .unwrap()
        .unwrap();
    assert!(!view.segments.is_empty(), "the transcript is safe");
    let why = (*r.gpu_refused.clone().expect("the move is never silent")).clone();
    let text = why.to_string().to_lowercase();
    for word in MEETING_WORDS {
        assert!(!text.contains(word), "{word} in {text}");
    }
    why
}

/// What an aborted child reads as: its exit code, or, while Windows Error Reporting holds the
/// process past [`KILL_WAIT`], no exit in time. Either way it is a dead child.
const ABORTED: &[&str] = &["0xc0000409", "did not exit"];

fn assert_child_died(mode: &str, expected: &[&str]) {
    let f = fixture(mode);
    let out = f.summarize().unwrap();
    let why = assert_moved_to_the_cpu(&f, &out);
    let LocalSummaryError::GpuChildDied { why } = why else {
        panic!("{mode}: {why:?}");
    };
    assert!(expected.iter().any(|e| why.contains(e)), "{mode}: {why}");
    // A dead child costs only this summary.
    assert!(!f.flag.load(Ordering::Acquire), "{mode}");
    assert!(!f.held(), "{mode}: the child is gone");
}

#[test]
fn summary_child_abort_moves_the_summary_to_the_cpu_and_says_so() {
    // What ggml's abort looks like from outside: a fast fail, 0xC0000409, with no error of any kind.
    assert_child_died("abort", ABORTED);
    assert_child_died("abort-at-once", ABORTED);
}

#[test]
fn summary_child_that_ends_without_a_final_line_moves_to_the_cpu() {
    assert_child_died("exit-silently", &["before it finished"]);
    assert_child_died("exit-after-loaded", &["before it finished"]);
}

#[test]
fn summary_child_whose_output_cannot_be_read_moves_to_the_cpu() {
    assert_child_died("garbage", &["cannot read"]);
    assert_child_died("two-finals", &["after its last line"]);
}

#[test]
fn summary_child_that_fails_after_its_last_line_still_is_a_dead_child() {
    assert_child_died("abort-after-done", ABORTED);
    assert_child_died("fail-exit-after-done", &["0x7"]);
}

#[test]
fn summary_child_stuck_after_its_last_line_is_killed() {
    assert_child_died("stuck-after-done", &["did not exit"]);
}

#[test]
fn summary_child_that_cannot_start_moves_to_the_cpu() {
    let f = fixture_with("done", |cfg| {
        cfg.gpu_child.as_mut().unwrap().program = PathBuf::from("no-such-program-here.exe");
    });
    let out = f.summarize().unwrap();
    let why = assert_moved_to_the_cpu(&f, &out);
    assert!(
        matches!(why, LocalSummaryError::GpuChildDied { ref why } if why.contains("could not start")),
        "{why:?}"
    );
}

/// A child that stalls after `at` is gone after the kill: its exclusive hold on a file ends,
/// which only its exit does. The control: it held that file while it ran.
fn assert_stalled_child_is_gone(mode: &str, edit: impl FnOnce(&mut LocalSummaryConfig)) {
    let f = Arc::new(fixture_with(mode, edit));
    let watcher = {
        let f = Arc::clone(&f);
        std::thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(60) {
                if f.held() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        })
    };
    let out = f.summarize().unwrap();
    let held_while_running = watcher.join().unwrap();
    assert!(held_while_running, "{mode}: the control saw the child run");
    assert!(
        !f.held(),
        "{mode}: the stalled child is gone after the kill"
    );
    let why = assert_moved_to_the_cpu(&f, &out);
    assert_eq!(why, LocalSummaryError::GpuChildStalled, "{mode}");
    // Its memory is free again: the card is tried again next time.
    assert!(!f.flag.load(Ordering::Acquire), "{mode}");
}

#[test]
fn summary_child_stalled_after_its_load_is_killed_and_moves_to_the_cpu() {
    assert_stalled_child_is_gone("stall", |cfg| {
        cfg.stall_limit = Duration::from_millis(800);
    });
}

#[test]
fn summary_child_stalled_in_its_load_is_killed_and_moves_to_the_cpu() {
    assert_stalled_child_is_gone("silent-load", |cfg| {
        // Long enough for the watcher to see the child hold its file on a busy machine.
        cfg.load_stall_limit = Duration::from_secs(3);
    });
}

#[test]
fn summary_child_a_slow_child_that_reports_progress_is_not_a_stall() {
    // 200 beats 25 ms apart: 5 s, more than twice the limit.
    let f = fixture_with("beating", |cfg| {
        cfg.stall_limit = Duration::from_secs(2);
    });
    let out = f.summarize().unwrap();
    assert_eq!(local(&out).backend, LlamaBackend::Vulkan);
    assert_eq!(local(&out).gpu_refused, None);
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
}

#[test]
fn summary_child_only_a_child_that_will_not_die_turns_the_card_off() {
    let flag = AtomicBool::new(false);
    assert_eq!(
        crate::summary::after_child_stall(true, &flag),
        LocalSummaryError::GpuChildStalled
    );
    assert!(!flag.load(Ordering::Acquire));
    assert_eq!(
        crate::summary::after_child_stall(false, &flag),
        LocalSummaryError::GpuStalled
    );
    assert!(flag.load(Ordering::Acquire));
}

#[test]
fn summary_child_finished_keeps_its_summary_backend_and_report() {
    let f = fixture("done");
    let out = f.summarize().unwrap();
    // The meeting's text reached the child on its standard input.
    assert_eq!(out.summary.title, "heard: shall we ship on friday");
    assert_eq!(
        f.stored(),
        ["heard: shall we ship on friday"],
        "the parent stores it"
    );
    let r = local(&out);
    assert_eq!(r.backend, LlamaBackend::Vulkan);
    assert_eq!(r.gpu_refused, None);
    assert_eq!(r.load_time, Duration::from_millis(7));
    assert_eq!(r.run.requests, 3);
    assert_eq!(
        r.run.steps,
        [StepTime {
            step: "single",
            wall: Duration::from_millis(13),
            requests: 3
        }]
    );
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
    assert!(!f.held());
}

#[test]
fn summary_child_native_output_on_the_same_pipe_is_not_a_failure() {
    let f = fixture("noisy");
    let out = f.summarize().unwrap();
    assert_eq!(local(&out).backend, LlamaBackend::Vulkan);
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
}

#[test]
fn summary_child_refused_load_moves_to_the_cpu_with_the_refusal() {
    let f = fixture("refused");
    let out = f.summarize().unwrap();
    let why = assert_moved_to_the_cpu(&f, &out);
    assert!(
        matches!(why, LocalSummaryError::Load(LlmError::Backend(ref m)) if m.contains("GPU")),
        "{why:?}"
    );
}

#[test]
fn summary_child_refused_load_and_a_failed_cpu_load_keep_both_errors() {
    let dir = TempDir::new("summary-child");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 302);
    let (cfg, _file) = config_for(&dir, "refused");
    let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let service = SummaryService::local_flagged(
        &cfg,
        Box::new(CpuOnly {
            loads: Arc::default(),
            unloadable: true,
        }),
        flag,
    )
    .unwrap();
    let err = service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(303),
            &CancelToken::new(),
        )
        .unwrap_err();
    let RuntimeError::LocalSummary(LocalSummaryError::LoadRetried { gpu, cpu }) = err else {
        panic!("{err:?}");
    };
    assert!(gpu.to_string().contains("GPU"), "{gpu}");
    assert!(cpu.to_string().contains("corrupt"), "{cpu}");
}

#[test]
fn summary_child_run_failure_is_the_error_and_no_second_run() {
    let f = fixture("failed");
    let err = f.summarize().unwrap_err();
    assert!(
        matches!(
            &err,
            RuntimeError::LocalSummary(LocalSummaryError::ChildRun(m)) if m.contains("reduce step")
        ),
        "{err:?}"
    );
    assert!(!err.is_retryable());
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
    assert!(f.stored().is_empty());
}

#[test]
fn summary_child_without_a_fallback_reports_a_retryable_failure() {
    let f = fixture_with("abort", |cfg| cfg.cpu_fallback = false);
    let err = f.summarize().unwrap_err();
    assert!(
        matches!(
            err,
            RuntimeError::LocalSummary(LocalSummaryError::GpuChildDied { .. })
        ),
        "{err:?}"
    );
    assert!(err.is_retryable());
    assert!(f.stored().is_empty());
}

#[test]
fn summary_child_cancel_kills_the_child_at_once() {
    let f = Arc::new(fixture("stall"));
    let cancel = CancelToken::new();
    let (ff, token) = (Arc::clone(&f), cancel.clone());
    let canceller = std::thread::spawn(move || {
        let started = Instant::now();
        while !ff.held() && started.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(5));
        }
        token.cancel();
        Instant::now()
    });
    let result = f.summarize_cancelled_by(&cancel);
    let cancelled_at = canceller.join().unwrap();
    let err = result.unwrap_err();
    assert!(
        matches!(&err, RuntimeError::LocalSummary(e) if e.is_cancelled()),
        "{err:?}"
    );
    // Well within the stall limit of 60 s and the 30 s grace of the in-process path.
    assert!(cancelled_at.elapsed() < Duration::from_secs(15));
    assert!(!f.held(), "the child is gone");
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
    assert!(f.stored().is_empty());
}

#[test]
fn summary_child_is_not_tried_after_a_stuck_child_in_this_session() {
    let f = fixture("abort");
    f.flag.store(true, Ordering::Release);
    let out = f.summarize().unwrap();
    let why = assert_moved_to_the_cpu(&f, &out);
    assert_eq!(why, LocalSummaryError::GpuStalled);
}

#[test]
fn summary_child_is_not_started_for_a_missing_model_file() {
    let f = fixture_with("abort", |cfg| {
        cfg.model_path = PathBuf::from("no-such-model.gguf");
    });
    assert!(matches!(
        f.summarize(),
        Err(RuntimeError::ModelMissing { .. })
    ));
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
}

#[test]
fn summary_child_runs_only_for_a_gpu_summary() {
    let f = fixture_with("abort", |cfg| cfg.gpu = false);
    let out = f.summarize().unwrap();
    assert_eq!(out.summary.title, "on the processor");
    assert_eq!(local(&out).gpu_refused, None);
}

/// A live window through `mode`: the mapper's answer and the count of CPU loads.
fn live_map(
    mode: &str,
    edit: impl FnOnce(&mut LocalSummaryConfig),
) -> (Result<SummaryV1, String>, usize) {
    let f = fixture_with(mode, edit);
    let mapper = f.service.live_mapper(2).expect("a local service");
    let input = SummaryService::input(&f.svc, f.id).unwrap();
    let answer = mapper.map(&input, &[0..input.segments.len()], &CancelToken::new());
    assert!(!f.held(), "{mode}: the child is gone");
    (answer, f.cpu_loads.load(Ordering::SeqCst))
}

#[test]
fn summary_child_live_window_runs_in_the_child() {
    let (answer, cpu) = live_map("done", |_| {});
    assert_eq!(answer.unwrap().title, "heard: shall we ship on friday");
    assert_eq!(cpu, 0);
}

#[test]
fn summary_child_live_window_lost_to_a_crash_or_stall_is_an_error_not_a_lost_recording() {
    // Windows Error Reporting can hold an aborting process open for seconds, so the short limit
    // is only for the stall.
    for (mode, words, limit) in [
        ("abort", ABORTED, Duration::from_secs(60)),
        (
            "stall",
            &["stopped responding"][..],
            Duration::from_millis(800),
        ),
    ] {
        let (answer, cpu) = live_map(mode, |cfg| cfg.stall_limit = limit);
        let why = answer.unwrap_err();
        assert!(words.iter().any(|w| why.contains(w)), "{mode}: {why}");
        for word in MEETING_WORDS {
            assert!(!why.to_lowercase().contains(word), "{why}");
        }
        // The summary after the meeting maps this window.
        assert_eq!(cpu, 0, "{mode}");
    }
}

#[test]
fn summary_child_live_window_with_a_refused_load_runs_on_the_cpu() {
    let (answer, cpu) = live_map("refused", |_| {});
    assert_eq!(answer.unwrap().title, "on the processor");
    assert_eq!(cpu, 1);
}

#[test]
fn summary_child_live_window_cancel_kills_the_child() {
    let f = Arc::new(fixture("stall"));
    let mapper = f.service.live_mapper(2).unwrap();
    let input = SummaryService::input(&f.svc, f.id).unwrap();
    let cancel = CancelToken::new();
    let (ff, token) = (Arc::clone(&f), cancel.clone());
    let canceller = std::thread::spawn(move || {
        let started = Instant::now();
        while !ff.held() && started.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(5));
        }
        token.cancel();
    });
    let answer = mapper.map(&input, &[0..input.segments.len()], &cancel);
    canceller.join().unwrap();
    assert!(answer.is_err());
    assert!(!f.held());
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
}

/// The real thing: the app's own exe in its hidden mode, on the real card, on an invented meeting
/// in a temporary store. A cancel kills the child; the summary after it still gets the card, which
/// a stuck earlier run would not leave free. It says nothing about speed.
#[test]
#[ignore = "needs a built app and the 4B model: cargo xtask build --app --target-dir C:\\eb\\lane-t93, then with EARSHOT_TEST_SUMMARY_CHILD_EXE at C:\\eb\\lane-t93\\release\\earshot.exe and EARSHOT_MODELS at the models folder: cargo test -p earshot-runtime --lib summary_child_real -- --ignored --nocapture"]
fn summary_child_real_app_on_the_real_card() {
    use crate::summary_model::{ChoiceReason, SUMMARY_MODELS, SummaryModelChoice};

    let exe = std::env::var_os("EARSHOT_TEST_SUMMARY_CHILD_EXE")
        .expect("EARSHOT_TEST_SUMMARY_CHILD_EXE names the built app");
    let root = std::env::var_os("EARSHOT_MODELS").expect("EARSHOT_MODELS names the models folder");
    let models = crate::backends::ModelDir::new(root);
    let model = SUMMARY_MODELS
        .iter()
        .find(|m| m.id == crate::summary_model::LOW_MEMORY_SUMMARY_MODEL)
        .unwrap();
    let choice = SummaryModelChoice {
        model,
        reason: ChoiceReason::Requested,
    };
    let mut cfg = crate::summary_model::local_summary_config(&models, &choice, true).unwrap();
    cfg.refuse_gpus = crate::model_settings::integrated_gpu_names(&crate::hardware());
    cfg.gpu_child = Some(GpuChild {
        program: PathBuf::from(exe),
        args: vec![OsString::from(SUMMARY_CHILD_FLAG)],
        env: Vec::new(),
    });
    let dir = TempDir::new("summary-child-real");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 310);
    let cpu_loads = Arc::new(AtomicUsize::new(0));
    let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let service = SummaryService::local_flagged(
        &cfg,
        Box::new(CpuOnly {
            loads: Arc::clone(&cpu_loads),
            unloadable: false,
        }),
        flag,
    )
    .unwrap();

    let cancel = CancelToken::new();
    let token = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        token.cancel();
    });
    let cancelled = service.summarize(&svc, id, &mut ChaCha8Rng::seed_from_u64(311), &cancel);
    canceller.join().unwrap();
    assert!(
        matches!(&cancelled, Err(RuntimeError::LocalSummary(e)) if e.is_cancelled()),
        "{cancelled:?}"
    );

    let out = service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(312),
            &CancelToken::new(),
        )
        .unwrap();
    let r = local(&out);
    println!("summary child backend: {}", r.backend.name());
    assert_ne!(r.backend, LlamaBackend::Cpu);
    assert_eq!(r.gpu_refused, None);
    assert_eq!(cpu_loads.load(Ordering::SeqCst), 0);
    assert!(!out.summary.title.is_empty());
    assert!(r.run.requests >= 1);
}

/// A service whose parent "runs from" `models`'s CUDA folder or `folder`, its child `mode`.
fn on_folder(mode: &str, models: &crate::backends::ModelDir, folder: PathBuf) -> Fixture {
    let dir = TempDir::new("summary-child");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 320);
    let (mut cfg, file) = config_for(&dir, mode);
    cfg.models = Some(models.clone());
    let cpu_loads = Arc::new(AtomicUsize::new(0));
    let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let loaded: &'static (dyn Fn() -> Option<PathBuf> + Send + Sync) =
        Box::leak(Box::new(move || Some(folder.clone())));
    let service = SummaryService::local_on_folder(
        &cfg,
        Box::new(CpuOnly {
            loads: Arc::clone(&cpu_loads),
            unloadable: false,
        }),
        flag,
        loaded,
    )
    .unwrap();
    Fixture {
        _dir: dir,
        svc,
        id,
        service,
        cpu_loads,
        flag,
        file,
    }
}

/// A models folder whose CUDA folder holds every summary file at its pinned size (nothing reads
/// their bytes here), and those files' paths.
fn installed_cuda(dir: &TempDir) -> (crate::backends::ModelDir, Vec<PathBuf>) {
    use crate::cuda::{CudaPart, folder, part_files};
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let cuda = folder(&models);
    std::fs::create_dir_all(&cuda).unwrap();
    let catalogue = crate::catalogue::Catalogue::embedded().unwrap();
    let mut paths = Vec::new();
    for f in part_files(catalogue, CudaPart::Summaries) {
        let path = cuda.join(&f.name);
        std::fs::File::create(&path)
            .unwrap()
            .set_len(f.size)
            .unwrap();
        paths.push(path);
    }
    assert!(!paths.is_empty());
    (models, paths)
}

fn assert_never_loaded(f: &Fixture, word: &str) {
    let out = f.summarize().unwrap();
    let why = assert_moved_to_the_cpu(f, &out);
    let LocalSummaryError::CudaFilesChanged { why } = why else {
        panic!("{why:?}");
    };
    assert!(why.contains(word), "{why}");
    // Not started at all: an `abort` child would read as a dead child.
    assert!(!f.flag.load(Ordering::Acquire));
}

#[test]
fn summary_child_gets_the_cuda_folder_while_it_looks_as_installed() {
    let dir = TempDir::new("summary-child-cuda");
    let (models, _) = installed_cuda(&dir);
    let cuda = crate::cuda::folder(&models);
    let f = on_folder("report-folder", &models, cuda.clone());
    let out = f.summarize().unwrap();
    assert_eq!(local(&out).backend, LlamaBackend::Cuda);
    assert_eq!(local(&out).gpu_refused, None);
    assert!(
        out.summary.title.contains("cuda"),
        "the child heard the folder: {}",
        out.summary.title
    );
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
}

#[test]
fn summary_child_never_loads_a_cuda_folder_with_a_library_dropped_in() {
    let dir = TempDir::new("summary-child-cuda");
    let (models, _) = installed_cuda(&dir);
    let cuda = crate::cuda::folder(&models);
    std::fs::write(cuda.join("dropped-in.dll"), b"not ours").unwrap();
    assert_never_loaded(&on_folder("abort", &models, cuda), "library");
}

#[test]
fn summary_child_never_loads_a_cuda_folder_with_a_file_of_another_size_or_gone() {
    let dir = TempDir::new("summary-child-cuda");
    let (models, paths) = installed_cuda(&dir);
    let cuda = crate::cuda::folder(&models);
    std::fs::File::options()
        .write(true)
        .open(&paths[0])
        .unwrap()
        .set_len(1)
        .unwrap();
    assert_never_loaded(&on_folder("abort", &models, cuda.clone()), "size");
    std::fs::remove_file(&paths[0]).unwrap();
    assert_never_loaded(&on_folder("abort", &models, cuda), "missing");
}

#[test]
fn summary_child_refuses_a_loaded_folder_it_has_no_models_folder_to_check() {
    assert_eq!(
        crate::cuda::child_folder_problem(std::path::Path::new("C:/set"), None),
        Some("the folder cannot be checked")
    );
}

#[test]
fn summary_child_does_not_judge_a_hand_named_folder_by_the_manifest() {
    // The replay's `--llama-dir`: a developer's folder, not the CUDA folder.
    let dir = TempDir::new("summary-child-cuda");
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let f = on_folder("report-folder", &models, dir.0.join("elsewhere"));
    let out = f.summarize().unwrap();
    assert!(
        out.summary.title.contains("elsewhere"),
        "{}",
        out.summary.title
    );
    assert_eq!(local(&out).gpu_refused, None);
}

#[test]
fn summary_child_live_window_with_changed_cuda_files_is_an_error_and_no_cpu_run() {
    let dir = TempDir::new("summary-child-cuda");
    let (models, _) = installed_cuda(&dir);
    let cuda = crate::cuda::folder(&models);
    std::fs::write(cuda.join("dropped-in.dll"), b"not ours").unwrap();
    let f = on_folder("abort", &models, cuda);
    let mapper = f.service.live_mapper(2).unwrap();
    let input = SummaryService::input(&f.svc, f.id).unwrap();
    let why = mapper
        .map(&input, &[0..input.segments.len()], &CancelToken::new())
        .unwrap_err();
    assert!(why.contains("CUDA files changed"), "{why}");
    // The summary after the meeting maps this window.
    assert_eq!(f.cpu_loads.load(Ordering::SeqCst), 0);
}

// The child's own side, in process.
/// A model that beats on every request and then answers.
struct BeatingModel(Heartbeat);

impl Generate for BeatingModel {
    fn context_tokens(&self) -> usize {
        16_384
    }

    fn prompt_tokens(&self, m: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
        CpuModel.prompt_tokens(m, prefix)
    }

    fn generate(
        &mut self,
        m: &[ChatMessage],
        prefix: &str,
        grammar: &str,
        max: usize,
        cancel: &CancelToken,
    ) -> Result<Generation, LlmError> {
        self.0.beat();
        // Long enough for the beat thread to see it.
        std::thread::sleep(POLL * 4);
        CpuModel.generate(m, prefix, grammar, max, cancel)
    }
}

/// Loads as `refuse` says.
struct ChildLoader {
    refuse: Option<&'static str>,
}

impl LoadLocal for ChildLoader {
    fn load(&self, config: &LocalSummaryConfig) -> Result<LoadedModel, RuntimeError> {
        assert!(config.gpu, "the child's load asks for the GPU");
        if let Some(refusal) = self.refuse {
            return Err(RuntimeError::LocalSummary(LocalSummaryError::Load(
                LlmError::Backend(refusal.to_owned()),
            )));
        }
        let heartbeat = Heartbeat::default();
        Ok(LoadedModel {
            heartbeat: heartbeat.clone(),
            model: Box::new(BeatingModel(heartbeat)),
            backend: LlamaBackend::Cuda,
            load_time: Duration::from_millis(9),
        })
    }
}

fn job(work: ChildWork) -> ChildJob {
    ChildJob {
        model_path: PathBuf::from("m.gguf"),
        model_id: "m".to_owned(),
        threads: 2,
        context_tokens: 16_384,
        max_output_tokens: 1024,
        assistant_prefix: String::new(),
        refuse_gpus: Vec::new(),
        llama_folder: None,
        work,
        input: SummaryInput::default(),
    }
}

/// `serve` on `job`: the exit code and the lines it wrote.
fn served(
    job: &ChildJob,
    loader: &dyn LoadLocal,
    load_folder: &dyn Fn(&std::path::Path) -> Result<(), String>,
) -> (i32, Vec<Heard>) {
    let mut out = Vec::new();
    let code = serve(
        serde_json::to_vec(job).unwrap().as_slice(),
        &mut out,
        loader,
        load_folder,
    );
    let (to, from) = std::sync::mpsc::channel();
    read_lines(out.as_slice(), &to);
    drop(to);
    (code, from.iter().collect())
}

fn whole() -> ChildWork {
    ChildWork::Whole {
        ranges: vec![0..0],
        done: vec![None],
    }
}

#[test]
fn summary_child_serves_loaded_beats_and_exactly_one_final_line() {
    let (code, heard) = served(&job(whole()), &ChildLoader { refuse: None }, &|_| {
        panic!("no folder was asked for")
    });
    assert_eq!(code, 0);
    assert_eq!(
        heard.first(),
        Some(&Heard::Line(ChildLine::Loaded {
            backend: BackendLine::Cuda
        }))
    );
    assert_eq!(heard.last(), Some(&Heard::End));
    assert!(heard.contains(&Heard::Line(ChildLine::Beat)), "{heard:?}");
    let finals: Vec<&Heard> = heard
        .iter()
        .filter(|h| matches!(h, Heard::Line(l) if l.is_final()))
        .collect();
    assert_eq!(finals.len(), 1, "{heard:?}");
    assert!(
        matches!(
            finals[0],
            Heard::Line(ChildLine::Done { backend: BackendLine::Cuda, load_time, .. })
                if *load_time == Duration::from_millis(9)
        ),
        "{heard:?}"
    );
    // The final line is the last of them.
    assert!(matches!(&heard[heard.len() - 2], Heard::Line(l) if l.is_final()));
}

#[test]
fn summary_child_serves_a_refused_load_as_its_one_line() {
    let (code, heard) = served(
        &job(whole()),
        &ChildLoader {
            refuse: Some("no card"),
        },
        &|_| Ok(()),
    );
    assert_eq!(code, 0);
    assert!(
        matches!(
            &heard[0],
            Heard::Line(ChildLine::LoadRefused { message }) if message.contains("no card")
        ),
        "{heard:?}"
    );
    assert_eq!(heard.len(), 2);
}

#[test]
fn summary_child_loads_the_parents_dll_folder_before_the_model() {
    let mut with_folder = job(whole());
    with_folder.llama_folder = Some(PathBuf::from("C:/set"));
    let seen = Mutex::new(Vec::new());
    let (code, heard) = served(&with_folder, &ChildLoader { refuse: None }, &|f| {
        seen.lock().unwrap().push(f.to_path_buf());
        Ok(())
    });
    assert_eq!(code, 0);
    assert_eq!(*seen.lock().unwrap(), [PathBuf::from("C:/set")]);
    assert!(matches!(&heard[0], Heard::Line(ChildLine::Loaded { .. })));
    // A folder that does not load refuses the load; the model is never touched.
    let (code, heard) = served(
        &with_folder,
        &ChildLoader {
            refuse: Some("must not be asked"),
        },
        &|_| Err("Windows error 126".to_owned()),
    );
    assert_eq!(code, 0);
    assert!(
        matches!(
            &heard[0],
            Heard::Line(ChildLine::LoadRefused { message }) if message.contains("126")
        ),
        "{heard:?}"
    );
}

#[test]
fn summary_child_refuses_a_job_it_cannot_read_and_prints_nothing_of_it() {
    let mut out = Vec::new();
    let code = serve(
        b"yes friday works".as_slice(),
        &mut out,
        &ChildLoader { refuse: None },
        &|_| Ok(()),
    );
    assert_eq!(code, EXIT_BAD_JOB);
    assert!(out.is_empty());
}

#[test]
fn summary_child_job_carries_the_input_unchanged() {
    let mut j = job(ChildWork::LastChunk { ranges: vec![0..3] });
    j.input.user_name = Some("A Person".to_owned());
    j.input.prompt_variant = PromptVariant::Current;
    j.input.meeting.title = Some("A title".to_owned());
    let back: ChildJob = serde_json::from_slice(&serde_json::to_vec(&j).unwrap()).unwrap();
    assert_eq!(back, j);
}

#[test]
fn summary_child_report_survives_the_wire() {
    let report = SummaryReport {
        requests: 4,
        repairs: 1,
        joined_parts: 2,
        generated_tokens: 99,
        generate_time: Duration::from_millis(123),
        steps: ["single", "map", "map-actions", "map-rest", "reduce"]
            .map(|step| StepTime {
                step,
                wall: Duration::from_millis(5),
                requests: 1,
            })
            .to_vec(),
        ..SummaryReport::default()
    };
    let wire = serde_json::to_vec(&ReportLine::of(&report)).unwrap();
    let back: ReportLine = serde_json::from_slice(&wire).unwrap();
    assert_eq!(back.report(), report);
}

#[test]
fn summary_child_lines_are_read_only_with_their_prefix() {
    let line = |l: &ChildLine| format!("{LINE_PREFIX}{}\n", serde_json::to_string(l).unwrap());
    let beat = line(&ChildLine::Beat);
    let done = line(&ChildLine::Failed {
        message: "x".to_owned(),
    });
    let read = |text: String| -> Vec<Heard> {
        let (to, from) = std::sync::mpsc::channel();
        read_lines(text.as_bytes(), &to);
        drop(to);
        from.iter().collect()
    };
    // Noise, an empty line and a prefix inside another line are not lines of the protocol.
    assert_eq!(
        read(format!("noise\n\nmore {beat}{beat}")),
        [Heard::Line(ChildLine::Beat), Heard::End]
    );
    // A line after the final one, a beat included.
    assert_eq!(
        read(format!("{done}{beat}")).last(),
        Some(&Heard::Bad("it wrote more after its last line"))
    );
    assert_eq!(
        read(format!("{LINE_PREFIX}{{\"line\":\"new_kind\"}}\n")),
        [Heard::Bad("it wrote a line Earshot cannot read")]
    );
    assert_eq!(read(String::new()), [Heard::End]);
}

#[test]
fn summary_child_mode_is_asked_for_only_by_its_flag_and_the_command_line_holds_nothing_else() {
    assert_eq!(child_main(&[OsString::from("--replay")]), None);
    assert_eq!(child_main(&[]), None);
    // Not `use_this_exe`: it is process-wide, and other tests of this binary build configs.
    assert_eq!(registered(), None);
    let child = this_exe().expect("this test binary found itself");
    assert_eq!(child.program, std::env::current_exe().unwrap());
    assert_eq!(child.args, [OsString::from(SUMMARY_CHILD_FLAG)]);
    assert!(child.env.is_empty());
}
