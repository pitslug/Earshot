//! The owner's calls (T12.73), for `cluster_sweep --calls`: the section "The owner's calls
//! (T12.73)" of `docs/far-speakers/short-turns.md`, committed before this mode was written.
//! Evaluation only, counts and times only.
//!
//! The store is copied into `--into` (inside a `fixtures/private/real-meetings/` folder) by the
//! `store_parts/copy.rs` rule and the copy is opened read-only; the original database is never
//! opened. The recordings are read in place, read-only, from the data folder that holds
//! `--db`, decoded by the final pass's own reader (`earshot_store::read_flac`). The call is
//! named only by `call_title` in a private settings file outside the repo; no title, id or name
//! is printed or written, and errors never quote one. Tracks are labelled `c` (the call) and
//! `t1`, `t2`, ... (every other live meeting whose reference recording is still on disk, in
//! start order); speakers `b<k>` (the baseline's) and `n<k>` (the pick's new ones).
//!
//! Each track runs the production final-pass diarisation on the CPU at the baseline and at
//! T12.66's pick through the shared record and replay (`short_turns::item_runs`, which fails
//! unless the replayed baseline equals the direct run), and moved and gained speech are counted
//! by the shared frame rule (`short_turns::frame_move`); any second a baseline speaker gains
//! fails the run (`short_turns::check_kept`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::TrackId;
use earshot_core::diar::{Diarization, DiarizationConfig};
use earshot_core::identity::EntityId;
use earshot_core::model::{AudioFileRepo, Meeting, MeetingRepo};
use earshot_runtime::backends::ModelDir;
use earshot_store::{DataDir, ReadOnlyStore, read_flac};
use serde::Deserialize;
use serde_json::{Value, json};

use super::short_turns::{Kept, Setting, check_kept, frame_move, item_runs, old_speakers};
use super::{Embedder, Segmenter, copy, floors, round4};

/// T12.66's pick (docs/far-speakers/short-turns.md, Result).
pub const PICK: Setting = Setting {
    f: 60,
    d: 80,
    m: 8,
    weighted: false,
};
/// The private settings file, under the home folder.
pub const SETTINGS_FILE: [&str; 2] = [".earshot-private", "t1273.toml"];

/// The private settings file's one key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallSettings {
    call_title: String,
}

/// `--calls` arguments.
#[derive(Debug)]
pub struct Args {
    pub db: PathBuf,
    pub into: PathBuf,
    pub models: ModelDir,
    /// The settings file (default `%USERPROFILE%\.earshot-private\t1273.toml`).
    pub call_file: Option<PathBuf>,
}

/// Parses the `--calls` flags (`--calls` itself already taken out).
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let (mut db, mut into, mut models, mut call_file) = (None, None, None, None);
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(val()?)),
            "--into" => into = Some(PathBuf::from(val()?)),
            "--models" => models = Some(ModelDir::new(val()?)),
            "--call-file" => call_file = Some(PathBuf::from(val()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    match (db, into, models) {
        (Some(db), Some(into), Some(models)) => Ok(Args {
            db,
            into,
            models,
            call_file,
        }),
        _ => Err("--calls needs --db, --into and --models".into()),
    }
}

/// The default settings file: `<home>\.earshot-private\t1273.toml`.
pub fn default_settings_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|v| !v.is_empty())
        .ok_or("no home folder (USERPROFILE) to find the private settings file in")?;
    let mut p = PathBuf::from(home);
    p.extend(SETTINGS_FILE);
    Ok(p)
}

/// The call's title from the settings file at `path`. Errors name the file, never its text
/// (a TOML error quotes the line, so it is not passed on).
pub fn read_call_title(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|_| {
        format!(
            "the call is named only in a private settings file, and {} cannot be read: create \
             it with one line, call_title = \"<the call's title as the app shows it>\"",
            path.display()
        )
    })?;
    let s: CallSettings = toml::from_str(&text).map_err(|_| {
        format!(
            "{} is not this task's settings file: it holds exactly one key, call_title",
            path.display()
        )
    })?;
    if s.call_title.trim().is_empty() {
        return Err(format!("{}: call_title is empty", path.display()));
    }
    Ok(s.call_title)
}

/// The one live meeting titled exactly `title`. Refuses none and more than one; never quotes
/// the title.
pub fn call_meeting(meetings: &[Meeting], title: &str) -> Result<EntityId, String> {
    let found: Vec<EntityId> = meetings
        .iter()
        .filter(|m| !m.meta.deleted && m.title == title)
        .map(|m| m.id)
        .collect();
    match found.as_slice() {
        [one] => Ok(*one),
        [] => Err("no live meeting has the call's title (from the settings file)".into()),
        many => Err(format!(
            "{} live meetings have the call's title; it must name exactly one",
            many.len()
        )),
    }
}

/// One track to diarise. Its meeting id and path stay in memory: they are never printed or
/// written.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    /// `c`, `t1`, `t2`, ...
    pub label: String,
    pub started_unix_ms: u64,
    pub utc_offset_minutes: i32,
    /// The recording, in place in the data folder.
    pub path: PathBuf,
    /// Its row's sample rate.
    pub rate_hz: u32,
    meeting: EntityId,
}

/// The reference recording of `meeting` when its file is on disk: the first live reference
/// track with an audio row whose file exists.
fn reference_recording(
    store: &ReadOnlyStore,
    data: &DataDir,
    meeting: EntityId,
) -> Result<Option<(PathBuf, u32)>, String> {
    let tracks = store.tracks(meeting).map_err(|_| "reading tracks")?;
    let files = store
        .audio_files(meeting)
        .map_err(|_| "reading audio file rows")?;
    for t in tracks
        .iter()
        .filter(|t| !t.meta.deleted && t.kind == TrackId::Reference)
    {
        for f in files.iter().filter(|f| f.track_id == t.id) {
            let path = data
                .resolve(&f.path)
                .map_err(|_| "a stored recording path is not inside the data folder")?;
            if path.is_file() {
                return Ok(Some((path, f.sample_rate_hz)));
            }
        }
    }
    Ok(None)
}

/// The tracks of the run: the call's reference recording (`c`, which must be on disk), then
/// the reference recording of every other live meeting whose file is on disk, in start order
/// (`t1`, `t2`, ...).
pub fn select_tracks(
    store: &ReadOnlyStore,
    data: &DataDir,
    title: &str,
) -> Result<Vec<Pick>, String> {
    let mut meetings: Vec<Meeting> = store
        .meetings()
        .map_err(|_| "reading meetings")?
        .into_iter()
        .filter(|m| !m.meta.deleted)
        .collect();
    meetings.sort_by_key(|m| (m.started_unix_ms, m.id));
    let call = call_meeting(&meetings, title)?;
    let pick = |m: &Meeting, label: String, (path, rate_hz): (PathBuf, u32)| Pick {
        label,
        started_unix_ms: m.started_unix_ms,
        utc_offset_minutes: m.utc_offset_minutes,
        path,
        rate_hz,
        meeting: m.id,
    };
    let mut out = Vec::new();
    let call_meeting = meetings
        .iter()
        .find(|m| m.id == call)
        .ok_or("the call vanished from the copy")?;
    match reference_recording(store, data, call)? {
        Some(r) => out.push(pick(call_meeting, "c".into(), r)),
        None => {
            return Err(
                "the call's far-end recording is not on disk (retention may have removed it)"
                    .into(),
            );
        }
    }
    for m in meetings.iter().filter(|m| m.id != call) {
        if let Some(r) = reference_recording(store, data, m.id)? {
            let label = format!("t{}", out.len());
            out.push(pick(m, label, r));
        }
    }
    Ok(out)
}

/// `YYYY-MM-DD HH:MM` local time of a meeting start, so the owner can find it in the app.
pub fn local_time(unix_ms: u64, utc_offset_minutes: i32) -> String {
    let secs =
        i64::try_from(unix_ms / 1000).unwrap_or(i64::MAX) + i64::from(utc_offset_minutes) * 60;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms", `civil_from_days` (as
    // core's export uses, which keeps it private).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}",
        sod / 3600,
        (sod / 60) % 60
    )
}

/// What one track measures.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackResult {
    pub label: String,
    pub seconds: f64,
    pub embeddings: usize,
    pub over_max_train: bool,
    pub baseline_speakers: usize,
    pub pick_speakers: usize,
    /// New speakers (`n<k>`) with their speech (s).
    pub new_speakers: Vec<(String, f64)>,
    /// Speech moved from each baseline speaker (`b<k>`) to a new one (s); only speakers that
    /// lost some.
    pub moved_from: Vec<(String, f64)>,
    /// Seconds a baseline speaker holds at the pick that it did not hold at the baseline.
    pub gained_s: f64,
    /// Each new speaker's turns (start, end in seconds).
    pub turns: Vec<(String, Vec<(f64, f64)>)>,
}

fn speakers_in(turns: &[earshot_core::metrics::der::Turn]) -> usize {
    turns
        .iter()
        .filter(|t| t.end > t.start)
        .map(|t| t.speaker.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

/// The measures of the pick `at` against the baseline `base` of one track `seconds` long.
/// Fails (through `check_kept`) when a baseline speaker gained anything.
pub fn track_result(
    label: &str,
    seconds: f64,
    base: &Diarization,
    at: &Diarization,
    embeddings: usize,
    max_train: usize,
) -> TrackResult {
    let uem = (0.0, seconds);
    let (base_turns, at_turns) = (base.der_turns(), at.der_turns());
    let map = old_speakers(base, at);
    let unmatched = map.iter().position(|k| *k == Kept::Missing);
    let was = floors::paint(&base_turns, uem);
    let now = floors::paint(&at_turns, uem);
    let mut new_new: Vec<usize> = at.new_speakers.clone();
    new_new.sort_unstable();
    new_new.dedup();
    let mut new_speech = vec![0.0f64; new_new.len()];
    let mut moved = vec![0.0f64; base.speakers.len()];
    let mut gained = 0.0;
    for i in 0..floors::frames(uem) {
        let f = frame_move(&was[i], &now[i], &map);
        if f.gained {
            gained += floors::FRAME_S;
        }
        for n in &f.new_here {
            if let Some(k) = new_new.iter().position(|x| x == n) {
                new_speech[k] += floors::FRAME_S;
            }
        }
        for &b in &f.lost {
            moved[b] += floors::FRAME_S;
        }
    }
    check_kept(&format!("track {label} at the pick"), unmatched, gained);
    let turns = new_new
        .iter()
        .enumerate()
        .map(|(k, n)| {
            let s = format!("S{n}");
            (
                format!("n{k}"),
                at_turns
                    .iter()
                    .filter(|t| t.speaker == s)
                    .map(|t| (t.start, t.end))
                    .collect(),
            )
        })
        .collect();
    TrackResult {
        label: label.to_owned(),
        seconds,
        embeddings,
        over_max_train: embeddings > max_train,
        baseline_speakers: speakers_in(&base_turns),
        pick_speakers: speakers_in(&at_turns),
        new_speakers: new_speech
            .iter()
            .enumerate()
            .map(|(k, s)| (format!("n{k}"), *s))
            .collect(),
        moved_from: moved
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > 0.0)
            .map(|(b, s)| (format!("b{b}"), *s))
            .collect(),
        gained_s: gained,
        turns,
    }
}

/// Everything up to the models: the private-tree check, the call's title, the copy and the
/// tracks. Nothing is created outside `--into`, and nothing at all when `--into` is refused.
pub fn prepare(a: &Args) -> Result<Vec<Pick>, String> {
    if !copy::inside_private_tree(&a.into) {
        return Err("--into must lie inside a fixtures/private/real-meetings/ folder".into());
    }
    let settings = match &a.call_file {
        Some(p) => p.clone(),
        None => default_settings_path()?,
    };
    let title = read_call_title(&settings)?;
    let data = DataDir::new(
        a.db.parent()
            .ok_or("--db has no folder: the recordings are read beside it")?,
    );
    let store = copy::copy_and_open(&a.db, &a.into, &[])?;
    select_tracks(&store, &data, &title)
}

/// Decodes a pick exactly as the final pass reads a stored track. Errors name the label only.
pub fn decode(p: &Pick) -> Result<Vec<f32>, String> {
    let (rate, samples) =
        read_flac(&p.path).map_err(|_| format!("{}: the recording cannot be read", p.label))?;
    if rate != p.rate_hz || rate != PIPELINE_RATE.hz() {
        return Err(format!(
            "{}: the recording is {rate} Hz, its row says {} Hz, the pipeline needs {}",
            p.label,
            p.rate_hz,
            PIPELINE_RATE.hz()
        ));
    }
    Ok(samples)
}

/// The JSON files: the report, each new speaker's turns, and each label's start time.
pub fn outputs(model: &str, picks: &[Pick], results: &[TrackResult]) -> (Value, Value, Value) {
    let report = json!({
        "task": "T12.73",
        "rule": "docs/far-speakers/short-turns.md, The owner's calls (T12.73)",
        "model": model,
        "threshold": super::threshold(super::HELD),
        "pick": {
            "floor": f64::from(PICK.f) / 100.0,
            "distance": f64::from(PICK.d) / 100.0,
            "min_speech_s": PICK.m,
            "weighting": if PICK.weighted { "weighted" } else { "plain" },
        },
        "tracks": results.iter().map(|r| json!({
            "label": r.label,
            "seconds": round4(r.seconds),
            "embeddings": r.embeddings,
            "over_max_train_embeddings": r.over_max_train,
            "speakers_baseline": r.baseline_speakers,
            "speakers_pick": r.pick_speakers,
            "new_speakers": r.new_speakers.iter().map(|(l, s)| json!({
                "label": l, "speech_s": round4(*s),
            })).collect::<Vec<_>>(),
            "moved_from": r.moved_from.iter().map(|(l, s)| json!({
                "label": l, "speech_s": round4(*s),
            })).collect::<Vec<_>>(),
            "kept_speakers_gaining_s": round4(r.gained_s),
        })).collect::<Vec<_>>(),
    });
    let turns: serde_json::Map<String, Value> = results
        .iter()
        .map(|r| {
            (
                r.label.clone(),
                json!(
                    r.turns
                        .iter()
                        .map(|(l, ts)| json!({
                            "speaker": l,
                            "turns": ts.iter().map(|(s, e)| json!([round4(*s), round4(*e)])).collect::<Vec<_>>(),
                        }))
                        .collect::<Vec<_>>()
                ),
            )
        })
        .collect();
    let labels: serde_json::Map<String, Value> = picks
        .iter()
        .map(|p| {
            (
                p.label.clone(),
                json!(local_time(p.started_unix_ms, p.utc_offset_minutes)),
            )
        })
        .collect();
    (report, Value::Object(turns), Value::Object(labels))
}

fn write(into: &Path, name: &str, v: &Value) {
    let path = into.join(name);
    let bytes = serde_json::to_vec_pretty(v).unwrap_or_else(|e| super::fail(e));
    std::fs::write(&path, bytes)
        .unwrap_or_else(|e| super::fail(format!("writing {}: {e}", path.display())));
}

/// `--calls`: diarises every pick at the baseline and the pick, prints counts per label and
/// writes `calls.json`, `turns.json` and `labels.json` into `into`.
pub fn run(
    picks: Vec<Pick>,
    mut parts: (Segmenter, Embedder),
    config: &DiarizationConfig,
    model: &str,
    into: &Path,
) {
    let max_train = config.clustering.max_train_embeddings;
    let mut results = Vec::new();
    for p in &picks {
        eprintln!("cluster_sweep --calls: track {}", p.label);
        let samples = decode(p).unwrap_or_else(|e| super::fail(e));
        let seconds = samples.len() as f64 / f64::from(PIPELINE_RATE.hz());
        let buffer = MonoBuffer::new(PIPELINE_RATE, samples);
        let (direct, runs, n, back) =
            item_runs(parts, config, &[PICK.regroup()], &buffer, &p.label);
        parts = back;
        let r = track_result(&p.label, seconds, &direct, &runs[0], n, max_train);
        let list = |v: &[(String, f64)]| {
            if v.is_empty() {
                "none".to_owned()
            } else {
                v.iter()
                    .map(|(l, s)| format!("{l} {s:.1} s"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        println!(
            "{}: {:.1} s, {} embeddings, speakers {} -> {}; new: {}; moved from: {}; kept gaining {:.2} s",
            r.label,
            r.seconds,
            r.embeddings,
            r.baseline_speakers,
            r.pick_speakers,
            list(&r.new_speakers),
            list(&r.moved_from),
            r.gained_s,
        );
        results.push(r);
    }
    let (report, turns, labels) = outputs(model, &picks, &results);
    write(into, "calls.json", &report);
    write(into, "turns.json", &turns);
    write(into, "labels.json", &labels);
    println!(
        "cluster_sweep --calls: calls.json, turns.json and labels.json in {}",
        into.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::model::{ClockDomain, Stamp, Track};
    use earshot_core::prompt::Template;
    use earshot_store::{AudioProvenance, Store};
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use earshot_core::identity::{DeviceId, Hlc, ManualClock};

    const CALL: &str = "An invented weekly catch-up";

    fn temp(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "earshot-cluster-calls-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    struct World {
        clock: ManualClock,
        hlc: Hlc,
        rng: ChaCha8Rng,
    }

    impl World {
        fn new() -> Self {
            World {
                clock: ManualClock::new(1_800_000_000_000),
                hlc: Hlc::new(DeviceId::from_bytes([9; 16])),
                rng: ChaCha8Rng::seed_from_u64(1273),
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
    }

    fn meeting(w: &mut World, title: &str, start: u64, deleted: bool) -> Meeting {
        let mut m = Meeting::new(title, start, 600, Template::General, &mut w.stamp());
        m.meta.deleted = deleted;
        m
    }

    #[test]
    fn cluster_sweep_calls_title_must_name_exactly_one_live_meeting() {
        let mut w = World::new();
        let a = meeting(&mut w, CALL, 1, false);
        let b = meeting(&mut w, "Another invented meeting", 2, false);
        let gone = meeting(&mut w, CALL, 3, true);
        // One live match, a tombstoned one ignored.
        assert_eq!(
            call_meeting(&[a.clone(), b.clone(), gone.clone()], CALL),
            Ok(a.id)
        );
        // Exact match only.
        let none = call_meeting(&[b.clone(), gone.clone()], CALL).unwrap_err();
        assert!(none.contains("no live meeting"), "{none}");
        assert!(call_meeting(std::slice::from_ref(&a), "an invented weekly catch-up").is_err());
        let again = meeting(&mut w, CALL, 4, false);
        let two = call_meeting(&[a, again, b, gone], CALL).unwrap_err();
        assert!(two.contains("2 live meetings"), "{two}");
        for e in [none, two] {
            assert!(!e.contains(CALL), "an error quoted the title");
        }
    }

    #[test]
    fn cluster_sweep_calls_settings_file_is_required_and_never_quoted() {
        let dir = temp("settings");
        let missing = dir.join("t1273.toml");
        let e = read_call_title(&missing).unwrap_err();
        assert!(e.contains("t1273.toml") && e.contains("call_title"), "{e}");
        std::fs::write(&missing, format!("call_title = \"{CALL}\"\n")).unwrap();
        assert_eq!(read_call_title(&missing).unwrap(), CALL);
        std::fs::write(
            &missing,
            format!("call_title = \"{CALL}\"\nother = \"{CALL}\"\n"),
        )
        .unwrap();
        let e = read_call_title(&missing).unwrap_err();
        assert!(!e.contains(CALL), "{e}");
        std::fs::write(&missing, "call_title = \"  \"\n").unwrap();
        assert!(read_call_title(&missing).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cluster_sweep_calls_local_time_is_the_meetings_own_clock() {
        // 951_825_600 s is 2000-02-29 12:00 UTC (a leap day, checked against Python's
        // datetime).
        assert_eq!(local_time(0, 0), "1970-01-01 00:00");
        assert_eq!(local_time(951_825_600_000, 0), "2000-02-29 12:00");
        assert_eq!(local_time(951_825_600_000, 600), "2000-02-29 22:00");
        assert_eq!(local_time(951_825_600_000, -780), "2000-02-28 23:00");
    }

    /// An invented store: the call (reference recorded), meeting A before it (recorded),
    /// B (its recording deleted from disk), C (a mic track only), D after the call
    /// (recorded), E tombstoned (recorded). Returns the database path and the meeting ids.
    fn build(dir: &Path) -> (PathBuf, Vec<EntityId>) {
        let data = DataDir::new(dir.join("data"));
        data.create().unwrap();
        let mut store = Store::open(&data.db_path()).unwrap();
        let mut w = World::new();
        let mut ids = Vec::new();
        let plan: [(&str, u64, bool, Option<TrackId>, bool); 6] = [
            (CALL, 5_000, false, Some(TrackId::Reference), true),
            (
                "Invented meeting A",
                1_000,
                false,
                Some(TrackId::Reference),
                true,
            ),
            (
                "Invented meeting B",
                2_000,
                false,
                Some(TrackId::Reference),
                false,
            ),
            ("Invented meeting C", 3_000, false, Some(TrackId::Mic), true),
            (
                "Invented meeting D",
                9_000,
                false,
                Some(TrackId::Reference),
                true,
            ),
            (
                "Invented meeting E",
                4_000,
                true,
                Some(TrackId::Reference),
                true,
            ),
        ];
        for (title, start, deleted, kind, keep) in plan {
            let m = meeting(&mut w, title, 1_800_000_000_000 + start, deleted);
            store.put_meeting(m.clone()).unwrap();
            ids.push(m.id);
            let Some(kind) = kind else { continue };
            let t = Track::new(m.id, kind, None, ClockDomain::Qpc, &mut w.stamp());
            store.put_track(t.clone()).unwrap();
            let mut wr = data.track_writer(m.id, kind, 16_000).unwrap();
            wr.write(&[0.05; 3200]).unwrap();
            let f = store
                .finalize_track_audio(
                    wr,
                    &data,
                    &t,
                    AudioProvenance {
                        pipeline_version: "earshot-0.1.0".into(),
                        aec_version: None,
                    },
                    &w.clock,
                    &mut w.rng,
                )
                .unwrap();
            if !keep {
                std::fs::remove_file(data.resolve(&f.path).unwrap()).unwrap();
            }
        }
        drop(store);
        (data.db_path(), ids)
    }

    fn private_into(dir: &Path) -> PathBuf {
        dir.join("fixtures")
            .join("private")
            .join("real-meetings")
            .join("run")
    }

    #[test]
    fn cluster_sweep_calls_labels_follow_start_order_and_skip_missing_recordings() {
        let dir = temp("select");
        let (db, ids) = build(&dir);
        let call_file = dir.join("t1273.toml");
        std::fs::write(&call_file, format!("call_title = \"{CALL}\"\n")).unwrap();
        let into = private_into(&dir);
        let before = std::fs::read(&db).unwrap();
        let picks = prepare(&Args {
            db: db.clone(),
            into: into.clone(),
            models: ModelDir::new(dir.join("models")),
            call_file: Some(call_file),
        })
        .unwrap();
        assert_eq!(
            std::fs::read(&db).unwrap(),
            before,
            "the original is untouched"
        );
        assert!(
            into.join("earshot.db").is_file(),
            "the copy is in the run folder"
        );
        let got: Vec<(&str, EntityId)> = picks
            .iter()
            .map(|p| (p.label.as_str(), p.meeting))
            .collect();
        // c the call; then A (earliest) and D; B's file is gone, C has no reference track, E
        // is tombstoned.
        assert_eq!(got, vec![("c", ids[0]), ("t1", ids[1]), ("t2", ids[4])]);
        for p in &picks {
            assert!(p.path.is_file());
            assert_eq!(decode(p).unwrap().len(), 3200);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cluster_sweep_calls_refuses_a_folder_outside_the_private_tree_and_creates_nothing() {
        let dir = temp("outside");
        let (db, _) = build(&dir);
        let call_file = dir.join("t1273.toml");
        std::fs::write(&call_file, format!("call_title = \"{CALL}\"\n")).unwrap();
        let outside = dir.join("run-out");
        let e = prepare(&Args {
            db,
            into: outside.clone(),
            models: ModelDir::new(dir.join("models")),
            call_file: Some(call_file),
        })
        .unwrap_err();
        assert!(e.contains("fixtures/private/real-meetings"), "{e}");
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cluster_sweep_calls_refuses_a_call_whose_recording_is_gone() {
        let dir = temp("gone");
        let (db, _) = build(&dir);
        let call_file = dir.join("t1273.toml");
        // B's reference recording was deleted from disk.
        std::fs::write(&call_file, "call_title = \"Invented meeting B\"\n").unwrap();
        let e = prepare(&Args {
            db,
            into: private_into(&dir),
            models: ModelDir::new(dir.join("models")),
            call_file: Some(call_file),
        })
        .unwrap_err();
        assert!(e.contains("not on disk") && !e.contains("Invented"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn diarization(
        turns: &[(usize, u64, u64)],
        clusters: Vec<usize>,
        new: Vec<usize>,
    ) -> Diarization {
        use earshot_core::asr::MeetingTime;
        use earshot_core::diar::{DiarSpeaker, Embedding, EmbeddingModelId, SpeakerTurn};
        let model = EmbeddingModelId::new("m@1");
        Diarization {
            speakers: clusters
                .iter()
                .map(|_| DiarSpeaker {
                    centroid: Embedding::new(model.clone(), vec![1.0, 0.0]),
                    speech: std::time::Duration::from_secs(1),
                })
                .collect(),
            turns: turns
                .iter()
                .map(|&(speaker, s, e)| SpeakerTurn {
                    speaker,
                    start: MeetingTime::from_millis(s),
                    end: MeetingTime::from_millis(e),
                })
                .collect(),
            unassigned: Vec::new(),
            new_speakers: new,
            speaker_clusters: clusters,
            embedding_model: model,
        }
    }

    #[test]
    fn cluster_sweep_calls_counts_new_speakers_moved_speech_and_turns() {
        // Baseline: b0 0-12 s and 20-24 s, b1 12-20 s. Pick: b0 0-10 s and 20-22 s, new n0
        // 10-12 s and 22-24 s, b1 12-20 s.
        let base = diarization(
            &[(0, 0, 12_000), (1, 12_000, 20_000), (0, 20_000, 24_000)],
            vec![3, 5],
            vec![],
        );
        let at = diarization(
            &[
                (0, 0, 10_000),
                (1, 10_000, 12_000),
                (2, 12_000, 20_000),
                (0, 20_000, 22_000),
                (1, 22_000, 24_000),
            ],
            vec![3, 7, 5],
            vec![1],
        );
        let r = track_result("c", 25.0, &base, &at, 40, 1000);
        assert_eq!((r.baseline_speakers, r.pick_speakers), (2, 3));
        assert_eq!(r.new_speakers.len(), 1);
        assert_eq!(r.new_speakers[0].0, "n0");
        assert!((r.new_speakers[0].1 - 4.0).abs() < 0.011, "{r:?}");
        assert_eq!(r.moved_from.len(), 1);
        assert_eq!(r.moved_from[0].0, "b0");
        assert!((r.moved_from[0].1 - 4.0).abs() < 0.011, "{r:?}");
        assert_eq!(r.gained_s, 0.0);
        assert_eq!(
            r.turns,
            vec![("n0".to_owned(), vec![(10.0, 12.0), (22.0, 24.0)])]
        );
        assert!(!r.over_max_train);
        // The baseline against itself: nothing new, nothing moved.
        let same = track_result("t1", 25.0, &base, &base, 40, 30);
        assert!(
            same.new_speakers.is_empty() && same.moved_from.is_empty() && same.turns.is_empty()
        );
        assert!(same.over_max_train);
    }

    #[test]
    fn cluster_sweep_calls_outputs_hold_labels_counts_and_times_only() {
        let dir = temp("outputs");
        let (db, ids) = build(&dir);
        let call_file = dir.join("t1273.toml");
        std::fs::write(&call_file, format!("call_title = \"{CALL}\"\n")).unwrap();
        let picks = prepare(&Args {
            db,
            into: private_into(&dir),
            models: ModelDir::new(dir.join("models")),
            call_file: Some(call_file),
        })
        .unwrap();
        let results: Vec<TrackResult> = picks
            .iter()
            .map(|p| TrackResult {
                label: p.label.clone(),
                seconds: 0.2,
                embeddings: 3,
                over_max_train: false,
                baseline_speakers: 2,
                pick_speakers: 3,
                new_speakers: vec![("n0".into(), 1.5)],
                moved_from: vec![("b1".into(), 1.5)],
                gained_s: 0.0,
                turns: vec![("n0".into(), vec![(0.5, 2.0)])],
            })
            .collect();
        let (report, turns, labels) = outputs("m@1", &picks, &results);
        // The call started at Unix 1_800_000_005 s in a meeting kept at UTC+10.
        assert_eq!(labels["c"], json!("2027-01-15 18:00"));
        assert_eq!(labels.as_object().unwrap().len(), 3);
        assert_eq!(report["tracks"][0]["label"], "c");
        assert_eq!(report["pick"]["floor"], 0.6);
        assert_eq!(turns["t2"][0]["turns"][0], json!([0.5, 2.0]));
        for v in [&report, &turns, &labels] {
            let text = v.to_string();
            assert!(
                !text.contains("Invented") && !text.contains(CALL),
                "a title leaked"
            );
            for id in &ids {
                assert!(!text.contains(&id.to_string()), "an id leaked");
            }
            assert!(!text.contains("audio"), "a path leaked");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
