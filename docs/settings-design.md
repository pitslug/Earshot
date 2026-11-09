# Settings and first-run design (T12.5)

Status: agreed with the owner, 2026-11-10. Build tasks T12.5a to T12.5c in PLAN.md. Unless this page says otherwise, behaviour and stored settings stay as they are.

## How it was

Walked through on the Vite preview (mock data, desktop and Surface machines), plus the code.

### First run: four steps

| Step | Holds | Problem |
|---|---|---|
| 1 Obsidian vault | Folder, subfolder, Save | Opens with something many users skip, and something nothing else needs |
| 2 Models | Hardware, recommendation, helpers, four transcription models, the check, two summary models, languages | The longest step, and it asks about summary models before the user has chosen who writes summaries; its own note says "You choose that in the next step" |
| 3 Summaries | This computer, own endpoint, none | Leaves out Claude Code and Codex, the options a subscriber most likely wants, while offering a URL form |
| 4 Audio check and your voice | Chirps, name, voice sample | The name is the one quick answer that improves every summary, and it sits under an optional step |

Across the steps:

- Languages came last on the models step, yet they decide the recommendation above them.
- Three ways out (Skip setup, Skip this step, Next), and Next refused while a field was unsaved.
- Later and Never appeared beside each optional model. Later was the default and did nothing. Never was stored, and only the wizard read it, so it had no visible effect elsewhere (from a code search).
- The wizard could finish with no transcription model.

### Settings: eight pages

| Page | Holds | Problem |
|---|---|---|
| General | Start with Windows, closing, show words while talking, shortcut, setup guide, theme, updates, crash logs | Unrelated things; the words setting is a transcription choice |
| Audio | Devices, capture mode, audio check | Coherent |
| Summaries | Writer, during the meeting, style | The local model and where it runs are on another page |
| Models | Hardware, recommendation, helpers, transcription, the check, summary models, where it runs, CUDA, languages | Does everything; the page Settings opens on by default, although it is listed fourth |
| Benchmark | One button | A top-level page for something rarely used |
| Speakers | When speakers are found, your name, your voice, people | Detection timing is a performance choice, not about people |
| Obsidian | Folder, subfolder | Fine, but small |
| Storage | One dropdown | Fine, but small |

Across the pages:

- Some settings applied at once and others needed Save, with no visible rule. Theme, writer and model choices applied at once; endpoint, vault, style and name needed Save.
- Wording assumed knowledge: ONNX Runtime, CUDA, Multilingual, Keeps up, "Automatic (graphics card, then processor)".
- A choice was split from its details: pick "On this computer" on Summaries, then go to Models to choose which model.

## Principles

1. Ask what only the user knows; work out the rest. Hardware and model fit are Earshot's job.
2. A choice sits with its details.
3. One obvious path, with detail one click away rather than on the page.
4. Nothing that needs a button press loses its promise: no download, sound or recording starts by itself.

## First run: four short steps

1. **Summaries.** On this computer (preselected), Claude, ChatGPT, or no summary. Own endpoint goes under "Other". Each option keeps its line saying what is sent where, in the same words as today.
2. **Download.** Meeting languages first, then one line and one button: "Earshot needs X GB for this computer", covering helpers, the recommended transcription model and a summary model only if step 1 chose this computer. "Choose different models" opens the full list. Downloads carry on in the background, as they do today.
3. **About me.** Your name, then the voice sample with a clear Skip. The sample needs the speech helpers, which is why this step follows Download: while they are still downloading it says so and enables itself when they arrive, and it never starts a download or a recording by itself.
4. **Optional extras.** Obsidian and the audio check as two rows, each opening when pressed. Then Finish.

Also:

- Next saves what is on the step, so no separate Save inside the wizard.
- One way out on each step (Back, Next), plus Skip setup at the top.
- Skip setup or Finish with no transcription model says that meetings need one, and that it is in Settings.
- Later and Never go: the download button is "now", and doing nothing is "later".

## Settings: seven pages, Models dissolved

In this order:

| Page | Holds | Comes from |
|---|---|---|
| General | Start with Windows, closing, shortcut, theme, updates, setup guide | General |
| Audio | Unchanged | Audio |
| System | Hardware, recommendation, speech helpers, where it runs, the model check, CUDA, benchmark | Models, Benchmark |
| Transcription | Languages first, transcription models, show words while talking, when speakers are found | Models, General, Speakers |
| Summaries | Writer; under "On this computer", the model; during the meeting; style | Summaries, Models |
| People | Your name, your voice, people | Speakers |
| Notes and storage | Obsidian, recordings retention, crash logs | Obsidian, Storage, General |

- Settings opens on General.
- Plain words: "Speech helpers" stays, ONNX Runtime goes to its detail text; "Multilingual" becomes "Many languages"; "Keeps up" says what it means.
- Save rule: a choice applies at once, typed text needs Save. Today's pages already nearly follow this; the rule makes it deliberate.
- Text elsewhere that names a page ("Open the Models page", "Settings, Models") follows the move.

No stored setting changes in any of this, except that the "never" record stops being written. An old record is ignored.

## Decisions

The owner, 2026-11-10:

1. **Summaries in first run:** "On this computer" preselected, Claude and ChatGPT one click away, each with its "what is sent" line.
2. **Voice sample:** in About me, with an option to skip. Consequence, set by the coordinator: the proposal had About me first, but the sample fails without the speech helpers, so About me follows Download and the languages moved onto the Download step, which they decide.
3. **The advanced page:** named System, always listed, between Audio and Transcription.
4. **Later and Never:** removed.
5. **Summary length:** left for later, perhaps as a few templated prompts (T12.17).

## What this design does not do

- Change what any setting does, other than where it appears and the points above.
- Change models, backends, privacy wording or downloads.
- Add settings.

## Build

T12.5a first run, T12.5b the Settings page moves, T12.5c the wording pass, in PLAN.md. `docs/ui-design.md` "Settings" and "First run" are updated in the same commits.
