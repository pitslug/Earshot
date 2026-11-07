//! Finding the JSON object in a model reply (PLAN.md 3.7 plain-text fallback). Applied in
//! every response mode, because servers that accept `response_format` still sometimes
//! wrap the object in a code fence or add prose.

use serde_json::Value;

/// At most this many `{` positions are tried in the brace scan, so a hostile or broken
/// reply cannot make extraction quadratic.
const MAX_BRACE_CANDIDATES: usize = 64;

/// The JSON value in `text`: the whole text if it is JSON; otherwise the first fenced
/// code block that holds a JSON object; otherwise the first balanced `{...}` span that
/// parses as an object. On failure, the parse error of the whole text (or "empty reply").
pub fn extract_json(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty reply".into());
    }
    let whole_err = match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => return Ok(v),
        Err(e) => e.to_string(),
    };
    for block in fenced_blocks(trimmed) {
        if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(block.trim()) {
            return Ok(v);
        }
    }
    for span in brace_spans(trimmed) {
        if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(span) {
            return Ok(v);
        }
    }
    Err(whole_err)
}

/// Contents of Markdown code fences (```` ``` ```` with an optional info string such as
/// `json`). An unclosed final fence yields the rest of the text.
fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        // Skip the info string up to the end of the line.
        let body_start = after.find('\n').map_or(after.len(), |i| i + 1);
        let body = &after[body_start..];
        match body.find("```") {
            Some(close) => {
                out.push(&body[..close]);
                rest = &body[close + 3..];
            }
            None => {
                out.push(body);
                break;
            }
        }
    }
    out
}

/// Balanced `{...}` spans, starting at each of the first [`MAX_BRACE_CANDIDATES`] `{` in
/// order; braces inside JSON strings are ignored.
fn brace_spans(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices('{')
        .take(MAX_BRACE_CANDIDATES)
        .filter_map(move |(start, _)| {
            matching_brace(&text[start..]).map(|end| &text[start..start + end])
        })
}

/// Byte length of the balanced object at the start of `s` (which starts with `{`).
fn matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_text_json_is_returned() {
        assert_eq!(extract_json(" {\"a\": 1}\n").unwrap(), json!({"a": 1}));
        assert_eq!(extract_json("[1]").unwrap(), json!([1]));
    }

    #[test]
    fn fenced_json_is_unwrapped() {
        let t = "Here you go:\n```json\n{\"a\": \"x ``` y\"}\n```\nThanks";
        // The fence closes at the first ``` after the body starts; that inner one breaks
        // the block, so the brace scan finds the object instead.
        assert_eq!(extract_json(t).unwrap(), json!({"a": "x ``` y"}));
        let t = "```\n{\"b\": [1, 2]}\n```";
        assert_eq!(extract_json(t).unwrap(), json!({"b": [1, 2]}));
        let t = "```json\n{\"c\": true}";
        assert_eq!(extract_json(t).unwrap(), json!({"c": true}));
    }

    #[test]
    fn object_surrounded_by_prose_is_found() {
        let t = "Sure! {\"title\": \"T {braces} \\\"quoted\\\"\", \"n\": {\"m\": 1}} Hope this helps {:";
        assert_eq!(
            extract_json(t).unwrap(),
            json!({"title": "T {braces} \"quoted\"", "n": {"m": 1}})
        );
    }

    #[test]
    fn a_broken_first_candidate_does_not_hide_a_later_object() {
        let t = "Example: {not json} and the answer: {\"ok\": 1}";
        assert_eq!(extract_json(t).unwrap(), json!({"ok": 1}));
    }

    #[test]
    fn truncated_or_missing_json_reports_the_parse_error() {
        let e = extract_json("{\"title\": \"T\", \"tldr\": \"cut").unwrap_err();
        assert!(e.contains("EOF"), "{e}");
        assert_eq!(extract_json("  \n ").unwrap_err(), "empty reply");
        assert!(extract_json("no json here").is_err());
    }

    #[test]
    fn many_braces_stay_bounded() {
        let t = "{".repeat(100_000);
        assert!(extract_json(&t).is_err());
    }
}
