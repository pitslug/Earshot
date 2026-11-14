//! Schema migrations. `MIGRATIONS[i]` takes the database from `user_version` `i` to `i + 1`;
//! applied migrations are never edited, a change is a new entry.
//!
//! Conventions: ids are 16-byte BLOBs (UUID bytes, so `ORDER BY id` is `EntityId` order);
//! sync metadata is spread over the `META` columns (HLC as wall ms, logical counter, node);
//! unsigned values are stored as INTEGER and range-checked on the way in and out; nested
//! values (words, tags, summary bodies) are JSON text. Only children of a meeting have
//! foreign keys; persons, voiceprints, folders and a meeting's folder may arrive by sync in
//! any order.

/// The sync-metadata columns of every syncable table, in this order.
pub(crate) const META: &str = "created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, \
     created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, \
     updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, \
     deleted INTEGER NOT NULL, origin_device BLOB NOT NULL";

/// Migration 1: the T5.1 data model.
const V1: &str = "
CREATE TABLE meeting (
    id BLOB PRIMARY KEY NOT NULL, {META},
    title TEXT NOT NULL,
    started_unix_ms INTEGER NOT NULL,
    utc_offset_minutes INTEGER NOT NULL,
    duration_ms INTEGER,
    tags TEXT NOT NULL,
    template TEXT NOT NULL
) STRICT;

CREATE TABLE track (
    id BLOB PRIMARY KEY NOT NULL, {META},
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    kind TEXT NOT NULL,
    device_name TEXT,
    clock_domain TEXT NOT NULL
) STRICT;
CREATE INDEX track_meeting ON track(meeting_id);

CREATE TABLE speaker (
    id BLOB PRIMARY KEY NOT NULL, {META},
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    role TEXT NOT NULL,
    display_name TEXT NOT NULL,
    person_id BLOB
) STRICT;
CREATE INDEX speaker_meeting ON speaker(meeting_id);

CREATE TABLE segment (
    id BLOB PRIMARY KEY NOT NULL, {META},
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    track_id BLOB NOT NULL REFERENCES track(id),
    pass TEXT NOT NULL,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL,
    text TEXT NOT NULL,
    words TEXT NOT NULL,
    confidence REAL NOT NULL,
    speaker_id BLOB REFERENCES speaker(id)
) STRICT;
CREATE INDEX segment_meeting ON segment(meeting_id, start_ms);

CREATE TABLE note (
    id BLOB PRIMARY KEY NOT NULL, {META},
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    at_ms INTEGER NOT NULL,
    text TEXT NOT NULL
) STRICT;
CREATE INDEX note_meeting ON note(meeting_id, at_ms);

CREATE TABLE summary (
    id BLOB PRIMARY KEY NOT NULL, {META},
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    schema_version INTEGER NOT NULL,
    template TEXT NOT NULL,
    model TEXT,
    body TEXT NOT NULL
) STRICT;
CREATE INDEX summary_meeting ON summary(meeting_id);

CREATE TABLE person (
    id BLOB PRIMARY KEY NOT NULL, {META},
    display_name TEXT NOT NULL,
    is_me INTEGER NOT NULL,
    merged_into BLOB
) STRICT;

CREATE TABLE voiceprint (
    id BLOB PRIMARY KEY NOT NULL, {META},
    person_id BLOB NOT NULL,
    model TEXT NOT NULL,
    sum BLOB NOT NULL,
    count INTEGER NOT NULL,
    speech_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX voiceprint_person ON voiceprint(person_id);
CREATE INDEX voiceprint_model ON voiceprint(model);

-- NeverSync tables: no sync metadata, rows are hard-deleted.
CREATE TABLE audio_file (
    id BLOB PRIMARY KEY NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    track_id BLOB NOT NULL REFERENCES track(id),
    path TEXT NOT NULL UNIQUE,
    format TEXT NOT NULL,
    sample_rate_hz INTEGER NOT NULL,
    frames INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL,
    pipeline_version TEXT NOT NULL,
    aec_version TEXT,
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX audio_file_meeting ON audio_file(meeting_id);

CREATE TABLE export_record (
    id BLOB PRIMARY KEY NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    device_id BLOB NOT NULL,
    path TEXT NOT NULL,
    exported_unix_ms INTEGER NOT NULL,
    UNIQUE (meeting_id, device_id)
) STRICT;

-- Device-local settings (this install's device id).
CREATE TABLE local_state (
    key TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) STRICT;
";

/// Migration 2: folders (T9.5a, docs/folders-design.md). `meeting.folder_id` has no foreign
/// key: sync may deliver a meeting before its folder. No unique index on `folder.name`:
/// tombstones clear every name to the same value and sync may deliver two live folders with
/// one name; the runtime checks live names.
const V2: &str = "
CREATE TABLE folder (
    id BLOB PRIMARY KEY NOT NULL, {META},
    name TEXT NOT NULL
) STRICT;

ALTER TABLE meeting ADD COLUMN folder_id BLOB;
";

/// Migration 3: the user's edit mark on a transcript segment (T12.16,
/// `SegmentRow::text_edited_at`): the HLC of the last text edit, all three NULL for text as
/// the recogniser wrote it, which every existing row reads as.
const V3: &str = "
ALTER TABLE segment ADD COLUMN edited_wall INTEGER;
ALTER TABLE segment ADD COLUMN edited_logical INTEGER;
ALTER TABLE segment ADD COLUMN edited_node BLOB;
";

/// Migration 4: whether a meeting has a live segment of a pass (`has_live_segment`, asked per
/// meeting by the meeting list, T9.24a) is answered from this index alone, never by reading a
/// long meeting's segment rows (T12.30).
const V4: &str = "
CREATE INDEX segment_live ON segment(meeting_id, pass, deleted);
";

/// Migration 5: a meeting kept on this computer as a synced meeting field (T11.1a,
/// docs/sync-spec.md section 5.1), so every synced copy of the meeting carries it. Filled from
/// T12.13's device-local record `summary-kept-local/<meeting>` (the meeting id in hyphenated
/// lower-case hex); the record stays, and the runtime reads either as kept. The rows are not
/// re-stamped: the field records what this device already knew, not an edit.
const V5: &str = "
ALTER TABLE meeting ADD COLUMN summary_kept_local INTEGER NOT NULL DEFAULT 0;
UPDATE meeting SET summary_kept_local = 1 WHERE EXISTS (
    SELECT 1 FROM local_state WHERE key = 'summary-kept-local/' || lower(
        substr(hex(meeting.id), 1, 8) || '-' || substr(hex(meeting.id), 9, 4) || '-' ||
        substr(hex(meeting.id), 13, 4) || '-' || substr(hex(meeting.id), 17, 4) || '-' ||
        substr(hex(meeting.id), 21, 12))
);
";

/// Migration 6: what sync pushes and what it holds back (T11.1b, docs/sync-spec.md section
/// 5.2). `sync_outbox` names every synced row changed on this device, filled by a trigger
/// pair on each table of [`SYNCED_TABLES`] whether or not sync is on, and here with every row
/// that already exists, so turning sync on later sends them too. `sync_pending` holds pulled
/// rows that cannot be applied yet, still sealed. Both are device-local (no sync metadata).
/// The triggers insert only an absent entry rather than `INSERT OR IGNORE`: SQLite replaces a
/// trigger's conflict clause with the firing statement's, and the store writes with upserts
/// (`ON CONFLICT DO UPDATE`), so an ignored conflict would be an error on a row's second
/// change. Written out table by table: a migration never changes, so a synced table added later
/// brings its own triggers in its own migration (pinned by
/// `store_sync_triggers_cover_every_synced_table`).
const V6: &str = "
CREATE TABLE sync_outbox (
    tbl TEXT NOT NULL,
    id BLOB NOT NULL,
    PRIMARY KEY (tbl, id)
) STRICT, WITHOUT ROWID;

CREATE TABLE sync_pending (
    key BLOB PRIMARY KEY NOT NULL,
    seq INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    logical INTEGER NOT NULL,
    node BLOB NOT NULL,
    body BLOB NOT NULL,
    reason TEXT NOT NULL
) STRICT;

CREATE TRIGGER meeting_sync_insert AFTER INSERT ON meeting BEGIN
    INSERT INTO sync_outbox SELECT 'meeting', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'meeting' AND id = NEW.id);
END;
CREATE TRIGGER meeting_sync_update AFTER UPDATE ON meeting BEGIN
    INSERT INTO sync_outbox SELECT 'meeting', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'meeting' AND id = NEW.id);
END;
CREATE TRIGGER track_sync_insert AFTER INSERT ON track BEGIN
    INSERT INTO sync_outbox SELECT 'track', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'track' AND id = NEW.id);
END;
CREATE TRIGGER track_sync_update AFTER UPDATE ON track BEGIN
    INSERT INTO sync_outbox SELECT 'track', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'track' AND id = NEW.id);
END;
CREATE TRIGGER speaker_sync_insert AFTER INSERT ON speaker BEGIN
    INSERT INTO sync_outbox SELECT 'speaker', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'speaker' AND id = NEW.id);
END;
CREATE TRIGGER speaker_sync_update AFTER UPDATE ON speaker BEGIN
    INSERT INTO sync_outbox SELECT 'speaker', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'speaker' AND id = NEW.id);
END;
CREATE TRIGGER segment_sync_insert AFTER INSERT ON segment BEGIN
    INSERT INTO sync_outbox SELECT 'segment', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'segment' AND id = NEW.id);
END;
CREATE TRIGGER segment_sync_update AFTER UPDATE ON segment BEGIN
    INSERT INTO sync_outbox SELECT 'segment', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'segment' AND id = NEW.id);
END;
CREATE TRIGGER note_sync_insert AFTER INSERT ON note BEGIN
    INSERT INTO sync_outbox SELECT 'note', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'note' AND id = NEW.id);
END;
CREATE TRIGGER note_sync_update AFTER UPDATE ON note BEGIN
    INSERT INTO sync_outbox SELECT 'note', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'note' AND id = NEW.id);
END;
CREATE TRIGGER summary_sync_insert AFTER INSERT ON summary BEGIN
    INSERT INTO sync_outbox SELECT 'summary', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'summary' AND id = NEW.id);
END;
CREATE TRIGGER summary_sync_update AFTER UPDATE ON summary BEGIN
    INSERT INTO sync_outbox SELECT 'summary', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'summary' AND id = NEW.id);
END;
CREATE TRIGGER person_sync_insert AFTER INSERT ON person BEGIN
    INSERT INTO sync_outbox SELECT 'person', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'person' AND id = NEW.id);
END;
CREATE TRIGGER person_sync_update AFTER UPDATE ON person BEGIN
    INSERT INTO sync_outbox SELECT 'person', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'person' AND id = NEW.id);
END;
CREATE TRIGGER voiceprint_sync_insert AFTER INSERT ON voiceprint BEGIN
    INSERT INTO sync_outbox SELECT 'voiceprint', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'voiceprint' AND id = NEW.id);
END;
CREATE TRIGGER voiceprint_sync_update AFTER UPDATE ON voiceprint BEGIN
    INSERT INTO sync_outbox SELECT 'voiceprint', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'voiceprint' AND id = NEW.id);
END;
CREATE TRIGGER folder_sync_insert AFTER INSERT ON folder BEGIN
    INSERT INTO sync_outbox SELECT 'folder', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'folder' AND id = NEW.id);
END;
CREATE TRIGGER folder_sync_update AFTER UPDATE ON folder BEGIN
    INSERT INTO sync_outbox SELECT 'folder', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'folder' AND id = NEW.id);
END;

INSERT OR IGNORE INTO sync_outbox SELECT 'meeting', id FROM meeting;
INSERT OR IGNORE INTO sync_outbox SELECT 'track', id FROM track;
INSERT OR IGNORE INTO sync_outbox SELECT 'speaker', id FROM speaker;
INSERT OR IGNORE INTO sync_outbox SELECT 'segment', id FROM segment;
INSERT OR IGNORE INTO sync_outbox SELECT 'note', id FROM note;
INSERT OR IGNORE INTO sync_outbox SELECT 'summary', id FROM summary;
INSERT OR IGNORE INTO sync_outbox SELECT 'person', id FROM person;
INSERT OR IGNORE INTO sync_outbox SELECT 'voiceprint', id FROM voiceprint;
INSERT OR IGNORE INTO sync_outbox SELECT 'folder', id FROM folder;
";

/// Tables with sync metadata (the HLC high-water mark is the greatest `updated_*` in them).
pub(crate) const SYNCED_TABLES: [&str; 9] = [
    "meeting",
    "track",
    "speaker",
    "segment",
    "note",
    "summary",
    "person",
    "voiceprint",
    "folder",
];

/// The migrations, in order, with `{META}` expanded.
pub(crate) fn migrations() -> Vec<String> {
    [V1, V2, V3, V4, V5, V6]
        .iter()
        .map(|m| m.replace("{META}", META))
        .collect()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    /// SHA-256 of each migration as applied. A migration that has shipped must never change
    /// (databases already at that version would silently differ); add a new one instead.
    /// Appending a migration appends its hash here.
    const APPLIED: &[&str] = &[
        "a852be73b469315e94109bf9f648c0201399ce286f94a86438e781e695bfdaed",
        "3994bcb0037913aeecece98d816f969dd6d5ba90f0a3a0d55f144403ed633107",
        "46159521b8052eff5fba4622c74f0f91717db9ec39170deba1757f35e48602d4",
        "85a4c55bdbd320805308174fdb3a2f531d9dfae9262659b726437b8caa14603b",
        "a682b36a9b8d6bd831f0f4865fa7554dd79330f35e80f10c7512389e3bcc1df4",
        "964bd0e4d3a4bb7e505659731a7dea0a1774269734863714d7a23c4af756b466",
    ];

    /// `(type, name, tbl_name, sql)` of every schema object, sorted.
    fn objects(c: &rusqlite::Connection) -> Vec<(String, String, String, Option<String>)> {
        let mut st = c
            .prepare(
                "SELECT type, name, tbl_name, sql FROM sqlite_master \
                 ORDER BY type, name",
            )
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The committed schema-1 fixture (`tests/data/schema_v1.sql`, used by
    /// `tests/folder.rs`) creates exactly the objects migration 1 creates, byte for byte, so
    /// the migration test runs on a real schema-1 database.
    #[test]
    fn store_folder_v1_fixture_has_exactly_the_schema_of_migration_1() {
        let fixture = rusqlite::Connection::open_in_memory().unwrap();
        fixture
            .execute_batch(include_str!("../tests/data/schema_v1.sql"))
            .unwrap();
        let fresh = rusqlite::Connection::open_in_memory().unwrap();
        fresh.execute_batch(&super::migrations()[0]).unwrap();
        let want = objects(&fresh);
        assert!(want.iter().any(|o| o.1 == "meeting"), "{want:?}");
        assert_eq!(objects(&fixture), want);
        let version: u32 = fixture
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    /// The committed schema-2 fixture (`tests/data/schema_v2.sql`, used by
    /// `tests/segment_edit.rs`) creates exactly the objects migrations 1 and 2 create, so
    /// the segment edit migration (3) is tested on a real schema-2 database.
    #[test]
    fn store_segment_edit_v2_fixture_has_exactly_the_schema_of_migrations_1_and_2() {
        let fixture = rusqlite::Connection::open_in_memory().unwrap();
        fixture
            .execute_batch(include_str!("../tests/data/schema_v2.sql"))
            .unwrap();
        let fresh = rusqlite::Connection::open_in_memory().unwrap();
        for m in &super::migrations()[..2] {
            fresh.execute_batch(m).unwrap();
        }
        let want = objects(&fresh);
        assert!(want.iter().any(|o| o.1 == "folder"), "{want:?}");
        assert_eq!(objects(&fixture), want);
        let version: u32 = fixture
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    /// The committed schema-3 fixture (`tests/data/schema_v3.sql`, used by
    /// `tests/segment_live_index.rs`) creates exactly the objects migrations 1 to 3 create, so
    /// the live-segment index migration (4) is tested on a real schema-3 database.
    #[test]
    fn store_segment_live_v3_fixture_has_exactly_the_schema_of_migrations_1_to_3() {
        let fixture = rusqlite::Connection::open_in_memory().unwrap();
        fixture
            .execute_batch(include_str!("../tests/data/schema_v3.sql"))
            .unwrap();
        let fresh = rusqlite::Connection::open_in_memory().unwrap();
        for m in &super::migrations()[..3] {
            fresh.execute_batch(m).unwrap();
        }
        let want = objects(&fresh);
        assert!(
            want.iter().any(|o| o
                .3
                .as_deref()
                .is_some_and(|sql| sql.contains("edited_wall"))),
            "{want:?}"
        );
        assert_eq!(objects(&fixture), want);
        let version: u32 = fixture
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
    }

    /// The committed schema-4 fixture (`tests/data/schema_v4.sql`, used by
    /// `tests/kept_local.rs`) creates exactly the objects migrations 1 to 4 create, so the
    /// kept-local migration (5) is tested on a real schema-4 database.
    #[test]
    fn store_kept_local_v4_fixture_has_exactly_the_schema_of_migrations_1_to_4() {
        let fixture = rusqlite::Connection::open_in_memory().unwrap();
        fixture
            .execute_batch(include_str!("../tests/data/schema_v4.sql"))
            .unwrap();
        let fresh = rusqlite::Connection::open_in_memory().unwrap();
        for m in &super::migrations()[..4] {
            fresh.execute_batch(m).unwrap();
        }
        let want = objects(&fresh);
        assert!(want.iter().any(|o| o.1 == "segment_live"), "{want:?}");
        assert_eq!(objects(&fixture), want);
        let version: u32 = fixture
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }

    /// The committed schema-5 fixture (`tests/data/schema_v5.sql`, used by
    /// `tests/sync_apply.rs`) creates exactly the objects migrations 1 to 5 create, so the
    /// sync migration (6) is tested on a real schema-5 database.
    #[test]
    fn store_sync_v5_fixture_has_exactly_the_schema_of_migrations_1_to_5() {
        let fixture = rusqlite::Connection::open_in_memory().unwrap();
        fixture
            .execute_batch(include_str!("../tests/data/schema_v5.sql"))
            .unwrap();
        let fresh = rusqlite::Connection::open_in_memory().unwrap();
        for m in &super::migrations()[..5] {
            fresh.execute_batch(m).unwrap();
        }
        let want = objects(&fresh);
        assert!(
            want.iter().any(|o| o
                .3
                .as_deref()
                .is_some_and(|sql| sql.contains("summary_kept_local"))),
            "{want:?}"
        );
        assert_eq!(objects(&fixture), want);
        let version: u32 = fixture
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 5);
    }

    /// Every synced table has its insert and update trigger feeding `sync_outbox`, and no
    /// other table has one: a synced table added later brings its triggers in its own
    /// migration, or this fails (T11.1b).
    #[test]
    fn store_sync_triggers_cover_every_synced_table() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        for m in super::migrations() {
            c.execute_batch(&m).unwrap();
        }
        let mut st = c
            .prepare(
                "SELECT name, tbl_name, sql FROM sqlite_master WHERE type = 'trigger' \
                 ORDER BY name",
            )
            .unwrap();
        let got: Vec<(String, String, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut want: Vec<(String, String)> = Vec::new();
        for t in super::SYNCED_TABLES {
            want.push((format!("{t}_sync_insert"), t.to_owned()));
            want.push((format!("{t}_sync_update"), t.to_owned()));
        }
        want.sort();
        let names: Vec<(String, String)> =
            got.iter().map(|(n, t, _)| (n.clone(), t.clone())).collect();
        assert_eq!(names, want);
        for (name, table, sql) in &got {
            let event = if name.ends_with("_insert") {
                "AFTER INSERT"
            } else {
                "AFTER UPDATE"
            };
            assert!(sql.contains(event), "{sql}");
            assert!(
                sql.contains(&format!("INSERT INTO sync_outbox SELECT '{table}', NEW.id"))
                    && sql.contains(&format!(
                        "WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = '{table}' AND id = NEW.id)"
                    )),
                "{sql}"
            );
        }
    }

    /// The query plan of `has_live_segment` on a database after `upto` migrations.
    fn live_segment_plan(upto: usize) -> String {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        for m in &super::migrations()[..upto] {
            c.execute_batch(m).unwrap();
        }
        let mut st = c
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                crate::repo::HAS_LIVE_SEGMENT_SQL
            ))
            .unwrap();
        // The plan does not depend on the values.
        st.query_map(rusqlite::params![vec![0u8; 16], "final"], |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ")
    }

    /// T12.30: whether a meeting has a live segment of a pass is answered from the index
    /// alone (migration 4), never by reading a long meeting's segment rows; before migration 4
    /// the only index (`segment_meeting`) left every row of the meeting to read.
    #[test]
    fn store_segment_live_existence_is_answered_from_its_index() {
        let plan = live_segment_plan(super::migrations().len());
        assert!(plan.contains("USING COVERING INDEX segment_live"), "{plan}");
        // The negative control: the schema before the index reads the rows.
        let before = live_segment_plan(3);
        assert!(!before.contains("COVERING INDEX"), "{before}");
    }

    /// Every table with sync metadata is in `SYNCED_TABLES` (the HLC resumes from them) and
    /// every listed table has the metadata.
    #[test]
    fn store_synced_tables_are_exactly_the_tables_with_sync_metadata_including_folder() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        for m in super::migrations() {
            c.execute_batch(&m).unwrap();
        }
        let mut st = c
            .prepare(
                "SELECT m.name FROM sqlite_master m, pragma_table_info(m.name) p \
                 WHERE m.type = 'table' AND p.name = 'updated_wall' ORDER BY m.name",
            )
            .unwrap();
        let with_meta: Vec<String> = st
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut listed: Vec<String> = super::SYNCED_TABLES
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        listed.sort();
        assert_eq!(with_meta, listed);
        assert!(listed.contains(&"folder".to_owned()));
    }

    #[test]
    fn store_applied_migrations_never_change() {
        let got: Vec<String> = super::migrations()
            .iter()
            .map(|m| {
                Sha256::digest(m.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect()
            })
            .collect();
        assert_eq!(
            got, APPLIED,
            "a shipped migration was edited (or a new one is unpinned)"
        );
    }
}
