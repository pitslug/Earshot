# NPU or GPU summaries without Phi Silica (T8.3h)

Task T8.3h: the non-CPU paths for local summaries on the Surface (Snapdragon X2 Elite, Adreno X2-85 GPU, Hexagon NPU) other than Phi Silica (`docs/phi-silica.md`): ONNX Runtime GenAI on the QNN EP, and llama.cpp on the Adreno GPU.

## Summary

- **NPU (ONNX Runtime GenAI on QNN): not now.** No NPU build of Qwen 3.5 exists; the published NPU LLM builds are fixed at 4,096 tokens of context, less than one chunk prompt plus its reply; the official GenAI packages send telemetry to Microsoft over HTTPS by default (a new network destination, against ground rule 7) unless built from source with it off; there is no Rust binding. It is the only path with a large expected speed-up (Qualcomm publishes Qwen3-4B at 26-30 tokens/s generation on the X2 Elite NPU), at the cost of a model change, shorter chunks, a new quality evaluation and an FFI crate (research estimate 3-6 weeks).
- **NPU through llama.cpp's Hexagon backend: blocked.** Experimental, no Q4_K_M, and on Windows its HTP library needs test signing (`bcdedit /set TESTSIGNING ON`).
- **GPU through llama.cpp: possible with what we pin.** `llama-cpp-sys-2` 0.1.160 has `opencl` and `vulkan` features and its vendored llama.cpp has OpenCL kernels and Vulkan shaders for Qwen 3.5's `GATED_DELTA_NET`, `SSM_CONV` and `SSM_SCAN` ops (checked in the crate source, 2026-11-03). The only published llama.cpp comparison on a Snapdragon GPU (older X1-85, Llama 7B Q4_0) found the GPU slower than the CPU, so only a measurement decides it.

## Checked on the Surface (2026-11-03)

- GPU: Qualcomm Adreno X2-85, driver 32.0.163.2. The driver ships `OpenCL_adreno.dll` and puts `OpenCL.dll` (32.0.163.0) in System32; no OpenCL C headers or import library are installed, so an OpenCL build needs the Khronos headers: a new download source (a manifest entry with licence and checksum), left for the owner.
- Vulkan: the LunarG Vulkan SDK 1.4.363.0 is installed (`VULKAN_SDK`) and `vulkan-1.dll` 1.4.363 is in System32; `earshot-llm-local` already forwards a `vulkan` feature to llama.cpp, and `earshot_llm_local::log::classify` recognises a Vulkan load. So llama.cpp on the Adreno through Vulkan can be measured with nothing new installed.

## GPU measurement: rule fixed before measuring (written 2026-11-03 02:30, before any GPU run)

- **Setup.** Qwen 3.5 4B Q4_K_M, the `sum_eval` example of `earshot-llm-local` built with its `vulkan` feature and run with `--gpu` (every layer offloaded), against the same example on the CPU from T8.3f's timing: dev split, reference source, ES2003a to c, `--chunk-seconds 600`, context 16,384, 6 CPU threads, the production prompt.
- **The GPU path earns a follow-up task only if all three hold:** (1) llama.cpp's load log classifies as Vulkan with every layer offloaded (`log::classify`; a CPU or mixed verdict is a fail); (2) all three meetings give a valid summary; (3) the total wall time is at most 0.67 of the CPU 4B's total (at least 1.5 times faster).
- **Otherwise** the CPU 4B stays (T8.3f) and the Vulkan path is not pursued. Quality is not judged on three meetings: ROUGE is reported for information only.

## GPU measurement: result (2026-11-03)

- **Command:** `cargo xtask sum-eval --backend vulkan --split dev --source reference --model qwen3.5-4b-q4_k_m,qwen3.5-9b-q4_k_m --limit 3 --chunk-seconds 600 --threads 6 --target-dir C:\eb\vk --out target/sum-eval/surface-vulkan`. `--backend vulkan` was added to `cargo xtask sum-eval` for this. It builds the example with `vulkan` and fails unless the result header records a Vulkan load (`sum_eval::check_gpu_backend`).
- **Load:** the 4B loaded on Vulkan in 2.1 s. The example's backend comes from `log::classify`, which refuses a CPU or mixed load when a GPU was asked for.
- **Run:** 03:20 to 06:58, 3 h 38 min, without finishing the first meeting. Over that time:
  - the CPU 4B took 553 s per meeting;
  - the process used about one CPU core throughout (13,034 s of CPU time);
  - Windows' GPU engine counters showed no utilisation for it at 06:58;
  - its partial result file stayed empty.

  Stopped on the owner's instruction. Whether it was hung or just extremely slow was not investigated.
- **Verdict under the rule:** fail on (2), no valid summary, and (3): 13,000 s against at most 1,112 s, which is 0.67 x 1,660 s. Condition (1) held.

## Vulkan diagnosis (T8.3i, 2026-11-03)

Why T8.3h's run loaded the model and then produced nothing. Probe: `crates/earshot-llm-local/examples/vk_probe.rs`. It loads a GGUF, decodes one chat prompt and a short reply, times each phase and cancels after a timeout, so a run that does not stop is stuck inside one `llama_decode`. Controls:

- the same probe in a CPU build;
- Qwen2.5 1.5B Q4_K_M, a plain transformer from the manifest's `llm-eval` set, fetched with the new `cargo xtask fetch-models --set llm-eval --model qwen2.5-1.5b-instruct-q4_k_m`.

Build: `llama-cpp-sys-2` 0.1.160, the newest on crates.io; Adreno X2-85, driver 32.0.163.2.

**Three separate faults**, none of which logs a warning:

1. **The integer dot product matrix path returns NaN for every model.** Qwen 3.5 4B and Qwen2.5 1.5B both stop with "the grammar allows no token here" after the prompt. `GGML_VK_DISABLE_INTEGER_DOT_PRODUCT=1` avoids it, and Qwen2.5 then answers sensibly ("hello world"). llama.cpp also logs "No suitable matrix core mode found" on this driver.
2. **Flash attention hangs once the cache passes 768 positions.**
   - With fault 1 avoided, Qwen2.5 completes 715 prompt tokens in 7.6 s.
   - 795 tokens never return (over 4 minutes inside one decode; no driver reset in the System log).
   - llama.cpp pads the attention cache in 256-position steps, so this is the step from 768 to 1,024.
   - With flash attention disabled (a temporary context setting, not committed), 795 tokens completes in 7.6 s.
   - The T8.3h prompts were 4,000 to 9,000 tokens: this is the overnight stall. The spinning CPU core is the wait on the GPU.
3. **Qwen 3.5 is wrong on Vulkan even with faults 1 and 2 avoided.**
   - Its replies are gibberish ("urintongaziersgiurin power perfect...") where the CPU build answers "hello" to the same prompt.
   - The plain transformer is right under the same settings, so the fault is in an operation Qwen 3.5 adds: its recurrent layers `GATED_DELTA_NET`, `SSM_CONV` or `SSM_SCAN`.
   - Upstream reports the same symptom on Adreno 740 for Qwen 3.5 ([llama.cpp#20603](https://github.com/ggml-org/llama.cpp/issues/20603), closed as stale without a fix). Earlier Snapdragon X reports describe "un-usably low-quality" Vulkan output and a suspected Adreno shader compiler bug ([discussion #8336](https://github.com/ggml-org/llama.cpp/discussions/8336)).

**Speed, where the output is right.** Qwen2.5 1.5B with faults 1 and 2 avoided, Vulkan against the CPU build:

| Prompt | Vulkan prompt | CPU prompt | Vulkan reply | CPU reply |
|---|---|---|---|---|
| 215 tokens | 47 tok/s | n/a | 10 tok/s | n/a |
| 795-815 tokens | 105 tok/s | 316 tok/s | 3.6 tok/s | 12.3 tok/s |

`GGML_VK_PERF_LOGGER` on a 215-token batch put 97% of the 3.06 s in three operations on the 8,960-wide feed-forward intermediate:

- elementwise ADD: 139 x 8.6 ms;
- GLU: 28 x 31 ms;
- one q4_K matrix multiply: 54 x 16.4 ms, at 360 GFLOP/s against 1,300-2,400 for the others.

The elementwise ops cost about 25 ns per element and scale linearly with the batch. `GGML_VK_DISABLE_HOST_VISIBLE_VIDMEM` and `GGML_VK_PREFER_HOST_MEMORY` changed nothing, so it is not buffer placement. It looks like how these shaders run on Adreno.

Switches that changed nothing for the NaN:

- `GGML_VK_DISABLE_FUSION`, `_MMVQ`, `_GRAPH_OPTIMIZE`, `_MULTI_ADD`, `_DOT`, `_ASYNC`;
- `GGML_VK_FA_SPARSE_DISABLE`, `GGML_VK_SERIALIZE_SUBMISSIONS`, `GGML_VK_FORCE_MMVQ`.

`GGML_VK_DISABLE_F16` made a 35-token prompt hang.

**Verdict: not repairable on our side in a way that helps.**

- Faults 1 and 2 have workarounds we could ship: an environment switch, and flash attention off for Vulkan.
- Fault 3 needs an upstream fix in llama.cpp's Vulkan shaders for Qwen 3.5's recurrent ops on Adreno. Without it, the model we ship cannot run there.
- Even fully working, this backend is 3 to 4 times slower than the CPU on this machine for both prompt and reply. Upstream has the same finding on Adreno 740 ("slower than CPU performance", #20603).

So the Vulkan path stays closed. Re-test it only if a later `llama-cpp-sys-2` release notes Adreno Vulkan fixes: `vk_probe` with the 4B and an 800-word prompt is a five-minute check. Qualcomm's own route for Adreno is llama.cpp's OpenCL backend ([Qualcomm developer blog](https://www.qualcomm.com/developer/blog/2024/11/introducing-new-opn-cl-gpu-backend-llama-cpp-for-qualcomm-adreno-gpu)). It lists the Adreno X1-85 as supported and recommends Q4_0 weights, but needs the Khronos OpenCL headers, a new download source and the owner's call. Our Q4_K_M models would need Q4_0 copies to run at its best.

## Vulkan reopened (T12.17, 2026-11-11, first pass)

Upstream since 0.1.160, both from Qualcomm engineers:

- [llama.cpp#30829](https://github.com/ggml-org/llama.cpp/pull/30829), merged 2026-11-10: scalar flash attention capped at 8 rows for heads above 128 on Adreno, because 16 rows spilled registers and one dispatch passed the Windows GPU timeout. Qwen 3.5 has 256-wide heads.
- [llama.cpp#30821](https://github.com/ggml-org/llama.cpp/pull/30821), a draft: the Adreno compiler miscompiles `unpack8()`/`pack32()` on 8-bit vectors (q8_1 quantisation for integer-dot matmuls, K-quant mat-vec, flash attention dequantisation). It replaces them with shifts on Qualcomm. Qualcomm says a driver fix will follow, with no date.
- Neither is in a crate release: `llama-cpp-sys-2` 0.1.161 (2026-11-03) updates nothing in llama.cpp.

**Probe.** llama.cpp master `aa75ff3` plus #30821 cherry-picked, built for ARM64 Vulkan with clang-cl and Ninja, OpenMP off, in a throwaway folder (nothing from it is committed). The tools are llama.cpp's own (`test-backend-ops`, `llama-completion`, `llama-bench`), not Earshot's code. Qwen 3.5 4B Q4_K_M; Adreno X2-85, driver 32.0.163.2, mains power. Long prompt: an invented 1,287-token meeting.

- **Op tests on Vulkan0 against the CPU:** `GATED_DELTA_NET` 43/43, `SSM_CONV` 60/60, `SSM_SCAN` 12/12 and `MUL_MAT` 2,521/2,521 pass. The full `FLASH_ATTN_EXT` suite did not finish in 25 minutes (inconclusive).
- **Fault 3 is gone.** The GPU answers the short prompt as the CPU does, and summarises the long prompt sensibly.
- **Faults 1 and 2 now show as one switch.** With the integer dot product path on, the prompt runs at about 4 tokens/s, and the long prompt with flash attention on gave no output in 900 s. With `GGML_VK_DISABLE_INTEGER_DOT_PRODUCT=1`, flash attention completes the long prompt correctly.
- **Speed, integer dot off, 4B (tokens/s):**

| | GPU | CPU, 6 threads |
|---|---|---|
| pp512 (`llama-bench`) | 30.7 | 119.4 |
| tg32 (`llama-bench`) | 20.3 | 21.1 |
| 1,287-token prompt | 28.1 | 105.7 |
| reply after it | 18.5 | 19.9 |

  Generation, 3-4 times slower than the CPU in T8.3i, is now level with it. Prompt processing is about 4 times slower, and summaries are mostly prompt.
- `GGML_VK_PERF_LOGGER`'s per-op times are not usable on this driver: one matmul shape ranged from 83 us to 285 ms within a run, and one reading implied 66 TFLOPS.

**Verdict (closed by the owner, 2026-11-11):** right answers with one switch, but under T8.3h's rule (at most 0.67 of the CPU's wall time) a prompt-dominated summary cannot pass at a quarter of the CPU's prompt speed. The full `sum-eval` was not run because no crate release carries these changes. The planned rerun on a newer driver was dropped: the newer X2 driver, 32.0.181.0, has one user report (not confirmed by Qualcomm) of an 87% drop in llama.cpp generation speed on the OpenCL backend, and a driver would need about a 6x prompt gain to pass.

**Reopen when** a `llama-cpp-sys-2` release carries #30829 and #30821 (or their successors) and Qualcomm ships a driver with the 8-bit vector compiler fix. Then run the probe again: `test-backend-ops` for `MUL_MAT` and the recurrent ops, then the 4B on a short and a 1,300-token prompt, with integer dot product on and off, against `llama-bench` on the CPU.

## NPU summaries, further (T12.7, 2026-11-11): what is published now

Documentation only so far; nothing downloaded or run.

- **Qualcomm's Qwen3-4B for the X2 Elite** ([qualcomm/Qwen3-4B](https://huggingface.co/qualcomm/Qwen3-4B), Apache-2.0 for the model; the compiled bundles' own terms are not stated). w4a16 bundles for Genie and GenieX (QAIRT 2.50) are on Qualcomm's own download host, with contexts of 512 to 4096 tokens (GenieX) or 4096 (Genie). Its X2 Elite rows, as published and not measured here:
  - GenieX QAIRT, 4096 context: about 26 tokens/s reply, time to first token 0.07 s (short prompt) to 2.2 s (a 4,096-token prompt), so about 1,900 prompt tokens/s;
  - Genie, 4096 context: about 30 tokens/s, 0.07 to 2.2 s.

  Against the CPU 4B on this Surface (about 110 prompt and 21 reply tokens/s, T12.17's numbers), that would be about 17 times faster on the prompt and 1.3 times on the reply. The page says Genie support "will be deprecated soon"; GenieX is the community successor.
- **The runtime is not ONNX Runtime.** Genie's `genie-t2t-run` comes from the QAIRT SDK; GenieX ships as an unsigned Windows ARM64 installer or `pip install geniex`, with a QAIRT plugin bundling runtime libraries v2.50.40. Their licence terms and any telemetry are not documented in what was read. Earshot already loads QNN SDK 2.51 libraries for transcription (plugin v2.7.0).
- **ONNX Runtime GenAI on QNN:** Microsoft publishes QNN builds such as Phi-4 mini and Phi-4 reasoning; community reports put NPU builds at a fixed context of about 4,096 tokens (static graphs). No Qwen 3.5-class NPU build was found from either vendor (Qualcomm has Qwen 3.5 only at 0.8B, as a llama.cpp GGUF).
- **What any NPU path costs:** a model change (Qwen3 4B or Phi-4 mini instead of Qwen 3.5), chunks that fit 4,096 tokens with the reply (today's chunks are 600 s, about 1,500-2,500 transcript tokens, so they may fit with a smaller reply budget), a new quality evaluation on the dev meetings, a telemetry check of the runtime before any meeting text goes in, and an FFI crate.

## NPU summaries, further (T12.7, 2026-11-11): measured on the Surface

Qualcomm's Qwen3-4B for the X2 Elite NPU (w4a16, Genie format, 4,096-token context) on the QAIRT Community SDK 2.50.40, both pinned in `xtask/npu-llm-eval.toml` (set `npu-llm-eval`, fetched and verified by `cargo xtask fetch-models --set npu-llm-eval`). That manifest is kept apart from `models/manifest.toml`, whose `hosts` are the app's download allowlist (pinned by `models::tests::npu_llm_eval_manifest_stays_out_of_the_app`). The SDK's licence is the Qualcomm AI Stack License, as for the `qnn` set. Scripts used are under `target/t127` (not committed); every prompt was invented or public AMI text.

**Telemetry and network: pass.**

- During a run of `genie-t2t-run` on an invented meeting, the process opened no TCP connection or UDP endpoint (polled every 0.5 s).
- It registered 67 ETW providers (`logman query providers -pid`). All of them were traced at every level with the three telemetry keywords while Genie decoded an invented 1,298-token meeting carrying four made-up marker words. Genie generated 302 tokens. Of 17,793 events of its process, no event's data (EventData, UserData, binary payloads as UTF-8 and UTF-16) held a marker word or any prompt word of seven letters or more.
- Only one event carried a telemetry keyword: `Microsoft.Windows.Graphics.D3D12Loader` `ProcessInfo`, system information. About 17,000 events came from one unnamed provider, `{d3bbf1c4-63b7-4707-b668-de8ad1888708}`, with no keyword (probably Qualcomm's driver tracing).
- The check fails closed: it needs Genie's own profile to show the whole prompt decoded. A first attempt under Windows PowerShell 5.1 started Genie without its arguments and was discarded.

**Speed (`genie-t2t-run --profile`, mains):**

| | NPU, Qwen3-4B w4a16 | CPU, Qwen 3.5 4B Q4_K_M (6 threads) |
|---|---|---|
| Load | 60-99 s per process start (3 GB of context binaries) | about 10 s |
| Prompt, 1,264 tokens | 1,144 tokens/s (first token 1.1 s) | about 106 tokens/s |
| Reply | 21 tokens/s | about 20 tokens/s |

The reply, which dominates a summary's time on both, is no faster on the NPU. Only the prompt is (about 11 times).

**Quality: not measurable yet.** `genie-service` (the SDK's OpenAI-compatible server, bound to `127.0.0.1` only) let `sum-eval --remote` drive the NPU through Earshot's own OpenAI-compatible summarizer, with the CPU 4B beside it on the same `--context 4096 --max-output 1024` plan, on ES2003a:

- **The CPU 4B failed to plan:** 11 map requests, then "the model's context window is too small: about 4240 tokens needed, 4096 available" for the merge. Earshot's plan (system prompt and schema, a reply of at least `REPLY_MIN_TOKENS`, `REPAIR_MAX_TOKENS` of headroom) does not fit a 4,096-token window.
- **On the NPU,** the first map reply was JSON of the right shape but broke Earshot's rules (17 citations on one bullet, at most 3, and an extra field). The repair request (original plus reply plus errors) overflowed Genie's cache ("KV$ exceeded budget, but longcontext is not enabled"). After that every request returned 0 tokens until the service was restarted. The same first request on a fresh service again gave the same reply in 18.7 s.
- **Service notes:** it rejects `temperature` 0 (Earshot sends 0.2). Greedy decoding came from `top-k` 1 in the Genie config. At its default log level it writes every prompt in full to its log, which a shipped use would have to turn off.

**What shipping would take,** beyond T12.7's list:

- a request plan for a 4,096-token window, repairs included;
- a way to keep the model loaded across a meeting's requests (one load is 60-99 s);
- recovery from a cache overflow without a restart;
- the native Genie API through an FFI crate instead of the service.

Because only prompt processing is faster, the gain is bounded: a summary request that prints 400 tokens still takes about 20 s on either.

## Recommendation

- **Keep summaries on the CPU with the 4B on the Surface** (T8.3f, merged 2026-11-03). Do not pursue llama.cpp on the Adreno through Vulkan: T8.3i found three driver or shader faults, one with no workaround for Qwen 3.5, and a correct path that is 3-4 times slower than the CPU.
- **OpenCL** is untested. It needs the Khronos headers, a new download source and so the owner's call. The one published Snapdragon GPU comparison makes a large win unlikely.
- **The NPU through ONNX Runtime GenAI** is the only path with a large expected speed-up. It needs:
  - a model change, since there is no NPU build of Qwen 3.5;
  - chunks that fit a 4,096-token context;
  - a GenAI build with telemetry off, or a decision from the owner on its telemetry;
  - an FFI crate;
  - a new quality evaluation.

  Revisit when an NPU build of a Qwen 3.5-class model with a longer context is published.

  T12.7 (2026-11-11, closed by the owner at this point) measured Qualcomm's Qwen3-4B there. Its telemetry check passed, and it reads prompts about 11 times faster than the CPU 4B but writes replies no faster, so a summary gains far less than 11 times. Its quality was not measured, because Earshot's request plan does not fit its 4,096-token window. Taking it further needs, first, an evaluation-only request plan for a 4,096-token window (repairs included), then `sum-eval` on the dev meetings, and only then the shipping items listed above.
- **A cheaper way to shorten the wait** without new hardware paths: summarise each 10-minute chunk (the map steps) during the meeting, so that only the last chunk and the reduce step run at hang-up. On the Surface CPU 4B that is about 290 s per meeting of about 30 minutes (the "at hang-up" column of `docs/summary-eval/surface-cpu-2026-11-03.md`), against 553 s for all the work after the meeting. The cost is CPU load while recording, so it needs a check against the live transcription budget. Not planned; for the owner.

