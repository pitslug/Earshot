//! Speaker check against echo on the mic track (T2.4e, final pass).
//!
//! Far-end speech that leaks into the mic sounds like the far-end speaker, not like the
//! user. After the final pass has diarised both tracks, [`speaker_echo_spans`] decides for
//! each embedded stretch of the mic track whether it is echo: its embedding is closer to a
//! reference-track ("Them") speaker than to the mic's own ("Me") voice by at least a
//! margin, and the far end was talking within the echo delay window. [`remove_speaker_echo`]
//! then drops the mic words covered only by echo stretches. Unlike an acoustic gate this
//! keeps the user's words during double talk whenever they are what the mic mostly hears.
//!
//! "Me" ([`me_speakers`]) is every mic speaker who talks alone, for at least
//! [`SpeakerEchoConfig::min_solo`] in total while the far end is silent (no reference turn
//! within the echo delay window: echo cannot exist then, so a user who sounds like a far-end
//! speaker is still Me), and every mic speaker whose centroid matches no Them speaker
//! (cosine below the model's open-set voiceprint threshold,
//! [`crate::voiceprint::calibrated_threshold`]). A mic track that only hears echo has no Me
//! speaker; then a stretch is echo when it matches a Them speaker at that threshold.
//!
//! Only with an echo path: without the AEC's delay lock ([`SpeakerEchoConfig::delay`]
//! `None`: headphones, a muted speaker, a canceller that never converged) every mic
//! speaker is Me and nothing is echo (T2.4f, coordinator decision for the owner: no echo
//! cleaning without a lock).
//!
//! Inputs come from the caller (the runtime's post processor or the acceptance harness):
//! the two diarisations, and embeddings of mic stretches (usually the mic turns, cut into
//! pieces of a few seconds so double talk does not hide in a long turn). Everything here is
//! pure; the margin is calibrated per embedding model on the development split only
//! (`asr_eval --leak-dev`, [`CALIBRATED_SPEAKER_ECHO_MARGINS`]).

use std::time::Duration;

use super::pipeline::{Diarization, SpeakerTurn};
use super::types::{DiarError, Embedding, EmbeddingModelId};
use crate::asr::{EchoDedup, MeetingTime, Word};
use crate::audio::SampleRate;

/// One embedded stretch of the mic track.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanEmbedding {
    /// Start (meeting time).
    pub start: MeetingTime,
    /// End (meeting time, exclusive).
    pub end: MeetingTime,
    /// Embedding of the stretch's audio.
    pub embedding: Embedding,
}

/// Settings of [`speaker_echo_spans`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeakerEchoConfig {
    /// A stretch is echo when its best Them similarity exceeds its best Me similarity by at
    /// least this (cosine). Calibrated per model on the development split
    /// ([`calibrated_speaker_echo_margin`]).
    pub margin: f32,
    /// Cosine at or above which two voices are the same speaker: the embedding model's
    /// open-set voiceprint threshold.
    pub same_speaker: f32,
    /// The AEC's echo delay lock (mic behind reference); `None` without a lock, and then
    /// nothing is echo.
    pub delay: Option<Duration>,
    /// Tolerance around the delay for the "far end was talking" check. Default 1 s.
    pub tolerance: Duration,
    /// A mic speaker who talks alone (far end silent within the delay window) for at least
    /// this in total is Me whatever their voice. Default 500 ms: longer than a diarisation
    /// turn edge sticking out of the window (the window already adds `tolerance` on both
    /// sides), shorter than one short sentence. Chosen before measuring (T2.4f).
    pub min_solo: Duration,
}

/// A speaker-check margin calibrated for one embedding model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibratedSpeakerEchoMargin {
    /// Exact [`EmbeddingModelId`] string.
    pub model: &'static str,
    /// [`SpeakerEchoConfig::margin`] for that model.
    pub margin: f32,
    /// Data, sweep set and selection rule.
    pub source: &'static str,
}

/// Speaker-check margins keyed by exact [`EmbeddingModelId`]; a model without an entry has
/// no speaker check ([`SpeakerEchoConfig::for_model`] fails) until it is calibrated on the
/// development split.
pub const CALIBRATED_SPEAKER_ECHO_MARGINS: &[CalibratedSpeakerEchoMargin] =
    &[CalibratedSpeakerEchoMargin {
        model: "campplus-en-voxceleb@357a834f702b",
        margin: CAMPPLUS_SPEAKER_ECHO_MARGIN,
        source: "asr.echo_leak development split (asr_eval --leak-dev: LibriSpeech dev-clean \
                 speakers, scenario seeds offset by 1000003), live path, text de-duplication \
                 then speaker check; sweep set -0.05 and 0.0 to 0.6 in steps of 0.1, extended \
                 by 0.1 while near-end recall at the top still rises; rule: highest near-end \
                 recall in double talk pooled over the aec and raw conditions, ties by lower \
                 aec leak, then lower raw leak, then the smaller margin",
    }];

/// The CAM++ VoxCeleb speaker-check margin (see [`CALIBRATED_SPEAKER_ECHO_MARGINS`] for
/// the rule). Dev sweep of T2.4f (lock required, solo-talk Me rule; leak after text
/// de-duplication + speaker check; near-end words kept of heard in double talk), canceller
/// output / raw mic:
///
/// | margin | leak aec | leak raw | near kept aec | near kept raw |
/// |---|---|---|---|---|
/// | text only | 2.10% | 21.22% | 100% | 92.2% |
/// | -0.05 | 1.92% | 4.37% | 96.2% | 81.2% |
/// | 0.0 | 1.92% | 4.45% | 100% | 81.2% |
/// | 0.1 | 1.92% | 6.03% | 100% | 84.4% |
/// | 0.2 | 2.01% | 6.99% | 100% | 87.5% |
/// | 0.3 | 2.01% | 8.03% | 100% | 90.6% |
/// | 0.4 | 2.01% | 8.47% | 100% | 90.6% |
/// | 0.5 | 2.01% | 9.43% | 100% | 92.2% |
/// | 0.6 | 2.01% | 10.39% | 100% | 92.2% |
///
/// Recall stops rising at 0.5 (0.6 is equal, so the set is not extended); 0.5 and 0.6 tie
/// on recall and aec leak, and 0.5 has the lower raw leak. At 0.5 the speaker check costs
/// no near-end word beyond text de-duplication and halves the raw leak. No-echo scenarios
/// (with and without far-end speech) kept 100% at every margin.
pub const CAMPPLUS_SPEAKER_ECHO_MARGIN: f32 = 0.5;

/// The dev-calibrated speaker-check margin for `model`, or `None` if it has none.
#[must_use]
pub fn calibrated_speaker_echo_margin(model: &EmbeddingModelId) -> Option<f32> {
    CALIBRATED_SPEAKER_ECHO_MARGINS
        .iter()
        .find(|c| c.model == model.as_str())
        .map(|c| c.margin)
}

impl SpeakerEchoConfig {
    /// The defaults for embeddings of `model`, without a delay lock (set
    /// [`SpeakerEchoConfig::delay`] from the AEC).
    ///
    /// # Errors
    /// [`DiarError::Config`] if the model has no calibrated voiceprint threshold or no
    /// calibrated speaker-check margin: an uncalibrated model gets no speaker check.
    pub fn for_model(model: &EmbeddingModelId) -> Result<Self, DiarError> {
        let same_speaker = crate::voiceprint::calibrated_threshold(model)
            .map_err(|e| DiarError::Config(format!("speaker echo check: {e}")))?;
        let margin = calibrated_speaker_echo_margin(model).ok_or_else(|| {
            DiarError::Config(format!(
                "speaker echo check: no calibrated margin for embedding model {}",
                model.as_str()
            ))
        })?;
        Ok(SpeakerEchoConfig {
            margin,
            same_speaker,
            delay: None,
            tolerance: Duration::from_secs(1),
            min_solo: Duration::from_millis(500),
        })
    }

    /// Allowed echo delay range in ms (mic behind reference), `None` without a lock.
    fn delay_range(&self) -> Option<(i64, i64)> {
        let tol = i64::try_from(self.tolerance.as_millis()).unwrap_or(i64::MAX / 4);
        let d = self
            .delay
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX / 4))?;
        Some((d - tol, d + tol))
    }
}

/// Which mic speakers are "Me": every one without a delay lock; with a lock, those who talk
/// alone for at least `min_solo` and those whose centroid matches no Them speaker (module
/// docs).
///
/// # Errors
/// [`DiarError::ModelMismatch`] if the diarisations used different embedding models.
pub fn me_speakers(
    mic: &Diarization,
    reference: &Diarization,
    config: &SpeakerEchoConfig,
) -> Result<Vec<usize>, DiarError> {
    let Some((lo, hi)) = config.delay_range() else {
        return Ok((0..mic.speakers.len()).collect());
    };
    // Mic-time stretches in which echo of the reference can be heard.
    let mut echo_possible: Vec<(i64, i64)> = reference
        .turns
        .iter()
        .map(|t| (ms(t.start) + lo, ms(t.end) + hi))
        .collect();
    echo_possible.sort_unstable();
    let min_solo = i64::try_from(config.min_solo.as_millis()).unwrap_or(i64::MAX / 4);
    let mut me = Vec::new();
    for (k, s) in mic.speakers.iter().enumerate() {
        let solo: i64 = mic
            .turns
            .iter()
            .filter(|t| t.speaker == k)
            .map(|t| uncovered_ms(ms(t.start), ms(t.end), &echo_possible))
            .sum();
        if solo >= min_solo {
            me.push(k);
            continue;
        }
        let mut best = f32::NEG_INFINITY;
        for t in &reference.speakers {
            best = best.max(s.centroid.cosine_similarity(&t.centroid)?);
        }
        if best < config.same_speaker {
            me.push(k);
        }
    }
    Ok(me)
}

fn ms(t: MeetingTime) -> i64 {
    i64::try_from(t.as_millis()).unwrap_or(i64::MAX / 4)
}

/// Length of `[start, end)` not covered by `intervals` (sorted by start).
fn uncovered_ms(start: i64, end: i64, intervals: &[(i64, i64)]) -> i64 {
    let (mut at, mut free) = (start, 0);
    for &(a, b) in intervals {
        if at >= end {
            break;
        }
        if b <= at {
            continue;
        }
        if a > at {
            free += a.min(end) - at;
        }
        at = at.max(b);
    }
    free + (end - at).max(0)
}

/// Longest stretch of a mic turn embedded as one [`SpanEmbedding`] (2 s): short enough that
/// double talk does not hide in a long turn.
pub const ECHO_SPAN_PIECE_MS: u64 = 2_000;

/// One piece of a mic turn to embed for [`speaker_echo_spans`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoSpanPiece {
    /// Start (meeting time).
    pub start: MeetingTime,
    /// End (meeting time, exclusive).
    pub end: MeetingTime,
    /// The piece's samples in the track's audio.
    pub samples: std::ops::Range<usize>,
}

/// The mic `turns` cut into pieces of at most [`ECHO_SPAN_PIECE_MS`] for the speaker check:
/// a remainder shorter than `min_samples` (the embedder's minimum) joins the previous
/// piece, and pieces with fewer than `min_samples` samples of the `len`-sample track at
/// `rate` (whole samples per millisecond: the pipeline rate) are dropped. The one rule
/// for the runtime's final pass and the `asr.echo_leak` harness.
#[must_use]
pub fn echo_span_pieces(
    turns: &[SpeakerTurn],
    rate: SampleRate,
    len: usize,
    min_samples: usize,
) -> Vec<EchoSpanPiece> {
    let per_ms = (rate.hz() / 1000) as usize;
    let to_sample = |ms: u64| (ms as usize).saturating_mul(per_ms).min(len);
    let mut out = Vec::new();
    for t in turns {
        let (s, e) = (t.start.as_millis(), t.end.as_millis());
        let mut a = s;
        while a < e {
            let mut b = a.saturating_add(ECHO_SPAN_PIECE_MS).min(e);
            if ((e - b) as usize).saturating_mul(per_ms) < min_samples {
                b = e;
            }
            if to_sample(b) - to_sample(a) >= min_samples {
                out.push(EchoSpanPiece {
                    start: MeetingTime::from_millis(a),
                    end: MeetingTime::from_millis(b),
                    samples: to_sample(a)..to_sample(b),
                });
            }
            a = b;
        }
    }
    out
}
/// For each of `spans`, whether it is echo (see the module docs).
///
/// # Errors
/// [`DiarError::ModelMismatch`] when embeddings come from different models.
pub fn speaker_echo_spans(
    mic: &Diarization,
    reference: &Diarization,
    spans: &[SpanEmbedding],
    config: &SpeakerEchoConfig,
) -> Result<Vec<bool>, DiarError> {
    let Some((lo, hi)) = config.delay_range() else {
        return Ok(vec![false; spans.len()]);
    };
    let me = me_speakers(mic, reference, config)?;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        // The far end talked at reference time span - delay for some allowed delay.
        let (from, to) = (ms(span.start) - hi, ms(span.end) - lo);
        let far_active = reference
            .turns
            .iter()
            .any(|t| ms(t.start) < to && from < ms(t.end));
        if !far_active {
            out.push(false);
            continue;
        }
        let mut them = f32::NEG_INFINITY;
        for t in &reference.speakers {
            them = them.max(span.embedding.cosine_similarity(&t.centroid)?);
        }
        let echo = if me.is_empty() {
            them >= config.same_speaker
        } else {
            let mut mine = f32::NEG_INFINITY;
            for &k in &me {
                mine = mine.max(
                    span.embedding
                        .cosine_similarity(&mic.speakers[k].centroid)?,
                );
            }
            them >= mine + config.margin
        };
        out.push(echo);
    }
    Ok(out)
}

/// Drops the mic words whose midpoint lies in an echo span and in no non-echo span
/// (a word no span covers is kept: no evidence).
#[must_use]
pub fn remove_speaker_echo(words: &[Word], spans: &[SpanEmbedding], echo: &[bool]) -> EchoDedup {
    let mut out = EchoDedup::default();
    for w in words {
        let mid = w.midpoint();
        let (mut in_echo, mut in_clean) = (false, false);
        for (s, &e) in spans.iter().zip(echo) {
            if s.start <= mid && mid < s.end {
                if e {
                    in_echo = true;
                } else {
                    in_clean = true;
                }
            }
        }
        if in_echo && !in_clean {
            out.removed.push(w.clone());
        } else {
            out.kept.push(w.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::{DiarSpeaker, EmbeddingModelId, SpeakerTurn};

    fn model() -> EmbeddingModelId {
        EmbeddingModelId::new("test-model")
    }

    fn emb(v: [f32; 3]) -> Embedding {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        Embedding::new(model(), v.iter().map(|x| x / n).collect())
    }

    fn diar(speakers: &[[f32; 3]], turns: &[(usize, u64, u64)]) -> Diarization {
        Diarization {
            speakers: speakers
                .iter()
                .map(|&c| DiarSpeaker {
                    centroid: emb(c),
                    speech: Duration::from_secs(5),
                })
                .collect(),
            turns: turns
                .iter()
                .map(|&(speaker, s, e)| SpeakerTurn {
                    speaker,
                    start: MeetingTime::from_millis(s),
                    end: MeetingTime::from_millis(e),
                })
                .collect(),
            embedding_model: model(),
        }
    }

    fn span(s: u64, e: u64, v: [f32; 3]) -> SpanEmbedding {
        SpanEmbedding {
            start: MeetingTime::from_millis(s),
            end: MeetingTime::from_millis(e),
            embedding: emb(v),
        }
    }

    fn config() -> SpeakerEchoConfig {
        SpeakerEchoConfig {
            margin: 0.1,
            same_speaker: 0.5,
            delay: Some(Duration::from_millis(150)),
            tolerance: Duration::from_secs(1),
            min_solo: Duration::from_millis(500),
        }
    }

    const ME: [f32; 3] = [1.0, 0.0, 0.0];
    const THEM: [f32; 3] = [0.0, 1.0, 0.0];

    #[test]
    fn diar_echo_spans_sounding_like_them_while_they_talk_are_echo() {
        // Mic: the user (Me) and an echo cluster; reference: one far-end speaker at 0-10 s.
        let mic = diar(&[ME, [0.1, 0.95, 0.0]], &[(0, 0, 4_000), (1, 4_000, 8_000)]);
        let reference = diar(&[THEM], &[(0, 0, 10_000)]);
        assert_eq!(me_speakers(&mic, &reference, &config()).unwrap(), [0]);
        let spans = [
            span(0, 2_000, ME),
            span(4_000, 6_000, [0.1, 0.95, 0.05]),
            // Double talk where the user dominates: closer to Me, kept.
            span(6_000, 8_000, [0.8, 0.5, 0.0]),
            // Sounds like Them but the far end is silent then (12-14 s, delay 150 ms): kept.
            span(12_000, 14_000, THEM),
        ];
        let echo = speaker_echo_spans(&mic, &reference, &spans, &config()).unwrap();
        assert_eq!(echo, [false, true, false, false]);
        let words = [
            Word::new("mine", 500, 900, 0.9),
            Word::new("echo", 4_500, 4_900, 0.9),
            Word::new("both", 6_500, 6_900, 0.9),
            Word::new("unspanned", 9_000, 9_400, 0.9),
        ];
        let out = remove_speaker_echo(&words, &spans, &echo);
        let kept: Vec<&str> = out.kept.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(kept, ["mine", "both", "unspanned"]);
    }

    #[test]
    fn diar_echo_a_mic_that_only_hears_echo_has_no_me_speaker() {
        let mic = diar(&[[0.05, 1.0, 0.0]], &[(0, 0, 5_000)]);
        let reference = diar(&[THEM], &[(0, 0, 5_000)]);
        assert!(me_speakers(&mic, &reference, &config()).unwrap().is_empty());
        let spans = [
            span(0, 2_000, [0.1, 1.0, 0.0]),
            span(2_000, 4_000, [1.0, 0.1, 0.2]),
        ];
        // Matches Them at the same-speaker threshold: echo; an unrelated voice is not.
        let echo = speaker_echo_spans(&mic, &reference, &spans, &config()).unwrap();
        assert_eq!(echo, [true, false]);
    }

    #[test]
    fn diar_echo_without_a_lock_nothing_is_echo() {
        // Headphones: no echo path, so no delay lock. The mic hears a user whose voice is
        // close to the far end's while the far end talks; nothing may be removed.
        let mic = diar(&[[0.1, 0.95, 0.0]], &[(0, 0, 8_000)]);
        let reference = diar(&[THEM], &[(0, 0, 10_000)]);
        let cfg = SpeakerEchoConfig {
            delay: None,
            ..config()
        };
        assert_eq!(me_speakers(&mic, &reference, &cfg).unwrap(), [0]);
        let spans = [span(0, 2_000, THEM), span(2_000, 4_000, [0.1, 0.95, 0.0])];
        let echo = speaker_echo_spans(&mic, &reference, &spans, &cfg).unwrap();
        assert_eq!(echo, [false, false]);
    }

    #[test]
    fn diar_echo_a_user_who_sounds_like_them_is_me_when_talking_alone() {
        // The user's centroid matches the far-end speaker at the same-speaker threshold,
        // but the user also talks at 12-15 s while the far end (0-10 s, delay 150 ms) is
        // silent: echo cannot exist then, so the user is Me and keeps the double talk.
        let user = [0.2, 0.95, 0.0];
        let mic = diar(&[user], &[(0, 2_000, 6_000), (0, 12_000, 15_000)]);
        let reference = diar(&[THEM], &[(0, 0, 10_000)]);
        assert_eq!(me_speakers(&mic, &reference, &config()).unwrap(), [0]);
        let spans = [span(2_000, 4_000, user), span(12_000, 14_000, user)];
        let echo = speaker_echo_spans(&mic, &reference, &spans, &config()).unwrap();
        assert_eq!(echo, [false, false]);
        // Talking alone for less than `min_solo` (a turn edge) does not make a speaker Me.
        let edge = diar(&[user], &[(0, 2_000, 11_300)]);
        assert!(
            me_speakers(&edge, &reference, &config())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn diar_echo_word_covered_by_a_clean_span_is_kept() {
        let spans = [span(0, 2_000, THEM), span(1_000, 3_000, ME)];
        let words = [
            Word::new("a", 200, 400, 0.9),
            Word::new("b", 1_400, 1_600, 0.9),
        ];
        let out = remove_speaker_echo(&words, &spans, &[true, false]);
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0].text, "b");
    }

    #[test]
    fn diar_echo_solo_time_ignores_covered_stretches() {
        assert_eq!(uncovered_ms(0, 1_000, &[]), 1_000);
        assert_eq!(
            uncovered_ms(0, 1_000, &[(-50, 200), (100, 300), (800, 900)]),
            600
        );
        assert_eq!(uncovered_ms(0, 1_000, &[(-10, 2_000)]), 0);
        assert_eq!(uncovered_ms(500, 600, &[(0, 100), (700, 800)]), 100);
    }

    #[test]
    fn diar_echo_margin_is_keyed_by_exact_model() {
        let campplus = EmbeddingModelId::new("campplus-en-voxceleb@357a834f702b");
        assert_eq!(
            calibrated_speaker_echo_margin(&campplus),
            Some(CAMPPLUS_SPEAKER_ECHO_MARGIN)
        );
        let cfg = SpeakerEchoConfig::for_model(&campplus).unwrap();
        assert_eq!(cfg.margin, CAMPPLUS_SPEAKER_ECHO_MARGIN);
        assert_eq!(cfg.delay, None);
        // Another model (or another revision of CAM++) has no speaker check.
        let other = EmbeddingModelId::new("campplus-en-voxceleb@000000000000");
        assert_eq!(calibrated_speaker_echo_margin(&other), None);
        assert!(matches!(
            SpeakerEchoConfig::for_model(&other),
            Err(DiarError::Config(_))
        ));
    }

    #[test]
    fn diar_echo_mismatched_models_are_errors() {
        let mic = diar(&[ME], &[(0, 0, 1_000)]);
        let mut reference = diar(&[THEM], &[(0, 0, 1_000)]);
        reference.speakers[0].centroid =
            Embedding::new(EmbeddingModelId::new("other"), vec![0.0, 1.0, 0.0]);
        assert!(me_speakers(&mic, &reference, &config()).is_err());
    }
    /// The span cutting `asr_eval`'s `mic_spans` did inline before it moved here (main at
    /// c1b52e1), kept verbatim as the reference the moved function must equal.
    fn harness_pieces(
        turns: &[SpeakerTurn],
        len: usize,
        min: usize,
    ) -> Vec<(u64, u64, usize, usize)> {
        let to_sample = |t: MeetingTime| (t.as_millis() as usize * 16).min(len);
        let mut pieces: Vec<(MeetingTime, MeetingTime)> = Vec::new();
        for t in turns {
            let (s, e) = (t.start.as_millis(), t.end.as_millis());
            let mut a = s;
            while a < e {
                let mut b = (a + 2_000).min(e);
                if (e - b) as usize * 16 < min {
                    b = e;
                }
                pieces.push((MeetingTime::from_millis(a), MeetingTime::from_millis(b)));
                a = b;
            }
        }
        pieces
            .into_iter()
            .filter(|(a, b)| to_sample(*b) - to_sample(*a) >= min)
            .map(|(a, b)| (a.as_millis(), b.as_millis(), to_sample(a), to_sample(b)))
            .collect()
    }

    #[test]
    fn diar_echo_span_pieces_equal_the_former_harness_cutting() {
        use rand_chacha::ChaCha8Rng;
        use rand_core::{Rng, SeedableRng};
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        let mut n = |m: u64| rng.next_u64() % m;
        for _ in 0..500 {
            let len = 16 * (n(20_000) as usize + 1);
            let min = [0, 160, 4_000, 8_000, 16_000][n(5) as usize];
            let mut turns = Vec::new();
            let mut t = n(500);
            for _ in 0..n(6) {
                let d = n(7_000) + 1;
                turns.push(SpeakerTurn {
                    speaker: n(3) as usize,
                    start: MeetingTime::from_millis(t),
                    end: MeetingTime::from_millis(t + d),
                });
                t += d + n(1_500);
            }
            let got: Vec<(u64, u64, usize, usize)> =
                echo_span_pieces(&turns, crate::audio::PIPELINE_RATE, len, min)
                    .into_iter()
                    .map(|p| {
                        (
                            p.start.as_millis(),
                            p.end.as_millis(),
                            p.samples.start,
                            p.samples.end,
                        )
                    })
                    .collect();
            assert_eq!(
                got,
                harness_pieces(&turns, len, min),
                "{turns:?} len {len} min {min}"
            );
        }
    }

    #[test]
    fn diar_echo_span_pieces_cut_two_seconds_and_fold_short_remainders() {
        let turn = |s: u64, e: u64| SpeakerTurn {
            speaker: 0,
            start: MeetingTime::from_millis(s),
            end: MeetingTime::from_millis(e),
        };
        let turns = [turn(0, 4_500), turn(5_000, 5_100)];
        let ms = |min: usize| -> Vec<(u64, u64)> {
            echo_span_pieces(&turns, crate::audio::PIPELINE_RATE, 16 * 6_000, min)
                .iter()
                .map(|p| (p.start.as_millis(), p.end.as_millis()))
                .collect()
        };
        // 0.25 s minimum: the 0.5 s remainder is its own piece; the 0.1 s turn is dropped.
        assert_eq!(ms(4_000), [(0, 2_000), (2_000, 4_000), (4_000, 4_500)]);
        // 0.6 s minimum: the 0.5 s remainder joins the previous piece.
        assert_eq!(ms(9_600), [(0, 2_000), (2_000, 4_500)]);
        let p = echo_span_pieces(&turns, crate::audio::PIPELINE_RATE, 16 * 6_000, 4_000);
        assert_eq!(p[1].samples, 32_000..64_000);
        // Pieces never reach past the audio.
        let short = echo_span_pieces(&turns, crate::audio::PIPELINE_RATE, 16 * 3_000, 4_000);
        assert_eq!(short.last().unwrap().samples, 32_000..48_000);
    }
}
