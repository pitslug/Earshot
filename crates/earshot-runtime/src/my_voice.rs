//! "Your voice" (T7.4h): the user's own voiceprint from a short microphone sample, made only
//! when the user asks for it (opt-in, PLAN.md 3.9 #4: voiceprints are biometric data), and
//! forgotten on request.
//!
//! - [`record_sample`] gathers [`SAMPLE_LENGTH`] of the microphone track of any
//!   [`AudioSource`] (the in-room WASAPI capture in the app, a replay in tests) and returns it
//!   at 16 kHz, quantised as a recording is stored (`audio::quantise_pcm16`), so the
//!   embedding sees what the final pass would. The audio is only ever held in memory: nothing
//!   here writes it anywhere.
//! - [`enrol_my_voice`] diarises the sample with the final pass's speaker models and
//!   settings (`PostConfig`'s clustering threshold for the exact embedding model), takes the
//!   voice that holds most of the speech and adds its centroid to the user's voiceprint
//!   (`voiceprint::enroll_me`: the `is_me` person, created if there is none), in one store
//!   transaction. It refuses, storing nothing, when the model has no calibrated voiceprint
//!   threshold ([`MyVoiceOutcome::Uncalibrated`]: a voiceprint of it could never match),
//!   when no speech was heard, when another voice holds more than a third of the speech
//!   ([`MyVoiceOutcome::SeveralVoices`]; a judgement, not measured: the sample is meant to be
//!   one person reading alone, and saving someone else's voice as the user's would name them
//!   "Me" in later meetings), and when a first voiceprint would rest on less speech than
//!   `EnrolmentConfig::min_new_speech` (10 s). A later sample adds to the saved voiceprint.
//! - [`my_voice`] says whether a voiceprint of the user is saved; [`forget_my_voice`]
//!   forgets every voiceprint of every `is_me` person through `Store::forget_voiceprints`
//!   (zeroed, tombstoned, checkpointed; `edit::forget_person_voices`).

use std::time::Duration;

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, SampleRate, quantise_pcm16, resample_all};
use earshot_core::capture::{
    AudioSource, CaptureSink, DeviceTime, Discontinuity, DiscontinuityKind, EndOfStream,
    SourceStatus, TrackId, mix_to_mono,
};
use earshot_core::diar::{
    DiarError, Diarizer, EmbeddingModelId, SpeakerEmbedder, SpeakerSegmenter,
};
use earshot_core::identity::EntityId;
use earshot_core::voiceprint::{
    DEFAULT_ME_NAME, VoiceprintError, VoiceprintRepo, enroll_me, identity_of, live_prints_of,
    me_person,
};

use crate::post::PostConfig;
use crate::{RuntimeError, Services};

#[cfg(test)]
#[path = "my_voice_tests.rs"]
mod tests;

/// How much microphone audio a sample holds: enough for the 10 s of speech a first
/// voiceprint needs when the user reads the page's text at an easy pace.
pub const SAMPLE_LENGTH: Duration = Duration::from_secs(30);

/// The share of the sample's speech that may belong to voices other than the main one
/// (module docs): at most one part in [`OTHER_VOICES_DENOMINATOR`].
pub const OTHER_VOICES_DENOMINATOR: u32 = 3;

/// Collects the microphone frames of a capture session (other tracks are ignored).
struct MicSink {
    channels: std::num::NonZeroU16,
    samples: Vec<f32>,
    /// The microphone was re-opened during the sample (possibly at another rate).
    device_changed: bool,
    ended: bool,
}

impl CaptureSink for MicSink {
    fn frames(&mut self, track: TrackId, frames: &[f32], _: DeviceTime) {
        if track == TrackId::Mic && !self.device_changed {
            mix_to_mono(frames, self.channels, &mut self.samples);
        }
    }

    fn discontinuity(&mut self, track: TrackId, d: Discontinuity) {
        // A gap (lost data) does not matter for a voice; another device does.
        if track == TrackId::Mic && matches!(d.kind, DiscontinuityKind::DeviceChanged { .. }) {
            self.device_changed = true;
        }
    }

    fn end(&mut self, _: EndOfStream) {
        self.ended = true;
    }
}

/// Records up to `length` of `source`'s microphone track, then stops the source and drains
/// it to its end. `idle` is called whenever the source has nothing ready (production: a
/// short sleep; a replay never idles); `progress` after each delivery with the microphone
/// audio gathered so far. A source that ends early (a replay) gives what it had. Returns
/// `None` when `cancel` fired (the source is stopped and drained first, and the audio
/// dropped).
///
/// # Errors
/// [`RuntimeError::Capture`] for a source error, a source without a microphone track, or a
/// microphone that changed during the sample (record again); resampling errors as
/// [`RuntimeError::Capture`].
pub fn record_sample<A: AudioSource>(
    source: &mut A,
    length: Duration,
    cancel: &CancelToken,
    idle: &mut dyn FnMut(),
    progress: &mut dyn FnMut(Duration),
) -> Result<Option<MonoBuffer>, RuntimeError> {
    let info = source
        .session()
        .track(TrackId::Mic)
        .ok_or_else(|| RuntimeError::Capture("the capture has no microphone track".into()))?
        .clone();
    let rate = info.rate;
    let wanted = frames_for(rate, length);
    let mut sink = MicSink {
        channels: info.channels,
        samples: Vec::with_capacity(wanted),
        device_changed: false,
        ended: false,
    };
    let mut stopping = false;
    let mut cancelled = false;
    while !sink.ended {
        if !stopping && (sink.samples.len() >= wanted || sink.device_changed) {
            source.stop();
            stopping = true;
        }
        if !stopping && cancel.is_cancelled() {
            cancelled = true;
            source.stop();
            stopping = true;
        }
        match source.pump(&mut sink) {
            Ok(SourceStatus::Delivered) => {
                progress(duration_of(rate, sink.samples.len().min(wanted)));
            }
            Ok(SourceStatus::Idle) => idle(),
            Ok(SourceStatus::Ended) => break,
            Err(e) => {
                source.stop();
                return Err(RuntimeError::Capture(format!(
                    "the microphone failed: {e:?}"
                )));
            }
        }
    }
    if cancelled {
        return Ok(None);
    }
    if sink.device_changed {
        return Err(RuntimeError::Capture(
            "the microphone changed during the sample; record again".into(),
        ));
    }
    sink.samples.truncate(wanted);
    let at_16k = if rate == PIPELINE_RATE {
        sink.samples
    } else {
        resample_all(&sink.samples, rate, PIPELINE_RATE)
            .map_err(|e| RuntimeError::Capture(format!("cannot resample the sample: {e}")))?
    };
    Ok(Some(MonoBuffer::new(
        PIPELINE_RATE,
        quantise_pcm16(&at_16k),
    )))
}

fn frames_for(rate: SampleRate, d: Duration) -> usize {
    usize::try_from(u128::from(rate.hz()) * d.as_millis() / 1000).unwrap_or(usize::MAX)
}

fn duration_of(rate: SampleRate, frames: usize) -> Duration {
    Duration::from_millis(
        u64::try_from(frames as u128 * 1000 / u128::from(rate.hz().max(1))).unwrap_or(u64::MAX),
    )
}

/// What [`enrol_my_voice`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MyVoiceOutcome {
    /// The main voice was added to the user's voiceprint (created if needed).
    Enrolled {
        /// The user's person (`is_me`).
        person: EntityId,
        /// The voiceprint.
        voiceprint: EntityId,
        /// The speech of the main voice in this sample.
        speech: Duration,
    },
    /// No speech was heard.
    NoSpeech,
    /// Another voice holds more than a third of the speech: nothing was saved.
    SeveralVoices {
        /// The main voice's speech.
        main: Duration,
        /// Everyone else's.
        others: Duration,
    },
    /// A first voiceprint needs more speech than the sample held.
    NotEnoughSpeech {
        /// Required.
        needed: Duration,
        /// Heard.
        got: Duration,
    },
    /// The speaker model has no calibrated threshold, so its voiceprint could never match.
    Uncalibrated {
        /// The model.
        model: EmbeddingModelId,
    },
    /// The cancel token fired before anything was saved.
    Cancelled,
}

/// Saves the user's voice from `audio` (16 kHz) with `segmenter` and `embedder` (the final
/// pass's speaker models) and `config`'s diarisation, thresholds and enrolment settings
/// (module docs). The models run before the store lock is taken; the enrolment is one
/// transaction.
///
/// # Errors
/// Diarisation errors (other than a cancellation), store and voiceprint errors.
pub fn enrol_my_voice<S: SpeakerSegmenter, E: SpeakerEmbedder>(
    services: &Services,
    segmenter: S,
    embedder: E,
    audio: &MonoBuffer,
    config: &PostConfig,
    cancel: &CancelToken,
) -> Result<MyVoiceOutcome, RuntimeError> {
    let model = embedder.info().model.clone();
    let threshold = match config
        .voiceprint_threshold(&model)
        .and_then(|_| config.clustering_threshold(&model))
    {
        Ok(t) => t,
        Err(RuntimeError::Uncalibrated { model, .. }) => {
            return Ok(MyVoiceOutcome::Uncalibrated { model });
        }
        Err(e) => return Err(e),
    };
    let mut diarization = config.diarization.clone();
    diarization.clustering.threshold = threshold;
    let mut diarizer =
        Diarizer::new(segmenter, embedder, diarization).map_err(RuntimeError::Diar)?;
    let found = match diarizer.diarize(audio, MeetingTime::ZERO, cancel) {
        Ok(d) => d,
        Err(DiarError::Cancelled) => return Ok(MyVoiceOutcome::Cancelled),
        Err(e) => return Err(RuntimeError::Diar(e)),
    };
    let total: Duration = found.speakers.iter().map(|s| s.speech).sum();
    let Some(main) = found
        .speakers
        .iter()
        .filter(|s| s.centroid.is_valid() && !s.speech.is_zero())
        .max_by_key(|s| s.speech)
    else {
        return Ok(MyVoiceOutcome::NoSpeech);
    };
    let others = total.saturating_sub(main.speech);
    if others * OTHER_VOICES_DENOMINATOR > total {
        return Ok(MyVoiceOutcome::SeveralVoices {
            main: main.speech,
            others,
        });
    }
    if cancel.is_cancelled() {
        return Ok(MyVoiceOutcome::Cancelled);
    }
    crate::edit::write(services, |tx, stamp| {
        match enroll_me(
            tx,
            std::slice::from_ref(&main.centroid),
            main.speech,
            earshot_core::voiceprint::SampleOrigin::Own,
            &config.enrolment,
            stamp,
        ) {
            Ok((person, voiceprint)) => Ok(MyVoiceOutcome::Enrolled {
                person,
                voiceprint,
                speech: main.speech,
            }),
            Err(VoiceprintError::NotEnoughSpeech { needed, got }) => {
                Ok(MyVoiceOutcome::NotEnoughSpeech { needed, got })
            }
            Err(e) => Err(RuntimeError::Voiceprint(e)),
        }
    })
}

/// [`enrol_my_voice`] with the installed speaker models, as the app runs it: ONNX Runtime is
/// initialised with the library a meeting would use (`host::init_onnx_for` for the
/// transcription setup's model and order, so the process never needs a second library),
/// then the final pass's speaker models load on [`crate::host::ONNX_ACCELERATOR`] with
/// [`PostConfig::default`]. A library fallback is reported to `on_fallback`.
///
/// # Errors
/// Missing speaker models or ONNX Runtime ([`RuntimeError::ModelMissing`] and others with
/// the fetch instruction), the setup's and [`enrol_my_voice`]'s errors.
pub fn enrol_with_installed_models(
    services: &Services,
    audio: &MonoBuffer,
    cancel: &CancelToken,
    on_fallback: &mut dyn FnMut(crate::backends::Accelerator, &RuntimeError),
) -> Result<MyVoiceOutcome, RuntimeError> {
    let models = crate::host::model_dir(&services.data);
    let order = crate::host::settled_order();
    let setup = {
        let db = services.store();
        crate::host::transcription_setup(&db, &models, &order, &crate::hardware())?
    };
    crate::host::init_onnx_for(&models, &setup.choice.model, &setup.order, on_fallback)?;
    let (segmenter, embedder) =
        crate::backends::load_diarisation(&models, crate::host::ONNX_ACCELERATOR)?;
    enrol_my_voice(
        services,
        segmenter,
        embedder,
        audio,
        &PostConfig::default(),
        cancel,
    )
}

/// Whether the user's voice is saved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MyVoice {
    /// The name the user gave (T9.1h); `None` until they give one.
    pub name: Option<String>,
    /// Live voiceprints of the user (any `is_me` person, or someone merged into them).
    pub voiceprints: usize,
    /// The speech behind them.
    pub speech: Duration,
}

impl MyVoice {
    /// A voiceprint of the user is saved.
    #[must_use]
    pub fn saved(&self) -> bool {
        self.voiceprints > 0
    }
}

/// The user's saved voice (one store read).
///
/// # Errors
/// Store errors.
pub fn my_voice(services: &Services) -> Result<MyVoice, RuntimeError> {
    let db = services.store();
    let store: &earshot_store::Store = &db;
    let mut out = MyVoice::default();
    for p in store.persons().map_err(RuntimeError::Voiceprint)? {
        if !(p.is_me && p.is_live()) {
            continue;
        }
        if p.display_name != DEFAULT_ME_NAME {
            out.name = Some(p.display_name.clone());
        }
        for id in identity_of(store, p.id).map_err(RuntimeError::Voiceprint)? {
            for (_, e) in live_prints_of(store, id).map_err(RuntimeError::Voiceprint)? {
                if e.count() > 0 {
                    out.voiceprints += 1;
                    out.speech += Duration::from_millis(e.speech_ms());
                }
            }
        }
    }
    Ok(out)
}

/// Forgets every voiceprint of every person marked as the user (live or not, and everyone
/// merged into them) in one store transaction (`Store::forget_voiceprints_of`: zeroed,
/// tombstoned, then one truncating checkpoint), so either all of them are forgotten or none.
/// Returns how many voiceprints and voice samples were forgotten. A checkpoint blocked by
/// another connection after the commit (`StoreError::Busy`) still counts as forgotten: the
/// deletion is committed.
///
/// A damaged row anywhere no longer stops it (T14.57, docs/sync-spec.md 16.5 item 23a). When
/// any person row's `is_me` or `id` does not read, the user may be that person, so it also
/// starts from the persons the user's own "Your voice" samples name (each only when its own
/// `is_me` does not read `false`); a person it still cannot reach makes the answer
/// [`crate::edit::Forgot::incomplete`].
///
/// # Errors
/// Store errors (nothing was forgotten then).
pub fn forget_my_voice(services: &Services) -> Result<crate::edit::Forgot, RuntimeError> {
    services.guarded_write(forget_my_voice_in)
}

/// [`forget_my_voice`]'s root.
///
/// # Errors
/// As [`forget_my_voice`].
pub(crate) fn forget_my_voice_in(
    w: &mut crate::services::ServicesWrite<'_>,
) -> Result<crate::edit::Forgot, RuntimeError> {
    use earshot_store::{Col, Deleted};
    let (mine, live, incomplete, damaged) = {
        let r = w.read();
        let links = r.person_links()?;
        let unplaced = |l: &earshot_store::PersonLink| !l.is_me.reads() || !l.id.reads();
        let mut mine: Vec<EntityId> = links
            .iter()
            .filter(|l| l.is_me == Col::Read(true))
            .filter_map(|l| l.id.read())
            .collect();
        let trigger = links.iter().any(unplaced);
        if trigger {
            // The persons the user's own samples name, unless they read as someone else.
            for s in r.live_sample_links()? {
                let Col::Read(Some(p)) = s.person_id else {
                    continue;
                };
                let someone_else = links
                    .iter()
                    .any(|l| l.id == Col::Read(p) && l.is_me == Col::Read(false));
                if s.is_own() && !someone_else && !mine.contains(&p) {
                    mine.push(p);
                }
            }
        }
        let mut reached = std::collections::BTreeSet::new();
        let mut live = std::collections::BTreeSet::new();
        for &person in &mine {
            let members = earshot_store::identity_of_links(&links, person).members;
            let rows = r.voice_rows_of(&members)?;
            for p in rows.prints.iter().filter(|p| p.deleted == Deleted::No) {
                live.insert(("voiceprint", p.rowid));
            }
            for s in &rows.samples {
                live.insert(("voiceprint_sample", s.rowid));
            }
            reached.extend(members);
        }
        let incomplete = trigger
            && links
                .iter()
                .any(|l| unplaced(l) && !l.id.read().is_some_and(|id| reached.contains(&id)));
        (mine, live.len(), incomplete, r.damaged_skipped())
    };
    if mine.is_empty() {
        return Ok(crate::edit::Forgot {
            voices: 0,
            damaged,
            incomplete,
        });
    }
    match w.forget_voiceprints_of(&mine) {
        Ok(n) => Ok(crate::edit::Forgot {
            voices: n.total(),
            damaged: damaged.max(n.skipped),
            incomplete,
        }),
        Err(earshot_store::StoreError::Busy(_)) => Ok(crate::edit::Forgot {
            voices: live,
            damaged,
            incomplete,
        }),
        Err(e) => Err(e.into()),
    }
}

/// Gives the user's person `name` (trimmed, the rule of every person's name; empty goes back to
/// the default) in every meeting, through [`crate::edit::rename_person`]. A summary already
/// written keeps its wording until it is written again; one that used the old name reads out of
/// date (T12.82, `crate::summary_names`). Returns how many speaker rows changed.
///
/// # Errors
/// [`crate::edit::EditError`] for an unusable name; store errors.
pub fn set_my_name(services: &Services, name: &str) -> Result<usize, RuntimeError> {
    let name = if name.trim().is_empty() {
        DEFAULT_ME_NAME
    } else {
        name
    };
    let me = crate::edit::write(services, |tx, stamp| {
        me_person(tx, stamp)
            .map(|p| p.id)
            .map_err(RuntimeError::Voiceprint)
    })?;
    crate::edit::rename_person(services, me, name)
}
