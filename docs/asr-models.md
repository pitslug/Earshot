# Transcription models: evaluation and ladder (T6.10e)

Measured on the desktop (x86_64, 32 hardware threads, RTX 5080, CUDA 13.4, cuDNN 9.26) with `cargo xtask model-eval`. Reports: `docs/model-eval/cuda.json` (all public corpora), `docs/model-eval/cpu-speed.json` (CPU speed; its whisper rows are superseded by `docs/model-eval/cpu-speed-whisper-2026-11-01.json`, see the findings). Definitions are in the module docs of `crates/earshot-runtime/examples/model_eval/main.rs`; they were fixed before measuring.

**AMI definition change (2026-10-24, T2.5b review).** The AMI columns now use the `accept m2` rule (`asr.wer_ami_ihm`) by the same code (`crates/earshot-asr-whisper/examples/asr_eval/refs.rs`, `load_ami_ihm` / `AmiHeadset::score`): the standard IHM condition with oracle segmentation, each annotated segment of a speaker decoded by one call on that speaker's own headset and scored against the speaker's words inside it (words outside every segment are deletions). They were re-measured for every model on CUDA on 2026-10-24 (`model_eval --corpus ami`, optimised release whisper.cpp) and are in `corpora.ami` of `docs/model-eval/cuda.json` (copied from the two raw run reports in `docs/model-eval/cuda-ami.json`). Until then the AMI column decoded the merged Mix-Headset stream of each meeting in VAD pieces against all four speakers' words (crosstalk, overlapping speech and no oracle segments), which gave about 6 to 10 points more; those numbers are kept, labelled, as `corpora.ami_mix_headset` and in the last column below. Every other column is from 2026-10-22/23 (the GPU speed includes the old AMI decode).

## How it was measured

- Every model runs the production path: Silero VAD speech pieces (`asr::transcribe_in_pieces`, default `PieceConfig`), with the T6.10b thread budget for a two-track meeting (2 threads on the GPU; 4 per track on the CPU here, which has 32).
- Backends are proven, never assumed: whisper.cpp's own log (re-checked after every call), ONNX Runtime's node placement for Parakeet. Every row ran on the backend it reports.
- Whisper runs greedy and beam 5. Parakeet decodes greedily (its only mode).
- **LibriSpeech**: the pinned test-clean subset (196 utterances). **AMI**: IS1009a + ES2004a, IHM with oracle segmentation exactly as `accept m2` (see above); word timing per headset from AMI's forced-aligned word times of the speaker's in-segment words. **Silence**: three 30 s synthetic clips (dither at -90 dBFS, white noise at -60 and -40 dBFS) decoded without VAD.
- Speed is decode time over audio time (lower is faster; under 1.0 keeps up with real time). GPU: over all corpora (about 70 minutes of audio per row). CPU: 10 minutes of AMI IS1009a (`--corpus speed`; accuracy does not depend on the backend, so the CPU pass measures speed only).

## Results

| Model | Decoding | LibriSpeech WER % | AMI IHM WER % | Word start error, median ms | Starts within 250 ms % | Words invented on silence/noise | GPU speed | CPU speed (4 threads) | Old AMI (merged Mix-Headset) WER % |
|---|---|---|---|---|---|---|---|---|---|
| parakeet-tdt-0.6b-v2 | greedy | **1.75** | **10.35** | **60** | **96.6** | 0 | 0.009 | 0.03 | 18.71 |
| parakeet-tdt-0.6b-v3 | greedy | 2.09 | 10.46 | 70 | 94.9 | 0 | 0.010 | 0.03 | 17.45 |
| parakeet-tdt-110m | greedy | 2.39 | 10.99 | 116 | 91.9 | 0 | 0.008 | 0.01 | 20.71 |
| parakeet-ctc-110m | greedy | 2.78 | 12.48 | 120 | 91.0 | 0 | 0.006 | 0.01 | 21.46 |
| large-v3-turbo | greedy | 1.77 | 14.13 | 149 | 63.4 | 6 | 0.023 | not run (too slow) | 20.47 |
| large-v3-turbo | beam | 1.92 | 13.82 | 132 | 65.2 | 6 | 0.044 | | 20.29 |
| large-v3-turbo-q5_0 | greedy | 1.86 | 14.42 | 149 | 63.4 | 6 | 0.024 | not run | 20.62 |
| large-v3-turbo-q5_0 | beam | 1.88 | 14.35 | 134 | 65.5 | 6 | 0.047 | | 20.20 |
| small.en | greedy | 2.58 | 13.36 | 130 | 66.7 | 0 | 0.025 | 0.13 | 22.31 |
| small.en | beam | 2.78 | 12.70 | 118 | 69.8 | 0 | 0.049 | | 21.32 |
| small.en-q5_1 | greedy | 2.65 | 12.84 | 126 | 67.6 | 0 | 0.026 | 0.13 | 22.93 |
| small.en-q5_1 | beam | 2.54 | 12.81 | 122 | 68.1 | 5 | 0.052 | | 21.98 |
| base.en | greedy | 4.12 | 14.70 | 132 | 66.6 | 0 | 0.017 | 0.04 | 22.91 |
| base.en | beam | 3.72 | 13.98 | 122 | 69.1 | 1 | 0.042 | | 21.72 |
| tiny.en | greedy | 5.32 | 16.88 | 126 | 67.9 | 0 | 0.016 | 0.02 | 26.38 |
| tiny.en | beam | 4.98 | 16.77 | 115 | 69.9 | 0 | 0.035 | | 25.06 |

Invented words count after the production non-speech filter (T6.7): large-v3-turbo (and q5_0) produced "Thank you." on all three clips with either decoding; base.en beam produced "You"; small.en-q5_1 beam produced ". . . . ." (counted as five tokens). Annotations such as "[BLANK_AUDIO]", "(water running)" and "[sizzling]" from the smaller Whisper models are dropped by the filter. Parakeet produced nothing at all.

### Findings

- **Parakeet TDT 0.6B is the best model measured on every axis.** It matches large-v3-turbo on clean speech, is about 3.5 points better on meeting audio (AMI IHM 10.35 against 13.82 for turbo with beam search), places words more precisely (median start error 60 ms against 132 to 149 ms; 97% within 250 ms against 63 to 70% for every Whisper model), which the echo rule and the speaker check depend on, invents nothing on silence or noise, and is 2 to 5x faster on the GPU.
- **On the CPU Parakeet is still fastest, by less than first measured.** Parakeet 0.6B runs at 0.03 of real time on 4 threads, against 0.04 for base.en and 0.13 for small.en. The whisper CPU speeds were re-measured on 2026-11-01 with optimised release whisper.cpp (`cargo xtask model-eval --backend cpu --model tiny.en,base.en,small.en,small.en-q5_1 --corpus speed --decoding greedy`, raw report `docs/model-eval/cpu-speed-whisper-2026-11-01.json`, quiet host); the first CPU run came from an unoptimised build (T2.2c) and was 4 to 28x slower (small.en 1.03, small.en-q5_1 3.50, base.en 0.31, tiny.en 0.16). Every model now keeps up with real time on this CPU. Whisper decodes token by token through a large decoder; Parakeet runs its encoder once and takes small decoder steps that skip frames (TDT).
- **Quantised Whisper is no shortcut on our CPU build**: on the optimised build small.en-q5_1 runs at the same speed as small.en (0.13 of real time each, portable AVX2 baseline, whisper.cpp 1.8.3); the first run's 3.5 against 1.03 came from the unoptimised build.
- **Beam search** gains at most 0.7 points of AMI WER for Whisper (base.en 14.70 -> 13.98, small.en 13.36 -> 12.70) and doubles its time or more; for the single pass during a meeting (D20) greedy is the better trade.
- **v2 against v3 in English**: v2 is better on clean read speech (1.75 vs 2.09) and, by the IHM rule, on meeting audio too (10.35 vs 10.46; the merged Mix-Headset stream had favoured v3, 17.45 vs 18.71). Both are far ahead of every Whisper model on meetings. v2 leads the English ladder; v3 is the multilingual model.
- **The 110M models** (TDT and CTC share one encoder) beat every Whisper model on meeting audio (10.99 and 12.48 against 12.70 for the best Whisper row, small.en beam) at a third of base.en's GPU time and a thirtieth of its CPU time. TDT is better than CTC at the same speed.
- **large-v3-turbo on meetings**: by the IHM rule turbo (14.13 greedy) ranks below small.en and its q5_1 quantisation (13.36, 12.84) and just above base.en (14.70); on the merged stream it had ranked second. `model_check`'s `QUALITY_ORDER` follows this report and was updated with it.

### Far-field AMI (SDM, T2.5c, informational)

The same meetings, oracle segments, references and scoring as the AMI IHM column, with every segment decoded from the meeting's single distant microphone (`Array1-01`, the first microphone of the first array: the Kaldi/ESPnet `sdm1` condition) instead of the speaker's headset; overlapping segments of different speakers are each decoded on their own, so crosstalk counts as insertions. Code: `refs::load_ami_sdm` (`crates/earshot-asr-whisper/examples/asr_eval/refs.rs`). Measured 2026-10-24 on CUDA: `model_eval --corpus ami_sdm` (raw report `docs/model-eval/cuda-ami-sdm.json`, copied unmodified into `corpora.ami_sdm` of `docs/model-eval/cuda.json`) and `asr_eval` (the `ami_sdm` informational check of `accept m2`). It is a realistic figure for a laptop or desk microphone at meeting distance, not a threshold.

| Model | Decoding | AMI IHM WER % | AMI SDM WER % | Source |
|---|---|---|---|---|
| parakeet-tdt-0.6b-v2 | greedy | 10.35 | 28.44 | model_eval |
| large-v3-turbo | beam | 13.82 | 34.70 | asr_eval (accept m2 check) |
| base.en | greedy | 14.70 | 42.26 | model_eval |
| base.en | beam | 13.98 | 41.85 | model_eval |

Far-field audio roughly doubles to triples the error rate; Parakeet keeps its lead.

## Ladder (decided with the owner)

Written to `models/manifest.toml` (`[[model]]` with languages, `[[ladder]]` per backend and scope).

| Backend | English | Multilingual |
|---|---|---|
| CUDA | parakeet-tdt-0.6b-v2 -> parakeet-tdt-110m -> base.en | parakeet-tdt-0.6b-v3 -> large-v3-turbo |
| CPU | parakeet-tdt-0.6b-v2 -> parakeet-tdt-110m -> base.en | parakeet-tdt-0.6b-v3 |
| Vulkan (T6.10d, proposed) | small.en -> base.en | large-v3-turbo |
| QNN, the Surface NPU (T8.3e) | parakeet-tdt-0.6b-v2 -> parakeet-tdt-110m | none |

- tiny.en, small.en, small.en-q5_1 and the quantised turbo are off the CUDA and CPU ladders: a Parakeet model beats each of them on accuracy and speed. They stay in the manifest. small.en is on the Vulkan ladder, where Parakeet cannot run.
- base.en stays as the last English rung because it is the only English model that can ship today (MIT); Parakeet ships only after a conversion script from NVIDIA's `.nemo` files and the owner's hosting decision (CC-BY-4.0, attribution).
- large-v3-turbo is the GPU model for languages outside Parakeet v3's 25. On the CPU no measured model covers them; a smaller multilingual Whisper model is not in the manifest yet.
- QNN (the Surface NPU, T8.3e from GATE-03a): the converted Parakeet models only (whisper.cpp has no QNN backend), best first as on the CPU. Measured on the Surface (`docs/model-eval/qnn-surface.json`, `cpu-surface.json`, `--corpus speed`): 0.6B v2 on the NPU WER 1.67% LibriSpeech / 1.83% smoke at rtf 0.02 (the float model on the CPU: 1.75% / 1.22% at rtf 0.04); 110M on the NPU 2.31% / 6.10% at rtf 0.02 (CPU 2.39% / 6.10% at 0.02). Both are within 0.61 points of the float model, so both are kept: 110M is the rung the check falls back to if the 0.6B conversion cannot load (memory or an HTP compile failure). No multilingual model is converted, so multilingual meetings use the CPU ladder. The check settles on the NPU on its own, as on CUDA (D20: NPU above CPU). Vulkan: see below (T6.10d).

## Vulkan (T6.10d)

Measured 2026-10-23 on the desktop with `cargo xtask model-eval --backend vulkan --model tiny.en,base.en,small.en,large-v3-turbo --corpus librispeech,speed --decoding greedy` (reports `docs/model-eval/vulkan.json`; `docs/model-eval/vulkan-igpu.json` with `GGML_VK_VISIBLE_DEVICES=0`, the CPU comparison in `docs/model-eval/cpu-librispeech.json`). whisper.cpp sees two Vulkan devices here: `Vulkan0` AMD Radeon(TM) Graphics (the Ryzen 9 9950X3D's integrated GPU, IGPU type) and `Vulkan1` NVIDIA GeForce RTX 5080; Earshot picks the first discrete device of a backend, else the first integrated one. Every row's backend is from whisper.cpp's log (`whisper_backend_init_gpu: using Vulkan<n> backend` and `whisper_model_load: Vulkan<n> total size`). Only whisper runs on Vulkan: ONNX Runtime (Parakeet, VAD, speakers) has no Vulkan provider here. The machine was shared with other builds during these runs, so speeds are upper bounds; word error rates do not depend on load.

| Model (greedy) | LibriSpeech WER %: CPU | Vulkan, RTX 5080 | Vulkan, integrated Radeon | Speed (10 min AMI), RTX 5080 | integrated Radeon |
|---|---|---|---|---|---|
| tiny.en | 5.32 | 5.30 | 5.34 | 0.035 | 0.087 |
| base.en | 4.12 | 4.12 | 4.12 | 0.024 | 0.092 |
| small.en | 2.56 (2026-10-24) | 2.56 | 2.56 | 0.028 | 0.171 |
| large-v3-turbo | not run (too slow on the CPU) | 1.75 | 1.71 | 0.047 | 0.569 |

- **Parity** (T8.2's rule, within 1.0 point of the CPU on 4,681 LibriSpeech words): tiny.en 5.32 on the CPU against 5.30 (RTX 5080) and 5.34 (integrated Radeon), base.en 4.12 on all three, small.en 2.56 on all three (101 substitutions, 11 deletions, 8 insertions on the CPU): within 0.04 points. The CPU small.en row was measured on 2026-10-24 with optimised release whisper.cpp (T2.2c) at 0.73 of real time on 4 threads, on a host shared with other builds (`cargo run --release -p earshot-runtime --example model_eval -- --backend cpu --model small.en --corpus librispeech --decoding greedy`, the `model-eval` runner's example; row appended to `docs/model-eval/cpu-librispeech.json`); the 2026-10-23 attempt was stopped after about four hours on the then unoptimised build (tiny.en 1.13 of real time against 0.16 in T6.10e). large-v3-turbo has no CPU run; against CUDA (1.77, T6.10e) it is within 0.06.
- **Re-measured for T8.2** (`cargo xtask vulkan-parity`, 2026-11-06 and -07; raw reports in `docs/model-eval/vulkan-parity/`, kept to the rule by `vulkan_parity_committed_reports_hold_the_rule`, backend from each row's recorded whisper.cpp log) on every Vulkan ladder model, greedy, 4,681 LibriSpeech words. CPU, RTX 5080, integrated Radeon: small.en 2.56, 2.56, 2.56; base.en 4.12, 4.12, 4.12; large-v3-turbo 1.77, 1.75, 1.71: within 0.06 points. This closes the turbo row above (not run on the CPU then). The reports' speed fields are from a host shared with other builds and are not performance figures. The integrated Radeon run used `GGML_VK_VISIBLE_DEVICES=0`.
- **Ladder** (proposed, needs the owner's confirmation like the T6.10e ladders): English small.en -> base.en (the best whisper models that invent nothing on silence, T6.10e), multilingual large-v3-turbo. On the integrated Radeon small.en runs at 0.17 of real time on one track, so the two-track check (limit 0.5) decides between it and base.en.
- **Vulkan only where it beats the CPU** (`model_check`): after Vulkan settles, the CPU ladder is searched too, and Vulkan is kept only when its model is more accurate (meeting WER order, `QUALITY_ORDER`, pinned to `docs/model-eval/cuda.json`) or the same model is measured faster. The `earshot benchmark` figures once quoted here (2026-10-23) were never saved as an artefact and have been removed; the rule is pinned by `model_check_vulkan_is_kept_only_when_it_beats_the_cpu` (`crates/earshot-runtime/src/model_check_tests.rs`), and a re-run of `earshot benchmark` on a Vulkan build gives the figures for a host.

## Language coverage

| Model | Languages |
|---|---|
| Parakeet TDT 0.6B v3 | 25 European languages, detected automatically: bg hr cs da nl en et fi fr de el hu it lv lt mt pl pt ro sk sl es sv ru uk (NVIDIA model card) |
| Parakeet TDT 0.6B v2, 110M TDT/CTC | English |
| Whisper large-v3-turbo (and q5_0) | 99 languages plus Cantonese (whisper.cpp's language table), selectable or detected |
| Whisper `.en` models | English |

Quality outside English was not measured here: for v3 and turbo it comes from the publishers' figures and is labelled as such wherever it is shown (T6.10c).

## Open points

- Prompt carry-over in turbo (lowercase pieces without punctuation) was not re-examined: turbo is no longer the English model.
- Parakeet ignores prompts, so names and jargon cannot be primed; NeMo's word boosting is not built.
