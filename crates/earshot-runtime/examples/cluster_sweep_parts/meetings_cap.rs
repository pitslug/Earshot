//! The owner's meetings for T12.76 (`cluster_sweep --meetings-cap`): the section "The owner's
//! meetings" of `docs/far-speakers/training-cap.md`, committed before this mode was written.
//! Informational only: nothing here selects. Counts and times only.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example cluster_sweep -- --meetings-cap \
//!     --db <earshot.db> --into fixtures/private/real-meetings/<run> --models <dir> \
//!     [--groups <groups.json>]
//! ```
//!
//! The store is copied into `--into` (inside `fixtures/private/real-meetings/`) by the
//! `store_parts/copy.rs` rule and opened read-only; recordings are read in place. Tracks are
//! labelled `m1`, `m2`, ... in start order, speakers `b<k>` (the baseline's) and `<cand><k>`;
//! no id, title or name is printed or written outside `--into`. `--groups` (a private file in
//! the run folder: `{"meeting": "<id>", "groups": {"<name>": [[start_ms, end_ms], ...]}}`)
//! names line groups of one meeting, reported as how each group's lines spread over speakers.

use std::collections::BTreeMap;
use std::path::PathBuf;

use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::TrackId;
use earshot_core::diar::{Diarization, DiarizationConfig};
use earshot_core::model::{AudioFileRepo, Meeting, MeetingRepo};
use earshot_runtime::backends::ModelDir;
use earshot_store::{DataDir, ReadOnlyStore, read_flac};
use serde_json::{Value, json};

use super::training_cap::{CANDIDATES, Candidate};
use super::{Embedder, HELD, Segmenter, copy, diarize_at, fail, floors};

/// `--meetings-cap` arguments.
pub struct Args {
    pub db: PathBuf,
    pub into: PathBuf,
    pub models: ModelDir,
    pub groups: Option<PathBuf>,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let (mut db, mut into, mut models, mut groups) = (None, None, None, None);
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(val()?)),
            "--into" => into = Some(PathBuf::from(val()?)),
            "--models" => models = Some(ModelDir::new(val()?)),
            "--groups" => groups = Some(PathBuf::from(val()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    match (db, into, models) {
        (Some(db), Some(into), Some(models)) => Ok(Args {
            db,
            into,
            models,
            groups,
        }),
        _ => Err("--meetings-cap needs --db, --into and --models".into()),
    }
}

/// A `--groups` file: the meeting's id and its named line groups (start and end ms).
type Groups = (String, BTreeMap<String, Vec<(u64, u64)>>);

/// One track: the meeting's id stays in memory only.
pub struct Track {
    pub label: String,
    pub kind: &'static str,
    pub path: PathBuf,
    meeting: String,
}

/// The first live track of `kind` in `meeting` whose recording is on disk.
fn recording(
    store: &ReadOnlyStore,
    data: &DataDir,
    meeting: &Meeting,
    kind: TrackId,
) -> Result<Option<PathBuf>, String> {
    let tracks = store.tracks(meeting.id).map_err(|_| "reading tracks")?;
    let files = store
        .audio_files(meeting.id)
        .map_err(|_| "reading audio file rows")?;
    for t in tracks.iter().filter(|t| !t.meta.deleted && t.kind == kind) {
        for f in files.iter().filter(|f| f.track_id == t.id) {
            let path = data
                .resolve(&f.path)
                .map_err(|_| "a stored recording path is not inside the data folder")?;
            if path.is_file() {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

/// Every live meeting with a recording on disk, in start order: its reference track when it
/// has one (a call: the far end), else its mic track (a meeting in the room).
pub fn tracks(a: &Args) -> Result<Vec<Track>, String> {
    if !copy::inside_private_tree(&a.into) {
        return Err("--into must lie inside a fixtures/private/real-meetings/ folder".into());
    }
    let data = DataDir::new(
        a.db.parent()
            .ok_or("--db has no folder: the recordings are read beside it")?,
    );
    let store = copy::copy_and_open(&a.db, &a.into, &[])?;
    let mut meetings: Vec<Meeting> = store
        .meetings()
        .map_err(|_| "reading meetings")?
        .into_iter()
        .filter(|m| !m.meta.deleted)
        .collect();
    meetings.sort_by_key(|m| (m.started_unix_ms, m.id));
    let has_reference = |m: &Meeting| -> Result<bool, String> {
        Ok(store
            .tracks(m.id)
            .map_err(|_| "reading tracks")?
            .iter()
            .any(|t| !t.meta.deleted && t.kind == TrackId::Reference))
    };
    let mut out = Vec::new();
    for m in &meetings {
        let (kind, name) = if has_reference(m)? {
            (TrackId::Reference, "far end")
        } else {
            (TrackId::Mic, "in room")
        };
        if let Some(path) = recording(&store, &data, m, kind)? {
            out.push(Track {
                label: format!("m{}", out.len() + 1),
                kind: name,
                path,
                meeting: m.id.to_string(),
            });
        }
    }
    Ok(out)
}

/// Seconds of overlap between speaker `a`'s turns in `x` and speaker `b`'s turns in `y`.
fn overlap_s(x: &Diarization, a: usize, y: &Diarization, b: usize) -> f64 {
    let mut s = 0u64;
    for t in x.turns.iter().filter(|t| t.speaker == a) {
        for u in y.turns.iter().filter(|u| u.speaker == b) {
            let lo = t.start.as_millis().max(u.start.as_millis());
            let hi = t.end.as_millis().min(u.end.as_millis());
            s += hi.saturating_sub(lo);
        }
    }
    s as f64 / 1000.0
}

/// For each speaker of `d`, the baseline speakers it holds speech of (seconds, at least 1 s).
pub fn sources(base: &Diarization, d: &Diarization) -> Vec<Vec<(usize, f64)>> {
    (0..d.speakers.len())
        .map(|k| {
            (0..base.speakers.len())
                .map(|j| (j, overlap_s(d, k, base, j)))
                .filter(|(_, s)| *s >= 1.0)
                .collect()
        })
        .collect()
}

/// The speaker of `d` overlapping `[s, e)` ms the longest, or `None`.
fn speaker_at(d: &Diarization, s: u64, e: u64) -> Option<usize> {
    let mut by: BTreeMap<usize, u64> = BTreeMap::new();
    for t in &d.turns {
        let o = e
            .min(t.end.as_millis())
            .saturating_sub(s.max(t.start.as_millis()));
        if o > 0 {
            *by.entry(t.speaker).or_default() += o;
        }
    }
    by.into_iter().max_by_key(|(_, o)| *o).map(|(k, _)| k)
}

fn decode(t: &Track) -> Vec<f32> {
    let (rate, samples) =
        read_flac(&t.path).unwrap_or_else(|_| fail(format!("{}: unreadable recording", t.label)));
    if rate != PIPELINE_RATE.hz() {
        fail(format!("{}: the recording is {rate} Hz", t.label));
    }
    samples
}

/// `--meetings-cap`: every track at every candidate; prints counts and writes
/// `meetings-cap.json` (counts) and `meetings-cap-turns.json` (turn times) into `into`.
pub fn run(
    a: &Args,
    picks: Vec<Track>,
    mut parts: (Segmenter, Embedder),
    config: &DiarizationConfig,
) {
    let groups: Option<Groups> = a.groups.as_ref().map(|p| {
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(p).unwrap_or_else(|_| fail("--groups cannot be read")),
        )
        .unwrap_or_else(|_| fail("--groups is not JSON"));
        let meeting = v["meeting"]
            .as_str()
            .unwrap_or_else(|| fail("--groups: no meeting"))
            .to_owned();
        let g = v["groups"]
            .as_object()
            .unwrap_or_else(|| fail("--groups: no groups"))
            .iter()
            .map(|(k, lines)| {
                let spans = lines
                    .as_array()
                    .unwrap_or_else(|| fail("--groups: a group is not a list"))
                    .iter()
                    .map(|l| (l[0].as_u64().unwrap_or(0), l[1].as_u64().unwrap_or(0)))
                    .collect();
                (k.clone(), spans)
            })
            .collect();
        (meeting, g)
    });
    if let Some((m, _)) = &groups
        && !picks.iter().any(|t| &t.meeting == m)
    {
        fail("--groups names a meeting with no kept recording");
    }
    let mut report = Vec::new();
    let mut turns_out = Vec::new();
    for t in &picks {
        eprintln!("cluster_sweep --meetings-cap: {}", t.label);
        let buffer = MonoBuffer::new(PIPELINE_RATE, decode(t));
        parts.0.replay = false;
        parts.1.replay = false;
        parts.1.outputs = 0;
        let (direct, p) = diarize_at(
            parts,
            &Candidate::B.config(config),
            HELD,
            floors::OFF,
            &buffer,
        );
        parts = p;
        let embeddings = parts.1.outputs;
        parts.0.replay = true;
        parts.1.replay = true;
        let mut runs = Vec::new();
        for c in CANDIDATES {
            let (d, p) = diarize_at(parts, &c.config(config), HELD, floors::OFF, &buffer);
            parts = p;
            if c == Candidate::B && d != direct {
                fail(format!(
                    "{}: the replayed baseline differs from the direct run",
                    t.label
                ));
            }
            runs.push((c, d));
        }
        parts.0.log.clear();
        parts.1.log.clear();

        println!(
            "{} ({}, {} embeddings): speakers {}",
            t.label,
            t.kind,
            embeddings,
            runs.iter()
                .map(|(c, d)| format!("{} {}", c.name(), d.speakers.len()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut cands = Vec::new();
        for (c, d) in &runs {
            let src = sources(&direct, d);
            let text: Vec<String> = src
                .iter()
                .enumerate()
                .map(|(k, s)| {
                    let from: Vec<String> =
                        s.iter().map(|(j, sec)| format!("b{j}:{sec:.0}s")).collect();
                    format!("{}{k}<-[{}]", c.name(), from.join(","))
                })
                .collect();
            println!("  {} {}", c.name(), text.join(" "));
            let mut entry = json!({
                "candidate": c.name(),
                "speakers": d.speakers.len(),
                "new_speakers": d.new_speakers.len(),
                "sources": src.iter().map(|s| s.iter().map(|(j, sec)| json!([j, (sec * 10.0).round() / 10.0])).collect::<Vec<_>>()).collect::<Vec<_>>(),
            });
            if let Some((m, g)) = &groups
                && m == &t.meeting
            {
                let mut spread = serde_json::Map::new();
                for (name, spans) in g {
                    let mut by: BTreeMap<String, usize> = BTreeMap::new();
                    for &(s, e) in spans {
                        let k = speaker_at(d, s, e)
                            .map_or_else(|| "-".to_owned(), |k| format!("{}{k}", c.name()));
                        *by.entry(k).or_default() += 1;
                    }
                    println!("    group {name}: {by:?}");
                    spread.insert(name.clone(), json!(by));
                }
                entry["groups"] = Value::Object(spread);
            }
            cands.push(entry);
            turns_out.push(json!({
                "track": t.label,
                "candidate": c.name(),
                "turns": d.turns.iter().map(|u| json!([u.speaker, u.start.as_millis(), u.end.as_millis()])).collect::<Vec<_>>(),
            }));
        }
        report.push(json!({
            "track": t.label,
            "kind": t.kind,
            "embeddings": embeddings,
            "candidates": cands,
        }));
    }
    let write = |name: &str, v: &Value| {
        let path = a.into.join(name);
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(v).unwrap_or_else(|e| fail(e)),
        )
        .unwrap_or_else(|e| fail(format!("writing {}: {e}", path.display())));
    };
    write("meetings-cap.json", &json!({ "tracks": report }));
    write("meetings-cap-turns.json", &json!(turns_out));
}
