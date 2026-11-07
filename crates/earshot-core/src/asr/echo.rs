//! Cross-track echo de-duplication (T2.4e, PLAN.md 4.0: the transcript must be clean).
//!
//! Far-end speech that leaks through the echo canceller into the mic is recognised a
//! second time on the mic ("Me") track. [`dedupe_echo`] removes such words by comparing
//! the mic transcript with the reference ("Them") transcript: a mic word is echo when it
//! is the same word as a reference word, spoken about one echo delay later, as part of a
//! run of at least [`EchoDedupConfig::min_run`] words that repeat the reference in order.
//!
//! - **Only with an echo path.** The delay is the AEC's delay lock
//!   (`EchoDedupConfig::delay`). Without a lock there is no evidence of an echo path
//!   (headphones, a muted speaker, a canceller that never converged) and nothing is
//!   removed (T2.4f, coordinator decision for the owner: no echo cleaning without a lock).
//! - **In order, at about the echo delay.** A pair (mic word, reference word) is a
//!   candidate when the normalised texts are equal ([`super::normalize_token`]) and the
//!   mic word starts `delay +- tolerance` after the reference word. Candidates chain when both indices advance by at most
//!   `max_gap + 1` (one recogniser error on either track does not break a run) and the
//!   offset changes by at most `step_jitter` between neighbours, so clock drift, which moves
//!   the offset slowly, keeps a run together while unrelated coincidences do not.
//! - **Runs, not words.** Only chains of `min_run` or more pairs are echo. A word both
//!   sides say at once ("yes", "okay") and short shared phrases survive; a near-end speaker
//!   quoting the far end a few seconds later is outside the delay window and survives.
//!   Echo shorter than `min_run` words is not removed (a known limit of the rule).
//!
//! The rule only sees text: residual echo that the recogniser garbles ("the cry issued
//! for its port" for "the cry issued from his pores") breaks runs and survives. On the
//! `asr.echo_leak` development split (`asr_eval --leak-dev`, T2.4e) de-duplication removed
//! about two thirds of the leaked far-end words; the rest were such fragments.
//!
//! Word times come from the recogniser; whisper.cpp's word times can be off by several
//! hundred ms (and collapse to the window end, see [`super::LocalAgreement`]), which the
//! default tolerance of 1 s absorbs. Defaults were chosen from these properties and the
//! AEC's delay range before measuring, not tuned on acceptance scenarios.
//!
//! **Rejected: an acoustic gate** (T2.4e, removed in T2.4f; the code is in commit
//! 6674476). It dropped mic words spoken while the canceller saw only the far end
//! (reference active, echo path locked, no double-talk decision averaged over a window).
//! It caught garbled residual echo but cost the user's words wherever the double-talk
//! detector missed the near end, so it was rejected for near-end loss (coordinator decision
//! for the owner) in favour of the final-pass speaker check
//! (`diar::speaker_echo_spans`). Dev sweep (`asr_eval --leak-dev`), leak after text
//! de-duplication + gate and near-end words kept of heard in double talk, canceller
//! output / raw mic:
//!
//! | min far-only share / near share / near window | leak aec | leak raw | near kept aec | near kept raw |
//! |---|---|---|---|---|
//! | text only (no gate) | 2.10% | 21.22% | 100% | 92.2% |
//! | 0.8 / 0.5 / 400 ms | 1.66% | 8.21% | 79.4% | 85.9% |
//! | 0.6 / 0.5 / 400 ms | 1.57% | 6.90% | 79.4% | 85.9% |
//! | 0.6 / 0.7 / 800 ms | 1.05% | 3.06% | 69.5% | 78.1% |

use std::time::Duration;

use super::agreement::normalize_token;
use super::transcriber::AsrError;
use super::types::{Segment, Word};

/// Settings of [`dedupe_echo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EchoDedupConfig {
    /// The echo delay (mic behind reference) from the AEC's delay lock; `None` when the
    /// canceller has no lock, and then nothing is echo.
    pub delay: Option<Duration>,
    /// Allowed difference between a pair's offset and the delay (word-time jitter of the
    /// recogniser on both tracks, plus drift since the lock was read). Default 1 s.
    pub tolerance: Duration,
    /// Largest change of offset between neighbouring pairs of one run. Default 500 ms.
    pub step_jitter: Duration,
    /// Shortest run of repeated words that counts as echo. Default 3.
    pub min_run: usize,
    /// Words either track may have between neighbouring pairs of a run (recogniser errors).
    /// Default 1.
    pub max_gap: usize,
}

impl Default for EchoDedupConfig {
    fn default() -> Self {
        EchoDedupConfig {
            delay: None,
            tolerance: Duration::from_secs(1),
            step_jitter: Duration::from_millis(500),
            min_run: 3,
            max_gap: 1,
        }
    }
}

impl EchoDedupConfig {
    /// The defaults with the AEC's delay lock.
    #[must_use]
    pub fn with_delay(delay: Duration) -> Self {
        EchoDedupConfig {
            delay: Some(delay),
            ..Self::default()
        }
    }

    /// Checks the settings.
    ///
    /// # Errors
    /// [`AsrError::Config`] if `min_run` is below 2 (single shared words must survive).
    pub fn validate(&self) -> Result<(), AsrError> {
        if self.min_run < 2 {
            return Err(AsrError::Config(
                "echo de-duplication needs min_run >= 2 so shared single words survive".into(),
            ));
        }
        Ok(())
    }

    /// Allowed offset range (mic start minus reference start) in ms; `None` without a lock.
    fn offset_range(&self) -> Option<(i64, i64)> {
        let ms = |d: Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX / 4);
        let tol = ms(self.tolerance);
        self.delay.map(|d| (ms(d) - tol, ms(d) + tol))
    }
}

/// Result of [`dedupe_echo`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EchoDedup {
    /// Mic words that are not echo, in order.
    pub kept: Vec<Word>,
    /// Mic words removed as echo of the reference, in order.
    pub removed: Vec<Word>,
}

fn millis(w: &Word) -> i64 {
    i64::try_from(w.start.as_millis()).unwrap_or(i64::MAX / 4)
}

/// Splits `mic` into kept words and echo of `reference`; see the module docs. Both are
/// transcripts in time order (word starts non-decreasing), as recognisers produce them.
///
/// # Errors
/// [`AsrError::Config`] for an invalid `config`.
pub fn dedupe_echo(
    mic: &[Word],
    reference: &[Word],
    config: &EchoDedupConfig,
) -> Result<EchoDedup, AsrError> {
    config.validate()?;
    let echo = echo_mask(mic, reference, config);
    let mut out = EchoDedup::default();
    for (w, is_echo) in mic.iter().zip(echo) {
        if is_echo {
            out.removed.push(w.clone());
        } else {
            out.kept.push(w.clone());
        }
    }
    Ok(out)
}

/// For each mic word, whether it belongs to an echo run (none without a delay lock).
fn echo_mask(mic: &[Word], reference: &[Word], config: &EchoDedupConfig) -> Vec<bool> {
    let Some((lo, hi)) = config.offset_range() else {
        return vec![false; mic.len()];
    };
    let jitter = i64::try_from(config.step_jitter.as_millis()).unwrap_or(i64::MAX / 4);
    let mic_tok: Vec<String> = mic.iter().map(|w| normalize_token(&w.text)).collect();
    let ref_tok: Vec<String> = reference.iter().map(|w| normalize_token(&w.text)).collect();
    // Candidate pairs (mic index, reference index, offset ms), in (i, j) order.
    let mut pairs: Vec<(usize, usize, i64)> = Vec::new();
    // Reference words in time order (callers pass transcripts in order); only those
    // starting in `[mic - hi, mic - lo]` can pair with a mic word.
    let ref_ms: Vec<i64> = reference.iter().map(millis).collect();
    for (i, m) in mic.iter().enumerate() {
        if mic_tok[i].is_empty() {
            continue;
        }
        let t = millis(m);
        let first = ref_ms.partition_point(|&r| r < t - hi);
        for j in first..reference.len() {
            let offset = t - ref_ms[j];
            if offset < lo {
                break;
            }
            if ref_tok[j] == mic_tok[i] {
                pairs.push((i, j, offset));
            }
        }
    }
    let step = config.max_gap + 1;
    let linked = |a: &(usize, usize, i64), b: &(usize, usize, i64)| {
        b.0 > a.0
            && b.0 - a.0 <= step
            && b.1 > a.1
            && b.1 - a.1 <= step
            && (b.2 - a.2).abs() <= jitter
    };
    // Longest chain ending at each pair (pairs are sorted by mic index, so predecessors
    // come first), and longest chain starting at each pair.
    let n = pairs.len();
    let mut fwd = vec![1usize; n];
    for b in 0..n {
        for a in (0..b).rev() {
            if pairs[b].0 - pairs[a].0 > step {
                break;
            }
            if linked(&pairs[a], &pairs[b]) {
                fwd[b] = fwd[b].max(fwd[a] + 1);
            }
        }
    }
    let mut bwd = vec![1usize; n];
    for a in (0..n).rev() {
        for b in a + 1..n {
            if pairs[b].0 - pairs[a].0 > step {
                break;
            }
            if linked(&pairs[a], &pairs[b]) {
                bwd[a] = bwd[a].max(bwd[b] + 1);
            }
        }
    }
    let mut mask = vec![false; mic.len()];
    for (k, p) in pairs.iter().enumerate() {
        if fwd[k] + bwd[k] > config.min_run {
            mask[p.0] = true;
        }
    }
    mask
}

/// [`dedupe_echo`] over mic segments (live finals or a final pass): every segment keeps
/// its id and track and loses its echo words; a segment left without words is dropped.
/// Returns the segments and the number of words removed.
///
/// # Errors
/// [`AsrError::Config`] for an invalid `config`.
pub fn dedupe_echo_segments(
    mic: &[Segment],
    reference: &[Word],
    config: &EchoDedupConfig,
) -> Result<(Vec<Segment>, usize), AsrError> {
    config.validate()?;
    let words: Vec<Word> = mic.iter().flat_map(|s| s.words.iter().cloned()).collect();
    let mask = echo_mask(&words, reference, config);
    let mut removed = 0;
    let mut out = Vec::new();
    let mut k = 0;
    for s in mic {
        let mut kept = Vec::new();
        for w in &s.words {
            if mask[k] {
                removed += 1;
            } else {
                kept.push(w.clone());
            }
            k += 1;
        }
        if kept.len() == s.words.len() {
            out.push(s.clone());
        } else if !kept.is_empty() {
            let mut seg = Segment::from_words(s.id, s.track, kept, s.start);
            seg.speaker_id = s.speaker_id;
            out.push(seg);
        }
    }
    Ok((out, removed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::MeetingTime;
    use crate::identity::EntityId;

    /// Words of `text`, the first starting at `start_ms`, one every `step_ms`.
    fn words(text: &str, start_ms: u64, step_ms: u64) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(k, t)| {
                let s = start_ms + k as u64 * step_ms;
                Word::new(t, s, s + step_ms - 20, 0.9)
            })
            .collect()
    }

    /// The same words `delay_ms` later (an echo).
    fn echo_of(w: &[Word], delay_ms: u64) -> Vec<Word> {
        w.iter()
            .map(|x| x.shifted(MeetingTime::from_millis(delay_ms)))
            .collect()
    }

    fn merged(mut a: Vec<Word>, b: Vec<Word>) -> Vec<Word> {
        a.extend(b);
        a.sort_by_key(|w| w.start);
        a
    }

    fn texts(w: &[Word]) -> Vec<&str> {
        w.iter().map(|x| x.text.as_str()).collect()
    }

    fn run(mic: &[Word], reference: &[Word], cfg: &EchoDedupConfig) -> EchoDedup {
        dedupe_echo(mic, reference, cfg).unwrap()
    }

    fn configs() -> [EchoDedupConfig; 2] {
        [
            EchoDedupConfig::with_delay(Duration::from_millis(150)),
            EchoDedupConfig::with_delay(Duration::from_millis(450)),
        ]
    }

    #[test]
    fn asr_echo_run_is_removed_and_near_speech_kept() {
        let far = words("we should ship it on Friday", 1_000, 300);
        let near = words("hello there", 0, 250);
        let mic = merged(near, echo_of(&far, 150));
        for cfg in configs() {
            let out = run(&mic, &far, &cfg);
            assert_eq!(texts(&out.kept), ["hello", "there"], "{cfg:?}");
            assert_eq!(out.removed.len(), 6);
        }
    }

    #[test]
    fn asr_echo_talk_over_with_shared_words_survives() {
        // Both say "yes" and later "thank you" at about the same time, each in their own
        // words otherwise: runs shorter than min_run are not echo.
        let far = merged(
            words("yes I agree", 1_000, 300),
            words("thank you all", 3_000, 300),
        );
        let near = merged(
            words("yes that works", 1_100, 300),
            words("thank you very much", 3_050, 300),
        );
        let out = run(
            &near,
            &far,
            &EchoDedupConfig::with_delay(Duration::from_millis(100)),
        );
        assert_eq!(out.kept, near);
        assert!(out.removed.is_empty());
    }

    #[test]
    fn asr_echo_repeated_phrases_are_removed_each_time_but_a_later_repeat_survives() {
        let far = merged(
            words("see you next week", 2_000, 300),
            words("see you next week", 10_000, 300),
        );
        // The near end says the phrase itself at 20 s, while the far end is silent.
        let near = words("see you next week", 20_000, 300);
        let mic = merged(echo_of(&far, 200), near);
        let out = run(
            &mic,
            &far,
            &EchoDedupConfig::with_delay(Duration::from_millis(200)),
        );
        assert_eq!(out.removed.len(), 8);
        assert_eq!(texts(&out.kept), ["see", "you", "next", "week"]);
        assert_eq!(out.kept[0].start.as_millis(), 20_000);
    }

    #[test]
    fn asr_echo_near_end_quoting_the_far_end_survives() {
        let far = words("the budget is final", 5_000, 300);
        // Quoted 3 s later: outside every allowed echo delay.
        let mic = words("so the budget is final", 7_700, 300);
        for cfg in configs() {
            let out = run(&mic, &far, &cfg);
            assert!(out.removed.is_empty(), "{cfg:?}: {:?}", out.removed);
        }
    }

    #[test]
    fn asr_echo_follows_slow_delay_drift_within_a_run() {
        // The offset grows by 40 ms per word, from 100 ms to 860 ms over 20 words (clock
        // drift seen through recogniser timing): the run stays together.
        let far = words(
            "one two three four five six seven eight nine ten eleven twelve thirteen \
             fourteen fifteen sixteen seventeen eighteen nineteen twenty",
            1_000,
            400,
        );
        let drifted: Vec<Word> = far
            .iter()
            .enumerate()
            .map(|(k, w)| w.shifted(MeetingTime::from_millis(100 + 40 * k as u64)))
            .collect();
        for cfg in configs() {
            let out = run(&drifted, &far, &cfg);
            assert!(out.kept.is_empty(), "{cfg:?}: {:?}", texts(&out.kept));
        }
        // An offset jump larger than step_jitter splits the run in two runs of 10: with
        // `min_run` 11 the unsplit (drifting) run of 20 is echo and the split halves are not.
        let jumped: Vec<Word> = far
            .iter()
            .enumerate()
            .map(|(k, w)| w.shifted(MeetingTime::from_millis(if k < 10 { 100 } else { 700 })))
            .collect();
        let long = EchoDedupConfig {
            min_run: 11,
            ..EchoDedupConfig::with_delay(Duration::from_millis(400))
        };
        assert!(run(&drifted, &far, &long).kept.is_empty());
        let split = run(&jumped, &far, &long);
        assert!(split.removed.is_empty(), "{:?}", texts(&split.removed));
        // With the default `min_run` each half is still a run.
        assert!(
            run(
                &jumped,
                &far,
                &EchoDedupConfig::with_delay(Duration::from_millis(400))
            )
            .kept
            .is_empty()
        );
    }

    #[test]
    fn asr_echo_without_a_lock_nothing_is_removed() {
        // Headphones: no echo path, so the canceller has no delay lock. Even a near-end
        // run that repeats the far end at an echo-like offset is the user's own speech.
        let far = words("we should ship it on Friday", 1_000, 300);
        let mic = echo_of(&far, 150);
        let out = run(&mic, &far, &EchoDedupConfig::default());
        assert_eq!(out.kept, mic);
        assert!(out.removed.is_empty());
        let id = EntityId::from_bytes([1; 16]);
        let seg = Segment::from_words(id, id, mic.clone(), MeetingTime::ZERO);
        let (segs, removed) = dedupe_echo_segments(
            std::slice::from_ref(&seg),
            &far,
            &EchoDedupConfig::default(),
        )
        .unwrap();
        assert_eq!((segs, removed), (vec![seg], 0));
    }

    #[test]
    fn asr_echo_tolerates_one_recognition_error_inside_a_run() {
        let far = words("please send the report to the whole team", 1_000, 300);
        let mut mic = echo_of(&far, 150);
        mic[3].text = "repord".into();
        let out = run(
            &mic,
            &far,
            &EchoDedupConfig::with_delay(Duration::from_millis(150)),
        );
        assert_eq!(texts(&out.kept), ["repord"]);
    }

    #[test]
    fn asr_echo_empty_tracks() {
        let far = words("a b c d", 0, 300);
        let cfg = EchoDedupConfig::with_delay(Duration::from_millis(150));
        assert_eq!(run(&[], &far, &cfg), EchoDedup::default());
        let out = run(&far, &[], &cfg);
        assert_eq!(out.kept, far);
        assert!(out.removed.is_empty());
        assert_eq!(run(&[], &[], &cfg), EchoDedup::default());
    }

    #[test]
    fn asr_echo_segments_lose_echo_words_and_empty_segments() {
        let far = words("we should ship it on Friday", 1_000, 300);
        let echo = echo_of(&far, 150);
        let id = |b: u8| EntityId::from_bytes([b; 16]);
        let seg = |b: u8, w: Vec<Word>| Segment::from_words(id(b), id(9), w, MeetingTime::ZERO);
        let mixed = merged(words("hello", 0, 300), echo[..3].to_vec());
        let segments = vec![
            seg(1, mixed),
            seg(2, echo[3..].to_vec()),
            seg(3, words("bye", 9_000, 300)),
        ];
        let (out, removed) = dedupe_echo_segments(
            &segments,
            &far,
            &EchoDedupConfig::with_delay(Duration::from_millis(150)),
        )
        .unwrap();
        assert_eq!(removed, 6);
        assert_eq!(out.len(), 2, "the all-echo segment is dropped");
        assert_eq!((out[0].id, out[0].text.as_str()), (id(1), "hello"));
        assert_eq!(out[1], segments[2], "an untouched segment is unchanged");
    }

    #[test]
    fn asr_echo_config_rejects_single_word_runs() {
        let cfg = EchoDedupConfig {
            min_run: 1,
            ..EchoDedupConfig::default()
        };
        assert!(matches!(
            dedupe_echo(&[], &[], &cfg),
            Err(AsrError::Config(_))
        ));
    }
}
