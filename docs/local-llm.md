# Local LLM in process: llama.cpp next to whisper.cpp (T6.19 spike)

Measured 2026-10-23 on the desktop (x86_64, 32 hardware threads, RTX 5080, CUDA 13.4, Vulkan SDK 1.4.363.0, LLVM clang-cl and Ninja for ARM64). Crate: `crates/earshot-llm-local`. Accept: `cargo test -p earshot-llm-local -- --include-ignored` (needs `cargo xtask fetch-models --set summary` and `--set ci`; since 2026-11-02, PLAN D22, the tests load the shipped Qwen 3.5 4B instead of the `llm` set's Qwen2.5 1.5B, and the `llm` set no longer exists).

## Result

llama.cpp and whisper.cpp run in one process on CPU, CUDA and Vulkan, and the crate's test binaries (both libraries linked) cross-compile for ARM64. The two ggml copies coexist because llama.cpp and its ggml are DLLs while whisper.cpp's ggml stays static in the executable. A schema-constrained summary from Qwen2.5 1.5B Instruct (Q4_K_M) parses and validates as `SummaryV1` (shape, non-blank text, grounding) on every backend.

## Binding crate

`llama-cpp-sys-2 =0.1.160` (utilityai/llama-cpp-rs, git `0dd5814c9b061930965d144c1653cd4d05054873`; MIT OR Apache-2.0; llama.cpp itself MIT). It vendors llama.cpp with ggml 0.26.0; whisper-rs-sys 0.15.0 vendors ggml 0.9.5. It is the most used and most actively released Rust binding (frequent releases tracking llama.cpp), it builds llama.cpp with CMake like whisper-rs-sys, forwards `CMAKE_*`/`GGML_*` environment variables, and has a `dynamic-link` feature. Only the sys crate is used, not the safe `llama-cpp-2` wrapper: the wrapper calls `ggml_*` functions from Rust (device listing, timing, logging), which bind to the wrong ggml in this process (below). Features: `dynamic-link` only (no `common`, no `openmp`); `cuda` and `vulkan` are forwarded from the crate's own features.

## Link approach and evidence

1. **Both static (default features): fails.** `cargo test -p earshot-llm-local --test coexist` with llama-cpp-sys-2 static and whisper-rs-sys as a dev-dependency: 533 `LNK2005` errors (`ggml_abs ... already defined in libwhisper_rs_sys-*.rlib(ggml.obj)`, every `quantize_*`, `ggml_fp16_to_fp32_row`, ...), then `LNK1169`. Even if the linker picked one copy, two ggml versions (0.9.5 and 0.26.0) are not ABI compatible.
2. **Shared ggml: not attempted.** whisper.cpp 1.8.3 would have to build against ggml 0.26.0 while whisper-rs-sys keeps generating bindings from its own ggml 0.9.5 headers; struct layouts could differ silently, and the whisper version pins (D13) would no longer describe what runs.
3. **llama.cpp as DLLs (`dynamic-link`): works.** llama-cpp-sys-2 builds `llama.dll`, `ggml.dll`, `ggml-base.dll`, `ggml-cpu.dll` (plus `ggml-cuda.dll` or `ggml-vulkan.dll`) with import libraries; whisper.cpp's ggml stays bundled in the whisper-rs-sys rlib. No duplicate symbols: the executable resolves its `ggml_*` from whisper's objects, and `llama.dll` was linked against `ggml.dll` when the DLL was built, so the Windows loader binds it there by DLL name. `llvm-readobj --coff-imports` on the coexistence test executable lists only `llama_*` imports from `llama.dll` (26 functions in the ARM64 build, x64 likewise) and nothing from any `ggml*.dll`.
4. **The one way to break it (measured).** A Rust call to a `ggml_*` binding of llama-cpp-sys-2 links without error but does not import from `ggml.dll`: `llama_cpp_sys_2::ggml_backend_dev_count()` added to the test produced no import entry, so it bound to whisper.cpp's static ggml and returned whisper's device count. Rust declares these functions without `dllimport`, and the linker resolves the plain symbol from whichever definition it meets first. Rule: Rust calls only `llama_*` functions; `ffi_calls_only_llama_functions` scans every `.rs` file of the crate (`src/` recursively, `tests/`, `examples/`, `build.rs`, comments included) for any identifier matching `\b(ggml|gguf)_[A-Za-z0-9_]+` and fails on all but the reviewed type `ggml_log_level`, so path calls, brace-list and renamed imports and bare calls after a glob import are all caught (self-tested form by form).
5. **Runtime.** `coexist_whisper_and_llama_run_in_one_process` loads whisper tiny.en (backend proven by `earshot_asr_whisper::evidence::load_verified`), transcribes a smoke clip, runs a llama.cpp summary on a second thread while whisper transcribes again on the first, then transcribes a third time: the three transcripts are identical and the summary validates. The two loggers stay separate (llama.cpp's goes through `llama_log_set` into `earshot_llm_local::log`, whisper's through whisper-rs's hooks into `log`): over the whole test, the llama ring holds llama.cpp lines (`llama_*`, `load_tensors`, `print_info`) and no `whisper_*` line, and whisper's capture holds `whisper_*` lines and no llama.cpp line. Each capture must have seen its own library, so an empty or broken capture fails rather than passing, and `coexist_logger_check_detects_crossed_or_missing_lines` is the negative control for the check. (ggml lines are not attributed: both ggml copies write the same messages.)

The test and run executables find the DLLs because the crate's `build.rs` adds llama-cpp-sys-2's `out/bin` (from `DEP_LLAMA_GGML_CMAKE_DIR`) as a native search path, which cargo puts on `PATH` for `cargo test`/`cargo run`. A shipped app must carry the DLLs next to the executable (T6.21).

## Build matrix

| Target | Backend | Build | Runs here | Notes |
|---|---|---|---|---|
| x86_64-pc-windows-msvc | CPU | ok | ok | Visual Studio generator, AVX2/FMA/F16C/BMI2 baseline from `.cargo/config.toml` `GGML_*` (llama-cpp-sys-2 forwards them) |
| x86_64-pc-windows-msvc | CUDA 13.4 | ok (`--features cuda`) | ok | sm_120 from `CMAKE_CUDA_ARCHITECTURES`; whisper and llama both on CUDA0 in the coexistence test |
| x86_64-pc-windows-msvc | Vulkan | fails with the VS generator; ok with `CMAKE_GENERATOR=Ninja` | ok | VS generator: llama.cpp's nested `vulkan-shaders-gen` ExternalProject configures but its build step builds nothing, then `file INSTALL cannot find ... vulkan-shaders-gen.exe` (twice, also after a retry). With Ninja (whisper-rs-sys builds fine with it too) the build succeeds. llama.cpp chose the RTX 5080 (`Vulkan1`) over the AMD iGPU (`Vulkan0`) |
| aarch64-pc-windows-msvc | CPU | ok (test binaries, `--no-run`) | not run (GATE-03) | clang-cl + Ninja, `GGML_CPU_ARM_ARCH=armv8.2-a+dotprod+i8mm`, `/EHsc`: the same environment as `cargo xtask build --target aarch64-pc-windows-msvc`. The coexistence executable and all four DLLs are `IMAGE_FILE_MACHINE_ARM64`; the executable imports only `llama_*` from `llama.dll` |

In the Vulkan build whisper.cpp runs on the CPU in the coexistence test: whisper.cpp's Vulkan log has no evidence rule yet (`earshot_asr_whisper::evidence`, T6.10d), and on this desktop whisper picks the AMD iGPU as Vulkan device 0. Both ggml copies are still linked and loaded; llama.cpp runs on Vulkan.

ARM64 command used (from the repo root, after `cargo clean -p llama-cpp-sys-2 -p whisper-rs-sys` for that target if an earlier configure failed):

```powershell
$env:CMAKE_GENERATOR='Ninja'; $env:CMAKE_C_COMPILER='clang-cl'; $env:CMAKE_CXX_COMPILER='clang-cl'; $env:CMAKE_C_COMPILER_TARGET='aarch64-pc-windows-msvc'; $env:CMAKE_CXX_COMPILER_TARGET='aarch64-pc-windows-msvc'; $env:GGML_CPU_ARM_ARCH='armv8.2-a+dotprod+i8mm'; $env:CXXFLAGS_aarch64_pc_windows_msvc='/EHsc'; $env:PATH="C:\Program Files\LLVM\bin;<ninja dir>;$env:PATH"
cargo test --no-run -p earshot-llm-local --target aarch64-pc-windows-msvc --target-dir C:\eb-arm
```

`cargo xtask build --target aarch64-pc-windows-msvc` builds the whole workspace with the same environment and so includes this crate.

## Schema-constrained output

The grammar comes from `earshot_llm_local::grammar`, a small JSON Schema to GBNF converter for the subset `schemas/summary.v1.json` uses (rule shapes from llama.cpp's `common/json-schema-to-grammar.cpp`; unsupported keywords are errors). llama.cpp's own converter lives in its `common` library, which also links cpp-httplib for model downloads; leaving `common` out keeps every HTTP-capable library out of the process (PLAN 3.9), and the crate builds llama.cpp without curl.

`grammar::summary_grammar(ids, limits)` narrows the schema per meeting before converting: citation items are an enum of the meeting's segment ids, decisions and action items cite at least one, every list is capped (12 items, 4 citations), and text must start with a non-space character. Every completed output is therefore valid under `summary::validate_value` (except text that is only non-ASCII whitespace), so the repair loop should rarely be needed for local models.

Sampling is greedy over the grammar: the best unconstrained token is checked against the grammar alone, and the whole vocabulary is filtered only when it is rejected (llama.cpp `common_sampler_sample` with `grammar_first = false`). Same output as filtering every candidate (identical reply bytes on CUDA), but filtering all 151,936 Qwen tokens per step made the grammar the bottleneck: CUDA generation went from 15.0 to 197.4 tokens/s.

## Speed

Qwen2.5 1.5B Instruct Q4_K_M, context 8192, the test meeting (prompt 1,696 tokens after the chat template; reply 629 to 792 tokens, ending with the grammar complete). Generation is greedy with the grammar fast path. Rust debug build unless noted (llama.cpp itself is always built optimised).

| Backend | Prompt tokens/s | Reply tokens/s | Notes |
|---|---|---|---|
| CUDA (RTX 5080) | 9,800 to 16,000 | 171 to 197; 214 to 217 since 2026-10-24 | 171 while whisper ran on the same GPU in the coexistence test; the 2026-10-24 figures (summary and coexistence tests) are after `sample_greedy` stopped copying the logits every token |
| Vulkan (RTX 5080) | 15,400 warm; 89 to 106 cold | 175 to 209 | the first prompt in a process pays shader pipeline creation (about 16 s); measured before the logits copy was removed |
| CPU (8 threads) | 320 to 358 (release, uncontended) | 38.6 to 39.2 (release, uncontended) | re-measured 2026-10-24 (below); 2026-10-23 figures (3.2 to 38.3) were taken under heavy parallel builds |

CPU re-measurement, 2026-10-24: the host was never fully quiet (other agents ran ci in parallel; about 17% total CPU before the run). Host CPU during each run was sampled every 2 s with `\Processor(_Total)\% Processor Time` (the test's own 8 threads are about 25% of the 32 hardware threads) and the number of `rustc`/`cl`/`link`/`nvcc` processes counted. Release test build (`cargo test --release -p earshot-llm-local --test summary -- --include-ignored --nocapture`), two identical requests per run:

| Run | Host CPU median (max) | Other build processes (max) | Reply tok/s | Prompt tok/s |
|---|---|---|---|---|
| release 1 | 51% (74%) | 3 | 16.7, 39.2 | 273, 320 |
| release 2 | 46% (51%) | 3 | 38.7, 38.6 | 177, 358 |
| debug 1 | 51% (74%) | 5 | 7.2, 7.6 | 130, 197 |
| debug 2 | 52% (71%) | 14 | 31.0, 11.7 | 334, 231 |

The ceiling is consistent: about 39 tokens/s on 8 threads whenever the other load stayed low (release run 2, both requests, and the best requests of 2026-10-23), with slowdowns of up to 5x whenever parallel builds peaked. The debug-build CPU runs were all taken under heavier load, so they say nothing about Rust debug overhead (CUDA runs at 215 tok/s in a debug build, so the Rust side costs under 5 ms per token). Use about 39 tok/s for the 1.5B Q4_K_M model on 8 desktop threads when sizing CPU models (T6.20), and expect much less while the app competes for the CPU.

Load time 0.6 to 1.5 s from the page cache. The spike model's summary was well formed but not always right (it named Alice as the owner of Carol's task); quality is T6.20's job.

## Build hazards found

- llama-cpp-sys-2 configures CMake with `always_configure(false)`: a failed or stale configure (for example the first ARM64 attempt without `ninja` on PATH, which left `CMAKE_MAKE_PROGRAM` empty) is reused on the next build. After changing any `CMAKE_*`/`GGML_*` variable or fixing the toolchain, run `cargo clean -p llama-cpp-sys-2` for that target (as for whisper-rs-sys).
- llama-cpp-sys-2 tries to hard-link the DLLs into the target directory only when the destination does not exist yet, so a copy there would go stale after a backend change; on this host it copied nothing (the glob found no files), and this crate uses `out/bin` directly instead. Do not rely on that copy step when packaging.
- Workspace builds now compile llama.cpp (CPU) in `cargo xtask ci`; the model-dependent tests are `#[ignore]`d with the exact command, and the unit tests (grammar, log evidence, the ggml-call guard) run in ci.

## ci cost of llama.cpp (measured 2026-10-24)

- Warm `cargo xtask ci` (second run in the same target directory): llama-cpp-sys-2 is not rebuilt; the clippy and test steps finish their builds in 0.4 s. llama.cpp adds nothing to a warm run.
- Cold run (fresh worktree, empty `target/`): the whole ci took 1,016 s. llama-cpp-sys-2's build script ran twice, once for the clippy step and once for the test step (two `out/` directories, written 08:34 and 08:37: the two steps resolve it as different units), in parallel with the rest of the workspace (clippy step 2 min 29 s and test build 2 min 47 s in total). A cold `cargo build -p llama-cpp-sys-2` alone into an empty target directory took 7 min 26 s while other agents were running ci on the host.
- Decision: no feature gate or separate ci step. The cost is paid once per target directory (a new worktree, `cargo clean`, a toolchain or `CMAKE_*`/`GGML_*` change), like whisper-rs-sys's, and gating the crate would drop its clippy and unit-test coverage from the default run or need a second build anyway. Revisit if warm runs ever rebuild it (for example when an env change re-runs its build script) or if the duplicate build for clippy and test becomes worth removing.

## Local summaries by default (T6.21b)

- Models: manifest set `summary` (`cargo xtask fetch-models --set summary`), Qwen 3.5 9B (Q4_K_M) and Qwen 3.5 4B (Q4_K_M), Apache-2.0, both with the empty think block as assistant prefix (the setup T6.20 measured). The table is `earshot_runtime::summary_model::SUMMARY_MODELS`, pinned to the manifest set by `summary_model_table_is_the_manifest_summary_set`.
- CPU rule (T8.3f): a summary that will run on the CPU gets the 4B whatever the memory (`ChoiceReason::OnTheCpu`; T6.20 measured the 9B at 16.4 min per hour of meeting on 8 desktop threads against 11.2 for the 4B, and GATE-03a's Surface replay took about 4.5 min for 6 minutes with the 9B). Where it runs is decided before llama.cpp loads (`summary_gpu_planned`: the runtime's `cuda` feature and CUDA first in the backend order). A GPU load that is refused and retried on the CPU keeps the GPU's model. Tested by `summary_model_on_the_cpu_is_the_4b_whatever_the_memory`.
- Memory rule (on the GPU): the 9B when `earshot_store::total_memory()` (`GlobalMemoryStatusEx` `ullTotalPhys`) is **more than 12 GiB**, else the 4B; unknown memory gives the 4B and says so. `ullTotalPhys` is installed memory minus what firmware and an integrated GPU reserve (the desktop's 64 GiB reports 66,157,043,712 bytes, 61.6 GiB, measured 2026-10-26), so a 16 GB machine reports under 16 GiB; with memory sold in 8/12/16/24/32 GB sizes, "more than 12 GiB reported" means more than 12 GB installed, and a 16 GB machine qualifies unless it reserves 4 GiB or more. Both sides are tested (`summary_model_choice_on_the_gpu_follows_the_memory_rule`).
- Backend: the GPU when the build's llama.cpp has one (`earshot_llm_local::supports_gpu`) and the runtime's backend order starts with CUDA, else the CPU. A refused GPU load is retried once on the CPU and the refusal printed (`LocalRunReport::gpu_refused`).
- CLI: without `--llm-url` / `--llm-provider`, `record` and `replay` summarise with the chosen model when its file is installed with the manifest size; otherwise one line names `cargo xtask fetch-models --set summary` and no summary is made (never a network endpoint). `--summary-model <id>` picks a table model (a missing named model fails before recording); `--no-summary` skips the summary. The run prints the model, the backend llama.cpp's load log shows and `network: none (local summary, 0 audit entries)`.
- DLLs: `earshot-cli.exe` delay-loads `llama.dll` (`crates/earshot-cli/build.rs`, `/DELAYLOAD:llama.dll` + `delayimp.lib`), so it starts without llama.cpp's DLLs; before the session a local summary checks for `llama.dll`, `ggml.dll`, `ggml-base.dll`, `ggml-cpu.dll` (and `ggml-cuda.dll` in a `cuda` build) beside the exe or on PATH and refuses with the build instruction when one is missing. A `cuda` build also checks for the CUDA runtime `ggml-cuda.dll` loads (`dumpbin /DEPENDENTS`, 2026-11-01: `ggml-cuda.dll` imports `cublas64_13.dll`, which imports `cublasLt64_13.dll`; cudart is static), by prefix (`cublas64_*.dll`, `cublasLt64_*.dll`, `session::CUDA_RUNTIME_DLL_PREFIXES`), so a machine without it gets a message instead of a crash in `supports_gpu()`. `cargo xtask build`, `accept m6`, `t613` and `sum-eval` copy `llama.dll` and llama.cpp's `ggml*.dll` from llama-cpp-sys-2's `<out_dir>/bin` (from cargo's JSON messages) next to the exe they spawn (`xtask::llama_dlls::install_beside`). A hand-made copy of `earshot-cli.exe` (a gate folder) needs those DLLs copied with it.

## For T6.20 and T6.21

- Package `llama.dll` and the `ggml*.dll` set next to the executable; keep whisper.cpp static. The crate's rule (no `ggml_*` from Rust) stays.
- `cargo xtask build --features vulkan` must build llama.cpp with `CMAKE_GENERATOR=Ninja` (the VS generator fails); CUDA and CPU work with either.
- The backend is read from llama.cpp's load log (`log::classify`: `using device`, `model buffer size`, `offloaded n/n layers`); a GPU request that ends on the CPU, a partial offload or mixed devices is an error, never a silent CPU run. A CUDA or Vulkan build lists every GPU it finds (`using device`) even for a CPU load, so a CPU verdict there needs `offloaded 0/n layers to GPU` and only `CPU*` model buffers (`cpu_load_is_cpu_on_every_build`, run on CPU and CUDA builds).
- The summary grammar plus `summary::validate_value` can replace the repair loop for local models; the existing split-and-combine planner sizes requests with `ContextBudget` as today.
