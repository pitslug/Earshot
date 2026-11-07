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

## Settings (T7.4)

Sections: General, Audio (devices, capture mode, audio check), Summaries (local model shown / own endpoint with URL, provider and key fields and the remote-endpoint warning / no summary; style text), Models (backend order and model check, downloads with size and progress), Benchmark (T7.4c: the current settings run against a bundled 2.5-minute meeting replayed in real time; transcript lag during it, final transcript and summary times after it, next to the model check's verdict; nothing stored as a meeting, no network), Speakers (detection timing, your voice: record or forget), Obsidian (vault folder and subfolder, validated on save), Storage (audio retention, the T7.4 list, default 7 days).

The benchmark never runs beside a meeting, a meeting job, the model check, the voice sample or the audio check, and none of them starts while it runs.

While the "your voice" sample is being recorded and saved (Speakers), a meeting cannot start and the model check cannot run; the sample in turn cannot start during a meeting or the model check.

## First run (T9.1d)

Four steps, each skippable, progress bar on top: 1 Obsidian vault, 2 Models (picked for this computer, size from the manifest, download now / later / never for optional ones), 3 Summaries (local / own endpoint / none), 4 Audio check and your voice (optional).

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
