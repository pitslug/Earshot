# Parakeet on the Snapdragon NPU (T8.3)

How Earshot runs NVIDIA Parakeet on the Surface's Hexagon NPU through ONNX Runtime's QNN execution provider, what was measured on the desktop, and what only GATE-03a can measure.

## The QNN execution provider package

- **ONNX Runtime no longer ships QNN.** Microsoft's `Microsoft.ML.OnnxRuntime.QNN` NuGet package stops at 1.24.4; from ONNX Runtime 1.25 the QNN EP is a *plugin* execution provider maintained by Qualcomm at github.com/onnxruntime/onnxruntime-qnn (NuGet `Qualcomm.ML.OnnxRuntime.QNN`, PyPI `onnxruntime-qnn` 2.x). A plugin is a separate DLL registered at run time into a standard ONNX Runtime (`RegisterExecutionProviderLibrary`) and selected per session through the device API (`SessionOptionsAppendExecutionProvider_V2`). Release v2.7.0 states "ONNX Runtime Compatibility: >= 1.24.1 (compiled with v1.29.0)", QAIRT SDK 2.51.0; it runs in our ONNX Runtime 1.31.
- **Pinned in `models/manifest.toml`** (GitHub release assets, sha256 = GitHub's published asset digest, member pins for the loaded libraries):
  - `onnxruntime-win-arm64`: Microsoft's ONNX Runtime 1.31.0 for Windows ARM64 (MIT), sets `arm64`, `qnn`; `onnxruntime.dll` sha256 `b6da6716...0641`.
  - `qnn-ep-win-arm64`: `onnxruntime-qnn-2.7.0-win-arm64.zip`, sha256 `a086c435...527a`, set `qnn`; `onnxruntime_providers_qnn.dll` `b4934e35...795c`, `QnnHtp.dll` `b1236387...9450`.
  - `qnn-ep-win-x64`: `onnxruntime-qnn-2.7.0-win-x64.zip`, sha256 `127e574b...819a`, set `qnn-x64`; `onnxruntime_providers_qnn.dll` `836ec9b4...d40b`, `QnnHtp.dll` `d336a033...bb77` (Qualcomm's HTP simulator).
- `earshot_onnx::qnn::register_qnn` checks both libraries against these pins before anything is loaded, registers the plugin once per process and picks its device: on ARM64 only an `NPU` device; on x64 the simulator's `CPU` device, reported as `qnn:htp-simulator`, never as the NPU.
- Only those two libraries are re-hashed at load. The plugin then loads `QnnSystem.dll`, `QnnHtpPrepare.dll`, the `QnnHtpV*Stub.dll` and `libQnnHtpV*Skel.so` from the same directory without verification (they are pinned only at extraction, by the archive's manifest SHA-256), the same posture as ONNX Runtime's `onnxruntime_providers_shared.dll`.

### Licence findings (checked 2026-10-24)

- `onnxruntime_providers_qnn.dll` (the plugin): MIT (`LICENSE` in the zip; Qualcomm Technologies and Microsoft).
- The QAIRT runtime libraries in the same zip (`QnnHtp.dll`, `QnnHtpPrepare.dll`, `QnnSystem.dll`, the `QnnHtpV*Stub.dll` / `libQnnHtpV*Skel.so` Hexagon libraries, `QnnGpu.dll`, `Genie.dll`, ...): Qualcomm's **"AI Stack License"** (`Qualcomm_LICENSE.pdf` in the zip, marked "QTI Proprietary/Confidential"). In short: a non-exclusive, revocable, royalty-free licence to use and copy the software *solely to develop your software applications*, to demonstrate it, to *distribute and sublicense it in object code only as incorporated in your application* (never standalone) and to benchmark it; no reverse engineering; no removal of notices; a list of prohibited AI uses (none applies to transcription); QTI may terminate at any time; you indemnify QTI for your use and distribution; California law, San Diego courts; acceptance is by downloading or using.
- **Development download is allowed** (use and copy to develop applications, benchmarking). The manifest keeps these artefacts in the development sets `qnn` and `qnn-x64` (licence id `MIT AND LicenseRef-Qualcomm-AI-Stack`, allowed only there by `models_are_cache_only_with_licence_and_permissive_terms`).
- **Redistribution** (bundling the HTP libraries with the ARM64 app) looks permitted only "as incorporated in Your software application", under an agreement that is revocable and carries an indemnity: that is T8.4's question for the owner (PLAN.md 7), not decided here.
- The owner accepted the AI Stack License for development on their own machines (2026-10-24); redistribution stays T8.4's question (Windows ML to be looked at then).

### Telemetry (ETW) check

The package's `Privacy.md` says Windows builds may send usage data through Windows TraceLogging, which Windows' Connected User Experiences service collects from events carrying Microsoft telemetry keywords. verified with an ETW listener before any real meeting runs on the NPU. The rule: system information (runtime and driver versions, CPU model, core count, memory, EP registration results) may be offered to telemetry; conversation content (audio, transcript text, summaries, speaker names, file or meeting names, prompts) never.

`cargo xtask qnn-etw` (`xtask/src/qnn_etw.rs`, Windows' own `logman` and `tracerpt`, no installs; needs an administrator or a member of "Performance Log Users") runs the `qnn_etw_probe` example three times on the smoke clip `61-70968-0000` (the longest committed one):

1. **Baseline:** Silero VAD on the CPU EP over the clip's frames, no plugin.
2. **QNN:** the plugin registered, Silero VAD on the QNN EP over the clip's frames (Silero is float with dynamic shapes, so the QNN EP takes 1 of its 212 nodes).
3. **Transcribe:** run 2 plus a transcription of the clip. When the NPU conversion of Parakeet TDT 110M is installed (`cargo xtask qnn-convert --model parakeet-tdt-110m`), its encoder runs on the QNN EP (the documented partition; the prediction network and joint on the CPU); otherwise the float model runs on the CPU EP and the verdict says `PASS (qualified)`. The transcript goes to a file, never to the console.

It traces every ETW provider the QNN process registers (`logman query providers -pid`, the union of three queries, which differed between runs) plus ONNX Runtime's two providers, at every level: ONNX Runtime's telemetry and log providers with every keyword, all others with the three telemetry keywords (the only events Windows telemetry collects; events without a keyword are delivered too). With every keyword on every provider, Windows' machine-wide diagnostics made a 1-5 GB trace in an 8-minute run; the decoded XML (several GB) is scanned line by line and deleted after a pass.

It reports the probe process's events by provider, keywords, event name and field names (never values), counts other processes' events on the traced providers in an informational line, and passes only when (`qnn_etw::verdict`, fail closed)

- every captured output was complete, every run's trace holds events of the probe's process including ONNX Runtime's `ProcessInfo` MEASURES event (the liveness signal), and every `Data` element parsed;
- every event with a telemetry keyword (`TELEMETRY`, `MEASURES`, `CRITICAL_DATA`), whoever sends it, is a reviewed system-information event with reviewed fields only (`qnn_etw::SYSTEM_EVENTS` plus `COMMON_FIELDS`); an unknown event or field fails and needs review before it is added; and
- no text of any event of the probe's process (attribute values, every text node including `UserData`, `Binary` payloads decoded as UTF-8 and UTF-16) contains a word (five letters or more) of the clip's reference or of the transcript heard, or the clip's name (compared in memory, never printed).

Desktop result (x64, Qualcomm's HTP simulator, 2026-10-24): **PASS**, the clip transcribed on the QNN EP (the simulator, 375-490 s for this 7 s clip padded to one 30 s piece). 6.7 million texts of the probe's events searched for 9 words and the clip name: none found.

| Runs | Provider | Keyword | Event names (field names) |
|---|---|---|---|
| all | Microsoft.ML.ONNXRuntime {3A26B1FF-...} | MEASURES | ProcessInfo (runtimeVersion, cpuModel, processorCount, totalMemoryMB, isDebuggerAttached, isRedist, frameworkName, serviceNames), DriverInfo (deviceClass, driverNames, driverVersions), RegisterEpLibraryStart (registrationName, runtimeVersion, frameworkName), RegisterEpLibraryEnd (registrationName, isSuccess, errorCode, errorCategory, errorMessage, frameworkName) |
| all | Microsoft.Windows.Graphics.DXCore {AB604427-...} | MEASURES | DXCoreCreateAdapterFactory, CreateAdapterList, GetAdapter, GetProperty: adapter enumeration by onnxruntime.dll's device discovery (calling module file name, adapter LUIDs, property ids, result codes) |
| all, Surface only | Microsoft.Windows.Graphics.DXCore {AB604427-...} | MEASURES | QueryAndFillAdapterExtendedProperiesOnPlatform (dxCoreTelemetryFactoryUUID, adapterLuid, gpuPreference, gpuPreferenceDecidingFactor, registryRequestedMode, finalMode): reviewed 2026-11-03 from its values on the Surface (GATE-03a step 9, `gates/GATE-03a.fail`): the three display adapters' per-boot LUIDs, preference codes all 0, a zero or per-process random factory id |
| all | Microsoft.Windows.Graphics.DXGI {03BBE5B8-...} | MEASURES | GpuPreferenceInfo (adapter LUIDs, hybrid roles, GPU preference codes) |
| all | Microsoft.Windows.TlgAggregateInternal {703FCC13-...} | TELEMETRY | TlgAggregateSummary (the DXCore counters above: provider GUID, counter names and values) |
| QNN runs | Microsoft.Windows.Graphics.D3D12Loader {9BED1243-...} | TELEMETRY | ProcessInfo (UTCReplace_AppSessionGuid, schemaVersion) |
| all | ONNXRuntimeTraceLoggingProvider {929DD115-...} | none (0x2) | ONNXRuntimeLogEvent (logger, category, location, message): ONNX Runtime's own log lines, not telemetry; up to 200,000 in the transcribing run (the QNN graph build); searched for content like every other event |
| all | Microsoft-Windows-User-Diagnostic, CentennialIssueTrackerTraceLogging | none | error codes and a loader diagnostic, not telemetry |

Every telemetry event also carries UTCReplace_AppSessionGuid, PartA_PrivTags and schemaVersion. All are system information, allowed under the owner's rule; the DXCore, DXGI and aggregation events were reviewed from their values on this machine (2026-10-24) before they joined the allowlist.

Limits: the trace covers ONNX Runtime and the plugin through the providers the QNN process registers; a provider registered only during the transcription is not listed (the listing runs in the QNN mode). GATE-03a repeats the check on the Surface's NPU, where a new event (for example from the NPU driver) fails until reviewed.

## Provider and evidence design

- **Order:** CUDA > QNN > Vulkan > CPU, QNN only in ARM64 `qnn` builds (`backends::with_npu`, D20: NPU > Vulkan > CPU on the Surface); the x64 simulator is never auto-selected. QNN runs ONNX models only, so it is offered to Parakeet models alone (`backends::model_order`); whisper never sees it and shows no fallback.
- **One ONNX Runtime per process:** for QNN, `init_onnx` loads the CPU library for the target architecture (`ORT_CPU_DYLIB` is the ARM64 build on ARM64) and registers the plugin into it; `onnx_library` chooses QNN only for a Parakeet model with QNN in the order.
- **ort's automatic device policy:** ort 2.0.0-rc.14 gives every new session ONNX Runtime's automatic EP selection with the "max efficiency" (prefer NPU) policy. Once the plugin is registered that silently moved CPU sessions (Silero, pyannote, CAM++, Parakeet's steps) onto QNN (`Could not determine default backend path for device`). Every CPU session now names the CPU EP's own device (`qnn_registration_leaves_cpu_sessions_on_the_cpu`).
- **Evidence:** ONNX Runtime's node-placement log, as for CUDA, now also itemised by operator (`Placement::ops`). There is one QNN rule (`check_placement` for `Qnn` is `check_qnn_parakeet`): a Parakeet NPU conversion has a **documented partition** (`earshot_onnx::parakeet::QNN_ENCODER_CPU_OPS`): the encoder's only CPU nodes are its input `QuantizeLinear` and output `DequantizeLinear` (the float boundary of the quantised graph); everything else is one fused QNN node; the prediction network and joint run wholly on the CPU. Any other CPU node is a refusal (`check_qnn_parakeet`), never an NPU claim.
- **Why the steps stay on the CPU:** greedy TDT decoding calls the prediction network (one LSTM step) and the joint (two small matmuls) once per emitted token or frame; a round trip to the NPU for each would cost more than the work.

## NPU-ready models (static shape, QDQ)

`cargo xtask qnn-convert --model <name>` (`xtask/src/qnn_convert.rs`) builds `<models>/parakeet-qnn/<name>/` for `parakeet-tdt-110m`, `parakeet-tdt-0.6b-v2` and `parakeet-tdt-0.6b-v3`:

1. Calibration pieces (T8.3c): `qnn_convert::calibration_plan` picks LibriSpeech **dev-clean** utterances that are *not* in the pinned subset (its dev-clean half is the dev WER set `librispeech_dev`, its test-clean half the acceptance set), speakers in numeric order, each speaker's utterances in (chapter, utterance) order: the recipe's `single` utterances as a piece each, then its next utterances joined with 0.5 s gaps into pieces of at most 30 s, of which the first `joined` are kept. The default (`j2`: 2 joined pieces per speaker) is 80 pieces from all 40 dev-clean speakers. The `parakeet_calibration` example writes their production NeMo features (`NemoMel`) at the static shape into `target/qnn-convert/cal-<n_mels>-j<joined>s<single>`. Never the test-clean or smoke sets that the accuracy is measured on.
2. A virtual environment in `target/qnn-convert/venv` from CPython 3.14 on PATH, filled with `pip install --require-hashes --no-deps --only-binary :all:` from `xtask/python/requirements-qnn-convert.txt` (onnx 1.23.2, onnxruntime 1.31.0, numpy 2.5.4 and their closure). There is no Rust route: ONNX Runtime's quantisation tools and the QNN QDQ configuration exist only in its Python package. PyPI is allowed for xtask developer tooling only, hash-pinned and never shipped; every pip call runs with `PIP_DISABLE_PIP_VERSION_CHECK=1` and `PIP_NO_INPUT=1`, and every step runs under a deadline. Calibration pieces are written to `cal-<n_mels>-j<joined>s<single>.part` with the plan they came from (`plan.txt`), renamed when complete with their count in `pieces.txt` (which must equal the plan's), and reused only while their `plan.txt` equals the plan computed now (`calibration_is_current`: a changed selection, corpus or fixtures path writes them again); the converter refuses a different count. The venv records the SHA-256 of the requirements it was installed from (`earshot-requirements.sha256`); a changed pin is installed again. At load, the runtime checks every model file of a conversion against the size and SHA-256 in its `conversion.json` (`backends::verify_qnn_conversion`) and refuses a mismatch, and refuses a conversion whose `task` is not in `backends::QNN_CONVERSION_TASKS` (`T8.3c`) or that has no `recipe`, naming the `qnn-convert` command (a T8.3 conversion loses everything after 16 s of a piece).
3. `xtask/python/qnn_convert.py`: fix `audio_signal` to `[1, n_mels, 3001]` (one 30 s piece, the piece planner's `max_piece`) and `length` to `[1]`; fold constants with ONNX Runtime's basic optimisations; quantise to QDQ with `get_qnn_qdq_config` and the recipe (`--recipe`, `qnn_convert::RECIPES`; default `a16w8pc-minmax-j2`: 16-bit activations, 8-bit weights with Conv weights per output channel, MinMax calibration; ONNX Runtime raises the opset to 21 for 16-bit QDQ), with **strided calibration** (one piece per stride, so every piece counts) and the **length arithmetic held exact** (below); write `encoder.onnx` + `encoder.weights` with the source's metadata restored, copy the float `decoder.onnx`, `joiner.onnx`, `tokens.txt`, and record the recipe, versions and hashes in `conversion.json` (`recipe`, `quantisation.*`). The file set is unchanged. `--out <dir>` writes a conversion elsewhere (for measuring variants).

The runtime pads each piece's features to the static 3001 frames after computing them on the piece's own length (`parakeet::static_features`), so the per-feature normalisation never sees the padding and the `length` input masks it in the encoder.

The conversions are hosted nowhere: shipping waits on where they are hosted (Parakeet is CC-BY-4.0) and on T8.4. `ParakeetModel::qnn_dir` is `parakeet-qnn/<name>`; a missing conversion is a refusal naming the `qnn-convert` command, and the CPU is used, reported.

## Measured on the desktop (x64, 2026-10-24)

**Accuracy cost of the conversion** (QDQ model on the CPU EP against the float model, same production piece path, greedy; `cargo xtask model-eval --backend cpu --model parakeet-tdt-110m,parakeet-tdt-110m-qnn,parakeet-tdt-0.6b-v2,parakeet-tdt-0.6b-v2-qnn --corpus librispeech,smoke`, report `docs/model-eval/qnn-quantisation-cpu.json`):

| Model | LibriSpeech subset WER % (196 utt.) | Smoke WER % (20 clips) |
|---|---|---|
| parakeet-tdt-110m, float | 2.39 | 6.10 |
| parakeet-tdt-110m, NPU conversion (A16W8 QDQ) | **6.54** (+4.15) | 6.71 (+0.61) |
| parakeet-tdt-0.6b-v2, float | 1.75 | 1.22 |
| parakeet-tdt-0.6b-v2, NPU conversion (A16W8 QDQ) | **5.96** (+4.21) | 1.22 (+0.00) |

- **The static shape is exact:** the fixed-shape float 110M encoder (`qnn_convert.py --float-only`, padding to 3001 frames) scores exactly the float model (2.39 / 6.10). The whole cost is quantisation.
- These T8.3 numbers are **superseded** (T8.3c, below): the cost was not quantisation noise but a calibration defect (almost all of it deletions: 207 against the float model's 11).

## T8.3c: what the conversion really costs (desktop, 2026-10-25)

**Root cause of the T8.3 cost.** T8.3 bounded the calibrator's memory with ONNX Runtime's `CalibMaxIntermediateOutputs = 4`. In ONNX Runtime 1.31 `MinMaxCalibrater.collect_data` *clears* the buffered outputs when that limit is reached without reducing them (`calibrate.py`, `clear_collected_data()` in the loop), so only the pieces after the last flush were calibrated: 1 of 89. Every activation range came from one 16 s piece, including the encoder's length arithmetic: `encoded_lengths` saturated at 201 frames (16 s), so on every longer piece the encoder masked out and the decoder never saw the rest (dev-clean pieces: lengths 351, 261, 303, 369 came out as 201). LibriSpeech test-clean has many utterances over 16 s; the smoke clips are shorter, which is why smoke barely moved. Fixed in the converter:

1. **Strided calibration** (`get_qnn_qdq_config(stride=1)`, the reader implements `__len__` / `set_range`): each piece's activations are reduced and merged before the next runs, so every piece counts and memory holds one piece.
2. **Exact length path** (`qnn_convert.py::length_path_tensors`): the float tensors of NeMo's length arithmetic (22 in the 110M, 16 in the 0.6B v2) (Cast/Add/Sub/Div from `length`, one element each, plus their constants 1, 2, 3) get a fixed 16-bit scale of 1/16 with zero point 0, which holds every multiple of 1/16 up to 4095.94 exactly. Calibrated like activations, their step was about 0.05 frames, so 261.0 could dequantise to 260.98 and the next cast floored it to 260: one encoder frame lost from the output and every length mask. Measured on 16 dev pieces: 9 of 16 encoded lengths off with calibration fixed but the length path calibrated, 0 of 16 with it exact. This is a correctness fix applied to every recipe, not a ranked variant.

Both are checked by behaviour in `qnn_convert_selftest_calibrates_every_piece_and_keeps_lengths_exact` (ignored: it needs the pinned venv; `cargo test -p xtask --lib qnn_convert_selftest -- --ignored`), which runs the real `qnn_convert.py` on a synthetic encoder with NeMo's three-stage length arithmetic (`xtask/python/qnn_convert_selftest.py`): the `audio_signal` range equals a single MinMax pass over all nine pieces, whose largest values sit in piece 2 (the last piece alone gives another), every length-path tensor has scale 1/16 and zero point 0, and the QDQ model's encoded lengths equal the float model's for every length 1..=3001. Reverting to `CalibMaxIntermediateOutputs = 4`, reading no piece per stride, a length scale of 1 or dropping the length overrides each fail it (checked 2026-10-25). `qnn_convert_script_source_uses_strided_calibration_and_length_overrides` is only a source pin that runs without the venv.

**Static input length.** Production feeds the encoder pieces of at most 30 s (`PieceConfig::max_piece`), padded to the static 3001 frames by `parakeet::static_features`, which refuses anything longer; no evaluation run dropped or refused a piece. Calibration pieces are cut at the same 30 s and padded the same way, and the static float encoder scores exactly the float model (T8.3), so the static shape matches what production feeds.

**Data (fixed before measuring).** Calibration: dev-clean minus the pinned subset (above). Dev WER: `--corpus librispeech_dev`, the pinned subset's 196 dev-clean utterances (never calibration data). Test WER: the pinned subset's 196 test-clean utterances and the 20 smoke clips, used only as the gate. Every number: QDQ model on the CPU EP against the float model, the production piece path, greedy (`model_eval`, 4 threads, the T6.10b budget). Cost = variant WER minus float WER on the same corpus. Raw reports, unmodified: `docs/model-eval/qnn-t8.3c/` (the file name names the variant; each was a `model_eval --backend cpu --model <m>-qnn --corpus librispeech_dev --corpus librispeech --corpus smoke` run against a models directory holding that conversion, built with `cargo xtask qnn-convert --model <m> --recipe <recipe> --out <dir>`).

**Selection rule (written 2026-10-25 before any variant was measured).**

1. Eligible: LibriSpeech test cost < 1.0 point and smoke cost < 1.5 points; if no variant has test cost < 1.0, test cost < 1.5 (GATE-03a's rule).
2. Among eligible, the lowest expected NPU cost class: (a) 8-bit activations everywhere; (b) 16-bit activations everywhere (the T8.3 precision); (c) A16W8 with some 16-bit weights; (d) float ops: never (they break the documented partition). Weight granularity and calibration method cost nothing on the NPU (same op counts and types).
3. Ties within a class: the lowest dev cost; then the fewer 16-bit tensors; then the smaller calibration set (shorter conversion).
4. The pick must keep parakeet-tdt-0.6b-v2's test cost < 1.5; otherwise the next variant in the same order.

Test numbers never rank eligible variants against each other.

**parakeet-tdt-110m** (float: test 2.39, dev 1.93, smoke 6.10; dev has 4883 reference words, test 4681, smoke 164):

| Variant (recipe) | Test WER % (cost) | Dev WER % (cost) | Smoke WER % (cost) | Encoder weights | Compute ops A16 / A8 |
|---|---|---|---|---|---|
| T8.3 conversion (1 of 89 pieces calibrated) | 6.54 (+4.15) | 7.74 (+5.81) | 6.71 (+0.61) | 112 MB | 1256 / 0 |
| `a16w8-minmax-j2`, calibration fixed, length path calibrated (diagnostic, an intermediate converter) | 2.37 (-0.02) | 1.99 (+0.06) | 6.71 (+0.61) | 112 MB | 1256 / 0 |
| `a16w8-minmax-j2` (fixed calibration and exact length path) | 2.39 (+0.00) | 2.03 (+0.10) | 6.71 (+0.61) | 112 MB | 1256 / 0 |
| `a16w8-minmax-j4s4` (320 pieces: 4 single + 4 joined per speaker) | 2.39 (+0.00) | 2.03 (+0.10) | 6.10 (+0.00) | 112 MB | 1256 / 0 |
| **`a16w8pc-minmax-j2`** (Conv weights per channel) | **2.35 (-0.04)** | **2.01 (+0.08)** | **6.10 (+0.00)** | 113 MB | 1256 / 0 |
| `a16w8-percentile-j2` (99.999th percentile) | 2.37 (-0.02) | 2.01 (+0.08) | 7.32 (+1.22) | 112 MB | 1256 / 0 |
| `a16w8-entropy-j2` | 2.41 (+0.02) | 2.01 (+0.08) | 6.71 (+0.61) | 112 MB | 1256 / 0 |
| `a8w8-minmax-j2` (8-bit activations) | 36.64 (+34.25) | 37.48 (+35.55) | 37.80 (+31.70) | 112 MB | 22 / 1234 |
| `a8w8pc-mixed-j2` (A8, inputs of LayerNorm, Add, Softmax, Reshape at 16 bit) | 2.39 (+0.00) | 1.93 (+0.00) | 6.10 (+0.00) | 113 MB | 876 / 380 (+291 type converts) |

Compute ops are the encoder's non-Q/DQ nodes with a quantised input, counted by the activation type of their inputs (A16: every one; the 22 A16 ops of the A8 variants are the length path). Every variant has the same 1256 ops, so within one activation class the expected HTP cost is the same; 16-bit activations cost HTP throughput against 8-bit (the gap is a GATE-03a measurement). The smoke cost of 0.61 is one word.

**Sensitivity sweep (dev data; `xtask/python/qnn_sensitivity.py sweep`, dev pieces from its `dev-plan` command, usage in its docstring).** Mean relative error of the encoder output against the float static model on 16 dev-set utterances (one per speaker), starting from A8W8 and promoting one group of activations to 16 bit at a time: A16W8 everywhere 0.109, A8W8 0.901. Per conformer layer no single layer matters (0.745-0.902; layer 15 the most). Per operator role across the encoder: inputs of LayerNormalization 0.723, Add 0.725, Softmax 0.821, Where 0.822, every other role between 0.898 and 0.906. With LayerNorm, Add and Softmax inputs at 16 bit (0.353), a second round found Reshape inputs (0.165; Conv 0.338, everything else above 0.34). That is the `a8w8pc-mixed-j2` recipe: it keeps every Gemm (the bulk of the encoder's arithmetic) on 16-bit inputs, so it saves 8-bit work only in the Conv, part of the MatMul and elementwise ops, and it adds 291 requantisations between the two activation types (the A16 and A8 graphs have none).

**The mixed variant is outside the rule.** `a8w8pc-mixed-j2` costs nothing measurable either, and its expected NPU cost may be lower than 16-bit activations everywhere, but the rule's classes (written before the sweep) have no "8-bit activations with 16-bit parts", and whether its 380 8-bit ops outweigh its 291 extra requantisations on the HTP is unknown without the NPU. It is not the default; it is kept as a recipe (`cargo xtask qnn-convert --model <m> --recipe a8w8pc-mixed-j2`) so GATE-03a can time it against the default if the default's NPU speed falls short.

**Deviations from the written rule (stated plainly).**

1. The length-calibrated diagnostic (dev 1.99 %, the lowest A16 dev WER) was excluded from the ranking: it is the converter before the length fix, which loses an encoder frame on 9 of 16 dev pieces, a correctness defect rather than a variant. The rule as written would have ranked it first.
2. `a16w8pc-minmax-j2`, `a16w8-percentile-j2` and `a16w8-entropy-j2` tie on dev (98 errors each) in class (b) with the same 16-bit tensors and calibration set, which the rule did not resolve. The tie-break below was defined **after** every variant's dev and test WERs had been seen: the finer dev number, the encoder-output error on the 16 dev utterances, is 0.034 for per-channel Conv weights against 0.112 (percentile), 0.109 (entropy) and 0.109 (`a16w8-minmax-j2`); the 8-bit per-tensor weights of the subsampling convolutions were the largest remaining error. Raw numbers: `docs/model-eval/qnn-t8.3c/110m-dev-encoder-error.json` (`qnn_sensitivity.py compare` on the finished conversions, every variant and the T8.3 conversion) and `110m-a16-reference-*.json` (`qnn_sensitivity.py sweep --a16-only` with each method and granularity, converted by the sweep itself). ONNX Runtime's QNN configuration applies `per_channel` to Conv only (Gemm and MatMul weights stay per tensor).

**Choice: `a16w8pc-minmax-j2`, the `qnn-convert` default.** Rule step 4 on parakeet-tdt-0.6b-v2 (float: test 1.75, dev 1.31, smoke 1.22): `a16w8-minmax-j2` 1.79 (+0.04) / 1.39 (+0.08) / 1.22 (+0.00); `a16w8pc-minmax-j2` 1.71 (-0.04) / 1.27 (-0.04) / 1.83 (+0.61, one word): eligible, so the pick stands. On the HTP simulator the `a16w8pc-minmax-j2` 110M encoder is still one fused QNN node with only its input `QuantizeLinear` and output `DequantizeLinear` on the CPU (`qnn_parakeet_conversion_runs_on_qnn_with_its_documented_partition`, `--features qnn`, 48 s). Conversion time on the desktop (32 threads, several conversions and evaluations running at once): 110M 4-10 min with 80 pieces MinMax (320 pieces: 12 min; entropy or percentile histograms: 33-34 min), 0.6B v2 16-36 min. Output: 110M encoder weights 113 MB, 0.6B v2 603 MB.

Before GATE-03a, the converted models must be rebuilt (`cargo xtask qnn-convert`): a T8.3 conversion has `"task": "T8.3"` and no `recipe` in its `conversion.json` and loses everything after 16 s of every piece; the runtime refuses it.

**The QNN EP on x64 (Qualcomm's HTP simulator), through `earshot_onnx`:**

- The plugin registers and offers its simulator as a CPU device; real placement logs were recorded from it for the tests.
- Silero VAD (float, dynamic shapes): the QNN EP takes 1 of 212 nodes; refused by the partition rule (`qnn_float_dynamic_model_is_mostly_left_on_the_cpu_and_refused`).
- The 110M NPU conversion: the whole encoder becomes **one fused QNN node**; only the input `QuantizeLinear` and output `DequantizeLinear` stay on the CPU (the documented partition), the prediction network and joint on the CPU. Load (HTP graph preparation in the simulator) about 86-102 s; decoding is far too slow to measure anything (about 6 minutes per 30 s piece) but it produced words ("Hedge offense" for "HEDGE A FENCE", "The story's written." exact). Speed and NPU accuracy come only from the Surface.
- The cross-compile `cargo xtask build --target aarch64-pc-windows-msvc --features qnn` builds the CLI (whisper.cpp with clang-cl through Ninja).

## What only GATE-03a can tell

NPU device registration, the encoder compile time on the HTP, NPU speed, NPU WER (within 1.5 points of the CPU) and a realtime meeting: `gates/GATE-03a.md`. A QNN ladder in `models/manifest.toml` (and so in `earshot benchmark`) waits on those measurements.
