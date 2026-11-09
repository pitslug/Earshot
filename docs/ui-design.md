# Desktop UI design (GATE-11, 2026-11-01)

Agreed with the owner in the GATE-11 session. PLAN.md D21 is the short form; this note is what M7 builds against. Mockups: `docs/ui-mockups/gate-11.html` (layout and flow only: control sizes and type come from Fluent, not from the mockup).

## Shape

- A full app window plus a tray icon. The window: meeting list on the left (grouped Today / Yesterday / older, a "New meeting" button on top, Settings at the bottom), the selected meeting on the right.
- While recording the window can shrink to a compact "listening" card (status, clock, two meters, word count, Stop). The tray shows record state and start/stop (T7.5).
- When the summary is stored and the window is not being looked at, a silent "Summary ready" notification: counts of actions and decisions, how many voices need names, buttons Open and Name voices. It never names the meeting (T7.5): Windows keeps notifications in its own database and shows them on the lock screen.
- Closing the window keeps Earshot in the tray; only "Quit Earshot" in the tray menu ends it, asking first during a meeting. Ctrl+Alt+Shift+R starts and stops a meeting from anywhere. Started by Windows, Earshot opens no window.

## Look and feel

- Quiet native: Windows 11 feel. Fluent UI React v9 (`@fluentui/react-components`, MIT) for every control, so sizes follow Fluent (32px controls, 4px corners, accent underline on a focused input), Segoe UI Variable at 14px body, follows the system light/dark setting, one accent colour, medium density.
- No custom control styling that fights Fluent: if a control looks wrong, fix it through Fluent tokens or theme, not per-component CSS.
- Every transcript view (live, transcript tab, any preview) is a grid: timestamp column where there is room, a fixed speaker column, and the text in its own column, wrapping beside the speaker, never under it.
- UI copy: sentence case, no em dashes (AGENTS rule 12), no "please", no exclamation marks.

## During the meeting (T7.2)

Reassurance, not a wall of text (D20):

- Status line, always in view at the top: red dot, "Listening", capture mode ("Call: mic + computer audio" or "In the room: mic only"), elapsed clock, Stop meeting (danger button) and Compact.
- Warnings as banners above the meters: Bluetooth hands-free mic, a track that fell behind or stopped, device change and restore, live speaker work stopped. One banner per condition, newest first, dismissible.
- Two level meters with a voice-activity dot each: "You" (mic) and "Them" (computer audio). In-room mode shows one meter. Live labels are You / Them only: speaker names exist only after hang-up.
- Three tiles: words so far, transcript behind (seconds), running on (backend that ran, from the runtime's evidence, e.g. "GPU (CUDA)").
- "Transcript as it lands", collapsible, open by default: stretches as they arrive with You / Them in the speaker column, a "next stretch transcribing" row at the end. Only this part scrolls. It follows the newest line until the user scrolls up, then "Jump to latest" goes back to following (T9.1g: newest stays at the bottom, the saved transcript is always in time order).
- Provisional words (T9.1m): while someone is talking, the unfinished stretch shows as the last row of the transcript, quieter and in italics, in place of the "next stretch transcribing" row. It only grows, and the settled text replaces the words it covers when it lands; words the settled text lacks are gone with it. One track per meeting: Them in a call (a call's mic shows none, because far-end words leak into it until the echo filter removes them), You in the room.
- A banner at the top of the transcript while a meeting's transcript cannot follow it (T9.1l). Waiting for an older meeting: "**Audio is recording.** Processing your previous meeting. Transcription will begin soon." Waiting for the app's own start: "**Audio is recording.** Transcription will begin soon." Then "Catching up on the transcript" until the backlog clears, then none. Past 30 minutes held, or a track that fell that far behind: "**Audio is recording.** The live transcript has stopped for this meeting. The full transcript will be ready after the meeting ends." Not dismissible: each goes by itself.
- After stop: a progress view of the post-meeting steps (`PostStage`) until the summary is stored or skipped. New meeting works throughout: the older meeting finishes behind the new one, its row reading "Processing", or "Waiting to finish" while another's work runs first.
- No notes editor in M7 (the owner's decision: notes come later).

## Meeting detail (T7.3)

- Header: title, date, duration, capture mode, transcript pass (final or draft), summary model and backend. Buttons: Export to Obsidian, Copy summary (when there is a summary and no new one is being written: plain Markdown, no citations), Re-transcribe, Re-summarise (Summarise when there is none); the header stays in view while the meeting scrolls. Deleting starts from the meeting list: a trash can on hover or keyboard focus, or the right-click menu (Menu key or Shift+F10 from the keyboard), then the confirm dialog.
- Tabs: Summary, Transcript.
- Summary tab: TL;DR, Decisions, Actions (owner column + task), each item with its citations as small links that jump to the cited transcript line. Actions are read-only in the app: ticking happens in the exported Obsidian note ("Tick actions in the Obsidian note"). A "Re-summarise" button uses the style saved in Settings (a one-off style was dropped); the new summary replaces the old one and the previous one is kept until the next re-summary, with a "Switch to previous summary" link.
- A banner when voices need names ("2 voices need names. Names flow into the summary and export.") opening the who-is-who panel.
- Who-is-who panel (right column): one row per speaker with a play-sample button, name (or a name field for unknown voices), where it came from (mic, matched voiceprint), share of talk time, one sample line, and Merge / Link to person. "Remember these voices for next time" is unticked by default (voiceprints are opt-in). Renames rewrite the speaker rows, so summary, transcript and export all follow.
- Transcript tab: the transcript grid (time, speaker, text), a cited line highlighted when reached from a citation.

## Settings (T7.4, T12.5b)

Seven pages, in this order, as agreed in docs/settings-design.md; Settings opens on General:

1. General: start with Windows, closing to the tray, the keyboard shortcut, theme, updates, and the setup guide.
2. Audio: devices, whether a new meeting is a call or in the room, audio check.
3. System: the hardware, the recommendation, the speech helpers, where each task runs, the model check, NVIDIA's CUDA libraries where offered, and the benchmark (T7.4c: the current settings run against a bundled 2.5-minute meeting replayed in real time; transcript lag during it, final transcript and summary times after it, next to the model check's verdict; nothing stored as a meeting, no network).
4. Transcription: meeting languages first (they decide the recommendation), the transcription models with downloads, size and progress (and their NPU versions on ARM64), whose words show while someone is talking, and when speakers are found.
5. Summaries: who writes the summary, with the summary model under "On this computer", your own endpoint with URL, provider and key fields and the remote-endpoint warning, Claude Code and Codex with their sign-in check, or no summary; summarising during the meeting; the style text.
6. People: your name, your voice (record or forget), and the people Earshot knows.
7. Notes and storage: the Obsidian vault folder and subfolder (validated on save), how long recordings are kept (the T7.4 list, default 7 days), and where the crash logs are.

The save rule (T12.5c), on every page and in the wizard: a choice (radio, switch, dropdown, checkbox) applies as soon as it changes; typed text is saved only by its Save button (or Enter in a one-line field), and in the wizard by Next. Leaving a field saves nothing. No choice sits inside a part that a Save button saves, with one exception: the provider of your own endpoint is saved with the address and model typed beside it, because a provider alone is not an endpoint. A folder picked with "Choose folder..." fills the vault field, which is typed text and saves with Save.

Wording (T12.5c): plain words in headings, labels and tags; a developer term (ONNX Runtime, Vulkan) only in detail text, never in a heading or label. A heading and the field under it never repeat the same words: the field keeps them as its accessible name only. "Keeps up" always says what with: live speech on this computer. Model names come only from the app's display names. Privacy wording (what is sent where, what is kept) changes only with the owner.

Settings holds one model state for System, Transcription and Summaries, so a download or the model check carries on across pages; the first-run wizard holds its own, and the two are never shown together.

The benchmark never runs beside a meeting, a meeting job, the model check, the voice sample or the audio check, and none of them starts while it runs.

While the "your voice" sample is being recorded and saved (People), a meeting cannot start and the model check cannot run; the sample in turn cannot start during a meeting or the model check.

## First run (T9.1d, T12.5a)

Four short steps with a progress bar on top, as agreed in docs/settings-design.md:

1. Summaries: on this computer (preselected), Claude through Claude Code, OpenAI through Codex, or no summary, each with its line saying what is sent where; your own endpoint under "Other".
2. Download: meeting languages, then one line and one button with the total size for this computer: speech helpers, the recommended transcription model, and a summary model only if step 1 chose this computer. "Choose different models" opens the full list. Downloads carry on in the background.
3. About me: your name, then the voice sample with its own Skip. While the speech helpers are not there, the record button is off with a line saying why, and it turns on when they arrive.
4. Optional extras: Obsidian and the audio check as two rows, each opening when pressed. Then Finish.

Next saves what is on the step, typed fields included, so the wizard has no Save buttons; a refused save stays on the step and says why. Back leaves what was typed unsaved. Back and Next are the way through, with Skip setup at the top. Finishing or skipping with no transcription model says on the next screen that meetings need one and that it is in Settings, Transcription; it stops nothing. Nothing downloads, records or plays a sound, and neither the model check nor the audio check runs, until its button is pressed. The one thing that runs by itself: when the Claude Code or Codex choice is shown on the Summaries step, its card asks that agent CLI whether it is installed and signed in, which sends no meeting content.

## Later (not M7)

- Notes during and after a meeting, including handwriting converted to text (PLAN 1, "Later").

## Answers recorded

| Question | Answer |
|---|---|
| Window | Full app + tray |
| During the meeting | Signals + landed text |
| Speaker naming | Who-is-who panel |
| Action items | Obsidian owns ticks |
| Re-summary | Replace, keep one undo |
| Look | Quiet native, Fluent UI React v9 |
| First run | Short wizard, 4 steps |
| Notes | Later, with handwriting as an option |

## Build notes

- The UI talks to Rust only through one typed IPC module (tauri-specta). `packages/ui` defines the interface and a mock implementation; component tests and the browser preview use the mock, the desktop app the Tauri one.
- Screens are checked by screenshot in a browser against the mock (light and dark) before a UI task is marked done: text clipping, wrapping and alignment are judged there, not from code.
