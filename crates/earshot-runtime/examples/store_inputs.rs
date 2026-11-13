//! `store_inputs` (T12.25a, evaluation only): turns the owner's own recorded meetings into
//! summary inputs for `sum_eval --job real:store`, privately. Run by `cargo xtask sum-eval
//! --store-db <earshot.db> --store-run <name>`, which checks first that the run folder is
//! git-ignored.
//!
//! 1. Copies `--db` (and its `-wal`, when there is one; never the `-shm`) into `--into`
//!    (`fixtures/private/real-meetings/<run>/`), refusing a folder that already holds a copy.
//!    The original is read once, as a file copy, and never opened.
//! 2. Opens the copy read-only (`earshot_store::ReadOnlyStore`: SQLite's read-only open,
//!    never `Store::open`, no migration).
//! 3. Picks meetings by a rule that reads no text ([`pick`]): live meetings with a Final
//!    transcript and a duration of at least `--min-minutes` (default 15), not named by
//!    `--exclude <id,...>`, most recent start first, the first `--count` (default 6).
//! 4. Writes, for each picked meeting `r<k>` (k = 1 for the most recent):
//!    `inputs/r<k>.json`, the `SummaryInput` production builds for it
//!    (`MeetingView::load` + `earshot_runtime::summary::evaluation_input`: the Final
//!    transcript, speaker names as the app shows them, no style, template or own words),
//!    which `sum_eval --store-inputs <run>` reads as the `earshot` source reads its inputs;
//!    `transcripts/r<k>.txt`, the transcript as a model reads it (the user turn of a
//!    single-request plan: header and numbered lines), for the real-item list writers and the
//!    judges; and `picked.json`, the labels with their meeting ids, starts and durations.
//!
//! Everything it writes stays in `--into`. It prints counts and the folder only: never a
//! meeting id, title, name or transcript text, also in its errors.

use std::path::{Path, PathBuf};

use earshot_core::identity::EntityId;
use earshot_core::model::{Meeting, MeetingRepo, TranscriptPass};
use earshot_core::prompt::{ContextBudget, PromptPlan, Role, SummaryInput, plan};
use earshot_runtime::summary::evaluation_input;
use earshot_runtime::view::MeetingView;
use earshot_store::ReadOnlyStore;
use serde_json::json;

/// What to do.
#[derive(Debug, Clone, PartialEq)]
struct Args {
    db: PathBuf,
    into: PathBuf,
    count: usize,
    min_minutes: u64,
    exclude: Vec<EntityId>,
}

/// One meeting as the pick rule sees it: no text.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    id: EntityId,
    started_unix_ms: u64,
    duration_ms: Option<u64>,
    deleted: bool,
    has_final: bool,
}

/// How many meetings each step of the rule kept (the only thing printed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Counts {
    /// Live meetings in the copy.
    meetings: usize,
    /// Of those, with a Final transcript and the minimum duration.
    eligible: usize,
    /// Of those, excluded by `--exclude`.
    excluded: usize,
    /// Picked.
    picked: usize,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        db: PathBuf::new(),
        into: PathBuf::new(),
        count: 6,
        min_minutes: 15,
        exclude: Vec::new(),
    };
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => a.db = val()?.into(),
            "--into" => a.into = val()?.into(),
            "--count" => a.count = val()?.parse().map_err(|e| format!("--count: {e}"))?,
            "--min-minutes" => {
                a.min_minutes = val()?.parse().map_err(|e| format!("--min-minutes: {e}"))?;
            }
            "--exclude" => {
                for (i, id) in val()?
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .enumerate()
                {
                    // The id itself is never echoed (an error names its position only).
                    a.exclude.push(
                        id.trim().parse().map_err(|_| {
                            format!("--exclude: entry {} is not a meeting id", i + 1)
                        })?,
                    );
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.db.as_os_str().is_empty() || a.into.as_os_str().is_empty() {
        return Err("needs --db and --into".into());
    }
    if a.count == 0 {
        return Err("--count must be at least 1".into());
    }
    Ok(a)
}

/// The pick rule (T12.25a, reads no text): live meetings with a Final transcript lasting at
/// least `min_ms`, minus `exclude`, the most recent start first (ties by id, newest first),
/// at most `count`. Returns the picked ids and the counts of each step.
fn pick(
    candidates: &[Candidate],
    count: usize,
    min_ms: u64,
    exclude: &[EntityId],
) -> (Vec<EntityId>, Counts) {
    let live: Vec<&Candidate> = candidates.iter().filter(|c| !c.deleted).collect();
    let mut eligible: Vec<&Candidate> = live
        .iter()
        .copied()
        .filter(|c| c.has_final && c.duration_ms.is_some_and(|d| d >= min_ms))
        .collect();
    let all_eligible = eligible.len();
    eligible.retain(|c| !exclude.contains(&c.id));
    let excluded = all_eligible - eligible.len();
    eligible.sort_by(|a, b| {
        b.started_unix_ms
            .cmp(&a.started_unix_ms)
            .then(b.id.cmp(&a.id))
    });
    let picked: Vec<EntityId> = eligible.iter().take(count).map(|c| c.id).collect();
    let counts = Counts {
        meetings: live.len(),
        eligible: all_eligible,
        excluded,
        picked: picked.len(),
    };
    (picked, counts)
}

/// The transcript as a model reads it: the user turn of a single-request plan of `input`
/// (a budget large enough that no meeting is split), with the header and numbered lines.
fn transcript_text(input: &SummaryInput) -> Result<String, String> {
    let budget = ContextBudget {
        context_window: 1 << 30,
        max_output_tokens: 8192,
    };
    match plan(input, budget).map_err(|e| e.to_string())? {
        PromptPlan::Single(p) => Ok(p
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")),
        PromptPlan::MapReduce(_) => Err("a meeting did not fit one request".into()),
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Steps 1 to 4 of the module docs.
fn run(a: &Args) -> Result<Counts, String> {
    let copy = a.into.join("earshot.db");
    if copy.exists() {
        return Err(format!(
            "{} already holds a copy; give the run another name",
            a.into.display()
        ));
    }
    if !a.db.is_file() {
        return Err("--db is not a file".into());
    }
    // SQLite keeps `-shm` beside the database while any connection has it open, and removes
    // it when the last one closes: a copy taken now could be a torn database and WAL pair.
    let mut shm = a.db.clone().into_os_string();
    shm.push("-shm");
    if Path::new(&shm).exists() {
        return Err(
            "the database is open (its -shm file exists); quit Earshot and try again".into(),
        );
    }
    for sub in ["inputs", "transcripts"] {
        let d = a.into.join(sub);
        std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    std::fs::copy(&a.db, &copy).map_err(|e| format!("copying the database: {e}"))?;
    let mut wal = a.db.clone().into_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    if wal.is_file() {
        std::fs::copy(&wal, a.into.join("earshot.db-wal"))
            .map_err(|e| format!("copying the database's WAL: {e}"))?;
    }
    let store = ReadOnlyStore::open(&copy).map_err(|e| format!("the copy: {e}"))?;
    let meetings: Vec<Meeting> = store.meetings().map_err(|e| e.to_string())?;
    let mut candidates = Vec::with_capacity(meetings.len());
    for m in &meetings {
        candidates.push(Candidate {
            id: m.id,
            started_unix_ms: m.started_unix_ms,
            duration_ms: m.duration_ms,
            deleted: m.meta.deleted,
            has_final: !m.meta.deleted
                && store
                    .has_live_segment(m.id, TranscriptPass::Final)
                    .map_err(|e| e.to_string())?,
        });
    }
    let unknown = a
        .exclude
        .iter()
        .filter(|id| !meetings.iter().any(|m| m.id == **id))
        .count();
    if unknown > 0 {
        return Err(format!(
            "{unknown} --exclude id(s) name no meeting in the copy"
        ));
    }
    let (picked, counts) = pick(&candidates, a.count, a.min_minutes * 60_000, &a.exclude);
    let mut index = Vec::new();
    for (k, id) in picked.iter().enumerate() {
        let label = format!("r{}", k + 1);
        let view = MeetingView::load(&store, *id)
            .map_err(|e| format!("{label}: {e}"))?
            .ok_or_else(|| format!("{label}: the meeting is gone"))?;
        if view.pass != Some(TranscriptPass::Final) {
            return Err(format!("{label}: no Final transcript"));
        }
        let input = evaluation_input(&view);
        write(
            &a.into.join("inputs").join(format!("{label}.json")),
            &serde_json::to_vec_pretty(&input).map_err(|e| e.to_string())?,
        )?;
        write(
            &a.into.join("transcripts").join(format!("{label}.txt")),
            transcript_text(&input)
                .map_err(|e| format!("{label}: {e}"))?
                .as_bytes(),
        )?;
        index.push(json!({
            "label": label,
            "meeting_id": id.to_string(),
            "started_unix_ms": view.meeting.started_unix_ms,
            "duration_ms": view.meeting.duration_ms,
            "segments": input.segments.len(),
        }));
    }
    let picked_file = json!({
        "task": "T12.25a",
        "rule": format!(
            "live meetings with a Final transcript lasting at least {} min, minus --exclude, \
             most recent start first, at most {}",
            a.min_minutes, a.count
        ),
        "counts": {
            "meetings": counts.meetings,
            "eligible": counts.eligible,
            "excluded": counts.excluded,
            "picked": counts.picked,
        },
        "meetings": index,
    });
    write(
        &a.into.join("picked.json"),
        &serde_json::to_vec_pretty(&picked_file).map_err(|e| e.to_string())?,
    )?;
    Ok(counts)
}

fn main() {
    let result = parse_args(std::env::args().skip(1)).and_then(|a| run(&a).map(|c| (a, c)));
    match result {
        Ok((a, c)) => println!(
            "store_inputs: {} meetings in the copy, {} with a Final transcript of {} min or \
             more, {} excluded, {} picked (r1 = the most recent); written to {}",
            c.meetings,
            c.eligible,
            a.min_minutes,
            c.excluded,
            c.picked,
            a.into.display()
        ),
        Err(e) => {
            eprintln!("store_inputs: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::asr::{MeetingTime, Segment};
    use earshot_core::capture::TrackId;
    use earshot_core::diar::SpeakerRole;
    use earshot_core::identity::{DeviceId, Hlc, ManualClock};
    use earshot_core::model::{ClockDomain, SegmentRow, Speaker, Stamp, Track};
    use earshot_core::prompt::Template;
    use earshot_store::Store;
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;

    fn id(n: u8) -> EntityId {
        format!("0193a5b0-1c00-7000-8000-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    fn cand(n: u8, started: u64, minutes: Option<u64>, has_final: bool) -> Candidate {
        Candidate {
            id: id(n),
            started_unix_ms: started,
            duration_ms: minutes.map(|m| m * 60_000),
            deleted: false,
            has_final,
        }
    }

    #[test]
    fn store_inputs_pick_takes_the_most_recent_long_final_meetings_only() {
        let mut deleted = cand(9, 900, Some(60), true);
        deleted.deleted = true;
        let all = [
            cand(1, 100, Some(30), true),
            cand(2, 200, Some(14), true),  // too short
            cand(3, 300, Some(15), true),  // exactly the minimum
            cand(4, 400, Some(40), false), // live draft only
            cand(5, 500, None, true),      // still recording
            cand(6, 600, Some(20), true),
            cand(7, 700, Some(25), true),
            deleted,
        ];
        let (picked, counts) = pick(&all, 2, 15 * 60_000, &[]);
        assert_eq!(picked, [id(7), id(6)]);
        assert_eq!(
            counts,
            Counts {
                meetings: 8 - 1,
                eligible: 4,
                excluded: 0,
                picked: 2
            }
        );
        // An excluded meeting makes room for the next one.
        let (picked, counts) = pick(&all, 2, 15 * 60_000, &[id(7), id(2)]);
        assert_eq!(picked, [id(6), id(3)]);
        assert_eq!(counts.excluded, 1);
        // Fewer eligible than asked for: all of them.
        let (picked, _) = pick(&all, 6, 15 * 60_000, &[]);
        assert_eq!(picked, [id(7), id(6), id(3), id(1)]);
    }

    #[test]
    fn store_inputs_args_never_echo_an_excluded_id() {
        let err = parse_args(
            ["--db", "a.db", "--into", "x", "--exclude", "not-an-id"].map(str::to_owned),
        )
        .unwrap_err();
        assert!(!err.contains("not-an-id"), "{err}");
        let a = parse_args(
            [
                "--db",
                "a.db",
                "--into",
                "x",
                "--exclude",
                &format!("{},{}", id(1), id(2)),
                "--count",
                "3",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(a.exclude, [id(1), id(2)]);
        assert_eq!((a.count, a.min_minutes), (3, 15));
        assert!(parse_args(["--db", "a.db"].map(str::to_owned)).is_err());
    }

    struct World {
        clock: ManualClock,
        hlc: Hlc,
        rng: ChaCha8Rng,
    }

    impl World {
        fn stamp(&mut self) -> Stamp<'_, ManualClock, ChaCha8Rng> {
            self.clock.advance_ms(1);
            Stamp {
                clock: &self.clock,
                hlc: &mut self.hlc,
                rng: &mut self.rng,
            }
        }
    }

    /// An invented meeting with a mic track, one named speaker and `lines` segments of
    /// `pass`, lasting `minutes`.
    fn add_meeting(
        store: &mut Store,
        w: &mut World,
        started: u64,
        minutes: u64,
        pass: TranscriptPass,
        lines: &[&str],
    ) -> EntityId {
        let mut m = Meeting::new(
            "Invented title",
            started,
            0,
            Template::General,
            &mut w.stamp(),
        );
        m.duration_ms = Some(minutes * 60_000);
        store.put_meeting(m.clone()).unwrap();
        let t = Track::new(
            m.id,
            TrackId::Mic,
            None,
            ClockDomain::Replay,
            &mut w.stamp(),
        );
        store.put_track(t.clone()).unwrap();
        let s = Speaker::new(m.id, SpeakerRole::Me, "Robin Example", None, &mut w.stamp());
        store.put_speaker(s.clone()).unwrap();
        for (i, text) in lines.iter().enumerate() {
            w.clock.advance_ms(1);
            let seg_id = EntityId::generate(&w.clock, &mut w.rng);
            let seg = Segment {
                id: seg_id,
                track: t.id,
                start: MeetingTime::from_millis(i as u64 * 5_000),
                end: MeetingTime::from_millis(i as u64 * 5_000 + 4_000),
                text: (*text).to_owned(),
                words: Vec::new(),
                confidence: 1.0,
                speaker_id: Some(s.id),
            };
            w.clock.advance_ms(1);
            let row = SegmentRow::new(m.id, pass, seg, &mut w.hlc, &w.clock);
            store.put_segment(row).unwrap();
        }
        m.id
    }

    fn temp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "earshot-store-inputs-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn store_inputs_write_production_inputs_of_the_picked_meetings_from_a_read_only_copy() {
        let root = temp_root("run");
        let db = root.join("source").join("earshot.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let mut w = World {
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([7; 16])),
            rng: ChaCha8Rng::seed_from_u64(5),
        };
        let (old, new, draft) = {
            let mut store = Store::open(&db).unwrap();
            let old = add_meeting(
                &mut store,
                &mut w,
                1_800_000_000_000,
                20,
                TranscriptPass::Final,
                &["We agree to ship the blue one.", "I will send the notes."],
            );
            let new = add_meeting(
                &mut store,
                &mut w,
                1_800_100_000_000,
                45,
                TranscriptPass::Final,
                &["Let us start."],
            );
            let draft = add_meeting(
                &mut store,
                &mut w,
                1_800_200_000_000,
                45,
                TranscriptPass::Live,
                &["A draft only."],
            );
            (old, new, draft)
        };
        let source_bytes = std::fs::read(&db).unwrap();
        let into = root.join("private").join("run-1");
        let a = Args {
            db: db.clone(),
            into: into.clone(),
            count: 6,
            min_minutes: 15,
            exclude: Vec::new(),
        };
        let counts = run(&a).unwrap();
        assert_eq!(
            counts,
            Counts {
                meetings: 3,
                eligible: 2,
                excluded: 0,
                picked: 2
            }
        );
        // The original is untouched.
        assert_eq!(std::fs::read(&db).unwrap(), source_bytes);
        // r1 is the most recent Final meeting; the draft is never picked.
        let picked: serde_json::Value =
            serde_json::from_slice(&std::fs::read(into.join("picked.json")).unwrap()).unwrap();
        let ids: Vec<&str> = picked["meetings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["meeting_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [new.to_string(), old.to_string()]);
        assert!(!ids.contains(&draft.to_string().as_str()));
        // The input is what production builds from the view of the copy.
        let r2: SummaryInput =
            serde_json::from_slice(&std::fs::read(into.join("inputs").join("r2.json")).unwrap())
                .unwrap();
        let ro = ReadOnlyStore::open(&into.join("earshot.db")).unwrap();
        let view = MeetingView::load(&ro, old).unwrap().unwrap();
        assert_eq!(r2, evaluation_input(&view));
        assert_eq!(r2.segments.len(), 2);
        assert_eq!(r2.segments[0].speaker, "Robin Example");
        assert!(r2.style.is_empty() && r2.vocabulary.is_empty());
        // The judge's transcript holds the numbered lines the model reads.
        let text = std::fs::read_to_string(into.join("transcripts").join("r2.txt")).unwrap();
        assert!(text.contains("We agree to ship the blue one."), "{text}");
        assert!(text.contains("[2]"), "{text}");
        // A second run into the same folder is refused, the copy kept.
        assert!(run(&a).unwrap_err().contains("already holds a copy"));
        // An --exclude id that names nothing is refused without naming it.
        let b = Args {
            into: root.join("private").join("run-2"),
            exclude: vec![id(1)],
            ..a.clone()
        };
        let err = run(&b).unwrap_err();
        assert!(!err.contains(&id(1).to_string()), "{err}");
        // Excluding the newest picks the older one as r1.
        let c = Args {
            into: root.join("private").join("run-3"),
            exclude: vec![new],
            ..a
        };
        let counts = run(&c).unwrap();
        assert_eq!((counts.excluded, counts.picked), (1, 1));
        // A database some process still has open (its -shm exists) is not copied.
        let mut shm = c.db.clone().into_os_string();
        shm.push("-shm");
        std::fs::write(&shm, b"").unwrap();
        let d = Args {
            into: root.join("private").join("run-4"),
            ..c
        };
        assert!(run(&d).unwrap_err().contains("is open"));
        assert!(!d.into.join("earshot.db").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
