//! The core repository traits over SQLite.

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::diar::EmbeddingModelId;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::{
    AudioFile, AudioFileRepo, DictionaryRepo, DictionaryWord, ExportRecord, ExportRepo, Folder,
    FolderRepo, Meeting, MeetingRepo, Note, RepoError, RequestState, SegmentRow, ShownLine,
    ShownLines, ShownSummary, Speaker, Summary, SummaryRequest, SummaryRequestRepo, Track,
    TranscriptPass, VaultPath,
};
use earshot_core::voiceprint::{
    Person, SampleRecord, Voiceprint, VoiceprintError, VoiceprintRepo, VoiceprintSample,
};
use rusqlite::types::{Type, Value};
use rusqlite::{Connection, Row, params_from_iter};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::db::{ReadOnlyStore, Store, StoreTx};

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

const MEETING: [&str; 8] = [
    "title",
    "started_unix_ms",
    "utc_offset_minutes",
    "duration_ms",
    "tags",
    "template",
    "folder_id",
    "summary_kept_local",
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
        summary_kept_local: r.get::<_, i64>(REST + 7)? != 0,
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
            Value::Integer(i64::from(m.summary_kept_local)),
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

const SEGMENT: [&str; 13] = [
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
    "pass_run",
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
        pass_run: get_opt_id(r, REST + 12)?,
    })
}

pub(crate) fn segments_of(c: &Connection, meeting: EntityId) -> Result<Vec<SegmentRow>, RepoError> {
    let sql = select_sql(
        "segment",
        &synced_cols(&SEGMENT),
        "WHERE meeting_id = ?1 ORDER BY start_ms, id",
    );
    query(c, &sql, &[id_value(meeting)], segment_row)
}

/// The segment rows of `meeting` in [`segments_of`]'s order, split into those that read and
/// the SQLite rowids of those whose stored values do not convert (damaged rows, T14.26).
pub(crate) fn segments_or_damaged(
    c: &Connection,
    meeting: EntityId,
) -> Result<(Vec<SegmentRow>, Vec<i64>), RepoError> {
    split_rows(
        c,
        "segment",
        synced_cols(&SEGMENT),
        "WHERE meeting_id = ?1 ORDER BY start_ms, id",
        &[id_value(meeting)],
        segment_row,
    )
}

/// The rows of `table` that `tail` selects with `params`, split into those `decode` reads and
/// the SQLite rowids of those whose stored values do not convert (T14.26). Any other error
/// fails the read.
fn split_rows<T>(
    c: &Connection,
    table: &str,
    mut cols: Vec<&'static str>,
    tail: &str,
    params: &[Value],
    decode: impl Fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<(Vec<T>, Vec<i64>), RepoError> {
    let rowid_at = cols.len();
    cols.push("rowid");
    let sql = select_sql(table, &cols, tail);
    let rows = query(c, &sql, params, |r| {
        Ok((decode(r), r.get::<_, i64>(rowid_at)?))
    })?;
    let (mut readable, mut damaged) = (Vec::new(), Vec::new());
    for (row, rowid) in rows {
        match row {
            Ok(row) => readable.push(row),
            Err(e) if is_unreadable(&e) => damaged.push(rowid),
            Err(e) => return Err(storage(e)),
        }
    }
    Ok((readable, damaged))
}

/// What one meeting's deletion reads of each table (T14.26): the rows that read, and the
/// rowids of the damaged ones. Same orders as the plain readers.
pub(crate) mod for_deletion {
    use super::*;
    use earshot_core::capture::TrackId;
    use earshot_core::diar::SpeakerRole;
    use earshot_core::model::ClockDomain;
    use earshot_core::prompt::Template;

    /// `col` kept when it is an integer in `lo..=hi`, else `fallback` (an SQL expression).
    fn int_in(col: &str, lo: i64, hi: i64, fallback: &str) -> String {
        format!(
            "{col} = CASE WHEN typeof({col}) = 'integer' AND {col} BETWEEN {lo} AND {hi} \
             THEN {col} ELSE {fallback} END"
        )
    }

    /// `col` kept when it is a 16-byte blob (an id), else `fallback`.
    fn id_or(col: &str, fallback: &str) -> String {
        format!(
            "{col} = CASE WHEN typeof({col}) = 'blob' AND length({col}) = 16 \
             THEN {col} ELSE {fallback} END"
        )
    }

    /// `col` kept when it is one of `names` (a unit enum's stored names), else the first.
    fn one_of<T: Serialize>(col: &str, names: &[T]) -> Result<String, RepoError> {
        let names = names
            .iter()
            .map(|v| match enum_name(v)? {
                Value::Text(s) => Ok(format!("'{s}'")),
                _ => Err(storage("not a unit enum")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(format!(
            "{col} = CASE WHEN {col} IN ({}) THEN {col} ELSE {} END",
            names.join(", "),
            names[0]
        ))
    }

    /// The canonical tombstone of a damaged row of `table` (T14.26), run after its content was
    /// erased and its `updated_*` stamped: every value that does not convert is replaced by
    /// the one a fresh tombstone of the table would hold (created stamps and origin from the
    /// new stamp), so the row reads and syncs. Ids and parent links are kept (a row whose
    /// `id` or `meeting_id` does not read cannot be helped). Valid values are left as they are.
    pub(crate) fn canonicalise(c: &Connection, table: &str, rowid: i64) -> Result<(), RepoError> {
        const U32: i64 = u32::MAX as i64;
        let mut sets = vec![
            int_in("created_wall", 0, i64::MAX, "updated_wall"),
            int_in("created_logical", 0, U32, "updated_logical"),
            id_or("created_node", "updated_node"),
            id_or("origin_device", "updated_node"),
        ];
        let ms = |col: &str| int_in(col, 0, i64::MAX, "0");
        let opt_ms = |col: &str| {
            format!(
                "{col} = CASE WHEN {col} IS NULL OR (typeof({col}) = 'integer' AND {col} >= 0) \
                 THEN {col} ELSE NULL END"
            )
        };
        match table {
            "meeting" => {
                sets.push(ms("started_unix_ms"));
                sets.push(int_in(
                    "utc_offset_minutes",
                    i64::from(i32::MIN),
                    i64::from(i32::MAX),
                    "0",
                ));
                sets.push(opt_ms("duration_ms"));
                let names: Vec<&str> = Template::ALL.iter().map(|t| t.name()).collect();
                sets.push(format!(
                    "template = CASE WHEN template IN ({}) THEN template ELSE '{}' END",
                    names
                        .iter()
                        .map(|n| format!("'{n}'"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Template::General.name()
                ));
                sets.push(int_in("summary_kept_local", 0, 1, "0"));
            }
            "track" => {
                sets.push(one_of("kind", &[TrackId::Mic, TrackId::Reference])?);
                sets.push(one_of(
                    "clock_domain",
                    &[
                        ClockDomain::Qpc,
                        ClockDomain::AndroidMonotonic,
                        ClockDomain::Replay,
                    ],
                )?);
            }
            "speaker" => sets.push(one_of(
                "role",
                &[
                    SpeakerRole::Remote,
                    SpeakerRole::Me,
                    SpeakerRole::Local,
                    SpeakerRole::Room,
                ],
            )?),
            "segment" => {
                sets.push(one_of(
                    "pass",
                    &[TranscriptPass::Final, TranscriptPass::Live],
                )?);
                sets.push(ms("start_ms"));
                sets.push(ms("end_ms"));
                // The edit mark reads only whole: any part that does not read clears it.
                let edit_ok = format!(
                    "(edited_wall IS NULL OR (typeof(edited_wall) = 'integer' AND edited_wall >= 0 \
                     AND typeof(edited_logical) = 'integer' AND edited_logical BETWEEN 0 AND {U32} \
                     AND typeof(edited_node) = 'blob' AND length(edited_node) = 16))"
                );
                for col in ["edited_wall", "edited_logical", "edited_node"] {
                    sets.push(format!(
                        "{col} = CASE WHEN {edit_ok} THEN {col} ELSE NULL END"
                    ));
                }
                sets.push(id_or("pass_run", "NULL"));
            }
            "note" => sets.push(ms("at_ms")),
            "summary" => {
                sets.push(int_in("schema_version", 0, U32, "1"));
                let names: Vec<String> = Template::ALL
                    .iter()
                    .map(|t| format!("'{}'", t.name()))
                    .collect();
                sets.push(format!(
                    "template = CASE WHEN template IN ({}) THEN template ELSE '{}' END",
                    names.join(", "),
                    Template::General.name()
                ));
            }
            other => return Err(storage(format!("no canonical tombstone for {other}"))),
        }
        exec(
            c,
            &format!("UPDATE {table} SET {} WHERE rowid = ?1", sets.join(", ")),
            &[Value::Integer(rowid)],
        )?;
        Ok(())
    }

    /// Whether the row `rowid` of `table` reads.
    pub(crate) fn reads(c: &Connection, table: &str, rowid: i64) -> Result<bool, RepoError> {
        let p = [Value::Integer(rowid)];
        let tail = "WHERE rowid = ?1";
        let bad = match table {
            "meeting" => split_rows(c, table, synced_cols(&MEETING), tail, &p, meeting_row)?.1,
            "track" => split_rows(c, table, synced_cols(&TRACK), tail, &p, track_row)?.1,
            "speaker" => split_rows(c, table, synced_cols(&SPEAKER), tail, &p, speaker_row)?.1,
            "segment" => split_rows(c, table, synced_cols(&SEGMENT), tail, &p, segment_row)?.1,
            "note" => split_rows(c, table, synced_cols(&NOTE), tail, &p, note_row)?.1,
            "summary" => split_rows(c, table, synced_cols(&SUMMARY), tail, &p, summary_row)?.1,
            other => return Err(storage(format!("no reader for {other}"))),
        };
        Ok(bad.is_empty())
    }

    /// The meeting row: `None` when absent, else the row or the rowid of a damaged one.
    pub(crate) fn meeting(
        c: &Connection,
        id: EntityId,
    ) -> Result<Option<Result<Meeting, i64>>, RepoError> {
        let (mut ok, mut bad) = split_rows(
            c,
            "meeting",
            synced_cols(&MEETING),
            "WHERE id = ?1",
            &[id_value(id)],
            meeting_row,
        )?;
        Ok(ok.pop().map(Ok).or_else(|| bad.pop().map(Err)))
    }

    pub(crate) fn segments(
        c: &Connection,
        m: EntityId,
    ) -> Result<(Vec<SegmentRow>, Vec<i64>), RepoError> {
        super::segments_or_damaged(c, m)
    }

    pub(crate) fn tracks(c: &Connection, m: EntityId) -> Result<(Vec<Track>, Vec<i64>), RepoError> {
        let tail = "WHERE meeting_id = ?1 ORDER BY CASE kind WHEN 'mic' THEN 0 ELSE 1 END, id";
        split_rows(
            c,
            "track",
            synced_cols(&TRACK),
            tail,
            &[id_value(m)],
            track_row,
        )
    }

    pub(crate) fn speakers(
        c: &Connection,
        m: EntityId,
    ) -> Result<(Vec<Speaker>, Vec<i64>), RepoError> {
        let tail = "WHERE meeting_id = ?1 ORDER BY created_wall, created_logical, created_node, id";
        split_rows(
            c,
            "speaker",
            synced_cols(&SPEAKER),
            tail,
            &[id_value(m)],
            speaker_row,
        )
    }

    pub(crate) fn notes(c: &Connection, m: EntityId) -> Result<(Vec<Note>, Vec<i64>), RepoError> {
        let tail = "WHERE meeting_id = ?1 ORDER BY at_ms, id";
        split_rows(
            c,
            "note",
            synced_cols(&NOTE),
            tail,
            &[id_value(m)],
            note_row,
        )
    }

    pub(crate) fn summaries(
        c: &Connection,
        m: EntityId,
    ) -> Result<(Vec<Summary>, Vec<i64>), RepoError> {
        let tail = "WHERE meeting_id = ?1 ORDER BY created_wall, created_logical, created_node, id";
        split_rows(
            c,
            "summary",
            synced_cols(&SUMMARY),
            tail,
            &[id_value(m)],
            summary_row,
        )
    }

    pub(crate) fn audio(
        c: &Connection,
        m: EntityId,
    ) -> Result<(Vec<AudioFile>, Vec<i64>), RepoError> {
        let tail = "WHERE meeting_id = ?1 ORDER BY id";
        split_rows(
            c,
            "audio_file",
            AUDIO.to_vec(),
            tail,
            &[id_value(m)],
            audio_row,
        )
    }
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

/// The query of [`shown_lines`]: `?1` the meeting id, `?2` the pass name. Only id, start and
/// text: never the words, which a long meeting holds most of its bytes in.
pub(crate) const SHOWN_LINES_SQL: &str = "SELECT id, start_ms, text FROM segment \
     WHERE meeting_id = ?1 AND pass = ?2 AND deleted = 0 ORDER BY start_ms, id";

/// The transcript `meeting` shows, without words (T14.24, the rule of
/// `earshot_core::model::shown_lines_of`): the pass is chosen from the `segment_live` index,
/// then that pass's live rows are read one by one, and a row whose values do not convert is
/// left out and counted rather than failing the read.
fn shown_lines(c: &Connection, meeting: EntityId) -> Result<ShownLines, RepoError> {
    let mut out = ShownLines::default();
    for pass in [TranscriptPass::Final, TranscriptPass::Live] {
        if !has_live_segment(c, meeting, pass)? {
            continue;
        }
        let params = [id_value(meeting), enum_name(&pass)?];
        let mut st = c.prepare_cached(SHOWN_LINES_SQL).map_err(storage)?;
        let rows = st
            .query_map(params_from_iter(params.iter()), |r| {
                Ok(ShownLine {
                    id: get_id(r, 0)?,
                    start: MeetingTime::from_millis(get_u64(r, 1)?),
                    text: r.get(2)?,
                })
            })
            .map_err(storage)?;
        for row in rows {
            match row {
                Ok(line) => out.lines.push(line),
                Err(e) if is_unreadable(&e) => out.unreadable += 1,
                Err(e) => return Err(storage(e)),
            }
        }
        out.pass = Some(pass);
        // SQLite orders ids as bytes, as `EntityId` does; sorting again keeps the rule's order
        // whatever the query plan.
        out.lines.sort_by_key(|l| (l.start, l.id));
        return Ok(out);
    }
    Ok(out)
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
    rest.push(opt_id(row.pass_run));
    let v = synced_values(s.id, &row.meta, rest)?;
    exec(c, &upsert_sql("segment", &synced_cols(&SEGMENT), "id"), &v)?;
    Ok(())
}

// ---- speaker ----

const SPEAKER: [&str; 5] = ["meeting_id", "role", "display_name", "person_id", "ignored"];

fn speaker_row(r: &Row<'_>) -> rusqlite::Result<Speaker> {
    Ok(Speaker {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        meeting_id: get_id(r, REST)?,
        role: get_enum(r, REST + 1)?,
        display_name: r.get(REST + 2)?,
        person_id: get_opt_id(r, REST + 3)?,
        ignored: r.get::<_, i64>(REST + 4)? != 0,
    })
}

pub(crate) fn speakers_of(c: &Connection, meeting: EntityId) -> Result<Vec<Speaker>, RepoError> {
    let sql = select_sql(
        "speaker",
        &synced_cols(&SPEAKER),
        "WHERE meeting_id = ?1 ORDER BY created_wall, created_logical, created_node, id",
    );
    query(c, &sql, &[id_value(meeting)], speaker_row)
}

/// Every (person, meeting) pair where a live speaker row of a live meeting is linked to the
/// person, each pair once, in one query (T12.80: how many meetings each person is in).
pub(crate) fn person_meetings(c: &Connection) -> Result<Vec<(EntityId, EntityId)>, RepoError> {
    query(
        c,
        "SELECT DISTINCT s.person_id, s.meeting_id FROM speaker s \
         JOIN meeting m ON m.id = s.meeting_id \
         WHERE s.deleted = 0 AND m.deleted = 0 AND s.person_id IS NOT NULL",
        &[],
        |r| Ok((get_id(r, 0)?, get_id(r, 1)?)),
    )
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
            Value::Integer(i64::from(s.ignored)),
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

pub(crate) fn summaries_of(c: &Connection, meeting: EntityId) -> Result<Vec<Summary>, RepoError> {
    let sql = select_sql(
        "summary",
        &synced_cols(&SUMMARY),
        "WHERE meeting_id = ?1 ORDER BY created_wall, created_logical, created_node, id",
    );
    query(c, &sql, &[id_value(meeting)], summary_row)
}

/// The query of [`current_summary_id`]: `?1` the meeting id. The order is
/// `earshot_core::model::current_summary`'s: the live row updated last, then the later in
/// creation order (`summaries_of`'s order), so the contract pins one to the other.
pub(crate) const CURRENT_SUMMARY_SQL: &str = "SELECT id FROM summary \
     WHERE meeting_id = ?1 AND deleted = 0 \
     ORDER BY updated_wall DESC, updated_logical DESC, updated_node DESC, \
     created_wall DESC, created_logical DESC, created_node DESC, id DESC LIMIT 1";

/// The id of `meeting`'s current summary, read without any summary body (the meeting list asks
/// this per meeting, T12.76).
pub(crate) fn current_summary_id(
    c: &Connection,
    meeting: EntityId,
) -> Result<Option<EntityId>, RepoError> {
    Ok(query(c, CURRENT_SUMMARY_SQL, &[id_value(meeting)], |r| {
        get_id(r, 0)
    })?
    .into_iter()
    .next())
}

/// [`CURRENT_SUMMARY_SQL`] giving the row's rowid, which reads whatever the row's values.
const CURRENT_SUMMARY_ROWID_SQL: &str = "SELECT rowid FROM summary \
     WHERE meeting_id = ?1 AND deleted = 0 \
     ORDER BY updated_wall DESC, updated_logical DESC, updated_node DESC, \
     created_wall DESC, created_logical DESC, created_node DESC, id DESC LIMIT 1";

/// The body of `meeting`'s current summary (T14.24): that one row, chosen as
/// [`current_summary_id`] chooses it; a current row that does not read gives no body and
/// `unreadable` 1, never an older summary.
fn shown_summary(c: &Connection, meeting: EntityId) -> Result<ShownSummary, RepoError> {
    let Some(rowid) = query(c, CURRENT_SUMMARY_ROWID_SQL, &[id_value(meeting)], |r| {
        r.get::<_, i64>(0)
    })?
    .pop() else {
        return Ok(ShownSummary::default());
    };
    let (mut ok, bad) = split_rows(
        c,
        "summary",
        synced_cols(&SUMMARY),
        "WHERE rowid = ?1",
        &[Value::Integer(rowid)],
        summary_row,
    )?;
    Ok(ShownSummary {
        summary: ok.pop().map(|s| s.summary),
        unreadable: bad.len(),
    })
}

pub(crate) fn put_summary(c: &Connection, s: &Summary) -> Result<(), RepoError> {
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

// ---- dictionary_word (T11.3e) ----

const DICTIONARY_WORD: [&str; 1] = ["word"];

fn dictionary_word_row(r: &Row<'_>) -> rusqlite::Result<DictionaryWord> {
    Ok(DictionaryWord {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        word: r.get(REST)?,
    })
}

fn get_dictionary_word(c: &Connection, id: EntityId) -> Result<Option<DictionaryWord>, RepoError> {
    let sql = select_sql(
        "dictionary_word",
        &synced_cols(&DICTIONARY_WORD),
        "WHERE id = ?1",
    );
    Ok(query(c, &sql, &[id_value(id)], dictionary_word_row)?.pop())
}

fn all_dictionary_words(c: &Connection) -> Result<Vec<DictionaryWord>, RepoError> {
    let sql = select_sql(
        "dictionary_word",
        &synced_cols(&DICTIONARY_WORD),
        "ORDER BY created_wall, created_logical, created_node, id",
    );
    query(c, &sql, &[], dictionary_word_row)
}

fn put_dictionary_word(c: &Connection, w: &DictionaryWord) -> Result<(), RepoError> {
    let v = synced_values(w.id, &w.meta, vec![text(&w.word)])?;
    exec(
        c,
        &upsert_sql("dictionary_word", &synced_cols(&DICTIONARY_WORD), "id"),
        &v,
    )?;
    Ok(())
}

// ---- summary_request (T11.3da) ----

const SUMMARY_REQUEST: [&str; 6] = [
    "meeting_id",
    "target_device",
    "asking_device",
    "segment_count",
    "state",
    "reason",
];

fn summary_request_row(r: &Row<'_>) -> rusqlite::Result<SummaryRequest> {
    let state: String = r.get(REST + 4)?;
    Ok(SummaryRequest {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        meeting_id: get_id(r, REST)?,
        target_device: get_device(r, REST + 1)?,
        asking_device: get_device(r, REST + 2)?,
        segment_count: get_u64(r, REST + 3)?,
        state: RequestState::from_name(&state)
            .ok_or_else(|| conv_err(REST + 4, Type::Text, BadValue(state.clone())))?,
        reason: r.get(REST + 5)?,
    })
}

fn get_summary_request(c: &Connection, id: EntityId) -> Result<Option<SummaryRequest>, RepoError> {
    let sql = select_sql(
        "summary_request",
        &synced_cols(&SUMMARY_REQUEST),
        "WHERE id = ?1",
    );
    Ok(query(c, &sql, &[id_value(id)], summary_request_row)?.pop())
}

fn all_summary_requests(c: &Connection) -> Result<Vec<SummaryRequest>, RepoError> {
    let sql = select_sql(
        "summary_request",
        &synced_cols(&SUMMARY_REQUEST),
        "ORDER BY created_wall, created_logical, created_node, id",
    );
    query(c, &sql, &[], summary_request_row)
}

fn put_summary_request(c: &Connection, s: &SummaryRequest) -> Result<(), RepoError> {
    need_meeting(c, s.meeting_id)?;
    let v = synced_values(
        s.id,
        &s.meta,
        vec![
            id_value(s.meeting_id),
            device_value(s.target_device),
            device_value(s.asking_device),
            int(s.segment_count)?,
            text(s.state.name()),
            text(&s.reason),
        ],
    )?;
    exec(
        c,
        &upsert_sql("summary_request", &synced_cols(&SUMMARY_REQUEST), "id"),
        &v,
    )?;
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
    Ok(Voiceprint::from_stored(
        get_id(r, 0)?,
        get_meta(r, 1)?,
        get_id(r, REST)?,
        EmbeddingModelId::new(model),
        chunks.iter().map(|b| f32::from_le_bytes(*b)).collect(),
        r.get(REST + 3)?,
        get_u64(r, REST + 4)?,
    ))
}

fn vp_err(e: RepoError) -> VoiceprintError {
    VoiceprintError::Repo(e.to_string())
}

fn get_person(c: &Connection, id: EntityId) -> Result<Option<Person>, RepoError> {
    let sql = select_sql("person", &synced_cols(&PERSON), "WHERE id = ?1");
    Ok(query(c, &sql, &[id_value(id)], person_row)?.pop())
}

pub(crate) fn all_persons(c: &Connection) -> Result<Vec<Person>, RepoError> {
    let sql = select_sql("person", &synced_cols(&PERSON), "ORDER BY id");
    query(c, &sql, &[], person_row)
}

pub(crate) fn put_person(c: &Connection, p: &Person) -> Result<(), RepoError> {
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
    let bytes: Vec<u8> = v.base_sum().iter().flat_map(|x| x.to_le_bytes()).collect();
    let vals = synced_values(
        v.id(),
        v.meta(),
        vec![
            id_value(v.person_id()),
            text(v.model().as_str()),
            Value::Blob(bytes),
            Value::Integer(i64::from(v.base_count())),
            int(v.base_speech_ms())?,
        ],
    )?;
    exec(
        c,
        &upsert_sql("voiceprint", &synced_cols(&VOICEPRINT), "id"),
        &vals,
    )?;
    Ok(())
}

// ---- voiceprint_sample (T12.77b2) ----

const VOICEPRINT_SAMPLE: [&str; 7] = [
    "voiceprint_id",
    "person_id",
    "model",
    "vector",
    "speech_ms",
    "meeting_id",
    "speaker_id",
];

/// A stored sample through the one check (`VoiceprintSample::from_stored`): a live row that
/// fails it is an error, never skipped.
fn voiceprint_sample_row(r: &Row<'_>) -> rusqlite::Result<VoiceprintSample> {
    let bytes: Vec<u8> = r.get(REST + 3)?;
    let (chunks, rest) = bytes.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(conv_err(
            REST + 3,
            Type::Blob,
            BadValue(format!("voice sample vector of {} bytes", bytes.len())),
        ));
    }
    let speech_ms = match r.get::<_, Option<i64>>(REST + 4)? {
        None => None,
        Some(_) => Some(get_u64(r, REST + 4)?),
    };
    let record = SampleRecord {
        id: get_id(r, 0)?,
        meta: get_meta(r, 1)?,
        voiceprint_id: get_opt_id(r, REST)?,
        person_id: get_opt_id(r, REST + 1)?,
        model: r
            .get::<_, Option<String>>(REST + 2)?
            .map(EmbeddingModelId::new),
        vector: chunks.iter().map(|b| f32::from_le_bytes(*b)).collect(),
        speech_ms,
        meeting_id: get_opt_id(r, REST + 5)?,
        speaker_id: get_opt_id(r, REST + 6)?,
    };
    VoiceprintSample::from_stored(record)
        .map_err(|e| conv_err(REST + 3, Type::Blob, BadValue(e.to_string())))
}

/// Live samples matching `filter` (a `WHERE` condition on `?1`, `?2`), in creation order.
fn samples_where(
    c: &Connection,
    filter: &str,
    p: &[Value],
) -> Result<Vec<VoiceprintSample>, RepoError> {
    let tail = format!(
        "WHERE deleted = 0 AND {filter} ORDER BY created_wall, created_logical, created_node, id"
    );
    let sql = select_sql("voiceprint_sample", &synced_cols(&VOICEPRINT_SAMPLE), &tail);
    query(c, &sql, p, voiceprint_sample_row)
}

fn put_voiceprint_sample(c: &Connection, s: &VoiceprintSample) -> Result<(), RepoError> {
    let r = SampleRecord::from(s.clone());
    let bytes: Vec<u8> = r.vector.iter().flat_map(|x| x.to_le_bytes()).collect();
    let vals = synced_values(
        r.id,
        &r.meta,
        vec![
            opt_id(r.voiceprint_id),
            opt_id(r.person_id),
            opt_text(r.model.as_ref().map(EmbeddingModelId::as_str)),
            Value::Blob(bytes),
            r.speech_ms.map_or(Ok(Value::Null), int)?,
            opt_id(r.meeting_id),
            opt_id(r.speaker_id),
        ],
    )?;
    exec(
        c,
        &upsert_sql("voiceprint_sample", &synced_cols(&VOICEPRINT_SAMPLE), "id"),
        &vals,
    )?;
    Ok(())
}

/// The sample reads of [`VoiceprintRepo`], shared by the store, its transaction and the
/// read-only store.
mod sample_reads {
    use super::*;

    pub(super) fn samples(
        c: &Connection,
        model: &EmbeddingModelId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        samples_where(c, "model = ?1", &[text(model.as_str())]).map_err(vp_err)
    }
    pub(super) fn samples_of(
        c: &Connection,
        print: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        samples_where(c, "voiceprint_id = ?1", &[id_value(print)]).map_err(vp_err)
    }
    pub(super) fn samples_of_person(
        c: &Connection,
        person: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        samples_where(c, "person_id = ?1", &[id_value(person)]).map_err(vp_err)
    }
    pub(super) fn samples_from(
        c: &Connection,
        meeting: EntityId,
        speaker: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        samples_where(
            c,
            "meeting_id = ?1 AND speaker_id = ?2",
            &[id_value(meeting), id_value(speaker)],
        )
        .map_err(vp_err)
    }
    pub(super) fn sample(
        c: &Connection,
        id: EntityId,
    ) -> Result<Option<VoiceprintSample>, VoiceprintError> {
        let sql = select_sql(
            "voiceprint_sample",
            &synced_cols(&VOICEPRINT_SAMPLE),
            "WHERE id = ?1",
        );
        Ok(query(c, &sql, &[id_value(id)], voiceprint_sample_row)
            .map_err(vp_err)?
            .pop())
    }
}

// ---- sync (T11.1b) ----

/// The row `id` of the synced `table`, tombstones included; `None` when absent or for a
/// table that is not synced.
pub(crate) fn synced_by_id(
    c: &Connection,
    table: &str,
    id: EntityId,
) -> Result<Option<crate::sync::SyncedRow>, RepoError> {
    synced_by_id_raw(c, table, id).map_err(storage)
}

/// [`synced_by_id`] that tells a row whose stored values do not convert (a damaged row: its
/// check or a column's value refused, or text that is not UTF-8, as [`is_unreadable`] judges
/// it for the store's own readers) apart from any other store error: `Ok(Err(()))` for the
/// damaged row, `Err` for everything else.
pub(crate) fn synced_by_id_or_damaged(
    c: &Connection,
    table: &str,
    id: EntityId,
) -> Result<Result<Option<crate::sync::SyncedRow>, ()>, RepoError> {
    match synced_by_id_raw(c, table, id) {
        Ok(row) => Ok(Ok(row)),
        Err(e) if is_unreadable(&e) => Ok(Err(())),
        Err(e) => Err(storage(e)),
    }
}

/// Whether the row `id` of the synced `table` is a tombstone, read from its `deleted` column
/// alone, so a row whose other values do not convert still answers (T14.26). `false` when
/// absent or when that column itself does not read.
pub(crate) fn row_reads_deleted(
    c: &Connection,
    table: &str,
    id: EntityId,
) -> Result<bool, RepoError> {
    let rows = query(
        c,
        &format!("SELECT deleted FROM {table} WHERE id = ?1"),
        &[id_value(id)],
        |r| Ok(r.get::<_, i64>(0).is_ok_and(|d| d != 0)),
    )?;
    Ok(rows.first().copied().unwrap_or(false))
}

/// Whether the summary request `id` reads as withdrawn by `asker`, from its `state` and
/// `asking_device` columns alone, so a row whose other values do not convert still answers
/// (the withdrawal rule's `KeepMine` case, T14.26 review). `false` when absent or when either
/// column itself does not read.
pub(crate) fn request_reads_withdrawn_by(
    c: &Connection,
    id: EntityId,
    asker: DeviceId,
) -> Result<bool, RepoError> {
    let rows = query(
        c,
        "SELECT state, asking_device FROM summary_request WHERE id = ?1",
        &[id_value(id)],
        |r| {
            let state = r.get::<_, String>(0).ok();
            let device = r.get::<_, Vec<u8>>(1).ok();
            Ok(
                state.as_deref() == Some(earshot_core::model::RequestState::Withdrawn.name())
                    && device.as_deref() == Some(&asker.as_uuid().as_bytes()[..]),
            )
        },
    )?;
    Ok(rows.first().copied().unwrap_or(false))
}

/// Whether `e` says a row's stored values do not convert (a damaged row), not that the store
/// failed.
fn is_damaged(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::InvalidColumnType(..)
    )
}

/// [`is_damaged`], or text that is not UTF-8: a row this crate's own readers leave out and
/// count (T14.24, T14.26), and the sync outbox and apply count the same way.
fn is_unreadable(e: &rusqlite::Error) -> bool {
    is_damaged(e) || matches!(e, rusqlite::Error::Utf8Error(..))
}

fn synced_by_id_raw(
    c: &Connection,
    table: &str,
    id: EntityId,
) -> rusqlite::Result<Option<crate::sync::SyncedRow>> {
    use crate::sync::SyncedRow as R;
    fn q<T>(
        c: &Connection,
        sql: &str,
        params: &[Value],
        f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<Vec<T>> {
        let mut st = c.prepare_cached(sql)?;
        let rows = st.query_map(params_from_iter(params.iter()), f)?;
        rows.collect()
    }
    let sql = |cols: &[&'static str]| select_sql(table, &synced_cols(cols), "WHERE id = ?1");
    let p = [id_value(id)];
    Ok(match table {
        "meeting" => q(c, &sql(&MEETING), &p, meeting_row)?.pop().map(R::Meeting),
        "track" => q(c, &sql(&TRACK), &p, track_row)?.pop().map(R::Track),
        "segment" => q(c, &sql(&SEGMENT), &p, segment_row)?.pop().map(R::Segment),
        "speaker" => q(c, &sql(&SPEAKER), &p, speaker_row)?.pop().map(R::Speaker),
        "note" => q(c, &sql(&NOTE), &p, note_row)?.pop().map(R::Note),
        "summary" => q(c, &sql(&SUMMARY), &p, summary_row)?.pop().map(R::Summary),
        "person" => q(c, &sql(&PERSON), &p, person_row)?.pop().map(R::Person),
        "voiceprint" => q(c, &sql(&VOICEPRINT), &p, voiceprint_row)?
            .pop()
            .map(R::Voiceprint),
        "folder" => q(c, &sql(&FOLDER), &p, folder_row)?.pop().map(R::Folder),
        "summary_request" => q(c, &sql(&SUMMARY_REQUEST), &p, summary_request_row)?
            .pop()
            .map(R::SummaryRequest),
        "dictionary_word" => q(c, &sql(&DICTIONARY_WORD), &p, dictionary_word_row)?
            .pop()
            .map(R::DictionaryWord),
        "voiceprint_sample" => q(c, &sql(&VOICEPRINT_SAMPLE), &p, voiceprint_sample_row)?
            .pop()
            .map(R::VoiceprintSample),
        _ => None,
    })
}

/// Writes `row` with the repository's own checks (a missing parent is
/// [`RepoError::MissingParent`]).
pub(crate) fn put_synced(c: &Connection, row: &crate::sync::SyncedRow) -> Result<(), RepoError> {
    use crate::sync::SyncedRow as R;
    match row {
        R::Meeting(m) => put_meeting(c, m),
        R::Track(t) => put_track(c, t),
        R::Segment(s) => put_segment(c, s),
        R::Speaker(s) => put_speaker(c, s),
        R::Note(n) => put_note(c, n),
        R::Summary(s) => put_summary(c, s),
        R::Person(p) => put_person(c, p),
        R::Voiceprint(v) => put_voiceprint(c, v),
        R::Folder(f) => put_folder(c, f),
        R::SummaryRequest(s) => put_summary_request(c, s),
        R::DictionaryWord(w) => put_dictionary_word(c, w),
        R::VoiceprintSample(s) => put_voiceprint_sample(c, s),
    }
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
            fn shown_lines(&self, meeting: EntityId) -> Result<ShownLines, RepoError> {
                shown_lines(&self.conn, meeting)
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
            fn current_summary_id(&self, meeting: EntityId) -> Result<Option<EntityId>, RepoError> {
                current_summary_id(&self.conn, meeting)
            }
            fn shown_summary(&self, meeting: EntityId) -> Result<ShownSummary, RepoError> {
                shown_summary(&self.conn, meeting)
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

        impl DictionaryRepo for $ty {
            fn dictionary_word(&self, id: EntityId) -> Result<Option<DictionaryWord>, RepoError> {
                get_dictionary_word(&self.conn, id)
            }
            fn dictionary_words(&self) -> Result<Vec<DictionaryWord>, RepoError> {
                all_dictionary_words(&self.conn)
            }
            fn put_dictionary_word(&mut self, word: DictionaryWord) -> Result<(), RepoError> {
                put_dictionary_word(&self.conn, &word)
            }
        }

        impl SummaryRequestRepo for $ty {
            fn summary_request(&self, id: EntityId) -> Result<Option<SummaryRequest>, RepoError> {
                get_summary_request(&self.conn, id)
            }
            fn summary_requests(&self) -> Result<Vec<SummaryRequest>, RepoError> {
                all_summary_requests(&self.conn)
            }
            fn put_summary_request(&mut self, request: SummaryRequest) -> Result<(), RepoError> {
                put_summary_request(&self.conn, &request)
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
            fn samples(
                &self,
                model: &EmbeddingModelId,
            ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
                sample_reads::samples(&self.conn, model)
            }
            fn samples_of(
                &self,
                print: EntityId,
            ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
                sample_reads::samples_of(&self.conn, print)
            }
            fn samples_of_person(
                &self,
                person: EntityId,
            ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
                sample_reads::samples_of_person(&self.conn, person)
            }
            fn samples_from(
                &self,
                meeting: EntityId,
                speaker: EntityId,
            ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
                sample_reads::samples_from(&self.conn, meeting, speaker)
            }
            fn sample(&self, id: EntityId) -> Result<Option<VoiceprintSample>, VoiceprintError> {
                sample_reads::sample(&self.conn, id)
            }
            fn put_sample(&mut self, sample: VoiceprintSample) -> Result<(), VoiceprintError> {
                put_voiceprint_sample(&self.conn, &sample).map_err(vp_err)
            }
        }
    };
}

impl_repos!(Store);
impl_repos!(StoreTx<'_>);

/// The refusal of every write on a [`ReadOnlyStore`].
fn read_only() -> RepoError {
    RepoError::Storage("the store is open read-only".into())
}

/// Reads as [`Store`] does; every write is refused before SQLite sees it.
impl MeetingRepo for ReadOnlyStore {
    fn meeting(&self, id: EntityId) -> Result<Option<Meeting>, RepoError> {
        get_meeting(&self.conn, id)
    }
    fn meetings(&self) -> Result<Vec<Meeting>, RepoError> {
        all_meetings(&self.conn)
    }
    fn put_meeting(&mut self, _meeting: Meeting) -> Result<(), RepoError> {
        Err(read_only())
    }
    fn tracks(&self, meeting: EntityId) -> Result<Vec<Track>, RepoError> {
        tracks_of(&self.conn, meeting)
    }
    fn put_track(&mut self, _track: Track) -> Result<(), RepoError> {
        Err(read_only())
    }
    fn segments(&self, meeting: EntityId) -> Result<Vec<SegmentRow>, RepoError> {
        segments_of(&self.conn, meeting)
    }
    fn has_live_segment(&self, meeting: EntityId, pass: TranscriptPass) -> Result<bool, RepoError> {
        has_live_segment(&self.conn, meeting, pass)
    }
    fn shown_lines(&self, meeting: EntityId) -> Result<ShownLines, RepoError> {
        shown_lines(&self.conn, meeting)
    }
    fn put_segment(&mut self, _segment: SegmentRow) -> Result<(), RepoError> {
        Err(read_only())
    }
    fn speakers(&self, meeting: EntityId) -> Result<Vec<Speaker>, RepoError> {
        speakers_of(&self.conn, meeting)
    }
    fn put_speaker(&mut self, _speaker: Speaker) -> Result<(), RepoError> {
        Err(read_only())
    }
    fn notes(&self, meeting: EntityId) -> Result<Vec<Note>, RepoError> {
        notes_of(&self.conn, meeting)
    }
    fn put_note(&mut self, _note: Note) -> Result<(), RepoError> {
        Err(read_only())
    }
    fn summaries(&self, meeting: EntityId) -> Result<Vec<Summary>, RepoError> {
        summaries_of(&self.conn, meeting)
    }
    fn current_summary_id(&self, meeting: EntityId) -> Result<Option<EntityId>, RepoError> {
        current_summary_id(&self.conn, meeting)
    }
    fn shown_summary(&self, meeting: EntityId) -> Result<ShownSummary, RepoError> {
        shown_summary(&self.conn, meeting)
    }
    fn put_summary(&mut self, _summary: Summary) -> Result<(), RepoError> {
        Err(read_only())
    }
}

/// Reads persons and voiceprints as [`Store`] does (T12.71); every write is refused before
/// SQLite sees it.
impl VoiceprintRepo for ReadOnlyStore {
    fn person(&self, id: EntityId) -> Result<Option<Person>, VoiceprintError> {
        get_person(&self.conn, id).map_err(vp_err)
    }
    fn persons(&self) -> Result<Vec<Person>, VoiceprintError> {
        all_persons(&self.conn).map_err(vp_err)
    }
    fn put_person(&mut self, _person: Person) -> Result<(), VoiceprintError> {
        Err(vp_err(read_only()))
    }
    fn voiceprints(&self, model: &EmbeddingModelId) -> Result<Vec<Voiceprint>, VoiceprintError> {
        voiceprints_where(&self.conn, "model", text(model.as_str())).map_err(vp_err)
    }
    fn voiceprints_of(&self, person: EntityId) -> Result<Vec<Voiceprint>, VoiceprintError> {
        voiceprints_where(&self.conn, "person_id", id_value(person)).map_err(vp_err)
    }
    fn put_voiceprint(&mut self, _voiceprint: Voiceprint) -> Result<(), VoiceprintError> {
        Err(vp_err(read_only()))
    }
    fn samples(&self, model: &EmbeddingModelId) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        sample_reads::samples(&self.conn, model)
    }
    fn samples_of(&self, print: EntityId) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        sample_reads::samples_of(&self.conn, print)
    }
    fn samples_of_person(
        &self,
        person: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        sample_reads::samples_of_person(&self.conn, person)
    }
    fn samples_from(
        &self,
        meeting: EntityId,
        speaker: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        sample_reads::samples_from(&self.conn, meeting, speaker)
    }
    fn sample(&self, id: EntityId) -> Result<Option<VoiceprintSample>, VoiceprintError> {
        sample_reads::sample(&self.conn, id)
    }
    fn put_sample(&mut self, _sample: VoiceprintSample) -> Result<(), VoiceprintError> {
        Err(vp_err(read_only()))
    }
}

/// Reads audio file rows as [`Store`] does (T12.73, which reads the recordings they name in
/// place); every write is refused before SQLite sees it.
impl AudioFileRepo for ReadOnlyStore {
    fn audio_files(&self, meeting: EntityId) -> Result<Vec<AudioFile>, RepoError> {
        audio_of(&self.conn, meeting)
    }
    fn all_audio_files(&self) -> Result<Vec<AudioFile>, RepoError> {
        all_audio(&self.conn)
    }
    fn put_audio_file(&mut self, _file: AudioFile) -> Result<(), RepoError> {
        Err(read_only())
    }
    fn delete_audio_file(&mut self, _id: EntityId) -> Result<bool, RepoError> {
        Err(read_only())
    }
}
