# Changelog

What each release changed, in plain words, newest first. It is written from the commit history between release tags, so it lists what was built, not every fix. Dates are the day the release was tagged. Versions that are missing (0.3.2, 0.4.1) were never tagged.

[docs/compatibility.md](docs/compatibility.md) says what an update does to your data, to sync and to the relay and worker.

## Unreleased (after 0.4.2)

- A line you edited is kept when the same meeting is transcribed again on another computer.
- A voice you chose to ignore is ignored on your other computers too.
- Find in a transcript, by speaker and by text.
- The meeting list groups meetings by week and month, each group can be folded, and a meeting can be renamed from its row.
- A simpler card for a named voice in the Speakers panel. Summary edit buttons no longer move the text.
- In a meeting in the room, the microphone is opened without Windows' sound processing by default, and a microphone that could not give that tries again.
- The Summaries page folds away each writer's setup, and says when Claude Code or Codex is not signed in. The Dictionary page points to the People page for saved names.
- The Sync page's list of computers keeps up while it is open, and the first-run Sync step knows when this computer already syncs.
- Project files: this changelog, SECURITY.md, CONTRIBUTING.md and docs/compatibility.md. README.md and PRIVACY.md rewritten.

**Updating:** the database changes (0.4.2 cannot open it afterwards) and the sync row format rises from 2 to 4, so every computer that syncs, and then the worker, must be updated.

## 0.4.2 - 2026-11-25

- Earshot offers to start recording when a call app opens your microphone ("Offer to record when a call starts" in Settings, General).
- An open web browser no longer holds off the offer to stop when a call ends.
- Move a transcript line to another speaker, and split a line in two.
- Speaker changes snap to sentence ends.
- Summaries are given the names of people you have saved that the meeting seems to mention, at most 15 per request, for spelling.
- Minimising keeps Earshot on the taskbar. The recording screen uses the full window width, the Speakers panel stays in view while the transcript scrolls, banners fit on one line where they can, and the side panel can be resized.

## 0.4.0 - 2026-11-18

- Saved voices are stored as one sample per meeting, so "Not" on a wrongly named voice, relinking or merging takes just that meeting's voice back out. Voice samples sync when voiceprint sync is on.
- "Not <name>" on a wrongly named voice. Choose a saved person as you type a name.
- A voice is named after a saved person only on a stricter match than before.
- The first-run guide gains a Sync step and the order of summary writers, and offers the grey words with a suggestion for this computer.
- Grey words for your own voice on the NPU.
- Status dots in the meeting list. The window follows sync, with one banner for a summary written on another computer.
- A summary is marked out of date when a name it uses changes.
- Quotes around a Dictionary word are dropped.
- Short turns are regrouped when speakers are separated.
- The relay image is renamed `earshot-server` (this release is also published under the old name `earshot-sync-server`). The worker's compose file takes `cpu` or `gpu` for its label.

**Updating:** the sync row format rises to 2. The first push from a 0.4.0 computer makes every older computer and worker in that sync stop with "update needed" until it is updated.

## 0.3.11 - 2026-11-17

- Text held back on your microphone track lands before its grey words leave.
- The summary worker is built without the speech and model engines it never used, and its presence says whether its model runs on the processor or the graphics card.
- A new meeting's live transcript can run beside an older meeting's local summary on some hardware pairs, with a banner.

## 0.3.10 - 2026-11-17

- Summary writers are one ordered list on the Summaries page.
- A voice is named after a saved person only when it beats the runner-up by a margin.
- Earshot says when the NPU model is being prepared at start-up.
- ONNX Runtime bindings upgraded.

## 0.3.9 - 2026-11-16

- Summary writers in order: tick the writers to use and Earshot asks them one after another. Re-summarise offers any of them.
- Remove another computer from the Sync page. A removed computer can join again with a pairing code.
- The summary worker waits to be joined instead of stopping.
- A setting for the microphone's sound processing.
- Clearer messages on the Surface about the NPU and the model check.

## 0.3.8 - 2026-11-15

- Summary offload: a computer can ask another of your computers to write its summaries.
- The Dictionary syncs, one row per word.
- The summary worker: a container that joins your sync and writes summaries on a server of yours.
- A computer says it has gone when Earshot quits or Windows sleeps.

## 0.3.7 - 2026-11-15

- Sync between your own computers: the relay server (a container you run), encrypted rows, the Sync page, pairing codes and which computers are online.
- A meeting kept on this computer stays kept on every computer it syncs to.
- On an NVIDIA card through CUDA, one copy of the transcription model's encoder is shared by every session, and grey words for your own voice run on the card when it has room. Grey words on the card need 12 GB of memory, as on the processor.
- Cancel a re-transcription or a re-summary.
- The summary header says when a summary was edited and names its writer.
- The Speakers panel says who a name links to and offers to merge a person named after you into you.
- Folders sort accented letters beside their base letters, and each folder row has its own menu button.
- Invisible formatting characters are refused in Dictionary words and the summary style.
- More Codex features are turned off for summary runs, and only the ones the installed Codex lists.
- Retired CUDA libraries are removed.

## 0.3.6 - 2026-11-14

- Grey words pause on battery and come back when the PC is plugged in.
- Delete a meeting that is still finishing.
- A new meeting's transcript waits for an older meeting's summary only when both need the same device.
- The meeting list shows the recording date.

## 0.3.5 - 2026-11-14

- The meeting screen says why grey words are off.
- A stretch of speech that was cut reaches the transcript as soon as nothing more can join it.
- The developer recording tool can open the microphone without Windows' sound processing.

## 0.3.4 - 2026-11-13

- Summary templates on the Summaries page, combined with your own style.
- Shorter rules for summaries written through Claude Code and Codex.
- A stuck meeting never blocks the ones after it: the final pass, a local summary load and a held live load are watched for progress.
- A lost audio device never ends a track for good: the track is opened again, and only that track ends if it cannot be.
- NPU models are prepared in a helper process.
- The window does less work while a meeting runs.

## 0.3.3 - 2026-11-13

- A recording that was interrupted (a crash, a power cut) is recovered at the next start.
- The recording is finished when Windows shuts down. The audio is kept when a recording write fails.
- Stop and Quit never wait for ever on the capture thread.
- Grey words on the NPU for the other people.
- Plain messages, with a way forward, for the failures a new user meets.
- Claude Code and Codex are run without the environment variables that could send a request elsewhere (another address, another provider, other credentials, a proxy, telemetry export).
- When a meeting ends, the window goes to its page.

## 0.3.1 - 2026-11-12

- Add words to the Dictionary from the transcript.
- Check for updates on a schedule.

## 0.3.0 - 2026-11-12

- The transcript marks words that sound like a Dictionary word or a saved name, and you can change them or keep them.
- On ARM64, the NPU model is prepared when the app starts.

## 0.2.5 - 2026-11-11

- Grey words run on the processor when CUDA transcription runs on a graphics card too small for them.
- Dictionary words are listed in alphabetical order.

## 0.2.4 - 2026-11-11

- The Dictionary page: your own words, given to summaries.
- Edit a meeting's title and transcript lines. Correct a summary's text, with Undo.
- "Keep this meeting on this computer" on the new-meeting screen.
- Choose the Claude Code and Codex model, and Claude Code's effort, from a list.
- The Speakers panel slides out, with Save and Ignore for each voice.
- Cancel for a meeting that is finishing behind a newer recording.

## 0.2.3 - 2026-11-10

- Settings rebuilt as pages, and a new first-run guide.
- Grey words for each track, yours and the other people's.
- Recording carries on through idle sleep, and any gap is reported.
- Notifications when a recording ends with the window hidden and when a re-summary finishes.
- The Summary and Transcript tabs stay in view.

## 0.2.2 - 2026-11-08

- Folders for meetings, in the side panel.
- ARM64: the installer carries the NPU runtime, the NPU is used once a model's NPU version is installed, and those versions are offered as downloads.

## 0.2.1 - 2026-11-08

- Search across meetings.
- Copy a meeting's summary as Markdown.
- Cancel while a meeting is finishing.
- Earshot shows when an update is available.
- In a meeting in the room, mark a voice as yourself.
- Uninstalling clears Earshot's crash-report opt-out.

## 0.2.0 - 2026-11-08

- In-app updates, signed with the project's own key. 0.1.0 has no in-app update: install a later version by hand.

## 0.1.0 - 2026-11-07

- First public release, x64 only, with the source.
- The installer carries Microsoft's Visual C++ runtime beside the app.
- Earshot opts its own program out of Windows Error Reporting and deletes its old crash dumps.
- Remove a person, with their saved voice.
