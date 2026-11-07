use std::collections::{BTreeSet, HashSet};

use serde_json::{Value, json};

use super::validate::{
    ACTION_ITEM_FIELDS, BULLET_FIELDS, DECISION_FIELDS, SECTION_FIELDS, SUMMARY_FIELDS,
};
use super::*;
use crate::identity::EntityId;

const S1: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
const S2: &str = "01890a5d-ac96-774b-bcce-b302099a8058";
const S3: &str = "01890a5d-ac96-774b-bcce-b302099a8059";
const UNKNOWN: &str = "01890a5d-ac96-774b-bcce-b3020999ffff";

fn id(s: &str) -> EntityId {
    s.parse().unwrap()
}

fn transcript() -> BTreeSet<EntityId> {
    [S1, S2, S3].into_iter().map(id).collect()
}

fn valid() -> Value {
    json!({
        "title": "Roadmap sync",
        "tldr": "Agreed to ship the beta on Friday.",
        "enhanced_notes": [
            {
                "heading": "Beta",
                "bullets": [
                    { "text": "Beta scope is frozen", "source_segment_ids": [S1] },
                    { "text": "From my notes only", "source_segment_ids": [] }
                ]
            }
        ],
        "decisions": [
            { "text": "Ship the beta on Friday", "source_segment_ids": [S1, S2] }
        ],
        "action_items": [
            { "owner": "Ana", "task": "Write release notes", "due": "Friday", "source_segment_ids": [S2] },
            { "owner": "Unassigned", "task": "Book a room", "due": null, "source_segment_ids": [S3] },
            { "owner": "Ben", "task": "Check CI", "due": null, "source_segment_ids": [S3] }
        ],
        "open_questions": ["Who owns support?"],
        "topics": ["beta", "release"]
    })
}

fn errors(v: &Value) -> Vec<(String, SummaryErrorKind)> {
    match validate_value(v, &transcript()) {
        Ok(s) => panic!("expected errors, got {s:?}"),
        Err(e) => e.into_iter().map(|e| (e.path, e.kind)).collect(),
    }
}

#[test]
fn summary_valid_document_parses_into_types() {
    let s = validate_value(&valid(), &transcript()).unwrap();
    assert_eq!(s.title, "Roadmap sync");
    assert_eq!(s.decisions[0].source_segment_ids, [id(S1), id(S2)]);
    assert_eq!(s.action_items[0].due.as_deref(), Some("Friday"));
    assert_eq!(s.action_items[1].due, None);
    assert_eq!(s.action_items[2].due, None);
    assert!(s.enhanced_notes[0].bullets[1].source_segment_ids.is_empty());

    let text = serde_json::to_string(&valid()).unwrap();
    assert_eq!(parse_summary(&text, &transcript()).unwrap(), s);
    assert!(validate_summary(&s, &transcript()).is_empty());
}

/// OpenAI strict structured outputs need every property required; optional values are
/// required-but-nullable instead.
#[test]
fn summary_due_is_required_but_nullable() {
    let mut v = valid();
    v["action_items"][2].as_object_mut().unwrap().remove("due");
    assert_eq!(
        errors(&v),
        [("/action_items/2/due".into(), SummaryErrorKind::MissingField)]
    );
    let schema = json_schema();
    let item = &schema["$defs"]["ActionItem"];
    assert!(item["required"].as_array().unwrap().contains(&json!("due")));
    assert_eq!(item["properties"]["due"]["type"], json!(["string", "null"]));
    assert!(item["properties"]["due"].get("default").is_none());
}

#[test]
fn summary_segment_id_sets_can_be_any_collection() {
    let s = validate_value(&valid(), &transcript()).unwrap();
    let hash: HashSet<EntityId> = transcript().into_iter().collect();
    let vec: Vec<EntityId> = transcript().into_iter().collect();
    assert!(validate_summary(&s, &hash).is_empty());
    assert!(validate_summary(&s, vec.as_slice()).is_empty());
}

#[test]
fn summary_invalid_json_is_one_root_error() {
    let e = parse_summary("{\"title\": ", &transcript()).unwrap_err();
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].path, "");
    assert!(matches!(e[0].kind, SummaryErrorKind::Json(_)));
    let e = parse_summary("[]", &transcript()).unwrap_err();
    assert_eq!(
        e[0].kind,
        SummaryErrorKind::WrongType { expected: "object" }
    );
}

#[test]
fn summary_missing_unknown_and_mistyped_fields_are_all_reported() {
    let mut v = valid();
    let o = v.as_object_mut().unwrap();
    o.remove("tldr");
    o.insert("summary".into(), json!("extra"));
    o.insert("topics".into(), json!("beta"));
    v["action_items"][0].as_object_mut().unwrap().remove("task");
    v["action_items"][1]["due"] = json!(5);
    v["enhanced_notes"][0]["bullets"][0]["text"] = json!(["x"]);
    v["open_questions"][0] = json!(null);
    let mut got = errors(&v);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        [
            (
                "/action_items/0/task".into(),
                SummaryErrorKind::MissingField
            ),
            (
                "/action_items/1/due".into(),
                SummaryErrorKind::WrongType {
                    expected: "string or null"
                }
            ),
            (
                "/enhanced_notes/0/bullets/0/text".into(),
                SummaryErrorKind::WrongType { expected: "string" }
            ),
            (
                "/open_questions/0".into(),
                SummaryErrorKind::WrongType { expected: "string" }
            ),
            ("/summary".into(), SummaryErrorKind::UnknownField),
            ("/tldr".into(), SummaryErrorKind::MissingField),
            (
                "/topics".into(),
                SummaryErrorKind::WrongType { expected: "array" }
            ),
        ]
    );
}

#[test]
fn summary_blank_text_is_rejected() {
    let mut v = valid();
    v["title"] = json!("  ");
    v["decisions"][0]["text"] = json!("");
    v["action_items"][0]["owner"] = json!("");
    v["action_items"][0]["due"] = json!(" ");
    v["topics"][1] = json!("");
    let mut got = errors(&v);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    let paths: Vec<&str> = got.iter().map(|g| g.0.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/action_items/0/due",
            "/action_items/0/owner",
            "/decisions/0/text",
            "/title",
            "/topics/1"
        ]
    );
    assert!(got.iter().all(|g| g.1 == SummaryErrorKind::Blank));
}

#[test]
fn summary_grounding_requires_a_citation_for_decisions_and_action_items() {
    let mut v = valid();
    v["decisions"][0]["source_segment_ids"] = json!([]);
    v["action_items"][2]["source_segment_ids"] = json!([]);
    assert_eq!(
        errors(&v),
        [
            (
                "/decisions/0/source_segment_ids".into(),
                SummaryErrorKind::Uncited
            ),
            (
                "/action_items/2/source_segment_ids".into(),
                SummaryErrorKind::Uncited
            ),
        ]
    );
}

#[test]
fn summary_grounding_requires_every_cited_id_to_exist() {
    let mut v = valid();
    v["decisions"][0]["source_segment_ids"] = json!([S1, UNKNOWN]);
    v["action_items"][0]["source_segment_ids"] = json!([UNKNOWN]);
    v["enhanced_notes"][0]["bullets"][1]["source_segment_ids"] = json!([UNKNOWN]);
    assert_eq!(
        errors(&v),
        [
            (
                "/enhanced_notes/0/bullets/1/source_segment_ids/0".into(),
                SummaryErrorKind::UnknownSegmentId(id(UNKNOWN))
            ),
            (
                "/decisions/0/source_segment_ids/1".into(),
                SummaryErrorKind::UnknownSegmentId(id(UNKNOWN))
            ),
            (
                "/action_items/0/source_segment_ids/0".into(),
                SummaryErrorKind::UnknownSegmentId(id(UNKNOWN))
            ),
        ]
    );
    // The same check on typed values.
    let mut s = validate_value(&valid(), &transcript()).unwrap();
    s.decisions[0].source_segment_ids.push(id(UNKNOWN));
    let e = validate_summary(&s, &transcript());
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].path, "/decisions/0/source_segment_ids/2");
    // An empty transcript grounds nothing.
    assert!(validate_value(&valid(), &BTreeSet::new()).is_err());
}

#[test]
fn summary_malformed_segment_ids_are_reported() {
    let mut v = valid();
    v["decisions"][0]["source_segment_ids"] = json!(["seg-1", 7]);
    assert_eq!(
        errors(&v),
        [
            (
                "/decisions/0/source_segment_ids/0".into(),
                SummaryErrorKind::InvalidSegmentId("seg-1".into())
            ),
            (
                "/decisions/0/source_segment_ids/1".into(),
                SummaryErrorKind::WrongType { expected: "string" }
            ),
        ]
    );
}

#[test]
fn summary_error_paths_escape_json_pointer_tokens_and_display_for_repair_prompts() {
    let mut v = valid();
    v.as_object_mut().unwrap().insert("a/b~c".into(), json!(1));
    let e = validate_value(&v, &transcript()).unwrap_err();
    assert_eq!(e[0].path, "/a~1b~0c");
    assert_eq!(e[0].to_string(), "/a~1b~0c: unknown field");

    let e = parse_summary("nope", &transcript()).unwrap_err();
    assert!(
        e[0].to_string().starts_with("(document): not valid JSON"),
        "{}",
        e[0]
    );

    let mut v = valid();
    v["decisions"][0]["source_segment_ids"] = json!([UNKNOWN]);
    let e = validate_value(&v, &transcript()).unwrap_err();
    assert_eq!(
        e[0].to_string(),
        format!("/decisions/0/source_segment_ids/0: segment id {UNKNOWN} is not in the transcript")
    );
}

/// The validator's field lists must match the serde types, as seen through the generated
/// schema, so the hand-written walker cannot drift from `SummaryV1`.
#[test]
fn summary_validator_field_lists_match_the_schema() {
    let schema = json_schema();
    let defs = &schema["$defs"];
    let check = |obj: &Value, fields: &[(&str, bool)], name: &str| {
        let props: BTreeSet<&str> = obj["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} has properties"))
            .keys()
            .map(String::as_str)
            .collect();
        let required: BTreeSet<&str> = obj["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        let want_props: BTreeSet<&str> = fields.iter().map(|f| f.0).collect();
        let want_required: BTreeSet<&str> = fields.iter().filter(|f| f.1).map(|f| f.0).collect();
        assert_eq!(props, want_props, "{name} properties");
        assert_eq!(required, want_required, "{name} required");
        assert_eq!(obj["additionalProperties"], json!(false), "{name}");
    };
    check(&schema, SUMMARY_FIELDS, "SummaryV1");
    check(&defs["NoteSection"], SECTION_FIELDS, "NoteSection");
    check(&defs["NoteBullet"], BULLET_FIELDS, "NoteBullet");
    check(&defs["Decision"], DECISION_FIELDS, "Decision");
    check(&defs["ActionItem"], ACTION_ITEM_FIELDS, "ActionItem");
}

#[test]
fn summary_schema_describes_segment_ids_as_uuid_strings() {
    let schema = json_schema();
    assert_eq!(
        schema["$schema"],
        json!("https://json-schema.org/draft/2020-12/schema")
    );
    assert_eq!(schema["title"], json!("Earshot meeting summary v1"));
    let ids = &schema["$defs"]["Decision"]["properties"]["source_segment_ids"];
    assert_eq!(ids["type"], json!("array"));
    assert_eq!(ids["items"], json!({ "type": "string", "format": "uuid" }));
    // Deterministic output (the committed file is drift-checked byte for byte).
    assert_eq!(json_schema(), schema);
}

#[test]
fn summary_round_trips_through_serde() {
    let s = validate_value(&valid(), &transcript()).unwrap();
    let back = serde_json::to_value(&s).unwrap();
    assert_eq!(validate_value(&back, &transcript()).unwrap(), s);
}
