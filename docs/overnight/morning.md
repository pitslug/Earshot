# Morning note, 2026-11-13

## Result

- **ci:** `cargo xtask ci` on main (08d3e11, every lane merged): all steps passed on the first run. The UI suite passed inside it, so the `MeetingDetail.test.tsx` failures several lanes saw were load.
- **Release:** v0.3.2 is a **draft**, built from 6244b40 (main, pushed): https://github.com/pitslug/Earshot/releases/tag/untagged-7e86eead4d19892cae8d
  - x64: bundle-check PASS, installer sha256 `f349eda1f5c4db95e905d77702dc9c9c0802eecb60111c576f9e25f53faa812e`.
  - ARM64: built and signed, not checked yet. Installer sha256 `C9764A3D20BF02CE176D40DC13E3D98CA6DACB2A29273866C40FABF74AD25450`.
  - Both version files are signed.

## Merged into main

Each lane had a lane-merge review before its merge. Advisories are in TODO.md; details are in `docs/overnight/<lane>.md`.

- **lane/recording:** T9.17 and T9.20 are done. T9.22 was split: T9.22a is done and T9.22b has been added. T9.21 was not started, because the fake stream seam its entry relies on does not exist; T9.21a has been added for it.
- **lane/copy:** T9.23 is done. The review blocked it once: the AVX2 refusal could never be reached, because ggml starts up earlier. It was fixed and re-reviewed.
- **lane/ui-light:** T9.24a is done. T9.24 itself still waits for the Surface profile.
- **lane/cli-env:** T9.25 is done, and the privacy review passed on its second round.
  - A user signed in only through `CLAUDE_CODE_OAUTH_TOKEN` or `CODEX_ACCESS_TOKEN` now reads as not signed in.
- **lane/npu-previews:** T12.12c steps 1 to 4 are done, and T12.12a is closed.
  - The 52 "vanished" words are a flicker, not lost words. T12.12d has been added for it.
  - There are eight reports, not seven.
- **lane/nemotron:** the T12.23 decoder is in. On the idle desktop, neither "Other people only" nor "Everyone" passes the rule:
  - Median lag and CPU are better.
  - On bundled audio, the far-end tail lag is about 36 s and 18% of shown words change.
  - Peak memory is 0.4-1.0 GiB above window previews.
  - The Surface decides.
- **lane/granite:** T12.27 is done. Granite 4.2 3B and 8B and Ornith 1.5 9B all fail the rule's first test, so the local writer stays on Qwen 3.5.
- **lane/stretches** (from the Surface): T12.24 measured that no shorter piece passes the rule. It stays open: the deciding lag runs were taken at 19-44% machine load, not idle.

## Not merged

- **lane/recovery** (T9.18, T9.19): done and pushed, never merged, as agreed. It branched before the T9.17 review fixes (29c334b), so its rebase takes those in.
- **Surface queue:** arrived later and merged in the morning, after one lane-merge review (clear, 4 advisories) and `public-guard --require-list` (ok, which also covers `lane/stretches`). Report: `docs/overnight/surface.md`.
  - `lane/nemotron-surface`: neither setting passes on the Surface either (`on` fails points 4, 5 and 6, `everyone` fails 5). T12.23 is closed: the window previews stay.
  - `lane/npu-previews-surface`: without `--force-previews` the far-end preview loads on the NPU (median 2.3 s on screen), and the mic preview is refused under Everyone. Only your real call is left for T12.12c.
  - `lane/ui-profile`: T9.24 before-profile. The 1 s clock and the 10 Hz level events re-render the whole app; a 3-hour transcript makes each commit about 280 ms. No fix has been started.
  - `lane/granite-surface`: Granite 3B is about 10% slower per token than Qwen 4B on the Surface CPU.

## Your steps, in order

1. Install the x64 build from the draft and record one real two-minute call. Check the transcript and summary.
2. Run the ARM64 installer check on the Surface from your own PowerShell (docs/arm64-release.md, at commit 6244b40). Compare its SHA-256 with `C9764A3D...AD25450` above.
3. Publish the draft, or say what failed.
4. `lane/recovery` hand checks: kill the app mid-recording, then restart Windows mid-recording on both machines. The exact steps are in `docs/overnight/recovery.md` on that branch, under "By hand (owner)".
5. Decisions waiting:
   - **Granite:** all three fail; I recommend keeping the writer paused. T12.25 and T12.26 are unparked by their condition, and starting them is your call.
   - **Stretch length:** under the rule as written, keep 30 s. If you want to act, re-run the lag idle first. 15 s is the value the numbers favour if point 6 becomes a tail criterion.
   - **NPU previews:** the preview's step sessions run at normal priority, not below normal. That is how your numbers were measured. Keep it, or change it and re-measure.
   - **CLI environment:** whether to replace the variable deny lists with an allow list.

## Housekeeping

- This note is committed on main but **not pushed**. Main was pushed once, at 6244b40.
- Lane build folders under `C:\eb` are deleted for the merged lanes. `C:\eb\recovery` stays.
- `C:\eb\lane-cli-env-review` was made tonight by a reviewer, and I left it.
- The lane worktrees under `.claude\worktrees\` are still there.
- The Granite and Ornith model files (about 13.5 GB) are still in the models folder.
- The subagents' pushes of lane/copy and lane/ui-light were refused by the permission check, so those two were merged from their local branches.
