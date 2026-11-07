//! The summary backend trait (PLAN.md 3.4).

use core::future::Future;

use super::SummaryV1;
use crate::asr::CancelToken;
use crate::prompt::SummaryInput;

/// Turns a meeting into a validated, grounded summary (PLAN.md 3.7).
///
/// Implementations (the OpenAI-compatible client in `earshot-llm`, test doubles) return
/// only summaries that pass [`super::validate_value`] against
/// [`SummaryInput::segment_ids`], so every citation exists in the transcript.
///
/// The call is asynchronous because the real backend is a network request; core never
/// runs it (it has no executor). The future is `Send` so a runtime can drive it on any
/// worker thread. Cancellation: dropping the future abandons the work; `cancel` stops it
/// cooperatively and makes it return the implementation's cancelled error.
pub trait Summarizer {
    /// What can go wrong.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Summarises `input`, polling `cancel`.
    fn summarize(
        &self,
        input: &SummaryInput,
        cancel: &CancelToken,
    ) -> impl Future<Output = Result<SummaryV1, Self::Error>> + Send;
}
