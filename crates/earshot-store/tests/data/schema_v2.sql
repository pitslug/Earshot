-- A schema-2 Earshot database (T12.16 fixture): the schema-1 fixture (schema_v1.sql) with
-- migration 2 applied and an invented folder holding its first meeting, dumped with Python
-- sqlite3 iterdump. src/schema.rs (store_segment_edit_v2_fixture_has_exactly_the_schema_of_
-- migrations_1_and_2) checks the DDL against migrations 1 and 2; tests/segment_edit.rs
-- migrates it. iterdump writes tables in name order (a segment row before the speaker
-- table), so foreign keys stay off while it loads; this is a connection setting, not stored.
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
CREATE TABLE meeting (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    title TEXT NOT NULL,
    started_unix_ms INTEGER NOT NULL,
    utc_offset_minutes INTEGER NOT NULL,
    duration_ms INTEGER,
    tags TEXT NOT NULL,
    template TEXT NOT NULL
, folder_id BLOB) STRICT;
INSERT INTO "meeting" VALUES(X'01A3185C50007D88AAE9AF321BC447EC',1800000000000,0,X'07070707070707070707070707070707',1800000000000,0,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707','Invented planning chat',1799000000000,600,120000,'["sample"]','general',X'01A3185C50007F11A1B2C3D4E5F60718');
INSERT INTO "meeting" VALUES(X'01A3185C500074059598DD7506CEB829',1800000000000,4,X'07070707070707070707070707070707',1800000000000,4,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707','',1799000500000,600,NULL,'[]','general',NULL);
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
) STRICT;
INSERT INTO "segment" VALUES(X'01A3185C50007B05A74E38FEA7EFE5C3',1800000000000,2,X'07070707070707070707070707070707',1800000000000,2,X'07070707070707070707070707070707',0,X'07070707070707070707070707070707',X'01A3185C50007D88AAE9AF321BC447EC',X'01A3185C50007E2581D8BBA9E3A301D1','final',0,900,'Invented words.','[{"text":"Invented","start":0,"end":400,"probability":0.9},{"text":"words.","start":410,"end":900,"probability":0.8}]',8.500000238418579102e-01,NULL);
CREATE TABLE speaker (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    role TEXT NOT NULL,
    display_name TEXT NOT NULL,
    person_id BLOB
) STRICT;
CREATE TABLE summary (
    id BLOB PRIMARY KEY NOT NULL, created_wall INTEGER NOT NULL, created_logical INTEGER NOT NULL, created_node BLOB NOT NULL, updated_wall INTEGER NOT NULL, updated_logical INTEGER NOT NULL, updated_node BLOB NOT NULL, deleted INTEGER NOT NULL, origin_device BLOB NOT NULL,
    meeting_id BLOB NOT NULL REFERENCES meeting(id),
    schema_version INTEGER NOT NULL,
    template TEXT NOT NULL,
    model TEXT,
    body TEXT NOT NULL
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
CREATE INDEX track_meeting ON track(meeting_id);
CREATE INDEX speaker_meeting ON speaker(meeting_id);
CREATE INDEX segment_meeting ON segment(meeting_id, start_ms);
CREATE INDEX note_meeting ON note(meeting_id, at_ms);
CREATE INDEX summary_meeting ON summary(meeting_id);
CREATE INDEX voiceprint_person ON voiceprint(person_id);
CREATE INDEX voiceprint_model ON voiceprint(model);
CREATE INDEX audio_file_meeting ON audio_file(meeting_id);
COMMIT;
PRAGMA user_version = 2;
