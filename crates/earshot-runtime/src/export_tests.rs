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
fn export_all_leaves_the_store_free_during_its_vault_walk() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = TempDir::new("export");
    let (svc, _) = services(&dir);
    let vault = vault(&dir);
    let a = meeting(&svc, "Alpha", 30);
    let b = meeting(&svc, "Beta", 31);
    // Never exported: the batch builds its vault index, which is the walk.
    let key = "export-test/written-during-walk";
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let svc_ref = &svc;
    let (searches, written) = std::thread::scope(|s| {
        // Stands in for a live session writing to the store.
        s.spawn(move || {
            if go_rx.recv().is_ok() {
                svc_ref.store().set_local_value(key, b"1").unwrap();
                let _ = done_tx.send(());
            }
        });
        let mut searches = 0;
        let mut written = None;
        let mut on_search = || {
            searches += 1;
            if searches == 1 {
                go_tx.send(()).unwrap();
                // Bounded rendezvous: the write finishes while the walk waits, which it
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
            &mut on_search,
        )
        .unwrap();
        let ids: Vec<_> = out.iter().map(|(m, _)| *m).collect();
        assert!(ids.contains(&a) && ids.contains(&b), "{ids:?}");
        assert!(out.iter().all(|(_, r)| r.is_ok()), "{out:?}");
        drop(go_tx);
        (searches, written)
    });
    assert!(searches >= 1, "the batch never searched the vault");
    assert_eq!(
        written,
        Some(true),
        "a store write waited for the vault walk"
    );
    assert_eq!(
        svc.store().local_value(key).unwrap().as_deref(),
        Some(&b"1"[..])
    );
    assert_eq!(notes_in(&vault).len(), 2);
    // Fingerprints were recorded after the writes: nothing is changed now.
    assert!(export_changed(&svc, &vault, 33).is_empty());
}
