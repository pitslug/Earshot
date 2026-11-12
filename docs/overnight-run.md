# Overnight run, 2026-11-12

Two machines work this batch: the desktop (coordinator) and the Surface (measurements). Each task's full spec is its entry in PLAN.md section 8; this file says who does what, in what order, and where each stops. AGENTS.md applies in full. Where this file and AGENTS.md differ, this file wins only on the points it names.

## Decisions already made by the owner (do not ask again)

- T9.17: when every track has failed to write, the meeting stops itself and notifies.
- T9.25: clear the variables that redirect Claude Code and Codex. The notice option is not built.
- T12.23: the NVIDIA Open Model License is agreed for evaluation only. Nothing ships.
- T12.25 and T12.26 are parked until T12.27 has judged Granite. Do not start them.
- T9.18 and T9.19 are built tonight but stay on `lane/recovery`, unmerged, until the owner's hand checks.
- No ci per merge. One `cargo xtask ci` on main at the very end. Read its exit before committing or pushing anything that depends on it.
- Pushing lane branches is allowed tonight. Main is pushed once, at the end, after ci passes.
- The release is built and signed tonight and left as a draft. The owner publishes it after a real call test in the morning. Never publish it yourself.

## How the two machines share work

They cannot message each other. The only channel is branches on `origin`.

- The pre-push hook pushes only HEAD and runs `cargo xtask public-guard --require-list`. Check a branch out before pushing it. If the hook refuses on the Surface because the private names list is missing, do not bypass it: keep working, leave results under `target\overnight\`, write a `git bundle` of the lane there, and say so in the report.
- A machine that needs another's branch runs `git fetch origin` between its own steps. If the branch is not there yet, it goes on with its next step and looks again later. After its own work is finished it looks every 20 minutes, at most 12 times, then stops and reports what it was waiting for.
- Lanes never edit PLAN.md, AGENTS.md, TODO.md or the private log. Each lane ends with a report file `docs/overnight/<lane>.md` on its branch: result per task, the accept commands with their output lines, numbers measured, anything for AGENTS.md, and what is left. The desktop coordinator applies those to PLAN.md and the private log when it merges.
- Timing figures are valid only on an idle machine. Never take a timing while a build, test run or another measurement runs on the same machine. Quality figures (WER, coverage, validity) may run beside builds.

## Desktop: coordinator and lanes

Start: `git status`, `git log --oneline -5`, then create the lanes below as worktrees from main with short target dirs (`C:\eb\<lane>`). At most 6 lanes at once.

### Lane `lane/recording` (ships): T9.17, T9.20, T9.21, T9.22, in that order

One lane because they touch the same files (`live/mod.rs`, `flac.rs`, `wasapi.rs`, `follow.rs`). One commit per task.

- T9.17 first. As soon as its commit exists, create `lane/recovery` from it (below).
- T9.20 and T9.22 need a reviewer each (capture contract, pipeline state machine). T9.22 removes its three TODO.md lines: report them for the coordinator, since lanes do not edit TODO.md.
- Every bound added is a named field with a clock-free test, and its doc says it is a judgement.
- If the night runs short, T9.21 is the one to leave: commit work in progress on `wip/T9.21` and report.

### Lane `lane/recovery` (does not ship tonight): T9.18, T9.19

Branched from `lane/recording` at the T9.17 commit. Reviewer on T9.18 (data model, deletion rules) and on T9.19 if it adds unsafe FFI.

- Finish the code and every automated accept command. The by-hand accepts (kill from Task Manager; restart Windows on both machines) are the owner's: write exact steps for each into the lane report.
- Push the branch. Do not merge it, do not mark the tasks `[x]`. It will need a rebase onto main after T9.20 to T9.22 land; do not do that tonight.

### Lane `lane/copy` (ships): T9.23

From main. Raw error text never reaches a window. Screens are judged by screenshot in light and dark on the Vite preview, served on the lane's own port.

### Lane `lane/ui-light` (ships): T9.24a

From main. Only the two fixes in T9.24a. T9.24 itself waits for the Surface profile.

### Lane `lane/cli-env` (ships): T9.25

From main. Clear `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, the Bedrock and Vertex switches and the proxy variables for Claude Code, and the equivalent set for Codex, taken from each CLI's own documentation or `--help`, not from memory. Prefer the known install folders over PATH. Update PRIVACY.md. Reviewer required (privacy).

### Lane `lane/granite` (evaluation only): T12.27

- Pins already looked up on 2026-11-12, to be verified again by the lane against Hugging Face before use: `ibm-granite/granite-4.1-3b-GGUF` commit `ab4701481089b58a082ef63cc1cee738887293ff`, file `granite-4.1-3b-Q4_K_M.gguf`, 2099501664 bytes, LFS SHA-256 `662b0626cd58f443baea23559b469df6576a81d349649c59413b36a9fb32eb29`; `ibm-granite/granite-4.1-8b-GGUF` commit `865b82c2e7970d82e3731278c88c57ae7138359c`, file `granite-4.1-8b-Q4_K_M.gguf`, 5347914400 bytes, LFS SHA-256 `ed902ac9eb6adce5a90c6a08c8ea201b50e23fdc5976d1cd0362006afac5309e`. Licence Apache-2.0 per the repo tags; confirm from the model card. Neither GGUF repo has a LICENSE file: record where the licence text was read.
- Order: manifest entries in set `llm-eval` and the `sum_eval` candidates; **push the branch as soon as that commit exists** (the Surface needs the pins for its timing run); check the chat template with the pinned llama.cpp; commit the rule in docs/summary-eval.md **before** any generation; then the dev split on the desktop GPU, its own `--out`.
- Judged round only if the rule's first test passes. Prior packets and keys live only in the main checkout's `target/sum-eval/judge-dev-*`: read them by absolute path, never copy or delete them.
- Stop after the tables and a recommendation. No test-split run, no default change.

### Lane `lane/nemotron` (evaluation only): T12.23 steps 1 to 3 on the desktop

- The download host must already be in `models/manifest.toml` `hosts` (`huggingface.co`, `github.com`, `raw.githubusercontent.com`). The package named in the task was researched, not fetched: confirm it exists and pin it, or stop and report.
- Order: pin; streaming decoder as an `earshot-onnx` example, checked against the reference implementation's text on the smoke clips; scan test that no shipped code names the loader; rule committed in docs/live-previews.md before measuring; `live_lag` evaluation loader.
- **Push the branch once `live_lag` can run it**, so the Surface can measure. The desktop CPU timing waits until every other lane has finished building (idle machine).
- If the decoder's text does not match the reference on the smoke clips, stop there and report. Do not measure a wrong decoder.

### End of the night on the desktop

1. Merge the finished lanes one at a time with `git merge --no-ff`: `lane/recording`, `lane/copy`, `lane/ui-light`, `lane/cli-env`, then the evaluation lanes that passed their accepts (`lane/granite`, `lane/nemotron`, and `lane/stretches` from the Surface if it is on origin). A reviewer reads each lane's combined diff before its merge, with the test and harness code. Update PLAN.md and the private log in each merge commit. An unfinished lane is not merged: it stays as its branch and is reported.
2. Never merge `lane/recovery`.
3. Take the idle-machine timings now (Nemotron on the desktop CPU).
4. One `cargo xtask ci` on main. Read the result. On a failure, fix and run again; never push on a red or unread ci.
5. Bump the version to 0.3.2 (`release: bump the version to 0.3.2`) and push main.
6. Build and check x64, build ARM64, sign both, as docs/updates.md and docs/arm64-release.md say. The signing key is at `%USERPROFILE%\.earshot-private\update-signing.key`.
7. Create the GitHub release `v0.3.2` as a **draft** holding both installers and both version files. A draft is not served to installed copies. Leave it a draft.
8. Delete each merged lane's `C:\eb\<lane>` with `cargo xtask clean-build <lane>`.
9. Write the morning note (below).

## Surface: measurements, one at a time

The Surface runs no shipped-code lane. Everything here is evaluation or measurement, on mains power, with nothing else heavy open. Use `EARSHOT_MODELS` and `EARSHOT_FIXTURES` at real paths if a person will need the files later.

Start from the brief's branch:

```powershell
git fetch origin
```

```powershell
git switch -c lane/stretches origin/overnight/2026-11-12
```

Queue, in order. Finish or park each before the next.

1. **T12.24 Shorter stretches** (owner of the whole task). Rule committed in docs/live-previews.md before measuring. Add the evaluation switch for `max_piece` (30, 20, 15, 10 s) without changing any shipped default. Measure settled-text lag with `live_lag` on `bundled` and `continuous`, and decode rtf on the NPU and the CPU. For accuracy: the `m2_prod` streams and AMI dev headset meetings decoded in pieces, never a test meeting. If the accuracy runs will not finish by the end of the queue, push what exists and say which runs are left; the desktop can run them from the branch. Push `lane/stretches`.
2. **T12.27 Surface timing.** When `origin/lane/granite` has the manifest pins: on a new branch `lane/granite-surface` from it, fetch the 3B and time it against the 4B on the CPU with the same dev meeting and settings, CPU-only build. Timing only: no quality claim from the Surface. Push.
3. **T9.24 before-profile.** On `lane/ui-profile` from the brief's branch: the React profiler on the Vite preview with a 3-hour mock meeting and a 300-meeting mock store, as T9.24 says. Numbers only, no fixes. If the profiler cannot be driven from the session, write the exact steps for the owner and move on. Push.
4. **T12.23 Surface run.** When `origin/lane/nemotron` can run in `live_lag`: on `lane/nemotron-surface` from it, the rule's Surface measurements with Parakeet on the NPU and the streaming model on 2 to 4 below-normal CPU threads. No NPU conversion tonight. Push.
5. Write `docs/overnight/surface.md` on the last branch pushed: every number, every command, what was skipped and why.

The ARM64 installer check (`cargo xtask bundle-check --installer ...`) must run from the owner's own PowerShell, so it is a morning step for the owner, not for the session.

## Stop and report instead of guessing

- Any stop condition in AGENTS.md section 5.
- A test that only passes by loosening a threshold or dropping a case.
- A download from a host not in the manifest.
- A measurement that contradicts a rule committed earlier: report the number, never adjust the rule.
- Anything that would publish, or change what a shipped build sends over the network beyond T9.25's clearing.

## Morning note (desktop writes it, last)

`docs/overnight/morning.md` on main, short: what merged, what did not and why, the ci result, the draft release link, and the owner's steps in order:

1. Install the x64 build from the draft and record one real two-minute call. Check the transcript and summary.
2. Run the ARM64 installer check on the Surface from your own PowerShell (docs/arm64-release.md) and compare the SHA-256.
3. Publish the draft, or say what failed.
4. `lane/recovery` hand checks: kill mid-recording; restart Windows mid-recording on both machines.
5. Decisions waiting: Granite recommendation, stretch length recommendation, Nemotron result.
