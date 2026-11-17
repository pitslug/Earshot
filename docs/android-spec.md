# Earshot on Android: draft spec (v2, M10)

Status: draft, 2026-11-17. Nothing here is built. Building waits for the desktop and Surface hand tests of 0.3.11 (the owner). Decisions still open are marked **Open** with a recommended default; a refreshed M10 task list follows at the end and replaces PLAN.md's M10 entries only once the owner agrees (scope and task order are the owner's, AGENTS.md 6.2).

## 1. What changed since M10 was planned (October)

M10 assumed a standalone phone app: capture, whisper `small.en` on the CPU, diarisation, a local summary, Obsidian export. Since then:

- **Sync exists (M11).** The phone can join the vault with a pairing code like the Surface, so meetings recorded on the phone appear on the desktop and the desktop's work appears on the phone. Audio never syncs (docs/sync.md).
- **Summaries have an order of writers (T12.63).** The phone can hand its summary to the desktop or to the Unraid summary worker, the way the Surface already does, so a local summary model on the phone becomes optional.
- **Parakeet replaced whisper as the default transcriber**, and the measured model choice (the model check) decides per machine. The phone's plan named whisper.
- **Google now offers on-device speech recognition to apps** (ML Kit GenAI Speech Recognition, section 4), which may let the phone transcribe without shipping a model at all. The owner chose this as the plan (2026-11-17), behind a spike.

What carries over unchanged: the in-person pipeline (mic only, no echo canceller), capture in a foreground service with PCM over JNI, the Obsidian export through the storage access framework, and the Wear OS remote control.

## 2. The phone's role

**Open (decision A1): standalone, recorder with sync, or both.** Recommended: both, with sync optional. Without sync the phone is a complete recorder and transcriber, and summaries follow the order of writers with whatever this phone can reach; with sync its meetings go to the other computers and its summary goes to the first writer in the order that answers (the desktop, the Unraid worker), as on the Surface.

**Open (decision A2): in-person meetings only.** Recommended: yes. Android restricts recording another app's call audio [Likely; to confirm in the spike], so v2 records the room through the phone's microphone and nothing else. Calls stay a desktop and Surface job.

**Open (decision A3): Wear OS in the first Android release or after.** Recommended: after (T10.7 stays last). The watch adds a second app, its own pairing and a link-traffic check, for a remote control.

## 3. Architecture

```
Pixel (in person)
  Kotlin foreground service (microphone)
    AudioRecord 16 kHz mono PCM
      -> JNI (direct native method, same process; never Tauri IPC for PCM) -> earshot-capture-android
      -> ML Kit GenAI speech recognition (streaming, word timings)    [section 4]
  earshot-runtime (in-person pipeline): FLAC writer, voice activity, diarisation (CAM++ on the CPU),
    words to speakers, the store, sync, export
  Tauri 2 mobile shell with packages/ui: record, meeting list, detail, settings (recorder routes)
```

- The Rust side is the existing runtime built for `aarch64-linux-android`: core, store, sync, runtime (in-person mode), and ONNX Runtime for the voice activity and speaker models only. whisper.cpp and llama.cpp are not built for Android unless the spike sends transcription back to Parakeet (section 4.4), which needs only ONNX Runtime anyway.
- Transcription is the one part that is not Rust: ML Kit is a Kotlin API. The Kotlin service feeds it the same PCM it sends over JNI and passes its words (text plus start time) to the runtime over JNI as transcript events. A new adapter, `earshot-asr-mlkit` (host-side logic only, JNI-free and testable on the desktop), turns them into `asr::Word`s on the meeting's timeline.
- Everything the runtime stores and syncs is the same data model as on Windows, so a phone meeting is an ordinary meeting on the desktop.

## 4. Transcription: ML Kit GenAI Speech Recognition (option 3)

### 4.1 What Google documents (read 2026-11-17)

From the official page (https://developers.google.com/ml-kit/genai/speech-recognition/android):

- **Alpha**, "not subject to any SLA or deprecation policy": the API may change or go away. Dependency `com.google.mlkit:genai-speech-recognition:1.0.0-alpha2`.
- **Two modes.** Advanced uses the on-device Gemini model through Android AICore and runs on the Pixel 10 and 11 series and some recent Qualcomm, MediaTek and Exynos phones; basic is the older on-device recogniser, API level 31 and up.
- **Languages:** advanced covers 21 locales including en-US (nine marked beta); there is no en-AU or en-GB in the list [Certain, from the page], so Australian English runs as en-US.
- **Input:** the microphone, a file, or a custom stream of 16-bit 16 kHz mono PCM through a file descriptor. Streaming must be written at real-time pace; one-shot (a file) is faster than real time.
- **Output:** partial then final text; **word timings** (each word's text and start time from the session start) only when streaming: asking for them on a one-shot source is refused. No speaker labels. No word end times.
- **Not stated:** any length limit, quota, or whether anything leaves the device. Both modes are described as on-device. Not supported on a phone with an unlocked bootloader. `checkStatus()` must be called first (AICore can be absent or still downloading).

### 4.2 How Earshot uses it

- **Live, streaming, advanced mode, word timings on.** The service streams the meeting's PCM at real-time pace (the microphone itself is real time), so the transcript and its word start times come during the meeting. That is also the only way to get word timings.
- **The live transcript is the final transcript.** A one-shot pass over the recording afterwards would lose the word timings, and a second streaming pass takes as long as the meeting. So the final pass on the phone does speakers only: CAM++ diarisation over the stored FLAC, words assigned to speakers by start time, the same echo-free in-person rules as the Surface's room mode. Word end times are taken as the next word's start, capped at the end of the voice activity it falls in.
- **A synced computer may re-transcribe later.** The phone's FLAC stays on the phone (audio never syncs), so re-transcription happens on the phone or not at all; the desktop sees the phone's transcript as it is.
- **Fallback:** where advanced mode is unavailable (`checkStatus` fails, an unlocked bootloader, another language) the phone uses Parakeet through ONNX Runtime on the CPU (section 4.4), never basic mode (no spike measures it) and never a cloud recogniser. The user sees which ran, as on the desktop.

### 4.3 Privacy (PLAN 3.9)

The owner's rule: the content of conversations never leaves the device except text to the summary writer they chose. Google's page calls the API on-device but does not promise that no data leaves, and AICore is a system app that updates itself. So the spike proves it rather than trusting it:

- A meeting transcribes in flight mode with Wi-Fi and mobile data off (advanced mode must work with no network at all once the model is downloaded).
- With the network on, the per-app traffic of Earshot and of AICore (Android's per-UID network statistics) during an hour's meeting is reported; any AICore traffic is looked at before advanced mode ships, as the NPU's telemetry was (T8.3b). System information to Google, if any, falls under the owner's rule for third-party runtimes; transcript or audio never.
- Earshot never uses the platform `SpeechRecognizer` without its on-device constructor, and never basic mode's network path.

### 4.4 The spike (decides 4.2 or the fallback)

Rule written before measuring, as for every evaluation here. Compared on the Pixel 11, advanced mode against Parakeet 0.6B v2 on the phone's CPU, both fed the same PCM through the streaming path at real time:

- **Accuracy:** WER after Whisper normalisation on public speech (the AMI SDM single-microphone meetings, the in-person condition) and on a recording of the owner's own voice in a room.
- **Word start times:** against the AMI annotations' word times.
- **An hour's session:** no drop, no stall, memory flat.
- **Battery and heat:** percentage per hour and the thermal status reported by Android, against Parakeet.
- **Privacy:** section 4.3.
- **Pass rule (proposed, to fix with the owner before running):** advanced mode ships if its WER is no worse than Parakeet's plus 2 points on AMI SDM, the hour runs clean, it uses less battery than Parakeet, and the privacy check is clean. Otherwise Parakeet on the CPU is the phone's transcriber and advanced mode is revisited when the API leaves alpha.

## 5. Summaries

- The order of summary writers (T12.63) works unchanged on the phone. Default for a phone with sync: the desktop or the Unraid worker first. Without sync, or with neither reachable: Claude through an API key or the user's endpoint if they set one, else no summary until a writer is reachable (a kept-local meeting is summarised only on the phone or by the user's own model).
- **Open (decision A4): on-device summaries with Gemini Nano (T10.4a).** Recommended: after the first Android release, as a spike, because the offload covers most cases and the transcription spike already exercises AICore.

## 6. Capture, battery and heat

- Foreground service with the microphone type, its notification while recording, a partial wake lock, permissions RECORD_AUDIO, POST_NOTIFICATIONS, FOREGROUND_SERVICE_MICROPHONE.
- A recording is never cut by transcription trouble: audio goes to the FLAC first, as on the desktop (a setting never costs a recording).
- Speakers during the meeting only with spare capacity and never when the phone reports it is hot; otherwise after the meeting, within 30 minutes (the T6.13 rule).

## 7. Refreshed M10 task list (proposed; replaces PLAN.md's M10 once agreed)

1. **GATE-05** Android toolchain (unchanged).
2. **T10.1 Android build spike**: core, store, sync, runtime (in-person, ONNX Runtime for voice activity and speakers) for `aarch64-linux-android`; findings in `docs/android-build.md`.
3. **T10.1a Transcription spike**: section 4.4, rule committed first; decides ML Kit advanced mode or Parakeet.
4. **T10.2 Android shell**: Tauri 2 mobile with packages/ui recorder routes (unchanged).
5. **T10.3 Foreground service capture**: unchanged, plus the ML Kit stream from the same PCM.
6. **T10.3a `earshot-asr-mlkit`**: words over JNI onto the meeting timeline, end times from the next start, host-side tests.
7. **T10.4 In-person mode**: live transcript kept as final, speakers in the final pass, the fallback of 4.2.
8. **T10.4b Sync on the phone**: pairing, presence, the order of summary writers with offload; no audio.
9. **T10.5 Android export** (unchanged).
10. **T10.6 M10 acceptance**: `accept m10` on the host plus the spike's recorded results.
11. **T10.4a On-device summaries**: after the first Android release, if A4 says so.
12. **T10.7 Wear OS companion**: after, per A3.
13. **GATE-06 Pixel field test**, **GATE-07 v2 sign-off** (unchanged).

## 8. Open decisions

| | Decision | Recommended |
|---|---|---|
| A1 | Standalone, recorder with sync, or both | Both, sync optional |
| A2 | In-person only | Yes |
| A3 | Wear OS in the first release | After |
| A4 | Gemini Nano summaries in the first release | After, as a spike |
| A5 | The spike's pass rule (4.4) | As proposed |
