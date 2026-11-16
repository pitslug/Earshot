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

#[test]
fn offload_host_continues_an_order_run_with_the_next_step() {
    let dir = TempDir::new("offload-order-continue");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 21, 2);
    let request = waiting_run(&svc, m, &[OrderStep::Computer(DESK), OrderStep::LOCAL]);
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
        helper: DESK,
        why: HereReason::NotClaimed,
        continuation: Continuation::Order {
            word: OutcomeWord::NoReply,
        },
    });
    assert_eq!(answer, ContinueAnswer::Started);
    let first = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(
        first,
        JobEvent::Warning {
            meeting: m.to_string(),
            message: crate::summary_order::continue_note(
                DESK,
                &crate::summary_order::StepContext::read(&svc)
            ),
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
        fn continue_run(&self, job: WriteHere) -> ContinueAnswer {
            panic!("a named run is never continued: {job:?}")
        }
        fn write_for(&self, _: earshot_core::model::SummaryRequest) {
            panic!("nothing asked this computer")
        }
    }
    let state = earshot_runtime::offload::Offload::new();
    state.step(&a.svc, &mut || None, &NoJobs).unwrap();
    let detail = crate::detail::meeting_detail(&a.svc, m).unwrap();
    assert_eq!(
        detail.summary_run.as_deref(),
        Some("Invented desk could not write the summary: declined.")
    );
}
