//! FLAC audio files, data-dir layout, retention and crash rules (PLAN.md 3.6).

mod common;

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::audio::f32_to_i16;
use earshot_core::capture::TrackId;
use earshot_core::identity::{Clock, DeviceId, EntityId, Hlc, ManualClock};
use earshot_core::model::{
    AudioFileRepo, ClockDomain, Meeting, MeetingRepo, SegmentRow, Stamp, Track, TranscriptPass,
    live_segments,
};
use earshot_core::prompt::Template;
use earshot_store::{
    AudioProvenance, DAY_MS, DataDir, FlacTrackWriter, HOUR_MS, RETENTION_CLOCK_PREFIX,
    RETENTION_CLOCK_VERSION, RetentionClock, RetentionPolicy, Store, StoreError, WriteFault,
    decode_retention_clock, due_audio, encode_retention_clock, load_meeting_audio, read_flac,
    remove_audio, retention_clock, retention_clock_key, run_retention,
};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

struct World {
    _tmp: TempDir,
    data: DataDir,
    store: Store,
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl World {
    fn new(tag: &str) -> Self {
        let tmp = TempDir::new(tag);
        let data = DataDir::new(tmp.path().join("Earshot"));
        data.create().unwrap();
        let store = Store::open(&data.db_path()).unwrap();
        World {
            _tmp: tmp,
            data,
            store,
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([5; 16])),
            rng: ChaCha8Rng::seed_from_u64(9),
        }
    }

    fn stamp(&mut self) -> Stamp<'_, ManualClock, ChaCha8Rng> {
        self.clock.advance_ms(1);
        Stamp {
            clock: &self.clock,
            hlc: &mut self.hlc,
            rng: &mut self.rng,
        }
    }

    fn meeting_with(&mut self, kinds: &[TrackId]) -> (Meeting, Vec<Track>) {
        let m = Meeting::new("", 1, 0, Template::General, &mut self.stamp());
        self.store.put_meeting(m.clone()).unwrap();
        let mut tracks = Vec::new();
        for &k in kinds {
            let t = Track::new(m.id, k, None, ClockDomain::Qpc, &mut self.stamp());
            self.store.put_track(t.clone()).unwrap();
            tracks.push(t);
        }
        (m, tracks)
    }

    fn record(
        &mut self,
        m: &Meeting,
        t: &Track,
        samples: &[f32],
    ) -> earshot_core::model::AudioFile {
        let mut w = self.data.track_writer(m.id, t.kind, 16_000).unwrap();
        w.write(samples).unwrap();
        self.store
            .finalize_track_audio(w, &self.data, t, provenance(), &self.clock, &mut self.rng)
            .unwrap()
    }
}

fn provenance() -> AudioProvenance {
    AudioProvenance {
        pipeline_version: "earshot-0.1.0".into(),
        aec_version: Some("aec-1".into()),
    }
}

fn sine(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.6 * (i as f32 * 0.031).sin() + 0.2 * (i as f32 * 0.17).cos())
        .collect()
}

#[test]
fn layout_paths_follow_plan_3_6() {
    let data = DataDir::new("C:/x/Earshot");
    assert_eq!(
        data.db_path(),
        std::path::Path::new("C:/x/Earshot").join("earshot.db")
    );
    assert_eq!(
        data.models_dir(),
        std::path::Path::new("C:/x/Earshot").join("models")
    );
    assert_eq!(
        data.logs_dir(),
        std::path::Path::new("C:/x/Earshot").join("logs")
    );
    let id: EntityId = "01920000-0000-7000-8000-000000000001".parse().unwrap();
    assert_eq!(
        DataDir::audio_rel(id, TrackId::Mic),
        "audio/01920000-0000-7000-8000-000000000001/mic.flac"
    );
    assert_eq!(
        DataDir::audio_rel(id, TrackId::Reference),
        "audio/01920000-0000-7000-8000-000000000001/loopback.flac"
    );
    for bad in ["../x.flac", "/abs.flac", "C:/abs.flac", "audio/../../x", ""] {
        assert!(data.resolve(bad).is_err(), "{bad:?} must not resolve");
    }
}

#[test]
fn flac_writer_round_trips_samples_exactly_at_16_bit() {
    let tmp = TempDir::new("flac");
    let path = tmp.path().join("t.flac");
    let input = sine(4096 * 3 + 1234);
    let mut w = FlacTrackWriter::create(&path, 16_000).unwrap();
    // Uneven chunks across block boundaries.
    for chunk in input.chunks(1000) {
        w.write(chunk).unwrap();
    }
    w.write(&[2.0, -2.0]).unwrap(); // clipped, not wrapped
    let done = w.finish().unwrap();
    assert_eq!(done.frames, input.len() as u64 + 2);
    assert_eq!(done.size_bytes, std::fs::metadata(&path).unwrap().len());

    let mut r = claxon::FlacReader::open(&path).unwrap();
    let info = r.streaminfo();
    assert_eq!(
        (info.sample_rate, info.channels, info.bits_per_sample),
        (16_000, 1, 16)
    );
    assert_eq!(info.samples, Some(input.len() as u64 + 2));
    assert_ne!(info.md5sum, [0; 16], "MD5 of the audio is recorded");
    let got: Vec<i32> = r.samples().map(Result::unwrap).collect();
    let mut want: Vec<i32> = input.iter().map(|&x| i32::from(f32_to_i16(x))).collect();
    want.extend([i32::from(i16::MAX), i32::from(f32_to_i16(-2.0))]);
    assert_eq!(got, want);

    let (rate, decoded) = read_flac(&path).unwrap();
    assert_eq!(rate, 16_000);
    assert_eq!(decoded.len(), want.len());
}

#[test]
fn flac_writer_empty_track_is_a_valid_file() {
    let tmp = TempDir::new("flac-empty");
    let path = tmp.path().join("e.flac");
    let done = FlacTrackWriter::create(&path, 16_000)
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(done.frames, 0);
    let (rate, samples) = read_flac(&path).unwrap();
    assert_eq!((rate, samples.len()), (16_000, 0));
}

#[test]
fn flac_unfinished_writer_leaves_only_a_part_file() {
    let tmp = TempDir::new("flac-part");
    let path = tmp.path().join("audio").join("m").join("mic.flac");
    let mut w = FlacTrackWriter::create(&path, 16_000).unwrap();
    w.write(&sine(10_000)).unwrap();
    drop(w); // a crash: never finished
    assert!(!path.exists(), "no final file before finish");
    assert!(
        path.with_extension("flac.part").exists(),
        "the partial audio is kept"
    );
}

#[test]
fn flac_writer_never_overwrites_a_finished_file() {
    let tmp = TempDir::new("flac-exists");
    let path = tmp.path().join("x.flac");
    FlacTrackWriter::create(&path, 16_000)
        .unwrap()
        .finish()
        .unwrap();
    assert!(FlacTrackWriter::create(&path, 16_000).is_err());
}

/// Writes `input` in uneven chunks through `fault` until a write fails; returns the writer
/// and how many samples were offered before the failure (all of them when none failed).
fn write_until_failure(
    path: &std::path::Path,
    input: &[f32],
    fault: WriteFault,
) -> (FlacTrackWriter, usize) {
    let mut w = FlacTrackWriter::create(path, 16_000).unwrap();
    w.inject_fault(fault);
    let mut offered = 0;
    for chunk in input.chunks(777) {
        offered += chunk.len();
        if w.write(chunk).is_err() {
            assert!(w.has_failed());
            // The writer takes nothing more after its first error.
            assert!(w.write(&input[..10]).is_err());
            return (w, offered);
        }
    }
    (w, offered)
}

/// Pseudo-random samples that compress little, so frame sizes are predictable (several KB).
fn noisy(n: usize) -> Vec<f32> {
    let mut x: u32 = 0x1234_5678;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 0.6
        })
        .collect()
}

fn quantised(input: &[f32]) -> Vec<i32> {
    input.iter().map(|&x| i32::from(f32_to_i16(x))).collect()
}

#[test]
fn flac_disk_full_keeps_every_whole_frame_written_before_it() {
    // T9.17: a full disk late in a meeting must not delete what was recorded. The fault lands
    // at many offsets: before the first frame, inside frames, on a frame boundary.
    let input = noisy(4096 * 6 + 999);
    let tmp = TempDir::new("flac-full");
    let mut boundary = None;
    for (i, limit) in [0u64, 1, 500, 3000, 9000, 15_000, 21_000]
        .into_iter()
        .enumerate()
    {
        let path = tmp.path().join(format!("f{i}.flac"));
        let (w, _) = write_until_failure(&path, &input, WriteFault::DiskFullAfter(limit));
        assert!(w.has_failed(), "limit {limit} is inside the audio");
        let kept = w.whole_frames();
        let done = w
            .finish()
            .expect("a full disk still lets the header be rewritten");
        assert!(done.cut_short);
        assert_eq!(done.frames, kept);
        assert_eq!(done.size_bytes, std::fs::metadata(&path).unwrap().len());
        assert!(!path.with_extension("flac.part").exists());
        // Whole 4096-sample frames, exactly the start of the input.
        assert_eq!(kept % 4096, 0, "limit {limit}");
        let mut r = claxon::FlacReader::open(&path).unwrap();
        // FLAC writes 0 for an unknown total, which claxon reads as None.
        let want = (kept > 0).then_some(kept);
        assert_eq!(r.streaminfo().samples, want);
        assert_eq!(r.streaminfo().md5sum, [0; 16], "MD5 unknown after a cut");
        let got: Vec<i32> = r.samples().map(Result::unwrap).collect();
        assert_eq!(got, quantised(&input[..kept as usize]), "limit {limit}");
        let (_, decoded) = read_flac(&path).unwrap();
        assert_eq!(decoded.len() as u64, kept);
        if limit == 0 {
            assert_eq!(kept, 0, "nothing fits: an empty but valid file");
        }
        if kept > 0 {
            boundary.get_or_insert(kept);
        }
    }
    assert!(boundary.is_some(), "some limit kept audio");
}

#[test]
fn flac_disk_full_on_the_last_partial_frame_keeps_the_frames_before_it() {
    // The failure comes only in finish(), encoding the last partial block.
    let input = noisy(4096 * 2 + 1000);
    let tmp = TempDir::new("flac-full-end");
    let path = tmp.path().join("e.flac");
    let mut w = FlacTrackWriter::create(&path, 16_000).unwrap();
    w.write(&input).unwrap();
    // Two whole frames are on disk; nothing more fits.
    w.inject_fault(WriteFault::DiskFullAfter(0));
    let done = w.finish().unwrap();
    assert!(done.cut_short);
    assert_eq!(done.frames, 4096 * 2);
    let (_, decoded) = read_flac(&path).unwrap();
    assert_eq!(decoded.len(), 4096 * 2);
}

#[test]
fn flac_broken_drive_leaves_the_part_file_for_recovery() {
    let input = noisy(4096 * 4);
    let tmp = TempDir::new("flac-broken");
    let path = tmp.path().join("b.flac");
    let (w, _) = write_until_failure(&path, &input, WriteFault::BrokenAfter(6000));
    assert!(w.has_failed());
    assert!(w.finish().is_err(), "the file cannot be finished");
    assert!(!path.exists());
    let part = path.with_extension("flac.part");
    assert!(part.exists(), "never deleted");
    assert!(std::fs::metadata(&part).unwrap().len() >= 6000);
}

#[test]
fn flac_fault_never_reached_finishes_whole() {
    // Negative control: a limit beyond the recording changes nothing.
    let input = sine(4096 * 3 + 17);
    let tmp = TempDir::new("flac-fault-late");
    let path = tmp.path().join("l.flac");
    let (w, offered) = write_until_failure(&path, &input, WriteFault::DiskFullAfter(10_000_000));
    assert_eq!(offered, input.len());
    assert!(!w.has_failed());
    let done = w.finish().unwrap();
    assert!(!done.cut_short);
    assert_eq!(done.frames, input.len() as u64);
    let r = claxon::FlacReader::open(&path).unwrap();
    assert_ne!(r.streaminfo().md5sum, [0; 16]);
}

#[test]
fn flac_disk_full_errors_are_named_as_such() {
    let full = StoreError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull));
    assert!(full.is_disk_full());
    let quota = StoreError::Io(std::io::Error::from(std::io::ErrorKind::QuotaExceeded));
    assert!(quota.is_disk_full());
    #[cfg(windows)]
    assert!(StoreError::Io(std::io::Error::from_raw_os_error(112)).is_disk_full());
    assert!(!StoreError::Io(std::io::Error::other("gone")).is_disk_full());
    assert!(!StoreError::Audio("encode".into()).is_disk_full());
    // What the writer reports for an injected full disk is a full disk.
    let tmp = TempDir::new("flac-full-kind");
    let mut w = FlacTrackWriter::create(&tmp.path().join("k.flac"), 16_000).unwrap();
    w.inject_fault(WriteFault::DiskFullAfter(0));
    let e = w.write(&sine(4096)).unwrap_err();
    assert!(e.is_disk_full(), "{e:?}");
}

#[test]
fn audio_row_is_written_only_after_the_flac_is_finalised() {
    let mut w = World::new("crash-row");
    let (m, tracks) = w.meeting_with(&[TrackId::Mic]);
    let mut writer = w.data.track_writer(m.id, TrackId::Mic, 16_000).unwrap();
    writer.write(&sine(20_000)).unwrap();
    let final_path = w
        .data
        .resolve(&DataDir::audio_rel(m.id, TrackId::Mic))
        .unwrap();
    assert!(
        w.store.audio_files(m.id).unwrap().is_empty(),
        "no row while recording"
    );
    assert!(!final_path.exists());

    let f = w
        .store
        .finalize_track_audio(
            writer,
            &w.data,
            &tracks[0],
            provenance(),
            &w.clock,
            &mut w.rng,
        )
        .unwrap();
    assert_eq!(f.path, DataDir::audio_rel(m.id, TrackId::Mic));
    assert_eq!((f.frames, f.sample_rate_hz), (20_000, 16_000));
    assert_eq!(f.size_bytes, std::fs::metadata(&final_path).unwrap().len());
    assert_eq!(f.pipeline_version, "earshot-0.1.0");
    assert_eq!(f.aec_version.as_deref(), Some("aec-1"));
    assert_eq!(f.created_unix_ms, w.clock.now_unix_ms());
    assert_eq!(w.store.audio_files(m.id).unwrap(), vec![f]);
}

#[test]
fn audio_finalise_for_a_missing_track_writes_no_row_and_keeps_the_audio() {
    let mut w = World::new("crash-orphan");
    let (m, _) = w.meeting_with(&[]);
    let ghost = Track::new(m.id, TrackId::Mic, None, ClockDomain::Qpc, &mut w.stamp());
    let mut writer = w.data.track_writer(m.id, TrackId::Mic, 16_000).unwrap();
    writer.write(&sine(100)).unwrap();
    let err = w
        .store
        .finalize_track_audio(writer, &w.data, &ghost, provenance(), &w.clock, &mut w.rng)
        .unwrap_err();
    assert!(matches!(err, StoreError::Repo(_)), "{err}");
    assert!(w.store.all_audio_files().unwrap().is_empty());
    let final_path = w
        .data
        .resolve(&DataDir::audio_rel(m.id, TrackId::Mic))
        .unwrap();
    assert!(
        final_path.with_extension("flac.part").exists(),
        "audio is not destroyed"
    );
}

/// Starts `m`'s retention clock at `since` (as the runtime does when the summary is stored).
fn start_clock(w: &mut World, m: &Meeting, since: u64) {
    w.store
        .set_local_value(&retention_clock_key(m.id), &encode_retention_clock(since))
        .unwrap();
}

#[test]
fn retention_keep_forever_deletes_nothing() {
    let mut w = World::new("ret-forever");
    let (m, t) = w.meeting_with(&[TrackId::Mic]);
    w.record(&m, &t[0], &sine(100));
    let since = w.clock.now_unix_ms();
    start_clock(&mut w, &m, since);
    let now = w.clock.now_unix_ms() + 10_000 * DAY_MS;
    let r = run_retention(&mut w.store, &w.data, RetentionPolicy::KeepForever, now).unwrap();
    assert_eq!((r.deleted_files, r.rows_removed, r.kept), (0, 0, 1));
    assert!(
        due_audio(&w.store, RetentionPolicy::KeepForever, now)
            .unwrap()
            .meetings
            .is_empty()
    );
}

#[test]
fn retention_default_keeps_audio_seven_days() {
    assert_eq!(
        RetentionPolicy::default(),
        RetentionPolicy::KeepFor { ms: 7 * DAY_MS }
    );
    assert_eq!(
        RetentionPolicy::hours(1),
        RetentionPolicy::KeepFor { ms: HOUR_MS }
    );
    assert_eq!(
        RetentionPolicy::days(90),
        RetentionPolicy::KeepFor { ms: 90 * DAY_MS }
    );
    assert_eq!(HOUR_MS * 24, DAY_MS);
}

#[test]
fn retention_ages_audio_from_the_clock_not_from_the_recording() {
    let mut w = World::new("ret-clock");
    // Recorded 30 days ago, summarised 10 days ago: past a 7-day limit.
    let (old_m, old_t) = w.meeting_with(&[TrackId::Mic, TrackId::Reference]);
    let a = w.record(&old_m, &old_t[0], &sine(100));
    let b = w.record(&old_m, &old_t[1], &sine(100));
    let now = a.created_unix_ms + 30 * DAY_MS;
    start_clock(&mut w, &old_m, now - 10 * DAY_MS);
    // Recorded 30 days ago, summarised only yesterday: kept.
    let (late_m, late_t) = w.meeting_with(&[TrackId::Mic]);
    let c = w.record(&late_m, &late_t[0], &sine(100));
    start_clock(&mut w, &late_m, now - DAY_MS);
    // Recorded 30 days ago, never summarised (a failed summary, no final pass): kept.
    let (open_m, open_t) = w.meeting_with(&[TrackId::Mic]);
    let d = w.record(&open_m, &open_t[0], &sine(100));
    // The transcript stays when the audio goes.
    let seg = Segment::from_words(
        EntityId::generate(&w.clock, &mut w.rng),
        old_t[0].id,
        vec![Word::new("hi", 0, 100, 1.0)],
        MeetingTime::ZERO,
    );
    let row = SegmentRow::new(old_m.id, TranscriptPass::Final, seg, &mut w.hlc, &w.clock);
    w.store.put_segment(row).unwrap();

    let policy = RetentionPolicy::days(7);
    let due = due_audio(&w.store, policy, now).unwrap();
    assert_eq!(
        due.meetings.keys().copied().collect::<Vec<_>>(),
        vec![old_m.id]
    );
    assert_eq!(due.meetings[&old_m.id], vec![a.clone(), b.clone()]);
    assert_eq!(due.unreadable_clocks, 0);

    let r = run_retention(&mut w.store, &w.data, policy, now).unwrap();
    assert_eq!(
        (r.deleted_files, r.missing_files, r.rows_removed, r.kept),
        (2, 0, 2, 2)
    );
    let mut left = w.store.all_audio_files().unwrap();
    left.sort_by_key(|f| f.id);
    let mut want = vec![c.clone(), d.clone()];
    want.sort_by_key(|f| f.id);
    assert_eq!(left, want);
    for f in [&a, &b] {
        assert!(!w.data.resolve(&f.path).unwrap().exists());
    }
    for f in [&c, &d] {
        assert!(w.data.resolve(&f.path).unwrap().exists());
    }
    assert!(
        !w.data
            .root()
            .join("audio")
            .join(old_m.id.to_string())
            .exists(),
        "empty dir removed"
    );
    assert_eq!(
        live_segments(&w.store, old_m.id, TranscriptPass::Final)
            .unwrap()
            .len(),
        1,
        "only audio goes"
    );
    assert!(w.store.meeting(old_m.id).unwrap().is_some());
}

#[test]
fn retention_counts_sub_day_limits_to_the_millisecond() {
    let mut w = World::new("ret-hours");
    let (m, t) = w.meeting_with(&[TrackId::Mic]);
    w.record(&m, &t[0], &sine(100));
    let since = w.clock.now_unix_ms();
    start_clock(&mut w, &m, since);
    let policy = RetentionPolicy::hours(1);
    let due = |w: &World, now| due_audio(&w.store, policy, now).unwrap().meetings.len();
    assert_eq!(due(&w, since + HOUR_MS - 1), 0);
    assert_eq!(due(&w, since + HOUR_MS), 1);
    // A clock in the future (the system clock went back) keeps the audio.
    assert_eq!(due(&w, since - 1), 0);
    let r = run_retention(&mut w.store, &w.data, policy, since + HOUR_MS - 1).unwrap();
    assert_eq!((r.rows_removed, r.kept), (0, 1));
    let r = run_retention(&mut w.store, &w.data, policy, since + HOUR_MS).unwrap();
    assert_eq!((r.deleted_files, r.rows_removed, r.kept), (1, 1, 0));
}

#[test]
fn retention_tolerates_a_row_whose_file_is_missing() {
    let mut w = World::new("ret-missing");
    let (m, t) = w.meeting_with(&[TrackId::Mic]);
    let f = w.record(&m, &t[0], &sine(100));
    start_clock(&mut w, &m, f.created_unix_ms);
    std::fs::remove_file(w.data.resolve(&f.path).unwrap()).unwrap();
    let now = f.created_unix_ms + 2 * DAY_MS;
    let r = run_retention(&mut w.store, &w.data, RetentionPolicy::days(1), now).unwrap();
    assert_eq!(
        (r.deleted_files, r.missing_files, r.rows_removed),
        (0, 1, 1)
    );
    assert!(w.store.all_audio_files().unwrap().is_empty());
}

#[test]
fn retention_keeps_audio_whose_clock_cannot_be_read() {
    let mut w = World::new("ret-unreadable");
    let (m, t) = w.meeting_with(&[TrackId::Mic]);
    let f = w.record(&m, &t[0], &sine(100));
    let mut newer = encode_retention_clock(f.created_unix_ms);
    newer[0] = 2;
    w.store
        .set_local_value(&retention_clock_key(m.id), &newer)
        .unwrap();
    assert_eq!(
        retention_clock(&w.store, m.id).unwrap(),
        RetentionClock::Unreadable
    );
    let now = f.created_unix_ms + 400 * DAY_MS;
    let r = run_retention(&mut w.store, &w.data, RetentionPolicy::days(1), now).unwrap();
    assert_eq!((r.rows_removed, r.kept, r.unreadable_clocks), (0, 1, 1));
    assert!(w.data.resolve(&f.path).unwrap().exists());
}

#[test]
fn retention_clock_records_round_trip_and_refuse_other_versions() {
    let id: EntityId = "01920000-0000-7000-8000-000000000001".parse().unwrap();
    assert_eq!(
        retention_clock_key(id),
        "audio-retention/01920000-0000-7000-8000-000000000001"
    );
    assert!(retention_clock_key(id).starts_with(RETENTION_CLOCK_PREFIX));
    let bytes = encode_retention_clock(1_800_000_123_456);
    assert_eq!(bytes.len(), 12);
    assert_eq!(&bytes[..4], &RETENTION_CLOCK_VERSION.to_le_bytes());
    assert_eq!(decode_retention_clock(&bytes), Some(1_800_000_123_456));
    assert_eq!(decode_retention_clock(&bytes[..11]), None);
    assert_eq!(
        decode_retention_clock(&[bytes.clone(), vec![0]].concat()),
        None
    );
    assert_eq!(decode_retention_clock(&[]), None);
    let mut other = bytes;
    other[0] = 9;
    assert_eq!(decode_retention_clock(&other), None);
    let w = World::new("ret-absent");
    assert_eq!(
        retention_clock(&w.store, id).unwrap(),
        RetentionClock::NotStarted
    );
}

#[test]
fn retention_remove_audio_deletes_only_the_files_given() {
    let mut w = World::new("ret-remove");
    let (m, t) = w.meeting_with(&[TrackId::Mic, TrackId::Reference]);
    let a = w.record(&m, &t[0], &sine(100));
    let b = w.record(&m, &t[1], &sine(100));
    let r = remove_audio(&mut w.store, &w.data, std::slice::from_ref(&a)).unwrap();
    assert_eq!(
        (r.deleted_files, r.missing_files, r.rows_removed),
        (1, 0, 1)
    );
    assert_eq!(w.store.all_audio_files().unwrap(), vec![b.clone()]);
    assert!(
        w.data.resolve(&b.path).unwrap().exists(),
        "the folder keeps the other track"
    );
}

#[test]
fn retranscribe_audio_tolerates_a_missing_file() {
    let mut w = World::new("retranscribe");
    let (m, t) = w.meeting_with(&[TrackId::Mic, TrackId::Reference]);
    let mic = sine(3000);
    w.record(&m, &t[0], &mic);
    let gone = w.record(&m, &t[1], &sine(500));
    std::fs::remove_file(w.data.resolve(&gone.path).unwrap()).unwrap();
    let audio = load_meeting_audio(&w.store, &w.data, m.id).unwrap();
    assert_eq!(audio.missing, vec![gone]);
    assert_eq!(audio.tracks.len(), 1);
    let (track, file, samples) = &audio.tracks[0];
    assert_eq!(track.id, t[0].id);
    assert_eq!(file.track_id, t[0].id);
    assert_eq!(samples.len(), mic.len());
    for (a, b) in samples.iter().zip(&mic) {
        assert!(
            (a - b).abs() <= 1.0 / 32_768.0 + 1e-6,
            "16-bit quantisation only"
        );
    }
}
