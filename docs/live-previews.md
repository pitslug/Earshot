# Provisional words while someone is talking (T9.1m)

Runs on 2026-11-07, real-time replay of public audio, production live session (speech stretches,
speaker work on), Parakeet 0.6B v2. A word's lag is the wall time it reached the host loop minus the
word's end in meeting time; the desktop adds only the event to the webview. Raw reports are in
`docs/live-previews/`, counts and times only.

**Start with "On an idle host" below**, the only runs with nothing else on the machine. The sections
after it were measured with other lanes and a ci run active, so compare within a table only. The
Surface and battery draw are not measured at all (GATE-03a first).

## On an idle host

Commit e28b693, nothing else running, machine load read median 0-3%, largest 4%. Far-end track.
Reports are `docs/live-previews/idle-*.json`.

| Audio, backend | Settled text median / p95, previews off | Settled text, on | On screen median / p95 | Largest on screen | Preview words shown | Settled text said otherwise | Left with no settled word |
|---|---|---|---|---|---|---|---|
| `continuous`, CPU | 37.4 / 50.2 s | 37.4 / 50.1 s | **2.4 / 4.4 s** | 51.4 s | 213 of 230 | 4 | 0 |
| `continuous`, CUDA | 36.9 / 49.9 s | 36.8 / 49.6 s | **1.6 / 2.8 s** | 50.8 s | 225 of 230 | 5 | 0 |
| `bundled`, CPU, first run | 14.9 / 37.9 s | 15.0 / 37.9 s | **2.9 / 6.5 s** | 40.6 s | 212 of 212 | 17 | 1 |
| `bundled`, CPU, second run | | 14.9 / 37.8 s | **2.8 / 8.7 s** | 40.6 s | 207 of 212 | 13 | 1 |
| `bundled`, CUDA | 14.1 / 36.8 s | 14.0 / 36.7 s | **1.9 / 6.4 s** | 39.4 s | 213 of 212 | 12 | 9 |

- **The median holds under 3 s in every run. The 95th percentile does so only on unbroken speech on CUDA**; on meeting audio it is 6.4-8.7 s. The largest stays at 40-50 s: the first words of a stretch, before two previews agree.
- **Stored words are identical** on and off, on both backends (`stored_sha256` per track). No audio dropped, no slow pauses, slowest preview 0.45 s; the 107-word and starved runs below did not recur in these runs, so those came from what else was on the machine or remain unexplained.
- **Real decodes did not slow:** rtf 0.034-0.043 off and 0.033-0.036 on, on the CPU; 0.014 off and 0.008 on, on CUDA.
- **Cost as before:** peak private memory 7.15 -> 9.39 GiB (CPU, bundled), 11.71 -> 15.40 GiB (CUDA); process CPU time 65 -> 131 s (bundled) and 31 -> 137 s (continuous) on the CPU, 29 -> 35 s and 14 -> 23 s on CUDA; one session 2.13 GiB, load 2.5 s on the CPU.
Memory with T9.1l: a preview decode still running when a meeting ends is left to finish, so its session
can overlap the next meeting's for as long as that decode takes (not measured; under 1 s at these rates).

## The baseline: no previews

First run, in a busy period (machine load median 5%, largest 30%):

| Audio | Track | Words | Stretches | Word lag median | p95 | largest | Decode rtf |
|---|---|---|---|---|---|---|---|
| `bundled` (AMI ES2004a, 150 s, two tracks) | mic | 242 | 11 | 19.0 s | 37.3 s | 39.3 s | 0.13 |
| `bundled` | reference | 212 | 8 | 23.6 s | 49.6 s | 54.5 s | 0.26 |
| `continuous` (smoke clips, 200 ms gaps, 120 s, reference only) | reference | 230 | 6 | 42.3 s | 56.8 s | 63.1 s | 0.19 |

- **The median is far over the 3 s target**, so the rule says go on.
- **Most of the wait is for the stretch, not the decode**, as the plan guessed, but "up to about 30 s"
  understates it: a person who never pauses waits about 40 s at the median and over 60 s at worst.
  `settle` keeps a cut piece back until the next part can no longer merge into it, so a piece ending
  20 s into a stretch settles about 30 s after its own end. Decoding is the small part: rtf 0.036 in
  the quiet runs below (docs/asr-models.md says 0.03), 0.13-0.26 in this busy one. Quiet runs of the same audio with no previews: reference median 14.9 s and p95 37.8 s on `bundled`, 37.6 s and 50.4 s on `continuous`.

## One copy of the weights (T9.1ma)

**Why a preview cost 2.2 GiB:** Parakeet 0.6B's encoder keeps 2.27 GiB of weights in `encoder.weights`, and every session held its own copy, three in a call. CPU sessions now share one (`earshot_onnx::shared_weights`). Raw reports: `docs/live-previews/shared-weights/`. All on the bundled meeting unless named, idle host (machine load median 0-6%), rule chosen after measuring: least memory with stored words identical and on-screen lag inside today's run-to-run spread.

| Way of loading | Peak private memory, previews on | Off | On screen median / p95 | Settled text changed a shown word | Runs |
|---|---|---|---|---|---|
| Each session its own copy (before) | 9.39 GiB | 7.15 GiB | 2.6-2.9 / 6.5-11.8 s | 11-17 of about 200 | 3 |
| **One copy, prepacking off (shipped)** | **5.81 GiB** | **5.52 GiB** | 2.7-2.9 / 8.9-10.4 s | 11-14 | 2 built |
| Same, trial patch | 5.81 GiB | 5.52 GiB | 2.7 / 10.0-17.3 s | 11-15 | 3 |
| One copy, prepacked copies shared | 7.68 GiB | | 2.2-2.8 / 5.5-9.9 s | 11-15 | 2 |
| Each its own, previews on Parakeet 110M | 7.90 GiB | | 2.0-2.1 / 21.9-22.8 s | 37-38 | 2 |

`continuous`, built: peak 5.50 GiB, on screen 2.0 / 3.8 s, 5 words changed. Stored words matched the earlier runs by `stored_sha256` in every run, both tracks. Real decodes rtf 0.023-0.026 against 0.022-0.036. Sessions alone (`built-memory.json`): three take 3.11 GiB against 6.69, loading peaks at 2.41 GiB, a 12 s decode takes 0.27 s against 0.25-0.29 s.

- **`AddExternalInitializersFromMemory` does not share:** it copies, and each session still cost 2.4 GiB (`share-heap*.json`). `AddInitializer`, one tensor at a time, does.
- **Prepacking is a second copy.** Off, each session's kernels read the shared tensors. The p95 is noisy between identical runs, so these runs cannot say whether turning it off costs anything there. The prepacked variant needs about 1.9 GiB more and stays the fallback if a machine shows otherwise.
- **110M is out:** cheapest in CPU time (62-68 s against 97-131 s) but about three times the corrected words, and a p95 of about 22 s on both backends.
- **CUDA does not share:** 16.41 GiB (no prepacking) and 17.68 GiB (shared prepacking) against 15.40 GiB, because the graphics card holds its own copies. 110M previews there reach 13.32 GiB with the same corrections. CUDA stays as it was.
- **The Surface is not measured.** Its encoder runs on QNN and does not share; only a CPU fallback there would.

## Can a preview share the track's session?

No. `Transcriber::transcribe` and `ort::Session::run` (2.0.0-rc.14) both take `&mut self`. Parakeet
keeps no state between calls, so sharing would be correct, but only behind a lock a real decode could
wait on for a whole preview (0.5-1.6 s measured), or on the worker's own thread with the same wait.
"No preview may delay a real decode" rules both out. Whisper is worse: sampled retries depend on the
state's history, so a preview must never touch it.

So one extra session per meeting: previews run on one track only (below), on their own thread below
normal priority, on background ONNX Runtime threads.

Memory (`memory-cpu.json`, `--memory`), before T9.1ma: **2.2 GiB of private memory per session, nothing shared**
between sessions, measured three times, load 4-14 s. The plan's "about 2.4 GB each" holds. The tables below are from before T9.1ma too.

| | CPU, previews off | CPU, on | CUDA, off | CUDA, on |
|---|---|---|---|---|
| Process peak private memory, bundled | 7.15 GiB | 9.39 GiB | 11.69 GiB | 15.42 GiB |
| Graphics memory, whole card (1.3 GiB before the run) | | | 9.1 GiB | 12.8 GiB |
| Process CPU time, bundled (150 s) | 64 s | 132 s | 29 s | 38 s |
| Process CPU time, continuous (120 s) | 31 s | 140 s | | 24 s |

So on the CPU about 0.5 of a core over a meeting and 0.9 of a core while someone talks without pause.

Setup cost of many short decodes: none found on the CPU. A length not seen before decodes as fast as
a repeat of it (first against second, 0.98/0.72, 0.90/0.81, 0.85/0.86, 0.90/0.91, 1.22/1.25,
1.26/2.05 s). On CUDA this was not isolated; preview rtf there was 0.008-0.015, so any such cost is
small against the 1 s interval.

## With previews

The CPU rows are the final code; the CUDA rows ran before the slow-preview pause, which could not have fired there (slowest preview 0.57 and 0.37 s). Machine load read in these runs: median 1-3%, largest 7%. Parakeet 0.6B v2, far-end track.

| Audio, backend | Word lag median / p95, settled text | On screen median / p95 | Preview words' own lag median / p95 | Preview words shown | Of those, the settled text said otherwise | Left with no settled word at them |
|---|---|---|---|---|---|---|
| `continuous` (230 words), CPU | 37.6 / 50.2 s | **2.4 / 5.3 s** | 2.4 / 4.4 s | 215 | 7 (3.3%) | 0 |
| `continuous`, CUDA | 36.8 / 49.6 s | **1.5 / 2.5 s** | 1.5 / 2.4 s | 225 | 5 (2.2%) | 0 |
| `bundled` (212 words), CPU | 14.8 / 37.8 s | **2.9 / 11.0 s** | 2.9 / 9.2 s | 201 | 11 (5.5%) | 1 |
| `bundled`, CUDA | 14.0 / 36.8 s | **1.9 / 5.8 s** | 1.9 / 3.4 s | 205 | 14 (6.8%) | 1 |

- **The 3 s target is met at the median in all four, and not at the 95th percentile.** The largest on-screen lag stays at about the settled text's, 40-50 s: a stretch's first words before any preview agrees, and words no preview caught.
- **The CPU varies from run to run.** Three bundled CPU runs on the same audio: 2.9 / 11.0 s (above, 201 words shown), an earlier one at 5.8 / 28.9 s with 107 words shown and 47 of 73 decodes adding nothing (report not kept), and one in which previews were starved. The cause of the 107-word run is not known. In the starved run (`previews-bundled-cpu-starved.json`, machine load read 1-3%, real decodes at their normal rtf 0.036) previews ran at rtf 0.34 and took over 3 s each, and a rule then in place turned them off for the meeting after 6 decodes: below normal priority, they lost the machine to something the 30 s load reading did not see, and real decodes did not. Previews now pause for 30 s and try again instead (`PreviewReport.slow_pauses`). A quiet-host run is what settles this.
- **A shown word never vanishes and returns**: only what a real decode covers leaves. The last column counts words that left with no settled word near them, which the plan asks for: the settled text lacked them. 35 of 74 decodes added nothing on the bundled meeting (two decodes must agree).
- **Stored words are identical on and off**: `stored_sha256` matches per track between on and off, on both backends and both audio sets, and between backends (`previews-*`, `previews-off-*`). Real decode rtf: 0.036 off and 0.034-0.038 on, on the CPU; 0.013 off and 0.010 on, on CUDA. Per run previews skipped 0-3 and cancelled 0-3, and dropped no audio.
- **Window**: 12 s kept. A 6 s window showed 53 of 230 words on continuous speech against 222 in the run beside it (`previews-continuous-cpu-window6.json`; the 222-word report was not kept). It ran in a noisier moment (slowest preview 2.9 s), so it is not a clean comparison, but nothing favours it.

## What was chosen

- **Interval 1 s, window 12 s**, the plan's starting values, and at least as long after a preview as
  it took, so they never queue.
- **One track per meeting: the far end in a call, the mic in the room** (since T9.14 the default,
  "Other people only"; "Everyone" adds the call's mic, below). A call's mic shows none by default: its
  words are held and cleaned by the echo filter, and far-end words leak in until that runs
  (`live_preview_mic_in_a_call_shows_no_far_end_word`). You know what you just said. A real decode
  covers it within the same lag as before.
- **Parakeet on the CPU and CUDA only.** The NPU stays off until GATE-03a and a measurement: its encoder
  always takes a full 30 s input and its memory on 16 GB is unmeasured. Whisper on Vulkan or the CPU has
  none: it would need its own second model, unmeasured here, and a preview must not touch the real
  state. `backends::previews_refused` is the one rule.
- **Machines:** at least 20 GiB of memory and, on CUDA, a card with at least 14 GiB (both judgements from
  the numbers above; 12 GB and 16 GB machines stay off). Off on battery. Off for the rest of a meeting
  when a track's queue passes 5 s, and during a hold and its catch-up. Three previews in a row slower
  than 3 s pause them for 30 s. Each turn-off is in `PreviewReport.off`, and affects neither recording nor stored transcripts.

## Both tracks in a call (T9.14)

The owner asked for grey words on their own voice in a call too: Settings, General, "Everyone" runs a second preview session on the mic track (`own_voice`, refused by `backends::previews_refused_second`). Measured 2026-11-08 on the desktop's CPU, idle host (machine load at most 7%), the bundled meeting, real time, Parakeet 0.6B v2 (`t914-others-bundled-cpu.json`, `t914-everyone-bundled-cpu.json`):

| | Other people only | Everyone |
|---|---|---|
| Peak private memory | 5.81 GiB | 6.08 GiB |
| Process CPU time over 150 s | 138 s | 217 s |
| Mic words on screen, median / p95 | 16.2 s / 32.9 s | 2.9 s / 22.0 s |
| Real decode rtf, mic / reference | 0.039 / 0.038 | 0.039 / 0.039 |
| Mic preview words that left without settling | - | 42 of 221 |

- **Memory**: the second session shares the weights, so its limit stays at one session's (`PREVIEW_BOTH_MIN_MEMORY`, 12 GiB). CUDA is refused until measured there.
- **Stored words are identical**: `stored_sha256` matches per track between the two runs.
- **Far-end words on the mic**: the 42 that left are the reason the design above kept the mic off. The AMI headsets pick up the other speakers, and the mic's grey words show them until the settled, echo-cleaned text replaces them. On speakers without headphones expect more. Display only; the General page says so under Everyone.

## NPU measurement (T12.4)

The app refuses previews on the NPU (`backends::previews_refused`: "not measured on the NPU yet"). To measure them on the Surface, `live_lag` takes `--backend qnn` and an evaluation-only `--force-previews`, which loads the preview sessions through `backends::load_preview_unrefused`: the same load as the app's (background threads, the NPU conversion, `check_qnn_parakeet`), without the refusal. Nothing the app or `host::live_previews` decides changes; `backends_preview_unrefused_load_is_used_by_no_shipped_code` keeps it out of shipped code.

**The rule, written and committed 2026-11-09 before any NPU run** (judgements, not derived; not while the owner is in a meeting; plugged in, since previews are off on battery; nothing else running). Each of "on" and "everyone" is judged against "off" on the same audio, and passes only if it passes on both `bundled` and `continuous`:

1. **It ran on the NPU.** Every preview session in `preview_sessions` reports `loaded.kind` Qnn with the encoder on `QNNExecutionProvider`, and both tracks' own sessions are on QNN. Otherwise the run says nothing about the NPU and is reported as such.
2. **Stored words are identical.** `stored_sha256` matches "off" for every track.
3. **The main transcript does not slow.** For each track, real decode rtf at most 1.5 times "off"'s (the NPU decodes at about 0.016, so contention for it shows as a multiple), and `catch_up_after_end_s` at most "off"'s plus 2 s. No audio dropped.
4. **The previews are worth having.** `on_screen_lag_s` median at most 3 s on each previewed track, no preview turned off for slowness (`off` empty), and at most 2 slow pauses per track.
5. **Memory leaves room on a 16 GB Surface.** In `--memory`, after the sessions the setting needs are loaded (two tracks' own plus one preview for "on", plus two for "everyone"), `system_free_gib` is at least 4 GiB.

If "on" passes, QNN leaves `previews_refused` for "Other people only". If "everyone" also passes, `previews_refused_second` allows QNN. A failing setting stays refused, and the numbers and the failed point go here. Load time per NPU session (about 29 s) is reported, not judged: previews load after the track's own worker exists, so a meeting never waits for them.

Build and run natively on the Surface, from the owner's own shell (the same build `cargo xtask model-eval --backend qnn` used there), with the models folder holding sets `qnn` (the plugin) and `qnn-models` (the NPU conversions) plus `ci` (VAD and speaker models). Add `--models <dir>` after `--backend qnn` if they are not in `%LOCALAPPDATA%\Earshot\models`. Each run is about 150 s of real time on the bundled meeting.

Previews off:

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews off --out target\live-lag\qnn-off-bundled.json
```

The other people only (one preview session, the far end):

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews on --force-previews --out target\live-lag\qnn-on-bundled.json
```

Everyone (a preview session on each track: four NPU encoders with the two tracks' own sessions):

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews everyone --force-previews --out target\live-lag\qnn-everyone-bundled.json
```

Repeat the three with `--audio continuous --seconds 120` (file names `-continuous`). Memory and load time per session: `--memory` loads one track's own session, then a first and a second preview session as the app loads them (always past the refusal), each followed by three 12 s decodes. A call holds two track sessions, so a meeting adds one, two or three sessions to that for off, on and everyone; the replay runs above measure the whole meeting.

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --memory --out target\live-lag\qnn-memory.json
```

What to read:

- `live_backends` and `onnx_placement`: each track's own session on QNN, from ONNX Runtime's placement (encoder on `QNNExecutionProvider`, prediction network and joint on the CPU).
- `preview_refusal`: what the app would decide on this machine (`app_first`, `app_own_voice`) and `forced_past_it`. A run without `--force-previews` on the NPU shows the refusal in `preview_sessions[].not_loaded` and no previews: that is the app's behaviour, not a failure.
- `preview_sessions`: one entry per preview load (`first` for the far end, `own_voice` for the mic under Everyone), `refusal_bypassed`, the backend the loaded session reports (`loaded.kind`, read from the encoder's placement) and each ONNX Runtime session's placement. A preview that did not reach the NPU has no `Qnn` there: say so beside its numbers.
- The main transcript: `decodes_mic.rtf`, `decodes_reference.rtf`, `reference.word_lag_s` and `catch_up_after_end_s`, off against on and everyone, judged by the committed rule. `stored_sha256` must match between the runs of one audio.
- The previews: `previews[]` per track (`rtf`, `slowest_s`, `slow_pauses`, `skipped_busy`, `off`), `reference.on_screen_lag_s` and, under Everyone, `mic.on_screen_lag_s`.
- Memory: `host.peak_private_gib` per run; in `--memory`, each `session N loaded` stage (`role`: a track's own, first preview, second preview) with `load_s`, `backend`, `placement`, `private_gib` and `system_free_gib`. The NPU's memory may sit outside this process's private bytes, so read the machine's free memory too.

**Result, 2026-11-09** (Surface, mains, the rule above unchanged; reports in `t124-qnn/`). Every session, the previews' included, ran its encoder on `QNNExecutionProvider`; stored words matched "off" on every track of every run.

| | Off | Other people only | Everyone |
|---|---|---|---|
| Real decode rtf, bundled mic / reference | 0.022 / 0.020 | 0.024 / 0.023 | 0.025 / 0.031 (1.49x) |
| Real decode rtf, continuous reference | 0.012 | 0.013 | 0.017 |
| Catch-up after the end, bundled / continuous | 1.8 / 1.9 s | 3.4 / 2.7 s | **5.0 / 5.4 s** |
| Far-end words on screen, median, bundled / continuous | 14.5 / 37.0 s | **2.6 / 2.1 s** | 5.7 / 36.9 s |
| Mic words on screen, median, bundled | 15.7 s | 16.1 s | 3.6 s |
| Preview audio dropped, slow pauses | - | 0, 0 | mic 1609 frames, 0 |
| Peak private memory, bundled | 6.73 GiB | 8.75 GiB | 10.76 GiB |

`--memory` (machine load 2%): each NPU session costs about 2.0 GiB of private memory and loads in 29-31 s; nothing is shared. System memory free: 6.04 GiB with no session, 3.25 with one, 1.10 with two, 1.06 with three.

- **Everyone fails** points 3 (catch-up +3.3 and +3.5 s against at most +2 s) and 4 (far-end median 5.7 s on the meeting, and on continuous speech only 55 words shown against 186: four sessions contend for one NPU). It stays refused on the NPU.
- **Other people only passes points 1 to 4 and fails point 5**: with the three sessions a call needs, 1.06 GiB was free against at least 4 GiB. The run started with only 6.04 GiB free of 16, so the Surface was holding about 10 GiB elsewhere; the rule's "nothing else running" was not met for memory. The owner then reported that the Surface idles above 60% memory use with nothing open (`system_free_gib` reads `Win32_OperatingSystem.FreePhysicalMemory`, Windows' available memory), so 6 GiB is its real starting point and a re-run would not change the answer.

**Decision (2026-11-09): both settings stay refused on the NPU** (`previews_refused` and `previews_refused_second` unchanged). Even with previews off, a call's two NPU sessions leave about 1.1 GiB available on the Surface before the summary model loads. Precompiled NPU models (T12.12) are the lead for less memory per session; if a session drops well below 2 GiB, this is re-judged under the same rule.

**Sharing one copy of the model between NPU sessions: not measurable with how Earshot loads, so there is no `--share-npu`.** ONNX Runtime's key is `ep.share_ep_contexts` (`kOrtSessionOptionShareEpContexts`, "Share EP related resources across sessions", in `include/onnxruntime/core/session/onnxruntime_session_options_config_keys.h`, beside `ep.stop_share_ep_contexts` and `ep.context_enable`). In the QNN plugin Earshot loads (onnxruntime-qnn tag `v2.7.0`, `onnxruntime/core/providers/qnn/qnn_execution_provider.cc` and `shared_context.h`):

1. Sharing works only for EP context models (a model holding `EPContext` nodes that point at a compiled QNN context binary, made with `ep.context_enable=1`): `GetCapability` takes the shared path only under `is_qnn_ctx_model && share_ep_contexts_`, and `Compile` looks graphs up in `SharedContext` only in `CompileContextModel`. Earshot loads the QDQ ONNX conversion and the plugin compiles it at every load, so the key would do nothing.
2. Even with EP context models it does not let identical sessions share: `SharedContext::GetSharedQnnModel` moves the graph out of the shared list and erases it, so each graph name is taken by one session. It is made for one context binary holding several different graphs (QNN weight sharing across graphs), not for three sessions of one graph.
3. The plugin's own `htp_share_resource_optimization=1` provider option shares the `QnnBackendManager` (backend handle and VTCM backup buffers) between sessions, not the weights; its exact provider-option key through the plugin interface is not confirmed here.

So the NPU sessions of an Everyone meeting (two tracks' own and two previews) each hold their own compiled graph; `--memory` measures what that costs. Making them share would need a different design (one session serving several tracks, or a context binary with one graph per session) and is a separate task.

## Your own words on the CPU beside CUDA (T12.3a)

With transcription on CUDA, Everyone refuses the mic track's preview session ("not measured on the graphics card yet"): a fourth session on the card costs about 3.7 GiB of graphics memory. T12.3a runs that one session on the CPU instead, with the CPU's shared weights, so the card holds no more than today.

**The rule, written and committed 2026-11-09 before any code or measurement** (judgements; desktop, RTX 5080, idle host, bundled and continuous audio, real time). "Everyone" with the mic preview on the CPU is judged against "Other people only" on CUDA, on the same audio, and passes only if it passes on both:

1. **Where it ran.** The far-end preview and both tracks' own sessions report CUDA from ONNX Runtime's placement; the mic preview reports the CPU. Otherwise the run says nothing about this design.
2. **Stored words are identical.** `stored_sha256` matches for every track.
3. **The main transcript does not slow.** For each track, real decode rtf at most 1.5 times the comparison run's, and `catch_up_after_end_s` at most its value plus 2 s. No audio dropped.
4. **The mic previews are worth having.** `mic.on_screen_lag_s` median at most 3 s on the bundled meeting (continuous audio has no mic speech), no preview turned off for slowness, at most 2 slow pauses per track; the far-end preview's on-screen median at most its comparison value plus 1 s.
5. **Cost.** Peak private memory at most 3 GiB above the comparison run's, and graphics memory no higher than the comparison run's plus 0.5 GiB (read from `nvidia-smi` beside the run).

If it passes, `previews_refused_second` lets CUDA run the mic preview on the CPU, within the CPU memory limit (`PREVIEW_BOTH_MIN_MEMORY`), with the General page saying where it runs. If it fails, CUDA keeps refusing, and the numbers and the failed point go here.

**Result, 2026-11-09 (overnight, idle desktop; reports and `nvidia-smi` logs in `t123a/`): fails point 4 by 5 ms.**

| | Other people only (CUDA) | Everyone, mic preview on the CPU |
|---|---|---|
| Sessions (from placement) | both tracks and far-end preview CUDA | the same, plus the mic preview on the CPU |
| Stored words, bundled and continuous | | identical (`stored_sha256` per track) |
| Real decode rtf, bundled mic / reference | 0.0115 / 0.0110 | 0.0117 / 0.0100 |
| Catch-up after the end, bundled / continuous | 0.39 / 0.34 s | 0.40 / 0.37 s |
| Mic words on screen, median, bundled | 15.6 s | **3.005 s** (limit 3 s) |
| Far-end words on screen, median, bundled / continuous | 2.00 / 1.51 s | 2.04 / 1.53 s |
| Slow pauses, preview audio dropped | 0, 0 | 0, 0 |
| Peak private memory, bundled / continuous | 15.42 / 14.80 GiB | 17.95 / 17.27 GiB (+2.5) |
| Card peak, bundled / continuous | 12,980 / 12,502 MiB | 12,970 / 12,570 MiB |

Points 1, 2, 3 and 5 pass with room; point 4's mic median is 3.005 s against at most 3 s, so the rule as written fails.

**The owner's decision, 2026-11-09, after the result:** accepted. The 5 ms over the 3 s limit was judged no real difference, and on a smaller card the mic preview falling back to the CPU is fine. This is a decision made after measuring, not a pass under the rule. The code was merged from `wip/T12.3a`: with transcription on CUDA, Everyone runs the mic preview on the CPU within `PREVIEW_BOTH_MIN_MEMORY`, and the General page says so.

## Previews on the CPU when the card is too small (T12.3c)

With transcription on CUDA and a card under `PREVIEW_MIN_CARD_MEMORY` (14 GiB, or unknown), `previews_refused` refused every preview: a third session on the card does not fit. T12.3c runs the preview sessions on the CPU instead, with the CPU's shared weights (one copy of the encoder for every CPU session), so the card holds what it holds with previews off. The far end's session (and the mic's in the room) and, under Everyone, the user's own as in T12.3a. The CPU's memory limits apply: `PREVIEW_MIN_MEMORY` (12 GiB) for the first session by default, lifted by "Other people only" and Everyone as on the CPU, and `PREVIEW_BOTH_MIN_MEMORY` (12 GiB) for the second. Automatic, no new setting; the page note replaces "too little memory" with where they run. The one rule is `backends::preview_backend` (`previews_refused` is its reason); a card with room keeps CUDA previews as before.

Cards that reach this: transcription stays off CUDA below `TRANSCRIPTION_MIN_CARD_MEMORY` (12 GiB), so a card of 12 GiB up to 14 GiB, or one whose memory cannot be read.

**How the small card is simulated.** The desktop's RTX 5080 has 16 GiB. `live_lag --card-memory-gib 12` (evaluation only) makes the preview rule read a machine whose every non-integrated graphics adapter has 12 GiB of dedicated memory: `earshot_runtime::hardware()` with only `dedicated_memory` replaced, all else as read. 12 GiB is the smallest card that transcribes on CUDA, the case this serves. The previews are then loaded through `backends::load_preview_on_hardware`, the production rule and loader on that machine instead of `crate::hardware()`: a separate named function, kept out of shipped code by `backends_preview_on_hardware_load_is_used_by_no_shipped_code`, never a flag on `load_preview`. Nothing else reads the simulated value: transcription runs on CUDA because `--backend cuda` asks for it, and the card's real memory is untouched, so the card reading in point 5 shows whether any preview session reached the card. The report records the simulated value (`simulated_card_gib`) and what the app would decide on that machine (`preview_refusal`, `app_first_on`).

**The rule, written and committed 2026-11-10 before the code was finished and before any measurement** (judgements, not derived; desktop, RTX 5080, transcription on CUDA, card simulated at 12 GiB in every run, idle host: no other lane, build or ci, not while the owner is in a meeting; bundled and continuous audio, real time). "Other people only" (`on`) and "Everyone" (`everyone`) are each judged against previews off on the same simulated machine (`off`, what a small card got before T12.3c), and each passes only if it passes on both audios:

1. **Where each session ran.** Both tracks' own sessions report CUDA from ONNX Runtime's placement (`live_backends`, `onnx_placement`); every preview session reports the CPU (`preview_sessions[].loaded.kind` and each track's `previews[].backend` `Cpu`); `preview_refusal.app_first` is null and `app_first_on` is `Cpu`. One control run, `on` on bundled without `--card-memory-gib`, must show the far-end preview on CUDA, so it is the simulated card that moved it. Otherwise the run says nothing about this design.
2. **Stored words are identical.** `stored_sha256` matches `off` for every track.
3. **The main transcript does not slow.** For each track, real decode rtf (`decodes_mic.rtf`, `decodes_reference.rtf`) at most 1.5 times `off`'s, and `catch_up_after_end_s` at most `off`'s plus 2 s. No audio dropped (`capture_max_lag_s` under 30 s, no preview `audio_dropped`).
4. **The previews are worth having.** `reference.on_screen_lag_s` median at most 3 s on both audios; under `everyone` also `mic.on_screen_lag_s` median at most 3 s on bundled (continuous has no mic speech) and the far-end median at most `on`'s plus 1 s. No preview turned off (`previews[].off` empty), at most 2 slow pauses per track.
5. **Memory.** Peak private memory (`host.peak_private_gib`) at most `off`'s plus 3 GiB for `on` and plus 3.5 GiB for `everyone`. The card's peak during the run (`nvidia-smi`, one reading a second) at most `off`'s plus 0.5 GiB for both. In `--memory`, the second CPU preview session adds at most 1 GiB of private memory over the first (the weights are shared).

If both pass, T12.3c ships as built. If `everyone` fails and `on` passes, the second session beside a small card goes back to refused (`second_preview_backend`), and the numbers and the failed point go here. If `on` fails, the first session goes back to refused too, as before T12.3c.

**Asked of the owner whatever the result, fixed before measuring:** the default memory limit. On CUDA a meeting's private memory was 11.7 GiB with previews off (before T9.1ma) and T12.3a's CPU session added 2.5 GiB, so `on` here is expected near 14 GiB of private memory, while `PREVIEW_MIN_MEMORY` (12 GiB) was set from a 5.8 GiB CPU meeting. The code uses the CPU's limit as asked; the measured peak goes to the owner with the question whether "automatic" beside a small card should use `PREVIEW_MIN_MEMORY_CUDA` (20 GiB) instead. A pass under the rule does not settle that.

**Method.** A CUDA build of the example (`--features onnx-cuda`), the models folder holding sets `ci` and `cuda` and the CUDA files the app downloads, cuDNN as for any CUDA run. Each replay is about 150 s (bundled) or 120 s (continuous) of real time. Beside each replay, in a second shell, log the card once a second and stop it with Ctrl+C when the replay ends:

```powershell
nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits -l 1 | Tee-Object -FilePath target\live-lag\t123c-on-bundled-smi.txt
```

(with the file name of the run beside it). The runs:

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --card-memory-gib 12 --audio bundled --previews off --out target\live-lag\t123c-off-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --card-memory-gib 12 --audio bundled --previews on --out target\live-lag\t123c-on-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --card-memory-gib 12 --audio bundled --previews everyone --out target\live-lag\t123c-everyone-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --card-memory-gib 12 --audio continuous --seconds 120 --previews off --out target\live-lag\t123c-off-continuous.json
```

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --card-memory-gib 12 --audio continuous --seconds 120 --previews on --out target\live-lag\t123c-on-continuous.json
```

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --card-memory-gib 12 --audio continuous --seconds 120 --previews everyone --out target\live-lag\t123c-everyone-continuous.json
```

The control for point 1 (no simulation: the far-end preview stays on CUDA):

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --audio bundled --previews on --out target\live-lag\t123c-control-on-bundled.json
```

Memory and load time per session: the track's own session on CUDA, then the first and second preview sessions where the rule puts them on the simulated machine (the CPU), each followed by three 12 s decodes:

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --card-memory-gib 12 --memory --out target\live-lag\t123c-memory.json
```

Reports and `nvidia-smi` logs go to `docs/live-previews/t123c/`, counts and times only.

## One CUDA session for a track's transcription and previews (T12.3b, spike)

ONNX Runtime's CUDA provider gives every session its own copy of the weights on the card (T9.1ma: "CUDA does not share"), so a track's own session and its preview session hold two copies of Parakeet 0.6B's encoder: about 3.7 GiB of graphics memory per preview session (9.1 against 12.8 GiB on the card, above). This spike asks whether one session can serve both. Evaluation only: nothing shipped changes, and what the app does with the answer is the owner's.

### What ort and ONNX Runtime allow (researched 2026-11-10)

- **A newer `ort` does not help.** The newest release is 2.0.0-rc.15 (crates.io, 2026-10-19; rc.14, which Earshot pins, is 2026-10-12). In both, `Session::run`, `run_with_options`, `run_binding` and `run_async` take `&mut self` (rc.14 `src/session/mod.rs`, rc.15 `src/session/run.rs`). The private `run_inner` takes `&self`, and its comment says ONNX Runtime's `Run` "is **not thread safe**" in practice whatever Microsoft says, that CUDA in particular crashes, and that the public functions stay `&mut self` until ONNX Runtime fixes it. `Session` is `Send + Sync`, citing microsoft/onnxruntime issue 114.
- **ONNX Runtime documents the opposite.** The high-level design page (onnxruntime.ai/docs/reference/high-level-design.html) says "Multiple threads can invoke the Run() method on the same inference session object", and in issue 114 a maintainer says it is safe with no external synchronisation. Later comments there report concurrency bugs with an execution provider (TensorRT) fixed release by release. So the documentation and the binding's author disagree, and the author has the crash reports.
- **How a CUDA session serves two threads** (ONNX Runtime `main`, read for this spike): `SessionState::AcquireDeviceStreamCollection` takes a stream collection out of a pool under `device_stream_pool_mutex_`, so concurrent `Run` calls get their own CUDA streams unless the provider uses one unified or user stream; the CUDA provider keeps a cuBLAS and cuDNN handle per thread (`PerThreadContext`, from a pool under a mutex), so each thread that runs the session adds its own handles and workspace on the card. Nothing in the provider serialises a whole `Run`; two `Run`s on two streams share the card's compute as two sessions already do.
- **An upgrade to rc.15 costs more than it buys here.** Its notes list breaking changes Earshot touches: `ExecutionProviderLibrary::unregister` is gone (`earshot_onnx::qnn` calls it after a failed QNN registration), `ThreadManager` must be `Send + Sync` (`StartingThreads`, the background preview threads), and `with_external_initializer_file_in_memory` takes `Arc<[u8]>`. Its library slot is now `util/once_lock.rs` rather than `once_lock_std.rs`, so the rc.14 workaround may be unneeded or wrong. It fixes "over 30 instances of undefined behavior", which is a reason to upgrade on its own, as a separate task.

### What a shared session looks like

**A lock around the encoder alone, and no `unsafe`.** Parakeet TDT is three sessions: the encoder (`encoder.onnx` with the 2.27 GiB `encoder.weights`, run once per 30 s chunk) and the small prediction network and joint (run once per token). Only the encoder is worth sharing. A preview gets its own prediction network and joint and borrows the track's encoder through `Arc<Mutex<OnnxSession>>`; a `MutexGuard` gives `&mut Session`, so `Session::run` is called one thread at a time, exactly as ort asks, with no `unsafe` and nothing for a reviewer of FFI. The cost is the wait: a real decode that wants the encoder while a preview holds it waits for the rest of that one encoder run (a 12 s window), never for the whole preview (its token loop runs on its own sessions, unlocked). The earlier "No" above was for a lock around a whole `Transcriber` call on the CPU, 0.5-1.6 s.

What it changes besides memory: the preview's encoder runs on the track's session, so its ONNX Runtime threads are the track's (normal priority), not the background ones; on CUDA they mostly wait for the card. The prediction network and joint stay on the preview's own background sessions. The CPU already shares weights (T9.1ma) and the NPU cannot (T12.4), so this is for CUDA only.

**Truly concurrent `Run` on one session** would need ort-sys's raw `Run` through `unsafe`, against the binding author's crash reports, and would gain little: two streams on one card already share its compute today. Not prototyped.

### The rule, written and committed 2026-11-10 before any code or measurement

Judgements, not derived. Desktop, RTX 5080, Parakeet 0.6B v2 on CUDA, idle host (no other lane, build or ci; not while the owner is in a meeting). The measurement is `live_lag --contention` (below): a track's real decodes on one thread and previews at the app's cadence on another, under three set-ups on the same audio: `alone` (the track's session, no previews), `own` (today: the preview on its own CUDA session, loaded as the app loads it) and `shared` (the preview borrowing the track's encoder). A real decode's added time is its wall time minus the same piece's time in `alone`. The spike passes only if every point holds:

1. **Where it ran.** Every session reports CUDA from ONNX Runtime's placement, and in `shared` the preview reports that it borrowed the track's encoder. Otherwise the run says nothing about this design.
2. **The same words.** Every real decode's words (text and times) in `own` and `shared` match `alone`'s piece by piece, and the shared preview's decode of a fixed window matches the track session's decode of it.
3. **No preview delays a real decode by more than a stated bound.** In `shared`: the longest a real decode waited for the encoder lock at most 250 ms; the median added time at most `own`'s median plus 30 ms; the largest added time at most `own`'s largest plus 250 ms. Why 250 ms: a whole preview decode of a 12 s window takes 0.1-0.18 s on CUDA (preview rtf 0.008-0.015, above) and the encoder is part of it, so 250 ms is one encoder run with room; it is a quarter of the 1 s preview interval, under 2% of the settled text's 14 s median lag, and an eighth of the +2 s catch-up that T12.3a and T12.3c allowed. Why 30 ms: about a fifth of a 15 s real decode on CUDA (0.165 s at rtf 0.011), where run-to-run noise sits.
4. **Previews stay worth having.** In `shared`, the median preview decode at most 1 s (the interval) and at most 1.5 times `own`'s.
5. **Card memory is saved.** The card's peak (`nvidia-smi`, once a second) in `shared` at most `alone`'s plus 0.75 GiB, against about 3.7 GiB for `own` today. The 0.75 GiB covers the preview's prediction network and joint and a second thread's cuBLAS and cuDNN workspace (a judgement).

**What the result means** (the owner decides; nothing ships from the spike):

- **All pass:** one session per track is possible. A follow-up task wires it into the live session (evaluation first) and is judged by a replay rule like T12.3c's (stored words identical, real decode rtf at most 1.5 times, catch-up at most +2 s, on-screen median at most 3 s, the card's peak). If that passes, the owner chooses between default on (CUDA previews borrow the track's encoder; `PREVIEW_MIN_CARD_MEMORY` re-judged, so 12-14 GiB cards keep previews on the card instead of T12.3c's CPU, and Everyone's mic preview could go back to CUDA instead of T12.3a's CPU), a setting, or default off.
- **Memory passes but point 3 or 4 fails:** default off, no setting proposed: T12.3a and T12.3c already keep previews off the card where it is short.
- **Point 5 fails:** sharing saves nothing worth having; nothing changes.
- **Point 1 or 2 fails:** the run says nothing; find out why before judging.

## Re-measuring on a quiet host

Never beside ci or a build. `bundled` and `continuous`, off and on; for CUDA add `--features onnx-cuda`
after `-p earshot-runtime` and `--backend cuda` after `.`.

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews off --out target\live-lag\off-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews on --out target\live-lag\on-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio continuous --seconds 120 --previews off --out target\live-lag\off-continuous.json
```

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio continuous --seconds 120 --previews on --out target\live-lag\on-continuous.json
```

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --memory --out target\live-lag\memory.json
```

Each report holds counts and times only. `host.machine_load_percent` says what the run saw, and matching
`stored_sha256` between an on and an off run says the same words were stored. On CUDA, read the card's
peak with `nvidia-smi --query-gpu=memory.used --format=csv,noheader` during the run.
