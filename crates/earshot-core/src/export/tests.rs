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
    // Quoted keys, single quotes and a user's keys around it; the first id line counts.
    let quoted = format!("---\n\"earshot_id\": '{}'\nmine: 2\n---\n", id(5));
    assert_eq!(earshot_id_of(&quoted), Some(id(5)));
    let twice = format!("---\nearshot_id: {}\nearshot_id: {}\n---\n", id(6), id(7));
    assert_eq!(earshot_id_of(&twice), Some(id(6)));
    // A list item or an indented line is not a top-level key.
    let nested = format!(
        "---\nparent:\n  earshot_id: {}\n- earshot_id: {}\n---\n",
        id(8),
        id(8)
    );
    assert_eq!(earshot_id_of(&nested), None);
    // An unclosed frontmatter holds no id.
    assert_eq!(
        earshot_id_of(&format!("---\nearshot_id: {}\n", id(9))),
        None
    );
}

#[test]
fn export_earshot_id_is_found_behind_a_utf8_bom() {
    let f = fixture();
    let doc = format!("\u{feff}{}", render(&input(&f), &opts()).unwrap().to_file());
    assert_eq!(earshot_id_of(&doc), Some(id(1)));
}

#[test]
fn export_two_renders_in_a_row_are_byte_identical() {
    // A re-export writes the whole render (T14.63), so the same meeting must render the same
    // bytes every time: the store then finds nothing to write (`exp.idempotent`).
    let mut f = fixture();
    f.summary.action_items = grouped_items();
    let mut i = input(&f);
    i.user = Some(casey());
    let a = render(&i, &opts()).unwrap();
    let b = render(&i, &opts()).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.to_file(), b.to_file());
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
fn export_golden_with_only_the_users_actions() {
    // Copied by hand to `acceptance/export/all_mine.md`, whose JSON is this input.
    let mut f = fixture();
    f.speakers[0].display_name = "Casey Lane".into();
    f.attendees = vec!["Ada Lovelace".into(), "Casey Lane".into()];
    f.summary.action_items = vec![
        item("Casey Lane", "Update the forecast", None, 22),
        item("Casey Lane", "Book the room", Some("Friday"), 21),
    ];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    assert_snapshot("all_mine", &r.to_file());
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
}

#[test]
fn export_an_empty_action_group_says_none() {
    let mut f = fixture();
    f.summary.action_items = vec![item("Ada Lovelace", "Send numbers", None, 21)];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    assert_eq!(
        actions_section(&r.region),
        "## Action items\n\n### My actions\n\n_None_\n\n\
         ### Other actions\n\n- [ ] Send numbers (Ada Lovelace)\n"
    );
    f.summary.action_items = vec![item("casey lane", "Book the room", Some("Monday"), 20)];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    assert_eq!(
        actions_section(&r.region),
        "## Action items\n\n### My actions\n\n- [ ] Book the room (due: Monday)\n\n\
         ### Other actions\n\n_None_\n"
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
fn export_action_order_and_line_key_follow_the_meeting_page() {
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

#[test]
fn export_note_sections_named_like_action_groups_hold_no_action_lines() {
    let mut f = fixture();
    f.summary.action_items = vec![
        item("Casey Lane", "Book the room", None, 20),
        item("Ada Lovelace", "Send numbers", None, 21),
    ];
    f.summary.enhanced_notes = vec![
        NoteSection {
            heading: "Action items".into(),
            bullets: vec![NoteBullet {
                text: "[x] Book the room".into(),
                source_segment_ids: vec![],
            }],
        },
        NoteSection {
            heading: "My actions".into(),
            bullets: vec![NoteBullet {
                text: "[x] Book the room".into(),
                source_segment_ids: vec![],
            }],
        },
    ];
    let mut i = input(&f);
    i.user = Some(casey());
    let r = render(&i, &opts()).unwrap();
    let doc = r.to_file();
    assert!(
        doc.contains("### Action items\n\n- [x] Book the room\n"),
        "{doc}"
    );
    assert!(
        doc.contains("### My actions\n\n- [x] Book the room\n\n## Open questions"),
        "{doc}"
    );
    // The note bullets keep their own marks and give none to the user's action.
    assert!(doc.contains("## Action items\n\n### My actions\n\n- [ ] Book the room\n"));
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
fn export_generated_text_cannot_hide_the_rest_of_the_note() {
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
        assert!(
            r.to_file().ends_with("<!-- earshot:end -->\n"),
            "{hiding:?}"
        );
    }
    // Mid-line runs cannot open a fence and are kept.
    let mut f = fixture();
    f.summary.tldr = "use ``` to fence".into();
    let r = render(&input(&f), &opts()).unwrap();
    assert!(r.region.contains("\nuse ``` to fence\n"), "{}", r.region);
}

#[test]
fn export_the_generated_escapes_are_idempotent() {
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
}

// ---- T5.4a: byte-length cap ----

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
