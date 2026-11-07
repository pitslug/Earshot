//! CTC vocabulary, greedy decoding and word timings (T6.10e1), for NeMo Parakeet models.
//!
//! - [`Vocab`] reads the token list shipped with the ONNX exports (sherpa-onnx `tokens.txt`,
//!   onnx-asr `vocab.txt`: one `<token> <id>` per line, split on the last space, ids 0..n in
//!   order, `<blk>` the blank, SentencePiece's U+2581 marking a word start).
//! - [`ctc_greedy`] is greedy CTC decoding (Graves et al. 2006, "best path"): the most likely
//!   class per frame, repeats collapsed, blanks removed. Each token keeps the frames of its
//!   run and the mean log-probability over them.
//! - [`words_from_tokens`] groups tokens into words at U+2581 and times them from their frames
//!   (sherpa-onnx `Convert()` and onnx-asr `asr.py` time a token by its first frame times the
//!   encoder frame, 80 ms for FastConformer's 8x subsampling); a word ends where its last token's
//!   run ends. Control tokens (`<unk>`, `<blk>`, byte tokens like `<0x0A>`) are not text.

use std::time::Duration;

use super::transcriber::AsrError;
use super::types::{MeetingTime, Word};

/// SentencePiece's word-start marker.
pub const WORD_START: char = '\u{2581}';

/// The blank token's name in NeMo exports.
pub const BLANK: &str = "<blk>";

/// A model's token list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vocab {
    tokens: Vec<String>,
    blank: usize,
}

impl Vocab {
    /// Parses `<token> <id>` lines (split on the last space; ids 0..n in order, one `<blk>`).
    ///
    /// # Errors
    /// [`AsrError::Config`] for a malformed line, out-of-order ids, or no blank.
    pub fn parse(text: &str) -> Result<Self, AsrError> {
        let bad = |m: String| Err(AsrError::Config(format!("vocabulary: {m}")));
        let mut tokens = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let Some((token, id)) = line.rsplit_once(' ') else {
                return bad(format!("line {} has no id: {line:?}", n + 1));
            };
            let Ok(id) = id.parse::<usize>() else {
                return bad(format!("line {} has a bad id: {line:?}", n + 1));
            };
            if id != tokens.len() {
                return bad(format!(
                    "line {} has id {id}, expected {}",
                    n + 1,
                    tokens.len()
                ));
            }
            tokens.push(token.to_owned());
        }
        let Some(blank) = tokens.iter().position(|t| t == BLANK) else {
            return bad(format!("no {BLANK} token"));
        };
        Ok(Self { tokens, blank })
    }

    /// Number of classes (tokens including the blank).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether the list is empty (never, for a parsed vocabulary).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The blank's id.
    #[must_use]
    pub fn blank(&self) -> usize {
        self.blank
    }

    /// Token text of `id`.
    #[must_use]
    pub fn token(&self, id: usize) -> Option<&str> {
        self.tokens.get(id).map(String::as_str)
    }
}

/// One decoded token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CtcToken {
    /// Token id.
    pub id: usize,
    /// First frame of its run.
    pub first_frame: usize,
    /// One past the last frame of its run.
    pub end_frame: usize,
    /// Mean log-probability of the token over its run.
    pub log_prob: f32,
}

/// Greedy CTC decoding of `log_probs` (`frames x classes`, row-major).
///
/// # Errors
/// [`AsrError::Config`] if the buffer does not hold `frames x classes` values or `blank` is
/// not a class.
pub fn ctc_greedy(
    log_probs: &[f32],
    frames: usize,
    classes: usize,
    blank: usize,
) -> Result<Vec<CtcToken>, AsrError> {
    if classes == 0 || blank >= classes || log_probs.len() != frames * classes {
        return Err(AsrError::Config(format!(
            "CTC output of {} values is not {frames} frames x {classes} classes (blank {blank})",
            log_probs.len()
        )));
    }
    let mut out: Vec<CtcToken> = Vec::new();
    let mut sum = 0.0f32;
    let mut prev: Option<usize> = None;
    for (t, row) in log_probs.chunks(classes).enumerate() {
        let (k, &p) = row
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap_or((blank, &0.0));
        if Some(k) == prev && k != blank {
            if let Some(last) = out.last_mut() {
                last.end_frame = t + 1;
                sum += p;
                #[allow(clippy::cast_precision_loss)]
                let n = (last.end_frame - last.first_frame) as f32;
                last.log_prob = sum / n;
            }
        } else if k != blank {
            out.push(CtcToken {
                id: k,
                first_frame: t,
                end_frame: t + 1,
                log_prob: p,
            });
            sum = p;
        }
        prev = Some(k);
    }
    Ok(out)
}

fn is_control(token: &str) -> bool {
    token.len() > 2 && token.starts_with('<') && token.ends_with('>')
}

fn at(frame: usize, frame_len: Duration) -> MeetingTime {
    let ms = u64::try_from(frame)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::try_from(frame_len.as_millis()).unwrap_or(u64::MAX));
    MeetingTime::from_millis(ms)
}

/// Words from decoded tokens (module docs), times relative to the buffer start.
#[must_use]
pub fn words_from_tokens(tokens: &[CtcToken], vocab: &Vocab, frame_len: Duration) -> Vec<Word> {
    struct Building {
        text: String,
        start: usize,
        end: usize,
        log_probs: Vec<f32>,
    }
    let finish = |b: Building, out: &mut Vec<Word>| {
        if b.text.is_empty() {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let mean = b.log_probs.iter().sum::<f32>() / b.log_probs.len().max(1) as f32;
        out.push(Word {
            text: b.text,
            start: at(b.start, frame_len),
            end: at(b.end, frame_len),
            probability: mean.exp().clamp(0.0, 1.0),
        });
    };
    let mut out = Vec::new();
    let mut cur: Option<Building> = None;
    for t in tokens {
        let Some(text) = vocab.token(t.id) else {
            continue;
        };
        if t.id == vocab.blank() || is_control(text) {
            continue;
        }
        let starts_word = text.starts_with(WORD_START);
        let piece = text.trim_start_matches(WORD_START);
        match (&mut cur, starts_word) {
            (Some(b), false) => {
                b.text.push_str(piece);
                b.end = t.end_frame;
                b.log_probs.push(t.log_prob);
            }
            _ => {
                if let Some(b) = cur.take() {
                    finish(b, &mut out);
                }
                cur = Some(Building {
                    text: piece.to_owned(),
                    start: t.first_frame,
                    end: t.end_frame,
                    log_probs: vec![t.log_prob],
                });
            }
        }
    }
    if let Some(b) = cur {
        finish(b, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(80);

    fn vocab() -> Vocab {
        Vocab::parse("<unk> 0\n\u{2581}the 1\n\u{2581}c 2\nat 3\n\u{2581} 4\n. 5\n<blk> 6\n")
            .unwrap()
    }

    /// Log-probs where frame t's best class is `best[t]` (log 0.9), others share the rest.
    fn frames(best: &[usize], classes: usize) -> Vec<f32> {
        let other = (0.1f32 / (classes - 1) as f32).ln();
        best.iter()
            .flat_map(|&b| (0..classes).map(move |k| if k == b { 0.9f32.ln() } else { other }))
            .collect()
    }

    #[test]
    fn asr_ctc_vocab_parses_the_export_format() {
        let v = vocab();
        assert_eq!((v.len(), v.blank()), (7, 6));
        assert_eq!(v.token(1), Some("\u{2581}the"));
        // A token containing a space is split on the last space.
        let v = Vocab::parse("a b 0\n<blk> 1\n").unwrap();
        assert_eq!(v.token(0), Some("a b"));
        for bad in [
            "<unk> 0\n",
            "a 1\n<blk> 0\n",
            "a\n<blk> 1\n",
            "a x\n<blk> 1\n",
        ] {
            assert!(
                matches!(Vocab::parse(bad), Err(AsrError::Config(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn asr_ctc_greedy_collapses_repeats_and_removes_blanks() {
        // the the - the | c c at at
        let lp = frames(&[1, 1, 6, 1, 2, 2, 3, 3], 7);
        let t = ctc_greedy(&lp, 8, 7, 6).unwrap();
        let ids: Vec<usize> = t.iter().map(|t| t.id).collect();
        assert_eq!(ids, [1, 1, 2, 3], "a blank separates two of the same token");
        assert_eq!((t[0].first_frame, t[0].end_frame), (0, 2));
        assert_eq!((t[1].first_frame, t[1].end_frame), (3, 4));
        assert_eq!((t[3].first_frame, t[3].end_frame), (6, 8));
        assert!((t[0].log_prob - 0.9f32.ln()).abs() < 1e-6);
        assert!(ctc_greedy(&lp, 7, 7, 6).is_err());
        assert!(ctc_greedy(&lp, 8, 7, 7).is_err());
    }

    #[test]
    fn asr_ctc_words_join_pieces_and_take_frame_times() {
        // "the cat." : the(0-2) c(3) at(4-6) .(6)
        let lp = frames(&[1, 1, 6, 2, 3, 3, 5, 6], 7);
        let t = ctc_greedy(&lp, 8, 7, 6).unwrap();
        let w = words_from_tokens(&t, &vocab(), FRAME);
        let texts: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["the", "cat."]);
        assert_eq!((w[0].start.as_millis(), w[0].end.as_millis()), (0, 160));
        assert_eq!((w[1].start.as_millis(), w[1].end.as_millis()), (240, 560));
        assert!((w[1].probability - 0.9).abs() < 1e-5);
    }

    #[test]
    fn asr_ctc_control_tokens_and_a_bare_word_marker_are_not_text() {
        // <unk> then a bare marker followed by "at": one word "at".
        let lp = frames(&[0, 4, 3, 6], 7);
        let t = ctc_greedy(&lp, 4, 7, 6).unwrap();
        let w = words_from_tokens(&t, &vocab(), FRAME);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].text, "at");
        assert_eq!(w[0].start.as_millis(), 80, "the word starts at its marker");
    }
}
