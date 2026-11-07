//! Evaluation metrics used by the acceptance harness (PLAN.md 4.0).
//!
//! - [`wer`]: word error rate with substitution/deletion/insertion counts and corpus-level
//!   aggregation (total errors / total reference words).
//! - [`normalize`]: English text normaliser (port of Whisper's `EnglishTextNormalizer`).
//! - [`der`]: diarisation error rate (collar, overlap scored, optimal speaker mapping, UEM).
//! - [`sisdr`]: scale-invariant signal-to-distortion ratio.
//! - [`erle`]: segmental echo return loss enhancement.
//! - [`rouge`]: ROUGE-N, ROUGE-L and ROUGE-Lsum (port of Google's `rouge-score`).
//! - [`items`]: decisions and action items caught against annotated reference items.
//!
//! Everything here is pure computation over buffers and strings.

pub mod der;
pub mod erle;
pub mod hungarian;
pub mod items;
pub mod normalize;
pub mod rouge;
pub mod sisdr;
pub mod wer;

#[cfg(test)]
mod rouge_reference;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod whisper_reference;
