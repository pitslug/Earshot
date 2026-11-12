//! Event mapping tests: every name contains `meeting_events`.

use std::time::{Duration, Instant};

use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, MeetingTime, Segment, TranscriptEvent, Word,
};
use earshot_core::capture::{EndReason, TrackId};
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::EntityId;
use earshot_core::pipeline::{Levels, PipelineError};
use earshot_core::summary::{ActionItem, Decision, SummaryV1};
use earshot_runtime::RuntimeError;
use earshot_runtime::host::{HostOutcome, HostPost, HostStep, SkipReason, SummaryResult};
use earshot_runtime::live::{LiveOutcome, RuntimeEvent, RuntimeWarning};
use earshot_runtime::post::{CleanupWork, PostSpeaker, PostStage};

use super::*;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([n; 16])
}

fn word(text: &str, start: u64) -> Word {
    Word {
        text: text.into(),
        start: MeetingTime::from_millis(start),
        end: MeetingTime::from_millis(start + 200),
        probability: 0.9,
    }
}

fn final_seg(track: TrackId, words: &[(&str, u64)]) -> RuntimeEvent {
    let words: Vec<Word> = words.iter().map(|(t, s)| word(t, *s)).collect();
    RuntimeEvent::Transcript {
        track,
        event: TranscriptEvent::Final(Segment::from_words(id(9), id(8), words, MeetingTime::ZERO)),
    }
}

fn levels(end: u64, cleaned: f32, reference: Option<f32>) -> RuntimeEvent {
    RuntimeEvent::Levels(Levels {
        end: MeetingTime::from_millis(end),
        mic_dbfs: cleaned + 3.0,
        cleaned_dbfs: cleaned,
        reference_dbfs: reference,
    })
}

fn ended(reason: EndReason, problems: Vec<RuntimeWarning>) -> RuntimeEvent {
    RuntimeEvent::Ended(LiveOutcome {
        meeting: id(1),
        tracks: vec![(TrackId::Mic, id(2))],
        end: MeetingTime::from_millis(60_000),
        reason,
        final_pass: earshot_core::pipeline::FinalPassRequest {
            end: MeetingTime::from_millis(60_000),
            tracks: vec![],
        },
        audio_files: vec![],
        unavailable_at_end: vec![],
        problems,
        echo_words_removed: 0,
        echo_locks: vec![],
    })
}

#[test]
fn far_end_banners_share_a_key_so_the_good_news_replaces_the_warning() {
    let MeetingEvent::Warning { key: silent, .. } =
        warning_event(&RuntimeWarning::FarEndSilent).unwrap()
    else {
        panic!("not a warning");
    };
    let MeetingEvent::Warning {
        key: heard, kind, ..
    } = warning_event(&RuntimeWarning::FarEndHeard).unwrap()
    else {
        panic!("not a warning");
    };
    assert_eq!(silent, heard);
    assert_eq!(kind, WarningKind::DeviceRestored);
}

#[test]
fn meeting_events_started_and_backend_name_the_tracks_and_label_the_backend() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let now = Instant::now();
    let started = m.map(
        &RuntimeEvent::Started {
            meeting: id(1),
            tracks: vec![(TrackId::Mic, id(2)), (TrackId::Reference, id(3))],
        },
        now,
    );
    assert_eq!(
        started,
        [MeetingEvent::Started {
            meeting: id(1).to_string(),
            mode: CaptureMode::Call,
            tracks: vec![LiveTrack::You, LiveTrack::Them],
        }]
    );
    let backend = |kind| RuntimeEvent::LiveBackend {
        track: TrackId::Reference,
        backend: BackendInfo {
            kind,
            device: "NVIDIA GeForce RTX 5080".into(),
            model: "ggml-large-v3-turbo".into(),
        },
        fallbacks: vec![],
    };
    for (kind, label) in [
        (BackendKind::Cuda, "GPU (CUDA)"),
        (BackendKind::Vulkan, "GPU (Vulkan)"),
        (BackendKind::Cpu, "CPU"),
        (BackendKind::Qnn, "NPU"),
    ] {
        assert_eq!(
            m.map(&backend(kind), now),
            [MeetingEvent::Backend {
                track: LiveTrack::Them,
                label: label.into(),
            }]
        );
    }
}

#[test]
fn meeting_events_levels_are_throttled_to_ten_a_second_with_the_given_clock() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let mut shown = Vec::new();
    // A level every 25 ms for one second: only those 100 ms apart go through.
    for k in 0..40 {
        shown.extend(m.map(&levels(k * 25, -30.0, Some(-20.0)), at(k * 25)));
    }
    assert_eq!(shown.len(), 10);
    assert_eq!(
        shown[0],
        MeetingEvent::Levels {
            you_dbfs: -30.0,
            them_dbfs: Some(-20.0),
        }
    );
    // A clock that goes back never lets a burst through.
    assert!(m.map(&levels(2_000, -30.0, None), at(500)).is_empty());
}

#[test]
fn meeting_events_levels_are_finite_and_room_mode_has_no_them_level() {
    let mut m = EventMapper::new(CaptureMode::InRoom);
    let t0 = Instant::now();
    assert_eq!(
        m.map(&levels(0, f32::NEG_INFINITY, None), t0),
        [MeetingEvent::Levels {
            you_dbfs: FLOOR_DBFS,
            them_dbfs: None,
        }]
    );
    assert_eq!(
        m.map(
            &levels(100, f32::NAN, Some(3.0)),
            t0 + Duration::from_secs(1)
        ),
        [MeetingEvent::Levels {
            you_dbfs: FLOOR_DBFS,
            them_dbfs: Some(0.0),
        }]
    );
}

#[test]
fn meeting_events_finals_land_with_their_text_and_count_words_partials_do_not() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let now = Instant::now();
    let landed = m.map(
        &final_seg(
            TrackId::Mic,
            &[("ship", 1_000), ("it", 1_300), ("Friday", 1_600)],
        ),
        now,
    );
    assert_eq!(
        landed,
        [
            MeetingEvent::Landed {
                track: LiveTrack::You,
                start_ms: 1_000,
                end_ms: 1_800,
                text: "ship it Friday".into(),
            },
            MeetingEvent::Words { total: 3 },
        ]
    );
    // Provisional words are their own event, and count for nothing (T9.1m).
    let shown = m.map(
        &RuntimeEvent::LivePreview {
            track: TrackId::Reference,
            words: vec![word("maybe", 2_000)],
        },
        now,
    );
    assert_eq!(
        shown,
        [MeetingEvent::Provisional {
            track: LiveTrack::Them,
            words: vec![crate::meeting_events::ProvisionalWord {
                start_ms: 2_000,
                text: "maybe".into(),
            }],
        }]
    );
    assert_eq!(m.words(), 3);
    let report = RuntimeEvent::LivePreviewReport(earshot_runtime::live::PreviewReport::default());
    assert!(m.map(&report, now).is_empty());
    let partial = RuntimeEvent::Transcript {
        track: TrackId::Reference,
        event: TranscriptEvent::Partial(earshot_core::asr::PartialSegment {
            segment: Segment::from_words(
                id(4),
                id(3),
                vec![word("maybe", 2_000)],
                MeetingTime::ZERO,
            ),
            committed_words: 0,
        }),
    };
    assert!(m.map(&partial, now).is_empty());
    let them = m.map(
        &final_seg(TrackId::Reference, &[("agreed", 2_000), ("then", 2_400)]),
        now,
    );
    assert_eq!(them[1], MeetingEvent::Words { total: 5 });
    assert_eq!(m.words(), 5);
}

#[test]
fn meeting_events_lag_reports_the_slowest_track() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let now = Instant::now();
    let lag = |track, s| RuntimeEvent::TranscriptLag {
        track,
        behind: Duration::from_secs(s),
    };
    assert_eq!(
        m.map(&lag(TrackId::Mic, 12), now),
        [MeetingEvent::Lag { seconds: 12.0 }]
    );
    assert_eq!(
        m.map(&lag(TrackId::Reference, 40), now),
        [MeetingEvent::Lag { seconds: 40.0 }]
    );
    assert_eq!(
        m.map(&lag(TrackId::Reference, 3), now),
        [MeetingEvent::Lag { seconds: 12.0 }]
    );
}

#[test]
fn meeting_events_voice_activity_and_end_reasons_map_one_to_one() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let now = Instant::now();
    assert_eq!(
        m.map(
            &RuntimeEvent::VoiceActivity {
                track: TrackId::Reference,
                speaking: true,
                at: MeetingTime::from_millis(5),
            },
            now
        ),
        [MeetingEvent::VoiceActivity {
            track: LiveTrack::Them,
            speaking: true,
        }]
    );
    m.map(
        &RuntimeEvent::Started {
            meeting: id(1),
            tracks: vec![(TrackId::Mic, id(2))],
        },
        now,
    );
    for (reason, shown) in [
        (EndReason::Stopped, EndedReason::Stopped),
        (EndReason::Finished, EndedReason::Finished),
        (EndReason::Failed, EndedReason::Failed),
    ] {
        // The stop's end is the recording saved, not the rest of a live transcript decoded.
        assert_eq!(
            m.map(
                &RuntimeEvent::RecordingSaved {
                    reason,
                    problems: vec![]
                },
                now
            ),
            [MeetingEvent::Ended {
                meeting: id(1).to_string(),
                reason: shown
            }]
        );
        assert_eq!(m.map(&ended(reason, vec![]), now), []);
    }
    // Finalising problems are banners before the end.
    let out = m.map(
        &ended(
            EndReason::Stopped,
            vec![RuntimeWarning::Recording {
                track: TrackId::Mic,
                error: "disk full".into(),
            }],
        ),
        now,
    );
    assert!(
        matches!(&out[0], MeetingEvent::Warning { kind: WarningKind::Recording, message, .. } if message.contains("disk full"))
    );
    assert_eq!(
        out.len(),
        1,
        "the end was said when the recording was saved"
    );
}

#[test]
fn meeting_events_diagnostics_are_not_shown() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let now = Instant::now();
    for e in [
        RuntimeEvent::Resynced {
            mic_skew_ms: 120.0,
            track_offset_ms: None,
        },
        RuntimeEvent::LivePartials {
            track: TrackId::Mic,
            enabled: false,
            behind: Duration::from_secs(7),
        },
        RuntimeEvent::Discontinuity {
            track: TrackId::Mic,
            at: MeetingTime::ZERO,
            kind: earshot_core::capture::DiscontinuityKind::DataLost,
        },
        RuntimeEvent::Transcript {
            track: TrackId::Mic,
            event: TranscriptEvent::Discarded { id: id(5) },
        },
    ] {
        assert!(m.map(&e, now).is_empty(), "{e:?}");
    }
}

fn banner(w: &RuntimeWarning) -> (String, WarningKind, String) {
    match warning_event(w) {
        Some(MeetingEvent::Warning { key, kind, message }) => (key, kind, message),
        other => panic!("{other:?}"),
    }
}

/// UI copy rules (docs/ui-design.md): no em dashes, no "please", no exclamation marks.
fn assert_copy(message: &str) {
    assert!(!message.contains('\u{2014}'), "{message}");
    assert!(!message.to_lowercase().contains("please"), "{message}");
    assert!(!message.contains('!'), "{message}");
    assert!(
        message.chars().next().is_some_and(char::is_uppercase),
        "{message}"
    );
}

#[test]
fn meeting_events_every_runtime_warning_has_a_banner_with_plain_copy() {
    let warnings = [
        RuntimeWarning::BluetoothHandsFreeMic {
            device_name: "Headset (Acme Headset)".into(),
            sample_rate: Some(16_000),
        },
        RuntimeWarning::DeviceUnavailable {
            track: TrackId::Mic,
        },
        RuntimeWarning::DeviceRestored {
            track: TrackId::Mic,
            device_name: "Desk mic".into(),
        },
        RuntimeWarning::Pipeline(PipelineError::LiveAsr {
            track: TrackId::Reference,
            error: AsrError::Cancelled,
        }),
        RuntimeWarning::Pipeline(PipelineError::AfterEnd),
        RuntimeWarning::LiveCutAtStop {
            track: TrackId::Mic,
            behind: Duration::from_secs(41),
        },
        RuntimeWarning::Capture("device busy".into()),
        RuntimeWarning::CaptureRepeated {
            message: "device busy".into(),
            times: 3,
        },
        RuntimeWarning::Recording {
            track: TrackId::Reference,
            error: "disk full".into(),
        },
        RuntimeWarning::Persistence("database locked".into()),
        RuntimeWarning::BackendFallback {
            track: TrackId::Mic,
            backend: BackendKind::Cpu,
            refused: vec![(BackendKind::Cuda, AsrError::Cancelled)],
        },
        RuntimeWarning::ChosenDeviceUnavailable {
            track: TrackId::Mic,
            chosen: "Acme Evolve".into(),
            used: "Surface Mic".into(),
            error: None,
        },
        RuntimeWarning::ChosenDeviceGivenUp {
            track: TrackId::Reference,
            chosen: "USB speakers".into(),
            used: "Speakers".into(),
        },
        RuntimeWarning::FarEndSilent,
        RuntimeWarning::FarEndHeard,
        RuntimeWarning::RecordingStopped {
            track: TrackId::Mic,
            at: MeetingTime::from_millis(61_000),
            disk_full: true,
            error: "full".into(),
        },
        RuntimeWarning::EveryRecordingStopped { disk_full: false },
        RuntimeWarning::DeviceStoppedAnswering,
        RuntimeWarning::LiveCatchUpStalled {
            track: TrackId::Reference,
        },
    ];
    let kinds: Vec<WarningKind> = warnings
        .iter()
        .map(|w| {
            let (_, kind, message) = banner(w);
            assert_copy(&message);
            kind
        })
        .collect();
    assert_eq!(
        kinds,
        [
            WarningKind::BluetoothHandsFree,
            WarningKind::DeviceUnavailable,
            WarningKind::DeviceRestored,
            WarningKind::TrackBehind,
            WarningKind::Audio,
            WarningKind::TrackBehind,
            WarningKind::Audio,
            WarningKind::Audio,
            WarningKind::Recording,
            WarningKind::Storage,
            WarningKind::Backend,
            WarningKind::DeviceFallback,
            WarningKind::DeviceFallback,
            WarningKind::Audio,
            WarningKind::DeviceRestored,
            WarningKind::Recording,
            WarningKind::Recording,
            WarningKind::Audio,
            WarningKind::TrackBehind,
        ]
    );
    let (_, _, bt) = banner(&warnings[0]);
    assert!(bt.contains("Headset (Acme Headset)") && bt.contains("computer's own mic"));
    let (_, _, cut) = banner(&warnings[5]);
    assert!(cut.contains("41 s behind"), "{cut}");
    let (_, _, fallback) = banner(&warnings[10]);
    assert_eq!(
        fallback,
        "Live transcription for your microphone runs on the CPU because the GPU (CUDA) could \
         not be used."
    );
}

#[test]
fn meeting_events_a_stopped_recording_says_when_and_why_in_plain_words() {
    // T9.17: once per track (one key per track), the time it stopped and a full disk named.
    let stopped = |track, ms, disk_full| {
        banner(&RuntimeWarning::RecordingStopped {
            track,
            at: MeetingTime::from_millis(ms),
            disk_full,
            error: "os error 112".into(),
        })
    };
    let (key, kind, message) = stopped(TrackId::Mic, 754_200, true);
    assert_copy(&message);
    assert_eq!(
        (key.as_str(), kind),
        ("recording:you", WarningKind::Recording)
    );
    assert_eq!(
        message,
        "The recording of your microphone stopped at 12:34 because the disk is full. What was \
         recorded before that is kept."
    );
    assert!(!message.contains("112"), "a full disk needs no error code");
    let (key, _, message) = stopped(TrackId::Reference, 3_725_000, false);
    assert_copy(&message);
    assert_eq!(key, "recording:them");
    assert!(message.contains("at 1:02:05"), "{message}");
    assert!(
        message.contains("os error 112"),
        "any other failure says what it was"
    );
    // A file that could not even be finished replaces that banner.
    let (failed_key, _, _) = banner(&RuntimeWarning::Recording {
        track: TrackId::Mic,
        error: "gone".into(),
    });
    assert_eq!(failed_key, "recording:you");

    for disk_full in [true, false] {
        let (key, kind, message) = banner(&RuntimeWarning::EveryRecordingStopped { disk_full });
        assert_copy(&message);
        assert_eq!(key, RECORDING_STOPPED_KEY);
        assert_eq!(kind, WarningKind::Recording);
        assert_eq!(message, every_recording_stopped_message(disk_full));
        assert_eq!(message.contains("disk is full"), disk_full);
    }
}

#[test]
fn meeting_events_a_chosen_device_that_is_gone_says_what_is_recorded_instead() {
    let w = |track, chosen: &str, error: Option<&str>| {
        banner(&RuntimeWarning::ChosenDeviceUnavailable {
            track,
            chosen: chosen.into(),
            used: "Speakers (Realtek)".into(),
            error: error.map(str::to_owned),
        })
    };
    let (key, kind, mic) = w(TrackId::Mic, "Acme Evolve", None);
    assert_eq!(key, "chosen-device:you");
    assert_eq!(kind, WarningKind::DeviceFallback);
    assert_eq!(
        mic,
        "Acme Evolve is not connected, so Earshot records the default microphone \
         (Speakers (Realtek)). You can choose another microphone in Settings, Audio."
    );
    let (key, _, out) = w(TrackId::Reference, "", Some("the device is in use"));
    assert_eq!(key, "chosen-device:them");
    assert_eq!(
        out,
        "The chosen output could not be opened (the device is in use), so Earshot records \
         computer audio from the default output (Speakers (Realtek)). You can choose another \
         output in Settings, Audio."
    );
    assert_copy(&out);
    // Given up during the meeting: it replaces the loss's "unavailable" banner (same key)
    // and says the default is recorded for the rest of the meeting, never that it is back.
    let (lost, _, _) = banner(&RuntimeWarning::DeviceUnavailable {
        track: TrackId::Mic,
    });
    let (key, kind, given_up) = banner(&RuntimeWarning::ChosenDeviceGivenUp {
        track: TrackId::Mic,
        chosen: "Acme Evolve".into(),
        used: "Surface Mic".into(),
    });
    assert_eq!(key, lost);
    assert_eq!(kind, WarningKind::DeviceFallback);
    assert_eq!(
        given_up,
        "Acme Evolve is still not connected, so Earshot records the default microphone \
         (Surface Mic) for the rest of this meeting."
    );
    assert!(!given_up.contains("is back"));
    let (key, _, out) = banner(&RuntimeWarning::ChosenDeviceGivenUp {
        track: TrackId::Reference,
        chosen: String::new(),
        used: "Speakers".into(),
    });
    assert_eq!(key, "device:them");
    assert_eq!(
        out,
        "The chosen output is still not connected, so Earshot records computer audio from the \
         default output (Speakers) for the rest of this meeting."
    );
    assert_copy(&out);
    // Its own key: a later "device unavailable" banner does not replace it.
    let (gone, _, _) = banner(&RuntimeWarning::DeviceUnavailable {
        track: TrackId::Mic,
    });
    assert_ne!(gone, "chosen-device:you");
}

#[test]
fn meeting_events_a_restored_device_replaces_its_unavailable_banner_only() {
    let (gone, _, gone_text) = banner(&RuntimeWarning::DeviceUnavailable {
        track: TrackId::Reference,
    });
    let (back, kind, back_text) = banner(&RuntimeWarning::DeviceRestored {
        track: TrackId::Reference,
        device_name: "Speakers (Realtek)".into(),
    });
    assert_eq!(gone, back);
    assert_eq!(kind, WarningKind::DeviceRestored);
    assert_eq!(
        gone_text,
        "Computer audio is unavailable. Earshot records silence until it is back."
    );
    assert_eq!(back_text, "Computer audio is back (Speakers (Realtek)).");
    let (mic, _, _) = banner(&RuntimeWarning::DeviceUnavailable {
        track: TrackId::Mic,
    });
    assert_ne!(mic, gone);
}

#[test]
fn meeting_events_speakers_off_is_one_banner() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let out = m.map(
        &RuntimeEvent::LiveSpeakersOff {
            reason: "speaker detection is set to after the call".into(),
        },
        Instant::now(),
    );
    assert_eq!(
        out,
        [MeetingEvent::Warning {
            key: "speakers".into(),
            kind: WarningKind::SpeakersOff,
            message: "Speakers are found after the meeting (speaker detection is set to after \
                      the call)."
                .into(),
        }]
    );
}

#[test]
fn meeting_events_live_summaries_off_is_one_banner_with_the_runtime_reason() {
    use earshot_runtime::live_summaries::{ON_BATTERY_REASON, UNPLUGGED_REASON};
    let mut m = EventMapper::new(CaptureMode::Call);
    let banner = |m: &mut EventMapper, reason: &str| {
        m.map(
            &RuntimeEvent::LiveSummariesOff {
                reason: reason.into(),
            },
            Instant::now(),
        )
    };
    let warning = |message: &str| MeetingEvent::Warning {
        key: "live-summaries".into(),
        kind: WarningKind::SummariesOff,
        message: message.into(),
    };
    assert_eq!(
        banner(&mut m, ON_BATTERY_REASON),
        [warning(
            "Summaries wait for the end of the call on battery power, as speaker detection does."
        )]
    );
    // A stop during the meeting replaces the start banner (same key).
    assert_eq!(
        banner(&mut m, UNPLUGGED_REASON),
        [warning(
            "Summarising stopped for the rest of the call when the computer switched to \
             battery power."
        )]
    );
    // Sentence case and one full stop, whatever the reason's own ending.
    assert_eq!(
        banner(&mut m, "no local summary model."),
        [warning("No local summary model.")]
    );
}

fn work(track: TrackId) -> CleanupWork {
    CleanupWork {
        track,
        to_transcribe: Duration::from_secs(10),
        to_diarise: Duration::from_secs(10),
    }
}

#[test]
fn meeting_events_post_progress_counts_the_final_pass_steps() {
    let mut p = PostProgressMapper::default();
    let works = [work(TrackId::Mic), work(TrackId::Reference)];
    let el = Duration::from_millis(5);
    let steps = [
        HostStep::PostLoaded { missing: &[] },
        HostStep::PostStarting {
            fallbacks: &[],
            work: &works,
            asr_backend: BackendKind::Cpu,
        },
        HostStep::Post(PostStage::Transcribed {
            track: TrackId::Mic,
            elapsed: el,
        }),
        HostStep::Post(PostStage::Diarised {
            track: TrackId::Reference,
            speakers: 2,
            elapsed: el,
        }),
        HostStep::Post(PostStage::Transcribed {
            track: TrackId::Reference,
            elapsed: el,
        }),
        HostStep::Post(PostStage::Diarised {
            track: TrackId::Mic,
            speakers: 1,
            elapsed: el,
        }),
        HostStep::Post(PostStage::EchoChecked { elapsed: el }),
        HostStep::Post(PostStage::Stored { elapsed: el }),
        HostStep::PostFinished(&Ok(HostPost::Skipped)),
        HostStep::SummaryStarting,
    ];
    let shown: Vec<(PostStep, u32, Option<u32>)> = steps
        .iter()
        .filter_map(|s| p.step(s))
        .map(|e| match e {
            MeetingEvent::PostProgress {
                step, done, total, ..
            } => (step, done, total),
            other => panic!("{other:?}"),
        })
        .collect();
    use LiveTrack::{Them, You};
    assert_eq!(
        shown,
        [
            (PostStep::Loading, 0, None),
            (PostStep::Starting, 0, Some(6)),
            (PostStep::Transcribed { track: You }, 1, Some(6)),
            (PostStep::Diarised { track: Them }, 2, Some(6)),
            (PostStep::Transcribed { track: Them }, 3, Some(6)),
            (PostStep::Diarised { track: You }, 4, Some(6)),
            (PostStep::EchoChecked, 5, Some(6)),
            (PostStep::Stored, 6, Some(6)),
            (PostStep::Summarising, 6, Some(6)),
        ]
    );
}

fn speaker(role: SpeakerRole, person: Option<EntityId>) -> PostSpeaker {
    PostSpeaker {
        id: id(20),
        role,
        track: TrackId::Reference,
        person,
        display_name: "Speaker 1".into(),
        speech: Duration::from_secs(30),
        ignored: false,
    }
}

#[test]
fn meeting_events_voices_to_name_skip_me_known_and_ignored_people() {
    let ignored = PostSpeaker {
        ignored: true,
        ..speaker(SpeakerRole::Remote, None)
    };
    let speakers = [
        speaker(SpeakerRole::Me, None),
        speaker(SpeakerRole::Remote, None),
        speaker(SpeakerRole::Remote, Some(id(30))),
        speaker(SpeakerRole::Room, None),
        ignored,
    ];
    assert_eq!(voices_to_name(&speakers), 2);
    assert_eq!(voices_to_name(&[]), 0);
    // The one rule: only an unnamed voice nobody ignored needs a name.
    assert!(needs_name(SpeakerRole::Room, false, false));
    assert!(!needs_name(SpeakerRole::Room, false, true));
    assert!(!needs_name(SpeakerRole::Remote, true, false));
    assert!(!needs_name(SpeakerRole::Me, false, false));
}

#[test]
fn meeting_events_a_stored_summary_counts_actions_and_decisions() {
    let summary = SummaryV1 {
        title: "Vendor review: pumps".into(),
        tldr: "Pumps ship in March.".into(),
        enhanced_notes: vec![],
        decisions: vec![
            Decision {
                text: "Order pumps".into(),
                source_segment_ids: vec![],
            };
            2
        ],
        action_items: vec![
            ActionItem {
                owner: "Sam".into(),
                task: "Send the order".into(),
                due: None,
                source_segment_ids: vec![],
            };
            4
        ],
        open_questions: vec![],
        topics: vec![],
    };
    assert_eq!(
        stored_event(&summary, 2),
        SummaryEvent::Stored {
            title: "Vendor review: pumps".into(),
            actions: 4,
            decisions: 2,
            voices_to_name: 2,
        }
    );
}

fn outcome(post: Result<HostPost, RuntimeError>, summary: SummaryResult) -> HostOutcome {
    HostOutcome {
        missing_tracks: vec![],
        post,
        summary,
        network: vec![],
        handovers: vec![],
        handover_record_failures: vec![],
        claude_effort_unread: false,
    }
}

#[test]
fn meeting_events_summary_results_map_with_plain_copy() {
    let skipped = summary_event(&outcome(
        Ok(HostPost::Skipped),
        SummaryResult::Skipped {
            reason: SkipReason::NotRequested,
        },
    ));
    assert_eq!(
        skipped,
        SummaryEvent::Skipped {
            reason: "Summaries are turned off.".into()
        }
    );
    let missing = summary_event(&outcome(
        Ok(HostPost::Skipped),
        SummaryResult::Skipped {
            reason: SkipReason::NotInstalled {
                model: "qwen3.5-9b",
                why: "run `cargo xtask fetch-models --set summary`".into(),
            },
        },
    ));
    let SummaryEvent::Skipped { reason } = &missing else {
        panic!("{missing:?}");
    };
    assert!(reason.starts_with("No summary model is installed"));
    assert!(reason.contains("Settings, Summaries"));
    assert!(!reason.contains("fetch-models"), "{reason}");
    let failed = summary_event(&outcome(
        Ok(HostPost::Skipped),
        SummaryResult::Failed {
            error: RuntimeError::Conflict("the transcript changed"),
            retryable: true,
        },
    ));
    // Fixed copy by the error's kind, never its text (T9.23).
    assert_eq!(
        failed,
        SummaryEvent::Failed {
            message: crate::plain_text::TRY_AGAIN_SOON.into(),
            retryable: true,
        }
    );
    // A failed final pass offers Re-transcribe, with the reason as fixed copy.
    let post_failed = summary_event(&outcome(
        Err(RuntimeError::Asr(earshot_core::asr::AsrError::Backend(
            "zq-decoder words".into(),
        ))),
        SummaryResult::Cancelled,
    ));
    let SummaryEvent::PostFailed { message, retryable } = &post_failed else {
        panic!("{post_failed:?}");
    };
    assert!(!retryable);
    assert!(
        message.starts_with("The transcript could not be finished."),
        "{message}"
    );
    assert!(
        message.ends_with(crate::plain_text::TRANSCRIPTION_FAILED),
        "{message}"
    );
    assert!(!message.contains("zq-"), "{message}");
    assert_eq!(
        summary_event(&outcome(Ok(HostPost::Skipped), SummaryResult::Cancelled)),
        SummaryEvent::Cancelled
    );
    let post_cancelled = HostPost::Cancelled {
        speaker_windows_deleted: Ok(1),
    };
    assert_eq!(
        summary_event(&outcome(Ok(post_cancelled), SummaryResult::Cancelled)),
        SummaryEvent::PostCancelled,
        "the meeting reads as not processed, not as a cancelled summary"
    );
    // A failed final pass is its own result, with the reason as fixed copy, not a skip
    // (T9.23: the screen offers Re-transcribe).
    let post_failed = summary_event(&outcome(
        Err(RuntimeError::NoTracks),
        SummaryResult::Skipped {
            reason: SkipReason::PostFailed,
        },
    ));
    assert_eq!(
        post_failed,
        SummaryEvent::PostFailed {
            message: format!(
                "The transcript could not be finished. The recording and the draft transcript \
                 are saved. {}",
                crate::plain_text::NO_AUDIO
            ),
            retryable: false,
        }
    );
    for e in [skipped, missing, failed, post_failed] {
        match e {
            SummaryEvent::Skipped { reason: m } | SummaryEvent::Failed { message: m, .. } => {
                assert_copy(&m);
            }
            _ => {}
        }
    }
}

#[test]
fn summary_fallback_note_is_only_for_a_summary_that_left_the_card() {
    assert_eq!(summary_fallback_note(&SummaryResult::Cancelled), None);
    assert_eq!(
        summary_fallback_note(&SummaryResult::Skipped {
            reason: SkipReason::NotRequested,
        }),
        None
    );
    let stalled = gpu_fallback_copy(&LocalSummaryError::GpuStalled);
    assert_copy(&stalled);
    assert!(stalled.contains("stopped responding"), "{stalled}");
    assert!(stalled.contains("restart Earshot"), "{stalled}");
    // A child that was killed freed the card: nothing to restart.
    let child_stalled = gpu_fallback_copy(&LocalSummaryError::GpuChildStalled);
    assert_copy(&child_stalled);
    assert!(
        child_stalled.contains("stopped responding"),
        "{child_stalled}"
    );
    assert!(!child_stalled.contains("restart"), "{child_stalled}");
    let died = gpu_fallback_copy(&LocalSummaryError::GpuChildDied {
        why: "it ended with exit code 0xc0000409".to_owned(),
    });
    assert_copy(&died);
    assert!(died.contains("recording and transcript are safe"), "{died}");
    assert!(!died.contains("0xc0000409"), "{died}");
    assert!(!died.contains("restart"), "{died}");
    let changed = gpu_fallback_copy(&LocalSummaryError::CudaFilesChanged {
        why: "a file is missing or has another size".to_owned(),
    });
    assert_copy(&changed);
    assert!(changed.contains("Restart Earshot"), "{changed}");
    assert!(!changed.contains("another size"), "{changed}");
    let refused = gpu_fallback_copy(&LocalSummaryError::Chunks(
        earshot_core::prompt::TimeChunkError::ZeroWindow,
    ));
    assert_copy(&refused);
    assert!(refused.contains("could not load"), "{refused}");
    assert!(!refused.contains("restart"), "{refused}");
}

/// A newer meeting may start from the save on, so a failed save is told with it, not at the end.
#[test]
fn meeting_queue_events_a_failed_save_is_told_with_the_save() {
    let mut m = EventMapper::new(CaptureMode::Call);
    let out = m.map(
        &RuntimeEvent::RecordingSaved {
            reason: EndReason::Stopped,
            problems: vec![RuntimeWarning::Recording {
                track: TrackId::Mic,
                error: "disk full".into(),
            }],
        },
        Instant::now(),
    );
    assert!(
        matches!(&out[0], MeetingEvent::Warning { kind: WarningKind::Recording, message, .. } if message.contains("disk full"))
    );
    assert!(matches!(out[1], MeetingEvent::Ended { .. }));
}

/// The screen after a meeting shows only banners sent after `Ended`, and whoever was not watching
/// during the meeting still has to hear about lost audio.
#[test]
fn meeting_events_a_recording_gap_comes_after_the_end_and_once() {
    let gap = RuntimeWarning::RecordingGap {
        total: std::time::Duration::from_secs(97),
    };
    let saving = RuntimeWarning::Recording {
        track: TrackId::Mic,
        error: "disk full".into(),
    };
    let mut m = EventMapper::new(CaptureMode::InRoom);
    let now = Instant::now();
    let out = m.map(
        &RuntimeEvent::RecordingSaved {
            reason: EndReason::Stopped,
            problems: vec![gap.clone(), saving.clone()],
        },
        now,
    );
    assert!(
        matches!(
            &out[0],
            MeetingEvent::Warning {
                kind: WarningKind::Recording,
                ..
            }
        ),
        "{out:?}"
    );
    assert!(matches!(out[1], MeetingEvent::Ended { .. }), "{out:?}");
    assert!(
        matches!(&out[2], MeetingEvent::Warning { key, message, .. }
            if key == "recording-gap" && message.contains("about 1 minute 37 seconds")),
        "{out:?}"
    );
    assert_eq!(out.len(), 3);
    let out = m.map(&ended(EndReason::Stopped, vec![gap, saving]), now);
    assert!(out.is_empty(), "already told: {out:?}");
}

#[test]
fn meeting_events_a_recording_gap_keeps_one_banner_with_the_total() {
    for (secs, words) in [
        (2, "about 2 seconds"),
        (40, "about 40 seconds"),
        (60, "about 1 minute,"),
        (97, "about 1 minute 37 seconds"),
        (120, "about 2 minutes,"),
        (185, "about 3 minutes 5 seconds"),
    ] {
        let (key, kind, message) = banner(&RuntimeWarning::RecordingGap {
            total: std::time::Duration::from_secs(secs),
        });
        assert_eq!(key, "recording-gap");
        assert_eq!(kind, WarningKind::Audio);
        assert!(message.contains(words), "{secs}: {message}");
        assert!(message.contains("lost"), "{message}");
    }
}

/// The outcome holds every problem, and the save's were told already: each is said once.
#[test]
fn meeting_queue_events_a_problem_told_with_the_save_is_not_told_again_at_the_end() {
    let saving = RuntimeWarning::Recording {
        track: TrackId::Mic,
        error: "disk full".into(),
    };
    let later = RuntimeWarning::Persistence("a later record".into());
    let mut m = EventMapper::new(CaptureMode::Call);
    let now = Instant::now();
    m.map(
        &RuntimeEvent::RecordingSaved {
            reason: EndReason::Stopped,
            problems: vec![saving.clone()],
        },
        now,
    );
    let out = m.map(&ended(EndReason::Stopped, vec![saving, later]), now);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(
        matches!(&out[0], MeetingEvent::Warning { message, .. } if message.contains("a later record"))
    );
}

#[test]
fn call_output_refused_says_what_is_still_recorded() {
    let (key, kind, message) = banner(&RuntimeWarning::CallOutputRefused {
        wanted: "Headset".into(),
        used: Some("Speakers".into()),
    });
    assert_eq!(key, "call-output");
    assert_eq!(kind, WarningKind::DeviceFallback);
    assert_eq!(
        message,
        "The call plays on Headset, which Earshot could not open, so it goes on recording \
         computer audio from Speakers."
    );
    let (_, _, message) = banner(&RuntimeWarning::CallOutputRefused {
        wanted: "Headset".into(),
        used: None,
    });
    assert_eq!(
        message,
        "The call plays on Headset, which Earshot could not open."
    );
}
