//! Every call into llama.cpp. All `unsafe` of the crate is here, in minimal blocks.
//!
//! Only `llama_*` functions are called (exported by `llama.dll`). Never call a `ggml_*` or
//! `gguf_*` function through `llama_cpp_sys_2` from Rust: whisper.cpp's ggml is linked
//! statically into the same executable, and an un-imported `ggml_*` reference from Rust
//! binds to that copy, not to `ggml.dll` (measured, see `docs/local-llm.md`).
//! `ffi_calls_only_llama_functions` in `lib.rs` enforces it.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::NonNull;
use std::sync::Once;

use llama_cpp_sys_2 as sys;

/// Initialises llama.cpp once per process and routes its log (and its ggml's) to
/// [`crate::log`].
pub(crate) fn init() {
    crate::gate::wait_for_llama();
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: `log_callback` matches the sys crate's log callback type, never unwinds and ignores the
        // null user data; llama_backend_init has no preconditions and runs once (Once).
        unsafe {
            sys::llama_log_set(Some(log_callback), std::ptr::null_mut());
            sys::llama_backend_init();
        }
    });
}

thread_local! {
    /// This thread's partial log line: ggml sends some lines in pieces
    /// (`GGML_LOG_LEVEL_CONT`).
    static PARTIAL: RefCell<String> = const { RefCell::new(String::new()) };
}

unsafe extern "C" fn log_callback(
    _level: sys::ggml_log_level,
    text: *const c_char,
    _: *mut c_void,
) {
    if text.is_null() {
        return;
    }
    // SAFETY: llama.cpp passes a NUL-terminated string that lives for this call.
    let piece = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    assemble(&piece, &mut crate::log::push);
}

/// Appends `piece` to this thread's partial line and passes every completed line (trailing
/// whitespace trimmed) to `emit`. Per thread because ggml continues a line on the thread
/// that started it, while several llama.cpp threads may log at once. A piece arriving while
/// the thread's storage is being torn down is passed on as a line of its own.
fn assemble(piece: &str, emit: &mut dyn FnMut(String)) {
    let done = PARTIAL.try_with(|partial| {
        let mut partial = partial.borrow_mut();
        partial.push_str(piece);
        while let Some(nl) = partial.find('\n') {
            let line: String = partial.drain(..=nl).collect();
            emit(line.trim_end().to_string());
        }
    });
    if done.is_err() {
        emit(piece.trim_end().to_string());
    }
}

/// `llama_print_system_info()` of llama.cpp's own ggml.
pub(crate) fn system_info() -> String {
    init();
    // SAFETY: returns a pointer to a static NUL-terminated buffer owned by llama.dll.
    let p = unsafe { sys::llama_print_system_info() };
    if p.is_null() {
        return String::new();
    }
    // SAFETY: non-null, NUL-terminated (above); copied before any other llama call.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Whether this llama.cpp build has a GPU backend.
pub(crate) fn supports_gpu_offload() -> bool {
    init();
    // SAFETY: no preconditions after init.
    unsafe { sys::llama_supports_gpu_offload() }
}

/// A loaded model and one context on it.
pub(crate) struct Engine {
    model: NonNull<sys::llama_model>,
    ctx: NonNull<sys::llama_context>,
    vocab: NonNull<sys::llama_vocab>,
    /// The tokens the context's memory holds for sequence 0, at positions 0, 1, ...: every
    /// token [`Engine::decode`] ran since the last [`Engine::clear`], less what
    /// [`Engine::keep_prefix`] removed (T12.25 prefix reuse).
    cached: Vec<sys::llama_token>,
    /// The micro-batch size the context was created with (`n_ubatch`).
    ubatch: usize,
}

/// Raw engine settings.
pub(crate) struct EngineParams {
    pub gpu_layers: i32,
    pub n_ctx: u32,
    pub n_batch: u32,
    pub threads: i32,
}

impl Engine {
    /// Loads `path` and creates a context, or `None` when llama.cpp refuses (its log says
    /// why).
    pub(crate) fn load(path: &CStr, p: &EngineParams) -> Option<Self> {
        init();
        // SAFETY: plain value-returning calls with no preconditions.
        let mut mp = unsafe { sys::llama_model_default_params() };
        mp.n_gpu_layers = p.gpu_layers;
        if p.gpu_layers > 0 {
            // The whole model on llama.cpp's first device, never split across two. Its list
            // holds discrete GPUs first and integrated ones only when there is no discrete
            // one (`llama_prepare_model_devices`, llama.cpp as pinned), so a caller that wants
            // no integrated GPU asks for the GPU only when a discrete one exists.
            mp.split_mode = sys::LLAMA_SPLIT_MODE_NONE;
            mp.main_gpu = 0;
        }
        // SAFETY: `path` is NUL-terminated and outlives the call; params are by value.
        let model = NonNull::new(unsafe { sys::llama_model_load_from_file(path.as_ptr(), mp) })?;
        // SAFETY: as above.
        let mut cp = unsafe { sys::llama_context_default_params() };
        cp.n_ctx = p.n_ctx;
        cp.n_batch = p.n_batch;
        cp.n_ubatch = p.n_batch.min(512);
        cp.n_threads = p.threads;
        cp.n_threads_batch = p.threads;
        // SAFETY: `model` is a live model from llama_model_load_from_file.
        let ctx = unsafe { sys::llama_init_from_model(model.as_ptr(), cp) };
        let Some(ctx) = NonNull::new(ctx) else {
            // SAFETY: `model` is live and owned here; nothing else refers to it.
            unsafe { sys::llama_model_free(model.as_ptr()) };
            return None;
        };
        // SAFETY: `model` is live; the vocab is owned by the model and lives as long.
        let vocab = unsafe { sys::llama_model_get_vocab(model.as_ptr()) };
        let Some(vocab) = NonNull::new(vocab.cast_mut()) else {
            // SAFETY: both live and owned here; the context is freed before its model.
            unsafe {
                sys::llama_free(ctx.as_ptr());
                sys::llama_model_free(model.as_ptr());
            }
            return None;
        };
        Some(Engine {
            model,
            ctx,
            vocab,
            cached: Vec::new(),
            ubatch: cp.n_ubatch as usize,
        })
    }

    /// The tokens the context's memory holds (see the field).
    pub(crate) fn cached(&self) -> &[sys::llama_token] {
        &self.cached
    }

    /// The context's micro-batch size (`n_ubatch`).
    pub(crate) fn ubatch(&self) -> usize {
        self.ubatch
    }

    /// A copy of sequence 0's whole state as it stands (`llama_state_seq_get_data`: the
    /// attention cache and, for a hybrid or recurrent model, the recurrent state), or `None`
    /// when llama.cpp writes nothing. A partial cache cannot be cut back on a recurrent model
    /// (`llama_memory_seq_rm` refuses), so prefix reuse saves the state at a boundary instead.
    pub(crate) fn save_state(&self) -> Option<Vec<u8>> {
        // SAFETY: live context; sequence 0 is the only one used.
        let size = unsafe { sys::llama_state_seq_get_size(self.ctx.as_ptr(), 0) };
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: live context; `buf` holds `size` writable bytes and outlives the call.
        let n =
            unsafe { sys::llama_state_seq_get_data(self.ctx.as_ptr(), buf.as_mut_ptr(), size, 0) };
        if n == 0 || n > size {
            return None;
        }
        buf.truncate(n);
        Some(buf)
    }

    /// Clears the memory and loads `state` (from [`Engine::save_state`] after decoding exactly
    /// `tokens`) into sequence 0, so the next decode continues at position `tokens.len()`.
    /// `false`, with the memory cleared, when llama.cpp refuses it.
    pub(crate) fn load_state(&mut self, state: &[u8], tokens: &[sys::llama_token]) -> bool {
        self.clear();
        // SAFETY: live context; `state` is readable for its length and outlives the call.
        let n = unsafe {
            sys::llama_state_seq_set_data(self.ctx.as_ptr(), state.as_ptr(), state.len(), 0)
        };
        if n == 0 {
            self.clear();
            return false;
        }
        self.cached = tokens.to_vec();
        true
    }

    /// The context size actually allocated.
    pub(crate) fn n_ctx(&self) -> u32 {
        // SAFETY: live context.
        unsafe { sys::llama_n_ctx(self.ctx.as_ptr()) }
    }

    /// The model's description (architecture, size, quantisation).
    pub(crate) fn description(&self) -> String {
        let mut buf = vec![0u8; 256];
        // SAFETY: live model; `buf` is writable for `buf.len()` bytes and llama.cpp writes a
        // NUL-terminated string of at most that size (snprintf semantics).
        let n = unsafe {
            sys::llama_model_desc(self.model.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        let n = usize::try_from(n)
            .unwrap_or(0)
            .min(buf.len().saturating_sub(1));
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    /// The model's built-in chat template.
    pub(crate) fn chat_template(&self) -> Option<CString> {
        // SAFETY: live model; a null name selects the default template. The returned
        // pointer is owned by the model and copied immediately.
        let p = unsafe { sys::llama_model_chat_template(self.model.as_ptr(), std::ptr::null()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: non-null NUL-terminated string owned by the live model.
        Some(unsafe { CStr::from_ptr(p) }.to_owned())
    }

    /// Formats `(role, content)` messages with `template`, ending with the assistant
    /// prefix. `None` when llama.cpp does not know the template.
    pub(crate) fn apply_template(
        template: &CStr,
        messages: &[(CString, CString)],
    ) -> Option<String> {
        init();
        let chat: Vec<sys::llama_chat_message> = messages
            .iter()
            .map(|(role, content)| sys::llama_chat_message {
                role: role.as_ptr(),
                content: content.as_ptr(),
            })
            .collect();
        let total: usize = messages
            .iter()
            .map(|(r, c)| r.as_bytes().len() + c.as_bytes().len())
            .sum();
        let mut buf = vec![0u8; 2 * total + 256];
        for _ in 0..2 {
            let len = i32::try_from(buf.len()).ok()?;
            // SAFETY: `chat` points at `chat.len()` messages whose strings (owned by
            // `messages`) outlive the call; `buf` is writable for `len` bytes.
            let n = unsafe {
                sys::llama_chat_apply_template(
                    template.as_ptr(),
                    chat.as_ptr(),
                    chat.len(),
                    true,
                    buf.as_mut_ptr().cast(),
                    len,
                )
            };
            let n = usize::try_from(n).ok()?;
            if n <= buf.len() {
                buf.truncate(n);
                return String::from_utf8(buf).ok();
            }
            buf.resize(n, 0);
        }
        None
    }

    /// Tokenizes `text`, parsing special tokens (the chat template's markers).
    pub(crate) fn tokenize(&self, text: &str) -> Option<Vec<sys::llama_token>> {
        let len = i32::try_from(text.len()).ok()?;
        let mut tokens: Vec<sys::llama_token> = vec![0; text.len() + 8];
        for _ in 0..2 {
            let cap = i32::try_from(tokens.len()).ok()?;
            // SAFETY: live vocab; `text` is valid for `len` bytes (no NUL needed);
            // `tokens` is writable for `cap` tokens.
            let n = unsafe {
                sys::llama_tokenize(
                    self.vocab.as_ptr(),
                    text.as_ptr().cast(),
                    len,
                    tokens.as_mut_ptr(),
                    cap,
                    true,
                    true,
                )
            };
            if n >= 0 {
                tokens.truncate(usize::try_from(n).ok()?);
                return Some(tokens);
            }
            if n == i32::MIN {
                return None;
            }
            tokens.resize(usize::try_from(n.unsigned_abs()).ok()?, 0);
        }
        None
    }

    /// Clears the context's memory (KV cache) so a new prompt starts at position 0.
    pub(crate) fn clear(&mut self) {
        self.cached.clear();
        // SAFETY: live context; the memory handle belongs to it.
        unsafe { sys::llama_memory_clear(sys::llama_get_memory(self.ctx.as_ptr()), true) };
    }

    /// Runs `tokens` through the model (appending to the context). llama.cpp return code:
    /// 0 is success. A failed decode clears the memory, whose contents are then unknown.
    pub(crate) fn decode(&mut self, tokens: &mut [sys::llama_token]) -> i32 {
        let Ok(n) = i32::try_from(tokens.len()) else {
            return -1;
        };
        // SAFETY: live context; the batch borrows `tokens`, which outlives the call
        // (llama_batch_get_one does not copy and llama_decode reads it synchronously).
        let rc = unsafe {
            let batch = sys::llama_batch_get_one(tokens.as_mut_ptr(), n);
            sys::llama_decode(self.ctx.as_ptr(), batch)
        };
        if rc == 0 {
            self.cached.extend_from_slice(tokens);
        } else {
            self.clear();
        }
        rc
    }

    /// Whether `token` ends generation.
    pub(crate) fn is_eog(&self, token: sys::llama_token) -> bool {
        // SAFETY: live vocab.
        unsafe { sys::llama_vocab_is_eog(self.vocab.as_ptr(), token) }
    }

    /// The bytes of `token` (special tokens not rendered).
    pub(crate) fn piece(&self, token: sys::llama_token, out: &mut Vec<u8>) {
        self.piece_with(token, out, false);
    }

    /// The bytes of `token`; `special` renders special tokens as their text (a thinking
    /// section's `</think>`, evaluation only).
    pub(crate) fn piece_with(&self, token: sys::llama_token, out: &mut Vec<u8>, special: bool) {
        let mut buf = [0u8; 128];
        let cap = i32::try_from(buf.len()).unwrap_or(i32::MAX);
        // SAFETY: live vocab; `buf` is writable for `cap` bytes.
        let n = unsafe {
            sys::llama_token_to_piece(
                self.vocab.as_ptr(),
                token,
                buf.as_mut_ptr().cast(),
                cap,
                0,
                special,
            )
        };
        if let Ok(n) = usize::try_from(n) {
            out.extend_from_slice(&buf[..n.min(buf.len())]);
            return;
        }
        let mut big = vec![0u8; usize::try_from(n.unsigned_abs()).unwrap_or(0)];
        let cap = i32::try_from(big.len()).unwrap_or(i32::MAX);
        // SAFETY: as above with `big`.
        let n = unsafe {
            sys::llama_token_to_piece(
                self.vocab.as_ptr(),
                token,
                big.as_mut_ptr().cast(),
                cap,
                0,
                special,
            )
        };
        if let Ok(n) = usize::try_from(n) {
            out.extend_from_slice(&big[..n.min(big.len())]);
        }
    }

    /// A grammar sampler for `grammar` (GBNF, start rule `root`). `None` when llama.cpp
    /// cannot parse the grammar (its log says why).
    pub(crate) fn grammar_sampler(&self, grammar: &CStr) -> Option<Sampler> {
        // SAFETY: live vocab; both strings are NUL-terminated and outlive the call (the
        // grammar is parsed and copied by llama.cpp).
        let g = unsafe {
            sys::llama_sampler_init_grammar(self.vocab.as_ptr(), grammar.as_ptr(), c"root".as_ptr())
        };
        NonNull::new(g).map(Sampler)
    }

    /// The greedy token the grammar allows at the last decoded position, accepted into the
    /// grammar.
    ///
    /// Same result as applying the grammar to every candidate and taking the best, but the
    /// unconstrained best token is checked alone first and the full vocabulary is filtered
    /// only when the grammar rejects it (llama.cpp `common/sampling.cpp`
    /// `common_sampler_sample`, `grammar_first = false`): the best allowed token is the
    /// overall best whenever the overall best is allowed. Filtering all ~150k candidates for
    /// every token made grammar sampling the bottleneck (measured: docs/local-llm.md).
    pub(crate) fn sample_greedy(&mut self, grammar: &mut Sampler) -> Option<sys::llama_token> {
        // SAFETY: live vocab.
        let n_vocab =
            usize::try_from(unsafe { sys::llama_vocab_n_tokens(self.vocab.as_ptr()) }).unwrap_or(0);
        // SAFETY: live context after a successful decode; idx -1 is its last output, an
        // array of `n_vocab` logits owned by the context and valid until the next decode.
        let logits = unsafe { sys::llama_get_logits_ith(self.ctx.as_ptr(), -1) };
        if logits.is_null() || n_vocab == 0 {
            return None;
        }
        // SAFETY: as above. Read in place, not copied (~150k floats per token): the slice
        // lives only for this call, which holds `&mut self`, so no `llama_decode` (the only
        // call that rewrites or frees the logits) can run while it is borrowed. The grammar
        // calls below touch only the sampler and the candidate arrays this crate owns.
        let logits = unsafe { std::slice::from_raw_parts(logits, n_vocab) };
        let g = grammar.0;
        greedy_with_grammar(logits, |cands| apply(g, cands), |token| accept(g, token))
    }
}

/// The sampling decision of [`Engine::sample_greedy`], with the grammar passed in as
/// `apply` (sets rejected candidates' logits to -inf) and `accept` (advances the grammar).
///
/// Every id handed to `apply` or `accept` is a valid index into `logits`: llama.cpp looks
/// token ids up with `.at()`, and the C++ exception an invalid id throws must never cross
/// the C ABI. `None`, with `accept` not called and no invalid id passed anywhere, when no
/// logit is finite or the grammar rejects every candidate.
pub(crate) fn greedy_with_grammar(
    logits: &[f32],
    mut apply: impl FnMut(&mut [sys::llama_token_data]),
    mut accept: impl FnMut(sys::llama_token),
) -> Option<sys::llama_token> {
    let token_of = |i: usize| sys::llama_token::try_from(i).ok();
    let (best, best_logit) = argmax(logits.iter().copied().enumerate())?;
    let best_id = token_of(best)?;
    let mut one = [sys::llama_token_data {
        id: best_id,
        logit: best_logit,
        p: 0.0,
    }];
    apply(&mut one);
    if one[0].logit.is_finite() {
        accept(best_id);
        return Some(best_id);
    }
    let mut all = Vec::with_capacity(logits.len());
    for (i, &logit) in logits.iter().enumerate() {
        all.push(sys::llama_token_data {
            id: token_of(i)?,
            logit,
            p: 0.0,
        });
    }
    apply(&mut all);
    let (pos, _) = argmax(all.iter().map(|d| d.logit).enumerate())?;
    let token = all[pos].id;
    accept(token);
    Some(token)
}

/// Position and value of the largest finite value (first on ties); `None` if none.
fn argmax(values: impl Iterator<Item = (usize, f32)>) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (i, v) in values {
        if v.is_finite() && best.is_none_or(|(_, b)| v > b) {
            best = Some((i, v));
        }
    }
    best
}

/// Applies the grammar sampler `g` to `cands` (rejected tokens get logit -inf).
fn apply(g: NonNull<sys::llama_sampler>, cands: &mut [sys::llama_token_data]) {
    let mut arr = sys::llama_token_data_array {
        data: cands.as_mut_ptr(),
        size: cands.len(),
        selected: -1,
        sorted: false,
    };
    // SAFETY: `g` is the live sampler borrowed by `sample_greedy`; `arr` points at
    // `cands.len()` initialised entries that outlive the call, every id a valid vocabulary
    // index (`greedy_with_grammar`); the grammar sampler only rewrites logits in place.
    unsafe { sys::llama_sampler_apply(g.as_ptr(), &raw mut arr) };
}

fn accept(g: NonNull<sys::llama_sampler>, token: sys::llama_token) {
    // SAFETY: `g` is the live sampler borrowed by `sample_greedy`; `token` is a valid
    // vocabulary index the grammar allowed (`greedy_with_grammar` passes nothing else).
    // llama.cpp's grammar accept looks the token's text up first, so an out-of-range id
    // would throw a C++ exception inside the call.
    unsafe { sys::llama_sampler_accept(g.as_ptr(), token) };
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: both pointers are live and owned by this Engine; the context is freed
        // before the model it was created from; the vocab belongs to the model.
        unsafe {
            sys::llama_free(self.ctx.as_ptr());
            sys::llama_model_free(self.model.as_ptr());
        }
    }
}

/// An owned sampler.
pub(crate) struct Sampler(NonNull<sys::llama_sampler>);

impl Drop for Sampler {
    fn drop(&mut self) {
        // SAFETY: owned sampler, freed once.
        unsafe { sys::llama_sampler_free(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// What a fake grammar saw: every id applied and every id accepted.
    #[derive(Default)]
    struct Seen {
        applied: Vec<sys::llama_token>,
        accepted: Vec<sys::llama_token>,
    }

    /// Runs [`greedy_with_grammar`] with a fake grammar that rejects ids not in `allowed`.
    fn run(logits: &[f32], allowed: &[sys::llama_token]) -> (Option<sys::llama_token>, Seen) {
        let seen = RefCell::new(Seen::default());
        let got = greedy_with_grammar(
            logits,
            |cands| {
                for c in cands.iter_mut() {
                    seen.borrow_mut().applied.push(c.id);
                    if !allowed.contains(&c.id) {
                        c.logit = f32::NEG_INFINITY;
                    }
                }
            },
            |t| seen.borrow_mut().accepted.push(t),
        );
        (got, seen.into_inner())
    }

    fn all_valid(seen: &Seen, n: usize) -> bool {
        seen.applied
            .iter()
            .chain(&seen.accepted)
            .all(|&t| usize::try_from(t).is_ok_and(|t| t < n))
    }

    #[test]
    fn log_partial_lines_are_assembled_per_thread() {
        use std::sync::{Arc, Barrier};
        // Two threads alternate pieces of their own lines: A's piece, then B's, per step.
        let barrier = Arc::new(Barrier::new(2));
        let spawn = |me: &'static str, first: bool| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut lines = Vec::new();
                for piece in [format!("{me} one "), format!("{me} two\n")] {
                    if !first {
                        barrier.wait();
                    }
                    assemble(&piece, &mut |l| lines.push(l));
                    if first {
                        barrier.wait();
                    }
                    barrier.wait();
                }
                lines
            })
        };
        let a = spawn("a", true);
        let b = spawn("b", false);
        assert_eq!(a.join().unwrap(), ["a one a two"]);
        assert_eq!(b.join().unwrap(), ["b one b two"]);
    }

    #[test]
    fn sampling_checks_the_best_token_alone_when_allowed() {
        let (got, seen) = run(&[0.1, 3.0, 2.0], &[0, 1, 2]);
        assert_eq!(got, Some(1));
        assert_eq!(seen.applied, [1]);
        assert_eq!(seen.accepted, [1]);
    }

    #[test]
    fn sampling_falls_back_to_the_best_allowed_token() {
        let (got, seen) = run(&[0.1, 3.0, 2.0], &[0, 2]);
        assert_eq!(got, Some(2));
        assert_eq!(seen.accepted, [2]);
        assert!(all_valid(&seen, 3), "applied {:?}", seen.applied);
    }

    #[test]
    fn sampling_without_a_finite_logit_touches_no_grammar() {
        for logits in [&[f32::NAN, f32::NEG_INFINITY, f32::INFINITY][..], &[]] {
            let (got, seen) = run(logits, &[0, 1, 2]);
            assert_eq!(got, None);
            assert!(
                seen.applied.is_empty() && seen.accepted.is_empty(),
                "applied {:?}, accepted {:?}",
                seen.applied,
                seen.accepted
            );
        }
    }

    #[test]
    fn sampling_rejected_everywhere_accepts_nothing() {
        let (got, seen) = run(&[0.1, 3.0, 2.0], &[]);
        assert_eq!(got, None);
        assert!(seen.accepted.is_empty(), "accepted {:?}", seen.accepted);
        assert!(all_valid(&seen, 3), "applied {:?}", seen.applied);
    }
}
