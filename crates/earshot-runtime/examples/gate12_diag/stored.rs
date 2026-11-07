//! `--stored <data-dir>` (T6.23b evidence): scores the final transcript an `earshot replay`
//! of the recording stored in `<data-dir>/earshot.db`, counts only.
//!
//! - words per label on each track (stored final pass, after echo cleaning and the speaker
//!   check); labels are "Me" and numbered roles (`speaker 2 (Remote)`), never a display
//!   name, so a speaker the user named does not reach the report;
//! - far-end words under "Me" (and under any mic label): mic words not matched to the script
//!   that match, in order and within [`LEAK_SLACK_MS`], the far-end reference, the replay's
//!   reference track transcribed by [`FAR_REFERENCE_MODEL`] (beam), as `model_eval` scores
//!   them;
//! - script recall per line (longest in-order match of each line's tokens against the
//!   words, as the rest of `gate12_diag`) and over the whole script in order;
//! - canceller status counts in the talk-over window from a replay with 8 ms status intervals and the
//!   live pipeline settings (`LiveConfig::default().pipeline`, so the build's canceller
//!   default).

use std::path::Path;

use earshot_core::asr::{TranscribeOptions, Word, normalize_token};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, WhisperLoader, load_silero, whisper_model,
};
use earshot_runtime::live::LiveConfig;
use earshot_runtime::view::MeetingView;
use earshot_store::Store;
use serde_json::{Value, json};

use super::{Args, Decoder, STATUS_INTERVAL, StatusCounts, fail, lcs, private, replay, tokens_of};

/// The fixed far-end reference model of `model_eval`'s gate12 corpus.
const FAR_REFERENCE_MODEL: &str = "large-v3-turbo";
/// A mic word matches a far-end reference word starting at most this far away
/// (`model_eval` `LEAK_SLACK_MS`).
const LEAK_SLACK_MS: u64 = 1_500;

/// Index pairs of a longest common subsequence of `a` and `b` under `same` (as
/// `model_eval`).
fn lcs_pairs(a: usize, b: usize, same: impl Fn(usize, usize) -> bool) -> Vec<(usize, usize)> {
    let mut dir = vec![0u8; (a + 1) * (b + 1)];
    let mut prev = vec![0u32; b + 1];
    let mut cur = vec![0u32; b + 1];
    for i in 1..=a {
        cur[0] = 0;
        for j in 1..=b {
            let (v, d) = if same(i - 1, j - 1) {
                (prev[j - 1] + 1, 0)
            } else if prev[j] >= cur[j - 1] {
                (prev[j], 1)
            } else {
                (cur[j - 1], 2)
            };
            cur[j] = v;
            dir[i * (b + 1) + j] = d;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let (mut i, mut j, mut out) = (a, b, Vec::new());
    while i > 0 && j > 0 {
        match dir[i * (b + 1) + j] {
            0 => {
                out.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
            1 => i -= 1,
            _ => j -= 1,
        }
    }
    out.reverse();
    out
}

/// Mic words that are far-end words (`model_eval`'s `far_leaks`).
fn far_leaks(mic: &[Word], script: &[String], far_reference: &[Word]) -> usize {
    let tok = |w: &[Word]| -> Vec<String> { w.iter().map(|x| normalize_token(&x.text)).collect() };
    let mic_tokens = tok(mic);
    let mut is_script = vec![false; mic.len()];
    for (i, _) in lcs_pairs(mic.len(), script.len(), |i, j| {
        !mic_tokens[i].is_empty() && mic_tokens[i] == script[j]
    }) {
        is_script[i] = true;
    }
    let rest: Vec<usize> = (0..mic.len()).filter(|&i| !is_script[i]).collect();
    let far_tokens = tok(far_reference);
    lcs_pairs(rest.len(), far_tokens.len(), |a, b| {
        let w = &mic[rest[a]];
        !mic_tokens[rest[a]].is_empty()
            && mic_tokens[rest[a]] == far_tokens[b]
            && w.start
                .as_millis()
                .abs_diff(far_reference[b].start.as_millis())
                <= LEAK_SLACK_MS
    })
    .len()
}

/// Recall of `words` per script line and over the whole script in order.
fn recall(lines: &[Vec<String>], words: &[Word]) -> Value {
    let tokens: Vec<String> = words.iter().flat_map(|w| tokens_of(&w.text)).collect();
    let per_line: Vec<Value> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| json!({"line": i + 1, "words": l.len(), "found": lcs(l, &tokens)}))
        .collect();
    let script: Vec<String> = lines.iter().flatten().cloned().collect();
    json!({
        "per_line": per_line,
        "sum_of_lines": lines.iter().map(|l| lcs(l, &tokens)).sum::<usize>(),
        "whole_script_in_order": lcs(&script, &tokens),
        "script_words": script.len(),
    })
}

/// A report label per speaker row, from its role only: "Me" for the Me row, else
/// `speaker <n> (<role>)` numbered in row order. Display names are never read.
fn speaker_labels(
    speakers: impl IntoIterator<Item = (EntityId, SpeakerRole)>,
) -> Vec<(EntityId, String)> {
    let mut n = 0;
    speakers
        .into_iter()
        .map(|(id, role)| {
            if role == SpeakerRole::Me {
                (id, "Me".to_owned())
            } else {
                n += 1;
                (id, format!("speaker {n} ({role:?})"))
            }
        })
        .collect()
}

/// The label of a segment's speaker; a segment without a known speaker row takes its
/// track's default ("Me" on the mic, "Them" on the reference, as `MeetingView`).
fn label_of(
    labels: &[(EntityId, String)],
    speaker: Option<EntityId>,
    track: Option<TrackId>,
) -> String {
    if let Some((_, l)) = speaker.and_then(|id| labels.iter().find(|(s, _)| *s == id)) {
        return l.clone();
    }
    match track {
        Some(TrackId::Mic) => "Me".to_owned(),
        Some(TrackId::Reference) => "Them".to_owned(),
        None => "unknown".to_owned(),
    }
}

pub(super) fn run(args: &Args, lines: &[Vec<String>], data: &Path) {
    eprintln!("gate12_diag: replaying (8 ms canceller status) for the canceller counts");
    let mut fine = LiveConfig::default().pipeline;
    fine.stats_interval = STATUS_INTERVAL;
    let diag = replay(&args.dir, fine);
    let (from, to) = (private().windows[0].from_ms, private().windows[0].to_ms);
    let counts = StatusCounts::of(&diag.status, from, to);

    eprintln!("gate12_diag: far-end reference by {FAR_REFERENCE_MODEL} (beam)");
    let rel = whisper_model(FAR_REFERENCE_MODEL).unwrap_or_else(|e| fail(e));
    let path = args.models.require(&rel).unwrap_or_else(|e| fail(e));
    let whisper = EvidenceLoader::default()
        .load_verified(&path, args.backend)
        .unwrap_or_else(|e| fail(format!("{FAR_REFERENCE_MODEL}: {e}")));
    let mut far_dec = Decoder {
        model: Box::new(whisper),
        vad: load_silero(&args.models, Accelerator::Cpu)
            .unwrap_or_else(|e| fail(format!("Silero: {e}"))),
        options: TranscribeOptions::batch(),
        backend: args.backend,
    };
    let (far_reference, _) = far_dec.pieces(&diag.reference);

    let store = Store::open(&data.join("earshot.db"))
        .unwrap_or_else(|e| fail(format!("{}: {e}", data.display())));
    let meetings: Vec<_> = store
        .meetings()
        .unwrap_or_else(|e| fail(format!("meetings: {e}")))
        .into_iter()
        .filter(|m| !m.meta.deleted)
        .collect();
    let [meeting] = meetings.as_slice() else {
        fail(format!(
            "expected one meeting in {}, found {}",
            data.display(),
            meetings.len()
        ));
    };
    let view = MeetingView::load(&store, meeting.id)
        .unwrap_or_else(|e| fail(format!("meeting: {e}")))
        .unwrap_or_else(|| fail("meeting deleted"));
    let kind = |seg: &earshot_core::asr::Segment| {
        view.tracks
            .iter()
            .find(|t| t.id == seg.track)
            .map(|t| t.kind)
    };
    // Numbered labels from the rows' roles: a display name (a person's name) never reaches
    // the counts-only report.
    let numbered = speaker_labels(view.speakers.iter().map(|s| (s.id, s.role)));
    let mut labels = serde_json::Map::new();
    let (mut me, mut mic_all) = (Vec::new(), Vec::new());
    for seg in &view.segments {
        let track = match kind(seg) {
            Some(TrackId::Mic) => "mic",
            Some(TrackId::Reference) => "reference",
            None => "unknown",
        };
        let name = label_of(&numbered, seg.speaker_id, kind(seg));
        let entry = labels
            .entry(format!("{track}: {name}"))
            .or_insert_with(|| json!(0));
        *entry = json!(entry.as_u64().unwrap_or(0) + seg.words.len() as u64);
        if track == "mic" {
            mic_all.extend(seg.words.iter().cloned());
            if name == "Me" {
                me.extend(seg.words.iter().cloned());
            }
        }
    }
    let script: Vec<String> = lines.iter().flatten().cloned().collect();
    let report = json!({
        "pass": format!("{:?}", view.pass),
        "canceller": format!("{:?}", LiveConfig::default().pipeline.aec.highpass),
        "words_per_label": labels,
        "far_reference_words": far_reference.len(),
        "far_words_under_me": far_leaks(&me, &script, &far_reference),
        "far_words_under_any_mic_label": far_leaks(&mic_all, &script, &far_reference),
        "recall_me": recall(lines, &me),
        "recall_all_mic_labels": recall(lines, &mic_all),
        "canceller_first_window": counts.json(),
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    if let Some(out) = &args.out {
        std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
    }
    println!("{text}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate12_diag_stored_labels_are_roles_never_names() {
        let id = |b: u8| EntityId::from_bytes([b; 16]);
        let labels = speaker_labels([
            (id(1), SpeakerRole::Remote),
            (id(2), SpeakerRole::Me),
            (id(3), SpeakerRole::Remote),
        ]);
        assert_eq!(
            labels,
            [
                (id(1), "speaker 1 (Remote)".to_owned()),
                (id(2), "Me".to_owned()),
                (id(3), "speaker 2 (Remote)".to_owned()),
            ]
        );
        assert_eq!(
            label_of(&labels, Some(id(3)), Some(TrackId::Reference)),
            "speaker 2 (Remote)"
        );
        // No (known) row: the track's default, as `MeetingView::speaker_name`.
        assert_eq!(label_of(&labels, None, Some(TrackId::Mic)), "Me");
        assert_eq!(
            label_of(&labels, Some(id(9)), Some(TrackId::Reference)),
            "Them"
        );
    }
}
