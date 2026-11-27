//! On-demand export tests (temp vault only, never a real one). Every name contains
//! `export`.

use earshot_core::export::ExportOptions;
use earshot_core::model::{ExportRepo, MeetingRepo, Note, Stamp};
use earshot_store::ExportAction;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{TempDir, meeting, services};

fn vault(dir: &TempDir) -> std::path::PathBuf {
    let v = dir.0.join("vault");
    std::fs::create_dir_all(v.join(".obsidian")).unwrap();
    v
}

fn notes_in(vault: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(vault.join("Meetings"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".md"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[test]
fn export_meeting_writes_one_note_with_transcript_and_records_it() {
    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let vault = vault(&dir);
    let id = meeting(&svc, "Friday release", 1);
    // Nothing is exported until asked.
    assert!(notes_in(&vault).is_empty());
    let report = export_meeting(
        &svc,
        &vault,
        id,
        &ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(2),
    )
    .unwrap();
    assert_eq!(report.action, ExportAction::Created);
    let files = notes_in(&vault);
    assert_eq!(files.len(), 1);
    let text = std::fs::read_to_string(vault.join("Meetings").join(&files[0])).unwrap();
    assert!(text.contains("yes friday works for the release"), "{text}");
    assert!(text.contains("Alice"));
    assert!(text.contains("check the release notes"));
    let db = svc.store();
    let device = db.device();
    assert!(db.export_record(id, device).unwrap().is_some());
    // Unknown meetings are an error, not a silent no-op.
    drop(db);
    assert!(matches!(
        export_meeting(
            &svc,
            &vault,
            earshot_core::identity::EntityId::from_bytes([9; 16]),
            &ExportOptions::default(),
            &mut ChaCha8Rng::seed_from_u64(3),
        ),
        Err(RuntimeError::Internal(_))
    ));
}

#[test]
fn export_all_then_changed_exports_only_new_or_edited_meetings() {
    let dir = TempDir::new("export");
    let (svc, clock) = services(&dir);
    let vault = vault(&dir);
    let a = meeting(&svc, "Alpha", 4);
    let b = meeting(&svc, "Beta", 5);
    let opts = ExportOptions::default();
    let mut rng = ChaCha8Rng::seed_from_u64(6);

    clock.advance_ms(1_000);
    let all = export_meetings(&svc, &vault, ExportSelection::All, &opts, &mut rng).unwrap();
    let ids: Vec<_> = all.iter().map(|(m, _)| *m).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&a) && ids.contains(&b));
    assert!(
        all.iter()
            .all(|(_, r)| matches!(r, Ok(rep) if rep.action == ExportAction::Created))
    );
    assert_eq!(notes_in(&vault).len(), 2);

    // Nothing changed since: nothing to export.
    clock.advance_ms(1_000);
    let none = export_meetings(&svc, &vault, ExportSelection::Changed, &opts, &mut rng).unwrap();
    assert!(none.is_empty(), "{none:?}");

    // A note added to Beta later: only Beta is exported again.
    clock.advance_ms(1_000);
    {
        let svc_clock = std::sync::Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*svc_clock,
            hlc,
            rng: &mut rng,
        };
        let note = Note::new(
            b,
            earshot_core::asr::MeetingTime::from_millis(30_000),
            "follow up with legal",
            &mut stamp,
        );
        store.put_note(note).unwrap();
    }
    clock.advance_ms(1_000);
    let changed = export_meetings(&svc, &vault, ExportSelection::Changed, &opts, &mut rng).unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].0, b);
    assert!(matches!(&changed[0].1, Ok(rep) if rep.action == ExportAction::Updated));
    let beta_file = notes_in(&vault)
        .into_iter()
        .find(|f| f.contains("Beta"))
        .expect("Beta note");
    let beta = std::fs::read_to_string(vault.join("Meetings").join(beta_file)).unwrap();
    assert!(beta.contains("follow up with legal"));

    // A deleted meeting is never exported.
    {
        let svc_clock = std::sync::Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut m = db.meeting(a).unwrap().unwrap();
        let (store, hlc) = db.parts();
        m.meta.mark_deleted(hlc, &*svc_clock);
        store.put_meeting(m).unwrap();
    }
    clock.advance_ms(1_000);
    let all = export_meetings(&svc, &vault, ExportSelection::All, &opts, &mut rng).unwrap();
    assert_eq!(all.iter().map(|(m, _)| *m).collect::<Vec<_>>(), [b]);
}

#[test]
fn export_into_an_unusable_vault_writes_nothing() {
    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let _ = meeting(&svc, "Gamma", 7);
    let missing = dir.0.join("no-such-vault");
    assert!(
        export_meetings(
            &svc,
            &missing,
            ExportSelection::All,
            &ExportOptions::default(),
            &mut ChaCha8Rng::seed_from_u64(8),
        )
        .is_err()
    );
    assert!(!missing.exists());
}

/// Exports "changed" and returns the meetings it wrote.
fn export_changed(
    svc: &Services,
    vault: &Path,
    seed: u64,
) -> Vec<earshot_core::identity::EntityId> {
    export_meetings(
        svc,
        vault,
        ExportSelection::Changed,
        &ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap()
    .into_iter()
    .map(|(m, r)| {
        r.unwrap();
        m
    })
    .collect()
}

#[test]
fn export_changed_sees_every_kind_of_edit_and_only_that_meeting() {
    // The clock never moves: edits in the same millisecond as the export still count.
    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let vault = vault(&dir);
    let a = meeting(&svc, "Alpha", 20);
    let b = meeting(&svc, "Beta", 21);
    assert_eq!(export_changed(&svc, &vault, 1).len(), 2);
    assert!(export_changed(&svc, &vault, 2).is_empty());

    let svc_clock = std::sync::Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(22);

    // Deleting a note (a tombstone only).
    {
        let mut db = svc.store();
        let mut note = db.notes(a).unwrap().remove(0);
        let (store, hlc) = db.parts();
        note.meta.mark_deleted(hlc, &*svc_clock);
        store.put_note(note).unwrap();
    }
    assert_eq!(export_changed(&svc, &vault, 3), [a]);

    // Renaming a speaker.
    {
        let mut db = svc.store();
        let mut s = db
            .speakers(b)
            .unwrap()
            .into_iter()
            .find(|s| s.display_name == "Alice")
            .unwrap();
        s.display_name = "Alice Smith".into();
        let (store, hlc) = db.parts();
        s.meta.touch(hlc, &*svc_clock);
        store.put_speaker(s).unwrap();
    }
    assert_eq!(export_changed(&svc, &vault, 4), [b]);
    let beta = notes_in(&vault)
        .into_iter()
        .find(|f| f.contains("Beta"))
        .unwrap();
    assert!(
        std::fs::read_to_string(vault.join("Meetings").join(beta))
            .unwrap()
            .contains("Alice Smith")
    );

    // Storing a summary.
    {
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*svc_clock,
            hlc,
            rng: &mut rng,
        };
        let summary = earshot_core::summary::SummaryV1 {
            title: "Alpha review".into(),
            tldr: "Reviewed alpha.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        };
        let row = earshot_core::model::Summary::new(
            a,
            earshot_core::prompt::Template::General,
            Some("m".into()),
            summary,
            &mut stamp,
        );
        store.put_summary(row).unwrap();
    }
    assert_eq!(export_changed(&svc, &vault, 5), [a]);

    // A row synced from a device whose clock is behind (older HLC than the export).
    {
        let mut other =
            earshot_core::identity::Hlc::new(earshot_core::identity::DeviceId::from_bytes([7; 16]));
        let old_clock = earshot_core::identity::ManualClock::new(1_000);
        let mut stamp = Stamp {
            clock: &old_clock,
            hlc: &mut other,
            rng: &mut rng,
        };
        let note = Note::new(
            b,
            earshot_core::asr::MeetingTime::from_millis(50_000),
            "added on the laptop",
            &mut stamp,
        );
        svc.store().put_note(note).unwrap();
    }
    assert_eq!(export_changed(&svc, &vault, 6), [b]);
    assert!(export_changed(&svc, &vault, 7).is_empty());
}

#[test]
fn export_all_leaves_the_store_free_during_its_vault_work() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let vault = vault(&dir);
    let a = meeting(&svc, "Alpha", 30);
    let b = meeting(&svc, "Beta", 31);
    // Never exported: each meeting's vault work creates its note.
    let key = "export-test/written-during-vault-work";
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let svc_ref = &svc;
    let (starts, written) = std::thread::scope(|s| {
        // Stands in for a live session writing to the store.
        s.spawn(move || {
            if go_rx.recv().is_ok() {
                svc_ref.store().set_local_value(key, b"1").unwrap();
                let _ = done_tx.send(());
            }
        });
        let mut starts = 0;
        let mut written = None;
        let mut on_vault = || {
            starts += 1;
            if starts == 1 {
                go_tx.send(()).unwrap();
                // Bounded rendezvous: the write finishes while the vault work waits, which it
                // cannot while the export holds the store lock.
                written = Some(done_rx.recv_timeout(Duration::from_secs(60)).is_ok());
            }
        };
        let out = export_meetings_observed(
            svc_ref,
            &vault,
            ExportSelection::All,
            &ExportOptions::default(),
            &mut ChaCha8Rng::seed_from_u64(32),
            &mut on_vault,
        )
        .unwrap();
        let ids: Vec<_> = out.iter().map(|(m, _)| *m).collect();
        assert!(ids.contains(&a) && ids.contains(&b), "{ids:?}");
        assert!(out.iter().all(|(_, r)| r.is_ok()), "{out:?}");
        drop(go_tx);
        (starts, written)
    });
    assert_eq!(starts, 2, "one vault step per meeting");
    assert_eq!(
        written,
        Some(true),
        "a store write waited for the vault work"
    );
    assert_eq!(
        svc.store().local_value(key).unwrap().as_deref(),
        Some(&b"1"[..])
    );
    assert_eq!(notes_in(&vault).len(), 2);
    // Fingerprints were recorded after the writes: nothing is changed now.
    assert!(export_changed(&svc, &vault, 33).is_empty());
}

// ---- T14.61: the note's action groups follow the meeting page ----

#[derive(serde::Deserialize)]
struct OwnerCases {
    cases: Vec<OwnerCase>,
}

#[derive(serde::Deserialize)]
struct OwnerCase {
    note: String,
    owner: String,
    /// `None`: no user name set.
    user: Option<String>,
    group: String,
    shown: Vec<String>,
}

/// `fixtures/ui/action_owner_cases.json`, which the meeting page's tests read too.
#[test]
fn export_action_owners_match_the_shared_cases() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/ui/action_owner_cases.json");
    let cases: OwnerCases = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(cases.cases.len() >= 6);
    for c in &cases.cases {
        let (group, shown) =
            earshot_core::export::owner_group(&c.owner, note_user(c.user.as_deref()));
        let group = match group {
            earshot_core::export::OwnerGroup::Mine => "mine",
            earshot_core::export::OwnerGroup::Other => "others",
            earshot_core::export::OwnerGroup::NoGroups => "single",
        };
        let want: Vec<&str> = c.shown.iter().map(String::as_str).collect();
        assert_eq!((group, shown), (c.group.as_str(), want), "{}", c.note);
    }
}

#[test]
fn export_note_user_is_none_for_the_default_or_a_blank_name() {
    assert!(note_user(None).is_none());
    assert!(note_user(Some("Me")).is_none());
    // The default in any case or spacing, as the meeting page compares it.
    assert!(note_user(Some("me")).is_none());
    assert!(note_user(Some(" ME ")).is_none());
    assert!(crate::edit::is_set_user_name("Mei"));
    assert!(!crate::edit::is_set_user_name("mE"));
    assert!(note_user(Some("  ")).is_none());
    assert_eq!(
        note_user(Some(" Casey Lane ")).map(|u| u.name),
        Some("Casey Lane")
    );
}

#[test]
fn export_note_groups_actions_once_the_user_has_a_name() {
    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let vault = vault(&dir);
    let id = meeting(&svc, "Planning", 30);
    let svc_clock = std::sync::Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(31);
    let item = |owner: &str, task: &str| earshot_core::summary::ActionItem {
        owner: owner.into(),
        task: task.into(),
        due: None,
        source_segment_ids: vec![],
    };
    {
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*svc_clock,
            hlc,
            rng: &mut rng,
        };
        let summary = earshot_core::summary::SummaryV1 {
            title: "Planning".into(),
            tldr: "Planned.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![
                item("Zoë Brontë and casey lane", "Book the room"),
                item("Robin Vale", "Send the slides"),
            ],
            open_questions: vec![],
            topics: vec![],
        };
        let row = earshot_core::model::Summary::new(
            id,
            earshot_core::prompt::Template::General,
            Some("m".into()),
            summary,
            &mut stamp,
        );
        store.put_summary(row).unwrap();
    }
    let note = |seed| {
        export_meeting(
            &svc,
            &vault,
            id,
            &ExportOptions::default(),
            &mut ChaCha8Rng::seed_from_u64(seed),
        )
        .unwrap();
        let file = notes_in(&vault).remove(0);
        std::fs::read_to_string(vault.join("Meetings").join(file)).unwrap()
    };
    // No name yet: one list, owners at the end.
    let before = note(1);
    assert!(
        before.contains(
            "## Action items\n\n- [ ] Book the room (Zoë Brontë, casey lane)\n\
             - [ ] Send the slides (Robin Vale)\n"
        ),
        "{before}"
    );
    // The user's name, matched as the meeting page matches it; the batch export agrees.
    crate::my_voice::set_my_name(&svc, "Casey Lane").unwrap();
    let after = note(2);
    let groups = "## Action items\n\n### My actions\n\n- [ ] Book the room (with Zoë Brontë)\n\n\
                  ### Other actions\n\n- [ ] Send the slides (Robin Vale)\n";
    assert!(after.contains(groups), "{after}");
    assert!(
        export_changed(&svc, &vault, 3).is_empty(),
        "the batch renders the same note"
    );
}

// ---- T14.63: a re-export replaces the note ----

/// Stores a summary of `id` with one invented action item, `- [ ] Send the slides (Robin
/// Vale)` in the note.
fn put_one_action(svc: &Services, id: EntityId, seed: u64) {
    let svc_clock = std::sync::Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*svc_clock,
        hlc,
        rng: &mut rng,
    };
    let summary = earshot_core::summary::SummaryV1 {
        title: "Planning".into(),
        tldr: "Planned.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![earshot_core::summary::ActionItem {
            owner: "Robin Vale".into(),
            task: "Send the slides".into(),
            due: None,
            source_segment_ids: vec![],
        }],
        open_questions: vec![],
        topics: vec![],
    };
    let row = earshot_core::model::Summary::new(
        id,
        earshot_core::prompt::Template::General,
        Some("m".into()),
        summary,
        &mut stamp,
    );
    store.put_summary(row).unwrap();
}

#[test]
fn export_again_replaces_a_ticked_line_and_the_users_own_lines() {
    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let vault = vault(&dir);
    let id = meeting(&svc, "Planning", 40);
    put_one_action(&svc, id, 41);
    assert!(!was_exported(&svc.store(), id).unwrap(), "never exported");
    let export = |seed| {
        export_meeting(
            &svc,
            &vault,
            id,
            &ExportOptions::default(),
            &mut ChaCha8Rng::seed_from_u64(seed),
        )
        .unwrap()
    };
    assert_eq!(export(42).action, ExportAction::Created);
    assert!(was_exported(&svc.store(), id).unwrap());
    let path = vault.join("Meetings").join(notes_in(&vault).remove(0));
    let first = std::fs::read_to_string(&path).unwrap();
    let line = "- [ ] Send the slides (Robin Vale)\n";
    assert!(first.contains(line), "{first}");
    // The user ticks the action and adds lines of their own, inside and after the body.
    let edited = first
        .replace(line, "- [x] Send the slides (Robin Vale)\n")
        .replace("## TL;DR\n", "## TL;DR\n\nMy line inside.\n")
        + "\nMy own follow-up.\n";
    assert!(edited.contains("My line inside."), "{edited}");
    std::fs::write(&path, &edited).unwrap();
    // Exporting again writes the note as rendered: the tick and the user's lines are gone.
    assert_eq!(export(43).action, ExportAction::Updated);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    assert_eq!(notes_in(&vault).len(), 1, "one note, no duplicate");
}

#[test]
fn export_was_exported_reads_only_this_devices_record() {
    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let vault = vault(&dir);
    let (a, b) = (meeting(&svc, "One", 50), meeting(&svc, "Two", 51));
    export_meeting(
        &svc,
        &vault,
        a,
        &ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(52),
    )
    .unwrap();
    let db = svc.store();
    assert!(was_exported(&db, a).unwrap());
    assert!(!was_exported(&db, b).unwrap());
    assert!(!was_exported(&db, EntityId::from_bytes([9; 16])).unwrap());
}

#[test]
fn export_reexport_copy_is_the_shared_table() {
    let table: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/ui/reexport_copy_cases.json"
    ))
    .unwrap();
    let text = |k: &str| table[k].as_str().unwrap().to_owned();
    assert_eq!(REEXPORT_WARNING, text("single"));
    assert_eq!(REEXPORT_CONFIRM, text("confirm"));
    assert_eq!(reexport_batch_warning(0), None);
    assert_eq!(reexport_batch_warning(1), Some(text("batch_one")));
    for n in [2, 7, 120] {
        assert_eq!(
            reexport_batch_warning(n),
            Some(text("batch_many").replace("{count}", &n.to_string())),
            "{n}"
        );
    }
    // Fixed copy: no em dash, no run of spaces.
    for k in ["single", "batch_one", "batch_many", "confirm", "cancel"] {
        assert!(
            !text(k).contains('\u{2014}') && !text(k).contains("  "),
            "{k}"
        );
    }
}
