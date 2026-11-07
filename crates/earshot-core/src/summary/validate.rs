//! Summary v1 validation: shape, non-blank text and grounding, with JSON Pointer paths.
//!
//! Rules are checked on a `serde_json::Value` by one walker, so LLM output with many
//! problems gets all of them reported in one pass (the repair loop in PLAN.md 3.7 feeds
//! them back to the model). Typed summaries are serialised and walked by the same code.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::hash::BuildHasher;

use serde_json::{Map, Value};

use super::SummaryV1;
use crate::identity::EntityId;

/// The transcript segment ids a summary may cite.
pub trait SegmentIds {
    /// Whether `id` is a segment of the transcript.
    fn contains_segment(&self, id: &EntityId) -> bool;
}

impl SegmentIds for BTreeSet<EntityId> {
    fn contains_segment(&self, id: &EntityId) -> bool {
        self.contains(id)
    }
}

impl<S: BuildHasher> SegmentIds for HashSet<EntityId, S> {
    fn contains_segment(&self, id: &EntityId) -> bool {
        self.contains(id)
    }
}

impl SegmentIds for [EntityId] {
    fn contains_segment(&self, id: &EntityId) -> bool {
        self.contains(id)
    }
}

/// What is wrong at a [`SummaryError::path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryErrorKind {
    /// The text is not JSON (or, internally, could not be converted).
    Json(String),
    /// The value has the wrong JSON type.
    WrongType {
        /// The expected JSON type.
        expected: &'static str,
    },
    /// A required field is absent (the path names the missing field).
    MissingField,
    /// A field that the schema does not define.
    UnknownField,
    /// Empty or whitespace-only text.
    Blank,
    /// A citation that is not a UUID.
    InvalidSegmentId(String),
    /// A decision or action item that cites no segment (grounding).
    Uncited,
    /// A cited segment id that is not in the transcript (grounding).
    UnknownSegmentId(EntityId),
    /// A citation in a model reply that is not a segment number (an integer from 1).
    InvalidSegmentNumber(String),
    /// A cited segment number that the request may not cite (not in the transcript, or
    /// outside the part a map request covers).
    UnknownSegmentNumber(u64),
    /// More citations on one item than [`super::MAX_CITATIONS`].
    TooManyCitations(usize),
    /// The same segment number twice in one item's citations.
    DuplicateSegmentNumber(u64),
    /// The same item [`super::REPEAT_LIMIT`] or more times in one list, or a reply the
    /// generator stopped as looping ([`super::repetition_errors`]).
    Repetition,
}

impl fmt::Display for SummaryErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SummaryErrorKind::Json(e) => write!(f, "not valid JSON: {e}"),
            SummaryErrorKind::WrongType { expected } => write!(f, "expected {expected}"),
            SummaryErrorKind::MissingField => f.write_str("missing required field"),
            SummaryErrorKind::UnknownField => f.write_str("unknown field"),
            SummaryErrorKind::Blank => f.write_str("must not be empty"),
            SummaryErrorKind::InvalidSegmentId(s) => {
                write!(f, "`{s}` is not a segment id (expected a UUID)")
            }
            SummaryErrorKind::Uncited => {
                f.write_str("must cite at least one transcript segment id")
            }
            SummaryErrorKind::UnknownSegmentId(id) => {
                write!(f, "segment id {id} is not in the transcript")
            }
            SummaryErrorKind::InvalidSegmentNumber(s) => {
                write!(f, "`{s}` is not a segment number (expected an integer)")
            }
            SummaryErrorKind::UnknownSegmentNumber(n) => {
                write!(
                    f,
                    "segment {n} is not one of the transcript lines you were given"
                )
            }
            SummaryErrorKind::TooManyCitations(n) => write!(
                f,
                "{n} citations; cite at most {} segments",
                super::MAX_CITATIONS
            ),
            SummaryErrorKind::DuplicateSegmentNumber(n) => {
                write!(f, "cites segment {n} twice; cite each segment once")
            }
            SummaryErrorKind::Repetition => {
                f.write_str("repeats an earlier item of the same list; write each item once")
            }
        }
    }
}

/// One validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryError {
    /// RFC 6901 JSON Pointer to the offending value (`""` is the whole document), for
    /// example `/action_items/2/source_segment_ids/0`.
    pub path: String,
    /// What is wrong there.
    pub kind: SummaryErrorKind,
}

impl fmt::Display for SummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "(document)"
        } else {
            &self.path
        };
        write!(f, "{path}: {}", self.kind)
    }
}

impl std::error::Error for SummaryError {}

/// Field lists of each object: `(name, required)`. Kept equal to the serde types by the
/// `summary_validator_field_lists_match_the_schema` test.
pub(super) type Fields = &'static [(&'static str, bool)];

pub(super) const SUMMARY_FIELDS: Fields = &[
    ("title", true),
    ("tldr", true),
    ("enhanced_notes", true),
    ("decisions", true),
    ("action_items", true),
    ("open_questions", true),
    ("topics", true),
];
pub(super) const SECTION_FIELDS: Fields = &[("heading", true), ("bullets", true)];
pub(super) const BULLET_FIELDS: Fields = &[("text", true), ("source_segment_ids", true)];
pub(super) const DECISION_FIELDS: Fields = &[("text", true), ("source_segment_ids", true)];
pub(super) const ACTION_ITEM_FIELDS: Fields = &[
    ("owner", true),
    ("task", true),
    ("due", true),
    ("source_segment_ids", true),
];

/// Parses LLM output text and validates it ([`validate_value`]).
pub fn parse_summary<I: SegmentIds + ?Sized>(
    json: &str,
    segment_ids: &I,
) -> Result<SummaryV1, Vec<SummaryError>> {
    let value: Value = serde_json::from_str(json).map_err(|e| {
        vec![SummaryError {
            path: String::new(),
            kind: SummaryErrorKind::Json(e.to_string()),
        }]
    })?;
    validate_value(&value, segment_ids)
}

/// Validates a JSON value against summary v1 and returns the typed summary, or every
/// error found: shape (missing, unknown and mistyped fields), blank text, malformed
/// segment ids, and grounding (each decision and action item cites at least one segment;
/// every cited id, in bullets too, is in `segment_ids`).
pub fn validate_value<I: SegmentIds + ?Sized>(
    value: &Value,
    segment_ids: &I,
) -> Result<SummaryV1, Vec<SummaryError>> {
    let mut w = Walker {
        ids: segment_ids,
        errors: Vec::new(),
    };
    w.summary(value);
    if !w.errors.is_empty() {
        return Err(w.errors);
    }
    serde_json::from_value(value.clone()).map_err(|e| {
        vec![SummaryError {
            path: String::new(),
            kind: SummaryErrorKind::Json(e.to_string()),
        }]
    })
}

/// Validates a typed summary with the same rules as [`validate_value`] (blank text and
/// grounding; the shape is guaranteed by the types). Empty when valid.
pub fn validate_summary<I: SegmentIds + ?Sized>(
    summary: &SummaryV1,
    segment_ids: &I,
) -> Vec<SummaryError> {
    match serde_json::to_value(summary) {
        Ok(v) => validate_value(&v, segment_ids).err().unwrap_or_default(),
        Err(e) => vec![SummaryError {
            path: String::new(),
            kind: SummaryErrorKind::Json(e.to_string()),
        }],
    }
}

/// RFC 6901 escaping of one reference token.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn child(path: &str, key: &str) -> String {
    format!("{path}/{}", escape(key))
}

struct Walker<'a, I: ?Sized> {
    ids: &'a I,
    errors: Vec<SummaryError>,
}

impl<I: SegmentIds + ?Sized> Walker<'_, I> {
    fn err(&mut self, path: String, kind: SummaryErrorKind) {
        self.errors.push(SummaryError { path, kind });
    }

    /// Checks that `v` is an object with exactly the known fields and every required one.
    fn object<'v>(
        &mut self,
        v: &'v Value,
        path: &str,
        fields: Fields,
    ) -> Option<&'v Map<String, Value>> {
        let Some(map) = v.as_object() else {
            self.err(
                path.to_string(),
                SummaryErrorKind::WrongType { expected: "object" },
            );
            return None;
        };
        for key in map.keys() {
            if !fields.iter().any(|(f, _)| f == key) {
                self.err(child(path, key), SummaryErrorKind::UnknownField);
            }
        }
        for (f, required) in fields {
            if *required && !map.contains_key(*f) {
                self.err(child(path, f), SummaryErrorKind::MissingField);
            }
        }
        Some(map)
    }

    fn text(&mut self, v: Option<&Value>, path: String) {
        match v {
            None => {}
            Some(Value::String(s)) if s.trim().is_empty() => {
                self.err(path, SummaryErrorKind::Blank);
            }
            Some(Value::String(_)) => {}
            Some(_) => self.err(path, SummaryErrorKind::WrongType { expected: "string" }),
        }
    }

    fn optional_text(&mut self, v: Option<&Value>, path: String) {
        match v {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s.trim().is_empty() => {
                self.err(path, SummaryErrorKind::Blank);
            }
            Some(Value::String(_)) => {}
            Some(_) => self.err(
                path,
                SummaryErrorKind::WrongType {
                    expected: "string or null",
                },
            ),
        }
    }

    fn array<'v>(&mut self, v: Option<&'v Value>, path: &str) -> &'v [Value] {
        match v {
            None => &[],
            Some(Value::Array(a)) => a,
            Some(_) => {
                self.err(
                    path.to_string(),
                    SummaryErrorKind::WrongType { expected: "array" },
                );
                &[]
            }
        }
    }

    fn texts(&mut self, v: Option<&Value>, path: &str) {
        for (i, item) in self.array(v, path).iter().enumerate() {
            self.text(Some(item), format!("{path}/{i}"));
        }
    }

    /// Citations: UUID strings that exist in the transcript; `must_cite` requires one.
    fn segment_ids(&mut self, v: Option<&Value>, path: &str, must_cite: bool) {
        let Some(v) = v else { return };
        let Value::Array(items) = v else {
            self.err(
                path.to_string(),
                SummaryErrorKind::WrongType { expected: "array" },
            );
            return;
        };
        if must_cite && items.is_empty() {
            self.err(path.to_string(), SummaryErrorKind::Uncited);
        }
        for (i, item) in items.iter().enumerate() {
            let p = format!("{path}/{i}");
            let Value::String(s) = item else {
                self.err(p, SummaryErrorKind::WrongType { expected: "string" });
                continue;
            };
            match s.parse::<EntityId>() {
                Ok(id) if self.ids.contains_segment(&id) => {}
                Ok(id) => self.err(p, SummaryErrorKind::UnknownSegmentId(id)),
                Err(_) => self.err(p, SummaryErrorKind::InvalidSegmentId(s.clone())),
            }
        }
    }

    fn summary(&mut self, v: &Value) {
        let Some(m) = self.object(v, "", SUMMARY_FIELDS) else {
            return;
        };
        self.text(m.get("title"), "/title".into());
        self.text(m.get("tldr"), "/tldr".into());

        for (i, section) in self
            .array(m.get("enhanced_notes"), "/enhanced_notes")
            .iter()
            .enumerate()
        {
            let sp = format!("/enhanced_notes/{i}");
            let Some(s) = self.object(section, &sp, SECTION_FIELDS) else {
                continue;
            };
            self.text(s.get("heading"), format!("{sp}/heading"));
            let bp = format!("{sp}/bullets");
            for (j, bullet) in self.array(s.get("bullets"), &bp).iter().enumerate() {
                let p = format!("{bp}/{j}");
                let Some(b) = self.object(bullet, &p, BULLET_FIELDS) else {
                    continue;
                };
                self.text(b.get("text"), format!("{p}/text"));
                self.segment_ids(
                    b.get("source_segment_ids"),
                    &format!("{p}/source_segment_ids"),
                    false,
                );
            }
        }

        for (i, d) in self
            .array(m.get("decisions"), "/decisions")
            .iter()
            .enumerate()
        {
            let p = format!("/decisions/{i}");
            let Some(d) = self.object(d, &p, DECISION_FIELDS) else {
                continue;
            };
            self.text(d.get("text"), format!("{p}/text"));
            self.segment_ids(
                d.get("source_segment_ids"),
                &format!("{p}/source_segment_ids"),
                true,
            );
        }

        for (i, a) in self
            .array(m.get("action_items"), "/action_items")
            .iter()
            .enumerate()
        {
            let p = format!("/action_items/{i}");
            let Some(a) = self.object(a, &p, ACTION_ITEM_FIELDS) else {
                continue;
            };
            self.text(a.get("owner"), format!("{p}/owner"));
            self.text(a.get("task"), format!("{p}/task"));
            self.optional_text(a.get("due"), format!("{p}/due"));
            self.segment_ids(
                a.get("source_segment_ids"),
                &format!("{p}/source_segment_ids"),
                true,
            );
        }

        self.texts(m.get("open_questions"), "/open_questions");
        self.texts(m.get("topics"), "/topics");
    }
}
