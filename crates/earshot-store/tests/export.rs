//! Export writer: atomic writes, re-export merge, find-by-id, vault validation, crash
//! recovery (`cargo test -p earshot-store export`). Every vault is a temp dir.

mod common;

use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_core::export::{ExportInput, ExportOptions, RenderedExport, render};
use earshot_core::identity::{DeviceId, Hlc, ManualClock};
use earshot_core::model::{ExportRepo, Meeting, MeetingRepo, Stamp};
use earshot_core::prompt::Template;
use earshot_core::summary::SummaryV1;
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
        let summary = SummaryV1 {
            title: "Weekly".into(),
            tldr: tldr.into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
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

#[test]
fn export_re_export_preserves_user_edits_outside_the_region() {
    let mut w = World::new("edits");
    let r = w.render("Old summary.");
    w.export(&r).unwrap();
    let path = w.file(r.path.as_str());
    let edited = std::fs::read_to_string(&path)
        .unwrap()
        .replace("source: earshot\n", "source: earshot\nstatus: reviewed\n")
        + "\nMy own follow-up.\n";
    std::fs::write(&path, &edited).unwrap();

    let r2 = w.render("New summary.");
    let rep = w.export(&r2).unwrap();
    assert_eq!(rep.action, ExportAction::Updated);
    let now = std::fs::read_to_string(&path).unwrap();
    assert!(now.contains("status: reviewed\n"));
    assert!(now.ends_with("\nMy own follow-up.\n"));
    assert!(now.contains("New summary.") && !now.contains("Old summary."));
    // And once more: byte-identical.
    assert_eq!(w.export(&r2).unwrap().action, ExportAction::Unchanged);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), now);
}

#[test]
fn export_finds_a_renamed_and_moved_note_by_earshot_id() {
    let mut w = World::new("moved");
    let r = w.render("One.");
    w.export(&r).unwrap();
    std::fs::create_dir_all(w.vault().join("Archive").join("2026")).unwrap();
    std::fs::rename(
        w.file(r.path.as_str()),
        w.vault()
            .join("Archive")
            .join("2026")
            .join("Renamed by me.md"),
    )
    .unwrap();

    let rep = w.export(&w.render("Two.")).unwrap();
    assert_eq!(rep.action, ExportAction::Updated);
    assert_eq!(rep.path.as_str(), "Archive/2026/Renamed by me.md");
    assert_eq!(
        files(&w.vault()),
        vec!["Archive/2026/Renamed by me.md"],
        "no duplicate"
    );
    assert!(
        std::fs::read_to_string(w.file(rep.path.as_str()))
            .unwrap()
            .contains("Two.")
    );
    let rec = w
        .store
        .export_record(w.meeting.id, DEVICE)
        .unwrap()
        .unwrap();
    assert_eq!(rec.path, rep.path);
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

    let rep = w.export(&w.render("After.")).unwrap();
    assert_eq!(rep.path, r.path, "the orphan is re-used");
    assert_eq!(
        files(&w.vault()),
        vec![r.path.as_str().to_owned()],
        "no second copy"
    );
    let text = std::fs::read_to_string(w.file(r.path.as_str())).unwrap();
    assert!(text.contains("After.") && text.ends_with("User text after the crash.\n"));
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
fn export_recovers_an_orphan_that_was_also_moved() {
    // No record (database lost) and the note moved: the whole vault is searched (T5.4b).
    let mut w = World::new("crash-moved");
    let r = w.render("x");
    std::fs::create_dir_all(w.vault().join("Elsewhere")).unwrap();
    std::fs::write(w.vault().join("Elsewhere").join("note.md"), r.to_file()).unwrap();
    let rep = w.export(&r).unwrap();
    assert_eq!(rep.action, ExportAction::Unchanged);
    assert_eq!(rep.path.as_str(), "Elsewhere/note.md");
    assert_eq!(files(&w.vault()), vec!["Elsewhere/note.md"]);
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
fn export_refuses_ambiguous_markers_without_touching_the_note() {
    let mut w = World::new("markers");
    let r = w.render("x");
    w.export(&r).unwrap();
    let path = w.file(r.path.as_str());
    let broken = std::fs::read_to_string(&path)
        .unwrap()
        .replace("<!-- earshot:end -->\n", "");
    std::fs::write(&path, &broken).unwrap();
    let err = w.export(&w.render("y")).unwrap_err();
    assert!(matches!(err, StoreError::Export(_)), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    assert_eq!(files(&w.vault()), vec![r.path.as_str().to_owned()]);
}

#[test]
fn export_search_skips_hidden_folders() {
    let mut w = World::new("hidden");
    let r = w.render("x");
    // A copy in Obsidian's trash must not be resurrected.
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
fn export_keeps_crlf_notes_crlf() {
    let mut w = World::new("crlf");
    let r = w.render("x");
    w.export(&r).unwrap();
    let path = w.file(r.path.as_str());
    let crlf = std::fs::read_to_string(&path)
        .unwrap()
        .replace('\n', "\r\n");
    std::fs::write(&path, &crlf).unwrap();
    assert_eq!(w.export(&r).unwrap().action, ExportAction::Unchanged);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), crlf);
}
