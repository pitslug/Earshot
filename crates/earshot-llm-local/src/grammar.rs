//! JSON Schema -> GBNF for llama.cpp's grammar sampler, and the grounded summary grammar.
//!
//! A small converter for the JSON Schema subset `schemas/summary.v1.json` uses, written here
//! instead of llama.cpp's `common/json-schema-to-grammar.cpp` because that file lives in
//! `llama-common`, which also links an HTTP client (cpp-httplib, for `-hf` model downloads);
//! leaving `llama-common` out keeps every network-capable library out of the process
//! (PLAN 3.9). The rule shapes (`space`, `char`, the UUID rule, the array repetition) follow
//! llama.cpp's converter (`common/json-schema-to-grammar.cpp`, the `SPACE_RULE`,
//! `PRIMITIVE_RULES` and `build_repetition` there) at the version vendored by
//! llama-cpp-sys-2 0.1.160.
//!
//! Supported: `type` (`object`, `array`, `string`, `null`, or a list of those), `properties`
//! with every property `required` and `additionalProperties: false` (properties are emitted
//! in `required` order), `items`, `minItems`, `maxItems`, `enum` of strings or non-negative
//! integers, `format: uuid`,
//! and `$ref` to `#/$defs/<name>`. Annotations (`description`, `title`, `$schema`, `$defs`)
//! are ignored. Anything else is a [`GrammarError`], never silently dropped.
//!
//! [`summary_grammar`] narrows the reply schema (`earshot_core::summary::llm_json_schema`,
//! citations by segment number) before converting it: every `source_segment_ids` item must
//! be one of the segment numbers the request may cite (an `enum`), decisions and action items
//! cite at least one segment and at most `max_citations`, arrays are capped, and text cannot
//! start with whitespace (so it is never blank). Every output of the grammar therefore passes
//! `earshot_core::summary::validate_reply` for those segments, except text that is only
//! non-ASCII whitespace and repeated items.

use earshot_core::summary::Citable;
use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value, json};

/// Why a schema could not be turned into a grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarError(pub String);

impl fmt::Display for GrammarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported JSON schema for a grammar: {}", self.0)
    }
}

impl std::error::Error for GrammarError {}

/// Options of [`schema_to_gbnf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrammarOptions {
    /// Plain strings must start with a printable, non-space ASCII-or-higher character, so
    /// they are never empty or whitespace-only. Strings with a `format` or `enum` are
    /// unaffected.
    pub non_blank_strings: bool,
    /// Compact JSON: no whitespace between tokens (the `space` rule matches only the empty
    /// string), as `serde_json::to_string` writes it. Indentation costs reply tokens (a
    /// summary nests four or five levels deep, and the indentation of every line came out
    /// of the reply budget, T6.20); without it, llama.cpp's `SPACE_RULE` allows one space
    /// or a newline with up to 20 spaces or tabs.
    pub compact: bool,
}

/// Limits [`summary_grammar`] puts on a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryLimits {
    /// Most items in any list (sections, bullets, decisions, action items, questions,
    /// topics).
    pub max_items: u32,
    /// Most segment ids one bullet, decision or action item cites.
    pub max_citations: u32,
}

impl Default for SummaryLimits {
    fn default() -> Self {
        SummaryLimits {
            max_items: 12,
            max_citations: u32::try_from(earshot_core::summary::MAX_CITATIONS).unwrap_or(u32::MAX),
        }
    }
}

/// `space` and string rules shared by every grammar (llama.cpp `SPACE_RULE`,
/// `PRIMITIVE_RULES["char"]`, `["string"]` and `["uuid"]`).
const BASE_RULES: &[(&str, &str)] = &[
    ("space", r#"| " " | "\n" [ \t]{0,20}"#),
    (
        "char",
        r#"[^"\\\x7F\x00-\x1F] | [\\] (["\\bfnrt] | "u" [0-9a-fA-F]{4})"#,
    ),
    ("string", r#""\"" char* "\"" space"#),
    ("text", r#""\"" [^"\\\x7F\x00-\x20] char* "\"" space"#),
    (
        "uuid",
        r#""\"" [0-9a-fA-F]{8} "-" [0-9a-fA-F]{4} "-" [0-9a-fA-F]{4} "-" [0-9a-fA-F]{4} "-" [0-9a-fA-F]{12} "\"" space"#,
    ),
    ("null", r#""null" space"#),
];

/// The `space` rule of a compact grammar: nothing.
const COMPACT_SPACE: &str = r#""""#;

/// Annotation keywords that do not constrain the output.
const ANNOTATIONS: &[&str] = &["description", "title", "$schema", "$defs"];

/// Converts `schema` to a GBNF grammar whose start rule is `root`.
///
/// # Errors
/// [`GrammarError`] for any keyword or shape outside the supported subset (module docs).
pub fn schema_to_gbnf(schema: &Value, options: GrammarOptions) -> Result<String, GrammarError> {
    let defs = schema
        .get("$defs")
        .map(|d| {
            d.as_object()
                .ok_or_else(|| GrammarError("`$defs` is not an object".into()))
        })
        .transpose()?;
    let mut c = Converter {
        defs,
        options,
        rules: BTreeMap::new(),
        used_base: Vec::new(),
    };
    let root = c.visit(schema, "root")?;
    c.rules.insert("root".into(), root);
    let mut out = String::new();
    // root first (llama.cpp only needs it to exist; first reads better), then the rest.
    out.push_str(&format!("root ::= {}\n", c.rules["root"]));
    for (name, body) in &c.rules {
        if name != "root" {
            out.push_str(&format!("{name} ::= {body}\n"));
        }
    }
    let needs_char = c.used_base.contains(&"string") || c.used_base.contains(&"text");
    for (name, body) in BASE_RULES {
        if *name == "space" {
            let body = if c.options.compact {
                COMPACT_SPACE
            } else {
                body
            };
            out.push_str(&format!("{name} ::= {body}\n"));
        } else if c.used_base.contains(name) || (*name == "char" && needs_char) {
            out.push_str(&format!("{name} ::= {body}\n"));
        }
    }
    Ok(out)
}

/// The summary v1 grammar for a meeting whose transcript has the segments `ids`:
/// citations are limited to `ids` (decisions and action items cite at least one), lists are
/// capped by `limits`, and text is never blank. Whitespace between tokens follows
/// llama.cpp's `SPACE_RULE` (indented output allowed).
///
/// Not compact: forcing compact JSON made greedy decoding much worse (T6.20, measured
/// 2026-10-25 on the AMI dev split: Qwen3 4B went from 16 of 16 valid summaries to 0 of 20,
/// Phi-4-mini from ROUGE-Lsum 0.311 to 0.153). These models write JSON with newlines; when
/// the newline before `]` is forbidden, the best allowed token is `,{`, so lists never
/// close and fill up to their cap. Indentation costs about one token per line.
///
/// The grammar follows the reply schema (`earshot_core::summary::llm_json_schema`):
/// citations are the segment numbers of `cites` (indices from 0; segment `i` is number
/// `i + 1`), as the prompt shows them.
///
/// # Errors
/// [`GrammarError`] when the reply schema no longer has the shape this narrowing expects
/// (a schema change must update it). An empty `cites` is not an error: the grammar then
/// allows only empty citation lists and no decisions or action items.
pub fn summary_grammar(cites: &Citable, limits: SummaryLimits) -> Result<String, GrammarError> {
    summary_grammar_with(cites, limits, false)
}

/// Which lists of a summary reply a request may fill (T6.20c two-call chunks, evaluation
/// only; production always asks for [`SummaryPart::Whole`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryPart {
    /// Every list ([`summary_grammar`]).
    Whole,
    /// Only `action_items`: `enhanced_notes`, `decisions`, `open_questions` and `topics`
    /// are empty lists (`title` and `tldr` stay required, non-blank text).
    ActionsOnly,
    /// Every list but `action_items`, which is empty.
    NoActions,
}

/// [`summary_grammar`] for one [`SummaryPart`]: the lists the part leaves out are capped at
/// zero items (`maxItems: 0`), so the reply keeps the summary v1 shape and validates as any
/// reply does. [`SummaryPart::Whole`] is exactly [`summary_grammar`].
///
/// # Errors
/// As [`summary_grammar`].
pub fn summary_part_grammar(
    cites: &Citable,
    limits: SummaryLimits,
    part: SummaryPart,
) -> Result<String, GrammarError> {
    grammar_for(cites, limits, false, part)
}

/// [`summary_grammar`], compact (no whitespace at all, [`GrammarOptions::compact`]) when
/// `compact` is set. Kept for experiments; see [`summary_grammar`] for why it is off.
///
/// # Errors
/// As [`summary_grammar`].
pub fn summary_grammar_with(
    cites: &Citable,
    limits: SummaryLimits,
    compact: bool,
) -> Result<String, GrammarError> {
    grammar_for(cites, limits, compact, SummaryPart::Whole)
}

fn grammar_for(
    cites: &Citable,
    limits: SummaryLimits,
    compact: bool,
    part: SummaryPart,
) -> Result<String, GrammarError> {
    let mut schema = earshot_core::summary::llm_json_schema();
    // Nothing citable (a merge whose partials cite nothing): every citation list is empty
    // and, since decisions and action items must cite a segment, there are none; the reply
    // validates exactly as `validate_reply` would take it. The enum value is never reachable
    // (lists hold no items) but keeps the item rule well formed.
    let nothing = cites.is_empty();
    let id_enum: Vec<Value> = if nothing {
        vec![json!(1)]
    } else {
        cites.iter().map(|i| json!(i + 1)).collect()
    };
    let limits = SummaryLimits {
        max_citations: if nothing { 0 } else { limits.max_citations },
        ..limits
    };
    let mut narrowed = 0;
    narrow(&mut schema, &id_enum, limits, &mut narrowed);
    if narrowed != 3 {
        return Err(GrammarError(format!(
            "expected `source_segment_ids` in NoteBullet, Decision and ActionItem, found {narrowed}"
        )));
    }
    if nothing {
        for list in ["decisions", "action_items"] {
            let items = schema
                .pointer_mut(&format!("/properties/{list}"))
                .and_then(Value::as_object_mut)
                .ok_or_else(|| GrammarError(format!("summary schema has no {list}")))?;
            items.insert("maxItems".into(), json!(0));
        }
    } else {
        for def in ["Decision", "ActionItem"] {
            let ids = schema
                .pointer_mut(&format!("/$defs/{def}/properties/source_segment_ids"))
                .and_then(Value::as_object_mut)
                .ok_or_else(|| GrammarError(format!("summary schema has no {def} citations")))?;
            ids.insert("minItems".into(), json!(1));
        }
    }
    let empty: &[&str] = match part {
        SummaryPart::Whole => &[],
        SummaryPart::ActionsOnly => &["enhanced_notes", "decisions", "open_questions", "topics"],
        SummaryPart::NoActions => &["action_items"],
    };
    for list in empty {
        schema
            .pointer_mut(&format!("/properties/{list}"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| GrammarError(format!("summary schema has no {list}")))?
            .insert("maxItems".into(), json!(0));
    }
    schema_to_gbnf(
        &schema,
        GrammarOptions {
            non_blank_strings: true,
            compact,
        },
    )
}

/// The grammar of a selection reply (T12.25 part 2, merge by selection, evaluation only;
/// production never calls it): the keys of `earshot_core::summary::SELECTION_FIELDS` in
/// that order. `decisions`, `action_items` and `open_questions` hold at most
/// `limits.max_items` (and at most as many as the list has items) numbers from 1 to that
/// list's count; `same_decisions` and `same_action_items` hold groups of 2 to `count` such
/// numbers, at most `count / 2` groups; `tldr` is non-blank. A list with no items can only be
/// empty. Whitespace as [`summary_grammar`] (indented output allowed). A number used twice is
/// left to `earshot_core::summary::parse_selection`.
///
/// # Errors
/// [`GrammarError`] only if the converter refuses the schema (it does not).
pub fn selection_grammar(
    counts: earshot_core::summary::SelectCounts,
    limits: SummaryLimits,
) -> Result<String, GrammarError> {
    let numbers = |count: usize| -> Value {
        let n: Vec<Value> = if count == 0 {
            vec![json!(1)]
        } else {
            (1..=count).map(|n| json!(n)).collect()
        };
        json!({"enum": n})
    };
    let kept = |count: usize| {
        json!({
            "type": "array",
            "maxItems": count.min(limits.max_items as usize),
            "items": numbers(count),
        })
    };
    let same = |count: usize| {
        json!({
            "type": "array",
            "maxItems": count / 2,
            "items": {
                "type": "array",
                "minItems": 2,
                "maxItems": count.max(2),
                "items": numbers(count),
            },
        })
    };
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "decisions": kept(counts.decisions),
            "action_items": kept(counts.action_items),
            "open_questions": kept(counts.open_questions),
            "same_decisions": same(counts.decisions),
            "same_action_items": same(counts.action_items),
            "tldr": {"type": "string"},
        },
        "required": earshot_core::summary::SELECTION_FIELDS,
    });
    schema_to_gbnf(
        &schema,
        GrammarOptions {
            non_blank_strings: true,
            compact: false,
        },
    )
}

/// The text that ends a thinking section (Qwen 3 and 3.5).
pub const THINK_CLOSE: &str = "</think>";

/// Rule names [`thinking_grammar`] and [`thinking_close_grammar`] add; a reply grammar that
/// already defines one is refused.
const THINK_RULES: [&str; 3] = ["think-text", "think-sep", "think-reply"];

/// A thinking section (T12.10, evaluation only; production never calls it): `reply_grammar`
/// (a GBNF grammar whose start rule is `root`, for example [`summary_grammar`]) after free
/// text, [`THINK_CLOSE`] and up to two newlines. The prompt's assistant prefix has opened the
/// section (`<think>\n`). The thinking text may hold any character but `<`, so the first
/// `</think>` always ends it; its length is not bounded here but by the generation loop
/// (`LocalLlm::generate_thinking`), which switches to [`thinking_close_grammar`] at the token
/// cap. `reply_grammar` is kept byte for byte except that its start rule is renamed
/// `think-reply`.
///
/// # Errors
/// [`GrammarError`] when `reply_grammar` does not start with a `root ::= ` rule or already
/// defines one of the added rule names.
pub fn thinking_grammar(reply_grammar: &str) -> Result<String, GrammarError> {
    let reply = renamed_reply(reply_grammar)?;
    Ok(format!(
        "root ::= think-text {close} think-sep think-reply\n\
         think-text ::= [^<]*\n\
         think-sep ::= \"\\n\"{{0,2}}\n\
         {reply}",
        close = gbnf_literal(THINK_CLOSE)
    ))
}

/// The grammar a thinking generation switches to when its thinking reaches the token cap
/// (evaluation only): an optional newline, [`THINK_CLOSE`], then the rest of
/// [`thinking_grammar`] after the thinking text.
///
/// # Errors
/// As [`thinking_grammar`].
pub fn thinking_close_grammar(reply_grammar: &str) -> Result<String, GrammarError> {
    let reply = renamed_reply(reply_grammar)?;
    Ok(format!(
        "root ::= \"\\n\"? {close} think-sep think-reply\n\
         think-sep ::= \"\\n\"{{0,2}}\n\
         {reply}",
        close = gbnf_literal(THINK_CLOSE)
    ))
}

/// `reply_grammar` with its first line's `root ::= ` renamed `think-reply ::= `.
fn renamed_reply(reply_grammar: &str) -> Result<String, GrammarError> {
    let body = reply_grammar
        .strip_prefix("root ::= ")
        .ok_or_else(|| GrammarError("the reply grammar does not start with `root ::= `".into()))?;
    for line in reply_grammar.lines() {
        let name = line.split(" ::= ").next().unwrap_or_default();
        if THINK_RULES.contains(&name) {
            return Err(GrammarError(format!(
                "the reply grammar already defines `{name}`"
            )));
        }
    }
    Ok(format!("think-reply ::= {body}"))
}

/// Caps every array and turns `source_segment_ids` items into the enum of citable
/// segment numbers.
fn narrow(v: &mut Value, ids: &[Value], limits: SummaryLimits, narrowed: &mut usize) {
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    if obj.get("type").and_then(Value::as_str) == Some("array") {
        obj.insert("maxItems".into(), json!(limits.max_items));
    }
    if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut)
        && let Some(cites) = props.get_mut("source_segment_ids")
    {
        *cites = json!({
            "type": "array",
            "items": { "enum": ids },
            "maxItems": limits.max_citations,
        });
        *narrowed += 1;
    }
    for (key, child) in obj.iter_mut() {
        if key == "source_segment_ids" {
            continue;
        }
        match child {
            Value::Object(_) => narrow(child, ids, limits, narrowed),
            Value::Array(items) => {
                for item in items {
                    narrow(item, ids, limits, narrowed);
                }
            }
            _ => {}
        }
    }
}

struct Converter<'a> {
    defs: Option<&'a Map<String, Value>>,
    options: GrammarOptions,
    rules: BTreeMap<String, String>,
    used_base: Vec<&'static str>,
}

impl Converter<'_> {
    fn base(&mut self, name: &'static str) -> String {
        if !self.used_base.contains(&name) {
            self.used_base.push(name);
        }
        name.to_string()
    }

    /// Returns a GBNF expression for `schema`; `name` names any rule it has to create.
    fn visit(&mut self, schema: &Value, name: &str) -> Result<String, GrammarError> {
        let obj = schema
            .as_object()
            .ok_or_else(|| GrammarError(format!("{name}: schema is not an object")))?;
        if let Some(r) = obj.get("$ref") {
            return self.reference(r, name);
        }
        if let Some(values) = obj.get("enum") {
            check_keys(obj, &["enum"], name)?;
            let body = enum_alternatives(values, name)?;
            return Ok(self.named(name, body));
        }
        let types: Vec<&str> = match obj.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts
                .iter()
                .map(|t| {
                    t.as_str()
                        .ok_or_else(|| GrammarError(format!("{name}: non-string type")))
                })
                .collect::<Result<_, _>>()?,
            _ => return Err(GrammarError(format!("{name}: no `type`"))),
        };
        let mut alts = Vec::new();
        for t in &types {
            alts.push(match *t {
                "object" => self.object(obj, name)?,
                "array" => self.array(obj, name)?,
                "string" => self.string(obj, name)?,
                "null" => self.base("null"),
                other => return Err(GrammarError(format!("{name}: type `{other}`"))),
            });
        }
        if types.len() > 1 {
            // Every keyword must belong to one of the listed types.
            check_keys(obj, &["type", "format"], name)?;
        }
        Ok(if alts.len() == 1 {
            alts.remove(0)
        } else {
            format!("( {} )", alts.join(" | "))
        })
    }

    fn reference(&mut self, r: &Value, name: &str) -> Result<String, GrammarError> {
        let target = r
            .as_str()
            .and_then(|s| s.strip_prefix("#/$defs/"))
            .ok_or_else(|| GrammarError(format!("{name}: only `#/$defs/<name>` refs")))?;
        let rule = rule_name(target);
        if !self.rules.contains_key(&rule) {
            let def = self
                .defs
                .and_then(|d| d.get(target))
                .ok_or_else(|| GrammarError(format!("{name}: unknown def `{target}`")))?;
            // Reserve the name first so a recursive ref terminates.
            self.rules.insert(rule.clone(), String::new());
            let expr = self.visit(def, &rule)?;
            // Objects and arrays store themselves under `rule`; anything else is an
            // expression to store.
            if expr != rule {
                self.rules.insert(rule.clone(), expr);
            }
        }
        Ok(rule)
    }

    fn object(&mut self, obj: &Map<String, Value>, name: &str) -> Result<String, GrammarError> {
        check_keys(
            obj,
            &["type", "properties", "required", "additionalProperties"],
            name,
        )?;
        if obj.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(GrammarError(format!(
                "{name}: objects need `additionalProperties: false`"
            )));
        }
        let props = obj
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| GrammarError(format!("{name}: object without `properties`")))?;
        let required: Vec<&str> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if required.len() != props.len() || !props.keys().all(|k| required.contains(&k.as_str())) {
            return Err(GrammarError(format!(
                "{name}: every property must be required (optional properties are not supported)"
            )));
        }
        let mut parts = Vec::new();
        for key in required {
            let prop_rule = format!("{name}-{}", rule_name(key));
            let value = self.visit(&props[key], &prop_rule)?;
            let lit = gbnf_literal(&serde_json::to_string(key).expect("a string serialises"));
            parts.push(format!("{lit} space \":\" space {value}"));
        }
        let body = if parts.is_empty() {
            r#""{" space "}" space"#.to_string()
        } else {
            format!(r#""{{" space {} "}}" space"#, parts.join(r#" "," space "#))
        };
        Ok(self.named(name, body))
    }

    fn array(&mut self, obj: &Map<String, Value>, name: &str) -> Result<String, GrammarError> {
        check_keys(obj, &["type", "items", "minItems", "maxItems"], name)?;
        let items = obj
            .get("items")
            .ok_or_else(|| GrammarError(format!("{name}: array without `items`")))?;
        let item = self.visit(items, &format!("{name}-item"))?;
        let bound = |k: &str| -> Result<Option<u64>, GrammarError> {
            obj.get(k)
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| GrammarError(format!("{name}: `{k}` is not a count")))
                })
                .transpose()
        };
        let min = bound("minItems")?.unwrap_or(0);
        let max = bound("maxItems")?;
        if max.is_some_and(|m| m < min) {
            return Err(GrammarError(format!("{name}: maxItems < minItems")));
        }
        let body = format!(
            r#""[" space {}"]" space"#,
            repetition(&format!("{item} "), r#""," space "#, min, max)
        );
        Ok(self.named(name, body))
    }

    fn string(&mut self, obj: &Map<String, Value>, name: &str) -> Result<String, GrammarError> {
        check_keys(obj, &["type", "format"], name)?;
        match obj.get("format").map(|f| f.as_str()) {
            None => Ok(if self.options.non_blank_strings {
                self.base("text")
            } else {
                self.base("string")
            }),
            Some(Some("uuid")) => Ok(self.base("uuid")),
            Some(f) => Err(GrammarError(format!("{name}: string format {f:?}"))),
        }
    }

    /// Stores `body` as rule `name` (unless it is the root) and returns the reference.
    fn named(&mut self, name: &str, body: String) -> String {
        if name == "root" {
            return body;
        }
        self.rules.insert(name.to_string(), body);
        name.to_string()
    }
}

/// `item (sep item){min-1,max-1}`, optional when `min == 0` (llama.cpp `build_repetition`).
fn repetition(item: &str, sep: &str, min: u64, max: Option<u64>) -> String {
    if max == Some(0) {
        return String::new();
    }
    let rest_min = min.saturating_sub(1);
    let rest = match max {
        Some(m) if m - 1 == rest_min => {
            if rest_min == 0 {
                String::new()
            } else {
                format!("( {sep}{item}){{{rest_min}}} ")
            }
        }
        Some(m) => format!("( {sep}{item}){{{rest_min},{}}} ", m - 1),
        None if rest_min == 0 => format!("( {sep}{item})* "),
        None => format!("( {sep}{item}){{{rest_min},}} "),
    };
    let one_or_more = format!("{item}{rest}");
    if min == 0 {
        format!("( {one_or_more})? ")
    } else {
        one_or_more
    }
}

fn enum_alternatives(values: &Value, name: &str) -> Result<String, GrammarError> {
    let values = values
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| GrammarError(format!("{name}: `enum` must be a non-empty list")))?;
    let mut alts = Vec::new();
    for v in values {
        if !(v.is_string() || v.is_u64()) {
            return Err(GrammarError(format!(
                "{name}: only string and non-negative integer enums"
            )));
        }
        alts.push(gbnf_literal(
            &serde_json::to_string(v).expect("a string or integer serialises"),
        ));
    }
    Ok(format!("( {} ) space", alts.join(" | ")))
}

fn check_keys(obj: &Map<String, Value>, allowed: &[&str], name: &str) -> Result<(), GrammarError> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
            return Err(GrammarError(format!("{name}: keyword `{key}`")));
        }
    }
    Ok(())
}

/// A GBNF rule name (`[a-zA-Z0-9-]+`) for a schema name.
fn rule_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `text` as a GBNF double-quoted literal.
fn gbnf_literal(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(non_blank: bool) -> GrammarOptions {
        GrammarOptions {
            non_blank_strings: non_blank,
            compact: false,
        }
    }

    #[test]
    fn grammar_object_properties_follow_required_order() {
        let s = json!({
            "type": "object", "additionalProperties": false,
            "properties": {"b": {"type": "string"}, "a": {"type": ["string", "null"]}},
            "required": ["b", "a"],
        });
        let g = schema_to_gbnf(&s, opts(false)).unwrap();
        let root = g.lines().next().unwrap();
        assert_eq!(
            root,
            r#"root ::= "{" space "\"b\"" space ":" space string "," space "\"a\"" space ":" space ( string | null ) "}" space"#
        );
        assert!(g.contains("\nnull ::= \"null\" space\n"), "{g}");
        assert!(!g.contains("\ntext ::="), "{g}");
    }

    #[test]
    fn grammar_array_bounds_follow_llama_cpp_repetition() {
        assert_eq!(
            repetition("x ", r#""," "#, 0, None),
            r#"( x ( "," x )* )? "#
        );
        assert_eq!(repetition("x ", r#""," "#, 1, None), r#"x ( "," x )* "#);
        assert_eq!(
            repetition("x ", r#""," "#, 0, Some(3)),
            r#"( x ( "," x ){0,2} )? "#
        );
        assert_eq!(repetition("x ", r#""," "#, 1, Some(1)), "x ");
        assert_eq!(
            repetition("x ", r#""," "#, 2, Some(2)),
            r#"x ( "," x ){1} "#
        );
        assert_eq!(repetition("x ", r#""," "#, 2, None), r#"x ( "," x ){1,} "#);
        assert_eq!(repetition("x ", r#""," "#, 0, Some(0)), "");
    }

    #[test]
    fn grammar_refuses_what_it_does_not_support() {
        let cases = [
            json!({"type": "integer"}),
            json!({"type": "string", "pattern": "a+"}),
            json!({"type": "string", "format": "date"}),
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": []}),
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]}),
            json!({"$ref": "#/$defs/Missing"}),
            json!({"$ref": "other.json#/x"}),
            json!({"enum": [1.5, true]}),
            json!({"enum": []}),
            json!({"type": "array", "items": {"type": "string"}, "minItems": 3, "maxItems": 2}),
            json!({"type": "array"}),
        ];
        for s in cases {
            assert!(schema_to_gbnf(&s, opts(false)).is_err(), "{s}");
        }
    }

    #[test]
    fn grammar_literals_are_escaped() {
        assert_eq!(gbnf_literal(r#""a\b""#), r#""\"a\\b\"""#);
        let g = schema_to_gbnf(&json!({"enum": ["say \"hi\""]}), opts(false)).unwrap();
        assert_eq!(
            g.lines().next().unwrap(),
            r#"root ::= ( "\"say \\\"hi\\\"\"" ) space"#
        );
    }

    #[test]
    fn grammar_converts_the_whole_summary_schema() {
        let g = schema_to_gbnf(&earshot_core::summary::json_schema(), opts(false)).unwrap();
        for rule in [
            "ActionItem ::=",
            "Decision ::=",
            "NoteBullet ::=",
            "NoteSection ::=",
            "uuid ::=",
        ] {
            assert!(g.contains(rule), "{rule} missing:\n{g}");
        }
        for field in [
            "title",
            "tldr",
            "enhanced_notes",
            "decisions",
            "action_items",
            "open_questions",
            "topics",
        ] {
            assert!(
                g.contains(&format!(r#""\"{field}\"""#)),
                "{field} missing:\n{g}"
            );
        }
    }

    #[test]
    fn grammar_for_a_summary_cites_only_the_requests_segment_numbers() {
        // Segments 4 and 5 (indices 3 and 4) of the meeting, as a map part may cite them.
        let g = summary_grammar(&(3..5).into(), SummaryLimits::default()).unwrap();
        let cite = r#"( "4" | "5" ) space"#;
        assert!(g.contains(cite), "{g}");
        assert!(!g.contains("uuid ::="), "no UUIDs:\n{g}");
        // Decisions and action items cite at least one segment; bullets may cite none.
        let rule = |name: &str| {
            g.lines()
                .find(|l| l.starts_with(&format!("{name} ::= ")))
                .unwrap_or_else(|| panic!("{name} missing:\n{g}"))
                .to_string()
        };
        // At most three citations (MAX_CITATIONS).
        assert!(rule("Decision-source-segment-ids").contains(r#""[" space Decision-source-segment-ids-item ( "," space Decision-source-segment-ids-item ){0,2} "]""#), "{g}");
        assert!(
            rule("ActionItem-source-segment-ids")
                .contains(r#""[" space ActionItem-source-segment-ids-item ( "," space"#),
            "{g}"
        );
        assert!(
            rule("NoteBullet-source-segment-ids")
                .contains(r#""[" space ( NoteBullet-source-segment-ids-item"#),
            "{g}"
        );
        // Text is never blank; `due` stays nullable.
        assert!(g.contains("\ntext ::= "), "{g}");
        assert!(!g.contains("\nstring ::= "), "{g}");
        assert!(
            rule("ActionItem").contains(r#""\"due\"" space ":" space ( text | null )"#),
            "{g}"
        );
        // Every list is capped.
        assert!(rule("root-topics").contains("{0,11}"), "{g}");
    }

    /// A valid reply citing segments 1 and 2.
    fn sample_reply() -> Value {
        json!({
            "title": "Remote kick-off",
            "tldr": "The team agreed on a yellow remote.",
            "enhanced_notes": [{"heading": "Design", "bullets": [
                {"text": "Yellow \"fruity\" case, 25 euros.", "source_segment_ids": [1, 2]}]}],
            "decisions": [{"text": "The remote is yellow.", "source_segment_ids": [2]}],
            "action_items": [{"owner": "Me", "task": "Send the minutes", "due": null,
                              "source_segment_ids": [1]}],
            "open_questions": [],
            "topics": ["design"],
        })
    }

    #[test]
    fn grammar_compact_summary_accepts_only_compact_json() {
        use crate::gbnf_match::Gbnf;
        let g = summary_grammar_with(&(0..2).into(), SummaryLimits::default(), true).unwrap();
        assert!(g.contains("\nspace ::= \"\"\n"), "{g}");
        // Field order of the schema's `required` lists, as a model writes the object.
        let compact = r#"{"title":"Remote kick-off","tldr":"The team agreed on a yellow remote.","enhanced_notes":[{"heading":"Design","bullets":[{"text":"Yellow \"fruity\" case, 25 euros.","source_segment_ids":[1,2]}]}],"decisions":[{"text":"The remote is yellow.","source_segment_ids":[2]}],"action_items":[{"owner":"Me","task":"Send the minutes","due":null,"source_segment_ids":[1]}],"open_questions":[],"topics":["design"]}"#;
        assert_eq!(
            serde_json::from_str::<Value>(compact).unwrap(),
            sample_reply()
        );
        // serde_json would sort keys; build the indented form in the grammar's field order.
        let pretty = indent(compact);
        assert!(pretty.contains("\n  "), "{pretty}");
        let gbnf = Gbnf::parse(&g);
        assert!(gbnf.accepts(compact), "compact form rejected:\n{compact}");
        assert!(!gbnf.accepts(&pretty), "indented form accepted");
        // One space after a colon is still whitespace: rejected too.
        assert!(!gbnf.accepts(&compact.replacen("\":", "\": ", 1)));
        // Negative controls: the recogniser is not vacuous.
        let unknown = compact.replace("[2]", "[3]");
        assert!(
            !gbnf.accepts(&unknown),
            "a segment the request may not cite accepted"
        );
        assert!(
            !gbnf.accepts(&compact[..compact.len() - 1]),
            "a cut reply accepted"
        );
        // The production summary grammar is the indented one: both forms, citable numbers only.
        let default =
            Gbnf::parse(&summary_grammar(&(0..2).into(), SummaryLimits::default()).unwrap());
        assert!(default.accepts(compact));
        assert!(default.accepts(&pretty));
        assert!(!default.accepts(&unknown));
        assert!(
            !default.accepts(&compact.replace("[1,2]", "[1,2,1,2]")),
            "over the cap"
        );
    }

    /// `json` (compact, no whitespace inside strings that matters here) re-indented with
    /// two spaces per level, keeping key order.
    fn indent(json: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        let mut chars = json.chars().peekable();
        while let Some(c) = chars.next() {
            if in_str {
                out.push(c);
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => {
                    in_str = true;
                    out.push(c);
                }
                '[' if chars.peek() == Some(&']') => {
                    chars.next();
                    out.push_str("[]");
                }
                '{' | '[' => {
                    depth += 1;
                    out.push(c);
                    out.push('\n');
                    out.push_str(&"  ".repeat(depth));
                }
                '}' | ']' => {
                    depth -= 1;
                    out.push('\n');
                    out.push_str(&"  ".repeat(depth));
                    out.push(c);
                }
                ',' => {
                    out.push(',');
                    out.push('\n');
                    out.push_str(&"  ".repeat(depth));
                }
                ':' => out.push_str(": "),
                c => out.push(c),
            }
        }
        out
    }

    #[test]
    fn grammar_with_nothing_citable_takes_exactly_the_citation_free_replies() {
        // A merge whose partials cite nothing: the remote path validates a reply without
        // citations, decisions or action items, so the local grammar must allow it.
        use crate::gbnf_match::Gbnf;
        let nothing = earshot_core::summary::Citable::default();
        let g = Gbnf::parse(&summary_grammar(&nothing, SummaryLimits::default()).unwrap());
        // Field order of the schema's `required` lists (serde_json would sort keys).
        let free = r#"{"title":"Remote kick-off","tldr":"The team met.","enhanced_notes":[{"heading":"Design","bullets":[{"text":"A yellow remote","source_segment_ids":[]}]}],"decisions":[],"action_items":[],"open_questions":["Which colour?"],"topics":["design"]}"#;
        assert!(g.accepts(free), "{free}");
        assert!(g.accepts(&indent(free)));
        let value: Value = serde_json::from_str(free).unwrap();
        let numbers = earshot_core::summary::SegmentNumbers::new([]);
        assert!(earshot_core::summary::validate_reply(&value, &numbers, &nothing).is_ok());
        // Any citation, decision or action item is refused, as validation refuses it.
        let cited = free.replace(r#""source_segment_ids":[]"#, r#""source_segment_ids":[1]"#);
        assert!(!g.accepts(&cited));
        let decided = free.replace(
            r#""decisions":[]"#,
            r#""decisions":[{"text":"Yellow","source_segment_ids":[]}]"#,
        );
        assert!(!g.accepts(&decided));
        let decided_value: Value = serde_json::from_str(&decided).unwrap();
        assert!(earshot_core::summary::validate_reply(&decided_value, &numbers, &nothing).is_err());
        let acted = free.replace(
            r#""action_items":[]"#,
            r#""action_items":[{"owner":"Ana","task":"Draw it","due":null,"source_segment_ids":[]}]"#,
        );
        assert!(!g.accepts(&acted));
        // With something citable the grammar still requires a decision to cite.
        let some = Gbnf::parse(&summary_grammar(&(0..2).into(), SummaryLimits::default()).unwrap());
        assert!(some.accepts(free));
        assert!(!some.accepts(&decided));
    }

    /// T12.25 part 2: the production summary grammar is pinned byte for byte, so evaluation
    /// work beside it (the selection merge) cannot change it. Regenerate with
    /// `EARSHOT_UPDATE_SNAPSHOTS=1` only for an intended production change, and review it.
    #[test]
    fn grammar_production_summary_grammar_is_pinned_byte_for_byte() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/snapshots/summary_grammar.gbnf");
        let limits = SummaryLimits::default();
        let mut text = String::new();
        for cites in [Citable::from(0..5), Citable::from(0..0)] {
            text.push_str(&summary_grammar(&cites, limits).unwrap());
            text.push_str("----\n");
        }
        if std::env::var_os("EARSHOT_UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        let pinned = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(text, pinned, "the production grammar changed");
    }

    #[test]
    fn grammar_selection_takes_only_numbers_in_range_and_a_tldr() {
        use crate::gbnf_match::Gbnf;
        use earshot_core::summary::SelectCounts;
        let counts = SelectCounts {
            decisions: 4,
            action_items: 2,
            open_questions: 0,
        };
        let g = Gbnf::parse(&selection_grammar(counts, SummaryLimits::default()).unwrap());
        let ok = r#"{"decisions":[1,4,3],"action_items":[2],"open_questions":[],"same_decisions":[[1,4]],"same_action_items":[],"tldr":"A yellow remote."}"#;
        assert!(g.accepts(ok));
        assert!(g.accepts(&indent(ok)));
        assert!(g.accepts(
            r#"{"decisions":[],"action_items":[],"open_questions":[],"same_decisions":[],"same_action_items":[],"tldr":"Nothing."}"#
        ));
        for bad in [
            ok.replace("[1,4,3]", "[1,5]"), // past the decisions
            ok.replace(r#""action_items":[2]"#, r#""action_items":[3]"#), // past the actions
            ok.replace("[[1,4]]", "[[1]]"), // a group of one
            ok.replace("[[1,4]]", "[[1,4],[2,3],[1,2]]"), // more groups than 4 / 2
            ok.replace("[1,4,3]", "[[1],[4]]"), // groups where numbers go
            ok.replace(r#""open_questions":[]"#, r#""open_questions":[1]"#), // none to give
            ok.replace(r#""A yellow remote.""#, r#""""#), // a blank tldr
            ok.replace(r#""A yellow remote.""#, r#"" x""#), // leading space
            ok.replace("[1,4,3]", "[0]"),   // numbers start at 1
            ok.replace(r#""decisions""#, r#""title":"t","decisions""#), // another field
            ok.replace(r#","same_action_items":[]"#, ""), // a missing field
        ] {
            assert_ne!(bad, ok);
            assert!(!g.accepts(&bad), "{bad}");
        }
        // The list cap applies (2 here), and never more numbers than items.
        let capped = Gbnf::parse(
            &selection_grammar(
                counts,
                SummaryLimits {
                    max_items: 2,
                    ..SummaryLimits::default()
                },
            )
            .unwrap(),
        );
        assert!(capped.accepts(&ok.replace("[1,4,3]", "[1,4]")));
        assert!(!capped.accepts(ok));
        assert!(!g.accepts(&ok.replace(r#""action_items":[2]"#, r#""action_items":[2,1,2]"#)));
    }

    #[test]
    fn grammar_thinking_wraps_the_production_grammar_unchanged() {
        let cites: Citable = (0..2).into();
        let production = summary_grammar(&cites, SummaryLimits::default()).unwrap();
        // The production grammar knows nothing of thinking.
        assert!(!production.contains("think"), "{production}");
        for g in [
            thinking_grammar(&production).unwrap(),
            thinking_close_grammar(&production).unwrap(),
        ] {
            // Everything after the added rules is the production grammar, its start rule
            // renamed and nothing else changed.
            let at = g.find("think-reply ::= ").unwrap();
            assert_eq!(
                &g[at + "think-reply ::= ".len()..],
                &production["root ::= ".len()..]
            );
        }
        // A grammar without a root rule, or one already using an added name, is refused.
        assert!(thinking_grammar("x ::= \"a\"\n").is_err());
        assert!(thinking_grammar("root ::= think-sep\nthink-sep ::= \"a\"\n").is_err());
        assert!(thinking_close_grammar("x ::= \"a\"\n").is_err());
    }

    #[test]
    fn grammar_thinking_accepts_a_think_section_then_the_summary() {
        use crate::gbnf_match::Gbnf;
        let cites: Citable = (0..2).into();
        let production = summary_grammar(&cites, SummaryLimits::default()).unwrap();
        let think = Gbnf::parse(&thinking_grammar(&production).unwrap());
        let close = Gbnf::parse(&thinking_close_grammar(&production).unwrap());
        let plain = Gbnf::parse(&production);
        let reply = r#"{"title":"Remote kick-off","tldr":"The team agreed on a yellow remote.","enhanced_notes":[],"decisions":[{"text":"The remote is yellow.","source_segment_ids":[2]}],"action_items":[],"open_questions":[],"topics":["design"]}"#;
        assert!(plain.accepts(reply));
        // The prefix opened the section: thinking text, the close, the summary.
        let thought =
            format!("The team chose yellow in segment 2. Nothing else.\n</think>\n\n{reply}");
        assert!(think.accepts(&thought), "{thought}");
        assert!(think.accepts(&format!("</think>{reply}")), "empty thinking");
        assert!(think.accepts(&format!("x</think>\n\n{}", indent(reply))));
        // Negative controls: no close, two closes, `<` in the thinking, a bad summary, more
        // than two newlines, a summary without thinking.
        assert!(!think.accepts(&format!("thinking {reply}")));
        assert!(!think.accepts(&format!("a</think></think>{reply}")));
        assert!(!think.accepts(&format!("a < b</think>{reply}")));
        assert!(!think.accepts(&format!("a</think>{}", reply.replace("[2]", "[3]"))));
        assert!(!think.accepts(&format!("a</think>\n\n\n{reply}")));
        assert!(!think.accepts(reply));
        // The production grammar takes no thinking at all.
        assert!(!plain.accepts(&thought));
        // At the cap the close grammar takes only the close and the summary.
        assert!(close.accepts(&format!("</think>\n\n{reply}")));
        assert!(close.accepts(&format!("\n</think>{reply}")));
        assert!(!close.accepts(&format!("more thinking</think>{reply}")));
        assert!(!close.accepts(reply));
    }

    #[test]
    fn grammar_summary_parts_empty_the_lists_a_two_call_request_leaves_out() {
        use crate::gbnf_match::Gbnf;
        let cites: Citable = (0..2).into();
        let limits = SummaryLimits::default();
        // Whole is the production grammar, byte for byte.
        assert_eq!(
            summary_part_grammar(&cites, limits, SummaryPart::Whole).unwrap(),
            summary_grammar(&cites, limits).unwrap()
        );
        let full = r#"{"title":"Remote kick-off","tldr":"The team agreed on a yellow remote.","enhanced_notes":[{"heading":"Design","bullets":[{"text":"Yellow case","source_segment_ids":[1]}]}],"decisions":[{"text":"The remote is yellow.","source_segment_ids":[2]}],"action_items":[{"owner":"Me","task":"Send the minutes","due":null,"source_segment_ids":[1]}],"open_questions":["Which colour?"],"topics":["design"]}"#;
        let actions_only = r#"{"title":"Part 1","tldr":"Tasks of part 1.","enhanced_notes":[],"decisions":[],"action_items":[{"owner":"Me","task":"Send the minutes","due":null,"source_segment_ids":[1]}],"open_questions":[],"topics":[]}"#;
        let no_actions = full.replace(
            r#""action_items":[{"owner":"Me","task":"Send the minutes","due":null,"source_segment_ids":[1]}]"#,
            r#""action_items":[]"#,
        );
        assert_ne!(no_actions, full);
        let g = |part| Gbnf::parse(&summary_part_grammar(&cites, limits, part).unwrap());
        let (whole, actions, rest) = (
            g(SummaryPart::Whole),
            g(SummaryPart::ActionsOnly),
            g(SummaryPart::NoActions),
        );
        for text in [full, actions_only, no_actions.as_str()] {
            assert!(whole.accepts(text), "{text}");
        }
        assert!(actions.accepts(actions_only));
        assert!(actions.accepts(&indent(actions_only)));
        assert!(
            !actions.accepts(full),
            "notes and decisions in an actions-only reply"
        );
        assert!(rest.accepts(&no_actions));
        assert!(rest.accepts(&indent(&no_actions)));
        assert!(!rest.accepts(full), "action items in a no-actions reply");
        // Each left-out list is refused on its own.
        for (list, item) in [
            ("enhanced_notes", r#"{"heading":"H","bullets":[]}"#),
            ("decisions", r#"{"text":"D","source_segment_ids":[1]}"#),
            ("open_questions", r#""Q?""#),
            ("topics", r#""t""#),
        ] {
            let filled =
                actions_only.replace(&format!("\"{list}\":[]"), &format!("\"{list}\":[{item}]"));
            assert_ne!(filled, actions_only, "{list}");
            assert!(!actions.accepts(&filled), "{list}");
            assert!(whole.accepts(&filled), "{list}");
        }
        // An actions-only reply still needs a non-blank title and tldr, and citable numbers.
        assert!(
            !actions.accepts(&actions_only.replace(r#""tldr":"Tasks of part 1.""#, r#""tldr":"""#))
        );
        assert!(!actions.accepts(&actions_only.replace("[1]}", "[3]}")));
    }
}
