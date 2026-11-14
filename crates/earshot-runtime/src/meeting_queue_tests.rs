//! The meeting queue (T9.1l) and when a newer meeting's live decoding runs beside an older
//! meeting's summary (T12.39). Every name contains `meeting_queue`.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::summary::{SummaryConfig, SummaryService};

const GIB: u64 = 1024 * 1024 * 1024;
const GUARD: Duration = Duration::from_secs(60);

#[test]
fn meeting_queue_tickets_take_turns_in_start_order() {
    let q = MeetingQueue::new();
    let cancel = CancelToken::new();
    let a = q.enter();
    let b = q.enter();
    let c = q.enter();
    assert!(!a.behind_older_work());
    assert!(a.wait_turn(&cancel));
    assert!(b.behind_older_work());

    // b's turn comes only when a has ended, c's only when both have.
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = std::thread::spawn({
        let cancel = cancel.clone();
        move || {
            let went = b.wait_turn(&cancel);
            tx.send(went).unwrap();
            drop(b);
        }
    });
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    drop(a);
    assert!(rx.recv_timeout(Duration::from_secs(60)).unwrap());
    waiting.join().unwrap();
    assert!(c.wait_turn(&cancel));
    assert!(!c.behind_older_work());
}

#[test]
fn meeting_queue_speaker_work_waits_only_for_older_final_passes() {
    let q = MeetingQueue::new();
    let a = q.enter();
    let b = q.enter();
    let c = q.enter();
    let stop = CancelToken::new();
    stop.cancel();
    assert!(!c.wait_final_passes(&stop), "a's final pass still runs");
    let go = CancelToken::new();
    a.final_pass_done();
    assert!(
        !c.wait_final_passes(&stop),
        "b's final pass is still to run"
    );
    b.final_pass_done();
    assert!(c.wait_final_passes(&go));
    assert!(!c.wait_turn(&stop), "their summaries still run");
}

#[test]
fn meeting_queue_a_cancelled_wait_returns_and_a_panic_ends_the_ticket() {
    let q = MeetingQueue::new();
    let older = q.enter();
    let newer = q.enter();
    let cancel = CancelToken::new();
    let waiter = std::thread::spawn({
        let cancel = cancel.clone();
        move || newer.wait_turn(&cancel)
    });
    cancel.cancel();
    assert!(!waiter.join().unwrap());
    // `newer` ended with the thread, and a ticket lost in a panic ends too.
    let panicked = std::thread::spawn(move || {
        let _ticket = older;
        panic!("a post-meeting bug");
    });
    assert!(panicked.join().is_err());
    assert!(!q.enter().behind_older_work());
}

#[test]
fn meeting_queue_an_ended_ticket_lets_the_next_go_while_a_stuck_thread_holds_it() {
    // T9.22: a thread given up (a model load that never returns) keeps its handle; ending the
    // ticket still gives the next meeting its turn.
    let q = MeetingQueue::new();
    let stuck = Arc::new(q.enter());
    let held_by_the_stuck_thread = Arc::clone(&stuck);
    let next = q.enter();
    let stop = CancelToken::new();
    stop.cancel();
    assert!(!next.wait_turn(&stop), "the older meeting still holds it");
    stuck.end();
    drop(stuck);
    assert!(next.wait_turn(&CancelToken::new()));
    assert!(next.wait_final_passes(&CancelToken::new()));
    // Ending it again, or the stuck thread letting go at last, changes nothing.
    held_by_the_stuck_thread.end();
    drop(held_by_the_stuck_thread);
    let after = q.enter();
    assert!(after.behind_older_work(), "`next` is still open");
    drop(next);
    assert!(!after.behind_older_work());
}

// T12.39: the rule.

/// A machine with `memory` of reported physical memory, a discrete card of `card` GiB, and `pairs`
/// switched on.
fn machine(memory: Option<u64>, card: Option<u64>, pairs: &[(Device, Device)]) -> Machine {
    Machine {
        hardware: Hardware {
            gpus: card
                .map(|gib| earshot_hardware::Gpu {
                    name: "a card".into(),
                    vendor_id: 0x10de,
                    dedicated_memory: Some(gib * GIB),
                    driver_version: None,
                    integrated: Some(false),
                    compute_capability: None,
                })
                .into_iter()
                .collect(),
            total_memory: memory,
            ..Hardware::default()
        },
        pairs: pairs.to_vec(),
        card_summary_fell_back: false,
    }
}

fn no_fallback() -> bool {
    false
}

/// Every pair, as if every measurement had passed.
const ALL_PAIRS: &[(Device, Device)] = &[
    (Device::Cpu, Device::Cpu),
    (Device::Cpu, Device::GraphicsCard),
    (Device::GraphicsCard, Device::Cpu),
    (Device::GraphicsCard, Device::GraphicsCard),
    (Device::Npu, Device::Cpu),
];

fn live(device: Device) -> LiveNeeds {
    LiveNeeds {
        device,
        model_size: 2_500_000_000,
        live_summaries: false,
    }
}

fn local(device: Device, size: u64) -> SummaryHold {
    SummaryHold::Local { device, size }
}

const NINE_B: u64 = 5_680_522_464;
const FOUR_B: u64 = 2_740_937_888;

#[test]
fn meeting_queue_a_summary_off_this_computer_never_holds_and_an_unrecorded_one_always_does() {
    // Nothing switched on, no memory known: (a) holds whatever the machine.
    let bare = machine(None, None, &[]);
    for device in [Device::Cpu, Device::GraphicsCard, Device::Npu] {
        assert!(may_run_beside(
            SummaryHold::OffThisComputer,
            &live(device),
            &bare
        ));
        let with_live_summaries = LiveNeeds {
            live_summaries: true,
            ..live(device)
        };
        assert!(
            may_run_beside(SummaryHold::OffThisComputer, &with_live_summaries, &bare),
            "no summary model here, so live summaries can run too"
        );
        let roomy = machine(Some(64 * GIB), Some(48), ALL_PAIRS);
        assert!(
            !may_run_beside(SummaryHold::Unknown, &live(device), &roomy),
            "an unrecorded summary holds as before"
        );
    }
}

#[test]
fn meeting_queue_another_device_runs_beside_only_when_its_pair_is_on_and_memory_holds_both() {
    let on = machine(Some(15 * GIB), None, &[(Device::Npu, Device::Cpu)]);
    let summary = local(Device::Cpu, FOUR_B);
    assert!(may_run_beside(summary, &live(Device::Npu), &on));
    // The pair is not switched on (the measurement has not passed).
    let off = machine(Some(15 * GIB), None, &[]);
    assert!(!may_run_beside(summary, &live(Device::Npu), &off));
    // Another pair is on, not this one.
    let other = machine(Some(15 * GIB), None, &[(Device::Cpu, Device::GraphicsCard)]);
    assert!(!may_run_beside(summary, &live(Device::Npu), &other));
    // Memory: under the limit, or unknown, holds; exactly the limit passes.
    for memory in [Some(BESIDE_MIN_MEMORY - 1), Some(8 * GIB), None] {
        let small = machine(memory, None, &[(Device::Npu, Device::Cpu)]);
        assert!(
            !may_run_beside(summary, &live(Device::Npu), &small),
            "{memory:?}"
        );
    }
    let exact = machine(Some(BESIDE_MIN_MEMORY), None, &[(Device::Npu, Device::Cpu)]);
    assert!(may_run_beside(summary, &live(Device::Npu), &exact));
    // The desktop's other pair: the summary on the card, transcription on the processor.
    let desktop = machine(
        Some(64 * GIB),
        Some(16),
        &[(Device::Cpu, Device::GraphicsCard)],
    );
    assert!(may_run_beside(
        local(Device::GraphicsCard, NINE_B),
        &live(Device::Cpu),
        &desktop
    ));
    assert!(
        !may_run_beside(
            local(Device::Cpu, NINE_B),
            &live(Device::GraphicsCard),
            &desktop
        ),
        "the reverse pair is its own measurement"
    );
}

#[test]
fn meeting_queue_the_same_graphics_card_runs_beside_only_with_room_for_both_models() {
    let pairs = &[(Device::GraphicsCard, Device::GraphicsCard)];
    // 2 x (5.7 + 2.5) GB is 15.3 GiB: a 16 GiB card has room, a 12 GiB one has not.
    let big = machine(Some(64 * GIB), Some(16), pairs);
    let small = machine(Some(64 * GIB), Some(12), pairs);
    let summary = local(Device::GraphicsCard, NINE_B);
    assert!(may_run_beside(summary, &live(Device::GraphicsCard), &big));
    assert!(!may_run_beside(
        summary,
        &live(Device::GraphicsCard),
        &small
    ));
    // The rule is on both models together: each fits the small card alone.
    assert!(crate::model_settings::fits_graphics_card(
        NINE_B,
        &small.hardware
    ));
    assert!(crate::model_settings::fits_graphics_card(
        live(Device::GraphicsCard).model_size,
        &small.hardware
    ));
    // The 4B with the same transcription model fits the small card.
    assert!(may_run_beside(
        local(Device::GraphicsCard, FOUR_B),
        &live(Device::GraphicsCard),
        &small
    ));
    // Room on the card is not enough without the pair or without system memory.
    let not_on = machine(Some(64 * GIB), Some(16), &[]);
    assert!(!may_run_beside(
        summary,
        &live(Device::GraphicsCard),
        &not_on
    ));
    let little_memory = machine(Some(8 * GIB), Some(16), pairs);
    assert!(!may_run_beside(
        summary,
        &live(Device::GraphicsCard),
        &little_memory
    ));
}

#[test]
fn meeting_queue_two_on_the_processor_and_live_summaries_beside_a_local_summary_always_hold() {
    let roomy = machine(Some(64 * GIB), Some(48), ALL_PAIRS);
    // Both on the CPU: never, even with the pair listed.
    assert!(!may_run_beside(
        local(Device::Cpu, FOUR_B),
        &live(Device::Cpu),
        &roomy
    ));
    // Live summaries are a summary model: never two at once, whatever the devices.
    for (live_device, summary_device) in [
        (Device::Npu, Device::Cpu),
        (Device::Cpu, Device::GraphicsCard),
        (Device::GraphicsCard, Device::GraphicsCard),
    ] {
        let needs = LiveNeeds {
            live_summaries: true,
            ..live(live_device)
        };
        assert!(
            may_run_beside(local(summary_device, FOUR_B), &live(live_device), &roomy),
            "{live_device:?} beside {summary_device:?} without live summaries"
        );
        assert!(
            !may_run_beside(local(summary_device, FOUR_B), &needs, &roomy),
            "{live_device:?} beside {summary_device:?} with live summaries"
        );
    }
}

#[test]
fn meeting_queue_this_machine_switches_on_only_measured_pairs() {
    // No pair of two work on the processor, and none on ARM64 until the Surface measures.
    assert!(!BESIDE_PAIRS.contains(&(Device::Cpu, Device::Cpu)));
    if cfg!(not(target_arch = "x86_64")) {
        assert!(BESIDE_PAIRS.is_empty());
    }
    assert_eq!(Machine::this_one().pairs, BESIDE_PAIRS.to_vec());
}

#[test]
fn meeting_queue_a_summary_holds_what_its_prepared_summary_runs_on() {
    assert_eq!(SummaryHold::of(None), SummaryHold::OffThisComputer);
    let clock: crate::SharedClock =
        Arc::new(earshot_core::identity::ManualClock::new(1_700_000_000_000));
    let remote = PreparedSummary {
        service: SummaryService::new(
            &SummaryConfig::new("http://127.0.0.1:9/v1", "m"),
            Arc::clone(&clock),
        )
        .unwrap(),
        local: None,
    };
    assert_eq!(SummaryHold::of(Some(&remote)), SummaryHold::OffThisComputer);
    for gpu in [false, true] {
        let choice = crate::summary_model::choose_summary_model(None, Some(8 * GIB), gpu).unwrap();
        let mut config = crate::summary::LocalSummaryConfig::new("unused.gguf", choice.model.id);
        config.chunk_seconds = 600;
        let prepared = PreparedSummary {
            service: SummaryService::local(&config).unwrap(),
            local: Some((choice, gpu)),
        };
        assert_eq!(
            SummaryHold::of(Some(&prepared)),
            SummaryHold::Local {
                device: if gpu {
                    Device::GraphicsCard
                } else {
                    Device::Cpu
                },
                size: choice.model.size,
            }
        );
    }
    assert_eq!(Device::of(Accelerator::Cpu), Device::Cpu);
    assert_eq!(Device::of(Accelerator::Cuda), Device::GraphicsCard);
    assert_eq!(Device::of(Accelerator::Vulkan), Device::GraphicsCard);
    assert_eq!(Device::of(Accelerator::Qnn), Device::Npu);
}

// T12.39: the queue.

/// Runs `wait` on a thread; the receiver hears whether it went.
fn waiting(wait: impl FnOnce() -> bool + Send + 'static) -> std::sync::mpsc::Receiver<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait());
    });
    rx
}

fn still_waiting(rx: &std::sync::mpsc::Receiver<bool>) {
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "it went"
    );
}

fn went(rx: &std::sync::mpsc::Receiver<bool>) {
    assert!(rx.recv_timeout(GUARD).expect("it never went"));
}

#[test]
fn meeting_queue_live_decoding_beside_a_summary_off_this_computer_waits_only_for_the_final_pass() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(SummaryHold::OffThisComputer);
    let newer = Arc::new(q.enter());
    let m = machine(None, None, &[]);
    let rx = waiting({
        let newer = Arc::clone(&newer);
        let m = m.clone();
        move || newer.wait_live_turn(&live(Device::Cpu), &m, &CancelToken::new())
    });
    // The older meeting's final pass still runs on the transcription device.
    still_waiting(&rx);
    assert!(newer.live_held(&live(Device::Cpu), &m));
    older.final_pass_done();
    // Its summary goes on, written elsewhere: the newer transcript starts now.
    went(&rx);
    assert!(!newer.live_held(&live(Device::Cpu), &m));
    // The newer meeting's own final pass and summary still wait for the older ticket.
    let stop = CancelToken::new();
    stop.cancel();
    assert!(!newer.wait_turn(&stop));
    drop(older);
    assert!(newer.wait_turn(&CancelToken::new()));
}

#[test]
fn meeting_queue_live_decoding_on_the_summarys_device_waits_for_the_older_ticket_to_end() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(local(Device::Cpu, FOUR_B));
    older.final_pass_done();
    let newer = Arc::new(q.enter());
    let m = machine(Some(64 * GIB), None, ALL_PAIRS);
    let rx = waiting({
        let newer = Arc::clone(&newer);
        let m = m.clone();
        move || newer.wait_live_turn(&live(Device::Cpu), &m, &CancelToken::new())
    });
    still_waiting(&rx);
    assert!(newer.live_held(&live(Device::Cpu), &m));
    older.end();
    went(&rx);
}

#[test]
fn meeting_queue_live_decoding_on_another_device_goes_with_its_pair_on_and_waits_without_it() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(local(Device::Cpu, FOUR_B));
    older.final_pass_done();
    let newer = q.enter();
    let on = machine(Some(15 * GIB), None, &[(Device::Npu, Device::Cpu)]);
    assert!(newer.wait_live_turn(&live(Device::Npu), &on, &CancelToken::new()));
    let off = machine(Some(15 * GIB), None, &[]);
    let stop = CancelToken::new();
    stop.cancel();
    assert!(!newer.wait_live_turn(&live(Device::Npu), &off, &stop));
    assert!(newer.live_held(&live(Device::Npu), &off));
}

#[test]
fn meeting_queue_every_older_ticket_counts_and_an_unrecorded_one_holds_as_before() {
    let q = MeetingQueue::new();
    let first = q.enter();
    first.set_summary(SummaryHold::OffThisComputer);
    first.final_pass_done();
    // The second records nothing (a host that does not say): it holds until it ends.
    let second = q.enter();
    second.final_pass_done();
    let newer = Arc::new(q.enter());
    let m = machine(Some(64 * GIB), Some(16), ALL_PAIRS);
    let rx = waiting({
        let newer = Arc::clone(&newer);
        let m = m.clone();
        move || newer.wait_live_turn(&live(Device::Npu), &m, &CancelToken::new())
    });
    still_waiting(&rx);
    // Recording it later lets the newer meeting go.
    second.set_summary(local(Device::Cpu, FOUR_B));
    went(&rx);
}

/// Needs a test sets by hand: `None` until known.
fn needs_of(slot: &Arc<Mutex<Option<LiveNeeds>>>) -> NeedsFn {
    let slot = Arc::clone(slot);
    Box::new(move || *slot.lock().unwrap())
}

#[test]
fn meeting_queue_the_hold_says_previous_meeting_only_while_the_transcript_waits_for_it() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(local(Device::Cpu, FOUR_B));
    let needs = Arc::new(Mutex::new(Some(live(Device::Npu))));
    let hold = Arc::new(MeetingHold::beside(
        Arc::new(q.enter()),
        Beside {
            needs: needs_of(&needs),
            machine: machine(Some(15 * GIB), None, &[(Device::Npu, Device::Cpu)]),
            card_summary_fell_back: no_fallback,
        },
    ));
    // Its final pass still runs: held, and the banner says so.
    assert_eq!(hold.reason(), Some(HoldReason::PreviousMeeting));
    let started = Arc::new(AtomicBool::new(false));
    let rx = waiting({
        let (hold, started) = (Arc::clone(&hold), Arc::clone(&started));
        move || {
            let went = hold.wait_transcription(&CancelToken::new());
            started.store(true, Ordering::SeqCst);
            went
        }
    });
    still_waiting(&rx);
    older.final_pass_done();
    // The summary runs on the CPU, the transcript on the NPU: no banner, and it starts.
    went(&rx);
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(hold.reason(), None);
    // The answer is kept: the banner and the wait never read two different needs.
    *needs.lock().unwrap() = Some(live(Device::Cpu));
    assert_eq!(hold.reason(), None);
    drop(older);
    assert_eq!(hold.reason(), None);
    assert!(hold.wait_transcription(&CancelToken::new()));
}

#[test]
fn meeting_queue_needs_unknown_at_the_banner_are_decided_once_known_and_then_kept() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(local(Device::Cpu, FOUR_B));
    older.final_pass_done();
    let needs = Arc::new(Mutex::new(None));
    let hold = MeetingHold::beside(
        Arc::new(q.enter()),
        Beside {
            needs: needs_of(&needs),
            machine: machine(Some(15 * GIB), None, &[(Device::Npu, Device::Cpu)]),
            card_summary_fell_back: no_fallback,
        },
    );
    // Not known yet (the CUDA start): as before, any older meeting holds.
    assert_eq!(hold.reason(), Some(HoldReason::PreviousMeeting));
    *needs.lock().unwrap() = Some(live(Device::Cpu));
    // Known: on the summary's own device it waits, and the banner says so.
    assert_eq!(hold.reason(), Some(HoldReason::PreviousMeeting));
    *needs.lock().unwrap() = Some(live(Device::Npu));
    assert_eq!(hold.reason(), Some(HoldReason::PreviousMeeting), "kept");
    let stop = CancelToken::new();
    stop.cancel();
    assert!(!hold.wait_transcription(&stop));
    drop(older);
    assert_eq!(hold.reason(), None);
}

#[test]
fn meeting_queue_a_hold_whose_needs_are_unknown_waits_for_every_older_ticket() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(SummaryHold::OffThisComputer);
    older.final_pass_done();
    let unknown = Arc::new(Mutex::new(None));
    let hold = Arc::new(MeetingHold::beside(
        Arc::new(q.enter()),
        Beside {
            needs: needs_of(&unknown),
            machine: machine(Some(64 * GIB), Some(16), ALL_PAIRS),
            card_summary_fell_back: no_fallback,
        },
    ));
    let rx = waiting({
        let hold = Arc::clone(&hold);
        move || hold.wait_transcription(&CancelToken::new())
    });
    still_waiting(&rx);
    drop(older);
    went(&rx);
    // A plain hold (no `beside`) is the T9.1l rule.
    let older = q.enter();
    older.set_summary(SummaryHold::OffThisComputer);
    older.final_pass_done();
    let plain = MeetingHold::new(Arc::new(q.enter()));
    assert_eq!(plain.reason(), Some(HoldReason::PreviousMeeting));
    let stop = CancelToken::new();
    stop.cancel();
    assert!(!plain.wait_transcription(&stop));
}

// T12.39: fallbacks end a beside decision.

static CARD_FELL_BACK: AtomicBool = AtomicBool::new(false);

fn card_fell_back() -> bool {
    CARD_FELL_BACK.load(Ordering::SeqCst)
}

#[test]
fn meeting_queue_after_a_card_summary_fell_back_card_summaries_count_as_on_the_processor() {
    let mut desktop = machine(
        Some(64 * GIB),
        Some(16),
        &[(Device::Cpu, Device::GraphicsCard)],
    );
    let summary = local(Device::GraphicsCard, NINE_B);
    assert!(may_run_beside(summary, &live(Device::Cpu), &desktop));
    desktop.card_summary_fell_back = true;
    assert!(
        !may_run_beside(summary, &live(Device::Cpu), &desktop),
        "the summary may be on the CPU now"
    );
    // A summary off this computer is unaffected; a CPU summary beside the NPU too.
    assert!(may_run_beside(
        SummaryHold::OffThisComputer,
        &live(Device::Cpu),
        &desktop
    ));
    let surface = Machine {
        card_summary_fell_back: true,
        ..machine(Some(15 * GIB), None, &[(Device::Npu, Device::Cpu)])
    };
    assert!(may_run_beside(
        local(Device::Cpu, FOUR_B),
        &live(Device::Npu),
        &surface
    ));
}

#[test]
fn meeting_queue_the_hold_reads_a_card_fallback_at_each_decision() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(local(Device::GraphicsCard, NINE_B));
    older.final_pass_done();
    let hold = MeetingHold::beside(
        Arc::new(q.enter()),
        Beside {
            needs: Box::new(|| Some(live(Device::Cpu))),
            machine: machine(
                Some(64 * GIB),
                Some(16),
                &[(Device::Cpu, Device::GraphicsCard)],
            ),
            card_summary_fell_back: card_fell_back,
        },
    );
    CARD_FELL_BACK.store(false, Ordering::SeqCst);
    assert_eq!(hold.reason(), None);
    CARD_FELL_BACK.store(true, Ordering::SeqCst);
    assert_eq!(hold.reason(), Some(HoldReason::PreviousMeeting));
    let stop = CancelToken::new();
    stop.cancel();
    assert!(!hold.wait_transcription(&stop));
    drop(older);
    assert!(hold.wait_transcription(&CancelToken::new()));
}

#[test]
fn meeting_queue_a_live_load_that_fell_back_onto_the_summarys_device_waits_again() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(local(Device::Cpu, FOUR_B));
    older.final_pass_done();
    let hold = Arc::new(MeetingHold::beside(
        Arc::new(q.enter()),
        Beside {
            needs: Box::new(|| Some(live(Device::Npu))),
            machine: machine(Some(15 * GIB), None, &[(Device::Npu, Device::Cpu)]),
            card_summary_fell_back: no_fallback,
        },
    ));
    // Planned on the NPU, beside the CPU summary: it goes.
    assert!(hold.wait_transcription(&CancelToken::new()));
    // The load landed where it was planned: nothing to wait for.
    assert!(hold.recheck_after_load(Device::Npu, &CancelToken::new()));
    // It fell back to the CPU, the summary's device: it waits until the older ticket ends, and
    // the banner says so.
    let rx = waiting({
        let hold = Arc::clone(&hold);
        move || hold.recheck_after_load(Device::Cpu, &CancelToken::new())
    });
    still_waiting(&rx);
    assert_eq!(hold.reason(), Some(HoldReason::PreviousMeeting));
    drop(older);
    went(&rx);
    // A plain hold has nothing to re-check.
    let plain = MeetingHold::new(Arc::new(q.enter()));
    assert!(plain.recheck_after_load(Device::Cpu, &CancelToken::new()));
}

#[test]
fn meeting_queue_a_live_load_that_fell_back_off_the_summarys_device_goes_on() {
    let q = MeetingQueue::new();
    let older = q.enter();
    older.set_summary(local(Device::GraphicsCard, NINE_B));
    older.final_pass_done();
    let hold = MeetingHold::beside(
        Arc::new(q.enter()),
        Beside {
            needs: Box::new(|| Some(live(Device::GraphicsCard))),
            machine: machine(Some(64 * GIB), Some(16), ALL_PAIRS),
            card_summary_fell_back: no_fallback,
        },
    );
    assert!(hold.wait_transcription(&CancelToken::new()));
    // CUDA refused, the track on the CPU, the summary on the card: a measured pair, it goes on.
    assert!(hold.recheck_after_load(Device::Cpu, &CancelToken::new()));
}
