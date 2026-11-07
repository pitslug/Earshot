//! `accept m5`: storage and Obsidian export (T5.5, PLAN.md 3.8 and 4.2 `exp.*`).
//!
//! Cases are `acceptance/export/<name>.json` (the renderer's input: meeting, attendees,
//! speakers, segments, notes, summary and export options, plus the expected vault path)
//! with `<name>.md`, the expected note byte for byte. Every case is run.
//!
//! - `exp.golden` (pass): for every case, `earshot_core::export::render` produces exactly
//!   the expected path and the expected bytes.
//! - `exp.idempotent` (pass): for every case, through the real writer
//!   (`earshot_store::export_note`, in-memory store, a fresh vault under the system temp
//!   dir, never a real vault):
//!   1. the first export creates the note with exactly the rendered bytes;
//!   2. a second export is `Unchanged` and the bytes are identical;
//!   3. after user edits (a frontmatter key, a tag, text above and below the managed
//!      region, a changed owned key, a scribble inside the region) a re-export keeps every
//!      user edit outside the region, restores the owned key, merges the tag, and leaves
//!      the managed region equal to the render;
//!   4. a further export is `Unchanged` and byte-identical (fixed point);
//!   5. after the user renames and moves the note, the export finds it by `earshot_id`
//!      and writes no second copy;
//!   6. with a store that has no export record (the database was lost), the export
//!      searches the whole vault, adopts the moved and renamed note and records it: no
//!      second copy. (Crashes before the rename are forced in `earshot_store`'s unit
//!      tests: since T5.4a the record is written before the rename.)
//!
//! Check: the case set contains [`REQUIRED_CASES`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use earshot_core::asr::Segment;
use earshot_core::export::{
    BEGIN_MARKER, END_MARKER, ExportInput, ExportOptions, RenderedExport, render,
};
use earshot_core::identity::{DeviceId, ManualClock};
use earshot_core::model::{ExportRepo, Meeting, MeetingRepo, Note, Speaker, TranscriptPass};
use earshot_core::summary::SummaryV1;
use earshot_store::{ExportAction, ExportReport, Store, export_note};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde::Deserialize;

use super::{Check, Ctx, Measurement, RunOutput};

/// Case names the golden set must contain.
pub const REQUIRED_CASES: &[&str] = &["em_dash_off", "full", "minimal", "no_summary"];

/// Device id used for the export records.
const DEVICE: DeviceId = DeviceId::from_bytes([0x5a; 16]);

/// Export options as stored in a case file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseOptions {
    folder: String,
    link_attendees: bool,
    replace_em_dash: bool,
    tags: Vec<String>,
}

/// The renderer input stored in `<name>.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseInput {
    options: CaseOptions,
    expected_path: String,
    meeting: Meeting,
    attendees: Vec<String>,
    speakers: Vec<Speaker>,
    segments: Vec<Segment>,
    notes: Vec<Note>,
    summary: Option<SummaryV1>,
    /// Which pass the segments are (`"final"`, `"live"`) or null without a transcript.
    transcript: Option<TranscriptPass>,
}

/// One golden case.
#[derive(Debug, Clone)]
pub struct ExportCase {
    /// File stem.
    pub name: String,
    /// Renderer input.
    pub input: CaseInput,
    /// Expected note.
    pub expected: String,
}

impl ExportCase {
    fn render(&self) -> Result<RenderedExport, String> {
        let i = &self.input;
        let o = ExportOptions {
            folder: i.options.folder.clone(),
            link_attendees: i.options.link_attendees,
            replace_em_dash: i.options.replace_em_dash,
            tags: i.options.tags.clone(),
        };
        render(
            &ExportInput {
                meeting: &i.meeting,
                attendees: &i.attendees,
                speakers: &i.speakers,
                segments: &i.segments,
                notes: &i.notes,
                summary: i.summary.as_ref(),
                transcript: i.transcript,
            },
            &o,
        )
        .map_err(|e| format!("render: {e}"))
    }
}

/// Loads every `<name>.json` in `dir` with its `<name>.md`, sorted by name. A JSON without
/// its expected note, or a note without its JSON, is an error.
pub fn load_cases(dir: &Path) -> Result<Vec<ExportCase>> {
    let mut stems: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        let (Some(stem), Some(ext)) = (p.file_stem(), p.extension()) else {
            continue;
        };
        let entry = stems
            .entry(stem.to_string_lossy().into_owned())
            .or_default();
        match ext.to_str() {
            Some("json") => entry.0 = true,
            Some("md") => entry.1 = true,
            _ => bail!("unexpected file {} in {}", p.display(), dir.display()),
        }
    }
    let mut cases = Vec::new();
    for (name, (json, md)) in stems {
        if !(json && md) {
            bail!("case {name} needs both {name}.json and {name}.md");
        }
        let input: CaseInput = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{name}.json")))
                .with_context(|| format!("reading case {name}"))?,
        )
        .with_context(|| format!("parsing case {name}"))?;
        let expected = std::fs::read_to_string(dir.join(format!("{name}.md")))?;
        cases.push(ExportCase {
            name,
            input,
            expected,
        });
    }
    Ok(cases)
}

/// `exp.golden` for one case.
pub fn golden(case: &ExportCase) -> Result<(), String> {
    let r = case.render()?;
    if r.path.as_str() != case.input.expected_path {
        return Err(format!(
            "path {:?}, expected {:?}",
            r.path.as_str(),
            case.input.expected_path
        ));
    }
    let got = r.to_file();
    if got != case.expected {
        let line = got
            .lines()
            .zip(case.expected.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| got.lines().count().min(case.expected.lines().count()));
        return Err(format!(
            "differs from the golden file at line {} (got {:?}, expected {:?}; {} vs {} bytes)",
            line + 1,
            got.lines().nth(line),
            case.expected.lines().nth(line),
            got.len(),
            case.expected.len()
        ));
    }
    Ok(())
}

fn ensure(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what()) }
}

fn md_files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.push(
                    p.strip_prefix(base)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The user's edits of step 3, applied to an exported note.
fn user_edit(doc: &str) -> Result<String, String> {
    let date_line = doc
        .lines()
        .find(|l| l.starts_with("date: "))
        .ok_or("no date line")?
        .to_owned();
    let mut s = doc.replacen(
        "source: earshot\n",
        "source: earshot\nreviewed_by: \"me\"\n",
        1,
    );
    s = if s.contains("tags: []\n") {
        s.replacen("tags: []\n", "tags:\n  - user-tag\n", 1)
    } else {
        s.replacen("tags:\n", "tags:\n  - user-tag\n", 1)
    };
    s = s.replacen(&date_line, "date: 1999-01-01", 1);
    s = s.replacen(
        &format!("{BEGIN_MARKER}\n"),
        &format!("User intro line.\n\n{BEGIN_MARKER}\nuser scribble inside\n"),
        1,
    );
    s.push_str("\nUser outro line.\n");
    for (needle, what) in [
        ("reviewed_by: \"me\"\n", "user key"),
        ("  - user-tag\n", "user tag"),
        ("date: 1999-01-01\n", "owned key edit"),
        ("user scribble inside\n", "scribble"),
    ] {
        ensure(s.contains(needle), || {
            format!("could not apply the {what} edit")
        })?;
    }
    Ok(s)
}

/// `exp.idempotent` for one case, in a fresh vault at `vault`.
pub fn idempotent(case: &ExportCase, vault: &Path) -> Result<(), String> {
    let r = case.render()?;
    let meeting = &case.input.meeting;
    std::fs::create_dir_all(vault).map_err(|e| e.to_string())?;
    let clock = ManualClock::new(meeting.started_unix_ms);
    let mut rng = ChaCha8Rng::seed_from_u64(5);
    let new_store = || -> Result<Store, String> {
        let mut s = Store::open_in_memory().map_err(|e| e.to_string())?;
        s.put_meeting(meeting.clone()).map_err(|e| e.to_string())?;
        Ok(s)
    };
    let mut export = |store: &mut Store| -> Result<ExportReport, String> {
        clock.advance_ms(1000);
        export_note(store, vault, DEVICE, meeting.id, &r, &clock, &mut rng)
            .map_err(|e| e.to_string())
    };
    let read = |rel: &str| -> Result<String, String> {
        let mut p = vault.to_path_buf();
        p.extend(rel.split('/'));
        std::fs::read_to_string(p).map_err(|e| e.to_string())
    };
    let mut store = new_store()?;

    // 1. Create.
    let a = export(&mut store)?;
    ensure(
        a.action == ExportAction::Created && a.path == r.path,
        || format!("first export: {:?} at {}", a.action, a.path),
    )?;
    let first = read(a.path.as_str())?;
    ensure(first == r.to_file(), || {
        "first export differs from the render".into()
    })?;

    // 2. Byte-identical re-export.
    let b = export(&mut store)?;
    ensure(b.action == ExportAction::Unchanged, || {
        format!("re-export: {:?}", b.action)
    })?;
    ensure(read(a.path.as_str())? == first, || {
        "re-export changed the bytes".into()
    })?;

    // 3. User edits survive.
    let edited = user_edit(&first)?;
    let mut file = vault.to_path_buf();
    file.extend(a.path.components());
    std::fs::write(&file, &edited).map_err(|e| e.to_string())?;
    let c = export(&mut store)?;
    ensure(c.action == ExportAction::Updated, || {
        format!("export after edits: {:?}", c.action)
    })?;
    let merged = read(a.path.as_str())?;
    let date_block = r
        .frontmatter
        .iter()
        .find(|(k, _)| k == "date")
        .map(|(_, t)| t.as_str())
        .ok_or("render has no date")?;
    for (ok, what) in [
        (
            merged.contains("reviewed_by: \"me\"\n"),
            "user frontmatter key lost",
        ),
        (merged.contains("  - user-tag\n"), "user tag lost"),
        (
            merged.contains(date_block) && !merged.contains("1999-01-01"),
            "owned key not restored",
        ),
        (
            merged.contains(&format!("User intro line.\n\n{BEGIN_MARKER}")),
            "text above the region lost",
        ),
        (
            merged.ends_with(&format!("{END_MARKER}\n\nUser outro line.\n")),
            "text below the region lost",
        ),
        (
            !merged.contains("user scribble inside"),
            "managed region not replaced",
        ),
    ] {
        ensure(ok, || what.to_owned())?;
    }
    let begin = merged.find(BEGIN_MARKER).ok_or("begin marker missing")?;
    let end = merged.find(END_MARKER).ok_or("end marker missing")? + END_MARKER.len() + 1;
    ensure(merged.get(begin..end) == Some(r.region.as_str()), || {
        "managed region differs from the render".into()
    })?;

    // 4. Fixed point.
    let d = export(&mut store)?;
    ensure(d.action == ExportAction::Unchanged, || {
        format!("export after merge: {:?}", d.action)
    })?;
    ensure(read(a.path.as_str())? == merged, || {
        "export after merge changed the bytes".into()
    })?;

    // 5. Renamed and moved by the user.
    let moved_rel = "Moved by user/renamed by user.md";
    let mut moved = vault.to_path_buf();
    moved.extend(moved_rel.split('/'));
    std::fs::create_dir_all(moved.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    std::fs::rename(&file, &moved).map_err(|e| e.to_string())?;
    let e = export(&mut store)?;
    ensure(
        e.path.as_str() == moved_rel && e.action == ExportAction::Unchanged,
        || format!("after a move: {:?} at {}", e.action, e.path),
    )?;
    ensure(md_files(vault) == vec![moved_rel.to_owned()], || {
        format!("after a move the vault has {:?}", md_files(vault))
    })?;

    // 6. No export record (database lost) and the note renamed and moved: adopted, not
    //    duplicated, and recorded.
    let mut fresh = new_store()?;
    let f = export(&mut fresh)?;
    ensure(
        f.path.as_str() == moved_rel && f.action == ExportAction::Unchanged,
        || format!("orphan recovery: {:?} at {}", f.action, f.path),
    )?;
    ensure(md_files(vault) == vec![moved_rel.to_owned()], || {
        format!("orphan recovery left {:?}", md_files(vault))
    })?;
    let rec = fresh
        .export_record(meeting.id, DEVICE)
        .map_err(|e| e.to_string())?
        .ok_or("orphan recovery wrote no record")?;
    ensure(rec.path.as_str() == moved_rel, || {
        format!("record points at {}", rec.path)
    })?;
    Ok(())
}

/// Evaluates loaded cases, using `scratch` for the per-case vaults.
pub fn evaluate(cases: &[ExportCase], scratch: &Path) -> RunOutput {
    let mut golden_fail = Vec::new();
    let mut idem_fail = Vec::new();
    for c in cases {
        if let Err(e) = golden(c) {
            golden_fail.push(format!("{}: {e}", c.name));
        }
        let vault: PathBuf = scratch.join(&c.name).join("vault");
        if let Err(e) = idempotent(c, &vault) {
            idem_fail.push(format!("{}: {e}", c.name));
        }
    }
    let n = cases.len();
    let measurement = |id: &str, fails: &[String], what: &str| {
        let mut m = Measurement::passed(id, n > 0 && fails.is_empty());
        m.detail = Some(if n == 0 {
            "no cases".to_owned()
        } else if fails.is_empty() {
            format!("{n}/{n} cases {what}")
        } else {
            format!("{}/{n} cases failed: {}", fails.len(), fails.join("; "))
        });
        m
    };
    let missing: Vec<&str> = REQUIRED_CASES
        .iter()
        .copied()
        .filter(|r| !cases.iter().any(|c| c.name == *r))
        .collect();
    RunOutput {
        measurements: vec![
            measurement("exp.golden", &golden_fail, "equal their golden files"),
            measurement(
                "exp.idempotent",
                &idem_fail,
                "re-export byte-identically, keep user edits, follow moves and recover orphans",
            ),
        ],
        checks: vec![Check::new(
            "export case coverage",
            missing.is_empty(),
            if missing.is_empty() {
                format!("{n} cases including {}", REQUIRED_CASES.join(", "))
            } else {
                format!("missing cases: {}", missing.join(", "))
            },
        )],
    }
}

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    let cases = load_cases(&ctx.root.join("acceptance").join("export"))?;
    let scratch = std::env::temp_dir().join(format!("earshot-accept-m5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    let out = evaluate(&cases, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_cases() -> Vec<ExportCase> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        load_cases(&root.join("acceptance").join("export")).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("earshot-m5-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn m5_committed_cases_pass() {
        let dir = scratch("pass");
        let out = evaluate(&repo_cases(), &dir);
        let _ = std::fs::remove_dir_all(&dir);
        for m in &out.measurements {
            assert_eq!(m.passed, Some(true), "{}: {:?}", m.id, m.detail);
        }
        assert!(out.checks.iter().all(|c| c.pass), "{:?}", out.checks);
    }

    #[test]
    fn m5_a_changed_golden_file_fails_exp_golden() {
        let mut cases = repo_cases();
        cases[0].expected = cases[0]
            .expected
            .replacen("source: earshot", "source: other", 1);
        let dir = scratch("golden");
        let out = evaluate(&cases, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out.measurements[0].passed, Some(false));
        assert_eq!(
            out.measurements[1].passed,
            Some(true),
            "idempotence does not use the golden"
        );
    }

    #[test]
    fn m5_a_wrong_expected_path_fails_exp_golden() {
        let mut cases = repo_cases();
        cases[0].input.expected_path = "Elsewhere/x.md".into();
        assert!(golden(&cases[0]).is_err());
    }

    #[test]
    fn m5_idempotence_fails_when_the_writer_does_not_produce_the_render() {
        // A foreign note at the target path forces the writer to a "(2)" name, which the
        // harness must report as a failed first export.
        let case = &repo_cases()[0];
        let dir = scratch("foreign");
        let vault = dir.join("vault");
        let mut target = vault.clone();
        target.extend(case.input.expected_path.split('/'));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "not ours\n").unwrap();
        let r = idempotent(case, &vault);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.unwrap_err().starts_with("first export"));
    }

    #[test]
    fn m5_no_cases_fail_both_and_coverage() {
        let out = evaluate(&[], &scratch("none"));
        assert!(out.measurements.iter().all(|m| m.passed == Some(false)));
        assert!(!out.checks[0].pass);
    }

    #[test]
    fn m5_user_edit_touches_every_edited_part() {
        let case = &repo_cases()[0];
        let doc = case.render().unwrap().to_file();
        let edited = user_edit(&doc).unwrap();
        assert!(edited.contains("User intro line.") && edited.ends_with("User outro line.\n"));
        assert!(!edited.contains(&case.render().unwrap().frontmatter[1].1));
    }
}
