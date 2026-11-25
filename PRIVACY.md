# Privacy

Earshot has no accounts and no telemetry of its own, and the project runs no server that your meetings pass through. Your audio stays on the computer that recorded it. Out of the box, nothing about a conversation leaves that computer.

Conversation content (transcripts, summaries, names, titles) can leave in two ways, and only if you set one up:

- **A summary writer that is not on this computer.** The meeting's text goes to the writer you ticked: your own server, Anthropic, Claude Code, OpenAI Codex or another computer of yours.
- **Sync.** Your meetings go, encrypted, to a sync server that you run yourself, so that your other computers can hold the same meetings. The server cannot read them. Audio never syncs.

This page describes the current code. If you find something here that the code does not do, please open an issue.

## What stays on your computer

- **Audio:** the microphone and computer-audio tracks, as FLAC files. No network code in Earshot accepts audio, and sync refuses anything that looks like audio before it is sent.
- **Transcripts, summaries, meeting titles, names and settings:** in a local SQLite database. They leave only as the two cases above describe.
- **Voice data:** see "Voices" below.
- **Grey words** shown while someone is talking are display only: they are never saved, logged, summarised, synced or exported.
- **Crash logs:** in a local `crash-logs` folder (Settings, Notes and storage shows where). They hold where the code failed (file, line, function names), the app version, the time and a fixed thread name, and never a panic message or any meeting content. Earshot does not upload them.

All of this is under `%LOCALAPPDATA%\Earshot`, which Earshot does not encrypt. Anyone or anything with access to your Windows account can read it. The app's window keeps its browser engine's files, such as your theme choice, in the same folder.

Three secrets are kept apart from that folder, in Windows Credential Manager: your Anthropic API key if you saved one (`Earshot/anthropic-api-key`), and, if you set up sync, the sync key and this computer's sync token (`Earshot/sync-key`, `Earshot/sync-token`). Earshot can use them, but no screen can show them again.

## What can leave, and when

| What | Where to | When |
|---|---|---|
| Requests for model files | `huggingface.co`, `github.com`, `raw.githubusercontent.com`, and on opt-in CUDA downloads `developer.download.nvidia.com`, plus the first two's download servers (`*.hf.co`, `release-assets.githubusercontent.com`) | Only when you start a download. Those hosts see your IP address and which file you asked for. Every file is checked against a pinned checksum. On ARM64, a model's NPU version comes from this project's GitHub releases. |
| Requests for updates | `github.com` (this project's releases) and its download server `release-assets.githubusercontent.com` | Only when you press **Check for updates** or **Download**, or, if you turned on "Check for updates automatically", each time Earshot starts and then on the schedule you picked (every day unless you change it). Every update is verified against a key built into Earshot before it runs. |
| Summary text | The summary writers you ticked, or one you pick for a re-summary | Only if you ticked or picked one that is not on this computer, as below. The default, the model on this computer, sends nothing. |
| Your meetings, encrypted | The sync server whose address you entered | Only if you set up sync, as below. Off until you do. |

Update checks are off unless you press **Check for updates** or turn on "Check for updates automatically" in Settings, General; that switch is off by default, and with it on Earshot checks when it starts and then on the schedule you pick under it (every hour, 4 hours, 12 hours, day or week). A check asks for one small file from this project's latest GitHub release, `earshot-update-x64.json` or `earshot-update-arm64.json` for your PC's architecture. The request names only that file: not your version, not your computer and no conversation content. Earshot compares the versions on your computer. GitHub sees your IP address and which file was asked for. The file's signature is checked against a signing key built into Earshot before anything in it is used, and nothing in it can send Earshot to another host. The installer is downloaded only when you press **Download**, and its size and SHA-256 are checked again right before it runs. An automatic check that fails says nothing.

## If you choose a summary writer that is not on this computer

On Settings, Summaries you tick up to three summary writers and put them in order. Earshot asks them in that order: if one cannot write the summary, the next one tries. By itself, Earshot never uses a writer you did not tick. The default is one writer, the model on this computer, which sends nothing anywhere. Each writer's row says where the transcript goes before you tick it.

A meeting's **Re-summarise** menu is the one other way in: it lists every writer that can run now, ticked or not (the model on this computer if it is downloaded, your own server or Anthropic if it is set up, Claude Code and Codex if they are installed, and your other computers that offer it). A writer you pick there is sent that meeting's text, for that one summary.

A writer that is not on this computer is sent, for each summary: the transcript text with speaker names and line times, the meeting's title, start time, length and attendee names, any notes, your summary style and template, your Dictionary words, your own name if you set one, and the names of people you have saved that appear to be mentioned in that meeting (at most 15 per request). It is never sent audio.

- **Your own server (an OpenAI-compatible endpoint):** to the address you enter. If it runs on your own machine or network, the text stays there.
- **Anthropic with your API key:** to Anthropic. The key is kept in Windows Credential Manager, and Earshot can use it but no screen can show it again.
- **Claude Code or OpenAI Codex:** Earshot gives the text to the program you installed, which connects to its vendor using your sign-in. Earshot cannot see or audit that request. It records locally when it handed text over, how many characters and the outcome, never the text. Earshot runs the program from its usual install folder before looking on your PATH, and runs it without your own settings and without the environment variables each program documents for sending requests elsewhere: another API address, another provider (for Claude Code: Amazon Bedrock, Google Vertex, Microsoft Foundry), an API key or token other than your saved sign-in, proxy settings (`HTTP_PROXY`, `HTTPS_PROXY` and the like), and Claude Code's telemetry export to a collector of your own (`CLAUDE_CODE_ENABLE_TELEMETRY` and the `OTEL_` settings). So the text goes only to Anthropic or OpenAI on the account you signed in with, even if your computer is set up to route these programs through a company gateway or proxy. If you only sign in through such a variable, Earshot treats the program as not signed in and moves to your next writer, or skips the summary with a message; on a network that only works through a proxy, that writer fails. Settings your organisation manages centrally for these programs can still apply.
- **Another computer of yours** (needs sync, below): the meeting is already on that computer through sync. It writes the summary with its own summary writer, which may itself be one of the writers above. The row names that writer, so you can see where the text ends up.

That provider's own terms then apply to the text. If none of your ticked writers can write a summary, there is no summary and the meeting says why.

**Keeping one meeting off the remote writers.** On the new-meeting screen, "Keep this meeting on this computer" can be ticked before recording starts. That meeting's summary is then written only by a model running on one of your own computers (a paired summary worker's own model counts as one): it skips your own server, Anthropic, Claude Code and Codex, whatever your order says, and with no such model available it gets no summary. In its Re-summarise menu those writers are greyed out. The one way to lift this is **Allow** on that meeting's own page, which asks first and names the writer: it lifts the rule for that meeting only, sends nothing by itself, and cannot be undone (the meeting cannot be kept on this computer again). Keeping a meeting does not stop sync: if you sync, the meeting still goes to your other computers, encrypted, like any other.

"Summarise during the meeting" (Settings, Summaries, off by default) uses only the model on this computer and never connects anywhere.

## If you set up sync

Sync is off until you set it up on Settings, Sync (or in the first-run steps). It needs a sync server, the relay, which you run yourself in Docker: this project hosts none. You enter its address. Earshot only accepts an `https://` address, or plain `http://` to this same computer.

**What is sent.** The rows that describe your meetings: titles, the list of each meeting's tracks (not their audio), transcripts (with their timing), speakers and their names, the people you have named, notes, summaries (earlier versions included, which is what Undo uses), folders, and requests for another computer to write a summary. Your Dictionary words sync too unless you turn "Sync Dictionary words" off. Saved voices sync only if you turn "Sync voiceprints" on (see "Voices"). Audio never syncs, and neither do your other settings. A meeting is not sent while it is still being recorded.

**How it is protected.** Every row is encrypted on your computer before it is sent, with a key that only your computers hold. The key is made on your first computer. A new computer gets it from a pairing code that you read off one computer and type into the other; the relay only ever holds the key wrapped under that code, for at most 10 minutes. The key and each computer's sign-in token live in Windows Credential Manager.

**What the relay can see.** How many rows there are, how big each one is, when each was last changed and by which of your computers (as a random id), when each computer registered and was last in touch, and the IP addresses that connect to it. It cannot see titles, text, names, what kind of thing a row is, or your computers' names. Its own log holds each request's method, path, status and duration, nothing else.

**Presence.** While sync is on, each computer regularly tells the others, through the relay and encrypted the same way, its name (by default the computer's own name; you can change it on Settings, Sync), its Earshot version, whether it writes summaries for your other computers and with what kind of writer, and whether it is recording now. That is how the list of computers on Settings, Sync is drawn.

**What sync does not do.**

- It is not a backup. Audio stays on the computer that recorded it, and a deletion syncs like anything else.
- There is no key change. Removing a computer on Settings, Sync stops it reaching the relay, but it does not take back the key that computer already holds. Wipe a lost computer as well.
- If you lose every computer, the key is lost with them and what the relay holds cannot be read by anyone.
- Turning sync off, or removing this computer, stops this computer's requests and leaves its own data alone. It does not delete what the relay or your other computers hold.

### The summary worker

The worker is an optional second container that writes summaries for your computers on a server of yours. To do that it must read the transcript, so it joins your sync as one more device and holds the key. **Once a worker is paired, whoever controls the host it runs on can read every meeting you sync.** Encryption then protects the relay's database, its backups and the network path, not that host. If that is not acceptable, do not start the worker: the relay alone never holds a key.

The worker records nothing, holds no audio and never takes voiceprints. It sends the transcript text only to the model endpoint in its own configuration, which in the supplied compose file is a model server on the same host with no route out.

## Voices

A voiceprint is a set of numbers that describes a voice. It can't be played back as audio, but it is biometric data, so:

- Saving your own voice is optional (Settings, People, "Your voice"), from a 30 second sample you record on purpose. The recording itself is held in memory and is never written to disk.
- Another person's voice is remembered only when you name it with Save in a meeting's Speakers panel, one voice at a time. A voice you ignore is never remembered.
- Each meeting's record on the computer that recorded it also holds a numeric voice summary per speaker, so names you gave survive a re-transcription. It never syncs, and it goes when the meeting is deleted.
- You can forget your own voice, or anyone's, on Settings, People. Deleting a meeting does not remove saved voices.
- Saved voices stay on the computer that saved them unless you turn on "Sync voiceprints" on Settings, Sync. It is off by default. With it on, saved voices travel to your other computers encrypted like any synced row, and a voice you forget on one computer is forgotten on every computer that syncs voiceprints.

## Calls and the microphone

Earshot records only when you start a meeting. Two helpers look at which programs are using audio, so that Earshot can ask you a question at the right time:

- **"Offer to record when a call starts"** (Settings, General, on by default): while Earshot is running and not recording, it checks whether a known call app (Microsoft Teams, Zoom, Webex, Slack, Discord, Skype; never a web browser) has the microphone open, and if so asks whether to start recording. Nothing records unless you say yes.
- **During a call you are recording:** it checks whether a known call app, or a browser, still has an audio session, so that it can ask whether to stop when the call seems to be over. It only asks. Only if you turn on a setting on the General page, which is off by default, does a question that nobody answers stop the recording by itself, after 3 minutes.

Both ask Windows which program owns each audio session, whether the session is active and what its level meter reads, and compare the program's name against a list built into Earshot. They keep nothing: the names are never stored, logged, shown or sent. Neither opens an audio stream, so neither hears any sound.

## Deleting

- **Audio** is deleted automatically after the final transcript, 7 days by default (Settings, Notes and storage). A meeting whose final transcript never completed keeps its audio until you delete the meeting. Stray audio files with no meeting record, such as a crash can leave, are removed after 7 days.
- **Deleting a meeting** removes its audio, transcript and summaries, earlier versions included. The database is set to overwrite deleted content. Editing a line or transcribing again is not a deletion: it is deleting the meeting that removes all of its text.
- **With sync**, deleting a meeting on one computer deletes it on every computer that syncs, including the audio on the computer that holds it. The relay keeps only the newest encrypted copy of each row, and for a deleted meeting that copy is an emptied marker, kept so that a computer that was away still hears of the deletion. Backups you make of the relay may hold older encrypted copies.
- **People stay until you remove them.** A person's name and saved voice stay on Settings, People after their meetings are deleted. **Remove** there deletes both, and their lines in your meetings go back to "Speaker 1" and so on. Summaries, notes and exported files that already mention them do not change.
- **Exported Obsidian notes stay.** They are your files in your vault, and Earshot does not delete them. If your vault syncs somewhere, your notes go with it.
- **Uninstalling keeps your data** unless you tick "Delete the application data", so a reinstall finds your meetings. That box deletes all of `%LOCALAPPDATA%\Earshot`, meetings and models included. The entries in Windows Credential Manager (a saved API key, the sync key and token) stay until you delete them there; "Remove this computer" on Settings, Sync removes the sync ones.

## What other software does

Earshot cannot control everything on your computer. These are the known cases:

- **Windows Error Reporting:** a crash report can hold part of a process's memory, which could include transcript text, and depending on your Windows settings may be sent to Microsoft. Earshot opts its own program out at every start, for your Windows account, and a test confirmed that this stops the report.
- **Local crash dumps:** a separate Windows setting can still save a crash dump on your computer, in `%LOCALAPPDATA%\CrashDumps`. Software can turn this on: the NVIDIA App appears to on PCs that have it. A dump stays on your computer but can hold fragments of what the app had in memory, including transcript text, and deleting a meeting does not reach it. Earshot deletes its own dumps from that folder the next time it starts. A dump folder set elsewhere is not cleaned.
- **The app's window:** it runs in Microsoft WebView2, in processes of Microsoft's that are not part of that opt-out and that display your transcript when you open one. Microsoft says WebView2 follows your Windows consent settings for crash reports. Earshot does not currently change this. WebView2 is Microsoft's component, kept up to date by Windows, and it may make requests of its own to Microsoft (for example for its own updates). Earshot does not control those, and its own checks cannot see them.
- **System information from runtimes:** Earshot turns ONNX Runtime's telemetry off. Runtimes can still offer Windows' diagnostic channel system information such as runtime version, CPU, memory and driver names and versions. A check on the NPU path found no transcript text in those events. Anything that could carry conversation content would not be accepted.
- **Windows notifications:** Windows stores these in its own database and can show them on the lock screen, so Earshot's use fixed text with at most a count or a version number, never a meeting's title or content. They are silent, so a microphone does not pick them up.
- **Your call app, your vault's sync, your backups and the server you run the relay on** do what they do with their own data and yours.

## How this is enforced

- All of Earshot's HTTP goes through one crate, `earshot-net`, with a host allowlist. An automated check fails the build if any other shipped code gains an HTTP client. The sync client's allowlist is exactly the sync server you entered.
- The app's web page can only talk to the app itself, through its content security policy.
- Only kinds of data marked as syncable can be turned into a synced row at all, audio is not one of them, and each row is checked for size and for the signatures of audio files before it is encrypted.
- An acceptance test replays a whole meeting and checks from `earshot-net`'s request log that nothing went anywhere except the test endpoint. Another runs real sync between test devices and a relay and checks that no audio was in anything sent.

These checks catch mistakes in Earshot's own code, and the replay can only see traffic that goes through `earshot-net`. They are not an outside audit. The installers are not code-signed with a certificate; in-app updates are signed with the project's own key, as above. To report a security problem privately, see [SECURITY.md](SECURITY.md).

## Recording other people

Earshot does not announce itself to anyone on a call. Telling people, and getting any consent the law requires where you and they are, is your responsibility.
