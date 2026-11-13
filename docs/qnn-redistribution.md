# Redistributing the QNN libraries (T8.4, PLAN.md section 7)

Research only, 2026-11-05. I am not a lawyer. Read the clauses named here yourself: the licence is `Qualcomm_LICENSE.pdf` in `%LOCALAPPDATA%\Earshot\models\onnxruntime-qnn\win-arm64-2.7.0`.

## Recommendation

Sizes in this note use 1 MB = 1,000,000 bytes.

Bundle the QNN libraries inside the ARM64 installer, and do not download them separately. The licence allows bundling, on terms you should read first. A separate download is the route it excludes. Keep Windows ML as a spike on the Surface, not the plan: it would remove the licence question, but it takes away the pinned binaries your privacy check depends on, and Microsoft does not list your Surface's chip.

While the install is only for your own machines there is nothing to decide, because you accepted the licence for development on 2026-10-24 and distribute to no one. The question bites on the first public release.

## What the licence says

Read from the PDF in the 2.7.0 zip. A public copy is on [Dell's site](https://www.dell.com/support/manuals/en-us/dell-pro-ai-studio/dpais_license/qualcommai-stack-license-terms-and-conditions-of-use?guid=guid-b08f0ec7-55e9-47bd-b8a5-a9a2c55afd23&lang=en-us). The sections I compared agree, but I did not compare the whole text. `onnxruntime_providers_qnn.dll` itself is MIT (`docs/qnn.md`), so only the QAIRT libraries are in question.

| Clause | Effect | Read it because |
|---|---|---|
| 1(iv) and last sentence of 1 | Distribution and sublicensing only in object code, incorporated in your application, never "on a standalone basis" | This is the whole answer to the question |
| 2(a), 2(b) | No reverse engineering; keep notices | Ship the licence and notices files with the app |
| 2(d) | Banned uses, including emotion recognition at work and biometric categorisation | See "Voiceprints" below |
| 2(e) | Biometric identification and employment are high-risk uses that Qualcomm advises against | See "Voiceprints" below |
| 8 | Qualcomm may end the licence at any time without cause; you must then delete all copies | A published installer can lose its right to exist on notice |
| 9(a) | You indemnify Qualcomm for your use, including posting apps on download sites or stores | Open-ended, personal if you publish as an individual |
| 10(a), 10(c) | No assignment; California law, San Diego courts | Matters if the project ever changes hands |

**Bundling: permitted.** [Likely] A DLL shipped beside the exe in your installer is the natural reading of incorporation in your application, though the licence does not define the term. Microsoft treats bundling Qualcomm's NuGet package as a normal route ([bring your own EPs](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/bring-your-own-eps)). That is Microsoft's view of the mechanics, not Qualcomm's permission.

**Downloaded by the app: not permitted in the forms available.** [Likely]
- From a host of yours: that is distributing the libraries on their own, which the last sentence of section 1 excludes.
- From Qualcomm's GitHub release: you distribute nothing, but the user becomes the licensee, and the grant covers developing applications, not running someone else's. It fits worse than bundling, not better.

`docs/cuda-libraries.md` ("Two tests") applies the same two tests to NVIDIA and explains why its routes stay open questions while these are rejected: NVIDIA's scope limit sits in a supplement beside a broader use grant, and its distribution wording leaves more room. [Likely]

**Neither the [plugin's README](https://github.com/onnxruntime/onnxruntime-qnn) nor its [NuGet page](https://www.nuget.org/packages/Qualcomm.ML.OnnxRuntime.QNN) addresses this.** Both show MIT, with no separate statement about the QAIRT libraries. [Certain] So don't read MIT on the package as covering them; `docs/qnn.md` found the split from the zip's own files.

## Voiceprints

`docs/qnn.md` says no banned use applies to transcription. That holds, but the app does more. [Likely]

- Clauses 2(d) and 2(e) reach use connected with the listed applications, not only the work the Software itself does, which is wider than what the NPU runs.
- Earshot enrols voiceprints and names speakers (T7.4h, T6.25). The NPU runs only Parakeet; speaker models stay on the CPU. Still, biometric identification is on the 2(e) list, and meeting notes at work are close to the employment item.
- 2(e) is advice rather than a ban, as I read it. The biometric categorisation item in 2(d) is limited to sensitive characteristics, so it looks aimed at something else: Earshot matches a voice to a person, it does not sort people by trait. Your call; I would not treat this as blocking.

## What would ship

From the 2.7.0 zip, sizes measured here:

| File | MB |
|---|---|
| `onnxruntime_providers_qnn.dll` (MIT) | 3.9 |
| `QnnHtp.dll` | 3.7 |
| `QnnHtpPrepare.dll` | 92.8 |
| `QnnSystem.dll` | 3.6 |
| `QnnHtpV73Stub.dll`, `libQnnHtpV73Skel.so` | 13.6 |
| `QnnHtpV81Stub.dll`, `libQnnHtpV81Skel.so` | 14.8 |
| **Total** | **about 132** |

These are the files `models/manifest.toml` pins for the plugin. Plus `Qualcomm_LICENSE.pdf`, `LICENSE` and `ThirdPartyNotices.txt`. The zip also holds `libqnnhtpv73.cat` and `libqnnhtpv81.cat` (about 12 KB each), the skels' signing catalogs. Since T12.12e (2026-11-13) they are pinned and shipped too: without them a context model compiled with them fails to load (docs/qnn.md). Whether the 92.8 MB `QnnHtpPrepare.dll` can be dropped is untested, and it would need a re-measure on the Surface.

## The Windows ML alternative

**How it works.** Windows downloads a shared QNN provider on request, the app gets its path and registers it into ONNX Runtime ([install EPs](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/initialize-execution-providers)). Windows 11 24H2 or later is required ([providers](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/supported-execution-providers)).

**Would it work for an unpackaged Tauri app?** Probably, but nobody has shown it here. [Likely]
- Microsoft's table lists unpackaged as supported for self-contained deployment, and C/C++ gets only self-contained mode ([deployment](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/distributing-your-app)). [Certain]
- That needs the `Microsoft.Windows.AI.MachineLearning` NuGet package's DLL in the install, and hand-written FFI from Rust to its C API (`WinMLEpCatalogCreate`, `WinMLEpEnsureReady`, and so on). Rust is not a documented route. No MSIX is required as far as the docs say.

**What it would solve.** Microsoft delivers the Qualcomm binaries, so you no longer distribute them, and clause 1(iv) and the distribution side of 9(a) fall away. Your install shrinks by about 132 MB. Microsoft still tells the app developer to read the QNN licence, whose text it says is in the Neural Processing SDK ([notices](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/third-party-notices)), so "use" remains under Qualcomm's terms. [Likely] I did not read that copy.

**What it would break:**

1. **Your Surface is not listed.** The QNN requirements name only Snapdragon X Elite (X1E) and X Plus (X1P), driver 30.0.140.0 or newer ([providers](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/supported-execution-providers)). The Surface is an X2 Elite. It may work, since the current provider is recent, but nothing says so. [Certain that it is unlisted]
2. **Your privacy check would stop binding.** Providers update automatically through Windows Update, and only the current version is supported ([comparison](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/windows-ml-eps-vs-bring-your-own), [releases](https://github.com/microsoft/WindowsML/wiki/Windows-ML-Execution-Provider-Releases)). `register_qnn` refuses a library that fails its SHA-256 pin, and only a passing `cargo xtask qnn-etw` clears a real meeting on the NPU (T8.3b). A silent update would invalidate that verdict or stop the NPU. Rethinking this is a privacy decision for you. [Certain about the mechanism]
3. **ONNX Runtime versions.** Windows ML 2.5.96 ships ONNX Runtime 1.28.3 ([versions](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/onnx-versions)); Earshot pins 1.31.0 through `ort` at C API 31. The sample registers the path into an `Ort::Env`, but the docs do not promise that a catalogue provider loads in another copy of ONNX Runtime. Dropping to Microsoft's copy would move every other model too. [Likely that it is a real obstacle]
4. **Different Qualcomm stack from what was measured.** The current catalogue provider carries QAIRT 2.48.40, with 2.51.40 due 2026-12-01 ([releases](https://github.com/microsoft/WindowsML/wiki/Windows-ML-Execution-Provider-Releases)); `docs/qnn.md` measured against plugin 2.7.0 with QAIRT 2.51.0.
5. **A download outside `earshot-net`.** Windows does it, so it appears in no audit log, and a managed device can block it ([comparison](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/windows-ml-eps-vs-bring-your-own)). It carries no conversation content, so it does not breach your rule, but it is a network path you would be accepting. [Likely]

Microsoft also documents a hybrid: catalogue first, a bundled copy as fallback ([comparison](https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/windows-ml-eps-vs-bring-your-own)). It keeps the bundling obligations and adds the uncertainty, so I would not start there.

## Options, ranked

1. **Bundle, under 1(iv).** Pins, the ETW verdict and the measured stack all stay as they are. Costs about 132 MB and the licence's risks: termination at will and the indemnity. Before a public release, ask Qualcomm in writing, and read 2(d), 2(e), 8 and 9(a).
2. **Windows ML, after a spike.** A short test on the Surface answers the first and third breakages: does the catalogue offer QNN on an X2 Elite, and does the provider load in ONNX Runtime 1.31? Worth doing only if bundling's terms put you off, since the update problem still needs a decision.
3. **No NPU in the published install.** ARM64 runs on the CPU, and QNN stays for your own machines. This is the safe default if you read the licence and dislike it. Summaries are unaffected, as NPU work is transcription only, but transcription loses the D20 order.
4. **Separate download of the libraries.** Rejected: the licence's last sentence of section 1.

## Could not establish

- Whether Qualcomm reads incorporation as covering DLLs beside an exe. No Qualcomm statement found; only the licence text.
- Whether the Windows ML catalogue offers QNN on the X2 Elite, or loads in another ONNX Runtime. Both need the Surface.
- What telemetry the Windows ML route adds. The ETW check has only run against the bundled plugin.
- Whether the licence in the Neural Processing SDK is the same text as the AI Stack License in the plugin zip.
