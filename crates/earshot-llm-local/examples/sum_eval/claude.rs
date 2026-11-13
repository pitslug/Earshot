//! A Claude ceiling without an API key (T6.20): the exact requests the
//! Anthropic provider path would send are exported as files, Claude Code agents answer
//! them, and their replies are imported and scored like any local candidate.
//!
//! - `--export-prompts <dir>`: for every meeting of each job, the plan of
//!   `earshot_core::prompt::plan` with [`ANTHROPIC_DEFAULT_BUDGET`] (what
//!   `LlmConfig::anthropic` uses). Each request step goes to `<meeting>.<source>.system.txt`
//!   (the `system` field: system messages joined by a blank line) and
//!   `<meeting>.<source>.user.txt` (the user turn: consecutive user messages joined by a
//!   blank line), as `earshot_llm::MessagesClient` sends them. The reply schema
//!   (`SUMMARY_LLM_SCHEMA_JSON`) goes to `schema.json`, and `index.json` lists every file
//!   with its reply budget and citable segment numbers. A meeting whose plan needs more
//!   than one request has its map steps exported as `<meeting>.<source>.part<k>.*` and is
//!   marked `"single": false`: its merge step depends on the map replies and cannot be
//!   exported ahead of time, so its import counts as a failure. The requests carry the
//!   Anthropic writer's shared rules ([`EXPORT_RULES`], standard), recorded as `rules` in
//!   `index.json` and every entry.
//! - `--import-replies <dir> --label <name>`: reads `<meeting>.<source>.reply.json` (the raw
//!   JSON the model returned), validates it with `earshot_core::summary::validate_reply`
//!   against the same segment numbers and citable set, and scores it with `score.rs`
//!   (ROUGE and all three item rules). A missing, unparsable or invalid reply is a failed
//!   meeting (scores 0, items missed), never dropped.

use std::path::{Path, PathBuf};

use std::borrow::Cow;

use earshot_core::prompt::{
    ChatMessage, Prompt, PromptPlan, Role, SummaryInput, SummaryRules, plan,
};
use earshot_core::summary::validate_reply;
use earshot_llm::{ANTHROPIC_DEFAULT_BUDGET, SUMMARY_LLM_SCHEMA_JSON};
use serde_json::{Value, json};

/// The shared rules the exported requests carry: the Anthropic API writer's
/// (`earshot_llm::LlmClient::summary_rules`, T12.17c), whatever the input says, because the
/// export is the request that writer would send. Recorded in `index.json` and every entry; the
/// agent-CLI candidates (`--claude-code`, `--codex`) write with the brief rules instead.
pub const EXPORT_RULES: SummaryRules = SummaryRules::Standard;

/// `input` with [`EXPORT_RULES`].
fn with_export_rules(input: &SummaryInput) -> Cow<'_, SummaryInput> {
    if input.rules == EXPORT_RULES {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(SummaryInput {
            rules: EXPORT_RULES,
            ..input.clone()
        })
    }
}

/// The name [`EXPORT_RULES`] is recorded under.
fn export_rules_name() -> Value {
    serde_json::to_value(EXPORT_RULES).unwrap_or(Value::Null)
}

/// The request steps of `input` under the Anthropic default budget with [`EXPORT_RULES`], and
/// whether the plan is one request.
pub fn steps(input: &SummaryInput) -> Result<(Vec<Prompt>, bool), String> {
    match plan(&with_export_rules(input), ANTHROPIC_DEFAULT_BUDGET).map_err(|e| e.to_string())? {
        PromptPlan::Single(p) => Ok((vec![p], true)),
        PromptPlan::MapReduce(chunks) => {
            Ok((chunks.into_iter().map(|c| c.prompt).collect(), false))
        }
    }
}

/// The `system` text and the single user turn of a first request, joined as the Messages
/// client joins them. A first request holds no assistant turn.
pub fn system_and_user(messages: &[ChatMessage]) -> Result<(String, String), String> {
    let mut system = Vec::new();
    let mut user = Vec::new();
    for m in messages {
        match m.role {
            Role::System => system.push(m.content.as_str()),
            Role::User => user.push(m.content.as_str()),
            Role::Assistant => return Err("a first request has no assistant turn".into()),
        }
    }
    Ok((system.join("\n\n"), user.join("\n\n")))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The citable segment numbers of a step, as runs (`1-40`, `41-80`).
fn citable_text(p: &Prompt) -> Vec<String> {
    p.cites
        .runs()
        .iter()
        .map(|r| format!("{}-{}", r.start + 1, r.end))
        .collect()
}

/// Writes the export files for one meeting and source; returns its index entry.
pub fn export_one(
    dir: &Path,
    meeting: &str,
    source: &str,
    input: &SummaryInput,
) -> Result<Value, String> {
    let (steps, single) = steps(input)?;
    let mut files = Vec::new();
    for (k, p) in steps.iter().enumerate() {
        let stem = if single {
            format!("{meeting}.{source}")
        } else {
            format!("{meeting}.{source}.part{}", k + 1)
        };
        let (system, user) = system_and_user(&p.messages)?;
        write(&dir.join(format!("{stem}.system.txt")), &system)?;
        write(&dir.join(format!("{stem}.user.txt")), &user)?;
        files.push(json!({
            "system": format!("{stem}.system.txt"),
            "user": format!("{stem}.user.txt"),
            "reply": format!("{stem}.reply.json"),
            "max_tokens": p.max_output_tokens,
            "citable_segment_numbers": citable_text(p),
            "estimated_prompt_tokens": p.estimated_tokens,
        }));
    }
    Ok(json!({
        "meeting": meeting,
        "source": source,
        "single": single,
        "rules": export_rules_name(),
        "segments": input.segments.len(),
        "steps": files,
        "note": if single { Value::Null } else { json!(
            "the plan needs map-reduce: only the map steps are exported; the merge step needs the map replies, so this meeting cannot be scored from exported prompts") },
    }))
}

/// Writes `schema.json` and `index.json` for `entries`.
pub fn export_finish(dir: &Path, entries: Vec<Value>) -> Result<(), String> {
    write(&dir.join("schema.json"), SUMMARY_LLM_SCHEMA_JSON)?;
    let index = json!({
        "task": "T6.20 Claude ceiling via Claude Code agents (exported prompts, no API call)",
        "budget": {
            "context_window": ANTHROPIC_DEFAULT_BUDGET.context_window,
            "max_output_tokens": ANTHROPIC_DEFAULT_BUDGET.max_output_tokens,
        },
        "rules": export_rules_name(),
        "how": "For each step, send `system` as the system prompt and `user` as the one user turn; the reply must be one JSON object following schema.json (citations are the [n] segment numbers of the transcript, only the listed citable numbers, at most three per item). Save the raw JSON as `reply`.",
        "schema": "schema.json",
        "entries": entries,
    });
    write(
        &dir.join("index.json"),
        &serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?,
    )
}

/// The imported reply for one meeting and source: the summary, or why there is none.
pub fn import_one(
    dir: &Path,
    meeting: &str,
    source: &str,
    input: &SummaryInput,
) -> Result<earshot_core::summary::CheckedReply, String> {
    let (steps, single) = steps(input)?;
    if !single {
        return Err(format!(
            "the plan needs {} requests and a merge; exported prompts cover single requests only",
            steps.len()
        ));
    }
    let path: PathBuf = dir.join(format!("{meeting}.{source}.reply.json"));
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("no reply {}: {e}", path.display()))?;
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("{}: not JSON: {e}", path.display()))?;
    let numbers = earshot_core::prompt::segment_numbers(input);
    validate_reply(&value, &numbers, &steps[0].cites).map_err(|errors| {
        let list: Vec<String> = errors.iter().map(ToString::to_string).collect();
        format!("invalid reply: {}", list.join("; "))
    })
}

#[cfg(test)]
mod tests {
    use earshot_core::asr::MeetingTime;
    use earshot_core::identity::EntityId;
    use earshot_core::prompt::{MeetingInfo, PromptSegment, Template};

    use super::*;

    fn input(n: usize) -> SummaryInput {
        SummaryInput {
            meeting: MeetingInfo {
                title: None,
                started: None,
                duration_ms: 60_000,
                attendees: vec!["Project Manager".into()],
            },
            segments: (0..n)
                .map(|i| PromptSegment {
                    id: format!("0193a5b0-1c00-7000-8000-{:012x}", i + 1)
                        .parse::<EntityId>()
                        .unwrap(),
                    start: MeetingTime::from_millis(i as u64 * 1000),
                    speaker: "Project Manager".into(),
                    text: format!("Point {i}: the remote is yellow."),
                })
                .collect(),
            notes: Vec::new(),
            template: Template::General,
            prompt_variant: Default::default(),
            rules: Default::default(),
            style: Default::default(),
            style_preset: Default::default(),
            vocabulary: Default::default(),
            user_name: None,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("earshot-sum-eval-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn claude_export_carries_and_records_the_anthropic_writers_rules() {
        let dir = scratch("claude-rules");
        let standard = input(3);
        let brief = SummaryInput {
            rules: SummaryRules::Brief,
            ..standard.clone()
        };
        let (a, _) = steps(&standard).unwrap();
        let (b, _) = steps(&brief).unwrap();
        assert_eq!(
            a[0].messages, b[0].messages,
            "the input's rules do not apply"
        );
        let entry = export_one(&dir, "ES2004a", "reference", &brief).unwrap();
        export_finish(&dir, vec![entry.clone()]).unwrap();
        assert_eq!(entry["rules"], "standard");
        let index: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("index.json")).unwrap())
                .unwrap();
        assert_eq!(index["rules"], "standard");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_export_writes_the_request_as_sent_and_import_validates_replies() {
        let dir = scratch("claude");
        let inp = input(3);
        let entry = export_one(&dir, "ES2004a", "reference", &inp).unwrap();
        export_finish(&dir, vec![entry.clone()]).unwrap();
        assert_eq!(entry["single"], true);
        let (steps, _) = steps(&inp).unwrap();
        let (system, user) = system_and_user(&steps[0].messages).unwrap();
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        assert_eq!(read("ES2004a.reference.system.txt"), system);
        assert_eq!(read("ES2004a.reference.user.txt"), user);
        assert!(user.contains("[1] "), "{user}");
        assert_eq!(read("schema.json"), SUMMARY_LLM_SCHEMA_JSON);
        let index: Value = serde_json::from_str(&read("index.json")).unwrap();
        assert_eq!(
            index["entries"][0]["steps"][0]["reply"],
            "ES2004a.reference.reply.json"
        );
        assert_eq!(
            index["entries"][0]["steps"][0]["citable_segment_numbers"],
            json!(["1-3"])
        );
        // No reply: a failure, not a skip.
        assert!(
            import_one(&dir, "ES2004a", "reference", &inp)
                .unwrap_err()
                .contains("no reply")
        );
        // A valid reply citing segment numbers imports with segment ids.
        let reply = r#"{"title":"T","tldr":"The remote is yellow.","enhanced_notes":[{"heading":"Design","bullets":[{"text":"Yellow","source_segment_ids":[1]}]}],"decisions":[{"text":"Yellow remote","source_segment_ids":[2]}],"action_items":[],"open_questions":[],"topics":["design"]}"#;
        std::fs::write(dir.join("ES2004a.reference.reply.json"), reply).unwrap();
        let ok = import_one(&dir, "ES2004a", "reference", &inp).unwrap();
        assert_eq!(
            ok.summary.decisions[0].source_segment_ids,
            [inp.segments[1].id]
        );
        // A number outside the meeting is invalid, as on every provider path.
        std::fs::write(
            dir.join("ES2004a.reference.reply.json"),
            reply.replace("[2]", "[9]"),
        )
        .unwrap();
        assert!(
            import_one(&dir, "ES2004a", "reference", &inp)
                .unwrap_err()
                .contains("invalid reply")
        );
        std::fs::write(dir.join("ES2004a.reference.reply.json"), "Here you go: {").unwrap();
        assert!(
            import_one(&dir, "ES2004a", "reference", &inp)
                .unwrap_err()
                .contains("not JSON")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_first_request_has_no_assistant_turn() {
        let m = [
            ChatMessage::new(Role::System, "a"),
            ChatMessage::new(Role::System, "b"),
            ChatMessage::new(Role::User, "u"),
        ];
        assert_eq!(system_and_user(&m).unwrap(), ("a\n\nb".into(), "u".into()));
        assert!(system_and_user(&[ChatMessage::new(Role::Assistant, "x")]).is_err());
    }
}
