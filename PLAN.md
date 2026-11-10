# Earshot - Build Plan

Earshot is an offline, Granola-style meeting notetaker. You type rough notes during a meeting; Earshot captures audio, transcribes it live, works out who said what, and afterwards merges your notes and the transcript into structured "enhanced notes" exported to Obsidian. Audio never leaves the device.

This file is the single source of truth for **what** to build and **in what order**. `AGENTS.md` defines **how** an agent works through it with the owner. The task list in section 8 is the live work queue: agents update its status markers as they go.

---

## 1. Scope

### v1 - Windows desktop (x86-64 and ARM64)
- Rust workspace, Tauri 2 + React shell.
- WASAPI microphone capture + loopback capture of the call app's audio (process loopback of the chosen or detected call app, falling back to loopback of the default render device), following device changes mid-call.
- Pure-Rust acoustic echo canceller (removes far-end audio leaking from speakers into the mic).
- Live transcription during the meeting (fast model) + a final high-accuracy pass at stop.
- whisper.cpp transcription via pluggable backends: CPU, CUDA (RTX 5080), Vulkan (fallback), QNN NPU (Snapdragon X2).
- Speaker diarisation, with local voiceprints that recognise recurring people.
- Summaries / enhanced notes via any OpenAI-compatible endpoint (default: a local server such as LM Studio or Ollama).
- Obsidian markdown export.
- Audio kept on-device with configurable retention (post-AEC mic track + loopback reference track, each tagged with the pipeline version that produced it), so meetings can be re-transcribed with better models later.

### v2 - Android (Pixel 11 Pro Max)
- In-person recorder only: mic capture, no loopback, no echo cancellation.
- Same core crate and pipeline. Speaker diarisation + voiceprints.
- Obsidian export to a vault folder on the device.

### Future - Sync container
- Optional self-hosted Docker container that syncs transcripts, notes, summaries and speaker metadata across devices.
- **Never stores or transports audio.**
- Voiceprints are local-only until this exists; once it is running, voiceprint sync becomes an opt-in setting.

### Later (features list)
- Notes during and after a meeting, including handwriting (pen on the Surface) converted to text on the device (GATE-11).

### Non-goals (for now)
- Automatic meeting detection, calendar integration, cloud transcription, multi-user accounts, real-time translation, audio sync of any kind.

---

## 2. Target hardware

| Device | Arch / OS | Role | Acceleration |
|---|---|---|---|
| Desktop | x86-64, Windows 11, RTX 5080 (Blackwell, sm_120) | v1 primary, likely build host | CUDA (primary), Vulkan (fallback), CPU |
| Surface Pro | ARM64, Windows 11, Snapdragon X2 | v1 primary laptop | QNN NPU via ONNX Runtime (target), CPU (baseline) |
| Pixel 11 Pro Max | arm64-v8a, Android | v2 in-person recorder | CPU first; NPU/GPU is a future spike |

The build host is whichever machine runs the agent. `cargo xtask doctor` reports the host triple and which backends it can exercise. Anything the host cannot exercise is covered by a hardware gate.

---

## 3. Architecture

### 3.1 Workspace layout

```
earshot/
  Cargo.toml                 workspace
  rust-toolchain.toml
  crates/
    earshot-core/            NO OS dependencies (see 3.2). Domain, DSP, AEC, pipeline state machine,
                             diarisation clustering, voiceprint matching, summary schema + validation,
                             Obsidian renderer, metrics (WER/DER/ERLE/SI-SDR), call simulator.
    earshot-asr-whisper/     whisper.cpp via whisper-rs. Features: cpu (default), cuda, vulkan.
    earshot-onnx/            ONNX Runtime (ort crate): Silero VAD, diarisation segmentation + speaker
                             embedding, execution-provider selection (CPU, CUDA, QNN).
    earshot-asr-qnn/         Whisper encoder/decoder on ONNX Runtime QNN EP (Snapdragon NPU).
    earshot-net/             The only crate allowed to make HTTP requests: one reqwest + rustls client,
                             host allowlist (manifest hosts, configured LLM URL, update host,
                             future sync server),
                             audit log of every request (used by tests to prove what left the machine).
    earshot-llm/             OpenAI-compatible chat client, built on earshot-net.
    earshot-store/           SQLite (rusqlite, bundled), FLAC audio files, retention, file export writer.
    earshot-capture-wasapi/  Windows mic + loopback capture (windows crate).
    earshot-capture-android/ JNI bridge receiving PCM from the Kotlin foreground service (dependency-free stub until T10.3).
    earshot-runtime/         Drives the core pipeline: threads, channels, backend wiring, persistence.
    earshot-cli/             Headless driver: transcribe, diarise, summarise, export, record, bench.
    earshot-sync-server/     (Future) axum + SQLite sync server, Dockerfile.
  apps/                      Tauri crates join the Cargo workspace only when created (T7.1, T10.2), so M0-M6
                             builds never need Node or a built frontend.
    desktop/                 Tauri 2 shell (Windows x64 + arm64)
    android/                 Tauri 2 mobile shell (Android) + Kotlin plugin
  packages/
    ui/                      Shared React + TypeScript components (pnpm workspace)
  xtask/                     cargo xtask: ci, purity, accept, fetch-fixtures, fetch-models, doctor, bench, build
  acceptance/                thresholds.toml (human-owned), scenario files, results/
  fixtures/                  manifest.toml, LICENSES.md, smoke/ (small, committed); large sets are fetched
  models/                    manifest.toml only (models are fetched, never committed)
  schemas/                   summary.v1.json (generated, committed, drift-checked)
  gates/                     GATE-*.md written by agents, *.pass / *.fail written by the human
  docs/                      design notes, sync.md (future)
```

### 3.2 Core purity contract (`earshot-core`)

`earshot-core` is sans-IO. The contract applies to the core **library target and its normal dependencies**. Its tests and dev-dependencies (for example `insta`) may read fixture files, read-only, from `EARSHOT_FIXTURES`.

The library must:
- Compile for `wasm32-unknown-unknown` (proof that nothing platform-specific is linked).
- Have no FFI, no `build.rs` that compiles native code, no platform crates (`windows`, `ndk`, `jni`, `libc`, `cpal`, `ort`, `whisper-rs`, `reqwest`, `tokio`, `rusqlite`, `getrandom`, `rayon`, ...).
- Not use `std::fs`, `std::net`, `std::process`, `std::thread::spawn`, `std::time::SystemTime::now` / `Instant::now`, or env vars. Time comes from a `Clock` trait; randomness from a seeded RNG passed in.
- Take audio as buffers (`&[f32]`) or `impl Read`, never paths. The call simulator and the replay source receive buffers; opening files is done by the CLI, runtime, xtask or tests.
- Allowed: `std::io::{Read, Write}` traits, `alloc`, `std::sync` primitives, pure-Rust crates (rustfft, rubato, serde, serde_json, thiserror, hound, rand_chacha, etc.) provided they pass the purity check. `uuid` only without its `v7`/`rng` features (they pull `getrandom`): build UUIDv7 with `uuid::Builder::from_unix_timestamp_millis` from the injected clock and RNG. Crates with thread-pool features (for example `flacenc`'s `par`) must have them disabled; the FLAC writer lives in `earshot-store` anyway.

Enforced by `cargo xtask purity`:
1. `cargo build -p earshot-core --lib --target wasm32-unknown-unknown`.
2. A ban list checked against `cargo tree -p earshot-core -e normal` (normal dependencies only; cargo-deny cannot scope bans to one member).
3. `cargo clippy -p earshot-core --lib` with `crates/earshot-core/clippy.toml` disallowed methods/types.

It runs inside `cargo xtask ci` from T0.3 onward.

All OS work (capture, files, network, GPU, threads) lives in adapter crates that implement core traits.

### 3.3 Pipeline

```
 Windows:  WASAPI mic ----\                                 /-> live ASR (fast model) -> live transcript events
                           > resample 16k -> AEC(mic, ref) -> VAD -> chunker
           WASAPI loopback/ (ref + "them" track)            \-> both tracks -> FLAC on disk
                                                                                |
 Android:  AudioRecord (foreground svc) -> JNI -> resample 16k -> VAD -> chunker      (no AEC, single track)
                                                                                |
 On stop:  final ASR pass (accurate model, full context) -> diarisation per track -> voiceprint match
           -> merge with user notes -> summary (LLM, schema-validated, grounded) -> store
 On demand: Obsidian export (one meeting or 'export all/changed'), only when the user asks (decided by the owner)
```

- The core pipeline is a **step-driven state machine**: `push_frames(track, frames, device_ts)` in, `PipelineEvent`s out. `earshot-runtime` owns the threads and calls into it. This keeps the core testable with the buffer replay source and the call simulator.
- The **linear** echo canceller adds no latency on the mic path; drift compensation acts on the **reference**, never the mic. The residual echo suppressor (postfilter) may add one fixed, reported latency of at most 32 ms (decided by the owner: Earshot never plays the cleaned mic back into a call, so a fixed delay only affects transcription and recording, where timestamps and evaluation compensate it exactly). Mic-derived metrics (SI-SDR, WER) are scored after compensating the reported latency.
- Live view labels the mic track "Me" and the loopback track "Them". Full diarisation runs at stop.
- Two-track policy (Windows): the mic track defaults to the single speaker "Me" unless clustering finds more than one local voice (shared room); the loopback track is diarised into remote speakers. In-person (Android, or a Windows "in-room" mode): one track, fully diarised.

### 3.4 Backends

| Concern | Trait (core) | Implementations |
|---|---|---|
| ASR | `Transcriber`, `StreamingTranscriber` | whisper.cpp CPU / CUDA / Vulkan (`earshot-asr-whisper`), QNN (`earshot-asr-qnn`), `FakeTranscriber` (tests) |
| VAD | `Vad` | energy VAD (core), Silero (`earshot-onnx`) |
| Diarisation | `SpeakerSegmenter`, `SpeakerEmbedder` | pyannote segmentation-3.0 ONNX + CAM++ or WeSpeaker ONNX (`earshot-onnx`), fakes |
| Summary | `Summarizer` | OpenAI-compatible (`earshot-llm`), replay mock (tests) |
| Capture | `AudioSource` | WASAPI, Android JNI, buffer replay (core; files are opened by the CLI/runtime) |
| Storage | `MeetingRepo`, `VoiceprintRepo`, `AudioSink` | SQLite + FLAC (`earshot-store`), in-memory (core tests) |
| Export | `render_obsidian(&Meeting, &ExportOptions) -> String` (pure, core) | file writer in `earshot-store` |

Backend auto-selection at runtime: on x86-64 summaries use a discrete GPU through Vulkan, else the CPU, and CUDA only after the user switches to it (D23, T8.4a; until T8.4a is built, a `cuda` build still puts CUDA first); transcription uses what the model check keeps. QNN > CPU on ARM64; CPU on Android. Always overridable in settings and CLI.

Default models (all fetched via `models/manifest.toml`, checksum-pinned):
- Live: whisper `base.en` (CPU/NPU), `small.en` (GPU).
- Final: whisper `large-v3-turbo` (GPU/NPU), `small.en` q5 (CPU, Android).
- Diarisation: pyannote segmentation-3.0 ONNX and a CAM++ (Apache-2.0) or WeSpeaker (CC BY 4.0) embedding model, taken from the sherpa-onnx GitHub release artefacts (no Hugging Face gated terms to accept).
- VAD: Silero VAD ONNX (MIT).

ONNX Runtime is Microsoft's official MIT release, fetched through the checksum-pinned `models/manifest.toml` (sets `ci`, `cuda`) and loaded dynamically (`ort` `load-dynamic`) on every target via `earshot_onnx::init`, with telemetry off; nothing is downloaded or linked at build time and no DirectML is used (decided by the owner). The CUDA execution provider needs cuDNN 9 on PATH; the QNN provider (ARM64) and Android builds ship as separate binaries bundled with the app.

### 3.5 Data model (sync-ready from day one)

Entities: `Meeting`, `Track` (kind, device name, clock domain), `AudioFile` (path, format, pipeline/AEC version that produced it), `Segment` (start, end, text, words, track, speaker_id, confidence), `Speaker` (per meeting, display name, optional `person_id`), `Person` (recurring identity, optional `merged_into` so duplicates created on two devices can be merged), `Voiceprint` (embedding vector, embedding model id, person_id), `Note` (user-typed, anchors as **milliseconds from meeting start**, never wall-clock), `Summary` (schema v1), `Folder` (name; a meeting names at most one through `Meeting.folder_id`, T9.5), `ExportRecord` (vault-relative path, device id).

The identity base (ids, clock, sync metadata, `SyncClass`) is built first, in T0.7, so every later type is born sync-ready. Every syncable row has: UUIDv7 id, `created_at`, `updated_at` as a hybrid logical clock, `deleted` tombstone, `origin_device_id`. Each entity type carries a compile-time `SyncClass`:
- `Syncable`: Meeting, Track (metadata only), Segment, Speaker, Person, Note, Summary, Folder.
- `SyncableOptIn`: Voiceprint (off by default, only when a sync server is configured and the user enables it).
- `NeverSync`: AudioFile, audio data, raw PCM, ExportRecord (device-local). The sync layer must not be able to serialise these (enforced by types from T0.7, tested again in M11).

### 3.6 Storage layout (per device)

```
%LOCALAPPDATA%\Earshot\            (Android: app-private files dir)
  earshot.db                        SQLite, WAL mode
  audio\<meeting-id>\mic.flac       16 kHz mono, post-AEC
  audio\<meeting-id>\loopback.flac  16 kHz mono (Windows only)
  models\                           fetched models
  logs\                             local only, no telemetry
```

Audio retention (clock rule changed 2026-11-05; T7.4k): 1 hour to 1 year or forever, default 7 days; a meeting's clock starts when its final transcript is first stored, whatever the summary does (a summary is made from the transcript, so it never needs the audio); a meeting whose final pass has not succeeded keeps its audio; only audio files are deleted. Re-transcribe is available while audio exists.

Crash safety: the `AudioFile` row is written only after its FLAC file is finalised; the retention job and re-transcribe tolerate rows whose file is missing. Model downloads are checksum-verified before being moved into `models\`, and manifest paths are relative and must resolve inside the models directory.

### 3.7 Summaries

- Input: transcript segments (with ids and speaker names) + the user's notes + meeting metadata + template.
- Output: JSON matching `schemas/summary.v1.json`: `title`, `tldr`, `enhanced_notes` (sections of bullets, each bullet with `source_segment_ids`), `decisions[]`, `action_items[] {owner, task, due?, source_segment_ids}`, `open_questions[]`, `topics[]`.
- Request strategy: `response_format: json_schema` -> fallback `json_object` -> fallback plain text with JSON extraction. Up to 2 repair rounds that feed validator errors back to the model.
- Grounding: every decision and action item must cite at least one existing segment id. Invalid citations count as schema failures.
- Long meetings: map-reduce over transcript chunks sized from a configured context window.
- Privacy: the UI shows a clear warning when the configured endpoint is not loopback or a private LAN address. The endpoint URL is validated when saved (http/https only, host present) and all requests go through `earshot-net`.

### 3.8 Obsidian export

- File: `<vault>/<folder>/YYYY-MM-DD HH-mm <Title>.md` (sanitised, hyphens not em dashes).
- YAML frontmatter: `earshot_id`, `date`, `start`, `duration`, `attendees` (as `[[Person]]` links if enabled), `tags`, `source: earshot`.
- Body: TL;DR, enhanced notes, decisions, action items as `- [ ]` tasks, open questions, the user's raw notes, and the transcript inside a collapsed callout (`> [!note]- Transcript`).
- Earshot-managed body content sits between `<!-- earshot:begin -->` and `<!-- earshot:end -->`. Re-export replaces only that region; user edits outside it are preserved.
- Frontmatter merge: Earshot-owned keys (`earshot_id`, `date`, `start`, `duration`, `attendees`, `source`) are updated in place on re-export; any other keys the user added are preserved; `tags` are merged (union).
- The vault path is validated when saved in settings (exists, is a directory, writable).
- Typography option `replace_em_dash` (default **on**): em dashes in generated text become " - ".
- Writes are atomic (temp file + rename) because the vault may be live-synced.
- Export is **on demand only** (decided by the owner): meetings live in Earshot until the user exports one, or several; nothing is written to the vault automatically. Because export is user-initiated, a note with no export record is located by a full vault search for its `earshot_id`, so renamed or moved notes are always found and never duplicated.

### 3.9 Privacy invariants (tested where possible)
1. Audio never leaves the device: no network code path accepts audio types.
2. No telemetry from Earshot, no crash upload. Logs are local. Conversation content (audio, transcripts, summaries, speaker and meeting names, prompts) never leaves the device except text to the configured LLM endpoint the user chose. System-information telemetry that third-party runtimes offer to the operating system's diagnostic channel (runtime version, CPU, memory, driver names and versions; measured for ONNX Runtime and the D3D12 loader) is accepted; any such event that could carry content is not.
3. Network access only for: model/fixture downloads (checksum-verified), the configured LLM endpoint, the update host when the user checks for updates or has turned automatic checks on (D24; signature-verified, carries no conversation content), and (future) the configured sync server.
4. Voiceprints are biometric data: local-only until sync exists, then opt-in, deletable per person.

Enforcement: all HTTP goes through `earshot-net` (host allowlist + audit log); the ci `net` step (`xtask/src/net_guard.rs`) fails if any HTTP client crate appears in a shipped crate's dependency tree outside `earshot-net` (on every target; app crates such as `earshot-desktop` on the targets they ship to, `net_guard::SHIPPED_TARGETS`), or a test-support crate appears as a normal dependency (xtask, a dev tool, is exempt); the Tauri CSP limits `connect-src` to IPC and self; `e2e.replay` asserts from the audit log that nothing went anywhere except the mock endpoint. One exception, chosen by the owner: the opt-in summary providers that run the user's own installed agent CLI on its own sign-in, "Claude through Claude Code" (T7.4ga, Anthropic, Claude.ai sign-in) and OpenAI Codex (T7.4gc, OpenAI, ChatGPT sign-in), hand the summary prompt text to that CLI, which connects to its vendor itself; Earshot cannot audit that request, so it records locally that it handed text to the CLI (when, how many characters, never the text), and each provider is off unless the user picks it.

---

## 4. Acceptance harness

Every milestone has a headless acceptance command:

```
cargo xtask accept m<N>
```

It runs the milestone's checks, writes `target/acceptance/m<N>.json` (metric, value, threshold, pass), copies a summary to `acceptance/results/m<N>.json` (committed so trends are visible), and exits non-zero on any failure. A milestone is done only when its acceptance command passes on the build host.

Thresholds live in `acceptance/thresholds.toml`. **Agents may tighten a threshold but must never loosen one** (see AGENTS.md). Values marked *provisional* are best estimates; if one proves unreachable after honest effort, the agent stops and asks instead of changing it.

### 4.0 Metric definitions

- **WER** is corpus-level (total errors / total reference words over the set), after Whisper-style English normalisation. Decoding: greedy for live/streaming, beam size 5 for batch and final pass, temperature 0 with Whisper's standard temperature fallback (retry a window at higher temperature when its output is too repetitive or low-confidence; decided by the owner, T2.4d); the report records the settings.
- **ERLE** = 10 log10(P_mic / P_out) over 1 s windows, median over t > 2 s, measured on **noise-free** far-end-only scenarios.
- **Time to 15 dB ERLE** (`aec.convergence`, `aec.path_change`; `metrics::erle::time_to_erle`): ERLE over 0.5 s windows every 0.1 s; the time is the end of the first window at or above 15 dB whose following 1 s also has median ERLE at or above 15 dB (end, not start: The owner kept the stricter reading, 2026-10-18); windows with mic power below 1e-6 are not scored; after a mid-call change, near-end speech regions plus a 0.3 s room tail are blanked. (Approved by the owner.)
- **aec.erle_linear** measures what the linear filter controls: it is scored on an unclipped far-only scenario (`far-only-linear`, identical to far-only except no soft clip). Under real clipping the linear stage is checked against the best possible linear canceller (same-seed unclipped echo, best single gain) and must stay within 3 dB of it. Total echo removal (`aec.erle_farend`) and double talk stay scored on the measured clipping level. (Decided by the owner after T1.2a showed the best linear canceller reaches only 10.5 dB at softclip 3.)
- **Loudspeaker nonlinearity** in gating scenarios is anchored to a measurement of real recordings (T1.2a), not chosen to suit results; the earlier softclip-4 copies (`softclip4-*`, milder than the measured gating level) are reported as informational only. `aec.erle_farend`, `aec.doubletalk_sisdr` and `aec.passthrough` are scored on the same AEC configuration in the same run, so a canceller that simply mutes cannot pass.
- **Double-talk** scenarios run at signal-to-echo ratios of 0 dB and -10 dB. `aec.doubletalk_sisdr` gates at SER 0 (and across the robustness grid at SER 0); SER -10 SI-SDR is reported as informational, because even a suppressor with perfect knowledge of the residual reaches only ~7-8 dB there. Near-end usefulness at SER 0 is gated by `asr.wer_aec_delta` in M2 (SER -10 informational). The product requirement is a clean transcript: far-end speech that leaks into the mic must never appear twice or be attributed to "Me"; `asr.echo_leak` gates that at SER 0 and -10 after cross-track echo de-duplication (T2.4e), so acoustic echo removal is a means, not the requirement. The near-end level guard (within 1 dB of clean) gates at SER 0 and is informational at SER -10, where echo removal is preferred over level. (Decided by the owner.)
- **Robustness** covers bulk delay 0-500 ms and drift +/-200 ppm (a wired USB interface measured inside that range).
- **Device switches mid-call**: in the speakers-to-headphones scenario the echo disappears and the near-end must pass through (scored with `aec.passthrough` after the switch); in the headphones-to-speakers and delay-jump scenarios the echo appears or moves and must be re-acquired (scored with `aec.path_change`). Output paths beyond 500 ms are out of scope for v1 and listed as a known limitation.
- **asr.wer_aec_delta** uses the double-talk scenarios (near-end speech with echo present).
- **asr.stream_determinism** (`asr.wer_stream_delta` until T2.5e) uses streams of concatenated LibriSpeech utterances (about 60 s each). Under D20 live stretches equal the final pass, so it counts the words that differ between them after Whisper normalisation: it measures determinism and stretch equality, not accuracy, which `asr.wer_ami_ihm` and `asr.wer_ls_base` cover.
- **dia.count_error** is scored on synthetic mixes of 1-6 speakers built from LibriSpeech, plus the AMI meetings. **dia.count_error_large** (T3.5a) is the mean over the 4-6 speaker mixes of the same set, so a diarizer that merges speakers in larger meetings fails even when the median over every item does not move.
- **Voiceprints**: enrolment 10 s per speaker; test clips are **2-3 s** (meeting-turn length). The open-set match threshold is fixed on a separate dev split (LibriSpeech dev-clean speakers) and never tuned on the acceptance speakers.
- **DER** follows the pyannote AMI-diarization-setup reference (`only_words` RTTMs with the published UEM files), a 0.25 s collar on each side of every reference boundary (NIST md-eval convention; in pyannote.metrics terms `collar = 0.5`, which is how `DerConfig.collar` is expressed), overlap scored.

### 4.1 Fixtures

| Set | Source | Licence | Used for |
|---|---|---|---|
| `smoke` (committed, < 5 MB) | 20 short, clean LibriSpeech test-clean utterances from at least 2 speakers | CC BY 4.0 | fast CI WER, AEC sim speech, two-speaker diarisation smoke |
| `librispeech` | LibriSpeech test-clean pinned subset covering all 40 speakers, each with >= 10 s for enrolment plus >= 3 further utterances; plus a pinned dev-clean speaker subset for threshold calibration | CC BY 4.0 | WER, voiceprint ID (speaker labels), open-set threshold calibration |
| `ami` | AMI corpus, 2 test meetings: headset mix (IHM-mix) + single distant mic (SDM), with pyannote AMI-diarization-setup `only_words` RTTMs and UEMs | CC BY 4.0 (verify RTTM/UEM source licence at T0.4a) | meeting WER, DER, in-person mode |
| `llm` (committed) | Recorded OpenAI-compatible responses incl. malformed ones | own | summary schema checks |
| `private` (gitignored, optional) | Your own recordings with reference transcripts, added at gates | yours | real-world WER, never committed |

Fixtures are fetched by `cargo xtask fetch-fixtures --set <name>` into `%LOCALAPPDATA%\earshot-dev\fixtures` (override with `EARSHOT_FIXTURES`), with SHA-256 pinned in `fixtures/manifest.toml`. Attribution goes in `fixtures/LICENSES.md`.

The AEC call simulator generates its own room impulse responses (image-source method, in core), so it needs only speech fixtures.

### 4.2 Initial thresholds (`acceptance/thresholds.toml`)

WER is computed after Whisper-style English text normalisation. DER uses a 0.25 s collar with overlap scored.

| ID | Milestone | Metric | Threshold |
|---|---|---|---|
| aec.erle_farend | M1 | ERLE, far-end-only, after 2 s | >= 25 dB |
| aec.erle_linear | M1 | ERLE of linear stage alone, unclipped far-only scenario | >= 12 dB |
| aec.convergence | M1 | time to ERLE >= 15 dB from cold start | <= 1.5 s |
| aec.path_change | M1 | recovery to ERLE >= 15 dB after echo path change | <= 2.0 s |
| aec.doubletalk_sisdr | M1 | SI-SDR of output vs clean near-end during double talk at SER 0 (SER -10 informational) | >= 10 dB (provisional) |
| aec.passthrough | M1 | SI-SDR output vs input (the input after the app high-pass when one is configured) when far end silent | >= 30 dB |
| aec.robustness | M1 | all of the above hold for delay 0-500 ms and drift +/-200 ppm (stereo convergence at 500 ms bulk delay informational; the owner) | pass |
| aec.rtf | M1 | real-time factor, release build, one core | <= 0.02 |
| asr.wer_smoke_base | M2 | smoke set, base.en, CPU, batch | <= 10% |
| asr.wer_ls_base | M2 | LibriSpeech subset, base.en, CPU, batch | <= 6.0% |
| asr.stream_determinism | M2 | words differing, after Whisper normalisation, between live (production live transcription: default model and `LiveTranscription` mode) and the final pass on the same audio, concatenated ~60 s streams | 0 |
| asr.wer_ls_default | M2 | LibriSpeech subset, production default transcription model (GPU if available, else CPU), one batch call per utterance | <= 3.5% |
| asr.wer_ami_ihm | M2 | AMI IHM, per-headset audio scored per speaker (T2.5b), production default transcription model | <= 22% (provisional) |
| asr.wer_aec_delta | M2 | WER of near-end after AEC minus WER of clean near-end, double-talk scenarios at SER 0 (SER -10 informational), production default transcription model | <= 3.0 points |
| asr.echo_leak | M2 | far-end reference words that appear in the near-end ("Me") transcript after echo de-duplication, as a share of far-end words; far-only and double-talk scenarios at SER 0 and -10, mono and stereo, base.en | <= 1% (provisional) |
| dia.der_ami_ihm | M3 | DER, AMI IHM-mix | <= 25% (provisional) |
| dia.der_ami_sdm | M3 | DER, AMI SDM | <= 35% (provisional) |
| dia.count_error | M3 | median absolute speaker-count error, synthetic 1-6 speaker mixes + AMI | <= 1 |
| dia.count_error_large | M3 | mean absolute speaker-count error, the synthetic 4-6 speaker mixes of dia.count_error | <= 1 |
| vp.top1 | M3 | voiceprint top-1 ID, 20 enrolled LibriSpeech speakers, 10 s enrolment, 2-3 s test clips | >= 95% |
| vp.false_accept | M3 | unknown speakers wrongly matched | <= 5% |
| sum.schema_replay | M4 | replayed LLM responses valid after repair | 100% |
| sum.grounding | M4 | invalid segment citations in accepted summaries | 0 |
| exp.golden | M5 | export output equals golden files | pass |
| exp.idempotent | M5 | re-export is byte-identical and preserves user edits | pass |
| e2e.replay | M6 | replayed two-track meeting -> valid summary + export, all invariants | pass |
| asr.wer_ami_sdm | M10 | AMI SDM, in-person pipeline, small.en q5 CPU | <= 40% (provisional) |
| sync.no_audio | M11 | no audio types/bytes in any sync payload or server table | pass |
| sync.convergence | M11 | two clients converge after concurrent edits | pass |

Hardware-measured numbers (live latency, RTF per backend, battery, real-world ERLE from GATE-10 recordings) are recorded at gates or reported as informational, not enforced headlessly.

---

## 5. Gates

A gate is a hard stop that waits for the human. The protocol (how gates are opened, passed and failed) is in AGENTS.md.

| Gate | After | Type | What you do |
|---|---|---|---|
| GATE-00 | T0.2 | toolchain (conditional) | Only opened if `cargo xtask doctor` finds missing system prerequisites. Install them. |
| GATE-10 | T1.0 | hardware | Early raw capture: record ~2 min of a real call on speakers (not headphones) with `earshot record-raw`; the mic/loopback pairs become private AEC fixtures. |
| GATE-01a | M6 | hardware | Real WASAPI capture on the desktop: mic/loopback, device switching, AEC with speakers (not headphones) on a real call. (Split from GATE-01 by the owner.) |
| GATE-01b | after T2.6 | hardware | The same on the Surface, once an ARM64 build exists. |
| GATE-02 | M7 | hardware | Dogfood the desktop app in one or more real meetings on the desktop. |
| GATE-03 | M8 | hardware | Surface: ARM64 build, CPU + NPU (QNN) benchmarks and WER parity. Desktop: CUDA benchmark if the agent could not run it. |
| GATE-04 | M9 | sign-off | v1 release sign-off: install the x64 and arm64 NSIS installers, first-run flow, a real meeting on each machine. |
| GATE-05 | before M10 | toolchain | Android Studio, SDK, NDK, a connected Pixel with USB debugging. |
| GATE-06 | M10 | hardware | Pixel: 10-minute in-person recording, screen off, check transcript, speakers, battery and thermals. |
| GATE-07 | M10 | sign-off | v2 sign-off. |
| GATE-08 | before M11 | decision | Approve the sync design (auth, encryption, hosting) in `docs/sync.md`. |
| GATE-09 | M11 | hardware | Deploy the container on a self-hosted server, sync two real devices, confirm no audio on the server. |

---

## 6. Pre-made decisions

| # | Decision | Why |
|---|---|---|
| D1 | Sans-IO core with step-driven pipeline | Keeps the core pure, deterministic and testable with the simulator and replay sources. |
| D2 | whisper.cpp for CPU/CUDA/Vulkan; ONNX Runtime QNN EP for the Snapdragon NPU | whisper.cpp's own QNN support is experimental; ORT QNN EP is Qualcomm's supported route. |
| D3 | CUDA is the primary x86 backend, Vulkan is the fallback | The desktop has an RTX 5080; Vulkan covers other GPUs. Blackwell needs a CUDA toolkit that supports sm_120. |
| D4 | Live draft + final pass | Live pane stays responsive; the final transcript gets the accurate model and full context. |
| D5 | Diarisation via ONNX (pyannote segmentation + CAM++/WeSpeaker), clustering in core | Mature models, permissive licences, no Python. |
| D6 | Voiceprints local-only until sync exists, then opt-in | Your choice; biometric data handled conservatively. |
| D7 | Kotlin `AudioRecord` inside a microphone foreground service, PCM to Rust over JNI | Most reliable way to keep recording with the screen off on modern Android. |
| D8 | Tauri 2 for both desktop and Android, shared React UI package | One UI codebase; Android gets a reduced recorder-only route set. |
| D9 | SQLite + FLAC on device | Simple, durable, lossless, pure-Rust FLAC encoder (in `earshot-store`, not core). |
| D10 | UUIDv7 + HLC + tombstones from T0.7 | Every type is sync-ready from birth; the future sync server is a bolt-on, not a migration. |
| D11 | Fixtures and models fetched with pinned checksums, never committed (except `smoke` and `llm`) | Keeps the repo small and licences explicit. |
| D12 | Em dashes replaced in exported notes by default | Matches common vault conventions. |
| D13 | CUDA wired from the first ASR task (T2.2) | Acceptance runs on the RTX 5080 instead of hours of CPU decoding. Vulkan and ARM64 stay in T2.6. Pinned (T2.2a): whisper-rs =0.16.0 (upstream 7558e1b7), whisper-rs-sys =0.15.0 vendoring whisper.cpp 1.8.3 and ggml 0.9.5; CUDA 13.4, `CMAKE_CUDA_ARCHITECTURES=120` (ggml builds sm_120a). |
| D14 | `earshot-net` is the only HTTP client | Makes privacy invariants 1 and 3 enforceable and testable. |
| D15 | Early raw capture (T1.0 + GATE-10) before the AEC is built | The AEC meets real mic/loopback recordings early instead of only its own simulator. |
| D16 | Purity contract covers the core lib target and normal deps; tests may read fixtures | Removes ambiguity about test IO; checked with `cargo tree -e normal`. |
| D17 | Official MIT ONNX Runtime via the manifest, load-dynamic everywhere | No build-time CDN, no DirectML redistribution; same mechanism on x64, ARM64 and Android. GPU ONNX uses the official CUDA 13 build plus cuDNN 9 installed by the owner. |
| D18 | AEC scenario realism anchored to measurement | Loudspeaker nonlinearity from the GATE-10 recording (blind measurement, T1.2a); every device-switch/path-change scenario has a measurable echo-only stretch after the event. |
| D19 | Summaries are local by default | Desktop and Surface embed llama.cpp in-process (no server, no port, schema-constrained JSON); the Pixel uses Gemini Nano through Android AICore on its TPU, with a small open model on the CPU as fallback. A user may opt in to their own Claude or OpenAI key (or any OpenAI-compatible endpoint). Bundled models are permissive-licensed (Apache 2.0, MIT); Gemini Nano is allowed because it is Google's on-device service, not redistributed by Earshot. Models are chosen per device by the summary-quality evaluation, not by reputation. |
| D20 | One transcription pass, during the meeting | No separate live transcript: each closed speech stretch is transcribed once with the good model on the per-track workers and shown as it lands (about 30-60 s behind on the desktop GPU). "Listening" is shown by level meters, voice activity and a growing word count. At hang-up only the last stretch, meeting-wide speaker clustering and naming, echo cleaning and the summary remain (1-3 min). The live chunker stays as an option, not the default. Backends: CUDA > Vulkan (integrated Intel/AMD graphics as a standard path) > CPU on x86-64; NPU > Vulkan > CPU on the Surface. Weak devices may run a smaller model as their single pass (T6.10c). |
| D21 | Desktop UI (GATE-11) | Full app window (meeting list + meeting) plus tray, compact listening card while recording. During the meeting: status, You/Them meters with voice activity, word count, transcript lag, backend, and the transcript as it lands. After: summary with clickable citations, read-only actions (ticks live in Obsidian), who-is-who panel, re-summary replaces with one undo. Quiet native look: Fluent UI React v9, Segoe UI Variable, system light/dark. Every transcript view puts speaker and text in separate columns. 4-step skippable first run. Notes deferred (PLAN 1, Later). Details: docs/ui-design.md.  (the app now offers System, Light or Dark, T9.1n.) |
| D22 | Tests use only the default models | Defaults are the English ladders in `models/manifest.toml` (CUDA and CPU: Parakeet TDT 0.6B v2 -> 110M -> base.en; QNN: 0.6B v2; Vulkan: small.en -> base.en) and the `summary` models. Any other model (large-v3-turbo, Parakeet v3, small.en off Vulkan) stays a user choice in the app, offered as recommended for the hardware, and is never tested, measured by a threshold or fetched for ci. Evaluation tools that choose the defaults (`model-eval`, `sum-eval`) stay, outside ci and every `accept`. |
| D23 | One install per architecture; Vulkan by default on x64, CUDA as an opt-in download | One x64 install and one ARM64 install, never a separate CUDA install and no choice of version at install time. On x64 summaries run on a discrete GPU through llama.cpp on Vulkan, with nothing to download from NVIDIA; transcription runs on what the model check keeps (Parakeet on the CPU, 0.03 of real time on 4 threads). The owner accepted Vulkan's measured cost (T8.4c: about 1.2 times CUDA's time, 91.8 to 93.9 s per hour of meeting, past that task's 90 s rule). On a machine with a suitable NVIDIA GPU, Settings offers a switch to CUDA: an in-app download through the checksum-pinned downloader, showing the size, from NVIDIA's own server (`developer.download.nvidia.com`); before a public release NVIDIA is asked in writing whether end users may fetch these files (`docs/cuda-libraries.md`). A machine without such a GPU never sees the offer. Replaces the earlier forms: libraries inside every install, then the CUDA build as the default. |
| D24 | In-app update, checked only when the user allows it | The app can update itself from one host the owner owns, through `earshot-net` (allowlist and audit log), never through Tauri's updater plugin. A "Check for updates" button in Settings; automatic checks are a device-local setting, off by default. A check sends no conversation content; the host sees the machine's IP address and the version asked for. A release is verified against a public key built into the app, not only by checksum. An update never installs during a meeting or a meeting job, and asks before restarting. Built in T9.1b. |

## 7. Open questions (agents ask the owner when they reach one)

- (Resolved, the owner: the project's own code is MIT, `LICENSE`. Third-party files keep their own terms; NVIDIA's parts are never under it.)
- Whether to re-host the AMI reference RTTMs/UEMs or derive them from AMI's own annotations (licence check in T0.4a).
- (Resolved 2026-11-07, the owner: the installer may carry NSIS's CPL-1.0 LZMA stub; the README credits it and says where its source is.)
- (Resolved 2026-11-07, the owner: the device switch of GATE-01a and the notification checks of GATE-02 move to after the x64 early release; the desktop has one output device. They stay open, and the gates stay `[ ]`.)
- (Resolved 2026-11-07, the owner: the app offers the pinned community Parakeet conversions (k2-fsa sherpa-onnx, CC-BY-4.0, checked by SHA-256) as downloads from their own hosts, in the x64 early release and until decided otherwise. Earshot does not host them: GitHub cannot hold the 2.27 GiB encoder, and the licence does not ask for it. The risk accepted is the uploader removing the files; converting from NVIDIA's `.nemo` and hosting on the owner's Hugging Face account is the way out if that happens.)
- (Resolved 2026-10-24: T8.3 targets Parakeet through Qualcomm's ONNX Runtime QNN plugin; the owner accepts the Qualcomm AI Stack License for development on their own machines; the NPU never runs a real meeting before an ETW check shows the plugin emits no telemetry.)
- (Resolved 2026-11-08, the owner: the Qualcomm QNN runtime libraries are bundled inside the ARM64 installer under clause 1(iv) of the Qualcomm AI Stack License, with its licence and notices files beside them; never downloaded on their own (docs/qnn-redistribution.md). Windows ML is not used.)
- (Resolved 2026-11-08, the owner: Earshot's own NPU conversions of Parakeet (`parakeet-qnn/<model>`, about 640 MB for the 0.6B and 137 MB for the 110M) are published on `pitslug/Earshot` with the CC-BY-4.0 credit, in the one release that holds every file Earshot hosts (titled "Earshot Dependencies"; its tag stays `cuda-set-llama-0.1.160-2` because installed apps fetch the CUDA set by that URL, and it is never marked latest, which would break the update check), and the app downloads them from the Models page through the manifest, checked by SHA-256. The community Parakeet copies themselves stay on their own hosts.)
- (Resolved 2026-11-07, the owner: end users fetch NVIDIA's files from NVIDIA's server without a written answer first. The owner accepts that risk at this scale.)
  Research for that decision: `docs/cuda-libraries.md` (no route settled on the licence text; ask NVIDIA in writing; measured about 1.35 GB on disk against D23's estimate of 2 to 3 GB; a CUDA install is about 400 to 500 MB by a guess, not D23's 40 MB app), `docs/arm64-cuda.md` (research half of T8.4b, which stays open for its build probe) and `docs/qnn-redistribution.md` (bundle inside the ARM64 installer, about 132 MB).
- (Resolved 2026-11-08, the owner: the update host is this repo's GitHub Releases, `github.com` and its download host `release-assets.githubusercontent.com`, for now. Settled: no MSI; one per-user NSIS installer, which updates without admin. docs/updates.md.)
- (Resolved 2026-11-08, the owner: T9.5 folders are one level with one folder per meeting, and the app opens on All meetings. Deleting a folder asks whether to delete its meetings or move them out, with moving as the default. docs/folders-design.md.)
- (Resolved 2026-11-08, the owner: NPUs other than Qualcomm's wait for test hardware, which the owner does not have. If one comes, the shape is: the ARM64 installer bundles Qualcomm's runtime, the only NPU on Windows ARM64 and the only route its licence allows (docs/qnn-redistribution.md); Intel (OpenVINO) and AMD (Ryzen AI) runtimes are optional downloads in the x64 app once the chip is detected, through the CUDA download's path (manifest, pinned SHA-256, verify before load), each after its own licence check, model conversion, accuracy measurement and telemetry check on real hardware. Windows ML's on-demand runtimes are not used, because silent updates would break the pins and the telemetry check.)
- Windows code signing for the installers (a paid certificate, the owner's decision): without it SmartScreen warns on install and possibly on every update. The update signature in D24 does not replace it.
- (Resolved, the owner: the opt-in CUDA download has two parts, in T8.4a.)
- (Resolved, the owner: our CUDA set is published in `pitslug/Earshot`'s releases, without waiting for NVIDIA's written answer. T9.1b's release host stays its own decision.)
- Installers (T9.1c, T9.1p): NSIS carries CPL-1.0 on its LZMA stub (clear, not permissive; OK it before an installer leaves the owner's machines); the Visual C++ runtime's four DLLs are bundled beside the exe (T9.1w, the owner), because downloading Microsoft's installer would need an admin prompt and a new network destination; WebView2 is `skip` because every other mode contacts Microsoft (Windows 11 ships it); the default per-user NSIS folder is the data folder, `%LOCALAPPDATA%\Earshot`, and the owner chose to keep it per-user.
- Sync encryption: TLS-only behind a reverse proxy vs end-to-end encrypted payloads (GATE-08).

---

## 8. Task list

### Status markers
- `[ ]` todo - `[~]` in progress (claimed; see LOG.md) - `[x]` done (commit: `git log --grep "^<task-id>:"`) - `[!]` blocked (waiting on an answer from the owner; see LOG.md)
- `GATE-NN` entries are gates. They are "done" only when `gates/GATE-NN.pass` exists.

### Rules for picking work (summary; full protocol in AGENTS.md)
Take the **first** task in document order whose status is `[ ]` and whose deps are all `[x]`. Tasks are done one at a time, one commit each (see AGENTS.md). Each task's `accept` command must pass before it is marked `[x]`.

M0 to M9 hold only v1 work; v1 is released at GATE-04. New ideas and features go only in M12 (and later milestones like it), with that milestone's own ids, never into M0 to M9 (the owner, 2026-11-08). A bug that blocks a v1 task or gate still goes before it. The section "Post-release v1 updates" is closed: it keeps the work done before M12 existed.

---

### M0 - Foundations

- [x] T0.1 Repository bootstrap
  - deps: none
  - do: rename branch `master` -> `main`; `.gitignore` (target, node_modules, fixtures caches, models, `fixtures/private`, `*.flac`, `*.wav` outside `fixtures/smoke`); `.editorconfig`; `rust-toolchain.toml` pinning the current stable Rust plus `rustfmt`, `clippy` and targets `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`, `wasm32-unknown-unknown`; Cargo workspace with every crate under `crates/` in 3.1 as an empty lib/bin that compiles (sync server excluded until M11; Tauri apps join at T7.1 and T10.2); `LOG.md`; `gates/README.md`; `acceptance/thresholds.toml` containing exactly the table in 4.2.
  - accept: `cargo build --workspace` and `cargo test --workspace`
- [x] T0.2 xtask: ci, doctor, accept skeleton
  - deps: T0.1
  - do: `cargo xtask ci` (fmt --check, clippy -D warnings, test, then a `purity` step that is a stub until T0.3); `cargo xtask doctor` (host triple, MSVC x64/ARM64 tools, Windows SDK, CMake, LLVM/clang for bindgen, Node LTS, pnpm, CUDA toolkit + `nvidia-smi`, Vulkan SDK, Docker, Android SDK/NDK; prints a table and machine-readable JSON); `cargo xtask accept <m>` framework that loads thresholds.toml, collects metrics, writes the reports in section 4, and fails on any miss or on a missing metric.
  - accept: `cargo xtask ci` passes; `cargo xtask accept m0` passes (m0 checks: thresholds.toml parses and every ID in 4.2 is present)
- [x] GATE-00 Toolchain prerequisites (conditional)
  - deps: T0.2
  - do: run `cargo xtask doctor`. If everything needed for M0-M7 on this host is present, write `gates/GATE-00.pass` yourself with the doctor output and mark done. Otherwise open the gate listing exactly what to install.
- [x] T0.3 Core purity guard
  - deps: T0.2
  - do: implement `cargo xtask purity` exactly as listed in 3.2 (wasm32 lib build; ban list over `cargo tree -p earshot-core -e normal`; `cargo clippy -p earshot-core --lib` with `crates/earshot-core/clippy.toml`); canary tests in xtask proving each guard trips (a banned normal dependency, a disallowed method in lib code) and that dev-dependencies and test-only `std::fs` reads are allowed. Replace the T0.2 stub so `ci` runs it.
  - accept: `cargo xtask purity` and `cargo xtask ci`
- [x] T0.3a Purity hardening and ci lint split
  - deps: T0.3
  - do: from the T0.3 review: (1) second `cargo tree -p earshot-core -e build --target all` pass banning native builders (cc, cmake, bindgen, pkg-config) with a canary path crate that has `[build-dependencies] cc`; (2) run the purity clippy step with `-F clippy::disallowed_methods -F clippy::disallowed_types` so an `#[allow]` in lib code cannot silence it, with a canary; (3) add `#![forbid(unsafe_code)]` to the core lib so FFI cannot be called, with a canary; (4) extend `crates/earshot-core/clippy.toml` with `std::path::Path::{exists, try_exists, is_file, is_dir, metadata, symlink_metadata, canonicalize, read_dir, read_link}`, `std::fs::DirEntry`, `std::io::{stdin, stdout, stderr}`, `std::env::{set_var, remove_var, set_current_dir}`; (5) make ci's workspace clippy step not apply core's clippy.toml to core test targets (e.g. `CLIPPY_CONF_DIR` pointing at a neutral config), so core tests may read fixtures per 3.2, with a canary proving a core test reading a file passes `cargo xtask ci` while the same call in lib code fails `cargo xtask purity`.
  - accept: `cargo test -p xtask --lib purity` and `cargo xtask ci`
- [x] T0.4 Fixture pipeline (LibriSpeech + smoke)
  - deps: T0.2
  - do: `fixtures/manifest.toml` (url, sha256, size, licence, attribution per file); `cargo xtask fetch-fixtures --set smoke|librispeech|ami|all` with resume and checksum verification; commit the `smoke` set + `fixtures/LICENSES.md`; pin the LibriSpeech test-clean and dev-clean subsets meeting the constraints in 4.1 (all 40 test speakers, >= 10 s enrolment + >= 3 further utterances each; smoke = 20 short clean utterances, >= 2 speakers). A test checks the subset constraints from the manifest metadata.
  - accept: `cargo xtask fetch-fixtures --set smoke,librispeech --verify` exits 0 and `cargo test -p xtask fixtures`
- [x] T0.4a AMI fixtures
  - deps: T0.4
  - do: add the AMI set: 2 test meetings, IHM-mix and SDM channels, pyannote AMI-diarization-setup `only_words` RTTMs and UEMs. Check every licence first; unclear licence -> stop and ask, and do not add that set.
  - accept: `cargo xtask fetch-fixtures --set ami --verify` exits 0
- [x] T0.5 Metrics in core
  - deps: T0.3
  - do: `earshot_core::metrics`: WER (word-level Levenshtein with S/D/I breakdown), English text normaliser (port of Whisper's EnglishTextNormalizer rules that matter: case, punctuation, numbers, contractions, British/American spellings list), DER (collar, overlap, optimal speaker mapping via Hungarian algorithm), SI-SDR, ERLE (segmental).
  - accept: `cargo test -p earshot-core metrics` with hand-computed reference cases for each metric
- [x] T0.5a Full Whisper English normaliser
  - deps: T0.5
  - do: from the T0.5 review: embed Whisper's full `english.json` British/American spelling table (MIT; cite the source and record its commit/hash), treat digit tokens as numeric values in number standardisation so suffix words attach ("25 percent" and "twenty five percent" normalise identically), handle "per cent" and currency as Whisper does; add `DerConfig` constructor or constant for the NIST +/-0.25 s collar (`collar: 0.5`) so runners cannot pass 0.25 by mistake. Add reference cases taken from Whisper's own normaliser behaviour (hand-written expected strings, not computed by our code).
  - accept: `cargo test -p earshot-core metrics` and `cargo xtask purity`
- [x] T0.6 Model manifest and fetcher
  - deps: T0.2
  - do: `models/manifest.toml` (whisper ggml tiny.en, base.en, small.en, small.en-q5_1, large-v3-turbo, large-v3-turbo-q5_0; Silero VAD ONNX; pyannote segmentation-3.0 ONNX and CAM++ or WeSpeaker ONNX from sherpa-onnx releases) with licence per model; `cargo xtask fetch-models [--set ci|desktop|all]`; pin SHA-256 on first verified download and commit the manifest.
  - accept: `cargo xtask fetch-models --set ci --verify`
- [x] T0.7 Domain identity base
  - deps: T0.3
  - do: in core: `EntityId` (UUIDv7 via `uuid::Builder::from_unix_timestamp_millis` from an injected `Clock` and seeded RNG; no `v7`/`rng` features), `Clock` trait, `Hlc` (hybrid logical clock), `SyncMeta { created_at, updated_at, deleted, origin_device_id }`, `SyncClass` marker traits (`Syncable`, `SyncableOptIn`, `NeverSync`) and the sync serialisation trait that only syncable types implement, with a compile-fail test (trybuild) proving a `NeverSync` type cannot be passed to it.
  - accept: `cargo test -p earshot-core identity` and `cargo xtask purity`

- [x] T0.5b SI-SDR of a silent estimate
  - deps: T0.5
  - do: `metrics::sisdr::si_sdr` returns +inf for an all-zero estimate, so a canceller that mutes everything would pass; return -inf (or an error) for a silent estimate, with a test; check every caller.
  - accept: `cargo test -p earshot-core metrics`

- [x] T0.8 Follow-ups: xtask tooling
  - deps: T0.7
  - do: decided by the owner: clear the advisory review findings in TODO.md. Each fix gets a test that fails without it where behaviour changes; items found to be wrong or not worth changing are closed with a one-line reason in the commit body. Fixed items are removed from TODO.md by the coordinator. Items: doctor Node LTS (doctor.rs), unit-less threshold cells (thresholds.rs), ci test step vs xtask feature unification (ci.rs), purity: reject cfg(clippy) variants with a source scan plus canary, the forbid-unsafe canary proving the `-F` flag, purity child cargo via `ci::cargo_command()` with a test (purity.rs; T0.3 area, always reviewed), crate-level clippy.toml step note, download: per-hop https and host check on redirects, body stall timeout, keep a cleanly truncated `.part`, the flaky resume test (download.rs), manifest: `--verify` re-hash of extracted LibriSpeech files, overlapping extract paths (manifest.rs, fixtures.rs).
  - accept: `cargo test -p xtask`, `cargo xtask ci`

### M1 - Echo canceller (pure Rust, in core)

- [x] T1.0 Raw capture tool
  - deps: T0.2
  - do: minimal `earshot-capture-wasapi`: shared-mode mic + loopback of the default render endpoint, f32 conversion, QPC timestamps per packet, silence synthesis when loopback delivers no packets; `earshot record-raw --seconds N --out <dir>` writes `mic.wav`, `loopback.wav` and `timestamps.json` (no AEC, no resampling). Non-device logic unit-tested; device tests `#[ignore]`. T6.3 extends this crate later.
  - accept: `cargo test -p earshot-capture-wasapi` and `cargo test -p earshot-cli`
- [x] T1.0a Raw capture device pairing and alignment
  - deps: T1.0
  - do: use the same device role (eConsole) for mic and loopback, add a `--mic-role` option, print both device names and formats to stderr before capturing; anchor frame 0 of both WAVs at the same origin QPC (trim leading frames of a first packet stamped before the origin; take the origin before starting the streams) so `wav_start_qpc` is identical for both tracks. Update the tests that encode the old behaviour.
  - accept: `cargo test -p earshot-capture-wasapi` and `cargo test -p earshot-cli`, plus a short `record-raw` showing equal `wav_start_qpc` on both tracks
- [x] GATE-10 Early raw capture on hardware
  - deps: T1.0a
  - do: open the gate with steps: on the desktop, run `earshot record-raw --seconds 120` during a real call on speakers (not headphones), talking over the far end at times; copy the output to `fixtures/private/raw-<date>/`; report anything odd (gaps, silence, clipping). T1.6 reports real-world ERLE on these pairs as informational.

- [x] T1.1 Audio primitives
  - deps: T0.5
  - do: frame/sample types, ring buffer, i16/f32 conversion, `Resampler` wrapper around rubato with runtime-adjustable ratio (for drift), WAV read/write over `std::io` traits.
  - accept: `cargo test -p earshot-core audio`
- [x] T1.1a Resampler ratio fixes
  - deps: T1.1
  - do: from the T1.1 review: compute the start-up trim once from the nominal ratio and make `output_delay()` report the value actually trimmed (rubato's delay scales with the current ratio, so a reset at a non-unity ratio misaligns by a frame); apply a relative ratio without ramp when nothing has been processed since construction/reset so a constant-ratio stream yields exactly round(input * ratio) and two identical streams match; bound accepted rates/ratio in `with_max_deviation` (error, not abort); make the vacuous `max_output_len` assertion real. Tests at a non-unity ratio, both directions, tolerance no wider than the documented bound.
  - accept: `cargo test -p earshot-core audio` and `cargo xtask purity`
- [x] T1.2 Call simulator
  - deps: T1.1a, T0.4, T0.3a
  - do: `earshot_core::sim`: image-source RIR generator (room size, absorption, positions), far-end and near-end speech passed in as buffers (tests load them from fixtures), configurable bulk delay, clock drift (ppm), loudspeaker nonlinearity (soft clip), background noise at SNR, mid-call echo-path change, and labelled silences between concatenated utterances (ground truth for VAD). Scenario files in `acceptance/aec_scenarios.toml`: far-only (noise-free), near-only, double-talk at SER 0 dB and -10 dB, path-change, delay-450ms, drift-plus/minus-200ppm, noisy-office, headphones (no echo), switch-speakers-to-headphones (echo stops mid-call), switch-headphones-to-speakers (echo starts mid-call), delay-jump (bulk delay jumps, e.g. 40 ms to 400 ms, as when switching to Bluetooth). Outputs mic, reference, clean near-end, echo-only signals and speech-region labels. Seeded and deterministic.
  - accept: `cargo test -p earshot-core sim` (energy and alignment sanity checks, determinism)
- [x] T1.2a Loudspeaker nonlinearity anchored to real data
  - deps: T1.2, GATE-10
  - do: measure loudspeaker-path nonlinearity on the GATE-10 recording with an estimator validated on simulator data (fit the best linear echo path on far-end-only stretches; measure the unexplained residual above the noise floor); set the gating scenarios' softclip to the level whose simulated residual is at least as severe as the measurement; keep the softclip=4 scenarios as informational `softclip4-*` copies; done blind to canceller scores (decided by the owner).
  - accept: the tool's tests (incl. `-- --ignored` on the private recording), `cargo test -p earshot-core sim`, `cargo xtask ci`
- [x] T1.3 Delay and drift estimation
  - deps: T1.2
  - do: GCC-PHAT bulk delay estimator with tracking (including sudden delay jumps and losing/regaining lock when echo stops/starts), drift estimator feeding the resampler ratio, and an optional warm-start delay hint (from the T7.4 audio check) that live tracking always overrides.
  - accept: `cargo test -p earshot-core aec::delay` (on every scenario with echo present: delay within +/-2 ms, drift within +/-20 ppm; on no-echo scenarios (near-only, headphones) the estimator reports `NoLock` and the AEC does not adapt)
- [x] T1.4 Linear echo canceller
  - deps: T1.3
  - do: partitioned-block frequency-domain adaptive filter (MDF/PBFDAF, rustfft), normalised step-size control, divergence detection and reset. Zero added latency on the mic path; drift is corrected on the reference by a zero-latency fractional reader (windowed sinc) at the estimated drift rate, not by `audio::Resampler` whose ~18 ms buffering exceeds the shortest echo delays (3.3).
  - accept: `cargo test -p earshot-core aec::linear` (aec.erle_linear, aec.convergence on far-only)
- [x] T1.5 Double-talk handling and residual echo suppression
  - deps: T1.4
  - do: double-talk detector freezing adaptation, frequency-domain residual echo suppressor, optional comfort noise, `EchoCanceller` public API (`process(mic, ref) -> out`).
  - accept: `cargo test -p earshot-core aec`
- [x] T1.5c Nonlinear residual echo suppression
  - deps: T1.5, T1.2a
  - do: approved by the owner: the suppressor may use a fixed, reported output latency of at most 32 ms (PLAN 3.3); timestamps and `aec::eval` compensate it. Add a nonlinear-echo-aware residual suppressor stage (standard in production AECs, e.g. a postfilter driven by a nonlinear echo estimate; cite the method) so double talk improves under the measured loudspeaker distortion, without attenuating the near-end voice (passthrough and near-end level guards stay). Re-measure `aec.doubletalk_sisdr` at SER 0 and -10 before any threshold discussion.
  - accept: `cargo test -p earshot-core aec` (new tests) and `cargo xtask purity`
- [x] T1.5b Stereo-reference echo canceller
  - deps: T1.5, T1.2a
  - do: approved by the owner after T1.2a found a real recording's echo has several paths close together (likely two loudspeakers) while the reference is a mono downmix. Simulator: stereo loudspeaker scenarios (two speakers at different positions, L/R far-end content partly decorrelated as in real stereo calls) alongside the mono ones; canceller: multichannel (2-reference) frequency-domain adaptive filter with the standard stereo-AEC non-uniqueness mitigation (cite the method), falling back to mono when the channels are identical; zero added latency on the mic path. The informational GATE-10 metric uses a stereo raw recording when one exists (GATE-10b).
  - accept: `cargo test -p earshot-core aec` (new stereo tests) and `cargo xtask purity`
- [x] T1.6 M1 acceptance
  - deps: T1.5, T1.5b, T1.5c, T1.2a, T0.5b
  - do: implement `accept m1`: run every scenario, compute all `aec.*` metrics per the definitions in 4.0, release-build RTF benchmark. If GATE-10 recordings exist in `fixtures/private/`, report real-world ERLE as informational (not a threshold).
  - accept: `cargo xtask accept m1`

- [x] T1.6a Follow-ups: core audio, identity, metrics, prompt
  - deps: T1.6
  - do: decided by the owner: clear the advisory review findings in TODO.md. Each fix gets a test that fails without it where behaviour changes; items found to be wrong or not worth changing are closed with a one-line reason in the commit body. Fixed items are removed from TODO.md by the coordinator. Items: resampler alignment doc or trim tested in both directions (audio/resample.rs), WavAudio checked constructor and dead `.max(1.0)` (audio/wav.rs), UUIDv7 same-ms ordering documented or counter (identity/id.rs), never_sync_envelope test asserting the NeverSync gate (tests/identity_sync_class.rs), DerConfig.collar private with constructors (metrics/der.rs), prompt ContextBudget repair headroom (prompt).
  - accept: `cargo test -p earshot-core`, `cargo xtask ci`

- [x] T1.6b Held-out evaluation set for the echo canceller
  - deps: T1.6
  - do: adopted by the owner (docs/aec-dev-split.md): the current scenarios and seeds are the dev set (all tuning); add a held-out set (the gating scenarios re-rendered with new seeds and a different LibriSpeech speaker pair) reported by `accept m1` as an informational check, plus a committed baseline (generated by a `cargo xtask` command, never hand-edited) and a no-regression check for canceller changes. Thresholds unchanged; nothing is retuned.
  - accept: `cargo test -p earshot-core aec`; `cargo test -p xtask`; `cargo xtask accept m1` (held-out results reported next to dev, baseline committed)

### M2 - Transcription

- [x] T1.6c Echo canceller generalises to new voices
  - deps: T1.6b
  - do: found by T1.6b (2026-10-24): on the held-out set (new seeds and a new LibriSpeech speaker pair) the canceller misses most dev bars: far-only ERLE 10.9 dB (dev 39.4; bar 25) with convergence up to 7 s, stereo far-only 5.9 dB (dev 35.2) and up to 14.2 s, double talk SI-SDR 6.6 dB mono and 4.4 dB stereo (bar 10), path change up to 3.5 s; a real recording's ERLE is close to the held-out figures, so the dev numbers overstate real performance. Find why (per-scenario traces on dev-only data: the delay lock, step control, the nonlinear stage, the stereo normalisation) using more dev voices (add dev speakers and seeds to the dev set, never the held-out pair), fix without retuning on held-out, and show held-out improvement with no dev bar lost. The held-out baseline is regenerated only after a fix and logged. Transcript echo cleaning (asr.echo_leak) already protects the product; this is about cleaner near-end audio and less reliance on text de-duplication.
  - accept: `cargo test -p earshot-core aec`; `cargo xtask accept m1` (held-out numbers before and after logged; no dev metric worse); `cargo xtask accept m2` (asr.echo_leak, asr.wer_aec_delta)

- [x] T2.1 ASR and VAD traits, chunker
  - deps: T1.1, T0.7
  - do: `Transcriber`, `StreamingTranscriber`, `Segment`/`Word` types, `Vad` trait + energy VAD, VAD-driven chunker (max 15 s, overlap, LocalAgreement-style stabilisation of partial text), `FakeTranscriber`.
  - accept: `cargo test -p earshot-core asr`
- [x] T2.2a whisper.cpp build with CUDA
  - deps: T0.6
  - do: in `earshot-asr-whisper`, add whisper-rs with features `cuda` (built when `doctor` reports CUDA) and `vulkan` (declared; wired in T2.6); get whisper.cpp compiling and linking with CUDA 13.4 for sm_120 (RTX 5080) and on CPU; pin the whisper-rs and whisper.cpp versions and record them in PLAN section 6 via the coordinator; a smoke test that loads tiny.en from `EARSHOT_MODELS` and transcribes one smoke clip on CPU and, when CUDA is present, on CUDA (no trait impl yet).
  - accept: `cargo test -p earshot-asr-whisper` and, when `doctor` reports CUDA, `cargo test -p earshot-asr-whisper --features cuda`
- [x] T2.2b whisper.cpp Transcriber
  - deps: T2.1, T2.2a
  - do: implement `Transcriber` / `StreamingTranscriber` on the T2.2a build: model loading, word timestamps, language (default en, `auto` option), thread count, cancellation, decoding settings from 4.0.
  - accept: `cargo test -p earshot-asr-whisper` (tiny.en on smoke set, asr.wer_smoke_tiny) and, when `doctor` reports CUDA, `cargo test -p earshot-asr-whisper --features cuda`
- [x] T2.3 ONNX Runtime foundation + Silero VAD
  - deps: T2.1, T0.6, T1.2
  - do: `earshot-onnx`: ort session builder with execution-provider selection (CPU, CUDA, QNN behind features; linking per 3.4), Silero VAD implementing `Vad`.
  - accept: `cargo test -p earshot-onnx vad`: >= 95% per-frame accuracy over the labelled silent gap frames between utterances, plus a mute guard that every labelled utterance contains detected speech; full-span accuracy reported as informational (the simulator's utterance spans include each clip's own silences). Decided by the owner.
- [x] T2.4 CLI: transcribe
  - deps: T2.2b, T2.3
  - do: `earshot transcribe <wav> [--model] [--backend] [--stream] [--json]`.
  - accept: `cargo run -p earshot-cli -- transcribe fixtures/smoke/<file>.flac --json` returns non-empty segments (the committed smoke clips are FLAC); CLI test in `crates/earshot-cli/tests`
- [x] T2.4b LocalAgreement text anchors
  - deps: T2.4
  - do: found by the T2.5 investigation: whisper.cpp collapses the last words of a partial decode to the buffer end, which pushed LocalAgreement's committed end forward and re-emitted committed text. Committed runs of 3+ words anchor without a time bound only at untimed words (start == end); the tentative anchor never reaches back before the last committed word's start; legitimately repeated speech is kept (test).
  - accept: `cargo test -p earshot-core asr`
- [x] T2.4c Fresh whisper state per beam-search call
  - deps: T2.2b
  - do: found by the T2.5 investigation: whisper.cpp 1.8.3 beam search samples candidates with decoder 0's RNG, seeded only when a state is created, so a reused state made batch results depend on earlier calls (AMI 31.5% vs 52.9%). Beam calls run on a fresh state (old one freed first); every call's whisper log is checked for its backend (`evidence::check_call_log`).
  - accept: `cargo test -p earshot-asr-whisper`
- [x] T2.4d Whisper temperature fallback
  - deps: T2.4c
  - do: decided by the owner: when a window's output is too repetitive (compression ratio) or low-confidence (average log-probability), retry it at higher temperature, Whisper's standard recipe (reference: openai/whisper transcribe.py thresholds), for batch and live decoding. `DecodingOptions` records the fallback settings; the report says how many windows fell back. Developed on dev data only (dev-clean, smoke); never tuned on the acceptance sets. A test reproduces a repetition loop and shows the fallback breaks it.
  - accept: `cargo test -p earshot-asr-whisper`, `cargo test -p earshot-core asr`
- [x] T2.4e Cross-track echo de-duplication
  - deps: T2.4d, T1.6
  - do: decided by the owner: the transcript must be clean: far-end speech that leaks into the mic is never transcribed twice or attributed to "Me". A pure core function compares the mic ("Me") transcript with the reference ("Them") transcript and drops mic words that repeat reference words in order at about the measured echo delay (from the AEC delay lock, plus a tolerance), requiring a run of several words so that genuine talk-over ("yes" said by both) survives; usable on live finals and on the final pass. Add `asr.echo_leak` to `accept m2` (simulated far-only and double-talk scenarios at SER 0 and -10, mono and stereo) and keep near-end retention covered by `asr.wer_aec_delta` at SER 0. Tests include talk-over with shared words, repeated phrases, a near-end speaker quoting the far end, and delay drift.
  - accept: `cargo test -p earshot-core asr`, `cargo xtask accept m2`
- [x] T2.4f Echo cleaning review follow-ups
  - deps: T2.4e
  - do: from the T2.4e review: no echo cleaning without an echo-path lock; a mic speaker talking alone is Me; speaker-check margin keyed by exact model and re-picked on dev by a stated rule; the aec condition's leak and near-end recall surfaced with a 98% near-end floor; ambiguous words counted; a no-echo scenario with far speech required; the rejected acoustic gate removed; test tightening.
  - accept: `cargo test -p earshot-core asr`, `cargo test -p earshot-core diar`, `cargo xtask ci`
- [x] T2.4g Enough near-end words for asr.wer_aec_delta
  - deps: T2.4f
  - do: found in T6.6d: `asr.wer_aec_delta` scores 35 near-end words, so one recognition error is 2.9 points and -90 dBFS dither alone swings it by up to 17 points; it cannot tell canceller changes from whisper noise. Score it over enough near-end speech to be stable (for example the SER 0 double-talk scenario rendered with several fixed seeds and the existing near-end pool, chosen and written down before measuring), report the per-seed spread, and keep SER -10 informational. The threshold is unchanged; record the before and after values in LOG.
  - accept: `cargo xtask accept m2`
- [x] T2.5b AMI per-headset scoring
  - deps: T0.4a
  - do: decided by the owner: `asr.wer_ami_ihm` uses the standard AMI IHM condition: add the per-headset audio (`<meeting>.Headset-0..3.wav`, same AMI host and CC BY 4.0 licence, SHA-256 pinned in fixtures/manifest.toml) to the `ami` set, transcribe each headset separately and score each against its own speaker's words (reference rules unchanged and confirmed by the owner), summing WerCounts. The merged-by-start-time single stream is removed from the metric.
  - accept: `cargo test -p xtask fixtures`, `cargo xtask fetch-fixtures --set ami --verify`
- [x] T2.5c Far-field AMI number (informational)
  - deps: T2.5b
  - do: from TODO (the owner and coordinator): beside `asr.wer_ami_ihm` (close-talk headsets), report an informational far-field number on the same meetings from AMI's single distant microphone (SDM, one array channel) with the same oracle segments and reference rules, so laptop-mic and desk-mic setups have a realistic accuracy figure. Audio pinned through fixtures/manifest.toml (CC BY 4.0). Not a threshold; measure first, then discuss with the owner.
  - accept: `cargo test -p xtask fixtures`; `cargo xtask fetch-fixtures --set ami --verify`; the measured SDM number logged
- [x] T2.5 M2 acceptance (asr.wer_stream_delta and asr.wer_aec_delta re-defined on the production setup, see LOG)
  - deps: T2.4, T1.6, T0.4a, T0.5a, T2.4d, T2.5b, T2.4e, T2.4g
  - do: implement `accept m2` covering every `asr.*` metric in 4.2 per the definitions in 4.0 (CUDA when `doctor` reports it, otherwise CPU; record backend and decoding settings in the report). The recorded backend must come from evidence of what whisper.cpp actually initialised (its `whisper_backend_init_gpu` log / weight-buffer lines, captured by the transcriber or the runner), never from `backend_info()` inference alone: whisper.cpp silently falls back to CPU if GPU init fails. First move the whisper log capture and `LogBackend` classification from the CLI into the earshot-asr-whisper lib so the CLI, the crate's tests and `accept m2` share one rule.
  - accept: `cargo xtask accept m2`
- [x] T2.5d Parakeet TDT skipped speech is decoded again
  - deps: T2.5
  - do: found by the M2 lane (2026-10-25): parakeet-tdt-0.6b-v2 left 20 s of one LibriSpeech stream without words (blank with duration 4 on every frame, depending on the exact input start and length). Piece decoding re-decodes VAD speech that no word covers for a transcriber that reports `may_skip_speech`, alone or in halves, bounded; live and final stay identical.
  - accept: `cargo test -p earshot-core asr_pieces`, `asr_stretch`; the ignored stream-19 regression test with its negative control; `cargo xtask accept m2`
- [x] T2.5e Stream delta becomes a determinism check
  - deps: T2.5
  - do: decided by the owner: under D20 live stretches equal the final pass, so `asr.wer_stream_delta` is 0 by construction and measures determinism and stretch equality, not accuracy (model accuracy is covered by `asr.wer_ami_ihm` and `asr.wer_ls_base`). Rename the id and its 4.0/4.2 text to say so (same production streams, same m2_prod measurement; the threshold becomes `== 0` words differing after Whisper normalisation, a tightening), in PLAN 4.2, `acceptance/thresholds.toml` and `EXPECTED_IDS` in one commit; no stronger reference is added. No run of its own: its `accept m2` is the next one that runs anyway (GATE-03's Surface runs, or a canceller change), so do it just before that run.
  - accept: `cargo test -p xtask --test thresholds`, `cargo xtask accept m0`, `cargo xtask accept m2`
- [x] T2.6 Vulkan build and ARM64 cross-compile
  - deps: T2.5
  - do: wire the `vulkan` feature; `cargo xtask build --target aarch64-pc-windows-msvc` compiles the workspace (whisper.cpp via CMake for ARM64; use clang-cl for the ARM64 whisper.cpp build if MSVC lacks the needed intrinsics). If a toolchain is missing, ask rather than installing system software.
  - accept: `cargo xtask build --target aarch64-pc-windows-msvc`, and `cargo xtask build --features vulkan` when `doctor` reports the Vulkan SDK

- [x] T2.5a Follow-ups: whisper and transcribe CLI
  - deps: T2.5
  - do: decided by the owner: clear the advisory review findings in TODO.md. Each fix gets a test that fails without it where behaviour changes; items found to be wrong or not worth changing are closed with a one-line reason in the commit body. Fixed items are removed from TODO.md by the coordinator. Items: whisper param template cache per language with strategy/temperature applied on the clone (transcriber.rs), determinism and cancel-lag test robustness (tests/transcriber_api.rs), CMAKE_CUDA_ARCHITECTURES comment plus doctor reporting compute capability, GGML_NATIVE=OFF with an explicit AVX2 baseline for release builds, CLI: requested vs detected language in JSON, cheap preconditions before the model load and `--backend cpu` in CLI tests, KNOWN_MODELS from models/manifest.toml, stream mode printing inside the push loop (earshot-cli transcribe.rs). DTW word timestamps stay a TODO (feature work).
  - accept: `cargo test -p earshot-asr-whisper`, `cargo test -p earshot-cli`, `cargo xtask ci` (accept m2 is T2.5's own accept and fails for T2.5's reasons)

### M3 - Diarisation and voiceprints

- [x] T3.1 Diarisation in core
  - deps: T2.1
  - do: `SpeakerSegmenter` and `SpeakerEmbedder` traits; agglomerative cosine clustering with threshold and optional known speaker count; word-to-speaker assignment; overlap handling; fakes for tests.
  - accept: `cargo test -p earshot-core diar` (synthetic embeddings with known clusters)
- [x] T3.1b Forced speaker count
  - deps: T3.1a
  - do: `ClusteringConfig::with_num_speakers` forces nearest-pair merges regardless of distance, so outlier singletons can survive while the real speakers merge (confirmed by probe in the T3.2-T3.4 review). Implement pyannote's rule (cut the dendrogram where the number of large clusters is closest to the target) or merge sub-`min_cluster_size` clusters into their nearest large cluster first; test with real clusters plus outliers.
  - accept: `cargo test -p earshot-core diar`
- [x] T3.1a Diarisation fidelity to pyannote 3.1
  - deps: T3.1
  - do: from the T3.1 review: centroid linkage as Euclidean distance between un-normalised means of unit embeddings so pyannote's 0.7046 threshold transfers directly (or document the conversion and make dev calibration mandatory); `min_active_ratio` default 0.0 unless a pyannote source line is cited; `constrained_assignment` default false (pyannote 3.1 default) with the deviation documented; `max_train_embeddings` 1000 as in pyannote; module docs name every deviation from pyannote's released defaults with its expected effect on DER; fakes embed overlapped samples as a centroid mixture plus noise, a test that `exclude_overlap: true` gives lower DER than false on an overlap scene, and a borderline clustering test (cluster distances near the threshold with realistic noise); fix the assign.rs:82-86 doc.
  - accept: `cargo test -p earshot-core diar` and `cargo xtask purity`
- [x] T3.2 ONNX diarisation backends
  - deps: T3.1a, T2.3
  - do: pyannote segmentation-3.0 and CAM++/WeSpeaker embedding in `earshot-onnx`, sliding windows, batching.
  - accept: `cargo test -p earshot-onnx diar` (two-speaker smoke mix from fixtures correctly split)
- [x] T3.3 Track policy
  - deps: T3.2
  - do: two-track policy and in-person single-track policy from 3.3.
  - accept: `cargo test -p earshot-core diar::policy`
- [x] T3.4 Voiceprints
  - deps: T3.2, T0.7
  - do: `Person` (with `merged_into`) and `Voiceprint` types on the T0.7 base, enrolment, running centroid update, open-set matching with a threshold calibrated on the dev-clean split only, embedding-model id versioning (mismatched models never compared), `VoiceprintRepo` trait with in-memory impl, "Me" enrolment API.
  - accept: `cargo test -p earshot-core voiceprint`
- [x] T3.2b CAM++ clustering threshold calibration
  - deps: T3.2, T3.3
  - do: calibrate the diarisation clustering threshold for the CAM++ embedder on synthetic scenes built only from the pinned LibriSpeech dev-clean speakers (never test-clean or AMI); the selection criterion is fixed in code before measuring; the constant is committed with its provenance and a test that reproduces it, like the voiceprint threshold. pyannote's 0.7046 stays WeSpeaker-only.
  - accept: `cargo test --release -p earshot-onnx diar_calibration -- --include-ignored` (the full reproduction is a visible `#[ignore]` in the default run: 16 min debug, 3-4 min release; the owner to confirm this use of ignore)
- [x] T3.5 M3 acceptance
  - deps: T3.3, T3.4, T2.5
  - do: implement `accept m3` per 4.0 (`dia.der_*` on AMI; `dia.count_error` on synthetic 1-6 speaker mixes + AMI; `vp.*` on LibriSpeech test speakers with 2-3 s test clips, threshold taken from the dev-clean calibration, never re-tuned here).
  - accept: `cargo xtask accept m3`
- [x] T3.5a Speaker-count bound for larger meetings (bound <= 1 confirmed)
  - deps: T3.5
  - do: decided by the owner: `dia.count_error` is a median over 34 items (10 of them k = 1-2), so it cannot see a diarizer that merges 5-6 speaker meetings. Add a PLAN 4.2 row `dia.count_error_large` (M3): mean absolute speaker-count error over the k = 4, 5 and 6 synthetic test-clean scenes of `accept m3` (same scenes and seeds, no new data), `<= 1`, the value set from the requirement before measuring (the owner to confirm the value when the task starts). Change PLAN 4.2, `acceptance/thresholds.toml` and `EXPECTED_IDS` in one commit; the m3 runner emits exactly one Measurement for it and recomputes it from the reported per-scene counts. If today's diarizer misses it, report the number and stop: never re-tune clustering on test-clean.
  - accept: `cargo test -p xtask --test thresholds`, `cargo xtask accept m0`, `cargo xtask accept m3`

### M4 - Summaries

- [x] T4.0 earshot-net
  - deps: T0.3
  - do: `earshot-net` crate per 3.1 and 3.9: one reqwest + rustls client, host allowlist (built from the model manifest hosts, the configured LLM URL and, later, the sync server), audit log of every request (method, host, path, byte counts), URL validation helper (http/https only, host present); a ci `net` step that fails if an HTTP client crate appears in any shipped crate's dependency tree outside `earshot-net` (xtask exempt).
  - accept: `cargo test -p earshot-net` (allowlist rejects other hosts; audit log records requests to a local test server) and `cargo xtask ci`
- [x] T4.1 Summary schema and validator
  - deps: T0.5, T0.7
  - do: Rust types for summary v1 with schemars; generate and commit `schemas/summary.v1.json`; xtask drift check in `ci`; validator in core returning structured errors including grounding (citations must exist).
  - accept: `cargo test -p earshot-core summary` and `cargo xtask ci`
- [x] T4.2 Prompt builder
  - deps: T4.1, T2.1
  - do: messages from transcript + user notes + speakers + template; token estimate; map-reduce chunking; templates as data files (`general`, `one-on-one`, `standup`).
  - accept: `cargo test -p earshot-core prompt` (snapshot tests)
- [x] T4.3 OpenAI-compatible client
  - deps: T4.2, T4.0
  - do: `earshot-llm` on `earshot-net`: chat completions, response_format fallback chain from 3.7, repair loop (max 2), timeouts, cancellation, optional API key, endpoint URL validation, `Summarizer` impl.
  - accept: `cargo test -p earshot-llm`
- [x] T4.4 LLM replay fixtures
  - deps: T4.3
  - do: `fixtures/llm/` cases (valid, fenced JSON, trailing prose, missing fields, bad citations, truncated) + local mock server (bound to 127.0.0.1:0) in tests. Optional live test runs only if `EARSHOT_LLM_URL` is set, and never gates.
  - accept: `cargo test -p earshot-llm replay`
- [x] T4.5 M4 acceptance
  - deps: T4.4
  - accept: `cargo xtask accept m4`

- [x] T4.5a LLM robustness and gate hardening
  - deps: T4.5
  - do: from the M4 review: (1) fall back on response_format only for a rejection on the first request of a run or one whose message names response_format/json_schema/json_object; a later 400 is `LlmError::Http`; (2) reserve repair headroom in planning or drop the echoed assistant message when the repair prompt would exceed the budget; (3) cap the echoed reply (e.g. 4 x max output tokens); (4) on `finish_reason: length` skip inner-object extraction and report truncation; (5) hand-written `Debug` for `earshot_net::NetRequest` redacting authorization, proxy-authorization, cookie and `*-api-key` headers; (6) move the mock/replay code into a non-shipped `earshot-llm-replay` crate (or add a ci check that shipped crates build without the `replay` feature); (7) add a committed unrecoverable `gives_up` replay case with `expect.outcome: "invalid"`, asserted to end in `Invalid { attempts: 3 }` with its recorded flow, counted in the flow check and explicitly excluded from `sum.schema_replay`; document that `ChatClient::complete` needs a tokio time driver.
  - accept: `cargo test -p earshot-llm`, `cargo xtask accept m4`, `cargo xtask ci`

### M5 - Storage and Obsidian export

- [x] T5.1 Data model
  - deps: T3.4, T4.1
  - do: the remaining entities from 3.5 on the T0.7 identity base (Meeting, Track, AudioFile, Speaker, Note with meeting-relative anchors, Summary, ExportRecord), repository traits, in-memory repo.
  - accept: `cargo test -p earshot-core model` (extends the T0.7 compile-fail test to every `NeverSync` entity)
- [x] T5.2 SQLite store and audio files
  - deps: T5.1
  - do: `earshot-store`: rusqlite (bundled), migrations, WAL, FLAC writer per track (flacenc, `par` feature off), retention job, layout and crash-safety rules from 3.6.
  - accept: `cargo test -p earshot-store` (includes: AudioFile row only after FLAC finalised; retention and re-transcribe tolerate a missing file)
- [x] T5.3 Obsidian renderer (core)
  - deps: T5.1
  - do: pure renderer per 3.8, filename sanitisation, managed-region merge function, frontmatter merge rules, em-dash option.
  - accept: `cargo test -p earshot-core export` (insta golden files, YAML frontmatter parse)
- [x] T5.4 Export writer
  - deps: T5.2, T5.3
  - do: atomic write, re-export merge, conflict handling if the file was renamed or moved (find by `earshot_id`), vault path validation.
  - accept: `cargo test -p earshot-store export` (includes recovery from a crash between rename and ExportRecord insert: the orphan file is found by `earshot_id` and re-used)
- [x] T5.5 M5 acceptance
  - deps: T5.4
  - accept: `cargo xtask accept m5`

- [x] T5.2c Store hardening
  - deps: T5.2
  - do: from the M5 review: orphan audio sweep in `run_retention` (`.flac`/`.part` with no row older than a grace period, counted in the report) and `.part` cleanup at startup; persist the HLC high-water mark (seed `Store::hlc` from the max stored timestamps) so `updated_at` never goes backwards across restarts; `PRAGMA secure_delete = ON` and a WAL checkpoint after `forget_voiceprints` so forgotten voiceprints do not survive in free pages/WAL (PLAN 3.9 #4); route `replace_segments` through a store transaction; `device_id` as INSERT OR IGNORE + SELECT in one transaction; a test pinning a hash of each applied migration; make in-memory and SQLite agree on `audio_file.path` uniqueness via the repo contract.
  - accept: `cargo test -p earshot-store` and `cargo test -p earshot-core model`
- [x] T5.4a Export safety and speed
  - deps: T5.4
  - do: from the M5 review: never clobber a concurrent Obsidian/LiveSync edit (re-stat len+mtime right before the rename and abort with a retryable error on change; on create, rename without replace so a late foreign note is never overwritten); write the intended ExportRecord before the rename (or limit the no-record search to the target folder's `<stem>*.md`) so the first export of a meeting does not walk the whole vault; check the meeting exists before writing; strip/re-emit a UTF-8 BOM; preserve mixed line endings; quoted inline tags; clean stray temp files; directory fsync where the platform needs it; title byte-length cap for ext4.
  - accept: `cargo test -p earshot-store export`, `cargo test -p earshot-core export`, `cargo xtask accept m5`

- [x] T5.4b On-demand export and full search
  - deps: T5.4a
  - do: decided by the owner: Obsidian export is on demand only (single meeting, or export all / changed), never automatic. With no export record, search the whole vault (non-hidden folders) for the note's `earshot_id` so a renamed or moved note is always found and never duplicated (cost accepted because export is user-initiated); restore accept m5 idempotence step 6 to adopting a moved note with no record. Also from the T5.4a review: stray temp cleanup only for temps older than a few minutes (never another export's live temp); `\\?\` prefix for long paths in `rename_noreplace` (std's `maybe_verbatim` rule) with a >260-char test; `forget_voiceprints` retries the WAL checkpoint briefly and a TRUNCATE checkpoint runs at store open/close so a forgotten vector cannot outlive the session. Note for v2 (Android): replace hard_link+remove with renameat2/O_EXCL fallback when M10 lands.
  - accept: `cargo test -p earshot-store`, `cargo test -p earshot-core export`, `cargo xtask accept m5`

- [x] T5.4c Follow-ups: store and export
  - deps: T5.4b
  - do: decided by the owner: clear the advisory review findings in TODO.md. Each fix gets a test that fails without it where behaviour changes; items found to be wrong or not worth changing are closed with a one-line reason in the commit body. Fixed items are removed from TODO.md by the coordinator. Items: one vault index per export batch, a walked note at an unusable path is an error not a duplicate, future-mtime temps and same-note concurrent temps (unique temp names) with the module doc corrected (export.rs), verbatim-prefix decision on the absolute length (fsutil.rs).
  - accept: `cargo test -p earshot-store`, `cargo xtask accept m5`, `cargo xtask ci`

- [x] T5.4d Follow-ups: store and export, second round
  - deps: T5.4c
  - do: clear the T5.4c review advisories (decided by the owner: clear all advisories): never remove a temp or probe this process did not create, batch index learns its own writes and rebuilds when old, temp staleness judged by the folder's own clock (probe file), `VaultIndex` internal, speakers listed in creation order (`created_at` then id) in the repo contract, InMemoryRepo and SQLite.
  - accept: `cargo test -p earshot-store`, `cargo xtask accept m5`, `cargo xtask ci`

### M6 - Windows capture and runtime

- [x] T6.1 Capture traits and replay source
  - deps: T1.1
  - do: `AudioSource`, `CaptureSession` (mic + optional ref, device timestamps), buffer replay source in core (file opening stays in the CLI/runtime).
  - accept: `cargo test -p earshot-core capture`
- [x] T6.2a Pipeline front end (split from T6.2: too large for one commit)
  - deps: T6.1, T1.5, T2.1, T3.3, T4.2, T5.1
  - do: step-driven sans-IO capture front end from 3.3: `pipeline::Pipeline` implements `CaptureSink`; per-track intake (packet-relative runs, resampling to 16 kHz, untrusted samples sanitised, gaps as silence, bounded jumps and track lag), echo cancellation on whole blocks (stereo reference via `process_stereo`, latency pre-roll removed), drift steering on the reference resampler only, events: cleaned Audio per track, Levels, Stats (AEC and track counters), Discontinuity, Error, Ended.
  - accept: `cargo test -p earshot-core pipeline` (fakes only)
- [x] T6.2b Live transcription and final-pass request
  - deps: T6.2a, T6.1, T2.1
  - do: per-track `StreamingChunker` over the pipeline's Audio, driven from `poll()` (core spawns no thread); track and segment ids from the config; Transcript events; backpressure (partials coalesced or off when the host falls behind, ASR backlog in Stats); an ASR error disables only that track's live ASR; on end, finish both chunkers and emit `FinalPassRequested` before `Ended`. Also the chunker follow-ups from TODO.md: tentative words dropped in the empty-range close branch (asr/chunker.rs), partial decode cadence vs backpressure, MeetingTime home and Segment.track ids.
  - accept: `cargo test -p earshot-core pipeline`, `cargo test -p earshot-core asr`
- [x] T6.3a Process loopback and device following
  - deps: T1.0a
  - do: in `earshot-capture-wasapi` (building on T1.0): **process loopback** (`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Windows 10 2004+) of a chosen process tree; call-app detection (processes with an active render session among known call apps such as Teams, Zoom, Webex, Slack, Discord, browsers; user-overridable); fallback to loopback of the default render endpoint; default-device change handling for both mic and render (`IMMNotificationClient`, re-open on the new endpoint, discontinuity marked in the timeline); stream error recovery; `earshot record-raw --loopback-process <name|pid|auto>` to exercise it. Device tests are `#[ignore]`.
  - accept: `cargo test -p earshot-capture-wasapi` and `cargo test -p earshot-cli`
- [x] T6.3b WASAPI AudioSource
  - deps: T6.1, T6.3a
  - do: wrap the T6.3a capture as the core `AudioSource` / `CaptureSession` (mic + loopback tracks with device timestamps and discontinuities), event-driven on the capture thread.
  - accept: `cargo test -p earshot-capture-wasapi` (non-device tests: format conversion, timestamp/drift math, silence synthesis)
- [x] T6.3c Stereo loopback reference
  - deps: T6.3b
  - do: keep the loopback (reference) track stereo end to end instead of downmixing: capture, `AudioSource` frames with a channel count, `record-raw` writing a stereo `loopback.wav` and a timestamps.json schema bump (reader updated in the same merge per AGENTS.md); the "Them" ASR track is still a mono mix derived downstream.
  - accept: `cargo test -p earshot-capture-wasapi`, `cargo test -p earshot-core capture`, `cargo test -p earshot-cli`
- [x] GATE-10b Stereo raw recording
  - deps: T6.3c
  - do: like GATE-10, but with the stereo loopback (`earshot record-raw` after T6.3c) on the desktop speakers; feeds the informational real-world ERLE for the stereo canceller (T1.5b/T1.6). Informational only; nothing gates on it.
- [x] T6.3d Follow-ups: WASAPI capture
  - deps: T6.3c
  - do: decided by the owner: clear the advisory review findings in TODO.md. Each fix gets a test that fails without it where behaviour changes; items found to be wrong or not worth changing are closed with a one-line reason in the commit body. Fixed items are removed from TODO.md by the coordinator. Items: clamp synthesized gaps to the recording deadline (timeline.rs), const assert on WAVEFORMATEX size, Drop for CaptureStream stopping the stream, non-tautological start_qpc assertions, optional track truncation at finish, trim packets stamped inside the previous packet, Track::reopen treating a channel-count change as a failed open (wasapi.rs, recording.rs, live.rs; unsafe FFI, always reviewed). The unbounded live channel cap waits for T6.4's pump cadence; the single-track in-room mode belongs to M10.
  - accept: `cargo test -p earshot-capture-wasapi`, `cargo xtask ci`

- [x] T6.3e Capture device test leftovers
  - deps: T6.3d
  - do: from the T6.3d re-review: bound the loopback first-packet lateness in the device test (< 500 ms) and name the chime path when it is missing.
  - accept: `cargo test -p earshot-capture-wasapi`, `cargo test -p earshot-capture-wasapi records_one_second_of_both -- --ignored`

- [x] T6.4a Live session (split from T6.4)
  - deps: T6.2b, T6.3b, T2.4, T3.2, T4.3, T5.4
  - do: `earshot-runtime` live session: Meeting and Track rows before capture, live ASR per track with the Track id, session thread pumping the source and polling the core pipeline, FLAC per track, live finals stored as the Live pass, duration and audio files finalised at the end, UI event stream, runtime policies (pump cadence, two-track DataLost re-sync when mic skew or track offset pass their budgets, device loss reported until restored, TimestampJump and capture errors surfaced, a track dead at the end listed). Never exports, no network.
  - accept: `cargo test -p earshot-runtime live`
- [x] T6.4b Post-meeting processing
  - deps: T6.4a, T3.2, T3.2b, T5.4
  - do: final pass (batch transcriber over the recorded FLAC, replace the Live pass with the Final pass), per-track diarisation with the two-track policy, Speaker rows, voiceprint matching and "Me" enrolment; model work behind factories so tests use fakes. The clustering threshold comes from a per-`EmbeddingModelId` config with no fallback (T3.2b supplies CAM++).
  - accept: `cargo test -p earshot-runtime post`
- [x] T6.4c Summary and on-demand export
  - deps: T6.4b, T4.3, T5.4
  - do: summary through earshot-llm over earshot-net only (configured allowlisted endpoint, audit log), stored as a Summary row; Obsidian export only as explicit commands (one meeting, export all/changed via `export_notes`). Tests use the earshot-llm-replay mock (dev-dependency only) and a temp vault and assert the audit log shows only the mock endpoint.
  - accept: `cargo test -p earshot-runtime summary`, `cargo test -p earshot-runtime export`
- [x] T6.4d Production backends
  - deps: T6.4c, T6.3b, T2.4
  - do: WASAPI live source wiring (cfg(windows)), whisper load with backend auto-selection verified by `earshot_asr_whisper::evidence`, ORT init with Silero, CAM++ and pyannote under one ORT library per process, model paths from DataDir / `EARSHOT_MODELS`; the WASAPI live-channel cap from the T6.3b review once the pump cadence is known. Tests needing models fail with the fetch instruction when missing.
  - accept: `cargo test -p earshot-runtime backends`
- [x] T6.4e Production whisper loader
  - deps: T6.4d, T2.4c
  - do: the production `WhisperLoader` over `earshot_asr_whisper::evidence::load_verified` (CUDA when expected, else CPU), wired into `live_asr_factory` and `post_processor`, with a tiny.en CPU test; the M2 lane's evidence module must be on main first.
  - accept: `cargo test -p earshot-runtime backends`

- [x] T6.3f No DataLost for a stream's first packet
  - deps: T6.3e
  - do: found in T6.5 on a real device: WASAPI flags the first packet after Start with DATA_DISCONTINUITY; it is not data loss. Suppressed for the first delivered packet after the origin or a re-open, in the live emitter and raw replay alike.
  - accept: `cargo test -p earshot-capture-wasapi`, `cargo test -p earshot-core capture`
- [x] T6.4f Reference resampler tail before an idle-track fill
  - deps: T6.4a
  - do: found in T6.5 replay (`reference: 129 late dropped`): the max_track_lag fill silenced a lagging track while its resampler still held samples, which then landed behind the lane end. `Intake::fill_idle_to` ends the open run first; a uniform replay has (restarts, late_dropped) == (0, 0).
  - accept: `cargo test -p earshot-runtime`, `cargo test -p earshot-core pipeline`
- [x] T6.5a CLI replay through the runtime (split from T6.5)
  - deps: T6.4e
  - do: `earshot replay --mic a.wav [--ref b.wav] --out <datadir> [--model] [--no-post] [--llm-url URL --llm-model NAME] [--export-vault DIR]` through the runtime with the production backends: prints backend per track (with fallbacks), AEC/clock stats, warnings and live finals, then the post pass and the backend that ran; summary only with --llm-url (earshot-net, single-endpoint allowlist, audit destinations printed); export only with --export-vault. The shipped binary carries no LLM mock (the ci `net` guard forbids test-support crates even behind a feature): tests start the earshot-llm-replay mock as a dev-dependency and pass its URL; the mock-from-a-directory replay lives in the xtask harness (T6.6).
  - accept: `cargo test -p earshot-cli`
- [x] T6.5b CLI live record and devices
  - deps: T6.5a
  - do: `earshot devices` (default mics, render loopback, mix formats, active render sessions) and `earshot record --seconds N --out <datadir>` via `start_wasapi_session` and the same driver, stopping on --seconds or Ctrl+C (a console control handler in earshot-capture-wasapi's wasapi.rs, the only Windows FFI crate) with every FLAC finalised. Device tests are #[ignore] under GATE-01.
  - accept: `cargo test -p earshot-cli`
- [x] T6.6c AEC review clean-ups
  - deps: T6.6a
  - do: from the stereo merge review: +inf cap only for SI-SDR metrics, non-empty T15E_SEEDS, no per-block allocation in the coherence statistic, hysteretic stereo switching, dead code.
  - accept: `cargo test -p earshot-core aec`, `cargo xtask accept m1`
- [x] T6.4g Echo de-duplication in the runtime
  - deps: T2.4e, T6.4b, T6.5b
  - do: apply the T2.4e de-duplication to live finals (before they are shown or stored as "Me") and to the final pass in the post processor, using the session's AEC delay; report how many words were removed per meeting. Tests through the runtime with a simulated leaky echo path: no far-end words stored on the mic track, near-end words kept.
  - accept: `cargo test -p earshot-runtime`
- [x] T1.5f Mono double-talk false positives in near-end pauses
  - deps: T6.6c
  - do: found in T6.6d: the mono linear-witness double-talk detector fires in near-end pauses during far-end speech and leaks echo bursts of -33 to -50 dB (two events on double-talk-ser-0 since T1.5e moved the first lock 120 ms later). A coherence AND-gate removed them but clipped near-end words; a fix needs a design that keeps near-end SI-SDR and does not regress GATE-10 far-end-only ERLE. Also look at the old -35 dB leak at 9.34-9.47 s where the postfilter gain is at its floor while the linear error is large. Not blocking: the transcript echo cleaning (T2.4e, T6.4g) keeps such leaks out of "Me".
  - accept: `cargo test -p earshot-core aec`, `cargo xtask accept m1`
- [x] T6.6 M6 acceptance
  - deps: T6.5b, T6.4g
  - do: `accept m6`: generate a two-track meeting with the simulator from AMI/LibriSpeech speech, replay through the runtime with whisper tiny.en/base.en and the LLM replay mock, then check e2e.replay (transcript present, "Me"/remote speakers separated, summary valid, export valid, audio files written, and the `earshot-net` audit log shows requests only to the mock endpoint).
  - accept: `cargo xtask accept m6`
- [x] T1.5d Double-talk detector independent of the linear echo estimate
  - deps: T1.6
  - do: decided by the owner: stereo double talk at SER 0 fails (as low as 2.7 dB) because the current detector uses the linear stage's echo estimate as its witness, and the stereo linear stage plateaus below the bar that enables it. Design a detector that does not depend on that estimate (for example a far-end/mic coherence or normalised cross-correlation detector, Benesty/Morgan/Cho 2000 or Gansler et al.), with the source cited. Every mono M1 threshold must still pass; the 10 dB target is unchanged; the second-seed sweep stays informational. Builds on the wip/T6.6a work.
  - accept: `cargo test -p earshot-core aec`, `cargo xtask accept m1`
- [x] T1.5e No echo lock on uncorrelated speech
  - deps: T1.5d
  - do: found in T6.5 replay: two unrelated clips (no echo path) briefly locked (0.3 ms) and the postfilter took 7.9 dB off the near-end voice, violating "never suppress without live evidence of an echo path". Fix at the root (e.g. the coherence statistic as a witness for a lock) with a regression test over several uncorrelated pairs: no suppression, output/input power within ~0.5 dB.
  - accept: `cargo test -p earshot-core aec`, `cargo xtask accept m1`
- [x] T6.6a Stereo echo cancellation acceptance
  - deps: T1.6, T1.5d, T1.5e
  - do: decided by the owner: M1 gates on a mono reference, but the desktop product path is stereo loopback, so stereo must be proven before hardware testing. Add gating stereo scenarios to `acceptance/aec_scenarios.toml` (the sim's `stereo` block: two loudspeakers, independent channels, an image change; at least far-only and double talk at SER 0 and SER -10) and score them in `aec_eval` through `EchoCanceller::process_stereo*` with `reference_lr`, under the same thresholds as mono, including the robustness grid. Also from the M1 review: sweep the event scenarios (path change, device switches) over the robustness grid as `aec.robustness` claims; map a `+inf` SI-SDR on a `>=` metric to a pass with the cap recorded instead of a non-finite failure; an informational second-seed sweep of the gating scenarios to show tuning has not overfit the fixed seeds. If stereo double talk at SER 0 (8.9 dB at the M1 merge) cannot reach 10 dB without loosening, stop and ask.
  - accept: `cargo test -p earshot-core aec`, `cargo xtask accept m1`

- [x] T6.6b Follow-ups: echo canceller and simulator
  - deps: T6.6a
  - do: decided by the owner: clear the advisory review findings in TODO.md. Each fix gets a test that fails without it where behaviour changes; items found to be wrong or not worth changing are closed with a one-line reason in the commit body. Fixed items are removed from TODO.md by the coordinator. Items: AEC config `validate()` instead of constructor asserts, absurd finite input bound on `sx` (aec), far-only false double-talk margin, `time_to_erle` hold median excluding the crossing window (only if it does not loosen the metric; otherwise close it), sim echo-level normalisation per delay segment, recovery_window_violations in echo time, blanking vs recovery window check (sim).
  - accept: `cargo test -p earshot-core aec`, `cargo test -p earshot-core sim`, `cargo xtask accept m1`, `cargo xtask ci`

- [x] T6.16 No repetition loops in the final pass
  - deps: T6.6
  - do: found at a hardware gate: the final pass (whisper batch, beam 5, whole track) looped one sentence on the mic track to the end, replacing minutes of the user's speech; whisper.cpp's temperature fallback (entropy and log-probability thresholds) did not catch it. Decode the final pass over VAD speech regions in bounded pieces, without carrying previous text into a piece that follows a long pause or a failed piece, and detect repetition (openai/whisper's compression-ratio rule, cited) to retry or drop the looping piece; never store a run of repeated text the audio does not support. Regression test on a private recording (`#[ignore]` with the exact command) plus a committed synthetic case (long quiet echo-residual stretch between speech).
  - accept: `cargo test -p earshot-core asr_pieces`, `cargo test -p earshot-runtime`, `cargo test -p earshot-cli --test final_pass_pieces`, the ignored private-recording test with its command (`cargo test --release -p earshot-cli --features cuda --target-dir target/cuda --test final_pass_pieces -- --include-ignored`: no repeated sentence in the stored mic transcript), `cargo xtask accept m6` (the end-to-end replay through the final pass; `accept m2` never runs the final pass, so it was replaced here)
- [x] T6.23 Echo leaks the final pass misses in talk-over
  - deps: T6.16
  - do: found at a hardware gate: far-end words under "Me" above the 1% target. (1) A phrase present in the far-end transcript survived the text rule, glued to the end of a "Me" line in a talk-over segment: check final-pass word times in piece-decoded talk-over segments against the rule's delay window and fix the cause (not by widening the tolerance without a dev measurement). (2) Echo residual misheard as unrelated words cannot be matched by text: measure why the speaker check kept them (span length, margin) on private recordings as dev data and the simulator, and fix without losing near-end words (`ECHO_NEAR_RECALL_FLOOR_PERCENT` holds). Talk-over is a robustness case, not the main use case (the owner).
  - accept: `cargo test -p earshot-runtime post_echo`, `cargo test -p earshot-core asr`, `cargo xtask accept m2` (asr.echo_leak and near-end floor)
- [x] T6.23a "Me" survives a stray voice on the mic track
  - deps: T6.23
  - do: found at a hardware gate: the final pass found two voices on the mic track, the user and a cluster holding one word, so `two_track`'s single-mic-voice rule did not apply and the user's stored transcript is "Speaker 2" (the live draft had "Me"). Decide by a rule written before measuring (for example: a mic cluster below a speech floor, or one the speaker check attributes to the far end, does not count as a mic voice; never by amount of speech for naming Me, AGENTS rule) and test it on synthetic two-track meetings with a tiny spurious mic cluster, keeping every existing two-track and in-person test.
  - accept: `cargo test -p earshot-runtime post`; `cargo test -p earshot-core diar`
- [x] T6.23b Near-end words lost in talk-over
  - deps: T6.23
  - do: found at a hardware gate: clear near-end words are missing from every transcript label during talk-over. Find where they are lost, counts only: transcribe the raw mic and the canceller's output over that window with the production model and compare script-word recall; then canceller (suppression in double talk, the mono-reference postfilter) versus transcription (piece cut, VAD, recover-speech). Fix the cause with a rule chosen on dev data (AGENTS AEC tuning rules; never on a private recording alone), and check the held-out AEC baseline.
  - accept: the diagnosis with numbers in LOG.md; the fix's own tests; `cargo xtask accept m1` and `accept m2` if the canceller changes
- [x] T6.23c Delay-jump recovery under the 30 Hz high-pass
  - deps: T6.23b
  - do: found when T6.23b made the 30 Hz audio high-pass (S30) the default (the owner accepted the trade): on the held-out set S30 slowed recovery after a delay jump (`aec.path_change`, delay-jump, worst seed) from 2.5 s to 4.7 s and lowered double-talk SER 0 SI-SDR from 9.95 to 9.25 dB; production was already over the 2 s path-change threshold on held-out (2.5 s), and the dev-wide sweep shows delay-jump misses of 2.5-3.5 s. Find why the filter slows re-convergence after a jump (filter group delay in the delay estimator, the faster convergence declaration, or the epoch reset) on dev data only, fix by a rule written before measuring, and bring held-out delay-jump back to at most 2.5 s without losing the S30 gains (rumble dev set).
  - accept: `cargo test -p earshot-core aec`; `cargo xtask accept m1` (held-out baseline no value worse than the S30 baseline, delay-jump path change logged); with the S30 default, `double-talk-ser-0` worst near-end-pause leak <= -6 dB (`MAX_NEAR_PAUSE_LEAK_DB`; S30 measured -5.9 dB) and postfilter SI-SDR in double talk >= the linear stage's (S30 measured 12.65 vs 13.17 dB), the two T1.5 invariants that today guard only the unfiltered canceller; `accept m2` `asr.wer_aec_delta` brought back towards -1 point (production before S30: -0.97; with S30: 2.90 against the 3.0 limit; the owner)
  - closed with the delay-jump fix only: held-out delay-jump path change 4.7 -> 1.7 s, path-change 3.5 -> 2.9 s, no held-out value worse. Three postfilter rounds showed every change that passes the two T1.5 invariants costs stereo far-only ERLE or recognised near-end words in double talk, so these stay a measured trade, not done: the invariants stay asserted on `HighPass::Off`, held-out double-talk SER 0 SI-SDR 9.25 dB (9.75 dB against the high-passed near end, informational), `asr.wer_aec_delta` 2.90. The owner: echo under "Me" matters only where it changes the summary; re-open from real meetings (T6.23d).
- [x] T6.23d Echo canceller on real meetings: room, mic and device (closed; canceller issues return as bugs from real meetings, judged by their effect on the summary)
  - deps: T6.23c
  - do: The owner, parked until a few real meetings exist: compare the canceller with a desk mic, with low-frequency room noise on and off, and on a laptop's built-in mics with Windows' own voice processing noted on or off; counts only (far-end words under "Me", recognised near-end words, low-frequency non-echo level as `xtask::aec_rumble` measures it). Decide from those whether the T6.23c leftovers (the two T1.5 invariants on the default, double-talk SI-SDR, `asr.wer_aec_delta`) are worth another round. Judge by effect on the summary, not on the transcript alone.
  - accept: per-setup counts in LOG; decision with the owner on whether to tune further
- [x] GATE-12 Real echo-leak recording
  - deps: T6.3c, T6.16, T6.23, T6.23a, T6.23b
  - do: requested by the owner: a real call on the desktop speakers with a known near-end script replayed through the runtime after T6.4g and scored for far-end words under "Me" and near-end words lost.
- [x] T6.17 Drift steering on real devices and locks at the search limit
  - deps: T6.6
  - do: found in a live `earshot record`: the reference drift steering alternated in sign between Stats snapshots although mic and speakers share one device clock, attenuation was often low, and the canceller reported a lock at 600 ms (`DelayConfig::max_delay`) while the far end was paused. Reproduce from a raw recording with real device timestamps (`record-raw` replays through `ReplayBuilder::from_raw_timeline`), find whether the steering loop oscillates (gain, measurement window, the restarts when the loopback resumes after silence), fix with a test that a constant-offset pair settles within a stated ppm, and refuse a lock at the edge of the search range or with a silent reference.
  - accept: `cargo test -p earshot-core pipeline`, `cargo test -p earshot-core aec`, `cargo xtask accept m1`; steering and lock trace over a raw recording logged
- [x] T6.18 CLI says when the final pass runs and what Ctrl+C cancels
  - deps: T6.6
  - do: found at a hardware gate: `earshot record` printed nothing for ~1.5 min after the session ended while the final pass ran, so the owner pressed Ctrl+C and the final pass was lost. Print a line when the final pass starts (with what it does and a rough duration) and when each stage finishes; a Ctrl+C during it says that it cancels the cleanup only (recordings and the live transcript are kept), and a second one ends at once.
  - accept: `cargo test -p earshot-cli`
- [x] T6.7 No Whisper non-speech markers in transcripts
  - deps: T6.6
  - do: found at a hardware gate: the final pass stored whisper's `[BLANK_AUDIO]` as "Me" segments. Drop whisper's bracketed non-speech annotations (`[BLANK_AUDIO]`, `[MUSIC]`, `(silence)` and the like, matched by whisper's own rules, cited) in live and final passes before segments are stored or shown, so they never reach the transcript, summary input or export; a segment left with no words is not stored. Test with a fake transcriber emitting the markers through the runtime.
  - accept: `cargo test -p earshot-runtime`, `cargo test -p earshot-core asr`
- [x] T6.8 Cleanup during the meeting: measure and design
  - deps: T6.6
  - do: requirements from the owner: the transcript is live; the summary arrives within about a minute of the end of the meeting, from the best transcript available then; the cleanup (final pass) runs incrementally during the meeting, a few minutes behind the live transcript, on stretches safely in the past, so that on the desktop the cleaned transcript lands within about a minute of the end of a 60-minute meeting (only the tail and the meeting-wide speaker clustering and naming are left at the end). A cleanup more than 30 minutes after the meeting is worthless: there is no deferred queue. The live transcript always has priority; cleanup pauses when the live backlog grows. On weaker devices cleanup is best effort in order of value: speakers first (embeddings, clustering), re-transcription only with spare capacity, otherwise the live words stand. A replay showed the final pass taking far too long for an hour's meeting. First measure: a per-stage breakdown (FLAC decode, whisper batch, VAD, segmentation, embeddings, clustering, voiceprints, store) for 6 and 60 minutes on CUDA and CPU, and whether live ASR keeps up in real time on the CPU (a replay paced at real time), written to a `docs/` note. Then propose the incremental design to the owner (stretch boundaries, context carried between stretches, priority and pausing, what is final at the end, effect on quality versus the whole-meeting pass, whether the cleanup model can be larger than live's) before building it.
  - accept: the measurement note and the owner's design decision recorded in LOG; the implementation becomes its own task(s)
- [x] T6.9 Far-end speaker over-splitting on real audio
  - deps: GATE-12
  - do: found at a hardware gate: far-end voices over-split on the loopback track of a private recording. Diagnose against the calibrated CAM++ threshold (dev data only, never tuned on a private recording) and the segment boundaries.
  - accept: diagnosis in LOG; fix task(s) added with their own accept
- [x] T6.9a Speaker changes on word boundaries
  - deps: T6.9
  - do: found by T6.9: on real recordings many far-end speaker changes fall inside a sentence with a short pause, and some words touch two voices. Measure word-level attribution on dev two-speaker scenes with known truth, plus AMI dev-series annotations if they hold the words. Write the rule before measuring (for example, a change needs a sentence end or a pause unless the new run holds at least N words). Implement it in `diar::assign` only if it beats no rule; never from a private recording.
  - accept: `cargo test -p earshot-core diar` with new cases for flips and mid-sentence cuts; the dev report committed; `cargo xtask accept m3` and `cargo xtask accept m6` unchanged or better
- [x] T6.9b Long scenes in the clustering calibration
  - deps: T6.9
  - status (2026-11-07): the long scenes and rule are merged, and the owner held the constant at 0.68. The rule picks 0.83 on clean LibriSpeech scenes (about 20 independent voices, not 40), but at 0.83 two of five AMI dev meetings collapse to one speaker and wrong-speaker words go from 13.16% to 32.53%. The ignored test pins both values.
  - do: found by T6.9: T3.2b used 15 s per speaker and no one-speaker scene, and 360 s dev scenes show a small over-count at 0.68. Add one-speaker and dominant-speaker long scenes to `diar_calibration` with the rule written first, using dev-clean speakers 21-40 (T6.9's tool saw the first 20). Change `CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD` only if the rule picks another value, never from a private recording.
  - accept: `cargo test --release -p earshot-onnx diar_calibration -- --include-ignored`; `cargo xtask accept m3`
- [x] T6.10a Live ASR on per-track worker threads
  - deps: T6.8
  - do: found in T6.8 (private notes): the session thread pumped one capture packet, then ran live ASR inside `Pipeline::poll`, so a slow decode stalled capture; past `LIVE_QUEUE_CAP` (30 s) WASAPI frames were dropped from the recording, and coalescing never triggered because each poll saw 10 ms of new audio. Decided by the owner: threads. The runtime runs each track's live ASR on its own worker thread (core `pipeline::LiveTrack`, the stage the pipeline used internally, made public), fed from the pipeline's `Audio` events through a channel; the session thread only captures, cancels echo, writes FLAC and handles events. Workers drain everything queued before each decode (so `PartialPolicy::Coalesce` works), keep `max_asr_backlog` (a track that falls 10 min behind stops its live ASR, recording continues), and are finished before the pipeline's final-pass request and `Ended` are handled. Core still spawns no thread; `Pipeline::attach_live_asr` stays for single-threaded hosts and tests. Main value: the recording never waits on transcription (talk-over is rare, so parallel tracks are a bonus; the phone has one track).
  - accept: `cargo test -p earshot-runtime` (a slow fake transcriber: every captured frame reaches the FLAC and the session keeps pumping during a decode), `cargo test -p earshot-core pipeline`, `cargo xtask accept m6`
- [x] T6.10b Live ASR backpressure, thread budget and capture-lag check
  - deps: T6.10a
  - do: from T6.8 and T6.10a: lower `PartialPolicy` to `Off` while a track's live backlog stays above a few seconds and restore it when it clears; size whisper's CPU threads per live worker to the host (two workers share the cores) instead of `min(4, cores)`; `earshot replay --realtime` reports the largest capture lag behind the wall clock and whether WASAPI's 30 s queue would have dropped audio. Also decide (from the T6.10a review) what a stop does with a large live backlog: today `LiveWorker::finish` decodes all of it (up to `max_asr_backlog`, 10 min) before `Ended`; bound it or cancel the rest (the final pass covers the transcript).
  - accept: `cargo test -p earshot-runtime`; `earshot replay --realtime --no-post` on the desktop CPU over a private recording: no capture lag beyond the queue, backlog and partial policy logged
- [x] T6.10e1 Parakeet backend: 110M CTC
  - deps: T6.10b
  - do: first part of the T6.10e evaluation (split along its stated build order). A Parakeet backend behind `asr::Transcriber` in `earshot-onnx`: a NeMo-exact log-mel front end (`AudioToMelSpectrogramPreprocessor` as configured in the models: 16 kHz, 25/10 ms, Hann 400 in a 512 FFT, preemphasis 0.97, zero-padded centre, power, Slaney mel, ln(x + 2^-24), per-feature normalisation with N-1, frames past the length zeroed; NeMo `features.py`, cross-checked against onnx-asr's NumPy port), checked against onnx-asr's `nemo128.onnx`; the NVIDIA parakeet-tdt_ctc-110m CTC branch as exported by sherpa-onnx (fp32, HF mirror pinned by commit and LFS SHA-256); vocabulary parsing, greedy CTC decoding and word timings in core (`asr::ctc`, 80 ms frames). Models in a new manifest set `eval` (not shipped; licence CC-BY-4.0, credit NVIDIA NeMo and Suno.ai). English only; no prompt; greedy only (reported).
  - accept: `cargo test -p earshot-core asr_ctc`; `cargo test -p earshot-onnx --lib parakeet`; `cargo test --release -p earshot-onnx --test parakeet -- --include-ignored` (needs `cargo xtask fetch-models --set eval`: front end matches `nemo128.onnx`; the smoke clips transcribe with WER at most 8%); `cargo test -p xtask models`
- [x] T6.10e2 Parakeet TDT decoding
  - deps: T6.10e1
  - do: second part of T6.10e: NeMo's greedy TDT loop (`GreedyTDTInfer`, durations [0..4], max symbols per frame 10) on the same backend, unlocking the 110M TDT branch and the 0.6B v2 (English) and v3 (multilingual) exports; pin the files in the `eval` set.
  - accept: `cargo test -p earshot-core asr_tdt`; `cargo test -p earshot-onnx parakeet`
- [x] T6.10e Transcription model evaluation
  - deps: T6.10e2
  - do: requested by the owner: offer the best model for the best devices with fallbacks for weaker ones, ordered by measurement, not reputation; Parakeet is built in properly as a user choice, not only measured (split out as T6.10e3, 2026-10-23). Candidates: NVIDIA Parakeet TDT-CTC 110M (English, a base/small-class size for weaker machines), Parakeet TDT 0.6B v2 (English) and v3 (about 25 European languages, auto language) through ONNX Runtime (licence CC-BY-4.0: check the exact terms and attribution before shipping; stop and ask if unclear), whisper large-v3-turbo (and its q5_0), small.en, base.en, tiny.en. Build order: (1) a Parakeet backend behind `asr::Transcriber` on the 110M model with its CTC head (audio features, ONNX encoder from the manifest, greedy CTC decoding, SentencePiece tokenizer, word timings from frames; reference implementation cited); (2) the TDT greedy decoding loop on the same backend, unlocking 0.6B v2 and v3; (3) the evaluation. If the task grows past about 800 changed lines, split it as T6.10e1/e2/e3 along that order. Measured on AMI (the m2 reference rules), the GATE-12 recordings (script words kept, far-end words under "Me"), long silence and echo residue (invented text, loops), word-timestamp quality for the echo and speaker rules, and speed per backend (CUDA, Vulkan, CPU here; QNN on the Surface at GATE-03) with the T6.10b thread budget; v2 against v3 in English. Result: a per-backend ladder (best to fallback) and language coverage per model, written to a `docs/` note and `models/manifest.toml`.
  - accept: the evaluation runs from `cargo xtask` (an extended `asr_eval` or a new runner) and writes its report; the ladder and the docs note committed
- [x] T6.10e3 Parakeet as a model choice
  - deps: T6.10e2
  - do: split out of T6.10e, after the desktop GPU evaluation put Parakeet TDT 0.6B first (LibriSpeech 1.75%, AMI 18.7% against large-v3-turbo 1.77% / 20.5%, word starts within 250 ms 97% against 41%, no invented text on silence or noise, 2-5x faster). The runtime loads Parakeet models (CTC 110M, TDT 110M, 0.6B v2, 0.6B v3) by name from the models directory for the live and final passes, alongside whisper: one model-name list (whisper and Parakeet), a loader that proves the backend from ONNX Runtime's node placement (a requested GPU with the encoder not on it is an error, never a CPU fallback) and records it like whisper's pass backend; `--model` in `earshot record`, `replay` and `transcribe` accepts the Parakeet names; English-only models refuse other languages; Parakeet ignores prompts and beam settings (reported). The models stay in the manifest `eval` set: shipping waits on a conversion script from NVIDIA's .nemo files and the owner's hosting decision.
  - accept: `cargo test -p earshot-runtime backends`; `cargo test -p earshot-cli`; `earshot replay` of a private recording with `--model parakeet-tdt-0.6b-v2` on CUDA and on the CPU, backend printed from the placement (logged)
- [x] T6.10c1 Model check rules
  - deps: T6.10b, T6.10e, T6.10e3
  - do: first part of T6.10c (split 2026-10-23, over the size guideline): the pure rules in `earshot-runtime` (`model_check`): the T6.10e ladders and language coverage as data pinned to `models/manifest.toml` by a test; the language filter (English ladder for English-only meetings, else the multilingual ladder's models covering every meeting language); the starting rung from hardware facts (GPU and free video memory, NPU, CPU threads, total memory); the verdict on a measured two-track run (speed and WER limits fixed before measuring); the search (comfortably fast and correct -> one rung up; too slow or wrong -> one rung down, then the next backend; stop when clear); the recommendation (best passing model, the next one down as fallback, better models not installed listed with their size).
  - accept: `cargo test -p earshot-runtime model_check`
- [x] T6.10c2 First-run model check: the test run, storage and `earshot benchmark`
  - deps: T6.10c1
  - do: second part of T6.10c: the hardware read, the bundled two-voice test clip, the two-track timed run with the T6.10b thread budget, device-local storage and re-run triggers, `earshot benchmark`, and `earshot record` using the choice when no `--model` is given. The full requirement as agreed with the owner: requested by the owner: Earshot must work on any laptop and use the best model the machine can run. (1) The user's meeting languages (first-run wizard and settings, default English, several allowed; a meeting can override) filter the candidates to models that cover them (T6.10e coverage). (2) A hardware read with no network picks the starting rung of the T6.10e ladder: dedicated GPU and free video memory (CUDA/Vulkan device info), NPU (QNN initialises), integrated Vulkan graphics, CPU threads, total memory. (3) A bundled clip of about 60 s with a known transcript (two voices taking turns, one with simulated echo, built from the committed LibriSpeech smoke clips, licence kept) runs the starting rung as a two-track load with the T6.10b thread budget: comfortably fast and correct (WER on the clip, which also catches a GPU or NPU that runs but outputs garbage) -> try one rung up; too slow or wrong -> step down a rung or to the next backend. The hardware guess only chooses where to start; the test decides. Stop as soon as the answer is clear. (4) Recommend the best passing model with the next one down as the automatic fallback when a meeting falls behind; a model not installed yet is offered for download with its size first (manifest, checksum); declined -> test the best installed one. Stored device-locally, shown in settings as recommended with a manual override, re-run when hardware, drivers, backends or installed models change; `earshot benchmark` prints the table and `earshot record` uses the choice when no `--model` is given. For languages other than English the clip checks speed and correctness of the backend only; per-language quality comes from T6.10e or published figures and is labelled as such.
  - accept: `cargo test -p earshot-runtime model_check`; `earshot benchmark` on the desktop (expected start: the top GPU rung, settled in one test) and a CPU-only run logged
- [x] T6.10d Vulkan in the automatic backend choice
  - deps: T6.10c2
  - do: (low priority for their devices since Parakeet leads the ladder and runs on ONNX Runtime, but finish, review and merge it for a broader release.) requested by the owner: integrated Intel and AMD GPUs (and other Vulkan devices) speed up whisper with nothing to install. Wire the whisper.cpp `vulkan` build (the Vulkan half of T2.6; the Vulkan SDK is a build-host install for the owner), backend evidence from whisper.cpp's log as for CUDA, and the automatic order CUDA > Vulkan > CPU on x86-64 (NPU > Vulkan > CPU on the Surface; D20: Vulkan is a standard path, not an afterthought; this task also covers the Vulkan halves of T2.6 and T8.2), where the T6.10c speed check keeps Vulkan only if it beats the CPU on that machine. WER within 1.0 point of the CPU (T8.2's rule) where a Vulkan device exists.
  - accept: `cargo xtask build --features vulkan`; `cargo test -p earshot-runtime backends`; on the desktop, `earshot benchmark` lists Vulkan and CPU with the backend from the whisper.cpp log; the WER parity run logged
- [x] T6.11 ONNX models on the CPU by default
  - deps: T6.8
  - do: found in T6.8: a `cuda` build puts Silero, pyannote segmentation and CAM++ on CUDA, which needs cuDNN to start at all and runs CAM++ 4.5x slower than the CPU (113 vs 25 ms per span; cuDNN `HEURISTIC` search did not help). PLAN T9.1 already says ONNX defaults to the CPU: make the CPU the default ONNX accelerator in `cuda` builds (whisper keeps CUDA), with CUDA ONNX only on explicit request. Investigate the CAM++ CUDA slowness (node placement log, per-shape setup) and record the cause in the docs note before CUDA ONNX is ever offered.
  - do (added 2026-10-22): cap ONNX Runtime's CPU threads under the same rule as whisper (T6.10b: leave the machine room for the call app); today ORT uses every core (the owner saw 65% CPU during a 6-minute post_bench run, mostly CAM++ and segmentation).
  - accept: `cargo test -p earshot-runtime backends`; a `cuda` replay starts and runs with no cuDNN on PATH; a test that the ONNX sessions get the capped thread count
- [x] T6.12 Whisper and speaker work in parallel in the final pass
  - deps: T6.8
  - do: from T6.8: in `PostProcessor::run`, run whisper batch decoding (GPU on the desktop) and per-track diarisation (CPU) at the same time, and the two tracks' diarisation side by side where cores allow; the echo check and labelling stay after both. Expected on the desktop: 60 min 425 s -> ~215 s. The result is identical to the sequential run.
  - accept: `cargo test -p earshot-runtime post` (same outcome as sequential with fakes); `post_bench` timings logged
- [x] T6.13a Incremental speech pieces
  - deps: T6.10b, T6.11, T6.12
  - do: first part of T6.13 (split 2026-10-23, agreed by the owner): a streaming version of `asr::transcribe_in_pieces` in core: fed audio as it arrives, it keeps the VAD probabilities, closes a piece once the silence after it (or the 30 s limit, cut at the quietest frame as `plan_pieces` does) is known, and transcribes each closed piece once with the same prompt rule (`context_gap`), retry ladder and compression-ratio guard; pieces dropped as loops are reported. Fed the same audio, its output equals `transcribe_in_pieces` (same pieces, same calls, same segments).
  - accept: `cargo test -p earshot-core asr_stretch` (equivalence with `transcribe_in_pieces` on the pieces test cases, in pushes of any size)
- [x] T6.13b Speech stretches on the live workers
  - deps: T6.13a
  - do: second part of T6.13: the per-track workers run the incremental pieces by default (the live chunker stays behind a setting); each stretch is stored as the live pass and shown as it lands, with the live echo text rule as now; decoding during the meeting is greedy (Parakeet has no beam; for whisper beam gained about 1 AMI point at twice the cost, T6.10e); at hang-up a worker that is behind finishes its queue within the 30-minute rule instead of being cut, since the cleanup no longer re-transcribes.
  - accept: `cargo test -p earshot-runtime live` (stretches appear during the session; the stop rule finishes the queue)
- [x] T6.13c Cleanup reuses the stretches
  - deps: T6.13b
  - do: third part of T6.13: the final pass takes the stored stretches instead of re-transcribing, transcribes only audio no stretch covers, then diarises, cleans echo, labels and stores the final pass as before.
  - accept: `cargo test -p earshot-runtime post` (same final transcript as the whole-meeting pass on fakes); `cargo test -p earshot-cli`
- [x] T6.13d1 Incremental diarisation in core
  - deps: T6.13c
  - do: first part of T6.13d (split 2026-10-23, core alone was about 1,300 lines): core `Diarizer` gains an incremental half (`DiarStream`, `Diarizer::advance`: segmentation, binarisation and embeddings of whole `batch_size` batches of windows on the meeting's global grid once their audio is known) and a final half (`Diarizer::diarize_from`: the remaining windows, clustering and reconstruction as today); `diarize` is `diarize_from` over empty results. Fed the same audio, stopped anywhere and resumed (also on a fresh diarizer through the versioned `WindowResults` bytes), the output equals the whole-track pass.
  - accept: `cargo test -p earshot-core diar`; `cargo test --release -p earshot-onnx diar_calibration -- --include-ignored` reproduces the committed constants
- [x] T6.13d2 Speaker work on the live workers, reused by the final pass
  - deps: T6.13d1
  - do: second part of T6.13d: the per-track worker runs `Diarizer::advance` after its decodes only when its decode queue is empty (live transcription has priority; when behind, speaker work stops and the stored prefix stays valid); segmentation and CAM++ on the CPU with `onnx_cpu_threads` (T6.11). At session end a versioned record (local_state `live-speakers/<meeting>/<track>`, version and header outside the payload: segmentation model with file SHA-256, embedding model id, `WindowParams`, why it stopped) is written; an unparseable or other-version record falls back to the whole pass with a warning, never an error. The final pass calls `diarize_from` when the params match and reports the reason otherwise (`PostOutcome`). Make reuse bit-exact against the FLAC the final pass reads (feed the stream 16-bit-quantised samples) and check ONNX outputs across thread counts.
  - accept: `cargo test -p earshot-runtime post` (same speakers and segments as the whole pass on fakes through FLAC; reuse and every fallback reason; a mutation check); `cargo test -p earshot-runtime live`; `cargo test -p earshot-cli`; `cargo xtask accept m6`
- [x] T6.13d3 Live speaker work on its own below-normal thread
  - deps: T6.13d2
  - do: from the T6.13d2 reviews (added 2026-10-24): live speaker batches ran on the transcription worker behind a 2 s backlog gate, so a busy track or the Surface (about 3.8 s per batch at 3 threads) stopped speaker work every meeting. Move speaker work onto one session speaker thread at below-normal OS priority with background ONNX sessions, so transcription never waits for it; keep bit-exact reuse, the transient records and the 180 s bound; cancel it at the end and wait at most `speaker_stop_wait` (2 s).
  - accept: `cargo test -p earshot-runtime` (transcription never waits for a blocked speaker batch; the speaker-window tests pass under full CPU saturation); `cargo test -p earshot-onnx --test diar` (background sessions bit-equal); `cargo xtask accept m6`
- [x] T6.13d4 Speaker detection timing setting
  - deps: T6.13d3
  - do: decided by the owner, replacing an earlier "off on battery" rule: a device-local setting with three values. "During the call" (default): live speaker work as T6.13d3. "After the call": no live speaker work; the end-of-meeting step does it (warning text: the final transcript and summary take longer after hang-up, but no cores are used for it during the meeting). "After the call on battery" (laptops only; ignored on a desktop with no battery): during the call on mains power, after it on battery; power status from the OS (`GetSystemPowerStatus` in the Windows adapter crate, minimal unsafe with a SAFETY comment), read at session start and on change; unplugging mid-meeting stops speaker work with the valid prefix kept. The reason is recorded and printed on the cleanup line. CLI flag on `record`/`replay` and a stored preference now; the settings screen (T7.4) shows the three options with the warning.
  - accept: `cargo test -p earshot-runtime` (each mode; on battery no speaker thread and the reason recorded; unplug mid-meeting stops it with the prefix kept; desktop ignores the battery mode); `cargo test -p earshot-capture-wasapi`; `cargo test -p earshot-cli`
- [x] T6.13e Stretches decode the audio the FLAC stores
  - deps: T6.13d3
  - do: found by the T6.13 harness (2026-10-24): on real audio the reused stretches differ from the whole-meeting pass (AMI IS1009a far end: one ~40-word turn garbled, 34.03 vs 29.74% WER), because the live stretch worker decodes f32 samples while the whole pass reads the 16-bit FLAC. Quantise the stretch worker's input exactly as the FLAC writer stores it (one helper shared with `live::speaker_thread`), keep the bit-exact reuse tests, and re-run `cargo xtask t613 quality` (target: 0 word differences between reuse and the whole pass on every input).
  - accept: `cargo test -p earshot-runtime`; `cargo xtask t613 quality --inputs librispeech,ami,gate12 --minutes 5` (word differences 0, logged); `cargo xtask accept m6`
- [x] T6.13 Transcription during the meeting (single pass)
  - deps: T6.13d4, T6.13e
  - do: acceptance and default for the single pass (split into T6.13a-d 2026-10-23). The full requirement: D20, replacing the earlier "live draft plus cleanup" design in private notes: the during-meeting pass is the default and only transcription. On the per-track workers (T6.10a), each closed speech stretch (cut at a pause, at most 30 s, VAD pieces as in T6.16, previous text as prompt across short pauses) is transcribed once with the good model and stored and shown as it lands; speaker segmentation and embeddings per stretch; echo cleaning per stretch with the lock stretches. The session reports listening status (meters, voice activity, words so far). At hang-up only the last stretch, meeting-wide clustering and voiceprint naming, the store write and the summary remain. A worker that falls behind keeps its queue (bounded by the 30-minute rule; nothing is deferred later than that) and the T6.10b stop rule applies; weak devices use the T6.10c model choice. The live chunker path stays available behind a setting, off by default. Quality checked against the whole-meeting pass on fixtures (AMI and the GATE-12 recordings) before it becomes the default; the whole-meeting pass stays as the fallback.
  - accept: `cargo test -p earshot-runtime` (stretch results match the whole-meeting pass on fakes; stretches appear during the session); a 60-minute `--realtime` replay on the desktop shows each stretch within about 60 s of its end and the final transcript and speakers stored within 60 s of the end of the meeting (logged); `cargo xtask accept m6`
- [x] T6.14 Whole speaker output as the default far end
  - deps: T6.3a
  - do: found in a Teams call: `--loopback-process auto` picked process loopback of `ms-teams.exe`, which delivered almost no audio, so the canceller never locked and every word from the speakers was transcribed as "Me". Decided by the owner: the whole default render device is the default far end for `record` and `record-raw` (and the reliable fallback); per-app capture stays available on explicit request (`auto`, a name or a pid) until T6.15 proves it.
  - accept: `cargo test -p earshot-cli`
- [x] T6.14a The app records the whole output and warns on a silent far end
  - deps: T6.14
  - do: found in Teams calls: the desktop app still asked for `auto`, against T6.14 and T6.15, so its reference track was all zeros: no echo lock, every word on the mic track, no far-end speakers, and no warning. A call in the app now records the whole output (the Windows default, or the one chosen on the Audio page). The warning moves here from T6.15: when the mic has spoken for 60 s and the far end's track for under 3 s (judgements), one banner, replaced if the far end is heard later.
  - accept: `cargo test -p earshot-runtime live_far_end`; `cargo test -p earshot-desktop audio_settings`; `cargo test -p earshot-desktop far_end`; `cmd /c pnpm -r test`; a real call is still GATE-01a and GATE-02
- [x] T6.15a Probe what Windows lets us hear of a Teams call
  - deps: T6.14a
  - do: private notes: reports say Windows hides `Communications` streams from process loopback, so no process of Teams would help. A standalone diagnostic, `examples/teams_probe.rs` in `earshot-capture-wasapi`: `call` measures every Teams process, every render session's meter and every output's loopback at the same time during a call; `category` plays a quiet tone as `Other` and as `Communications` under process loopback of itself. It prints levels only, keeps no audio and opens no network. The owner runs it (GATE-style, it plays sound) and the result decides T6.15.
  - accept: `cargo test -p earshot-capture-wasapi --example teams_probe`; `cargo run -p earshot-capture-wasapi --example teams_probe -- call --seconds 3` runs here
- [x] T6.24 Bluetooth headset mode warning
  - deps: T6.14
  - do: from TODO: when the chosen mic is a Bluetooth hands-free (HFP) endpoint, Windows records at 8-16 kHz and degrades playback; detect it from the endpoint's properties (form factor / bus / sample rate), warn at session start and in the CLI, and suggest the laptop's own mic with the headphones. Cover Bluetooth headphones with the Surface's own mics and with the headset mic in GATE-01b.
  - accept: `cargo test -p earshot-capture-wasapi` (classification of recorded endpoint property sets; device test where a Bluetooth device exists)
- [x] T6.25 Speaker names survive a re-run of the cleanup
  - deps: T6.13d2
  - do: from TODO (T6.4b/T6.4c): a rename of a speaker on a re-processed track is lost when the cleanup runs again, because speaker rows are re-created. Carry identity across runs (match new clusters to the previous run's speakers by embedding centroid with the calibrated threshold, keep display names and Person links), with tests for rename, merge and a new speaker.
  - accept: `cargo test -p earshot-runtime post`
- [x] T6.26 Delete a meeting
  - deps: T6.13d2
  - do: from TODO (T6.13c/T6.13d2 reviews): deleting a meeting tombstones its synced rows, deletes its audio files (file before row), erases every device-local record keyed by it (`live-stretches/*`, `live-speakers/*`, `echo-*`, `pass-backend/*`, `export-fingerprint/*`) with secure_delete and a checkpoint, and leaves the Obsidian note alone (export is the user's). Also sweeps records of meetings whose cleanup never ran once their meeting is deleted.
  - accept: `cargo test -p earshot-store`; `cargo test -p earshot-runtime` (no row, file or local value of the meeting remains, on-disk erasure checked with a second connection open)
- [x] T6.27 Single-track in-room capture on the desktop
  - deps: T6.6
  - do: from TODO (T6.3b lane notes): `WasapiSource` always opens a reference track; add a mic-only in-room mode (no loopback, no canceller, in-person track policy) for meetings with no call audio, and an `EndReason::Failed` for a capture that ended on an error.
  - accept: `cargo test -p earshot-capture-wasapi`; `cargo test -p earshot-runtime`
- [x] T2.2d DTW word timestamps for whisper (low priority)
  - deps: T2.2b
  - do: from TODO (T2.2b lane): whisper word times from DTW with per-model alignment heads (flash attention off), measured against the current token-timestamp times on the smoke set; Parakeet (the default) already has accurate word times (T6.10e: 97% within 250 ms), so this only matters for whisper rungs.
  - accept: `cargo test -p earshot-asr-whisper` (alignment-head tables pinned per model; word-time error measured and logged)
- [x] GATE-01a Real capture on the desktop
  - deps: T6.6, T6.6a, T6.7, T6.10a, T6.10b, T6.14, T6.17, T6.18 (GATE-12 removed by the owner: talk-over leak is a robustness case and does not block real-call testing)
  - do: Changed by the owner: judged on the app, not `earshot record`. Real meetings since the capture fix count as the real call; Ctrl+C becomes quitting from the tray during the cleanup, keeping the audio with the meeting left `Unprocessed`; the device switch happens during any real call in the app. Originally: split from GATE-01 by the owner. Steps in gates/GATE-01.md (desktop part): device tests, `earshot devices`, a real call on speakers with an output-device switch, the cleanup progress and Ctrl+C during it, a listening note on residual echo in `mic.flac`.
- [ ] GATE-01b Real capture on the Surface
  - deps: GATE-01a, T2.6
  - do: the same steps on the Surface (Snapdragon X2 Elite, 12 cores, Hexagon NPU, 16 GB) once an ARM64 build exists (gates/GATE-01.md, Surface part).
- [x] T6.19 Local LLM spike: llama.cpp in-process
  - deps: T6.6
  - do: D19. Embed llama.cpp in a new adapter crate (bindings pinned with `=` like whisper-rs), in the same process as whisper.cpp: both vendor ggml, so prove one binary links and runs both (shared ggml or a dynamic library; no duplicate symbols), on CUDA, Vulkan, CPU, and an ARM64 cross-compile. Generate from a small permissive GGUF model fetched through `models/manifest.toml` with JSON-schema-constrained output against `schemas/summary.v1.json`. Record the build matrix, link approach and tokens per second per backend in a `docs/` note. Stop and ask if the two ggml copies cannot coexist.
  - accept: `cargo test -p <new crate>` (a schema-constrained generation that parses as `SummaryV1`); the docs note
- [x] T6.20 Summary-quality evaluation
  - deps: T6.19
  - do: D19. An evaluation over AMI meetings with their human abstractive summaries and decision/action annotations (CC BY 4.0, fetched through `fixtures/manifest.toml`): transcripts from the AMI references and from Earshot's own final pass, summarised by each candidate, scored against the human summaries (a reference metric such as ROUGE-L, cited) and for decisions and action items caught. Measure first, then agree thresholds with the owner and add them to PLAN 4.2. Candidates (permissive only): Qwen3 4B/8B/14B, Phi-4-mini, and current equivalents; the remote opt-in providers as an informational ceiling.
  - accept: the evaluation runs from `cargo xtask` and writes a report; thresholds agreed and recorded (no summary-quality threshold; quality is informational, tuned through the prompt in T6.20b)
- [x] T6.20a Summary quality judged by atomic facts
  - deps: T6.20
  - do: found in T6.20: AMI annotates decisions and actions as long sentences bundling several items, so token-overlap matching undercounts a summary that lists them separately (ES2004c: the model covered four of five bundled decisions and scored 0/1), and no overlap rule sees contradictions (the model said voice recognition was excluded; the annotator said "possibly, if in budget"). Split each reference decision/action sentence into atomic facts (once, with a local model, outputs committed and spot-checked by hand), then judge each fact against the summary with a local LLM judge as covered, contradicted or missing; validate the judge on a hand-labelled dev sample (agreement reported) before trusting it; report coverage and contradiction rates per model on dev and test. Contradictions count against a model more than misses.
  - accept: `cargo test -p xtask sum_eval`; judge agreement on the hand-labelled sample logged; dev and test tables in docs/summary-eval.md
- [x] T6.20b Summary prompt tuning on the default model (keep the current prompt; lean's gain was one item of 33 and it failed one test meeting)
  - deps: T6.20
  - do: found in T6.20: summary quality is not gated (it needs a human and real use, and moves with the prompt), so improve the prompt rather than the model. On Qwen 3.5 9B chunked only (the default), write a few prompt variants (for example: what counts as a decision versus a discussion point, owners named only when stated, one action per task, fewer notes), each with its reason written before it is measured. Compare them on dev only: the automatic scores on the 20 dev meetings and the blinded meaning judge (Claude Code agents, the T6.20 brief) on the 5 judged dev meetings, with the T6.20 rows as anchors. Pick by a rule written before measuring (more decisions and actions covered, no more contradicted); run test once for the winner; prompt snapshots regenerated and reviewed; change the format key.
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p earshot-llm-local`; variant tables (automatic and judged) and the single test run in docs/summary-eval.md
- [x] T6.20c Two-call chunk summaries trial (2026-10-31: no gain; judged 9B actions 7 -> 4 of 11, owners guessed wrong; the owner: two-call not used, kept as an evaluation option)
  - deps: T6.20b
  - do: the meaning judge found Qwen 3.5 9B covering about half the actions Claude covers. Evaluation only (production unchanged): per 600 s chunk, call A extracts only action items (owner, task, due, citations) and call B everything else; the combine step merges paraphrased duplicate actions (code removes exact repeats only) and never drops a distinct commitment; report actions in chunk outputs against the final summary. No KV-cache prefix reuse yet. Qwen 3.5 4B and 9B on the 5 judged dev meetings, current single-call chunked and Claude as anchors, blinded meaning judge. Report only: no selection rule and no default change from 5 meetings (the owner); your own prompt variants and a larger judge sample may follow.
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p earshot-llm-local`; `cargo test -p xtask sum_eval`; automatic and judged tables in docs/summary-eval.md
- [x] T6.20d Larger model screen: Qwen3-30B-A3B-Instruct-2507
  - deps: T6.20c
  - do: one clearly larger local model as a test, unlikely to ship. Evaluation only: model in manifest set `llm-eval` (community GGUF pinned by commit and SHA-256, Apache-2.0 on the Qwen card), a `sum_eval` candidate, the 5 judged dev meetings chunked with the production prompt, blinded meaning judge with Claude and the shipped 9B as anchors. Report only. Result (2026-11-01): judged 17 / 7 against the 9B's 19 / 8 (within noise, not better) at about 10x the time on the CPU; the 9B stays the default.
  - accept: `cargo test -p xtask models`; `cargo test -p xtask sum_eval`; automatic and judged tables in docs/summary-eval.md
- [x] T6.20e Summary prompt rounds for actions and decisions, judged by hand
  - deps: T6.20b
  - do: iterate on the shipped summary rules to find more real action items and decisions, judged by hand each round. Dev only: the 5 judged dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), AMI reference transcripts (`--source reference`, no audio), 600 s chunks (production setup), tuned on Qwen 3.5 4B (the CPU and live-summary model) on the desktop GPU; the 9B is run on the baseline and the final prompt as a check. Round 0 is today's prompt. Each round is one new `PromptVariant` (a complete rules file under `src/prompt/templates/variants/`, its snapshot, its reason written in the doc before the run, its own `--out`); never edit `system.md` during the rounds. Every action and decision in every summary is judged against the transcript, not only the AMI list, and listed item by item in `docs/summary-eval/prompt-rounds-<date>.md`: real items caught (reworded counts), owner right, made up, real items missed; the per-meeting list of real items is written from the transcripts before round 1 and kept fixed. Stop after two rounds in a row with no gain, or at 6 rounds. A round counts as better only if real actions plus real decisions caught rises with no rise in made-up items and every summary valid. The AMI test series is not run during the rounds; the winner runs on it once (automatic scores only) and the owner decides whether it replaces `system.md` (then the format key bumps and `accept m4` re-runs). Result (2026-11-04): 5 rounds (0-4), stopped after two without a gain; winner round 2 `settled` (4B 35 -> 37 real items caught of 60, made up 24 -> 10; 9B check 37 -> 40, made up 19 -> 15); on the test series (automatic) the 9B failed one meeting from a repeated item and action scores did not rise, so `system.md` is unchanged pending the owner (docs/summary-eval/prompt-rounds-2026-11-04.md).
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p xtask sum_eval`; the rounds doc with every item listed and the stop reason
- [x] T6.20f Adopt the `settled` summary rules as the production prompt
  - deps: T6.20e
  - do: The owner adopted T6.20e's winner after reading its results, including the 9B test-series failure from a repeated item. `system.md` becomes the `settled` rules (action items: tasks for after the meeting, the owner the person addressed; decisions: every settled choice, the final one, not proposals). The `settled` variant is removed (it would equal `Current`) and today's earlier rules are kept as `PromptVariant::Previous` for baselines. The sum-eval format key bumps to `indented-numbered-r5` in both places; snapshots are regenerated and the map snapshot's budget grows by the measured system-message growth (464 tokens) so its plan shape is unchanged; `accept m4` re-runs.
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p xtask sum_eval`; `cargo xtask accept m4`; `cargo xtask ci`
- [x] T6.21a Local summary backend in the runtime
  - deps: T6.20
  - do: D19, split from T6.21. The runtime's summary service gets a local backend over `earshot-llm-local`: the model loaded on its own thread (`LocalLlm` is `!Send`), the meeting summarised with the chunk plan of T6.20 (600 s windows over the final transcript, else the live draft), and the result stored through the same transaction and checks as a remote summary (deleted meeting or changed segment ids -> `Conflict`, nothing written; only an empty title filled). The backend that ran is taken from llama.cpp's load log (`log::classify`), never from the request, and recorded with the summary's model name. No `NetClient` exists for a local summary and its audit log stays empty. The runtime's `cuda` feature forwards to `earshot-llm-local`.
  - accept: `cargo test -p earshot-runtime summary` (local tests with the small `llm`-set model, `-- --include-ignored` after `cargo xtask fetch-models --set llm`)
- [x] T6.21b Local summaries by default: model choice and shipping
  - deps: T6.21a
  - do: D19, split from T6.21. A shipped manifest set `summary` with Qwen 3.5 9B (Q4_K_M) and Qwen 3.5 4B (Q4_K_M), Apache-2.0, pinned like every model; the choice is the 9B when the machine reports more than 12 GiB of physical memory (`total_memory()`; Windows reports installed memory minus firmware and GPU reservations, so a 16 GB machine reports less than 16 GiB; the owner), else the 4B (T6.20: 9B chunked is the default, 4B the low-memory fallback; Gemma 4 not shipped). The CLI (`record`, `replay`) summarises locally by default when the chosen model is installed and no `--llm-url` / `--llm-provider` is given, with `--no-summary` to skip; remote endpoints only when the user configures one. `cargo xtask build` (and the gate build) puts `llama.dll` and llama.cpp's `ggml*.dll` next to the exe, and the exe refuses to start a local summary with a clear message when they are missing.
  - accept: `cargo test -p earshot-cli`; `cargo test -p xtask build`; a replay with the local model stores a valid summary with zero audit entries
- [x] T6.21c Summaries made during the meeting (closed as covered by T8.3j; the cheaper combine step is not pursued)
  - deps: T6.21b, T6.20b, T6.20c
  - do: found in T6.20 (chunking experiment): the wait at hang-up is the last chunk plus the combine step, which is 60-70% of it; summarising each 600 s window while the meeting runs would cut the wait only if those chunk summaries are kept. Decide how the live draft's chunk summaries relate to the final pass (which replaces the live rows and their segment ids), and whether a cheaper combine step (carry the chunks' decisions and actions through, rewrite only the overview) keeps quality on dev. Design with the owner first.
  - accept: agreed with the owner when the design is
- [x] T6.22 Claude as an opt-in summary provider
  - deps: T6.21
  - do: D19. A native Anthropic Messages API client in `earshot-llm` on `earshot-net` (allowlist is exactly the configured host; API key never logged; text-only request), alongside the OpenAI-compatible client; JSON output validated and repaired as today. Live tests only against a user-provided key and never gate a task.
  - accept: `cargo test -p earshot-llm` (mock server for both providers)

### M7 - Desktop app

- [x] GATE-11 Design the desktop UI together
  - deps: T6.6
  - do: hard stop requested by the owner before any GUI work: the UI (layout, flows, look and feel, Tauri/React structure) is designed together with the owner in an interactive session; no lane starts M7 until this gate passes and the outcome is recorded in PLAN. Input from the owner (D20): the meeting view is reassurance, not a scrolling live transcript: listening status, voice activity, a word count and the transcript as it lands, then a "summary ready" notification.
- [x] T7.1 Tauri scaffold and IPC contract
  - deps: T6.4d, GATE-11
  - do: pnpm workspace (`apps/desktop`, `packages/ui`), React + TypeScript + Vite, vitest, Tauri 2 (its crate joins the Cargo workspace here); typed commands/events via tauri-specta; CSP with `connect-src` limited to IPC and self. D21: Fluent UI React v9 (`@fluentui/react-components`) with a theme that follows the system light/dark setting; `packages/ui` owns one IPC interface with a Tauri implementation and a mock implementation (component tests and a browser preview of every screen run on the mock); app shell with the meeting list and an empty state.
  - accept: `pnpm -r test` and `cargo xtask ci`
- [x] T7.2a Runtime host layer for the app
  - deps: T6.6
  - do: found while preparing T7.2 (2026-11-01): the host glue the app needs lives only in `earshot-cli/src/session.rs`. Move its non-printing parts into `earshot-runtime` (`host` module): model choice (`choose_model`, `ModelChoice`), summary plan (local / remote / none, DLL check), session start for both capture modes, and a post-meeting runner that takes a progress callback (`PostStage`) and a `CancelToken` and then runs the summary; the CLI calls it so behaviour stays identical (CLI tests unchanged). Add what the meeting screen needs and the runtime lacks: per-track voice activity in events (decided in T7.2a: core `EnergyVad`, an adaptive-floor energy VAD, on the session thread over the pipeline audio; the workers' Silero runs 30-60 s late, so it cannot drive a live indicator), transcript lag in seconds, a meeting activity query (recording / processing / idle, safe after a crash), and a host-level "summary stored" or "summary skipped/failed" outcome.
  - accept: `cargo test -p earshot-runtime host`; `cargo test -p earshot-cli`; `cargo xtask ci`
- [x] T7.2 Live meeting screen
  - deps: T7.1, T7.2a
  - do: D21 / docs/ui-design.md "During the meeting": capture mode choice and record/stop, status line and clock, warning banners, You/Them meters with voice activity, words / transcript lag / backend tiles, the transcript as it lands (speaker and text in separate columns), compact card, post-meeting progress (`PostStage`) and the summary-ready notification. No notes editor (deferred, PLAN 1 Later). Checked by screenshot in the browser preview, light and dark.
  - accept: `pnpm -r test` (component tests with mocked IPC)
- [x] T7.3a Runtime meeting editing
  - deps: T7.2a
  - do: found while preparing T7.3 (2026-11-01): runtime functions the meeting detail needs, each one store transaction that re-reads rows under the lock and changes only its own fields: rename speaker, merge speakers (repoint segments, tombstone the loser), link a speaker to a new or existing Person, rename a Person (rewrites its speaker rows), list persons with voiceprint status, remember voices from a meeting (enrol from the stored identity centroids, opt-in) and forget a person's voiceprints; summary history kept to one undo (`store_summary` keeps the previous summary restorable, `undo_summary` swaps them, `MeetingView` and export still read the newest); re-transcribe a meeting from its stored audio (no live reuse; missing audio reported); a speaker audio sample (longest segment, capped, from the FLAC, missing file handled). Reviewer required (data model and biometrics).
  - accept: `cargo test -p earshot-runtime edit`; `cargo test -p earshot-store`; `cargo xtask ci`
- [x] T7.3 Meeting detail and speakers
  - deps: T7.2, T7.3a
  - do: D21 / docs/ui-design.md "Meeting detail": header, summary (TL;DR, decisions, read-only actions with owners) with citations that jump to transcript lines, transcript tab (time, speaker, text columns), who-is-who panel (play sample, name, merge, link to Person, remember voices opt-in, forget voiceprints), re-transcribe, re-summarise (replace, previous kept for one undo), export, delete. Checked by screenshot in the browser preview, light and dark.
  - accept: `pnpm -r test`
- [x] T7.3b Voices without words
  - deps: T7.3
  - do: an in-room meeting could show a speaker with no lines and no sample (a diarised voice that no transcript word was assigned to). The final pass creates speaker rows only for diarised speakers that own at least one Final segment (labelling runs before the rows are written; a wordless cluster takes no part in carrying earlier names, and an earlier row it would have continued is tombstoned like any replaced row), and reports each wordless voice as one `PostWarning::VoiceWithoutWords` (track, speech). The meeting detail lists only speakers with a segment in the transcript it shows, so rows from before this fix disappear too.
  - accept: `cargo test -p earshot-core diar`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-desktop detail`; `cargo xtask ci`
- [x] T7.3c Test only the default models
  - deps: T7.3b
  - do: D22. (1) `asr.wer_ami_ihm` measures the production default model (the English ladder's first rung for the backend, through `m2_prod` like the deltas; same AMI IHM rule, threshold 22% unchanged) instead of large-v3-turbo; no turbo row. (2) `asr.wer_smoke_tiny` becomes `asr.wer_smoke_base` on base.en (threshold 10% unchanged) and every test, example and accept runner that loads tiny.en, large-v3-turbo, small.en (outside Vulkan) or a non-`summary` local LLM (qwen2.5-1.5b) moves to a default model, every assertion kept. (3) Manifest sets: `ci` holds only default models (drops tiny.en, gains Parakeet 0.6B v2 and 110M from `eval`, about 3 GB); `desktop` holds only defaults; the other models move to a user-choice set the app offers; `cargo test -p xtask models` fails if `ci` or `desktop` holds a transcription or summary model outside the default ladders. PLAN 4.2, `acceptance/thresholds.toml` and `EXPECTED_IDS` change in one commit. Split if past about 800 lines.
  - accept: `cargo test -p xtask models`; `cargo xtask accept m2`; `cargo xtask ci`
- [x] T7.3d Delete dialog opened from the menu keeps focus
  - deps: T7.3
  - do: found on the Surface's first `cargo xtask ci` (2026-11-02): "asks before deleting and names the meeting" failed about 1 run in 5 there (never on the desktop, 0 in 22): the alert dialog was in the DOM with `aria-hidden="true"` because focus stayed on the "More actions" trigger, which Fluent's Menu focuses in the commit that closes it. Open the delete dialog only after the menu has closed (controlled Menu, the dialog opened from the parent's effect after the menu's close effect). The test also asserts that focus is inside the dialog and not on the trigger. Root cause found on the Surface (2026-11-03): jsdom has no layout, so tabster's `isVisible` rejected every element, the dialog focused its own surface, tabster never activated the modal and hid it on its 250 ms pass; whether the test's query beat that pass was timing. Fixed in the jsdom setup (`offsetParent`, body size); the menu item is back to a plain `setDeleting(true)`.
  - accept: `cmd /c pnpm -r test`; `cargo xtask ci` (the race does not occur on the desktop; the Surface confirms the fix with 15 repeated runs of `src/detail/MeetingDetail.test.tsx`, logged when the owner runs them)
- [x] T7.3e Native ARM64 builds from every xtask cargo
  - deps: T2.6
  - do: found on the Surface's first `cargo xtask ci` (2026-11-02): the workspace clippy step configured llama.cpp with the Visual Studio generator and MSVC, which ggml refuses on ARM64 ("MSVC on ARM requires GGML_NATIVE=ON, use clang"); only `cargo xtask build` set up clang-cl and Ninja. `ci::cargo_command()` now adds the same environment (`build::target_env` for ARM64, LLVM `bin` first on PATH) on an ARM64 Windows host, so `ci`, `accept` and every child cargo build natively there.
  - accept: `cargo test -p xtask --lib ci_arm64`; `cargo build -p xtask --target aarch64-pc-windows-msvc`; `cargo xtask ci` (the Surface's own `cargo xtask ci` confirms it)
- [x] T7.4a Streaming model downloads in the app
  - deps: T7.3, T7.3b, T7.3c
  - do: from TODO (earshot-net review): in-app model downloads go through `earshot-net` with a streaming API (no `max_body` buffering), resume from `.part`, SHA-256 and size verification as xtask does, and redirect hosts listed in the manifests (`redirect_hosts`, e.g. the HuggingFace CDN) checked by the allowlist on every hop.
  - accept: `cargo test -p earshot-net`; `cargo test -p earshot-runtime` (download through a local mock with a redirect and a resume)
- [x] T7.4d Hardware facts and model catalogue in the runtime
  - deps: T7.4a
  - do: split from T7.4 (2026-11-04). What "This is your hardware" and the Models page need, with no UI: (1) hardware facts: processor brand string and physical/logical cores, every display adapter with its exact name, dedicated video memory and driver version (DXGI), the NPU's name when present (DXCore ML adapters), total memory; Windows calls live in one adapter module with all `unsafe` in one file and SAFETY comments (core stays pure), a missing fact is `None`, never a guess. (2) A runtime model catalogue from the embedded `models/manifest.toml`: per transcription and summary model its artefacts (url, path, size, sha256), languages and ladders, with every cache path jail-checked under the models directory (the xtask `jail_join` rule), pinned to the manifest by test so the runtime and xtask cannot disagree. (3) Install state per model (all artefacts present with the right size; a full hash only on demand) and removal of a model that is not the chosen one, reporting the bytes freed. (4) The recommendation without any download: `model_check::starting_rung` over the facts for a language set, plus the summary model rule, with the reason text; "too slow here" comes only from a stored check, never from fixed text; fix `host::choose_model`'s hard-coded `["en"]` so the stored check's languages are the meeting languages.
  - accept: `cargo test -p earshot-hardware` (facts parsed from fixed inputs; one test prints the real facts of this host); `cargo test -p earshot-runtime catalogue`; `cargo test -p earshot-runtime hardware`; `cargo xtask ci`
- [x] T7.4e Model settings, download and check jobs, IPC
  - deps: T7.4d
  - do: split from T7.4. Device-local settings (new `DeviceRecord` variants): chosen transcription model, chosen summary model, "where it runs" per task (automatic / a named part / processor), meeting languages. The app's meeting start, re-transcribe and summaries use them through `host` (today they call `choose_model(None, ..)` and `SummaryRequest::default()`); a model forced onto a part it cannot use runs on the processor and the reason is reported. A download job per model (all its artefacts through `earshot_runtime::download`, progress in bytes, pause/cancel keeping `.part`, one job per model) and a check job (the model check run in-app, recorded like `earshot benchmark`), each with its own event type keyed by model, under `catch_unwind` like `JobRunner`. IPC commands with mocks and regenerated bindings: hardware facts, catalogue with install state and recommendation, get/set the model settings, start/cancel a download, remove a model, run the check. From the T7.4d review: derive the "Recommended" and "Lighter" choices from `recommend::recommend_transcription` (today `transcription_choices` takes the first backend's rung without the Vulkan rule, so a Vulkan build would contradict itself; add a Vulkan case); pass the stored check's fallback model and `host::FALLBACK_MODEL` as protected names to `Catalogue::remove`; use one core count for the advice and the check (`hardware_facts().cores`) or pin both.
  - accept: `cargo test -p earshot-runtime`; `cargo test -p earshot-desktop`; `pnpm -r test`; `cargo xtask ci`
- [x] T7.4ea One ONNX Runtime library per app session
  - deps: T7.4e
  - do: found in the T7.4e review, decided by the owner: ONNX Runtime is one library per process, so with T6.11's rule (CUDA library only for a Parakeet model on CUDA) an app session that ran a Whisper meeting could not later run Parakeet on CUDA, nor the reverse, until a restart. `backends::onnx_library` now loads the CUDA library whenever CUDA is in the order, for every model (the CUDA library serves the CPU sessions too; QNN keeps its rule, it uses the CPU library's DLL).
  - accept: `cargo test -p earshot-runtime backends`
- [x] T7.4f Settings screen and the Models page
  - deps: T7.4e
  - do: split from T7.4. The Settings screen (left section list, sections as in docs/ui-design.md; sections not built yet show what will be there) opened from the shell's Settings button, and the Models page as in `docs/ui-mockups/t7-4.html` and T7.4's recorded answers: hardware rows that follow the current choices, the recommendation, four transcription choices, the summary models, download progress, the check result, Remove with space freed, "where it runs" with the fallback note. UI only through `useIpc()`; judged by screenshot of the Vite preview in light and dark.
  - accept: `pnpm -r test`; `cargo test -p earshot-desktop`; screenshots in light and dark
- [x] T7.4g Summaries settings page
  - deps: T7.4f
  - do: split from T7.4: who writes the summary (on this computer with the chosen model / own endpoint with URL, provider and model, validated with the remote-endpoint warning / no summary), persisted and used by the app's summary setup; the "Summarise during the meeting" toggle (closes T8.3jc, its agreed text). An Anthropic API key is pasted once on the page and kept in Windows Credential Manager (encrypted to the Windows account), never in Earshot's database, logs, exports or IPC responses (the page only learns whether one is stored); Remove deletes it; it is sent only as `x-api-key` to the configured Anthropic endpoint through `earshot-net`. A third-party "log in with Claude" for API calls does not exist (checked against Anthropic's authentication docs 2026-11-04: API keys, Workload Identity Federation, App Attest for iOS/macOS); the Claude.ai sign-in route is T7.4ga.
  - accept: `pnpm -r test`; `cargo test -p earshot-desktop`; screenshots in light and dark
- [x] T7.4ga Claude through Claude Code as a summary provider
  - deps: T7.4g
  - do: The owner, the way another desktop app does: a fourth summary choice that runs the user's installed Claude Code CLI in print mode on the user's Claude.ai sign-in. Earshot finds the CLI, checks it is installed and signed in (status shown on the Summaries page, with how to sign in), runs it with no tools, no user settings or MCP servers, in an empty temporary folder, with only the summary prompt text on stdin, a deadline and cancellation, and validates and repairs the reply exactly like the other providers (schema, citations). The PLAN 3.9 exception applies: Earshot records locally when it handed text to Claude Code and how many characters, never the text. Off unless chosen. Before Earshot is installed for anyone else, Anthropic's terms on products offering a Claude.ai sign-in are checked again.
  - accept: `cargo test -p earshot-llm` (a fake CLI binary: success, invalid JSON then repair, not signed in, missing CLI, timeout, cancel); `cargo test -p earshot-desktop`; `pnpm -r test`; one real summary of a replayed meeting through Claude Code logged
- [x] T7.4gb Claude through Claude Code against Qwen 3.5 4B: speed and quality
  - deps: T7.4ga
  - do: how quick and how good a Claude summary is next to the 4B. The 5 judged dev meetings of T6.20e (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c; AMI reference transcripts, 600 s chunks, the production prompt), summarised by Qwen 3.5 4B on the Surface processor and by Claude through Claude Code (default model, recorded), each meeting timed from the start of the summary to the stored result; every action and decision judged by hand against the fixed real-item lists in docs/summary-eval/prompt-rounds-2026-11-04.md with its judging rules (caught, owner right, made up, missed). Public AMI text only goes to Claude. Report only, no default changes.
  - accept: a results table per meeting (time, real actions and decisions caught, made up, owners right) with every item listed, in docs/summary-eval/
- [x] T7.4gc OpenAI Codex as a summary provider
  - deps: T7.4ga
  - do: the same route as T7.4ga for OpenAI: a fifth summary choice that runs the user's installed Codex CLI on its ChatGPT sign-in, the way another desktop app does. Reuses T7.4ga's pieces: `earshot_proc::run` (kill-on-close job, deadline, cancel, complete output), the prompt only on stdin, no tools, sandbox and approvals at their most restrictive, no MCP servers or user configuration that adds tools, an empty temporary working folder, validation and repair like the other providers, a sign-in status for the Summaries page, the handover record (when, characters, outcome; never text), and a missing CLI or sign-in skipping the summary with a banner, never the recording and never a fallback to another provider. PLAN 3.9's exception is extended to it. Flags come only from the installed CLI's `--help`. Before Earshot is installed for anyone else, OpenAI's terms on products using a ChatGPT sign-in are checked.
  - accept: `cargo test -p earshot-llm` (a fake CLI: success, repair, not signed in, missing CLI, timeout, cancel, cut-off output); `cargo test -p earshot-desktop`; `pnpm -r test`; one real summary of a short fixed public transcript through the installed Codex logged
- [x] T7.4gd Follow-ups from the T7.4 reviews
  - deps: T7.4ga
  - do: clear the TODO.md follow-ups of T7.4a-T7.4ga outside the summaries code (those wait for T7.4gc). Each fix gets a test that fails without it where behaviour changes; an item found wrong or not worth changing is closed with a one-line reason in the commit body. Items: one shared case table for `host_matches` run by both earshot-net and xtask (as `fixtures/ami/parse_cases.toml` pins the AMI parsers); `download()` reports a `.part` that was already complete distinctly from a resumed transfer, and a size-only present check for the Models page; a direct test of `PendingEntry`'s redirect-hop drop arm; one helper for the verdict of a (model, backend) pair tested twice (`StoredCheck::verdict` and `ModelSearch`); `catalogue_broken_manifests_are_errors` asserting the error variant per case; `if cfg!(windows)` in the real-facts tests replaced by `cfg_attr(not(windows), ignore = ...)`; removing or downloading a model refused (`busy`) while the model check runs, and a Rust test of `cancel_check` ending in one `Failed`; the desktop's model-check child moved onto `earshot_proc` (one kill-on-close job object); `RuntimeEvent::LiveSummariesOff` shown as a meeting banner. Fixed items are removed from TODO.md by the coordinator.
  - accept: `cargo test -p earshot-net`; `cargo test -p xtask`; `cargo test -p earshot-runtime`; `cargo test -p earshot-desktop`; `cargo test -p earshot-proc`; `pnpm -r test`; `cargo xtask ci`
- [x] T7.4h Speakers, Obsidian and General pages
  - deps: T7.4f
  - do: split from T7.4: speaker detection timing (the T6.13d4 setting and its warning text); "your voice": record a short sample and enrol it as Me (opt-in), or forget it; Obsidian vault folder and subfolder, validated on save, used by export (replaces `AppState.vault = None`); General (start with Windows, theme follows the system) as far as D21 lists it.
  - accept: `pnpm -r test`; `cargo test -p earshot-desktop`; `cargo test -p earshot-runtime`; screenshots in light and dark
- [x] T7.4i Audio page: devices and capture mode
  - deps: T7.4f
  - do: split from T7.4: list capture and render endpoints (WASAPI enumeration in `earshot-capture-wasapi`, `unsafe` only in `wasapi.rs`), choose the mic and output by endpoint id with "default" kept as an option, the default capture mode, persisted and used by `start_meeting`; a chosen device that is gone falls back to the default with a warning.
  - accept: `cargo test -p earshot-capture-wasapi`; `cargo test -p earshot-desktop`; `pnpm -r test`; device tests under GATE-01a
- [x] T7.4j Audio check
  - deps: T7.4i
  - do: split from T7.4: the optional audio check (first run and Settings, never during a meeting) as T7.4 describes: chirps through the chosen output, speaker-to-mic delay and echo level, headphones detected, too quiet or clipping flagged, stored per mic + output pair as the delay estimator's warm-start hint (`set_delay_hint`).
  - accept: `cargo test -p earshot-runtime audio_check` (chirp analysis on recorded and simulated inputs); a device run logged
- [x] T7.4ja Audio check: an echo with two arrivals is heard
  - deps: T7.4j
  - do: the owner reported the check saying the microphone did not hear chirps it plainly heard, on a tablet's built-in speakers. Each chirp's correlation peak lands on one of two arrivals about 3 ms apart, so a two and two split failed the 1 ms agreement rule. Widen `audio_check::AGREE` to 5 ms (agreed by the owner); lags far apart still do not count as one echo.
  - accept: `cargo test -p earshot-core --lib audio_check`; `cargo test -p earshot-runtime --lib audio_check`; a device run on the Surface
- [x] T7.4k Storage: audio retention
  - deps: T7.4f
  - do: split from T7.4: the retention setting with T7.4's list (1 hour to forever, default 7 days); the clock starts when the summary is stored, or when the final pass ends with summaries off; never before a summary succeeds (changed by the owner: the clock starts when the final transcript is stored, whatever the summary does); only audio files go; runs at start-up and periodically; shortening it shows how many recordings go and asks first. `earshot_store::RetentionPolicy` gains sub-day durations and the summary-gated rule.
  - accept: `cargo test -p earshot-store retention`; `cargo test -p earshot-runtime retention`; `pnpm -r test`
- [x] T7.4 Settings and model manager
  - deps: T7.3, T7.4a, T7.4d, T7.4e, T7.4f, T7.4g, T7.4h, T7.4i, T7.4j, T7.4k (split 2026-11-04; T7.4 is done when they are)
  - do: D21 / docs/ui-design.md "Settings" sections (General, Audio, Summaries, Models, Speakers, Obsidian, Storage). (includes the T6.13d4 speaker-detection timing setting: during the call / after the call / after the call on battery, laptops only, with its warning text.) optional **audio check** (first-run and settings, never during a meeting): play short chirps through the selected output, measure speaker-to-mic delay and echo level, detect headphones (no echo heard), flag too-quiet or clipping levels, store the result per mic + output device pair as a warm-start hint for the delay estimator (T1.3); backend choice with auto-detect, model downloads through `earshot-net` with checksum + progress (manifest paths jail-checked under the models directory), summaries: local (the T6.21b model choice, with the model shown), a configured LLM endpoint (validated, remote-endpoint warning), or no summary ("no summary" is a setting, the CLI's `--no-summary`), vault path and folder (validated on save), retention (audio retention choices 1 hour, 4 hours, 12 hours, 1 day, 3 days, 7 days, 14 days, 28 days, 3 months (90 days), 6 months (180 days), 1 year (365 days), forever; default 7 days; the clock starts when the final transcript is stored, whatever the summary does (it was the stored summary until then); a meeting whose final pass has not succeeded keeps its audio; only audio files are deleted, transcript, summary, speakers and export stay; deletion runs at start-up and periodically, so overdue audio goes when the app next runs; shortening the setting applies to existing meetings after a confirmation that shows how many recordings go), typography, "Me" enrolment. Models page: first "This is your hardware": processor, GPU(s), NPU, RAM, as the runtime detects them; then "On this device we recommend ..." with the default preselected; the user keeps it or picks an alternative. Transcription offers exactly 3 choices, each with one line on why you would use it: the recommended model (first rung of this backend's English ladder), a lighter one (the next rung: older or busy machines, less memory) and a multilingual one (meetings not in English); summaries offer the shipped models (the T6.21b choice and the other one, with why) plus own endpoint and none. The recommendation is shown at once from the ladders and the memory rule (no model needs to be downloaded first); after the download the model check confirms it keeps up and steps down the ladder if not, and says so. The same page is first-run step 2 (T9.1). The owner's answers on the mockup (2026-11-04, docs/ui-mockups/t7-4.html): exact hardware names (each row also says what Earshot uses that part for; the memory row matters only for summaries on the processor, the graphics row shows its video memory); a model that is too slow on this computer is shown with "too slow here" and stays selectable (the user may prefer accuracy), and is left out only when it physically cannot fit in memory; the model check runs automatically after a download, and when it says too slow the user is told and may keep the model; transcription offers 4 choices (recommended, lighter, multilingual for European languages, multilingual for every language), and onboarding asks which languages are spoken (a tick list) and recommends from it; models not chosen can be removed, showing the space freed; helper models (VAD, speaker models) are never listed; "where it runs" can be overridden per task; Settings keeps the left section list. Round 3: "This is your hardware" follows the current model and "where it runs" choices (each row lists what runs on that part); a model forced onto a part it cannot use (no NPU version, too big for the graphics card) runs on the processor with a note on the page; onboarding recommends Parakeet v3 when any European language is ticked (English included) and Whisper large-v3-turbo for any other language; Whisper large-v3-turbo's text says it is less accurate and slower than Parakeet (AMI IHM 14.1% against 10.4%) and is for languages Parakeet v3 does not cover; the page makes no speed claim for Whisper large-v3-turbo on any computer (unmeasured on the Surface; the T7.4c benchmark shows it); the page shows no summary timings (they depend on meeting length and on summarising during the meeting); no larger summary models are offered (Qwen 3.5 27B and Qwen3 30B-A3B measured no better than the 9B).
  - accept: `pnpm -r test`
- [x] T7.4l Folder picker for the Obsidian vault
  - deps: T7.4h
  - do: A "Choose folder..." button beside the vault field opens the Windows folder dialog (IFileOpenDialog with FOS_PICKFOLDERS, owned by the app window, starting at the current vault) and fills the field; the vault is still checked and stored only on Save. No Tauri dialog or fs plugin (no new crates); the Win32 calls live in `folder_picker_win.rs` with SAFETY comments; cancel leaves the field as it was.
  - accept: `cargo test -p earshot-desktop --bins obsidian`; `cmd /c pnpm -r test`; the dialog opened and a folder chosen in the built app (or the ignored `obsidian_folder_picker_opens_and_returns_a_folder` run by the owner)
- [x] T7.4c Benchmark in Settings
  - deps: T7.4
  - do: The owner, later: a "Benchmark" section in Settings, like a game's benchmark: run the current settings (transcription model and where it runs, summary model and where it runs, summarising during the meeting on or off) against a bundled benchmark meeting and show the timings (transcript lag during the meeting, wait from hang-up to summary) next to what the check measured. The meeting is a short two-track replay shipped with the app (licence checked and attributed, e.g. an AMI excerpt under CC BY 4.0), run through the production replay path; no network. Nothing from the run is stored as a meeting.
  - accept: `pnpm -r test`; `cargo test -p earshot-desktop`; a benchmark run on the Surface and the desktop logged
- [x] T7.4b Editable summary style
  - deps: T7.4, T6.20c
  - do: the user can edit the summary's style (tone, length, emphasis, jargon) as a device setting that is one slot in the combine (final) step's prompt; the fixed core (schema, citations, merging duplicates, never dropping a commitment) stays outside the user's text, and layout stays in the export renderer. If chunk outputs are stored, a style change re-runs only the combine step.
  - accept: `cargo test -p earshot-core prompt` (a style slot cannot remove the fixed rules); `pnpm -r test`
- [x] T7.5 Tray and hotkey
  - deps: T7.4
  - do: tray icon with record state, global start/stop hotkey, minimise to tray during recording; closing the window during a meeting hides it to the tray, and quitting during a meeting asks first, then stops and finalises the recording (found in T7.2: today a process exit can cut finalisation); OS notification for "summary ready"; started by Windows (the T7.4h Run value gains a `--autostart` argument), Earshot starts in the tray with no window and no taskbar button.
  - accept: `pnpm -r test` and `cargo test -p earshot-desktop`
- [x] T7.6 M7 acceptance
  - deps: T7.5
  - do: `accept m7`: app binary `--replay <fixture> --llm-url <loopback> --data <dir> --vault <empty dir> --model <name> --exit-after-export` headless mode that runs and exits before any window or webview is created, checked like e2e.replay; the app's release build (what `tauri build --no-bundle` runs) for x64; arm64 build if the toolchain is present. (2026-11-05: the plan's `--llm-mock <dir>` became a loopback URL, because a shipped binary carries no LLM mock; the mock runs in xtask.)
  - accept: `cargo xtask accept m7`
- [x] T7.6a An empty object file from a stopped native build
  - deps: T7.6
  - do: found by the owner's first `cargo xtask build --app --features cuda` (2026-11-05): ggml-cuda's `sumrows.obj` was left at 0 bytes, MSBuild then treated it as up to date, and every rebuild failed with LNK1136 behind cmake's "exit code: 1". Before a build, `xtask build` removes 0-byte `.obj` files under the profile's native build folders and says so; when a build fails it names the first native error rather than only cargo's.
  - accept: `cargo test -p xtask build`; `cargo xtask build --app --features cuda` on the desktop
- [x] GATE-02 Desktop dogfood
  - deps: T7.6, T7.6a
  - do: open the gate: install the x64 build, use it in at least one real meeting, report issues in `gates/GATE-02.fail` or results in `.pass`. Optionally add the recording and a corrected transcript under `fixtures/private/`.

### M8 - Acceleration

- [x] T8.1 Benchmark tool
  - deps: T7.6
  - do: `cargo xtask bench asr|diar|aec --backend <b>` reporting RTF, latency p50/p95 for live chunks, peak memory; JSON output in `acceptance/results/bench-<host>-<backend>.json`.
  - accept: `cargo xtask bench asr --backend cpu`
- [x] T8.2 Vulkan parity
  - deps: T8.1
  - do: Vulkan build and WER parity test (within 1.0 point of CPU) where a Vulkan device exists; otherwise compile-only.
  - accept: `cargo xtask build --features vulkan` and parity test when available
- [x] T8.3 Parakeet on the NPU (ORT QNN)
  - deps: T2.6, T6.10e, T6.10c2
  - do: rewritten and pulled forward ahead of M7 by the owner (replaces the earlier Whisper-on-QNN plan, since Parakeet leads the ladder, T6.10e): Parakeet on the Snapdragon NPU through ONNX Runtime's QNN execution provider. (1) Microsoft's ORT release with the QNN EP for ARM64 fetched through models/manifest.toml (SHA-256), loaded through `earshot_onnx::init`; licence terms of the QNN/HTP libraries recorded (redistribution stays T8.4's question). (2) A `qnn` backend in the runtime and model check (ARM64 order QNN > CPU, D20) with backend evidence from ORT node placement: a model meant for the NPU with nodes on the CPU EP is an error unless its documented partition says so. (3) NPU-ready Parakeet models (static input shapes, quantised QDQ) from a reproducible conversion command, with the accuracy cost measured on the desktop (QDQ on the CPU EP vs float, LibriSpeech subset and smoke WER); converted models are built locally until the owner decides hosting. (4) ARM64 cross-compile with the qnn feature. (5) gates/GATE-03a.md for the Surface.
  - accept: `cargo xtask build --target aarch64-pc-windows-msvc --features qnn`; `cargo test -p earshot-onnx`; `cargo test -p earshot-runtime backends` (provider selection, evidence classification on recorded QNN placement logs, the static-shape piece runner with a fake session); the conversion command and the measured QDQ-vs-float WER logged; gates/GATE-03a.md written
- [x] T8.3c NPU conversion accuracy
  - deps: T8.3
  - do: found in T8.3 (2026-10-24): the static-shape QDQ conversion of Parakeet costs about 4.2 WER points against the float model (QDQ on the CPU EP vs float, desktop), while GATE-03a allows QNN within 1.5 points of the CPU. Bring the cost under 1.5 points (target 1.0) without tuning on acceptance data: calibration audio from LibriSpeech dev-clean only, then calibration size and variety, per-channel weights, activation calibration method, 16-bit activations or mixed precision for the most sensitive layers (per-layer sensitivity on dev data); pick by a rule written before measuring (smallest cost under 1.5 points with the lowest expected NPU cost), make it the `qnn-convert` default.
  - accept: `cargo test -p xtask qnn`; `cargo test -p earshot-runtime backends`; every variant's dev and test WER, size and op precision counts logged; `cargo xtask qnn-convert` default changed and docs/qnn.md updated
- [x] T8.3d Review the Surface's DXCore GPU-preference telemetry event
  - deps: T8.3
  - do: found by GATE-03a step 9 on the Surface (2026-11-03, `gates/GATE-03a.fail`): Windows' DXCore sends `QueryAndFillAdapterExtendedProperiesOnPlatform` (MEASURES; fields adapterLuid, gpuPreference, gpuPreferenceDecidingFactor, registryRequestedMode, finalMode, dxCoreTelemetryFactoryUUID) in every mode, the CPU-only baseline included; the desktop never sent it. Its values on the Surface (read from the kept traces) are adapter LUIDs, zero preference codes and a per-process factory id: system information. With the owner's agreement, add it to `qnn_etw::SYSTEM_EVENTS` beside the other DXCore events with a comment naming the reviewed values, and record the review in docs/qnn.md.
  - accept: `cargo test -p xtask qnn_etw`; GATE-03a step 9 re-run by the owner prints the unqualified PASS with `transcribe-backend qnn qnn:npu`
- [x] GATE-03a Parakeet on the Surface NPU
  - deps: T8.3, T8.3c, T8.3d
  - do: on the Surface: `earshot benchmark` lists QNN and CPU with the backend from ORT's node placement; WER parity on the LibriSpeech subset (QNN within 1.5 points of CPU, the GATE-03 rule); a realtime replay keeps up. Steps in gates/GATE-03a.md.
- [x] T8.3e Measured QNN ladder in the model check
  - deps: GATE-03a
  - do: found by GATE-03a (2026-11-03, `gates/GATE-03a.pass`): D22 names a QNN English ladder (Parakeet TDT 0.6B v2) but `models/manifest.toml` has no `qnn` `[[ladder]]`, so `earshot benchmark` on the Surface tested only the CPU rungs and recommended the CPU although the NPU ran both models at rtf 0.02 with WER within 1.5 points. Add the `qnn` English ladder from the gate's measurements (`docs/model-eval/qnn-surface.json`, `cpu-surface.json`; 0.6B v2 -> 110M as in D22's CPU and CUDA ladders, the second rung only if those numbers support it) with its source in docs/asr-models.md, so the model check tests QNN first on an ARM64 host with an NPU and records which backend ran from ORT's placement.
  - accept: `cargo test -p xtask models`; `cargo test -p earshot-runtime backends`; on the Surface, `earshot benchmark` (built with `--features qnn`) tests and recommends a QNN rung
- [x] T8.3f Summary model by where it runs
  - deps: T6.21b, GATE-03a
  - do: a short meeting's summary took minutes because the Surface (15.5 GiB reported, no CUDA) got Qwen 3.5 9B on the CPU; T6.20 already found the 9B "not a CPU candidate" (16.4 min per hour of meeting on 8 desktop threads, 4B 11.2) and named the 4B for the Surface, but T6.21b picks by memory alone. (1) Time the 4B and the 9B on the Surface CPU with the production setup (`cargo xtask sum-eval --backend cpu --split dev --source reference --model qwen3.5-4b-q4_k_m,qwen3.5-9b-q4_k_m --limit 3 --chunk-seconds 600 --threads 6`, 6 = `default_threads()` on 12 cores). (2) Change `summary_model::choose_summary_model` so a summary that will run on the CPU gets the 4B whatever the memory, and the 9B only on the GPU (memory rule unchanged there); the choice and `describe()` say why. Quality numbers stay T6.20's (no new quality run).
  - accept: `cargo test -p earshot-runtime summary_model`; the Surface timings logged (per meeting and per hour of meeting, both models); `cargo xtask ci`
- [x] T8.3g Phi Silica feasibility for summaries
  - deps: T8.3f
  - do: consider Windows' built-in Phi Silica (`Microsoft.Windows.AI.Text.LanguageModel`, Copilot+ NPU) for summaries on the Surface. Spike, no shipped code: from Microsoft's documentation and, if access allows, a small probe outside the shipped crates: availability on the Surface, what an unpackaged Rust/Tauri app needs (package identity, Limited Access Feature, Windows App SDK runtime), context and output limits against a 10-minute chunk, structured output, content filtering, off-device data and telemetry (the owner's rule), terms of use. If usable: a plan to measure it with `sum-eval` on the dev meetings against the 4B, plus a `qnn-etw`-style telemetry check, before any shipped use.
  - accept: `docs/phi-silica.md` with the findings, sources and a verdict; any probe's output logged
- [x] T8.3h NPU or GPU summaries without Phi Silica
  - deps: T8.3f
  - do: the other non-CPU paths for summaries on the Surface: ONNX Runtime GenAI on the QNN EP (the runtime and plugin GATE-03a cleared; an NPU build of a summary model, structured output support, context limits) and llama.cpp on the Adreno GPU (OpenCL backend; quant support for Q4_K_M; `llama-cpp-sys-2` support). Spike: documentation first, then measure the most promising path on the Surface with `sum-eval` (dev, CPU 4B as the baseline) by a rule written before measuring. No shipped change without the owner.
  - accept: `docs/npu-summaries.md` with findings, sources, any measurements and a recommendation
- [x] T8.3i Why llama.cpp on Vulkan stalls on the Adreno
  - deps: T8.3h
  - do: find why T8.3h's Vulkan run loaded Qwen 3.5 4B fully on the Adreno X2-85 and then produced nothing in 3 h 38 min (about one CPU core busy, no GPU use seen), and whether it is repairable. Reproduce with one short prompt and llama.cpp's full log in a probe outside the shipped crates; time prompt processing and generation separately; isolate the cause (Qwen 3.5's recurrent ops `GATED_DELTA_NET`/`SSM_CONV`/`SSM_SCAN` against a plain transformer GGUF already in the manifest, partial offload, llama.cpp's `GGML_VK_*` switches, the driver) and check upstream llama.cpp issues and fixes after the pinned 0.1.160. A fix that needs a newer `llama-cpp-sys-2` or a driver update is proposed to the owner, not done. If a working setup is found, re-run T8.3h's sum-eval rule.
  - accept: `docs/npu-summaries.md` Vulkan diagnosis section with the cause (or the narrowed candidates and their evidence) and a repair verdict; if repaired, T8.3h's rule re-run and logged
- [x] T8.3j Summarise during the meeting, as a setting
  - deps: T8.3f
  - do: let the user choose in the app to summarise the meeting chunk by chunk while it is recorded, so only the last chunk and the reduce step run at hang-up (Surface CPU 4B, T8.3f: about 290 s at hang-up per 30-minute meeting instead of 553 s). Design first: map steps run on the live transcript, but the final pass replaces it and summaries cite segment ids, so decide whether live map outputs are re-cited, redone or reused only when the final transcript of that chunk is unchanged (measure on GATE-12 and AMI replays); map work runs at below-normal priority and never delays live transcription (as speaker work, T6.13d); a device-local setting with a toggle in Settings; off when on battery if speaker work is. Split into a/b (runtime, UI) if larger than the 3.3 limit.
  - accept: runtime tests for the reuse rule and priority; ui tests for the setting; on the Surface, a replay with the setting on logs time from hang-up to summary and the largest live transcript lag, against the same replay with it off
  - design: `docs/live-summaries.md`; the owner chose option A: live partials re-cited onto the final transcript after the final pass, redone when they cannot be. Split into T8.3ja, T8.3jb, T8.3jc; T8.3j is done when all three are.
- [x] T8.3ja Quality check for two-track chunk summaries (measured 2026-11-03: the rule failed by one decision; the owner accepted it the same day because the mode is an opt-in setting, off by default, see `docs/live-summaries.md`)
  - deps: T8.3f
  - do: `cargo xtask sum-eval` mode that maps each AMI chunk with two-track speaker labels (one headset "Me", the other three merged as "Them"), re-cites and renames each partial as option A of `docs/live-summaries.md` does (the re-cite rule lives in shipped code, used by both), and reduces with the real names; a partial that cannot be fully re-cited is mapped again normally and counted. Run it beside the normal mode on the dev split with the 4B on the CPU.
  - accept: on dev meetings ES2003b, ES2011b, IS1008b, TS3004b, TS3006b (before measuring), every meeting valid and ROUGE-Lsum and item coverage each no more than 0.02 below the normal run of the same build (rule fixed in `docs/live-summaries.md` before measuring); results and redo counts logged. A fail stops T8.3jb and T8.3jc and is reported to the owner.
- [x] T8.3jb Map chunks during the meeting
  - deps: T8.3jb1, T8.3jb2
  - do: the runtime half of option A: a below-normal-priority map worker beside the speaker thread that maps each finished 600 s window of the Live rows (model loaded per chunk), a device-local `live-summaries/<meeting>` record deleted with the other live records, re-citing and redo in `host::after_meeting` before the last chunk and the reduce, the device-local setting `settings/live-summaries` (off by default, off on battery when speaker work is) and a CLI flag; thread priority of llama.cpp's workers decided by measurement (docs/live-summaries.md).
  - accept: runtime tests for re-citing, redo, the record's deletion and that live transcription never waits for the worker; on the Surface, a replay with the setting on and off logs time from hang-up to summary and the largest live transcript lag
  - split (2026-11-03, over the 3.3 size limit): T8.3jb is done when T8.3jb1 and T8.3jb2 are.
- [x] T8.3jb1 Map worker, record, re-cite at hang-up, setting and CLI flag
  - deps: T8.3ja
  - do: the runtime and CLI part of T8.3jb's `do`: a map worker thread (`live::summary_thread`, below-normal priority like the speaker thread) fed by the session thread with the Live rows of each window once every transcribed track has settled past its end (a worker's `decoded_through`, read before its events, and the echo filter's held mic finals); the `live-summaries/<meeting>` record written at the session end (model, window length, partials by window), deleted by `store_summary`'s transaction and by meeting deletion; at summary time the local path re-cites each recorded partial (`summary::recite`) onto the transcript being summarised and reuses it for its window, redoing a partial that cannot be re-cited; the device-local setting `settings/live-summaries` with `--live-summaries on|off` and `--save-live-summaries` in the CLI, honoured only for local summaries; off on battery when speaker work is (`after-on-battery`). LLM threads during the meeting from one rule (`live_summary_threads`), fixed by T8.3jb2.
  - accept: `cargo test -p earshot-runtime live_summar`, `cargo test -p earshot-runtime summary`, `cargo test -p earshot-runtime delete`, `cargo test -p earshot-cli`, `cargo xtask ci`
- [x] T8.3jb2 Live summaries measured on the Surface
  - deps: T8.3jb1
  - do: on the Surface, replay a 30-minute two-track recording in real time with the 4B on the CPU, setting on and off; decide the LLM thread count during a meeting (and whether llama.cpp's own threads need lowering) by the largest live transcript lag; record the rule and numbers in `docs/live-summaries.md`.
  - accept: both replays logged (time from hang-up to stored summary, largest transcript lag per track, windows mapped live, reused, redone); `cargo xtask ci`
- [x] T8.3jc Live summaries setting in the app (closed by T7.4g)
  - deps: T8.3jb, T7.4g
  - do: built in T7.4g (Summaries settings page) and closed with it. A toggle for `settings/live-summaries` in the app, through new IPC commands with mocks. Off by default; its description says it shortens the wait at hang-up and may miss some points.
  - accept: ui tests for the toggle; `cargo test -p earshot-desktop`; screenshots in light and dark
- [x] T8.3l Qwen 3.5 2B as a faster CPU summary model
  - deps: T8.3f
  - do: Qwen 3.5 2B is the largest Qwen 3.5 Qualcomm publishes for Snapdragon, but only as GGUF for llama.cpp (no true NPU build); on our own llama.cpp it runs on the CPU with our grammar and 16K context, about twice as fast as the 4B. Add it to the manifest (`llm-eval`, Unsloth GGUF pinned like the 4B) and `sum-eval`'s candidates, then run it beside the 4B on T8.3ja's five dev meetings (normal chunked mode, CPU, 6 threads) and report validity, ROUGE-Lsum, item coverage and time. No shipping change without the owner.
  - accept (changed by the owner, before the run: 2-3 meetings each, hang-up times, action items judged by hand with reworded or combined items counting as caught): `cargo test -p xtask models`; `cargo xtask sum-eval --backend cpu --split dev --source reference --model qwen3.5-4b-q4_k_m,qwen3.5-2b-q4_k_m --chunk-seconds 600 --threads 6 --meeting IS1008b,TS3006a,ES2011c` (each model alone) -> `docs/summary-eval/actions-review-2026-11-03.md`
- [x] T8.4c Summaries on Vulkan against CUDA
  - deps: T8.3h
  - do: measure whether the GPU summaries run well on Vulkan on the RTX 5080, because Parakeet needs no GPU and a Vulkan summary would need no NVIDIA download. The rule is in `docs/vulkan-summaries.md`, written before measuring. No shipped change without the owner; the result decides T8.4a's scope (PLAN 7).
  - accept: `cargo xtask sum-eval --backend vulkan` and `--backend cuda` with the rule's setup; results and the verdict in `docs/vulkan-summaries.md`
- [x] T8.4d A long action list loses the whole summary
  - deps: T8.4c
  - do: found by T8.4c (2026-11-06): Qwen 3.5 9B on CUDA fails dev meeting ES2003b on every run: one 600 s map step's reply reaches its `prompt::reply_tokens` budget (2103 tokens) in the middle of a long action list, three times, and the meeting gets no summary. Earlier chunked runs of the 9B were 20 of 20 valid, on an older format key, so check whether a later prompt or format change caused it. Find whether production fails the same way on the real path, then fix the cause, chosen on dev data by a rule written before measuring: a budget that fits what a map step can honestly produce, or a map step that keeps what it has rather than losing the meeting. Never make a cut-off reply pass as complete.
  - accept: `cargo test -p earshot-llm`; `cargo test -p earshot-core prompt`; `cargo xtask sum-eval --backend cuda --split dev --source reference --model qwen3.5-9b-q4_k_m --chunk-seconds 600` valid on ES2003b and on every dev meeting; `cargo xtask accept m4`
- [x] T8.4aa Summaries on a discrete GPU through Vulkan
  - deps: T7.6, T8.4c, T8.4d
  - do: part (1) and (2) of T8.4a without the time limit. The runtime's `vulkan` feature builds llama.cpp on Vulkan. The device rule: a discrete GPU with room for the model (`model_settings::fits_graphics_card`), never by index, anything else on the CPU. llama.cpp is told the device through `llama_model_params` (`main_gpu`, no split), and it already lists discrete GPUs before integrated ones, so no device list is needed and Rust still calls no ggml function; a load whose evidence names an integrated GPU counts as refused. `summary_gpu`, `summary_gpu_planned`, `summary_gpu_here`, the model choice, PLAN 3.4 and the invariants follow. `cargo xtask build` sets the x64 Vulkan environment itself (TODO.md, T8.4c).
  - accept: `cargo test -p earshot-llm-local`; `cargo test -p earshot-runtime summary_model`; tests with fake hardware and fake logs that the desktop's list picks the RTX and never the integrated Radeon, that no discrete GPU or no room stays on the CPU; `cargo xtask build --app --features vulkan`; `--include-ignored` model test on the desktop naming the RTX in its evidence
- [x] T8.4ab A stalled GPU moves the summary to the CPU
  - deps: T8.4aa
  - do: the time limit of T8.4a part (1): no progress from the GPU for a limit moves the summary to the CPU, because Qwen 3.5 stalled for hours on the Surface's Adreno (T8.3i). The fallback is shown, never silent. llama.cpp cannot be interrupted inside a GPU call, so the stuck thread is left behind (T9.3 is the separate process, post-release): 3 minutes without progress, a judgement; the GPU stays off for the rest of that app session.
  - accept: a test with a fake stalled model that the summary ends on the CPU and says so; `cargo test -p earshot-runtime summary`
- [x] T8.4ac Vulkan in the shipped x64 build
  - deps: T8.4aa, T8.4ab
  - do: part (5) of T8.4a: `build --app` and `build --bundle` on x64 use `vulkan`; `ggml-vulkan.dll` joins `expected_files` and `LLAMA_DLLS`; `vulkan-1.dll` (about 1.7 MB) is bundled beside the exe, because the exe and `ggml-vulkan.dll` import it and the app otherwise does not start on a PC without it, pinned in a manifest with its licence file read first and its notices shipped; every dev meeting on the RTX 5080 through production, valid; the install folder and download sizes logged; a replay from the installed folder with nothing NVIDIA on PATH.
  - accept: `cargo xtask build --bundle`; `cargo xtask bundle-check`; the 20 dev meetings valid in `cargo xtask sum-eval --backend vulkan`
- [x] T8.4ada Summaries on CUDA from a second folder of DLLs
  - deps: T8.4ac
  - do: the summaries half of T8.4ad, to docs/cuda-install.md (agreed by the owner). Rule (the owner): the app always uses its own pinned copies of NVIDIA's files, never one found on the machine; it checks the machine only to decide whether to offer CUDA. `earshot_llm_local::load_from_folder` loads a folder's llama.cpp set by full path before any llama call; any failure is an error value and the app stays on the set beside the exe, and says so. Measured: a set with no code for the card passes the load and then ends the process on the first GPU call (exit 0xC0000409, the recording and final transcript survive), so a folder is first tried in a throwaway child of the exe (`llama_probe`), for future cards and the generations this project cannot test. `cargo xtask build --cuda-set` builds the combined set (RTX 2xxx to 5xxx) with the 13.4 toolkit. The replay mode gets `--llama-dir` and `--local-summary`, to prove it in the real app. Choosing the folder from a setting, and checksums, come with the download (T8.4ae).
  - accept: `cargo test -p earshot-llm-local dll_folder`; `cargo test -p earshot-desktop headless`; `cargo test -p earshot-desktop llama_probe`; `cargo test -p xtask --lib build_cuda_set`; `cargo xtask build --cuda-set`; the Vulkan app's replay ends on Vulkan with no `--llama-dir`, on CUDA with the set and cuBLAS, and on Vulkan with a note without cuBLAS, nothing NVIDIA on PATH, backend from the load's evidence; a set built without the desktop's generation does not crash the replay
- [x] T8.4ad One x64 install that can load CUDA
  - deps: T8.4ac, T8.4ada
  - do: what is left after T8.4ada is Parakeet on CUDA through ONNX Runtime's library, from the same exe. Parts of (3) and (4) of T8.4a, the design agreed with the owner in docs/cuda-install.md. Today a CUDA ggml imports cuBLAS at load time and a CUDA whisper is linked into the exe, so one exe cannot start without NVIDIA's files. Decided by the owner: the shipped app's Whisper never runs on CUDA, only on Vulkan or the CPU, so the exe holds no CUDA code; large-v3-turbo, the model for languages outside Parakeet v3's 25, runs on Vulkan at 0.047 of real time on the RTX 5080 against 0.023 on CUDA. CUDA serves Parakeet through ONNX Runtime's library and summaries through llama.cpp. And one combined `ggml-cuda.dll` for every supported generation, RTX 2xxx (and GTX 16xx) to 5xxx, not one per generation; GTX 10xx and older cannot use CUDA 13 and are not offered it. That library is built by this project, so it cannot come from NVIDIA: T8.4ae needs a host the owner owns for it. Measure the options for loading llama.cpp's two sets of DLLs (a second folder loaded at start, delay-loading, ggml's dynamic backends) and their size. Replace the Blackwell-only `CMAKE_CUDA_ARCHITECTURES=120` for what ships, chosen from `cargo xtask doctor`'s capabilities and logged with its size cost.
  - accept: a test that the exe has no static import of a CUDA DLL; the same install runs a replay on Vulkan without NVIDIA's files and on CUDA with them, backend from the evidence logs
- [x] T8.4ae CUDA files as a verified download
  - deps: T8.4ad
  - do: part (3) of T8.4a: the two parts as manifest entries with size, SHA-256 and licence, under `developer.download.nvidia.com` plus any redirect host once observed, only through `earshot_runtime::download`, each file's SHA-256 checked before load. Check archive contents, redirects and Range support before pinning. A CUDA request without the files is `RuntimeError::Unavailable`.
  - accept: a test that a library failing its SHA-256 is not loaded; `cargo test -p earshot-runtime catalogue`; `cargo test -p xtask models`
- [x] T8.4af The CUDA switch in Settings
  - deps: T8.4ae
  - do: part (3) of T8.4a: Settings, Models offers CUDA only when `earshot_hardware::read()` reports an NVIDIA GPU its libraries support (compute capability, docs/cuda-libraries.md), with sizes and NVIDIA's terms before the download, off by default, two separate removable parts, switching back is one setting; the first start that chooses the second part runs the model check, and its copy states only what was measured on a real meeting replay.
  - accept: tests with a fake hardware read that the offer appears only with a suitable GPU; `cmd /c pnpm -r test`; the owner's window check. A start on an x64 PC with no NVIDIA card moved to T9.1, for a tester after the source is public
- [x] T8.4a One x64 build: Vulkan by default, CUDA as an opt-in download
  - deps: T7.6, T8.4c, T8.4aa, T8.4ab, T8.4ac, T8.4ad, T8.4ae, T8.4af
  - do: split into T8.4aa to T8.4af (2026-11-06); this entry is their joint acceptance. D23. The shipped x64 app runs summaries through llama.cpp on Vulkan and needs no NVIDIA file to start. (1) Vulkan: `ggml-vulkan.dll` in the install; a device rule that picks a discrete GPU with room for the model (`model_settings::fits_graphics_card`), never by index, because on the desktop `Vulkan0` is the integrated Radeon; anything else stays on the CPU until measured; a time limit that moves a summary to the CPU if the GPU makes no progress, because Qwen 3.5 stalled for hours on the Surface's Adreno (T8.3i), with the fallback shown, never silent. ARM64 stays on the CPU for summaries. (2) The summary model rule (`summary_gpu_planned`, `summary_gpu_here`) and the backend order follow this; PLAN 3.4 and the invariants that name CUDA first change with it. (3) The switch: Settings, Models offers CUDA only when `earshot_hardware::read()` reports an NVIDIA GPU that NVIDIA's libraries support, with the size shown and NVIDIA's terms before the download; off by default. It has two separate parts, each with its own size and each removable: summaries on CUDA (cuBLAS: 426 MB download, about 548 MB on disk), and, optional on top, transcription on CUDA (cuDNN and the ONNX Runtime CUDA library: about 1.6 GB more to download, about 800 MB more on disk), which starts the model check at the first start that chooses CUDA (a check when the files arrive cannot pick CUDA, because the choice applies at the next start, so that one is dropped). The second part's copy says plainly that transcription already keeps up on the CPU and that accuracy is the same, and that its gain is less CPU work, with the CPU saved measured here on a real meeting replay, because voice detection and speaker work stay on the CPU either way; the copy states only what was measured. The files are manifest entries with size, SHA-256 and licence, under `developer.download.nvidia.com` plus any redirect host once observed, downloaded only through `earshot_runtime::download`, with each file's SHA-256 checked before it is loaded, because they live in a folder the user can write to. Check archive contents, redirects and Range support before pinning. The exe has no static import of a CUDA DLL. A CUDA request without the files is the existing `RuntimeError::Unavailable` path, never a silent CPU run reported as CUDA, and switching back to Vulkan is one setting. (4) Replace the Blackwell-only `CMAKE_CUDA_ARCHITECTURES=120` for whatever ships, chosen from `cargo xtask doctor`'s capabilities and logged with its size cost; the local dev setting may stay. (5) Before Vulkan is the default: every dev meeting on the RTX 5080 through production, valid, with T8.4d fixed. Log the measured size of the install folder and the download. Building the CUDA part needs the toolkit, so it builds on the desktop; `cargo xtask ci` still never needs CUDA.
  - accept: `cargo xtask build --bundle`; a test that the exe has no static import of a CUDA DLL; tests with a fake hardware read that the Vulkan device rule picks the discrete GPU and the CUDA offer appears only with a suitable NVIDIA GPU; a test that a library failing its SHA-256 is not loaded; a test that a stalled GPU moves the summary to the CPU and says so; on the desktop, the installed folder runs a replay with summaries on Vulkan on the RTX with nothing NVIDIA on PATH, and after the in-app switch runs one on CUDA (backend from the evidence logs); started by the owner on an x64 PC with no NVIDIA card, it offers no download and reports the backend it used.
- [x] T8.4b ARM64 CUDA spike
  - deps: T8.3
  - do: D23. CUDA 13.4 reportedly adds Windows on Arm64 as a developer preview, for NVIDIA's RTX Spark (N1X) machines. From NVIDIA's own release notes and a probe outside the shipped crates: does whisper.cpp build for ARM64 CUDA, does llama.cpp, does ONNX Runtime publish an ARM64 CUDA library, what driver does it need, and may the DLLs be redistributed? Installing the toolkit is the owner's step. No such machine exists here, so nothing can be run on CUDA: the report says what is proven by a build and what is not.
  - accept: `docs/arm64-cuda.md` with the findings, sources and a recommendation; the owner decides from it whether the ARM64 install offers the CUDA download on an NVIDIA machine (never shipped as working without a run on real hardware).
- [x] T8.4e QNN libraries in the ARM64 install
  - deps: T8.4b
  - do: the owner approved bundling (section 7, 2026-11-08). The ARM64 installer carries the QNN libraries the manifest pins for the plugin (`onnxruntime_providers_qnn.dll`, `QnnHtp.dll`, `QnnHtpPrepare.dll`, `QnnSystem.dll`, the V73 and V81 stubs and skels) plus `Qualcomm_LICENSE.pdf`, the plugin's `LICENSE` and ONNX Runtime's `ThirdPartyNotices.txt`, copied from the verified extraction, named in the install's NOTICE. `earshot_onnx::qnn` finds them beside the exe (a package root parameter with the same SHA-256 pins), never in the models folder for a shipped build. `bundle.rs` refuses QNN files and imports only on x64; on ARM64 `expected_files` lists exactly these. The `.cat` files stay out until the Surface shows they are needed.
  - accept: `cargo test -p xtask bundle`; `cargo test -p earshot-onnx qnn`; `cargo xtask build --target aarch64-pc-windows-msvc --bundle`
- [x] T8.4f NPU in the ARM64 app's backend order
  - deps: T8.4e
  - do: a `qnn` feature on `earshot-desktop` forwarding to `earshot-runtime/qnn`, added by `build::shipped_features` for an ARM64 app, so the order is QNN > CPU for Parakeet there. A machine whose NPU or QNN library refuses to load starts and runs on the CPU with the reason shown once (fake loader test). No CUDA on ARM64: the owner has not chosen it after T8.4b.
  - accept: `cargo test -p earshot-runtime backends`; `cargo test -p xtask build`; `cargo xtask build --target aarch64-pc-windows-msvc --app`
- [x] T8.4g Hosted NPU conversions
  - deps: T8.4f
  - do: the owner chose (section 7, 2026-11-08) Earshot's GitHub release "Earshot Dependencies" (tag `cuda-set-llama-0.1.160-2`, never marked latest). Publish the `parakeet-qnn/parakeet-tdt-0.6b-v2` and `parakeet-tdt-110m` conversions that GATE-03a measured (checked against their `conversion.json` and docs/qnn.md before upload) as zips with a NOTICE carrying the CC-BY-4.0 credit; manifest entries with size, SHA-256 and licence, extracted through `archive::extract_verified` into `parakeet-qnn/<model>`, offered on the Models page of an ARM64 app only, and kept by `backends::verify_qnn_conversion`. Uploading is the owner's go-ahead.
  - accept: `cargo test -p xtask models`; `cargo test -p earshot-runtime catalogue`; `cargo xtask fetch-models --set qnn-models --verify` after upload
- [x] T8.4h ARM64 bundle-check
  - deps: T8.4e
  - do: `cargo xtask bundle-check --target <triple>` checks the build it names, and refuses (rather than checking the host's build) when the target cannot run on this host, saying to run it on an ARM64 machine. `--installer <path>` checks a given installer file, so the Surface checks the exact ARM64 installer the desktop built and the owner signs on the desktop (the owner, 2026-11-08: the signing key stays on the desktop); it prints the installer's SHA-256 so the signed file can be matched to the checked one. On ARM64 it also checks the QNN files are present and pinned.
  - accept: `cargo test -p xtask bundle`
- [x] T8.4 ARM64 package
  - deps: T8.3, T8.3e, T8.4b, T8.4e, T8.4f, T8.4g, T8.4h
  - do: joint acceptance of T8.4e to T8.4h: built on the desktop, judged on the Surface. Backend auto-select QNN > CPU; a machine without that hardware starts and runs on what it has.
  - accept: `cargo xtask build --target aarch64-pc-windows-msvc --bundle` on the desktop; `cargo xtask bundle-check` on the Surface
- [x] GATE-03 Accelerated backends on hardware
  - deps: T8.4
  - do: open the gate: on the Surface run `cargo xtask accept m2 --backend cpu`, `--backend qnn`, and `bench` for both; on the desktop run `bench --backend cuda` if not already in `acceptance/results`. Also (from T6.23d): a short take on the Surface's built-in mic with the T6.23d script (`record-raw`), Windows Voice Clarity / Studio Effects noted on or off, scored counts only as the desktop takes were (script words under "Me", far-end text under "Me", summary action owners, low-frequency level); informational, not a pass criterion. Pass criteria (changed by the owner 2026-11-08, before `accept m2` finished: the old "live chunk p95 under 3 s" was written for 3 s chunks, and since T6.13b a stretch decodes only once it closes, so the desktop's RTX 5080 also measures 25.3 s): QNN WER within 1.5 points of CPU; the live bench keeps up (`kept_up`, no audio dropped); live latency p50 under 3 s; the Surface's largest live latency within 1 s of the desktop's on the same bundled meeting (`acceptance/results/bench-x86_64-pc-windows-msvc-cuda.json`; "no worse" changed to "within 1 s" by the owner on 2026-11-09 after the result, 25.41 s against 25.25 s, because the NPU decodes at half the card's speed and a 30 s stretch takes about 0.24 s longer); `accept m2 --backend cpu` passes. Words on screen within seconds is T12.4's speech-to-screen measurement.

### M9 - v1 release

- [x] T9.1c Installer bundles
  - deps: T7.6 (brought ahead of GATE-03, T8.4 and T8.4a by the owner)
  - do: part of T9.1. `cargo xtask build --bundle [--target <t>]`: NSIS for each architecture (the MSI was dropped by T9.1p), from the same build rules as `build::app`, unsigned. Tauri's bundler tools (NSIS, its plugin) come only through a manifest entry with licence and SHA-256, never its own unpinned download. The install holds what the app needs to start and run a replay on the CPU, and no CUDA or QNN library: the CUDA download waits on T8.4a's host decision and the QNN libraries on T8.4's terms question, so until then the x64 installer is the default build, not D23's CUDA build. Models stay in-app downloads.
  - accept: `cargo xtask build --bundle` for both targets; `cargo test -p xtask build`; the x64 NSIS install in a temp folder runs the hidden replay and uninstalls clean. ARM64 is compile-only from an x64 host.
- [x] T9.1d First-run wizard
  - deps: T7.4 (brought ahead with T9.1c)
  - do: part of T9.1. D21 / docs/ui-design.md "First run": four skippable steps with a progress bar (Obsidian vault, models, summaries, audio check and your voice), reusing the Settings pages' IPC and rules rather than copying them; shown once, the "done" state a device-local record through the registry; download now / later / never with the size shown. It plays no sound and records nothing until the user presses the button.
  - accept: `cmd /c pnpm -r test`; `cargo test -p earshot-desktop`; `cargo test -p earshot-runtime first_run`; screenshots in light and dark
- [x] T9.1e Local crash logs
  - deps: T7.5 (brought ahead with T9.1c)
  - do: part of T9.1. A panic in the app writes a local file (version, thread, source location, backtrace, time) and never the panic's message, which could hold meeting text; nothing is uploaded; old files are removed by count. Settings, General shows where they are. Reviewer required (privacy).
  - accept: `cargo test -p earshot-desktop crash` (a panic whose message holds a marker leaves a file without the marker)
- [x] T9.1f A clean install can start a meeting
  - deps: T9.1d, T7.4a
  - do: found by T9.1c (2026-11-05): an installed app with an empty models folder cannot start a meeting. It needs ONNX Runtime and the speech helpers (Silero, segmentation, CAM++) plus a transcription model; `Catalogue` lists only transcription and summary models, and the refusal tells the user to run `cargo xtask fetch-models`. The app downloads what a meeting needs through `earshot_runtime::download` (same hosts, allowlist and checks), including safe archive extraction with xtask's rules (stored and deflate, no writing past the declared size, no duplicates or escaping paths), offers it in the wizard's models step and on the Models page with its size, and refuses a meeting with fixed user copy that points there, never a developer command. CUDA and QNN libraries stay out (T8.4a, T8.4). Models already allowed in-app stay as they are; Parakeet's pinned copies are offered to everyone (the owner, 2026-11-07, section 7).
  - accept: `cargo test -p earshot-runtime catalogue`, `download`; `cargo test -p earshot-desktop`; `cmd /c pnpm -r test`; from an empty models folder with a local server, the helpers and ONNX Runtime download, verify and load; a meeting refused for a missing helper shows no `cargo` text.
- [x] T9.1i Faint echo of the far end is not a voice on the mic
  - deps: T6.23a
  - do: faint echo can appear as a quiet extra mic voice, which the text rule (runs of 3+ words) cannot see. A third rule in `drop_echo_clusters`, defined after seeing a private recording: a mic voice is dropped and its words removed when it is at least 10 dB below the loudest mic voice, at least 80% of its words fall inside a cleaning stretch while the far end speaks (+-1 s), and it holds at most 10% of the mic's words. Each number is a judgement. All three are required: a real person in the room talks when the far end does not, and is never removed for being quiet.
  - accept: `cargo test -p earshot-runtime post_echo_quiet`; `cargo xtask ci`
- [x] T9.1h Your name in summaries and exports
  - deps: T7.3, T7.4h
  - do: found by the owner: a summary called them "the speaker", and owners read "Me", though their voice was recognised; they should be able to give their name, so a shared action list says it. "Your name" in Settings, Speakers and the first-run wizard (`VoiceSection`) renames the user's person, in every meeting. With a name, every summary request carries one rule: the user appears as that name, refer to them by it, never "the speaker"; with none, every prompt stays byte-identical. Two different people with one first name in a meeting are told apart in summaries, exports and the meeting's transcript, one rule per meeting (the owner): first name and last initial when that separates them ("Casey P", "Casey M"), else a number ("Casey (1)", "Casey (2)"). The stored names do not change. Live labels stay You and Them.
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p earshot-runtime view`; `cargo test -p earshot-runtime my_name`; `cargo test -p earshot-desktop speaker_settings`; `cmd /c pnpm -r test`
- [x] T9.1g Meeting screen: Stop always in view, follow the newest line
  - deps: T7.2
  - do: found by the owner in real meetings: Stop goes out of view as the transcript grows. Pin Stop meeting in the status line at the top, beside the clock, so it stays visible however long the transcript gets. The live transcript follows the newest line as it lands; scrolling up stops following and shows "Jump to latest", which goes back to following. Newest stays at the bottom (the owner agreed, to be revisited if following does not feel right); the saved transcript is always in time order. docs/ui-design.md "During the meeting" changes with it.
  - accept: `cmd /c pnpm -r test` with tests that Stop stays in the pinned header, that a new line follows only while at the end, and that "Jump to latest" appears after scrolling up and resumes following; judged by screenshot of the Vite preview in light and dark with a long transcript
- [x] T9.1l Start recording while the previous meeting finishes
  - deps: T8.4af
  - do: agreed by the owner. Today `MeetingDriver` treats recording, the final pass and the summary as one job, so a second Start is `busy` until the summary ends. The owner wants recording to start at once and accepts a gap in the live transcript. Split recording from a background queue that runs the final pass and summary, one meeting recording at a time. Open capture and the FLAC writers immediately and hold the new meeting's live model load and decoding until the older job ends. This inverts today's start order (AGENTS.md: rows only after live transcription is attached, speaker models before the FLAC writers), so those invariants change with it, and a failed model load during a hold must not cost the recording. Queue the audio during the hold and decode it in order afterwards. Older jobs and held work run first-in, first-out, so only one summary model runs at a time and live summary windows wait too. Hold speaker work only while a final pass runs, if that proves workable, to reduce hitting `SPEAKER_MAX_BUFFER` (180 s); passing it stops live speaker work and leaves more for the final pass, losing no audio. Add meeting ids to events where the UI needs them. Show the older meeting as waiting to finish. Banner at the top of the meeting's transcript: holding, "**Audio is recording.** Processing your previous meeting. Transcription will begin soon."; after the older job ends, "Catching up on the transcript" until the backlog clears, then none; past `STRETCH_MAX_BACKLOG` (30 min), "**Audio is recording.** The live transcript has stopped for this meeting. The full transcript will be ready after the meeting ends." The same hold replaces the CUDA start's give-way: a meeting started during the CUDA start records at once, holds its model until the decision lands, and keeps CUDA. Remove what that makes unused in the same change: `cuda::Choice`'s give-way, `host::order_for_meeting`'s branch, its banner, and `start_check_giving_way` if nothing else needs it. Check whether the hold also removes `busy` refusals from a running check or voice sample. Not in this task: transcription and a summary at the same time, which can follow if measurements on the desktop and Surface support it. Catch-up speed on the Surface NPU and a CPU fallback, and how long a final pass holds a new meeting, are unmeasured: report both if measured here. Independent reviewer (AGENTS.md 3.5: pipeline state).
  - accept: `cargo test -p earshot-runtime meeting_queue`, and `--features onnx-cuda --test meeting_queue_cuda` for "ends on CUDA" (a replay starts a second meeting while a slow fake summary runs and the older summary finishes; the second meeting's FLAC holds all its audio from the start and its live words match a run with no hold; passing the backlog cap; a failed model load during a hold; quitting during a hold keeps the audio and leaves the meeting `Unprocessed`; a meeting started during a hung CUDA trial records from the start and ends on CUDA); `cargo test -p earshot-desktop meeting_queue`; `cmd /c pnpm -r test` (the three banner states)
- [x] T9.1ma Measure words from speech to the screen
  - deps: T9.1l
  - do: the `live_lag` example and the baseline with no previews (split from T9.1m at about 5,000 lines).
  - accept: `cargo test -p earshot-runtime --example live_lag`
- [x] T9.1mb Decode the unfinished stretch for display only
  - deps: T9.1ma
  - do: `live::preview`, one session of its own on one track, below normal priority.
  - accept: `cargo test -p earshot-runtime live_preview`
- [x] T9.1mc Show provisional words in the meeting's transcript
  - deps: T9.1mb
  - do: `MeetingEvent::Provisional`, quieter italic text, replaced when settled text lands.
  - accept: `cmd /c pnpm -r test`
- [x] T9.1m Show provisional words while someone is talking
  - deps: T9.1l
  - status (2026-11-07): built and merged (T9.1ma, T9.1mb, T9.1mc). Measured on an idle host (docs/live-previews.md). Asked of the owner: the p95 miss, the memory cost and a Settings switch. Previews are off on the Surface until GATE-03a and a measurement.
  - do: agreed by the owner; before T9.1j and T9.1k, so it ships in the release and the public repo starts with it. With live stretches, words appear only when a stretch's decode returns, and lag of up to about 30 s is normal on continuous speech. Parakeet 0.6B v2 decodes at rtf 0.02 on the Surface NPU and 0.03 on the desktop CPU with 4 threads (docs/asr-models.md), so most of that wait should be for the stretch to end, not for decoding; that is not yet measured. Thresholds and "starting values" here are judgements until measured.
    **First:** a timing report: for each stretch and preview, the time from audio captured to words reaching the UI, counts and times only, never text. Record a baseline on a real-time replay with no preview, on an idle host, and report it to the owner. Rule: carry on building previews unless the median lag is already under 3 s.
    **Then:** on each live track, decode the unfinished stretch about once a second (starting value) and show it as provisional text, styled differently from settled text; the real stretch decode replaces it when it returns. Decode only the last 10-15 s of the unfinished stretch (starting value). Words must not flicker: a new window starts mid-speech, so its first word is cut and words change between previews. Join windows and hold back unstable words with the existing `LocalAgreement` rules rather than a new one, and measure how often shown words change. Parakeet can stay on blank for the rest of a piece depending on the exact input (`Transcriber::may_skip_speech`), so a preview must not make shown words vanish and return: test that case.
    Check first whether a preview can share a track's Parakeet session; a separate session per track may cost about 2.4 GB each (not measured), against a 7.7 GB peak measured on the CPU and 16 GB on the Surface. Report the memory cost before choosing. For whisper, sampled retries are history-dependent, so a preview must not touch the state or RNG of the transcriber making real decodes. Real stretch decodes always go first. A preview never queues behind another: when busy, skip it or lengthen the interval.
    Previews are display-only: never in `StretchRecord`, the store, stored echo cleaning, summaries, exports or final-pass reuse; stretch decoding, its state and its results do not change. They arrive by a new event, separate from `MeetingEvent::Landed`, which stays the only event for settled text. Provisional text is never logged or stored. Off on battery by default, as `after-on-battery` is for speaker work, until the Surface numbers support otherwise. Test the mic track against the echo rule: far-end words may appear briefly before the filter removes them; choose the mic behaviour from that test and log the choice (for example, hold mic previews for the AEC delay, or leave them off). Start with Parakeet on the NPU, CUDA and a CPU that can spare the time; decide from measurement whether Whisper on Vulkan and CPU fallback get previews, off where a track would fall behind. Any turn-off is visible in the report and affects neither recording nor stored transcripts. Previews stay off while T9.1l holds transcription and during catch-up after a hold.
    **Measure and report**, on an idle host, never beside ci or a build: desktop median and 95th percentile from speech to on-screen words with and without previews, previews skipped, how often shown words changed, CPU and memory, on the backend the printed evidence names; any setup cost of many short decodes (CAM++ on CUDA paid about 110 ms per new input length; check Parakeet). Surface: the same plus battery draw, once GATE-03a has cleared real meetings on the NPU; if this finishes first, record that run as a follow-up and do not claim the result. Target: words on screen within about 3 s on the desktop, a judgement outside `acceptance/thresholds.toml`; if missed, report it to the owner rather than widen it. Independent reviewer, asked to confirm no preview path reaches a stored result. Not in this task: shorter stretches, the chunker as default, or a native streaming model; if previews fall short, those are a separate decision for the owner.
  - accept: `cargo test -p earshot-runtime live_preview` (with previews on and off a replay stores identical words and identical stretch records; real decodes go ahead of previews under a slow fake transcriber, and previews are skipped rather than queued; provisional text never reaches the store, an export or a summary request; a blank-staying decode does not make shown words vanish and return); `cargo test -p earshot-runtime post_stretches` unchanged; `cargo xtask accept m2` -> pass with no threshold changed; `cmd /c pnpm -r test` (provisional text appearing, replaced by settled text, removed when a preview had words the settled text lacks); `cargo xtask ci`
- [x] T9.1j No build machine's paths in shipped files
  - deps: T9.1c, T9.1m
  - do: anything published holds the path it was built from: the CUDA set's DLLs held `C:\Users\<name>\.cargo\...` in 300+ places through llama.cpp's assert messages until they were rebuilt with `CARGO_HOME` at a neutral path. The installers' exe and DLLs were not checked and are expected to have the same paths, from C and C++ asserts and Rust panic locations. Release builds use neutral paths, and `bundle-check` fails on the build user's name or home folder in any installed file.
  - accept: `cargo xtask bundle-check` includes the scan, with a test that the scan catches a planted path
- [x] T9.1ka Public guard
  - deps: T0.2
  - do: `cargo xtask public-guard`, a ci step and a tracked `.githooks/pre-push`. It enforces in a repo whose HEAD tracks `PUBLIC_REPO` or whose first commit's subject is the public start. It refuses: any line of a names list held outside the repo (`%USERPROFILE%\.earshot-private\names.txt`, `EARSHOT_PRIVATE_NAMES`) in HEAD's bytes (UTF-8, UTF-16, PNG and FLAC text), paths, commit text or history diffs; a commit email not ending in `@users.noreply.github.com`; a recording or database off the allowlist; `LOG.md`, `gates/`, `fixtures/private/` at any depth, at the tip or in history. Otherwise it prints one line and passes. A missing list warns, and `--require-list` (the hook) fails. Output names list line numbers and hashes a path that holds a name. A root `LICENSE` copyright line may hold a name, never an email. The pre-push hook refuses any ref that is not HEAD and fails closed.
  - accept: `cargo test -p xtask public_guard`; `cargo xtask public-guard --require-list` in the cleaned copy
- [x] T9.1kb Private recording settings outside the repo
  - deps: T9.1ka
  - do: `gate12_diag`, `model_eval`, `t613 quality` and the ignored tests `final_pass_pieces_gate12_take2` and `echo_nl_gate10_sets_the_scenario_soft_clip` read the windows, script lines, key words, script words, residue stretches, an expected delay range and folder names read off private recordings from `%USERPROFILE%\.earshot-private\gate12.toml` (`asr_eval/gate12_private.rs`, shared by path). A missing file prints a visible skip of the private part (`model_eval` still runs an explicit `--gate12-dir`) and fails the ignored tests with a clear message; a damaged file is an error. No assertion's strength or bound changes value.
  - accept: `cargo build -p earshot-runtime --example gate12_diag --example model_eval`; `cargo test -p xtask t613`
- [x] T9.1kc Private details out of main's code and tests
  - deps: T9.1kb
  - do: The owner. Dated recording folders move to `gate12.toml`; the real audio interface and webcam in test data become invented devices; the test-script sentence in five test files becomes invented text. No assertion changes.
  - accept: `cargo xtask accept m1`; the crates' tests; `cargo xtask ci`
- [x] T9.1k Earshot's source goes public from a fresh start
  - deps: T8.4ad, T8.4ae, T8.4af, T9.1j, T9.1m, T9.1ka, T9.1kb
  - status: public. The owner read the files and approved the push. The CUDA set is in this repo's releases, and the owner has deleted `earshot-releases`.
  - do: decided by the owner. One repo: public `pitslug/Earshot`, starting from one cleaned commit, and the working folder is that repo. The earlier history is an archive that is no longer worked in. Recordings, logs and personal details stay out, as files ignored by git on the owner's machine. Automated code tests stay in, on invented names and public audio. A pre-push hook and a `cargo xtask ci` step check for them.
  - accept: the guard's tests catch a planted email, name and recording; `git log --format='%ae %ce' | sort -u` in the new repo shows only the noreply address; the owner's approval of the diff recorded
- [x] T9.1n Choose light or dark in Settings
  - deps: T9.1g
  - do: The owner, changing D21, which offered no choice. Settings, General gets Theme: System (the default), Light or Dark, applied at once and from the first paint at start, with no flash of the wrong theme. The page's text saying the theme follows Windows goes. The native title bar follows the choice if Tauri can set it. A device preference, no meeting content.
  - accept: `cmd /c pnpm -r test` (each choice picks its theme whatever the system says; System follows a live change; the choice survives a reload); screenshots of the preview in both themes
- [x] T9.1o Meeting actions: buttons at the top, delete from the list
  - deps: T9.1g
  - do: The owner. Re-summarise (Summarise when there is none) and Re-transcribe become buttons at the top of the meeting beside Export to Obsidian, and the More menu goes. "Recording no longer kept" moves onto the disabled Re-transcribe. The Summary tab's own stale-summary prompt and "Switch to previous summary" stay. Delete moves to the meeting list: a trash can that slides out on hover and also shows on keyboard focus, and a right-click menu reachable from the keyboard. Both open the existing confirm dialog, because deletion cannot be undone; any meeting can be deleted without opening it; a meeting recording or processing is still refused.
  - accept: `cmd /c pnpm -r test` (no existing assertion about what delete does or refuses is lost, only where it starts); screenshots of the preview in both themes
- [x] T9.1md Use less memory for transcription sessions (committed and referenced in code as T9.1ma, an id used twice)
  - deps: T9.1m
  - do: The owner asked why a preview costs 2.2 GiB. Each Parakeet 0.6B session holds its own copy of a 2.27 GiB weights file, so a call holds three. Measure on an idle host, with `live_lag --memory` and the on and off runs of docs/live-previews.md: (a) one copy of the encoder weights shared between a meeting's sessions, through ONNX Runtime's external initializers; (b) previews on Parakeet 110M (about 0.42 GiB of encoder), with on-screen lag and how often settled text differs. Keep what wins, stored words identical on and off, and record the numbers in docs/live-previews.md.
  - status (2026-11-07): one copy of the weights, prepacking off, CPU sessions only: peak 9.39 -> 5.81 GiB with previews, 7.15 -> 5.52 GiB without. 110M rejected (about three times the corrected words). CUDA unchanged.
  - accept: `cargo run --release -p earshot-runtime --example live_lag -- . --memory --out target\live-lag\memory.json`; `cargo test --release -p earshot-onnx --test shared_weights -- --include-ignored`; `cargo test -p earshot-runtime live_preview`; `cargo xtask accept m2`
- [x] T9.1me A Settings switch for provisional words, and the memory limit (committed and referenced in code as T9.1mb, an id used twice)
  - deps: T9.1md
  - do: The owner. A device-local on or off in Settings, on by default where the machine qualifies; turning it on below the limit is allowed, with a note about memory. A recording never suffers for it: it is off on battery and when behind, as now. Decided by the owner: 12 GiB on the CPU, CUDA unchanged at 20 GiB and a 14 GiB card; the NPU stays off until GATE-03a. Also decided: a card under 12 GiB transcribes on the CPU, because a call needs about 9.1 GiB of the card and the same model gives the same words on the CPU.
  - accept: `cargo test -p earshot-runtime live_preview`; `cargo test -p earshot-runtime preview`; `cargo test -p earshot-desktop cuda`; `cmd /c pnpm -r test`; the preview checked in light and dark
- [x] T9.1mf Saved people on the Speakers page (committed and referenced in code as T9.1mc, an id used twice)
  - deps: T7.4h
  - do: The owner. Settings, Speakers lists everyone Earshot knows other than the user, saying whose voice is saved, with rename (every meeting, the existing `renamePerson`) and forget voice (asks first, the existing `forgetPersonVoices`).
  - accept: `cmd /c pnpm -r test`; screenshots of the preview in light and dark
- [x] T9.1p One per-user installer per architecture
  - deps: T9.1c
  - do: The owner, after the pinned bundler's `installMode: "both"` turned out to preselect everyone for admins, prompt UAC even on "just me" and ignore `/D=`: NSIS with `installMode: "currentUser"`, no "everyone" option, no Program Files, no MSI. `xtask::bundle`, `bundle-check` and `xtask/bundler-tools.toml` lose WiX. A second Windows user installs Earshot and downloads models for themselves.
  - accept: `cargo test -p xtask --lib bundle`; `cargo xtask build --bundle`; `cargo xtask bundle-check`
- [x] T9.1q Small fixes from the follow-up list
  - deps: T8.4af
  - do: a summary model chosen by name and not downloaded skips the summary with a banner rather than refusing the meeting; extraction renames a folder aside and takes a cancel; small CUDA and clean-install fixes from TODO.md.
  - accept: `cargo test -p earshot-runtime`; `cargo test -p earshot-desktop`; `cargo test -p xtask --lib manifest`
- [x] T9.2a README and privacy page for the x64 early release
  - deps: T9.1k
  - do: The owner. `README.md` (install, usage, backends, credits) and `PRIVACY.md` (PLAN 3.9 as the code does it) ahead of T9.1 and T9.1b, whose update and ARM64 parts T9.2 still adds. Each claim checked against the code by a separate agent.
  - accept: `cargo xtask public-guard`; the fact-check's findings resolved
- [x] T9.1s One short app identifier
  - deps: T9.1p
  - do: the owner: `io.github.pitslug.earshot` is odd to see in a user's folder. The identifier becomes `earshot`, so the browser engine's files sit in `%LOCALAPPDATA%\Earshot\EBWebView` with everything else. The uninstaller's "Delete the application data" box then deletes the whole folder, meetings and models included, where it used to delete only the browser files; a silent or unticked uninstall still keeps them. Android needs a dotted id and will set its own at T10.
  - accept: `cargo xtask build --bundle`; `cargo xtask bundle-check`
- [x] T9.1t Opt out of Windows Error Reporting
  - deps: T9.1e
  - do: the owner: a crash report can hold part of a process's memory, so transcript text, and depending on Windows settings go to Microsoft. The app excludes its own exe, for the current user, at start (`WerAddExcludedApplication`), before any model or child process runs. The child modes run the same exe, so they are covered by name. All `unsafe` in `wer_win.rs`. A failure never stops the app. The real effect is checked from the owner's own shell: an agent shell's registry is MSIX-virtualised. The owner's first run (2026-11-07): the report and event 1001 stop, but `LocalDumps` (apparently switched on by the NVIDIA App's HKLM entries) still writes a local dump, so the app deletes `<exe>.<number>.dmp` from `%LOCALAPPDATA%\CrashDumps` at each start, and PRIVACY.md says so (the owner).
  - accept: `cargo test -p earshot-desktop wer`; `cargo test -p earshot-desktop crash`; owner's check that a forced crash queues no report
- [x] T9.1v The app's exe is earshot.exe
  - deps: T9.1p
  - do: the owner: people see the exe's name, so the app's binary is `earshot` and the developer tool, which ships to nobody, becomes `earshot-cli`. The package stays `earshot-desktop`, so `cargo test -p` and the guards are unchanged. No `-x64` in the name: the installer's file name carries the architecture.
  - accept: `cargo test -p xtask bundle`; `cargo test -p earshot-cli`; `cargo xtask build --bundle`; `cargo xtask bundle-check`
- [x] T9.1u Remove a person from Speakers
  - deps: T7.4h
  - do: the owner: people's names and voiceprints stay when a meeting is deleted, and only removing them on the Speakers page deletes them. Meeting deletion stops forgetting voiceprints (replaces T6.26's rule; a meeting's own centroid record still goes with it). Remove erases the identity: the person rows (name cleared, tombstoned), voiceprints and speaker centroids in one transaction, then a checkpoint; every speaker row linked to them in a kept meeting goes back to "Speaker N", unlinked. The user is refused. Text already written (summaries, notes, exported files) is not rewritten, and the dialog says so.
  - accept: `cargo test -p earshot-store delete`; `cargo test -p earshot-runtime edit`; `cmd /c pnpm --filter @earshot/ui test`
- [x] T9.1w The Visual C++ runtime beside the exe
  - deps: T9.1p
  - do: the owner: a PC without Microsoft's redistributable must still run the app, with no admin prompt or download at install. `xtask::bundle` copies `VC_RUNTIME_DLLS` (the four DLLs the exe, llama.cpp, ggml and ONNX Runtime import) from the building Visual Studio's newest `VC\Redist\MSVC` folder into every install, and refuses a binary importing a runtime library the list lacks. A run on a PC without the runtime is still to come, and the release notes say so.
  - accept: `cargo test -p xtask bundle`; `cargo xtask build --bundle`; `cargo xtask bundle-check`; `cargo xtask ci`
- [x] T9.1r x64 early release
  - deps: T9.1c, T9.1d, T9.2a, T9.1s, T9.1t, T9.1u, T9.1v, T9.1w
  - do: The owner: x64 first, as a 0.x normal GitHub release (a pre-release would leave the CUDA set marked Latest), ahead of T8.4, GATE-03, T9.1b and GATE-04, which stay v1's. No in-app update, so the notes say each version is installed by hand. Concluded with the owner: NSIS's stub, NVIDIA (section 7). The owner's listening note and their word that the app was usable on real calls are in the gate notes, and the owner approved publishing (2026-11-07). GATE-01a and GATE-02 stay open for what moved to after release. The release notes say what was not tried: a PC with no NVIDIA card or no Visual C++ runtime, a device switch during a call, and the notification buttons. Those three are checked after release.
  - accept: `cargo xtask build --bundle`; `cargo xtask bundle-check`; the owner's approval of the release recorded
- [x] T9.1 Installers and first run
  - deps: GATE-03, T8.4a, T9.1c, T9.1d, T9.1e, T9.1f, T9.1g, T9.1h, T9.1i, T9.1j, T9.1k, T9.1l, T9.1m, T9.1n, T9.1o, T9.1p
  - note: outstanding from T8.4a and T8.4af: a tester starts the app on an x64 PC with no NVIDIA card: it offers no download and reports the backend it used. Until then the release notes say so.
  - do: one per-user NSIS installer per architecture (T9.1p), first-run wizard (vault, models, LLM endpoint, "Me" enrolment), local-only crash logs. No separate CUDA install (D23). Installers stay small: models are offered in-app (first-run wizard and settings) with size shown and download now / later / never, through the checksum-pinned downloader (decided by the owner); the switch to CUDA is in Settings, Models (T8.4a). Wizard per D21 / docs/ui-design.md "First run": four skippable steps (vault, models, summaries, audio check and your voice); the models step is T7.4's Models page (hardware listing, recommendation, 3 transcription choices).
  - accept: `cargo xtask build --bundle` for both targets
- [x] T9.1ba In-app update: the code
  - deps: T9.1p
  - do: T9.1b's items 1 to 5 through tests and review, with no key yet and nothing published.
  - accept: `cargo test -p earshot-update`; `cargo test -p earshot-runtime update`; `cargo test -p earshot-desktop update`; `cmd /c pnpm -r test`; `cargo xtask ci`
- [x] T9.1b In-app update
  - deps: T9.1ba (moved up by the owner, 2026-11-08: T9.1 waits on Surface hardware)
  - do: D24. First stop and ask the owner for the release host (section 7): no host is added without their OK. (1) A signed version file and the installer on that one host; the host is the app's only update destination, fixed at build time, https only, with its own allowlist in `earshot-net` and every request in the audit log; a check sends nothing but the request for that file. No Tauri updater plugin and no HTTP client outside `earshot-net` (the ci `net` step stays as it is); update the note in `apps/desktop/src-tauri/Cargo.toml`. (2) Download through the `earshot_runtime::download` rules (streaming, `.part` resume, size and SHA-256, then rename). (3) Before anything runs, verify the version file and the installer's hash against a public key built into the app, and refuse a version not newer than the running one. The private key stays offline and is never committed (ground rule 9); a `cargo xtask` command signs a release given the key's path, and the docs say what to do if the key is lost. (4) Settings: a "Check for updates" button showing the version, size and result, and a device-local "check automatically" setting, off by default, written through the local-state registry. (5) Installing goes through `activity::WorkGate`, so it is refused during a meeting or a meeting job, the model check, the voice sample, the audio check and the benchmark; it asks before restarting, exits through `shell::quit_app` and starts the verified installer through `earshot_proc`. A failed or refused update leaves the installed app and the user's data untouched. Failures reach the UI as fixed copy. Reviewer required (network code and privacy, AGENTS 3.5).
  - accept: `cargo test -p earshot-net`; `cargo test -p earshot-runtime update` (tests against a local server: a good release passes; a bad signature, a changed installer, an older or equal version and a redirect to another host are each refused with nothing run; no request with automatic checks off and no button press; the audit log shows only the update host); `cargo test -p earshot-desktop`; `cmd /c pnpm -r test`; `cargo xtask ci`; by hand with the owner, on x64 and ARM64, an installed build updates itself to a newer one and the meetings and settings are intact afterwards.
- [x] T9.2 Docs
  - deps: T9.1, T9.1b
  - do: `README.md` (install, usage, backends), `PRIVACY.md` (invariants from 3.9).
  - accept: `cargo xtask ci`
- [ ] T9.1a Low-end laptop check
  - deps: T6.13, T6.10c2, T6.10d
  - do: a target, not a must: on a 4-core laptop without a discrete GPU (for example a 10th-gen Intel), the transcript keeps up within the 30-minute rule and the recording never drops audio. Measured on an older laptop with a real meeting or a `--realtime --timeline` replay: capture lag, transcript lag, model and backend chosen, CPU load; part of the decision on releasing more broadly at GATE-04.
  - accept: the measurement logged; findings become tasks
- [ ] GATE-04 v1 sign-off
  - deps: T9.2

### Post-release v1 updates

Closed: work done after the x64 early release, before M12 existed. New ideas go in M12.

- [x] T9.3 GPU summaries in a separate process
  - deps: T8.4ab
  - do: T8.4ab leaves a stalled GPU's thread behind, which holds its graphics memory until a restart, and a driver crash still takes the whole app down. Run GPU summaries in a child process (as the model check does, `earshot_proc::KillOnClose`), so a stall kills the child and frees the memory and a driver crash costs only that summary. Covers progress, cancelling, the handover of the transcript and live summaries; content stays on the machine and never enters a log or command line.
  - accept: tests with a fake child that a stall or a crash moves the summary to the CPU and says so; a stalled child is gone after the kill
- [x] T9.7 Search across meetings
  - deps: T7.3
  - do: the owner (2026-11-08): a box above the meeting list that finds meetings by title, transcript and summary text, on this computer only, and opens the one picked. No new table or migration.
  - accept: `cargo test -p earshot-runtime search`; `cargo test -p earshot-desktop search`; `cmd /c pnpm --filter @earshot/ui test`; screenshot light and dark
- [x] T9.8 Copy the summary
  - deps: T7.3
  - do: the owner: a button on a meeting that copies its summary as plain Markdown.
  - accept: `cmd /c pnpm --filter @earshot/ui test`; screenshot light and dark
- [x] T9.9 Cancel on "Finishing the meeting"
  - deps: T7.3
  - do: the owner: a Cancel on that view stops the final pass and the summary. The audio and the meeting stay, marked as not processed, and can be re-transcribed.
  - accept: `cargo test -p earshot-desktop meeting`; `cmd /c pnpm --filter @earshot/ui test`; screenshot light and dark
- [x] T9.10 Show when an update is available
  - deps: T9.1ba
  - do: the owner: when a check finds an update, a mark beside Settings says so, not only the General page.
  - accept: `cmd /c pnpm --filter @earshot/ui test`; screenshot light and dark
- [x] T9.11 Clear the crash report exclusion on uninstall
  - deps: T9.1t
  - do: the owner: the uninstaller removes the exe's name from the user's Windows Error Reporting exclusion list, except on an update. `bundle-check` proves it under its registry guard.
  - accept: `cargo test -p xtask bundle`; `cargo xtask build --bundle`; `cargo xtask bundle-check`
- [x] T9.12 Mark a voice as yourself in an in-room meeting
  - deps: T7.4h
  - do: the owner: in an in-room meeting with no saved voice, the user can say a voice is theirs, instead of naming themselves creating a second person. Reviewer required: it changes who counts as the user.
  - accept: `cargo test -p earshot-runtime edit`; `cargo test -p earshot-desktop detail`; `cmd /c pnpm --filter @earshot/ui test`; screenshot light and dark
- [x] T9.16 Keep recording through idle sleep and report any gap
  - deps: T7.5
  - do: the owner: real meetings lost audio when Windows put an idle computer into Modern Standby during a recording: capture stopped, the gap was stored as silence and nothing was said. Hold a Windows power request for the life of capture, and report a gap in the mic track during the meeting and after it. Reviewer required: new unsafe FFI in `wasapi.rs`.
  - accept: `cargo test -p earshot-runtime --lib live_recording_gap`; `cargo test -p earshot-capture-wasapi keep_awake`; `cargo test -p earshot-desktop --bin earshot meeting_events`; a real run past the Windows idle timeout on the Surface with no standby
- [x] T9.5a Folders: model and migration
  - deps: T9.1
  - do: docs/folders-design.md "Data model": `Folder` (synced, `syncable_row!`), repo trait and contract cases, the `folder` table in `SYNCED_TABLES`, `meeting.folder_id BLOB` (no foreign key, `Option<EntityId>` with `serde(default)`) in schema version 2 by an appended migration with its `APPLIED` pin, cleared by core `delete_meeting`; trybuild `never_sync_envelope.stderr` regenerated. A committed version-1 database migrates. Reviewer required.
  - accept: `cargo test -p earshot-store folder`; `cargo test -p earshot-core model`
- [x] T9.5b Folders: runtime
  - deps: T9.5a
  - do: `earshot_runtime::folders`, the one owner of folder reads and edits: create, rename, move (refused while recording), delete keeping meetings, delete with meetings (each through `Services::delete_meeting`, no lock held across the loop, closing transaction re-selects members, then `flush_erasures`; failures leave the folder with what is left and return counts), resolve (a dangling id reads as no folder). Case-insensitive unique live names checked in the transaction; `EditError` cases that never quote a name. Tests in docs/folders-design.md "Tests to write".
  - accept: `cargo test -p earshot-runtime folders`
- [x] T9.5c Folders: UI
  - deps: T9.5b
  - do: IPC commands and bindings; the Folders group, the filter (opens on All meetings), Move to folder in the row menu, Rename and Delete with the two-choice dialog (keeping the meetings is the default) in `AppShell`; the start-up screen for a database saved by a newer Earshot. Screens checked light and dark.
  - accept: `cargo test -p earshot-desktop folders`; `cmd /c pnpm -r test`
- [x] T9.5 Folders in the side panel
  - deps: T9.5a, T9.5b, T9.5c
  - do: joint acceptance; design in docs/folders-design.md (the owner, 2026-11-08).
  - accept: `cargo xtask ci`
### M12 - After v1 (ideas)

New ideas and features, after the v1 release or beside it, with their own ids (the owner, 2026-11-08). A task moved here keeps its old id in brackets, because commits and code comments name it.

- [ ] T12.1 Offer to stop when the call ends (was T9.4)
  - deps: T6.14a, T6.15a
  - status (2026-11-07): the code is merged (`call_end`, `RenderSessionPoller`, `MeetingEvent::CallEndOffer`). The owner changed the text below: a notification that stays on screen with Stop recording and Keep recording buttons, and a stop 3 minutes after the offer if nobody answers (the plan's 60 s was too likely to cut a live call, and asking alone leaves a recording running with nobody there). Left: the hang-up measurement and the real Teams and Zoom calls. The 30 s and 3 minutes are judgements until measured.
  - do: stop the recording once the meeting is over. First measure, with `teams-probe` extended, what a known call app's render sessions do at hang-up: during a test call Teams held active sessions and with no call none, but the moment of hang-up, being on hold, and Zoom are unmeasured. Then, in call mode only: when the call app's active session ends and the far end stays quiet, show a banner "The call seems to have ended" with Stop recording and Keep recording, plus a notification that stays on screen with the same buttons when the window is not focused, whose text is fixed copy; nobody answering for 3 minutes stops the recording and says so. Never stop on silence alone (hold, breaks, waiting for someone to join), and never without the chance to keep recording: a wrong stop loses part of a meeting. In-room meetings still stop by hand.
  - accept: the hang-up measurement logged; `cargo test -p earshot-runtime` with replayed session changes (ended then quiet stops after the wait; Keep recording cancels; silence with the session still active never stops; in-room never stops); `cmd /c pnpm -r test`; tried by the owner on real Teams and Zoom calls

- [ ] T12.2 Record the output the call app plays on (was T6.15)
  - deps: T6.14, T6.14a, T6.15a
  - status (2026-11-07): the code is merged, opt-in as `--loopback-process follow-call` (`call_output`, `Track::move_output`); the app stays on `DefaultDevice`. Left: real calls including a non-default output, a Bluetooth headset and a ringer on a second output, and Zoom, which `teams_probe call` does not look at.
  - do: per-app capture is out for Teams: T6.15a measured a Communications stream as silent under process loopback and the call's process delivering exact zeros (private notes). Instead, with no output chosen on the Audio page, record the output holding a known call app's active session (`list_render_sessions` already walks every output), so a call on a headset is recorded while Windows defaults to the speakers, and follow it during the meeting with a two-poll rule so a passing sound does not move it. An output chosen on the Audio page always wins. `auto` stays off as an app default. Other call apps (Zoom) may still suit process loopback: measure with `teams-probe` before offering it.
  - accept: `cargo test -p earshot-capture-wasapi`; device tests under GATE-01a on a Teams call on a non-default output

- [x] T12.3 Provisional words for each track (was T9.14)
  - deps: T9.1m
  - do: the owner, 2026-11-08: in a call, previews run on the far end only, so a solo test shows none and the user's own words settle only at a pause. Replace the on/off setting with Off / Other people only / Everyone (device-local, an older stored on/off reads as Other people only; in the room both read as the mic). Everyone runs a second preview session on the mic track in a call, under the same refusals per backend, and falls back to the other people with a note when this computer cannot run both (memory, card, slowness); measure the second session with `live_lag --memory` before choosing its limit. The General page says what each choice shows.
  - accept: `cargo test -p earshot-runtime preview`; `cargo test -p earshot-desktop --bin earshot preview`; `cmd /c pnpm -r test`

- [x] T12.3a Your own provisional words on the CPU when transcription is on CUDA (rule failed by 5 ms on point 4; accepted by the owner 2026-11-09, docs/live-previews.md)
  - deps: T12.3
  - do: the owner, 2026-11-08: when transcription runs on CUDA and the card cannot take the mic track's preview session (`previews_refused_second`), Everyone runs that session on the CPU with shared weights (T9.1md) instead of falling back to other people only, within the CPU memory limit (`PREVIEW_BOTH_MIN_MEMORY`). Automatic, no new setting; the General page note says where it runs. Write the rule for "the main transcript does not slow" before measuring and commit it, then measure on the desktop with `live_lag --memory`.
  - accept: `cargo test -p earshot-runtime preview`; `cargo test -p earshot-desktop --bin earshot preview`; the rule and measurements in docs/live-previews.md

- [x] T12.3b One CUDA session for a track's transcription and previews (spike)
  - deps: T12.3
  - status (2026-11-11): rule committed first (ca9f310); evaluation-only prototype merged (one encoder shared through a Mutex, no unsafe; ort rc.15 does not help), reviewed clear; measured on an idle desktop, all five points pass in both runs (lock wait at most 43 ms, card 0.59 GiB over alone, about 3.1 GiB less than a separate preview session). Under the rule a replay follow-up comes next (T12.3d); no shipped change.
  - do: the owner, 2026-11-08: ONNX Runtime's CUDA provider gives every session its own copy of the weights on the card. Find whether one session can serve both: first whether a newer `ort` lets `Session::run` take `&self`, else scope a minimal unsafe wrapper over the C API's `Run` (reviewer required). Write the rule (no preview delays a real decode by more than a stated bound) before measuring; measure card memory and how long real decodes wait. The result decides default on, default off or a setting (the owner). No shipped change without the owner.
  - accept: the rule, findings and numbers in docs/live-previews.md

- [ ] T12.3d Shared CUDA encoder on whole meetings (follow-up to T12.3b)
  - deps: T12.3b
  - do: T12.3b's shared encoder passed its rule on contention runs (docs/live-previews.md). Measure it on whole replays the way T12.3c was (bundled and continuous, off / on / everyone, stored words, main rtf and catch-up, on-screen lag, private and card memory), with a rule written and committed before measuring, on an idle desktop. Evaluation only: the result goes to the owner, who chooses default on (perhaps lowering `PREVIEW_MIN_CARD_MEMORY`), a setting or default off.
  - accept: the rule and measurements in docs/live-previews.md

- [x] T12.3c Provisional words on the CPU when the graphics card is too small (rule failed point 4 by 30 ms; accepted by the owner 2026-11-11)
  - deps: T12.3a
  - status (2026-11-11): measured on an idle desktop (docs/live-previews.md, T12.3c Result): points 1, 2, 3 and 5 pass; point 4 fails (far-end on-screen median 3.03 s for Other people only and 3.53 s for Everyone on bundled audio, against 3 s). Under the rule the first session beside a small card is refused again; the tooling stays. Open for the owner: accept the 30 ms miss (as T12.3a's 5 ms was), and the 12 vs 20 GiB limit.
  - decision (2026-11-11, the owner): accept the miss ("still a hell of a lot better than 30-60 s"), as T12.3a's 5 ms was; the CPU session beside a small card is restored (revert of 7591d46 plus the T12.3c `preview_settings.rs`). The 12 vs 20 GiB limit stays as built.
  - do: the owner, 2026-11-09: with transcription on CUDA and a card under `PREVIEW_MIN_CARD_MEMORY`, `previews_refused` refuses every preview today. Run the preview sessions on the CPU with shared weights instead (the far end's, and under Everyone your own as in T12.3a), within the CPU memory limits (`PREVIEW_MIN_MEMORY`, `PREVIEW_BOTH_MIN_MEMORY`), and say so on the General page (replacing `NOTE_CARD`). Automatic, no new setting. Write the rule (where each session ran, identical stored words, the main transcript does not slow, on-screen lag, memory) and commit it before measuring; measure on the desktop with `live_lag --memory`, with the card limit simulated through the hardware the rule reads.
  - accept: `cargo test -p earshot-runtime preview`; `cargo test -p earshot-desktop --bin earshot preview`; the rule and measurements in docs/live-previews.md

- [x] T12.4 Provisional words on the NPU (was T9.13)
  - deps: T9.1m, T8.4
  - do: the owner, 2026-11-08: the NPU is the part made for this, and `previews_refused` turns previews off there only because nothing was measured. On the Surface, measure a meeting with previews on QNN against off with `live_lag` (speech-to-screen, the main transcript's lag and rtf, memory), on replays and a real meeting; write the rule for "the main transcript does not slow" before measuring and commit it. If it holds, the NPU leaves `previews_refused`; if not, the numbers and the reason go in docs/live-previews.md. Not while the owner is in a meeting. Added by the owner the same day: measure Everyone too (previews on both tracks, three NPU sessions with the main transcription), and whether the NPU sessions of a meeting can share one copy of the model (ONNX Runtime's EP context sharing between sessions of one process) and what that saves in memory and load time. The measurement tooling (`live_lag --backend qnn`, an evaluation-only way past the refusal) is built on the desktop first; production refusals change only after the measurement.
  - accept: `cargo test -p earshot-runtime preview`; the rule, the measurements and the decision in docs/live-previews.md

- [x] T12.5 Settings and first-run tidy (was T9.15) (design agreed 2026-11-10; built by T12.5a to T12.5c)
  - deps: T9.1d, T7.4f
  - do: the owner, 2026-11-08: Settings and the first-run wizard grew one task at a time and are confusing. A design session with the owner first: walk through every page and step as they are, then agree the structure (what goes where, what first run asks and what it leaves to defaults, wording) in docs/settings-design.md before any code. Then split into build tasks with their own accept commands. Behaviour and stored settings stay as they are unless the design changes them.
  - accept: docs/settings-design.md agreed by the owner; the build tasks added to this plan

- [x] T12.5a First run as designed
  - deps: T12.5
  - status (2026-11-10): built, reviewed and merged (8915be7); tests pass in the batch ci; the owner checked the screens on the preview and approved them.
  - do: docs/settings-design.md "First run": Summaries (with Claude and ChatGPT, this computer preselected, own endpoint under Other), Download (languages first, one button sized to the summary choice, the full list on request), About me (name, voice sample with a Skip; the sample waits for the speech helpers and says so, starting nothing by itself), Optional extras (Obsidian, audio check). Next saves the step. Finishing or skipping with no transcription model says meetings need one. Later and Never go: stop writing the record, and an old record still reads. The wizard stays a view of `AppShell` built from the Settings parts. Update docs/ui-design.md "First run" and AGENTS.md's first-run bullet.
  - accept: `cargo test -p earshot-runtime first_run`; `cargo test -p earshot-desktop bindings`; `cmd /c pnpm --filter @earshot/ui test`; screenshot of each step, light and dark, on `?firstRun=1` with `clean=1`, on both mock machines; the owner checks it on screen

- [x] T12.5b Settings pages as designed
  - deps: T12.5
  - status (2026-11-10): built, reviewed and merged (492e70c); tests pass in the batch ci, including the page-name guards; the owner checked the screens on the preview and approved them.
  - do: docs/settings-design.md "Settings": General, Audio, System, Transcription, Summaries, People, Notes and storage, in that order, opening on General. Models and Benchmark dissolve into System, Transcription and Summaries; Speakers, Obsidian and Storage move as listed. Stored settings and IPC do not change. `useModels` stays the only subscriber to `ModelJobEvent`, so it moves above the pages that share it. Text that names a page follows the move, on both sides of IPC (`speaker_settings::failure_copy`, `plain_text`, the wizard). Update docs/ui-design.md "Settings" and the AGENTS.md bullets that name these pages.
  - accept: `cargo test -p earshot-desktop --bin earshot`; `cmd /c pnpm --filter @earshot/ui test`; a test that no user-facing string names a page that no longer exists; screenshot of each page, light and dark, on both mock machines; the owner checks it on screen

- [x] T12.5c Settings wording and the save rule
  - deps: T12.5a, T12.5b
  - status (2026-11-10): merged (df0674f), reviewed clear; UI tests 389 pass; the old and new text listed for the owner in chat. Privacy wording unchanged; "Your own endpoint" -> "Your own server" proposed, not made.
  - do: docs/settings-design.md: plain words ("Many languages" for Multilingual, what "Keeps up" means, ONNX Runtime only in detail text), and the save rule: a choice applies at once, typed text needs Save, on every page. Privacy wording does not change without the owner.
  - accept: `cmd /c pnpm --filter @earshot/ui test`; `cargo test -p earshot-desktop --bin earshot`; the old and new text listed for the owner

- [ ] T12.6 Shared models for an "everyone" install (was T9.6)
  - deps: T9.1p
  - do: discussed with the owner, after release and only if a real user asks. `%ProgramData%\Earshot\models`, readable by all and writable only with admin, because the folder holds DLLs another user would run. Meetings, voiceprints and settings stay per user. Every file a shared load reads is checked, and its cost measured.
  - accept: written if it is taken up

- [ ] T12.7 Summaries on the Surface NPU, further (was T8.3k)
  - deps: T8.3h
  - do: explore NPU summaries beyond T8.3h's documentation review. Spike outside the shipped crates: choose a published QNN (Hexagon) build of an instruct model small enough for the Surface (Qualcomm AI Hub or ONNX Runtime GenAI, licence checked), add it and any runtime to `models/manifest.toml` (an evaluation set, pinned with SHA-256); find how to run ONNX Runtime GenAI with its telemetry off and prove it with an ETW check like `cargo xtask qnn-etw` before any meeting text goes in (AMI public text first); measure prompt and reply tokens/s and the real context limit; run `sum-eval` quality on the dev meetings with chunks resized to fit. Report what shipping would take (Rust binding, structured output or grammar, a Qwen 3.5-class NPU conversion, chunk redesign). No shipped change without the owner.
  - accept: `docs/npu-summaries.md` NPU section with measurements, the telemetry check output and a recommendation; sum-eval results logged

- [x] T12.8 How many voices are in the far-end recordings (was T6.9c)
  - deps: T6.9
  - status (2026-11-10): two voices, confirmed by the owner's listen (one asks, the other answers, matching where the harness put them); T6.9's first finding stands and no embedding-side task is added.
  - do: Confirm the voice count of the private test recording. If one voice, T6.9's first finding is wrong and an embedding-side fix task is added.
  - accept: the answer recorded

- [x] T12.9 Calibrate clustering with AMI dev meetings in the data (was T6.9d)
  - deps: T6.9b
  - decision (2026-11-10): the owner keeps 0.68: the gain is three short over-splits, 0.72 sits one step below a merge, and a split is easy to fix in the app while a merge is not. The accept commands are for a move and were not run.
  - status (2026-11-10): measured with the owner's agreement (`cluster_sweep`, docs/far-speakers/ami-dev-calibration.md, the rule committed first). The rule picks 0.72 on its count branch (C 4 -> 1, AMI DER 0.1014 -> 0.0992), one grid step below a merge cliff at 0.73; the lane's judgement is 0.70 if anything. The threshold stays 0.68. Left: the owner's decision; a move then runs this task's accept commands and changes the two pinning tests in the same commit.
  - do: proposed by T6.9b: clean LibriSpeech scenes favour a higher threshold while real meetings already under-count at 0.68. A new rule, committed before measuring, with AMI dev mixes (set `ami-dev`) and long scenes on disjoint audio. Only with the owner's agreement.
  - accept: `cargo test --release -p earshot-onnx diar_calibration -- --include-ignored`; `cargo xtask accept m3`; `cargo xtask accept m6`

- [x] T12.10 Qwen thinking on against off (spike, evaluation only) (was T6.20g)
  - deps: T6.20f, T9.5, T8.4
  - do: the owner, 2026-11-08: every summary evaluation ran Qwen 3.5 with thinking off (the empty `<think>` prefix); measure thinking on. An evaluation-only option in `sum_eval` (its own format key in both `examples/sum_eval/main.rs` and `xtask::sum_eval`) whose grammar allows a bounded `<think>` section before the summary JSON (`earshot_llm_local::grammar`, a token cap on the thinking so it cannot use the reply budget; production grammar byte-identical). Write the rule before measuring and commit it: thinking on earns a follow-up task only if the blinded meaning judge finds missed decisions plus missed action items fall by at least a margin fixed in the rule, with no rise in invalid summaries; time per meeting reported beside it. Run on the desktop GPU, dev split only, 9B and 4B, off against on. No production change without the owner; not while the owner is in a meeting.
  - accept: `cargo test -p earshot-llm-local grammar`; `cargo test -p xtask sum_eval`; the rule, tables (judged and timing) and a recommendation in docs/summary-eval.md

- [ ] T12.11 Short echo fragments under "Me" in the final transcript
  - deps: none
  - status (2026-11-10): round 2 (isolated echo islands after the speaker check) removed no word on leak-dev or rumble, so no code change (docs/echo-fragments.md). Open: the GATE-02 shape does not occur in the dev simulator, so a further rule needs new dev scenarios, defined before measuring.
  - do: found at GATE-02 (2026-11-08): in a call where the user said nothing, the final transcript kept one 2-word mic segment under "Me", 0.7 s long and wholly inside far-end speech. `asr::dedupe_echo` matches runs of 3+ words by design, and the speaker check names the only mic voice "Me". Find a rule that removes such fragments without costing near-end words in talk-over: write it before measuring, design and tune only on `asr_eval --leak-dev` (and `--rumble`), keep `ECHO_NEAR_RECALL_FLOOR_PERCENT` and `asr.echo_leak` holding. No change if no rule passes.
  - accept: `cargo test -p earshot-core asr`; `cargo test -p earshot-runtime post`; `cargo xtask accept m2`; the rule and leak-dev numbers in docs

- [ ] T12.12 NPU sessions from precompiled models (spike)
  - deps: T12.4
  - do: found by T12.4 (2026-11-09): each NPU session compiles the Parakeet conversion at load (29-31 s) and costs about 2.0 GiB of private memory, nothing shared, so a call on the 16 GB Surface (idling above 60% memory use) leaves about 1.1 GiB available. Measure ONNX Runtime's EP context models for the QNN plugin (`ep.context_enable`, a compiled context binary made once and loaded by later sessions): load time and private memory per session with `live_lag --memory`, whether the QDQ initializers then stay in memory, and whether `ep.share_ep_contexts` or the plugin's weight sharing then lets sessions share. Where a compiled binary would live, how it is pinned and verified like other model files, and what invalidates it (driver, plugin, model version) go in the findings. Evaluation only; no shipped change without the owner. If a session drops well below 2 GiB, T12.4's rule is run again.
  - accept: findings, `--memory` reports and a recommendation in docs/live-previews.md

- [x] T12.13 Keep a meeting's summary on this computer
  - deps: T7.4g
  - status (2026-11-10): merged, privacy review clear (3 advisories: copy fixed, the rest in TODO.md and LOG); runtime 835, desktop meeting 101, UI 405 pass on the merge; batch ci at the end of the night.
  - decision (2026-11-10, the owner): the switch is set only when a meeting starts, never later and never during it; a local meeting may later be switched to the remote writer (local -> remote, with a confirm, nothing sent until Re-summarise), never the other way, so no remote summary ever exists on a kept-local meeting.
  - do: the owner: a "keep this local" switch when a meeting starts (start only, by the decision below), because the summary writer is one global setting and some meetings should never leave the computer. It is stored per meeting, device-locally, and read wherever a summary starts (after the meeting and on re-summarise), with no network client built for such a meeting. Without a local summary model it skips the summary with a banner and never falls back to a remote writer; a record that cannot be read counts as local. Decide with the owner whether switching it off later is allowed, and how an existing remote summary reads when it is switched on. Reviewer required: privacy.
  - accept: `cargo test -p earshot-runtime summary`; `cargo test -p earshot-desktop --bin earshot meeting`; `cmd /c pnpm --filter @earshot/ui test`; a test that a kept-local meeting opens no connection with a remote writer set (audit log empty); screenshot light and dark

- [x] T12.14 Keep the Summary and Transcript tabs in view
  - deps: T7.3
  - do: the owner: the tabs scroll away with the page, and the page's scroll position carries across a tab change, so opening Summary from the end of a long transcript lands at the end of the summary. Pin the tabs under the sticky meeting header, open Summary at its top, and keep a citation scrolling to its row in the transcript.
  - accept: `cmd /c pnpm --filter @earshot/ui test`; screenshot of a long meeting, light and dark

- [ ] T12.15 Your vocabulary (split 2026-11-10 into T12.15a and T12.15b)
  - deps: T7.4b
  - do: the owner: a section in Settings for the user's own words (acronyms, company and site names), so they come out right. First, a device-local list, checked and bounded like the summary style, given to every summary request. Then a spike on the transcript, because Parakeet takes no prompt and Whisper does: compare a whisper prompt, hotword boosting in the transducer's search and a correction pass after decoding, measuring false corrections as well as gains on dev data. A correction pass must never change a word without keeping the original. No test or doc holds a word from the user's list. An empty list leaves every prompt byte-identical.
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p earshot-runtime summary`; `cmd /c pnpm --filter @earshot/ui test`; the spike's numbers and a recommendation in docs

- [x] T12.15a Your vocabulary in summaries
  - deps: T7.4b
  - status (2026-11-10): merged (lane abe5b9f), reviewed clear (3 advisories in TODO.md); core prompt 77, runtime summary 156, desktop 346, UI 401 pass in the lane; batch ci at the end of the night.
  - do: T12.15's first half: a device-local word list on its own Settings page, Dictionary (the owner, 2026-11-10), between Summaries and People, checked and bounded like the summary style, given to every summary request; an empty list leaves every prompt byte-identical; no test or doc holds a word from the user's list.
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p earshot-runtime summary`; `cmd /c pnpm --filter @earshot/ui test`

- [x] T12.15b Your vocabulary in the transcript (spike)
  - deps: T12.15a
  - status (2026-11-11): rule committed first (6d36616), measured on LibriSpeech dev-clean, merged; reviewed clear (4 advisories in TODO.md). Only the whisper prompt clears the bar (B-WER -20.1%, by 0.1 point); Parakeet boosting reaches -19.1% within the guards, correction passes fail the false-correction guards. No shipped change; the owner decides any follow-up (docs/vocabulary.md).
  - do: T12.15's second half. The words come from the Dictionary and the names of saved people (the owner, 2026-11-10: a saved Kam should win over Cam, Kat over Cat); people's names are used on this computer only and never added to a summary request beyond the meeting's own speakers. Compare a whisper prompt, hotword boosting in the transducer's search and a correction pass after decoding, measuring false corrections as well as gains on dev data, with the rule written before measuring. A correction pass never changes a word without keeping the original. No shipped change without the owner.
  - accept: the spike's numbers and a recommendation in docs

- [x] T12.15c Your words in the transcript: Parakeet's nudge on meeting speech (failed: not shipped)
  - deps: T12.15b
  - status (2026-11-11): rule committed first (a607b95), measured on 5 AMI dev meetings (859ddf8), merged, reviewed clear (3 advisories: one folded into the doc, two in TODO.md). Fails G2 and G3: WER 12.63% -> 16.89%, U-WER +4.3 points, listed B-WER unchanged (30.00%), G4 passes. The boost inserts short fillers and letters over breath, laughter and crosstalk, which read speech lacks. T12.15d is not started. Open for the owner: measure T12.15b's gated variant (never turns blank into a word; -17.4% on LibriSpeech) under its own rule on this data, or go to T12.15e.
  - decision (2026-11-11, the owner): ship T12.15b's chosen boost (open, bonus 2: B-WER -19.1% within every guard) in the next release; the 20% bar is waived. It is re-measured on meeting speech first.
  - do: evaluation, rule written and committed before measuring (docs/vocabulary.md, new section). The variant is fixed: boost open 2, nothing tuned. Data: AMI dev meeting speech only (headset audio of the `ami-dev` meetings, a new fixture set pinned in `fixtures/manifest.toml`, oracle segments as `asr.wer_ami_ihm` decodes them), never a test meeting. Lists: per meeting, the AMI named-entity annotations of that meeting if v1.6.2 holds them, else T12.15b's rare-word rule; filled with distractors to 100 words. Guards G1-G3 as in T12.15b against the Parakeet baseline on the same segments, plus G4: on non-speech (model_eval's `silence` corpus) the boost writes no listed word. Passes when every guard passes and listed-word B-WER is lower than the baseline's. Decode time recorded with the host load, deciding nothing.
  - accept: the numbers and the verdict in docs/vocabulary.md; `cargo test -p xtask fixtures`

- [ ] T12.15d Your words in the transcript: ship the nudge (blocked: T12.15c failed)
  - deps: T12.15c
  - do: only if T12.15c passes. Parakeet decodes with the boost (open, bonus 2) in every production decode: live stretches, the final pass, re-transcribe and provisional words, on every backend that runs Parakeet; whisper is unchanged (its prompt is not shipped). The list: the Dictionary's words, then the words of saved people's names (the owner, 2026-11-10: a saved Kam wins over Cam), deduplicated without case, at most `MAX_VOCABULARY_WORDS` (the measured size); title-case and lower-case paths as in T12.15b. People's names stay on this computer: they never join a summary request. Read once per live session at its start and by each final pass; the stretch record keeps the list's digest so a stretch decoded with another list is decoded again (record version bump, `post::reusable`); an empty list decodes byte-identically to today (pinned). The Dictionary page says transcripts now use the words (Parakeet only). The boost leaves T12.15b's evaluation-only scan tests' intent intact: `transcribe_biased` stays evaluation only, production goes through one shipped constructor.
  - accept: `cargo test -p earshot-onnx parakeet`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-runtime live`; `cmd /c pnpm --filter @earshot/ui test`; `cargo xtask accept m2` unchanged

- [ ] T12.15e Your words in the transcript: suggestions beside words
  - deps: T12.15d
  - do: the owner, 2026-11-11 (option b): after the nudge, a decoded word that sounds like a listed word is marked with the listed word as a suggestion, accepted with a click (an edit through `edit::edit_segment`); nothing changes until accepted. A quiet design first (T12.15b's careful matcher suggested about 70 times an hour on audiobook speech, a third wrong): measure how many remain after the nudge on T12.15c's data, then agree the design with the owner.
  - accept: agreed with the owner

- [x] T12.16 Edit a meeting's title, transcript and summary
  - deps: T7.3
  - status (2026-11-11): merged (T12.16a 08fec3e, T12.16b 0563027), reviewed clear (4 advisories: banner copy fixed at merge, rest in TODO.md); migration 3 adds the segment edit mark; desktop 354, runtime, store and UI pass on the merge (UI timeouts under lane load passed on re-run); batch ci at the end.
  - decision (2026-11-10, the owner): a re-transcribe keeps edited segments and says so in a warning; an edit to the transcript marks the summary out of date (as re-transcribe does), sending nothing by itself.
  - do: the owner: correct a title and fix words in the transcript or summary. Today only speakers can be edited. Edits go through `edit::write`; an edit to a transcript segment keeps its id and times so citations still work, and its word times stay honest about what changed. Decide with the owner what a re-transcribe does to edited text (keep, warn or replace) and whether a changed transcript marks the summary out of date. A replaced summary keeps today's undo. Exports follow the edited text.
  - accept: `cargo test -p earshot-runtime edit`; `cargo test -p earshot-desktop --bin earshot detail`; `cmd /c pnpm --filter @earshot/ui test`; screenshot light and dark

- [ ] T12.17 A few summary templates, shorter by default
  - deps: T7.4b
  - do: the owner, 2026-11-10: summaries are too long; decisions and actions are the priority, not minutiae. Offer a few templated prompts on Summaries, built on the summary style setting so every decision and action stays required. Not before the next local model is evaluated (Qwen 4 if it comes in the 9B and 4B sizes), because a local model's rules are judged on that model. Rule written before measuring, on the owner's meetings and AMI dev, with the 12-item cap (`SummaryLimits::max_items`) re-examined in the same evaluation.
  - accept: `cargo test -p earshot-core prompt`; `cargo test -p earshot-runtime summary`; `cmd /c pnpm --filter @earshot/ui test`; the rule and numbers in docs/summary-eval.md, with no private measurement committed

- [x] T12.18 Choose the Claude and Codex model and effort from a list
  - status (2026-11-11): merged (lane 4b26020), reviewed clear (5 advisories in TODO.md, three folded into T12.20); Codex effort left out: no real run could confirm `model_reasoning_effort` (the account's workspace was refused). llm, runtime summary 171, desktop 356, UI pass (load timeouts passed alone); batch ci at the end.
  - deps: T7.4ga, T7.4gc, T12.15a
  - do: the owner, 2026-11-10: typing a model name is not intuitive. On the Summaries page, Claude Code's model becomes a dropdown (Claude Code's default, Fable, Opus, Sonnet, Haiku: the CLI's aliases for the latest model, so the list does not go stale) with Other for a typed name, and an effort dropdown (Claude Code's default, Low, Medium, High, Extra high, Max), passed as `--effort` only when the installed CLI's `--help` lists it. Codex: a model dropdown with Codex's default and Other; effort only if a real run confirms `model_reasoning_effort` on the installed CLI, from its own capture. Both are choices, so they apply at once (save rule) and the Save model button goes. Effort is a fixed value from a list, never free text, on the command line; the stored values are device-local, an unknown stored value reads as the default and is reported. The first-run Summaries step uses the same parts.
  - accept: `cargo test -p earshot-llm claude_code`; `cargo test -p earshot-llm codex`; `cargo test -p earshot-runtime summary`; `cmd /c pnpm --filter @earshot/ui test`; screenshot light and dark

- [x] T12.19 The Speakers panel slides out, with Ignore
  - deps: T12.16
  - status (2026-11-11): merged (6580a95, review fixes 1ac5f8d), privacy-focused review clear (7 advisories: 4 fixed in the lane, 3 in TODO.md); runtime edit 54, post 106, desktop 358, UI 423 pass on the lane's merge with main; batch ci at the end.
  - do: the owner, 2026-11-10: the "Remember these voices for next time" tickbox for all speakers goes; each unnamed voice gets its own Save (name it, link it to that person and remember the voice, in one step, the per-person consent replacing the tickbox). A saved voice moves out of the needs-a-name list into a collapsed list of named voices in the same panel, where a wrong name or a wrong match can still be fixed or forgotten; a voice recognised in a later meeting is named there, counts as named and so never opens the panel, and is marked as recognised. "Who is who" becomes "Speakers", a panel that slides in from the right and pushes the transcript aside rather than covering it. Closed by default when no voice needs a name, open by itself when one does; a Speakers button in the meeting header toggles it, with a badge such as "2 need names". Each unnamed voice gets Ignore: an ignored voice no longer counts as needing a name (so it never opens the panel), keeps its "Speaker N" label in the transcript, summary and export, is never enrolled or matched as a person, and can be un-ignored from the panel. Ignore is a per-meeting user edit through `edit::write`, carried across a re-transcribe like other user edits (`identities::match_previous`), and `meeting_events::needs_name` stays the one rule, now excluding ignored voices. Text naming "Who is who" follows the rename.
  - accept: `cargo test -p earshot-runtime edit`; `cargo test -p earshot-desktop --bin earshot detail`; `cmd /c pnpm --filter @earshot/ui test`; screenshot of the panel open and closed, light and dark

- [x] T12.20 UI review follow-ups from TODO.md
  - status (2026-11-11): merged; all ten items fixed with failing-first tests (plus two from the T12.18 and T12.15a reviews), review blocked once (effort banner on a failed run) and fixed; UI 437, desktop 363, runtime summary 175, llm pass; batch ci at the end.
  - deps: T12.16, T12.13, T12.18
  - do: the owner, 2026-11-10: clear the UI advisories in TODO.md that need no decision: AppShell's missing Cancel for a meeting finishing behind a newer recording (`cancellingId`), `mockSummaries.ts` refusal messages pinned against Rust, NPU versions in the first-run `ModelChoices`, the wizard's savers stopping at the first refusal, the TypeScript page-name guard matching the Rust one's forms, PeoplePage's Enter racing a save in flight, and `SpeakerTimingSection` showing voice notices. Each closed line leaves TODO.md in the same commit; anything that turns out to need the owner stays with a note.
  - accept: `cmd /c pnpm --filter @earshot/ui test`; `cargo test -p earshot-desktop --bin earshot`

### M10 - v2 Android in-person recorder

- [ ] GATE-05 Android toolchain
  - deps: GATE-04
  - do: conditional like GATE-00: self-pass if `doctor` finds SDK, NDK, cargo-ndk and an attached device; otherwise list what to install.
- [ ] T10.1 Android build spike
  - deps: GATE-05
  - do: build core, whisper-rs and ort (`load-dynamic`, ORT Android AAR libraries bundled) for `aarch64-linux-android`; write findings to `docs/android-build.md`.
  - accept: `cargo xtask build --target aarch64-linux-android`
- [ ] T10.2 Android shell
  - deps: T10.1
  - do: `apps/android` Tauri 2 mobile using `packages/ui` with recorder-only routes: record, meeting list, detail, settings.
  - accept: `cargo xtask android build` produces an APK; `pnpm -r test`
- [ ] T10.3 Foreground service capture
  - deps: T10.2
  - do: Kotlin Tauri plugin: microphone foreground service, permissions (RECORD_AUDIO, POST_NOTIFICATIONS, FOREGROUND_SERVICE_MICROPHONE), partial wake lock, `AudioRecord` -> a direct JNI native method on the service class (same process as the Rust library; never Tauri plugin IPC/JSON for PCM) -> `earshot-capture-android`.
  - accept: `cargo test -p earshot-capture-android` (host-side buffer/JNI-free logic) and APK builds
- [ ] T10.4 In-person mode in runtime
  - deps: T10.3, T6.13
  - do: mic-only pipeline, no AEC, single-track diarisation, thermal/battery-aware model choice. Cleanup follows the T6.8 design (T6.13): during the meeting, best effort (speakers first, re-transcription only with spare capacity and never when the device is hot), never deferred more than 30 minutes after the end.
  - accept: `cargo test -p earshot-runtime in_person`
- [ ] T10.4a On-device summaries on the Pixel
  - deps: T10.4, T6.21b
  - do: D19. A spike first: Gemini Nano through AICore and the ML Kit GenAI APIs (Prompt API) on the Pixel's TPU from the Tauri Android app: availability on the Pixel 11, input limit, latency, battery; the existing split-and-combine planner sized to the input limit; output validated and repaired against `summary.v1`. Fallback when Gemini Nano is unavailable: the smallest T6.20 model that meets the thresholds, on the CPU via the T6.19 crate. Nothing leaves the phone unless the user opted in to a remote provider.
  - accept: on-device test on the Pixel recorded at GATE-06; `cargo test -p earshot-runtime summary`
- [ ] T10.5 Android export
  - deps: T10.4
  - do: Obsidian vault folder via SAF tree URI, share sheet fallback.
  - accept: `cargo test -p earshot-store export` and APK builds
- [ ] T10.6 M10 acceptance
  - deps: T10.5
  - do: `accept m10`: in-person pipeline on the AMI SDM fixture on the host (asr.wer_ami_sdm, dia.der_ami_sdm), APK build. Emulator tests run if available but do not gate.
  - accept: `cargo xtask accept m10`
- [ ] T10.7 Wear OS companion (remote control)
  - deps: T10.6
  - do: decided by the owner: a thin Wear OS app that controls the phone app; the phone does all capture, transcription and summarising. The watch can start and stop a recording, add a bookmark at the current meeting time (stored on the meeting as a bookmark row, shown in the summary and the Obsidian note; the same bookmark is reachable from the desktop with a hotkey), and shows status (recording or not, elapsed time, mic level, a warning when capture stops). Transport: only direct messages to a connected nearby phone (Wearable `MessageClient` / `ChannelClient` to a node reported nearby); never `DataClient` data items or any path that can sync through Google's cloud; when the phone is not nearby the watch says so and does nothing. Only control signals and status cross the link (start, stop, bookmark, elapsed time, levels, error codes); never transcript text, speaker or meeting names, or summaries; the watch stores nothing. Privacy check: a test that the watch-side message types carry no free text from the meeting, and a capture of the link traffic during a test recording.
  - accept: `cargo xtask accept m10` still passes; the watch app builds; an emulator pair test (watch + phone) starts, bookmarks and stops a replayed meeting, and the link-traffic check passes
- [ ] GATE-06 Pixel field test
  - deps: T10.6
- [ ] GATE-07 v2 sign-off
  - deps: GATE-06

### M11 - Future: sync container

- [ ] T11.1 Sync design
  - deps: GATE-07
  - do: `docs/sync.md`: entity coverage, HLC last-writer-wins + tombstones, `Person.merged_into` handling, cursor pull/push API, device auth (tokens; OIDC via a self-hosted provider optional), encryption options, voiceprint opt-in flow, how audio exclusion is enforced, and write/lock discipline between the sync client and the runtime on the shared SQLite database.
  - accept: document exists and covers every bullet above
- [ ] GATE-08 Approve sync design
  - deps: T11.1
- [ ] T11.2 Sync server
  - deps: GATE-08
  - do: `earshot-sync-server` (axum + SQLite), Dockerfile, `docker-compose.yml`, request size limits, rejects binary payloads other than bounded voiceprint vectors.
  - accept: `cargo test -p earshot-sync-server`
- [ ] T11.3 Sync client
  - deps: T11.2
  - do: client in runtime, background sync, voiceprint sync setting (default off), UI status.
  - accept: `cargo test -p earshot-runtime sync`
- [ ] T11.4 M11 acceptance
  - deps: T11.3
  - do: `accept m11`: server via Docker if available else the binary; two simulated clients; sync.convergence; sync.no_audio (type-level check plus a scan of every payload and server table for RIFF, fLaC and OggS signatures and oversized blobs).
  - accept: `cargo xtask accept m11`
- [ ] GATE-09 Self-hosted deployment
  - deps: T11.4
