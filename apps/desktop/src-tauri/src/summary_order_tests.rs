//! T12.63b: the order of summary writers in the app: its copy against the shared table, the
//! Summaries page's order, the banners, the re-summarise menu, the meeting detail's account and
//! the meeting's run read when its summary is due. Every name holds `summary`
//! (`cargo test -p earshot-desktop summary`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use earshot_core::asr::{CancelToken, MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, SegmentRow, Stamp, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_runtime::host::{SkipReason, SummaryResult};
use earshot_runtime::summary_order::run::{RunStepView, RunView, StepResult, SummaryRun};
use earshot_runtime::summary_settings::EndpointProvider;
use earshot_runtime::sync::{MemoryVault, ModelPlace, SyncVault};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{T0, TempDir, services};

const DESK: DeviceId = DeviceId::from_bytes([0x5d; 16]);
const LOCAL: OrderStep = OrderStep::LOCAL;
const ENDPOINT: OrderStep = OrderStep::Writer(WriterStep::Endpoint);
const CLAUDE: OrderStep = OrderStep::Writer(WriterStep::ClaudeCode);
const CODEX: OrderStep = OrderStep::Writer(WriterStep::Codex);

fn table() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../fixtures/ui/summary_order_cases.json"
    ))
    .unwrap()
}

fn cx() -> StepContext {
    StepContext {
        provider: EndpointProvider::OpenAiCompatible,
        host: Some("models.example.test".into()),
        names: [(
            DESK,
            Named {
                name: Some("Invented desk".into()),
                writer: Some(WriterKind::Local),
                place: None,
            },
        )]
        .into_iter()
        .collect(),
    }
}

fn view(step: OrderStep, result: StepResult) -> RunStepView {
    RunStepView { step, result }
}

fn moved(step: OrderStep, word: OutcomeWord) -> RunStepView {
    view(step, StepResult::Moved(word))
}

#[test]
fn summary_order_copy_matches_the_shared_table() {
    let t = table();
    // Every outcome word has its text, and nothing else is listed.
    let words = t["words"].as_object().unwrap();
    assert_eq!(words.len(), OutcomeWord::ALL.len());
    for w in OutcomeWord::ALL {
        assert_eq!(words[&word_key(w)].as_str(), Some(w.text()), "{w:?}");
        assert_eq!(word(w), w.text());
    }
    assert_eq!(COPY.not_now, t["not_now"].as_str().unwrap());
    // The skipped copy of a "not now" step is the table's (replacing T12.63a's stand-in).
    assert_eq!(
        crate::meeting_events::skip_copy(&SkipReason::NotNow),
        COPY.not_now
    );
    // A refused order reads as the table says.
    let refusals = &COPY.refusals;
    for (word, e) in [
        ("too_many_steps", SummaryOrderError::TooManySteps),
        ("twice", SummaryOrderError::Twice),
        ("this_device", SummaryOrderError::ThisDevice),
    ] {
        assert_eq!(order_error(e).message, refusals[word], "{word}");
    }
    assert_eq!(refusals.len(), 3);
    // The Summaries page's list (T12.68): each control names its writer, a refused fourth tick
    // names the most steps, which is the order's own limit.
    let page = &COPY.page;
    for text in [&page.r#use, &page.move_up, &page.move_down] {
        assert!(text.contains("{writer}"), "{text}");
    }
    assert!(page.step.contains("{n}") && page.too_many.contains("{n}"));
    assert_eq!(
        page.too_many.replace(
            "{n}",
            &earshot_runtime::summary_order::MAX_STEPS.to_string()
        ),
        "At most 3 writers can be ticked. Untick one first."
    );
    assert!(
        !page.kept_local.is_empty()
            && !page.not_set_up.is_empty()
            && !page.not_downloaded.is_empty()
            && !page.list.is_empty()
    );
    // Public copy only: no em dash anywhere in the table.
    let text = include_str!("../../../../fixtures/ui/summary_order_cases.json");
    assert!(!text.contains('\u{2014}'));
}

#[test]
fn summary_order_steps_are_labelled_with_where_the_transcript_goes() {
    let cx = cx();
    let cases = [
        (LOCAL, "On this computer", "Stays on this computer"),
        (
            ENDPOINT,
            "Model server (models.example.test)",
            "Goes to models.example.test",
        ),
        (
            CLAUDE,
            "Claude (through Claude Code)",
            "Goes to Anthropic through Claude Code",
        ),
        (
            CODEX,
            "OpenAI (through Codex)",
            "Goes to OpenAI through Codex",
        ),
        (
            OrderStep::Computer(DESK),
            "On Invented desk (local model)",
            "Goes to Invented desk, which writes it with its local model",
        ),
    ];
    for (step, label, dest) in cases {
        assert_eq!(step_label(step, &cx), label);
        assert_eq!(destination(step, &cx), dest);
    }
    let anthropic = StepContext {
        provider: EndpointProvider::Anthropic,
        ..cx.clone()
    };
    assert_eq!(step_label(ENDPOINT, &anthropic), "Anthropic API");
    assert_eq!(destination(ENDPOINT, &anthropic), "Goes to Anthropic");
    let unset = StepContext {
        host: None,
        ..cx.clone()
    };
    assert_eq!(step_label(ENDPOINT, &unset), "Your own server");
    // A computer this process has not heard of, and one whose writer kind is not known.
    let stranger = OrderStep::Computer(DeviceId::from_bytes([1; 16]));
    assert_eq!(step_label(stranger, &cx), "On Your other computer");
    assert_eq!(destination(stranger, &cx), "Goes to Your other computer");
    assert_eq!(writer_name(stranger, &cx), "Your other computer");
    // A name holding `$&` stays as written.
    let odd = StepContext {
        names: [(
            DESK,
            Named {
                name: Some("Desk $& co".into()),
                writer: None,
                place: None,
            },
        )]
        .into_iter()
        .collect(),
        ..cx.clone()
    };
    assert_eq!(step_label(OrderStep::Computer(DESK), &odd), "On Desk $& co");
    // T11.6a: a summary worker's own local model is labelled with where it runs, in the label
    // and the line under it; a place never labels another writer kind.
    let rack = |writer, place| StepContext {
        names: [(
            DESK,
            Named {
                name: Some("Invented rack".into()),
                writer: Some(writer),
                place: Some(place),
            },
        )]
        .into_iter()
        .collect(),
        ..cx.clone()
    };
    let on_card = rack(WriterKind::Local, ModelPlace::GraphicsCard);
    assert_eq!(
        step_label(OrderStep::Computer(DESK), &on_card),
        "On Invented rack (local model, graphics card)"
    );
    assert_eq!(
        destination(OrderStep::Computer(DESK), &on_card),
        "Goes to Invented rack, which writes it with its local model, graphics card"
    );
    assert_eq!(
        step_label(
            OrderStep::Computer(DESK),
            &rack(WriterKind::Local, ModelPlace::Processor)
        ),
        "On Invented rack (local model, processor)"
    );
    assert_eq!(
        step_label(
            OrderStep::Computer(DESK),
            &rack(WriterKind::OpenaiCompatible, ModelPlace::Processor)
        ),
        "On Invented rack (own server)"
    );
}

#[test]
fn summary_order_steps_banner_says_what_became_of_each_step() {
    let cx = cx();
    // One step: today's banners say it.
    assert_eq!(
        steps_text(&[moved(LOCAL, OutcomeWord::NotInstalled)], &cx),
        None
    );
    // Step 1 wrote it: nothing new.
    assert_eq!(
        steps_text(
            &[
                view(LOCAL, StepResult::Wrote),
                view(CLAUDE, StepResult::NotTried)
            ],
            &cx
        ),
        None
    );
    // A later step wrote it.
    assert_eq!(
        steps_text(
            &[
                moved(OrderStep::Computer(DESK), OutcomeWord::Offline),
                moved(LOCAL, OutcomeWord::NotInstalled),
                view(CODEX, StepResult::Wrote),
            ],
            &cx
        )
        .as_deref(),
        Some("Codex wrote the summary. Invented desk: offline; This computer: not installed.")
    );
    // Every step failed.
    assert_eq!(
        steps_text(
            &[
                moved(CLAUDE, OutcomeWord::NotSignedIn),
                moved(ENDPOINT, OutcomeWord::NoApiKey),
            ],
            &cx
        )
        .as_deref(),
        Some(
            "No summary. Claude Code: not signed in; The model server at models.example.test: \
             no API key."
        )
    );
    // A run that stopped (cancelled, a conflict) or still waits says nothing here.
    assert_eq!(
        steps_text(
            &[
                moved(CLAUDE, OutcomeWord::NotSignedIn),
                view(LOCAL, StepResult::NotTried),
            ],
            &cx
        ),
        None
    );
    assert_eq!(
        steps_text(
            &[
                view(OrderStep::Computer(DESK), StepResult::Asked),
                view(LOCAL, StepResult::NotTried),
            ],
            &cx
        ),
        None
    );
    // Steps dropped for a meeting kept on this computer do not count: a one-step order.
    let kept = [
        moved(CLAUDE, OutcomeWord::SkippedKeptLocal),
        moved(LOCAL, OutcomeWord::NotInstalled),
    ];
    assert_eq!(tried_steps(&kept), 1);
    assert_eq!(steps_text(&kept, &cx), None);
    let banner = steps_banner(
        &[
            moved(CLAUDE, OutcomeWord::NotSignedIn),
            view(LOCAL, StepResult::Wrote),
        ],
        &cx,
    )
    .unwrap();
    let MeetingEvent::Warning { key, .. } = banner else {
        panic!("{banner:?}");
    };
    assert_eq!(key, STEPS_BANNER_KEY);
}

#[test]
fn summary_order_start_banners_name_the_next_step() {
    let cx = cx();
    let steps = [CLAUDE, CODEX, LOCAL];
    let banners = start_banners(
        &steps,
        &[
            (0, OutcomeWord::NotSignedIn),
            (1, OutcomeWord::NotInstalled),
        ],
        &cx,
    );
    let texts: Vec<(String, String)> = banners
        .into_iter()
        .map(|b| match b {
            MeetingEvent::Warning { key, message, .. } => (key, message),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            (
                "summary-start-0".to_owned(),
                "Claude Code can't write summaries now (not signed in); This computer will."
                    .to_owned()
            ),
            (
                "summary-start-1".to_owned(),
                "Codex can't write summaries now (not installed); This computer will.".to_owned()
            ),
        ]
    );
    // The last step has no next one.
    let last = start_banners(&[CLAUDE, LOCAL], &[(1, OutcomeWord::NotInstalled)], &cx);
    assert!(matches!(
        &last[0],
        MeetingEvent::Warning { message, .. }
            if message == "This computer can't write summaries now (not installed)."
    ));
    // A one-step order keeps today's banners.
    assert!(start_banners(&[CLAUDE], &[(0, OutcomeWord::NotSignedIn)], &cx).is_empty());
}

#[test]
fn summary_order_detail_note_reads_the_last_run() {
    let cx = cx();
    let run = |origin, steps: Vec<OrderStep>, state| SummaryRun {
        run: 1,
        origin,
        steps,
        cursor: 0,
        outcomes: Vec::new(),
        waiting_on: None,
        state,
        writer: None,
    };
    use earshot_runtime::summary_order::run::{RunOrigin, RunState, StepOutcome};
    // An unreadable record, and a run not ended with nothing working on the meeting.
    assert_eq!(
        run_note(&RunView::Unreadable, false, &cx).as_deref(),
        Some(COPY.interrupted.as_str())
    );
    let unfinished = RunView::Unfinished(run(RunOrigin::Order, vec![LOCAL], RunState::Running));
    assert_eq!(
        run_note(&unfinished, false, &cx).as_deref(),
        Some(COPY.interrupted.as_str())
    );
    // While the meeting's work or a job holds it, it is not interrupted.
    assert_eq!(run_note(&unfinished, true, &cx), None);
    // A named computer that handed it back.
    let mut named = run(
        RunOrigin::Named,
        vec![OrderStep::Computer(DESK)],
        RunState::Ended,
    );
    named.outcomes.push(StepOutcome {
        step: 0,
        word: OutcomeWord::NoReply,
    });
    assert_eq!(
        run_note(&RunView::Ended(named), false, &cx).as_deref(),
        Some("Invented desk could not write the summary: no reply.")
    );
    // An ended order run: its steps.
    let mut later = run(RunOrigin::Order, vec![CLAUDE, LOCAL], RunState::Ended);
    later.outcomes.push(StepOutcome {
        step: 0,
        word: OutcomeWord::NotSignedIn,
    });
    later.writer = Some(1);
    assert_eq!(
        run_note(&RunView::Ended(later), false, &cx).as_deref(),
        Some("This computer wrote the summary. Claude Code: not signed in.")
    );
    assert_eq!(run_note(&RunView::None, false, &cx), None);
}

fn host() -> SyncHost {
    SyncHost::new(Arc::new(MemoryVault::default()) as Arc<dyn SyncVault>)
}

fn endpoint_value() -> SummarySettingsValue {
    let mut v = crate::summaries::settings_view(&services(&TempDir::new("summary-order-value")))
        .unwrap()
        .value;
    v.url = "http://127.0.0.1:8080/v1".into();
    v.model = "invented-model".into();
    v
}

#[test]
fn summary_order_page_stores_and_refuses_orders() {
    let dir = TempDir::new("summary-order-page");
    let svc = services(&dir);
    let host = host();
    // A fresh install: the local model alone.
    let v = order_view(&svc, &host).unwrap();
    assert!(!v.none);
    assert_eq!(
        v.steps.iter().map(|s| &s.step).collect::<Vec<_>>(),
        [&StepChoice::Local]
    );
    assert_eq!(v.max_steps, 3);
    assert_eq!(v.steps[0].destination, "Stays on this computer");
    // The four writers are always offered.
    for c in [
        StepChoice::Local,
        StepChoice::Endpoint,
        StepChoice::ClaudeCode,
        StepChoice::Codex,
    ] {
        assert!(v.offered.iter().any(|s| s.step == c), "{c:?}");
    }
    // Three steps with a computer.
    let desk = StepChoice::Computer {
        device: DESK.to_string(),
    };
    let v = set_order(
        &svc,
        &host,
        &[StepChoice::ClaudeCode, desk.clone(), StepChoice::Local],
        None,
    )
    .unwrap();
    assert_eq!(v.steps.len(), 3);
    assert!(
        v.offered.iter().any(|s| s.step == desk),
        "a chosen computer stays offered"
    );
    assert_eq!(
        earshot_runtime::summary_order::summary_order(&svc.store())
            .unwrap()
            .0
            .steps(),
        [CLAUDE, OrderStep::Computer(DESK), LOCAL]
    );
    // Refused, nothing stored: a writer twice, four steps, this computer, an endpoint without its
    // fields, an id that is not one.
    let this = StepChoice::Computer {
        device: svc.store().device().to_string(),
    };
    for (steps, settings) in [
        (vec![StepChoice::Local, StepChoice::Local], None),
        (
            vec![
                StepChoice::Local,
                StepChoice::ClaudeCode,
                StepChoice::Codex,
                desk.clone(),
            ],
            None,
        ),
        (vec![this], None),
        (vec![StepChoice::Endpoint], None),
        (
            vec![StepChoice::Computer {
                device: "not-an-id".into(),
            }],
            None,
        ),
    ] {
        let e = set_order(&svc, &host, &steps, settings).unwrap_err();
        assert_eq!(e.kind, "invalid_setting", "{steps:?}: {}", e.message);
    }
    assert_eq!(
        earshot_runtime::summary_order::summary_order(&svc.store())
            .unwrap()
            .0
            .steps(),
        [CLAUDE, OrderStep::Computer(DESK), LOCAL]
    );
    // An endpoint as step 2 with its fields, stored with the order in one go.
    let v = set_order(
        &svc,
        &host,
        &[StepChoice::Local, StepChoice::Endpoint],
        Some(&endpoint_value()),
    )
    .unwrap();
    assert_eq!(v.steps[1].label, "Model server (127.0.0.1)");
    let (settings, _) = earshot_runtime::summary_settings::summary_settings(&svc.store()).unwrap();
    assert_eq!(settings.url, "http://127.0.0.1:8080/v1");
    assert_eq!(
        settings.writer,
        earshot_runtime::summary_settings::SummaryWriter::OnThisComputer,
        "the writer key mirrors step 1"
    );
    // No summary.
    let v = set_order(&svc, &host, &[], None).unwrap();
    assert!(v.none);
    assert!(v.steps.is_empty());
}

#[test]
fn summary_order_live_switch_follows_the_order() {
    let dir = TempDir::new("summary-order-live");
    let svc = services(&dir);
    let host = host();
    let desk = StepChoice::Computer {
        device: DESK.to_string(),
    };
    let live = |svc: &Services| {
        crate::summaries::settings_view(svc)
            .unwrap()
            .live_unavailable
            .is_none()
    };
    assert!(live(&svc), "the local model alone");
    set_order(&svc, &host, &[desk.clone(), StepChoice::Local], None).unwrap();
    assert!(live(&svc), "a computer, then the local model");
    set_order(
        &svc,
        &host,
        &[StepChoice::ClaudeCode, StepChoice::Local],
        None,
    )
    .unwrap();
    assert!(!live(&svc), "Claude Code first");
    set_order(&svc, &host, &[desk, StepChoice::ClaudeCode], None).unwrap();
    assert!(!live(&svc));
}

/// Fixed answers for the re-summarise menu.
struct Here {
    local: bool,
    key: bool,
    claude: bool,
    codex: bool,
}

impl WritersHere for Here {
    fn local_installed(&self, _: &Services) -> bool {
        self.local
    }
    fn key_saved(&self) -> bool {
        self.key
    }
    fn claude_installed(&self) -> bool {
        self.claude
    }
    fn codex_installed(&self) -> bool {
        self.codex
    }
}

/// A meeting with `n` final segments.
fn meeting_with_final(svc: &Services, seed: u64, n: usize) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Invented review", T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let track = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let rows: Vec<SegmentRow> = (0..n)
        .map(|i| {
            let start = 1_000 * i as u64;
            let seg = Segment::from_words(
                EntityId::generate(stamp.clock, &mut *stamp.rng),
                track.id,
                vec![Word::new("invented", start, start + 300, 0.9)],
                MeetingTime::ZERO,
            );
            SegmentRow::new(m.id, TranscriptPass::Final, seg, stamp.hlc, stamp.clock)
        })
        .collect();
    let id = m.id;
    store.put_meeting(m).unwrap();
    store.put_track(track).unwrap();
    for r in rows {
        store.put_segment(r).unwrap();
    }
    id
}

#[test]
fn summary_order_resummarise_menu_offers_what_can_run_here() {
    let dir = TempDir::new("summary-order-menu");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 3, 2);
    let all = Here {
        local: true,
        key: true,
        claude: true,
        codex: true,
    };
    let steps = |v: &ResummariseView| v.writers.iter().map(|w| w.step.clone()).collect::<Vec<_>>();
    // No endpoint set: it is not offered.
    let v = resummarise_view(&svc, m, &all).unwrap();
    assert_eq!(
        steps(&v),
        [StepChoice::Local, StepChoice::ClaudeCode, StepChoice::Codex]
    );
    assert!(v.writers.iter().all(|w| w.disabled.is_none()));
    // An Anthropic endpoint needs a saved key.
    crate::summaries::set_settings(
        &svc,
        &SummarySettingsValue {
            provider: crate::summaries::ProviderChoice::Anthropic,
            url: "https://api.anthropic.com".into(),
            ..endpoint_value()
        },
    )
    .unwrap();
    assert!(steps(&resummarise_view(&svc, m, &all).unwrap()).contains(&StepChoice::Endpoint));
    let no_key = Here { key: false, ..all };
    assert!(!steps(&resummarise_view(&svc, m, &no_key).unwrap()).contains(&StepChoice::Endpoint));
    // T14.31: a key saved for another address is not offered, as the Summaries page says and
    // the run refuses; saved for this address, it is.
    let proxy = SummarySettingsValue {
        provider: crate::summaries::ProviderChoice::Anthropic,
        url: "https://proxy.example.invalid".into(),
        ..endpoint_value()
    };
    crate::summaries::set_settings(&svc, &proxy).unwrap();
    assert!(!steps(&resummarise_view(&svc, m, &all).unwrap()).contains(&StepChoice::Endpoint));
    {
        let mut db = svc.store();
        earshot_runtime::summary_settings::bind_api_key(&mut db, &proxy.settings()).unwrap();
    }
    assert!(steps(&resummarise_view(&svc, m, &all).unwrap()).contains(&StepChoice::Endpoint));
    crate::summaries::set_settings(
        &svc,
        &SummarySettingsValue {
            provider: crate::summaries::ProviderChoice::Anthropic,
            url: "https://api.anthropic.com".into(),
            ..endpoint_value()
        },
    )
    .unwrap();
    assert!(!steps(&resummarise_view(&svc, m, &all).unwrap()).contains(&StepChoice::Endpoint));
    {
        let mut db = svc.store();
        earshot_runtime::summary_settings::unbind_api_key(&mut db).unwrap();
    }
    assert!(steps(&resummarise_view(&svc, m, &all).unwrap()).contains(&StepChoice::Endpoint));
    let nothing = Here {
        local: false,
        key: false,
        claude: false,
        codex: false,
    };
    assert!(
        resummarise_view(&svc, m, &nothing)
            .unwrap()
            .writers
            .is_empty()
    );
    // A meeting kept on this computer: every remote writer is disabled with the reason.
    {
        let mut db = svc.store();
        let mut row = db.meeting(m).unwrap().unwrap();
        row.summary_kept_local = true;
        db.put_meeting(row).unwrap();
    }
    let v = resummarise_view(
        &svc,
        m,
        &Here {
            key: true,
            ..nothing_but_all()
        },
    )
    .unwrap();
    for w in &v.writers {
        let expected = (w.step != StepChoice::Local).then(|| COPY.resummarise.kept_local.clone());
        assert_eq!(w.disabled, expected, "{:?}", w.step);
    }
    assert_eq!(v.writers.len(), 4);
}

fn nothing_but_all() -> Here {
    Here {
        local: true,
        key: true,
        claude: true,
        codex: true,
    }
}

/// A key store that counts its reads (it holds no key).
#[derive(Default)]
struct CountedKeys(AtomicU32);

impl earshot_runtime::summary_settings::ApiKeyStore for CountedKeys {
    fn anthropic_key(
        &self,
    ) -> Result<
        Option<earshot_runtime::summary_settings::ApiKey>,
        earshot_runtime::summary_settings::ApiKeyError,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

/// Codex counting its calls, answering "not installed".
#[derive(Default)]
struct CountedCodex(AtomicU32);

impl CodexProbe for CountedCodex {
    fn status(&self) -> earshot_runtime::codex::CodexStatus {
        self.0.fetch_add(1, Ordering::SeqCst);
        earshot_runtime::codex::CodexStatus::NotInstalled
    }
}

/// Claude Code that must never be asked.
struct NeverClaude;

impl ClaudeCodeProbe for NeverClaude {
    fn status(&self) -> earshot_runtime::claude_code::ClaudeCodeStatus {
        panic!("Claude Code is in no order here")
    }
}

/// Codex that must never be asked.
struct NeverCodex;

impl CodexProbe for NeverCodex {
    fn status(&self) -> earshot_runtime::codex::CodexStatus {
        panic!("a meeting kept on this computer never checks Codex")
    }
}

/// A meeting kept on this computer with a multi-step order: the start's checks read no key and
/// probe no agent CLI for the steps the meeting drops (docs/summary-order.md 4.2 and 4.5).
#[test]
fn summary_order_kept_meeting_start_checks_ask_nothing_remote() {
    let dir = TempDir::new("summary-order-kept-start");
    let svc = services(&dir);
    let host = host();
    set_order(
        &svc,
        &host,
        &[StepChoice::ClaudeCode, StepChoice::Codex, StepChoice::Local],
        None,
    )
    .unwrap();
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("no-models"));
    let keys = CountedKeys::default();
    let request = crate::meeting::StartRequest {
        mode: crate::meeting_events::CaptureMode::Call,
        title: String::new(),
        keep_local: true,
    };
    let s = crate::meeting::meeting_summary(
        &svc,
        &models,
        &request,
        &[earshot_runtime::backends::Accelerator::Cpu],
        (&keys, &NeverClaude, &NeverCodex),
    )
    .unwrap();
    assert_eq!(s.steps, vec![LOCAL], "only the local step is left");
    assert_eq!(keys.0.load(Ordering::SeqCst), 0);
    assert!(s.keep_summary_local);
}

/// The summary reply of the mock endpoint.
fn summary_reply() -> earshot_llm_replay::mock::MockResponse {
    let summary = earshot_core::summary::SummaryV1 {
        title: "Invented title".into(),
        tldr: "An invented summary.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    };
    earshot_llm_replay::mock::MockResponse::completion(
        &serde_json::to_string(&summary).unwrap(),
        "stop",
    )
}

/// What a meeting with no recorded tracks hands the work after it.
fn outcome(meeting: EntityId) -> earshot_runtime::live::LiveOutcome {
    earshot_runtime::live::LiveOutcome {
        meeting,
        tracks: Vec::new(),
        end: MeetingTime::ZERO,
        reason: earshot_core::capture::EndReason::Finished,
        final_pass: earshot_core::pipeline::FinalPassRequest {
            end: MeetingTime::ZERO,
            tracks: Vec::new(),
        },
        audio_files: Vec::new(),
        unavailable_at_end: Vec::new(),
        problems: Vec::new(),
        echo_words_removed: 0,
        echo_locks: Vec::new(),
    }
}

/// The production `after` step of a prepared meeting (`Prepared::after`, made by
/// `HostPrepare::prepare`) over a meeting with no recorded tracks: its final pass has nothing to
/// process, so no model loads, and its summary step runs as in the app.
fn run_after(
    prepared: crate::meeting::Prepared,
    svc: &Services,
    meeting: EntityId,
) -> earshot_runtime::host::HostOutcome {
    (prepared.after)(svc, &outcome(meeting), &CancelToken::new(), &mut |_| {})
}

/// The real pre-flight (`HostPrepare::prepare`) with the order at the start, then the summary
/// step read when it is due: an order changed during the meeting is the one used, and the
/// writer it replaced receives nothing (docs/summary-order.md 4.1, section 7). Needs
/// `cargo xtask fetch-models --set ci` for the pre-flight's transcription model.
#[test]
fn summary_order_an_order_changed_during_a_meeting_is_the_one_used() {
    use crate::meeting::{HostPrepare, MeetingSource, Prepare, StartRequest};
    use earshot_runtime::model_settings::{ModelPick, ModelSettings, set_model_settings};

    let dir = TempDir::new("summary-order-during");
    let svc = services(&dir);
    let server = earshot_llm_replay::mock::MockServer::start(vec![summary_reply()]).unwrap();
    {
        let mut db = svc.store();
        set_model_settings(
            &mut db,
            &ModelSettings {
                transcription: ModelPick::Named("base.en".into()),
                ..ModelSettings::default()
            },
        )
        .unwrap();
    }
    let host = host();
    let value = SummarySettingsValue {
        url: server.endpoint().to_string(),
        model: "test-model".into(),
        ..endpoint_value()
    };
    set_order(&svc, &host, &[StepChoice::Endpoint], Some(&value)).unwrap();
    let keys = Arc::new(CountedKeys::default());
    let codex = Arc::new(CountedCodex::default());
    let probes = SummaryProbes {
        keys: Arc::clone(&keys) as Arc<_>,
        claude: Arc::new(NeverClaude),
        codex: Arc::clone(&codex) as Arc<_>,
    };
    let prepare = HostPrepare::with_source(Box::new(|_, _| {
        Ok(MeetingSource {
            starter: Box::new(|_, _, _, _| unreachable!("this test starts no capture")),
            clock_domain: ClockDomain::Replay,
            notices: Vec::new(),
            call_signal: None,
        })
    }))
    .with_probes(probes.clone());
    let queue = earshot_runtime::meeting_queue::MeetingQueue::new();
    let ticket = Arc::new(queue.enter());
    let request = StartRequest {
        mode: crate::meeting_events::CaptureMode::Call,
        title: String::new(),
        keep_local: false,
    };
    let preflight = || {
        prepare
            .prepare(&svc, &request, &ticket)
            .unwrap_or_else(|e| panic!("{e:?}: cargo xtask fetch-models --set ci"))
    };
    let prepared = preflight();
    // No local step at the start: nothing held here.
    assert_eq!(
        ticket.summary(),
        earshot_runtime::meeting_queue::SummaryHold::OffThisComputer
    );
    // During the meeting the user puts Codex in the endpoint's place.
    set_order(&svc, &host, &[StepChoice::Codex], None).unwrap();
    let m = meeting_with_final(&svc, 5, 2);
    let done = run_after(prepared, &svc, m);
    assert!(
        matches!(
            done.summary,
            SummaryResult::Skipped {
                reason: SkipReason::CodexUnavailable(_)
            }
        ),
        "{:?}",
        done.summary
    );
    assert!(
        server.requests().is_empty(),
        "the endpoint received nothing"
    );
    assert!(done.network.is_empty());
    assert_eq!(codex.0.load(Ordering::SeqCst), 1);
    // The negative control: with the endpoint still in the order, it writes the summary.
    set_order(&svc, &host, &[StepChoice::Endpoint], Some(&value)).unwrap();
    let prepared = preflight();
    let other = meeting_with_final(&svc, 6, 2);
    let done = run_after(prepared, &svc, other);
    assert!(
        matches!(done.summary, SummaryResult::Stored { .. }),
        "{:?}",
        done.summary
    );
    assert_eq!(server.requests().len(), 1);
}

/// The pre-flight's queue hold follows the order at the start (docs/summary-order.md 4.5): the
/// local model whenever any step is it, and nothing for an order with no local step.
#[test]
fn summary_order_preflight_holds_the_local_model_when_any_step_is_it() {
    use crate::meeting::{HostPrepare, MeetingSource, Prepare, StartRequest};
    use earshot_runtime::meeting_queue::SummaryHold;
    use earshot_runtime::model_settings::{ModelPick, ModelSettings, set_model_settings};

    let dir = TempDir::new("summary-order-hold");
    let svc = services(&dir);
    set_model_settings(
        &mut svc.store(),
        &ModelSettings {
            transcription: ModelPick::Named("base.en".into()),
            ..ModelSettings::default()
        },
    )
    .unwrap();
    let host = host();
    let prepare = HostPrepare::with_source(Box::new(|_, _| {
        Ok(MeetingSource {
            starter: Box::new(|_, _, _, _| unreachable!("this test starts no capture")),
            clock_domain: ClockDomain::Replay,
            notices: Vec::new(),
            call_signal: None,
        })
    }))
    .with_probes(SummaryProbes {
        keys: Arc::new(CountedKeys::default()),
        claude: Arc::new(earshot_runtime::claude_code::FixedClaudeCode(
            earshot_runtime::claude_code::ClaudeCodeStatus::NotInstalled,
        )),
        codex: Arc::new(CountedCodex::default()),
    });
    let request = StartRequest {
        mode: crate::meeting_events::CaptureMode::Call,
        title: String::new(),
        keep_local: false,
    };
    let desk = StepChoice::Computer {
        device: DESK.to_string(),
    };
    let queue = earshot_runtime::meeting_queue::MeetingQueue::new();
    for (steps, local) in [
        (vec![desk.clone(), StepChoice::ClaudeCode], false),
        (
            vec![desk.clone(), StepChoice::ClaudeCode, StepChoice::Local],
            true,
        ),
        (vec![StepChoice::Local], true),
    ] {
        set_order(&svc, &host, &steps, None).unwrap();
        let ticket = Arc::new(queue.enter());
        drop(
            prepare
                .prepare(&svc, &request, &ticket)
                .unwrap_or_else(|e| panic!("{e:?}: cargo xtask fetch-models --set ci")),
        );
        assert_eq!(
            matches!(ticket.summary(), SummaryHold::Local { .. }),
            local,
            "{steps:?}: {:?}",
            ticket.summary()
        );
        if !local {
            assert_eq!(ticket.summary(), SummaryHold::OffThisComputer);
        }
    }
}

/// Whether `text` sets a `RunPlan`'s `order` to something: the field name on its own (not
/// `order_run` or `summary_order`), then `: Some`.
fn sets_run_order(text: &str) -> bool {
    let needle = concat!("order", ": Some");
    text.match_indices(needle).any(|(i, _)| {
        !text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// No run of the app overrides the order of summary writers (review of T12.63b): every app
/// `RunPlan` reads the stored order (`order: None`); only earshot-cli runs the writer its command
/// line names.
#[test]
fn summary_order_no_app_run_overrides_the_order() {
    use std::path::{Path, PathBuf};
    // The rule itself, on the test's own fixture text.
    let set = concat!(
        "RunPlan { writers, local_not_now: &f, ",
        "order",
        ": Some(o) }"
    );
    assert!(sets_run_order(set));
    assert!(sets_run_order(concat!(
        "\n    ",
        "order",
        ": Some(writers.order()),"
    )));
    assert!(!sets_run_order("order_run: Some(plan),"));
    assert!(!sets_run_order("summary_order: Some(x)"));
    assert!(!sets_run_order(concat!("order", ": None,")));

    fn scan(dir: &Path, found: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                scan(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    scan(&src, &mut files);
    let setting: Vec<_> = files
        .iter()
        .filter(|(_, text)| sets_run_order(text))
        .map(|(p, _)| p.display().to_string())
        .collect();
    assert!(
        setting.is_empty(),
        "an app run overrides the order: {setting:?}"
    );
    // The scan reads the right files: the meeting's run plan is there, reading the stored order.
    assert!(
        files.iter().any(|(p, text)| p.ends_with("meeting.rs")
            && text.contains("with_order_run")
            && text.contains(concat!("order", ": None"))),
        "the scan did not read meeting.rs"
    );
    // And the one place that may, the CLI, does (the rule finds a real use).
    let cli =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../crates/earshot-cli/src/session.rs");
    assert!(sets_run_order(&std::fs::read_to_string(cli).unwrap()));
}
