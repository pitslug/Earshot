//! The AMI conditions of `m2_prod` (moved here from `asr_eval` with T7.3c, PLAN D22):
//! `asr.wer_ami_ihm` and the informational `ami_sdm`, on the production default model.
//!
//! The rule is unchanged from `asr_eval` (T2.5b, decided by the owner; T2.5c for SDM):
//! every annotated segment of a speaker (`segments/<meeting>.<X>.segments.xml`, clipped to
//! the meeting's UEM) is one utterance, decoded by one batch call (the `decode` closure) on
//! that speaker's headset (IHM, `refs::load_ami_ihm`) or on the meeting's single distant
//! microphone `Array1-01` (SDM, `refs::load_ami_sdm`), and aligned against the speaker's
//! words whose start lies in `[segment start, segment end)` (`refs::AmiHeadset::score`);
//! WerCounts are summed over segments, headsets and meetings. A speaker word inside the UEM
//! but outside every segment is a deletion in the reported `outside_segments` bucket, never
//! dropped. The hypothesis is the words production keeps (`refs::kept_words`, as
//! `Transcription::into_segments`), joined by `refs::hypothesis_text`. Also reported,
//! informational: the same decodes joined and aligned against each speaker's whole
//! reference (`joined_alignment`).

use std::time::Duration;

use earshot_core::metrics::wer::word_errors_normalized;
use serde_json::{Value, json};

use super::{Score, fail, refs};

/// The informational far-field AMI result (T2.5c), reported under `informational`.
pub const INFO_AMI_SDM: &str = "ami_sdm";

/// The `set` text of `mic`'s condition and its id.
fn describe(mic: refs::AmiMic) -> (&'static str, &'static str) {
    match mic {
        refs::AmiMic::Ihm => (
            "asr.wer_ami_ihm",
            "AMI IS1009a + ES2004a, IHM with oracle segmentation (Kaldi/ESPnet): each \
             annotated segment of a speaker (segments/*.segments.xml, clipped to the UEM) \
             decoded by one batch call on the speaker's headset (channel from \
             corpusResources/meetings.xml) and aligned against the speaker's words starting \
             inside it; words outside every segment count as deletions; hypothesis: the words \
             production keeps (refs::kept_words); references: AMI manual annotations v1.6.2",
        ),
        refs::AmiMic::Sdm => (
            INFO_AMI_SDM,
            "informational, not a threshold (T2.5c): AMI IS1009a + ES2004a, SDM (Array1-01, \
             the first microphone of the first far-field array) with the oracle segments, \
             references and scoring of asr.wer_ami_ihm: each annotated segment of a speaker \
             (clipped to the UEM) decoded by one batch call on the shared Array1-01 recording \
             (overlapping segments of different speakers each decoded on their own) and \
             aligned against that speaker's words starting inside it; words outside every \
             segment count as deletions; hypothesis: refs::kept_words; references: AMI \
             manual annotations v1.6.2",
        ),
    }
}

/// One AMI condition: every oracle segment of `set` (loaded for `mic`) decoded by `decode`
/// (the kept hypothesis text and the decode time of one batch call), scored by
/// `refs::AmiHeadset::score`. Returns the corpus WER (%) and the report fields (`set`,
/// `score`, `outside_segments`, `joined_alignment`, `meetings`, `headsets`).
pub fn score(
    mic: refs::AmiMic,
    set: &[refs::AmiHeadset],
    mut decode: impl FnMut(&[f32]) -> (String, Duration),
) -> (f64, Value) {
    let (id, set_doc) = describe(mic);
    if set.iter().any(|h| h.channel != mic.channel(h.headset)) {
        fail(format!("{id}: a speaker was loaded for another microphone"));
    }
    let mut score = Score::default();
    let mut joined = Score::default();
    let mut outside = Score::default();
    let mut headsets = Vec::new();
    let mut meetings = Vec::new();
    for meeting in refs::AMI_MEETINGS {
        let mut meeting_score = Score::default();
        for h in set.iter().filter(|h| h.meeting == meeting) {
            let (channel, agent, plan) = (h.headset, &h.speaker, &h.plan);
            eprintln!(
                "m2_prod: {id}: {meeting} {} (speaker {agent} on Headset-{channel}, {} \
                 segments, {} words outside every segment)",
                h.channel,
                plan.spans.len(),
                plan.outside.len()
            );
            let mut hyps = Vec::with_capacity(plan.spans.len());
            let mut decodes = Vec::with_capacity(plan.spans.len());
            for audio in h.segment_audio() {
                let (text, d) = decode(audio);
                hyps.push(text);
                decodes.push((audio.len(), d));
            }
            let counts = h.score(&hyps).unwrap_or_else(|e| fail(e));
            let mut speaker = Score::default();
            for (&c, &(n, d)) in counts.segments.iter().zip(&decodes) {
                for s in [&mut score, &mut meeting_score, &mut speaker] {
                    s.add(c, n, d);
                }
            }
            // Speaker words outside every segment: deletions in their own bucket.
            if !plan.outside.is_empty() {
                for s in [&mut score, &mut meeting_score, &mut speaker, &mut outside] {
                    s.add(counts.outside, 0, Duration::ZERO);
                }
            }
            if speaker.counts != counts.total() {
                fail(format!(
                    "{id}: {meeting} speaker {agent}: scores do not add up"
                ));
            }
            let samples_total: usize = h.ranges.iter().map(ExactSizeIterator::len).sum();
            // Informational: the same decodes joined and aligned against the speaker's whole
            // reference.
            let whole_ref = refs::ami_reference(std::slice::from_ref(&h.words), h.uem.0, h.uem.1);
            let parts: Vec<&str> = hyps
                .iter()
                .map(String::as_str)
                .filter(|t| !t.is_empty())
                .collect();
            let hyp = parts.join(" ");
            joined.add(
                word_errors_normalized(&whole_ref, &hyp),
                samples_total,
                Duration::ZERO,
            );
            headsets.push(json!({
                "meeting": meeting,
                "headset": channel,
                "channel": h.channel,
                "speaker": agent,
                "uem": [h.uem.0, h.uem.1],
                "segments": plan.spans.len(),
                "outside_words": plan.outside,
                "score": speaker.json(),
                "reference": whole_ref,
                "hypothesis": hyp,
            }));
        }
        eprintln!("m2_prod: {id}: {meeting} = {:.3}%", meeting_score.percent());
        meetings.push(json!({ "meeting": meeting, "score": meeting_score.json() }));
    }
    eprintln!(
        "m2_prod: {id} = {:.3}% (joined alignment {:.3}%, {} words outside segments)",
        score.percent(),
        joined.percent(),
        outside.counts.ref_words
    );
    (
        score.percent(),
        json!({
            "set": set_doc,
            "score": score.json(),
            "outside_segments": outside.json(),
            "joined_alignment": joined.json(),
            "meetings": meetings,
            "headsets": headsets,
        }),
    )
}
