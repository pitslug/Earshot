# Overnight report: lane/ui-light

## T9.24a Fewer download progress events and a cheaper meeting list

Result: done (one commit, `T9.24a: ...`). PLAN.md not edited (lane rule): the coordinator marks T9.24a `[x]`.

### What changed
- `earshot_runtime::download::PROGRESS_INTERVAL` (250 ms, at most 4 reports a second) and `ProgressThrottle` (time passed in, no clock): the first report and a complete one (`done >= total`) go out at once, a repeat of the last report sent is dropped, anything else inside the interval is held, and `finish` hands out the last held report. `with_throttled_progress` wraps a run and sends the held report after it ends, whatever the end (done, failed, cancelled).
- `host::download_model` (the only source of `ModelJobEvent::Progress`, through `HostModelWork`) runs `Catalogue::download` inside `with_throttled_progress` with `Instant::now`. The desktop's `ModelWork` fakes bypass it, so the model job tests keep their exact progress sequences.
- `MeetingRepo::has_live_segment(meeting, pass)`: new trait method with a default from `segments` (the old rule); the SQLite store overrides it with `SELECT 1 FROM segment WHERE meeting_id = ?1 AND pass = ?2 AND deleted = 0 LIMIT 1`. Pinned by `repo_contract::meeting_repo` (both repos), which compares it with `live_segments` for both passes, a tombstoned row and a meeting with none.
- `Services::meeting_activity` (what the meeting list reads per meeting) asks `has_live_segment(.., Final)` instead of loading every Final segment.

### Accept
- `cargo test -p earshot-runtime download` -> pass (58 passed in the lib, including the four new `download_progress_*` / `download_throttled_*` tests)
- `cargo test -p earshot-desktop --bin earshot` -> pass (381 passed, 7 ignored; includes `list_meetings_final_transcript_answer_equals_loading_the_segments`)
- `cmd /c pnpm --filter @earshot/ui test` -> pass on the second run (36 files, 470 tests). The first run, while other lanes were building, exited 1; its output was cut to the last lines by my own pipe, so the failing test is not known. This lane changes no UI or TypeScript file, so I treat it as load, not this change: the coordinator's batch ci is the check.

Also run: `cargo test -p earshot-core --lib model` (23 passed), `cargo test -p earshot-store` (all passed), `cargo test -p earshot-runtime --lib services` and `activity` (pass), `cargo clippy -p earshot-core -p earshot-store -p earshot-runtime -p earshot-desktop --all-targets -- -D warnings` with `CLIPPY_CONF_DIR` at the worktree root (clean), `cargo fmt --all -- --check` (clean). `cargo xtask ci` not run (lane rule); `purity` is worth watching for the core trait change (a default method using only core types).

### Reviewer
None required by PLAN.md for T9.24a; the lane-merge review covers it.

### Proposed AGENTS.md bullet
- Download progress (T9.24a) is thinned once, `earshot_runtime::download::ProgressThrottle` (`PROGRESS_INTERVAL` 250 ms; first and complete reports at once, the last held report always sent through `with_throttled_progress`), applied in `host::download_model`; a new download source that feeds a window event goes through `with_throttled_progress`. Whether a meeting has a final transcript is `MeetingRepo::has_live_segment` (SQLite: one indexed row), never `live_segments(..).is_empty()` in a per-meeting loop.

### TODO.md
- Add: `apps/desktop/src-tauri/src/cuda_settings.rs:1062` CUDA part downloads still send `CudaEvent::Progress` per network chunk (`host::download_cuda_part` is not throttled; T9.24a covered only `ModelJobEvent`).

### Left
Nothing of T9.24a. T9.24 itself waits for the Surface profile.
