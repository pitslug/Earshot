//! Obsidian renderer tests (`cargo test -p earshot-core export`).

use std::path::PathBuf;

use yaml_rust2::{Yaml, YamlLoader};

use super::*;
use crate::asr::{MeetingTime, Segment, Word};
use crate::diar::SpeakerRole;
use crate::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use crate::model::{Meeting, Note, Speaker, TranscriptPass};
use crate::prompt::Template;
use crate::summary::{ActionItem, Decision, NoteBullet, NoteSection, SummaryV1};

/// Compares `actual` with `src/export/snapshots/<name>.md`. Set `EARSHOT_UPDATE_SNAPSHOTS=1`
/// to (re)write it, then review the diff. A missing snapshot fails; it is never created
/// silently. Only git's line-ending conversion is tolerated (the renderer emits LF only).
fn assert_snapshot(name: &str, actual: &str) {
    assert!(!actual.contains('\r'), "renderer output is LF only");
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "src", "export", "snapshots"]
        .iter()
        .collect::<PathBuf>()
        .join(format!("{name}.md"));
    if std::env::var_os("EARSHOT_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| {
            panic!(
                "snapshot {} is missing ({e}); run with EARSHOT_UPDATE_SNAPSHOTS=1 and review it",
                path.display()
            )
        })
        .replace("\r\n", "\n");
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "snapshot {name} differs at line {}:\n expected: {:?}\n   actual: {:?}\n(re-run with EARSHOT_UPDATE_SNAPSHOTS=1 if the change is intended)",
            line + 1,
            expected.lines().nth(line),
            actual.lines().nth(line)
        );
    }
}

fn id(n: u8) -> EntityId {
    let mut b = [0u8; 16];
    b[0] = 0x01;
    b[1] = 0x92;
    b[6] = 0x70;
    b[8] = 0x80;
    b[15] = n;
    EntityId::from_bytes(b)
}

fn meta() -> SyncMeta {
    let node = DeviceId::from_bytes([7; 16]);
    let t = HlcTimestamp {
        wall_ms: 1,
        logical: 0,
        node,
    };
    SyncMeta {
        created_at: t,
        updated_at: t,
        deleted: false,
        origin_device_id: node,
    }
}

struct Fixture {
    meeting: Meeting,
    speakers: Vec<Speaker>,
    segments: Vec<Segment>,
    notes: Vec<Note>,
    summary: SummaryV1,
    attendees: Vec<String>,
}

/// The invented meeting these tests export.
fn fixture() -> Fixture {
    let m = Meeting {
        id: id(1),
        meta: meta(),
        title: "Q4 planning \u{2014} budget/forecast".into(),
        started_unix_ms: 1_792_209_900_000,
        utc_offset_minutes: 600,
        duration_ms: Some(2_850_000),
        tags: vec!["planning".into(), "Team Sync".into()],
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    };
    let me = Speaker {
        id: id(10),
        meta: meta(),
        meeting_id: m.id,
        role: SpeakerRole::Me,
        display_name: "Me".into(),
        person_id: None,
        ignored: false,
    };
    let ada = Speaker {
        id: id(11),
        meta: meta(),
        meeting_id: m.id,
        role: SpeakerRole::Remote,
        display_name: "Ada Lovelace".into(),
        person_id: Some(id(50)),
        ignored: false,
    };
    let seg = |n: u8, sp: EntityId, start: u64, words: &[&str]| {
        let mut s = Segment::from_words(
            id(n),
            id(2),
            words
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    Word::new(
                        *w,
                        start + i as u64 * 300,
                        start + i as u64 * 300 + 250,
                        0.9,
                    )
                })
                .collect(),
            MeetingTime::ZERO,
        );
        s.speaker_id = Some(sp);
        s
    };
    let segments = vec![
        seg(
            20,
            me.id,
            1_000,
            &["Let's", "start", "with", "the", "budget."],
        ),
        seg(
            21,
            ada.id,
            65_000,
            &[
                "We",
                "are",
                "over",
                "by",
                "ten",
                "percent\u{2014}mostly",
                "travel.",
            ],
        ),
        seg(22, me.id, 3_725_000, &["Then", "we", "cut", "travel."]),
    ];
    let notes = vec![
        Note {
            id: id(30),
            meta: meta(),
            meeting_id: m.id,
            at: MeetingTime::from_millis(75_000),
            text: "budget over\ncheck travel \u{2014} ask Ada".into(),
        },
        Note {
            id: id(31),
            meta: meta(),
            meeting_id: m.id,
            at: MeetingTime::from_millis(2_000_000),
            text: "decide by Friday".into(),
        },
    ];
    let summary = SummaryV1 {
        title: "Q4 budget".into(),
        tldr: "Budget is 10% over\u{2014}travel is the cause. Travel gets cut.".into(),
        enhanced_notes: vec![NoteSection {
            heading: "Budget".into(),
            bullets: vec![
                NoteBullet {
                    text: "Over by ten percent \u{2014} mostly travel".into(),
                    source_segment_ids: vec![id(21)],
                },
                NoteBullet {
                    text: "Check travel with Ada".into(),
                    source_segment_ids: vec![],
                },
            ],
        }],
        decisions: vec![Decision {
            text: "Cut travel".into(),
            source_segment_ids: vec![id(22)],
        }],
        action_items: vec![
            ActionItem {
                owner: "Ada Lovelace".into(),
                task: "Send revised travel numbers".into(),
                due: Some("Friday".into()),
                source_segment_ids: vec![id(21)],
            },
            ActionItem {
                owner: "Me".into(),
                task: "Update the forecast".into(),
                due: None,
                source_segment_ids: vec![id(22)],
            },
        ],
        open_questions: vec!["Does the cut include conferences?".into()],
        topics: vec!["budget".into()],
    };
    Fixture {
        meeting: m,
        speakers: vec![me, ada],
        segments,
        notes,
        summary,
        attendees: vec!["Ada Lovelace".into(), "Me".into()],
    }
}

fn input(f: &Fixture) -> ExportInput<'_> {
    ExportInput {
        meeting: &f.meeting,
        attendees: &f.attendees,
        speakers: &f.speakers,
        segments: &f.segments,
        notes: &f.notes,
        summary: Some(&f.summary),
        transcript: Some(TranscriptPass::Final),
        unknown_speaker_lines: &[],
        user: None,
    }
}

fn opts() -> ExportOptions {
    ExportOptions::default()
}

/// The frontmatter text between the `---` lines.
fn frontmatter(doc: &str) -> &str {
    let rest = doc.strip_prefix("---\n").expect("starts with ---");
    &rest[..rest.find("\n---\n").expect("closing ---") + 1]
}

// ---- golden files ----

#[test]
fn export_golden_full_meeting() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    assert_snapshot("full", &r.to_file());
}

#[test]
fn export_golden_without_summary() {
    let f = fixture();
    let mut i = input(&f);
    i.summary = None;
    // Not processed yet: the live draft, marked as such.
    i.transcript = Some(TranscriptPass::Live);
    let r = render(&i, &opts()).unwrap();
    assert_snapshot("no_summary", &r.to_file());
}

#[test]
fn export_golden_em_dash_kept_and_plain_attendees() {
    let f = fixture();
    let o = ExportOptions {
        replace_em_dash: false,
        link_attendees: false,
        ..opts()
    };
    let r = render(&input(&f), &o).unwrap();
    assert_snapshot("em_dash_off", &r.to_file());
}

#[test]
fn export_golden_unknown_speaker_line() {
    // T12.65: the last line is one the final pass left without a speaker: "Unknown speaker".
    // The first line also has no speaker but is not among them: "Unknown", as before.
    let mut f = fixture();
    let first = f.segments.first().unwrap().id;
    let last = f.segments.last().unwrap().id;
    for s in &mut f.segments {
        if s.id == first || s.id == last {
            s.speaker_id = None;
        }
    }
    let unknown = [last];
    let mut i = input(&f);
    i.unknown_speaker_lines = &unknown;
    let r = render(&i, &opts()).unwrap();
    let file = r.to_file();
    assert_eq!(file.matches(" Unknown speaker:**").count(), 1, "{file}");
    assert_eq!(file.matches(" Unknown:**").count(), 1, "{file}");
    assert_snapshot("unknown_speaker", &file);
}

// ---- file name ----

#[test]
fn export_file_name_uses_local_start_and_sanitised_title() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    assert_eq!(
        r.path.as_str(),
        "Meetings/2026-10-17 14-05 Q4 planning - budget-forecast.md"
    );
    let o = ExportOptions {
        folder: String::new(),
        ..opts()
    };
    assert_eq!(
        render(&input(&f), &o).unwrap().path.as_str(),
        "2026-10-17 14-05 Q4 planning - budget-forecast.md"
    );
}

#[test]
fn export_file_name_falls_back_to_the_summary_title_then_meeting() {
    let mut f = fixture();
    f.meeting.title = "  ".into();
    let r = render(&input(&f), &opts()).unwrap();
    assert!(
        r.path.as_str().ends_with(" 14-05 Q4 budget.md"),
        "{}",
        r.path
    );
    let mut i = input(&f);
    i.summary = None;
    let r = render(&i, &opts()).unwrap();
    assert!(r.path.as_str().ends_with(" 14-05 Meeting.md"), "{}", r.path);
}

#[test]
fn export_date_handles_negative_offsets_and_day_boundaries() {
    let mut f = fixture();
    // 2026-01-01T03:30Z at UTC-05:00 is 2025-12-31 22:30 local.
    f.meeting.started_unix_ms = 1_767_238_200_000;
    f.meeting.utc_offset_minutes = -300;
    let r = render(&input(&f), &opts()).unwrap();
    assert!(
        r.path.as_str().starts_with("Meetings/2025-12-31 22-30 "),
        "{}",
        r.path
    );
    // 2024-02-29 (leap day) 00:00 UTC.
    f.meeting.started_unix_ms = 1_709_164_800_000;
    f.meeting.utc_offset_minutes = 0;
    let r = render(&input(&f), &opts()).unwrap();
    assert!(
        r.path.as_str().starts_with("Meetings/2024-02-29 00-00 "),
        "{}",
        r.path
    );
}

#[test]
fn export_sanitise_file_name_uses_hyphens_and_never_em_dashes() {
    assert_eq!(
        sanitize_file_name("a/b\\c:d*e?f\"g<h>i|j"),
        "a-b-c-d-e-f-g-h-i-j"
    );
    assert_eq!(sanitize_file_name("x#y^z[w]"), "x-y-z-w-");
    assert_eq!(
        sanitize_file_name("one \u{2014} two\u{2013}three"),
        "one - two-three"
    );
    assert_eq!(
        sanitize_file_name("tabs\tand\nnewlines"),
        "tabs and newlines"
    );
    assert_eq!(sanitize_file_name("  spaced   out  "), "spaced out");
    assert_eq!(sanitize_file_name("trailing dots..."), "trailing dots");
    assert_eq!(sanitize_file_name("..."), "");
    let long = "\u{e9}".repeat(150);
    let s = sanitize_file_name(&long);
    assert_eq!(s.chars().count(), MAX_TITLE_CHARS);
    assert!(!sanitize_file_name("a\u{2014}b").contains('\u{2014}'));
}

#[test]
fn export_folder_must_be_a_vault_relative_path() {
    let f = fixture();
    for bad in ["../outside", "/abs", "C:/x", "a//b"] {
        let o = ExportOptions {
            folder: bad.into(),
            ..opts()
        };
        assert!(render(&input(&f), &o).is_err(), "{bad:?}");
    }
}

// ---- em dash ----

#[test]
fn export_replace_em_dash_spaces_it_as_a_hyphen() {
    assert_eq!(replace_em_dash("a\u{2014}b"), "a - b");
    assert_eq!(replace_em_dash("a \u{2014} b"), "a - b");
    assert_eq!(replace_em_dash("a  \u{2014}\u{2014}  b"), "a - b");
    assert_eq!(replace_em_dash("no dash"), "no dash");
    assert_eq!(
        replace_em_dash("en\u{2013}dash stays"),
        "en\u{2013}dash stays"
    );
}

#[test]
fn export_em_dashes_are_replaced_in_generated_text_by_default() {
    let f = fixture();
    let doc = render(&input(&f), &opts()).unwrap().to_file();
    let generated: String = doc
        .lines()
        .filter(|l| !l.starts_with("- `") && !l.starts_with("  "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!generated.contains('\u{2014}'), "{generated}");
    // The user's own notes are not generated text: kept as typed.
    assert!(doc.contains("check travel \u{2014} ask Ada"));
    let kept = render(
        &input(&f),
        &ExportOptions {
            replace_em_dash: false,
            ..opts()
        },
    )
    .unwrap()
    .to_file();
    assert!(kept.contains("over\u{2014}travel"));
}

// ---- frontmatter ----

#[test]
fn export_frontmatter_parses_as_yaml_with_the_plan_keys() {
    let f = fixture();
    let doc = render(&input(&f), &opts()).unwrap().to_file();
    let y = &YamlLoader::load_from_str(frontmatter(&doc)).unwrap()[0];
    assert_eq!(y["earshot_id"].as_str(), Some(id(1).to_string().as_str()));
    assert_eq!(y["date"].as_str(), Some("2026-10-17"));
    assert_eq!(y["start"].as_str(), Some("14:05"));
    assert_eq!(y["duration"].as_str(), Some("0:47:30"));
    assert_eq!(y["source"].as_str(), Some("earshot"));
    let att: Vec<&str> = y["attendees"]
        .as_vec()
        .unwrap()
        .iter()
        .filter_map(Yaml::as_str)
        .collect();
    assert_eq!(att, vec!["[[Ada Lovelace]]", "[[Me]]"]);
    let tags: Vec<&str> = y["tags"]
        .as_vec()
        .unwrap()
        .iter()
        .filter_map(Yaml::as_str)
        .collect();
    assert_eq!(tags, vec!["meeting", "planning", "Team-Sync"]);
}

#[test]
fn export_frontmatter_escapes_awkward_strings_and_handles_missing_values() {
    let mut f = fixture();
    f.meeting.duration_ms = None;
    f.attendees = vec!["O'Brien \"Bob\" [x]|y".into(), "  ".into()];
    let doc = render(&input(&f), &opts()).unwrap().to_file();
    let y = &YamlLoader::load_from_str(frontmatter(&doc)).unwrap()[0];
    assert!(y["duration"].is_null());
    let att: Vec<&str> = y["attendees"]
        .as_vec()
        .unwrap()
        .iter()
        .filter_map(Yaml::as_str)
        .collect();
    assert_eq!(
        att,
        vec!["[[O'Brien \"Bob\" x-y]]"],
        "link-breaking characters removed, blanks dropped"
    );
    f.attendees.clear();
    let doc = render(&input(&f), &opts()).unwrap().to_file();
    let y = &YamlLoader::load_from_str(frontmatter(&doc)).unwrap()[0];
    assert!(y["attendees"].as_vec().unwrap().is_empty());
}

#[test]
fn export_earshot_id_is_found_in_a_file() {
    let f = fixture();
    let doc = render(&input(&f), &opts()).unwrap().to_file();
    assert_eq!(earshot_id_of(&doc), Some(id(1)));
    assert_eq!(earshot_id_of(&doc.replace('\n', "\r\n")), Some(id(1)));
    let plain = format!("---\nfoo: 1\nearshot_id: {}\n---\nbody\n", id(3));
    assert_eq!(earshot_id_of(&plain), Some(id(3)));
    assert_eq!(earshot_id_of("no frontmatter"), None);
    assert_eq!(earshot_id_of("---\nearshot_id: nope\n---\n"), None);
    // Only the frontmatter counts, not a mention in the body.
    assert_eq!(
        earshot_id_of(&format!("---\na: 1\n---\nearshot_id: {}\n", id(4))),
        None
    );
}

// ---- merge ----

#[test]
fn export_merge_into_its_own_output_is_byte_identical() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    assert_eq!(merge(&doc, &r).unwrap(), doc);
    assert_eq!(merge(&merge(&doc, &r).unwrap(), &r).unwrap(), doc);
}

#[test]
fn export_merge_preserves_user_edits_outside_the_region() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    // The user adds a key, a tag, edits an owned key, writes above and below the region,
    // and edits inside it.
    let edited = doc
        .replace(
            "source: earshot\n",
            "source: earshot\nproject: \"[[Budget 2027]]\"\nrating: 5\n",
        )
        .replace("  - planning\n", "  - planning\n  - mine\n")
        .replace("date: 2026-10-17\n", "date: 1999-01-01\n")
        .replace(BEGIN_MARKER, &format!("My intro.\n\n{BEGIN_MARKER}"))
        .replace("## Decisions", "## Decisions edited inside");
    let edited = format!("{edited}\n## My follow-up\n- call Ada\n");

    let mut f2 = fixture();
    f2.summary.tldr = "New summary.".into();
    let r2 = render(&input(&f2), &opts()).unwrap();
    let merged = merge(&edited, &r2).unwrap();

    assert!(
        merged.contains("project: \"[[Budget 2027]]\"\nrating: 5\n"),
        "user keys kept in place"
    );
    assert!(merged.contains("date: 2026-10-17\n"), "owned key restored");
    assert!(!merged.contains("1999"));
    assert!(
        merged.contains("My intro.\n\n<!-- earshot:begin -->"),
        "text above kept"
    );
    assert!(
        merged.ends_with("<!-- earshot:end -->\n\n## My follow-up\n- call Ada\n"),
        "text below kept"
    );
    assert!(merged.contains("New summary."), "region replaced");
    assert!(
        !merged.contains("edited inside"),
        "edits inside the region are replaced"
    );
    let y = &YamlLoader::load_from_str(frontmatter(&merged)).unwrap()[0];
    let tags: Vec<&str> = y["tags"]
        .as_vec()
        .unwrap()
        .iter()
        .filter_map(Yaml::as_str)
        .collect();
    assert_eq!(
        tags,
        vec!["meeting", "planning", "mine", "Team-Sync"],
        "tags are a union"
    );
    // Re-merging the same render is a fixed point.
    assert_eq!(merge(&merged, &r2).unwrap(), merged);
}

#[test]
fn export_merge_reads_inline_and_scalar_tag_lists() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    for (tags, want) in [
        (
            "tags: [mine, \"#other\", meeting]\n",
            vec!["mine", "other", "meeting", "planning", "Team-Sync"],
        ),
        (
            "tags: solo\n",
            vec!["solo", "meeting", "planning", "Team-Sync"],
        ),
        (
            "tags:\n- a\n- b\n",
            vec!["a", "b", "meeting", "planning", "Team-Sync"],
        ),
    ] {
        let doc = format!("---\nearshot_id: \"{}\"\n{tags}---\nuser text\n", id(1));
        let merged = merge(&doc, &r).unwrap();
        let y = &YamlLoader::load_from_str(frontmatter(&merged)).unwrap()[0];
        let got: Vec<&str> = y["tags"]
            .as_vec()
            .unwrap()
            .iter()
            .filter_map(Yaml::as_str)
            .collect();
        assert_eq!(got, want, "{tags:?}");
    }
}

#[test]
fn export_merge_appends_the_region_when_the_markers_were_removed() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = "---\nmine: 1\n---\nAll mine.\n";
    let merged = merge(doc, &r).unwrap();
    assert!(merged.contains("mine: 1\n"));
    assert!(merged.contains("All mine.\n\n<!-- earshot:begin -->"));
    assert!(merged.ends_with("<!-- earshot:end -->\n"));
    assert_eq!(
        merge(&merged, &r).unwrap(),
        merged,
        "fixed point after the append"
    );
    // No frontmatter at all: one is added in front.
    let merged = merge("Just text.\n", &r).unwrap();
    assert!(merged.starts_with("---\nearshot_id: "));
    assert!(merged.contains("---\nJust text.\n\n<!-- earshot:begin -->"));
}

#[test]
fn export_merge_refuses_ambiguous_markers() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    for body in [
        format!("{END_MARKER}\nx\n{BEGIN_MARKER}\n"),
        format!("{BEGIN_MARKER}\nx\n"),
        format!("x\n{END_MARKER}\n"),
        format!("{BEGIN_MARKER}\n{BEGIN_MARKER}\n{END_MARKER}\n"),
        format!("{BEGIN_MARKER}\n{END_MARKER}\n{BEGIN_MARKER}\n{END_MARKER}\n"),
    ] {
        let doc = format!("---\na: 1\n---\n{body}");
        assert!(merge(&doc, &r).is_err(), "{body:?}");
    }
}

#[test]
fn export_merge_keeps_crlf_line_endings() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file().replace('\n', "\r\n") + "user\r\n";
    let merged = merge(&doc, &r).unwrap();
    assert_eq!(merged, doc);
}

// ---- T14.27: ticked action items survive a re-export ----

const ADA_TASK: &str = "Send revised travel numbers (due: Friday) (Ada Lovelace)";
const ME_TASK: &str = "Update the forecast (Me)";

#[test]
fn export_merge_keeps_ticked_action_items_whose_text_is_unchanged() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    // The user ticks one item and marks the other with a plugin's "in progress" marker.
    let ticked = doc
        .replace(&format!("- [ ] {ADA_TASK}"), &format!("- [x] {ADA_TASK}"))
        .replace(&format!("- [ ] {ME_TASK}"), &format!("- [/] {ME_TASK}"));
    assert_ne!(ticked, doc);
    // Re-exporting the same render keeps both marks: still a fixed point.
    assert_eq!(merge(&ticked, &r).unwrap(), ticked);

    // A new summary with one item reworded: the unchanged item keeps its mark, the
    // reworded one comes back unticked, the rest of the region is replaced.
    let mut f2 = fixture();
    f2.summary.tldr = "New summary.".into();
    f2.summary.action_items[1].task = "Update the forecast by Monday".into();
    let r2 = render(&input(&f2), &opts()).unwrap();
    let merged = merge(&ticked, &r2).unwrap();
    assert!(merged.contains(&format!("- [x] {ADA_TASK}\n")), "{merged}");
    assert!(
        merged.contains("- [ ] Update the forecast by Monday (Me)\n"),
        "{merged}"
    );
    assert!(!merged.contains("- [/]"), "{merged}");
    assert!(merged.contains("New summary."));
    // Apart from the carried mark the merge is the plain merge of the new render.
    let plain = merge(&doc, &r2).unwrap();
    assert_eq!(
        merged,
        plain.replace(&format!("- [ ] {ADA_TASK}"), &format!("- [x] {ADA_TASK}"))
    );
}

#[test]
fn export_merge_keeps_the_users_own_marker_character_and_crlf() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    for mark in ['X', '-', '>', '!', '\u{2713}'] {
        let doc = r.to_file().replace(
            &format!("- [ ] {ME_TASK}"),
            &format!("- [{mark}] {ME_TASK}"),
        );
        assert_eq!(merge(&doc, &r).unwrap(), doc, "{mark:?}");
        let crlf = doc.replace('\n', "\r\n");
        assert_eq!(merge(&crlf, &r).unwrap(), crlf, "{mark:?} CRLF");
    }
}

#[test]
fn export_merge_carries_marks_of_repeated_task_lines_in_order() {
    let mut f = fixture();
    // Two items with identical lines (other citations, so not one item, T14.61), then a
    // third with other text.
    let item = f.summary.action_items[1].clone();
    let mut again = item.clone();
    again.source_segment_ids = vec![id(20)];
    f.summary.action_items = vec![item, again, f.summary.action_items[0].clone()];
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    let first = doc.find(&format!("- [ ] {ME_TASK}")).unwrap();
    let second = doc[first + 1..].find(&format!("- [ ] {ME_TASK}")).unwrap() + first + 1;
    // Tick only the second copy.
    let mut ticked = doc.clone();
    ticked.replace_range(second..second + 5, "- [x]");
    assert_eq!(merge(&ticked, &r).unwrap(), ticked);
    // A new render that keeps one copy only takes the first copy's mark (unticked).
    let mut f2 = fixture();
    f2.summary.action_items = f.summary.action_items[1..].to_vec();
    let r2 = render(&input(&f2), &opts()).unwrap();
    let merged = merge(&ticked, &r2).unwrap();
    assert_eq!(
        merged.matches(&format!("- [ ] {ME_TASK}")).count(),
        1,
        "{merged}"
    );
    assert_eq!(merged.matches("- [x]").count(), 0, "{merged}");
}

#[test]
fn export_merge_keeps_a_tick_on_a_line_written_before_the_escaping_rule() {
    let mut f = fixture();
    f.summary.action_items[1].task = "Check the <b>bold</b> ![chart] text".into();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    let escaped = "Check the &lt;b>bold&lt;/b> !&#91;chart] text (Me)";
    assert!(doc.contains(&format!("- [ ] {escaped}\n")), "{doc}");
    // As a version from before T14.28 wrote it, raw, and the user ticked it.
    let old = doc.replace(
        &format!("- [ ] {escaped}"),
        "- [x] **Me**: Check the <b>bold</b> ![chart] text",
    );
    let merged = merge(&old, &r).unwrap();
    assert!(merged.contains(&format!("- [x] {escaped}\n")), "{merged}");
    // Already escaped text is not escaped again: the ticked new form is a fixed point.
    assert_eq!(merge(&merged, &r).unwrap(), merged);
    // The control: other text still does not match.
    let other = doc.replace(
        &format!("- [ ] {escaped}"),
        "- [x] **Me**: Check the <i>bold</i>",
    );
    assert!(!merge(&other, &r).unwrap().contains("- [x]"));
}

#[test]
fn export_merge_into_its_own_output_stays_the_identity_when_generated_text_holds_a_mark() {
    let mut f = fixture();
    // Two summary bullets render as `- [x] foo` then `- [ ] foo`.
    f.summary.enhanced_notes[0].bullets[0].text = "[x] Check travel".into();
    f.summary.enhanced_notes[0].bullets[1].text = "[ ] Check travel".into();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    assert!(
        doc.contains("- [x] Check travel\n- [ ] Check travel\n"),
        "{doc}"
    );
    assert_eq!(merge(&doc, &r).unwrap(), doc);
}

#[test]
fn export_merge_keeps_a_tick_after_a_plugin_appended_to_the_line() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    // A task plugin ticked the item and added a done date after its text.
    let done = doc.replace(
        &format!("- [ ] {ME_TASK}"),
        &format!("- [x] {ME_TASK} \u{2705} 2026-11-20"),
    );
    let merged = merge(&done, &r).unwrap();
    assert!(merged.contains(&format!("- [x] {ME_TASK}\n")), "{merged}");
    assert!(
        !merged.contains("2026-11-20"),
        "the appended text is not carried"
    );
    // An exact copy still comes first: the exact pass pairs before the appended one.
    assert_eq!(merge(&merged, &r).unwrap(), merged);
}

#[test]
fn export_merge_does_not_carry_a_tick_to_a_longer_item_sharing_its_start() {
    let mut f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let ticked = r
        .to_file()
        .replace(&format!("- [ ] {ME_TASK}"), &format!("- [x] {ME_TASK}"));
    // The new summary words the item longer: a different item, unticked.
    f.summary.action_items[1].task = "Update the forecast numbers".into();
    let r2 = render(&input(&f), &opts()).unwrap();
    let merged = merge(&ticked, &r2).unwrap();
    assert!(
        merged.contains("- [ ] Update the forecast numbers (Me)\n"),
        "{merged}"
    );
    assert!(!merged.contains("- [x]"), "{merged}");
    // A prefix that ends mid-word does not match either.
    let partial = r
        .to_file()
        .replace(&format!("- [ ] {ME_TASK}"), &format!("- [x] {ME_TASK}s"));
    assert!(!merge(&partial, &r).unwrap().contains("- [x]"));
}

#[test]
fn export_merge_keeps_a_tick_written_under_the_wider_escaping_rule() {
    let mut f = fixture();
    f.summary.action_items[1].task = "Keep costs <5% over".into();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    let line = doc
        .lines()
        .find(|l| l.starts_with("- [ ] Keep costs"))
        .unwrap()
        .to_owned();
    // The note as T14.28's first rule wrote it (every `<` escaped), then ticked.
    let wider = doc.replace(&line, &line.replace("- [ ]", "- [x]").replace('<', "&lt;"));
    let merged = merge(&wider, &r).unwrap();
    assert!(merged.contains(&line.replace("- [ ]", "- [x]")), "{merged}");
}

#[test]
fn export_merge_never_carries_a_mark_from_outside_the_region() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    // The user copied the item above the region and ticked that copy.
    let doc = r
        .to_file()
        .replace(BEGIN_MARKER, &format!("- [x] {ME_TASK}\n\n{BEGIN_MARKER}"));
    let merged = merge(&doc, &r).unwrap();
    assert_eq!(merged, doc);
    assert_eq!(merged.matches(&format!("- [ ] {ME_TASK}")).count(), 1);
}

// ---- T14.61: the note follows the meeting page ----

/// A stand-in for the runtime's `edit::person_name_key` (core has no copy of it): white
/// space collapsed, lower case.
fn test_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn casey() -> ExportUser<'static> {
    ExportUser {
        name: "Casey Lane",
        key: test_key,
    }
}

fn item(owner: &str, task: &str, due: Option<&str>, seg: u8) -> ActionItem {
    ActionItem {
        owner: owner.into(),
        task: task.into(),
        due: due.map(Into::into),
        source_segment_ids: vec![id(seg)],
    }
}

/// Invented action items: the user's own, someone else's, one the user shares, the same
/// task with other citations, and one two other people share.
fn grouped_items() -> Vec<ActionItem> {
    vec![
        item("Casey Lane", "Book the room", None, 20),
        item("Ada Lovelace", "Send numbers", Some("Friday"), 21),
        item(" casey  LANE ", "Draft the plan", None, 22),
        item("Ada Lovelace", "Draft the plan", None, 22),
        item("Bo Tran", "Draft the plan", None, 20),
        item("Bo Tran", "Check rates", None, 21),
        item("Ada Lovelace", "Check rates", None, 21),
    ]
}

/// The "## Action items" section of a rendered region.
fn actions_section(region: &str) -> &str {
    let start = region.find("## Action items\n").expect("action items");
    let rest = &region[start..];
    &rest[..rest.find("\n## ").unwrap_or(rest.len())]
}

#[test]
fn export_golden_with_the_users_own_actions() {
    // Copied by hand to `acceptance/export/my_actions.md`, whose JSON is this input.
    let mut f = fixture();
    f.speakers[0].display_name = "Casey Lane".into();
    f.attendees = vec!["Ada Lovelace".into(), "Casey Lane".into()];
    f.summary.action_items = vec![
        item("Casey Lane", "Update the forecast", None, 22),
        item(
            "Ada Lovelace",
            "Send revised travel numbers",
            Some("Friday"),
            21,
        ),
        item("Ada Lovelace", "Update the forecast", None, 22),
    ];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    assert_snapshot("my_actions", &r.to_file());
}

#[test]
fn export_region_follows_the_meeting_page_order() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let at = |h: &str| {
        r.region
            .find(h)
            .unwrap_or_else(|| panic!("{h} missing: {}", r.region))
    };
    let order = [
        "## TL;DR",
        "## Decisions",
        "## Action items",
        "## Notes",
        "### Budget",
        "## Open questions",
        "## My notes",
        "> [!note]- Transcript",
    ];
    for w in order.windows(2) {
        assert!(
            at(w[0]) < at(w[1]),
            "{} before {}: {}",
            w[0],
            w[1],
            r.region
        );
    }
}

#[test]
fn export_actions_are_grouped_into_the_users_and_other_peoples() {
    let mut f = fixture();
    f.summary.action_items = grouped_items();
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    assert_eq!(
        actions_section(&r.region),
        "## Action items\n\n\
         ### My actions\n\n\
         - [ ] Book the room\n\
         - [ ] Draft the plan (with Ada Lovelace)\n\n\
         ### Other actions\n\n\
         - [ ] Draft the plan (Bo Tran)\n\
         - [ ] Send numbers (due: Friday) (Ada Lovelace)\n\
         - [ ] Check rates (Bo Tran, Ada Lovelace)\n",
        "{}",
        r.region
    );
    let doc = r.to_file();
    assert_eq!(merge(&doc, &r).unwrap(), doc, "still a fixed point");
}

#[test]
fn export_an_empty_action_group_is_left_out() {
    let mut f = fixture();
    f.summary.action_items = vec![item("Ada Lovelace", "Send numbers", None, 21)];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    assert_eq!(
        actions_section(&r.region),
        "## Action items\n\n### Other actions\n\n- [ ] Send numbers (Ada Lovelace)\n"
    );
    f.summary.action_items = vec![item("casey lane", "Book the room", Some("Monday"), 20)];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    assert_eq!(
        actions_section(&r.region),
        "## Action items\n\n### My actions\n\n- [ ] Book the room (due: Monday)\n"
    );
}

#[test]
fn export_actions_without_a_user_name_are_one_list_with_owners() {
    let mut f = fixture();
    f.summary.action_items = grouped_items();
    let blank = |name| ExportUser {
        name,
        key: test_key,
    };
    for user in [None, Some(blank("")), Some(blank("  "))] {
        let mut i = input(&f);
        i.user = user;
        let r = render(&i, &opts()).unwrap();
        assert_eq!(
            actions_section(&r.region),
            "## Action items\n\n\
             - [ ] Book the room (Casey Lane)\n\
             - [ ] Draft the plan (Bo Tran)\n\
             - [ ] Send numbers (due: Friday) (Ada Lovelace)\n\
             - [ ] Check rates (Bo Tran, Ada Lovelace)\n\
             - [ ] Draft the plan (casey  LANE, Ada Lovelace)\n",
            "{user:?}"
        );
    }
}

#[test]
fn export_the_user_owns_by_the_key_they_were_given() {
    let u = casey();
    assert!(u.owns("Casey Lane"));
    assert!(u.owns("  casey\tLANE "));
    assert!(!u.owns("Casey"));
    // The key is the caller's: one that folds nothing matches only the exact name.
    let exact = ExportUser {
        name: "Casey Lane",
        key: |s| s.to_owned(),
    };
    assert!(exact.owns("Casey Lane"));
    assert!(!exact.owns("casey lane"));
    // A name the key makes blank owns nothing, not even a blank owner.
    let blank = ExportUser {
        name: " ",
        key: test_key,
    };
    assert!(!blank.owns(""));
    assert!(!blank.owns(" "));
}

#[test]
fn export_action_order_and_merge_key_follow_the_meeting_page() {
    let mut f = fixture();
    f.summary.action_items = vec![
        item("Ada Lovelace", "Cites nothing", None, 0),
        item("Ada Lovelace", "Cites the third line", None, 22),
        item("Ada Lovelace", "Cites the second line", Some("Friday"), 21),
        item("Bo Tran", "Cites the second line", Some("Monday"), 21),
        item("Bo Tran", "Cites the second line", Some("Friday"), 21),
        item(
            "Ada Lovelace",
            "Cites a line not in the transcript",
            None,
            99,
        ),
        item("Bo Tran", "Cites the first and third lines", None, 22),
    ];
    f.summary.action_items[0].source_segment_ids.clear();
    f.summary.action_items[6].source_segment_ids.push(id(20));
    let r = render(&input(&f), &opts()).unwrap();
    // By earliest cited line, stable; another due date is another item; items citing no
    // line of the transcript last, in summary order.
    assert_eq!(
        actions_section(&r.region),
        "## Action items\n\n\
         - [ ] Cites the first and third lines (Bo Tran)\n\
         - [ ] Cites the second line (due: Friday) (Ada Lovelace, Bo Tran)\n\
         - [ ] Cites the second line (due: Monday) (Bo Tran)\n\
         - [ ] Cites the third line (Ada Lovelace)\n\
         - [ ] Cites nothing (Ada Lovelace)\n\
         - [ ] Cites a line not in the transcript (Ada Lovelace)\n",
        "{}",
        r.region
    );
}

/// The action items as a version before T14.61 rendered [`grouped_items`]: one
/// "**Owner**: task" line per item, in summary order.
const OLD_ACTIONS: &str = "## Action items\n\n\
     - [ ] **Casey Lane**: Book the room\n\
     - [ ] **Ada Lovelace**: Send numbers (due: Friday)\n\
     - [ ] ** casey  LANE **: Draft the plan\n\
     - [ ] **Ada Lovelace**: Draft the plan\n\
     - [ ] **Bo Tran**: Draft the plan\n\
     - [ ] **Bo Tran**: Check rates\n\
     - [ ] **Ada Lovelace**: Check rates\n";

/// A note exported before T14.61 (old order and action format), with `ticks` (lines of
/// [`OLD_ACTIONS`] without their checkbox) ticked.
fn old_note(new_doc: &str, new_region: &str, ticks: &[&str]) -> String {
    let mut old = OLD_ACTIONS.to_owned();
    for t in ticks {
        let line = format!("- [ ] {t}\n");
        assert!(old.contains(&line), "{t}");
        old = old.replace(&line, &format!("- [x] {t}\n"));
    }
    new_doc.replace(actions_section(new_region), &old)
}

#[test]
fn export_merge_keeps_ticks_from_a_note_written_before_the_action_groups() {
    let mut f = fixture();
    f.summary.action_items = grouped_items();
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    let doc = r.to_file();
    // The user ticked their own item, Ada's half of the shared one and Bo's half of the
    // one Bo and Ada share.
    let old = old_note(
        &doc,
        &r.region,
        &[
            "**Casey Lane**: Book the room",
            "**Ada Lovelace**: Draft the plan",
            "**Bo Tran**: Check rates",
        ],
    );
    assert!(old.contains("**Bo Tran**: Check rates"), "{old}");
    let merged = merge(&old, &r).unwrap();
    let section = actions_section(&merged);
    assert_eq!(
        section,
        "## Action items\n\n\
         ### My actions\n\n\
         - [x] Book the room\n\
         - [x] Draft the plan (with Ada Lovelace)\n\n\
         ### Other actions\n\n\
         - [ ] Draft the plan (Bo Tran)\n\
         - [ ] Send numbers (due: Friday) (Ada Lovelace)\n\
         - [x] Check rates (Bo Tran, Ada Lovelace)\n",
        "{merged}"
    );
    // Apart from the marks the merge is the plain new render, and it is a fixed point.
    assert_eq!(
        merged.replace("- [x]", "- [ ]"),
        merge(&doc, &r).unwrap(),
        "only marks carried"
    );
    assert_eq!(merge(&merged, &r).unwrap(), merged);

    // The user's own half of a shared item counts too: ticked from the user's line alone.
    let old = old_note(&doc, &r.region, &["** casey  LANE **: Draft the plan"]);
    let merged = merge(&old, &r).unwrap();
    assert!(
        merged.contains("- [x] Draft the plan (with Ada Lovelace)\n"),
        "{merged}"
    );
    assert_eq!(merged.matches("- [x]").count(), 1, "{merged}");

    // Re-exported with no user name set: one list, the same ticks.
    let old = old_note(
        &doc,
        &r.region,
        &[
            "**Casey Lane**: Book the room",
            "**Ada Lovelace**: Draft the plan",
        ],
    );
    let r2 = render(&input(&f), &opts()).unwrap();
    let merged = merge(&old, &r2).unwrap();
    assert!(
        merged.contains("- [x] Book the room (Casey Lane)\n"),
        "{merged}"
    );
    assert!(
        merged.contains("- [x] Draft the plan (casey  LANE, Ada Lovelace)\n"),
        "{merged}"
    );
    assert_eq!(merged.matches("- [x]").count(), 2, "{merged}");
}

#[test]
fn export_merge_does_not_carry_a_tick_to_another_persons_item_across_the_format_change() {
    let mut f = fixture();
    f.summary.action_items = vec![item("Bo Tran", "Send numbers", None, 21)];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    let doc = r.to_file();
    // Before T14.61 the item was Ada's and the user ticked it; now it is Bo's.
    let old = doc.replace(
        actions_section(&r.region),
        "## Action items\n\n- [x] **Ada Lovelace**: Send numbers\n",
    );
    let merged = merge(&old, &r).unwrap();
    assert!(
        merged.contains("- [ ] Send numbers (Bo Tran)\n"),
        "{merged}"
    );
    assert!(!merged.contains("- [x]"), "{merged}");
    // The control: as Bo's line it carries.
    let merged = merge(&old.replace("Ada Lovelace", "Bo Tran"), &r).unwrap();
    assert!(
        merged.contains("- [x] Send numbers (Bo Tran)\n"),
        "{merged}"
    );
}

/// `doc` with its action items section replaced by `actions` (a hand-written old form).
fn with_old_actions(doc: &str, region: &str, actions: &str) -> String {
    doc.replace(actions_section(region), actions)
}

#[test]
fn export_merge_never_moves_another_persons_tick_onto_the_users_own_line() {
    for due in [None, Some("Friday")] {
        let mut f = fixture();
        f.summary.action_items = vec![item("Casey Lane", "Send numbers", due, 21)];
        let mut i = input(&f);
        i.user = Some(casey());
        let r = render(&i, &opts()).unwrap();
        let doc = r.to_file();
        let task = match due {
            Some(d) => format!("Send numbers (due: {d})"),
            None => "Send numbers".to_owned(),
        };
        assert!(
            doc.contains(&format!("### My actions\n\n- [ ] {task}\n")),
            "{doc}"
        );
        // Ada's item, ticked, in every form an older note can hold it: Other actions, the
        // one list and the form before T14.61.
        for old in [
            format!("## Action items\n\n### Other actions\n\n- [x] {task} (Ada Lovelace)\n"),
            format!("## Action items\n\n- [x] {task} (Ada Lovelace)\n"),
            format!("## Action items\n\n- [x] **Ada Lovelace**: {task}\n"),
            // The user's line shared with Ada is another owner set: not this action either.
            format!("## Action items\n\n### My actions\n\n- [x] {task} (with Ada Lovelace)\n"),
        ] {
            let merged = merge(&with_old_actions(&doc, &r.region, &old), &r).unwrap();
            assert!(!merged.contains("- [x]"), "{old:?}: {merged}");
        }
        // The control: the user's own line carries, in each form.
        for old in [
            format!("## Action items\n\n### My actions\n\n- [x] {task}\n"),
            format!("## Action items\n\n- [x] {task} (Casey Lane)\n"),
            format!("## Action items\n\n- [x] **Casey Lane**: {task}\n"),
        ] {
            let merged = merge(&with_old_actions(&doc, &r.region, &old), &r).unwrap();
            assert!(
                merged.contains(&format!("### My actions\n\n- [x] {task}\n")),
                "{old:?}: {merged}"
            );
        }
    }
}

#[test]
fn export_merge_reads_a_task_ending_in_its_own_bracket_against_the_render() {
    let mut f = fixture();
    f.summary.action_items = vec![
        item("Casey Lane", "Write spec (draft)", None, 20),
        item("Robin Vale", "Write spec", None, 21),
    ];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    let doc = r.to_file();
    let mine = "### My actions\n\n- [x] Write spec (draft)\n";
    let robin = "### Other actions\n\n- [x] Write spec (Robin Vale)\n";
    assert!(doc.contains(&mine.replace("[x]", "[ ]")), "{doc}");
    assert!(doc.contains(&robin.replace("[x]", "[ ]")), "{doc}");
    for (old, want) in [
        // Before T14.61.
        (
            "- [x] **Robin Vale**: Write spec\n- [ ] **Casey Lane**: Write spec (draft)\n",
            Some(robin),
        ),
        (
            "- [ ] **Robin Vale**: Write spec\n- [x] **Casey Lane**: Write spec (draft)\n",
            Some(mine),
        ),
        // One list, no user name then.
        (
            "- [x] Write spec (draft) (Casey Lane)\n- [ ] Write spec (Robin Vale)\n",
            Some(mine),
        ),
        // Ambiguous: an Other line that reads as Robin's task owned by "draft", or as an
        // ownerless task: it is neither action here, so the tick is lost, never moved.
        ("### Other actions\n\n- [x] Write spec (draft)\n", None),
    ] {
        let merged = merge(
            &with_old_actions(&doc, &r.region, &format!("## Action items\n\n{old}")),
            &r,
        )
        .unwrap();
        assert_eq!(
            merged.matches("- [x]").count(),
            usize::from(want.is_some()),
            "{old:?}: {merged}"
        );
        if let Some(want) = want {
            assert!(merged.contains(want), "{old:?}: {merged}");
        }
    }
}

#[test]
fn export_merge_carries_ticks_across_a_user_name_change_or_loses_them() {
    let casey_as = |name: &'static str| ExportUser {
        name,
        key: test_key,
    };
    let render_with = |owner: &str, user: Option<ExportUser<'static>>| {
        let mut f = fixture();
        f.summary.action_items = vec![
            item(owner, "Book the room", None, 20),
            item("Ada Lovelace", "Send numbers", None, 21),
        ];
        let mut i = input(&f);
        i.user = user;
        render(&i, &opts()).unwrap()
    };
    let tick = |doc: &str, line: &str| {
        assert!(doc.contains(&format!("- [ ] {line}\n")), "{line}: {doc}");
        doc.replace(&format!("- [ ] {line}\n"), &format!("- [x] {line}\n"))
    };

    // Set: a one-list note, then a name: the explicit owner maps to the user's line.
    let old = tick(
        &render_with("Casey Lane", None).to_file(),
        "Book the room (Casey Lane)",
    );
    let r = render_with("Casey Lane", Some(casey()));
    let merged = merge(&old, &r).unwrap();
    assert!(
        merged.contains("### My actions\n\n- [x] Book the room\n"),
        "{merged}"
    );
    assert_eq!(merged.matches("- [x]").count(), 1, "{merged}");

    // Renamed, and the summary names the user by the new name: still the user's line.
    let old = tick(
        &render_with("Casey", Some(casey_as("Casey"))).to_file(),
        "Book the room",
    );
    let r = render_with("Casey Lane", Some(casey()));
    let merged = merge(&old, &r).unwrap();
    assert!(
        merged.contains("### My actions\n\n- [x] Book the room\n"),
        "{merged}"
    );
    assert_eq!(merged.matches("- [x]").count(), 1, "{merged}");

    // Cleared: an old "My actions" line names the user only by its place, and the merge
    // does not know the old name, so the tick is lost; nothing is moved.
    let old = tick(
        &render_with("Casey Lane", Some(casey())).to_file(),
        "Book the room",
    );
    let r = render_with("Casey Lane", None);
    let merged = merge(&old, &r).unwrap();
    assert!(
        merged.contains("- [ ] Book the room (Casey Lane)\n"),
        "{merged}"
    );
    assert!(!merged.contains("- [x]"), "{merged}");

    // Renamed so the owner text no longer names the user: lost the same way.
    let r = render_with("Casey Lane", Some(casey_as("Robin Vale")));
    let merged = merge(&old, &r).unwrap();
    assert!(
        merged.contains("- [ ] Book the room (Casey Lane)\n"),
        "{merged}"
    );
    assert!(!merged.contains("- [x]"), "{merged}");
}

/// `doc` re-merged with its action items replaced by `old` (lines after "## Action items").
fn merged_from(r: &RenderedExport, old: &str) -> String {
    let doc = r.to_file();
    merge(
        &with_old_actions(&doc, &r.region, &format!("## Action items\n\n{old}")),
        r,
    )
    .unwrap()
}

fn render_items(items: Vec<ActionItem>, user: Option<ExportUser<'static>>) -> RenderedExport {
    let mut f = fixture();
    f.summary.action_items = items;
    let mut i = input(&f);
    i.user = user;
    render(&i, &opts()).unwrap()
}

#[test]
fn export_merge_appended_text_never_hides_an_owner_list() {
    // An ownerless item in the one list, and Ada's longer item, ticked.
    let r = render_items(vec![item("", "Send numbers", None, 21)], None);
    assert!(r.region.contains("- [ ] Send numbers\n"), "{}", r.region);
    let merged = merged_from(&r, "- [x] Send numbers tomorrow (Ada Lovelace)\n");
    assert!(!merged.contains("- [x]"), "{merged}");
    // The control: a plugin's date with no owner group still carries.
    let merged = merged_from(&r, "- [x] Send numbers \u{2705} 2026-11-20\n");
    assert!(merged.contains("- [x] Send numbers\n"), "{merged}");

    // The user's own "Book", and the user's longer item shared with Ada, ticked.
    let r = render_items(vec![item("Casey Lane", "Book", None, 20)], Some(casey()));
    let merged = merged_from(
        &r,
        "### My actions\n\n- [x] Book the room (with Ada Lovelace)\n",
    );
    assert!(!merged.contains("- [x]"), "{merged}");
    let merged = merged_from(&r, "### My actions\n\n- [x] Book \u{2705} 2026-11-20\n");
    assert!(merged.contains("- [x] Book\n"), "{merged}");

    // A Tasks plugin date after an owner group: another owner set, never moved.
    let r = render_items(
        vec![item("Casey Lane", "Send numbers", None, 21)],
        Some(casey()),
    );
    let merged = merged_from(
        &r,
        "### My actions\n\n- [x] Send numbers (with Ada Lovelace) \u{2705} 2026-11-20\n",
    );
    assert!(!merged.contains("- [x]"), "{merged}");
    // The control: the same owner set with the date after it carries.
    let r = render_items(
        vec![item("Ada Lovelace", "Send numbers", None, 21)],
        Some(casey()),
    );
    let merged = merged_from(
        &r,
        "### Other actions\n\n- [x] Send numbers (Ada Lovelace) \u{2705} 2026-11-20\n",
    );
    assert!(
        merged.contains("- [x] Send numbers (Ada Lovelace)\n"),
        "{merged}"
    );
}

#[test]
fn export_merge_takes_old_owner_lines_only_when_together_they_name_every_owner() {
    let r = render_items(
        vec![
            item("Casey Lane", "Draft the plan", None, 22),
            item("Ada Lovelace", "Draft the plan", None, 22),
        ],
        Some(casey()),
    );
    assert!(
        r.region
            .contains("- [ ] Draft the plan (with Ada Lovelace)\n"),
        "{}",
        r.region
    );
    // Ada's line alone names {Ada}, not {Ada, the user}.
    let merged = merged_from(&r, "- [x] **Ada Lovelace**: Draft the plan\n");
    assert!(!merged.contains("- [x]"), "{merged}");
    // The control: both owners' lines, one ticked.
    let merged = merged_from(
        &r,
        "- [x] **Ada Lovelace**: Draft the plan\n- [ ] **Casey Lane**: Draft the plan\n",
    );
    assert!(
        merged.contains("- [x] Draft the plan (with Ada Lovelace)\n"),
        "{merged}"
    );
}

#[test]
fn export_merge_reads_one_list_lines_as_one_list_only_without_subheadings() {
    let r = render_items(
        vec![
            item("Casey Lane", "Book the room", None, 20),
            item("Ada Lovelace", "Send numbers", None, 21),
        ],
        Some(casey()),
    );
    // "### My actions" deleted: its lines now sit above "### Other actions" and could be
    // anyone's, so nothing carries from them.
    for old in [
        "- [x] Book the room\n\n### Other actions\n\n- [ ] Send numbers (Ada Lovelace)\n",
        "- [x] Send numbers (Ada Lovelace)\n\n### Other actions\n\n- [ ] Send numbers (Ada Lovelace)\n",
    ] {
        let merged = merged_from(&r, old);
        assert!(!merged.contains("- [x]"), "{old:?}: {merged}");
    }
    // Both deleted: a one list again. An owner list carries; a line with none is not the
    // user's own.
    let merged = merged_from(
        &r,
        "- [x] Book the room\n- [x] Send numbers (Ada Lovelace)\n",
    );
    assert!(
        merged.contains("- [x] Send numbers (Ada Lovelace)\n"),
        "{merged}"
    );
    assert_eq!(merged.matches("- [x]").count(), 1, "{merged}");
}

#[test]
fn export_merge_carries_nothing_by_owners_when_the_action_list_disagrees() {
    let r = render_items(
        vec![
            item("Casey Lane", "Book the room", None, 20),
            item("Ada Lovelace", "Send numbers", None, 21),
        ],
        Some(casey()),
    );
    let old = "- [x] **Casey Lane**: Book the room\n- [ ] **Ada Lovelace**: Send numbers\n";
    // The control: with the render's own list the old form carries.
    assert!(merged_from(&r, old).contains("- [x] Book the room\n"));
    let mut short = r.clone();
    short.actions.pop();
    let mut changed = r.clone();
    changed.actions[0].text = "Book a room".into();
    let mut longer = r.clone();
    longer.actions.push(r.actions[0].clone());
    for (what, bad) in [("short", short), ("changed", changed), ("longer", longer)] {
        let merged = merged_from(&bad, old);
        assert!(!merged.contains("- [x]"), "{what}: {merged}");
    }
}

// ---- T14.28: generated text cannot embed or fetch ----

/// Text that would make Obsidian fetch or embed something, or hide text, if written raw.
const HOSTILE: [&str; 7] = [
    "![](https://example.invalid/a.png)",
    "![[Secret note]]",
    "<img src=\"https://example.invalid/b.png\">",
    "<iframe src=\"https://example.invalid/\"></iframe>",
    "<https://example.invalid/c>",
    "\\![x](https://example.invalid/d.png)",
    "<!-- hidden -->",
];

#[test]
fn export_generated_text_cannot_form_an_image_an_embed_or_html() {
    let mut f = fixture();
    let all = HOSTILE.join(" and ");
    f.summary.tldr = all.clone();
    f.summary.enhanced_notes[0].heading = all.clone();
    f.summary.enhanced_notes[0].bullets[0].text = all.clone();
    f.summary.decisions[0].text = all.clone();
    f.summary.action_items[0].owner = all.clone();
    f.summary.action_items[0].task = all.clone();
    f.summary.action_items[0].due = Some(all.clone());
    f.summary.open_questions[0] = all.clone();
    f.speakers[1].display_name = all.clone();
    f.segments[0].text = all.clone();
    f.notes.clear();
    let r = render(&input(&f), &opts()).unwrap();
    let region = r
        .region
        .strip_prefix(&format!("{BEGIN_MARKER}\n"))
        .and_then(|s| s.strip_suffix(&format!("{END_MARKER}\n")))
        .unwrap();
    assert!(!region.contains("!["), "{region}");
    assert!(!region.contains('<'), "{region}");
    // Ordinary text around the triggers is kept, the triggers written as entities that
    // Obsidian's reading view shows as the original characters.
    assert!(
        region.contains("!&#91;](https://example.invalid/a.png)"),
        "{region}"
    );
    assert!(region.contains("!&#91;[Secret note]]"), "{region}");
    assert!(region.contains("&lt;img src=\"https://example.invalid/b.png\">"));
    assert!(region.contains("&lt;!-- hidden -->"));
    // Every generated place got the escape: tldr, heading, bullet, decision, owner, task,
    // due, question, the speaker name on its one line and the transcript line.
    assert_eq!(region.matches("&lt;iframe").count(), 10, "{region}");
    // Still a fixed point.
    let doc = r.to_file();
    assert_eq!(merge(&doc, &r).unwrap(), doc);
}

#[test]
fn export_ordinary_prose_with_the_trigger_characters_still_reads_naturally() {
    let mut f = fixture();
    f.summary.tldr = "Costs fell! [Budget] is <5% over, a -> b, and 3 > 2.".into();
    let r = render(&input(&f), &opts()).unwrap();
    assert!(
        r.region
            .contains("\nCosts fell! [Budget] is <5% over, a -> b, and 3 > 2.\n"),
        "{}",
        r.region
    );
    // Single markers and short runs are prose too.
    f.summary.tldr = "Up 5% to $10, a ~ b, `x` and `` y ``.".into();
    let r = render(&input(&f), &opts()).unwrap();
    assert!(
        r.region
            .contains("\nUp 5% to $10, a ~ b, `x` and `` y ``.\n"),
        "{}",
        r.region
    );
}

/// Text that would hide the rest of the note if written raw at the start of a line or a
/// list item's text.
const HIDING: [&str; 6] = [
    "%% everything after this is a comment",
    "```",
    "~~~~ notes",
    "  ```rust",
    "$$ x",
    "a %%% b $$$ c",
];

#[test]
fn export_generated_text_cannot_hide_the_users_text_after_the_region() {
    for hiding in HIDING {
        let mut f = fixture();
        f.summary.tldr = hiding.into();
        f.summary.enhanced_notes[0].heading = hiding.into();
        f.summary.enhanced_notes[0].bullets[0].text = hiding.into();
        f.summary.decisions[0].text = hiding.into();
        f.summary.action_items[0].owner = hiding.into();
        f.summary.action_items[0].task = hiding.into();
        f.summary.action_items[0].due = Some(hiding.into());
        f.summary.open_questions[0] = hiding.into();
        f.speakers[1].display_name = hiding.into();
        f.segments[0].text = hiding.into();
        let r = render(&input(&f), &opts()).unwrap();
        let doc = format!("{}\nMy own words after the region.\n", r.to_file());
        let region = &r.region;
        assert!(!region.contains("%%"), "{hiding:?}: {region}");
        assert!(!region.contains("$$"), "{hiding:?}: {region}");
        // No line of the region, nor a list item's text, opens a fence.
        for line in region.lines() {
            let text = line
                .trim_start_matches(' ')
                .strip_prefix("- ")
                .unwrap_or(line)
                .trim_start_matches(' ');
            assert!(
                !text.starts_with("```") && !text.starts_with("~~~"),
                "{hiding:?}: {line:?}"
            );
        }
        assert!(doc.ends_with("<!-- earshot:end -->\n\nMy own words after the region.\n"));
        assert_eq!(merge(&doc, &r).unwrap(), doc, "{hiding:?}");
    }
    // Mid-line runs cannot open a fence and are kept.
    let mut f = fixture();
    f.summary.tldr = "use ``` to fence".into();
    let r = render(&input(&f), &opts()).unwrap();
    assert!(r.region.contains("\nuse ``` to fence\n"), "{}", r.region);
}

#[test]
fn export_the_generated_escapes_are_idempotent_and_undone_by_the_plain_form() {
    let samples = [
        HOSTILE.join(" "),
        HIDING.join(" "),
        "```x".to_owned(),
        "~~~".to_owned(),
        "<5% and <b> and &lt;i>".to_owned(),
    ];
    for s in samples {
        let once = text::neutralise_generated(&s);
        assert_eq!(text::neutralise_generated(&once), once, "{s:?}");
        assert_eq!(
            merge::plain_form(&once),
            merge::plain_form(&s),
            "{s:?} -> {once:?}"
        );
    }
}

#[test]
fn export_the_users_own_notes_are_written_as_typed() {
    let mut f = fixture();
    f.notes[1].text = "see ![chart](https://example.invalid/e.png) <b>now</b>".into();
    let doc = render(&input(&f), &opts()).unwrap().to_file();
    assert!(
        doc.contains("- `00:33:20` see ![chart](https://example.invalid/e.png) <b>now</b>\n"),
        "{doc}"
    );
}

#[test]
fn export_content_cannot_forge_region_markers() {
    let mut f = fixture();
    f.notes[1].text = format!("evil {END_MARKER} trick");
    f.summary.tldr = BEGIN_MARKER.to_owned();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = r.to_file();
    assert_eq!(doc.matches(BEGIN_MARKER).count(), 1);
    assert_eq!(doc.matches(END_MARKER).count(), 1);
    assert_eq!(merge(&doc, &r).unwrap(), doc);
}

// ---- T5.4a: BOM, mixed line endings, quoted tags, byte-length cap ----

#[test]
fn export_merge_keeps_a_utf8_bom() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    let doc = format!("\u{feff}{}", r.to_file());
    assert_eq!(earshot_id_of(&doc), Some(id(1)));
    assert_eq!(merge(&doc, &r).unwrap(), doc, "BOM re-emitted, fixed point");
    let mut f2 = fixture();
    f2.summary.tldr = "Changed.".into();
    let merged = merge(&doc, &render(&input(&f2), &opts()).unwrap()).unwrap();
    assert!(merged.starts_with("\u{feff}---\n"));
    assert_eq!(merged.matches('\u{feff}').count(), 1);
}

#[test]
fn export_merge_preserves_mixed_line_endings_outside_the_region() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    // An LF note where the user's additions (a key and a trailing paragraph) are CRLF.
    let doc = r
        .to_file()
        .replace("source: earshot\n", "source: earshot\nmine: yes\r\n")
        + "\r\nUser line one\r\nUser line two\r\n";
    assert_eq!(
        merge(&doc, &r).unwrap(),
        doc,
        "fixed point keeps every line ending"
    );
    let mut f2 = fixture();
    f2.summary.tldr = "Changed.".into();
    let r2 = render(&input(&f2), &opts()).unwrap();
    let merged = merge(&doc, &r2).unwrap();
    assert!(merged.contains("mine: yes\r\n"));
    assert!(merged.ends_with("<!-- earshot:end -->\n\r\nUser line one\r\nUser line two\r\n"));
    assert!(
        merged.contains("Changed.\n"),
        "generated text uses the dominant LF"
    );
    // A mostly-CRLF note gets CRLF generated text.
    let crlf = r.to_file().replace('\n', "\r\n");
    let merged = merge(&crlf, &r2).unwrap();
    assert!(merged.contains("Changed.\r\n"));
    assert!(
        !merged.replace("\r\n", "").contains('\n'),
        "no bare LF introduced"
    );
}

#[test]
fn export_merge_reads_quoted_inline_tags() {
    let f = fixture();
    let r = render(&input(&f), &opts()).unwrap();
    for (tags, want) in [
        (
            "tags: [\"a b\", 'c,d', e]\n",
            vec!["a-b", "c-d", "e", "meeting", "planning", "Team-Sync"],
        ),
        (
            "tags: \"x y\" z\n",
            vec!["x-y", "z", "meeting", "planning", "Team-Sync"],
        ),
    ] {
        let doc = format!("---\nearshot_id: \"{}\"\n{tags}---\nuser text\n", id(1));
        let merged = merge(&doc, &r).unwrap();
        let y = &YamlLoader::load_from_str(frontmatter(&merged)).unwrap()[0];
        let got: Vec<&str> = y["tags"]
            .as_vec()
            .unwrap()
            .iter()
            .filter_map(Yaml::as_str)
            .collect();
        assert_eq!(got, want, "{tags:?}");
    }
}

#[test]
fn export_file_name_fits_a_255_byte_file_system_limit() {
    let long = "\u{3042}".repeat(150); // 3 bytes each
    let s = sanitize_file_name(&long);
    assert!(s.len() <= MAX_TITLE_BYTES, "{} bytes", s.len());
    assert!(
        s.chars().all(|c| c == '\u{3042}'),
        "cut on a character boundary"
    );
    let emoji = "\u{1f600}".repeat(80); // 4 bytes each
    assert!(sanitize_file_name(&emoji).len() <= MAX_TITLE_BYTES);
    let mut f = fixture();
    f.meeting.title = long;
    let r = render(&input(&f), &opts()).unwrap();
    // Room for the date prefix, ".md" and a " (n)" collision suffix.
    assert!(r.path.file_name().len() + " (9999)".len() <= 255);
}
