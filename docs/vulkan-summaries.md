# Summaries on Vulkan against CUDA (T8.4c)

Question: can the desktop's GPU summaries run on Vulkan, so an NVIDIA machine needs no NVIDIA download?

## Rule, written before measuring

Setup: the shipped GPU model, Qwen 3.5 9B (`qwen3.5-9b-q4_k_m`), production chunking (600 s), reference source, dev meetings ES2003b, ES2011b, IS1008b, TS3004b, TS3006b (the T8.3ja set), one build per backend from the same commit, on the RTX 5080, on an otherwise idle host. Vulkan restricted to the RTX with `GGML_VK_VISIBLE_DEVICES`, and the result counted only if llama.cpp's load log shows the RTX with every layer offloaded.

Vulkan passes only if all of these hold against the CUDA run of the same commit:

1. Every meeting is valid, and none stalls (a cutoff of 30 minutes per meeting).
2. ROUGE-Lsum and item coverage are each no more than 0.02 below CUDA (the T8.3ja margin).
3. Wall time per hour of meeting is at most 1.5 times CUDA's and at most 90 s (the `sum.gpu_seconds_per_hour` figure in `docs/summary-eval.md`).

A pass makes "no NVIDIA download" a real option for the owner; it changes nothing by itself. Dev data only, and no test-split run.

## Results

Measured 2026-11-06 on the desktop, commit 38cb29a, RTX 5080, 8 CPU threads, every layer on the GPU (load logs: `Vulkan0 (NVIDIA GeForce RTX 5080)` and `CUDA0 (NVIDIA GeForce RTX 5080)`, 33/33 layers). Raw results of the first run of each: `docs/vulkan-summaries/`. Each backend was run twice; the summaries were byte-identical between runs of one backend and different between backends.

| | CUDA | Vulkan |
|---|---|---|
| Valid meetings | 4 of 5 | 5 of 5 |
| Truncated replies, repairs | 3, 2 | 0, 0 |
| ROUGE-Lsum, all 5 (a failed meeting scores 0) | 0.281 | 0.355 |
| ROUGE-Lsum, the 4 meetings valid on both | 0.351 | 0.354 |
| Items covered, the 4 meetings | 11 of 32 (decisions 10 of 22, actions 1 of 10) | 12 of 32 (decisions 9 of 22, actions 3 of 10) |
| Wall time per hour of meeting, two runs | 77.2 s, 77.2 s | 93.9 s, 91.8 s |
| Same, the 4 meetings, first run | 74.0 s | 90.6 s |
| Wait at hang-up per meeting (about 33 min) | 22.4 s | 25.6 s, 25.1 s |

## Verdict

**Fails the rule as written, on one clause, narrowly.**

1. Valid, no stall: pass.
2. Quality within 0.02: pass, on the 4 shared meetings as well as on all 5, and with items counted together. Decisions alone are 0.045 lower on the 4 shared meetings; the rule did not separate them. Five meetings is too few to call Vulkan better.
3. Time: at most 1.5 times CUDA passes (1.19 to 1.22 times); at most 90 s fails (91.8 to 93.9 s). This is not loosened here. The owner decides whether 2 to 4 s per hour of meeting matters.

## Found on the way

- **CUDA, the shipped path, loses `ES2003b` on every run:** one map step's reply is cut off at its budget (`max_reply_budget` 2103 tokens) three times, in the middle of a long, sensible action list, so the meeting gets no summary. That is T8.4d. Greedy output differs by backend, so Vulkan's clean run is not evidence that Vulkan is immune.
- `cargo xtask sum-eval --backend vulkan` on x64 needs `CMAKE_GENERATOR=Ninja` and a Visual Studio x64 environment set by hand: without them llama.cpp's shader generator fails to build.
- This is one GPU. Qwen 3.5 stalled on Vulkan on the Surface's Adreno (T8.3i), so nothing here says Vulkan works on other GPUs. On the desktop `Vulkan0` unrestricted is the integrated Radeon: a shipped Vulkan path needs its own device choice.

## The whole dev split through the shipped path (T8.4ac, 2026-11-06)

T8.4c ran 5 meetings with a device forced by hand. This is all 20 dev meetings, with no device setting: llama.cpp chose `Vulkan1 (NVIDIA GeForce RTX 5080)` itself, commit 1828e5b plus T8.4ac's build changes. Raw result: `vulkan-summaries/t84ac-dev-20-vulkan.json`. CUDA is T8.4d's run of the same meetings and format, `target/sum-eval/t84d-cuda-2`.

| | CUDA | Vulkan |
|---|---|---|
| Valid meetings | 20 of 20 | 20 of 20 |
| Joined summaries | 1 (TS3006d, 5 parts) | 0 |
| Cut-off replies, looping replies, repairs | 1, 3, 3 | 0, 0, 0 |
| ROUGE-Lsum | 0.307 | 0.318 |
| Wall time, all steps | 874.9 s | 975.7 s |
| Per hour of meeting (38,355 s of audio) | 82.1 s | 91.6 s |
| Wait at hang-up per meeting | not compared here | 23.9 s |

Vulkan decodes a little differently, so the same prompts give different text: that TS3006d's merge did not loop here is one run on one meeting, not a fix, and the join fallback stays. 91.6 s is over T8.4c's 90 s line by the same narrow margin the owner accepted there. One GPU only: nothing here says Vulkan works on other cards, which is why T8.4ab watches for a stall.

## What ships (T8.4ac)

Measured on the x64 build of 2026-11-06:

| | |
|---|---|
| NSIS installer | 22.7 MB |
| MSI | 46.9 MB |
| Installed files | 139.7 MB |
| `earshot.exe` | 88.2 MB |
| `ggml-vulkan.dll` | 45.2 MB |
| `vulkan-1.dll` | 1.7 MB |

`earshot.exe` and `ggml-vulkan.dll` both import `vulkan-1.dll`, so a PC without one could not start the app. The install carries LunarG's loader (Vulkan Runtime 1.4.363.0, MIT and Apache 2.0, licence file beside it), byte for byte the file Windows has on this desktop. With no driver Vulkan finds no device and summaries run on the CPU [Likely: not run on such a PC yet]. Transcription uses Vulkan only if the model check keeps it; with no check it runs on the CPU, which a first version of this build got wrong and `accept m7` caught.
