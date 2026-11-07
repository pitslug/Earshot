//! Schema migrations. `MIGRATIONS[i]` takes the database from `user_version` `i` to `i + 1`;
//! applied migrations are never edited, a change is a new entry.
//!
//! Conventions: ids are 16-byte BLOBs (UUID bytes, so `ORDER BY id` is `EntityId` order);
//! sync metadata is spread over the `META` columns (HLC as wall ms, logical counter, node);
//! unsigned values are stored as INTEGER and range-checked on the way in and out; nested
//! values (words, tags, summary bodies) are JSON text. Only children of a meeting have
//! foreign keys; persons and voiceprints may arrive by sync in any order.

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

/// Tables with sync metadata (the HLC high-water mark is the greatest `updated_*` in them).
pub(crate) const SYNCED_TABLES: [&str; 8] = [
    "meeting",
    "track",
    "speaker",
    "segment",
    "note",
    "summary",
    "person",
    "voiceprint",
];

/// The migrations, in order, with `{META}` expanded.
pub(crate) fn migrations() -> Vec<String> {
    [V1].iter().map(|m| m.replace("{META}", META)).collect()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    /// SHA-256 of each migration as applied. A migration that has shipped must never change
    /// (databases already at that version would silently differ); add a new one instead.
    /// Appending a migration appends its hash here.
    const APPLIED: &[&str] = &["a852be73b469315e94109bf9f648c0201399ce286f94a86438e781e695bfdaed"];

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
