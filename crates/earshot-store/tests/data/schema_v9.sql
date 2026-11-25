-- A schema-9 Earshot database (T12.93 fixture): the schema-8 fixture (schema_v8.sql) with
-- migration 9 applied (the voice sample table and its triggers). Dumped with Python
-- sqlite3 iterdump. src/schema.rs
-- (store_ignore_v9_fixture_has_exactly_the_schema_of_migrations_1_to_9) checks the DDL
-- against migrations 1 to 9; tests/speaker_ignored.rs migrates it. iterdump writes tables in
-- name order, so foreign keys stay off while it loads; this is a connection setting, not stored.
PRAGMA foreign_keys = OFF;
BEGIN TRANSACTION;
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
CREATE TABLE dictionary_word (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    word TEXT NOT NULL
) STRICT;
CREATE TABLE export_record (
    id BLOB PRIMARY KEY NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    device_id BLOB NOT NULL,
    path TEXT NOT NULL,
    exported_unix_ms INTEGER NOT NULL,
    UNIQUE (meeting_id, device_id)
) STRICT;
CREATE TABLE folder (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    name TEXT NOT NULL
) STRICT;
INSERT INTO "folder" VALUES(X'01A3185C50007F11A1B2C3D4E5F60718',1800000000000,5,X'07070707070707070707070707070707',1800000000000,5,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707','Invented folder');
CREATE TABLE local_state (
    key TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) STRICT;
INSERT INTO "local_state" VALUES('summary-kept-local/01a3185c-5000-7d88-aae9-af321bc447ec',X'010000006C6F63616C');
INSERT INTO "local_state" VALUES('echo-locks/01a3185c-5000-7405-9598-dd7506ceb829',X'010000005B5D');
CREATE TABLE meeting (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    title TEXT NOT NULL,
    started_unix_ms INTEGER NOT NULL,
    utc_offset_minutes INTEGER NOT NULL,
    duration_ms INTEGER,
    tags TEXT NOT NULL,
    template TEXT NOT NULL
, folder_id BLOB, summary_kept_local INTEGER NOT NULL DEFAULT 0) STRICT;
INSERT INTO "meeting" VALUES(X'01A3185C50007D88AAE9AF321BC447EC',1800000000000,0,X'07070707070707070707070707070707',1800000000000,0,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707','Invented planning chat',1799000000000,600,120000,'["sample"]','general',X'01A3185C50007F11A1B2C3D4E5F60718',1);
INSERT INTO "meeting" VALUES(X'01A3185C500074059598DD7506CEB829',1800000000000,4,X'07070707070707070707070707070707',1800000000000,4,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707','',1799000500000,600,NULL,'[]','general',NULL,0);
CREATE TABLE note (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    at_ms INTEGER NOT NULL,
    text TEXT NOT NULL
) STRICT;
INSERT INTO "note" VALUES(X'01A3185C500075E09BDAB9CBC4BBC104',1800000000000,3,X'07070707070707070707070707070707',1800000000000,3,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707',X'01A3185C50007D88AAE9AF321BC447EC',500,'A typed note');
CREATE TABLE person (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    display_name TEXT NOT NULL,
    is_me INTEGER NOT NULL,
    merged_into BLOB
) STRICT;
INSERT INTO "person" VALUES(X'01900000000070008000000000005001',1800000000000,0,X'07070707070707070707070707070707',1800000000000,0,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707','Invented Person',0,NULL);
CREATE TABLE segment (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    track_id BLOB NOT NULL REFERENCES track(id),
    pass TEXT NOT NULL,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL,
    text TEXT NOT NULL,
    words TEXT NOT NULL,
    confidence REAL NOT NULL,
    speaker_id BLOB REFERENCES speaker(id)
, edited_wall INTEGER, edited_logical INTEGER, edited_node BLOB) STRICT;
INSERT INTO "segment" VALUES(X'01A3185C50007B05A74E38FEA7EFE5C3',1800000000000,2,X'07070707070707070707070707070707',1800000000000,2,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707',X'01A3185C50007D88AAE9AF321BC447EC',X'01A3185C50007E2581D8BBA9E3A301D1','final',0,900,'Invented words.','[{"text":"Invented","start":0,"end":400,"probability":0.9},{"text":"words.","start":410,"end":900,"probability":0.8}]',8.500000238418579102e-01,NULL,NULL,NULL,NULL);
CREATE TABLE speaker (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    role TEXT NOT NULL,
    display_name TEXT NOT NULL,
    person_id BLOB
) STRICT;
INSERT INTO "speaker" VALUES(X'01900000000070008000000000005201',1800000000000,0,X'07070707070707070707070707070707',1800000000000,0,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707',X'01A3185C500074059598DD7506CEB829','remote','Speaker 9',X'01900000000070008000000000005001');
CREATE TABLE summary (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    schema_version INTEGER NOT NULL,
    template TEXT NOT NULL,
    model TEXT,
    body TEXT NOT NULL
) STRICT;
INSERT INTO "summary" VALUES(X'01900000000070008000000000005301',1800000000000,0,X'07070707070707070707070707070707',1800000000000,0,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707',X'01A3185C500074059598DD7506CEB829',1,'general',NULL,'{"title":"Invented","tldr":"An invented summary.","enhanced_notes":[],"decisions":[],"action_items":[],"open_questions":[],"topics":[]}');
CREATE TABLE summary_request (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    target_device BLOB NOT NULL,
    asking_device BLOB NOT NULL,
    segment_count INTEGER NOT NULL,
    state TEXT NOT NULL,
    reason TEXT NOT NULL
) STRICT;
CREATE TABLE sync_outbox (
    tbl TEXT NOT NULL,
    id BLOB NOT NULL,
    PRIMARY KEY (tbl, id)
) STRICT, WITHOUT ROWID;
INSERT INTO "sync_outbox" VALUES('folder',X'01A3185C50007F11A1B2C3D4E5F60718');
INSERT INTO "sync_outbox" VALUES('meeting',X'01A3185C500074059598DD7506CEB829');
INSERT INTO "sync_outbox" VALUES('meeting',X'01A3185C50007D88AAE9AF321BC447EC');
INSERT INTO "sync_outbox" VALUES('note',X'01A3185C500075E09BDAB9CBC4BBC104');
INSERT INTO "sync_outbox" VALUES('person',X'01900000000070008000000000005001');
INSERT INTO "sync_outbox" VALUES('segment',X'01A3185C50007B05A74E38FEA7EFE5C3');
INSERT INTO "sync_outbox" VALUES('speaker',X'01900000000070008000000000005201');
INSERT INTO "sync_outbox" VALUES('summary',X'01900000000070008000000000005301');
INSERT INTO "sync_outbox" VALUES('track',X'01A3185C50007E2581D8BBA9E3A301D1');
INSERT INTO "sync_outbox" VALUES('voiceprint',X'01900000000070008000000000005101');
CREATE TABLE sync_pending (
    key BLOB PRIMARY KEY NOT NULL,
    seq INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    logical INTEGER NOT NULL,
    node BLOB NOT NULL,
    body BLOB NOT NULL,
    reason TEXT NOT NULL
) STRICT;
CREATE TABLE track (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    kind TEXT NOT NULL,
    device_name TEXT,
    clock_domain TEXT NOT NULL
) STRICT;
INSERT INTO "track" VALUES(X'01A3185C50007E2581D8BBA9E3A301D1',1800000000000,1,X'07070707070707070707070707070707',1800000000000,1,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707',X'01A3185C50007D88AAE9AF321BC447EC','mic',NULL,'replay');
CREATE TABLE voiceprint (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    person_id BLOB NOT NULL,
    model TEXT NOT NULL,
    sum BLOB NOT NULL,
    count INTEGER NOT NULL,
    speech_ms INTEGER NOT NULL
) STRICT;
INSERT INTO "voiceprint" VALUES(X'01900000000070008000000000005101',1800000000000,0,X'07070707070707070707070707070707',1800000000000,0,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707',X'01900000000070008000000000005001','invented-model',X'0000003F0000003F0000003F0000003F',1,5000);
CREATE TABLE voiceprint_sample (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    voiceprint_id BLOB,
    person_id BLOB,
    model TEXT,
    vector BLOB NOT NULL,
    speech_ms INTEGER,
    meeting_id BLOB,
    speaker_id BLOB,
    CHECK (
        (deleted = 1 AND voiceprint_id IS NULL AND person_id IS NULL AND model IS NULL
            AND length(vector) = 0 AND speech_ms IS NULL AND meeting_id IS NULL
            AND speaker_id IS NULL)
        OR (deleted = 0 AND voiceprint_id IS NOT NULL AND person_id IS NOT NULL
            AND model IS NOT NULL AND length(vector) > 0 AND speech_ms IS NOT NULL
            AND (meeting_id IS NULL) = (speaker_id IS NULL))
    )
) STRICT;
CREATE INDEX track_meeting ON track(meeting_id);
CREATE INDEX speaker_meeting ON speaker(meeting_id);
CREATE INDEX segment_meeting ON segment(meeting_id, start_ms);
CREATE INDEX note_meeting ON note(meeting_id, at_ms);
CREATE INDEX summary_meeting ON summary(meeting_id);
CREATE INDEX voiceprint_person ON voiceprint(person_id);
CREATE INDEX voiceprint_model ON voiceprint(model);
CREATE INDEX audio_file_meeting ON audio_file(meeting_id);
CREATE INDEX segment_live ON segment(meeting_id, pass, deleted);
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
CREATE INDEX summary_request_meeting ON summary_request(meeting_id);
CREATE TRIGGER summary_request_sync_insert AFTER INSERT ON summary_request BEGIN
    INSERT INTO sync_outbox SELECT 'summary_request', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'summary_request' AND id = NEW.id);
END;
CREATE TRIGGER summary_request_sync_update AFTER UPDATE ON summary_request BEGIN
    INSERT INTO sync_outbox SELECT 'summary_request', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'summary_request' AND id = NEW.id);
END;
CREATE TRIGGER dictionary_word_sync_insert AFTER INSERT ON dictionary_word BEGIN
    INSERT INTO sync_outbox SELECT 'dictionary_word', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'dictionary_word' AND id = NEW.id);
END;
CREATE TRIGGER dictionary_word_sync_update AFTER UPDATE ON dictionary_word BEGIN
    INSERT INTO sync_outbox SELECT 'dictionary_word', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'dictionary_word' AND id = NEW.id);
END;
CREATE INDEX voiceprint_sample_print ON voiceprint_sample(voiceprint_id);
CREATE INDEX voiceprint_sample_person ON voiceprint_sample(person_id);
CREATE INDEX voiceprint_sample_model ON voiceprint_sample(model);
CREATE INDEX voiceprint_sample_origin ON voiceprint_sample(meeting_id, speaker_id);
CREATE TRIGGER voiceprint_sample_sync_insert AFTER INSERT ON voiceprint_sample BEGIN
    INSERT INTO sync_outbox SELECT 'voiceprint_sample', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'voiceprint_sample' AND id = NEW.id);
END;
CREATE TRIGGER voiceprint_sample_sync_update AFTER UPDATE ON voiceprint_sample BEGIN
    INSERT INTO sync_outbox SELECT 'voiceprint_sample', NEW.id
        WHERE NOT EXISTS (SELECT 1 FROM sync_outbox WHERE tbl = 'voiceprint_sample' AND id = NEW.id);
END;
COMMIT;
PRAGMA user_version = 9;
