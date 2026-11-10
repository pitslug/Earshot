//! What the app says when something a meeting needs is not on this computer (T9.1f): fixed
//! copy that points at the Settings page holding it (T12.5b): the speech helpers on System,
//! transcription models on Transcription, summary models on Summaries. The runtime's own text
//! names developer commands
//! (`RuntimeError`'s Display, for the CLI and for tests); none of them may reach a window.
//!
//! The exits, each pinned by a test in this module: every runtime error becomes an `IpcError`
//! through `From<RuntimeError>` ([`runtime_text`]); the events a job or a meeting sends pass
//! through [`plain_meeting_event`], [`plain_job_event`] and [`plain_model_event`] on their
//! way into the sink, whatever built them (a banner, a summary's failure, a child's error);
//! the model check's child sends [`runtime_text`]; the ONNX Runtime fallback banners use
//! [`library_problem`]. Transcript text and summary titles are the user's and pass unchanged.
//!
//! The copy names what to download only when the error's own file says what it is
//! ([`Catalogue::model_of_file`]); otherwise it points at the System and Transcription pages,
//! which list what is missing. Telling someone to fetch the wrong thing is worse than telling them where to look.

use earshot_runtime::RuntimeError;
use earshot_runtime::catalogue::{Catalogue, ModelKind, display_name};
use earshot_runtime::post::PostWarning;

use crate::cuda_settings::CudaEvent;
use crate::jobs::JobEvent;
use crate::meeting_events::{LiveTrack, MeetingEvent, SummaryEvent};
use crate::model_jobs::ModelJobEvent;

/// What the user is told when a download is missing and nothing says which.
pub const MODELS_NEEDED: &str = "Earshot needs a download that is not on this computer yet. Open \
     Settings: the System and Transcription pages show what is missing.";

/// The speech helpers are what is missing.
pub const HELPERS_NEEDED: &str = "Earshot needs the speech helpers, which are not on this \
     computer yet. Open Settings, System to download them.";

/// A transcription model is what is missing. Which one is the user's choice, so none is named:
/// on a clean install the one a run would try is only the fallback.
pub const TRANSCRIPTION_NEEDED: &str = "Earshot needs a transcription model that is not on this \
     computer yet. Open Settings, Transcription to download one.";

/// A transcription model and the speech helpers are both missing, as on a clean install: said in
/// one go, so a meeting is not refused for one and then for the other.
pub const TRANSCRIPTION_AND_HELPERS_NEEDED: &str = "Earshot needs a transcription model and the \
     speech helpers, which are not on this computer yet. Open Settings to download them: the \
     model on the Transcription page, the helpers on the System page.";

/// A summary model chosen in Settings is what is missing, when its file does not say which.
pub const SUMMARY_MODEL_NEEDED: &str = "The summary model you chose is not on this computer yet. \
     Open Settings, Summaries to download it or choose another.";

/// What the app says about what the final pass warned of, as `(banner key, text)`: fixed copy and
/// counts, one banner per kind and track, never a warning's own developer text. Left out: what
/// changes nothing for the user (a stored record that could not be read means the track was
/// transcribed or diarised whole; the backend each pass used is on the meeting's header).
#[must_use]
pub fn post_warning_copy(warnings: &[PostWarning]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for track in [LiveTrack::You, LiveTrack::Them] {
        let of_track = |t: earshot_core::capture::TrackId| LiveTrack::of(t) == track;
        let dropped: usize = warnings
            .iter()
            .map(|w| match w {
                PostWarning::RepetitionDropped { track: t, spans } if of_track(*t) => spans.len(),
                _ => 0,
            })
            .sum();
        if dropped > 0 {
            let passages = if dropped == 1 {
                "1 passage".to_owned()
            } else {
                format!("{dropped} passages")
            };
            out.push((
                format!("post-repetition:{}", track.key()),
                format!(
                    "{passages} from {} could not be transcribed, because every attempt repeated \
                     itself. The final transcript has nothing for them.",
                    track.device()
                ),
            ));
        }
        let voices = warnings
            .iter()
            .filter(
                |w| matches!(w, PostWarning::VoiceWithoutWords { track: t, .. } if of_track(*t)),
            )
            .count();
        if voices > 0 {
            let heard = if voices == 1 {
                "1 voice".to_owned()
            } else {
                format!("{voices} voices")
            };
            out.push((
                format!("post-voices:{}", track.key()),
                format!(
                    "Earshot heard {heard} on {} with no transcribed words, so the speaker list \
                     leaves them out.",
                    track.device()
                ),
            ));
        }
    }
    for w in warnings {
        let shown = match w {
            PostWarning::NotEnoughSpeechForMe { .. } => Some((
                "post-my-voice",
                "This meeting had too little of your voice to remember it.",
            )),
            PostWarning::SkippedVoiceprints(_) => Some((
                "post-voiceprints",
                "Some remembered voices could not be read, so those people were not recognised.",
            )),
            PostWarning::ProvenanceNotStored(_) => Some((
                "post-record",
                "The final transcript was saved, but Earshot could not save its note of how it \
                 was made.",
            )),
            PostWarning::NoSpeakerEchoCheck { .. } => Some((
                "post-echo",
                "The check for echo by voice could not run, so some of the other side may show \
                 as your words.",
            )),
            PostWarning::SpeakerNamesNotCarried { .. } => Some((
                "post-names",
                "The names given in this meeting could not be kept. Name its voices again.",
            )),
            PostWarning::BackendChanged { .. }
            | PostWarning::StretchRecordUnreadable { .. }
            | PostWarning::SpeakerRecordUnreadable { .. }
            | PostWarning::RepetitionDropped { .. }
            | PostWarning::VoiceWithoutWords { .. } => None,
        };
        if let Some((key, text)) = shown
            && !out.iter().any(|(k, _)| k == key)
        {
            out.push((key.to_owned(), text.to_owned()));
        }
    }
    out
}

/// Text only a developer can act on. Lower case and specific, so a device called "Cargo Pro"
/// or a folder name does not trip it; `xtask` also covers every `cargo xtask` command.
const DEVELOPER_WORDS: &[&str] = &[
    "cargo ",
    "xtask",
    "fetch-models",
    "qnn-convert",
    "EARSHOT_MODELS",
    "manifest.toml",
];

/// Whether `text` tells the reader to run a developer command or names a developer file.
#[must_use]
pub fn names_developer_tool(text: &str) -> bool {
    DEVELOPER_WORDS.iter().any(|w| text.contains(w))
}

/// `text`, or [`MODELS_NEEDED`] when it names a developer tool.
#[must_use]
pub fn plain(text: String) -> String {
    if names_developer_tool(&text) {
        MODELS_NEEDED.to_owned()
    } else {
        text
    }
}

/// The file `e` says is missing or damaged.
fn missing_file(e: &RuntimeError) -> Option<&std::path::Path> {
    match e {
        RuntimeError::ModelMissing { path, .. } | RuntimeError::ModelIncomplete { path, .. } => {
            Some(path)
        }
        _ => None,
    }
}

fn model_file_missing(e: &RuntimeError) -> bool {
    missing_file(e).is_some()
}

/// The copy for summary model `model` missing, named as the user knows it.
fn summary_copy_of(model: &str) -> String {
    format!(
        "The summary model you chose, {}, is not on this computer yet. Open Settings, Summaries to \
         download it or choose another.",
        display_name(model)
    )
}

/// The copy for a missing transcription model. `helpers_there` is whether the speech helpers are
/// installed in the same folder; unknown counts as there, and the next refusal then names them.
fn transcription_copy(helpers_there: bool) -> &'static str {
    if helpers_there {
        TRANSCRIPTION_NEEDED
    } else {
        TRANSCRIPTION_AND_HELPERS_NEEDED
    }
}

/// What the user is told when summary model `model`, chosen in Settings, is not downloaded.
#[must_use]
pub fn chosen_summary_model(model: &str) -> String {
    summary_copy_of(model)
}

/// Why a meeting kept on this computer (T12.13) gets no summary: only a summary model on this
/// computer may write it and `model` is not downloaded. `ahead` for the banner at the start
/// ("will not run"), else for the summary result and a refused re-summarise.
#[must_use]
pub fn kept_local_no_model(model: &str, ahead: bool) -> String {
    let lead = if ahead {
        "The summary will not run"
    } else {
        "There is no summary"
    };
    format!(
        "{lead}: this meeting is kept on this computer, so only a summary model here can write          it, and {} is not downloaded yet. Download it in Settings, Summaries, then summarise          this meeting.",
        display_name(model)
    )
}

/// What the user is told when a summary that would leave this computer is refused for a meeting
/// kept on it (T12.13).
pub const KEPT_LOCAL_REFUSED: &str = "This meeting is kept on this computer, so its summary is     written only by the summary model here. Nothing was sent.";

/// What the user is told about runtime error `e`.
#[must_use]
pub fn runtime_text(e: &RuntimeError) -> String {
    if matches!(e, RuntimeError::SummaryKeptLocal(_)) {
        return KEPT_LOCAL_REFUSED.to_owned();
    }
    let Some(path) = missing_file(e) else {
        return plain(e.to_string());
    };
    let owner = Catalogue::embedded()
        .ok()
        .and_then(|c| c.model_of_file(path));
    match owner.map(|m| (m.kind, m.name.as_str())) {
        Some((ModelKind::Helpers, _)) => HELPERS_NEEDED.to_owned(),
        Some((ModelKind::Transcription, _)) => {
            let helpers_there = Catalogue::embedded()
                .ok()
                .and_then(|c| c.helpers_beside(path))
                .unwrap_or(true);
            transcription_copy(helpers_there).to_owned()
        }
        Some((ModelKind::Summary, name)) => summary_copy_of(name),
        // A model's NPU files are never loaded without every one of them (`npu_order`); the
        // Transcription page offers them.
        Some((ModelKind::NpuVersion, _)) | None => MODELS_NEEDED.to_owned(),
    }
}

/// What the user is told when the summary model chosen in Settings is not installed
/// (`host::SummaryPlanError::NotInstalled`): the caller knows it is a summary model whatever
/// the error's file says.
#[must_use]
pub fn summary_model_not_installed(e: &RuntimeError) -> String {
    let owner = missing_file(e)
        .and_then(|path| Catalogue::embedded().ok()?.model_of_file(path))
        .filter(|m| m.kind == ModelKind::Summary);
    owner.map_or_else(
        || SUMMARY_MODEL_NEEDED.to_owned(),
        |m| summary_copy_of(&m.name),
    )
}

/// Why an ONNX Runtime library could not load, inside a banner that says what runs instead.
#[must_use]
pub fn library_problem(e: &RuntimeError) -> String {
    let text = e.to_string();
    if model_file_missing(e) || names_developer_tool(&text) {
        "it is not downloaded".to_owned()
    } else {
        text
    }
}

fn plain_summary(result: SummaryEvent) -> SummaryEvent {
    match result {
        SummaryEvent::Skipped { reason } => SummaryEvent::Skipped {
            reason: plain(reason),
        },
        SummaryEvent::Failed { message, retryable } => SummaryEvent::Failed {
            message: plain(message),
            retryable,
        },
        other => other,
    }
}

/// `event` with its copy made plain; never touches transcript text or a summary's title.
#[must_use]
pub fn plain_meeting_event(event: MeetingEvent) -> MeetingEvent {
    match event {
        MeetingEvent::Warning { key, kind, message } => MeetingEvent::Warning {
            key,
            kind,
            message: plain(message),
        },
        MeetingEvent::Summary { meeting, result } => MeetingEvent::Summary {
            meeting,
            result: plain_summary(result),
        },
        other => other,
    }
}

/// `event` with its copy made plain.
#[must_use]
pub fn plain_job_event(event: JobEvent) -> JobEvent {
    match event {
        JobEvent::Summary { meeting, result } => JobEvent::Summary {
            meeting,
            result: plain_summary(result),
        },
        JobEvent::Warning { meeting, message } => JobEvent::Warning {
            meeting,
            message: plain(message),
        },
        JobEvent::Failed {
            meeting,
            job,
            message,
            retryable,
        } => JobEvent::Failed {
            meeting,
            job,
            message: plain(message),
            retryable,
        },
        JobEvent::Retranscribed {
            meeting,
            missing_tracks,
            no_audio,
            summary_stale,
            warnings,
        } => JobEvent::Retranscribed {
            meeting,
            missing_tracks,
            no_audio,
            summary_stale,
            warnings: warnings.into_iter().map(plain).collect(),
        },
        other => other,
    }
}

/// `event` with its copy made plain.
#[must_use]
pub fn plain_model_event(event: ModelJobEvent) -> ModelJobEvent {
    match event {
        ModelJobEvent::Warning { model, message } => ModelJobEvent::Warning {
            model,
            message: plain(message),
        },
        ModelJobEvent::Failed {
            model,
            message,
            retryable,
        } => ModelJobEvent::Failed {
            model,
            message: plain(message),
            retryable,
        },
        other => other,
    }
}

/// `event` with its copy made plain.
#[must_use]
pub fn plain_cuda_event(event: CudaEvent) -> CudaEvent {
    match event {
        CudaEvent::Warning { part, message } => CudaEvent::Warning {
            part,
            message: plain(message),
        },
        CudaEvent::Failed {
            part,
            message,
            retryable,
        } => CudaEvent::Failed {
            part,
            message: plain(message),
            retryable,
        },
        other => other,
    }
}

#[cfg(test)]
mod post_warning_tests {
    use std::time::Duration;

    use earshot_core::asr::{BackendKind, MeetingTime};
    use earshot_core::capture::TrackId;
    use earshot_runtime::post::PostWarning;

    use super::{names_developer_tool, post_warning_copy};

    #[test]
    fn post_warnings_read_as_fixed_copy_with_counts_per_track_and_kind() {
        let span = (
            MeetingTime::from_millis(1_000),
            MeetingTime::from_millis(2_000),
        );
        let voice = |track| PostWarning::VoiceWithoutWords {
            track,
            speech: Duration::from_secs(1),
        };
        let copy = post_warning_copy(&[
            PostWarning::RepetitionDropped {
                track: TrackId::Mic,
                spans: vec![span, span],
            },
            PostWarning::RepetitionDropped {
                track: TrackId::Mic,
                spans: vec![span],
            },
            voice(TrackId::Reference),
            voice(TrackId::Reference),
            voice(TrackId::Mic),
            PostWarning::SkippedVoiceprints(2),
            PostWarning::SkippedVoiceprints(1),
        ]);
        let keys: Vec<&str> = copy.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "post-repetition:you",
                "post-voices:you",
                "post-voices:them",
                "post-voiceprints"
            ]
        );
        assert!(copy[0].1.starts_with("3 passages from your microphone"));
        assert!(copy[1].1.contains("1 voice on your microphone"));
        assert!(copy[2].1.contains("2 voices on computer audio"));
    }

    #[test]
    fn post_warnings_never_show_a_warnings_own_text_and_leave_out_what_changes_nothing() {
        let reason = "cargo xtask fetch-models secret-reason";
        let copy = post_warning_copy(&[
            PostWarning::ProvenanceNotStored(reason.into()),
            PostWarning::NoSpeakerEchoCheck {
                reason: reason.into(),
            },
            PostWarning::SpeakerNamesNotCarried {
                reason: reason.into(),
            },
        ]);
        assert_eq!(copy.len(), 3);
        for (_, text) in &copy {
            assert!(!text.contains("secret-reason"));
            assert!(!names_developer_tool(text));
            assert!(!text.contains('\u{2014}'));
        }
        assert!(
            post_warning_copy(&[
                PostWarning::BackendChanged {
                    before: BackendKind::Cpu,
                    after: BackendKind::Cpu,
                },
                PostWarning::StretchRecordUnreadable {
                    track: TrackId::Mic,
                    reason: reason.into(),
                },
                PostWarning::SpeakerRecordUnreadable {
                    track: TrackId::Mic,
                    reason: reason.into(),
                },
            ])
            .is_empty()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use earshot_runtime::backends::{Accelerator, CAMPPLUS, ModelDir, SILERO};
    use earshot_runtime::host;

    use super::*;
    use crate::ipc::IpcError;
    use crate::meeting_events::{WarningKind, skip_copy};
    use crate::testutil::TempDir;

    fn no_developer_text(text: &str) {
        for word in [
            "cargo",
            "xtask",
            "fetch-models",
            "EARSHOT_MODELS",
            "manifest",
        ] {
            assert!(!text.contains(word), "{word} in: {text}");
        }
    }

    /// The real refusals from a real empty models folder: each file a meeting needs in turn,
    /// with the copy each must give.
    fn real_refusals(dir: &TempDir) -> Vec<(RuntimeError, &'static str)> {
        let models = ModelDir::new(dir.0.join("models"));
        let mut out = vec![
            (
                // A clean install: both named at once, so a meeting is refused only once.
                host::require_models(&models, "base.en", true).unwrap_err(),
                TRANSCRIPTION_AND_HELPERS_NEEDED,
            ),
            (
                earshot_runtime::backends::init_onnx(&models, Accelerator::Cpu).unwrap_err(),
                HELPERS_NEEDED,
            ),
        ];
        let file = dir.0.join("models").join("whisper/ggml-base.en.bin");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"x").unwrap();
        // The transcription model is there: now a helper is what is missing.
        out.push((
            host::require_models(&models, "base.en", true).unwrap_err(),
            HELPERS_NEEDED,
        ));
        let silero = dir.0.join("models").join(SILERO);
        std::fs::create_dir_all(silero.parent().unwrap()).unwrap();
        std::fs::write(&silero, b"x").unwrap();
        out.push((
            host::require_models(&models, "base.en", true).unwrap_err(),
            HELPERS_NEEDED,
        ));
        assert!(!dir.0.join("models").join(CAMPPLUS).exists());
        out
    }

    #[test]
    fn plain_text_a_meeting_refused_for_a_missing_model_or_helper_shows_no_developer_command() {
        let dir = TempDir::new("plain-refusal");
        for (e, copy) in real_refusals(&dir) {
            // The runtime's own text keeps the command for the CLI and developers.
            assert!(e.to_string().contains("cargo xtask fetch-models"), "{e}");
            let ipc = IpcError::from(e);
            assert_eq!(ipc.kind, "model");
            assert_eq!(ipc.message, copy);
            no_developer_text(&ipc.message);
        }
        // Each points at the Settings page that holds what is missing (T12.5b).
        for (copy, pages) in [
            (MODELS_NEEDED, &["System and Transcription pages"][..]),
            (HELPERS_NEEDED, &["Settings, System"][..]),
            (TRANSCRIPTION_NEEDED, &["Settings, Transcription"][..]),
            (
                TRANSCRIPTION_AND_HELPERS_NEEDED,
                &["Transcription page", "System page"][..],
            ),
            (SUMMARY_MODEL_NEEDED, &["Settings, Summaries"][..]),
        ] {
            for page in pages {
                assert!(copy.contains(page), "{copy}");
            }
            no_developer_text(copy);
            assert_eq!(earshot_runtime::catalogue::manifest_id_in(copy), None);
        }
        // Copy that does not know what is missing names nothing to download.
        assert!(!MODELS_NEEDED.contains("speech helpers"));
        assert!(!MODELS_NEEDED.contains("transcription"));
        assert_eq!(
            earshot_runtime::catalogue::manifest_id_in(MODELS_NEEDED),
            None
        );
    }

    #[test]
    fn plain_text_a_missing_summary_model_is_never_sent_to_the_helpers_or_transcription() {
        use earshot_runtime::summary_model::{SUMMARY_MODELS, installed};
        let dir = TempDir::new("plain-summary");
        let models = ModelDir::new(dir.0.join("models"));
        let summary = &SUMMARY_MODELS[0];
        let missing = installed(&models, summary).unwrap_err();
        // A file of another size is the other variant, with the same answer.
        let file = dir.0.join("models").join(summary.file);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"x").unwrap();
        let incomplete = installed(&models, summary).unwrap_err();
        assert!(matches!(incomplete, RuntimeError::ModelIncomplete { .. }));
        for e in [missing, incomplete] {
            for text in [runtime_text(&e), summary_model_not_installed(&e)] {
                assert!(text.contains(display_name(summary.id)), "{text}");
                assert!(text.contains("Settings, Summaries"), "{text}");
                assert!(!text.contains("speech helpers"), "{text}");
                assert!(!text.contains("transcription"), "{text}");
                no_developer_text(&text);
                assert_eq!(earshot_runtime::catalogue::manifest_id_in(&text), None);
            }
        }
        // The caller knows it is a summary model even when the file says nothing.
        let other = RuntimeError::ModelMissing {
            path: PathBuf::from("somewhere/else.bin"),
            fetch_set: None,
        };
        assert_eq!(summary_model_not_installed(&other), SUMMARY_MODEL_NEEDED);
        assert_eq!(runtime_text(&other), MODELS_NEEDED);
    }

    #[test]
    fn plain_text_every_other_runtime_error_with_a_developer_command_is_replaced_too() {
        let incomplete = RuntimeError::ModelIncomplete {
            path: PathBuf::from("models/x.bin"),
            expected: 2,
            found: 1,
            fetch_set: "ci",
        };
        assert_eq!(IpcError::from(incomplete).message, MODELS_NEEDED);
        // Text the runtime builds for a QNN build and in `earshot_onnx::init`.
        let onnx = RuntimeError::Internal(
            "the NPU conversion is missing: run `cargo xtask qnn-convert --model m`",
        );
        assert_eq!(IpcError::from(onnx).message, MODELS_NEEDED);
        let unknown = RuntimeError::UnknownModel("old".into());
        no_developer_text(&IpcError::from(unknown).message);
        // Nothing else changes.
        let other = RuntimeError::Capture("the microphone is busy".into());
        assert_eq!(
            IpcError::from(other).message,
            "capture: the microphone is busy"
        );
    }

    #[test]
    fn plain_text_events_lose_developer_commands_and_keep_the_users_words() {
        let dev = "run `cargo xtask fetch-models --set ci`".to_owned();
        let event = plain_meeting_event(MeetingEvent::Warning {
            key: "k".into(),
            kind: WarningKind::Backend,
            message: dev.clone(),
        });
        let MeetingEvent::Warning { message, .. } = event else {
            panic!("a warning stays a warning");
        };
        assert_eq!(message, MODELS_NEEDED);
        for result in [
            SummaryEvent::Skipped {
                reason: dev.clone(),
            },
            SummaryEvent::Failed {
                message: format!("The cleanup after the meeting failed: {dev}"),
                retryable: false,
            },
        ] {
            let MeetingEvent::Summary { result, .. } = plain_meeting_event(MeetingEvent::Summary {
                meeting: "m".into(),
                result,
            }) else {
                panic!("a summary stays a summary");
            };
            no_developer_text(&serde_json::to_string(&result).unwrap());
        }
        for event in [
            JobEvent::Warning {
                meeting: "m".into(),
                message: dev.clone(),
            },
            JobEvent::Summary {
                meeting: "m".into(),
                result: SummaryEvent::Skipped {
                    reason: dev.clone(),
                },
            },
        ] {
            no_developer_text(&serde_json::to_string(&plain_job_event(event)).unwrap());
        }
        for event in [
            ModelJobEvent::Warning {
                model: None,
                message: dev.clone(),
            },
            ModelJobEvent::Failed {
                model: None,
                message: dev.clone(),
                retryable: false,
            },
        ] {
            no_developer_text(&serde_json::to_string(&plain_model_event(event)).unwrap());
        }
        // A person may say anything in a meeting: transcript and titles pass unchanged.
        let said = MeetingEvent::Landed {
            track: crate::meeting_events::LiveTrack::You,
            start_ms: 0,
            end_ms: 1_000,
            text: "run cargo xtask fetch-models".into(),
        };
        assert_eq!(plain_meeting_event(said.clone()), said);
        let titled = MeetingEvent::Summary {
            meeting: "m".into(),
            result: SummaryEvent::Stored {
                title: "cargo xtask review".into(),
                actions: 0,
                decisions: 0,
                voices_to_name: 0,
            },
        };
        assert_eq!(plain_meeting_event(titled.clone()), titled);
        // A device called Cargo is not a command.
        assert!(!names_developer_tool("Cargo Pro headset is back"));
    }

    #[test]
    fn plain_text_the_onnx_fallback_banners_say_what_runs_without_a_developer_command() {
        let dir = TempDir::new("plain-library");
        let missing = earshot_runtime::backends::init_onnx(
            &ModelDir::new(dir.0.join("models")),
            Accelerator::Cpu,
        )
        .unwrap_err();
        let banner = crate::meeting::onnx_fallback_message(Accelerator::Cuda, &missing);
        no_developer_text(&banner);
        assert!(banner.contains("it is not downloaded"), "{banner}");
        let check = crate::model_jobs::check_fallback_message(Accelerator::Cuda, &missing);
        no_developer_text(&check);
        assert!(check.contains("it is not downloaded"), "{check}");
    }

    /// With the helpers there, the copy names only the transcription model.
    #[test]
    fn plain_text_a_missing_transcription_model_names_the_helpers_only_when_they_are_missing_too() {
        assert_eq!(transcription_copy(true), TRANSCRIPTION_NEEDED);
        assert_eq!(transcription_copy(false), TRANSCRIPTION_AND_HELPERS_NEEDED);
    }

    #[test]
    fn plain_text_a_skipped_summary_points_at_the_summaries_page() {
        let dir = TempDir::new("plain-skip");
        let models = ModelDir::new(dir.0.join("models"));
        let why = host::require_models(&models, "base.en", false)
            .unwrap_err()
            .to_string();
        let copy = skip_copy(&earshot_runtime::host::SkipReason::NotInstalled {
            model: "qwen3.5-4b-q4_k_m",
            why,
        });
        no_developer_text(&copy);
        assert!(copy.contains("Settings, Summaries"), "{copy}");
        assert_eq!(earshot_runtime::catalogue::manifest_id_in(&copy), None);
    }

    /// The desktop's own sources never spell a developer command in shown text; the runtime's
    /// are caught at the exits above.
    #[test]
    fn plain_text_no_desktop_source_writes_a_developer_command_into_copy() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") || name == "plain_text.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Comments may explain; string literals may not instruct.
            for (i, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                assert!(
                    !code.contains("cargo xtask") && !code.contains("fetch-models"),
                    "{name}:{}: {line}",
                    i + 1
                );
            }
        }
    }
}
