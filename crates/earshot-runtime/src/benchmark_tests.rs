//! Benchmark tests: replay sources, fake live transcription and the level fakes' final pass;
//! a manual clock for the timings. Every name contains `benchmark`.

use std::sync::atomic::{AtomicU64, Ordering};

use earshot_core::asr::{
    AsrError, BackendKind, ChunkerConfig, EnergyVad, TranscribeOptions, Transcriber, Transcription,
};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::pipeline::LiveAsr;

use super::*;
use crate::host::tests::fake_post;
use crate::live::LiveTranscription;
use crate::model_bench::StoredCheck;
use crate::post::tests::levels;
use crate::summary_settings::{SummarySettings, set_summary_settings};
use crate::testutil::{T0, TempDir, services};

/// A clock the test moves.
#[derive(Debug, Default)]
struct TestClock(AtomicU64);

impl TestClock {
    fn advance(&self, d: Duration) {
        self.0.fetch_add(ms(d), Ordering::SeqCst);
    }
}

impl PaceClock for TestClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

/// Live transcription that hears nothing (the measurements need no words).
struct Silent;

impl Transcriber for Silent {
    fn transcribe(
        &mut self,
        _: &MonoBuffer,
        _: &TranscribeOptions,
        _: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        Ok(Transcription {
            segments: Vec::new(),
            language: None,
            backend: self.backend_info(),
        })
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: BackendKind::Fake,
            device: "fake".into(),
            model: "silent".into(),
        }
    }
}

fn ids() -> earshot_core::asr::SegmentIds {
    let mut n = 0u8;
    Box::new(move || {
        n = n.wrapping_add(1);
        EntityId::from_bytes([0xB7, n, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1])
    })
}

/// Live transcription on both tracks.
fn factory() -> LiveAsrFactory {
    Box::new(|_, entity| {
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(Silent),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    })
}

/// The level fakes' scene (`host::tests::recorded`), 20 s.
fn meeting() -> BenchmarkMeeting {
    BenchmarkMeeting::new(
        levels(20_000, &[(0.1, 1_000, 6_000)], 101),
        levels(20_000, &[(0.4, 7_000, 12_000), (0.7, 13_000, 18_000)], 111),
    )
}

fn config() -> LiveConfig {
    LiveConfig {
        title: MEETING_TITLE.into(),
        clock_domain: ClockDomain::Replay,
        idle_wait: Duration::from_millis(1),
        transcription: LiveTranscription::Chunker,
        ..LiveConfig::default()
    }
}

/// The folders left under `root`.
fn left(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// A run of the host's runner with no summary.
fn plain_run<'a>(models: &'a ModelDir, cancel: &'a CancelToken) -> AfterMeeting<'a> {
    AfterMeeting {
        models,
        model: "base.en",
        order: &[Accelerator::Cpu],
        post: true,
        whole_pass: false,
        summary: None,
        no_summary: SkipReason::NotRequested,
        post_cancel: cancel,
        summary_cancel: cancel,
    }
}

/// The level fakes' final pass through the host's runner, the clock moved by 4 s before
/// the pass reports its end and by 1 s more before it returns.
fn after<'a>(clock: &'a Arc<TestClock>, calls: &'a AtomicU64, models: &'a ModelDir) -> AfterFn<'a> {
    Box::new(move |svc, outcome, cancel, progress| {
        calls.fetch_add(1, Ordering::SeqCst);
        let run = AfterMeeting {
            models,
            model: "base.en",
            order: &[Accelerator::Cpu],
            post: true,
            whole_pass: false,
            summary: None,
            no_summary: SkipReason::NotRequested,
            post_cancel: cancel,
            summary_cancel: cancel,
        };
        let result = host::after_meeting_with(svc, outcome, &run, fake_post, &mut |step| {
            if matches!(step, HostStep::PostFinished(_)) {
                clock.advance(Duration::from_secs(4));
            }
            progress(step);
        });
        clock.advance(Duration::from_secs(1));
        result
    })
}

#[test]
fn benchmark_run_measures_from_the_replay_end_and_leaves_no_files() {
    let dir = TempDir::new("benchmark");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let mut steps = Vec::new();
    let measured = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: Some(factory()),
            after: after(&clock, &calls, &models),
            clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
            live_summaries: LiveSummariesRun::NotRun,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &CancelToken::new(),
        &mut |p| steps.push(p),
    )
    .unwrap()
    .expect("not cancelled");
    assert_eq!(measured.meeting, Duration::from_secs(20));
    // One lag per track per second of meeting audio (Stats interval), give or take the end.
    assert!(measured.lags.len() >= 30, "{}", measured.lags.len());
    let mut tracks: Vec<TrackId> = measured.live_backends.iter().map(|(t, _)| *t).collect();
    tracks.sort();
    assert_eq!(tracks, [TrackId::Mic, TrackId::Reference]);
    assert!(!measured.live_stopped);
    // The session's own measure (real time, with fake decodes), not this test clock's. The
    // test clock does not move during the replay; the final pass ends 4 s after it.
    assert!(
        measured.catch_up < Duration::from_secs(60),
        "{:?}",
        measured.catch_up
    );
    assert_eq!(measured.transcript_ready, Duration::from_secs(4));
    assert_eq!(
        measured.summary,
        MeasuredSummary::Skipped(BenchSkip::NotRequested)
    );
    assert_eq!(
        measured.final_backend.as_ref().map(|b| b.kind),
        Some(BackendKind::Fake)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Progress: the replay from 0 to its end, then finishing, then transcribing.
    assert_eq!(
        steps.first(),
        Some(&BenchmarkProgress::Replaying {
            heard: Duration::ZERO,
            total: Duration::from_secs(20)
        })
    );
    let tail: Vec<&BenchmarkProgress> = steps
        .iter()
        .filter(|p| !matches!(p, BenchmarkProgress::Replaying { .. }))
        .collect();
    assert_eq!(
        tail,
        [
            &BenchmarkProgress::FinishingLive,
            &BenchmarkProgress::Transcribing
        ]
    );
    // The throwaway store and recordings are gone.
    assert!(left(&root).is_empty(), "{:?}", left(&root));
}

#[test]
fn benchmark_cancel_during_the_replay_runs_nothing_after_and_leaves_no_files() {
    let dir = TempDir::new("benchmark-cancel");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let result = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: Some(factory()),
            after: after(&clock, &calls, &models),
            clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
            live_summaries: LiveSummariesRun::NotRun,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &cancel,
        &mut |_| token.cancel(),
    )
    .unwrap();
    assert_eq!(result, None);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no final pass after a cancel"
    );
    assert!(left(&root).is_empty(), "{:?}", left(&root));
}

#[test]
fn benchmark_cancel_during_the_final_pass_is_cancelled_not_a_result() {
    let dir = TempDir::new("benchmark-cancel-post");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let result = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: Some(factory()),
            after: after(&clock, &calls, &models),
            clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
            live_summaries: LiveSummariesRun::NotRun,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &cancel,
        &mut |p| {
            if p == BenchmarkProgress::Transcribing {
                token.cancel();
            }
        },
    )
    .unwrap();
    assert_eq!(result, None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(left(&root).is_empty(), "{:?}", left(&root));
}

#[test]
fn benchmark_failed_final_pass_is_an_error_and_leaves_no_files() {
    let dir = TempDir::new("benchmark-fail");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let meeting = meeting();
    let after: AfterFn<'_> = Box::new(|svc, outcome, cancel, progress| {
        let run = AfterMeeting {
            models: &models,
            model: "base.en",
            order: &[Accelerator::Cpu],
            post: true,
            whole_pass: false,
            summary: None,
            no_summary: SkipReason::NotRequested,
            post_cancel: cancel,
            summary_cancel: cancel,
        };
        host::after_meeting_with(
            svc,
            outcome,
            &run,
            || -> Result<crate::host::tests::Fakes, RuntimeError> {
                Err(RuntimeError::Internal("model gone"))
            },
            progress,
        )
    });
    let e = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: None,
            after,
            clock: Arc::new(TestClock::default()),
            live_summaries: LiveSummariesRun::NotRun,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(e, RuntimeError::Internal("model gone")), "{e:?}");
    assert!(left(&root).is_empty(), "{:?}", left(&root));
}

#[test]
fn benchmark_scratch_sweeps_only_stale_run_folders() {
    let dir = TempDir::new("benchmark-scratch");
    let root = dir.0.join("scratch");
    std::fs::create_dir_all(root.join("run-1-2")).unwrap();
    std::fs::create_dir_all(root.join("keep")).unwrap();
    let scratch = Scratch::create(&root).unwrap();
    assert!(scratch.path().is_dir());
    // A fresh leftover is kept (it may be another run's); only old ones go.
    assert!(root.join("run-1-2").is_dir());
    assert!(root.join("keep").is_dir());
    std::fs::write(scratch.path().join("earshot.db"), b"x").unwrap();
    let path = scratch.path().to_owned();
    drop(scratch);
    assert!(!path.exists());
}

fn measured(lags: &[u64], catch_up: u64, stopped: bool) -> Measured {
    Measured {
        meeting: Duration::from_secs(150),
        lags: lags.iter().map(|&s| Duration::from_millis(s)).collect(),
        live_backends: vec![
            (
                TrackId::Reference,
                BackendInfo {
                    kind: BackendKind::Cuda,
                    device: "GPU".into(),
                    model: "m".into(),
                },
            ),
            (
                TrackId::Mic,
                BackendInfo {
                    kind: BackendKind::Cpu,
                    device: "cpu".into(),
                    model: "m".into(),
                },
            ),
        ],
        live_stopped: stopped,
        live_summaries: LiveSummariesRun::On,
        catch_up: Duration::from_millis(catch_up),
        transcript_ready: Duration::from_millis(9_000),
        final_backend: None,
        summary: MeasuredSummary::Stored {
            ready: Duration::from_millis(40_000),
            model: "qwen3.5-4b".into(),
            backend: "cpu".into(),
        },
    }
}

fn setup(check: Option<StoredCheck>) -> BenchmarkSetup {
    BenchmarkSetup {
        model: "parakeet-tdt-0.6b-v2".into(),
        order: vec![Accelerator::Cpu],
        summary: BenchSummary::Skipped(BenchSkip::NotRequested),
        speaker_timing: SpeakerTiming::AfterCall,
        live_summaries: true,
        check,
        style: SummaryStyle::default(),
        vocabulary: Vocabulary::default(),
        notes: vec!["Parakeet 0.6B v3 has no NPU version, so it runs on the processor.".into()],
    }
}

#[test]
fn benchmark_record_takes_the_lags_the_mic_first_and_judges_keeping_up() {
    let r = BenchmarkRecord::new(
        &measured(&[300, 9_000, 1_200, 700], 2_000, false),
        &setup(None),
        T0,
        "0.1.0",
    );
    assert_eq!((r.lag_max_ms, r.lag_median_ms), (Some(9_000), Some(700)));
    assert_eq!(r.live_backends[0].kind, "cpu", "mic first");
    assert_eq!(r.live_backends[1].kind, "cuda");
    assert!(r.kept_up);
    assert_eq!(r.settings.speaker_timing, "after");
    assert_eq!(r.settings.live_summaries, LiveSummariesRun::On);
    assert_eq!(r.notes, setup(None).notes);
    assert_eq!(
        r.summary,
        RecordSummary::Stored {
            ready_ms: 40_000,
            model: "qwen3.5-4b".into(),
            backend: "cpu".into()
        }
    );
    assert_eq!(r.check, None);
    // Late by more than the limit, or a stopped track: not kept up.
    let limit = ms(CATCH_UP_LIMIT);
    assert!(BenchmarkRecord::new(&measured(&[], limit, false), &setup(None), T0, "x").kept_up);
    assert!(!BenchmarkRecord::new(&measured(&[], limit + 1, false), &setup(None), T0, "x").kept_up);
    assert!(!BenchmarkRecord::new(&measured(&[1], 0, true), &setup(None), T0, "x").kept_up);
    let none = BenchmarkRecord::new(&measured(&[], 0, false), &setup(None), T0, "x");
    assert_eq!((none.lag_max_ms, none.lag_median_ms), (None, None));
}

#[test]
fn benchmark_record_shows_the_checks_verdict_for_the_model_on_the_live_backend() {
    let check = StoredCheck {
        fingerprint: "f".into(),
        model: "parakeet-tdt-0.6b-v2".into(),
        backend: BackendKind::Cuda,
        fallback: None,
        trials: vec![crate::model_bench::StoredTrial {
            model: "parakeet-tdt-0.6b-v2".into(),
            backend: BackendKind::Cpu,
            verdict: StoredVerdict::TooSlow,
        }],
    };
    let r = BenchmarkRecord::new(&measured(&[1], 0, false), &setup(Some(check)), T0, "x");
    // Live transcription ran on the CPU on the mic: the CPU trial's verdict.
    assert_eq!(r.check, Some(StoredVerdict::TooSlow));
}

#[test]
fn benchmark_median_is_the_lower_middle() {
    let d = |v: &[u64]| {
        v.iter()
            .map(|&s| Duration::from_secs(s))
            .collect::<Vec<_>>()
    };
    assert_eq!(median(&d(&[])), None);
    assert_eq!(median(&d(&[5])), Some(Duration::from_secs(5)));
    assert_eq!(median(&d(&[4, 1, 3, 2])), Some(Duration::from_secs(2)));
    assert_eq!(median(&d(&[3, 1, 2])), Some(Duration::from_secs(2)));
}

#[test]
fn benchmark_record_round_trips_and_an_unreadable_one_is_reported() {
    let dir = TempDir::new("benchmark-record");
    let (svc, _) = services(&dir);
    assert_eq!(
        stored_benchmark(&svc.store()).unwrap(),
        StoredBenchmark::default()
    );
    let r = BenchmarkRecord::new(&measured(&[1, 2], 0, false), &setup(None), T0, "0.1.0");
    record_benchmark(&mut svc.store(), &r).unwrap();
    let stored = stored_benchmark(&svc.store()).unwrap();
    assert_eq!(stored.record.as_ref(), Some(&r));
    assert!(!stored.unreadable);
    // Only the latest is kept.
    let mut later = r.clone();
    later.ran_at_ms += 1;
    record_benchmark(&mut svc.store(), &later).unwrap();
    assert_eq!(stored_benchmark(&svc.store()).unwrap().record, Some(later));
    // Another version or a damaged payload reads as unreadable, never an error.
    for bad in [&b"\x02\x00\x00\x00{}"[..], b"\x01\x00\x00\x00{", b"\x01"] {
        crate::local::set(&mut svc.store(), &DeviceRecord::Benchmark.key(), bad).unwrap();
        let s = stored_benchmark(&svc.store()).unwrap();
        assert_eq!((s.record, s.unreadable), (None, true));
    }
}

#[test]
fn benchmark_record_holds_no_content() {
    // The record names settings, timings and backends only: every string field is one of
    // those (no transcript or summary text can reach it).
    let r = BenchmarkRecord::new(&measured(&[1], 0, false), &setup(None), T0, "0.1.0");
    let json = serde_json::to_value(&r).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "app_version",
            "catch_up_ms",
            "check",
            "final_backend",
            "kept_up",
            "lag_max_ms",
            "lag_median_ms",
            "live_backends",
            "meeting_ms",
            "notes",
            "ran_at_ms",
            "settings",
            "summary",
            "transcript_ready_ms"
        ]
    );
    let mut settings: Vec<&str> = json["settings"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    settings.sort_unstable();
    assert_eq!(
        settings,
        ["live_summaries", "speaker_timing", "transcription_model"]
    );
}

#[test]
fn benchmark_summary_never_prepares_a_summary_written_elsewhere() {
    let dir = TempDir::new("benchmark-summary");
    let (svc, _) = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = earshot_hardware::Hardware::default();
    let summary_of = |svc: &Services| {
        let settings = host::summary_from_settings(
            &svc.store(),
            &[Accelerator::Cpu],
            &hw,
            &crate::summary_settings::NoApiKeys,
            crate::keep_local::SummaryPrivacy::Writer,
        )
        .unwrap();
        prepare_bench_summary(&settings, &models, Arc::clone(&svc.clock), &[]).unwrap()
    };
    // Default: on this computer, but the model is not installed here.
    let BenchSummary::Skipped(BenchSkip::NotInstalled { model }) = summary_of(&svc) else {
        panic!("expected not installed");
    };
    assert!(model.starts_with("qwen"), "{model}");
    for (writer, skip) in [
        (SummaryWriter::NoSummary, BenchSkip::NotRequested),
        (
            SummaryWriter::OwnEndpoint,
            BenchSkip::NotOnThisComputer {
                writer: "endpoint".into(),
            },
        ),
        (
            SummaryWriter::ClaudeCode,
            BenchSkip::NotOnThisComputer {
                writer: "claude-code".into(),
            },
        ),
        (
            SummaryWriter::Codex,
            BenchSkip::NotOnThisComputer {
                writer: "codex".into(),
            },
        ),
    ] {
        let settings = SummarySettings {
            writer,
            url: "http://127.0.0.1:9/v1".into(),
            model: "m".into(),
            ..SummarySettings::default()
        };
        set_summary_settings(&mut svc.store(), &settings).unwrap();
        let BenchSummary::Skipped(got) = summary_of(&svc) else {
            panic!("{writer:?} prepared a summary");
        };
        assert_eq!(got, skip, "{writer:?}");
    }
}

#[test]
fn benchmark_live_summaries_written_elsewhere_are_recorded_as_not_run() {
    // Claude Code writes the summary and "summarise during the meeting" is on: it cannot run
    // in the benchmark, and the record says so instead of copying the setting.
    let dir = TempDir::new("benchmark-live-off");
    let (svc, _) = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let settings = SummarySettings {
        writer: SummaryWriter::ClaudeCode,
        live: true,
        ..SummarySettings::default()
    };
    set_summary_settings(&mut svc.store(), &settings).unwrap();
    let hw = earshot_hardware::Hardware::default();
    let read = host::summary_from_settings(
        &svc.store(),
        &[Accelerator::Cpu],
        &hw,
        &crate::summary_settings::NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(read.live, "the setting is on");
    let summary = prepare_bench_summary(&read, &models, Arc::clone(&svc.clock), &[]).unwrap();
    let (mapper, run) = live_summaries_start(&summary, read.live, &[Accelerator::Cpu]);
    assert!(mapper.is_none());
    assert_eq!(run, LiveSummariesRun::NotRun);
    let mut m = measured(&[1], 0, false);
    m.live_summaries = run;
    let r = BenchmarkRecord::new(&m, &setup(None), T0, "x");
    assert_eq!(r.settings.live_summaries, LiveSummariesRun::NotRun);
    let (_, off) = live_summaries_start(&summary, false, &[Accelerator::Cpu]);
    assert_eq!(off, LiveSummariesRun::Off);
}

#[test]
fn benchmark_run_keeps_the_live_summaries_state_it_was_given() {
    let dir = TempDir::new("benchmark-state");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let measured = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: None,
            after: after(&clock, &calls, &models),
            live_summaries: LiveSummariesRun::NotRun,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap()
    .unwrap();
    assert_eq!(measured.live_summaries, LiveSummariesRun::NotRun);
}

/// A power supply fixed for the test.
struct OnBattery;

impl crate::power::PowerSource for OnBattery {
    fn supply(&self) -> crate::power::PowerSupply {
        crate::power::PowerSupply::Battery
    }
}

/// A model the battery rule keeps from starting.
struct UnusedMapper;

impl crate::live_summaries::MapChunk for UnusedMapper {
    fn model_id(&self) -> &str {
        "unused"
    }

    fn seconds(&self) -> u32 {
        2
    }

    fn map(
        &self,
        _: &earshot_core::prompt::SummaryInput,
        _: &[std::ops::Range<usize>],
        _: &CancelToken,
    ) -> Result<earshot_core::summary::SummaryV1, String> {
        Err("not called".into())
    }
}

#[test]
fn benchmark_live_summaries_that_never_started_carry_the_runtimes_reason() {
    let dir = TempDir::new("benchmark-battery");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let measured = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: LiveConfig {
                live_summaries: Some(LiveSummaries {
                    mapper: Arc::new(UnusedMapper),
                    background: false,
                }),
                speaker_timing: SpeakerTiming::AfterCallOnBattery,
                power: crate::power::PowerProbe::new(OnBattery),
                ..config()
            },
            factory: Some(factory()),
            after: after(&clock, &calls, &models),
            live_summaries: LiveSummariesRun::On,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        measured.live_summaries,
        LiveSummariesRun::Stopped {
            reason: crate::live_summaries::ON_BATTERY_REASON.into()
        }
    );
}

#[test]
fn benchmark_catch_up_does_not_depend_on_when_the_loop_reads_the_event() {
    // The test clock jumps 1000 s while the loop is busy, before it reads the event.
    let dir = TempDir::new("benchmark-stamp");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let jump = Arc::clone(&clock);
    let measured = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: Some(factory()),
            after: after(&clock, &calls, &models),
            live_summaries: LiveSummariesRun::Off,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &CancelToken::new(),
        &mut |p| {
            if p == BenchmarkProgress::FinishingLive {
                jump.advance(Duration::from_secs(1000));
            }
        },
    )
    .unwrap()
    .unwrap();
    assert!(
        measured.catch_up < Duration::from_secs(1000),
        "{:?}",
        measured.catch_up
    );
}

#[test]
fn benchmark_skip_is_the_results_own_reason() {
    let setup_reason = BenchSkip::NotOnThisComputer {
        writer: "codex".into(),
    };
    assert_eq!(
        skip_of(&SkipReason::NotRequested, Some(&setup_reason)).unwrap(),
        setup_reason
    );
    assert_eq!(
        skip_of(
            &SkipReason::NotInstalled {
                model: "qwen3.5-4b-q4_k_m",
                why: "fetch it".into()
            },
            Some(&BenchSkip::NotRequested)
        )
        .unwrap(),
        BenchSkip::NotInstalled {
            model: "qwen3.5-4b-q4_k_m".into()
        }
    );
    // A prepared summary that did not run is not "set to no summary".
    assert!(matches!(
        skip_of(&SkipReason::NotRequested, None),
        Err(RuntimeError::Internal(_))
    ));
    assert!(matches!(
        skip_of(&SkipReason::PostFailed, Some(&BenchSkip::NotRequested)),
        Err(RuntimeError::Internal(_))
    ));
}

#[test]
fn benchmark_prepared_summary_skipped_by_the_host_is_an_error_not_a_setting() {
    let dir = TempDir::new("benchmark-skip");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let e = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: None,
            after: after(&clock, &calls, &models),
            live_summaries: LiveSummariesRun::Off,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        None,
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(e, RuntimeError::Internal(_)), "{e:?}");
    assert!(left(&root).is_empty(), "{:?}", left(&root));
}

#[test]
fn benchmark_copies_the_users_summary_style_into_the_throwaway_store() {
    let dir = TempDir::new("benchmark-style");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let meeting = meeting();
    let seen = std::sync::Mutex::new(None);
    let seen_words = std::sync::Mutex::new(Vec::new());
    let after: AfterFn<'_> = Box::new(|svc, outcome, cancel, progress| {
        *seen.lock().unwrap() = Some(
            crate::summary_style::summary_style(&svc.store())
                .unwrap()
                .0
                .as_str()
                .to_owned(),
        );
        // The word list too (T12.15a; invented words).
        *seen_words.lock().unwrap() = crate::vocabulary::vocabulary(&svc.store())
            .unwrap()
            .0
            .words()
            .to_vec();
        host::after_meeting_with(
            svc,
            outcome,
            &plain_run(&models, cancel),
            fake_post,
            progress,
        )
    });
    run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: None,
            after,
            live_summaries: LiveSummariesRun::Off,
            style: SummaryStyle::new("Short, no jargon.").unwrap(),
            vocabulary: Vocabulary::new(["Quillmarsh", "QXR-7"]).unwrap(),
            clock: Arc::new(TestClock::default()),
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap()
    .unwrap();
    assert_eq!(seen.lock().unwrap().as_deref(), Some("Short, no jargon."));
    assert_eq!(*seen_words.lock().unwrap(), ["Quillmarsh", "QXR-7"]);
}

#[test]
fn benchmark_that_reached_the_network_fails_and_records_nothing() {
    let dir = TempDir::new("benchmark-network");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let meeting = meeting();
    let after: AfterFn<'_> = Box::new(|svc, outcome, cancel, progress| {
        let mut out = host::after_meeting_with(
            svc,
            outcome,
            &plain_run(&models, cancel),
            fake_post,
            progress,
        );
        out.network.push(earshot_net::AuditEntry {
            timestamp_ms: T0,
            method: "POST".into(),
            scheme: "https".into(),
            host: "example.invalid".into(),
            port: 443,
            path: "/".into(),
            request_bytes: 1,
            response_bytes: 0,
            status: None,
            outcome: earshot_net::AuditOutcome::Failed("test".into()),
        });
        out
    });
    let e = run_benchmark(
        BenchmarkRun {
            meeting: &meeting,
            config: config(),
            factory: None,
            after,
            live_summaries: LiveSummariesRun::Off,
            style: SummaryStyle::default(),
            vocabulary: Vocabulary::default(),
            clock: Arc::new(TestClock::default()),
            realtime: false,
            services_clock: Arc::new(ManualClock::new(T0)),
            scratch_root: &root,
        },
        Some(&BenchSkip::NotRequested),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(
            e,
            RuntimeError::Internal("the benchmark reached outside this computer")
        ),
        "{e:?}"
    );
    assert!(left(&root).is_empty(), "{:?}", left(&root));
}

#[test]
fn benchmark_panic_while_the_session_runs_still_deletes_the_scratch_folder() {
    let dir = TempDir::new("benchmark-panic");
    let root = dir.0.join("scratch");
    let models = ModelDir::new(dir.0.join("models"));
    let clock = Arc::new(TestClock::default());
    let calls = AtomicU64::new(0);
    let meeting = meeting();
    let run = BenchmarkRun {
        meeting: &meeting,
        config: config(),
        factory: Some(factory()),
        after: after(&clock, &calls, &models),
        live_summaries: LiveSummariesRun::Off,
        style: SummaryStyle::default(),
        vocabulary: Vocabulary::default(),
        clock: Arc::clone(&clock) as Arc<dyn PaceClock>,
        realtime: false,
        services_clock: Arc::new(ManualClock::new(T0)),
        scratch_root: &root,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_benchmark(
            run,
            Some(&BenchSkip::NotRequested),
            &CancelToken::new(),
            &mut |_| panic!("the host's progress handler failed"),
        )
    }));
    assert!(result.is_err());
    // The session was stopped and joined first, so nothing held the folder open.
    assert!(left(&root).is_empty(), "{:?}", left(&root));
}

/// The real run on this machine (T7.4c accept): default settings on a fresh store, the
/// installed models, the bundled meeting replayed in real time. Prints the record.
#[test]
#[ignore = "runs the installed models in real time for about 3 minutes plus processing: \
            cargo test --release -p earshot-runtime benchmark_real_run -- --ignored --nocapture \
            (needs `cargo xtask fetch-models --set ci` and, for the summary, `--set summary`)"]
fn benchmark_real_run_with_the_installed_models() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir_of = repo.join("apps/desktop/src-tauri/resources/benchmark");
    let meeting = BenchmarkMeeting::from_flac(
        &std::fs::read(dir_of.join("mic.flac")).unwrap(),
        &std::fs::read(dir_of.join("reference.flac")).unwrap(),
    )
    .unwrap();
    let dir = TempDir::new("benchmark-real");
    let data = DataDir::new(dir.0.join("user"));
    data.create().unwrap();
    let store = data.open_store().unwrap();
    let user = Services::new(store, data.clone(), Arc::new(crate::SystemClock)).unwrap();
    let models = host::model_dir(&data);
    let mut dll_dirs: Vec<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
        .into_iter()
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        dll_dirs.extend(std::env::split_paths(&path));
    }
    let mut last = None;
    let record = run_from_settings(
        &user,
        &models,
        &meeting,
        &dll_dirs,
        &dir.0.join("scratch"),
        env!("CARGO_PKG_VERSION"),
        &CancelToken::new(),
        &mut |p| {
            let phase = std::mem::discriminant(&p);
            if last != Some(phase) {
                eprintln!("benchmark: {p:?}");
                last = Some(phase);
            }
        },
    )
    .unwrap()
    .expect("not cancelled");
    println!("{}", serde_json::to_string_pretty(&record).unwrap());
    assert!(left(&dir.0.join("scratch")).is_empty());
    assert_eq!(
        stored_benchmark(&user.store()).unwrap().record,
        Some(record)
    );
}
