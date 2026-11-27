//! T12.63a: running the order of summary writers (docs/summary-order.md section 4), with fake
//! writers over the loopback mock endpoint. Computer steps across two devices are in
//! `offload_tests.rs`. Every test here sits in `summary_order::run`, so
//! `cargo test -p earshot-runtime summary_order` selects them.

use std::collections::HashMap;
use std::sync::Mutex;

use earshot_core::asr::CancelToken;
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::{MeetingRepo, RequestState, SummaryRequest, SummaryRequestRepo};
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};

use super::*;
use crate::host::{PreparedSummary, SkipReason, SummaryResult};
use crate::summary::{SummaryConfig, SummaryService};
use crate::summary_order::{SummaryOrder, set_summary_order};
use crate::testutil::{TempDir, meeting, services};

const DESK: DeviceId = DeviceId::from_bytes([7; 16]);

const LOCAL: OrderStep = OrderStep::LOCAL;
const ENDPOINT: OrderStep = OrderStep::Writer(WriterStep::Endpoint);
const CLAUDE: OrderStep = OrderStep::Writer(WriterStep::ClaudeCode);
const CODEX: OrderStep = OrderStep::Writer(WriterStep::Codex);

fn reply(title: &str) -> MockResponse {
    let summary = SummaryV1 {
        title: title.into(),
        tldr: "The team agreed to ship on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["release".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

fn failing() -> MockServer {
    MockServer::start(vec![MockResponse::status(500, "{}")]).unwrap()
}

/// What a fake writer step builds.
enum Fake {
    /// A service over this mock endpoint.
    Mock(String),
    /// No summary, for this reason.
    Skip(SkipReason),
    /// Set-up fails with this error.
    Stop(fn() -> RuntimeError),
}

/// What a fake runs when a computer is asked about.
type Meanwhile = Box<dyn Fn(&Services)>;

/// Fake [`StepWriters`]: what each writer builds and each computer answers, and every step asked.
#[derive(Default)]
struct Writers {
    writers: HashMap<WriterStep, Fake>,
    computers: HashMap<DeviceId, OffloadChoice>,
    asked: Mutex<Vec<OrderStep>>,
    /// Runs when a computer is asked about (to change the store meanwhile).
    on_computer: Option<Meanwhile>,
}

impl Writers {
    fn meanwhile(mut self, f: impl Fn(&Services) + 'static) -> Self {
        self.on_computer = Some(Box::new(f));
        self
    }

    fn with(mut self, step: WriterStep, fake: Fake) -> Self {
        self.writers.insert(step, fake);
        self
    }

    fn mock(self, step: WriterStep, server: &MockServer) -> Self {
        self.with(step, Fake::Mock(server.endpoint().to_string()))
    }

    fn computer(mut self, device: DeviceId, choice: OffloadChoice) -> Self {
        self.computers.insert(device, choice);
        self
    }

    fn asked(&self) -> Vec<OrderStep> {
        self.asked.lock().unwrap().clone()
    }
}

impl StepWriters for Writers {
    fn writer(&self, services: &Services, step: WriterStep) -> Built {
        self.asked.lock().unwrap().push(OrderStep::Writer(step));
        match self.writers.get(&step) {
            Some(Fake::Mock(url)) => Built::Service(Box::new(PreparedSummary {
                service: SummaryService::new(
                    &SummaryConfig::new(url.as_str(), "test-model"),
                    services.clock.clone(),
                )
                .unwrap(),
                local: None,
            })),
            Some(Fake::Skip(reason)) => Built::Skip(reason.clone()),
            Some(Fake::Stop(e)) => Built::Stop(e()),
            None => panic!("{step:?} was not expected to be asked"),
        }
    }

    fn computer(&self, services: &Services, device: DeviceId) -> OffloadChoice {
        self.asked.lock().unwrap().push(OrderStep::Computer(device));
        if let Some(f) = &self.on_computer {
            f(services);
        }
        self.computers
            .get(&device)
            .cloned()
            .unwrap_or_else(|| crate::offload::decide(Some(device), None, false))
    }
}

fn plan(writers: &Writers) -> RunPlan<'_> {
    RunPlan {
        writers,
        local_not_now: &|| false,
        order: None,
    }
}

fn order(steps: &[OrderStep]) -> SummaryOrder {
    SummaryOrder::new(steps.to_vec()).unwrap()
}

fn not_installed() -> SkipReason {
    SkipReason::NotInstalled {
        model: "qwen3.5-4b",
        why: "not installed".into(),
    }
}

fn record(svc: &Services, m: EntityId) -> SummaryRun {
    match read(&svc.store(), m).unwrap() {
        RunRecord::Read(r) => r,
        other => panic!("{other:?}"),
    }
}

fn results(out: &RunOutcome) -> Vec<StepResult> {
    out.steps.iter().map(|s| s.result).collect()
}

fn kept_local(svc: &Services, m: EntityId) {
    let mut db = svc.store();
    let mut row = db.meeting(m).unwrap().unwrap();
    row.summary_kept_local = true;
    db.put_meeting(row).unwrap();
}

// ---- the move-on table ----

#[test]
fn summary_order_run_move_on_table_lists_each_result_and_stops_on_the_rest() {
    use earshot_llm::{ClaudeCodeError, CodexError, LlmError};
    use earshot_llm_local::summarize::SummaryFailure;

    use crate::summary::LocalSummaryError;
    let llm = |e: LlmError| RuntimeError::Llm(e);
    let local = |e: LocalSummaryError| RuntimeError::LocalSummary(e);
    let invalid = || {
        llm(LlmError::Invalid {
            attempts: 3,
            errors: vec![],
        })
    };
    let prompt = || {
        llm(LlmError::Prompt(
            earshot_core::prompt::PromptError::ContextTooSmall {
                needed: 9,
                available: 1,
            },
        ))
    };
    let http = || {
        llm(LlmError::Http {
            status: 500,
            message: String::new(),
        })
    };
    // Local model: every local failure (after its own CPU retry), an invalid reply, too long.
    for e in [
        local(LocalSummaryError::Load(earshot_llm_local::LlmError::Load(
            "x".into(),
        ))),
        local(LocalSummaryError::LoadRetried {
            gpu: Box::new(LocalSummaryError::GpuStalled),
            cpu: Box::new(LocalSummaryError::Load(earshot_llm_local::LlmError::Load(
                "x".into(),
            ))),
        }),
        local(LocalSummaryError::Chunks(
            earshot_core::prompt::TimeChunkError::ZeroWindow,
        )),
        local(LocalSummaryError::GpuStalled),
        local(LocalSummaryError::LoadStalled),
        local(LocalSummaryError::GpuChildStalled),
        local(LocalSummaryError::GpuChildDied { why: "x".into() }),
        local(LocalSummaryError::CudaFilesChanged { why: "x".into() }),
        local(LocalSummaryError::ChildRun {
            message: "x".into(),
            kind: None,
        }),
        local(LocalSummaryError::Run(SummaryFailure::Invalid {
            step: "single",
            attempts: 3,
            errors: vec![],
            last_reply_tail: String::new(),
        })),
        invalid(),
        prompt(),
    ] {
        // Every one moves on; which word it moves on with is its kind (T14.66), pinned by
        // `summary_order_run_local_failures_move_on_with_their_kind`.
        assert!(failure_word(WriterStep::Local, &e).is_some(), "{e}");
    }
    // Endpoint: HTTP, transport, refused, format, invalid, too long; a key that cannot be read.
    for e in [
        http(),
        llm(LlmError::Net(earshot_net::NetError::TooManyRedirects)),
        llm(LlmError::Refused { category: None }),
        llm(LlmError::FormatRejected {
            status: 400,
            message: String::new(),
        }),
        invalid(),
        prompt(),
    ] {
        assert_eq!(
            failure_word(WriterStep::Endpoint, &e),
            Some(OutcomeWord::Failed),
            "{e}"
        );
    }
    assert_eq!(
        failure_word(
            WriterStep::Endpoint,
            &RuntimeError::ApiKey(crate::summary_settings::ApiKeyError::Missing)
        ),
        Some(OutcomeWord::NoApiKey)
    );
    // Claude Code and Codex: their own errors, an invalid reply, too long.
    for e in [
        ClaudeCodeError::SignInBusy,
        ClaudeCodeError::TimedOut(std::time::Duration::from_secs(1)),
        ClaudeCodeError::PromptTooLong,
    ] {
        assert_eq!(
            failure_word(WriterStep::ClaudeCode, &llm(LlmError::ClaudeCode(e))),
            Some(OutcomeWord::Failed)
        );
    }
    assert_eq!(
        failure_word(
            WriterStep::ClaudeCode,
            &llm(LlmError::ClaudeCode(ClaudeCodeError::NotSignedIn))
        ),
        Some(OutcomeWord::NotSignedIn)
    );
    for e in [
        CodexError::AccessDenied,
        CodexError::ToolRan,
        CodexError::UnknownEvent,
    ] {
        assert_eq!(
            failure_word(WriterStep::Codex, &llm(LlmError::Codex(e))),
            Some(OutcomeWord::Failed)
        );
    }
    assert_eq!(
        failure_word(WriterStep::Codex, &invalid()),
        Some(OutcomeWord::Failed)
    );
    assert_eq!(
        failure_word(WriterStep::ClaudeCode, &prompt()),
        Some(OutcomeWord::Failed)
    );

    // Stops: cancellation, a changed transcript or deleted meeting, busy, store, a handover that
    // could not be recorded, and anything not listed for the step.
    let stops = [
        (WriterStep::Endpoint, llm(LlmError::Cancelled)),
        (
            WriterStep::Local,
            local(LocalSummaryError::Run(SummaryFailure::Cancelled)),
        ),
        (
            WriterStep::Endpoint,
            RuntimeError::Conflict("the transcript changed during the summary"),
        ),
        (
            WriterStep::Local,
            RuntimeError::MeetingBusy(EntityId::from_bytes([1; 16])),
        ),
        (
            WriterStep::Endpoint,
            RuntimeError::Store(earshot_store::StoreError::Corrupt("x".into())),
        ),
        (
            WriterStep::ClaudeCode,
            llm(LlmError::ClaudeCode(ClaudeCodeError::NotRecorded)),
        ),
        (
            WriterStep::Codex,
            llm(LlmError::Codex(CodexError::NotRecorded)),
        ),
        (WriterStep::Endpoint, llm(LlmError::BadResponse("x".into()))),
        // Another writer's error is not listed for this step.
        (WriterStep::Local, http()),
        (
            WriterStep::Endpoint,
            llm(LlmError::ClaudeCode(ClaudeCodeError::SignInBusy)),
        ),
        (WriterStep::Endpoint, RuntimeError::Internal("x")),
    ];
    for (step, e) in stops {
        assert_eq!(failure_word(step, &e), None, "{step:?} {e}");
    }
}

#[test]
fn summary_order_run_skip_and_unavailable_words() {
    use earshot_llm::{ClaudeCodeError, CodexError};
    assert_eq!(skip_word(&not_installed()), Some(OutcomeWord::NotInstalled));
    assert_eq!(
        skip_word(&SkipReason::ChosenNotInstalled { model: "x" }),
        Some(OutcomeWord::NotInstalled)
    );
    assert_eq!(
        skip_word(&SkipReason::KeptLocalNoModel { model: "x" }),
        Some(OutcomeWord::NotInstalled)
    );
    assert_eq!(
        skip_word(&SkipReason::NoApiKey(
            crate::summary_settings::ApiKeyError::Missing
        )),
        Some(OutcomeWord::NoApiKey)
    );
    assert_eq!(
        skip_word(&SkipReason::ClaudeCodeUnavailable(
            ClaudeCodeError::NotSignedIn
        )),
        Some(OutcomeWord::NotSignedIn)
    );
    assert_eq!(
        skip_word(&SkipReason::CodexUnavailable(CodexError::NotInstalled)),
        Some(OutcomeWord::NotInstalled)
    );
    assert_eq!(skip_word(&SkipReason::NotRequested), None);
    assert_eq!(skip_word(&SkipReason::PostFailed), None);
    for (why, word) in [
        (Unavailable::SyncOff, OutcomeWord::Offline),
        (Unavailable::Unreachable, OutcomeWord::Offline),
        (Unavailable::Offline, OutcomeWord::Offline),
        (Unavailable::NotOffering, OutcomeWord::NotOffering),
        (Unavailable::NoWriter, OutcomeWord::NoWriter),
        (Unavailable::Recording, OutcomeWord::Recording),
        (Unavailable::NoFinalTranscript, OutcomeWord::Failed),
        (Unavailable::NotAsked, OutcomeWord::Failed),
    ] {
        assert_eq!(unavailable_word(why), word);
    }
    // The words the user reads are fixed and distinct.
    let texts: std::collections::BTreeSet<_> = OutcomeWord::ALL.iter().map(|w| w.text()).collect();
    assert_eq!(texts.len(), OutcomeWord::ALL.len());
    assert_eq!(
        OutcomeWord::SkippedKeptLocal.text(),
        "skipped (kept on this computer)"
    );
}

// ---- the record ----

#[test]
fn summary_order_run_record_bytes_are_pinned() {
    let run = SummaryRun {
        run: 42,
        origin: RunOrigin::Order,
        steps: vec![OrderStep::Computer(DESK), LOCAL],
        cursor: 1,
        outcomes: vec![StepOutcome {
            step: 0,
            word: OutcomeWord::NoReply,
        }],
        waiting_on: None,
        state: RunState::Continuing,
        writer: None,
        stopped: None,
    };
    let mut want = 1u32.to_le_bytes().to_vec();
    want.extend_from_slice(
        br#"{"run":42,"origin":"order","steps":[{"computer":"07070707-0707-0707-0707-070707070707"},{"writer":"local"}],"cursor":1,"outcomes":[{"step":0,"word":"no_reply"}],"waiting_on":null,"state":"continuing","writer":null}"#,
    );
    assert_eq!(encode(&run), want);
    assert_eq!(decode(Some(&want)), RunRecord::Read(run));
}

#[test]
fn summary_order_run_record_that_cannot_be_read_is_unreadable() {
    assert_eq!(decode(None), RunRecord::Absent);
    assert_eq!(decode(Some(b"junk")), RunRecord::Unreadable);
    let with = |v: u32, json: &str| {
        let mut b = v.to_le_bytes().to_vec();
        b.extend_from_slice(json.as_bytes());
        decode(Some(&b))
    };
    let ok =
        r#"{"run":1,"origin":"named","steps":[{"writer":"local"}],"cursor":0,"state":"running"}"#;
    assert!(matches!(with(1, ok), RunRecord::Read(_)));
    assert_eq!(
        with(2, ok),
        RunRecord::Unreadable,
        "a version this build does not know"
    );
    assert_eq!(
        with(
            1,
            r#"{"run":1,"origin":"named","steps":[{"writer":"local"}],"cursor":5,"state":"running"}"#
        ),
        RunRecord::Unreadable,
        "a cursor past its steps"
    );
    assert_eq!(
        with(
            1,
            r#"{"run":1,"origin":"named","steps":[],"cursor":0,"state":"waiting","outcomes":[{"step":0,"word":"failed"}]}"#
        ),
        RunRecord::Unreadable
    );
    assert_eq!(
        with(
            1,
            r#"{"run":1,"origin":"order","steps":[],"cursor":0,"state":"ended","extra":true}"#
        ),
        RunRecord::Unreadable
    );
    assert_eq!(
        with(
            1,
            r#"{"run":1,"origin":"order","steps":[],"cursor":0,"state":"a-new-state"}"#
        ),
        RunRecord::Unreadable
    );
}

// ---- running ----

#[test]
fn summary_order_run_moves_on_through_the_table_and_the_audit_is_the_union() {
    let dir = TempDir::new("order-run-moves");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 1);
    let bad = failing();
    let good = MockServer::start(vec![reply("Written by the third step")]).unwrap();
    let writers = Writers::default()
        .mock(WriterStep::Endpoint, &bad)
        .with(
            WriterStep::ClaudeCode,
            Fake::Skip(SkipReason::ClaudeCodeUnavailable(
                earshot_llm::ClaudeCodeError::NotSignedIn,
            )),
        )
        .mock(WriterStep::Local, &good);
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[ENDPOINT, CLAUDE, LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(
        matches!(out.summary, SummaryResult::Stored { .. }),
        "{:?}",
        out.summary
    );
    assert_eq!(
        results(&out),
        vec![
            StepResult::Moved(OutcomeWord::Failed),
            StepResult::Moved(OutcomeWord::NotSignedIn),
            StepResult::Wrote
        ]
    );
    // Every endpoint any step reached is in the audit.
    let urls: Vec<String> = out.network.iter().map(|e| e.host.clone()).collect();
    assert_eq!(out.network.len(), 2, "{urls:?}");
    assert_eq!((bad.requests().len(), good.requests().len()), (1, 1));
    let r = record(&svc, m);
    assert_eq!(
        (r.state, r.writer, r.waiting_on),
        (RunState::Ended, Some(2), None)
    );
    assert_eq!(r.origin, RunOrigin::Order);
}

#[test]
fn summary_order_run_every_step_failed_ends_with_no_summary() {
    let dir = TempDir::new("order-run-all-failed");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 2);
    let bad = failing();
    let writers = Writers::default()
        .with(WriterStep::Local, Fake::Skip(not_installed()))
        .mock(WriterStep::Endpoint, &bad);
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[LOCAL, ENDPOINT])),
        &plan(&writers),
        &CancelToken::new(),
    );
    // The last step's failure, as one writer's result says it.
    assert!(
        matches!(out.summary, SummaryResult::Failed { .. }),
        "{:?}",
        out.summary
    );
    assert_eq!(
        results(&out),
        vec![
            StepResult::Moved(OutcomeWord::NotInstalled),
            StepResult::Moved(OutcomeWord::Failed)
        ]
    );
    let r = record(&svc, m);
    assert_eq!((r.state, r.writer), (RunState::Ended, None));
    assert!(svc.store().summaries(m).unwrap().is_empty());
}

#[test]
fn summary_order_run_a_one_step_order_keeps_todays_result() {
    let dir = TempDir::new("order-run-one-step");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 3);
    let writers = Writers::default().with(
        WriterStep::Endpoint,
        Fake::Skip(SkipReason::NoApiKey(
            crate::summary_settings::ApiKeyError::Missing,
        )),
    );
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[ENDPOINT])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(matches!(
        out.summary,
        SummaryResult::Skipped {
            reason: SkipReason::NoApiKey(_)
        }
    ));
}

#[test]
fn summary_order_run_stops_on_anything_not_listed() {
    let dir = TempDir::new("order-run-stops");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 4);
    let writers = Writers::default()
        .with(
            WriterStep::ClaudeCode,
            Fake::Stop(|| RuntimeError::MeetingBusy(EntityId::from_bytes([3; 16]))),
        )
        .with(WriterStep::Local, Fake::Skip(not_installed()));
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[CLAUDE, LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(matches!(
        out.summary,
        SummaryResult::Failed {
            error: RuntimeError::MeetingBusy(_),
            ..
        }
    ));
    assert_eq!(writers.asked(), vec![CLAUDE], "no next step");
    assert_eq!(record(&svc, m).state, RunState::Ended);
    // A cancelled run stops before anything is built.
    let cancel = CancelToken::new();
    cancel.cancel();
    let none = Writers::default();
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[CLAUDE, LOCAL])),
        &plan(&none),
        &cancel,
    );
    assert!(matches!(out.summary, SummaryResult::Cancelled));
    assert!(none.asked().is_empty());
    // A summary the endpoint answers with a changed transcript is a conflict: it stops too.
    let server = MockServer::start(vec![reply("Never stored")]).unwrap();
    let conflict = Writers::default()
        .mock(WriterStep::Endpoint, &server)
        .with(WriterStep::Local, Fake::Skip(not_installed()));
    let deleted = meeting(&svc, "Deleted", 5);
    svc.delete_meeting(deleted).unwrap();
    let out = run_summary_order(
        &svc,
        deleted,
        RunStart::Order(order(&[ENDPOINT, LOCAL])),
        &plan(&conflict),
        &CancelToken::new(),
    );
    assert!(
        matches!(out.summary, SummaryResult::Failed { .. }),
        "{:?}",
        out.summary
    );
    assert!(
        conflict.asked().is_empty(),
        "a deleted meeting starts no run"
    );
}

#[test]
fn summary_order_run_none_is_no_summary_and_asks_nothing() {
    let dir = TempDir::new("order-run-none");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 6);
    let writers = Writers::default();
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(SummaryOrder::none()),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(matches!(
        out.summary,
        SummaryResult::Skipped {
            reason: SkipReason::NotRequested
        }
    ));
    assert!(writers.asked().is_empty());
    assert_eq!(record(&svc, m).state, RunState::Ended);
    // Not for a kept-local meeting either: no summary stays no summary.
    kept_local(&svc, m);
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(SummaryOrder::none()),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(matches!(out.summary, SummaryResult::Skipped { .. }));
    assert!(writers.asked().is_empty());
}

#[test]
fn summary_order_run_kept_local_drops_remote_steps_and_ends_with_the_local_model() {
    let dir = TempDir::new("order-run-kept");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 7);
    kept_local(&svc, m);
    // No local model installed: today's kept-local result, and no remote step was ever built or
    // probed.
    let writers = Writers::default().with(WriterStep::Local, Fake::Skip(not_installed()));
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[CLAUDE, ENDPOINT, CODEX])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert_eq!(writers.asked(), vec![LOCAL]);
    assert!(
        matches!(
            out.summary,
            SummaryResult::Skipped {
                reason: SkipReason::KeptLocalNoModel { .. }
            }
        ),
        "{:?}",
        out.summary
    );
    assert_eq!(
        results(&out),
        vec![
            StepResult::Moved(OutcomeWord::SkippedKeptLocal),
            StepResult::Moved(OutcomeWord::SkippedKeptLocal),
            StepResult::Moved(OutcomeWord::SkippedKeptLocal),
            StepResult::Moved(OutcomeWord::NotInstalled),
        ]
    );
    let r = record(&svc, m);
    assert_eq!(
        r.steps,
        vec![CLAUDE, ENDPOINT, CODEX, LOCAL],
        "the implicit local step"
    );
    // A computer step stays: that computer writes it with its local model or declines.
    let w2 = Writers::default()
        .computer(
            DESK,
            OffloadChoice::HereInstead {
                device: DESK,
                name: None,
                why: Unavailable::Offline,
            },
        )
        .with(WriterStep::Local, Fake::Skip(not_installed()));
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[CLAUDE, OrderStep::Computer(DESK)])),
        &plan(&w2),
        &CancelToken::new(),
    );
    assert_eq!(w2.asked(), vec![OrderStep::Computer(DESK), LOCAL]);
    assert_eq!(
        results(&out),
        vec![
            StepResult::Moved(OutcomeWord::SkippedKeptLocal),
            StepResult::Moved(OutcomeWord::Offline),
            StepResult::Moved(OutcomeWord::NotInstalled),
        ]
    );
}

#[test]
fn summary_order_run_named_runs_are_not_filtered_and_have_no_next_step() {
    let dir = TempDir::new("order-run-named");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 8);
    let bad = failing();
    let writers = Writers::default().mock(WriterStep::Endpoint, &bad);
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Named(ENDPOINT),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(matches!(out.summary, SummaryResult::Failed { .. }));
    assert_eq!(writers.asked(), vec![ENDPOINT], "one step only");
    let r = record(&svc, m);
    assert_eq!((r.origin, r.state), (RunOrigin::Named, RunState::Ended));
    // A remote writer named for a meeting kept on this computer is today's refusal, before
    // anything is built or probed: never the local model instead.
    kept_local(&svc, m);
    let quiet = Writers::default();
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Named(CLAUDE),
        &plan(&quiet),
        &CancelToken::new(),
    );
    assert!(matches!(
        out.summary,
        SummaryResult::Failed {
            error: RuntimeError::SummaryKeptLocal(_),
            ..
        }
    ));
    assert!(quiet.asked().is_empty());
}

#[test]
fn summary_order_run_a_local_step_the_hold_did_not_cover_is_not_now() {
    let dir = TempDir::new("order-run-not-now");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 9);
    let good = MockServer::start(vec![reply("Written by the endpoint")]).unwrap();
    let writers = Writers::default().mock(WriterStep::Endpoint, &good);
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[LOCAL, ENDPOINT])),
        &RunPlan {
            writers: &writers,
            local_not_now: &|| true,
            order: None,
        },
        &CancelToken::new(),
    );
    assert!(matches!(out.summary, SummaryResult::Stored { .. }));
    assert_eq!(
        writers.asked(),
        vec![ENDPOINT],
        "the local model was never built"
    );
    assert_eq!(
        results(&out),
        vec![StepResult::Moved(OutcomeWord::NotNow), StepResult::Wrote]
    );
}

#[test]
fn summary_order_run_not_now_needs_an_open_newer_ticket_and_an_uncovered_hold() {
    use crate::meeting_queue::{Device, MeetingQueue, SummaryHold};
    let queue = MeetingQueue::new();
    let older = queue.enter();
    older.set_summary(SummaryHold::OffThisComputer);
    assert!(!local_not_now(&older), "no newer meeting: it runs");
    let newer = queue.enter();
    assert!(local_not_now(&older));
    older.set_summary(SummaryHold::Local {
        device: Device::Cpu,
        size: 1,
    });
    assert!(!local_not_now(&older), "the hold covered the local model");
    older.set_summary(SummaryHold::OffThisComputer);
    drop(newer);
    assert!(!local_not_now(&older));
}

#[test]
fn summary_order_run_start_withdraws_the_open_request_it_asked() {
    let dir = TempDir::new("order-run-withdraw");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 10);
    let asked = crate::offload::ask(&svc, m, DESK).unwrap().unwrap();
    let writers = Writers::default().with(WriterStep::Local, Fake::Skip(not_installed()));
    let _ = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    let r = svc.store().summary_request(asked).unwrap().unwrap();
    assert_eq!(r.state, RequestState::Withdrawn);
    assert!(crate::offload::authored(&r));
}

#[test]
fn summary_order_run_waiting_with_a_newer_summary_reads_as_ended() {
    let dir = TempDir::new("order-run-view");
    let (svc, clock) = services(&dir);
    let m = meeting(&svc, "Planning", 11);
    assert_eq!(run_view(&svc.store(), m).unwrap(), RunView::None);
    let writers = Writers::default().computer(
        DESK,
        OffloadChoice::Ask {
            device: DESK,
            name: "Invented desk".into(),
            writer: WriterKind::Local,
        },
    );
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    let SummaryResult::Offloaded { request, .. } = out.summary else {
        panic!("{:?}", out.summary);
    };
    assert_eq!(results(&out), vec![StepResult::Asked, StepResult::NotTried]);
    let RunView::Waiting(w) = run_view(&svc.store(), m).unwrap() else {
        panic!("waiting");
    };
    assert_eq!((w.waiting_on, w.cursor), (Some(request), 1));
    // The other computer's summary arrives (here stored directly) before the asker's step: the
    // view reads it as ended, never interrupted.
    clock.advance_ms(1000);
    let view = crate::view::MeetingView::load(&**svc.store(), m)
        .unwrap()
        .unwrap();
    crate::summary::store_summary(
        &svc,
        &view,
        "other".into(),
        &SummaryV1 {
            title: "From the desk".into(),
            tldr: "Done.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        },
        &mut *crate::os_seeded_rng(),
    )
    .unwrap();
    let RunView::Ended(e) = run_view(&svc.store(), m).unwrap() else {
        panic!("ended");
    };
    assert_eq!((e.writer, e.state), (Some(0), RunState::Ended));
    // The record itself still waits until the asker's step reads the request's `Done`.
    assert_eq!(record(&svc, m).state, RunState::Waiting);
}

#[test]
fn summary_order_run_unfinished_and_unreadable_records_read_as_interrupted() {
    let dir = TempDir::new("order-run-interrupted");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 12);
    let mut running = SummaryRun::new(RunOrigin::Order);
    running.steps = vec![LOCAL];
    crate::local::set(&mut svc.store(), &record_key(m), &encode(&running)).unwrap();
    assert!(matches!(
        run_view(&svc.store(), m).unwrap(),
        RunView::Unfinished(_)
    ));
    crate::local::set(&mut svc.store(), &record_key(m), b"damaged").unwrap();
    assert_eq!(run_view(&svc.store(), m).unwrap(), RunView::Unreadable);
}

#[test]
fn summary_order_run_start_checks_name_the_steps_known_to_fail() {
    let dir = TempDir::new("order-run-checks");
    let (svc, _) = services(&dir);
    let good = MockServer::start(vec![]).unwrap();
    let writers = Writers::default()
        .with(
            WriterStep::ClaudeCode,
            Fake::Skip(SkipReason::ClaudeCodeUnavailable(
                earshot_llm::ClaudeCodeError::NotSignedIn,
            )),
        )
        .mock(WriterStep::Local, &good)
        .computer(
            DESK,
            OffloadChoice::HereInstead {
                device: DESK,
                name: None,
                why: Unavailable::Recording,
            },
        );
    let checks = start_checks(&svc, &[CLAUDE, OrderStep::Computer(DESK), LOCAL], &writers);
    assert_eq!(
        checks,
        vec![(0, OutcomeWord::NotSignedIn), (1, OutcomeWord::Recording)]
    );
    assert!(good.requests().is_empty(), "a check sends nothing");
}

#[test]
fn summary_order_run_the_decision_is_made_when_the_summary_is_due() {
    let dir = TempDir::new("order-run-due");
    let (svc, _) = services(&dir);
    // The order at meeting start says Claude Code; it is removed during the meeting.
    set_summary_order(&mut svc.store(), &order(&[CLAUDE]), None).unwrap();
    let outcome = crate::host::tests::recorded(&svc, 401);
    set_summary_order(&mut svc.store(), &order(&[LOCAL]), None).unwrap();
    let good = MockServer::start(vec![reply("Written by the local step")]).unwrap();
    let writers = Writers::default().mock(WriterStep::Local, &good);
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let cancel = CancelToken::new();
    let p = plan(&writers);
    let host = crate::host::after_meeting_with(
        &svc,
        &outcome,
        &crate::host::AfterMeeting {
            models: &models,
            model: "base.en",
            order: &[crate::backends::Accelerator::Cpu],
            post: true,
            whole_pass: false,
            summary: None,
            no_summary: SkipReason::NotRequested,
            post_cancel: &cancel,
            summary_cancel: &cancel,
            offload: None,
            order_run: Some(&p),
        },
        crate::host::tests::fake_post,
        &mut |_| {},
    );
    assert!(
        matches!(host.summary, SummaryResult::Stored { .. }),
        "{:?}",
        host.summary
    );
    assert_eq!(
        writers.asked(),
        vec![LOCAL],
        "Claude Code never received the meeting"
    );
    assert_eq!(host.steps.len(), 1);
    assert_eq!(host.network.len(), 1);
}

#[test]
fn summary_order_run_a_failed_final_pass_starts_no_run() {
    let dir = TempDir::new("order-run-post-failed");
    let (svc, _) = services(&dir);
    let outcome = crate::host::tests::recorded(&svc, 402);
    let writers = Writers::default();
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let cancel = CancelToken::new();
    let p = plan(&writers);
    let host = crate::host::after_meeting_with(
        &svc,
        &outcome,
        &crate::host::AfterMeeting {
            models: &models,
            model: "base.en",
            order: &[crate::backends::Accelerator::Cpu],
            post: true,
            whole_pass: false,
            summary: None,
            no_summary: SkipReason::NotRequested,
            post_cancel: &cancel,
            summary_cancel: &cancel,
            offload: None,
            order_run: Some(&p),
        },
        || Err::<crate::host::tests::Fakes, _>(RuntimeError::Internal("no models")),
        &mut |_| {},
    );
    assert!(matches!(
        host.summary,
        SummaryResult::Skipped {
            reason: SkipReason::PostFailed
        }
    ));
    assert!(writers.asked().is_empty());
    assert_eq!(
        read(&svc.store(), outcome.meeting).unwrap(),
        RunRecord::Absent
    );
}

#[test]
fn summary_order_run_a_helper_runs_only_its_advertised_writer() {
    let dir = TempDir::new("order-run-helper");
    let (svc, _) = services(&dir);
    let me = svc.store().device();
    let asker = DeviceId::from_bytes([5; 16]);
    let m = meeting(&svc, "Planning", 13);
    // This computer's own order is [local, Claude Code]; its local model fails.
    set_summary_order(&mut svc.store(), &order(&[LOCAL, CLAUDE]), None).unwrap();
    let bad = failing();
    let writers = Writers::default().mock(WriterStep::Local, &bad);
    // A request another computer asked, claimed here.
    let request = crate::edit::write(&svc, |tx, stamp| {
        let mut r = SummaryRequest::new(m, me, asker, 2, stamp);
        r.state = RequestState::Claimed;
        r.meta.touch(stamp.hlc, stamp.clock);
        tx.put_summary_request(r.clone())?;
        Ok(r)
    })
    .unwrap();
    let queue = crate::meeting_queue::MeetingQueue::new();
    let done = crate::offload::write_for_request(
        &svc,
        &queue,
        &request,
        &|privacy| helper_writer(&svc, &writers, privacy),
        &CancelToken::new(),
    );
    assert_eq!(
        done,
        crate::offload::HelperOutcome::Declined(reason::FAILED)
    );
    assert_eq!(writers.asked(), vec![LOCAL], "Claude Code was never called");
    assert_eq!(bad.requests().len(), 1);
    assert_eq!(
        svc.store()
            .summary_request(request.id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Declined
    );
}

#[test]
fn summary_order_run_a_helper_turns_a_remote_kind_local_for_a_kept_meeting() {
    let dir = TempDir::new("order-run-helper-kept");
    let (svc, _) = services(&dir);
    set_summary_order(&mut svc.store(), &order(&[CLAUDE, LOCAL]), None).unwrap();
    let writers = Writers::default().with(WriterStep::Local, Fake::Skip(not_installed()));
    let w = helper_writer(&svc, &writers, crate::keep_local::SummaryPrivacy::KeptLocal);
    assert!(matches!(
        w,
        HelperWriter::Decline(reason::KEPT_LOCAL_NO_MODEL)
    ));
    assert_eq!(writers.asked(), vec![LOCAL]);
    // No writer step at all: no writer.
    set_summary_order(&mut svc.store(), &order(&[OrderStep::Computer(DESK)]), None).unwrap();
    let none = Writers::default();
    assert!(matches!(
        helper_writer(&svc, &none, crate::keep_local::SummaryPrivacy::Writer),
        HelperWriter::Decline(reason::NO_WRITER)
    ));
    assert!(none.asked().is_empty());
}

#[test]
fn summary_order_run_step_lines_hold_fixed_words_only() {
    let line = |step, result| RunStepView { step, result }.line();
    assert_eq!(
        line(LOCAL, StepResult::Wrote),
        "the local model: wrote the summary"
    );
    assert_eq!(
        line(CLAUDE, StepResult::Moved(OutcomeWord::NotSignedIn)),
        "Claude Code: not signed in"
    );
    assert_eq!(
        line(OrderStep::Computer(DESK), StepResult::Asked),
        "computer 07070707-0707-0707-0707-070707070707: asked, waiting"
    );
    assert_eq!(line(CODEX, StepResult::NotTried), "Codex: not tried");
    assert_eq!(
        line(ENDPOINT, StepResult::Moved(OutcomeWord::NoApiKey)),
        "your endpoint: no API key"
    );
}

fn ask(device: DeviceId) -> OffloadChoice {
    OffloadChoice::Ask {
        device,
        name: "Invented desk".into(),
        writer: WriterKind::Local,
    }
}

#[test]
fn summary_order_run_an_answer_for_another_device_is_never_asked() {
    let dir = TempDir::new("order-run-other-device");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 20);
    let elsewhere = DeviceId::from_bytes([0x66; 16]);
    // The host answers "ask" for a device that is not this step's.
    let writers = Writers::default()
        .computer(DESK, ask(elsewhere))
        .with(WriterStep::Local, Fake::Skip(not_installed()));
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(
        svc.store().summary_requests().unwrap().is_empty(),
        "no request anywhere"
    );
    assert_eq!(
        results(&out),
        vec![
            StepResult::Moved(OutcomeWord::Failed),
            StepResult::Moved(OutcomeWord::NotInstalled)
        ]
    );
}

#[test]
fn summary_order_run_a_local_step_skipped_not_now_says_so() {
    let dir = TempDir::new("order-run-not-now-only");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 21);
    let writers = Writers::default();
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[LOCAL])),
        &RunPlan {
            writers: &writers,
            local_not_now: &|| true,
            order: None,
        },
        &CancelToken::new(),
    );
    assert!(
        matches!(
            out.summary,
            SummaryResult::Skipped {
                reason: SkipReason::NotNow
            }
        ),
        "{:?}",
        out.summary
    );
    assert!(writers.asked().is_empty());
    assert_eq!(skip_word(&SkipReason::NotNow), Some(OutcomeWord::NotNow));
}

#[test]
fn summary_order_run_a_failed_ask_stops_except_when_the_request_was_not_written() {
    let dir = TempDir::new("order-run-ask-errors");
    let (svc, _) = services(&dir);
    // Which errors stop.
    assert!(stops_ask(&RuntimeError::Store(
        earshot_store::StoreError::Busy("x".into())
    )));
    assert!(stops_ask(&RuntimeError::Conflict(
        "the summary run was replaced"
    )));
    assert!(stops_ask(&RuntimeError::Edit(
        crate::edit::EditError::MeetingNotFound(EntityId::from_bytes([1; 16]))
    )));
    assert!(!stops_ask(&RuntimeError::Internal("not written")));

    // A store error: the run stops and the next step is never built.
    let m = meeting(&svc, "Planning", 22);
    let writers = Writers::default().computer(DESK, ask(DESK));
    fault::fail_ask_store();
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(matches!(
        out.summary,
        SummaryResult::Failed {
            error: RuntimeError::Store(_),
            ..
        }
    ));
    assert_eq!(writers.asked(), vec![OrderStep::Computer(DESK)]);
    assert!(svc.store().summary_requests().unwrap().is_empty());

    // The run replaced meanwhile (another run started): it stops, and the newer record stays.
    let m = meeting(&svc, "Planning", 23);
    let newer = SummaryRun {
        run: 7,
        ..SummaryRun::new(RunOrigin::Named)
    };
    let newer_bytes = encode(&newer);
    let writers = Writers::default()
        .computer(DESK, ask(DESK))
        .meanwhile(move |svc| {
            crate::local::set(&mut svc.store(), &record_key(m), &newer_bytes).unwrap();
        });
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(
        matches!(
            out.summary,
            SummaryResult::Failed {
                error: RuntimeError::Conflict(_),
                ..
            }
        ),
        "{:?}",
        out.summary
    );
    assert_eq!(writers.asked(), vec![OrderStep::Computer(DESK)]);
    assert_eq!(record(&svc, m).run, 7);

    // The meeting deleted meanwhile: it stops.
    let m = meeting(&svc, "Planning", 24);
    let writers = Writers::default()
        .computer(DESK, ask(DESK))
        .meanwhile(move |svc| {
            svc.delete_meeting(m).unwrap();
        });
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
        &plan(&writers),
        &CancelToken::new(),
    );
    assert!(
        matches!(out.summary, SummaryResult::Failed { .. }),
        "{:?}",
        out.summary
    );
    assert_eq!(writers.asked(), vec![OrderStep::Computer(DESK)]);
}

// ---- T14.66: Stop's record, and each move recorded as it happens ----

#[test]
fn summary_order_run_stopped_record_bytes_are_pinned() {
    let request = EntityId::from_bytes([9; 16]);
    let run = SummaryRun {
        run: 43,
        origin: RunOrigin::Named,
        steps: vec![OrderStep::Computer(DESK)],
        cursor: 1,
        outcomes: vec![StepOutcome {
            step: 0,
            word: OutcomeWord::Stopped,
        }],
        waiting_on: None,
        state: RunState::Ended,
        writer: None,
        stopped: Some(request),
    };
    let mut want = 1u32.to_le_bytes().to_vec();
    want.extend_from_slice(
        br#"{"run":43,"origin":"named","steps":[{"computer":"07070707-0707-0707-0707-070707070707"}],"cursor":1,"outcomes":[{"step":0,"word":"stopped"}],"waiting_on":null,"state":"ended","writer":null,"stopped":"09090909-0909-0909-0909-090909090909"}"#,
    );
    assert_eq!(encode(&run), want);
    assert_eq!(decode(Some(&want)), RunRecord::Read(run));
}

#[test]
fn summary_order_run_a_record_with_an_unknown_word_is_unreadable() {
    let with = |json: &str| {
        let mut b = 1u32.to_le_bytes().to_vec();
        b.extend_from_slice(json.as_bytes());
        decode(Some(&b))
    };
    assert_eq!(
        with(
            r#"{"run":1,"origin":"order","steps":[{"writer":"local"}],"cursor":1,"outcomes":[{"step":0,"word":"a-word-from-later"}],"state":"ended"}"#
        ),
        RunRecord::Unreadable
    );
    // Control: a word this build knows reads.
    assert!(matches!(
        with(
            r#"{"run":1,"origin":"order","steps":[{"writer":"local"}],"cursor":1,"outcomes":[{"step":0,"word":"too_long"}],"state":"ended"}"#
        ),
        RunRecord::Read(_)
    ));
}

/// Writers that look at the run record whenever a step is built.
struct Peek<'a> {
    inner: &'a Writers,
    seen: Mutex<Vec<SummaryRun>>,
    /// Replaces the record (a new run) while the first step is built.
    replace_first: bool,
    meeting: EntityId,
}

impl StepWriters for Peek<'_> {
    fn writer(&self, services: &Services, step: WriterStep) -> Built {
        let RunRecord::Read(run) = read(&services.store(), self.meeting).unwrap() else {
            panic!("no record");
        };
        let first = self.seen.lock().unwrap().is_empty();
        self.seen.lock().unwrap().push(run.clone());
        if first && self.replace_first {
            let mut other = run;
            other.run = other.run.wrapping_add(1);
            crate::local::set(
                &mut services.store(),
                &record_key(self.meeting),
                &encode(&other),
            )
            .unwrap();
        }
        self.inner.writer(services, step)
    }

    fn computer(&self, services: &Services, device: DeviceId) -> OffloadChoice {
        self.inner.computer(services, device)
    }
}

fn reporting(
    svc: &Services,
    m: EntityId,
    steps: &[OrderStep],
    writers: &dyn StepWriters,
) -> (RunOutcome, usize) {
    let mut changes = 0;
    let out = run_summary_order_reporting(
        svc,
        m,
        RunStart::Order(order(steps)),
        &RunPlan {
            writers,
            local_not_now: &|| false,
            order: None,
        },
        &CancelToken::new(),
        &mut || changes += 1,
    );
    (out, changes)
}

#[test]
fn summary_order_run_each_move_is_recorded_before_the_next_step() {
    let dir = TempDir::new("order-run-move-recorded");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 21);
    let bad = failing();
    let good = MockServer::start(vec![reply("Written by the third step")]).unwrap();
    let inner = Writers::default()
        .with(WriterStep::Local, Fake::Skip(not_installed()))
        .mock(WriterStep::Endpoint, &bad)
        .mock(WriterStep::ClaudeCode, &good);
    let peek = Peek {
        inner: &inner,
        seen: Mutex::new(Vec::new()),
        replace_first: false,
        meeting: m,
    };
    let (out, changes) = reporting(&svc, m, &[LOCAL, ENDPOINT, CLAUDE], &peek);
    assert!(matches!(out.summary, SummaryResult::Stored { .. }));
    assert_eq!(changes, 2, "one change per step left with a later step");
    let seen = peek.seen.lock().unwrap().clone();
    // When the endpoint is built the local step's move is recorded, cursor advanced; when Claude
    // is, the endpoint's too.
    assert_eq!(seen[1].outcome_of(0), Some(OutcomeWord::NotInstalled));
    assert_eq!((seen[1].cursor, seen[1].state), (1, RunState::Running));
    assert_eq!(seen[2].outcome_of(1), Some(OutcomeWord::Failed));
    assert_eq!(seen[2].cursor, 2);
    // The outcome is the plain run's on the same writers.
    let m2 = meeting(&svc, "Planning again", 22);
    let bad2 = failing();
    let good2 = MockServer::start(vec![reply("Written by the third step")]).unwrap();
    let plain = Writers::default()
        .with(WriterStep::Local, Fake::Skip(not_installed()))
        .mock(WriterStep::Endpoint, &bad2)
        .mock(WriterStep::ClaudeCode, &good2);
    let p = run_summary_order(
        &svc,
        m2,
        RunStart::Order(order(&[LOCAL, ENDPOINT, CLAUDE])),
        &plan(&plain),
        &CancelToken::new(),
    );
    assert_eq!(results(&p), results(&out));
}

#[test]
fn summary_order_run_no_change_for_dropped_steps_or_a_failing_last_step() {
    let dir = TempDir::new("order-run-no-change");
    let (svc, _) = services(&dir);
    // A kept-local meeting: Claude is dropped before the run, the local step fails last.
    let m = meeting(&svc, "Planning", 23);
    kept_local(&svc, m);
    let writers = Writers::default().with(WriterStep::Local, Fake::Skip(not_installed()));
    let (out, changes) = reporting(&svc, m, &[CLAUDE, LOCAL], &writers);
    assert!(!matches!(out.summary, SummaryResult::Stored { .. }));
    assert_eq!(changes, 0);
}

#[test]
fn summary_order_run_a_replaced_record_stops_the_run_and_a_failed_write_does_not() {
    let dir = TempDir::new("order-run-replaced");
    let (svc, _) = services(&dir);
    // The record is replaced (a new run) while the first step runs: the move write finds
    // another run's record and the run stops.
    let m = meeting(&svc, "Planning", 24);
    let inner = Writers::default().with(WriterStep::Local, Fake::Skip(not_installed()));
    let peek = Peek {
        inner: &inner,
        seen: Mutex::new(Vec::new()),
        replace_first: true,
        meeting: m,
    };
    let (out, changes) = reporting(&svc, m, &[LOCAL, ENDPOINT], &peek);
    assert!(
        matches!(
            &out.summary,
            SummaryResult::Failed {
                error: RuntimeError::Conflict(RUN_REPLACED),
                ..
            }
        ),
        "{:?}",
        out.summary
    );
    assert_eq!(changes, 0);
    assert_eq!(inner.asked(), vec![LOCAL], "the endpoint never ran");
    // A failed move write only loses the account: the run goes on.
    let m2 = meeting(&svc, "Planning again", 25);
    let good = MockServer::start(vec![reply("Written by the second step")]).unwrap();
    let writers = Writers::default()
        .with(WriterStep::Local, Fake::Skip(not_installed()))
        .mock(WriterStep::Endpoint, &good);
    fault::fail_move();
    let (out, changes) = reporting(&svc, m2, &[LOCAL, ENDPOINT], &writers);
    assert!(matches!(out.summary, SummaryResult::Stored { .. }));
    assert_eq!(changes, 0);
}

#[test]
fn summary_order_run_local_failures_move_on_with_their_kind() {
    use crate::summary::{LocalFailureKind as K, LocalSummaryError as L};
    use earshot_llm_local::LlmError as E;
    use earshot_llm_local::summarize::SummaryFailure as F;
    let local = |e: L| failure_word(WriterStep::Local, &RuntimeError::LocalSummary(e));
    assert_eq!(
        local(L::Run(F::TooLong {
            prompt: 9,
            available: 1
        })),
        Some(OutcomeWord::TooLong)
    );
    assert_eq!(
        local(L::Load(E::Load("x".into()))),
        Some(OutcomeWord::LoadFailed)
    );
    assert_eq!(local(L::Load(E::LoadStuck)), Some(OutcomeWord::LoadStuck));
    assert_eq!(
        local(L::Load(E::Backend("x".into()))),
        Some(OutcomeWord::WrongBackend)
    );
    assert_eq!(local(L::GpuStalled), Some(OutcomeWord::Stalled));
    assert_eq!(local(L::GpuChildStalled), Some(OutcomeWord::CardStopped));
    assert_eq!(
        local(L::CudaFilesChanged { why: "x".into() }),
        Some(OutcomeWord::CudaChanged)
    );
    assert_eq!(
        local(L::ChildRun {
            message: "x".into(),
            kind: Some(K::ReplyUnusable)
        }),
        Some(OutcomeWord::ReplyUnusable)
    );
    assert_eq!(
        local(L::ChildRun {
            message: "x".into(),
            kind: None
        }),
        Some(OutcomeWord::Failed),
        "Other keeps failed"
    );
    assert_eq!(local(L::Run(F::Cancelled)), None, "a cancel stops");
    // Control: endpoint and agent-CLI failures keep `failed`.
    let http = || {
        RuntimeError::Llm(earshot_llm::LlmError::Http {
            status: 500,
            message: String::new(),
        })
    };
    assert_eq!(
        failure_word(WriterStep::Endpoint, &http()),
        Some(OutcomeWord::Failed)
    );
}

#[test]
fn summary_order_local_failure_kind_of_every_variant() {
    use crate::summary::{LocalFailureKind as K, LocalSummaryError as L};
    use earshot_llm_local::LlmError as E;
    use earshot_llm_local::summarize::SummaryFailure as F;
    // Every variant, nested ones included, each built with two different texts: the kind never
    // depends on the text.
    let cases = |w: &str| -> Vec<(L, K)> {
        let llm = |e: E| {
            vec![
                (L::Load(e.clone()), load_kind(&e)),
                (L::Run(F::Model(e.clone())), run_kind(&e)),
            ]
        };
        let mut v = vec![
            (
                L::Run(F::TooLong {
                    prompt: w.len(),
                    available: 1,
                }),
                K::TooLong,
            ),
            (
                L::Run(F::Plan(
                    earshot_core::prompt::PromptError::ContextTooSmall {
                        needed: w.len(),
                        available: 1,
                    },
                )),
                K::TooLong,
            ),
            (
                L::Run(F::Invalid {
                    step: "single",
                    attempts: 3,
                    errors: vec![],
                    last_reply_tail: w.into(),
                }),
                K::ReplyUnusable,
            ),
            (L::Run(F::Cancelled), K::Cancelled),
            (
                L::Run(F::Grammar(earshot_llm_local::grammar::GrammarError(
                    w.into(),
                ))),
                K::Other,
            ),
            (
                L::Chunks(earshot_core::prompt::TimeChunkError::ZeroWindow),
                K::Other,
            ),
            (L::GpuChildDied { why: w.into() }, K::CardStopped),
            (L::GpuChildStalled, K::CardStopped),
            (L::GpuStalled, K::Stalled),
            (L::LoadStalled, K::Stalled),
            (L::CudaFilesChanged { why: w.into() }, K::CudaChanged),
            (
                L::LoadRetried {
                    gpu: Box::new(L::GpuStalled),
                    cpu: Box::new(L::Load(E::LoadStuck)),
                },
                K::LoadStuck,
            ),
            (
                L::LoadRetried {
                    gpu: Box::new(L::Load(E::Backend(w.into()))),
                    cpu: Box::new(L::Load(E::Load(w.into()))),
                },
                K::LoadFailed,
            ),
            (
                L::ChildRun {
                    message: w.into(),
                    kind: None,
                },
                K::Other,
            ),
            (
                L::ChildLoad {
                    message: w.into(),
                    kind: None,
                },
                K::Other,
            ),
        ];
        for k in K::ALL {
            v.push((
                L::ChildRun {
                    message: w.into(),
                    kind: Some(k),
                },
                k,
            ));
            v.push((
                L::ChildLoad {
                    message: w.into(),
                    kind: Some(k),
                },
                k,
            ));
        }
        for e in [
            E::Load(w.into()),
            E::Input(w.into()),
            E::ContextTooSmall {
                prompt: w.len(),
                max_tokens: 1,
                context: 1,
            },
            E::Decode(7),
            E::Utf8,
            E::Backend(w.into()),
            E::LoadStuck,
            E::Cancelled,
        ] {
            v.extend(llm(e));
        }
        v
    };
    fn load_kind(e: &earshot_llm_local::LlmError) -> crate::summary::LocalFailureKind {
        use crate::summary::LocalFailureKind as K;
        use earshot_llm_local::LlmError as E;
        match e {
            E::Load(_) => K::LoadFailed,
            E::LoadStuck => K::LoadStuck,
            E::Backend(_) => K::WrongBackend,
            E::ContextTooSmall { .. } => K::TooLong,
            E::Cancelled => K::Cancelled,
            E::Input(_) | E::Decode(_) | E::Utf8 => K::Other,
        }
    }
    fn run_kind(e: &earshot_llm_local::LlmError) -> crate::summary::LocalFailureKind {
        use crate::summary::LocalFailureKind as K;
        use earshot_llm_local::LlmError as E;
        match e {
            E::ContextTooSmall { .. } => K::TooLong,
            E::Cancelled => K::Cancelled,
            _ => K::Other,
        }
    }
    for (a, b) in cases("first words")
        .into_iter()
        .zip(cases("other words, longer"))
    {
        assert_eq!(a.0.kind(), a.1, "{:?}", a.0);
        assert_eq!(b.0.kind(), a.0.kind(), "{:?}", b.0);
    }
    // Control: a cancel is the cancel kind, which keeps the cancelled copy.
    assert_eq!(L::Run(F::Cancelled).kind(), K::Cancelled);
    // A child's words: every kind but `cancelled` reads back; anything else is none.
    for k in K::ALL {
        let back = K::from_child(k.word());
        assert_eq!(back, (k != K::Cancelled).then_some(k));
    }
    assert_eq!(K::from_child("a-word-from-later"), None);
}
