# Lane npu-previews: T12.12c

Result: steps (1) to (4) done and committed (`T12.12c: provisional words on the NPU for the other people`). Step (5) is the owner's on the Surface. No reviewer subagent ran in the lane: the lane-merge reviewer should read the refusal tests listed below.

## Reports (step 1)

Copied unmodified from the owner's `OneDrive\live-lag` into `docs/live-previews/`, SHA-256 checked before and after the commit. Eight files, not seven as PLAN says: the seven `qnn2-*.json` (off, on, everyone for bundled and continuous, plus `qnn2-memory.json`) and `qnn3-memory.json`. All hold counts, timings, model ids and ONNX Runtime placement only: no transcript text, no path.

- qnn2-everyone-bundled 0a023dc6c237a17fa584d1d496c5a60b465da7f0ac471e7815965fa1b9a9789d
- qnn2-everyone-continuous 8f6cd59e1436d05e184ed06d9185fd140fa395a39c56006f90de3dd6608e121a
- qnn2-memory c3be5f6aec9b054be44db8bc19aafc4c6c9f6ad94017386b924efbfa76505410
- qnn2-off-bundled 8ef210ab6d69cb003acc2813b17f14e942136b8f8e04791bb03863e8461ebd65
- qnn2-off-continuous acdca0a6d7c6723a0fd303123604fee63650aa8b2f180ac66fa78ff28dd5ed14
- qnn2-on-bundled 48dd86ce2c975891b4b69b2e3574831605405e5ed7bf7ea3a841b4707550097b
- qnn2-on-continuous 535feca137460a177922ccec30e63dafb08aa2a863fe7d4688dd4c02a4a19b95
- qnn3-memory 6ff52f576c4bb29b3a55ac706c7c3f67ad68b6f01d1ba912e50981a77d310532

docs/live-previews.md has a new section "Previews on the NPU from the compiled model (T12.12c)": points 1 to 4 pass for both settings; point 5 as written fails (and would with previews off: 3.75 GiB free after a call's two sessions); the amendment is labelled as the owner's, made after the measurement. On the amended point "on" adds 0.77 / 0.22 GiB (bundled / continuous, peak working set over off) and "everyone" 1.47 / 0.85 GiB.

## Code (step 2)

- `backends::preview_backend`: QNN runs the previews' session on QNN, default memory limit `PREVIEW_MIN_MEMORY_NPU` (12 GiB, a judgement, documented); "on" and "everyone" lift it as on the CPU.
- `backends::second_preview_backend`: QNN refused with `OWN_VOICE_NPU_REFUSED` (says why: shown words vanished, not understood).
- `backends::qnn_load_steps`: a preview on QNN loads only a compiled model (a helper compile is allowed; never an in-process compile or the ordinary load), else `PREVIEW_NEEDS_COMPILED`, which is `PreviewReport.off`, never a failed meeting.
- `preview_settings`: `NOTE_NPU` ("Not available on this computer's NPU yet") removed; `NOTE_EVERYONE_NPU` for "Everyone" on an NPU lead; the page uses the NPU limit there. The `can_change: false` branch is now unreachable for every lead and keeps a generic `NOTE_NOT_HERE`.
- No reason or note contains a manifest id (tested with `catalogue::manifest_id_in`).

## Tests (step 3)

Refusal tests for the merge reviewer: `backends_previews_on_the_npu_run_there_above_the_limit_and_refuse_below`, `backends_previews_own_voice_stays_refused_on_the_npu_with_its_reason`, `backends_previews_on_the_npu_load_there_and_refuse_before_loading`, `backends_previews_follow_a_parakeet_model_without_its_npu_version_to_the_cpu`, `backends_preview_on_the_npu_loads_only_a_compiled_model`, `backends_preview_on_the_npu_never_takes_the_ordinary_load`; live sessions `live_preview_on_the_npu_runs_for_the_far_end_and_refuses_your_own_voice`, `live_preview_on_the_npu_below_the_limit_is_off_by_default_and_loads_nothing`, `live_preview_on_the_npu_is_off_on_battery_and_loads_nothing`, `live_preview_on_the_npu_stays_off_during_a_hold_and_its_catch_up`, `live_preview_on_the_npu_without_the_npu_version_costs_the_meeting_nothing`; page tests in `preview_settings`. The T12.4 scan tests pass unchanged.

Existing tests changed because the owner's decision changed the behaviour they pinned (no bound loosened): `backends_previews_stay_refused_on_the_npu_by_default` replaced by the tests above; QNN taken out of the "refused" lists in the T12.3c first-session test and `live_preview_runs_only_where_it_was_measured_and_fits`; `preview_on_cannot_override_the_npu_or_a_model_without_them` and `preview_everyone_cannot_override_the_npu` replaced; live_lag's app-loader tests now expect the far end to load and the own voice to be refused.

Accept results (desktop, other lanes building):

- `cargo test -p earshot-runtime --lib backends` -> `test result: ok. 63 passed; 0 failed; 5 ignored`
- `cargo test -p earshot-runtime --lib preview` (covers `live_preview`) -> `test result: ok. 69 passed; 0 failed`
- `cargo test -p earshot-runtime --example live_lag` -> `test result: ok. 12 passed; 0 failed`
- `cargo test -p earshot-desktop --bin earshot` -> `test result: ok. 380 passed; 0 failed; 7 ignored`
- `cmd /c pnpm --filter @earshot/ui test` -> `Tests 470 passed (470)`
- `cargo clippy -p earshot-runtime --all-targets -- -D warnings` (CLIPPY_CONF_DIR at the worktree root) clean; `cargo fmt --all -- --check` clean.
- The Surface `live_lag` run without `--force-previews`: not run (step 5).

## Vanished words (step 4)

`live_lag` gained `vanished_kinds` (counts only). Run on the desktop CPU (the pattern is not NPU-specific: T9.14's CPU run had 42), `--audio bundled --previews everyone`, three times; reports `docs/live-previews/t1212c-everyone-bundled-cpu{,-2,-3}.json`. Run 1 under load (up to 65%) had the far end's previews turned off and no mic vanishes. Run 2: 52 vanished, all settled on the mic within 2 s. Run 3: 40 vanished, all settled on the mic after they left (0 already settled, 0 far end only, 0 never settled). The echo filter removed 0 words. Stored words identical across runs.

Finding: not echo removal and not lost words, a flicker: shown words leave before the settled text that holds them lands, then come back as settled text. Recommendation: keep "Everyone" refused on the NPU; add a task to find what drops a shown word before its stretch lands on short stretches and fix it in `live::preview` (all backends), then re-judge "Everyone" on the NPU.

## Step 5: Surface commands (the owner)

From `lane/npu-previews`, natively on the Surface, no `--force-previews`:

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews on --out target\live-lag\qnn4-on-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio continuous --seconds 120 --previews on --out target\live-lag\qnn4-on-continuous.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews everyone --out target\live-lag\qnn4-everyone-bundled.json
```

Pass: in the first two, `preview_sessions[0].loaded.kind` is Qnn with `refusal_bypassed` false and `preview_refusal.app_first` null; in the third the `own_voice` entry's `not_loaded` is the own-voice reason and the far end still loads on QNN. Then one real call with words on screen within about 3 s.

## For the coordinator

- PLAN: T12.12c steps 1 to 4 done, step 5 pending; PLAN says "seven reports", there are eight. Add a task "Shown provisional words leave before their settled text lands on short stretches" (the step 4 finding), a dep of re-judging "Everyone" on the NPU.
- AGENTS.md, the provisional-words bullet: replace "cannot override the NPU or whisper" with: on the NPU the first session runs on QNN from the compiled model only (`backends::qnn_load_steps`: no in-process compile, no ordinary load) under `PREVIEW_MIN_MEMORY_NPU`; the user's own voice is refused there (`OWN_VOICE_NPU_REFUSED`) until the vanished-words task is done.
- TODO.md: nothing.
