# Lane cleanup: review follow-ups for 0.3.3 (T12.30)

Branch `lane/cleanup` from main 9c1c645. One commit per item, except items 7, 12 and 13 (one function, one commit) and items 5 and 6 (the fix and its test). The coordinator adds T12.30 "Last night's review follow-ups" to PLAN.md.

## Items

1. Audio check failure copy (d5b3845). `audio_check::failure_copy` maps `RuntimeError::Capture(_)` to fixed copy `CAPTURE_FAILED`; the runtime's text (which can hold a device error's debug output) no longer reaches the page. The runtime's own `{e:?}` stays for the CLI. Test: `audio_check` tests assert the fixed copy and no debug text.
2. Model errors (a738720). `models::catalogue_copy` and `settings_copy` map every `CatalogueError` and `SettingsError` by kind; models only by display name; no file, path, URL or manifest id. Also `model_jobs`: unknown model, not downloading and downloader setup errors are fixed copy. The UI mock (`mockModels.ts`) follows. New test `models_errors_reach_the_page_as_fixed_copy_without_files_or_ids`.
3. Gone chosen output (a627588). `DeviceGone { what: GONE_OUTPUT }` is `StartProblem::NoOutput`; the two `what` words are constants in `problem.rs` used by `chosen_device`. Test extended (failed first).
4. README (cf96712). The proxy sentence now covers in-app update checks.
5, 6. Download progress (96e5688). `download::download`, the one function models, CUDA parts and update installers go through, thins its own reports with `ProgressThrottle`; `host::download_model`'s wrap and `with_throttled_progress` are gone. New loopback test `download_progress_of_many_network_chunks_is_thinned_on_the_real_path` (200 network chunks; failed with 201 reports before). `download_cancelled_keeps_the_part_for_a_resume` now sends its first transfer slowly so a thinned report still arrives mid-transfer, and also asserts the held report is sent at the end; no assertion removed.
7, 12, 13. Preview NPU loads (2d96bbb). `QnnLoadFor { Track, Preview }` is an explicit argument of `load_parakeet_with` / `load_parakeet_qnn` / `qnn_load_steps`, no longer derived from background threads. A preview never compiles (in process or the helper) and never takes the ordinary load: `PREVIEW_NEEDS_COMPILED`. `recorded_qnn_load` records only a track's load, so a preview cannot take the `npu-compiled` banner or replace `last_load`. Tests: `backends_preview_on_the_npu_loads_only_a_compiled_model` now expects the helper case to refuse (the owner's decision), `backends_preview_npu_loads_are_not_recorded` new; the T12.4/T12.3c scan tests pass.
8. Previews page (ac3bfda). `general::previews_view_on` takes the next meeting's model and order from `host::transcription_setup` and places it with `recommend::runs_on_here`. Test `previews_page_follows_where_the_chosen_model_runs` covers both cases; negative control: with the old lead it fails.
9. Codex env (1af77c2). `CODEX_REFRESH_TOKEN_URL_OVERRIDE` in `codex::CLEARED_ENV`, comment citing codex.exe 0.166.0 and the owner's 2026-11-13 decision; added to the test's `MUST_CLEAR` (failed first). `docs/overnight/cli-env.md` notes it is now cleared.
10. Claude Code probe env (357b497). The fake records its environment for `--help` and `--version`; tests assert the effort probe (via a summary run and `accepts_effort`) and the version query run cleared. One `assert_cleared` helper replaces three copies.
11. CUDA wait (1f9d359). `wait_for_the_library_choice` is `cuda::wait_for_onnx_unless(cancel) && !cancel.is_cancelled()`. New binary `tests/meeting_queue_gate.rs` proves through `MeetingHold` that a meeting sleeps at the gate and gives way to a cancel (timed out with the old sleep loop).
14. Live-segment index (eeb6426). Migration 4 `CREATE INDEX segment_live ON segment(meeting_id, pass, deleted)`, `SCHEMA_VERSION` 4, APPLIED hash, `tests/data/schema_v3.sql` (schema_v2 + migration 3, Python iterdump) pinned to migrations 1-3, migration test `tests/segment_live_index.rs`, plan test `store_segment_live_existence_is_answered_from_its_index` (covering index; negative control on schema 3). `tests/segment_edit.rs`'s exact `SCHEMA_VERSION == 3` became `const { assert!(SCHEMA_VERSION >= 3) }` as `tests/folder.rs` does.

## Test results (this lane, CARGO_TARGET_DIR=C:\eb\cleanup)
- `cargo test -p earshot-desktop`: 396 passed, 7 ignored; 1 passed.
- `cargo test -p earshot-runtime --lib`: 939 passed, 16 ignored. Plus `--test meeting_queue_gate`, `meeting_queue_cancel`, `meeting_queue_cuda`, `catalogue_clean_install`: pass.
- `cargo test -p earshot-store`: every binary ok.
- `cargo test -p earshot-llm --test claude_code --test codex`: 15 passed, 1 ignored each.
- `cargo test -p earshot-capture-wasapi`: 169 passed, 21 ignored.
- `cmd /c pnpm --filter @earshot/ui test`: exit 0 (after the mock change).
- clippy `-D warnings` (CLIPPY_CONF_DIR at the worktree root) on earshot-desktop, earshot-runtime, earshot-capture-wasapi, earshot-llm, earshot-store: clean. `cargo fmt --all -- --check`: clean.
- Not run: `cargo xtask ci` (lane rule), ARM64 `qnn` feature build (QNN code paths touched are not feature-gated).

## Reviewer
- Items 9-10: code-reviewer, CLEAR with 4 advisories; applied: the `assert_cleared` helper, the version probe's env, the cli-env.md note. TODO.md line and AGENTS wording are below.
- Item 14: code-reviewer, CLEAR with 2 advisories (AGENTS notes below).

## Proposed AGENTS.md changes
- T9.25 bullet: "each name cited from the vendor's docs" -> "each name cited from the vendor's docs, or from the binary with the owner's decision (today `CODEX_REFRESH_TOKEN_URL_OVERRIDE`, codex.exe 0.166.0)".
- T9.24a bullet: progress is thinned once, inside `earshot_runtime::download::download` (every model, CUDA part and update download); `with_throttled_progress` no longer exists, and a caller never adds its own throttle. Remove "(CUDA part and update downloads do not yet: TODO.md)".
- T9.1m bullet: the NPU preview "helper compile allowed" is no longer true: a preview's NPU load never compiles, in process or the helper (`qnn_load_steps`, `QnnLoadFor::Preview`), and is never recorded (`recorded_qnn_load`); whose load it is is an explicit `QnnLoadFor`, never derived from background threads. "the Settings previews page reads `accelerator_order().first()`, not the chosen model's placement, T12.12c" -> "the Settings previews page judges where the next meeting's model runs (`host::transcription_setup` + `recommend::runs_on_here`, T12.30)".
- New: errors on the model pages are only `models::catalogue_copy` / `settings_copy` (fixed copy by kind, display names only); the UI mock mirrors their texts.
- New (store): a migration whose cost scales with the data (CREATE INDEX, a backfill) holds the IMMEDIATE write lock for its whole run, so another process opening the store meanwhile fails Busy after `BUSY_TIMEOUT`; migration 4 is the first. The store runs no `ANALYZE` or `PRAGMA optimize`; adding either means pinning the `segment_live` plan on an analysed, populated table too.
- A held meeting waits for the CUDA choice only at `cuda::wait_for_onnx_unless` (never a sleep loop); gate tests through `MeetingHold` live in `tests/meeting_queue_gate.rs`.

## TODO.md
Remove the lines for each of the 14 review items above (audio check copy, model error copy, DeviceGone output, README proxy, CUDA and update progress, the progress loopback test, preview `npu_compiled::record`, previews page lead, `CODEX_REFRESH_TOKEN_URL_OVERRIDE` (TODO.md:146), Claude Code `--help` env, meeting_queue sleep-poll, preview helper compile, explicit preview flag, `has_live_segment` index). Nothing to add.

## Left
Nothing skipped. Pushing is left to the coordinator (brief: do not push).
