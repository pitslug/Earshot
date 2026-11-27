//! T12.63b: a hand-back continues the meeting's run of the order of summary writers
//! (docs/summary-order.md 4.3), and re-summarise "On <device>" is a named run. A child of
//! `offload_tests.rs`, sharing its helpers; every name holds `offload`
//! (`cargo test -p earshot-desktop offload`).

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::SummaryRequestRepo;
use earshot_runtime::Services;
use earshot_runtime::host::{SkipReason, SummaryResult};
use earshot_runtime::meeting_queue::MeetingQueue;
use earshot_runtime::offload::{
    Continuation, ContinueAnswer, HereReason, OffloadChoice, OffloadHost, WriteHere, WriterKind,
};
use earshot_runtime::summary_order::run::{
    Built, OutcomeWord, RunPlan, RunRecord, RunStart, StepWriters, run_summary_order,
};
use earshot_runtime::summary_order::{OrderStep, SummaryOrder, WriterStep};

use super::{NoWriter, meeting_with_final, pair, relay};
use crate::ipc::IpcError;
use crate::jobs::{JobEvent, JobRunner, JobSink, JobSummary, JobWork, SummaryStart};
use crate::offload::HostOffload;
use crate::testutil::{TempDir, services};

const DESK: DeviceId = DeviceId::from_bytes([6; 16]);

/// Writers that ask `DESK` and build no writer step.
struct AskDesk;

impl StepWriters for AskDesk {
    fn writer(&self, _: &Services, _: WriterStep) -> Built {
        Built::Stop(earshot_runtime::RuntimeError::Internal(
            "no writer step here",
        ))
    }

    fn computer(&self, _: &Services, device: DeviceId) -> OffloadChoice {
        OffloadChoice::Ask {
            device,
            name: "Invented desk".into(),
            writer: WriterKind::Local,
        }
    }
}

/// Starts an order run of `steps` on `m` whose step 1 asks `DESK`; returns the request.
fn waiting_run(svc: &Services, m: EntityId, steps: &[OrderStep]) -> EntityId {
    let out = run_summary_order(
        svc,
        m,
        RunStart::Order(SummaryOrder::new(steps.to_vec()).unwrap()),
        &RunPlan {
            writers: &AskDesk,
            local_not_now: &|| false,
            order: None,
        },
        &CancelToken::new(),
    );
    match out.summary {
        SummaryResult::Offloaded { request, .. } => request,
        other => panic!("{other:?}"),
    }
}

/// Job work that records how each re-summary was started.
#[derive(Default)]
struct RecordingJobs(Mutex<Vec<SummaryStart>>);

impl JobWork for RecordingJobs {
    fn retranscribe(
        &self,
        _: &Services,
        _: EntityId,
        _: &CancelToken,
        _: &mut dyn FnMut(earshot_runtime::host::HostStep<'_>),
        _: &mut dyn FnMut(String),
    ) -> Result<earshot_runtime::edit::Retranscribed, IpcError> {
        unreachable!("only re-summaries here")
    }

    fn summarise(
        &self,
        _: &Services,
        _: EntityId,
        _: &CancelToken,
        _: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError> {
        unreachable!("the runner asks summarise_run")
    }

    fn summarise_run(
        &self,
        _: &Services,
        _: EntityId,
        start: &SummaryStart,
        _: &CancelToken,
        _: &mut dyn FnMut(String),
    ) -> Result<JobSummary, IpcError> {
        self.0.lock().unwrap().push(start.clone());
        Ok(SummaryResult::Skipped {
            reason: SkipReason::NotRequested,
        }
        .into())
    }
}

/// The computer `offload_host_continues_an_order_run_with_the_next_step` hands back from, its
/// own: the names this process has heard (`offload::COMPUTERS`) are process-wide, and other
/// tests here name `DESK` while it runs, which changed its expected note between the host's
/// read and the assertion's.
const HAND_BACK_DESK: DeviceId = DeviceId::from_bytes([0x46; 16]);

#[test]
fn offload_host_continues_an_order_run_with_the_next_step() {
    let dir = TempDir::new("offload-order-continue");
    let svc = services(&dir);
    // What the production chooser (`summary_order::remembering`) remembers when it asks.
    crate::offload::remember_computer(HAND_BACK_DESK, Some("Invented hand-back desk".into()), None);
    let m = meeting_with_final(&svc, 21, 2);
    let request = waiting_run(
        &svc,
        m,
        &[OrderStep::Computer(HAND_BACK_DESK), OrderStep::LOCAL],
    );
    let work = Arc::new(RecordingJobs::default());
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: JobSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let runner = JobRunner::new(
        Arc::clone(&work) as Arc<dyn JobWork>,
        sink,
        crate::activity::WorkGate::new(),
        &crate::cuda_settings::CudaLatch::resolved_off(),
    );
    let host = HostOffload::new(
        svc.clone(),
        runner.clone(),
        MeetingQueue::new(),
        crate::activity::WorkGate::new(),
        Arc::new(NoWriter),
    );
    let answer = host.continue_run(WriteHere {
        meeting: m,
        request,
        helper: HAND_BACK_DESK,
        why: HereReason::NotClaimed,
        continuation: Continuation::Order {
            word: OutcomeWord::NoReply,
        },
    });
    assert_eq!(answer, ContinueAnswer::Started);
    let first = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let message = crate::summary_order::continue_note(
        HAND_BACK_DESK,
        &crate::summary_order::StepContext::read(&svc),
    );
    assert!(
        message.starts_with("Invented hand-back desk could not write the summary"),
        "the note names the computer: {message}"
    );
    assert_eq!(
        first,
        JobEvent::Warning {
            meeting: m.to_string(),
            message,
        }
    );
    runner.join();
    assert_eq!(
        *work.0.lock().unwrap(),
        [SummaryStart::Continue {
            request,
            word: OutcomeWord::NoReply,
        }],
        "the run goes on from its own steps, never a fresh order"
    );
}

/// A Claude Code probe that must never be asked.
struct NeverClaude;
impl earshot_runtime::claude_code::ClaudeCodeProbe for NeverClaude {
    fn status(&self) -> earshot_runtime::claude_code::ClaudeCodeStatus {
        panic!("a writer the run did not copy was asked")
    }
}

/// A Codex probe that must never be asked.
struct NeverCodex;
impl earshot_runtime::codex::CodexProbe for NeverCodex {
    fn status(&self) -> earshot_runtime::codex::CodexStatus {
        panic!("a writer the run did not copy was asked")
    }
}

/// The production re-summary of a continuation (`jobs::summarise_run_with`) tries the steps the
/// run copied when it started, never the order as it is now (docs/summary-order.md section 7).
#[test]
fn offload_continuation_tries_the_copied_steps_not_the_order_now() {
    let dir = TempDir::new("offload-order-copied");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 22, 2);
    let request = waiting_run(&svc, m, &[OrderStep::Computer(DESK), OrderStep::LOCAL]);
    // The order changes meanwhile to Claude Code alone.
    earshot_runtime::summary_order::set_summary_order(
        &mut svc.store(),
        &SummaryOrder::new(vec![OrderStep::Writer(WriterStep::ClaudeCode)]).unwrap(),
        None,
    )
    .unwrap();
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("no-models"));
    let vault = crate::credentials::MemoryVault::default();
    let done = crate::jobs::summarise_run_with(
        &svc,
        m,
        &SummaryStart::Continue {
            request,
            word: OutcomeWord::Declined,
        },
        &CancelToken::new(),
        &models,
        (&vault, &NeverClaude, &NeverCodex),
        None,
        &mut |_| {},
        &mut || {},
    )
    .unwrap();
    // The copied local step was tried (not installed here); Claude Code never was.
    let RunRecord::Read(run) = earshot_runtime::summary_order::run::read(&svc.store(), m).unwrap()
    else {
        panic!("a run record");
    };
    assert_eq!(run.steps, vec![OrderStep::Computer(DESK), OrderStep::LOCAL]);
    assert_eq!(run.outcome_of(0), Some(OutcomeWord::Declined));
    assert_eq!(run.outcome_of(1), Some(OutcomeWord::NotInstalled));
    assert!(matches!(done.result, SummaryResult::Skipped { .. }));
    // Every step was tried and left: the banner says what became of each.
    let note = done.note.expect("the steps banner");
    assert!(note.starts_with("No summary."), "{note}");
    assert!(note.contains("declined"), "{note}");
}

/// Re-summarise "On <device>" starts a named run (docs/summary-order.md 5): a hand-back ends the
/// run and nothing is written here instead.
#[test]
fn offload_resummarise_on_a_computer_is_a_named_run_that_a_hand_back_ends() {
    let relay = relay();
    let (a, b) = pair(&relay);
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    earshot_runtime::sync::set_device_name(&b.svc, "Invented desk").unwrap();
    b.client().send_presence(false).unwrap();
    let m = meeting_with_final(&a.svc, 23, 2);
    crate::offload::ask_helper(&a.svc, &a.host, &m.to_string(), &b.id().to_string()).unwrap();
    let RunRecord::Read(run) =
        earshot_runtime::summary_order::run::read(&a.svc.store(), m).unwrap()
    else {
        panic!("a run record");
    };
    assert_eq!(
        run.origin,
        earshot_runtime::summary_order::run::RunOrigin::Named
    );
    assert_eq!(run.steps, vec![OrderStep::Computer(b.id())]);
    let request = run.waiting_on.expect("it waits on the request");
    // The computer claims it and declines it (it has no writer); the asker's step then settles
    // it and starts no job.
    let (ca, cb) = (a.client(), b.client());
    ca.tick().unwrap();
    cb.tick().unwrap();
    cb.tick().unwrap();
    let (runner, _events) = super::jobs();
    let helper: Arc<dyn OffloadHost> = Arc::new(HostOffload::new(
        b.svc.clone(),
        runner,
        MeetingQueue::new(),
        crate::activity::WorkGate::new(),
        Arc::new(NoWriter),
    ));
    earshot_runtime::offload::Offload::new()
        .step(&b.svc, &mut || None, &*helper)
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while b
        .svc
        .store()
        .summary_request(request)
        .unwrap()
        .unwrap()
        .state
        != earshot_core::model::RequestState::Declined
    {
        assert!(std::time::Instant::now() < deadline, "never declined");
        std::thread::sleep(Duration::from_millis(10));
    }
    cb.tick().unwrap();
    ca.tick().unwrap();
    struct NoJobs;
    impl OffloadHost for NoJobs {
        fn writing(&self) -> Vec<earshot_core::identity::EntityId> {
            Vec::new()
        }
        fn cancel_writing(&self, _request: earshot_core::identity::EntityId) {}
        fn continue_run(&self, job: WriteHere) -> ContinueAnswer {
            panic!("a named run is never continued: {job:?}")
        }
        fn write_for(&self, _: earshot_core::model::SummaryRequest) {
            panic!("nothing asked this computer")
        }
    }
    // The declined row has arrived but the step has not run: no banner and no account yet. The
    // window is told only after the step (T12.81), whose report names the meeting, so its one
    // read shows the account.
    let before = crate::detail::meeting_detail(&a.svc, m).unwrap();
    assert_eq!(
        (before.waiting_for, before.summary_run),
        (None, None),
        "what a read between apply and step would show"
    );
    let state = earshot_runtime::offload::Offload::new();
    let r = state.step(&a.svc, &mut || None, &NoJobs).unwrap();
    assert_eq!(r.meetings.iter().copied().collect::<Vec<_>>(), vec![m]);
    let detail = crate::detail::meeting_detail(&a.svc, m).unwrap();
    assert_eq!(
        detail.summary_run.as_deref(),
        Some("Invented desk could not write the summary: declined.")
    );
}

/// A continuation whose run no longer waits on its request (review of T12.63b): the job ends
/// quietly, with `JobEvent::Settled`, nothing run, written or sent, and never a skipped summary.
#[test]
fn offload_continuation_that_finds_nothing_to_go_on_with_ends_quietly() {
    let dir = TempDir::new("offload-order-settled");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 24, 2);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("no-models"));
    let vault = crate::credentials::MemoryVault::default();
    let done = crate::jobs::summarise_run_with(
        &svc,
        m,
        &SummaryStart::Continue {
            request: EntityId::from_bytes([9; 16]),
            word: OutcomeWord::Declined,
        },
        &CancelToken::new(),
        &models,
        (&vault, &NeverClaude, &NeverCodex),
        None,
        &mut |w| panic!("nothing to tell: {w}"),
        &mut || {},
    )
    .unwrap();
    assert!(done.settled);
    assert_eq!(done.note, None);
    assert_eq!(
        earshot_runtime::summary_order::run::read(&svc.store(), m).unwrap(),
        RunRecord::Absent,
        "nothing written"
    );
    // The runner turns it into the one quiet terminal event.
    struct Settles;
    impl JobWork for Settles {
        fn retranscribe(
            &self,
            _: &Services,
            _: EntityId,
            _: &CancelToken,
            _: &mut dyn FnMut(earshot_runtime::host::HostStep<'_>),
            _: &mut dyn FnMut(String),
        ) -> Result<earshot_runtime::edit::Retranscribed, IpcError> {
            unreachable!("only re-summaries here")
        }
        fn summarise(
            &self,
            _: &Services,
            _: EntityId,
            _: &CancelToken,
            _: &mut dyn FnMut(String),
        ) -> Result<SummaryResult, IpcError> {
            unreachable!("the runner asks summarise_run")
        }
        fn summarise_run(
            &self,
            _: &Services,
            _: EntityId,
            _: &SummaryStart,
            _: &CancelToken,
            _: &mut dyn FnMut(String),
        ) -> Result<JobSummary, IpcError> {
            Ok(JobSummary {
                settled: true,
                ..JobSummary::from(SummaryResult::Skipped {
                    reason: SkipReason::NotRequested,
                })
            })
        }
    }
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: JobSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let runner = JobRunner::new(
        Arc::new(Settles),
        sink,
        crate::activity::WorkGate::new(),
        &crate::cuda_settings::CudaLatch::resolved_off(),
    );
    runner
        .start_summary(
            &svc,
            m,
            SummaryStart::Continue {
                request: EntityId::from_bytes([9; 16]),
                word: OutcomeWord::Declined,
            },
            None,
        )
        .unwrap();
    runner.join();
    let events: Vec<JobEvent> = rx.try_iter().collect();
    assert_eq!(
        events,
        [JobEvent::Settled {
            meeting: m.to_string()
        }]
    );
}

/// Why re-summarise "On <device>" did not ask: from the step's outcome word, and "no final
/// transcript" only when the meeting has none (review of T12.63b).
#[test]
fn offload_named_run_reason_follows_its_outcome_word() {
    use crate::offload::not_asked_reason;
    use earshot_runtime::offload::Unavailable;
    assert_eq!(
        not_asked_reason(Some(OutcomeWord::Failed), false),
        Unavailable::NoFinalTranscript
    );
    assert_eq!(
        not_asked_reason(Some(OutcomeWord::Failed), true),
        Unavailable::NotAsked
    );
    assert_eq!(
        not_asked_reason(Some(OutcomeWord::Recording), true),
        Unavailable::Recording
    );
    assert_eq!(
        not_asked_reason(Some(OutcomeWord::Offline), true),
        Unavailable::Offline
    );
    assert_eq!(not_asked_reason(None, true), Unavailable::NotAsked);
}

/// A named computer run asks only the computer the user named: an answer about any other
/// device is not asked, and nothing is written (the run's trust boundary, review of T12.63b).
#[test]
fn offload_named_run_never_asks_a_device_it_was_not_decided_for() {
    use earshot_core::model::SummaryRequestRepo;
    let dir = TempDir::new("offload-order-mismatch");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 25, 2);
    let laptop = DeviceId::from_bytes([8; 16]);
    let writers = crate::offload::AskOnly {
        device: DESK,
        choice: OffloadChoice::Ask {
            device: DESK,
            name: "Invented desk".into(),
            writer: WriterKind::Local,
        },
    };
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Named(OrderStep::Computer(laptop)),
        &RunPlan {
            writers: &writers,
            local_not_now: &|| false,
            order: None,
        },
        &CancelToken::new(),
    );
    assert!(
        !matches!(out.summary, SummaryResult::Offloaded { .. }),
        "{:?}",
        out.summary
    );
    assert!(svc.store().summary_requests().unwrap().is_empty());
    // Its own computer is asked.
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Named(OrderStep::Computer(DESK)),
        &RunPlan {
            writers: &writers,
            local_not_now: &|| false,
            order: None,
        },
        &CancelToken::new(),
    );
    assert!(matches!(out.summary, SummaryResult::Offloaded { .. }));
}

/// A host that records each run handed back to this computer and starts it.
#[derive(Default)]
struct Continues(Mutex<Vec<WriteHere>>);

impl OffloadHost for Continues {
    fn writing(&self) -> Vec<earshot_core::identity::EntityId> {
        Vec::new()
    }
    fn cancel_writing(&self, _request: earshot_core::identity::EntityId) {}
    fn continue_run(&self, job: WriteHere) -> ContinueAnswer {
        self.0.lock().unwrap().push(job);
        ContinueAnswer::Started
    }
    fn write_for(&self, _: earshot_core::model::SummaryRequest) {
        panic!("nothing asked this computer")
    }
}

/// The meeting's waiting banner as `meeting_detail` gives it (T12.81).
fn banner(svc: &Services, m: EntityId) -> Option<String> {
    crate::detail::meeting_detail(svc, m).unwrap().waiting_for
}

const THEN_HERE: &str = "Waiting for Invented desk to write the summary. If it does not answer, this computer writes it.";
const THEN_NOTHING: &str = "Waiting for Invented desk to write the summary. If it does not answer, no summary is written. Re-summarise to try again.";

/// T12.81 A4: the banner names the computer and says what follows its step in an order run, and
/// what a request from before the order (no run record) does.
#[test]
fn offload_waiting_banner_follows_the_run_and_a_legacy_request() {
    let dir = TempDir::new("offload-banner-runs");
    let svc = services(&dir);
    crate::offload::remember_computer(DESK, Some("Invented desk".into()), None);
    let then_local = meeting_with_final(&svc, 31, 2);
    waiting_run(
        &svc,
        then_local,
        &[OrderStep::Computer(DESK), OrderStep::LOCAL],
    );
    assert_eq!(banner(&svc, then_local).as_deref(), Some(THEN_HERE));
    let alone = meeting_with_final(&svc, 32, 2);
    waiting_run(&svc, alone, &[OrderStep::Computer(DESK)]);
    assert_eq!(banner(&svc, alone).as_deref(), Some(THEN_NOTHING));
    let legacy = meeting_with_final(&svc, 33, 2);
    earshot_runtime::offload::ask(&svc, legacy, DESK)
        .unwrap()
        .expect("asked");
    assert!(matches!(
        earshot_runtime::summary_order::run::read(&svc.store(), legacy).unwrap(),
        RunRecord::Absent
    ));
    assert_eq!(banner(&svc, legacy).as_deref(), Some(THEN_HERE));
}

/// T12.81 A2: a summary created after the open request ends the banner, though the request's
/// own row has not arrived yet (sync can bring them in different pages).
#[test]
fn offload_a_summary_that_arrived_before_its_request_ends_the_banner() {
    let dir = TempDir::new("offload-banner-arrived");
    let svc = services(&dir);
    for (seed, run) in [(34, true), (35, false)] {
        let m = meeting_with_final(&svc, seed, 2);
        if run {
            waiting_run(&svc, m, &[OrderStep::Computer(DESK), OrderStep::LOCAL]);
        } else {
            earshot_runtime::offload::ask(&svc, m, DESK)
                .unwrap()
                .unwrap();
        }
        assert!(banner(&svc, m).is_some());
        put_summary_now(&svc, m, seed);
        let me = svc.store().device();
        assert_eq!(
            earshot_runtime::offload::waiting_for(&svc.store(), me, m).unwrap(),
            Some(DESK),
            "the request is still open"
        );
        assert_eq!(banner(&svc, m), None, "run: {run}");
    }
}

/// A live summary of `m`, created now.
fn put_summary_now(svc: &Services, m: EntityId, seed: u64) {
    use earshot_core::model::{MeetingRepo, Stamp, Summary};
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let body = earshot_core::summary::SummaryV1 {
        title: "Invented review".into(),
        tldr: "Written on the other computer.".into(),
        enhanced_notes: Vec::new(),
        decisions: Vec::new(),
        action_items: Vec::new(),
        open_questions: Vec::new(),
        topics: Vec::new(),
    };
    let s = Summary::new(
        m,
        earshot_core::prompt::Template::General,
        None,
        body,
        &mut stamp,
    );
    store.put_summary(s).unwrap();
}

/// T12.81 B1: a named request nobody claims is withdrawn by the offload step; the step names
/// the meeting (the window reads it once, after the step), and that one read shows the run's
/// account and no waiting banner.
#[test]
fn offload_a_named_request_that_times_out_clears_its_banner_and_shows_the_note() {
    let dir = TempDir::new("offload-named-timeout");
    let (svc, clock) = crate::testutil::services_with_clock(&dir);
    crate::offload::remember_computer(DESK, Some("Invented desk".into()), None);
    let m = meeting_with_final(&svc, 36, 2);
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Named(OrderStep::Computer(DESK)),
        &RunPlan {
            writers: &AskDesk,
            local_not_now: &|| false,
            order: None,
        },
        &CancelToken::new(),
    );
    assert!(matches!(out.summary, SummaryResult::Offloaded { .. }));
    assert_eq!(banner(&svc, m).as_deref(), Some(THEN_NOTHING));
    let host = Continues::default();
    let state = earshot_runtime::offload::Offload::new();
    let r = state.step(&svc, &mut || None, &host).unwrap();
    assert!(r.meetings.is_empty(), "not yet: {r:?}");
    clock.advance_ms(earshot_runtime::offload::CLAIM_WITHIN_MS);
    let r = state.step(&svc, &mut || None, &host).unwrap();
    assert_eq!(r.meetings.iter().copied().collect::<Vec<_>>(), vec![m]);
    assert!(
        host.0.lock().unwrap().is_empty(),
        "a named run is never continued"
    );
    let detail = crate::detail::meeting_detail(&svc, m).unwrap();
    assert_eq!(detail.waiting_for, None);
    assert_eq!(
        detail.summary_run.as_deref(),
        Some("Invented desk could not write the summary: no reply.")
    );
}

/// T12.81 B1: order and legacy runs handed back name their meetings in the same step that starts
/// their jobs (the jobs' own events then follow the page).
#[test]
fn offload_runs_handed_back_name_their_meetings_with_their_jobs() {
    let dir = TempDir::new("offload-order-timeout");
    let (svc, clock) = crate::testutil::services_with_clock(&dir);
    let m = meeting_with_final(&svc, 37, 2);
    waiting_run(&svc, m, &[OrderStep::Computer(DESK), OrderStep::LOCAL]);
    let legacy = meeting_with_final(&svc, 38, 2);
    earshot_runtime::offload::ask(&svc, legacy, DESK)
        .unwrap()
        .unwrap();
    clock.advance_ms(earshot_runtime::offload::CLAIM_WITHIN_MS);
    let host = Continues::default();
    let r = earshot_runtime::offload::Offload::new()
        .step(&svc, &mut || None, &host)
        .unwrap();
    let mut both = vec![m, legacy];
    both.sort();
    assert_eq!(r.meetings.iter().copied().collect::<Vec<_>>(), both);
    let jobs: Vec<EntityId> = host.0.lock().unwrap().iter().map(|w| w.meeting).collect();
    assert_eq!(jobs.len(), 2, "both continue here");
    assert!(jobs.contains(&m) && jobs.contains(&legacy));
    assert_eq!(banner(&svc, m), None);
    assert_eq!(banner(&svc, legacy), None);
}
