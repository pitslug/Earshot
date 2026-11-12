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

The app refused previews on the NPU here (`backends::previews_refused`); since T12.12c it runs them for the other people (section "Previews on the NPU from the compiled model" below). To measure them on the Surface, `live_lag` takes `--backend qnn` and an evaluation-only `--force-previews`, which loads the preview sessions through `backends::load_preview_unrefused`: the same load as the app's (background threads, the NPU conversion, `check_qnn_parakeet`), without the refusal. Nothing the app or `host::live_previews` decides changes; `backends_preview_unrefused_load_is_used_by_no_shipped_code` keeps it out of shipped code.

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

**Decision (2026-11-09): both settings stay refused on the NPU** (`previews_refused` and `previews_refused_second` unchanged). Even with previews off, a call's two NPU sessions leave about 1.1 GiB available on the Surface before the summary model loads. Precompiled NPU models (T12.12) are the lead for less memory per session; if a session drops well below 2 GiB, this is re-judged under the same rule. (T12.12 found about 0.13 GiB per extra session from a compiled context model: re-judge once the app loads that way, T12.12a.)

**Sharing one copy of the model between NPU sessions: not measurable with how Earshot loads, so there is no `--share-npu`.** ONNX Runtime's key is `ep.share_ep_contexts` (`kOrtSessionOptionShareEpContexts`, "Share EP related resources across sessions", in `include/onnxruntime/core/session/onnxruntime_session_options_config_keys.h`, beside `ep.stop_share_ep_contexts` and `ep.context_enable`). In the QNN plugin Earshot loads (onnxruntime-qnn tag `v2.7.0`, `onnxruntime/core/providers/qnn/qnn_execution_provider.cc` and `shared_context.h`):

1. Sharing works only for EP context models (a model holding `EPContext` nodes that point at a compiled QNN context binary, made with `ep.context_enable=1`): `GetCapability` takes the shared path only under `is_qnn_ctx_model && share_ep_contexts_`, and `Compile` looks graphs up in `SharedContext` only in `CompileContextModel`. Earshot loads the QDQ ONNX conversion and the plugin compiles it at every load, so the key would do nothing.
2. Even with EP context models it does not let identical sessions share: `SharedContext::GetSharedQnnModel` moves the graph out of the shared list and erases it, so each graph name is taken by one session. It is made for one context binary holding several different graphs (QNN weight sharing across graphs), not for three sessions of one graph.
3. The plugin's own `htp_share_resource_optimization=1` provider option shares the `QnnBackendManager` (backend handle and VTCM backup buffers) between sessions, not the weights; its exact provider-option key through the plugin interface is not confirmed here.

So the NPU sessions of an Everyone meeting (two tracks' own and two previews) each hold their own compiled graph; `--memory` measures what that costs. Making them share would need a different design (one session serving several tracks, or a context binary with one graph per session) and is a separate task.

## NPU sessions from compiled context models (T12.12, 2026-11-11)

**Question:** can a compiled QNN context model (`ep.context_enable=1`: the plugin writes an ONNX model with one `EPContext` node pointing at a compiled context binary) bring an NPU session below T12.4's 2.0 GiB and 29-31 s? Measured with evaluation-only loaders (since T12.12a the shipped `OnnxSession::load_writing_context` and `ParakeetTdt::load_with_encoder`) and the probe `crates/earshot-onnx/examples/qnn_context_probe.rs` (its `ctx-share` mode was removed with them):

```powershell
cargo run --release -p earshot-onnx --features qnn --example qnn_context_probe -- <models>\parakeet-qnn\parakeet-tdt-0.6b-v2 target\t1212\v2 compile
```

then `qdq`, `ctx` or `ctx-share` instead of `compile`, with `--sessions N` and the smoke FLACs, each mode in its own process. Surface, mains, plugin v2.7.0 (QNN SDK 2.51.0). Free memory at the start was only 2.2-3.9 GiB of 15.5 (the Claude app, AdGuard and Defender held about 6 GiB), so free memory was read beside each load and fewer of today's sessions were loaded.

**Compiling** takes as long as one of today's loads (0.6B v2 31.7 s, 110M 17.4 s) and writes `encoder_ctx.onnx` (a few KiB, the Parakeet metadata kept, so the loader's checks pass unchanged) and `encoder_ctx_qnn.bin` (0.6B v2 600 MiB, 110M 119 MiB). The model records `ep_sdk_version` `v2.51.0...` and `ep_compatibility_info.QNNExecutionProvider` `v2:6:2.51.0:5.51.0:81:0:0:0`.

| Parakeet 0.6B v2 | Load | Private memory after | Free memory dropped by |
|---|---|---|---|
| Today (`qdq`), session 1 | 28.9 s | 2.17 GiB | 2.64 GiB |
| Context model, session 1 | 0.7 s | 0.07 GiB | 0.63 GiB |
| Context model, sessions 2 and 3 | 0.2 s each | +0.03-0.05 GiB each | 0.12-0.14 GiB each |
| Context model with `ep.share_ep_contexts=1` | 0.8 / 0.3 / 0.2 s | the same | the same |

Parakeet 110M: today 17.2 s and about 0.8-1.0 GiB of private memory per session; from the context model 0.5 s then 0.1 s, 0.06 GiB then +0.02 GiB.

- **Same output.** On all 20 smoke clips (162 words) the context model's words equal today's in text, times and probability to six decimals, for both models, and every session of a mode equals its first. Decode time per clip is unchanged (0.15-0.17 s). Every session kept the NPU partition (`check_qnn_partition` Ok).
- **Where the memory went.** Today the plugin compiles the QDQ graph at each load and keeps the QDQ initializers and the compiled graph in the process. From a context model it maps the binary: about 0.6 GiB of the machine's memory for the first session (the binary's size), then about 0.13 GiB per extra session.
- **`ep.share_ep_contexts` does nothing here**, as T12.4 read from the plugin source: each session still takes its own graph from the binary.

**What a shipped version needs** (the brief for T12.12a; what was built is in the next section):

- **Where it comes from:** compiled on the user's own computer, once per model, the first time the NPU conversion is used, or after its download. It costs one ordinary load (about 30 s), with no new download source and nothing published per chip. It is our own file, so the file a load reads must be checked like any model file: record its SHA-256 when written, and verify it before every load.
- **What invalidates it:** a different `encoder.onnx` or `encoder.weights` (manifest digests), a different QNN plugin or `QnnHtp*.dll` (the pins in `QNN_ARM64`), or a different `ep_compatibility_info` (SDK and chip), plus the NPU driver version for safety. Any mismatch, or a load the plugin refuses, falls back to today's load and compiles again: saving memory never costs a meeting.
- **Where it lives:** beside the conversion (`parakeet-qnn/<model>/compiled/`), removed with the model, and never a recording's data.
- **What it changes:** a call's two NPU sessions would hold about 0.8 GiB of the machine's memory instead of about 5.3 GiB, and load in about a second instead of a minute. That is "well below 2 GiB", so T12.4's rule (both settings, unchanged) is to be run again once the app loads this way.

**Recommendation:** load the NPU encoder from a context model compiled on the computer (task T12.12a, with the owner's go-ahead), then re-run T12.4's rule. The memory gain matters even with previews off: today a call on the Surface leaves about 1 GiB free before the summary model loads.

## Loading compiled NPU models in the app (T12.12a, stopped 2026-11-11)

Built on `wip/T12.12a` (`earshot_runtime::npu_compiled`, the AGENTS.md bullet there), not merged. On the Surface the first load compiled and kept the context model (25 s), the next loaded it in 1.2 s with exactly the words, times and probabilities of the ordinary load, and a damaged binary was refused, compiled again and noted. Two findings:

- **Plugin v2.7.0's own compatibility check always answers "prefer recompilation"** for its own compiles: it writes the VTCM size as 0 ("unset") into `ep_compatibility_info` (`v2:6:2.51.0:5.51.0:81:0:0:0`) and calls only an equal VTCM optimal (`cache_compatibility/qnn_cache_compatibility_manager.cc`). Only "unsupported" can be a reason to compile again.
- **A blocker: loading a context model crashes the process (0xC0000005) once any QNN session has been dropped**, unless an ordinary (compiling) QNN load came after that drop. Measured in single processes with Parakeet 110M:

| Sequence in one process | Result |
|---|---|
| context model, a second beside it | fine |
| context model, drop it, context model | crash |
| context model kept, a second dropped, a third | crash |
| ordinary load, drop it, context model | crash |
| context model, drop it, ordinary load kept, context model beside it | fine |
| the same crashing sequences with `QnnSystem.dll` or `QnnHtp.dll` held loaded, or `disable_file_mapped_weights=1` | crash |

In the app every meeting drops its NPU sessions at the end, so the next meeting's first compiled load would end the process, recording included. T12.12 never saw it because each probe ran its loads in a fresh process without drops.

**The way round it (T12.12b, the owner's go-ahead 2026-11-11): never close a QNN session.** Every crashing sequence above has a close; none without one crashed. So a dropped QNN session goes to an idle pool for the life of the process, keyed by model file and session settings, and the next load of the same takes it back (about 1.4 s, most of it checking the files and loading the CPU step sessions). The compile, an ordinary load whose session would otherwise be closed, runs in a helper: the app's, the CLI's and `live_lag`'s own exe in the hidden `--npu-compile` mode, which writes the context model and exits. Measured on the Surface:

- Real-NPU test (110M, compiling in process as tests do): compile 25 s, compiled load 1.2-1.8 s, five close-and-reload rounds from the pool without a crash, a damaged copy refused, compiled again and noted; words, times and probabilities equal to the ordinary load every time.
- CLI with the helper: the first `transcribe` on the NPU 29.9 s whole run for 110M (56 s for 0.6B v2), the second 2.8 s (7.3 s), the same words.
- Stress run (`npu_compiled_stress_many_meetings_in_one_process`, 0.6B v2): 25 meetings in one process, each with both tracks' sessions and the preview open at once (the preview closed and loaded again in every third, off in every fourth), a final-pass session after them, everything closed at the end: no crash in 250 s, every load from the compiled model, every decode equal to the first. The pool held 4 sessions (two tracks, the preview, the final pass, which use different settings) and did not grow. An earlier run without the meeting's sessions held open together (they could take turns) also passed, 25 meetings in 489 s.

**Compiled at start-up** (the owner's suggestion): on ARM64 the app compiles, in the helper and on a thread of its own, the NPU model a meeting would load now (`host::prepare_npu_model`) as soon as it opens; a meeting that starts meanwhile records at once and its load waits for the compile, then takes the compiled model, so no meeting compiles twice. A compile is needed only once per model per machine and after a conversion, plugin, ONNX Runtime or NPU driver change. If the automatic model check (it starts after a transcription model's download) runs at the same time, its timings are taken beside a compile.

**Idle battery** (`npu_compiled_idle_battery`, 2026-11-11, the Surface unplugged, screen on, the Claude app open, 10 min of 15 s samples of Windows' `BatteryStatus.DischargeRate` after a 1 min settle): no session loaded 6,341 mW mean, two 0.6B v2 sessions held idle 5,790 mW. The run with sessions drew less, so the difference is inside the noise of the rest of the machine: no cost measurable this way.

What it costs: the pool holds every session a meeting used while Earshot stays open (in the tray too), and their files stay mapped, so a compiled folder in use cannot be replaced until Earshot closes. A plugin release that fixes the crash would let the pool go.

## Previews on the NPU from the compiled model (T12.12c)

**T12.4's rule run again (2026-11-12, the Surface, mains, the rule unchanged).** With the app loading NPU encoders from the compiled model (T12.12a, T12.12b), "on" and "everyone" were judged against "off" on `bundled` and `continuous`, with `--force-previews` (the app still refused previews there). Reports, copied unmodified: `qnn2-off-bundled.json`, `qnn2-on-bundled.json`, `qnn2-everyone-bundled.json`, the same three `-continuous`, `qnn2-memory.json` and `qnn3-memory.json` (the memory run again on a freshly restarted Surface), eight files. Every NPU session, the previews' included, ran its encoder on `QNNExecutionProvider`; machine load during the runs 19-29% (median).

| | Off | Other people only | Everyone |
|---|---|---|---|
| 1. Every session on the NPU | yes | yes | yes |
| 2. Stored words equal "off", bundled and continuous | - | yes | yes |
| 3. Real decode rtf, bundled mic / reference | 0.021 / 0.021 | 0.022 / 0.021 (1.05x / 1.03x) | 0.023 / 0.021 (1.08x / 1.01x) |
| 3. Real decode rtf, continuous reference | 0.012 | 0.012 (1.01x) | 0.012 (0.99x) |
| 3. Catch-up after the end, bundled / continuous | 0.43 / 0.48 s | 0.42 / 0.49 s | 0.41 / 0.46 s |
| 4. Far-end words on screen, median, bundled / continuous | 14.1 / 36.9 s | **2.7 / 1.9 s** | **2.3 / 1.9 s** |
| 4. Mic words on screen, median, bundled | 15.4 s | 15.4 s | **2.5 s** |
| 4. Previews turned off, slow pauses, audio dropped | - | none, 0, 0 | none, 0, 0 |
| Peak working set, bundled / continuous | 3.00 / 3.11 GiB | 3.77 / 3.33 GiB | 4.47 / 3.96 GiB |

Points 1 to 4 pass for both settings, by wide margins: the compiled sessions (T12.12) removed the NPU contention T12.4 saw.

**Point 5 as written fails, and would fail with previews off.** `qnn3-memory.json`, from a fresh restart: 4.90 GiB free with no session, 4.60 after a track's own session, 4.07 after a second session (3.75 after its decodes), 2.16 after a third. A call holds two track sessions, so with previews off the Surface is already at about 3.75 GiB free, under the 4 GiB the point asks for; "on" adds one session and "everyone" two. Each NPU session costs about 0.63 GiB of working set (the stages' `peak_working_set_gib`: 0.69, 1.32, 1.95). The point measured the Surface's idle memory, not what previews cost.

**Amended (the owner, 2026-11-12, after seeing these numbers, not before):** point 5 becomes what previews add over "off" on the same audio, measured as the replay's peak working set: at most 1 GiB for "Other people only" and 1.5 GiB for "Everyone". Read on the reports: "on" adds 0.77 GiB (bundled) and 0.22 GiB (continuous); "everyone" adds 1.47 and 0.85 GiB. Both pass the amended point ("everyone" by 0.03 GiB on bundled). This amendment was made after the measurement, so it is a decision on these numbers, not a rule fixed before them.

**Decision (the owner, 2026-11-12):** "Other people only" runs on the NPU. "Everyone" stays refused there: on `bundled` 52 of the 236 provisional words of the user's own voice were shown and then vanished (against 7 of 201 on the far end), and that is to be understood first (step 4 below).

**What the app does now** (`backends::preview_backend`, `second_preview_backend`, `qnn_load_steps`):

- A track on the NPU has its previews' session on the NPU. By default only with `PREVIEW_MIN_MEMORY_NPU` (12 GiB, a judgement: a session added at most 0.77 GiB to a meeting here, a 16 GB Surface reports about 15.5 GiB and must pass, an 8 GB machine has little room); "Other people only" or "Everyone" chosen by the user lifts it, as on the CPU. Off on battery, during a hold and its catch-up, and for a track that falls behind, as everywhere.
- A preview on the NPU loads only the compiled model: it may have the helper process compile one, but never compiles in the app's own process and never takes the ordinary load (about 2 GiB and 30 s, held for the process's life because a QNN session is never closed). A preview that cannot load that way is `PreviewReport.off` and the meeting goes on without previews.
- The user's own voice in a call has no preview session beside the NPU (`OWN_VOICE_NPU_REFUSED`); the Settings page shows "Everyone" with a note saying it shows the other people only on the NPU and why.
- A Parakeet model whose NPU version is not installed runs its tracks on the processor (`npu_order`), and its previews follow the track to the CPU's rule.

**Vanished words of the user's own voice (step 4, evaluation only, 2026-11-13).** The question: are the 52 vanished words the live echo filter removing far-end words (expected), or words the settled text never held? `live_lag` now counts, for every preview word that left with no settled text at it, whether the track's own settled text holds the same word within 2 s and whether that settled word had already arrived when it left (`own_settled_before`) or came later (`own_settled_after`), else whether the other track's settled text holds it (`other_track_settled_nearby`), else `never_settled`; beside it the live echo filter's `echo_words_removed` and `echo_locks`. The 2 s window was fixed before the run (the echo filter's delay plus or minus 1 s, and preview word times moving between decodes). The pattern is not NPU-specific (T9.14's CPU run already had 42 of 221 vanished on the mic), so it was run on the desktop CPU, `--audio bundled --previews everyone`, three times (reports `t1212c-everyone-bundled-cpu.json`, `-2`, `-3`; counts only; other lanes were building, machine load median 12%, 6% and 1%):

| Run | Mic preview words vanished | Settled on the mic later, within 2 s | Already settled when they left | Far end only | Never settled | Echo filter removed |
|---|---|---|---|---|---|---|
| 1 (load up to 65%) | 0 (the far end's previews turned off: fell behind) | - | - | - | - | 0 |
| 2 | 52 | 52 in all (this run counted before the split into later and already) | - | 0 | 0 | 0 |
| 3 | 40 | 40 | 0 | 0 | 0 | 0 |

Stored words were identical in all three (`stored_sha256` equal).

**Reading:** they are neither. No vanished word belongs to the far end only and the echo filter removed nothing on this meeting (`echo_words_removed` 0, with one echo lock). Every vanished word reached the mic's settled text within 2 s of where the preview showed it, after it had left: the preview showed a real word, took it away, and the settled text brought it back a moment later. That is a flicker, not lost or wrong words. The module promises that settled text lands before the words it replaces leave (`live::preview`), so something takes words off screen before their stretch's decode lands; the far end (7 of 201 on the Surface, 1 here) shows it far less than the mic, whose stretches are short and many (11 stretches, median 5 s, on `bundled`).

**Recommendation:** keep "Everyone" refused on the NPU for now, as decided, and add a task to find what takes a shown word away before its settled text lands on short stretches (a `live_lag` count of which preview update dropped it: a new window no longer holding it, or the frontier moving past it) and fix it; it applies on every backend, so it is a fix to `live::preview`, not to the NPU. Once vanished words are near the far end's rate, "Everyone" on the NPU can be judged again on the amended point 5 (it passed points 1 to 5 here).

**Checked by hand on the Surface (step 5, the owner).** From the branch, natively on the Surface, no `--force-previews`:

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews on --out target\live-lag\qnn4-on-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio continuous --seconds 120 --previews on --out target\live-lag\qnn4-on-continuous.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews everyone --out target\live-lag\qnn4-everyone-bundled.json
```

Read: in the first two, `preview_sessions[0].loaded.kind` is `Qnn` with `refusal_bypassed` false, and `preview_refusal.app_first` is null; in the third, the `own_voice` entry has `not_loaded` saying it is not for your own voice on the NPU yet, and the far end's still loads on QNN. Then one real call: words of the other side on screen within about 3 s.

**The Surface check without `--force-previews`, 2026-11-13** (this branch built natively, `--backend qnn`, mains; reports `qnn4-*.json` here, counts and times only; one run each, other user apps open):

- `--previews on`, `bundled`: the far-end preview loaded on QNN through the app's own loader (`preview_sessions[0]`: `loaded.kind` Qnn, `refusal_bypassed` false; `preview_refusal.app_first` null, `app_first_on` Qnn). Far-end words on screen median 2.3 s, p95 7.8 s (settled 14.1 / 37.0 s); 190 kept, 9 changed, 1 vanished; no slow pause, nothing dropped; track rtf 0.021 / 0.021, catch-up 0.41 s, peak private 2.71 GiB.
- `--previews on`, `continuous --seconds 120`: loaded on QNN the same way; on screen median 1.8 s, p95 3.5 s (settled 36.9 / 49.6 s); 211 kept, 7 changed, 0 vanished; catch-up 0.48 s.
- `--previews everyone`, `bundled`: the far end loaded on QNN as above; the mic preview was not loaded, `not_loaded` and `preview_refusal.app_own_voice` both the own-voice reason ("not for your own voice on the NPU yet: some words shown there vanished again, and why is not understood"); mic words reached the screen only as settled text (15.5 / 32.3 s).

So the automated part of step 5 holds. The real call with words on screen within about 3 s is still the owner's.

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

### Result (measured 2026-11-11, idle desktop, one run per condition)

Reports and card logs: `docs/live-previews/t123c/`. Off is previews off on the simulated 12 GiB card.

| Condition | Stored words | Main rtf vs off | Catch-up (s) | Far-end on-screen median (s) | Own on-screen median (s) | Private peak (GiB) | Card peak (GiB) |
|---|---|---|---|---|---|---|---|
| off, bundled | | | 0.47 | | | 11.71 | 9.09 |
| on, bundled | same | mic 0.89x, ref 0.99x | 0.38 | **3.03** | | 14.25 (+2.54) | 9.09 (+0.00) |
| everyone, bundled | same | mic 0.88x, ref 1.03x | 0.43 | **3.53** | **3.30** | 14.54 (+2.83) | 9.11 (+0.01) |
| off, continuous | | | 0.50 | | | 11.11 | 8.70 |
| on, continuous | same | ref 0.99x | 0.42 | 2.43 | | 13.65 (+2.54) | 8.70 (+0.00) |
| everyone, continuous | same | ref 1.01x | 0.46 | 2.48 | | 13.79 (+2.68) | 8.71 (+0.01) |

By point: 1 passes (both tracks' sessions on CUDA, every preview session on the CPU, `app_first_on` Cpu; the control without the simulation put the far-end preview on CUDA). 2 passes (stored words identical). 3 passes (rtf at most 1.03 times off's, catch-up under off's, capture lag at most 0.03 s, nothing dropped). 4 **fails**: on bundled audio the far-end on-screen median is 3.03 s for `on` and 3.53 s for `everyone`, and the user's own 3.30 s, against 3 s; no preview turned off and no slow pauses. 5 passes (private memory +2.54 and +2.83 GiB, card +0.01 GiB, the second CPU session +0.26 GiB in `--memory`).

**Decision under the rule:** `on` fails, so beside a small card the first session goes back to refused, as before T12.3c (commit "T12.3c: refuse previews beside a small card again"); the evaluation tooling stays. `on` missed by 30 ms on one audio, as T12.3a missed by 5 ms; whether to accept it, and the 12 vs 20 GiB question above, is the owner's.

**The owner's decision, 2026-11-11:** accepted, as T12.3a's 5 ms was; T12.3c ships as built (the refusal commit reverted), with the 12 GiB limit as built.

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

### The prototype (written after the rule)

- `earshot_onnx::parakeet::SharedEncoder`, `ParakeetTdt::share_encoder` and `ParakeetTdt::load_sharing`: the encoder field of `ParakeetTdt` is its own session or a borrowed one; a model with its own encoder runs exactly the calls it ran before (the encoder call moved into `run_encoder`, unchanged), and a borrowing one takes the lock for each encoder call and counts how long it waited (`take_encoder_waits`).
- `earshot_runtime::backends::load_preview_sharing_encoder` (evaluation only, CUDA only): the track's session as `load_parakeet` loads it, made shareable, and a preview borrowing its encoder, with its own prediction network and joint on the preview's background threads. No shipped code names any of these (`backends_preview_sharing_encoder_is_used_by_no_shipped_code`).
- `live_lag --contention alone|own|shared` (CUDA only): loads the set-up, decodes a fixed window with the track and the preview (point 2's second half), warms every piece length up untimed, then decodes the reference track in 15 s pieces three times, 0.5 s apart, while a preview thread decodes a 12 s window moving 1 s at a time and waits the longer of 1 s and its own decode. It reports every real decode (`real_decodes[]`: `pass`, `piece`, `took_s`, `encoder_wait_s`, `words_sha256`), `real_took_s`, `real_encoder_wait_s`, the previews' decode times and waits, every session's placement and `encoder_users`, and this process's memory. Counts, times and hashes only.
- Checked on the CPU only (no card run in this spike): `parakeet_tdt_shared_encoder_gives_the_same_words` (ignored, `ci` set) shows a borrowing model gives the same words as the one that shared it, alone and with both decoding at once on two threads, and the TDT smoke test still passes with the refactored encoder call.

### Method

A CUDA build of the example (`--features onnx-cuda`), the models folder holding sets `ci` and `cuda` and the CUDA files the app downloads, cuDNN as for any CUDA run (the AGENTS.md CUDA 13.4/13.5 rule if building from an agent shell). Beside each run, in a second shell, log the card once a second and stop it with Ctrl+C when the run ends:

```powershell
nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits -l 1 | Tee-Object -FilePath target\live-lag\t123b-shared-1-smi.txt
```

(with the file name of the run beside it). Each set-up twice, interleaved so drift spreads evenly: alone, own, shared, shared, own, alone (`-1` and `-2` in the file names):

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --contention alone --out target\live-lag\t123b-alone-1.json
```

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --contention own --out target\live-lag\t123b-own-1.json
```

```powershell
cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- . --backend cuda --contention shared --out target\live-lag\t123b-shared-1.json
```

Then the same three in reverse order with `-2`. The added time of each real decode is its `took_s` minus `alone`'s for the same `pass` and `piece` (the same run number):

```powershell
$a = Get-Content target\live-lag\t123b-alone-1.json | ConvertFrom-Json; $s = Get-Content target\live-lag\t123b-shared-1.json | ConvertFrom-Json; $added = for ($i = 0; $i -lt $a.real_decodes.Count; $i++) { $s.real_decodes[$i].took_s - $a.real_decodes[$i].took_s }; $added | Sort-Object
```

What to read, by point: 1, `placements` (every entry `CUDAExecutionProvider`) and `encoder_users` 3 in `shared`; 2, `real_decodes[].words_sha256` equal to `alone`'s piece by piece and `fixed_window.preview_matches_track` true; 3, `real_encoder_wait_s.max` and the added times against `own`'s; 4, `previews.took_s.median`; 5, the largest reading in each `nvidia-smi` log, `shared` against `alone`. A point that holds in one run of a pair and not the other is reported as such, not averaged away. Reports and logs go to `docs/live-previews/t123b/`, counts and times only.

### Provisional reading (before any measurement)

Not a result. Point 5 should pass by construction (one encoder instead of two), and point 3 depends on how long one encoder call on a 12 s window takes on the card, which no report here isolates. If the spike passes, the replay task above is what can change a default.

### Result (measured 2026-11-11, idle desktop, two interleaved runs)

Reports and card logs: `docs/live-previews/t123b/`. Added time is a real decode's `took_s` minus `alone`'s for the same pass and piece.

| Run | Same words | Longest lock wait (ms) | Added median (ms) | Added largest (ms) | Preview median (s) | Card peak (GiB) |
|---|---|---|---|---|---|---|
| alone 1 / 2 | | | | | | 5.18 / 5.17 |
| own 1 / 2 | yes / yes | | -0.3 / -0.7 | 156 / 172 | 0.072 / 0.067 | 8.90 / 8.90 |
| shared 1 / 2 | yes / yes | 40 / 43 | 21.4 / 12.8 | 111 / 91 | 0.097 / 0.097 | 5.77 / 5.76 |

By point, both runs: 1 passes (every session placed on CUDA, ONNX Runtime's usual CPU shape nodes aside; `encoder_users` 3). 2 passes (every real decode's words equal `alone`'s; the fixed window's preview matches the track). 3 passes (wait at most 43 ms against 250; added median within `own`'s plus 30 ms; largest under `own`'s plus 250 ms). 4 passes (preview median 0.097 s, under 1 s and under 1.5 times `own`'s 0.067-0.072). 5 passes (card peak 0.59 GiB over `alone`, against the 0.75 bound; about 3.1 GiB less than `own`).

**Under the rule:** all pass, so the next step is a follow-up replay task (a T12.3c-style rule on whole meetings) before the owner chooses default on, a setting or default off. No shipped change.

## Provisional words from a streaming model (T12.23, spike, evaluation only)

Today's previews decode the last 12 s of a stretch again every second with a second copy of the transcription model. A cache-aware streaming model reads each new piece of audio once. This spike measures one: NVIDIA Nemotron Speech Streaming 0.6B English (cache-aware FastConformer RNNT), sherpa-onnx's 560 ms int8 export (manifest set `stream-eval`, 662 MB). The NVIDIA Open Model License is agreed by the owner for evaluation only (2026-11-12): nothing ships, and no shipped crate names the model (`backends_nemotron_preview_load_is_used_by_no_shipped_code`).

**The decoder.** `earshot_onnx::nemotron` ports sherpa-onnx v2.0.1's streaming NeMo transducer step for step: kaldi-native-fbank's features with sherpa-onnx's NeMo options, 65 feature frames per chunk moving 56 (560 ms), the encoder's three cache tensors carried between chunks, greedy RNNT with the LSTM state and the last prediction carried over. Checked by `cargo test -p earshot-onnx --example nemotron_probe -- --include-ignored` against sherpa-onnx's own Python package (2.0.1, hash-pinned wheels in a throwaway venv) on the 20 smoke clips:

- **Text:** the same text and every token the same on all 20 clips (306 tokens), whole and pushed in uneven pieces as a live session pushes it.
- **Token times:** 31 of 306 differ, by up to 3 encoder frames (240 ms). The reference itself moves times as much: the same package run on Earshot's pinned ONNX Runtime 1.31.0 instead of its own 1.28.2 changes 24 of 286 token times by up to 3 frames, and one comma. Given kaldi-native-fbank's own features (`nemotron_probe --decode-features`), the port gives sherpa-onnx-on-1.31.0's tokens and times exactly on every clip; the port's front end differs from kaldi-native-fbank's by float rounding in the FFT (at most 2.6e-4 in a log-mel value), which is what moves a few times.

**In a live session.** `LivePreviews::stream` (evaluation only, `None` in the app) gives each previewed track a streaming model instead of window decodes. The track's preview thread feeds it every packet of the track's pipeline audio once (quantised as the FLAC stores it), at below-normal priority on background ONNX Runtime threads, and shows the words it has finished; everything else is today's: which tracks have previews ("Other people only": the far end in a call; "Everyone": the mic too), when they start and stop, what settled text removes, the events, the report. A word is shown once a whole chunk (7 encoder frames, 560 ms) has been decoded after its last token with nothing added, because a later token could still continue it; so a shown word never changes. A gap in the audio (dropped packets) starts the model over. The model runs on the CPU whatever the track's backend, with `--stream-threads` threads.

### The rule, written and committed 2026-11-13 before any measurement

Judgements, not derived. Idle host only (no other lane, build, ci or measurement; not while the owner is in a meeting; plugged in). Parakeet 0.6B v2 transcribes the tracks as in the app. "Streaming" is a run with `--stream nemotron`; "window" is today's previews with the same choice on the same machine and backend; "off" is previews off. Each of "Other people only" (`on`) and "Everyone" (`everyone`) passes only if every point holds on both `bundled` and `continuous`:

1. **Where it ran.** Every streaming session's three ONNX Runtime sessions are on `CPUExecutionProvider` (`preview_sessions[].placement`), each previewed track's `previews[].backend` is `Cpu`, and both tracks' own sessions are where `--backend` asked (`live_backends`, `onnx_placement`). Otherwise the run says nothing about this design.
2. **Stored words are identical.** `stored_sha256` equals "off"'s for every track.
3. **The main transcript does not slow** (T12.4's point 3). For each track, real decode rtf at most 1.5 times "off"'s and `catch_up_after_end_s` at most "off"'s plus 2 s; no audio dropped (`previews[].audio_dropped` 0), no preview turned off (`previews[].off` empty) and at most 2 slow pauses per track.
4. **The words come sooner, or no later.** On each previewed track, `on_screen_lag_s` median at most 3 s and at most window's median plus 0.5 s, and its p95 at most window's p95.
5. **The words are as good.** On each previewed track, `previews.changed / (previews.kept + previews.changed)` at most 10% and at most twice window's; `previews.vanished` is reported beside it. (The streaming model is not the model that settles the text, so some changes are its own word choices rather than mistakes; the bound allows for that.)
6. **It costs less.** Process CPU time (`host.process_cpu_s`) at most window's, and peak private memory (`host.peak_private_gib`) at most window's. Where window previews are refused (on the Surface, "Everyone" on the NPU), cost is judged against "off" with T12.12c's limits instead: peak private memory at most 1 GiB over "off" for `on` and 1.5 GiB for `everyone`, process CPU time reported; points 4 and 5 then use their absolute bounds only (median at most 3 s, changed at most 10%).

**Desktop**: transcription on the CPU, `--stream-threads 2` (sherpa-onnx's default; fixed before measuring). **Surface**: transcription on the NPU (`--backend qnn`), the streaming model on 2, 3 and 4 threads, each count judged on its own; the smallest count that passes is the one to recommend. Window previews on the NPU are those of T12.12c.

**What the result means** (the owner decides; nothing ships from the spike). If `on` passes on the Surface: a follow-up task to ship streaming previews needs the model's licence decided for shipping (today evaluation only), `preview.rs` fed by the stream (the seam exists), the memory limits in `previews_refused` re-derived, a download of about 660 MB, and an answer for whisper users (the streaming model is English only and independent of the transcription model, so it could serve them too). If it passes only on the desktop: report it; the Surface is what decides. If it fails point 4 or 5: the window previews stay. A fixed-chunk NPU conversion of the encoder (PLAN step 4) is considered only if the CPU result fails on the Surface for cost (point 3 or 6) while points 4 and 5 hold.

### Method

The models folder needs sets `ci` and `stream-eval` (and on the Surface `qnn` and `qnn-models`): `cargo xtask fetch-models --set stream-eval`. Each run is about 150 s (`bundled`) or 120 s (`continuous`) of real time. Reports go to `docs/live-previews/t1223/`, counts and times only. Desktop, CPU (run "off" and "window" too unless an idle-host report of the same commit exists):

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews off --out target\live-lag\t1223-off-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews on --out target\live-lag\t1223-window-on-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews on --stream nemotron --stream-threads 2 --out target\live-lag\t1223-stream-on-bundled.json
```

Then the last two with `--previews everyone` (`-everyone-` in the file names), and all of them with `--audio continuous --seconds 120` (`-continuous`). Surface: add `--features qnn` after `-p earshot-runtime` and `--backend qnn` after `.`, and run the streaming runs with `--stream-threads 2`, `3` and `4` (`-t2`, `-t3`, `-t4` in the file names).

What to read, by point: 1, `preview_sessions[]` (`slot`, `stream`, `placement`) and `previews[].backend`; 2, `reference.stored_sha256` and `mic.stored_sha256`; 3, `decodes_mic.rtf`, `decodes_reference.rtf`, `catch_up_after_end_s`, `previews[]` (`audio_dropped`, `off`, `slow_pauses`); 4, `reference.on_screen_lag_s` and under `everyone` `mic.on_screen_lag_s`; 5, `reference.previews` and `mic.previews` (`kept`, `changed`, `vanished`); 6, `host.process_cpu_s` and `host.peak_private_gib`. A point that holds on one audio and not the other fails.

### Result on the desktop (2026-11-13)

Run by the coordinator on an idle desktop (machine load median 2-4% in every report; no build, test or other measurement running), transcription on the CPU, `--stream-threads 2`, after lane/nemotron's fix commit dec19c6. Reports in `docs/live-previews/t1223/`, unmodified. The rule above was not changed.

| Audio, choice | 1 where | 2 stored | 3 main transcript | 4 lag (median; p95) streaming vs window | 5 changed, streaming vs window | 6 CPU s; peak private GiB, streaming vs window (off) |
|---|---|---|---|---|---|---|
| bundled, on | pass | pass | pass | far end 0.76 vs 2.75 s; **36.4 vs 9.2 s**: fail | **18.0% vs 5.6%**: fail | 92 vs 121 (64); **6.24 vs 5.81** (5.52): fail |
| bundled, everyone | pass | pass | pass | far end 0.77 vs 3.89; **36.9 vs 22.3**; mic 1.00 vs 2.46; 22.4 vs 21.9: fail | far end **18.0% vs 8.5%**, mic 3.3% vs 2.7%: fail | 133 vs 190 (64); **6.94 vs 6.08** (5.52): fail |
| continuous, on | pass | pass | pass | 0.85 vs 2.38; 1.8 vs 4.9: pass | 3.1% vs 5.2%: pass | 64 vs 135 (32); **5.93 vs 5.50** (5.21): fail |
| continuous, everyone | pass | pass | pass | far end 0.85 vs 2.36; 1.6 vs 4.3: pass (the mic is silent on `continuous`) | far end 3.1% vs 2.7%: pass | 92 vs 132 (32); **6.63 vs 5.63** (5.21): fail |

Point 3 in detail: decode rtf at or below "off" on both tracks, catch-up 0.97-1.49 s against 1.43-1.50 s off, no audio dropped, no preview off, no slow pause. Vanished words: far end 0-2 in every run; mic under `everyone` on `bundled` 50 against window's 42.

**Verdict on the desktop: neither "Other people only" nor "Everyone" passes.** The streaming model shows words sooner at the median (under 1 s against 2.4-3.9 s) and uses less CPU (a quarter to a half less than window previews), but on `bundled` its far-end p95 on-screen lag is about 36 s (some far-end words seem to reach the screen only when the settled text does; the reports do not say why, and this was not investigated), it changes 18% of the far-end words it shows, and its peak private memory is 0.4-1.0 GiB above window previews on every run. Under the rule the Surface decides; it measures from lane/nemotron at dec19c6 or later. (Coordinator's note, not part of the rule: unless the Surface differs sharply, the window previews stay.)

### Result on the Surface, 2026-11-13

The Surface (Snapdragon X2 Elite, 16 GB, mains), `lane/nemotron` at `dec19c6` (commit 7, after the review fixes), the rule above unchanged. Both tracks transcribed on QNN in every run (`live_backends`); every streaming session's three ONNX Runtime sessions on `CPUExecutionProvider`. "Window" is today's previews on the NPU loaded with `--force-previews`, the same load T12.12c allows for "Other people only" (that branch is not merged here); "Everyone" window previews stay refused on the NPU, so `everyone` is judged against "off" as the rule says. One run per setting; the machine had other user apps open (machine load median 4-34% across runs, in each report), none of this session's builds or measurements. Reports: `live-previews/t1223/t1223-qnn-*.json`.

| Run | Ref on-screen median / p95 | Mic on-screen median / p95 | Ref changed (kept / changed / vanished) | Mic changed | Process CPU | Peak private | Preview rtf (t2 / t3 / t4) |
|---|---|---|---|---|---|---|---|
| off, bundled | 14.1 / 37.0 (settled) | 15.4 / 32.3 (settled) | - | - | 39 s | 2.66 GiB | - |
| window on, bundled | 2.4 / 13.1 | - | 10 of 184 (174 / 10 / 1) | - | 54 s | 2.71 GiB | 0.023 (NPU) |
| stream on, bundled | 0.8 / 36.0 | - | 37 of 210 (173 / 37 / 3) | - | 88-91 s | 3.36 GiB | 0.183 / 0.148 / 0.129 |
| stream everyone, bundled | 0.8 / 36.0 | 1.0-1.1 / 21.4 | 37 of 210 | 7 of 184 (177 / 7 / 50) | 138-144 s | 4.05 GiB | 0.18 / 0.15 / 0.13 per track |
| off, continuous | 36.9 / 49.7 (settled) | - | - | - | 19 s | 2.53 GiB | - |
| window on, continuous | 1.9 / 5.0 | - | 7 of 218 (211 / 7 / 0) | - | 38 s | 2.59 GiB | 0.018 (NPU) |
| stream on, continuous | 0.8-0.9 / 1.8 | - | 6 of 226 (220 / 6 / 0) | - | 60-62 s | 3.23 GiB | 0.183 / 0.150 / 0.130 |
| stream everyone, continuous | 0.8-0.9 / 1.7-1.9 | - | 6 of 226 | - | 100-104 s | 3.92 GiB | 0.18 / 0.15 / 0.13 per track |

The thread count changed only the streaming model's own rtf (2 to 4 threads: 0.18 to 0.13); every word count, lag and stored hash was identical across 2, 3 and 4 threads, so each count is judged the same.

By point, at every thread count:

1. **Where it ran**: holds.
2. **Stored words identical**: holds (`stored_sha256` equals "off" on every track of every run).
3. **Main transcript**: holds (track rtf 0.020-0.021 against off's 0.021 on `bundled`, 0.012 against 0.012 on `continuous`; catch-up within 0.1 s of off's; no audio dropped, no preview off, no slow pause).
4. **Sooner**: the median holds everywhere (0.8-1.1 s against window's 2.4 and 1.9 s). The p95 fails for `on` on `bundled`: 36.0 s against window's 13.1 s. Most of the far-end words show within a second, but about one in twenty settled words never had a streamed word under it, so it waited for settled text as with previews off. On `continuous` it holds (1.8 against 5.0 s). For `everyone`, judged by the absolute bound only, the medians hold.
5. **As good**: fails on `bundled` for both settings. The far end's changed share is 37 of 210, 17.6%, against the 10% bound and twice window's 5.4%. On `continuous` it holds (2.7%). The mic under `everyone` changed 3.8%, but 50 of its previewed words vanished before settling (the window previews' far-end-on-the-mic effect, T12.12c point 4, appears here too).
6. **Cost**: `on` fails against window (process CPU 88-91 s against 54 s on `bundled`, 60-62 against 38 s on `continuous`; peak private 3.36 against 2.71 GiB and 3.23 against 2.59 GiB). `everyone` against off holds T12.12c's 1.5 GiB limit (+1.39 GiB on both audios); CPU time reported (138-144 s and 100-104 s against off's 39 and 19 s).

**Under the rule, neither setting passes on the Surface at 2, 3 or 4 threads**: `on` fails points 4, 5 and 6, `everyone` fails point 5. The window previews stay. The rule's NPU conversion step is not indicated, since the failures are not only cost (points 4 and 5 fail too). What the numbers say beside the verdict: the streaming model shows far-end words much sooner at the median (under a second against about 2 s) and is steady on continuous speech, but on real meeting audio it disagrees with Parakeet's settled text about three times as often as the window previews and misses some words entirely, at about 1.7 times the window previews' CPU time on this machine. The desktop's CPU run is the coordinator's.

The rule asks for an idle host and these reports show 4-34% machine load, so a pass could not have been claimed on them; the failures stand because points 4 and 5 are word counts and point 6 is the process's own CPU time (coordinator's note at merge, from the lane-merge review).

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

## Shorter speech stretches for Parakeet (T12.24)

Settled text arrives a median 14-15 s and p95 38-50 s after the words on meeting audio, and the wait is the stretch, not the decode. `PieceConfig::max_piece` is 30 s because that is one Whisper window; Parakeet has no such window. This measures whether a shorter `max_piece` gives settled text sooner without costing accuracy. Evaluation only: no shipped default changes here, and Whisper keeps 30 s whatever the result.

**The rule, written and committed 2026-11-12 before any measurement** (judgements, not derived).

Setup:

- Parakeet TDT 0.6B v2 only. `max_piece` 30 s (today) against 20, 15 and 10 s, every other `PieceConfig` field at its default, the live stretches and the final pass using the same value (D20). The value is set only through the evaluation-only `--max-piece <s>` of `live_lag`, `m2_prod` and `stretch_eval`; without it each runs production's `PieceConfig::default()` unchanged.
- The Surface (Snapdragon X2 Elite, 16 GB), on mains. Timings (lag, rtf) only on an otherwise idle machine; accuracy runs may share it.

Measurements:

- **Lag** (timing): `live_lag --backend qnn --previews off --max-piece <s>` on `bundled` and on `continuous --seconds 120`. Read `reference.word_lag_s` (median, p95) on both, `mic.word_lag_s` on `bundled`, `catch_up_after_end_s` and the decode rtf of each track.
- **Streams** (accuracy): `m2_prod --backend cpu --metric asr.stream_determinism --max-piece <s>`: the pinned LibriSpeech test-clean utterances in about 60 s streams, each decoded by the production live stretches and by the final pass without reuse. Words differing between the two, and the live and final WER.
- **AMI dev** (accuracy and rtf): `stretch_eval --backend cpu|qnn` over the five AMI dev mixes (`ES2003a`, `ES2011a`, `IS1008a`, `TS3004a`, `TS3006a`, fixture set `ami-dev`, dev data, never an acceptance input), each whole recording decoded once per value by the final-pass transcriber (`final_pass_asr`) with Silero and `transcribe_in_pieces` on 16-bit-quantised audio, keeping the words production keeps. The reference is every participant's words (`refs::ami_reference` over the whole recording, ordered by start time), so the WER counts overlapping speech and crosstalk the same way for every value: it compares values with each other and is not comparable with `asr.wer_ami_ihm`. These are the Mix-Headset recordings rather than single headsets because a mix decoded in pieces exercises piece cuts on meeting speech without counting one headset's crosstalk as insertions. Corpus WER over summed counts.
- **Cuts** (AMI dev, counted separately): a cut is a boundary between consecutive pieces of `plan_pieces` (the same VAD and plan the decode used) with no gap between them, so speech was split rather than paused. For each cut: the reference and hypothesis words starting within 2 s either side, scored as one more `WerCounts` (cut-window WER, summed over cuts), and a word doubled when the last kept word of the earlier piece and the first of the later piece are equal after Whisper normalisation and both lie within 1 s of the cut. Reported per value: cuts, cut-window WER, doubled words, and doubled words per 10 minutes of audio.
- **Decode rtf**: AMI dev decode wall time over audio length, CPU and NPU, from idle-machine `stretch_eval` runs.

A shorter value passes only if every point holds against 30 s on the same data:

1. **D20 holds**: 0 words differ between live and final on the streams, at every value.
2. **AMI dev WER** rises by at most 0.5 points (absolute), on the CPU and on the NPU each against its own 30 s run.
3. **Streams final-pass WER** rises by at most 0.2 points.
4. **Cuts**: at most 1 doubled word per 10 minutes of AMI dev audio.
5. **NPU cost**: AMI dev decode rtf on the NPU at most 0.1 (every piece is a full 30 s static encoder run there, so more pieces cost more; 0.1 leaves room for two tracks and previews).
6. **It is worth having**: `reference.word_lag_s` median on `bundled` at least 2 s lower than at 30 s, and its p95 on `continuous` at least 5 s lower.

The recommendation is the passing value with the lowest `bundled` reference median lag (a tie goes to the longer value). If none passes, Parakeet keeps 30 s. A measurement that contradicts this rule is reported as it is; the rule is not changed after measuring. A passing value becomes Parakeet's default only through a follow-up task (a `PieceConfig` per model, `STRETCH_RECORD_VERSION` handling, `accept m2` re-run).

**Result, 2026-11-13** (the Surface, mains; the rule above unchanged; reports in `live-previews/t1224/`, counts and times only, the stream reports with their reference and hypothesis text removed). Every live session ran both tracks on QNN; every `stretch_eval` decode ran on its requested backend. One run per value. The lag runs saw 20-44% machine load from other open apps (`host.machine_load_percent` in each report), so read small lag differences with care.

Settled-text lag, `live_lag --backend qnn --previews off` (median / p95, seconds):

| `max_piece` | bundled reference | bundled mic | continuous reference | catch-up (bundled) | decode rtf mic / ref (bundled) |
|---|---|---|---|---|---|
| 30 (today) | 14.1 / 37.0 | 15.4 / 32.3 | 36.9 / 49.7 | 0.42 s | 0.022 / 0.021 |
| 20 | 13.8 / 33.7 | 15.3 / 26.5 | 27.3 / 37.1 | 0.42 s | 0.025 / 0.024 |
| 15 | 14.1 / 22.4 | 15.5 / 24.4 | 19.9 / 25.2 | 0.58 s | 0.026 / 0.028 |
| 10 | 11.7 / 18.0 | 12.1 / 20.9 | 13.9 / 18.1 | 0.40 s | 0.034 / 0.030 |

Accuracy and cost (AMI dev mixes: 5 meetings, 5,796 s, 12,413 reference words; streams: 26 LibriSpeech test-clean streams, 1,796 s, 4,681 words):

| `max_piece` | AMI dev WER CPU / NPU | cuts | cut-window WER CPU / NPU | doubled CPU / NPU (per 10 min) | decode rtf CPU / NPU | streams final WER | live vs final words differing |
|---|---|---|---|---|---|---|---|
| 30 | 17.70% / 17.86% | 121 | 24.13% / 24.31% | 2 / 3 (0.21 / 0.31) | 0.0359 / 0.0150 | 1.624% | 0 of 4,677 |
| 20 | 18.09% / 18.01% | 209 | 21.83% / 21.25% | 3 / 5 (0.31 / 0.52) | 0.0370 / 0.0172 | 1.666% | 0 of 4,674 |
| 15 | 18.12% / 18.22% | 307 | 21.63% / 21.41% | 2 / 2 (0.21 / 0.21) | 0.0398 / 0.0202 | 1.688% | 0 of 4,678 |
| 10 | 18.98% / 19.17% | 467 | 22.70% / 22.70% | 7 / 6 (0.72 / 0.62) | 0.0431 / 0.0247 | 1.837% | 0 of 4,677 |

Under the rule, against 30 s:

- **20 s**: points 1-5 pass (AMI +0.39 CPU, +0.15 NPU; streams +0.04; doubled at most 0.52 per 10 min; NPU rtf 0.017). Point 6 fails: the bundled reference median falls 0.3 s, not 2 s (the continuous p95 falls 12.6 s).
- **15 s**: points 1-5 pass (AMI +0.42 CPU, +0.36 NPU; streams +0.06; NPU rtf 0.020). Point 6 fails: the bundled median is unchanged (the continuous p95 falls 24.5 s).
- **10 s**: point 6 passes (bundled median -2.4 s, continuous p95 -31.6 s), but point 2 fails (AMI +1.28 CPU, +1.31 NPU against 0.5) and point 3 fails (streams +0.21 against 0.2).

**No value passes, so under the rule Parakeet keeps 30 s.** What the numbers say beside the verdict (not a decision): on meeting audio with natural pauses (`bundled`) the median wait is set by the pauses, not by `max_piece`, so only the tail moves (p95 37.0 -> 22.4 s at 15 s); the gain is large only for someone who never pauses (`continuous`). D20 held at every value (0 words differ). Cut-window WER is lower at 20 and 15 s than at 30 s because 30 s cuts only the longest unbroken stretches, where recognition is already hardest. If the owner wants the tail shortened, 15 s is the value the numbers favour (every accuracy and cost point passes, p95 lag on both audios down 12-25 s); that would need the rule's point 6 changed to a tail criterion by the owner, labelled as decided after the measurement.

Reproduce (each from the repo root on the Surface, idle for the timed ones):

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews off --max-piece 15 --out target\overnight\t1224\lag-qnn-bundled-15.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio continuous --seconds 120 --previews off --max-piece 15 --out target\overnight\t1224\lag-qnn-continuous-15.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example stretch_eval -- . --backend qnn --out target\overnight\t1224\ami-dev-qnn.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example stretch_eval -- . --backend cpu --out target\overnight\t1224\ami-dev-cpu.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example m2_prod -- . --backend cpu --models $env:LOCALAPPDATA\Earshot\models --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --metric asr.stream_determinism --max-piece 15
```
