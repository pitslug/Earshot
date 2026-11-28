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
        stopped: None,
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

/// The page's order of `steps` since T14.72: their online service picked first (the endpoint's
/// by the provider of `settings`), then the page's own `set_order`.
fn ordered(
    svc: &Services,
    host: &SyncHost,
    steps: &[StepChoice],
    settings: Option<&SummarySettingsValue>,
) -> SummaryOrderView {
    let order_steps: Vec<OrderStep> = steps.iter().map(|s| s.step().unwrap()).collect();
    crate::testutil::pick_for(svc, &order_steps, settings.map(|v| v.settings().provider));
    set_order(svc, host, steps, settings).unwrap()
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
    // T14.72: the local model is offered; the online writers are the one Online service row,
    // which reads "Choose a service" and cannot be ticked until a service is picked.
    assert_eq!(
        v.offered.iter().map(|s| &s.step).collect::<Vec<_>>(),
        [&StepChoice::Local]
    );
    assert_eq!(
        v.online,
        OnlineRowView {
            service: None,
            step: None,
            reason: Some(COPY.page.choose_service.clone()),
            label: None,
            destination: None,
        }
    );
    // Three steps with a computer.
    let desk = StepChoice::Computer {
        device: DESK.to_string(),
    };
    let v = ordered(
        &svc,
        &host,
        &[StepChoice::ClaudeCode, desk.clone(), StepChoice::Local],
        None,
    );
    assert_eq!(v.steps.len(), 3);
    assert!(
        v.offered.iter().any(|s| s.step == desk),
        "a chosen computer stays offered"
    );
    assert!(
        v.offered.iter().all(|s| s.step != StepChoice::ClaudeCode),
        "the online writer is the Online service row, never an offered step"
    );
    assert_eq!(v.online.service, Some(ServiceChoice::ClaudeCode));
    assert_eq!(v.online.step, Some(StepChoice::ClaudeCode));
    assert_eq!(v.online.reason, None);
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
    let v = ordered(
        &svc,
        &host,
        &[StepChoice::Local, StepChoice::Endpoint],
        Some(&endpoint_value()),
    );
    assert_eq!(v.steps[1].label, "Model server (127.0.0.1)");
    let (settings, _) = earshot_runtime::summary_settings::summary_settings(&svc.store()).unwrap();
    assert_eq!(settings.url, "http://127.0.0.1:8080/v1");
    assert_eq!(
        settings.writer,
        earshot_runtime::summary_settings::SummaryWriter::OnThisComputer,
        "the writer key mirrors step 1"
    );
    // No summary.
    let v = ordered(&svc, &host, &[], None);
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
    ordered(&svc, &host, &[desk.clone(), StepChoice::Local], None);
    assert!(live(&svc), "a computer, then the local model");
    ordered(
        &svc,
        &host,
        &[StepChoice::ClaudeCode, StepChoice::Local],
        None,
    );
    assert!(!live(&svc), "Claude Code first");
    ordered(&svc, &host, &[desk, StepChoice::ClaudeCode], None);
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
    let host = host();
    let m = meeting_with_final(&svc, 3, 2);
    let all = nothing_but_all();
    let steps = |v: &ResummariseView| v.writers.iter().map(|w| w.step.clone()).collect::<Vec<_>>();
    let menu = |here: &Here| steps(&resummarise_view(&svc, m, here).unwrap());
    // Nothing picked: no online writer, whatever is installed.
    assert_eq!(menu(&all), [StepChoice::Local]);
    // T14.72 (12.9): Claude Code picked and installed is offered, ticked or not; Codex,
    // installed and not picked, is not.
    pick_service(&svc, &host, ServiceChoice::ClaudeCode).unwrap();
    assert_eq!(
        earshot_runtime::summary_order::summary_order(&svc.store())
            .unwrap()
            .0
            .steps(),
        [LOCAL],
        "not ticked"
    );
    assert_eq!(menu(&all), [StepChoice::Local, StepChoice::ClaudeCode]);
    ordered(
        &svc,
        &host,
        &[StepChoice::ClaudeCode, StepChoice::Local],
        None,
    );
    assert_eq!(menu(&all), [StepChoice::Local, StepChoice::ClaudeCode]);
    assert_eq!(
        menu(&Here {
            claude: false,
            ..nothing_but_all()
        }),
        [StepChoice::Local],
        "a picked CLI that is not installed is not offered"
    );
    // Codex picked and installed: offered, and Claude Code no longer.
    pick_service(&svc, &host, ServiceChoice::Codex).unwrap();
    assert_eq!(menu(&all), [StepChoice::Local, StepChoice::Codex]);
    // A picked Anthropic endpoint needs a key saved for its address.
    pick_service(&svc, &host, ServiceChoice::AnthropicApi).unwrap();
    let anthropic = |url: &str| SummarySettingsValue {
        provider: crate::summaries::ProviderChoice::Anthropic,
        url: url.into(),
        ..endpoint_value()
    };
    ordered(
        &svc,
        &host,
        &[StepChoice::Local, StepChoice::Endpoint],
        Some(&anthropic("https://api.anthropic.com")),
    );
    assert!(menu(&all).contains(&StepChoice::Endpoint));
    let no_key = Here {
        key: false,
        ..nothing_but_all()
    };
    assert!(!menu(&no_key).contains(&StepChoice::Endpoint));
    // T14.31: a key saved for another address is not offered, as the Summaries page says and
    // the run refuses; saved for this address, it is.
    let proxy = anthropic("https://proxy.example.invalid");
    ordered(
        &svc,
        &host,
        &[StepChoice::Local, StepChoice::Endpoint],
        Some(&proxy),
    );
    assert!(!menu(&all).contains(&StepChoice::Endpoint));
    {
        let mut db = svc.store();
        earshot_runtime::summary_settings::bind_api_key(&mut db, &proxy.settings()).unwrap();
    }
    assert!(menu(&all).contains(&StepChoice::Endpoint));
    ordered(
        &svc,
        &host,
        &[StepChoice::Local, StepChoice::Endpoint],
        Some(&anthropic("https://api.anthropic.com")),
    );
    assert!(!menu(&all).contains(&StepChoice::Endpoint));
    {
        let mut db = svc.store();
        earshot_runtime::summary_settings::unbind_api_key(&mut db).unwrap();
    }
    assert!(menu(&all).contains(&StepChoice::Endpoint));
    // A picked OpenAI-compatible endpoint needs no key; picking it clears the Anthropic address,
    // so it is offered once its form is saved.
    pick_service(&svc, &host, ServiceChoice::OwnServer).unwrap();
    assert!(!menu(&no_key).contains(&StepChoice::Endpoint), "not set up");
    ordered(
        &svc,
        &host,
        &[StepChoice::Local, StepChoice::Endpoint],
        Some(&endpoint_value()),
    );
    assert_eq!(menu(&no_key), [StepChoice::Local, StepChoice::Endpoint]);
    let nothing = Here {
        local: false,
        key: false,
        claude: false,
        codex: false,
    };
    assert_eq!(menu(&nothing), [StepChoice::Endpoint]);
    // A meeting kept on this computer: the online writer is disabled with the reason.
    {
        let mut db = svc.store();
        let mut row = db.meeting(m).unwrap().unwrap();
        row.summary_kept_local = true;
        db.put_meeting(row).unwrap();
    }
    let v = resummarise_view(&svc, m, &all).unwrap();
    for w in &v.writers {
        let expected = (w.step != StepChoice::Local).then(|| COPY.resummarise.kept_local.clone());
        assert_eq!(w.disabled, expected, "{:?}", w.step);
    }
    assert_eq!(v.writers.len(), 2);
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
    ordered(
        &svc,
        &host,
        &[StepChoice::ClaudeCode, StepChoice::Local],
        None,
    );
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
    ordered(&svc, &host, &[StepChoice::Endpoint], Some(&value));
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
    // During the meeting the user picks Codex as the online service: it takes the endpoint's
    // place (T14.72).
    pick_service(&svc, &host, ServiceChoice::Codex).unwrap();
    assert_eq!(
        earshot_runtime::summary_order::summary_order(&svc.store())
            .unwrap()
            .0
            .steps(),
        [CODEX]
    );
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
    // The negative control: with the endpoint picked again (still set up, in Codex's place), it
    // writes the summary.
    pick_service(&svc, &host, ServiceChoice::OwnServer).unwrap();
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
        ordered(&svc, &host, &steps, None);
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

// ---- T14.66: stopped and moving notes, Stop's copy, local failure kinds ----

#[test]
fn summary_order_detail_note_says_a_run_was_stopped_or_moves_on() {
    use earshot_runtime::summary_order::run::{RunOrigin, RunState, StepOutcome};
    let cx = cx();
    let request = EntityId::from_bytes([9; 16]);
    // A named computer the user stopped: the stopped note, not a hand-back.
    let mut stopped = SummaryRun {
        run: 1,
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
    let want = "You stopped this summary and asked Invented desk to stop writing it. Re-summarise to write it again.";
    assert_eq!(
        run_note_stopped(&RunView::Ended(stopped.clone()), false, true, &cx).as_deref(),
        Some(want)
    );
    // A computer not known is named mid-sentence in lower case.
    let mut unknown = stopped.clone();
    unknown.steps = vec![OrderStep::Computer(DeviceId::from_bytes([0x77; 16]))];
    let note = run_note_stopped(&RunView::Ended(unknown), false, true, &cx).unwrap();
    assert!(
        note.starts_with(&format!(
            "You stopped this summary and asked {} to stop",
            crate::offload::waiting_device_name_in(None)
        )),
        "{note}"
    );
    assert!(!note.contains(&format!("asked {}", COPY.unnamed)), "{note}");
    // A summary came after the stopped request: the note falls.
    assert_eq!(
        run_note_stopped(&RunView::Ended(stopped.clone()), false, false, &cx),
        None
    );
    // An order run that left steps before it was stopped says them too.
    stopped.origin = RunOrigin::Order;
    stopped.steps = vec![LOCAL, OrderStep::Computer(DESK), CLAUDE];
    stopped.cursor = 2;
    stopped.outcomes = vec![
        StepOutcome {
            step: 0,
            word: OutcomeWord::TooLong,
        },
        StepOutcome {
            step: 1,
            word: OutcomeWord::Stopped,
        },
    ];
    assert_eq!(
        run_note_stopped(&RunView::Ended(stopped), false, true, &cx).as_deref(),
        Some(
            "You stopped this summary and asked Invented desk to stop writing it. Re-summarise to write it again. Tried first: This computer: too long."
        )
    );
    // A run going on after a step was left: what became of it, and what it tries now.
    let moving = SummaryRun {
        run: 2,
        origin: RunOrigin::Order,
        steps: vec![LOCAL, CLAUDE],
        cursor: 1,
        outcomes: vec![StepOutcome {
            step: 0,
            word: OutcomeWord::CardStopped,
        }],
        waiting_on: None,
        state: RunState::Running,
        writer: None,
        stopped: None,
    };
    assert_eq!(
        run_note_stopped(&RunView::Unfinished(moving.clone()), true, false, &cx).as_deref(),
        Some("So far: This computer: graphics card process stopped. Now trying Claude Code.")
    );
    // Control: with nothing working on the meeting it is interrupted, as before.
    assert_eq!(
        run_note_stopped(&RunView::Unfinished(moving), false, false, &cx).as_deref(),
        Some(COPY.interrupted.as_str())
    );
}

#[test]
fn summary_order_waiting_banner_lists_the_steps_already_left() {
    use earshot_runtime::summary_order::run::{RunOrigin, RunState, StepOutcome};
    let cx = cx();
    let mut waiting = SummaryRun {
        run: 3,
        origin: RunOrigin::Order,
        steps: vec![LOCAL, CLAUDE, OrderStep::Computer(DESK)],
        cursor: 3,
        outcomes: vec![
            StepOutcome {
                step: 0,
                word: OutcomeWord::LoadFailed,
            },
            StepOutcome {
                step: 1,
                word: OutcomeWord::NotSignedIn,
            },
        ],
        waiting_on: Some(EntityId::from_bytes([8; 16])),
        state: RunState::Waiting,
        writer: None,
        stopped: None,
    };
    assert_eq!(
        earlier_text(&RunView::Waiting(waiting.clone()), &cx).as_deref(),
        Some("Tried first: This computer: could not load; Claude Code: not signed in.")
    );
    // Control: a run asked at step 1 has nothing before it.
    waiting.outcomes.clear();
    assert_eq!(earlier_text(&RunView::Waiting(waiting), &cx), None);
}

#[test]
fn summary_order_stop_copy_says_why_nothing_stopped() {
    use earshot_runtime::summary_order::run::StopAnswer;
    let cx = cx();
    assert_eq!(stop_copy(StopAnswer::Stopped { device: DESK }, &cx), None);
    assert_eq!(
        stop_copy(StopAnswer::AlreadyWritten { device: Some(DESK) }, &cx).as_deref(),
        Some("Invented desk had already written the summary.")
    );
    // A summary whose writer is not known names no device.
    assert_eq!(
        stop_copy(StopAnswer::AlreadyWritten { device: None }, &cx).as_deref(),
        Some(COPY.stop.already_summarised.as_str())
    );
    assert_eq!(
        stop_copy(StopAnswer::NotWaiting, &cx).as_deref(),
        Some(COPY.stop.not_waiting.as_str())
    );
}

#[test]
fn summary_order_every_local_failure_kind_has_fixed_copy_and_a_word() {
    use earshot_runtime::summary::LocalFailureKind as K;
    for k in K::ALL {
        match (k, local_failure_copy(k)) {
            (K::Other | K::Cancelled, copy) => assert_eq!(copy, None, "{k:?}"),
            (_, Some(copy)) => {
                assert!(!copy.is_empty() && !copy.contains('\u{2014}') && !copy.contains("  "));
            }
            (_, None) => panic!("{k:?} has no copy"),
        }
    }
    // Each kind's word in the order exists in the table.
    for w in OutcomeWord::ALL {
        assert!(!word(w).is_empty(), "{w:?}");
    }
}

#[test]
fn summary_order_a_local_failure_is_said_by_its_kind_in_the_window() {
    use earshot_runtime::RuntimeError;
    use earshot_runtime::summary::LocalSummaryError as L;
    let text = |e: L| crate::plain_text::runtime_text(&RuntimeError::LocalSummary(e));
    assert_eq!(text(L::GpuStalled), COPY.local_failures.stalled);
    assert_eq!(text(L::GpuChildStalled), COPY.local_failures.card_stopped);
    assert_eq!(
        text(L::CudaFilesChanged {
            why: "invented words".into()
        }),
        COPY.local_failures.cuda_changed
    );
    // A child's own kind is said; with none, today's copy.
    assert_eq!(
        text(L::ChildRun {
            message: "invented words".into(),
            kind: Some(earshot_runtime::summary::LocalFailureKind::TooLong),
        }),
        COPY.local_failures.too_long
    );
    assert_eq!(
        text(L::Chunks(earshot_core::prompt::TimeChunkError::ZeroWindow)),
        crate::plain_text::LOCAL_SUMMARY_FAILED
    );
}

/// T14.66 (docs/summary-order.md 11.5): a meeting's own summary that leaves a writer for the next
/// tells the host as it happens (`HostStep::SummaryRunChanged`, the window's
/// `MeetingEvent::SummaryRunChanged` with the meeting's id only). Control: a first step that
/// writes tells nothing. Needs `cargo xtask fetch-models --set ci` for the pre-flight's model.
#[test]
fn summary_order_a_meetings_summary_says_each_writer_it_leaves() {
    use crate::meeting::{HostPrepare, MeetingSource, Prepare, StartRequest};
    use crate::meeting_events::PostProgressMapper;
    use earshot_runtime::model_settings::{ModelPick, ModelSettings, set_model_settings};

    for left_first in [true, false] {
        let dir = TempDir::new("summary-order-moves");
        let svc = services(&dir);
        let server = earshot_llm_replay::mock::MockServer::start(vec![summary_reply()]).unwrap();
        set_model_settings(
            &mut svc.store(),
            &ModelSettings {
                transcription: ModelPick::Named("base.en".into()),
                ..ModelSettings::default()
            },
        )
        .unwrap();
        let host = host();
        let value = SummarySettingsValue {
            url: server.endpoint().to_string(),
            model: "test-model".into(),
            ..endpoint_value()
        };
        // Since T14.72 an order holds one online writer: the writer left is a computer that
        // cannot be asked (sync off; it was Codex before). Not the local model: a meeting reads
        // the real models folder, where one may be installed.
        let desk = StepChoice::Computer {
            device: DESK.to_string(),
        };
        let order: &[StepChoice] = if left_first {
            &[desk, StepChoice::Endpoint]
        } else {
            &[StepChoice::Endpoint, desk]
        };
        ordered(&svc, &host, order, Some(&value));
        let probes = SummaryProbes {
            keys: Arc::new(CountedKeys::default()),
            claude: Arc::new(NeverClaude),
            codex: Arc::new(CountedCodex::default()),
        };
        let prepare = HostPrepare::with_source(Box::new(|_, _| {
            Ok(MeetingSource {
                starter: Box::new(|_, _, _, _| unreachable!("this test starts no capture")),
                clock_domain: ClockDomain::Replay,
                notices: Vec::new(),
                call_signal: None,
            })
        }))
        .with_probes(probes);
        let queue = earshot_runtime::meeting_queue::MeetingQueue::new();
        let ticket = Arc::new(queue.enter());
        let request = StartRequest {
            mode: crate::meeting_events::CaptureMode::Call,
            title: String::new(),
            keep_local: false,
        };
        let prepared = prepare
            .prepare(&svc, &request, &ticket)
            .unwrap_or_else(|e| panic!("{e:?}: cargo xtask fetch-models --set ci"));
        let m = meeting_with_final(&svc, 7, 2);
        let mut mapper = PostProgressMapper::of(m.to_string());
        let mut events = Vec::new();
        let done = (prepared.after)(&svc, &outcome(m), &CancelToken::new(), &mut |step| {
            if let Some(e @ MeetingEvent::SummaryRunChanged { .. }) = mapper.step(&step) {
                events.push(e);
            }
        });
        assert!(
            matches!(done.summary, SummaryResult::Stored { .. }),
            "{:?}",
            done.summary
        );
        if left_first {
            assert_eq!(
                events,
                vec![MeetingEvent::SummaryRunChanged {
                    meeting: m.to_string()
                }]
            );
        } else {
            assert!(events.is_empty(), "{events:?}");
        }
    }
}

/// T14.66: a re-summary that leaves a writer for the next tells the job as it happens (the job's
/// `JobEvent::SummaryRunChanged`). Control: one whose first writer writes tells nothing.
#[test]
fn summary_order_a_resummary_says_each_writer_it_leaves() {
    for left_first in [true, false] {
        let dir = TempDir::new("summary-order-job-moves");
        let svc = services(&dir);
        let server = earshot_llm_replay::mock::MockServer::start(vec![summary_reply()]).unwrap();
        let host = host();
        let value = SummarySettingsValue {
            url: server.endpoint().to_string(),
            model: "test-model".into(),
            ..endpoint_value()
        };
        // Since T14.72 an order holds one online writer: the writer left is the local model,
        // not installed here (it was Codex before).
        let order: &[StepChoice] = if left_first {
            &[StepChoice::Local, StepChoice::Endpoint]
        } else {
            &[StepChoice::Endpoint, StepChoice::Local]
        };
        ordered(&svc, &host, order, Some(&value));
        let m = meeting_with_final(&svc, 8, 2);
        let models = earshot_runtime::backends::ModelDir::new(dir.0.join("no-models"));
        let keys = CountedKeys::default();
        let codex = CountedCodex::default();
        let mut changes = 0;
        let done = crate::jobs::summarise_run_with(
            &svc,
            m,
            &crate::jobs::SummaryStart::Order,
            &CancelToken::new(),
            &models,
            (&keys, &NeverClaude, &codex),
            None,
            &mut |_| {},
            &mut || changes += 1,
        )
        .unwrap();
        assert!(
            matches!(done.result, SummaryResult::Stored { .. }),
            "{:?}",
            done.result
        );
        assert_eq!(changes, usize::from(left_first));
    }
}

// ---- T14.72: the one online service (docs/summary-order.md 12) ----

/// The copy of 12.11 is the table's, word for word with the runtime's fixed notes and refusals.
#[test]
fn summary_order_online_service_copy_matches_the_shared_table() {
    use earshot_runtime::summary_order as order;
    let t = table();
    let page = &COPY.page;
    for (key, ours) in [
        ("online_service", &page.online_service),
        ("choose_service", &page.choose_service),
        ("service_label", &page.service_label),
        ("second_online_dropped", &page.second_online_dropped),
        ("unknown_service", &page.unknown_service),
        ("not_the_service", &page.not_the_service),
        ("settings_changed", &page.settings_changed),
    ] {
        assert_eq!(t["page"][key].as_str(), Some(ours.as_str()), "{key}");
    }
    assert_eq!(page.online_service, "Online service");
    assert_eq!(page.choose_service, "Choose a service");
    assert_eq!(page.service_label, "Service");
    // The runtime's notes and refusals are the same text.
    assert_eq!(page.second_online_dropped, order::SECOND_ONLINE_DROPPED);
    assert_eq!(page.unknown_service, order::UNKNOWN_SERVICE);
    assert_eq!(page.not_the_service, order::NOT_THE_SERVICE);
    assert_eq!(page.settings_changed, order::SETTINGS_CHANGED);
    assert_eq!(word(OutcomeWord::NotChosen), "not chosen");
    // Each refusal reaches the page as its fixed copy.
    for (e, want) in [
        (SummaryOrderError::StaleProvider, &page.settings_changed),
        (SummaryOrderError::NotThePick, &page.not_the_service),
        (SummaryOrderError::TwoOnline, &page.not_the_service),
    ] {
        let got = order_error(e);
        assert_eq!((got.kind.as_str(), &got.message), ("invalid_setting", want));
    }
    for (e, want) in [
        (
            earshot_runtime::summary_settings::SummarySettingsError::StaleProvider,
            &page.settings_changed,
        ),
        (
            earshot_runtime::summary_settings::SummarySettingsError::NotTheService,
            &page.not_the_service,
        ),
    ] {
        let got = crate::summaries::settings_error(e);
        assert_eq!((got.kind.as_str(), &got.message), ("invalid_setting", want));
    }
    // A one-step run whose step was not chosen says so in its banner (4.4).
    assert_eq!(
        crate::meeting_events::skip_copy(&SkipReason::NotChosen),
        page.not_the_service
    );
    // The provider refusal joins the Summaries settings table.
    assert_eq!(
        crate::testutil::summary_cases()["messages"]["settings_changed"].as_str(),
        Some(page.settings_changed.as_str())
    );
}

/// Picking a service is one command that returns both views as stored (12.6, 12.8): the row
/// names the pick, is tickable only when it can be a step, and the setting view follows the
/// provider of an endpoint service.
#[test]
fn summary_order_online_service_pick_returns_both_views() {
    let dir = TempDir::new("summary-order-online-pick");
    let svc = services(&dir);
    let host = host();
    let saved = pick_service(&svc, &host, ServiceChoice::Codex).unwrap();
    assert_eq!(saved.order.online.service, Some(ServiceChoice::Codex));
    assert_eq!(saved.order.online.step, Some(StepChoice::Codex));
    assert_eq!(saved.order, order_view(&svc, &host).unwrap());
    assert_eq!(
        saved.settings,
        crate::summaries::settings_view(&svc).unwrap()
    );
    // An endpoint service not set up cannot be ticked yet, and says why.
    let saved = pick_service(&svc, &host, ServiceChoice::AnthropicApi).unwrap();
    assert_eq!(
        saved.order.online.service,
        Some(ServiceChoice::AnthropicApi)
    );
    assert_eq!(saved.order.online.step, None);
    assert_eq!(
        saved.order.online.reason,
        Some(COPY.page.not_set_up.clone())
    );
    assert_eq!(
        saved.settings.value.provider,
        crate::summaries::ProviderChoice::Anthropic
    );
    // An order holding an online writer that is not the pick is refused with fixed copy.
    let e = set_order(&svc, &host, &[StepChoice::Codex], None).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str()),
        ("invalid_setting", COPY.page.not_the_service.as_str())
    );
    // A stale settings save through the page's own command is refused with fixed copy.
    let stale = crate::summaries::SummarySettingsValue {
        provider: crate::summaries::ProviderChoice::OpenAiCompatible,
        live: true,
        ..saved.settings.value.clone()
    };
    let e = crate::summaries::set_settings(&svc, &stale).unwrap_err();
    assert_eq!(e.message, COPY.page.settings_changed);
    assert!(!crate::summaries::settings_view(&svc).unwrap().value.live);
    // The control: the same save from the view just returned is stored.
    let fresh = crate::summaries::SummarySettingsValue {
        live: true,
        ..saved.settings.value
    };
    assert!(
        crate::summaries::set_settings(&svc, &fresh)
            .unwrap()
            .value
            .live
    );
}

/// 12.9: a named re-summarise of an online writer that is not the pick is refused before a job
/// starts, and a job whose step finds the pick changed fails with the same copy and hands
/// nothing over. Control: the picked service is accepted and runs.
#[test]
fn summary_order_online_service_named_start_takes_only_the_pick() {
    let dir = TempDir::new("summary-order-online-named");
    let svc = services(&dir);
    let host = host();
    pick_service(&svc, &host, ServiceChoice::ClaudeCode).unwrap();
    let e = named_step_allowed(&svc, CODEX).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str()),
        ("invalid_setting", COPY.page.not_the_service.as_str())
    );
    named_step_allowed(&svc, CLAUDE).unwrap();
    named_step_allowed(&svc, LOCAL).unwrap();
    // The pick changed after the command: the job's step reads `not chosen`.
    let m = meeting_with_final(&svc, 11, 2);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("no-models"));
    let keys = CountedKeys::default();
    let codex = CountedCodex::default();
    let e = crate::jobs::summarise_run_with(
        &svc,
        m,
        &crate::jobs::SummaryStart::Named(CODEX),
        &CancelToken::new(),
        &models,
        (&keys, &NeverClaude, &codex),
        None,
        &mut |_| {},
        &mut || {},
    )
    .unwrap_err();
    assert_eq!(e.message, COPY.page.not_the_service);
    assert_eq!(codex.0.load(Ordering::SeqCst), 0, "Codex never built");
    // The control: the picked endpoint is accepted and writes.
    let server = earshot_llm_replay::mock::MockServer::start(vec![summary_reply()]).unwrap();
    let value = SummarySettingsValue {
        url: server.endpoint().to_string(),
        model: "test-model".into(),
        ..endpoint_value()
    };
    ordered(&svc, &host, &[StepChoice::Endpoint], Some(&value));
    named_step_allowed(&svc, ENDPOINT).unwrap();
    let done = crate::jobs::summarise_run_with(
        &svc,
        m,
        &crate::jobs::SummaryStart::Named(ENDPOINT),
        &CancelToken::new(),
        &models,
        (&keys, &NeverClaude, &codex),
        None,
        &mut |_| {},
        &mut || {},
    )
    .unwrap();
    assert!(
        matches!(done.result, SummaryResult::Stored { .. }),
        "{:?}",
        done.result
    );
    assert_eq!(server.requests().len(), 1);
}

/// The coordinator's decision on T14.72's row: the Online service row's label and destination
/// are exactly what a StepView of the picked step carries (the same Rust functions), so the UI
/// keeps no name rule. Control: no pick gives neither.
#[test]
fn summary_order_online_service_row_is_labelled_as_its_step_view() {
    let dir = TempDir::new("summary-order-online-label");
    let svc = services(&dir);
    let host = host();
    let row = order_view(&svc, &host).unwrap().online;
    assert_eq!((row.label, row.destination), (None, None), "no pick");
    let server_value = SummarySettingsValue {
        url: "http://127.0.0.1:8080/v1".into(),
        model: "test-model".into(),
        ..endpoint_value()
    };
    for (service, step, value) in [
        (ServiceChoice::ClaudeCode, CLAUDE, None),
        (ServiceChoice::Codex, CODEX, None),
        (ServiceChoice::OwnServer, ENDPOINT, Some(&server_value)),
    ] {
        pick_service(&svc, &host, service).unwrap();
        if let Some(v) = value {
            ordered(&svc, &host, &[StepChoice::Endpoint], Some(v));
        }
        let row = order_view(&svc, &host).unwrap().online;
        let want = step_view(step, &StepContext::read(&svc));
        assert_eq!(
            row.label.as_deref(),
            Some(want.label.as_str()),
            "{service:?}"
        );
        assert_eq!(
            row.destination.as_deref(),
            Some(want.destination.as_str()),
            "{service:?}"
        );
    }
    let row = order_view(&svc, &host).unwrap().online;
    assert_eq!(row.label.as_deref(), Some("Model server (127.0.0.1)"));
    assert_eq!(row.destination.as_deref(), Some("Goes to 127.0.0.1"));
    // A ticked step reads the same in the order's own list.
    let ticked = order_view(&svc, &host).unwrap();
    assert_eq!(ticked.steps[0].label, row.label.unwrap());
    // An Anthropic pick with no address yet (the switch cleared it) still names Anthropic
    // (12.8); control: the own server with no address is "Your own server".
    pick_service(&svc, &host, ServiceChoice::AnthropicApi).unwrap();
    let row = order_view(&svc, &host).unwrap().online;
    assert_eq!(row.label.as_deref(), Some("Anthropic API"));
    assert_eq!(row.destination.as_deref(), Some("Goes to Anthropic"));
    pick_service(&svc, &host, ServiceChoice::OwnServer).unwrap();
    let row = order_view(&svc, &host).unwrap().online;
    assert_eq!(row.label.as_deref(), Some("Your own server"));
    assert_eq!(
        row.destination.as_deref(),
        Some("Goes to the address you enter")
    );
}

/// Job work that records how each re-summary was started (and runs nothing).
#[derive(Default)]
struct RecordingWork(std::sync::Mutex<Vec<crate::jobs::SummaryStart>>);

impl crate::jobs::JobWork for RecordingWork {
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
        start: &crate::jobs::SummaryStart,
        _: &CancelToken,
        _: &mut dyn FnMut(String),
    ) -> Result<crate::jobs::JobSummary, IpcError> {
        self.0.lock().unwrap().push(start.clone());
        Ok(SummaryResult::Skipped {
            reason: SkipReason::NotRequested,
        }
        .into())
    }
}

/// Review advisory 2 (12.9): the command's own body refuses a named online writer that is not
/// the pick before any job starts. Control: the pick is accepted and its job starts.
#[test]
fn summary_order_online_service_resummarise_with_refuses_before_a_job_starts() {
    let dir = TempDir::new("summary-order-online-resummarise-with");
    let svc = services(&dir);
    let host = host();
    let m = meeting_with_final(&svc, 13, 2);
    pick_service(&svc, &host, ServiceChoice::ClaudeCode).unwrap();
    let work = Arc::new(RecordingWork::default());
    let sink: crate::jobs::JobSink = Arc::new(|_| {});
    let runner = crate::jobs::JobRunner::new(
        Arc::clone(&work) as Arc<dyn crate::jobs::JobWork>,
        sink,
        crate::activity::WorkGate::new(),
        &crate::cuda_settings::CudaLatch::resolved_off(),
    );
    let e = resummarise_named(&svc, &runner, &m.to_string(), &StepChoice::Codex).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str()),
        ("invalid_setting", COPY.page.not_the_service.as_str())
    );
    runner.join();
    assert!(work.0.lock().unwrap().is_empty(), "no job started");
    // The control: the pick starts its job.
    resummarise_named(&svc, &runner, &m.to_string(), &StepChoice::ClaudeCode).unwrap();
    runner.join();
    assert_eq!(
        *work.0.lock().unwrap(),
        vec![crate::jobs::SummaryStart::Named(CLAUDE)]
    );
}

/// Review advisory 3 (12.13 item 12): picking a service records no run, no handover and asks no
/// CLI. Control: a run of the picked service afterwards does record a run and probe its CLI, so
/// the same observations can see a run.
#[test]
fn summary_order_online_service_picking_alone_sends_nothing() {
    let dir = TempDir::new("summary-order-online-pick-alone");
    let svc = services(&dir);
    let host = host();
    let m = meeting_with_final(&svc, 17, 2);
    let codex = CountedCodex::default();
    let handovers = |svc: &Services| {
        svc.store()
            .local_value(&format!("summary-handovers/{m}"))
            .unwrap()
    };
    pick_service(&svc, &host, ServiceChoice::Codex).unwrap();
    assert_eq!(
        earshot_runtime::summary_order::run::run_view(&svc.store(), m).unwrap(),
        RunView::None
    );
    assert_eq!(handovers(&svc), None);
    assert_eq!(codex.0.load(Ordering::SeqCst), 0);
    // The control: a named run of the pick records its run and probes Codex once.
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("no-models"));
    let keys = CountedKeys::default();
    let _ = crate::jobs::summarise_run_with(
        &svc,
        m,
        &crate::jobs::SummaryStart::Named(CODEX),
        &CancelToken::new(),
        &models,
        (&keys, &NeverClaude, &codex),
        None,
        &mut |_| {},
        &mut || {},
    );
    assert_eq!(codex.0.load(Ordering::SeqCst), 1);
    assert_ne!(
        earshot_runtime::summary_order::run::run_view(&svc.store(), m).unwrap(),
        RunView::None
    );
}
