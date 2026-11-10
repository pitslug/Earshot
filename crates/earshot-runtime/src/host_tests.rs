//! Host layer tests: fakes for the final pass, the loopback mock endpoint for summaries,
//! temp data directories. Every name contains `host`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use earshot_core::asr::{CancelToken, FakeTranscriber, MeetingTime, Segment, Word};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::{DeviceTime, ReplayBuilder, TrackId};
use earshot_core::diar::EmbeddingModelId;
use earshot_core::model::{
    AudioFileRepo, ClockDomain, Meeting, MeetingRepo, SegmentRow, Stamp, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::live::{LiveConfig, LiveOutcome, start_live};
use crate::post::tests::{LevelEmbedder, LevelSegmenter, levels, raw};
use crate::post::{ModelThresholds, PostConfig, PostProcessor};
use crate::testutil::{T0, TempDir, services};
use crate::{MeetingActivity, MeetingJob};

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

/// A two-track meeting recorded by a live session from a replay (no live transcription),
/// with audio the level fakes tell apart (the post round-trip test's scene).
pub(crate) fn recorded(svc: &Services, seed: u64) -> LiveOutcome {
    let mic = levels(20_000, &[(0.1, 1_000, 6_000)], seed);
    let them = levels(
        20_000,
        &[(0.4, 7_000, 12_000), (0.7, 13_000, 18_000)],
        seed + 10,
    );
    let source = ReplayBuilder::new(
        DeviceTime::from_ticks(5_000_000),
        MonoBuffer::new(PIPELINE_RATE, mic),
    )
    .reference(MonoBuffer::new(PIPELINE_RATE, them))
    .build()
    .unwrap();
    start_live(
        source,
        svc.clone(),
        LiveConfig {
            clock_domain: ClockDomain::Replay,
            idle_wait: Duration::from_millis(1),
            ..LiveConfig::default()
        },
        None,
        Box::new(ChaCha8Rng::seed_from_u64(seed + 20)),
    )
    .unwrap()
    .wait()
    .unwrap()
}

pub(crate) type Fakes = PostLoad<FakeTranscriber, LevelSegmenter, LevelEmbedder>;

/// The level fakes' final pass: one word on the mic, two on the reference.
pub(crate) fn fake_post() -> Result<Fakes, RuntimeError> {
    let model = EmbeddingModelId::new("level@1");
    let cfg = PostConfig {
        clustering_thresholds: ModelThresholds::new().with(model.clone(), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(model.clone(), 0.5),
        ..PostConfig::default()
    };
    let processor = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[("me", 3_000, 3_300)])]),
            Ok(vec![
                raw(&[("first", 9_000, 9_300)]),
                raw(&[("second", 15_000, 15_300)]),
            ]),
        ]),
        LevelSegmenter(earshot_core::diar::SegmenterInfo::pyannote_segmentation_3_0("level")),
        LevelEmbedder(earshot_core::diar::EmbedderInfo {
            model,
            dimension: 4,
            min_samples: 4000,
        }),
        cfg,
    )?;
    Ok(PostLoad {
        processor,
        asr_backend: BackendKind::Cpu,
        fallbacks: Vec::new(),
    })
}

/// [`AfterMeeting`] over an unused models directory (the fakes load nothing).
fn run<'a>(
    models: &'a ModelDir,
    summary: Option<&'a SummaryService>,
    post_cancel: &'a CancelToken,
    summary_cancel: &'a CancelToken,
) -> AfterMeeting<'a> {
    AfterMeeting {
        models,
        model: "base.en",
        order: &[Accelerator::Cpu],
        post: true,
        whole_pass: false,
        summary,
        no_summary: SkipReason::NotRequested,
        post_cancel,
        summary_cancel,
    }
}

/// A name for each step, for order checks.
fn step_name(step: &HostStep<'_>) -> String {
    match step {
        HostStep::PostLoaded { missing } => format!("loaded {missing:?}"),
        HostStep::PostStarting { .. } => "starting".into(),
        HostStep::Post(stage) => match stage {
            PostStage::Transcribed { track, .. } => format!("transcribed {track:?}"),
            PostStage::Diarised { track, .. } => format!("diarised {track:?}"),
            PostStage::EchoChecked { .. } => "echo".into(),
            PostStage::Stored { .. } => "stored".into(),
        },
        HostStep::PostFinished(r) => format!("finished ok={}", r.is_ok()),
        HostStep::SummaryStarting => "summary".into(),
    }
}

#[test]
fn host_after_meeting_runs_the_final_pass_then_the_summary() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 101);
    let meeting = outcome.meeting;
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Unprocessed),
        "recorded, no final pass yet"
    );
    let server = MockServer::start(vec![reply("Release planning")]).unwrap();
    let endpoint = server.endpoint();
    let service = SummaryService::new(
        &SummaryConfig::new(endpoint.as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let (post_cancel, summary_cancel) = (CancelToken::new(), CancelToken::new());
    let mut steps = Vec::new();
    let mut during = None;
    let result = after_meeting_with(
        &svc,
        &outcome,
        &run(&models, Some(&service), &post_cancel, &summary_cancel),
        fake_post,
        &mut |step| {
            if matches!(step, HostStep::PostStarting { .. }) {
                during = Some(svc.meeting_activity(meeting).unwrap());
            }
            steps.push(step_name(&step));
        },
    );
    assert_eq!(during, Some(Some(MeetingActivity::Processing)));
    assert!(result.missing_tracks.is_empty());
    let Ok(HostPost::Done(post)) = &result.post else {
        panic!("{:?}", result.post);
    };
    assert_eq!(post.final_segments, 3);
    // Every step in order: transcription and diarisation steps interleave (T6.12).
    assert_eq!(steps.first().map(String::as_str), Some("loaded []"));
    assert_eq!(steps[1], "starting");
    let tail: Vec<&str> = steps[steps.len() - 4..]
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(tail, ["echo", "stored", "finished ok=true", "summary"]);
    let mut middle: Vec<&str> = steps[2..steps.len() - 4]
        .iter()
        .map(String::as_str)
        .collect();
    middle.sort_unstable();
    assert_eq!(
        middle,
        [
            "diarised Mic",
            "diarised Reference",
            "transcribed Mic",
            "transcribed Reference"
        ]
    );
    let SummaryResult::Stored {
        summary_id,
        model,
        backend,
        title_set,
        outcome: summary,
    } = &result.summary
    else {
        panic!("{:?}", result.summary);
    };
    assert_eq!(model, "test-model");
    assert_eq!(*backend, SummaryBackend::Remote);
    assert!(*title_set);
    assert_eq!(summary.summary.title, "Release planning");
    // The network record: every request went to the configured endpoint.
    assert_eq!(result.network.len(), server.requests().len());
    assert!(!result.network.is_empty());
    for e in &result.network {
        assert_eq!(
            (e.host.as_str(), e.port),
            (endpoint.host_str().unwrap(), endpoint.port().unwrap())
        );
    }
    let db = svc.store();
    assert_eq!(db.summaries(meeting).unwrap()[0].id, *summary_id);
    drop(db);
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Ready)
    );
}

#[test]
fn host_after_meeting_summary_of_a_kept_local_meeting_never_reaches_the_endpoint() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 121);
    crate::local::set(
        &mut svc.store(),
        &crate::keep_local::record_key(outcome.meeting),
        &crate::keep_local::encode(),
    )
    .unwrap();
    let server = MockServer::start(vec![reply("Release planning")]).unwrap();
    // A remote service that should never have been built for this meeting (the plan makes
    // none, `keep_local_tests`): the summary step itself still refuses it.
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let (a, b) = (CancelToken::new(), CancelToken::new());
    let result = after_meeting_with(
        &svc,
        &outcome,
        &AfterMeeting {
            post: false,
            ..run(&models, Some(&service), &a, &b)
        },
        || -> Result<Fakes, RuntimeError> { panic!("the post pass must not load") },
        &mut |_| {},
    );
    assert!(
        matches!(
            &result.summary,
            SummaryResult::Failed {
                error: RuntimeError::SummaryKeptLocal(m),
                retryable: false
            } if *m == outcome.meeting
        ),
        "{:?}",
        result.summary
    );
    assert!(result.network.is_empty() && result.handovers.is_empty());
    assert!(server.requests().is_empty());
    assert!(svc.store().summaries(outcome.meeting).unwrap().is_empty());

    // The plan the app makes for it: no service at all, skipped with its own reason.
    let skipped = after_meeting_with(
        &svc,
        &outcome,
        &AfterMeeting {
            post: false,
            no_summary: SkipReason::KeptLocalNoModel {
                model: "qwen3.5-4b-q4_k_m",
            },
            ..run(&models, None, &a, &b)
        },
        || -> Result<Fakes, RuntimeError> { panic!("the post pass must not load") },
        &mut |_| {},
    );
    assert!(matches!(
        skipped.summary,
        SummaryResult::Skipped {
            reason: SkipReason::KeptLocalNoModel { .. }
        }
    ));
    assert!(skipped.network.is_empty() && skipped.handovers.is_empty());
}

#[test]
fn host_after_meeting_without_post_or_summary_does_nothing() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 111);
    let models = ModelDir::new(dir.0.join("models"));
    let (a, b) = (CancelToken::new(), CancelToken::new());
    let mut steps = Vec::new();
    let result = after_meeting_with(
        &svc,
        &outcome,
        &AfterMeeting {
            post: false,
            no_summary: SkipReason::NotInstalled {
                model: "qwen3.5-4b-q4_k_m",
                why: "not here".into(),
            },
            ..run(&models, None, &a, &b)
        },
        || -> Result<Fakes, RuntimeError> { panic!("the post pass must not load") },
        &mut |step| steps.push(step_name(&step)),
    );
    assert!(matches!(result.post, Ok(HostPost::Skipped)), "{result:?}");
    assert!(matches!(
        &result.summary,
        SummaryResult::Skipped {
            reason: SkipReason::NotInstalled {
                model: "qwen3.5-4b-q4_k_m",
                ..
            }
        }
    ));
    assert!(result.network.is_empty());
    assert_eq!(steps, ["finished ok=true"]);
    assert_eq!(
        svc.meeting_activity(outcome.meeting).unwrap(),
        Some(MeetingActivity::Unprocessed)
    );
}

#[test]
fn host_after_meeting_reports_tracks_without_a_recording() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 121);
    for t in [TrackId::Mic, TrackId::Reference] {
        let path = svc
            .data
            .resolve(&DataDir::audio_rel(outcome.meeting, t))
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }
    let models = ModelDir::new(dir.0.join("models"));
    let (a, b) = (CancelToken::new(), CancelToken::new());
    let mut steps = Vec::new();
    let result = after_meeting_with(
        &svc,
        &outcome,
        &run(&models, None, &a, &b),
        || -> Result<Fakes, RuntimeError> { panic!("nothing to load for") },
        &mut |step| steps.push(step_name(&step)),
    );
    assert!(
        matches!(result.post, Ok(HostPost::NothingToProcess)),
        "{result:?}"
    );
    assert_eq!(result.missing_tracks, [TrackId::Mic, TrackId::Reference]);
    assert_eq!(steps, ["loaded [Mic, Reference]", "finished ok=true"]);
    assert!(matches!(
        result.summary,
        SummaryResult::Skipped {
            reason: SkipReason::NotRequested
        }
    ));
}

#[test]
fn host_cancelled_post_keeps_the_draft_deletes_speaker_windows_and_still_summarises() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 131);
    let meeting = outcome.meeting;
    let key = format!("live-speakers/{meeting}/mic");
    svc.store().set_local_value(&key, b"embeddings").unwrap();
    let server = MockServer::start(vec![reply("Draft summary")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let (post_cancel, summary_cancel) = (CancelToken::new(), CancelToken::new());
    post_cancel.cancel();
    let result = after_meeting_with(
        &svc,
        &outcome,
        &run(&models, Some(&service), &post_cancel, &summary_cancel),
        fake_post,
        &mut |_| {},
    );
    let Ok(HostPost::Cancelled {
        speaker_windows_deleted,
    }) = &result.post
    else {
        panic!("{:?}", result.post);
    };
    assert_eq!(speaker_windows_deleted.as_ref().ok(), Some(&1));
    assert_eq!(svc.store().local_value(&key).unwrap(), None);
    // The recordings stay, nothing final was stored, and the summary still ran.
    assert_eq!(svc.store().audio_files(meeting).unwrap().len(), 2);
    assert!(
        matches!(result.summary, SummaryResult::Stored { .. }),
        "{:?}",
        result.summary
    );
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Unprocessed)
    );
}

#[test]
fn host_post_cancel_on_both_tokens_loads_nothing_and_asks_nothing() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 161);
    let meeting = outcome.meeting;
    let speakers = format!("live-speakers/{meeting}/mic");
    let stretches = format!("live-stretches/{meeting}/mic");
    svc.store()
        .set_local_value(&speakers, b"embeddings")
        .unwrap();
    svc.store().set_local_value(&stretches, b"pieces").unwrap();
    let server = MockServer::start(vec![reply("Never")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut steps = Vec::new();
    let result = after_meeting_with(
        &svc,
        &outcome,
        &run(&models, Some(&service), &cancel, &cancel),
        || -> Result<Fakes, RuntimeError> { panic!("a cancelled pass loads no model") },
        &mut |step| steps.push(step_name(&step)),
    );
    assert!(
        matches!(result.post, Ok(HostPost::Cancelled { .. })),
        "{:?}",
        result.post
    );
    assert!(
        matches!(result.summary, SummaryResult::Cancelled),
        "{:?}",
        result.summary
    );
    assert_eq!(steps, ["loaded []", "finished ok=true"]);
    assert!(result.network.is_empty(), "{:?}", result.network);
    // What a tray quit leaves: the recordings, the transcript records, nothing final.
    assert_eq!(svc.store().audio_files(meeting).unwrap().len(), 2);
    assert!(
        svc.store().local_value(&stretches).unwrap().is_some(),
        "the live pieces stay"
    );
    assert_eq!(
        svc.store().local_value(&speakers).unwrap(),
        None,
        "no final pass will finish from the speaker windows (biometric)"
    );
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Unprocessed)
    );
}

/// A cancel that arrives as the final pass stores its result costs nothing it stored.
#[test]
fn host_post_cancel_at_the_moment_of_storing_keeps_the_final_transcript() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 171);
    let meeting = outcome.meeting;
    let server = MockServer::start(vec![reply("Never")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let cancel = CancelToken::new();
    let mut steps = Vec::new();
    let result = after_meeting_with(
        &svc,
        &outcome,
        &run(&models, Some(&service), &cancel, &cancel),
        fake_post,
        &mut |step| {
            if matches!(step, HostStep::Post(PostStage::Stored { .. })) {
                cancel.cancel();
            }
            steps.push(step_name(&step));
        },
    );
    assert!(
        matches!(result.post, Ok(HostPost::Done(_))),
        "{:?}",
        result.post
    );
    assert!(
        matches!(result.summary, SummaryResult::Cancelled),
        "{:?}",
        result.summary
    );
    assert!(!steps.iter().any(|s| s == "summary"), "{steps:?}");
    assert!(result.network.is_empty(), "{:?}", result.network);
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Ready),
        "the final transcript stays"
    );
}

#[test]
fn host_post_a_cancellation_from_either_model_reads_as_cancelled() {
    use earshot_core::diar::DiarError;
    let (set, unset) = (CancelToken::new(), CancelToken::new());
    set.cancel();
    let asr = RuntimeError::Asr(AsrError::Cancelled);
    let diar = RuntimeError::Diar(DiarError::Cancelled);
    assert!(is_cancelled_pass(&asr, &set));
    assert!(is_cancelled_pass(&diar, &set), "the echo check's embedder");
    assert!(!is_cancelled_pass(&diar, &unset), "nobody asked");
    let failure = RuntimeError::Internal("models broke");
    assert!(!is_cancelled_pass(&failure, &set), "a failure stays one");
}

#[test]
fn host_post_cancelled_before_its_turn_reads_as_a_cancelled_pass() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 181);
    let meeting = outcome.meeting;
    let speakers = format!("live-speakers/{meeting}/mic");
    svc.store()
        .set_local_value(&speakers, b"embeddings")
        .unwrap();
    let result = HostOutcome::cancelled(&svc, meeting);
    assert!(
        matches!(result.post, Ok(HostPost::Cancelled { .. })),
        "{:?}",
        result.post
    );
    assert!(matches!(result.summary, SummaryResult::Cancelled));
    assert_eq!(svc.store().local_value(&speakers).unwrap(), None);
    assert_eq!(svc.store().audio_files(meeting).unwrap().len(), 2);
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Unprocessed)
    );
}

#[test]
fn host_post_failure_stops_before_the_summary() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 141);
    let server = MockServer::start(vec![reply("Never")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let (a, b) = (CancelToken::new(), CancelToken::new());
    let mut steps = Vec::new();
    let result = after_meeting_with(
        &svc,
        &outcome,
        &run(&models, Some(&service), &a, &b),
        || -> Result<Fakes, RuntimeError> { Err(RuntimeError::Internal("models broke")) },
        &mut |step| steps.push(step_name(&step)),
    );
    assert!(
        matches!(result.post, Err(RuntimeError::Internal("models broke"))),
        "{result:?}"
    );
    assert!(matches!(
        result.summary,
        SummaryResult::Skipped {
            reason: SkipReason::PostFailed
        }
    ));
    assert!(result.network.is_empty());
    assert!(server.requests().is_empty());
    assert_eq!(steps, ["loaded []", "finished ok=false"]);
}

#[test]
fn host_summary_failure_and_cancellation_are_reported_not_raised() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let meeting = crate::testutil::meeting(&svc, "", 151);
    let server = MockServer::start(vec![MockResponse::status(500, "upstream down")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let r = run_summary(&svc, meeting, &service, &CancelToken::new());
    let SummaryResult::Failed { error, retryable } = &r else {
        panic!("{r:?}");
    };
    assert!(!retryable, "{error}");
    assert!(svc.store().summaries(meeting).unwrap().is_empty());

    let server = MockServer::start(vec![reply("Never")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let cancel = CancelToken::new();
    cancel.cancel();
    let r = run_summary(&svc, meeting, &service, &cancel);
    assert!(matches!(r, SummaryResult::Cancelled), "{r:?}");
    assert!(svc.store().summaries(meeting).unwrap().is_empty());
}

#[test]
fn host_summary_failure_with_a_cancelled_token_is_still_a_failure() {
    // Only the cancellation error is `Cancelled`. A failing endpoint cannot show this: the
    // LLM client checks the token first (a biased select), so a cancelled token always ends
    // the request as `LlmError::Cancelled`. A failure before any request (the meeting is
    // gone) with the token cancelled must stay a failure.
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let server = MockServer::start(vec![MockResponse::status(500, "upstream down")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let cancel = CancelToken::new();
    cancel.cancel();
    let r = run_summary(&svc, EntityId::from_bytes([0xAB; 16]), &service, &cancel);
    let SummaryResult::Failed { error, retryable } = &r else {
        panic!("{r:?}");
    };
    assert!(
        matches!(error, RuntimeError::Internal("no such meeting")),
        "{error:?}"
    );
    assert!(!retryable);
    assert!(server.requests().is_empty());
}

#[test]
fn host_after_meeting_refused_while_the_meeting_is_deleted_runs_nothing() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 181);
    let server = MockServer::start(vec![reply("Never")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let (a, b) = (CancelToken::new(), CancelToken::new());
    let claim = svc.begin_deletion(outcome.meeting).unwrap();
    let mut steps = Vec::new();
    let result = after_meeting_with(
        &svc,
        &outcome,
        &run(&models, Some(&service), &a, &b),
        || -> Result<Fakes, RuntimeError> { panic!("nothing may load") },
        &mut |step| steps.push(step_name(&step)),
    );
    drop(claim);
    assert!(
        matches!(&result.post, Err(RuntimeError::MeetingBusy(m)) if *m == outcome.meeting),
        "{result:?}"
    );
    assert!(steps.is_empty(), "{steps:?}");
    assert!(matches!(
        result.summary,
        SummaryResult::Skipped {
            reason: SkipReason::PostFailed
        }
    ));
    assert!(result.network.is_empty());
    assert!(server.requests().is_empty());
}

#[test]
fn host_progress_may_read_the_store_when_the_final_pass_reports_stored() {
    // The app's progress handler reads the store; `Stored` must not be reported under the
    // store lock. A regression deadlocks the pass, so it runs on its own thread and the
    // test fails at the deadline instead of hanging.
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 191);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = svc.clone();
    std::thread::spawn(move || {
        let models = ModelDir::new(std::env::temp_dir().join("earshot-host-unused-models"));
        let (a, b) = (CancelToken::new(), CancelToken::new());
        let mut at_stored = None;
        let result = after_meeting_with(
            &worker,
            &outcome,
            &run(&models, None, &a, &b),
            fake_post,
            &mut |step| {
                if matches!(step, HostStep::Post(PostStage::Stored { .. })) {
                    at_stored = Some(worker.meeting_activity(outcome.meeting).unwrap());
                }
            },
        );
        let _ = tx.send((matches!(result.post, Ok(HostPost::Done(_))), at_stored));
    });
    let (done, at_stored) = rx
        .recv_timeout(Duration::from_secs(120))
        .expect("the final pass deadlocked reporting Stored under the store lock");
    assert!(done);
    assert_eq!(at_stored, Some(Some(MeetingActivity::Processing)));
}

/// A meeting row as a live session writes it at the start (no duration yet), with one
/// track and one live segment, and no lease: what a crash during the meeting leaves.
fn crashed_meeting(svc: &Services, seed: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let m = Meeting::new("", T0, 0, Template::General, &mut stamp);
    let track = Track::new(m.id, TrackId::Mic, None, ClockDomain::Qpc, &mut stamp);
    let seg = Segment::from_words(
        EntityId::generate(stamp.clock, &mut *stamp.rng),
        track.id,
        vec![Word::new("hello", 1_000, 1_300, 0.9)],
        MeetingTime::ZERO,
    );
    store.put_meeting(m.clone()).unwrap();
    store.put_track(track).unwrap();
    let row = SegmentRow::new(m.id, TranscriptPass::Live, seg, stamp.hlc, stamp.clock);
    store.put_segment(row).unwrap();
    m.id
}

#[test]
fn host_meeting_activity_after_a_crash_and_through_its_life() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    let meeting = crashed_meeting(&svc, 161);
    // A crash: rows written, no duration, nothing holds the meeting.
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Interrupted)
    );
    // Leases of this process decide while held, recording first; an export does not count.
    {
        let _export = svc.lease_meeting(meeting).unwrap();
        assert_eq!(
            svc.meeting_activity(meeting).unwrap(),
            Some(MeetingActivity::Interrupted)
        );
        let processing = svc
            .lease_meeting_for(meeting, MeetingJob::Processing)
            .unwrap();
        assert_eq!(
            svc.meeting_activity(meeting).unwrap(),
            Some(MeetingActivity::Processing)
        );
        let recording = svc
            .lease_meeting_for(meeting, MeetingJob::Recording)
            .unwrap();
        assert_eq!(
            svc.meeting_activity(meeting).unwrap(),
            Some(MeetingActivity::Recording)
        );
        drop(recording);
        assert_eq!(
            svc.meeting_activity(meeting).unwrap(),
            Some(MeetingActivity::Processing)
        );
        drop(processing);
    }
    // Recorded to the end, live draft only.
    let set = |f: &dyn Fn(&mut Meeting)| {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut m = db.meeting(meeting).unwrap().unwrap();
        f(&mut m);
        let (store, hlc) = db.parts();
        m.meta.touch(hlc, &*clock);
        store.put_meeting(m).unwrap();
    };
    set(&|m| m.duration_ms = Some(60_000));
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Unprocessed)
    );
    // A final pass that found no speech still ran: its backend is recorded.
    let backend = earshot_core::asr::BackendInfo {
        kind: BackendKind::Cpu,
        device: "cpu".into(),
        model: "fake".into(),
    };
    crate::backends::record_pass_backend(
        &mut svc.store(),
        meeting,
        TranscriptPass::Final,
        None,
        &backend,
    )
    .unwrap();
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Ready)
    );
    // Deleted or unknown: none.
    set(&|m| {
        m.meta.deleted = true;
    });
    assert_eq!(svc.meeting_activity(meeting).unwrap(), None);
    assert_eq!(
        svc.meeting_activity(EntityId::from_bytes([9; 16])).unwrap(),
        None
    );
}

#[test]
fn host_meeting_activity_is_ready_with_a_final_transcript() {
    let dir = TempDir::new("host");
    let (svc, _) = services(&dir);
    // A finished meeting with final segments (and no recorded backend).
    let meeting = crate::testutil::meeting(&svc, "Done", 171);
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Ready)
    );
}

#[test]
fn host_model_choice_follows_the_request_then_a_current_check() {
    let dir = TempDir::new("host");
    let store = Store::open(&dir.0.join("earshot.db")).unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    assert_eq!(
        choose_model(Some("tiny.en"), &store, &models, DEFAULT_LANGUAGES).unwrap(),
        (
            ModelChoice {
                model: "tiny.en".into(),
                backend: None
            },
            ModelSource::Requested
        )
    );
    assert_eq!(
        choose_model(None, &store, &models, DEFAULT_LANGUAGES).unwrap(),
        (
            ModelChoice {
                model: FALLBACK_MODEL.into(),
                backend: None
            },
            ModelSource::Fallback("no model check yet")
        )
    );
    let fingerprint = check_fingerprint(&models, &hardware_facts(), &["en"]);
    let check = |fingerprint: String| crate::model_bench::StoredCheck {
        fingerprint,
        model: "parakeet-tdt-110m".into(),
        backend: BackendKind::Cpu,
        fallback: Some("base.en".into()),
        trials: Vec::new(),
    };
    let mut store = store;
    crate::model_bench::record_check(&mut store, &check(fingerprint)).unwrap();
    let (choice, source) = choose_model(None, &store, &models, DEFAULT_LANGUAGES).unwrap();
    assert_eq!(source, ModelSource::ModelCheck);
    assert_eq!(choice.backend, Some(BackendKind::Cpu));
    assert_eq!(
        choice.order(&[Accelerator::Cuda, Accelerator::Cpu]),
        [Accelerator::Cpu]
    );
    crate::model_bench::record_check(&mut store, &check("old".into())).unwrap();
    let (choice, source) = choose_model(None, &store, &models, DEFAULT_LANGUAGES).unwrap();
    assert_eq!(choice.model, FALLBACK_MODEL);
    assert_eq!(
        source,
        ModelSource::Fallback("the model check is out of date for this machine")
    );
}

#[test]
fn host_model_choice_reads_the_check_for_the_meeting_languages() {
    let dir = TempDir::new("host");
    let mut store = Store::open(&dir.0.join("earshot.db")).unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let check = crate::model_bench::StoredCheck {
        fingerprint: check_fingerprint(&models, &hardware_facts(), &["de", "en"]),
        model: "parakeet-tdt-0.6b-v3".into(),
        backend: BackendKind::Cpu,
        fallback: None,
        trials: Vec::new(),
    };
    crate::model_bench::record_check(&mut store, &check).unwrap();
    // Asked with the languages the check ran for (in any order or case): it holds.
    let (choice, source) = choose_model(None, &store, &models, &["EN", "de"]).unwrap();
    assert_eq!(source, ModelSource::ModelCheck);
    assert_eq!(choice.model, "parakeet-tdt-0.6b-v3");
    assert_eq!(choice.backend, Some(BackendKind::Cpu));
    // English meetings: a check for German and English says nothing about them.
    let (choice, source) = choose_model(None, &store, &models, DEFAULT_LANGUAGES).unwrap();
    assert_eq!(choice.model, FALLBACK_MODEL);
    assert_eq!(
        source,
        ModelSource::Fallback("the model check is out of date for this machine")
    );
}

#[test]
fn host_require_models_needs_the_post_models_only_with_the_post_pass() {
    let dir = TempDir::new("host");
    let whisper = asr_model("tiny.en").unwrap().files().remove(0);
    for f in [whisper.as_str(), SILERO] {
        let p = dir.0.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }
    let models = ModelDir::new(&dir.0);
    require_models(&models, "tiny.en", false).unwrap();
    let e = require_models(&models, "tiny.en", true).unwrap_err();
    assert!(matches!(e, RuntimeError::ModelMissing { .. }), "{e:?}");
    assert!(matches!(
        require_models(&models, "no-such-model", false),
        Err(RuntimeError::UnknownModel(_))
    ));
}

#[test]
fn host_live_summaries_setting_says_where_it_came_from_and_needs_a_local_model() {
    let dir = TempDir::new("host");
    let mut store = Store::open(&dir.0.join("earshot.db")).unwrap();
    assert_eq!(
        choose_live_summaries(&mut store, None, false).unwrap(),
        (false, TimingSource::Default)
    );
    assert_eq!(
        choose_live_summaries(&mut store, Some(true), false).unwrap(),
        (true, TimingSource::Requested)
    );
    assert_eq!(
        choose_live_summaries(&mut store, None, false).unwrap(),
        (false, TimingSource::Default),
        "not saved"
    );
    choose_live_summaries(&mut store, Some(true), true).unwrap();
    assert_eq!(
        choose_live_summaries(&mut store, None, false).unwrap(),
        (true, TimingSource::Stored)
    );

    // Only a local summary model summarises during the meeting.
    let order = [crate::backends::Accelerator::Cpu];
    assert_eq!(
        live_summaries_for(None, &order).unwrap_err(),
        LIVE_SUMMARIES_NEED_LOCAL
    );
    let remote = PreparedSummary {
        service: SummaryService::new(
            &SummaryConfig::new("http://127.0.0.1:9/v1", "m"),
            Arc::new(earshot_core::identity::ManualClock::new(T0)),
        )
        .unwrap(),
        local: None,
    };
    assert_eq!(
        live_summaries_for(Some(&remote), &order).unwrap_err(),
        LIVE_SUMMARIES_NEED_LOCAL
    );
    let choice = crate::summary_model::choose_summary_model(None, Some(8 << 30), false).unwrap();
    let mut config = crate::summary::LocalSummaryConfig::new("unused.gguf", choice.model.id);
    config.chunk_seconds = 600;
    let local = PreparedSummary {
        service: SummaryService::local(&config).unwrap(),
        local: Some((choice, false)),
    };
    let live = live_summaries_for(Some(&local), &order).unwrap();
    assert_eq!(live.mapper.model_id(), choice.model.id);
    assert_eq!(live.mapper.seconds(), 600);
    assert!(live.background, "production runs it below normal priority");
}

#[test]
fn host_speaker_timing_says_where_it_came_from() {
    let dir = TempDir::new("host");
    let mut store = Store::open(&dir.0.join("earshot.db")).unwrap();
    assert_eq!(
        choose_speaker_timing(&mut store, None, false).unwrap(),
        (SpeakerTiming::DuringCall, TimingSource::Default)
    );
    assert_eq!(
        choose_speaker_timing(&mut store, Some(SpeakerTiming::AfterCall), false).unwrap(),
        (SpeakerTiming::AfterCall, TimingSource::Requested)
    );
    assert_eq!(
        choose_speaker_timing(&mut store, None, false).unwrap().1,
        TimingSource::Default,
        "not saved"
    );
    assert_eq!(
        choose_speaker_timing(&mut store, Some(SpeakerTiming::AfterCall), true).unwrap(),
        (SpeakerTiming::AfterCall, TimingSource::RequestedAndSaved)
    );
    assert_eq!(
        choose_speaker_timing(&mut store, None, false).unwrap(),
        (SpeakerTiming::AfterCall, TimingSource::Stored)
    );
    store
        .set_local_value(crate::speaker_timing::SPEAKER_TIMING_KEY, b"after-on-solar")
        .unwrap();
    assert_eq!(
        choose_speaker_timing(&mut store, None, false).unwrap(),
        (
            SpeakerTiming::DuringCall,
            TimingSource::UnknownStored("after-on-solar".into())
        )
    );
}

/// A models directory with `ids`' summary model files at their manifest size (sparse).
fn summary_models(dir: &TempDir, ids: &[&str]) -> ModelDir {
    for id in ids {
        let m = crate::summary_model::SUMMARY_MODELS
            .iter()
            .find(|m| m.id == *id)
            .unwrap();
        let path = dir.0.join(m.file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(m.size)
            .unwrap();
    }
    ModelDir::new(&dir.0)
}

#[test]
fn host_summary_plan_prefers_an_endpoint_then_an_installed_local_model() {
    const GIB: u64 = 1 << 30;
    let dir = TempDir::new("host");
    let none = summary_models(&dir, &[]);
    let remote = SummaryConfig::new("http://127.0.0.1:9/v1", "m");
    // On the GPU, where the memory rule picks the model (T8.3f).
    let plan =
        |r: &SummaryRequest, memory, models: &ModelDir| summary_plan(r, memory, true, models);
    let request = SummaryRequest {
        remote: Some(remote),
        ..SummaryRequest::default()
    };
    assert!(matches!(
        plan(&request, Some(64 * GIB), &none),
        Ok(SummaryPlan::Remote(_))
    ));
    let skip = SummaryRequest {
        skip: true,
        ..SummaryRequest::default()
    };
    let p = plan(&skip, Some(64 * GIB), &none).unwrap();
    assert!(matches!(p, SummaryPlan::Skip));
    assert_eq!(skip_reason(&p), Some(SkipReason::NotRequested));
    // The default model is not installed: no summary, with the fetch command.
    let p = plan(&SummaryRequest::default(), Some(64 * GIB), &none).unwrap();
    let SummaryPlan::NotInstalled(choice, why) = &p else {
        panic!("{p:?}");
    };
    assert_eq!(choice.model.id, "qwen3.5-9b-q4_k_m");
    assert!(why.contains("fetch-models --set summary"), "{why}");
    assert!(matches!(
        skip_reason(&p),
        Some(SkipReason::NotInstalled {
            model: "qwen3.5-9b-q4_k_m",
            ..
        })
    ));
    // Installed: used.
    let dir9 = TempDir::new("host");
    let nine = summary_models(&dir9, &["qwen3.5-9b-q4_k_m"]);
    let p = plan(&SummaryRequest::default(), Some(15 * GIB), &nine).unwrap();
    assert!(matches!(&p, SummaryPlan::Local(c) if c.model.id == "qwen3.5-9b-q4_k_m"));
    assert_eq!(skip_reason(&p), None);
    // The same machine summarising on the CPU wants the 4B, which is not installed.
    let p = summary_plan(&SummaryRequest::default(), Some(15 * GIB), false, &nine).unwrap();
    assert!(
        matches!(&p, SummaryPlan::NotInstalled(c, _)
            if c.model.id == "qwen3.5-4b-q4_k_m" && c.reason == crate::summary_model::ChoiceReason::OnTheCpu),
        "{p:?}"
    );
    // Asked for by name: must exist and be installed.
    let named = |id: &str| SummaryRequest {
        local_model: Some(id.into()),
        ..SummaryRequest::default()
    };
    assert!(matches!(
        plan(&named("no-such-model"), None, &nine),
        Err(SummaryPlanError::UnknownModel(_))
    ));
    assert!(matches!(
        plan(&named("qwen3.5-4b-q4_k_m"), None, &nine),
        Err(SummaryPlanError::NotInstalled(_))
    ));
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "the llama DLL check is Windows-only; elsewhere it would call llama.cpp"
)]
fn host_prepare_summary_checks_the_llama_dlls_before_any_llama_call() {
    let dir = TempDir::new("host");
    let models = summary_models(&dir, &["qwen3.5-4b-q4_k_m"]);
    let plan = summary_plan(&SummaryRequest::default(), Some(4 << 30), false, &models).unwrap();
    assert!(matches!(plan, SummaryPlan::Local(_)), "{plan:?}");
    let clock: SharedClock = Arc::new(earshot_core::identity::ManualClock::new(T0));
    let empty: Vec<PathBuf> = vec![dir.0.join("no-dlls-here")];
    let e = prepare_summary(&plan, &models, clock.clone(), &empty).unwrap_err();
    let SummarySetupError::LlamaDllsMissing(missing) = e else {
        panic!("{e:?}");
    };
    assert_eq!(missing, LLAMA_DLLS);
    // Plans without a summary build nothing; a remote plan needs no DLL.
    assert!(
        prepare_summary(&SummaryPlan::Skip, &models, clock.clone(), &empty)
            .unwrap()
            .is_none()
    );
    let remote = SummaryPlan::Remote(SummaryConfig::new("http://127.0.0.1:9/v1", "m"));
    let prepared = prepare_summary(&remote, &models, clock, &empty)
        .unwrap()
        .unwrap();
    assert!(prepared.local.is_none());
    assert_eq!(prepared.service.model(), "m");
}

#[test]
fn host_transcription_uses_vulkan_only_when_the_check_kept_it() {
    use Accelerator::{Cpu, Cuda, Vulkan};
    let choice = |backend| ModelChoice {
        model: "base.en".to_owned(),
        backend,
    };
    // No check on this machine: never Vulkan, as the shipped x64 app must behave out of the box.
    assert_eq!(choice(None).order(&[Vulkan, Cpu]), [Cpu]);
    // A build's other accelerators are untouched.
    assert_eq!(choice(None).order(&[Cuda, Cpu]), [Cuda, Cpu]);
    assert_eq!(choice(None).order(&[Cuda, Vulkan, Cpu]), [Cuda, Cpu]);
    // The check's answer stands, whichever way it went.
    assert_eq!(
        choice(Some(BackendKind::Vulkan)).order(&[Vulkan, Cpu]),
        [Vulkan, Cpu]
    );
    assert_eq!(choice(Some(BackendKind::Cpu)).order(&[Vulkan, Cpu]), [Cpu]);
}

/// A Claude Code stand-in whose `--help` fails and whose runs print `stdout`: a batch file run
/// by `cmd /c call` (T12.20).
#[cfg(windows)]
fn claude_help_fails(dir: &TempDir, name: &str, stdout: &str) -> earshot_llm::ClaudeCli {
    let reply = dir.0.join(format!("{name}.json"));
    std::fs::write(&reply, stdout).unwrap();
    let bat = dir.0.join(format!("{name}.bat"));
    std::fs::write(
        &bat,
        format!(
            "@if \"%1\"==\"--help\" exit /b 1\r\n@type \"{}\"\r\n",
            reply.display()
        ),
    )
    .unwrap();
    earshot_llm::ClaudeCli {
        program: "cmd".into(),
        leading_args: vec!["/c".into(), "call".into(), bat.into_os_string()],
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "the Claude Code stand-in is cmd.exe")]
fn host_after_meeting_says_claude_code_used_its_default_effort_only_for_a_stored_summary() {
    #[cfg(windows)]
    {
        let dir = TempDir::new("host");
        let (svc, _) = services(&dir);
        let models = ModelDir::new(dir.0.join("models"));
        let summary = SummaryV1 {
            title: "Invented planning".into(),
            tldr: "An invented summary.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        };
        let ok = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "result": serde_json::to_string(&summary).unwrap(), "stop_reason": "end_turn",
        })
        .to_string();
        let service = |name: &str, stdout: &str| {
            let config = crate::claude_code::ClaudeCodeSummaryConfig {
                model: None,
                effort: Some(earshot_llm::claude_code::Effort::High),
                cli: Some(claude_help_fails(&dir, name, stdout)),
                request_timeout: None,
            };
            SummaryService::claude_code(&config, svc.clock.clone()).unwrap()
        };
        let after = |service: &SummaryService, seed: u64, cancel: &CancelToken| {
            let outcome = recorded(&svc, seed);
            let a = CancelToken::new();
            after_meeting_with(
                &svc,
                &outcome,
                &run(&models, Some(service), &a, cancel),
                fake_post,
                &mut |_| {},
            )
        };
        // Stored: the summary was written at the default effort, and that is said.
        let stored = service("ok", &ok);
        let result = after(&stored, 131, &CancelToken::new());
        assert!(
            matches!(result.summary, SummaryResult::Stored { .. }),
            "{:?}",
            result.summary
        );
        assert!(result.claude_effort_unread);
        // A failed run with the same unreadable help: no summary, so nothing to say.
        let failing = service(
            "fails",
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"invented failure"}"#,
        );
        let result = after(&failing, 141, &CancelToken::new());
        assert!(
            matches!(result.summary, SummaryResult::Failed { .. }),
            "{:?}",
            result.summary
        );
        assert!(
            failing.claude_effort_unread(),
            "the help was asked and unread"
        );
        assert!(!result.claude_effort_unread);
        // Cancelled, on a service whose help was already found unreadable: nothing either.
        let cancel = CancelToken::new();
        cancel.cancel();
        let result = after(&stored, 151, &cancel);
        assert!(matches!(result.summary, SummaryResult::Cancelled));
        assert!(stored.claude_effort_unread());
        assert!(!result.claude_effort_unread);
        // The rule the re-summary job uses too.
        assert!(!claude_effort_unread(&stored, &SummaryResult::Cancelled));
        assert!(!claude_effort_unread(
            &failing,
            &SummaryResult::Skipped {
                reason: SkipReason::NotRequested
            }
        ));
    }
}
