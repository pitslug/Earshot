# CUDA libraries for the x64 app (T8.4a first step, D23)

Research only, 2026-11-05. No host joins the manifest from this note: that is the owner's decision (PLAN.md section 7). I am not a lawyer; the clauses to read yourself are named below.

## Recommendation

No route is settled on the licence text, so ask NVIDIA in writing before a public release, and add no host until it answers. If it says yes to both questions, I prefer hosting a DLL-only zip on GitHub (route E): no new host, half the download, and the licensee is someone actually developing an application. Fetching from NVIDIA directly (route A) is second, because on the words of the licence an end user falls outside its scope. See "Two tests" and the ranking at the end.

For the owner's own machines nothing is blocked: they is a licensee developing an application, so either route passes for T8.4a's test runs. A new host in the manifest is still their call.

**Units:** in this note 1 MB is 1,000,000 bytes and 1 GB is 1,000 MB, for download and on-disk figures alike, because NVIDIA, PyPI and GitHub publish byte counts.

## Three things that change the plan

1. **The libraries are about 1.35 GB on disk, not 2 to 3 GB.** [Certain, measured here] See the table below. D23's estimate came from a CUDA 12.4 file list.
2. **The CUDA code inside the install will not be small.** [Likely] D23 assumes an app of about 40 MB. A release `earshot.exe` built for one GPU architecture (`C:\eb\host\release`, 2026-11-05) is already 82.0 MB, and that build's `ggml-cuda.dll` is 56.3 MB, because the GPU kernels are compiled in. Each extra real architecture adds roughly that kernel size to both copies of ggml. See "GPU architecture list".
3. **Having an NVIDIA GPU is not enough to make the offer.** [Certain] cuDNN's minimum is Turing, compute capability 7.5 ([support matrix](https://docs.nvidia.com/deeplearning/cudnn/backend/latest/reference/support-matrix.html)). A GTX 10 series card would download over a gigabyte for nothing. T8.4a's offer also needs a compute capability check, which `earshot_hardware::read()` would have to supply.

## What Earshot actually needs

From the import tables of the binaries on this machine (read with a small PE parser, nothing run or built):

| File | MB on disk | Why |
|---|---|---|
| `cublas64_13.dll` | 54.9 | Imported by `onnxruntime_providers_cuda.dll`, `ggml-cuda.dll` and the CUDA `earshot.exe` |
| `cublasLt64_13.dll` | 492.8 | Imported by `cublas64_13.dll` and the ONNX provider |
| 10 `cudnn*64_9.dll` files | 591.7 | ONNX Runtime's provider loads them at run time (LOG.md: refused without `cudnn64_9.dll`). Not in its import table |
| `onnxruntime.dll`, `onnxruntime_providers_cuda.dll`, `onnxruntime_providers_shared.dll` | 214.6 | The ONNX Runtime 1.31.0 CUDA build (zip 309.5 MB, already pinned in `models/manifest.toml`) |
| **Total** | **about 1,354** | |

- `cudart64_13.dll` is **not** needed: none of those binaries imports it, because cudart is linked statically (AGENTS.md says the same for ggml). [Certain for ONNX Runtime 1.31.0 and these ggml builds] A run-time load cannot be ruled out from an import table, so T8.4a's "nothing on PATH" run is the proof.
- Sizes are from toolkit 13.4.1 and cuDNN 9.26, the CUDA 13.4 variant. Other versions will differ. T8.4a measures the real set.

## Versions

- **CUDA:** ONNX Runtime documents that a CUDA 13.0 build needs CUDA 13.0 or newer and cuDNN 9.x, though its table stops at 1.30.x ([ONNX Runtime CUDA EP](https://onnxruntime.ai/docs/execution-providers/CUDA-ExecutionProvider.html)). The 1.31.0 zip's import names (`*_13.dll`) fit that. [Likely]
- **Latest NVIDIA releases:** CUDA 13.5.0, dated 2026-11-04, with cuBLAS 13.10.0.20 ([redist JSON](https://developer.download.nvidia.com/compute/cuda/redist/redistrib_13.5.0.json)); cuDNN 9.28.0.42 ([redist JSON](https://developer.download.nvidia.com/compute/cudnn/redist/redistrib_9.28.0.json)).
- **Pin 13.4.x, not 13.5, for now.** cuDNN 9.28 lists support for CUDA 13.0 through 13.4.1 and recommends 13.4.1 ([support matrix](https://docs.nvidia.com/deeplearning/cudnn/backend/latest/reference/support-matrix.html)). That is also the toolkit on the desktop (`version.json`: 13.4.1), which pairs with cuBLAS 13.7.0.27 ([13.4 notes](https://docs.nvidia.com/cuda/archive/13.4.1/cuda-toolkit-release-notes/index.html)). [Likely]
- **Driver:** existing CUDA 13.x applications run on drivers 580 or newer; new 13.4 features and platforms need R615 ([13.4 notes](https://docs.nvidia.com/cuda/archive/13.4.1/cuda-toolkit-release-notes/index.html), [minor version compatibility](https://docs.nvidia.com/deploy/cuda-compatibility/minor-version-compatibility.html)). The second page warns that applications compiling device code to PTX will not work on older drivers. It lists no Windows minimum for 13.x, and neither does the cuDNN matrix. [Certain that it is unlisted] Whether an old driver fails cleanly here is unproven.

## The licence terms

Both NVIDIA documents share one structure. **Read these clauses yourself** in the [CUDA EULA](https://docs.nvidia.com/cuda/eula/index.html) (also `EULA.txt` in the toolkit folder):

- **Section 1.1.1 item 3, the grant:** distributable portions may be distributed as incorporated, in object code, into an application.
- **Section 1.1.2, distribution requirements:** the application needs material additional functionality; distributable parts may be accessed only by your application; your own terms must be consistent with the EULA.
- **Section 1.2 item 2:** "you may not distribute or sublicense the SDK as a stand-alone product".
- **Section 1.2 item 7:** you indemnify NVIDIA for use outside the agreement.
- **Section 2.6, Attachment A:** lists `cublas.dll`, `cublasLt.dll`, `cudart.dll` for Windows. It explains that names carry a version (its example is `cudart64_90.dll`), so `cublas64_13.dll` is covered. [Likely]

The [cuDNN licence](https://docs.nvidia.com/deeplearning/cudnn/sla/index.html) (agreement and supplement dated 2022-02-22, the current page) names as distributable "the runtime files .so and .dll". So the 10 DLLs are covered, for an application on systems with NVIDIA GPUs (supplement 4.1). **Limitation 2.5** bars using the SDK in a way that subjects it to an open source licence. Read that one against the open project licence question in PLAN.md section 7: a copyleft choice could collide. [Likely]

**Neither document mentions hosting, downloading at run time or the internet** beyond the opening acceptance wording. [Certain, read for this note] So nothing states whether an in-app fetch counts as incorporation into an application, or whether a click in Earshot accepts NVIDIA's terms. A project that already fetches cudart and cuBLAS (not cuDNN) from NVIDIA marks the acceptance point as unverified ([hece PR 85](https://github.com/altintasutku/hece/pull/85)). Third-party, not guidance, and it does not raise the scope question below.

Microsoft's wording is the nearest precedent for the mechanism: its Windows ML components use cuBLAS, which an app acquires only after setting a flag accepting its licence ([Windows ML notices](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/third-party-notices)). So if a fetch happens, a visible accept step is the sensible shape. [Likely] That does not answer who the licensee is or what they may do.

EULA section 2.5 invites licence questions at nvidia-compute-license-questions@nvidia.com. I found no NVIDIA staff answer on this: the one forum thread has only the asker's posts ([forum](https://forums.developer.nvidia.com/t/redistribution-of-cuda-and-cudnn/190143)).

## Two tests, applied to both vendors

`docs/qnn-redistribution.md` rejects a separate Qualcomm download on these two tests, so they apply here too: who becomes the licensee, and is the grant scoped to developing applications?

**Read these three clauses yourself:** CUDA EULA Supplement 2.1 (License Scope) with the sentence opening that supplement that makes it prevail on conflict; EULA section 1.1.1 item 1; cuDNN Supplement 4.1.

| | NVIDIA | Qualcomm |
|---|---|---|
| Grant to use | 1.1.1 item 1 allows installing and using the SDK, with no purpose stated | 1(i) allows use and copying only to develop your applications |
| Scope limit | Supplement 2.1 and cuDNN 4.1: licensed for you to develop applications, only for systems with NVIDIA GPUs. The supplement prevails on conflict | The limit is in the grant itself |
| Distribution wording | Incorporated into an application (1.1.1 item 3), but Attachment A says files may be distributed with your applications. cuDNN's supplement has no such wording | Incorporated in your application, and never standalone |
| Fetch from the vendor: licensee | The end user | The end user |
| Fetch from the vendor: does the grant fit? | **No on the words:** the user develops nothing | **No** |
| App author hosts: licensee | The author, who is developing an application | The author, likewise |
| App author hosts: does the grant fit? | Scope yes; incorporation and standalone open | Scope yes; standalone excluded in terms |

**Answer: the same on scope, different in degree.** [Likely] A vendor fetch puts the end user outside the words of both licences. The item 1 wording does not rescue route A by itself: it is broader, but the supplement narrows it and wins on conflict. A kinder reading exists, that 2.1 is aimed at the NVIDIA hardware limit rather than at who may run the files. [Guessing] NVIDIA does run a public redistributable server and labels its PyPI wheels runtime libraries ([PyPI](https://pypi.org/project/nvidia-cublas/)), which suggests the intent, but neither is licence text.

**Why Qualcomm still gets a harder no.** Its limit is in the grant, and it excludes standalone distribution of the Software outright. NVIDIA's text leaves more room: Attachment A's distributed-with wording, a ban aimed at the SDK as a product, and a clause inviting questions. So asking NVIDIA is worth doing, and these two routes are open questions here rather than rejections. [Likely]

**Questions for NVIDIA, one email:**

1. May an application fetch the Attachment A files and cuDNN's DLLs from NVIDIA's server at the user's request, and on what terms does the user hold them?
2. May the developer host a zip of only those files, public but used only by that application?

## Our own CUDA set, hosted publicly, for an open source Earshot

Added 2026-11-06, read from `EULA.txt` in toolkit 13.4 and from the built files. Not legal advice. This narrows the "E" route above: the set and NVIDIA's own DLLs are different questions, and this note had treated them as one.

**What the set holds.** [Certain, read from the import table of `C:\eb\cu3\cuda-set\ggml-cuda.dll` and llama.cpp's `ggml-cuda/CMakeLists.txt`] llama.cpp's code and GPU kernels compiled by us, with `cudart_static` linked in. It imports `cublas64_13.dll`, so cuBLAS stays a separate file; a comment in that CMake file says the Windows toolkit offers no static cuBLAS, and Attachment A's Windows row lists none.

| Question | Text | Reading |
|---|---|---|
| May the static runtime ship? | Attachment A names `cudart_static.lib` on Windows; 1.1.1 item 3 grants distribution as incorporated in object code into an application | Yes. It is the most literal case the grant describes. [Likely] |
| Is it the SDK as a stand-alone product (1.2 item 2)? | The set is a compiled component of Earshot and is useless alone | No. [Likely] Contrast a zip of NVIDIA's own DLLs, which is the shape that clause is aimed at |
| Does separate hosting matter? | 1.1.1 says incorporated into an application; Attachment A says distributed with applications; neither mentions downloads | Open but small: the set is part of the application, delivered in two pieces. [Likely] |
| Open source allowed? | 1.1.6 allows developing and testing applications under OSI-approved licences | Yes for the project. The words are "develop and test", not "distribute". [Certain on the words] |
| Which licence? | 1.2 item 5 bars use that would subject the SDK to an open source licence, with source-disclosure and free-redistribution terms as examples | Permissive (MIT or Apache-2.0) fits. GPL or AGPL over a file with `cudart_static` linked in is the collision, unless a linking exception carves it out. [Likely] |

**Duties that come with it, all on the publisher:**

- 1.1.2 item 5: distribution terms must be consistent with the EULA, so a notice must say the NVIDIA parts are not under Earshot's licence and name NVIDIA's terms. A blanket MIT grant over the set would be wrong.
- 1.1.2 item 2: distributable parts accessed only by your application. Any program can load a DLL from a user folder. No redistributor can meet this in the strict sense, including the ones below. [Guessing that it matters]
- 1.2 item 1: keep NVIDIA's proprietary notices. 1.2 item 3: never suggest NVIDIA endorses Earshot.
- 1.2 item 7: an indemnity to NVIDIA for use outside the agreement. It is uncapped and personal unless an entity publishes. The strongest reason to get the answer in writing.

**Practice, which is not permission:**

- llama.cpp (MIT) publishes CUDA builds and a separate `cudart-llama-bin-win-cuda-13.4-x64.zip` of 423 MB on GitHub ([b12144](https://github.com/ggml-org/llama.cpp/releases/tag/b12144)); its workflow copies cuBLAS into a separate archive. That is the harder case than ours, and it has run for years. No licence discussion turned up in its issues.
- Ollama (MIT) publishes a 1.6 GB Windows zip on GitHub, and its [release workflow](https://github.com/ollama/ollama/blob/main/.github/workflows/release.yaml) installs NVIDIA's cudart, cuBLAS and cuDNN to build it. I did not open the zip.
- KoboldCpp (AGPL-3.0) publishes a 622 MB `koboldcpp.exe`, also without opening it here. So copyleft projects do this too; that does not make the clause go away.
- NVIDIA's [September 2025 post](https://developer.nvidia.com/blog/developers-can-now-get-cuda-directly-from-their-favorite-third-party-platforms/) says it is opening redistribution to named platforms and that more are coming. It says nothing about application authors, and those platforms may have their own deals. [Guessing about the deals]
- A [Debian discussion](https://lists.debian.org/debian-ai/2020/12/msg00037.html) reached no answer and suggested asking NVIDIA. Found no NVIDIA statement on any of this.

**Decision:** published without waiting for NVIDIA's answer, with Earshot's own code under MIT. cuBLAS and cuDNN stay on NVIDIA's server.

**Conclusion.** Hosting the set publicly under a permissive licence with a notice looks permitted on the words, and no worse than what larger MIT projects already do. [Likely] cuBLAS and cuDNN keep the open questions in the routes below. Still send the email: it is cheap, and the indemnity makes a written answer worth having.

## The routes

Download sizes are for current versions on x64. Add the 309.5 MB ONNX Runtime zip to each.

| Route | Download | New hosts | Published by | Terms position |
|---|---|---|---|---|
| A. NVIDIA redist server | cuBLAS 426 MB + cuDNN 1,266 MB | `developer.download.nvidia.com` | NVIDIA, SHA-256 in its JSON | No hosting by the owner, but the end user becomes the licensee and develops nothing: outside Supplement 2.1 on its words. Whether the fetch is the owner distributing is also unstated. Needs NVIDIA's answer |
| B. PyPI wheels | `nvidia-cublas` 427 MB + `nvidia-cudnn-cu13` 406 MB | `files.pythonhosted.org` | NVIDIA's installer team, SHA-256 in PyPI's JSON | As A, but breaks AGENTS.md section 9 (PyPI never at run time) |
| C. conda-forge | Not measured | `conda.anaconda.org`, probably more | conda-forge packaging | NVIDIA's licences by reference; `.conda` needs a new extractor |
| D. NuGet | n/a | `api.nuget.org` | No NVIDIA-owned package found | Third-party repackaging: rejected |
| E. The owner hosts, on GitHub | About 830 MB (a guess from the sizes above) | None | The owner | The owner is the licensee and is developing an application, so scope fits. But section 1.1.2, standalone and the indemnity all apply to them. Needs NVIDIA's answer |

Sources: [cuBLAS on PyPI](https://pypi.org/pypi/nvidia-cublas/json) (`LicenseRef-NVIDIA-Proprietary`), [cuDNN on PyPI](https://pypi.org/pypi/nvidia-cudnn-cu13/json) (no licence metadata), [conda-forge cuDNN](https://api.anaconda.org/package/conda-forge/cudnn), [conda-forge cuBLAS](https://api.anaconda.org/package/conda-forge/libcublas), [NuGet search](https://www.nuget.org/packages?q=cublas+nvidia).

Notes on the routes:

- **Why A's cuDNN is big:** the archive is 1,266 MB against 592 MB of DLLs we use, so it holds more than the DLLs, and the PyPI wheel is 406 MB. [Guessing: libraries and headers] I downloaded neither, so **no archive's contents or layout is verified**.
- **A's availability is good but not promised:** the manifests go back to 13.0.0 and cuDNN 9.0.0, but there is no `redistrib_13.4.0.json`, the preview, and the 13.4.1 to 13.4.3 files all carry one later date ([CUDA index](https://developer.download.nvidia.com/compute/cuda/redist/), [cuDNN index](https://developer.download.nvidia.com/compute/cudnn/redist/)). [Certain as of today] So NVIDIA can withdraw or rewrite a manifest. Pin from general releases only, keep URL and SHA-256 in our own manifest rather than reading NVIDIA's at run time, and treat a vanished archive as a retryable download failure. Whether the preview's archives themselves are gone, I did not check.
- **E's precedent:** llama.cpp publishes a separate runtime zip of 423.5 MB, `cudart-llama-bin-win-cuda-13.4-x64.zip` ([release b12125](https://github.com/ggml-org/llama.cpp/releases/tag/b12125)). That shows practice, not permission, and per [PR 26650](https://github.com/ggml-org/llama.cpp/pull/26650) it holds cudart and cuBLAS, not cuDNN. A public release asset anyone can fetch is the shape the standalone clause is aimed at, so E needs NVIDIA's written answer first. [Likely] E's other dependency is a public release host (T9.1b's open question): another host than GitHub would be a new one.
- **The only route with no open licence question** is shipping the DLLs inside the installer, D23's first form, which the owner dropped on size. [Likely] It is not on the ranking because of that decision, but it is the fallback if NVIDIA says no to both.
- **Redirects: none.** Observed 2026-11-06 (T8.4ae): `developer.download.nvidia.com` answers HEAD and GET of both pinned zips directly (AkamaiNetStorage), so it adds no redirect host. `Accept-Ranges: bytes`; a Range request gets 206 with a matching `Content-Range`. Both are plain zips, deflate, not zip64, unencrypted, so the app's extractor reads them. Layout: `<archive>/bin/x64/*.dll`, `<archive>/LICENSE`, plus headers and import libraries (cuDNN's are most of its size, 2.1 GB uncompressed for one static library), which the app never writes. Each LICENSE was read and agrees with this note's clauses (CUDA EULA "Last updated: January 26, 2026"; cuDNN agreement and supplement v. February 22, 2022).
- **The user-writable folder:** the rule that only your application may access the distributable parts sits awkwardly with DLLs any program could load from `%LOCALAPPDATA%`. T8.4a's per-file SHA-256 check protects Earshot, not that clause. [Guessing that it matters] Installing per user is ordinary practice, but read item 2 of section 1.1.2.

## GPU architecture list

`CMAKE_CUDA_ARCHITECTURES` today is `120` (ggml turns it into `120a`), so a shipped build would run only on RTX 50 cards.

| Cards | Compute capability ([NVIDIA list](https://developer.nvidia.com/cuda-gpus)) |
|---|---|
| RTX 20, GTX 16 | 7.5 |
| RTX 30 | 8.6 |
| RTX 40 | 8.9 |
| RTX 50 | 12.0 |
| RTX Spark N1X (ARM64 only) | 12.1 |

- **ggml's own default** on CUDA 13 (non-native): `75-virtual 80-virtual 86-real 89-real 120a-real 121a-real`, plus `90-virtual` in llama.cpp's copy (vendored `ggml-cuda/CMakeLists.txt`). llama.cpp's Windows x64 CUDA 13.4 zip, built with some such list, is 170.8 MB against 158.4 MB for ARM64 ([release assets](https://api.github.com/repos/ggml-org/llama.cpp/releases/tags/b12125)).
- **Binary rules:** a real binary runs on the same major version with an equal or newer minor, never across a major; PTX can be compiled on the fly for newer GPUs ([programming guide 13.1](https://docs.nvidia.com/cuda/archive/13.1.1/pdf/CUDA_C_Programming_Guide.pdf), as summarised by search, [Blackwell guide](https://docs.nvidia.com/cuda/blackwell-compatibility-guide/)). So `86-real` should also run on RTX 40. [Likely]
- **Avoid virtual targets for shipping.** They mean a compile on first run and, per the minor version page above, fail on older drivers. [Likely]

**Proposed x64 list: `75-real;86-real;120a-real`.** Add `89-real` only if T8.4a measures a real speed gain. `121a-real` is for an ARM64 build only.

**Measured 2026-11-06 (docs/cuda-install.md):** three real architectures make `ggml-cuda.dll` 70.2 MB, about 7 MB per extra generation, and the shipped exe will hold no CUDA code. The estimate below was far too high and is kept as written.

**Cost.** [Guessing, from one data point] About 55 MB uncompressed per real architecture, per copy of ggml, and there are two copies (whisper's static one in the exe, llama.cpp's DLL). Three architectures add about 220 MB to the install's disk size over today's one-architecture build, giving an install of roughly 400 to 500 MB. That is not D23's 40 MB. Two ways out, for the owner to weigh after T8.4a measures:

1. Put llama.cpp's `ggml-cuda.dll` in the CUDA download rather than the install. It is already a separate DLL, so this looks cheap. [Likely]
2. Whisper's kernels are harder: they are static in the exe, so moving them needs a change to how whisper is built. [Likely]

**Untested risk:** a future GPU with no matching kernel. Without PTX, what ggml does then is unknown. It must fall to Vulkan or the CPU through the existing refusal path, never crash. T8.4a should test it, for example with a build lacking the desktop's own architecture. [Guessing at the behaviour]

## Options, ranked

All of these wait on NVIDIA's answer before a public release. The order changed after applying the two tests: E was third and A first.

1. **E. The owner hosts on GitHub.** The only fetch route where the licensee is developing an application, with no new host, which is the strongest privacy story, and the smallest download (about 1.14 GB including ONNX Runtime, a guess). Cost: The owner becomes the distributor, with the indemnity, the standalone clause is unresolved, and cuDNN lacks the distributed-with wording.
2. **A. NVIDIA direct, with an accept screen.** NVIDIA carries the hosting, published checksums, one new host, 2.0 GB including ONNX Runtime. On the words of Supplement 2.1 an end user is outside the licence. First choice instead if NVIDIA confirms this route and the owner would rather not be a distributor.
3. **B. PyPI wheels.** Smallest NVIDIA-published download (1.14 GB) and one new host, with A's scope problem. The owner would also have to lift their rule that PyPI is never used at run time, and the cuDNN package carries no licence metadata.
4. **C. conda-forge.** More hosts, a new archive format, A's scope problem, no advantage.
5. **D. NuGet.** No NVIDIA-owned package.

## Could not establish

- Whether an in-app click counts as accepting NVIDIA's licences.
- Whether Supplement 2.1 and cuDNN 4.1 leave an end user who only runs an application within the licence after fetching the files from NVIDIA, and whether such a fetch counts as the app's author distributing.
- The contents of any wheel, and redirect and Range behaviour on the routes other than A (A: see "Redirects: none" above).
- A documented Windows driver minimum for CUDA 13.x.
- Which GPU architectures Microsoft's ONNX Runtime CUDA build covers.
