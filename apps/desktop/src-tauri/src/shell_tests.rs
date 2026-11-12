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
    assert_eq!(toggle_action(DriverPhase::Idle), ToggleAction::Start);
    assert_eq!(toggle_action(DriverPhase::Recording), ToggleAction::Stop);
    assert_eq!(toggle_action(DriverPhase::Processing), ToggleAction::Start);
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
    // Nothing for a summary nobody asked for, a re-transcription or a job's steps.
    for e in [
        JobEvent::Summary {
            meeting: id.clone(),
            result: SummaryEvent::Cancelled,
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
fn call_end_toast_is_fixed_copy_that_stays_and_answers_in_one_click() {
    let t = call_end_toast();
    assert_eq!(t.title, "The call seems to have ended");
    assert_eq!(
        t.body,
        "Earshot will stop recording within 3 minutes unless you keep it going."
    );
    assert_eq!(t.click, ToastClick::Window);
    assert_eq!(
        t.buttons,
        vec![
            ("Stop recording", ToastClick::StopRecording),
            ("Keep recording", ToastClick::KeepRecording),
        ]
    );
    assert!(t.stays);
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
    let offer = |open| MeetingEvent::CallEndOffer { open };
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
