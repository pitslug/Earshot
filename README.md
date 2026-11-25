# Earshot

A Windows meeting recorder that keeps your conversations on your own computer. It records your microphone and your computer's audio, writes a transcript as the meeting goes, works out who said what, and summarises the meeting afterwards. Models run on your machine, and your audio never leaves it. Read [PRIVACY.md](PRIVACY.md) for exactly what stays and what can go.

## Status

**[Download the installer](https://github.com/pitslug/Earshot/releases/latest)** from Releases: `x64` for most Windows PCs, `arm64` for Snapdragon PCs such as the Surface Pro. Or build it from source (below). [CHANGELOG.md](CHANGELOG.md) says what each release changed.

Earshot is built and checked on two computers: an x64 desktop with an NVIDIA graphics card and a Snapdragon X2 Elite Surface Pro. It should run on other Windows 11 PCs, but expect rough edges on hardware unlike those. Not tried yet: an x64 PC with no NVIDIA graphics card, a PC without Microsoft's Visual C++ runtime already installed (the installer carries its own copy), and an in-app update of the ARM64 version.

Earshot updates itself from Settings, General: press **Check for updates**, or turn on "Check for updates automatically" (off by default), which checks when Earshot starts and then on a schedule you pick. An update never installs an older version, and after an update your data cannot be opened by the older version again (see "Where your data is").

The installers are not code-signed with a certificate, and the project does not plan to buy one. In-app updates are signed with the project's own key instead, which Earshot checks before it runs anything. What Windows does with an unsigned installer:

- **SmartScreen** warns "Windows protected your PC". Check the file came from this repo's Releases page, then choose "More info", then "Run anyway".
- **Smart App Control**, if it is on, blocks an unsigned installer it does not recognise and offers no way to run it. The only way past it is to turn Smart App Control off for every app, which this project does not ask you to do.

To report a security problem, see [SECURITY.md](SECURITY.md).

## What it does

- **Two ways to record:** a call (your mic plus your computer's audio) or a meeting in the room (mic only).
- **Echo removal:** on a call without headphones, a built-in echo canceller removes the other people's voices from your mic track.
- **Live transcript:** speech appears as it is transcribed, and recording carries on even if the transcript falls behind. Optional grey words show what someone is saying within a few seconds, until the settled text replaces them.
- **Who said what:** speakers are separated after the meeting. You can name them, and optionally have Earshot remember a voice for next time, one voice at a time.
- **Summaries:** a TL;DR, decisions and actions, each linked to the transcript lines it came from. These are written on your computer by default. You can tick up to three summary writers and put them in order.
- **Fixing things:** edit a transcript line, move it to another speaker or split it in two, find text in a transcript, correct a summary (with Undo), and transcribe or summarise a meeting again.
- **Dictionary:** your own words, such as names and acronyms (up to 100). Summaries use them for spelling, and the transcript marks a word that sounds like one of them so you can correct it.
- **Obsidian export:** one Markdown note per meeting, on demand. Your own edits outside Earshot's section of the note are kept when you export again, except for the frontmatter fields Earshot owns, such as `date` and `attendees`.
- **Folders and search:** sort meetings into folders in the side panel, and search every meeting's title, transcript and summary.
- **Sync between your own computers:** optional, through a small server you run yourself. Meetings are encrypted before they leave, and audio never syncs. See "Sync" below.
- **Audio retention:** recordings are deleted 7 days after the final transcript by default, and you can change that in Settings, Notes and storage. Transcripts stay until you delete the meeting.

The default models are English. Other languages are a choice in Settings, Transcription, using models the project's tests do not cover.

## What you need

- Windows 11, x64 or ARM64. The ARM64 version has been tried on one Snapdragon X2 Elite Surface Pro.
- Memory: a replayed meeting transcribed on the CPU peaked at about 5.5 to 5.8 GiB, and writing the summary is extra. No minimum has been established. The grey words are on by default only with more than 12 GB, in practice a 16 GB PC (see "Where the work runs").
- Disk space for models, which the app offers to download and shows the size of first: the default transcription model is about 2.5 GB, and the local summary model is about 2.7 GB or 5.7 GB depending on your machine. On ARM64, a Parakeet model's NPU version adds about 640 MB (0.6B v2) or 140 MB (110M).
- No graphics card is required. No NVIDIA software is required unless you opt in to CUDA.
- On an x64 PC, Whisper models and summaries made on the PC need a processor with AVX2 and FMA. Without them Earshot refuses those with a message rather than starting them, and Parakeet models and summaries written elsewhere still work. This has not been tried on such a processor.
- A direct internet connection for downloads and update checks. Earshot does not use a proxy, so on a work network that reaches the internet only through one, model downloads and the in-app update check fail: download models and check for updates on another network.
- For sync only: somewhere to run a Docker container that your computers can reach over HTTPS.

## Using it

1. Run the installer (per user, no admin needed). The first-run guide has five steps, and **Skip setup** leaves it at any point:
   - **Summaries:** who writes the summary after each meeting.
   - **Sync:** join your other computer, set up sync, or not now.
   - **Download:** the models this computer needs. Nothing downloads until you press the button.
   - **About me:** your name, and optionally a sample of your voice so Earshot recognises you.
   - **Optional extras:** whether to show grey words while someone is talking, your Obsidian folder and an audio check.

   Everything in it is in Settings afterwards.
2. Choose **New meeting**, or press `Ctrl+Alt+Shift+R` anywhere, or use the tray icon. The same shortcut stops the meeting. While Earshot is running, it also offers to start recording when it sees a call app (such as Teams or Zoom) open your microphone; nothing records unless you say yes, and "Offer to record when a call starts" in Settings, General turns the offer off.
3. Stop the meeting when it is over. During a call, when the call seems to have ended, Earshot asks whether to stop. It only asks: only if you turn on a setting on the General page, which is off by default, does a question that nobody answers stop the recording by itself, after 3 minutes. Earshot then finishes the transcript, separates the speakers and writes the summary. You can start another meeting straight away.
4. Name any voices it asks about, then use **Export to Obsidian** if you want a note, or **Copy summary**.

While a meeting is finishing, **Cancel** stops the transcript and summary work; the recording is kept and can be transcribed again from its page. In a meeting in the room, **This is me** on a voice marks it as yours without saving your voice.

Closing the window leaves Earshot in the tray. **Quit Earshot** in the tray menu ends it. When you are not looking at its window, Earshot shows Windows notifications: at a meeting's start, the end of recording and its summary (a re-summary's too, and when a summary or transcript could not be made), when a call seems to have started, when a call you are recording seems to have ended (to ask whether to stop), and when an automatic update check finds a new version. One more is shown even while you are looking: when a recording stopped by itself because no more audio could be saved. They use fixed text with at most a count or a version number, never a meeting's title or content, and make no sound.

**Tell people you are recording.** Laws on recording conversations vary by place, and getting consent is your responsibility.

## Sync

Sync keeps your meetings the same on each of your own computers. It is off until you set it up in Settings, Sync.

- You run the meeting point yourself: a small relay in a Docker container, reached over HTTPS (put it behind your reverse proxy). This project hosts nothing. The steps are in the comments of its compose file, [crates/earshot-sync-server/docker-compose.yml](crates/earshot-sync-server/docker-compose.yml), which is attached to each release.
- The first computer is set up with the relay's address and its setup secret. Each further computer joins with a pairing code shown on one that already syncs.
- Everything is encrypted on your computer before it is sent, with a key only your computers hold. The relay cannot read it.
- What syncs: titles, transcripts, speakers, people, summaries, folders and, unless you turn it off, your Dictionary. Saved voices sync only if you turn that on. Audio and your other settings never sync.
- A computer can ask another of your computers to write its summaries ("Write summaries for my other computers" in Settings, Sync, on the one that should do the writing).
- Optional: a summary worker container on a server of yours can write summaries too. Read its warning first: once paired, whoever controls that server can read every synced meeting. See [docs/sync.md](docs/sync.md), "Running the worker".

All your computers, the relay and the worker should run the same Earshot version. When you update, update every computer, and the worker last: [docs/compatibility.md](docs/compatibility.md) explains why.

## Where your data is

Everything Earshot keeps is in `%LOCALAPPDATA%\Earshot` on each computer:

| What | Where | Back it up? |
|---|---|---|
| Meetings, transcripts, summaries, people, saved voices, settings | `earshot.db`, with `earshot.db-wal` and `earshot.db-shm` beside it when they exist | Yes |
| Recordings that have not been deleted yet | `audio\` | Yes, if you want the audio |
| Models | `models\` | No: they can be downloaded again |
| The program itself, its window's browser files, crash logs, downloaded updates and working files | the rest of the folder | No |

- **Backing up.** Quit Earshot from the tray first. Then copy `earshot.db`, any `earshot.db-wal` and `earshot.db-shm` files beside it (the three belong together), and the `audio` folder. Check that your backup tool includes `%LOCALAPPDATA%`: many leave it out. Nothing in that folder is encrypted, so a copy is as readable as the original.
- **Restoring.** Quit Earshot and put those files back where they were. Do not run one copy on two PCs that sync with each other: the database carries the identity the first PC syncs under. To put your meetings on a second PC, use sync instead.
- **Not in the folder.** A saved Anthropic API key and the sync key and token are in Windows Credential Manager, not in the backup. On a fresh Windows account or a replacement PC, enter the API key again.
- **Sync after a restore to a fresh Windows account or PC.** The restored copy still says it syncs, but its key is gone, so Settings, Sync says this computer's sync key is missing. The relay still holds the old registration under the same identity and refuses a pairing code for it. So, in this order: on another of your computers, remove the restored computer from the list on Settings, Sync; on the restored one, choose **Remove this computer**; then join again with a pairing code from the other computer. If no other computer still syncs, the key is lost: start the relay on a new, empty volume and set sync up again. Restoring an older copy over a computer that kept its sync key has not been tried.
- **Sync is not a backup.** Audio never syncs, settings do not sync, and a meeting you delete on one computer is deleted on all of them.
- **Upgrading is one-way.** A new version may change the database the first time it starts. After that an older Earshot cannot open it: it shows a message asking you to install the latest version, and leaves the data untouched. Earshot does not take a copy of the database before it upgrades, on purpose: a hidden copy would keep meetings you later delete. If you want a way back, make your own backup before you update, and delete it when you no longer need it.
- **Uninstalling** keeps your data unless you tick "Delete the application data", which deletes the whole folder, meetings and models included.

## Where the work runs

| Work | x64 default | Optional on x64 | ARM64 |
|---|---|---|---|
| Transcription | Parakeet TDT 0.6B v2 on the CPU, or what the app's model check keeps for your machine | Whisper models on Vulkan or the CPU; Parakeet on an NVIDIA card through CUDA | Parakeet on the CPU, or on the NPU (ONNX Runtime with Qualcomm's QNN) once you download that model's NPU version in Settings, Transcription; Whisper on the CPU |
| Summaries | Qwen 3.5 on a discrete graphics card through Vulkan when it has room, otherwise on the CPU | CUDA on an NVIDIA card; another writer (below); or no summaries | Qwen 3.5 4B on the CPU; another writer (below); or no summaries |
| Voice detection and speakers | CPU | None | CPU |

The app shows which backend actually ran, read from the runtime's own logs rather than from what was requested. CUDA is an opt-in download in Settings, System, offered only on a suitable NVIDIA card in an x64 PC: the set is described in [docs/cuda-install.md](docs/cuda-install.md).

**Grey words.** The choice is in Settings, Transcription, "Show words while someone is talking": **Off**, **Other people only** (the people on a call, or everyone in the room) or **Everyone** (your own voice on a call too). They are never saved and change nothing that is. The limits, as the code has them:

- They run only with a Parakeet model, never with Whisper.
- They run where transcription runs: on the CPU, on the NPU of a Snapdragon PC, or through CUDA on an NVIDIA card that reports at least 7.5 GiB of memory (an 8 GB card). With CUDA transcription on a smaller card, the grey words run on the CPU instead.
- They pause while the PC is on battery and come back when it is plugged in.
- The memory limit is 12 GiB as Windows reports it. A PC sold as 12 GB reports a little less, so in practice this means a 16 GB PC or larger.
- Until you choose, they are on for the other people on a PC at or above that limit, and off below it. Choosing **Other people only** turns them on below the limit too; meetings may then run slower.
- **Everyone** needs a PC at or above that limit whatever you choose. Below it, it shows the other people's words only.

**Summary writers.** In Settings, Summaries you tick the writers to use and put them in order (at most three); if one cannot write a summary, the next one tries. Besides the model on this computer they are: your own server (any OpenAI-compatible endpoint), Anthropic with your own API key, your own installed Claude Code or OpenAI Codex, and another computer of yours through sync. The transcript text is sent to whichever of those writes the summary, as [PRIVACY.md](PRIVACY.md) explains. None of them is on by default. By itself Earshot uses only the writers you ticked; a meeting's **Re-summarise** menu also lists every writer that can run now, ticked or not, and one you pick there is sent that meeting's text.

"Keep this meeting on this computer" on the new-meeting screen keeps one meeting away from all the writers that are not your own computers. It can only be chosen before recording starts. **Allow** on that meeting's page lifts it for that meeting, and that cannot be undone.

## Build from source

You need `rustup` (the toolchain is pinned in `rust-toolchain.toml`), Node LTS and pnpm, plus the Windows build tools. The first command below lists what is missing on your machine.

```powershell
cargo xtask doctor
```

```powershell
cargo xtask build --bundle
```

That builds the app and its unsigned per-user installer for your PC's architecture. For work on the app itself, `pnpm --filter @earshot/desktop tauri dev` runs it against the Vite dev server, but that build has no Cargo features: transcription and summaries run on the CPU only (no Vulkan, no CUDA). `cargo xtask build --app` builds the app with the shipped backends and the build setup they need. [docs/arm64-release.md](docs/arm64-release.md) covers building the ARM64 installer on an x64 PC, and [docs/updates.md](docs/updates.md) how in-app updates are signed and published. To run the checks, fetch what the tests load and run `ci`:

```powershell
cargo xtask fetch-models --set ci
```

```powershell
cargo xtask fetch-fixtures --set smoke,librispeech
```

```powershell
cargo xtask ci
```

Run `cargo xtask` from the repo root. [CONTRIBUTING.md](CONTRIBUTING.md) says how to send a change. [PLAN.md](PLAN.md) is the design and task list, and [AGENTS.md](AGENTS.md) holds the working rules and everything learned while building. Both refer to private logs and hardware notes that are not in this repo.

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
