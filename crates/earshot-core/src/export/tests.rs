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
    };
    let me = Speaker {
        id: id(10),
        meta: meta(),
        meeting_id: m.id,
        role: SpeakerRole::Me,
        display_name: "Me".into(),
        person_id: None,
    };
    let ada = Speaker {
        id: id(11),
        meta: meta(),
        meeting_id: m.id,
        role: SpeakerRole::Remote,
        display_name: "Ada Lovelace".into(),
        person_id: Some(id(50)),
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
