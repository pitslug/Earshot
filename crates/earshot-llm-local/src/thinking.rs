//! Qwen thinking on (T12.10, evaluation only; production never calls this module): a reply
//! that may think before its summary JSON, with the thinking bounded by a token cap that is
//! counted apart from the reply budget.
//!
//! The assistant prefix opens the section ([`QWEN_THINKING_PREFIX`] instead of production's
//! empty block [`QWEN_THINKING_OFF_PREFIX`]); the grammar is
//! [`crate::grammar::thinking_grammar`] around the request's own summary grammar. The
//! generation loop ([`LocalLlm::generate_thinking`]) counts thinking tokens and reply tokens
//! apart: the reply keeps its whole budget, and when the thinking reaches the cap the loop
//! switches to [`crate::grammar::thinking_close_grammar`], which allows only `</think>` and
//! the reply (a forced close; the close itself may take up to [`THINK_CLOSE_SLACK`] more
//! tokens). The thinking text is dropped: the returned reply is what follows `</think>`, and
//! only token counts leave this module ([`ThinkingStats`]).
//!
//! [`ThinkingLlm`] wraps a model loaded with [`thinking_room`] more context than the run's
//! budget and reports the budget as its context, so every plan, reply budget and repair
//! headroom is exactly the thinking-off run's and the thinking room comes on top.

use std::ffi::CString;
use std::time::Instant;

use earshot_core::asr::CancelToken;
use earshot_core::prompt::ChatMessage;

use crate::grammar::{THINK_CLOSE, thinking_close_grammar, thinking_grammar};
use crate::summarize::Generate;
use crate::{Generation, LOOP_CHECK_TOKENS, LlmError, LocalLlm, Stop, is_looping};

/// What production's Qwen 3 and 3.5 prompts end with (thinking off): an empty thinking block.
pub const QWEN_THINKING_OFF_PREFIX: &str = "<think>\n\n</think>\n\n";

/// What a thinking prompt ends with: the opened section, as Qwen's template renders it with
/// thinking on.
pub const QWEN_THINKING_PREFIX: &str = "<think>\n";

/// Tokens a forced close may take past the cap: up to three while a character split over
/// tokens completes (the grammar cannot switch inside one), then `</think>` itself (eight
/// characters, at worst one token each), and a newline before it.
pub const THINK_CLOSE_SLACK: usize = 16;

/// The largest thinking cap a run may ask for.
pub const MAX_THINKING_TOKENS: usize = 32_768;

/// Context a request needs beside its prompt and reply budget for thinking capped at `cap`.
#[must_use]
pub const fn thinking_room(cap: usize) -> usize {
    cap + THINK_CLOSE_SLACK
}

/// The context to load a model with for a run whose budget is `context` tokens and whose
/// thinking is capped at `cap`: `context` plus [`thinking_room`].
///
/// # Errors
/// A cap of 0 or above [`MAX_THINKING_TOKENS`], or a total that does not fit a `u32`.
pub fn thinking_load_context(context: u32, cap: usize) -> Result<u32, String> {
    if cap == 0 || cap > MAX_THINKING_TOKENS {
        return Err(format!(
            "a thinking cap must be 1 to {MAX_THINKING_TOKENS} tokens, not {cap}"
        ));
    }
    u32::try_from(thinking_room(cap))
        .ok()
        .and_then(|room| context.checked_add(room))
        .ok_or_else(|| format!("a context of {context} plus thinking of {cap} is too large"))
}

/// Whether a request of `prompt` tokens with a reply budget of `reply` and thinking capped at
/// `cap` fits a context of `context` tokens, the thinking room counted apart from the reply.
#[must_use]
pub fn thinking_fits(prompt: usize, reply: usize, cap: usize, context: usize) -> bool {
    prompt
        .checked_add(reply)
        .and_then(|n| n.checked_add(thinking_room(cap)))
        .is_some_and(|n| n <= context)
}

/// Whether `bytes` ends on a character boundary (no UTF-8 sequence cut off at the end).
fn ends_on_char_boundary(bytes: &[u8]) -> bool {
    // The last lead byte within the last four bytes, and how long its sequence is.
    for back in 1..=bytes.len().min(4) {
        let b = bytes[bytes.len() - back];
        let len = match b {
            0x00..=0x7F => 1,
            0x80..=0xBF => continue,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        };
        return back >= len;
    }
    // Empty, or only continuation bytes (not UTF-8 at all: nothing to wait for).
    true
}

/// What the loop does after a thinking token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkStep {
    /// Still thinking.
    Thinking,
    /// The cap is reached: switch to the close grammar.
    ForceClose,
    /// `</think>` was written; the reply starts at this byte.
    Closed(usize),
    /// The close did not come within [`THINK_CLOSE_SLACK`] tokens of the cap.
    Overrun,
}

/// Counts the thinking tokens of one reply and decides when to force the close.
#[derive(Debug)]
pub(crate) struct ThinkTracker {
    cap: usize,
    tokens: usize,
    forced: bool,
    /// Bytes already searched for `</think>` (less its length, so a close split over two
    /// pieces is found).
    scanned: usize,
}

impl ThinkTracker {
    pub(crate) fn new(cap: usize) -> Self {
        ThinkTracker {
            cap,
            tokens: 0,
            forced: false,
            scanned: 0,
        }
    }

    /// Thinking tokens so far.
    pub(crate) fn tokens(&self) -> usize {
        self.tokens
    }

    /// Whether the close was forced.
    pub(crate) fn forced(&self) -> bool {
        self.forced
    }

    /// One more thinking token, whose piece `bytes` (everything generated) now ends with.
    pub(crate) fn after_token(&mut self, bytes: &[u8]) -> ThinkStep {
        self.tokens += 1;
        let close = THINK_CLOSE.as_bytes();
        let from = self.scanned.min(bytes.len());
        if let Some(i) = bytes[from..].windows(close.len()).position(|w| w == close) {
            return ThinkStep::Closed(from + i + close.len());
        }
        self.scanned = bytes.len().saturating_sub(close.len() - 1);
        if self.tokens >= self.cap + THINK_CLOSE_SLACK {
            return ThinkStep::Overrun;
        }
        if !self.forced && self.tokens >= self.cap && ends_on_char_boundary(bytes) {
            self.forced = true;
            return ThinkStep::ForceClose;
        }
        ThinkStep::Thinking
    }
}

/// One thinking reply ([`LocalLlm::generate_thinking`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkingGeneration {
    /// The reply: `text` is what followed `</think>` (empty when the section never closed),
    /// `generated_tokens` counts thinking and reply tokens together.
    pub generation: Generation,
    /// Tokens up to and including the one that wrote `</think>`.
    pub thinking_tokens: usize,
    /// Whether the cap forced the close.
    pub forced_close: bool,
}

impl LocalLlm {
    /// [`LocalLlm::generate_cancellable`] for a reply that thinks first (module docs;
    /// evaluation only): `reply_grammar` is the request's own grammar (start rule `root`),
    /// wrapped here in [`thinking_grammar`]; `assistant_prefix` must have opened the section
    /// ([`QWEN_THINKING_PREFIX`]). The reply gets all of `max_tokens`; the thinking up to
    /// `think_cap` tokens (plus [`THINK_CLOSE_SLACK`] for a forced close) beside it. The loop
    /// guard watches the reply only.
    ///
    /// # Errors
    /// As [`LocalLlm::generate_cancellable`]; [`LlmError::ContextTooSmall`] (with the
    /// thinking room in `max_tokens`) when the prompt, the reply budget and the thinking room
    /// do not all fit ([`thinking_fits`]).
    pub fn generate_thinking(
        &mut self,
        messages: &[ChatMessage],
        assistant_prefix: &str,
        reply_grammar: &str,
        think_cap: usize,
        max_tokens: usize,
        cancel: &CancelToken,
    ) -> Result<ThinkingGeneration, LlmError> {
        let mut prompt = self.format_chat(messages)?;
        prompt.push_str(assistant_prefix);
        let grammar = |g: Result<String, crate::grammar::GrammarError>| {
            g.map_err(|e| LlmError::Input(e.to_string())).and_then(|g| {
                CString::new(g).map_err(|_| LlmError::Input("grammar contains NUL".into()))
            })
        };
        let think_grammar = grammar(thinking_grammar(reply_grammar))?;
        let close_grammar = grammar(thinking_close_grammar(reply_grammar))?;
        let rejected = || LlmError::Input("llama.cpp rejected the grammar (see its log)".into());
        let mut sampler = self
            .engine
            .grammar_sampler(&think_grammar)
            .ok_or_else(rejected)?;
        let mut close_sampler = Some(
            self.engine
                .grammar_sampler(&close_grammar)
                .ok_or_else(rejected)?,
        );
        let mut tokens = self
            .engine
            .tokenize(&prompt)
            .ok_or_else(|| LlmError::Input("tokenization failed".into()))?;
        let context = self.engine.n_ctx() as usize;
        if !thinking_fits(tokens.len(), max_tokens, think_cap, context) {
            return Err(LlmError::ContextTooSmall {
                prompt: tokens.len(),
                max_tokens: max_tokens + thinking_room(think_cap),
                context,
            });
        }
        self.engine.clear();
        let started = Instant::now();
        for chunk in tokens.chunks_mut(2048) {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            let rc = self.engine.decode(chunk);
            if rc != 0 {
                return Err(LlmError::Decode(rc));
            }
            self.heartbeat.beat();
        }
        let prompt_time = started.elapsed();
        let started = Instant::now();
        let mut bytes = Vec::new();
        let mut tracker = ThinkTracker::new(think_cap);
        let mut reply_start: Option<usize> = None;
        let mut reply_tokens = 0;
        let mut stop = Stop::MaxTokens;
        loop {
            if reply_start.is_some() && reply_tokens >= max_tokens {
                break;
            }
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            let token = self
                .engine
                .sample_greedy(&mut sampler)
                .ok_or_else(|| LlmError::Input("the grammar allows no token here".into()))?;
            if self.engine.is_eog(token) {
                stop = Stop::EndOfGeneration;
                break;
            }
            match reply_start {
                None => {
                    // `</think>` may be a special token: render it, so the close is seen.
                    self.engine.piece_with(token, &mut bytes, true);
                    match tracker.after_token(&bytes) {
                        ThinkStep::Thinking => {}
                        ThinkStep::ForceClose => {
                            if let Some(s) = close_sampler.take() {
                                sampler = s;
                            }
                        }
                        ThinkStep::Closed(at) => reply_start = Some(at),
                        ThinkStep::Overrun => break,
                    }
                }
                Some(start) => {
                    self.engine.piece(token, &mut bytes);
                    reply_tokens += 1;
                    if reply_tokens.is_multiple_of(LOOP_CHECK_TOKENS) && is_looping(&bytes[start..])
                    {
                        stop = Stop::Looping;
                        break;
                    }
                }
            }
            let rc = self.engine.decode(&mut [token]);
            if rc != 0 {
                return Err(LlmError::Decode(rc));
            }
            self.heartbeat.beat();
        }
        let generate_time = started.elapsed();
        let reply = reply_start.map_or(&[][..], |at| &bytes[at..]);
        Ok(ThinkingGeneration {
            generation: Generation {
                text: String::from_utf8(reply.to_vec()).map_err(|_| LlmError::Utf8)?,
                stop,
                prompt_tokens: tokens.len(),
                generated_tokens: tracker.tokens() + reply_tokens,
                prompt_time,
                generate_time,
            },
            thinking_tokens: tracker.tokens(),
            forced_close: tracker.forced(),
        })
    }
}

/// Thinking counts of the replies a [`ThinkingLlm`] made since the last
/// [`ThinkingLlm::take_stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingStats {
    /// Replies.
    pub requests: usize,
    /// Thinking tokens over all replies.
    pub thinking_tokens: usize,
    /// The most thinking tokens of one reply.
    pub max_thinking_tokens: usize,
    /// Replies whose close the cap forced.
    pub forced_closes: usize,
}

/// A model whose every reply thinks first, capped at `cap` tokens (module docs): its
/// [`Generate::context_tokens`] is the run's budget, the thinking room comes on top.
pub struct ThinkingLlm {
    llm: LocalLlm,
    cap: usize,
    context: usize,
    stats: ThinkingStats,
}

impl ThinkingLlm {
    /// `llm` thinking up to `cap` tokens per reply, planned as a context of `context` tokens.
    ///
    /// # Errors
    /// [`LlmError::Input`] for a cap outside 1 to [`MAX_THINKING_TOKENS`] or a model loaded
    /// with less than `context` plus [`thinking_room`] (load it with
    /// [`thinking_load_context`]): the thinking is never taken out of the reply budget.
    pub fn new(llm: LocalLlm, cap: usize, context: usize) -> Result<Self, LlmError> {
        if cap == 0 || cap > MAX_THINKING_TOKENS {
            return Err(LlmError::Input(format!(
                "a thinking cap must be 1 to {MAX_THINKING_TOKENS} tokens, not {cap}"
            )));
        }
        let have = llm.context_tokens();
        if context.saturating_add(thinking_room(cap)) > have {
            return Err(LlmError::Input(format!(
                "the model's context of {have} tokens leaves no room for {cap} thinking tokens \
                 beside a budget of {context}"
            )));
        }
        Ok(ThinkingLlm {
            llm,
            cap,
            context,
            stats: ThinkingStats::default(),
        })
    }

    /// The counts since the last call, reset.
    pub fn take_stats(&mut self) -> ThinkingStats {
        std::mem::take(&mut self.stats)
    }
}

impl Generate for ThinkingLlm {
    fn context_tokens(&self) -> usize {
        self.context
    }

    fn prompt_tokens(&self, messages: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
        self.llm.prompt_tokens(messages, prefix)
    }

    fn generate(
        &mut self,
        messages: &[ChatMessage],
        prefix: &str,
        grammar: &str,
        max_tokens: usize,
        cancel: &CancelToken,
    ) -> Result<Generation, LlmError> {
        let g = self
            .llm
            .generate_thinking(messages, prefix, grammar, self.cap, max_tokens, cancel)?;
        self.stats.requests += 1;
        self.stats.thinking_tokens += g.thinking_tokens;
        self.stats.max_thinking_tokens = self.stats.max_thinking_tokens.max(g.thinking_tokens);
        self.stats.forced_closes += usize::from(g.forced_close);
        Ok(g.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `pieces` as thinking tokens; the steps.
    fn run(cap: usize, pieces: &[&[u8]]) -> Vec<ThinkStep> {
        let mut t = ThinkTracker::new(cap);
        let mut bytes = Vec::new();
        pieces
            .iter()
            .map(|p| {
                bytes.extend_from_slice(p);
                t.after_token(&bytes)
            })
            .collect()
    }

    #[test]
    fn thinking_a_close_within_the_cap_ends_the_section_where_it_ends() {
        let steps = run(8, &[b"The team ", b"chose yellow.", b"\n", b"</think>"]);
        assert_eq!(
            steps,
            [
                ThinkStep::Thinking,
                ThinkStep::Thinking,
                ThinkStep::Thinking,
                ThinkStep::Closed(b"The team chose yellow.\n</think>".len()),
            ]
        );
        // A close split over pieces is found too.
        let steps = run(8, &[b"ok </th", b"ink>\n"]);
        assert_eq!(steps[1], ThinkStep::Closed(b"ok </think>".len()));
    }

    #[test]
    fn thinking_over_the_cap_is_forced_closed_once_then_bounded() {
        let mut t = ThinkTracker::new(3);
        let mut bytes = Vec::new();
        let mut steps = Vec::new();
        for _ in 0..3 + THINK_CLOSE_SLACK {
            bytes.extend_from_slice(b"word ");
            steps.push(t.after_token(&bytes));
        }
        assert_eq!(&steps[..2], [ThinkStep::Thinking, ThinkStep::Thinking]);
        assert_eq!(steps[2], ThinkStep::ForceClose, "at the cap");
        assert!(t.forced());
        // Forced once only; a close that never comes ends the reply at the slack.
        assert!(
            steps[3..steps.len() - 1]
                .iter()
                .all(|s| *s == ThinkStep::Thinking)
        );
        assert_eq!(*steps.last().unwrap(), ThinkStep::Overrun);
        assert_eq!(t.tokens(), 3 + THINK_CLOSE_SLACK);
        // After a forced close the close is still recognised.
        let mut t = ThinkTracker::new(1);
        assert_eq!(t.after_token(b"a"), ThinkStep::ForceClose);
        assert_eq!(t.after_token(b"a</think>"), ThinkStep::Closed(9));
    }

    #[test]
    fn thinking_waits_for_a_whole_character_before_forcing_the_close() {
        // "e" with an acute accent (two bytes) split over two tokens at the cap.
        let steps = run(1, &[&[0xC3], &[0xA9]]);
        assert_eq!(steps, [ThinkStep::Thinking, ThinkStep::ForceClose]);
        assert!(ends_on_char_boundary("ab\u{e9}\u{20ac}".as_bytes()));
        assert!(!ends_on_char_boundary(&"\u{20ac}".as_bytes()[..2]));
        assert!(ends_on_char_boundary(b""));
    }

    #[test]
    fn thinking_room_is_counted_apart_from_the_reply_budget() {
        // The reply keeps its budget: a request fits only with the thinking room on top.
        let (prompt, reply, cap) = (6_000, 2_048, 1_024);
        let exact = prompt + reply + thinking_room(cap);
        assert!(thinking_fits(prompt, reply, cap, exact));
        assert!(!thinking_fits(prompt, reply, cap, exact - 1));
        assert!(!thinking_fits(usize::MAX, reply, cap, usize::MAX));
        // The load context is the budget plus the room; a bad cap is refused, not shrunk.
        assert_eq!(
            thinking_load_context(16_384, cap).unwrap(),
            16_384 + 1_024 + THINK_CLOSE_SLACK as u32
        );
        assert!(thinking_load_context(16_384, 0).is_err());
        assert!(thinking_load_context(16_384, MAX_THINKING_TOKENS + 1).is_err());
        assert!(thinking_load_context(u32::MAX, 1).is_err());
    }
}
