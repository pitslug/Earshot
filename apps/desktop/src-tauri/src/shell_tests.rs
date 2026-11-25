//! Tray, shortcut and notification rules. Every name contains `shell`.

use std::ffi::OsString;

use super::*;
use crate::meeting_events::{CaptureMode, EndedReason, LiveTrack};

fn started() -> MeetingEvent {
    MeetingEvent::Started {
        meeting: "m1".into(),
        mode: CaptureMode::Call,
        tracks: vec![LiveTrack::You, LiveTrack::Them],
    }
}

fn ended() -> MeetingEvent {
    MeetingEvent::Ended {
        meeting: "m1".into(),
        reason: EndedReason::Stopped,
    }
}

fn stored(voices_to_name: u32) -> SummaryEvent {
    SummaryEvent::Stored {
        title: "Budget with Sam".into(),
        actions: 3,
        decisions: 1,
        voices_to_name,
    }
}

#[test]
fn shell_only_the_autostart_argument_means_started_by_windows() {
    assert!(!started_by_windows(&[]));
    assert!(!started_by_windows(&[OsString::from("--other")]));
    assert!(started_by_windows(&[OsString::from(AUTOSTART_ARG)]));
    assert!(started_by_windows(&[
        OsString::from("--other"),
        OsString::from(AUTOSTART_ARG)
    ]));
}

#[test]
fn shell_phase_follows_a_meeting_from_the_tray_to_its_summary() {
    let mut t = PhaseTracker::default();
    assert_eq!(t.phase(), Phase::Idle);
    assert!(t.begin_start());
    assert!(!t.begin_start(), "one start at a time");
    assert_eq!(t.phase(), Phase::Starting);
    assert!(t.apply(&started()));
    assert_eq!(t.phase(), Phase::Recording);
    assert_eq!(t.meeting(), Some("m1"));
    assert!(!t.begin_start(), "a meeting is on");
    // Anything else keeps the phase.
    assert!(!t.apply(&MeetingEvent::Words { total: 4 }));
    assert_eq!(t.phase(), Phase::Recording);
    assert!(t.apply(&ended()));
    assert_eq!(t.phase(), Phase::Finishing);
    assert!(t.apply(&MeetingEvent::Summary {
        meeting: "m1".into(),
        result: stored(0)
    }));
    assert_eq!(t.phase(), Phase::Idle);
    assert_eq!(t.meeting(), Some("m1"), "a notification still opens it");
}

#[test]
fn meeting_queue_shell_a_new_meeting_starts_while_the_last_one_finishes() {
    let mut t = PhaseTracker::default();
    t.apply(&started());
    t.apply(&ended());
    assert_eq!(t.phase(), Phase::Finishing);
    assert!(t.begin_start(), "finishing keeps nothing out");
    assert!(!t.begin_start(), "one start at a time");
    // A start that fails goes back to what the last meeting was doing.
    t.start_failed();
    assert_eq!(t.phase(), Phase::Finishing);
    assert!(t.begin_start());
    // The older meeting ends while the start runs: the phase is the start's, and a failure now
    // finds nothing running.
    assert!(!t.apply(&MeetingEvent::Summary {
        meeting: "m1".into(),
        result: stored(0)
    }));
    assert_eq!(t.phase(), Phase::Starting);
    t.start_failed();
    assert_eq!(t.phase(), Phase::Idle);

    // The older meeting's events change nothing for the one recording.
    t.apply(&started());
    t.apply(&ended());
    t.apply(&MeetingEvent::Started {
        meeting: "m2".into(),
        mode: CaptureMode::Call,
        tracks: vec![LiveTrack::You],
    });
    assert_eq!(t.meeting(), Some("m2"));
    assert!(!t.apply(&MeetingEvent::Summary {
        meeting: "m1".into(),
        result: stored(0)
    }));
    assert_eq!(t.phase(), Phase::Recording);
}

#[test]
fn shell_phase_follows_a_meeting_started_from_the_window_too() {
    let mut t = PhaseTracker::default();
    assert!(t.apply(&started()));
    assert_eq!(t.phase(), Phase::Recording);
}

#[test]
fn shell_a_failed_start_returns_to_idle_but_never_undoes_a_started_meeting() {
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    t.start_failed();
    assert_eq!(t.phase(), Phase::Idle);
    assert!(t.begin_start());
    t.apply(&started());
    t.start_failed();
    assert_eq!(t.phase(), Phase::Recording);
}

#[test]
fn shell_tray_shows_the_dot_only_while_recording_and_offers_only_what_can_be_done() {
    let idle = tray_view(Phase::Idle);
    assert!(!idle.recording && idle.toggle_enabled);
    assert_eq!(idle.toggle_label, "Start meeting");
    let rec = tray_view(Phase::Recording);
    assert!(rec.recording && rec.toggle_enabled);
    assert_eq!(rec.toggle_label, "Stop meeting");
    let starting = tray_view(Phase::Starting);
    assert!(!starting.recording && !starting.toggle_enabled);
    // A meeting finishing keeps nothing out (T9.1l).
    let finishing = tray_view(Phase::Finishing);
    assert!(!finishing.recording && finishing.toggle_enabled);
    assert_eq!(finishing.toggle_label, "Start meeting");
}

#[test]
fn shell_toggle_and_quit_follow_the_driver() {
    assert_eq!(toggle_action(DriverPhase::Idle, false), ToggleAction::Start);
    assert_eq!(
        toggle_action(DriverPhase::Recording, false),
        ToggleAction::Stop
    );
    assert_eq!(
        toggle_action(DriverPhase::Processing, false),
        ToggleAction::Start
    );
    // T14.6: a meeting that has not said `Started` yet is starting, and a press leaves it alone.
    assert_eq!(
        toggle_action(DriverPhase::Recording, true),
        ToggleAction::Nothing
    );
    assert_eq!(quit_action(DriverPhase::Idle), QuitAction::Quit);
    assert_eq!(
        quit_action(DriverPhase::Recording),
        QuitAction::Ask { recording: true }
    );
    assert_eq!(
        quit_action(DriverPhase::Processing),
        QuitAction::Ask { recording: false }
    );
}

#[test]
fn shell_a_held_shortcut_counts_once() {
    let mut l = KeyLatch::default();
    assert!(l.pressed());
    assert!(!l.pressed(), "a repeat while held");
    assert!(!l.pressed());
    l.released();
    assert!(l.pressed());
}

#[test]
fn shell_summary_toast_counts_and_offers_names_only_when_needed() {
    let t = summary_toast(3, 1, 0);
    assert_eq!(t.title, "Summary ready");
    assert_eq!(t.body, "3 actions, 1 decision.");
    assert_eq!(t.buttons, vec![("Open", ToastClick::Meeting)]);
    let t = summary_toast(1, 0, 1);
    assert_eq!(t.body, "1 action, 0 decisions. 1 voice needs a name.");
    assert_eq!(t.buttons[1], ("Name voices", ToastClick::NameVoices));
    assert_eq!(
        summary_toast(0, 2, 2).body,
        "0 actions, 2 decisions. 2 voices need names."
    );
}

#[test]
fn shell_notifications_never_carry_meeting_content() {
    let toast = finished_toast(&stored(2)).unwrap();
    let text = format!("{toast:?}");
    assert!(!text.contains("Budget"), "{text}");
    let failed = finished_toast(&SummaryEvent::Failed {
        message: "Sam's secret reason".into(),
        retryable: false,
    })
    .unwrap();
    assert!(!format!("{failed:?}").contains("Sam"));
    for t in [toast, failed, listening_toast()] {
        let text = format!("{} {}", t.title, t.body);
        assert!(!text.contains('\u{2014}') && !text.contains('!'), "{text}");
    }
}

#[test]
fn shell_a_recording_that_ends_says_so_once_with_fixed_copy() {
    let saved = ended_toast(EndedReason::Stopped, false).unwrap();
    assert_eq!(saved.title, "Recording saved");
    assert_eq!(
        saved.body,
        "Earshot is finishing the meeting in the background."
    );
    assert_eq!(saved.click, ToastClick::Window);
    assert!(saved.buttons.is_empty());
    assert!(!saved.stays);
    assert_eq!(ended_toast(EndedReason::Finished, false), Some(saved));
    let failed = ended_toast(EndedReason::Failed, false).unwrap();
    assert_eq!(failed.title, "Recording stopped");
    assert_eq!(
        failed.body,
        "Something went wrong. What was recorded is kept: open Earshot to see why."
    );
    // The call-end stop already said "Earshot stopped recording": no second notification.
    for reason in [
        EndedReason::Stopped,
        EndedReason::Finished,
        EndedReason::Failed,
    ] {
        assert_eq!(ended_toast(reason, true), None, "{reason:?}");
    }
    for t in [ended_toast(EndedReason::Stopped, false), Some(failed)]
        .into_iter()
        .flatten()
    {
        let text = format!("{} {}", t.title, t.body);
        assert!(!text.contains('\u{2014}') && !text.contains('!'), "{text}");
    }
}

#[test]
fn shell_a_resummary_notifies_like_a_meetings_summary() {
    use crate::jobs::{JobEvent, JobKind};
    let id = "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".to_owned();
    let done = JobEvent::Summary {
        meeting: id.clone(),
        result: stored(2),
        note: None,
    };
    assert_eq!(
        job_toast(&done),
        Some((finished_toast(&stored(2)).unwrap(), id.clone()))
    );
    // A summary that ran and failed, and a re-summarise job that failed before it ran, both say so.
    let failed_summary = JobEvent::Summary {
        meeting: id.clone(),
        result: SummaryEvent::Failed {
            message: "Sam's secret reason".into(),
            retryable: true,
        },
        note: None,
    };
    let failed_job = JobEvent::Failed {
        meeting: id.clone(),
        job: JobKind::Resummarise,
        message: "Sam's secret reason".into(),
        retryable: true,
    };
    for e in [failed_summary, failed_job] {
        let (toast, meeting) = job_toast(&e).unwrap();
        assert_eq!(toast.title, "The summary was not made");
        assert_eq!(meeting, id);
        assert!(!format!("{toast:?}").contains("Sam"));
    }
    // Nothing for a summary nobody asked for, a re-transcription, a job's steps or a job the
    // user cancelled (T12.41).
    for e in [
        JobEvent::Summary {
            meeting: id.clone(),
            result: SummaryEvent::Cancelled,
            note: None,
        },
        JobEvent::Offloaded {
            meeting: id.clone(),
            message: "Waiting for your other computer to write the summary.".into(),
        },
        JobEvent::Cancelled {
            meeting: id.clone(),
            job: JobKind::Resummarise,
        },
        JobEvent::Cancelled {
            meeting: id.clone(),
            job: JobKind::Retranscribe,
        },
        JobEvent::Failed {
            meeting: id.clone(),
            job: JobKind::Retranscribe,
            message: "m".into(),
            retryable: false,
        },
        JobEvent::Warning {
            meeting: id.clone(),
            message: "m".into(),
        },
    ] {
        assert_eq!(job_toast(&e), None, "{e:?}");
    }
}

#[test]
fn shell_no_notification_for_a_summary_nobody_asked_for() {
    assert_eq!(
        finished_toast(&SummaryEvent::Skipped {
            reason: "No summary".into()
        }),
        None
    );
    assert_eq!(finished_toast(&SummaryEvent::Cancelled), None);
    assert_eq!(finished_toast(&SummaryEvent::PostCancelled), None);
}

#[test]
fn shell_toast_clicks_round_trip_through_their_arguments() {
    for click in [
        ToastClick::Window,
        ToastClick::Meeting,
        ToastClick::NameVoices,
        ToastClick::StopRecording,
        ToastClick::KeepRecording,
        ToastClick::StartRecording,
        ToastClick::NotNow,
    ] {
        assert_eq!(ToastClick::from_argument(click.argument()), click);
    }
    assert_eq!(ToastClick::from_argument("other"), ToastClick::Window);
}

#[test]
fn shell_record_dot_marks_the_lower_right_and_leaves_the_rest() {
    let (w, h) = (32_u32, 32_u32);
    let base = vec![10_u8; (w * h * 4) as usize];
    let out = with_record_dot(&base, w, h);
    assert_eq!(out.len(), base.len());
    let pixel = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [out[i], out[i + 1], out[i + 2], out[i + 3]]
    };
    assert_eq!(pixel(23, 23), [0xD1, 0x34, 0x38, 0xFF]);
    assert_eq!(pixel(0, 0), [10, 10, 10, 10]);
    assert_eq!(pixel(31, 0), [10, 10, 10, 10]);
    assert_eq!(pixel(0, 31), [10, 10, 10, 10]);
    // Any size works.
    assert_eq!(with_record_dot(&[], 0, 0), Vec::<u8>::new());
    assert_eq!(with_record_dot(&[1, 2, 3, 4], 1, 1).len(), 4);
}

#[test]
fn shell_call_end_toast_copy_is_the_shared_tables() {
    // T14.52 review: the meeting screen reads the same table, so editing one side fails here.
    let table: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/ui/call_end_copy_cases.json"
    ))
    .unwrap();
    let copy = |key: &str| table[key].as_str().unwrap().to_owned();
    for (stops, body) in [(true, "will_stop"), (false, "asks_only")] {
        let t = call_end_toast(stops);
        assert_eq!(t.title, copy("notification_title"));
        assert_eq!(t.body, copy(body));
        assert_eq!(
            t.buttons
                .iter()
                .map(|(label, _)| (*label).to_owned())
                .collect::<Vec<_>>(),
            [copy("stop_button"), copy("keep_button")]
        );
    }
    assert_eq!(CALL_END_WILL_STOP, copy("will_stop"));
    assert_eq!(CALL_END_ASKS_ONLY, copy("asks_only"));
}

#[test]
fn call_end_toast_is_fixed_copy_that_stays_and_answers_in_one_click() {
    let t = call_end_toast(true);
    assert_eq!(t.title, "The call seems to have ended");
    assert_eq!(
        t.body,
        "Earshot will stop recording within 3 minutes unless you keep it going."
    );
    // T14.52: with "stop recording by itself" off nothing promises a stop; the rest is the same.
    let asks = call_end_toast(false);
    assert_eq!(asks.title, t.title);
    assert_eq!(
        asks.body,
        "Do you want to stop recording? Earshot keeps recording until you stop it."
    );
    assert!(!asks.body.contains("will stop"), "{}", asks.body);
    for t in [t, asks] {
        assert_eq!(t.click, ToastClick::Window);
        assert_eq!(
            t.buttons,
            vec![
                ("Stop recording", ToastClick::StopRecording),
                ("Keep recording", ToastClick::KeepRecording),
            ]
        );
        assert!(t.stays);
    }
    // What it says is what the rule does.
    assert_eq!(
        earshot_runtime::call_end::CallEndConfig::default().stop_after,
        std::time::Duration::from_secs(3 * 60)
    );
    // The others go by themselves.
    assert!(!listening_toast().stays);
    assert!(!call_end_stopped_toast().stays);
    // Sent before the recording is saved, so it cannot say that it is.
    assert!(!call_end_stopped_toast().body.contains("saved"));
    assert!(!summary_toast(1, 1, 1).stays);
}

#[test]
fn shell_meeting_that_stopped_itself_always_notifies_with_fixed_copy() {
    // T9.17: the banner of a meeting no track could record any more asks for a notification,
    // whatever the window shows; no other banner does.
    let stopped = crate::meeting_events::warning_event(
        &earshot_runtime::live::RuntimeWarning::EveryRecordingStopped { disk_full: true },
    )
    .unwrap();
    assert_eq!(
        always_shown_toast(&stopped),
        Some(recording_stopped_toast())
    );
    let other = crate::meeting_events::warning_event(
        &earshot_runtime::live::RuntimeWarning::RecordingStopped {
            track: earshot_core::capture::TrackId::Mic,
            at: earshot_core::asr::MeetingTime::from_millis(1000),
            disk_full: true,
            error: "full".into(),
        },
    )
    .unwrap();
    assert_eq!(always_shown_toast(&other), None);
    assert_eq!(always_shown_toast(&started()), None);
    assert_eq!(always_shown_toast(&ended()), None);
    let t = recording_stopped_toast();
    assert!(!t.stays);
    assert!(t.buttons.is_empty());
    // Its own notification said it ended: the end's is not shown after it.
    assert_eq!(ended_toast(EndedReason::Stopped, true), None);
}

#[test]
fn call_end_question_closes_with_the_recording_and_not_with_an_older_meetings_summary() {
    let offer = |open| MeetingEvent::CallEndOffer { open, stops: true };
    let summary = |meeting: &str| MeetingEvent::Summary {
        meeting: meeting.into(),
        result: SummaryEvent::Cancelled,
    };
    // While recording, the question decides.
    assert!(call_end_open_after(&offer(true), Phase::Recording, false));
    assert!(!call_end_open_after(&offer(false), Phase::Recording, true));
    assert!(!call_end_open_after(
        &MeetingEvent::CallEndStopped,
        Phase::Recording,
        true
    ));
    // Other events leave it, an older meeting's summary behind a recording one included.
    assert!(call_end_open_after(
        &summary("older"),
        Phase::Recording,
        true
    ));
    assert!(!call_end_open_after(
        &summary("older"),
        Phase::Recording,
        false
    ));

    // Through the real tracker: the end of the recording closes it.
    let mut tracker = PhaseTracker::default();
    let started = MeetingEvent::Started {
        meeting: "m".into(),
        mode: CaptureMode::Call,
        tracks: Vec::new(),
    };
    tracker.apply(&started);
    assert_eq!(tracker.phase(), Phase::Recording);
    let ended = MeetingEvent::Ended {
        meeting: "m".into(),
        reason: EndedReason::Stopped,
    };
    tracker.apply(&ended);
    assert!(!call_end_open_after(&ended, tracker.phase(), true));

    // A meeting thread's panic: a failed summary and no end. No stale button survives it.
    let mut tracker = PhaseTracker::default();
    tracker.apply(&started);
    let panicked = summary("m");
    tracker.apply(&panicked);
    assert!(!call_end_open_after(&panicked, tracker.phase(), true));
}

#[test]
fn call_end_a_click_on_a_closed_question_does_nothing() {
    assert_eq!(
        call_end_answer(ToastClick::StopRecording, true),
        Some(CallEndAnswer::Stop)
    );
    assert_eq!(
        call_end_answer(ToastClick::KeepRecording, true),
        Some(CallEndAnswer::Keep)
    );
    assert_eq!(call_end_answer(ToastClick::StopRecording, false), None);
    assert_eq!(call_end_answer(ToastClick::KeepRecording, false), None);
    assert_eq!(call_end_answer(ToastClick::Window, true), None);
}

#[test]
fn shell_sync_quit_waits_for_gone_presence_only_inside_the_exit_limit() {
    use std::time::Duration;
    // Nothing saved: the whole gone wait.
    assert_eq!(gone_wait(Duration::ZERO), QUIT_GONE_WAIT);
    // A long save leaves less, and never past the exit limit.
    assert_eq!(
        gone_wait(EXIT_SAVE_LIMIT - Duration::from_millis(300)),
        Duration::from_millis(300)
    );
    assert_eq!(gone_wait(EXIT_SAVE_LIMIT), Duration::ZERO);
    assert_eq!(gone_wait(EXIT_SAVE_LIMIT * 2), Duration::ZERO);
    assert!(QUIT_GONE_WAIT < EXIT_SAVE_LIMIT);
}

#[test]
fn shell_call_start_toast_is_fixed_copy_that_stays() {
    let t = call_start_toast();
    assert_eq!(t.title, "A call seems to have started");
    assert_eq!(t.body, "Do you want Earshot to record it?");
    assert_eq!(t.click, ToastClick::Window);
    assert_eq!(
        t.buttons,
        vec![
            ("Start recording", ToastClick::StartRecording),
            ("Not now", ToastClick::NotNow),
        ]
    );
    assert!(t.stays);
}

#[test]
fn shell_call_start_answers_only_an_open_question() {
    assert_eq!(
        call_start_answer(ToastClick::StartRecording, true),
        Some(true)
    );
    assert_eq!(call_start_answer(ToastClick::NotNow, true), Some(false));
    // A late click on a question that closed never starts a recording.
    assert_eq!(call_start_answer(ToastClick::StartRecording, false), None);
    assert_eq!(call_start_answer(ToastClick::NotNow, false), None);
    for other in [
        ToastClick::Window,
        ToastClick::Meeting,
        ToastClick::StopRecording,
        ToastClick::KeepRecording,
    ] {
        assert_eq!(call_start_answer(other, true), None);
    }
}

#[test]
fn shell_mic_reading_counts_a_dedicated_call_app_only() {
    use earshot_runtime::call_start::MicReading;
    let session = |name: &str, active: bool| earshot_capture_wasapi::SessionPeak {
        endpoint: "Microphone".into(),
        endpoint_id: "{mic}".into(),
        pid: 10,
        process_name: name.into(),
        active,
        peak: None,
    };
    assert_eq!(mic_reading(None), None);
    assert_eq!(
        mic_reading(Some(Ok(vec![session("ms-teams.exe", true)]))),
        Some(MicReading::CallApp)
    );
    assert_eq!(
        mic_reading(Some(Ok(vec![session("ms-teams.exe", false)]))),
        Some(MicReading::NoCallApp)
    );
    assert_eq!(
        mic_reading(Some(Ok(vec![session("msedge.exe", true)]))),
        Some(MicReading::NoCallApp)
    );
    assert_eq!(
        mic_reading(Some(Err("unreadable".into()))),
        Some(MicReading::Unknown)
    );
}

/// A tray that behaves as Tauri's does: a call on the main thread draws at once, and a call on
/// any other thread is posted and waits until the main thread has run it.
struct MainThreadTray {
    main: std::thread::ThreadId,
    posted: std::sync::Mutex<std::sync::mpsc::Sender<PostedDraw>>,
    shown: std::sync::Mutex<Vec<TrayView>>,
}

/// A draw waiting for the main thread, and how its caller hears it ran.
type PostedDraw = (TrayView, std::sync::mpsc::Sender<()>);

impl MainThreadTray {
    /// A tray whose main thread is the calling thread.
    fn new() -> (Arc<Self>, std::sync::mpsc::Receiver<PostedDraw>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let tray = Arc::new(Self {
            main: std::thread::current().id(),
            posted: std::sync::Mutex::new(tx),
            shown: std::sync::Mutex::default(),
        });
        (tray, rx)
    }

    /// The main thread runs a posted draw.
    fn run(&self, (view, done): PostedDraw) {
        self.shown.lock().unwrap().push(view);
        let _ = done.send(());
    }
}

impl TraySurface for MainThreadTray {
    fn show(&self, view: TrayView) {
        if std::thread::current().id() == self.main {
            self.shown.lock().unwrap().push(view);
            return;
        }
        let (done, ran) = std::sync::mpsc::channel();
        self.posted.lock().unwrap().send((view, done)).unwrap();
        ran.recv().unwrap();
    }
}

/// Bounds only a failing case: every wait below is a rendezvous.
const SHELL_GUARD: std::time::Duration = std::time::Duration::from_secs(60);

#[test]
fn shell_tray_refresh_from_another_thread_never_blocks_the_main_thread() {
    // T14.5: a meeting's thread refreshes the tray and waits for the main thread to run the
    // call; the main thread (the shortcut, pressed now) refreshes too. Neither may wait for a
    // lock the other holds.
    let (finished, shown) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (tray, posted) = MainThreadTray::new();
        let board = Arc::new(TrayBoard::default());
        board.set(Arc::clone(&tray) as Arc<dyn TraySurface>);
        let from_meeting = Arc::clone(&board);
        let meeting = std::thread::spawn(move || from_meeting.refresh(|| Phase::Recording));
        // The meeting's thread is inside its tray call, waiting for this thread.
        let waiting = posted.recv().unwrap();
        // The main thread draws now, before it is back in its event loop.
        board.refresh(|| Phase::Finishing);
        tray.run(waiting);
        meeting.join().unwrap();
        let _ = finished.send(tray.shown.lock().unwrap().clone());
    });
    let shown = shown.recv_timeout(SHELL_GUARD).expect(
        "the main thread waited for the tray while another thread's tray call waited for it",
    );
    assert_eq!(
        shown,
        vec![tray_view(Phase::Finishing), tray_view(Phase::Recording)]
    );
}

/// A tray on one thread that changes the phase during its first draw, as another thread would.
struct MovesThePhase {
    phase: Arc<std::sync::Mutex<Phase>>,
    shown: std::sync::Mutex<Vec<TrayView>>,
}

impl TraySurface for MovesThePhase {
    fn show(&self, view: TrayView) {
        let mut shown = self.shown.lock().unwrap();
        if shown.is_empty() {
            *self.phase.lock().unwrap() = Phase::Finishing;
        }
        shown.push(view);
    }
}

#[test]
fn shell_tray_refresh_draws_again_when_the_phase_moved_while_it_drew() {
    // With no lock around the drawing an older phase could land last: the draw is repeated until
    // what it drew is still the phase.
    let phase = Arc::new(std::sync::Mutex::new(Phase::Recording));
    let tray = Arc::new(MovesThePhase {
        phase: Arc::clone(&phase),
        shown: std::sync::Mutex::default(),
    });
    let board = TrayBoard::default();
    // No tray yet (it could not be made): nothing to draw, and nothing waits.
    board.refresh(|| *phase.lock().unwrap());
    board.set(Arc::clone(&tray) as Arc<dyn TraySurface>);
    board.refresh(|| *phase.lock().unwrap());
    assert_eq!(
        *tray.shown.lock().unwrap(),
        vec![tray_view(Phase::Recording), tray_view(Phase::Finishing)]
    );
    // A phase that stays is drawn once.
    board.refresh(|| *phase.lock().unwrap());
    assert_eq!(tray.shown.lock().unwrap().len(), 3);
}

#[test]
fn shell_call_start_setting_is_read_once_and_follows_what_the_page_stores() {
    // T14.9: the watch asked the store twice a second. Now it reads once, and the General page's
    // command tells it of a change.
    let reads = std::cell::Cell::new(0);
    let read = |value: Option<bool>| {
        let reads = &reads;
        move || {
            reads.set(reads.get() + 1);
            value
        }
    };
    let setting = OfferSetting::default();
    // No store yet: off for now, and asked again.
    assert!(!setting.get(read(None)));
    assert!(!setting.get(read(None)));
    assert_eq!(reads.get(), 2);
    // Read: kept from then on.
    assert!(setting.get(read(Some(true))));
    assert!(setting.get(read(Some(false))), "the kept value, not a read");
    assert!(setting.get(read(None)));
    assert_eq!(reads.get(), 3);
    // The page turns it off, then on: the watch follows without a read.
    setting.set(false);
    assert!(!setting.get(read(Some(true))));
    setting.set(true);
    assert!(setting.get(read(Some(false))));
    assert_eq!(reads.get(), 3);
    // A change made before the first read wins over what that read finds.
    let setting = OfferSetting::default();
    assert!(!setting.get(|| {
        setting.set(false);
        Some(true)
    }));
}

#[test]
fn shell_a_start_in_the_tray_with_no_data_says_so_in_a_notification() {
    // T14.9: started by Windows, no window opens, and a store that failed to open was silent
    // until a meeting would not start.
    assert_eq!(start_toast(false, true), Some(data_unopened_toast()));
    // With a window on screen the window says it; with a store there is nothing to say.
    assert_eq!(start_toast(true, true), None);
    assert_eq!(start_toast(false, false), None);
    assert_eq!(start_toast(true, false), None);
    let t = data_unopened_toast();
    assert_eq!(t.title, "Earshot could not open its data");
    assert_eq!(
        t.body,
        "Meetings cannot be recorded until this is fixed. Open Earshot to see why."
    );
    assert_eq!(t.click, ToastClick::Window);
    assert!(t.buttons.is_empty());
    assert!(!t.stays);
    let text = format!("{} {}", t.title, t.body);
    assert!(!text.contains('\u{2014}') && !text.contains('!'), "{text}");
}

#[test]
fn shell_window_csp_allows_no_plugins_base_tags_or_form_posts() {
    // T14.9: `default-src` does not cover `base-uri` or `form-action`, and `object-src` fell
    // back to it ('self'). The page uses none of the three.
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    let csp = config["app"]["security"]["csp"].as_str().unwrap();
    let directives: Vec<&str> = csp.split(';').map(str::trim).collect();
    for wanted in [
        "object-src 'none'",
        "base-uri 'none'",
        "form-action 'none'",
        // What was there stays as it was.
        "default-src 'self'",
        "script-src 'self'",
        "style-src 'self' 'unsafe-inline'",
        "img-src 'self' data:",
        "font-src 'self'",
        "media-src blob:",
        "connect-src ipc: http://ipc.localhost",
    ] {
        assert!(directives.contains(&wanted), "{wanted} in: {csp}");
    }
    assert_eq!(directives.len(), 10, "{csp}");
}

fn summary_of(meeting: &str) -> MeetingEvent {
    MeetingEvent::Summary {
        meeting: meeting.into(),
        result: SummaryEvent::RecordingFailed {
            message: "The meeting stopped.".into(),
        },
    }
}

#[test]
fn shell_a_start_with_no_started_event_still_ends() {
    // T14.5: the driver answered the tray's start with a meeting whose thread stopped before its
    // `Started`. Its last event must end the start, or the tray stays on "Starting" for good.
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    assert!(!t.start_reported("m7"), "still starting");
    assert_eq!(t.phase(), Phase::Starting);
    assert!(t.apply(&summary_of("m7")));
    assert_eq!(t.phase(), Phase::Idle);
    assert!(t.begin_start(), "the tray starts meetings again");

    // The answer may come after that last event.
    assert!(!t.apply(&summary_of("m8")));
    assert_eq!(t.phase(), Phase::Starting);
    assert!(t.start_reported("m8"));
    assert_eq!(t.phase(), Phase::Idle);

    // With an end and no `Started`: finishing, then idle.
    assert!(t.begin_start());
    t.start_reported("m9");
    assert!(t.apply(&MeetingEvent::Ended {
        meeting: "m9".into(),
        reason: EndedReason::Failed,
    }));
    assert_eq!(t.phase(), Phase::Finishing);
    assert_eq!(t.meeting(), Some("m9"));
    assert!(t.apply(&summary_of("m9")));
    assert_eq!(t.phase(), Phase::Idle);
}

#[test]
fn shell_a_start_whose_meeting_is_already_over_does_not_say_earshot_is_listening() {
    // T14.5 review: the meeting died before its `Started` and its last event came before the
    // driver's answer. The start ended there, and "Earshot is listening" would be false.
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    t.apply(&summary_of("m8"));
    assert!(t.start_reported("m8"));
    assert_eq!(started_toast(t.phase()), None);
    // The same behind an older meeting that still finishes.
    let mut t = PhaseTracker::default();
    t.apply(&started());
    t.apply(&ended());
    assert!(t.begin_start());
    t.apply(&summary_of("m2"));
    assert!(t.start_reported("m2"));
    assert_eq!(t.phase(), Phase::Finishing);
    assert_eq!(started_toast(t.phase()), None);
    // A meeting so short that it started and ended before the answer.
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    t.apply(&started());
    t.apply(&ended());
    assert!(!t.start_reported("m1"));
    assert_eq!(started_toast(t.phase()), None);

    // A meeting that records, in either order of its `Started` and the answer, says so.
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    t.apply(&started());
    assert!(!t.start_reported("m1"));
    assert_eq!(started_toast(t.phase()), Some(listening_toast()));
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    assert!(!t.start_reported("m1"));
    assert_eq!(t.phase(), Phase::Starting);
    assert_eq!(started_toast(t.phase()), Some(listening_toast()));
}

#[test]
fn shell_a_reported_start_changes_nothing_for_a_meeting_that_started() {
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    // The usual order: `Started` first, then the driver's answer.
    t.apply(&started());
    assert!(!t.start_reported("m1"));
    assert_eq!(t.phase(), Phase::Recording);
    // The other order.
    let mut t = PhaseTracker::default();
    assert!(t.begin_start());
    assert!(!t.start_reported("m1"));
    assert!(t.apply(&started()));
    assert_eq!(t.phase(), Phase::Recording);
    assert!(t.apply(&ended()));
    assert!(t.apply(&MeetingEvent::Summary {
        meeting: "m1".into(),
        result: stored(0)
    }));
    assert_eq!(t.phase(), Phase::Idle);
    // A meeting already over when its answer comes (a short replay).
    assert!(!t.start_reported("m1"));
    assert_eq!(t.phase(), Phase::Idle);
}

#[test]
fn shell_a_start_with_no_started_event_goes_back_to_the_older_meeting() {
    // An older meeting finishes behind the start: only the started meeting's own last event ends
    // the start, and the tray then shows what the older one is doing.
    let mut t = PhaseTracker::default();
    t.apply(&started());
    t.apply(&ended());
    assert!(t.begin_start());
    t.start_reported("m2");
    assert!(t.apply(&summary_of("m2")));
    assert_eq!(t.phase(), Phase::Finishing, "the older meeting still runs");
    assert!(t.apply(&MeetingEvent::Summary {
        meeting: "m1".into(),
        result: stored(0)
    }));
    assert_eq!(t.phase(), Phase::Idle);

    // The older one ends first, and its summary is never taken for the new meeting's.
    let mut t = PhaseTracker::default();
    t.apply(&started());
    t.apply(&ended());
    assert!(t.begin_start());
    assert!(!t.apply(&MeetingEvent::Summary {
        meeting: "m1".into(),
        result: stored(0)
    }));
    assert!(!t.start_reported("m2"), "m1's end is not m2's");
    assert_eq!(t.phase(), Phase::Starting);
    assert!(t.apply(&summary_of("m2")));
    assert_eq!(t.phase(), Phase::Idle);
}
