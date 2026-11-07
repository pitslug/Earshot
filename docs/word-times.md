# Whisper word times: token timestamps vs DTW (T2.2d)

Whisper rungs can time words in two ways (`earshot_asr_whisper::WordTiming`):

- `TokenTimestamps` (the default): whisper.cpp token-level timestamps; a word spans its first token's `t0` to its last token's `t1`.
- `Dtw` (opt-in): whisper.cpp DTW over the model's alignment heads (`dtw_token_timestamps`, flash attention off). The heads are pinned per manifest model in `earshot_asr_whisper::dtw::ALIGNMENT_HEADS`, together with the text-decoder shape they are for, which is checked against the loaded model. Word boundaries follow openai/whisper `timing.py` (`find_alignment`): a word starts at the DTW time of the token before it and ends at that of its own last token.

Parakeet (the default transcriber) is not affected; it already times 96.6% of AMI word starts within 250 ms (T6.10e).

## Measurement rule

Fixed before the first measurement, in `crates/earshot-asr-whisper/examples/asr_eval/word_times.rs` (`asr_eval --metric word_times`, informational, never part of `accept m2`):

- Each model is loaded twice on the same backend, once per timing, through `evidence::load_verified`. Both loads use `TranscribeOptions::live()` (greedy with whisper's temperature fallback).
- The `ami` corpus is AMI IHM with oracle segments (IS1009a, ES2004a; the `asr.wer_ami_ihm` set), one call per segment on the speaker's headset.
  - Hypothesis words are `refs::kept_words`; a word's start is the segment start plus its word start.
  - Reference starts come from the AMI manual annotations v1.6.2 (forced alignments).
  - Words are matched per headset by a longest common subsequence of normalised tokens, as in `model_eval`'s `ami` timing.
  - Reported: median and p90 absolute start error, share within 250 ms, and the signed median.
- The `smoke` corpus is the 20 committed clips. It gives speed only, plus a relative DTW-vs-token shift (it has no word alignments).

**Decision rule.** DTW becomes a model's default only if two things hold:

- on `ami`, its median and its p90 absolute start error are both at least 20 ms (one DTW frame) below token timestamps';
- its decode time is at most 1.10x token timestamps' on the CUDA `ami` run and on the CPU `smoke` run.

## Run 1: first DTW candidate, off by one token (2026-10-25, CUDA, RTX 5080)

This candidate started a word at its own first token's `t_dtw`. whisper.cpp stores a token's own decoder-row jump, and that row attends to the audio of the next token, so starts came out one token late. Signed medians were +122 to +316 ms.

| model | token median / p90 ms | within 250 ms | DTW median / p90 ms | within 250 ms |
|---|---|---|---|---|
| tiny.en | 126 / 638 | 67.9% | 320 / 654 | 26.9% |
| base.en | 132 / 668 | 66.6% | 128 / 294 | 84.8% |
| small.en | 130 / 692 | 66.7% | 188 / 392 | 67.8% |
| small.en-q5_1 | 126 / 681 | 67.6% | 188 / 389 | 68.0% |
| large-v3-turbo | 149 / 800 | 63.4% | 180 / 378 | 71.4% |
| large-v3-turbo-q5_0 | 149 / 810 | 63.4% | 180 / 376 | 71.1% |

The token-timestamp numbers reproduce `docs/model-eval/cuda.json` (greedy, `corpora.ami.timing`) exactly.

## Run 2: the committed rule (2026-10-25, CUDA, RTX 5080)

Raw report: `docs/model-eval/word-times-cuda.json`, unmodified. This run's binary was built before a clippy-only refactor of `collect_segments` that does not change behaviour. The report's `rule_met_this_run` field (false for every model) is not a decision: it mixed the CUDA smoke slowdown into the speed check. The harness now reports `accuracy_rule_met`, plus `ami_speed_check_met` (CUDA runs only) and `smoke_speed_check_met` (CPU runs only).

| model | token median / p90 ms | within 250 ms | DTW median / p90 ms | within 250 ms | DTW signed median ms | ami slowdown | smoke slowdown |
|---|---|---|---|---|---|---|---|
| tiny.en | 126 / 638 | 67.9% | 104 / 268 | 88.6% | +90 | 1.80 | 1.75 |
| base.en | 132 / 668 | 66.6% | 70 / 442 | 82.8% | -64 | 0.98 | 1.18 |
| small.en | 130 / 692 | 66.7% | 42 / 294 | 88.4% | -17 | 0.91 | 0.38 |
| small.en-q5_1 | 126 / 681 | 67.6% | 42 / 292 | 88.5% | -16 | 0.94 | 0.40 |
| large-v3-turbo | 149 / 800 | 63.4% | 42 / 355 | 87.1% | -28 | 1.24 | 0.83 |
| large-v3-turbo-q5_0 | 149 / 810 | 63.4% | 42 / 368 | 86.8% | -28 | 0.92 | 0.76 |

The decoded text was identical in all 505 AMI segments for every model. Words with start == end (untimed for LocalAgreement) went from 111-129 to 63-98 per model, except tiny.en (109 -> 180, of about 4,300 words).

## Upstream defect: DTW must not become a default yet

DTW must not become a default until this is fixed upstream or worked around, whatever the quiet-host measurement shows:

- **Out-of-bounds read.** In whisper.cpp 1.8.3 (vendored by whisper-rs-sys 0.15.0), the placement loop of `whisper_exp_compute_token_level_timestamps_dtw` (`src/whisper.cpp`, about lines 8925-8948) advances to the next segment after every placed token (`++seg_i; tok_i = seg_i->tokens.begin();`) without checking for `result_all.end()`.
  - A window's text can end without a closing timestamp token; the trailing-text branch of `whisper_full_with_state` (about lines 7680-7705) then stores it as a segment that ends on a text token.
  - Placing that last token dereferences `result_all.end()`. This is undefined behaviour (a read past the vector).
  - It did not crash in the runs above, but that is no evidence of safety.
- **Per-window allocation.** The same function creates a ggml context of `dtw_mem_size` (whisper-rs default 128 MiB) for every window of every call and frees it afterwards (its own "FIXME: Allocating mem everytime"). This comes on top of the extra decoder pass and is part of DTW's slowdown on short calls such as AMI segments and live pieces.

## Decision state

- **Blocked by the upstream defect above,** independent of the rule.
- **Accuracy part of the rule: met by every model.** DTW's median and p90 are both at least 20 ms lower than token timestamps' for all six.
- **Speed part: not decidable yet.** Both runs shared the host with three other lanes' evaluations (processor queue length up to 59). The slowdowns above swing from 0.38 to 1.80 and are not a measurement of DTW's cost. The CPU smoke run could not be done.
- **DTW stays opt-in.** The default stays `TokenTimestamps` until a run on a quiet host (CUDA `ami` and CPU `smoke`) applies the rule.
- **If DTW becomes a default** (per model, in `ALIGNMENT_HEADS`), `accept m2` must be re-run, because `asr.stream_determinism` and `asr.echo_leak` use word times through LocalAgreement and the echo de-duplication.

## Quiet-host run

Run from the repo root in PowerShell, with no other evaluation or build running. `$fx` is the fixtures directory (`EARSHOT_FIXTURES`, default `%LOCALAPPDATA%\earshot-dev\fixtures`; the AMI set is fetched with `cargo xtask fetch-fixtures --set ami`). The models need `cargo xtask fetch-models --set desktop`.

CUDA, AMI (the error measurement and the CUDA speed check):

```powershell
$env:PATH = "C:\Program Files\NVIDIA\CUDNN\v9.26\bin\13.4\x64;" + $env:PATH
cargo build --release -p earshot-asr-whisper --features cuda --example asr_eval --target-dir C:\eb-dtw
$fx = "$env:LOCALAPPDATA\earshot-dev\fixtures"
C:\eb-dtw\release\examples\asr_eval.exe (Get-Location).Path --backend cuda --models "$env:LOCALAPPDATA\Earshot\models" --fixtures $fx --metric word_times --word-time-corpora ami > word-times-cuda-quiet.json
```

CPU, smoke (the CPU speed check):

```powershell
cargo build --release -p earshot-asr-whisper --example asr_eval
$fx = "$env:LOCALAPPDATA\earshot-dev\fixtures"
target\release\examples\asr_eval.exe (Get-Location).Path --backend cpu --models "$env:LOCALAPPDATA\Earshot\models" --fixtures $fx --metric word_times --word-time-corpora smoke > word-times-cpu-quiet.json
```

Each model's `ami_speed_check_met` (CUDA report) and `smoke_speed_check_met` (CPU report) must be true, that is `ami_dtw_slowdown` and `smoke_dtw_slowdown` at most 1.10, together with `accuracy_rule_met`, before DTW could become that model's default. Even then, the upstream defect above must be resolved first.
