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
- **A cheaper way to shorten the wait** without new hardware paths: summarise each 10-minute chunk (the map steps) during the meeting, so that only the last chunk and the reduce step run at hang-up. On the Surface CPU 4B that is about 290 s per meeting of about 30 minutes (the "at hang-up" column of `docs/summary-eval/surface-cpu-2026-11-03.md`), against 553 s for all the work after the meeting. The cost is CPU load while recording, so it needs a check against the live transcription budget. Not planned; for the owner.

