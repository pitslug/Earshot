# One CUDA session for transcription and previews (T12.3b, findings)

Spike findings, 2026-11-09, from reading sources only (nothing built or measured). On CUDA each Parakeet session holds its own copy of the encoder on the card: about 3.7 GiB per extra session (docs/live-previews.md, whole-card 9.1 GiB off against 12.8 GiB with one preview session).

## What the library allows

- `ort` 2.0.0-rc.14 (our pin): `Session::run`, `run_with_options`, `run_binding*` and `run_async` all take `&mut self` (`ort-2.0.0-rc.14/src/session/mod.rs` lines 241, 283, 381, 386, 452, 588), although `Session` is `Send + Sync` (mod.rs 757-758). The crate documents why ("Why does `run` take `&mut self`?", mod.rs about line 110): concurrent `Run` crashed or corrupted memory with some execution providers, CUDA among them; its private `run_inner` takes `&self` with a comment warning against exposing it.
- `ort` 2.0.0-rc.15 (2026-10-19, the newest): the same `&mut self` and the same explanation. It fixes 30+ undefined-behaviour bugs and has breaking changes (`InMemorySession` and `ValueRefMut` lose `DerefMut`, `run_async` takes owned inputs, `with_external_initializer_file_in_memory` takes `Arc<[u8]>`, `ThreadManager` must be `Send + Sync`). An upgrade is worth its own task for those fixes, with the AGENTS.md ort checks re-run, but it does not give shared sessions.
- ONNX Runtime itself: a maintainer said concurrent `Run` on one session is safe (microsoft/onnxruntime#114, 2018); later reports show concurrency bugs with some providers (#19275). The CUDA provider gives each calling thread its own cuBLAS and cuDNN handles; `use_ep_level_unified_stream` and `user_compute_stream` control streams; with CUDA graphs, concurrent `Run` on one session is forbidden. Whether a plain `Run` gets its own stream per call was not confirmed.

## What sharing would take

Only a hand-written `unsafe` call of the C API's `Run` on the shared session's raw pointer (`AsPointer::ptr()`, `ort_sys` is public), copying ort's own `run_inner` call, in one reviewed block in earshot-onnx. Encoder only: the encoder holds the 2.27 GiB of weights; the decoder and joint are small step sessions and stay per instance. Parakeet keeps no state between calls (LSTM state is passed in and out), so sharing is correct in meaning. About 150 to 250 lines: the wrapper, `Arc` plumbing in `OnnxSession` and `ParakeetTdt`, a loader hook so a preview borrows the track's encoder, and stream options. Two ways to serialise:

- **A mutex around the encoder run:** correct by construction; a real decode can wait for one preview encoder pass (0.37 to 0.57 s at worst measured on CUDA). The rule "no preview delays a real decode" would need a stated bound instead.
- **No lock:** relies on concurrent `Run` being safe on our exact stack, which ort's maintainers say crashes with CUDA.

## Recommendation

Not worth building yet. T12.3a (the mic preview on the CPU beside CUDA) adds no card memory at all, so the shared session would only save the one far-end preview session (about 3.7 GiB) on CUDA. If that saving is wanted later, the decision measurement is a small probe in earshot-onnx (one encoder on CUDA called from two threads through the wrapper, card memory read with `nvidia-smi`) plus a "real decode waited" timing in `live_lag`, with the bound committed before measuring. Unsafe FFI needs the owner's go-ahead and a reviewer (AGENTS 3.5).
