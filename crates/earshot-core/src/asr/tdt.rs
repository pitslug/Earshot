//! Greedy TDT decoding (T6.10e2): Token-and-Duration Transducers (Xu et al. 2023, "Efficient
//! Sequence Transduction by Jointly Predicting Tokens and Durations"), as used by NVIDIA's
//! Parakeet TDT models.
//!
//! Reference: NVIDIA NeMo `nemo/collections/asr/parts/submodules/rnnt_greedy_decoding.py`,
//! `GreedyTDTInfer._greedy_decode`, with the Parakeet decoding config (`durations
//! [0, 1, 2, 3, 4]`, `greedy.max_symbols 10`). At frame `t` the joint network scores the
//! encoder frame against the prediction network's output; the best token (over the vocabulary
//! including blank) and the best duration are taken independently. A non-blank token is
//! emitted at `t` and the prediction network advances on it; blank leaves it unchanged. Then
//! `t` advances by the duration. With duration 0 the decoder stays on the frame and emits
//! again, up to `max_symbols` per frame, after which `t` advances by one.
//!
//! One shortcut, the same as sherpa-onnx (`offline-transducer-greedy-search-nemo-decoder.cc`,
//! `DecodeOneTDT`) and onnx-asr (`asr.py`): a blank with duration 0 advances `t` by one at
//! once. NeMo would repeat the identical joint call (same frame, same prediction state) until
//! the symbol budget is spent and then advance by one; the output is the same.
//!
//! Tokens are returned as [`CtcToken`]s (start frame, end frame `t + max(duration, 1)`, and the
//! log-softmax probability of the token over the vocabulary), so [`super::words_from_tokens`]
//! times words for both decoders.

use super::ctc::CtcToken;
use super::transcriber::{AsrError, CancelToken};

/// A TDT model: the prediction network and the joint network.
pub trait TdtModel {
    /// The prediction network's recurrent state.
    type State;

    /// The state before any token (zeros).
    ///
    /// # Errors
    /// Model errors.
    fn initial_state(&mut self) -> Result<Self::State, AsrError>;

    /// One prediction-network step on `token` (the blank id starts a sequence): its output
    /// and the next state.
    ///
    /// # Errors
    /// Model errors.
    fn predict(
        &mut self,
        token: usize,
        state: &Self::State,
    ) -> Result<(Vec<f32>, Self::State), AsrError>;

    /// Joint network on encoder frame `frame` and prediction output `pred`: the vocabulary's
    /// logits (blank included) followed by one logit per duration.
    ///
    /// # Errors
    /// Model errors.
    fn joint(&mut self, frame: usize, pred: &[f32]) -> Result<Vec<f32>, AsrError>;
}

/// Decoding settings (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdtConfig {
    /// Classes of the vocabulary, blank included.
    pub vocab_len: usize,
    /// The blank's id.
    pub blank: usize,
    /// Frame skips the duration logits stand for.
    pub durations: Vec<usize>,
    /// Most tokens emitted on one frame.
    pub max_symbols: usize,
}

impl TdtConfig {
    /// The Parakeet decoding settings for a vocabulary of `vocab_len` classes with `blank`.
    #[must_use]
    pub fn parakeet(vocab_len: usize, blank: usize) -> Self {
        Self {
            vocab_len,
            blank,
            durations: vec![0, 1, 2, 3, 4],
            max_symbols: 10,
        }
    }
}

fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i)
}

fn log_softmax_at(v: &[f32], k: usize) -> f32 {
    let max = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sum: f32 = v.iter().map(|x| (x - max).exp()).sum();
    v[k] - max - sum.ln()
}

/// Greedy TDT decoding over `frames` encoder frames (module docs).
///
/// # Errors
/// [`AsrError::Config`] for an invalid config or joint output of the wrong length; model
/// errors; [`AsrError::Cancelled`].
pub fn tdt_greedy<M: TdtModel + ?Sized>(
    model: &mut M,
    frames: usize,
    config: &TdtConfig,
    cancel: &CancelToken,
) -> Result<Vec<CtcToken>, AsrError> {
    if config.blank >= config.vocab_len || config.durations.is_empty() || config.max_symbols == 0 {
        return Err(AsrError::Config(format!("invalid TDT config {config:?}")));
    }
    let want = config.vocab_len + config.durations.len();
    let start = model.initial_state()?;
    let (mut pred, mut state) = model.predict(config.blank, &start)?;
    let mut out = Vec::new();
    let mut t = 0;
    let mut emitted = 0;
    while t < frames {
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        let logits = model.joint(t, &pred)?;
        if logits.len() != want {
            return Err(AsrError::Config(format!(
                "TDT joint returned {} logits, expected {want}",
                logits.len()
            )));
        }
        let (tokens, durs) = logits.split_at(config.vocab_len);
        let k = argmax(tokens);
        let skip = config.durations[argmax(durs)];
        if k != config.blank {
            out.push(CtcToken {
                id: k,
                first_frame: t,
                end_frame: t + skip.max(1),
                log_prob: log_softmax_at(tokens, k),
            });
            (pred, state) = model.predict(k, &state)?;
            emitted += 1;
        }
        if skip > 0 {
            t += skip;
            emitted = 0;
        } else if k == config.blank || emitted >= config.max_symbols {
            t += 1;
            emitted = 0;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted model: at (frame, number of tokens emitted so far) the joint picks
    /// `(token, duration index)`; anything unscripted is blank with duration 1.
    /// `(frame, tokens emitted so far) -> (token, duration index)`.
    type Step = ((usize, usize), (usize, usize));

    struct Script {
        steps: Vec<Step>,
        vocab: usize,
        predicts: Vec<usize>,
        joints: usize,
    }

    impl TdtModel for Script {
        type State = usize; // tokens emitted so far

        fn initial_state(&mut self) -> Result<usize, AsrError> {
            Ok(0)
        }

        fn predict(&mut self, token: usize, state: &usize) -> Result<(Vec<f32>, usize), AsrError> {
            self.predicts.push(token);
            let next = if token == self.vocab - 1 {
                *state
            } else {
                state + 1
            };
            #[allow(clippy::cast_precision_loss)]
            Ok((vec![next as f32], next))
        }

        fn joint(&mut self, frame: usize, pred: &[f32]) -> Result<Vec<f32>, AsrError> {
            self.joints += 1;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let emitted = pred[0] as usize;
            let (k, d) = self
                .steps
                .iter()
                .find(|(key, _)| *key == (frame, emitted))
                .map_or((self.vocab - 1, 1), |(_, v)| *v);
            let mut v = vec![0.0f32; self.vocab + 5];
            v[k] = 5.0;
            v[self.vocab + d] = 5.0;
            Ok(v)
        }
    }

    fn script(steps: &[Step]) -> Script {
        Script {
            steps: steps.to_vec(),
            vocab: 6,
            predicts: Vec::new(),
            joints: 0,
        }
    }

    fn run(s: &mut Script, frames: usize) -> Vec<CtcToken> {
        tdt_greedy(s, frames, &TdtConfig::parakeet(6, 5), &CancelToken::new()).unwrap()
    }

    #[test]
    fn asr_tdt_emits_tokens_and_skips_by_their_durations() {
        // frame 0: token 1, skip 2 -> frame 2: token 2, skip 3 -> frame 5: blanks to the end.
        let mut s = script(&[((0, 0), (1, 2)), ((2, 1), (2, 3))]);
        let t = run(&mut s, 8);
        let got: Vec<(usize, usize, usize)> = t
            .iter()
            .map(|t| (t.id, t.first_frame, t.end_frame))
            .collect();
        assert_eq!(got, [(1, 0, 2), (2, 2, 5)]);
        // Frames 5, 6, 7 are blanks of duration 1: 2 + 3 joint calls in all.
        assert_eq!(s.joints, 5);
        // The prediction network runs once to start (on blank) and once per emitted token.
        assert_eq!(s.predicts, [5, 1, 2]);
        assert!(t[0].log_prob < 0.0 && t[0].log_prob > -0.1);
    }

    #[test]
    fn asr_tdt_duration_zero_emits_again_on_the_same_frame_up_to_the_limit() {
        // Frame 0 emits 1, 2, 3 with duration 0, then blank (duration 1).
        let mut s = script(&[((0, 0), (1, 0)), ((0, 1), (2, 0)), ((0, 2), (3, 0))]);
        let t = run(&mut s, 2);
        let got: Vec<(usize, usize)> = t.iter().map(|t| (t.id, t.first_frame)).collect();
        assert_eq!(got, [(1, 0), (2, 0), (3, 0)]);
        // An endless duration-0 token stops at max_symbols (10) per frame, then moves on.
        let steps: Vec<_> = (0..50).map(|n| ((0, n), (4, 0))).collect();
        let mut s = script(&steps);
        let t = run(&mut s, 1);
        assert_eq!(t.len(), 10);
    }

    #[test]
    fn asr_tdt_blank_with_duration_zero_moves_on() {
        // Blank (5) with duration 0 on frame 0: no emission and no endless loop.
        let mut s = script(&[((0, 0), (5, 0)), ((1, 0), (2, 1))]);
        let t = run(&mut s, 2);
        let got: Vec<(usize, usize)> = t.iter().map(|t| (t.id, t.first_frame)).collect();
        assert_eq!(got, [(2, 1)]);
        assert_eq!(s.joints, 2);
    }

    #[test]
    fn asr_tdt_rejects_bad_configs_and_joint_shapes_and_honours_cancel() {
        let mut s = script(&[]);
        let bad = TdtConfig {
            durations: vec![],
            ..TdtConfig::parakeet(6, 5)
        };
        assert!(matches!(
            tdt_greedy(&mut s, 3, &bad, &CancelToken::new()),
            Err(AsrError::Config(_))
        ));
        let wrong = TdtConfig::parakeet(7, 5);
        assert!(matches!(
            tdt_greedy(&mut s, 3, &wrong, &CancelToken::new()),
            Err(AsrError::Config(_))
        ));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            tdt_greedy(&mut s, 3, &TdtConfig::parakeet(6, 5), &cancel),
            Err(AsrError::Cancelled)
        ));
    }
}
