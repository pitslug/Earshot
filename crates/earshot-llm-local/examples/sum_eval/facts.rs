//! Atomic facts and the local meaning judge (T6.20a).
//!
//! AMI annotates decisions and actions as long sentences that bundle several items, so one
//! verdict per sentence undercounts a summary that lists them separately and hides a
//! contradiction of one part. Here each reference sentence is split once into atomic facts
//! (`--split-facts`, a local model; the result is committed as `fixtures/ami/atomic_facts.json`
//! and spot-checked by hand), and each fact is judged against a summary by a local model
//! (`--judge-facts`) as covered, contradicted or missing.
//!
//! The facts replace the sentences in the existing judge packets (`--facts` with
//! `--judge-packets` and `--import-verdicts`, `judge.rs`): ids become `D1.1`, `D1.2`, ... and
//! the verdict file format, blinding, fingerprints and import rules do not change, so a judge
//! agent and the local judge write the same files. `--agreement` compares two such verdict
//! directories: that is how the local judge is validated on a hand-labelled dev sample.
//!
//! Rates count facts, not sentences. A contradiction counts against a model more than a miss:
//! [`net_score`] is the coverage rate minus [`CONTRADICTION_WEIGHT`] times the contradiction
//! rate (the weight was written before measuring and is a judgement, not derived).

use std::collections::BTreeMap;
use std::path::Path;

use earshot_core::metrics::items::content_tokens;
use earshot_core::prompt::{ChatMessage, Role};
use earshot_llm_local::grammar::{GrammarOptions, schema_to_gbnf};
use earshot_llm_local::{LocalLlm, Stop};
use serde_json::{Value, json};

use crate::judge;

/// What a contradiction costs against one covered fact in [`net_score`].
pub const CONTRADICTION_WEIGHT: f64 = 2.0;
/// Most facts one sentence may give (the longest AMI sentences bundle about six).
const MAX_FACTS: u32 = 12;
/// A fact keeps at least this share of its content tokens from the sentence, else the split
/// added words of its own and the whole sentence stays one fact.
pub const FACT_SOURCE_SHARE: f64 = 0.5;
/// The facts together hold at least this share of the sentence's content tokens, else the
/// split lost part of the sentence and the whole sentence stays one fact.
pub const SENTENCE_COVER_SHARE: f64 = 0.7;
/// Reply budget of one split.
const SPLIT_TOKENS: usize = 1024;
/// Reply budget of one verdict; the second try asks for a shorter reason.
const JUDGE_TOKENS: usize = 256;
/// Format of the committed facts file.
const FILE_FORMAT: u64 = 1;

/// The split prompt. A change means splitting again and re-judging.
const SPLIT_SYSTEM: &str = "You split one sentence from a meeting's official record into \
atomic facts. An atomic fact states exactly one decision, choice, requirement or task.\n\
Rules:\n\
- Use the sentence's own words. Add nothing it does not say.\n\
- Each fact must make sense alone: repeat the subject or topic where the sentence shares it \
between facts.\n\
- Keep every condition and hedge with the fact it belongs to, such as \"possibly\" or \"if it \
is in the budget\".\n\
- Keep who is responsible and any deadline in the fact.\n\
- Every part of the sentence must appear in some fact.\n\
- A sentence that states one thing stays one fact, unchanged.\n\
Reply with JSON only: {\"facts\": [\"...\"]}";

/// The judge prompt: the verdict rules of docs/summary-eval.md, per fact. This is the one
/// allowed revision (T6.20a's rule), and it still fails that rule on the gold set: the judge
/// is not validated, so do not choose between models on its rates.
const JUDGE_SYSTEM: &str = "You compare one fact from a meeting's official record with a \
summary of that meeting.\n\
Ask these in order and stop at the first yes:\n\
1. covered: does the summary state the fact's main content, in any words, anywhere (tldr, \
decisions, action items or notes)? Several parts together may cover it. Ignore wrappers such as \
\"the group decided\" or \"the manager instructed X to\": what counts is the choice or the task \
itself. Extra detail in the summary is fine.\n\
2. contradicted: does the summary state the opposite about the same thing: another value for \
the same choice, the same task given to a different person, or something the fact says is \
undecided stated as certain or as excluded? Both claims must be about the same thing and cannot \
both be true.\n\
3. missing: otherwise. A detail the summary leaves out, or a topic the summary treats \
differently without denying the fact, is missing and never contradicted.\n\
An owner or a date counts only when the fact states it.\n\
Examples, invented:\n\
Fact: The manager instructed the designer to draw the logo. Summary: Designer: draw the logo. \
-> covered\n\
Fact: The case will be blue. Summary: The case will be red. -> contradicted\n\
Fact: The case will be blue. Summary: The case will be plastic. -> missing\n\
Fact: The designer will draw the logo. Summary: Engineer: draw the logo. -> contradicted\n\
Fact: The designer will draw the logo by Friday. Summary: Designer: draw the logo. -> covered\n\
Write the reason first: name the summary parts used (such as S-D2), or say that nothing \
matches, in at most 25 words.\n\
Reply with JSON only: {\"reason\": \"...\", \"verdict\": \"covered\"}";

/// `{"facts": [...]}`, one to [`MAX_FACTS`] non-blank strings.
fn split_grammar() -> Result<String, String> {
    schema_to_gbnf(
        &json!({
            "type": "object",
            "properties": {"facts": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1,
                "maxItems": MAX_FACTS,
            }},
            "required": ["facts"],
            "additionalProperties": false,
        }),
        GrammarOptions {
            non_blank_strings: true,
            compact: false,
        },
    )
    .map_err(|e| e.to_string())
}

/// `{"reason": "...", "verdict": <one of judge::VERDICTS>}`, reason first.
fn judge_grammar() -> Result<String, String> {
    schema_to_gbnf(
        &json!({
            "type": "object",
            "properties": {
                "reason": {"type": "string"},
                "verdict": {"enum": judge::VERDICTS},
            },
            "required": ["reason", "verdict"],
            "additionalProperties": false,
        }),
        GrammarOptions {
            non_blank_strings: true,
            compact: false,
        },
    )
    .map_err(|e| e.to_string())
}

fn chat(system: &str, user: String) -> Vec<ChatMessage> {
    vec![
        ChatMessage {
            role: Role::System,
            content: system.to_owned(),
        },
        ChatMessage {
            role: Role::User,
            content: user,
        },
    ]
}

/// The share of `part`'s content tokens (by count) that `whole` holds.
fn token_share(part: &BTreeMap<String, usize>, whole: &BTreeMap<String, usize>) -> f64 {
    let total: usize = part.values().sum();
    if total == 0 {
        return 0.0;
    }
    let held: usize = part
        .iter()
        .map(|(t, n)| (*n).min(whole.get(t).copied().unwrap_or(0)))
        .sum();
    held as f64 / total as f64
}

/// The facts to keep for `sentence` from a model's `reply`: exact repeats dropped (ignoring
/// case), and the whole sentence as the one fact, with the reason, when a fact has no content,
/// adds too many words of its own ([`FACT_SOURCE_SHARE`]) or the facts lose too much of the
/// sentence ([`SENTENCE_COVER_SHARE`]). A sentence is therefore never judged on less than it
/// said.
pub fn checked_facts(sentence: &str, reply: &[String]) -> (Vec<String>, Option<String>) {
    let whole = |why: String| (vec![sentence.to_owned()], Some(why));
    let source = content_tokens(sentence);
    let mut seen = Vec::new();
    let mut facts = Vec::new();
    let mut union: BTreeMap<String, usize> = BTreeMap::new();
    for fact in reply {
        let fact = fact.split_whitespace().collect::<Vec<_>>().join(" ");
        let key = fact.to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let tokens = content_tokens(&fact);
        if tokens.is_empty() {
            return whole(format!("a fact without content: {fact:?}"));
        }
        let share = token_share(&tokens, &source);
        if share < FACT_SOURCE_SHARE {
            return whole(format!("a fact with words of its own: {fact:?}"));
        }
        for (t, n) in tokens {
            let e = union.entry(t).or_insert(0);
            *e = (*e).max(n);
        }
        facts.push(fact);
    }
    if facts.is_empty() {
        return whole("no facts".to_owned());
    }
    let cover = token_share(&source, &union);
    if cover < SENTENCE_COVER_SHARE {
        return whole(format!("the facts hold only {cover:.2} of the sentence"));
    }
    (facts, None)
}

/// The committed facts file (`fixtures/ami/atomic_facts.json`).
#[derive(Debug, Clone)]
pub struct FactsFile(pub Value);

impl FactsFile {
    /// Reads the file, checking its format.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if v["format"] != FILE_FORMAT || !v["meetings"].is_object() {
            return Err(format!("{}: not a facts file", path.display()));
        }
        Ok(Self(v))
    }

    /// The meeting's judge reference packet, its ids `D1.1`, `D1.2`, ..., after checking that
    /// the sentences the file was split from are `sentences` (a `judge::reference_packet`),
    /// id by id: a changed annotation or a file of another meeting fails.
    pub fn reference_packet(&self, meeting: &str, sentences: &Value) -> Result<Value, String> {
        let m = &self.0["meetings"][meeting];
        let mut out = json!({"meeting": meeting});
        for list in judge::LISTS {
            let want = sentences[list]
                .as_array()
                .ok_or_else(|| format!("{meeting}: no {list} in the annotations"))?;
            let have = m[list]
                .as_array()
                .ok_or_else(|| format!("{meeting}: not in the facts file"))?;
            if want.len() != have.len() {
                return Err(format!(
                    "{meeting}: {} {list} in the annotations, {} in the facts file",
                    want.len(),
                    have.len()
                ));
            }
            let mut facts = Vec::new();
            for (w, h) in want.iter().zip(have) {
                if w["id"] != h["id"] || w["text"] != h["text"] {
                    return Err(format!(
                        "{meeting}: {} differs from the annotations; split again",
                        w["id"]
                    ));
                }
                let id = h["id"].as_str().unwrap_or_default();
                let list = h["facts"]
                    .as_array()
                    .filter(|l| !l.is_empty())
                    .ok_or_else(|| format!("{meeting}: {id} has no facts"))?;
                for (i, fact) in list.iter().enumerate() {
                    let text = fact
                        .as_str()
                        .filter(|t| !t.trim().is_empty())
                        .ok_or_else(|| format!("{meeting}: {id} has a blank fact"))?;
                    facts.push(json!({"id": format!("{id}.{}", i + 1), "text": text}));
                }
            }
            out[list] = json!(facts);
        }
        Ok(out)
    }
}

/// Splits one sentence with the model; a cut-off or unreadable reply keeps the whole sentence.
fn split_one(
    llm: &mut LocalLlm,
    grammar: &str,
    prefix: &str,
    sentence: &str,
) -> Result<(Vec<String>, Option<String>), String> {
    let messages = chat(SPLIT_SYSTEM, format!("Sentence: {sentence}"));
    let g = llm
        .generate_with_prefix(&messages, prefix, grammar, SPLIT_TOKENS)
        .map_err(|e| e.to_string())?;
    if g.stop != Stop::EndOfGeneration {
        return Ok((
            vec![sentence.to_owned()],
            Some(format!("the reply stopped: {:?}", g.stop)),
        ));
    }
    let reply: Value = serde_json::from_str(&g.text).map_err(|e| format!("split reply: {e}"))?;
    let list: Vec<String> = reply["facts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str().map(str::to_owned))
        .collect();
    Ok(checked_facts(sentence, &list))
}

/// `--split-facts <file>`: adds the meetings of `meetings` to the facts file. A meeting
/// already in it stays as it is (a split is done once); the file is written after every
/// meeting, so a stopped run resumes.
pub fn split_meetings(
    llm: &mut LocalLlm,
    model_id: &str,
    prefix: &str,
    path: &Path,
    meetings: &[(String, Value)],
) -> Result<(), String> {
    let mut file = if path.is_file() {
        FactsFile::read(path)?.0
    } else {
        json!({
            "format": FILE_FORMAT,
            "licence": "Text derived from the AMI manual annotations v1.6.2, CC BY 4.0 (fixtures/LICENSES.md)",
            "split_by": model_id,
            "meetings": {},
        })
    };
    if file["split_by"] != model_id {
        return Err(format!(
            "{} was split by {}, not {model_id}",
            path.display(),
            file["split_by"]
        ));
    }
    let grammar = split_grammar()?;
    for (meeting, sentences) in meetings {
        if !file["meetings"][meeting.as_str()].is_null() {
            eprintln!("sum_eval: {meeting} is already split");
            continue;
        }
        let mut entry = json!({});
        for list in judge::LISTS {
            let mut items = Vec::new();
            for s in sentences[list].as_array().into_iter().flatten() {
                let text = s["text"].as_str().unwrap_or_default();
                let (facts, kept_whole) = split_one(llm, &grammar, prefix, text)?;
                let mut item = json!({"id": s["id"], "text": text, "facts": facts});
                if let Some(why) = kept_whole {
                    eprintln!("sum_eval: {meeting} {}: kept whole: {why}", s["id"]);
                    item["kept_whole"] = json!(why);
                }
                items.push(item);
            }
            entry[list] = json!(items);
        }
        file["meetings"][meeting.as_str()] = entry;
        let mut text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        text.push('\n');
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("sum_eval: split {meeting}");
    }
    Ok(())
}

/// The summary packet as the text the local judge reads (the whole packet, no model name).
pub fn summary_text(packet: &Value) -> String {
    let mut out = format!(
        "TLDR: {}\n\nDecisions:\n",
        packet["tldr"].as_str().unwrap_or("")
    );
    for d in packet["decisions"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{}: {}\n",
            d["id"].as_str().unwrap_or(""),
            d["text"].as_str().unwrap_or("")
        ));
    }
    out.push_str("\nAction items:\n");
    for a in packet["action_items"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{}: owner: {}; task: {}; due: {}\n",
            a["id"].as_str().unwrap_or(""),
            a["owner"].as_str().unwrap_or("none"),
            a["task"].as_str().unwrap_or(""),
            a["due"].as_str().unwrap_or("none"),
        ));
    }
    out.push_str("\nNotes:\n");
    out.push_str(packet["notes"].as_str().unwrap_or(""));
    out
}

/// One `{"id", "verdict", "reason"}` from a model reply; `None` when it is cut off or
/// unreadable. A reason is one line.
pub fn read_verdict(id: &str, stop: Stop, text: &str) -> Option<Value> {
    if stop != Stop::EndOfGeneration {
        return None;
    }
    let reply: Value = serde_json::from_str(text).ok()?;
    let verdict = reply["verdict"].as_str()?;
    if !judge::VERDICTS.contains(&verdict) {
        return None;
    }
    let reason = reply["reason"]
        .as_str()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if reason.is_empty() {
        return None;
    }
    Some(json!({"id": id, "verdict": verdict, "reason": reason}))
}

/// Judges one fact against one summary text. A reply cut off at its budget is asked for once
/// more with a shorter reason; a second failure stops the run (never a guessed verdict).
fn judge_one(
    llm: &mut LocalLlm,
    grammar: &str,
    prefix: &str,
    id: &str,
    fact: &str,
    summary: &str,
) -> Result<Value, String> {
    let user = format!("Fact: {fact}\n\nSummary:\n{summary}");
    let g = llm
        .generate_with_prefix(
            &chat(JUDGE_SYSTEM, user.clone()),
            prefix,
            grammar,
            JUDGE_TOKENS,
        )
        .map_err(|e| format!("{id}: {e}"))?;
    if let Some(v) = read_verdict(id, g.stop, &g.text) {
        return Ok(v);
    }
    let shorter = format!("{user}\n\nKeep the reason under ten words.");
    let g = llm
        .generate_with_prefix(
            &chat(JUDGE_SYSTEM, shorter),
            prefix,
            grammar,
            2 * JUDGE_TOKENS,
        )
        .map_err(|e| format!("{id}: {e}"))?;
    read_verdict(id, g.stop, &g.text).ok_or_else(|| format!("{id}: no readable verdict"))
}

/// The verdict file a packet of an invalid summary gets without a model call: every fact is
/// missing, as for a judge agent.
pub fn all_missing(reference: &Value, meeting: &str, label: &str) -> Value {
    let mut out = json!({"meeting": meeting, "model": label});
    for list in judge::LISTS {
        let v: Vec<Value> = reference[list]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| json!({"id": r["id"], "verdict": "missing", "reason": "No valid summary."}))
            .collect();
        out[list] = json!(v);
    }
    out
}

/// `--judge-facts <packets>` with `--out <dir>`: copies the packets of `meetings` to `out` and
/// writes the local judge's `verdict.<label>.json` beside them. The packets are not changed,
/// so `--import-verdicts` on `out` passes the same fingerprint and blinding checks. A verdict
/// file already in `out` is kept (a stopped run resumes).
pub fn judge_packets_locally(
    llm: &mut LocalLlm,
    prefix: &str,
    packets: &Path,
    out: &Path,
    meetings: &[String],
) -> Result<usize, String> {
    let grammar = judge_grammar()?;
    let read = |p: &Path| -> Result<Value, String> {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
    };
    let set = packets.join("packet-set.json");
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    if set.is_file() {
        std::fs::copy(&set, out.join("packet-set.json")).map_err(|e| e.to_string())?;
    }
    let mut judged = 0;
    for meeting in meetings {
        let reference = read(&packets.join(meeting).join("reference.json"))?;
        judge::write(out, meeting, "reference.json", &reference)?;
        for label in judge::packet_labels(&packets.join(meeting))? {
            let name = format!("summary.{label}.json");
            let summary = read(&packets.join(meeting).join(&name))?;
            judge::write(out, meeting, &name, &summary)?;
            let verdict_name = format!("verdict.{label}.json");
            if out.join(meeting).join(&verdict_name).is_file() {
                continue;
            }
            let verdict = if summary["valid"] == true {
                let text = summary_text(&summary);
                let mut v = json!({"meeting": meeting, "model": label});
                for list in judge::LISTS {
                    let mut items = Vec::new();
                    for r in reference[list].as_array().into_iter().flatten() {
                        let id = r["id"].as_str().unwrap_or_default();
                        let fact = r["text"].as_str().unwrap_or_default();
                        items.push(
                            judge_one(llm, &grammar, prefix, id, fact, &text)
                                .map_err(|e| format!("{meeting} {label} {e}"))?,
                        );
                        judged += 1;
                    }
                    v[list] = json!(items);
                }
                v
            } else {
                all_missing(&reference, meeting, &label)
            };
            judge::check_verdict(&reference, &verdict, meeting, &label)?;
            judge::write(out, meeting, &verdict_name, &verdict)?;
        }
        eprintln!("sum_eval: judged {meeting}");
    }
    Ok(judged)
}

/// The coverage rate minus [`CONTRADICTION_WEIGHT`] times the contradiction rate of one
/// `{"covered", "contradicted", "missing"}` count; `None` without facts.
pub fn net_score(c: &Value) -> Option<f64> {
    let n = |k: &str| c[k].as_u64().unwrap_or(0) as f64;
    let total = n("covered") + n("contradicted") + n("missing");
    (total > 0.0).then(|| (n("covered") - CONTRADICTION_WEIGHT * n("contradicted")) / total)
}

/// Markdown rates per model and list: `rows` holds `(model, counts)`, counts as
/// `judge::check_verdict` shapes them.
pub fn rates_table(rows: &[(String, Value)]) -> String {
    let mut t = String::from(
        "| model | list | facts | covered | contradicted | missing | coverage % | contradiction % | net % |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for (model, c) in rows {
        let both = {
            let mut sum = json!({});
            for kind in judge::VERDICTS {
                sum[kind] = json!(
                    c["decisions"][kind].as_u64().unwrap_or(0)
                        + c["action_items"][kind].as_u64().unwrap_or(0)
                );
            }
            sum
        };
        for (name, counts) in [
            ("decisions", &c["decisions"]),
            ("actions", &c["action_items"]),
            ("all", &both),
        ] {
            let n = |k: &str| counts[k].as_u64().unwrap_or(0);
            let total = n("covered") + n("contradicted") + n("missing");
            let pct = |x: u64| 100.0 * x as f64 / (total as f64).max(1.0);
            t.push_str(&format!(
                "| {model} | {name} | {total} | {} | {} | {} | {:.1} | {:.1} | {:.1} |\n",
                n("covered"),
                n("contradicted"),
                n("missing"),
                pct(n("covered")),
                pct(n("contradicted")),
                net_score(counts).unwrap_or(0.0) * 100.0,
            ));
        }
    }
    t
}

/// Agreement of `test` with `gold` over the same items: percent agreement, Cohen's kappa
/// (J. Cohen, 1960: (po - pe) / (1 - pe)), the 3x3 confusion counts and, per verdict, the
/// precision and recall of `test` against `gold`.
pub fn agreement(pairs: &[(String, String)]) -> Value {
    let n = pairs.len();
    let count = |g: Option<&str>, t: Option<&str>| {
        pairs
            .iter()
            .filter(|(a, b)| g.is_none_or(|x| x == a) && t.is_none_or(|x| x == b))
            .count()
    };
    let agree = pairs.iter().filter(|(a, b)| a == b).count();
    let po = agree as f64 / (n as f64).max(1.0);
    let pe: f64 = judge::VERDICTS
        .iter()
        .map(|v| {
            count(Some(v), None) as f64 * count(None, Some(v)) as f64 / ((n * n) as f64).max(1.0)
        })
        .sum();
    let kappa = if (1.0 - pe).abs() < 1e-12 {
        Value::Null
    } else {
        json!((po - pe) / (1.0 - pe))
    };
    let mut confusion = json!({});
    let mut per_verdict = json!({});
    for g in judge::VERDICTS {
        for t in judge::VERDICTS {
            confusion[g][t] = json!(count(Some(g), Some(t)));
        }
        let hit = count(Some(g), Some(g));
        let ratio = |d: usize| (d > 0).then(|| hit as f64 / d as f64);
        per_verdict[g] = json!({
            "gold": count(Some(g), None),
            "judge": count(None, Some(g)),
            "precision": ratio(count(None, Some(g))),
            "recall": ratio(count(Some(g), None)),
        });
    }
    json!({
        "items": n,
        "agree": agree,
        "percent": po * 100.0,
        "kappa": kappa,
        "confusion_gold_by_judge": confusion,
        "per_verdict": per_verdict,
    })
}

/// `--agreement <gold dir> --against <judge dir>`: compares the verdict files of `meetings`.
/// Every item of the gold file must have a verdict on the other side, and both sides must hold
/// the same reference packet and summary packets (fails closed); disagreements are listed.
pub fn compare_dirs(gold: &Path, other: &Path, meetings: &[String]) -> Result<Value, String> {
    let read = |p: &Path| -> Result<Value, String> {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut pairs = Vec::new();
    let mut disagreements = Vec::new();
    for meeting in meetings {
        let (g, o) = (gold.join(meeting), other.join(meeting));
        let reference = read(&g.join("reference.json"))?;
        if reference != read(&o.join("reference.json"))? {
            return Err(format!("{meeting}: the two sides judged different facts"));
        }
        let labels = judge::packet_labels(&g)?;
        if labels != judge::packet_labels(&o)? {
            return Err(format!("{meeting}: the two sides hold different summaries"));
        }
        for label in labels {
            let s = format!("summary.{label}.json");
            if read(&g.join(&s))? != read(&o.join(&s))? {
                return Err(format!("{meeting} {label}: the summaries differ"));
            }
            let v = format!("verdict.{label}.json");
            let (gv, ov) = (read(&g.join(&v))?, read(&o.join(&v))?);
            judge::check_verdict(&reference, &gv, meeting, &label)?;
            judge::check_verdict(&reference, &ov, meeting, &label)?;
            for list in judge::LISTS {
                for item in gv[list].as_array().into_iter().flatten() {
                    let found = ov[list]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|x| x["id"] == item["id"])
                        .ok_or_else(|| format!("{meeting} {label} {}: no verdict", item["id"]))?;
                    let (a, b) = (
                        item["verdict"].as_str().unwrap_or_default(),
                        found["verdict"].as_str().unwrap_or_default(),
                    );
                    if a != b {
                        disagreements.push(json!({
                            "meeting": meeting, "label": label, "id": item["id"],
                            "gold": a, "judge": b,
                            "gold_reason": item["reason"], "judge_reason": found["reason"],
                        }));
                    }
                    pairs.push((a.to_owned(), b.to_owned()));
                }
            }
        }
    }
    let mut out = agreement(&pairs);
    out["disagreements"] = json!(disagreements);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLED: &str = "The group decided on an LCD screen and rubber buttons.";

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn facts_a_good_split_is_kept_without_repeats() {
        let (facts, why) = checked_facts(
            BUNDLED,
            &strings(&[
                "The group decided on an LCD screen.",
                "the group decided on an LCD  screen.",
                "The group decided on rubber buttons.",
            ]),
        );
        assert_eq!(why, None);
        assert_eq!(
            facts,
            strings(&[
                "The group decided on an LCD screen.",
                "The group decided on rubber buttons."
            ])
        );
    }

    #[test]
    fn facts_a_split_that_adds_or_loses_words_keeps_the_sentence() {
        for reply in [
            // A fact of the model's own.
            strings(&[
                "The group decided on an LCD screen.",
                "Marketing hated titanium.",
            ]),
            // Half of the sentence is gone.
            strings(&["The group decided."]),
            // Nothing to judge.
            strings(&[]),
            strings(&["the of a"]),
        ] {
            let (facts, why) = checked_facts(BUNDLED, &reply);
            assert_eq!(facts, strings(&[BUNDLED]), "{reply:?}");
            assert!(why.is_some(), "{reply:?}");
        }
    }

    fn sentences() -> Value {
        json!({
            "meeting": "M1",
            "decisions": [{"id": "D1", "text": BUNDLED}],
            "action_items": [{"id": "A1", "text": "Send the drawings."}],
        })
    }

    fn file() -> FactsFile {
        FactsFile(json!({"format": 1, "meetings": {"M1": {
            "decisions": [{"id": "D1", "text": BUNDLED, "facts": ["LCD screen chosen.", "Rubber buttons chosen."]}],
            "action_items": [{"id": "A1", "text": "Send the drawings.", "facts": ["Send the drawings."]}],
        }}}))
    }

    #[test]
    fn facts_reference_packet_numbers_each_fact_of_a_sentence() {
        let p = file().reference_packet("M1", &sentences()).unwrap();
        assert_eq!(p["decisions"][0]["id"], "D1.1");
        assert_eq!(p["decisions"][1]["id"], "D1.2");
        assert_eq!(p["decisions"][1]["text"], "Rubber buttons chosen.");
        assert_eq!(p["action_items"][0]["id"], "A1.1");
    }

    #[test]
    fn facts_reference_packet_refuses_a_file_that_does_not_match_the_annotations() {
        let mut changed = sentences();
        changed["decisions"][0]["text"] = json!("Something else.");
        assert!(file().reference_packet("M1", &changed).is_err());
        let mut extra = sentences();
        extra["action_items"] = json!([
            {"id": "A1", "text": "Send the drawings."},
            {"id": "A2", "text": "Book a room."},
        ]);
        assert!(file().reference_packet("M1", &extra).is_err());
        assert!(file().reference_packet("M2", &sentences()).is_err());
        let mut blank = file();
        blank.0["meetings"]["M1"]["decisions"][0]["facts"] = json!([" "]);
        assert!(blank.reference_packet("M1", &sentences()).is_err());
    }

    #[test]
    fn facts_verdict_replies_fail_closed() {
        let good = r#"{"reason": "S-D2 says\nthe same.", "verdict": "covered"}"#;
        let v = read_verdict("D1.1", Stop::EndOfGeneration, good).unwrap();
        assert_eq!(v["verdict"], "covered");
        assert_eq!(v["reason"], "S-D2 says the same.");
        assert!(read_verdict("D1.1", Stop::MaxTokens, good).is_none());
        assert!(read_verdict("D1.1", Stop::Looping, good).is_none());
        assert!(read_verdict("D1.1", Stop::EndOfGeneration, "{").is_none());
        let other = r#"{"reason": "x", "verdict": "maybe"}"#;
        assert!(read_verdict("D1.1", Stop::EndOfGeneration, other).is_none());
        let blank = r#"{"reason": "  ", "verdict": "covered"}"#;
        assert!(read_verdict("D1.1", Stop::EndOfGeneration, blank).is_none());
    }

    #[test]
    fn facts_grammars_convert() {
        assert!(split_grammar().unwrap().contains("root ::="));
        let g = judge_grammar().unwrap();
        for v in judge::VERDICTS {
            assert!(g.contains(v), "{g}");
        }
    }

    #[test]
    fn facts_an_invalid_summary_is_all_missing_in_the_verdict_format() {
        let reference = file().reference_packet("M1", &sentences()).unwrap();
        let v = all_missing(&reference, "M1", "S1");
        let c = judge::check_verdict(&reference, &v, "M1", "S1").unwrap();
        assert_eq!(c["decisions"]["missing"], 2);
        assert_eq!(c["action_items"]["missing"], 1);
    }

    #[test]
    fn facts_a_contradiction_costs_more_than_a_miss() {
        let n = net_score(&json!({"covered": 6, "contradicted": 0, "missing": 4})).unwrap();
        let c = net_score(&json!({"covered": 6, "contradicted": 4, "missing": 0})).unwrap();
        assert!((n - 0.6).abs() < 1e-9, "{n}");
        assert!((c - (-0.2)).abs() < 1e-9, "{c}");
        assert!(net_score(&json!({"covered": 0, "contradicted": 0, "missing": 0})).is_none());
    }

    #[test]
    fn facts_rates_table_has_each_list_and_their_sum() {
        let counts = json!({
            "decisions": {"covered": 3, "contradicted": 1, "missing": 0},
            "action_items": {"covered": 1, "contradicted": 0, "missing": 1},
        });
        let t = rates_table(&[("m".to_owned(), counts)]);
        assert!(
            t.contains("| m | decisions | 4 | 3 | 1 | 0 | 75.0 | 25.0 | 25.0 |"),
            "{t}"
        );
        assert!(
            t.contains("| m | actions | 2 | 1 | 0 | 1 | 50.0 | 0.0 | 50.0 |"),
            "{t}"
        );
        assert!(
            t.contains("| m | all | 6 | 4 | 1 | 1 | 66.7 | 16.7 | 33.3 |"),
            "{t}"
        );
    }

    /// Worked by hand: 10 items, 7 agree, po 0.7; gold 5c/2x/3m, judge 6c/1x/3m, so
    /// pe = (5*6 + 2*1 + 3*3) / 100 = 0.41 and kappa = 0.29 / 0.59.
    #[test]
    fn facts_agreement_matches_a_hand_computed_kappa() {
        let row = |g: &str, t: &str, n: usize| {
            std::iter::repeat_n((g.to_owned(), t.to_owned()), n).collect::<Vec<_>>()
        };
        let mut pairs = row("covered", "covered", 4);
        pairs.extend(row("covered", "missing", 1));
        pairs.extend(row("contradicted", "covered", 1));
        pairs.extend(row("contradicted", "contradicted", 1));
        pairs.extend(row("missing", "covered", 1));
        pairs.extend(row("missing", "missing", 2));
        let a = agreement(&pairs);
        assert_eq!(a["items"], 10);
        assert_eq!(a["agree"], 7);
        assert!(
            (a["kappa"].as_f64().unwrap() - 0.29 / 0.59).abs() < 1e-9,
            "{a}"
        );
        assert_eq!(a["confusion_gold_by_judge"]["missing"]["covered"], 1);
        assert_eq!(a["per_verdict"]["contradicted"]["gold"], 2);
        assert_eq!(a["per_verdict"]["contradicted"]["precision"], 1.0);
        assert_eq!(a["per_verdict"]["contradicted"]["recall"], 0.5);
    }

    #[test]
    fn facts_agreement_of_one_verdict_everywhere_has_no_kappa() {
        let pairs = vec![("missing".to_owned(), "missing".to_owned()); 3];
        assert_eq!(agreement(&pairs)["kappa"], Value::Null);
    }

    #[test]
    fn facts_summary_text_shows_every_part_without_the_model() {
        let t = summary_text(&json!({
            "model": "S1", "tldr": "A meeting.", "notes": "## Topic\n- A note",
            "decisions": [{"id": "S-D1", "text": "Use rubber."}],
            "action_items": [{"id": "S-A1", "owner": "PM", "task": "Book", "due": null}],
        }));
        for part in [
            "A meeting.",
            "S-D1: Use rubber.",
            "owner: PM; task: Book; due: none",
            "- A note",
        ] {
            assert!(t.contains(part), "{t}");
        }
        assert!(!t.contains("S1"), "{t}");
    }

    #[test]
    fn facts_compare_dirs_fails_closed_on_different_packets() {
        let root = std::env::temp_dir().join(format!("sum-eval-facts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, b) = (root.join("a"), root.join("b"));
        let reference = file().reference_packet("M1", &sentences()).unwrap();
        let summary = json!({"valid": true});
        let gold = all_missing(&reference, "M1", "S1");
        let mut other = gold.clone();
        other["decisions"][0]["verdict"] = json!("covered");
        for (dir, v) in [(&a, &gold), (&b, &other)] {
            judge::write(dir, "M1", "reference.json", &reference).unwrap();
            judge::write(dir, "M1", "summary.S1.json", &summary).unwrap();
            judge::write(dir, "M1", "verdict.S1.json", v).unwrap();
        }
        let meetings = vec!["M1".to_owned()];
        let r = compare_dirs(&a, &b, &meetings).unwrap();
        assert_eq!(
            (r["items"].as_u64(), r["agree"].as_u64()),
            (Some(3), Some(2))
        );
        assert_eq!(r["disagreements"][0]["id"], "D1.1");
        judge::write(&b, "M1", "summary.S1.json", &json!({"valid": false})).unwrap();
        assert!(compare_dirs(&a, &b, &meetings).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
