# Earshot

A Windows meeting recorder that keeps your conversations on your own computer. It records your microphone and your computer's audio, writes a transcript as the meeting goes, works out who said what, and summarises the meeting afterwards. Models run on your machine, and your audio never leaves it. Read [PRIVACY.md](PRIVACY.md) for exactly what stays and what can go.

## Status

Earshot is early software. **[Download the installer](https://github.com/pitslug/Earshot/releases/latest)** from Releases: `x64` for most Windows PCs, `arm64` for Snapdragon PCs such as the Surface Pro. Or build it from source (below).

From 0.2.0, Earshot updates itself from Settings, General: press **Check for updates**, or turn on "Check for updates automatically" (off by default), which checks when Earshot starts and then on a schedule you pick. 0.1.x has no in-app update, so install the next version by hand.

The installers are not code-signed with a certificate, and the project does not plan to buy one. In-app updates are signed with the project's own key instead, which Earshot checks before it runs anything. What Windows does with an unsigned installer:

- **SmartScreen** warns "Windows protected your PC". Check the file came from this repo's Releases page, then choose "More info", then "Run anyway".
- **Smart App Control**, if it is on, blocks an unsigned installer it does not recognise and offers no way to run it. The only way past it is to turn Smart App Control off for every app, which this project does not ask you to do.

Not tried yet: a PC with no NVIDIA graphics card, a PC without Microsoft's Visual C++ runtime already installed (the installer carries its own copy), and an in-app update of the ARM64 version.

## What it does

- **Two ways to record:** a call (your mic plus your computer's audio) or a meeting in the room (mic only).
- **Echo removal:** on a call without headphones, a built-in echo canceller removes the other people's voices from your mic track.
- **Live transcript:** speech appears as it is transcribed, and recording carries on even if the transcript falls behind. Optional grey words show what someone is saying within a few seconds, until the settled text replaces them.
- **Who said what:** speakers are separated after the meeting. You can name them, and optionally have Earshot remember voices for next time.
- **Summaries:** a TL;DR, decisions and actions, each linked to the transcript lines it came from. These run locally by default.
- **Obsidian export:** one Markdown note per meeting, on demand. Your own edits outside Earshot's section of the note are kept when you export again, except for the frontmatter fields Earshot owns, such as `date` and `attendees`.
- **Folders and search:** sort meetings into folders in the side panel, and search every meeting's title, transcript and summary.
- **Audio retention:** recordings are deleted 7 days after the final transcript by default, and you can change that in Settings. Transcripts stay until you delete the meeting.

The default models are English. Other languages are a choice in Settings, using models the project's tests do not cover.

## What you need

- Windows 11, x64 or ARM64. The ARM64 version has been tried on one Snapdragon X2 Elite Surface Pro.
- Memory: a replayed meeting transcribed on the CPU peaked at about 5.5 to 5.8 GiB, and writing the summary is extra. No minimum has been established.
- Disk space for models, which the app offers to download and shows the size of first: the default transcription model is about 2.5 GB, and the local summary model is about 2.7 GB or 5.7 GB depending on your machine. On ARM64, a Parakeet model's NPU version adds about 640 MB (0.6B v2) or 140 MB (110M).
- No graphics card is required. No NVIDIA software is required unless you opt in to CUDA.
- On an x64 PC, Whisper models and summaries made on the PC need a processor with AVX2 (most from 2013 on). Without it Earshot says so and offers the rest.
- A direct internet connection for downloads. Earshot does not use a proxy, so on a work network that reaches the internet only through one, model downloads fail: download them on another network.

## Using it

1. Run the installer (per user, no admin needed). The first-run steps cover your Obsidian folder, models, summaries, an audio check and your voice. Each one can be skipped.
2. Choose **New meeting**, or press `Ctrl+Alt+Shift+R` anywhere, or use the tray icon. The same shortcut stops the meeting.
3. When the meeting ends, Earshot finishes the transcript, separates the speakers and writes the summary. You can start another meeting straight away.
4. Name any voices it asks about, then use **Export to Obsidian** if you want a note, or **Copy summary**.

While a meeting is finishing, **Cancel** stops the transcript and summary work; the recording is kept and can be transcribed again from its page. In a meeting in the room, **This is me** on a voice marks it as yours without saving your voice.

Closing the window leaves Earshot in the tray. **Quit Earshot** in the tray menu ends it. When you are not looking at its window, Earshot shows Windows notifications at a meeting's start, the end of recording and its summary (a re-summary's too), and asks whether to stop when a call seems to have ended. They use fixed text, never a meeting's title or content, and make no sound.

**Tell people you are recording.** Laws on recording conversations vary by place, and getting consent is your responsibility.

## Where the work runs

| Work | x64 default | Optional on x64 | ARM64 |
|---|---|---|---|
| Transcription | Parakeet TDT 0.6B v2 on the CPU, or what the app's model check keeps for your machine | Whisper models on Vulkan or the CPU; Parakeet on an NVIDIA card through CUDA | Parakeet on the CPU, or on the NPU (ONNX Runtime with Qualcomm's QNN) once you download that model's NPU version on the Models page; Whisper on the CPU |
| Summaries | Qwen 3.5 on a discrete graphics card through Vulkan when it has room, otherwise on the CPU | CUDA on an NVIDIA card; your own endpoint; or no summaries | Qwen 3.5 4B on the CPU; your own endpoint; or no summaries |
| Voice detection and speakers | CPU | None | CPU |

The app shows which backend actually ran, read from the runtime's own logs rather than from what was requested. CUDA is an opt-in download in Settings, offered only on a suitable NVIDIA card in an x64 PC: the set is described in [docs/cuda-install.md](docs/cuda-install.md).

The grey words are in Settings, General, "Show words while someone is talking": **Off**, **Other people only** (the people on a call, or everyone in the room) or **Everyone** (your own voice on a call too). They are never saved and change nothing that is. They run only with a Parakeet model, on the CPU or through CUDA, never on the NPU, and they are off on battery. They are on by default for the other people when the PC has at least 12 GB of memory (CPU), or 20 GB plus a graphics card with at least 14 GB (CUDA). Below the memory limit you can still turn them on; the card limit stays. With **Everyone**, the grey words for your own voice run on the CPU and need at least 12 GB, even when your transcription runs on CUDA.

Summaries can also go to a provider you choose: any OpenAI-compatible endpoint, Anthropic with your own API key, or your own installed Claude Code or OpenAI Codex. The transcript text is then sent to that provider, as [PRIVACY.md](PRIVACY.md) explains. None of these is on by default.

## Build from source

You need `rustup` (the toolchain is pinned in `rust-toolchain.toml`), Node LTS and pnpm, plus the Windows build tools. The first command below lists what is missing on your machine.

```powershell
cargo xtask doctor
```

```powershell
cargo xtask build --bundle
```

That builds the app and its unsigned per-user installer for your PC's architecture. [docs/arm64-release.md](docs/arm64-release.md) covers building the ARM64 installer on an x64 PC, and [docs/updates.md](docs/updates.md) how in-app updates are signed and published. To run the checks, fetch what the tests load and run `ci`:

```powershell
cargo xtask fetch-models --set ci
```

```powershell
cargo xtask fetch-fixtures --set smoke,librispeech
```

```powershell
cargo xtask ci
```

Run `cargo xtask` from the repo root. [PLAN.md](PLAN.md) is the design and task list, and [AGENTS.md](AGENTS.md) holds the working rules and everything learned while building. Both refer to private logs and hardware notes that are not in this repo.

## Licence and credits

Earshot's own code is under the [MIT licence](LICENSE). Third-party files keep their own terms, and nothing from NVIDIA is covered by the MIT licence. Every downloadable file is listed in [models/manifest.toml](models/manifest.toml) with its source, licence, checksum and attribution.

Models are not included in the installer. They are downloaded when you ask for them, from the sources in the manifest:

- **Transcription:** NVIDIA Parakeet TDT 0.6B v2 and 110M (NVIDIA, with NVIDIA NeMo and Suno.ai for 110M), CC-BY-4.0, converted to ONNX by the k2-fsa sherpa-onnx project. Their NPU versions for ARM64 are Earshot's own quantised conversions of those, CC-BY-4.0, downloaded from this project's releases. OpenAI Whisper weights in the whisper.cpp format, MIT.
- **Summaries:** Qwen 3.5 4B and 9B (Alibaba Cloud Qwen team), Apache-2.0, GGUF conversions by Unsloth.
- **Speech helpers:** Silero VAD (MIT), ONNX Runtime (MIT), and through sherpa-onnx, pyannote segmentation 3.0 (MIT) and 3D-Speaker CAM++ (Apache-2.0).
- **Installer:** built with [NSIS](https://nsis.sourceforge.io/) (zlib/libpng licence), whose LZMA decompression code is under the Common Public License 1.0. The source is available from the NSIS project.
- **NPU runtime (ARM64 installer only):** the ONNX Runtime QNN execution provider (Qualcomm and Microsoft, MIT) with Qualcomm's AI Runtime libraries under the Qualcomm AI Stack License. Both licences are installed beside them, and `NOTICE.md` in the install names every file.
- **Visual C++ runtime:** the installer carries Microsoft's runtime DLLs beside the app, as Microsoft's Visual Studio terms allow.
- **Runtimes:** whisper.cpp and llama.cpp (MIT). The optional CUDA set links NVIDIA's runtime, under NVIDIA's terms ([docs/cuda-libraries.md](docs/cuda-libraries.md)).
- **Benchmark audio:** an excerpt of meeting ES2004a from the AMI Meeting Corpus, University of Edinburgh and the AMI Consortium, CC BY 4.0.
