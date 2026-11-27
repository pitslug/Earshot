//! Export writer: atomic writes, re-export replaces the note, find-by-id, vault validation, crash
//! recovery (`cargo test -p earshot-store export`). Every vault is a temp dir.

mod common;

use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_core::export::{ExportInput, ExportOptions, RenderedExport, render};
use earshot_core::identity::{DeviceId, Hlc, ManualClock};
use earshot_core::model::{ExportRepo, Meeting, MeetingRepo, Stamp};
use earshot_core::prompt::Template;
use earshot_core::summary::{ActionItem, SummaryV1};
use earshot_store::{ExportAction, Store, StoreError, VaultError, export_note, validate_vault};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

const DEVICE: DeviceId = DeviceId::from_bytes([0xde; 16]);

struct World {
    tmp: TempDir,
    store: Store,
    clock: ManualClock,
    rng: ChaCha8Rng,
    meeting: Meeting,
}

impl World {
    fn new(tag: &str) -> Self {
        let tmp = TempDir::new(tag);
        std::fs::create_dir_all(tmp.path().join("vault")).unwrap();
        let clock = ManualClock::new(1_792_209_900_000);
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let mut hlc = Hlc::new(DEVICE);
        let mut meeting = Meeting::new(
            "Weekly sync",
            1_792_209_900_000,
            600,
            Template::General,
            &mut Stamp {
                clock: &clock,
                hlc: &mut hlc,
                rng: &mut rng,
            },
        );
        meeting.duration_ms = Some(600_000);
        let mut store = Store::open_in_memory().unwrap();
        store.put_meeting(meeting.clone()).unwrap();
        World {
            tmp,
            store,
            clock,
            rng,
            meeting,
        }
    }

    fn vault(&self) -> PathBuf {
        self.tmp.path().join("vault")
    }

    fn render(&self, tldr: &str) -> RenderedExport {
        self.render_with(tldr, vec![])
    }

    fn render_with(&self, tldr: &str, action_items: Vec<ActionItem>) -> RenderedExport {
        let summary = SummaryV1 {
            title: "Weekly".into(),
            tldr: tldr.into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items,
            open_questions: vec![],
            topics: vec![],
        };
        render(
            &ExportInput {
                transcript: None,
                meeting: &self.meeting,
                attendees: &["Ada".to_owned()],
                speakers: &[],
                segments: &[],
                notes: &[],
                summary: Some(&summary),
                unknown_speaker_lines: &[],
                user: None,
            },
            &ExportOptions::default(),
        )
        .unwrap()
    }

    fn export(&mut self, r: &RenderedExport) -> Result<earshot_store::ExportReport, StoreError> {
        self.clock.advance_ms(1000);
        let vault = self.vault();
        export_note(
            &mut self.store,
            &vault,
            DEVICE,
            self.meeting.id,
            r,
            &self.clock,
            &mut self.rng,
        )
    }

    fn file(&self, rel: &str) -> PathBuf {
        let mut p = self.vault();
        p.extend(rel.split('/'));
        p
    }
}

/// Every file under `dir` (relative, `/`-separated, sorted).
fn files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn export_validate_vault_requires_an_existing_writable_directory() {
    let tmp = TempDir::new("vault-check");
    assert!(matches!(
        validate_vault(&tmp.path().join("missing")),
        Err(VaultError::Missing(_))
    ));
    let file = tmp.path().join("file.md");
    std::fs::write(&file, "x").unwrap();
    assert!(matches!(
        validate_vault(&file),
        Err(VaultError::NotADirectory(_))
    ));
    validate_vault(tmp.path()).unwrap();
    assert_eq!(
        files(tmp.path()),
        vec!["file.md"],
        "the write probe is removed"
    );
}

#[test]
fn export_refuses_an_invalid_vault() {
    let mut w = World::new("bad-vault");
    let r = w.render("x");
    std::fs::remove_dir_all(w.vault()).unwrap();
    assert!(matches!(w.export(&r), Err(StoreError::Vault(_))));
    assert_eq!(w.store.export_record(w.meeting.id, DEVICE).unwrap(), None);
}

#[test]
fn export_creates_the_note_and_records_it() {
    let mut w = World::new("create");
    let r = w.render("First.");
    let rep = w.export(&r).unwrap();
    assert_eq!(rep.action, ExportAction::Created);
    assert_eq!(rep.path, r.path);
    assert_eq!(
        std::fs::read_to_string(w.file(r.path.as_str())).unwrap(),
        r.to_file()
    );
    assert_eq!(
        files(&w.vault()),
        vec![r.path.as_str().to_owned()],
        "no temp file left"
    );
    let rec = w
        .store
        .export_record(w.meeting.id, DEVICE)
        .unwrap()
        .unwrap();
    assert_eq!(rec.path, r.path);
    assert_eq!(rec.exported_unix_ms, rep.exported_unix_ms);
}

#[test]
fn export_re_export_is_byte_identical() {
    let mut w = World::new("idempotent");
    let r = w.render("Same.");
    w.export(&r).unwrap();
    let first = std::fs::read(w.file(r.path.as_str())).unwrap();
    let id_before = w
        .store
        .export_record(w.meeting.id, DEVICE)
        .unwrap()
        .unwrap()
        .id;
    let rep = w.export(&r).unwrap();
    assert_eq!(rep.action, ExportAction::Unchanged);
    assert_eq!(std::fs::read(w.file(r.path.as_str())).unwrap(), first);
    let rec = w
        .store
        .export_record(w.meeting.id, DEVICE)
        .unwrap()
        .unwrap();
    assert_eq!(rec.id, id_before, "the record keeps its id");
}

/// One invented action item, rendered as `- [ ] Book the room (Robin Vale)`.
fn booking() -> Vec<ActionItem> {
    vec![ActionItem {
        owner: "Robin Vale".into(),
        task: "Book the room".into(),
        due: None,
        source_segment_ids: vec![],
    }]
}

const BOOKING_LINE: &str = "- [ ] Book the room (Robin Vale)\n";

/// The note at `path` as a user left it: the action ticked, a frontmatter key, a line inside
/// the Earshot part and a line after it added.
fn edit_like_a_user(path: &Path) -> String {
    let doc = std::fs::read_to_string(path).unwrap();
    assert!(doc.contains(BOOKING_LINE), "{doc}");
    let edited = doc
        .replace(BOOKING_LINE, "- [x] Book the room (Robin Vale)\n")
        .replace("source: earshot\n", "source: earshot\nstatus: reviewed\n")
        .replace("## TL;DR\n", "## TL;DR\n\nMy line inside.\n")
        + "\nMy own follow-up.\n";
    std::fs::write(path, &edited).unwrap();
    edited
}

#[test]
fn export_re_export_replaces_the_whole_note() {
    // T14.63: export is export. A re-export writes the render and nothing else: the tick,
    // the user's key and the user's lines, inside or after the Earshot part, are gone.
    let mut w = World::new("replace");
    let r = w.render_with("Old summary.", booking());
    w.export(&r).unwrap();
    let path = w.file(r.path.as_str());
    let edited = edit_like_a_user(&path);

    // The same render: still replaced (the note differs from it), so the tick goes too.
    let rep = w.export(&r).unwrap();
    assert_eq!(rep.action, ExportAction::Updated);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), r.to_file());

    // A new render over a user's edits: the whole file is the new render.
    std::fs::write(&path, &edited).unwrap();
    let r2 = w.render_with("New summary.", booking());
    let rep = w.export(&r2).unwrap();
    assert_eq!(rep.action, ExportAction::Updated);
    let now = std::fs::read_to_string(&path).unwrap();
    assert_eq!(now, r2.to_file());
    assert!(now.contains(BOOKING_LINE), "the tick is overwritten: {now}");
    for gone in [
        "- [x]",
        "status: reviewed",
        "My line inside.",
        "My own follow-up.",
        "Old summary.",
    ] {
        assert!(!now.contains(gone), "{gone:?} survived: {now}");
    }
    // And once more: byte-identical, nothing written.
    assert_eq!(w.export(&r2).unwrap().action, ExportAction::Unchanged);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), now);
    assert_eq!(files(&w.vault()), vec![r.path.as_str().to_owned()]);
}

#[test]
fn export_a_moved_note_is_left_alone_and_written_again_at_the_recorded_path() {
    // T14.63: the vault is never searched. A note the user renamed or moved is theirs; a
    // re-export writes the meeting's note again at the path its record names.
    let mut w = World::new("moved");
    let r = w.render_with("One.", booking());
    w.export(&r).unwrap();
    let moved_text = edit_like_a_user(&w.file(r.path.as_str()));
    std::fs::create_dir_all(w.vault().join("Archive").join("2026")).unwrap();
    let moved = w
        .vault()
        .join("Archive")
        .join("2026")
        .join("Renamed by me.md");
    std::fs::rename(w.file(r.path.as_str()), &moved).unwrap();

    let r2 = w.render_with("Two.", booking());
    let rep = w.export(&r2).unwrap();
    assert_eq!(rep.action, ExportAction::Created);
    assert_eq!(rep.path, r.path, "the recorded path");
    assert_eq!(
        std::fs::read_to_string(w.file(rep.path.as_str())).unwrap(),
        r2.to_file()
    );
    // The moved note is untouched: same bytes, still there.
    assert_eq!(std::fs::read_to_string(&moved).unwrap(), moved_text);
    assert_eq!(
        files(&w.vault()),
        vec![
            "Archive/2026/Renamed by me.md".to_owned(),
            r.path.as_str().to_owned()
        ]
    );
    let rec = w
        .store
        .export_record(w.meeting.id, DEVICE)
        .unwrap()
        .unwrap();
    assert_eq!(rec.path, r.path);
    // Deleted by the user: written again at the same path, too.
    std::fs::remove_file(w.file(r.path.as_str())).unwrap();
    let rep = w.export(&r2).unwrap();
    assert_eq!((rep.action, &rep.path), (ExportAction::Created, &r.path));
}

#[test]
fn export_recovers_from_a_crash_between_rename_and_record_insert() {
    // The note was renamed into place but the process died before the ExportRecord insert:
    // the vault has the file, the store has no record.
    let mut w = World::new("crash");
    let r = w.render("Before the crash.");
    std::fs::create_dir_all(w.file(r.path.as_str()).parent().unwrap()).unwrap();
    std::fs::write(
        w.file(r.path.as_str()),
        r.to_file() + "\nUser text after the crash.\n",
    )
    .unwrap();
    assert_eq!(w.store.export_record(w.meeting.id, DEVICE).unwrap(), None);

    let after = w.render("After.");
    let rep = w.export(&after).unwrap();
    assert_eq!(rep.path, r.path, "the orphan is re-used");
    assert_eq!(
        files(&w.vault()),
        vec![r.path.as_str().to_owned()],
        "no second copy"
    );
    // Replaced whole (T14.63), the user's text included.
    let text = std::fs::read_to_string(w.file(r.path.as_str())).unwrap();
    assert_eq!(text, after.to_file());
    assert_eq!(
        w.store
            .export_record(w.meeting.id, DEVICE)
            .unwrap()
            .unwrap()
            .path,
        r.path
    );
}

#[test]
fn export_without_a_record_leaves_a_moved_note_alone() {
    // No record (database lost) and the meeting's note moved by the user: nothing searches
    // for it (T14.63); a new note is written at the render's path.
    let mut w = World::new("crash-moved");
    let r = w.render("x");
    std::fs::create_dir_all(w.vault().join("Elsewhere")).unwrap();
    std::fs::write(w.vault().join("Elsewhere").join("note.md"), r.to_file()).unwrap();
    let rep = w.export(&r).unwrap();
    assert_eq!((rep.action, &rep.path), (ExportAction::Created, &r.path));
    assert_eq!(
        files(&w.vault()),
        vec!["Elsewhere/note.md".to_owned(), r.path.as_str().to_owned()]
    );
}

#[test]
fn export_without_a_record_takes_the_next_free_name_beside_another_meetings_note() {
    // Another meeting's note sits at the render's path (same title and start minute): it is
    // never overwritten; this meeting's note takes the next free name, and the record names
    // the path used, so the next export writes there.
    let mut w = World::new("collision");
    let r = w.render("Ours.");
    let theirs = r.to_file().replace(
        &w.meeting.id.to_string(),
        "01920000-0000-7000-8000-00000000abcd",
    );
    assert_ne!(theirs, r.to_file());
    std::fs::create_dir_all(w.file(r.path.as_str()).parent().unwrap()).unwrap();
    std::fs::write(w.file(r.path.as_str()), &theirs).unwrap();
    let rep = w.export(&r).unwrap();
    assert_eq!(rep.action, ExportAction::Created);
    assert!(rep.path.as_str().ends_with(" (2).md"), "{}", rep.path);
    assert_eq!(
        std::fs::read_to_string(w.file(r.path.as_str())).unwrap(),
        theirs
    );
    assert_eq!(
        std::fs::read_to_string(w.file(rep.path.as_str())).unwrap(),
        r.to_file()
    );
    assert_eq!(
        w.store
            .export_record(w.meeting.id, DEVICE)
            .unwrap()
            .unwrap()
            .path,
        rep.path
    );
    // Again: the recorded "(2)" note, theirs still untouched.
    let rep2 = w.export(&w.render("Ours again.")).unwrap();
    assert_eq!(
        (rep2.action, &rep2.path),
        (ExportAction::Updated, &rep.path)
    );
    assert_eq!(
        std::fs::read_to_string(w.file(r.path.as_str())).unwrap(),
        theirs
    );
}

#[test]
fn export_requires_the_meeting_in_the_store() {
    let mut w = World::new("no-meeting");
    let r = w.render("x");
    let vault = w.vault();
    let ghost = earshot_core::identity::EntityId::from_bytes([9; 16]);
    let err = export_note(
        &mut w.store,
        &vault,
        DEVICE,
        ghost,
        &r,
        &w.clock,
        &mut w.rng,
    )
    .unwrap_err();
    assert!(matches!(err, StoreError::Repo(_)), "{err}");
    assert!(files(&vault).is_empty(), "nothing written");
}

#[test]
fn export_never_overwrites_someone_elses_note_at_the_target_path() {
    let mut w = World::new("foreign");
    let r = w.render("x");
    let target = w.file(r.path.as_str());
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "---\ntitle: mine\n---\nNot an Earshot note.\n").unwrap();
    let rep = w.export(&r).unwrap();
    assert_eq!(rep.action, ExportAction::Created);
    assert!(rep.path.as_str().ends_with(" (2).md"), "{}", rep.path);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "---\ntitle: mine\n---\nNot an Earshot note.\n"
    );
    assert_eq!(
        std::fs::read_to_string(w.file(rep.path.as_str())).unwrap(),
        r.to_file()
    );
}

#[test]
fn export_replaces_a_note_whose_markers_were_removed() {
    // The region markers no longer mean anything to a re-export (T14.63): a note with one
    // deleted is replaced like any other note of the meeting.
    let mut w = World::new("markers");
    let r = w.render("x");
    w.export(&r).unwrap();
    let path = w.file(r.path.as_str());
    let broken = std::fs::read_to_string(&path)
        .unwrap()
        .replace("<!-- earshot:end -->\n", "");
    std::fs::write(&path, &broken).unwrap();
    let r2 = w.render("y");
    assert_eq!(w.export(&r2).unwrap().action, ExportAction::Updated);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), r2.to_file());
    assert_eq!(files(&w.vault()), vec![r.path.as_str().to_owned()]);
}

#[test]
fn export_leaves_a_copy_in_a_hidden_folder_alone() {
    let mut w = World::new("hidden");
    let r = w.render("x");
    // A copy in Obsidian's trash is never read or resurrected.
    std::fs::create_dir_all(w.vault().join(".trash")).unwrap();
    std::fs::write(w.vault().join(".trash").join("old.md"), r.to_file()).unwrap();
    let rep = w.export(&r).unwrap();
    assert_eq!(rep.action, ExportAction::Created);
    assert_eq!(rep.path, r.path);
    assert_eq!(
        std::fs::read_to_string(w.vault().join(".trash").join("old.md")).unwrap(),
        r.to_file()
    );
}

#[test]
fn export_replaces_a_crlf_note_with_the_render() {
    // The note is written as rendered (LF), whatever line endings a sync client gave it.
    let mut w = World::new("crlf");
    let r = w.render("x");
    w.export(&r).unwrap();
    let path = w.file(r.path.as_str());
    let crlf = std::fs::read_to_string(&path)
        .unwrap()
        .replace('\n', "\r\n");
    std::fs::write(&path, &crlf).unwrap();
    assert_eq!(w.export(&r).unwrap().action, ExportAction::Updated);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), r.to_file());
    assert_eq!(w.export(&r).unwrap().action, ExportAction::Unchanged);
}
