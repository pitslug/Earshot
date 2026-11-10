# Summarising during the meeting (T8.3j): design

Status: option A chosen by the owner; the quality check (T8.3ja) accepted by the owner; the runtime and CLI built in T8.3jb1 (section "What was built"); the Surface measurement is T8.3jb2 and the app setting T8.3jc.

## Goal

With the setting on, each 10-minute chunk is summarised while the meeting is still being recorded. At hang-up only the last chunk and the reduce step remain. On the Surface CPU with the 4B (T8.3f), that is about 290 s from hang-up for a 30-minute meeting instead of 553 s.

## How summaries work today

After the meeting, `host::after_meeting` runs the final pass, then the summary:

1. `time_chunks` cuts the transcript into 600 s windows on segment boundaries.
2. A map step summarises each window into a partial `SummaryV1` that cites that window's segments.
3. The reduce step merges the partials.

Segments are numbered 1..N over the whole meeting. A map step's partial stores segment ids.

## What changes between the live transcript and the final one

- **Ids.** The final pass writes new Final rows with new ids and keeps the Live rows. A partial made during the meeting cites Live ids, which the final summary cannot use: `store_summary` refuses ids outside the current transcript, and reduce drops them silently.
- **Speakers.** Live rows are "Me" and "Them" (mic and loopback). Final rows carry diarised speakers ("Speaker 2", a known person, "Me"). Action-item owners and attendees differ.
- **Words.** For reused stretches the words are identical (D20, `post::reusable`). Echo cleaning and segment boundaries can still differ.
- **Prompt details.** "Part p of N", the duration line and notes near a chunk's end are only known at hang-up. They are small.

## Options

**A. Re-cite live partials after the final pass (recommended).**

- During the meeting, a background worker summarises each finished window from the Live rows.
- At hang-up, the final pass runs as now. Each live partial is then rewritten onto the final transcript:
  - every cited Live segment becomes the Final segment(s) on the same track that overlap it most in time, at most `MAX_CITATIONS`;
  - an action-item owner of "Me" or "Them" becomes the final speaker who owns those segments, when there is exactly one.
- A partial that cannot be fully re-cited is redone from the final transcript, so the result is never worse than today's flow, only slower for that chunk.
- Then the last chunk is mapped and the reduce runs as today.
- Risk: free text in a partial may say "Them" where the final summary would name a person. Measured before it ships (below).

**B. Reuse a live partial only when its chunk's final transcript is unchanged.**

Simple to reason about, but speaker names always change from Me/Them to diarised names, so almost nothing would be reused. Not recommended.

**C. Summarise the live transcript at hang-up, before the final pass, then re-summarise after it.**

A fast draft summary appears soon after hang-up and is replaced by the proper one later. It does not shorten the wait for the final summary and doubles the LLM work. Possible as a later addition.

## Running it during the meeting

- **Thread.** One map worker beside the speaker thread (`live::speaker_thread` pattern), at below-normal priority. It never blocks capture or transcription; if it falls behind, the remaining chunks are simply mapped after the meeting.
- **Model.** Load the model per chunk and drop it after (a load takes 2-5 s), so 3-6 GB is not held for the whole meeting.
- **CPU budget.**
  - On the Surface, transcription runs on the NPU after T8.3e, so the CPU cores are mostly free.
  - On a CPU-only machine the LLM threads compete with live transcription, so the worker gets the cores live transcription does not use. It turns off on battery when speaker work does (`after-on-battery`).
- **Open risk: thread priority.** llama.cpp creates its own CPU threads, and on Windows they do not inherit a lowered priority. Our rules forbid calling ggml directly (`ffi_calls_only_llama_functions`), so its threadpool's priority setting cannot be used as is. Mitigations, in order:
  1. Fewer LLM threads during a meeting.
  2. Lower each llama.cpp worker thread from outside, as `priority::lower_pool_thread` does for ONNX.
  3. Accept normal priority if the measured live lag stays inside budget.

  Decided by measurement on the Surface (accept: largest live transcript lag with the setting on and off).
- **Storage.** Live partials are a device-local record `live-summaries/<meeting>` (they hold summary text, i.e. content). They are deleted by the final summary's write and by meeting deletion, like `live-stretches/*`.

## Quality check before it can be turned on (rule written before measuring)

`cargo xtask sum-eval` gains a mode that summarises each AMI dev chunk with two-track labels instead of names: one headset is "Me" and the other three are merged as "Them". It then re-cites and renames as option A does, and reduces with the real names.

On the dev split, 4B, CPU, the mode is acceptable if the conditions below hold. The owner narrowed the first run to five meetings on 2026-11-03, before any result was scored: the "b" meeting of each dev series (ES2003b, ES2011b, IS1008b, TS3004b, TS3006b; one per series, each long enough to have several chunks). The rest of the dev split runs only if the owner asks.

The mode is acceptable if:

- every meeting gives a valid summary;
- ROUGE-Lsum and item coverage are each no more than 0.02 below the normal run of the same build.

Otherwise the setting stays hidden and the result is reported.

## Result of the quality check (T8.3ja, 2026-11-03)

**The rule fails, by one item.** Command, with `--two-track` added for the second run:

```
cargo xtask sum-eval --backend cpu --split dev --source reference --model qwen3.5-4b-q4_k_m --chunk-seconds 600 --threads 6 --meeting ES2003b,ES2011b,IS1008b,TS3004b,TS3006b
```

The two runs ran side by side, so both took about twice as long as the 4B alone.

| | Normal | Two-track | Rule |
|---|---|---|---|
| Valid summaries | 5/5 | 5/5 | pass |
| ROUGE-Lsum | 0.258 | 0.301 | pass (+0.043) |
| Decision coverage | 12/31 (0.387) | 11/31 (0.355) | **fail** (-0.032; one item is 0.032) |
| Action coverage | 2/10 (0.200) | 2/10 (0.200) | pass |
| Decisions: strict / reference recall (not in the rule) | 3 / 7 | 5 / 9 | |
| Actions: strict / reference recall (not in the rule) | 1 / 1 | 1 / 2 | |

**Re-citing.** Every live chunk was re-cited and reused (13 of 13), none redone. The wait at hang-up fell 39-60% per meeting under the same contention.

| Meeting | Normal ROUGE-Lsum | Two-track ROUGE-Lsum | Normal dec. cov. | Two-track dec. cov. | Normal act. cov. | Two-track act. cov. |
|---|---|---|---|---|---|---|
| ES2003b | 0.207 | 0.249 | 3/9 | 4/9 | 0/0 | 0/0 |
| ES2011b | 0.280 | 0.263 | 1/8 | 2/8 | 0/2 | 0/2 |
| IS1008b | 0.259 | 0.281 | 0/2 | 0/2 | 1/4 | 0/4 |
| TS3004b | 0.238 | 0.276 | 1/4 | 1/4 | 0/1 | 1/1 |
| TS3006b | 0.305 | 0.438 | 7/8 | 4/8 | 1/3 | 1/3 |

The loss is one meeting, TS3006b (7 to 4 decisions covered). Two other meetings gained. Under the rule as written, T8.3jb and T8.3jc stay blocked until the owner decides; the doc allows running the other 15 dev meetings if they asks. Raw results: `target/sum-eval/t83ja-b-normal` and `target/sum-eval/t83ja-b-twotrack` (not committed).

**Decision.** The result is accepted without running the other 15 dev meetings: summarising during the meeting is an opt-in setting, off by default, for users who want a shorter wait at hang-up over the last bit of quality. The default stays as today (whole summary after the meeting). The setting's description in the app says that it is faster at hang-up and may miss some points. T8.3jb and T8.3jc go ahead. The 4B stays the CPU summary model (T8.3l).

## The setting

- A device-local setting `settings/live-summaries` (`DeviceRecord`), off by default, read once per meeting start like speaker timing.
- The app has no Settings screen yet (T7.4 is open). Either:
  - T8.3j's UI half waits for T7.4 and adds a toggle there; or
  - T8.3j adds a minimal Settings panel with this toggle (and speaker timing) that T7.4 later extends.

## Proposed split

- T8.3ja: the sum-eval two-track mode and its measurement (decides whether the rest goes ahead).
- T8.3jb: the runtime map worker, re-citing, storage and the CLI flag.
- T8.3jc: the setting in the app.

## What was built (T8.3jb1, 2026-11-03)

- **Module.** `earshot_runtime::live_summaries` (setting, record, reuse rule) and `live::summary_thread` (the worker). The production model is `SummaryService::live_mapper`, the post-meeting local summary model with fewer threads; a remote summary service has none, so the setting is honoured only with a local model (a live session never makes a connection).
- **When a window is mapped.** The session thread keeps the Live rows it stores. A window `[k s, (k+1) s)` is sent to the worker once every transcribed track has settled past its end: the track's live worker decoded past it (`decoded_through`, read before the worker's events), and for the mic no final starting before it is still held by the live echo filter (`LiveEchoFilter::held_from`). The worker sees the live transcript up to the window's end with no title, duration, attendees or notes, speakers "Me" and "Them", as T8.3ja measured. A window nobody spoke in is not mapped.
- **The worker.** One thread at below-normal priority (as the speaker thread; it stops with a reason if it cannot lower itself), one window at a time, the model loaded per window and freed after it. Transcription never waits for it (`live_summaries_never_hold_up_live_transcription`). At the end the window in progress is cancelled and the worker is waited for at most `LiveConfig::speaker_stop_wait`; windows not finished are mapped by the summary after the meeting.
- **On the graphics card (T9.3).** In the app a window on the card runs in a child process at below-normal priority (`earshot_runtime::summary_child`), watched for stalls, its compute threads included. A crash or a stall costs that window, which the summary after the meeting maps, and never the recording; a cancel kills the child at once. A window on the processor, which is all the T8.3jb2 measurements cover, stays in this thread.
- **LLM threads during the meeting.** `live_summary_threads(cores, live, cap)`: the logical cores live transcription does not use, less two, at least one, at most the post-meeting count (Surface: 12 cores, 6 live CPU threads -> 4). llama.cpp's own compute threads keep normal priority (no ggml call is allowed); T8.3jb2 decides by the measured live transcript lag whether this is enough.
- **Battery.** With `--speakers after-on-battery` the worker does not start on battery and stops when the machine is unplugged (`RuntimeEvent::LiveSummariesOff`).
- **Record.** `live-summaries/<meeting>` (version 2, u32 LE then JSON: model id, window length, partials by window, why the worker stopped early, and since T12.20 the Dictionary words the windows were mapped with; a version-1 record still reads, with no words), written at the session end when there is a partial. `store_summary` deletes it in its write transaction (local or remote summary) and flushes the erasure; meeting deletion sweeps it. A failed or cancelled summary keeps it for the retry.
- **At summary time.** The local path reads the record and the Live rows, re-cites each partial onto the transcript being summarised (the final pass, or the live draft when the post pass was cancelled), and reuses it for its window (`summarize_chunks_with`). A partial that cannot be re-cited, or a record of another model, window length, Dictionary word list (or none recorded) or version, is not used and those windows are mapped normally. `LocalRunReport::live` counts recorded, reused, redone and why a record was not used; the CLI prints it.
- **CLI.** `--live-summaries on|off` and `--save-live-summaries` on `record` and `replay`; the run prints the setting and its source, each window summarised during the meeting, the reuse counts, and `summary: stored N s after the recording ended`.

## Measured on the Surface (T8.3jb2, 2026-11-03)

Input: the first 30 minutes of AMI ES2004b as a two-track call, headset 0 as the mic and headsets 1 and 2 summed as the far end (built by a scratch script from the fetched AMI headsets; timing only, no summary quality is scored on this test-series meeting). `earshot-cli replay --realtime` with `C:\eb\host\release\earshot-cli.exe` (`cargo xtask build`, CPU), Parakeet TDT 0.6B v2 on the CPU for both tracks, speaker work during the call, the 4B on the CPU (`live_summary_threads`: 4 threads during the meeting), once with `--live-summaries on` and once `off`, one after the other on an otherwise idle machine. Events and lag lines in `target/t83jb2/{on,off}-events.jsonl` (not committed).

| | Off | On |
|---|---|---|
| Summary stored after the recording ended | 457.5 s (4 requests) | 293.5 s (2 requests, 2 of 2 live windows reused) |
| Windows summarised during the meeting | - | window 0 in 111 s, window 1 in 125 s |
| Largest live transcript lag, mic | 59.0 s | 64.5 s |
| Largest live transcript lag, far end | 61.8 s | 61.8 s |
| Largest capture lag | 0.04 s | 0.09 s |
| Live finals / words | 125 / 3499 | 125 / 3499 |

- **Wait at hang-up: 36% shorter** (164 s less) for a 30-minute meeting.
- **Live transcription:** the same transcript. The roughly 60 s lags are the speech-stretch design itself (a stretch is decoded once it is settled; on the far end the lag passed 20 s at minute 1:46, long before the first window was mapped) and are the same with the setting off. The mic's largest lag grew by 5.5 s while window 0 was mapped (it passed 20 s at 10:32 in both runs).
- **Decision (rule fixed before this measurement in T8.3jb1, mitigation 3 of "Open risk: thread priority"):** keep llama.cpp's compute threads at normal priority with `live_summary_threads` (4 on the Surface). Fewer threads were not needed: the cost is at most a few seconds of extra lag on a track that is already behind by a minute, and nothing is dropped.

