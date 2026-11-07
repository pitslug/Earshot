//! Judge packets for a meaning-based check of decisions and action items (T6.20a, the owner
//! 2026-10-25): per meeting, the AMI annotated items and each model's stored summary as plain
//! text, so a judge (Claude Code agents) can decide whether a reference item is covered in
//! meaning, which no token rule can.
//!
//! - `reference.json`: `{"meeting", "decisions": [{"id", "text"}], "action_items": [...]}`,
//!   one entry per annotated sentence as written (nothing split or merged); the annotators'
//!   "NA"/"None" markers and sentences without content tokens are left out
//!   (`items::reference_items`, the rule of every item metric). Ids are `D1`, `D2`, ... and
//!   `A1`, `A2`, ... in annotation order.
//! - `summary.<model>.json`: `{"meeting", "model", "valid", "failure", "tldr", "notes",
//!   "decisions": [{"id", "text"}], "action_items": [{"id", "owner", "task", "due"}]}`, from
//!   the stored `SummaryV1` with every citation removed; `notes` is the enhanced notes as
//!   plain text (`## heading` lines and `- bullet` lines). A meeting without a valid summary
//!   has `"valid": false` and empty lists (it is a miss for the judge too). A blinded packet
//!   ([`blind`]) carries no failure text.

use std::ffi::OsString;
use std::path::Path;

use earshot_core::metrics::items::reference_items;
use earshot_core::summary::SummaryV1;
use serde_json::{Value, json};

use crate::ami::AbsSumm;

/// `reference.json` for `meeting`.
pub fn reference_packet(meeting: &str, r: &AbsSumm) -> Value {
    let list = |sentences: &[String], prefix: &str| -> Vec<Value> {
        let refs: Vec<&str> = sentences.iter().map(String::as_str).collect();
        reference_items(&refs)
            .into_iter()
            .enumerate()
            .map(|(i, t)| json!({"id": format!("{prefix}{}", i + 1), "text": t}))
            .collect()
    };
    json!({
        "meeting": meeting,
        "decisions": list(&r.decisions, "D"),
        "action_items": list(&r.actions, "A"),
    })
}

/// `summary.<model>.json` for `meeting` (None: the run produced no valid summary).
pub fn summary_packet(
    meeting: &str,
    model: &str,
    summary: Option<&SummaryV1>,
    failure: Option<&str>,
) -> Value {
    let Some(s) = summary else {
        return json!({
            "meeting": meeting, "model": model, "valid": false, "failure": failure,
            "tldr": "", "notes": "", "decisions": [], "action_items": [],
        });
    };
    let mut notes = Vec::new();
    for sec in &s.enhanced_notes {
        notes.push(format!("## {}", sec.heading));
        for b in &sec.bullets {
            notes.push(format!("- {}", b.text));
        }
    }
    json!({
        "meeting": meeting,
        "model": model,
        "valid": true,
        "failure": Value::Null,
        "tldr": s.tldr,
        "notes": notes.join("\n"),
        "decisions": s.decisions.iter().enumerate()
            .map(|(i, d)| json!({"id": format!("S-D{}", i + 1), "text": d.text}))
            .collect::<Vec<_>>(),
        "action_items": s.action_items.iter().enumerate()
            .map(|(i, a)| json!({"id": format!("S-A{}", i + 1), "owner": a.owner, "task": a.task, "due": a.due}))
            .collect::<Vec<_>>(),
    })
}

/// Blinded labels `S1`, `S2`, ... for the summaries of `meeting`, one per entry of
/// `models`, in the same order: the models ranked by the FNV-1a hash of
/// `<meeting>|<model>`, so the order is fixed for a meeting but differs between meetings,
/// and a label says nothing about the model. The key (label to model) is kept outside the
/// packet directory.
pub fn blind_labels(meeting: &str, models: &[String]) -> Vec<String> {
    let hash = |m: &str| -> u64 {
        format!("{meeting}|{m}")
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            })
    };
    let mut order: Vec<usize> = (0..models.len()).collect();
    order.sort_by_key(|&i| (hash(&models[i]), i));
    let mut labels = vec![String::new(); models.len()];
    for (rank, i) in order.into_iter().enumerate() {
        labels[i] = format!("S{}", rank + 1);
    }
    labels
}

/// Blinds a summary packet for the judge: `model` becomes `label` and `failure` is
/// cleared, since a failure message can name its source (step names of a local run, the
/// reply file of a Claude Code agent). An invalid summary still says `"valid": false`.
pub fn blind(packet: &mut Value, label: &str) {
    packet["model"] = json!(label);
    packet["failure"] = Value::Null;
}

/// The `(model, packet)` summaries of `meeting` blinded ([`blind_labels`], [`blind`]) and
/// returned as `(label, model, packet)` in label order (`S1`, `S2`, ..., numerically), so
/// packets written in this order leave no trace of the source order (file times).
pub fn blinded_in_label_order(
    meeting: &str,
    packets: Vec<(String, Value)>,
) -> Vec<(String, String, Value)> {
    let labels = blind_labels(
        meeting,
        &packets.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
    );
    let mut out: Vec<(String, String, Value)> = packets
        .into_iter()
        .zip(labels)
        .map(|((model, mut packet), label)| {
            blind(&mut packet, &label);
            (label, model, packet)
        })
        .collect();
    let rank = |l: &str| {
        l.trim_start_matches('S')
            .parse::<usize>()
            .unwrap_or(usize::MAX)
    };
    out.sort_by_key(|(l, _, _)| rank(l));
    out
}

/// The verdicts a judge may give a reference item.
pub const VERDICTS: [&str; 3] = ["covered", "contradicted", "missing"];

/// The two reference lists of a packet and their key in a verdict file.
pub const LISTS: [&str; 2] = ["decisions", "action_items"];

/// Checks `verdict` (`verdict.<model>.json`, format in `docs/summary-eval.md`) against the
/// meeting's `reference` packet and counts it: `{"decisions": {"covered", "contradicted",
/// "missing"}, "action_items": {...}}`. Fails closed: the meeting and model must be the
/// file's, every reference id must have exactly one verdict and no other id may appear,
/// every verdict is one of [`VERDICTS`] and every reason is one non-empty line.
pub fn check_verdict(
    reference: &Value,
    verdict: &Value,
    meeting: &str,
    model: &str,
) -> Result<Value, String> {
    let at = format!("{meeting} {model}");
    if verdict["meeting"] != meeting || verdict["model"] != model {
        return Err(format!(
            "{at}: the file names meeting {} and model {}",
            verdict["meeting"], verdict["model"]
        ));
    }
    let mut out = json!({});
    for list in LISTS {
        let want: Vec<&str> = reference[list]
            .as_array()
            .ok_or_else(|| format!("{meeting}: reference packet without {list}"))?
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        let got = verdict[list]
            .as_array()
            .ok_or_else(|| format!("{at}: no {list} list"))?;
        let mut seen = std::collections::BTreeSet::new();
        let mut counts = json!({"covered": 0, "contradicted": 0, "missing": 0});
        for v in got {
            let id = v["id"]
                .as_str()
                .ok_or_else(|| format!("{at}: a {list} verdict without an id"))?;
            if !want.contains(&id) {
                return Err(format!("{at}: {id} is not a reference {list} id"));
            }
            if !seen.insert(id) {
                return Err(format!("{at}: {id} judged twice"));
            }
            let kind = v["verdict"].as_str().unwrap_or_default();
            if !VERDICTS.contains(&kind) {
                return Err(format!(
                    "{at}: {id}: verdict {} is not one of {VERDICTS:?}",
                    v["verdict"]
                ));
            }
            let reason = v["reason"].as_str().unwrap_or_default();
            if reason.trim().is_empty() || reason.contains(['\n', '\r']) {
                return Err(format!("{at}: {id}: the reason must be one non-empty line"));
            }
            counts[kind] = json!(counts[kind].as_u64().unwrap_or(0) + 1);
        }
        if let Some(missing) = want.iter().find(|id| !seen.contains(*id)) {
            return Err(format!("{at}: {missing} has no verdict"));
        }
        out[list] = counts;
    }
    Ok(out)
}

/// `a` plus `b`, count by count (the shape [`check_verdict`] returns).
pub fn add_counts(a: &mut Value, b: &Value) {
    for list in LISTS {
        for kind in VERDICTS {
            let sum = a[list][kind].as_u64().unwrap_or(0) + b[list][kind].as_u64().unwrap_or(0);
            a[list][kind] = json!(sum);
        }
    }
}

/// Writes `value` as pretty JSON to `dir/<meeting>/<file>`.
pub fn write(dir: &Path, meeting: &str, file: &str, value: &Value) -> Result<(), String> {
    let d = dir.join(meeting);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join(file);
    std::fs::write(
        &p,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// The labels of the summary packets in a meeting's packet directory `mdir`
/// (`summary.<label>.json`), sorted. Fails on a `verdict.<label>.json` without a summary
/// packet (a stale file or a mistyped label), which would otherwise be ignored.
pub fn packet_labels(mdir: &Path) -> Result<Vec<String>, String> {
    let mut summaries = Vec::new();
    let mut verdicts = Vec::new();
    for entry in std::fs::read_dir(mdir).map_err(|e| format!("{}: {e}", mdir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", mdir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let label = |prefix: &str| {
            name.strip_prefix(prefix)
                .and_then(|r| r.strip_suffix(".json"))
                .map(str::to_owned)
        };
        if let Some(l) = label("summary.") {
            summaries.push(l);
        } else if let Some(l) = label("verdict.") {
            verdicts.push(l);
        }
    }
    summaries.sort();
    let orphans: Vec<String> = verdicts
        .iter()
        .filter(|v| !summaries.contains(v))
        .map(|v| format!("verdict.{v}.json"))
        .collect();
    if !orphans.is_empty() {
        return Err(format!(
            "{}: {} without a summary packet",
            mdir.display(),
            orphans.join(", ")
        ));
    }
    Ok(summaries)
}

/// The file in a packet directory that records its packet-set fingerprint
/// ([`packet_fingerprint`]); the blinding key holds the same value as `packet_set`.
pub const PACKET_SET_FILE: &str = "packet-set.json";

/// A fingerprint of the packets of `meetings` in `dir`: FNV-1a (64 bit) over, per meeting
/// in sorted order, `reference.json` and every `summary.*.json` (sorted by name), each as
/// `<meeting>/<file>`, its length and its bytes. Verdict files are not part of it. It ties a
/// blinding key to the packets it was written with (a guard against mix-ups, not against
/// tampering).
pub fn packet_fingerprint(dir: &Path, meetings: &[String]) -> Result<String, String> {
    let mut meetings: Vec<&String> = meetings.iter().collect();
    meetings.sort();
    meetings.dedup();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    for meeting in meetings {
        let mdir = dir.join(meeting);
        let entries = std::fs::read_dir(&mdir).map_err(|e| format!("{}: {e}", mdir.display()))?;
        let files = fingerprint_files(&mdir, entries.map(|e| e.map(|e| e.file_name())))?;
        for f in files {
            let p = mdir.join(&f);
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            feed(format!("{meeting}/{f}\n{}\n", bytes.len()).as_bytes());
            feed(&bytes);
        }
    }
    Ok(format!("{h:016x}"))
}

/// The files of one meeting directory `mdir` that [`packet_fingerprint`] covers, sorted:
/// `reference.json` and every `summary.*.json` among `names` (the directory's entries). An
/// entry that cannot be read fails the fingerprint rather than leaving a packet out of it.
fn fingerprint_files(
    mdir: &Path,
    names: impl IntoIterator<Item = std::io::Result<OsString>>,
) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    for name in names {
        let name = name
            .map_err(|e| format!("{}: {e}", mdir.display()))?
            .to_string_lossy()
            .into_owned();
        if name == "reference.json" || (name.starts_with("summary.") && name.ends_with(".json")) {
            files.push(name);
        }
    }
    files.sort();
    Ok(files)
}

/// Records `fingerprint` in the packet directory ([`PACKET_SET_FILE`]).
pub fn write_packet_set(dir: &Path, fingerprint: &str) -> Result<(), String> {
    let p = dir.join(PACKET_SET_FILE);
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&json!({"packet_set": fingerprint}))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// Checks that the packets of `meetings` in `dir` are the ones the blinding `key` (if any)
/// and the directory's [`PACKET_SET_FILE`] were written for: both records must agree with
/// each other and with the fingerprint of the files there now. Fails closed when one
/// record is missing; a round written before fingerprints (neither record) is accepted
/// with a note for the caller to print.
pub fn check_packet_set(
    key: Option<&Value>,
    dir: &Path,
    meetings: &[String],
) -> Result<Option<String>, String> {
    let p = dir.join(PACKET_SET_FILE);
    let recorded =
        match std::fs::read_to_string(&p) {
            Ok(text) => Some(
                serde_json::from_str::<Value>(&text)
                    .map_err(|e| format!("{}: {e}", p.display()))?["packet_set"]
                    .as_str()
                    .ok_or_else(|| format!("{}: no packet_set", p.display()))?
                    .to_owned(),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", p.display())),
        };
    let keyed = match key {
        Some(k) => match &k["packet_set"] {
            Value::Null => None,
            v => Some(
                v.as_str()
                    .ok_or("the blinding key's packet_set is not a string")?
                    .to_owned(),
            ),
        },
        None => None,
    };
    let want = match (key.is_some(), keyed, recorded) {
        (_, None, None) => {
            return Ok(Some(format!(
                "{}: no packet-set fingerprint (packets written before fingerprints); the key \
                 is not checked against them",
                dir.display()
            )));
        }
        (_, Some(k), Some(r)) if k == r => r,
        (_, Some(k), Some(r)) => {
            return Err(format!(
                "the blinding key is for packet set {k}, {} records {r}: another round's key",
                p.display()
            ));
        }
        (true, None, Some(_)) => {
            return Err(format!(
                "the blinding key has no packet_set but {} has one: another round's key",
                p.display()
            ));
        }
        (_, Some(_), None) => {
            return Err(format!(
                "the blinding key has a packet_set but {} is missing",
                p.display()
            ));
        }
        (false, None, Some(r)) => r,
    };
    let now = packet_fingerprint(dir, meetings)?;
    if now != want {
        return Err(format!(
            "{}: the packets of these meetings fingerprint as {now}, written as {want}: packets \
             or meetings changed since the key was written",
            dir.display()
        ));
    }
    Ok(None)
}

/// The packet of `model` for `meeting` from an earlier blinded round (T6.20c: the Claude
/// anchor, whose replies are no longer stored): `<dir>/<meeting>/summary.<label>.json`, the
/// label read from that round's blinding key `key`. Fails closed when the key has no such
/// model for the meeting, the packet names another meeting, or the round's
/// `reference.json` differs from `reference` (the packet rebuilt from the annotations). The
/// packet comes back with `model` set to `model`; everything else is unchanged.
pub fn prior_packet(
    dir: &Path,
    key: &Path,
    meeting: &str,
    model: &str,
    reference: &AbsSumm,
) -> Result<Value, String> {
    let read = |p: &Path| -> Result<Value, String> {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
    };
    let k = read(key)?;
    let labels: Vec<&str> = k[meeting]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, m)| m.as_str() == Some(model))
        .map(|(l, _)| l.as_str())
        .collect();
    let [label] = labels[..] else {
        return Err(format!(
            "{}: {} labels for {model} in {meeting}, expected one",
            key.display(),
            labels.len()
        ));
    };
    let mdir = dir.join(meeting);
    if read(&mdir.join("reference.json"))? != reference_packet(meeting, reference) {
        return Err(format!(
            "{}: reference.json differs from the annotations",
            mdir.display()
        ));
    }
    let mut packet = read(&mdir.join(format!("summary.{label}.json")))?;
    if packet["meeting"] != meeting || packet["model"] != label {
        return Err(format!(
            "{}: summary.{label}.json names meeting {} and model {}",
            mdir.display(),
            packet["meeting"],
            packet["model"]
        ));
    }
    packet["model"] = json!(model);
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use earshot_core::summary::{ActionItem, Decision, NoteBullet, NoteSection};

    use super::*;

    #[test]
    fn judge_packets_drop_citations_and_markers() {
        let r = AbsSumm {
            abstract_: vec![],
            actions: vec!["NA.".into()],
            decisions: vec![
                "The remote will have an LCD screen and rubber buttons.".into(),
                "None.".into(),
            ],
            problems: vec![],
        };
        let p = reference_packet("ES2004c", &r);
        assert_eq!(
            p["decisions"],
            json!([{"id": "D1", "text": "The remote will have an LCD screen and rubber buttons."}])
        );
        assert_eq!(p["action_items"], json!([]));
        let id = "0193a5b0-1c00-7000-8000-000000000001".parse().unwrap();
        let s = SummaryV1 {
            title: "T".into(),
            tldr: "We met.".into(),
            enhanced_notes: vec![NoteSection {
                heading: "Design".into(),
                bullets: vec![NoteBullet {
                    text: "LCD".into(),
                    source_segment_ids: vec![id],
                }],
            }],
            decisions: vec![Decision {
                text: "Use an LCD".into(),
                source_segment_ids: vec![id],
            }],
            action_items: vec![ActionItem {
                owner: "Ana".into(),
                task: "Draw it".into(),
                due: None,
                source_segment_ids: vec![id],
            }],
            open_questions: vec![],
            topics: vec![],
        };
        let p = summary_packet("ES2004c", "m", Some(&s), None);
        assert!(
            !p.to_string().contains("0193a5b0"),
            "citations removed: {p}"
        );
        assert_eq!(p["notes"], "## Design\n- LCD");
        assert_eq!(
            p["decisions"][0],
            json!({"id": "S-D1", "text": "Use an LCD"})
        );
        assert_eq!(p["action_items"][0]["owner"], "Ana");
        let f = summary_packet("ES2004c", "m", None, Some("invalid"));
        assert_eq!(
            (f["valid"].clone(), f["decisions"].clone()),
            (json!(false), json!([]))
        );
    }

    /// A fresh directory under the temp dir for one test.
    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("sum_eval_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn judge_import_refuses_a_verdict_without_a_summary_packet() {
        let root = temp_root("judge_orphan");
        let mdir = root.join("M1");
        std::fs::create_dir_all(&mdir).unwrap();
        for f in [
            "reference.json",
            "summary.S1.json",
            "summary.S2.json",
            "verdict.S1.json",
            "verdict.S2.json",
        ] {
            std::fs::write(mdir.join(f), "{}").unwrap();
        }
        assert_eq!(packet_labels(&mdir).unwrap(), ["S1", "S2"]);
        // A verdict for a label with no packet (a stale file, a mistyped label) is reported,
        // never ignored.
        std::fs::write(mdir.join("verdict.S3.json"), "{}").unwrap();
        let e = packet_labels(&mdir).unwrap_err();
        assert!(e.contains("verdict.S3.json"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn judge_fingerprint_files_select_reference_and_summaries_sorted() {
        let mdir = Path::new("packets").join("M1");
        let names = [
            "summary.S2.json",
            "verdict.S1.json",
            "reference.json",
            "summary.S1.json",
            "summary.S1.txt",
            "notes.json",
            "packet-set.json",
        ]
        .map(|n| Ok(OsString::from(n)));
        assert_eq!(
            fingerprint_files(&mdir, names).unwrap(),
            ["reference.json", "summary.S1.json", "summary.S2.json"]
        );
    }

    #[test]
    fn judge_fingerprint_files_fail_on_an_unreadable_entry_anywhere() {
        let mdir = Path::new("packets").join("M1");
        for at in 0..3 {
            let names = (0..3).map(|i| {
                if i == at {
                    Err(std::io::Error::other("entry unreadable"))
                } else {
                    Ok(OsString::from(format!("summary.S{i}.json")))
                }
            });
            let e = fingerprint_files(&mdir, names).unwrap_err();
            assert!(e.contains(&mdir.display().to_string()), "{e}");
            assert!(e.contains("entry unreadable"), "{e}");
        }
    }

    #[test]
    fn judge_packet_set_fingerprint_ties_the_key_to_the_packets() {
        let root = temp_root("judge_fingerprint");
        let dir = root.join("packets");
        for m in ["M1", "M2"] {
            write(&dir, m, "reference.json", &json!({"meeting": m})).unwrap();
            write(&dir, m, "summary.S1.json", &json!({"model": "S1"})).unwrap();
        }
        let meetings = ["M1".to_string(), "M2".to_string()];
        let fp = packet_fingerprint(&dir, &meetings).unwrap();
        // Verdicts are not part of the set; the same packets give the same fingerprint.
        std::fs::write(dir.join("M1").join("verdict.S1.json"), "{}").unwrap();
        assert_eq!(packet_fingerprint(&dir, &meetings).unwrap(), fp);
        let key = json!({"packet_set": fp, "M1": {"S1": "a"}, "M2": {"S1": "a"}});
        write_packet_set(&dir, &fp).unwrap();
        assert_eq!(check_packet_set(Some(&key), &dir, &meetings), Ok(None));
        assert_eq!(check_packet_set(None, &dir, &meetings), Ok(None));
        // A key from another packet set, or a key without a fingerprint for fingerprinted
        // packets.
        let other = json!({"packet_set": "0000000000000000"});
        assert!(check_packet_set(Some(&other), &dir, &meetings).is_err());
        assert!(check_packet_set(Some(&json!({"M1": {}})), &dir, &meetings).is_err());
        // Another meeting set.
        assert!(check_packet_set(Some(&key), &dir, &meetings[..1]).is_err());
        // A packet changed, added or removed after the key was written.
        write(&dir, "M2", "summary.S1.json", &json!({"model": "S2"})).unwrap();
        assert!(check_packet_set(Some(&key), &dir, &meetings).is_err());
        write(&dir, "M2", "summary.S1.json", &json!({"model": "S1"})).unwrap();
        assert_eq!(check_packet_set(Some(&key), &dir, &meetings), Ok(None));
        write(&dir, "M2", "summary.S2.json", &json!({"model": "S2"})).unwrap();
        assert!(check_packet_set(Some(&key), &dir, &meetings).is_err());
        std::fs::remove_file(dir.join("M2").join("summary.S2.json")).unwrap();
        // A fingerprinted key with the packet directory's record missing.
        std::fs::remove_file(dir.join(PACKET_SET_FILE)).unwrap();
        assert!(check_packet_set(Some(&key), &dir, &meetings).is_err());
        // A round written before fingerprints (neither side has one) is accepted with a
        // note the caller prints.
        let legacy = json!({"M1": {"S1": "a"}, "M2": {"S1": "a"}});
        let note = check_packet_set(Some(&legacy), &dir, &meetings).unwrap();
        assert!(note.is_some_and(|n| n.contains("fingerprint")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn judge_blinded_packets_keep_no_failure_text() {
        // A failure message can name its source (a local model's step names, a file of
        // Claude replies), so a blinded packet says only that the summary is not valid.
        let failure = "reduce step invalid after 3 requests: claude-dev-medium/ES2003c.json";
        let mut p = summary_packet("ES2003c", "qwen3.5-4b-chunked-twocall", None, Some(failure));
        blind(&mut p, "S3");
        assert_eq!(p["model"], "S3");
        assert_eq!(p["valid"], false);
        assert!(p["failure"].is_null(), "{p}");
        let text = p.to_string();
        assert!(
            !text.contains("reduce step") && !text.contains("claude"),
            "{text}"
        );
        // Nothing else changes.
        let mut want = summary_packet("ES2003c", "S3", None, None);
        want["failure"] = Value::Null;
        assert_eq!(p, want);
    }

    #[test]
    fn judge_blinded_packets_come_in_label_order_not_source_order() {
        // Eleven sources, so `S10` and `S11` test the numeric order too.
        let models: Vec<String> = (1..=11).map(|i| format!("model-{i}")).collect();
        let packets: Vec<(String, Value)> = models
            .iter()
            .map(|m| (m.clone(), summary_packet("ES2003c", m, None, Some("x"))))
            .collect();
        let labels = blind_labels("ES2003c", &models);
        let want: Vec<String> = (1..=11).map(|i| format!("S{i}")).collect();
        assert_ne!(
            labels, want,
            "the test needs a source order unlike the label order"
        );
        let out = blinded_in_label_order("ES2003c", packets);
        let got: Vec<&str> = out.iter().map(|(l, _, _)| l.as_str()).collect();
        assert_eq!(got, want);
        for (label, model, packet) in &out {
            // Each label still names its own model, and the packet is blinded.
            let i = models.iter().position(|m| m == model).unwrap();
            assert_eq!(&labels[i], label);
            assert_eq!(packet["model"], label.as_str());
            assert!(packet["failure"].is_null());
        }
    }

    #[test]
    fn judge_blind_labels_are_distinct_fixed_and_vary_by_meeting() {
        let models: Vec<String> = ["claude", "phi", "q4", "q9", "phi-c", "q4-c", "q9-c"]
            .map(String::from)
            .to_vec();
        let a = blind_labels("ES2003c", &models);
        let mut sorted = a.clone();
        sorted.sort();
        assert_eq!(sorted, ["S1", "S2", "S3", "S4", "S5", "S6", "S7"]);
        assert_eq!(a, blind_labels("ES2003c", &models));
        let meetings = ["ES2011c", "IS1008c", "TS3004c", "TS3006c"];
        assert!(meetings.iter().any(|m| blind_labels(m, &models) != a));
    }

    #[test]
    fn judge_verdicts_are_counted_and_checked_strictly() {
        let reference = json!({
            "meeting": "ES2003c",
            "decisions": [{"id": "D1", "text": "a"}, {"id": "D2", "text": "b"}],
            "action_items": [{"id": "A1", "text": "c"}],
        });
        let good = json!({
            "meeting": "ES2003c", "model": "m",
            "decisions": [
                {"id": "D2", "verdict": "contradicted", "reason": "S-D1 says the opposite"},
                {"id": "D1", "verdict": "covered", "reason": "S-D3"},
            ],
            "action_items": [{"id": "A1", "verdict": "missing", "reason": "not mentioned"}],
        });
        let c = check_verdict(&reference, &good, "ES2003c", "m").unwrap();
        assert_eq!(
            c,
            json!({
                "decisions": {"covered": 1, "contradicted": 1, "missing": 0},
                "action_items": {"covered": 0, "contradicted": 0, "missing": 1},
            })
        );
        let mut total = json!({});
        add_counts(&mut total, &c);
        add_counts(&mut total, &c);
        assert_eq!(total["decisions"]["covered"], 2);
        assert_eq!(total["action_items"]["missing"], 2);
        let broken = |f: &dyn Fn(&mut Value)| {
            let mut v = good.clone();
            f(&mut v);
            check_verdict(&reference, &v, "ES2003c", "m").unwrap_err()
        };
        assert!(broken(&|v| v["model"] = json!("other")).contains("names"));
        assert!(
            broken(&|v| v["decisions"]
                .as_array_mut()
                .unwrap()
                .pop()
                .map(drop)
                .unwrap())
            .contains("no verdict")
        );
        assert!(broken(&|v| v["decisions"][0]["id"] = json!("D1")).contains("twice"));
        assert!(broken(&|v| v["decisions"][0]["id"] = json!("D9")).contains("not a reference"));
        assert!(broken(&|v| v["decisions"][0]["verdict"] = json!("partly")).contains("not one of"));
        assert!(
            broken(&|v| v["decisions"][0]["reason"] = json!(" ")).contains("one non-empty line")
        );
        assert!(
            broken(&|v| v["decisions"][0]["reason"] = json!("a\nb")).contains("one non-empty line")
        );
        assert!(broken(&|v| v["action_items"] = json!(null)).contains("no action_items"));
    }

    #[test]
    fn judge_prior_packets_come_from_the_earlier_rounds_key_and_fail_closed() {
        let root = std::env::temp_dir().join(format!("sum_eval_prior_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("packets");
        let reference = AbsSumm {
            abstract_: vec![],
            actions: vec!["Ana sends the minutes.".into()],
            decisions: vec!["The remote is yellow.".into()],
            problems: vec![],
        };
        write(
            &dir,
            "M1",
            "reference.json",
            &reference_packet("M1", &reference),
        )
        .unwrap();
        let mut claude = summary_packet("M1", "S2", None, Some("x"));
        claude["tldr"] = json!("kept as written");
        write(&dir, "M1", "summary.S2.json", &claude).unwrap();
        let key = root.join("key.json");
        std::fs::write(&key, r#"{"M1": {"S1": "local", "S2": "claude"}}"#).unwrap();

        let p = prior_packet(&dir, &key, "M1", "claude", &reference).unwrap();
        assert_eq!(p["model"], "claude");
        assert_eq!(p["tldr"], "kept as written");
        // Fails closed: unknown model or meeting, a changed reference, a mislabelled packet.
        assert!(prior_packet(&dir, &key, "M1", "other", &reference).is_err());
        assert!(prior_packet(&dir, &key, "M2", "claude", &reference).is_err());
        let changed = AbsSumm {
            decisions: vec!["The remote is red.".into()],
            ..reference.clone()
        };
        assert!(prior_packet(&dir, &key, "M1", "claude", &changed).is_err());
        claude["model"] = json!("S1");
        write(&dir, "M1", "summary.S2.json", &claude).unwrap();
        assert!(prior_packet(&dir, &key, "M1", "claude", &reference).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
