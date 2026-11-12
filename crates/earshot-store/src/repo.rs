//! The core repository traits over SQLite.

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::diar::EmbeddingModelId;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::{
    AudioFile, AudioFileRepo, ExportRecord, ExportRepo, Folder, FolderRepo, Meeting, MeetingRepo,
    Note, RepoError, SegmentRow, Speaker, Summary, Track, TranscriptPass, VaultPath,
};
use earshot_core::voiceprint::{Person, Voiceprint, VoiceprintError, VoiceprintRepo};
use rusqlite::types::{Type, Value};
use rusqlite::{Connection, Row, params_from_iter};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::db::{Store, StoreTx};

const META_COLS: [&str; 8] = [
    "created_wall",
    "created_logical",
    "created_node",
    "updated_wall",
    "updated_logical",
    "updated_node",
    "deleted",
    "origin_device",
];

fn storage(e: impl std::fmt::Display) -> RepoError {
    RepoError::Storage(e.to_string())
}

/// `id`, the meta columns, then `rest`.
fn synced_cols(rest: &[&'static str]) -> Vec<&'static str> {
    let mut v = vec!["id"];
    v.extend(META_COLS);
    v.extend(rest);
    v
}

fn upsert_sql(table: &str, cols: &[&str], conflict: &str) -> String {
    let names = cols.join(", ");
    let qs: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
    let keys: Vec<&str> = conflict.split(',').map(str::trim).collect();
    let sets: Vec<String> = cols
        .iter()
        .filter(|c| !keys.contains(c))
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    format!(
        "INSERT INTO {table} ({names}) VALUES ({}) ON CONFLICT({conflict}) DO UPDATE SET {}",
        qs.join(", "),
        sets.join(", ")
    )
}

fn select_sql(table: &str, cols: &[&str], tail: &str) -> String {
    format!("SELECT {} FROM {table} {tail}", cols.join(", "))
}

fn int(v: u64) -> Result<Value, RepoError> {
    i64::try_from(v)
        .map(Value::Integer)
        .map_err(|_| storage(format!("value {v} exceeds the SQLite integer range")))
}

fn id_value(id: EntityId) -> Value {
    Value::Blob(id.as_uuid().as_bytes().to_vec())
}

fn device_value(id: DeviceId) -> Value {
    Value::Blob(id.as_uuid().as_bytes().to_vec())
}

fn opt_id(id: Option<EntityId>) -> Value {
    id.map_or(Value::Null, id_value)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn opt_text(s: Option<&str>) -> Value {
    s.map_or(Value::Null, text)
}

fn json<T: Serialize + ?Sized>(v: &T) -> Result<Value, RepoError> {
    serde_json::to_string(v).map(Value::Text).map_err(storage)
}

/// The serde name of a unit enum variant (`"mic"`, `"remote"`, ...).
fn enum_name<T: Serialize>(v: &T) -> Result<Value, RepoError> {
    match serde_json::to_value(v).map_err(storage)? {
        serde_json::Value::String(s) => Ok(Value::Text(s)),
        other => Err(storage(format!("not a unit enum: {other}"))),
    }
}

fn meta_values(m: &SyncMeta) -> Result<[Value; 8], RepoError> {
    Ok([
        int(m.created_at.wall_ms)?,
        Value::Integer(i64::from(m.created_at.logical)),
        device_value(m.created_at.node),
        int(m.updated_at.wall_ms)?,
        Value::Integer(i64::from(m.updated_at.logical)),
        device_value(m.updated_at.node),
        Value::Integer(i64::from(m.deleted)),
        device_value(m.origin_device_id),
    ])
}

fn synced_values(id: EntityId, meta: &SyncMeta, rest: Vec<Value>) -> Result<Vec<Value>, RepoError> {
    let mut v = vec![id_value(id)];
    v.extend(meta_values(meta)?);
    v.extend(rest);
    Ok(v)
}

// ---- row decoding (rusqlite::Result inside query closures) ----

fn conv_err(
    i: usize,
    ty: Type,
    e: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(i, ty, Box::new(e))
}

#[derive(Debug)]
struct BadValue(String);
impl std::fmt::Display for BadValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BadValue {}

fn get_u64(r: &Row<'_>, i: usize) -> rusqlite::Result<u64> {
    let v: i64 = r.get(i)?;
    u64::try_from(v).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(i, v))
}

fn get_opt_u64(r: &Row<'_>, i: usize) -> rusqlite::Result<Option<u64>> {
    r.get::<_, Option<i64>>(i)?
        .map(|v| u64::try_from(v).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(i, v)))
        .transpose()
}

fn get_id(r: &Row<'_>, i: usize) -> rusqlite::Result<EntityId> {
    Ok(EntityId::from_bytes(r.get::<_, [u8; 16]>(i)?))
}

fn get_opt_id(r: &Row<'_>, i: usize) -> rusqlite::Result<Option<EntityId>> {
    Ok(r.get::<_, Option<[u8; 16]>>(i)?.map(EntityId::from_bytes))
}

fn get_device(r: &Row<'_>, i: usize) -> rusqlite::Result<DeviceId> {
    Ok(DeviceId::from_bytes(r.get::<_, [u8; 16]>(i)?))
}

fn get_json<T: DeserializeOwned>(r: &Row<'_>, i: usize) -> rusqlite::Result<T> {
    let s: String = r.get(i)?;
    serde_json::from_str(&s).map_err(|e| conv_err(i, Type::Text, e))
}

fn get_enum<T: DeserializeOwned>(r: &Row<'_>, i: usize) -> rusqlite::Result<T> {
    let s: String = r.get(i)?;
    serde_json::from_value(serde_json::Value::String(s)).map_err(|e| conv_err(i, Type::Text, e))
}

/// Sync metadata from the 8 columns starting at `i`.
fn get_meta(r: &Row<'_>, i: usize) -> rusqlite::Result<SyncMeta> {
    Ok(SyncMeta {
        created_at: HlcTimestamp {
            wall_ms: get_u64(r, i)?,
            logical: r.get(i + 1)?,
            node: get_device(r, i + 2)?,
        },
        updated_at: HlcTimestamp {
            wall_ms: get_u64(r, i + 3)?,
            logical: r.get(i + 4)?,
            node: get_device(r, i + 5)?,
        },
        deleted: r.get(i + 6)?,
        origin_device_id: get_device(r, i + 7)?,
    })
}

/// Index of the first column after `id` and the meta columns.
const REST: usize = 1 + META_COLS.len();

fn query<T>(
    c: &Connection,
    sql: &str,
    params: &[Value],
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, RepoError> {
    let mut st = c.prepare_cached(sql).map_err(storage)?;
    let rows = st
        .query_map(params_from_iter(params.iter()), f)
        .map_err(storage)?;
    rows.collect::<rusqlite::Result<Vec<T>>>().map_err(storage)
}

fn exec(c: &Connection, sql: &str, params: &[Value]) -> Result<usize, RepoError> {
    c.prepare_cached(sql)
        .and_then(|mut st| st.execute(params_from_iter(params.iter())))
        .map_err(storage)
}

fn exists(c: &Connection, sql: &str, params: &[Value]) -> Result<bool, RepoError> {
    Ok(!query(c, sql, params, |_| Ok(()))?.is_empty())
}

/// Whether any meeting row exists, tombstones included.
pub(crate) fn any_meeting(c: &Connection) -> Result<bool, RepoError> {
    exists(c, "SELECT 1 FROM meeting LIMIT 1", &[])
}

fn need_meeting(c: &Connection, id: EntityId) -> Result<(), RepoError> {
    if exists(c, "SELECT 1 FROM meeting WHERE id = ?1", &[id_value(id)])? {
        Ok(())
    } else {
        Err(RepoError::MissingParent {
            entity: "meeting",
            id,
        })
    }
}

fn need_child(
    c: &Connection,
    table: &'static str,
    meeting: EntityId,
    id: EntityId,
) -> Result<(), RepoError> {
    let sql = format!("SELECT 1 FROM {table} WHERE id = ?1 AND meeting_id = ?2");
    if exists(c, &sql, &[id_value(id), id_value(meeting)])? {
        Ok(())
    } else {
        Err(RepoError::MissingParent { entity: table, id })
    }
}

// ---- meeting ----

const MEETING: [&str; 7] = [
    "title",
    "started_unix_ms",
    "utc_offset_minutes",
    "duration_ms",
    "tags",
    "template",
    "folder_id",
];

fn meeting_row(r: &Row<'_>) -> rusqlite::Result<Meeting> {
    let template: String = r.get(REST + 5)?;
    Ok(Meeting {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        title: r.get(REST)?,
        started_unix_ms: get_u64(r, REST + 1)?,
        utc_offset_minutes: r.get(REST + 2)?,
        duration_ms: get_opt_u64(r, REST + 3)?,
        tags: get_json(r, REST + 4)?,
        template: template
            .parse()
            .map_err(|_| conv_err(REST + 5, Type::Text, BadValue(template.clone())))?,
        folder_id: get_opt_id(r, REST + 6)?,
    })
}

fn get_meeting(c: &Connection, id: EntityId) -> Result<Option<Meeting>, RepoError> {
    let sql = select_sql("meeting", &synced_cols(&MEETING), "WHERE id = ?1");
    Ok(query(c, &sql, &[id_value(id)], meeting_row)?.pop())
}

fn all_meetings(c: &Connection) -> Result<Vec<Meeting>, RepoError> {
    let sql = select_sql(
        "meeting",
        &synced_cols(&MEETING),
        "ORDER BY started_unix_ms, id",
    );
    query(c, &sql, &[], meeting_row)
}

fn put_meeting(c: &Connection, m: &Meeting) -> Result<(), RepoError> {
    let v = synced_values(
        m.id,
        &m.meta,
        vec![
            text(&m.title),
            int(m.started_unix_ms)?,
            Value::Integer(i64::from(m.utc_offset_minutes)),
            m.duration_ms.map_or(Ok(Value::Null), int)?,
            json(&m.tags)?,
            text(m.template.name()),
            opt_id(m.folder_id),
        ],
    )?;
    exec(c, &upsert_sql("meeting", &synced_cols(&MEETING), "id"), &v)?;
    Ok(())
}

// ---- track ----

const TRACK: [&str; 4] = ["meeting_id", "kind", "device_name", "clock_domain"];

fn track_row(r: &Row<'_>) -> rusqlite::Result<Track> {
    Ok(Track {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        meeting_id: get_id(r, REST)?,
        kind: get_enum(r, REST + 1)?,
        device_name: r.get(REST + 2)?,
        clock_domain: get_enum(r, REST + 3)?,
    })
}

fn tracks_of(c: &Connection, meeting: EntityId) -> Result<Vec<Track>, RepoError> {
    let sql = select_sql(
        "track",
        &synced_cols(&TRACK),
        "WHERE meeting_id = ?1 ORDER BY CASE kind WHEN 'mic' THEN 0 ELSE 1 END, id",
    );
    query(c, &sql, &[id_value(meeting)], track_row)
}

fn put_track(c: &Connection, t: &Track) -> Result<(), RepoError> {
    need_meeting(c, t.meeting_id)?;
    let v = synced_values(
        t.id,
        &t.meta,
        vec![
            id_value(t.meeting_id),
            enum_name(&t.kind)?,
            opt_text(t.device_name.as_deref()),
            enum_name(&t.clock_domain)?,
        ],
    )?;
    exec(c, &upsert_sql("track", &synced_cols(&TRACK), "id"), &v)?;
    Ok(())
}

// ---- segment ----

const SEGMENT: [&str; 12] = [
    "meeting_id",
    "track_id",
    "pass",
    "start_ms",
    "end_ms",
    "text",
    "words",
    "confidence",
    "speaker_id",
    "edited_wall",
    "edited_logical",
    "edited_node",
];

/// An optional HLC timestamp from the 3 columns starting at `i` (all NULL: `None`).
fn get_opt_hlc(r: &Row<'_>, i: usize) -> rusqlite::Result<Option<HlcTimestamp>> {
    let Some(wall_ms) = get_opt_u64(r, i)? else {
        return Ok(None);
    };
    Ok(Some(HlcTimestamp {
        wall_ms,
        logical: r.get(i + 1)?,
        node: get_device(r, i + 2)?,
    }))
}

fn opt_hlc_values(t: Option<HlcTimestamp>) -> Result<[Value; 3], RepoError> {
    Ok(match t {
        None => [Value::Null, Value::Null, Value::Null],
        Some(t) => [
            int(t.wall_ms)?,
            Value::Integer(i64::from(t.logical)),
            device_value(t.node),
        ],
    })
}

fn segment_row(r: &Row<'_>) -> rusqlite::Result<SegmentRow> {
    let words: Vec<Word> = get_json(r, REST + 6)?;
    let confidence: f64 = r.get(REST + 7)?;
    Ok(SegmentRow {
        meta: get_meta(r, 1)?,
        meeting_id: get_id(r, REST)?,
        pass: get_enum(r, REST + 2)?,
        segment: Segment {
            id: get_id(r, 0)?,
            track: get_id(r, REST + 1)?,
            start: MeetingTime::from_millis(get_u64(r, REST + 3)?),
            end: MeetingTime::from_millis(get_u64(r, REST + 4)?),
            text: r.get(REST + 5)?,
            words,
            // Stored from an f32, so the narrowing is exact.
            confidence: confidence as f32,
            speaker_id: get_opt_id(r, REST + 8)?,
        },
        text_edited_at: get_opt_hlc(r, REST + 9)?,
    })
}

fn segments_of(c: &Connection, meeting: EntityId) -> Result<Vec<SegmentRow>, RepoError> {
    let sql = select_sql(
        "segment",
        &synced_cols(&SEGMENT),
        "WHERE meeting_id = ?1 ORDER BY start_ms, id",
    );
    query(c, &sql, &[id_value(meeting)], segment_row)
}

/// The query of [`has_live_segment`]: `?1` the meeting id, `?2` the pass name.
pub(crate) const HAS_LIVE_SEGMENT_SQL: &str =
    "SELECT 1 FROM segment WHERE meeting_id = ?1 AND pass = ?2 AND deleted = 0 LIMIT 1";

/// Whether `meeting` has a live segment of `pass`: answered from the `segment_live` index
/// alone (migration 4, T12.30) instead of every segment and its words (the meeting list asks
/// this per meeting, T9.24a).
fn has_live_segment(
    c: &Connection,
    meeting: EntityId,
    pass: TranscriptPass,
) -> Result<bool, RepoError> {
    exists(
        c,
        HAS_LIVE_SEGMENT_SQL,
        &[id_value(meeting), enum_name(&pass)?],
    )
}

fn put_segment(c: &Connection, row: &SegmentRow) -> Result<(), RepoError> {
    let s = &row.segment;
    need_meeting(c, row.meeting_id)?;
    need_child(c, "track", row.meeting_id, s.track)?;
    if let Some(sp) = s.speaker_id {
        need_child(c, "speaker", row.meeting_id, sp)?;
    }
    let mut rest = vec![
        id_value(row.meeting_id),
        id_value(s.track),
        enum_name(&row.pass)?,
        int(s.start.as_millis())?,
        int(s.end.as_millis())?,
        text(&s.text),
        json(&s.words)?,
        Value::Real(f64::from(s.confidence)),
        opt_id(s.speaker_id),
    ];
    rest.extend(opt_hlc_values(row.text_edited_at)?);
    let v = synced_values(s.id, &row.meta, rest)?;
    exec(c, &upsert_sql("segment", &synced_cols(&SEGMENT), "id"), &v)?;
    Ok(())
}

// ---- speaker ----

const SPEAKER: [&str; 4] = ["meeting_id", "role", "display_name", "person_id"];

fn speaker_row(r: &Row<'_>) -> rusqlite::Result<Speaker> {
    Ok(Speaker {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        meeting_id: get_id(r, REST)?,
        role: get_enum(r, REST + 1)?,
        display_name: r.get(REST + 2)?,
        person_id: get_opt_id(r, REST + 3)?,
    })
}

fn speakers_of(c: &Connection, meeting: EntityId) -> Result<Vec<Speaker>, RepoError> {
    let sql = select_sql(
        "speaker",
        &synced_cols(&SPEAKER),
        "WHERE meeting_id = ?1 ORDER BY created_wall, created_logical, created_node, id",
    );
    query(c, &sql, &[id_value(meeting)], speaker_row)
}

fn put_speaker(c: &Connection, s: &Speaker) -> Result<(), RepoError> {
    need_meeting(c, s.meeting_id)?;
    let v = synced_values(
        s.id,
        &s.meta,
        vec![
            id_value(s.meeting_id),
            enum_name(&s.role)?,
            text(&s.display_name),
            opt_id(s.person_id),
        ],
    )?;
    exec(c, &upsert_sql("speaker", &synced_cols(&SPEAKER), "id"), &v)?;
    Ok(())
}

// ---- note ----

const NOTE: [&str; 3] = ["meeting_id", "at_ms", "text"];

fn note_row(r: &Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        meeting_id: get_id(r, REST)?,
        at: MeetingTime::from_millis(get_u64(r, REST + 1)?),
        text: r.get(REST + 2)?,
    })
}

fn notes_of(c: &Connection, meeting: EntityId) -> Result<Vec<Note>, RepoError> {
    let sql = select_sql(
        "note",
        &synced_cols(&NOTE),
        "WHERE meeting_id = ?1 ORDER BY at_ms, id",
    );
    query(c, &sql, &[id_value(meeting)], note_row)
}

fn put_note(c: &Connection, n: &Note) -> Result<(), RepoError> {
    need_meeting(c, n.meeting_id)?;
    let v = synced_values(
        n.id,
        &n.meta,
        vec![
            id_value(n.meeting_id),
            int(n.at.as_millis())?,
            text(&n.text),
        ],
    )?;
    exec(c, &upsert_sql("note", &synced_cols(&NOTE), "id"), &v)?;
    Ok(())
}

// ---- summary ----

const SUMMARY: [&str; 5] = ["meeting_id", "schema_version", "template", "model", "body"];

fn summary_row(r: &Row<'_>) -> rusqlite::Result<Summary> {
    let template: String = r.get(REST + 2)?;
    Ok(Summary {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        meeting_id: get_id(r, REST)?,
        schema_version: r.get(REST + 1)?,
        template: template
            .parse()
            .map_err(|_| conv_err(REST + 2, Type::Text, BadValue(template.clone())))?,
        model: r.get(REST + 3)?,
        summary: get_json(r, REST + 4)?,
    })
}

fn summaries_of(c: &Connection, meeting: EntityId) -> Result<Vec<Summary>, RepoError> {
    let sql = select_sql(
        "summary",
        &synced_cols(&SUMMARY),
        "WHERE meeting_id = ?1 ORDER BY created_wall, created_logical, created_node, id",
    );
    query(c, &sql, &[id_value(meeting)], summary_row)
}

fn put_summary(c: &Connection, s: &Summary) -> Result<(), RepoError> {
    need_meeting(c, s.meeting_id)?;
    let v = synced_values(
        s.id,
        &s.meta,
        vec![
            id_value(s.meeting_id),
            Value::Integer(i64::from(s.schema_version)),
            text(s.template.name()),
            opt_text(s.model.as_deref()),
            json(&s.summary)?,
        ],
    )?;
    exec(c, &upsert_sql("summary", &synced_cols(&SUMMARY), "id"), &v)?;
    Ok(())
}

// ---- folder ----

const FOLDER: [&str; 1] = ["name"];

fn folder_row(r: &Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        name: r.get(REST)?,
    })
}

fn get_folder(c: &Connection, id: EntityId) -> Result<Option<Folder>, RepoError> {
    let sql = select_sql("folder", &synced_cols(&FOLDER), "WHERE id = ?1");
    Ok(query(c, &sql, &[id_value(id)], folder_row)?.pop())
}

fn all_folders(c: &Connection) -> Result<Vec<Folder>, RepoError> {
    let sql = select_sql(
        "folder",
        &synced_cols(&FOLDER),
        "ORDER BY created_wall, created_logical, created_node, id",
    );
    query(c, &sql, &[], folder_row)
}

fn put_folder(c: &Connection, f: &Folder) -> Result<(), RepoError> {
    let v = synced_values(f.id, &f.meta, vec![text(&f.name)])?;
    exec(c, &upsert_sql("folder", &synced_cols(&FOLDER), "id"), &v)?;
    Ok(())
}

// ---- audio_file (NeverSync) ----

const AUDIO: [&str; 11] = [
    "id",
    "meeting_id",
    "track_id",
    "path",
    "format",
    "sample_rate_hz",
    "frames",
    "size_bytes",
    "pipeline_version",
    "aec_version",
    "created_unix_ms",
];

fn audio_row(r: &Row<'_>) -> rusqlite::Result<AudioFile> {
    Ok(AudioFile {
        id: get_id(r, 0)?,
        meeting_id: get_id(r, 1)?,
        track_id: get_id(r, 2)?,
        path: r.get(3)?,
        format: get_enum(r, 4)?,
        sample_rate_hz: r.get(5)?,
        frames: get_u64(r, 6)?,
        size_bytes: get_u64(r, 7)?,
        pipeline_version: r.get(8)?,
        aec_version: r.get(9)?,
        created_unix_ms: get_u64(r, 10)?,
    })
}

fn audio_of(c: &Connection, meeting: EntityId) -> Result<Vec<AudioFile>, RepoError> {
    let sql = select_sql("audio_file", &AUDIO, "WHERE meeting_id = ?1 ORDER BY id");
    query(c, &sql, &[id_value(meeting)], audio_row)
}

fn all_audio(c: &Connection) -> Result<Vec<AudioFile>, RepoError> {
    let sql = select_sql("audio_file", &AUDIO, "ORDER BY created_unix_ms, id");
    query(c, &sql, &[], audio_row)
}

fn put_audio(c: &Connection, f: &AudioFile) -> Result<(), RepoError> {
    need_meeting(c, f.meeting_id)?;
    need_child(c, "track", f.meeting_id, f.track_id)?;
    if exists(
        c,
        "SELECT 1 FROM audio_file WHERE path = ?1 AND id != ?2",
        &[text(&f.path), id_value(f.id)],
    )? {
        return Err(RepoError::Conflict {
            entity: "audio_file",
            field: "path",
        });
    }
    let v = vec![
        id_value(f.id),
        id_value(f.meeting_id),
        id_value(f.track_id),
        text(&f.path),
        enum_name(&f.format)?,
        Value::Integer(i64::from(f.sample_rate_hz)),
        int(f.frames)?,
        int(f.size_bytes)?,
        text(&f.pipeline_version),
        opt_text(f.aec_version.as_deref()),
        int(f.created_unix_ms)?,
    ];
    exec(c, &upsert_sql("audio_file", &AUDIO, "id"), &v)?;
    Ok(())
}

fn delete_audio(c: &Connection, id: EntityId) -> Result<bool, RepoError> {
    Ok(exec(c, "DELETE FROM audio_file WHERE id = ?1", &[id_value(id)])? > 0)
}

// ---- export_record (NeverSync) ----

const EXPORT: [&str; 5] = ["id", "meeting_id", "device_id", "path", "exported_unix_ms"];

fn export_row(r: &Row<'_>) -> rusqlite::Result<ExportRecord> {
    let path: String = r.get(3)?;
    Ok(ExportRecord {
        id: get_id(r, 0)?,
        meeting_id: get_id(r, 1)?,
        device_id: get_device(r, 2)?,
        path: VaultPath::new(&path).map_err(|e| conv_err(3, Type::Text, e))?,
        exported_unix_ms: get_u64(r, 4)?,
    })
}

fn get_export(
    c: &Connection,
    meeting: EntityId,
    device: DeviceId,
) -> Result<Option<ExportRecord>, RepoError> {
    let sql = select_sql(
        "export_record",
        &EXPORT,
        "WHERE meeting_id = ?1 AND device_id = ?2",
    );
    Ok(query(
        c,
        &sql,
        &[id_value(meeting), device_value(device)],
        export_row,
    )?
    .pop())
}

fn put_export(c: &Connection, r: &ExportRecord) -> Result<(), RepoError> {
    need_meeting(c, r.meeting_id)?;
    let v = vec![
        id_value(r.id),
        id_value(r.meeting_id),
        device_value(r.device_id),
        text(r.path.as_str()),
        int(r.exported_unix_ms)?,
    ];
    exec(
        c,
        &upsert_sql("export_record", &EXPORT, "meeting_id, device_id"),
        &v,
    )?;
    Ok(())
}

// ---- person and voiceprint ----

const PERSON: [&str; 3] = ["display_name", "is_me", "merged_into"];
const VOICEPRINT: [&str; 5] = ["person_id", "model", "sum", "count", "speech_ms"];

fn person_row(r: &Row<'_>) -> rusqlite::Result<Person> {
    Ok(Person {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        display_name: r.get(REST)?,
        is_me: r.get(REST + 1)?,
        merged_into: get_opt_id(r, REST + 2)?,
    })
}

fn voiceprint_row(r: &Row<'_>) -> rusqlite::Result<Voiceprint> {
    let model: String = r.get(REST + 1)?;
    let bytes: Vec<u8> = r.get(REST + 2)?;
    let (chunks, rest) = bytes.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(conv_err(
            REST + 2,
            Type::Blob,
            BadValue(format!("voiceprint vector of {} bytes", bytes.len())),
        ));
    }
    Ok(Voiceprint {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        person_id: get_id(r, REST)?,
        model: EmbeddingModelId::new(model),
        sum: chunks.iter().map(|b| f32::from_le_bytes(*b)).collect(),
        count: r.get(REST + 3)?,
        speech_ms: get_u64(r, REST + 4)?,
    })
}

fn vp_err(e: RepoError) -> VoiceprintError {
    VoiceprintError::Repo(e.to_string())
}

fn get_person(c: &Connection, id: EntityId) -> Result<Option<Person>, RepoError> {
    let sql = select_sql("person", &synced_cols(&PERSON), "WHERE id = ?1");
    Ok(query(c, &sql, &[id_value(id)], person_row)?.pop())
}

fn all_persons(c: &Connection) -> Result<Vec<Person>, RepoError> {
    let sql = select_sql("person", &synced_cols(&PERSON), "ORDER BY id");
    query(c, &sql, &[], person_row)
}

fn put_person(c: &Connection, p: &Person) -> Result<(), RepoError> {
    let v = synced_values(
        p.id,
        &p.meta,
        vec![
            text(&p.display_name),
            Value::Integer(i64::from(p.is_me)),
            opt_id(p.merged_into),
        ],
    )?;
    exec(c, &upsert_sql("person", &synced_cols(&PERSON), "id"), &v)?;
    Ok(())
}

fn voiceprints_where(c: &Connection, filter: &str, p: Value) -> Result<Vec<Voiceprint>, RepoError> {
    let sql = select_sql(
        "voiceprint",
        &synced_cols(&VOICEPRINT),
        &format!("WHERE {filter} = ?1 ORDER BY id"),
    );
    query(c, &sql, &[p], voiceprint_row)
}

fn put_voiceprint(c: &Connection, v: &Voiceprint) -> Result<(), RepoError> {
    let bytes: Vec<u8> = v.sum.iter().flat_map(|x| x.to_le_bytes()).collect();
    let vals = synced_values(
        v.id,
        &v.meta,
        vec![
            id_value(v.person_id),
            text(v.model.as_str()),
            Value::Blob(bytes),
            Value::Integer(i64::from(v.count)),
            int(v.speech_ms)?,
        ],
    )?;
    exec(
        c,
        &upsert_sql("voiceprint", &synced_cols(&VOICEPRINT), "id"),
        &vals,
    )?;
    Ok(())
}

/// Implements the repository traits for a type with a `conn` field that derefs to a
/// [`Connection`].
macro_rules! impl_repos {
    ($ty:ty) => {
        impl MeetingRepo for $ty {
            fn meeting(&self, id: EntityId) -> Result<Option<Meeting>, RepoError> {
                get_meeting(&self.conn, id)
            }
            fn meetings(&self) -> Result<Vec<Meeting>, RepoError> {
                all_meetings(&self.conn)
            }
            fn put_meeting(&mut self, meeting: Meeting) -> Result<(), RepoError> {
                put_meeting(&self.conn, &meeting)
            }
            fn tracks(&self, meeting: EntityId) -> Result<Vec<Track>, RepoError> {
                tracks_of(&self.conn, meeting)
            }
            fn put_track(&mut self, track: Track) -> Result<(), RepoError> {
                put_track(&self.conn, &track)
            }
            fn segments(&self, meeting: EntityId) -> Result<Vec<SegmentRow>, RepoError> {
                segments_of(&self.conn, meeting)
            }
            fn has_live_segment(
                &self,
                meeting: EntityId,
                pass: TranscriptPass,
            ) -> Result<bool, RepoError> {
                has_live_segment(&self.conn, meeting, pass)
            }
            fn put_segment(&mut self, segment: SegmentRow) -> Result<(), RepoError> {
                put_segment(&self.conn, &segment)
            }
            fn speakers(&self, meeting: EntityId) -> Result<Vec<Speaker>, RepoError> {
                speakers_of(&self.conn, meeting)
            }
            fn put_speaker(&mut self, speaker: Speaker) -> Result<(), RepoError> {
                put_speaker(&self.conn, &speaker)
            }
            fn notes(&self, meeting: EntityId) -> Result<Vec<Note>, RepoError> {
                notes_of(&self.conn, meeting)
            }
            fn put_note(&mut self, note: Note) -> Result<(), RepoError> {
                put_note(&self.conn, &note)
            }
            fn summaries(&self, meeting: EntityId) -> Result<Vec<Summary>, RepoError> {
                summaries_of(&self.conn, meeting)
            }
            fn put_summary(&mut self, summary: Summary) -> Result<(), RepoError> {
                put_summary(&self.conn, &summary)
            }
        }

        impl FolderRepo for $ty {
            fn folder(&self, id: EntityId) -> Result<Option<Folder>, RepoError> {
                get_folder(&self.conn, id)
            }
            fn folders(&self) -> Result<Vec<Folder>, RepoError> {
                all_folders(&self.conn)
            }
            fn put_folder(&mut self, folder: Folder) -> Result<(), RepoError> {
                put_folder(&self.conn, &folder)
            }
        }

        impl AudioFileRepo for $ty {
            fn audio_files(&self, meeting: EntityId) -> Result<Vec<AudioFile>, RepoError> {
                audio_of(&self.conn, meeting)
            }
            fn all_audio_files(&self) -> Result<Vec<AudioFile>, RepoError> {
                all_audio(&self.conn)
            }
            fn put_audio_file(&mut self, file: AudioFile) -> Result<(), RepoError> {
                put_audio(&self.conn, &file)
            }
            fn delete_audio_file(&mut self, id: EntityId) -> Result<bool, RepoError> {
                delete_audio(&self.conn, id)
            }
        }

        impl ExportRepo for $ty {
            fn export_record(
                &self,
                meeting: EntityId,
                device: DeviceId,
            ) -> Result<Option<ExportRecord>, RepoError> {
                get_export(&self.conn, meeting, device)
            }
            fn put_export_record(&mut self, record: ExportRecord) -> Result<(), RepoError> {
                put_export(&self.conn, &record)
            }
        }

        impl VoiceprintRepo for $ty {
            fn person(&self, id: EntityId) -> Result<Option<Person>, VoiceprintError> {
                get_person(&self.conn, id).map_err(vp_err)
            }
            fn persons(&self) -> Result<Vec<Person>, VoiceprintError> {
                all_persons(&self.conn).map_err(vp_err)
            }
            fn put_person(&mut self, person: Person) -> Result<(), VoiceprintError> {
                put_person(&self.conn, &person).map_err(vp_err)
            }
            fn voiceprints(
                &self,
                model: &EmbeddingModelId,
            ) -> Result<Vec<Voiceprint>, VoiceprintError> {
                voiceprints_where(&self.conn, "model", text(model.as_str())).map_err(vp_err)
            }
            fn voiceprints_of(&self, person: EntityId) -> Result<Vec<Voiceprint>, VoiceprintError> {
                voiceprints_where(&self.conn, "person_id", id_value(person)).map_err(vp_err)
            }
            fn put_voiceprint(&mut self, voiceprint: Voiceprint) -> Result<(), VoiceprintError> {
                put_voiceprint(&self.conn, &voiceprint).map_err(vp_err)
            }
        }
    };
}

impl_repos!(Store);
impl_repos!(StoreTx<'_>);
