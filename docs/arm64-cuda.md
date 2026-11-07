# CUDA on Windows on Arm64 (T8.4b, D23)

Research, first written 2026-11-05 and re-checked against NVIDIA's pages and the upstream release lists on 2026-11-06. Nothing was downloaded, installed, built or run for this note: toolkit folders and crate sources already on the desktop were only read. Sizes use 1 MB = 1,000,000 bytes.

## Recommendation

Do not offer the CUDA download on ARM64 for now; keep the ARM64 install at QNN > CPU. Two reasons decide it:

1. **Transcription cannot use it.** ONNX Runtime publishes no Windows ARM64 CUDA library, so Parakeet stays on the CPU, and the shipped app offers CUDA for transcription only to Parakeet. Only the summaries part of T8.4a could exist. [Certain]
2. **Nobody here can run it**, and PLAN says it never ships as working without a run on real hardware. An N1X machine has no Hexagon NPU either, so it would run at CPU speed for transcription whatever we choose. [Certain for the rule; Likely that N1X has no QNN path, since QNN needs a Snapdragon]

**Revisit when** someone with an RTX Spark machine can test. At that point try Vulkan summaries first: it would need no NVIDIA libraries and no licence question, and llama.cpp publishes a Windows ARM64 Vulkan build. Today's rule keeps Vulkan off ARM64 (AGENTS.md), and whether NVIDIA's ARM64 driver gives a usable Vulkan device for llama.cpp is unestablished. [Guessing]

The choice is the owner's (T8.4). The compile-only probe is not run: see "Probe".

## NVIDIA's position

| Question | Answer | Tag | Source |
|---|---|---|---|
| Is it real? | Yes. 13.4 adds Windows on Arm, for the "N1X Laptop ecosystem" | [Certain] | [NVIDIA blog](https://developer.nvidia.com/blog/cuda-toolkit-13-4-adds-windows-on-arm-support-and-greater-control-over-shared-gpus/) |
| Still a preview? | No. PLAN's "developer preview" is out of date: 13.4.1 is the general release and supersedes the 13.4.0 preview | [Certain] | [13.4 notes PDF](https://docs.nvidia.com/cuda/archive/13.4.1/pdf/CUDA_Toolkit_Release_Notes.pdf) |
| Current toolkit | 13.5 lists arm64 (Windows) for most components, cuBLAS 13.10.0.20 among them | [Certain] | [13.5 notes](https://docs.nvidia.com/cuda/cuda-toolkit-release-notes/index.html) |
| Driver | RTX Spark on Windows: 616.41 or later. Not bundled with the toolkit since 13.1. 13.5's Windows branch is R620 | [Certain] | [13.5 notes](https://docs.nvidia.com/cuda/cuda-toolkit-release-notes/index.html) |
| GPU target | N1X and GB10 are compute capability 12.1, so `121a-real` | [Certain] | [NVIDIA GPU list](https://developer.nvidia.com/cuda-gpus) |
| Known issues naming Windows on Arm | None in the 13.5 notes | [Certain that none is listed] | [13.5 notes](https://docs.nvidia.com/cuda/cuda-toolkit-release-notes/index.html) |
| Toolkit on the desktop | 13.4.1 holds `bin\arm64` and `lib\arm64`; 13.5.0 is installed beside it | [Certain, read here] | Local folders |
| Machines on sale | Press reports first RTX Spark machines shipping from October 2026. I did not confirm a retail date from NVIDIA or a maker | [Likely] | [Windows Central](https://windowscentral.com/hardware/nvidia/nvidia-confirms-rtx-spark-configurations-and-availability-first-devices-expected-to-begin-shipping-as-soon-as-next-month-with-two-n1x-configs-on-offer) |

**NVIDIA's documents do not agree.** The 13.5 Windows installation guide lists x86_64 as the only native compiler target and marks cross-compilation unsupported, though only as 32-bit on 64-bit, and says nothing on Arm64 hosts ([guide](https://docs.nvidia.com/cuda/cuda-installation-guide-microsoft-windows/index.html)). cuDNN's support matrix does not mention Windows arm64 ([matrix](https://docs.nvidia.com/deeplearning/cudnn/backend/latest/reference/support-matrix.html)), yet NVIDIA publishes a `windows-arm64` cuDNN archive of 1,029.9 MB ([redist JSON](https://developer.download.nvidia.com/compute/cudnn/redist/redistrib_9.28.0.json)). A listed archive does not prove a supported path. [Certain that they differ]

## The three engines

**llama.cpp: built upstream and run on the hardware.** [Likely, by the PR's own account]
- Prerelease b12167 (2026-11-06) ships `llama-b12167-bin-win-cuda-13.4-arm64.zip` (158.0 MB) and `cudart-llama-bin-win-cuda-13.4-arm64.zip` (153.3 MB) ([releases](https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=3)). [Certain]
- `ggml-cuda.dll` is cross-compiled on an x64 host with MSVC and the x64-hosted CUDA toolchain against ARM64 import libraries, and is loaded by clang-built ggml and llama. It was run on a native RTX Spark system with TinyLlama 1.1B ([PR 26650](https://github.com/ggml-org/llama.cpp/pull/26650), merged 2026-08-10). That PR used the preview archives and said it would need updating at general release; whether it has been, I did not establish.
- **Fit with Earshot:** our llama.cpp is already DLLs and T8.4ada already builds a separate CUDA set, so this method matches our shape. The pinned `llama-cpp-sys-2` 0.1.160's ggml CUDA CMake handles only ARM64EC specially, so the build is unproven. OpenMP also needed care upstream ([issue 25030](https://github.com/ggml-org/llama.cpp/issues/25030)). [Certain for what the CMake says]

**whisper.cpp: built upstream; Earshot's pinned copy would not link as it stands.**
- Prerelease b5454 (2026-10-06) ships `whisper-bin-win-cuda-13.4-arm64.zip` (291.2 MB) with an empty release body; prerelease is its only status mark ([b5454](https://api.github.com/repos/ggml-org/whisper.cpp/releases/tags/b5454)). I found no report of it running on the hardware. [Certain for the listing]
- The pinned `whisper-rs-sys` 0.15.0's `build.rs` adds `lib/x64` as its CUDA link search on a Windows host, whatever the target, so an ARM64 link looks for x64 libraries. Its ggml CUDA CMake has no ARM64 handling. [Certain, read here]
- Earshot's ARM64 build is one clang-cl and Ninja CMake project with ggml static inside the exe (T2.6). llama.cpp's method relies on a separately built MSVC DLL, which that build does not have. [Likely that this is the harder failure]
- **It matters less than it looks:** the shipped x64 app gives whisper no CUDA at all. `model_order` offers CUDA only to Parakeet (AGENTS.md, T8.4af). So ARM64 needs no whisper CUDA unless whisper becomes the transcription path on N1X, which would be a new decision.

**ONNX Runtime: nothing published.**
- v1.31.1 (2026-10-26) has `onnxruntime-win-arm64`, `-win-arm64x`, `-win-x64`, and `-win-x64-gpu_cuda12` and `_cuda13`: no Windows ARM64 CUDA build ([assets](https://api.github.com/repos/microsoft/onnxruntime/releases/latest)). [Certain]
- I found no report of a build from source. [Certain that I found none]

## What Earshot's code would need

- `cuda_switch::offered` refuses anything but x86_64. [Certain]
- `earshot_hardware::set_holds_code_for` accepts major 12 only at exactly 12.0, so N1X (12.1) is refused, correctly: the x64 set has no `121a-real`. An ARM64 set is its own build with its own list. [Certain]
- `MIN_NVIDIA_DRIVER_RELEASE` is 580. RTX Spark needs 616.41 or later, so an ARM64 offer needs its own floor. [Certain]
- `set_holds_code_for` reads NVML through `earshot_hardware`, and whether NVML reports an N1X like a discrete card is unknown. An integrated N1X may also fail the discrete-GPU rule of `summary_placement`. [Guessing]
- A new `[[cuda_archive]]` set, a second GitHub release and the two-parser manifest changes. About a day of work plus the owner's hardware. [Guessing at the time]

## Redistribution

- The CUDA EULA's Attachment A lists files by name with no architecture, and its text does not mention Windows arm64 ([EULA](https://docs.nvidia.com/cuda/eula/index.html); `EULA.txt` in the 13.4 toolkit has no "arm64" or "aarch64"). The arm64 archives carry the same licence label ([redist JSON](https://developer.download.nvidia.com/compute/cuda/redist/redistrib_13.5.0.json)). So the x64 terms apply, including the open question on end users under Supplement 2.1. [Likely] Clauses to read: "Two tests" in `docs/cuda-libraries.md`. No new question arises for ARM64.
- llama.cpp publishes an ARM64 runtime zip holding `cudart64_13.dll`, `cublas64_13.dll` and `cublasLt64_13.dll` ([PR 26650](https://github.com/ggml-org/llama.cpp/pull/26650)). Practice, not permission.
- **Size if offered:** cuBLAS only, since no ONNX Runtime library would use cuDNN: 279.9 MB to download ([redist JSON](https://developer.download.nvidia.com/compute/cuda/redist/redistrib_13.5.0.json)), 217.3 MB on disk (`cublas64_13.dll` 24.2, `cublasLt64_13.dll` 193.1, read from the local 13.4.1). The ARM64 `ggml-cuda.dll` is on top; I have no measured size. [Certain for those files]

## Proven and not proven

| Claim | State |
|---|---|
| The toolkit and libraries exist for ARM64 | Documented, and on this disk |
| llama.cpp ARM64 CUDA upstream | Built and run, by others |
| whisper.cpp ARM64 CUDA upstream | Built by others, run not established |
| Earshot's pinned llama.cpp builds for ARM64 CUDA | **Unproven**: no probe |
| Earshot's pinned whisper.cpp builds for ARM64 CUDA | **Would need a change**: x64 link path in its build script |
| Anything in Earshot runs on an N1X | **Unproven**: no hardware |
| ONNX Runtime CUDA on ARM64 | **Does not exist** as a release |

## Probe

PLAN's T8.4b also names a compile probe. It was not run: the 2026-11-06 lane brief was research only. It needs nothing installed, since the toolkit is on the desktop, so it is proposed as its own task:

1. `cargo xtask build --target aarch64-pc-windows-msvc --cuda-set` with `CMAKE_CUDA_ARCHITECTURES=121`, once `--cuda-set` accepts that target.
2. Record whether the DLL set compiles, and the first failure if not.
3. Skip whisper: the link path above is already known, and no shipped path uses it.

A pass proves compilation only, never a run. Its value is low until a machine exists, so I would leave it until then.

## Could not establish

- Any report of a whisper or Parakeet workload on an N1X.
- How Vulkan or NVML behave on an N1X under Windows.
- Which toolkit the current llama.cpp ARM64 assets were built with: their names say 13.4 while the release notes link says 13.5.
