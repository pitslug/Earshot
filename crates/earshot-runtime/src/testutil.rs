//! Test-only helpers shared by the summary and export tests.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use earshot_core::asr::{Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, Note, SegmentRow, Speaker, Stamp, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_store::{DataDir, Store};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use crate::Services;

/// Start of every test clock.
pub const T0: u64 = 1_792_294_800_000;

/// A temp directory removed on drop (std only; no tempfile crate, AGENTS.md).
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-runtime-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How far a buffer may lie from its timeline and still be found: more than 16-bit
/// quantisation (half an LSB, 1.5e-5), so a FLAC copy or the 16-bit samples a stretch
/// worker decodes (T6.13e) are found too.
pub const LOCATE_TOLERANCE: f32 = 1e-4;

/// Where `needle` lies in `haystack` within [`LOCATE_TOLERANCE`], checked first at its
/// loudest sample.
#[must_use]
pub fn locate_close(haystack: &[f32], needle: &[f32]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let m = (0..needle.len())
        .max_by(|&a, &b| needle[a].abs().total_cmp(&needle[b].abs()))
        .unwrap_or(0);
    (0..=haystack.len() - needle.len()).find(|&i| {
        (haystack[i + m] - needle[m]).abs() <= LOCATE_TOLERANCE
            && needle
                .iter()
                .zip(&haystack[i..])
                .all(|(a, b)| (a - b).abs() <= LOCATE_TOLERANCE)
    })
}

/// [`earshot_core::asr::FakeTranscriber::timeline`] that finds each buffer with
/// [`locate_close`] and hands the timeline's own samples there to the fake, so a buffer
/// that went through 16-bit quantisation (a stretch worker, T6.13e) gets the same answer
/// as the original samples.
pub struct Timeline {
    audio: Vec<f32>,
    inner: earshot_core::asr::FakeTranscriber,
}

/// A [`Timeline`] over `audio` with the reference `words`.
#[must_use]
pub fn timeline(audio: Vec<f32>, words: Vec<Word>) -> Timeline {
    Timeline {
        inner: earshot_core::asr::FakeTranscriber::timeline(audio.clone(), words),
        audio,
    }
}

impl earshot_core::asr::Transcriber for Timeline {
    fn transcribe(
        &mut self,
        audio: &earshot_core::audio::MonoBuffer,
        options: &earshot_core::asr::TranscribeOptions,
        cancel: &earshot_core::asr::CancelToken,
    ) -> Result<earshot_core::asr::Transcription, earshot_core::asr::AsrError> {
        let exact = match locate_close(&self.audio, &audio.samples) {
            Some(at) => earshot_core::audio::MonoBuffer::new(
                audio.rate,
                self.audio[at..at + audio.samples.len()].to_vec(),
            ),
            None => audio.clone(),
        };
        self.inner.transcribe(&exact, options, cancel)
    }

    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        self.inner.backend_info()
    }
}

/// Services over a fresh store in `dir`, with a clock the test can move.
pub fn services(dir: &TempDir) -> (Services, Arc<ManualClock>) {
    let data = DataDir::new(dir.0.join("data"));
    data.create().unwrap();
    let store = Store::open(&data.db_path()).unwrap();
    let clock = Arc::new(ManualClock::new(T0));
    let svc = Services::new(store, data, clock.clone()).unwrap();
    (svc, clock)
}

/// A finished two-track meeting: "Me" and "Alice", a final transcript, one note.
pub fn meeting(svc: &Services, title: &str, seed: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new(title, T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let them = Track::new(
        m.id,
        TrackId::Reference,
        None,
        ClockDomain::Replay,
        &mut stamp,
    );
    let me = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp);
    let alice = Speaker::new(m.id, SpeakerRole::Remote, "Alice", None, &mut stamp);
    let seg =
        |track: &Track, speaker: &Speaker, text: &str, start: u64, stamp: &mut Stamp<'_, _, _>| {
            let words: Vec<Word> = text
                .split(' ')
                .enumerate()
                .map(|(i, w)| {
                    Word::new(w, start + 400 * i as u64, start + 400 * i as u64 + 300, 0.9)
                })
                .collect();
            let mut s = Segment::from_words(
                EntityId::generate(stamp.clock, &mut *stamp.rng),
                track.id,
                words,
                earshot_core::asr::MeetingTime::from_millis(start),
            );
            s.speaker_id = Some(speaker.id);
            s
        };
    let segments = vec![
        seg(&mic, &me, "shall we ship on friday", 1_000, &mut stamp),
        seg(
            &them,
            &alice,
            "yes friday works for the release",
            5_000,
            &mut stamp,
        ),
    ];
    let note = Note::new(
        m.id,
        earshot_core::asr::MeetingTime::from_millis(4_000),
        "check the release notes",
        &mut stamp,
    );
    store.put_meeting(m.clone()).unwrap();
    store.put_track(mic).unwrap();
    store.put_track(them).unwrap();
    store.put_speaker(me).unwrap();
    store.put_speaker(alice).unwrap();
    for s in segments {
        let row = SegmentRow::new(m.id, TranscriptPass::Final, s, stamp.hlc, stamp.clock);
        store.put_segment(row).unwrap();
    }
    store.put_note(note).unwrap();
    m.id
}
