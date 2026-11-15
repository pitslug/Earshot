//! T12.63a: the stored order of summary writers (docs/summary-order.md section 3). Every name
//! holds `summary_order`, so `cargo test -p earshot-runtime summary_order` selects them.

use earshot_core::identity::DeviceId;

use super::*;
use crate::Services;
use crate::keep_local::SummaryPrivacy;
use crate::local::DeviceRecord;
use crate::summary_settings::{
    EndpointProvider, SummarySettings, SummaryWriter, set_summary_settings, summary_settings,
};
use crate::testutil::{TempDir, services};

const DESK: DeviceId = DeviceId::from_bytes([7; 16]);
const LAPTOP: DeviceId = DeviceId::from_bytes([9; 16]);

const LOCAL: OrderStep = OrderStep::LOCAL;
const ENDPOINT: OrderStep = OrderStep::Writer(WriterStep::Endpoint);
const CLAUDE: OrderStep = OrderStep::Writer(WriterStep::ClaudeCode);
const CODEX: OrderStep = OrderStep::Writer(WriterStep::Codex);

fn order(steps: &[OrderStep]) -> SummaryOrder {
    SummaryOrder::new(steps.to_vec()).unwrap()
}

/// Endpoint fields the rules accept (a loopback model server).
fn with_endpoint() -> SummarySettings {
    SummarySettings {
        provider: EndpointProvider::OpenAiCompatible,
        url: "http://127.0.0.1:8080/v1".into(),
        model: "invented-model".into(),
        ..SummarySettings::default()
    }
}

fn raw(store: &Store, key: &str) -> Option<String> {
    store
        .local_value(key)
        .unwrap()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[test]
fn summary_order_shape_is_at_most_three_steps_and_nothing_twice() {
    assert!(SummaryOrder::new(vec![]).unwrap().is_none());
    assert!(SummaryOrder::new(vec![LOCAL, CLAUDE, OrderStep::Computer(DESK)]).is_ok());
    assert!(matches!(
        SummaryOrder::new(vec![LOCAL, CLAUDE, CODEX, ENDPOINT]),
        Err(SummaryOrderError::TooManySteps)
    ));
    assert!(matches!(
        SummaryOrder::new(vec![CLAUDE, LOCAL, CLAUDE]),
        Err(SummaryOrderError::Twice)
    ));
    assert!(matches!(
        SummaryOrder::new(vec![OrderStep::Computer(DESK), OrderStep::Computer(DESK)]),
        Err(SummaryOrderError::Twice)
    ));
    // Two different computers are two writers.
    assert!(
        SummaryOrder::new(vec![OrderStep::Computer(DESK), OrderStep::Computer(LAPTOP)]).is_ok()
    );
}

#[test]
fn summary_order_bytes_are_pinned() {
    let mut want = 1u32.to_le_bytes().to_vec();
    want.extend_from_slice(
        br#"{"steps":[{"computer":"07070707-0707-0707-0707-070707070707"},{"writer":"local"},{"writer":"claude-code"}]}"#,
    );
    let o = order(&[OrderStep::Computer(DESK), LOCAL, CLAUDE]);
    assert_eq!(encode(&o), want);
    assert_eq!(decode(&want), Some(o));
    let mut none = 1u32.to_le_bytes().to_vec();
    none.extend_from_slice(br#"{"none":true}"#);
    assert_eq!(encode(&SummaryOrder::none()), none);
    assert_eq!(decode(&none), Some(SummaryOrder::none()));
    let mut codex = 1u32.to_le_bytes().to_vec();
    codex.extend_from_slice(br#"{"steps":[{"writer":"endpoint"},{"writer":"codex"}]}"#);
    assert_eq!(decode(&codex), Some(order(&[ENDPOINT, CODEX])));
}

#[test]
fn summary_order_decode_refuses_what_breaks_the_rules() {
    let with = |json: &str| {
        let mut b = 1u32.to_le_bytes().to_vec();
        b.extend_from_slice(json.as_bytes());
        decode(&b)
    };
    assert_eq!(with(r#"{"steps":[]}"#), None, "no steps and not none");
    assert_eq!(with(r#"{"none":true,"steps":[{"writer":"local"}]}"#), None);
    assert_eq!(
        with(r#"{"steps":[{"writer":"local"},{"writer":"local"}]}"#),
        None
    );
    assert_eq!(
        with(
            r#"{"steps":[{"writer":"local"},{"writer":"codex"},{"writer":"endpoint"},{"writer":"claude-code"}]}"#
        ),
        None
    );
    assert_eq!(with(r#"{"steps":[{"writer":"a-new-writer"}]}"#), None);
    assert_eq!(with(r#"{"steps":[{"writer":"local"}],"extra":1}"#), None);
    let mut v2 = 2u32.to_le_bytes().to_vec();
    v2.extend_from_slice(br#"{"steps":[{"writer":"local"}]}"#);
    assert_eq!(decode(&v2), None, "a version this build does not know");
    assert_eq!(decode(b"xy"), None);
}

#[test]
fn summary_order_mirror_is_what_an_older_build_reads() {
    let m = |steps: &[OrderStep]| order(steps).mirror();
    assert_eq!(
        m(&[OrderStep::Computer(DESK), LOCAL]),
        Mirror {
            writer: SummaryWriter::OnThisComputer,
            computer: Some(DESK)
        }
    );
    // A computer that is not step 1 is not mirrored: an older build would ask it first.
    assert_eq!(
        m(&[LOCAL, OrderStep::Computer(DESK)]),
        Mirror {
            writer: SummaryWriter::OnThisComputer,
            computer: None
        }
    );
    assert_eq!(m(&[CLAUDE, ENDPOINT]).writer, SummaryWriter::ClaudeCode);
    assert_eq!(
        m(&[OrderStep::Computer(DESK)]),
        Mirror {
            writer: SummaryWriter::NoSummary,
            computer: Some(DESK)
        }
    );
    assert_eq!(
        SummaryOrder::none().mirror(),
        Mirror {
            writer: SummaryWriter::NoSummary,
            computer: None
        }
    );
}

#[test]
fn summary_order_from_old_keys() {
    assert_eq!(
        SummaryOrder::from_old(SummaryWriter::Codex, Some(DESK)),
        order(&[OrderStep::Computer(DESK), CODEX])
    );
    assert_eq!(
        SummaryOrder::from_old(SummaryWriter::NoSummary, Some(DESK)),
        order(&[OrderStep::Computer(DESK)])
    );
    assert!(SummaryOrder::from_old(SummaryWriter::NoSummary, None).is_none());
    assert_eq!(
        SummaryOrder::from_old(SummaryWriter::OnThisComputer, None),
        order(&[LOCAL])
    );
}

#[test]
fn summary_order_with_no_stored_order_reads_the_old_keys() {
    let dir = TempDir::new("summary-order-old");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    // Nothing stored at all: the default writer.
    assert_eq!(summary_order(&db).unwrap(), (order(&[LOCAL]), vec![]));
    crate::local::set(&mut db, &DeviceRecord::SummaryWriter.key(), b"claude-code").unwrap();
    crate::local::set(
        &mut db,
        &DeviceRecord::SummaryOffloadDevice.key(),
        DESK.to_string().as_bytes(),
    )
    .unwrap();
    assert_eq!(
        summary_order(&db).unwrap().0,
        order(&[OrderStep::Computer(DESK), CLAUDE])
    );
    crate::local::set(&mut db, &DeviceRecord::SummaryWriter.key(), b"none").unwrap();
    assert_eq!(
        summary_order(&db).unwrap().0,
        order(&[OrderStep::Computer(DESK)])
    );
}

#[test]
fn summary_order_is_stored_with_its_mirror() {
    let dir = TempDir::new("summary-order-write");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    let o = order(&[LOCAL, OrderStep::Computer(DESK), CLAUDE]);
    set_summary_order(&mut db, &o, None).unwrap();
    assert_eq!(summary_order(&db).unwrap(), (o.clone(), vec![]));
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("local"));
    assert_eq!(
        raw(&db, crate::offload::OFFLOAD_DEVICE_KEY).as_deref(),
        Some("")
    );
    // An older build's readers see the mirror.
    assert_eq!(
        summary_settings(&db).unwrap().0.writer,
        SummaryWriter::OnThisComputer
    );
    assert_eq!(crate::offload::chosen_helper(&db).unwrap(), None);
    let first = order(&[OrderStep::Computer(DESK), CODEX]);
    set_summary_order(&mut db, &first, None).unwrap();
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("codex"));
    assert_eq!(crate::offload::chosen_helper(&db).unwrap(), Some(DESK));
    assert_eq!(summary_order(&db).unwrap().0, first);
    set_summary_order(&mut db, &SummaryOrder::none(), None).unwrap();
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("none"));
    assert!(summary_order(&db).unwrap().0.is_none());
}

#[test]
fn summary_order_endpoint_fields_are_required_wherever_the_endpoint_sits() {
    let dir = TempDir::new("summary-order-endpoint");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    let o = order(&[LOCAL, ENDPOINT]);
    assert!(matches!(
        set_summary_order(&mut db, &o, None),
        Err(SummaryOrderError::Settings(
            SummarySettingsError::MissingUrl
        ))
    ));
    assert_eq!(
        db.local_value(SUMMARY_ORDER_KEY).unwrap(),
        None,
        "nothing stored"
    );
    // With the page's endpoint fields given in the same write, it is stored.
    set_summary_order(&mut db, &o, Some(&with_endpoint())).unwrap();
    assert_eq!(summary_order(&db).unwrap().0, o);
    assert_eq!(
        summary_settings(&db).unwrap().0.url,
        "http://127.0.0.1:8080/v1"
    );
    // Against the stored fields too.
    set_summary_order(&mut db, &order(&[ENDPOINT]), None).unwrap();
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("endpoint"));
}

#[test]
fn summary_order_refuses_this_computer_as_a_step() {
    let dir = TempDir::new("summary-order-self");
    let (svc, _) = services(&dir);
    let me = svc.store().device();
    let mut db = svc.store();
    assert!(matches!(
        set_summary_order(&mut db, &order(&[OrderStep::Computer(me), LOCAL]), None),
        Err(SummaryOrderError::ThisDevice)
    ));
    assert_eq!(db.local_value(SUMMARY_ORDER_KEY).unwrap(), None);
}

#[test]
fn summary_order_old_keys_win_after_a_downgrade_edit() {
    let dir = TempDir::new("summary-order-downgrade");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    set_summary_order(&mut db, &order(&[LOCAL, CLAUDE]), None).unwrap();
    // An older build changes the writer: its choice is read, never the stale order.
    crate::local::set(&mut db, &DeviceRecord::SummaryWriter.key(), b"codex").unwrap();
    assert_eq!(summary_order(&db).unwrap(), (order(&[CODEX]), vec![]));
    // And a helper it chose.
    set_summary_order(&mut db, &order(&[LOCAL]), None).unwrap();
    crate::local::set(
        &mut db,
        &DeviceRecord::SummaryOffloadDevice.key(),
        DESK.to_string().as_bytes(),
    )
    .unwrap();
    assert_eq!(
        summary_order(&db).unwrap().0,
        order(&[OrderStep::Computer(DESK), LOCAL])
    );
}

#[test]
fn summary_order_unreadable_reads_the_old_keys_with_a_note() {
    let dir = TempDir::new("summary-order-unreadable");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    crate::local::set(&mut db, &DeviceRecord::SummaryWriter.key(), b"claude-code").unwrap();
    crate::local::set(&mut db, &DeviceRecord::SummaryOrder.key(), b"not an order").unwrap();
    let (o, notes) = summary_order(&db).unwrap();
    assert_eq!(o, order(&[CLAUDE]));
    assert_eq!(notes, vec![UNREADABLE_ORDER.to_owned()]);
    assert!(!notes[0].contains("not an order"), "never the stored text");
    let mut v9 = 9u32.to_le_bytes().to_vec();
    v9.extend_from_slice(br#"{"steps":[{"writer":"local"}]}"#);
    crate::local::set(&mut db, &DeviceRecord::SummaryOrder.key(), &v9).unwrap();
    assert_eq!(
        summary_order(&db).unwrap().1,
        vec![UNREADABLE_ORDER.to_owned()]
    );
    // A stored endpoint step whose endpoint is gone is refused on read too.
    let endpoint_first = encode(&order(&[ENDPOINT]));
    crate::local::set(&mut db, &DeviceRecord::SummaryOrder.key(), &endpoint_first).unwrap();
    crate::local::set(&mut db, &DeviceRecord::SummaryWriter.key(), b"endpoint").unwrap();
    let (o, notes) = summary_order(&db).unwrap();
    assert_eq!(notes, vec![UNREADABLE_ORDER.to_owned()]);
    assert_eq!(
        o,
        order(&[LOCAL]),
        "the old writer without its endpoint reads as the default"
    );
}

#[test]
fn summary_order_and_its_mirror_are_one_transaction() {
    let dir = TempDir::new("summary-order-fault");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    set_summary_order(&mut db, &order(&[LOCAL]), None).unwrap();
    let before = (
        raw(&db, SUMMARY_ORDER_KEY),
        raw(&db, SUMMARY_WRITER_KEY),
        raw(&db, crate::offload::OFFLOAD_DEVICE_KEY),
    );
    fault::fail_after_order();
    let refused = set_summary_order(&mut db, &order(&[OrderStep::Computer(DESK), CODEX]), None);
    assert!(matches!(refused, Err(SummaryOrderError::Store(_))));
    let after = (
        raw(&db, SUMMARY_ORDER_KEY),
        raw(&db, SUMMARY_WRITER_KEY),
        raw(&db, crate::offload::OFFLOAD_DEVICE_KEY),
    );
    assert_eq!(after, before, "neither the order nor the mirror changed");
    assert_eq!(summary_order(&db).unwrap().0, order(&[LOCAL]));
}

#[test]
fn summary_order_the_page_writer_and_helper_go_through_the_order() {
    let dir = TempDir::new("summary-order-page");
    let (svc, _) = services(&dir);
    {
        let mut db = svc.store();
        set_summary_order(&mut db, &order(&[LOCAL, CODEX, CLAUDE]), None).unwrap();
        // The page's one writer replaces the first writer step and keeps the others.
        set_summary_settings(
            &mut db,
            &SummarySettings {
                writer: SummaryWriter::ClaudeCode,
                ..SummarySettings::default()
            },
        )
        .unwrap();
        assert_eq!(summary_order(&db).unwrap().0, order(&[CLAUDE, CODEX]));
        assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("claude-code"));
    }
    // The helper becomes the step-1 computer, then goes.
    crate::offload::set_chosen_helper(&svc, Some(DESK)).unwrap();
    assert_eq!(
        summary_order(&svc.store()).unwrap().0,
        order(&[OrderStep::Computer(DESK), CLAUDE, CODEX])
    );
    crate::offload::set_chosen_helper(&svc, Some(LAPTOP)).unwrap();
    assert_eq!(
        summary_order(&svc.store()).unwrap().0,
        order(&[OrderStep::Computer(LAPTOP), CLAUDE, CODEX])
    );
    crate::offload::set_chosen_helper(&svc, None).unwrap();
    assert_eq!(
        summary_order(&svc.store()).unwrap().0,
        order(&[CLAUDE, CODEX])
    );
    // No summary keeps a computer (an older build read writer none with a helper the same way).
    crate::offload::set_chosen_helper(&svc, Some(DESK)).unwrap();
    let mut db = svc.store();
    set_summary_settings(
        &mut db,
        &SummarySettings {
            writer: SummaryWriter::NoSummary,
            ..SummarySettings::default()
        },
    )
    .unwrap();
    assert_eq!(
        summary_order(&db).unwrap().0,
        order(&[OrderStep::Computer(DESK)])
    );
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("none"));
    assert_eq!(crate::offload::chosen_helper(&db).unwrap(), Some(DESK));
}

#[test]
fn summary_order_kept_local_filter_drops_remote_steps_and_ends_with_the_local_model() {
    let o = order(&[CLAUDE, OrderStep::Computer(DESK), ENDPOINT]);
    assert_eq!(
        o.for_meeting(SummaryPrivacy::KeptLocal),
        vec![
            (CLAUDE, true),
            (OrderStep::Computer(DESK), false),
            (ENDPOINT, true),
            (LOCAL, false)
        ]
    );
    assert_eq!(
        o.filtered(SummaryPrivacy::KeptLocal),
        vec![OrderStep::Computer(DESK), LOCAL]
    );
    // The order's own local step is not added twice.
    let with_local = order(&[CODEX, LOCAL]);
    assert_eq!(with_local.filtered(SummaryPrivacy::KeptLocal), vec![LOCAL]);
    // Not for a meeting that is not kept, and never for no summary.
    assert_eq!(o.filtered(SummaryPrivacy::Writer), o.steps().to_vec());
    assert!(
        SummaryOrder::none()
            .for_meeting(SummaryPrivacy::KeptLocal)
            .is_empty()
    );
}

#[test]
fn summary_order_live_summaries_and_the_hold_follow_the_filtered_order() {
    assert!(live_summaries_allowed(&[LOCAL, CLAUDE]));
    assert!(live_summaries_allowed(&[OrderStep::Computer(DESK), LOCAL]));
    assert!(!live_summaries_allowed(&[CLAUDE, LOCAL]));
    assert!(!live_summaries_allowed(&[
        OrderStep::Computer(DESK),
        CLAUDE,
        LOCAL
    ]));
    assert!(!live_summaries_allowed(&[]));
    assert!(any_local(&[CLAUDE, LOCAL]));
    assert!(!any_local(&[CLAUDE, OrderStep::Computer(DESK)]));
    assert!(order(&[LOCAL, CLAUDE]).first_is_local());
    assert!(!order(&[OrderStep::Computer(DESK), LOCAL]).first_is_local());
}

#[test]
fn summary_order_hold_is_local_when_any_step_is_local() {
    use crate::meeting_queue::SummaryHold;
    let dir = TempDir::new("summary-order-hold");
    let (svc, _) = services(&dir);
    let db = svc.store();
    let hw = crate::hardware();
    let cpu = [crate::backends::Accelerator::Cpu];
    assert_eq!(
        crate::host::order_summary_hold(&db, &[CLAUDE, ENDPOINT], &cpu, &hw),
        SummaryHold::OffThisComputer
    );
    for steps in [
        &[CLAUDE, LOCAL][..],
        &[LOCAL][..],
        &[OrderStep::Computer(DESK), LOCAL][..],
    ] {
        assert!(
            matches!(
                crate::host::order_summary_hold(&db, steps, &cpu, &hw),
                SummaryHold::Local { .. }
            ),
            "{steps:?}"
        );
    }
}

#[test]
fn summary_order_benchmark_runs_the_local_step_when_it_is_the_first_writer_here() {
    use crate::benchmark::{BenchSkip, BenchSummary, prepare_bench_summary};
    let dir = TempDir::new("summary-order-bench");
    let (svc, _) = services(&dir);
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let read = |svc: &Services| {
        crate::host::summary_from_settings(
            &svc.store(),
            &[crate::backends::Accelerator::Cpu],
            &crate::hardware(),
            &crate::summary_settings::NoApiKeys,
            SummaryPrivacy::Writer,
        )
        .unwrap()
    };
    set_summary_order(
        &mut svc.store(),
        &order(&[OrderStep::Computer(DESK), LOCAL]),
        None,
    )
    .unwrap();
    // A computer first is skipped: the local model is the first writer here, so it is
    // benchmarked as before (here not installed, which the benchmark says as such).
    assert!(matches!(
        prepare_bench_summary(&read(&svc), &models, svc.clock.clone(), &[]).unwrap(),
        BenchSummary::Skipped(BenchSkip::NotInstalled { .. })
    ));
    set_summary_order(&mut svc.store(), &order(&[CLAUDE, LOCAL]), None).unwrap();
    assert!(matches!(
        prepare_bench_summary(&read(&svc), &models, svc.clock.clone(), &[]).unwrap(),
        BenchSummary::Skipped(BenchSkip::NotOnThisComputer { .. })
    ));
    set_summary_order(&mut svc.store(), &order(&[LOCAL, CLAUDE]), None).unwrap();
    let s = read(&svc);
    assert!(matches!(
        prepare_bench_summary(&s, &models, svc.clock.clone(), &[]).unwrap(),
        BenchSummary::Skipped(BenchSkip::NotInstalled { .. })
    ));
}

#[test]
fn summary_order_writer_kind_is_the_first_writer_step() {
    use crate::offload::WriterKind;
    let dir = TempDir::new("summary-order-kind");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    set_summary_order(
        &mut db,
        &order(&[OrderStep::Computer(DESK), CODEX, LOCAL]),
        None,
    )
    .unwrap();
    assert_eq!(WriterKind::of_order(&db).unwrap(), Some(WriterKind::Codex));
    set_summary_order(&mut db, &order(&[OrderStep::Computer(DESK)]), None).unwrap();
    assert_eq!(WriterKind::of_order(&db).unwrap(), None);
    set_summary_order(&mut db, &order(&[ENDPOINT]), Some(&with_endpoint())).unwrap();
    assert_eq!(
        WriterKind::of_order(&db).unwrap(),
        Some(WriterKind::OpenaiCompatible)
    );
}

#[test]
fn summary_order_choosing_a_helper_keeps_a_writer_word_this_build_does_not_know() {
    let dir = TempDir::new("summary-order-unknown-writer");
    let (svc, _) = services(&dir);
    crate::local::set(
        &mut svc.store(),
        &DeviceRecord::SummaryWriter.key(),
        b"a-writer-from-a-newer-build",
    )
    .unwrap();
    crate::offload::set_chosen_helper(&svc, Some(DESK)).unwrap();
    let db = svc.store();
    assert_eq!(
        raw(&db, SUMMARY_WRITER_KEY).as_deref(),
        Some("a-writer-from-a-newer-build"),
        "never replaced by the default"
    );
    assert_eq!(crate::offload::chosen_helper(&db).unwrap(), Some(DESK));
    assert_eq!(
        summary_order(&db).unwrap().0,
        order(&[OrderStep::Computer(DESK), LOCAL])
    );
}

#[test]
fn summary_order_choosing_a_helper_keeps_an_endpoint_writer_without_its_fields() {
    let dir = TempDir::new("summary-order-empty-endpoint");
    let (svc, _) = services(&dir);
    crate::local::set(
        &mut svc.store(),
        &DeviceRecord::SummaryWriter.key(),
        b"endpoint",
    )
    .unwrap();
    crate::offload::set_chosen_helper(&svc, Some(DESK)).unwrap();
    crate::offload::set_chosen_helper(&svc, Some(LAPTOP)).unwrap();
    let db = svc.store();
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("endpoint"));
    assert_eq!(crate::offload::chosen_helper(&db).unwrap(), Some(LAPTOP));
    drop(db);
    crate::offload::set_chosen_helper(&svc, None).unwrap();
    assert_eq!(crate::offload::chosen_helper(&svc.store()).unwrap(), None);
}
