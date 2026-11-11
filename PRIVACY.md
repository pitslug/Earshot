# Privacy

Earshot has no accounts, no servers and no telemetry of its own. Your audio stays on your computer. The only conversation content that can leave is text (the transcript, any notes and the meeting's details), and only if you choose a summary provider that is not on your computer.

This page describes the current code. If you find something here that the code does not do, please open an issue.

## What stays on your computer

- **Audio:** the mic and computer-audio tracks, as FLAC files. No network code in Earshot accepts audio.
- **Transcripts, summaries, meeting titles and names:** in a local SQLite database.
- **Voice data:** see "Voices" below.
- **Grey words** shown while someone is talking are display only: they are never saved, logged, summarised or exported.
- **Crash logs:** in a local `crash-logs` folder. They hold where the code failed (file, line, function names), the app version, the time and a fixed thread name, and never a panic message or any meeting content. Earshot does not upload them.

All of this is under `%LOCALAPPDATA%\Earshot`, which Earshot does not encrypt. Anyone or anything with access to your Windows account can read it. The app's window keeps its browser engine's files, such as your theme choice, in the same folder.

## What can leave, and when

| What | Where to | When |
|---|---|---|
| Requests for model files | `huggingface.co`, `github.com`, `raw.githubusercontent.com`, and on opt-in CUDA downloads `developer.download.nvidia.com`, plus the first two's download servers (`*.hf.co`, `release-assets.githubusercontent.com`) | Only when you start a download. Those hosts see your IP address and which file you asked for. Every file is checked against a pinned checksum. On ARM64, a model's NPU version comes from this project's GitHub releases. |
| Requests for updates | `github.com` (this project's releases) and its download server `release-assets.githubusercontent.com` | Only when you press **Check for updates** or **Download**, or each time Earshot starts if you turned on "Check for updates when Earshot starts". Every update is verified against a key built into Earshot before it runs. |
| Summary text | The provider you chose | Only if you chose one, as below. Local summaries, the default, send nothing. |

Update checks are off unless you press **Check for updates** or turn on "Check for updates when Earshot starts" in Settings, General; that switch is off by default. A check asks for one small file from this project's latest GitHub release, `earshot-update-x64.json` or `earshot-update-arm64.json` for your PC's architecture. The request names only that file: not your version, not your computer and no conversation content. Earshot compares the versions on your computer. GitHub sees your IP address and which file was asked for. The file's signature is checked against the key built into Earshot before anything in it is used, and nothing in it can send Earshot to another host. The installer is downloaded only when you press **Download**, and its size and SHA-256 are checked again right before it runs. An automatic check that fails says nothing.

## If you choose a summary provider

The default is a model running on your computer. If you choose something else, each summary sends the transcript text with speaker names and line times, plus the meeting's title, start time, length and attendee names, any notes, your style text and your own name if you set one. It never sends audio.

- **An OpenAI-compatible endpoint:** to the address you enter. If it runs on your own machine or network, the text stays there.
- **Anthropic with your API key:** to Anthropic. The key is kept in Windows Credential Manager, and Earshot can use it but no screen can show it again.
- **Claude Code or OpenAI Codex:** Earshot gives the text to the program you installed, which connects to its vendor using your sign-in. Earshot cannot see or audit that request. It records locally when it handed text over, how many characters and the outcome, never the text.

That provider's own terms then apply to the text. Earshot never falls back to a remote provider you did not pick. If yours is unavailable, the summary is skipped or fails and says so. If your own endpoint is saved without a usable address and model, Earshot uses the local model and says so.

## Voices

A voiceprint is biometric data, so:

- Saving your own voice is optional, from a sample you record on purpose. The sample itself is held in memory and is never written to disk.
- Another person's voice is remembered only when you name it with Save in a meeting's Speakers panel, one voice at a time. Your own voice is never saved there, and a voice you ignore is never remembered.
- Each meeting's record holds a numeric voice summary per speaker, so names you gave survive a re-transcription. It can't be played back as audio, and it goes when the meeting is deleted.
- You can forget your own voice, or anyone's, in Settings, Speakers. Voiceprints stay on this computer, and deleting a meeting does not remove them.

## Deleting

- **Audio** is deleted automatically after the final transcript, 7 days by default (Settings, Storage). A meeting whose final transcript never completed keeps its audio until you delete the meeting. Stray audio files with no meeting record, such as a crash can leave, are removed after 7 days.
- **Deleting a meeting** removes its audio, transcript and summaries. The database is set to overwrite deleted content.
- **People stay until you remove them.** A person's name and saved voice stay in Settings, Speakers after their meetings are deleted. **Remove** there deletes both, and their lines in your meetings go back to "Speaker 1" and so on. Summaries, notes and exported files that already mention them do not change.
- **Exported Obsidian notes stay.** They are your files in your vault, and Earshot does not delete them. If your vault syncs somewhere, your notes go with it.
- **Uninstalling keeps your data** unless you tick "Delete the application data", so a reinstall finds your meetings. That box deletes all of `%LOCALAPPDATA%\Earshot`, meetings and models included. Any saved API key stays in Windows Credential Manager until you delete it there.

## What other software does

Earshot cannot control everything on your computer. These are the known cases:

- **Windows Error Reporting:** a crash report can hold part of a process's memory, which could include transcript text, and depending on your Windows settings may be sent to Microsoft. Earshot opts its own program out at every start, for your Windows account, and a test confirmed that this stops the report.
- **Local crash dumps:** a separate Windows setting can still save a crash dump on your computer, in `%LOCALAPPDATA%\CrashDumps`. Software can turn this on: the NVIDIA App appears to on PCs that have it. A dump stays on your computer but can hold fragments of what the app had in memory, including transcript text, and deleting a meeting does not reach it. Earshot deletes its own dumps from that folder the next time it starts. A dump folder set elsewhere is not cleaned.
- **The app's window:** it runs in Microsoft WebView2, in processes of Microsoft's that are not part of that opt-out and that display your transcript when you open one. Microsoft says WebView2 follows your Windows consent settings for crash reports. Earshot does not currently change this.
- **System information from runtimes:** Earshot turns ONNX Runtime's telemetry off. Runtimes can still offer Windows' diagnostic channel system information such as runtime version, CPU, memory and driver names and versions. A check on the NPU path found no transcript text in those events. Anything that could carry conversation content would not be accepted.
- **Windows notifications:** Windows stores these in its own database and can show them on the lock screen, so Earshot's use fixed text and counts, never a meeting's title or content. They are silent, so a microphone does not pick them up.
- **Your call app, your vault's sync and your backups** do what they do with their own data and yours.

## How this is enforced

- All of Earshot's HTTP goes through one crate, `earshot-net`, with a host allowlist. An automated check fails the build if any other shipped code gains an HTTP client.
- The app's web page can only talk to the app itself, through its content security policy.
- An acceptance test replays a whole meeting and checks from `earshot-net`'s request log that nothing went anywhere except the test endpoint.

These checks catch mistakes in Earshot's own code, and the replay can only see traffic that goes through `earshot-net`. They are not an outside audit. The installers are not code-signed with a certificate; in-app updates are signed with the project's own key, as above.

## Recording other people

Earshot does not announce itself to anyone on a call. Telling people, and getting any consent the law requires where you and they are, is your responsibility.
