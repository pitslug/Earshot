# Granite 4.2 3B against Qwen 3.5 4B on the Surface CPU (T12.27, timing only)

Timing only: one dev meeting cannot say anything about quality, and no quality claim is made from the Surface (docs/overnight-run.md). Quality is the desktop's run under the rule in docs/summary-eval.md.

Setup: the Surface (Snapdragon X2 Elite, 12 cores, ARM64, 16 GB), mains, llama.cpp CPU build (no CUDA or Vulkan feature), 6 threads (`earshot_llm_local::default_threads()` on 12 cores), the production prompt, chunked 600 s, dev meeting ES2003a (1,124 s of audio), reference transcript, thinking off for both (Qwen: `QWEN_NO_THINK`; Granite: the empty think block `<think></think>`). Same settings as T8.3f. The two models ran one after the other in one invocation, nothing else of this session running; other user apps were open (the machine was not otherwise loaded by builds or measurements).

```powershell
cargo xtask sum-eval --backend cpu --split dev --source reference --model qwen3.5-4b-q4_k_m,granite-4.2-3b-q4_k_m --meeting ES2003a --chunk-seconds 600 --threads 6 --out target\overnight\granite-surface
```

| | Qwen 3.5 4B | Granite 4.2 3B |
|---|---|---|
| valid | 1 of 1 | 1 of 1 |
| requests (map + reduce) | 2 + 1 | 2 + 1 |
| load | 3.5 s | 2.1 s |
| tokens read (prompt) | 9,099 | 8,652 |
| tokens written | 2,272 | 1,698 |
| prompt rate | 106 tok/s | 98 tok/s |
| reply rate | 16.9 tok/s | 15.3 tok/s |
| prompt time / generation time | 85.8 s / 134.3 s | 88.4 s / 111.3 s |
| whole meeting | 220.3 s | 199.8 s |
| map steps total / reduce | 143.1 s / 77.2 s | 144.5 s / 55.4 s |
| at hang-up (last map + reduce) | 151.3 s | 127.5 s |
| repairs, truncated, looping | 0, 0, 0 | 0, 0, 0 |

Reading: per token Granite 3B is slightly slower than the Qwen 4B on this CPU (reads at 92% and writes at 90% of the 4B's rate), so it is not the smaller-is-faster model its size suggests. It finished sooner here (127.5 against 151.3 s at hang-up) only because it wrote 25% fewer tokens, mostly in the reduce. Whether the shorter reply loses items is the desktop's quality question. One meeting, one run each: a single timing, not a distribution.

The full sum-eval table is in `granite-surface-cpu-2026-11-13-table.md` (scores on one meeting are noise and are not a result). The result files with summary text stay under `target\overnight\granite-surface` on the Surface and are not committed.
