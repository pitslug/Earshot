# One x64 install that can load CUDA (T8.4ad design)

Proposed 2026-11-06, for the owner to agree before it is built. Decided already by the owner: one combined library for every supported generation, and Whisper never on CUDA in the shipped app.

## What was measured

**The combined library.** llama.cpp 0.1.160, CUDA toolkit 13.4.1, `CMAKE_CUDA_ARCHITECTURES=75-real;86-real;120a-real`, cold build 7 min 19 s with Ninja:

| | MB |
|---|---|
| `ggml-cuda.dll`, three generations | 70.2 |
| `ggml-cuda.dll`, RTX 50 only, for comparison | 56.3 |
| The whole CUDA set: that, `llama.dll`, `ggml.dll`, `ggml-base.dll`, `ggml-cpu.dll` | 74.7 |
| cuBLAS, from NVIDIA (`cublas64_13.dll`, `cublasLt64_13.dll`) | 547.7 |

docs/cuda-libraries.md guessed about 55 MB per generation. Each extra generation cost about 7 MB. The real file holds `sm_75`, `sm_86` and `sm_120a` code. 8.6 code should also serve RTX 40 by NVIDIA's binary rules. [Likely: no such card here]

**One exe, either set.** `vk_probe`, built only against the Vulkan build, was copied byte for byte into two folders, with PATH holding only Windows and no CUDA variables, loading Qwen 3.5 4B with every layer on the card:

| Folder | Result |
|---|---|
| Vulkan DLLs | Ran: `Vulkan1 (NVIDIA GeForce RTX 5080)`, 33/33 layers |
| CUDA DLLs, no cuBLAS | Windows refused to start it: 0xC0000135, DLL not found |
| CUDA DLLs, cuBLAS beside them | Ran: `CUDA0 (NVIDIA GeForce RTX 5080)`, 33/33 layers |

So llama.cpp's backend is decided only by which DLLs load, with no second build of the app. The second row is why the app must not simply load the CUDA set and hope.

## The design

**The exe holds no CUDA code.** The shipped feature stays `vulkan-summaries`, with Whisper on Vulkan or the CPU. A new runtime feature adds only ONNX Runtime's CUDA provider, which loads its library at run time. [Likely: its effect on the exe's imports is checked first, by T8.4ad's accept]

**Summaries: two folders.** The Vulkan set stays beside the exe. The CUDA set and cuBLAS go in a folder under the models folder, downloaded by T8.4ae. At start, before any llama.cpp call:

1. If the CUDA switch is off: nothing changes.
2. If it is on: every file in the folder is checked against its SHA-256, because the folder is writable by the user, then the app loads that folder's `llama.dll` by full path with `LoadLibraryExW` (`LOAD_WITH_ALTERED_SEARCH_PATH`), so its own `ggml*.dll` win over the ones beside the exe.
3. The app's own exe tries the folder in a throwaway child process (see "A card with no matching code"). Only if that child ran on the GPU does the app load the folder itself.
4. A failure at any step leaves the app on Vulkan, and it says so. Never a silent CPU run reported as CUDA.

`llama.dll` is delay-loaded already (T6.21b), so a load with the same name is the one the exe's calls bind to. [Certain: T8.4ada, measured in the real app's replay with the backend from llama.cpp's own log]

**A DLL cannot be unloaded, so a switch needs a restart.** The Settings page says so and offers to restart when no meeting is on.

**Transcription.** Parakeet on CUDA uses ONNX Runtime's own CUDA library and cuDNN. It covers every generation already, so nothing is built here. Its files are the optional second part. ONNX Runtime is one library per process, so this switch also applies at the next start.

Measured on T8.4ad, the shipped exe with PATH holding only Windows and the folder as the only copy of NVIDIA's files, the backend read from ONNX Runtime's placement log:

- `ort/cuda` adds nothing to the exe's imports: ort loads ONNX Runtime at run time, so its CUDA feature only adds the provider's options.
- cuBLAS needs no help. `onnxruntime_providers_cuda.dll` imports it and it is found beside that DLL.
- cuDNN does. It is loaded by bare name at run time, which does not look in that folder: without help the provider refuses every Parakeet session ("cuDNN is unavailable"), and the meeting runs on the CPU, reported. With only `cudnn64_9.dll` loaded in advance the whole process ends (0xC0000409). So all ten files are loaded by full path before ONNX Runtime, each after what it imports (`cuda::preload_for_provider`), and nothing goes on PATH.
- `cudart64_13.dll` is not loaded and not needed.
- A Whisper meeting with the switch on also loads the ONNX Runtime CUDA library and cuDNN, for the helpers' CPU sessions, and runs on the CPU or Vulkan.

On the 80 s two-track replay (as fast as it reads, summary on Vulkan included), three runs each on the RTX 5080:

| | Process CPU, s | Wall, s | Peak working set, MB | Peak private, MB |
|---|---|---|---|---|
| Parakeet on the CPU | 36.3, 31.0, 30.4 | 22.8, 18.4, 18.1 | 5,947 | 7,144 |
| Parakeet on CUDA | 25.8, 25.4, 26.0 | 15.9, 15.1, 15.6 | 6,880 | 9,510 |

This is one short replay, not a meeting in real time; the plan's copy needs T8.4af's own measurement on a real meeting replay.

**The folder is tried first, in a child,** as llama.cpp's set is: `--cuda-transcription-probe --models <dir>` (`transcription_probe.rs`) chooses CUDA, loads the smallest installed Parakeet model there and decodes 2 s of a generated tone, never audio from a recording. Only a clean exit naming CUDA, with no session left without CUDA nodes, counts (a session may still place some of its nodes on the CPU). If the library then fails to load in the app anyway, CUDA leaves the order for the rest of that session (`cuda::record_load_failure`), so it is reported once.

**The switch at start** (`cuda_settings::begin`, T8.4af). With the stored switch on, the app closes two gates and returns at once, so the window and tray never wait: llama calls wait in `earshot_llm_local::gate`, ONNX Runtime loads and anything about to read the order for one wait in `cuda::wait_for_onnx` (`host::settled_order`). On its own thread it verifies the summaries part (every byte), tries it in a child and loads the llama.cpp set; then it tries the transcription part in a child, which reads every byte first, and chooses ONNX Runtime's folder, which reads them again. A recording never waits, because a setting never costs a recording: a meeting started in that moment records at once and holds its models until the decision lands (T9.1l, `earshot_runtime::meeting_queue`), so it runs on what the start chooses, and its audio queues meanwhile; the trial's own limit (`START_TRIAL_LIMIT`) bounds how long. Summaries are unaffected: one written after the meeting can still wait at llama.cpp's gate. A refusal is one note per part, and `accelerator_order` holds CUDA only after the choice.

**The model check and the choice.** The check tests what a run can use, so its child repeats the parent's choice (`--cuda-transcription`) and only then reads the order. The order is in the check's fingerprint, so the first start that chooses CUDA finds the stored answer out of date and runs the check (`check_after_start`, only when the model is automatic), which gives way: a meeting starting beside it cancels it and is never refused. That is the only check CUDA starts: a finished download starts none, because the switch can only be turned on once the files are there, and it applies at the next start. The folder's state is not in the fingerprint: without the choice nothing the check reads comes from the folder, and with it the child checks every byte, so a check may also start beside a part's download. While the start runs, a check, a voice sample and a benchmark are refused and can be tried again, because each would wait for it holding a pass that refuses meetings.

**A bug in ort this works around.** ort 2.0.0-rc.14 marks its library slot as set even when loading fails, and every later ort call then reads uninitialised memory (`util/once_lock_std.rs`). So `earshot_onnx::init` runs ort's own checks on a throwaway load first (`ort::util::probe`), and a library refused there leaves the CPU library free to load.

**A card with no matching code.** Measured on T8.4ada with a set built for `75-real;86-real` only, on the RTX 5080: the DLLs and the model load, and the first GPU call aborts the whole process (0xC0000409, `ggml-cuda.cu:109: CUDA error`). No error reaches Rust, so an in-process check cannot catch it. In the real app the meeting's recording and final transcript were already stored and only the summary was lost, but the app would die on every summary.

So a folder is tried first in a child of the exe: `--llama-probe <folder> --model <gguf>` (`llama_probe.rs`) loads the set and the smallest installed summary model on the GPU and decodes 4 tokens. A crash, a non-zero exit, a timeout or no named backend all refuse the folder. It covers future cards and the generations never run here alike, because it asks the card rather than a table. Measured with the model in the disk cache: 1.8 s when it works, 1.7 s for the crash, 0.0 s for missing cuBLAS. A cold disk will be slower. [Likely: not measured]

To rebuild the gap set: `cargo build --release -p earshot-llm-local --features cuda --target-dir C:\eb\cu4` with Ninja, the Visual Studio x64 environment, `CUDA_PATH` at v13.4 and `CMAKE_CUDA_ARCHITECTURES=75-real;86-real`.

## What a user downloads

| Part | From | Download | On disk |
|---|---|---|---|
| Summaries: our CUDA set | GitHub releases (`pitslug/Earshot`) | 57.7 MB | 74.7 MB |
| Summaries: cuBLAS | NVIDIA | 423.6 MB | 547.8 MB |
| Transcription, optional: cuDNN | NVIDIA | 1,265.9 MB | 602.7 MB |
| Transcription, optional: ONNX Runtime's CUDA build | GitHub (Microsoft's release) | 309.5 MB | 214.9 MB |

Every row is exact (`models/manifest.toml`, T8.4ae; licence files included), so Summaries is 481.3 MB to download and 622.5 MB on disk, and Transcription 1,575.4 MB and 817.7 MB. A zip stays on disk until its files are extracted, and archives go one at a time, so a Transcription download needs about 1.9 GB free at its peak and leaves 818 MB.

The set's DLLs must match earshot-llm-local's `llama-cpp-sys-2` pin, so upgrading that pin needs a new release of the set. `cuda_llama_set_matches_the_llama_cpp_sys_pin` fails until the manifest names one; it compares the version in the name, not the DLLs.

## What this needs from the owner

- **The set is published**: [cuda-set-llama-0.1.160-2](https://github.com/pitslug/Earshot/releases/tag/cuda-set-llama-0.1.160-2), no new host in the manifest. Its DLLs are those of the first release; only the link in `NOTICE.md` changed, and because that file is pinned, an existing install shows as partial and downloads the set once more. It carries no conversation content.
- **The licence question.** That set links NVIDIA's runtime statically, which `docs/cuda-libraries.md` already asks NVIDIA about; the written answer stays open before a public release.

## Risks

- **Only RTX 5080 tested.** 7.5 and 8.6 code is built and never run here. The trial stops such a card crashing the app; it does not show the summaries are as good there.
- **The trial covers start-up, not everything after it.** A set that passes 4 tokens could still abort later, for example on a kernel a short prompt never reaches. T9.3 contains that: a summary on the card runs in its own process, so such an abort costs only that summary, which moves to the processor and says so. Fake children prove the move; no real card has aborted here on a real summary. Each summary's child loads the folder again, so the parent first checks its names and sizes (no unpinned library, every summary file its size) and on a failure runs that summary on the processor with a note. It reads no bytes per summary: the start's full verify and trial cover those.
- **Toolkit:** 13.5 now sits beside 13.4, and Visual Studio's build rules follow the newer one. Release builds of this set use Ninja with `CUDA_PATH` at 13.4, the version cuDNN 9.28 lists.
- **What transcription on CUDA gains:** measured 2026-11-06 on the RTX 5080, a two-track recording replayed as fast as it reads, shipped exe, three runs each way, no summary (its port closed, so each run ended on that error): process CPU 150.7, 150.8, 151.6 s without and 101.3, 102.0, 100.5 s with; wall 37.4, 36.0, 36.0 s without and 28.3, 27.3, 27.2 s with; peak private memory 7.7 GB without and 12.4 GB with; peak working set 7.2 GB without and 3.4 GB with. Speaker work and voice detection stay on the CPU in both. One recording and one card, replayed rather than live, so a real-time meeting's share is not measured.
- **Run time:** the probe's first Vulkan prompt took 5.5 s against 0.06 s on CUDA, on one short prompt that probably includes first-use setup. T8.4ac's 20 meetings are the real measure: 91.6 s per hour of meeting.
