# Overnight report: lane/copy

## T9.23 Plain words and a way forward for the failures a new user meets

Result: done, one commit (`T9.23: Fixed copy and a way forward for the failures a new user meets`). No reviewer run: T9.23 is not on the AGENTS.md 3.5 list; the lane-merge reviewer reads it.

### What changed
- `plain_text::runtime_text` is exhaustive over `RuntimeError` and returns fixed copy chosen by the error's kind (sub-kinds for store, summary and capture errors), never its text. `IpcError::from` uses it for every error, so `IpcError.message` is never a runtime error's own text.
- Capture starts: `earshot_capture_wasapi::problem::start_problem` reads the HRESULT and the failing step: `E_ACCESSDENIED` -> microphone privacy switch (copy names Windows Settings, Privacy and security, Microphone), `E_NOTFOUND` -> no microphone (or no output on the render endpoint), `AUDCLNT_E_DEVICE_IN_USE` -> in use, device invalidated -> no microphone, anything else -> generic. New `RuntimeError::CaptureStart { problem, detail }` (`earshot_runtime::capture_start_error`), used by meeting starts, the audio check and the voice sample.
- Data folder that cannot be opened: IPC kind `data_folder` (`DATA_FOLDER_COPY`), UI `DataFolderScreen` whose Try again calls `restart_app` (the store opens once, at start, and the background loops start there, so a restart is the honest retry). A database saved by a newer Earshot keeps `newer_data`.
- A failed final pass is the new `SummaryEvent::PostFailed` ("Transcript not finished", Re-transcribe on the spot); a session that failed after the save is PostFailed too. A failed summary offers Re-summarise; a cancelled finish now also offers Re-transcribe. The buttons start the job and open the meeting's page, which shows its progress; a refusal shows as an alert on the finished meeting's screen. Notification for PostFailed: "The transcript was not finished".
- Downloads: `DownloadError::reached_no_server` (a transport error before any response) gives `NO_SERVER_ANSWERED`, which says a work network that needs a proxy may be why and that Earshot does not use one. No proxy support was added. README "What you need" says the same, and notes AVX2.
- AVX2: `earshot_runtime::cpu` (AVX2 and FMA, read once per process); `backends::select_whisper` and the llama.cpp loader refuse with `RuntimeError::ProcessorTooOld` and fixed copy instead of an illegal instruction. Parakeet and the helpers are untouched.
- Also fixed raw text: the meeting, job and model-job thread-spawn errors, the Credential Manager store's own words in `no_key_copy`, and the "no data directory" message.

### Accept and checks (this session, outputs seen)
- `cargo test -p earshot-desktop --bin earshot plain_text` -> `test result: ok. 14 passed; 0 failed`
- `cargo test -p earshot-desktop --bin earshot meeting_events` -> `test result: ok. 23 passed; 0 failed`
- `cmd /c pnpm --filter @earshot/ui test` -> `Test Files 36 passed (36)`, `Tests 476 passed (476)`
- `cargo test -p earshot-desktop --bin earshot` -> `385 passed; 0 failed; 7 ignored`
- `cargo test -p earshot-capture-wasapi` -> `162 passed; 0 failed; 21 ignored`
- `cargo test -p earshot-runtime --lib -- cpu download backends summary` -> `298 passed; 0 failed; 10 ignored`
- `cargo clippy -p earshot-capture-wasapi -p earshot-runtime -p earshot-desktop --all-targets -- -D warnings` (CLIPPY_CONF_DIR at the worktree root) -> clean; wasm32 clippy of earshot-capture-wasapi -> clean
- `cargo fmt --all -- --check` -> clean
- Not run: `cargo xtask ci` (lanes do not).

### Screenshots (Vite preview on port 1431, mock IPC, 1100x720)
- `?dataFolder=1`, light and dark: centred "Earshot could not open its data" heading, the fixed copy, a primary Try again button. Readable in both.
- `?summary=postFailed`, after a mock meeting is stopped, light and dark: heading "Meeting saved", error bar "Transcript not finished" with the fixed copy, primary Re-transcribe and secondary Open. Pressing Re-transcribe opened the meeting's page with "Re-transcribing from the recording".
- `?summary=failed`, light and dark: heading "Meeting finished", error bar "Summary failed" with the fixed copy, primary Re-summarise and secondary Open.
No Copy button was pressed.

### Tests changed because the task reverses what they pinned
- `plain_text_every_other_runtime_error_with_a_developer_command_is_replaced_too`: a capture error was asserted to pass its text through; it now asserts the fixed copy.
- `ipc_error_keeps_the_runtime_errors_retryability`: asserted the message equals `Display`; now asserts the fixed copy and that the meeting id is absent.
- `meeting_events_summary_results_map_with_plain_copy`, `meeting_queue_driver_a_session_failure_after_the_save_does_not_blame_the_recording`, `meeting_driver_a_session_that_cannot_start_is_an_ipc_error`, `summaries_no_api_key_skips_the_meetings_summary_with_a_banner`: updated to the fixed copy and `PostFailed`; every original property (retryable flags, "does not blame the recording", the start error's kind) is kept.

### Proposed AGENTS.md bullet
- Errors reach a window only as fixed copy chosen by kind (T9.23): `plain_text::runtime_text` is exhaustive over `RuntimeError` and `plain_text_every_runtime_error_maps_to_fixed_copy` builds one of each variant twice with different words, so a new variant needs a copy and a sample. Capture starts carry `CaptureProblem` from the HRESULT (`earshot_capture_wasapi::problem`); a store that cannot open at start is IPC kind `data_folder`, retried by restarting; a failed final pass is `SummaryEvent::PostFailed`. whisper.cpp and llama.cpp loads go through `earshot_runtime::cpu::require_native_builds` (AVX2 and FMA on x64).

### TODO.md lines to add
- `apps/desktop/src-tauri/src/audio_check.rs` `failure_copy`: `RuntimeError::Capture(m)` still passes the runtime's text through, and `earshot-runtime/src/audio_check.rs` builds one with `{e:?}` ("the capture failed").
- `apps/desktop/src-tauri/src/models.rs` `internal`, `remove_error` and `settings_error` still show catalogue and settings errors' own text (through `plain`).

### Left
- Nothing of T9.23's do text. Not checked by hand: a real microphone with the privacy switch off, a real proxy network, a processor without AVX2.

## Follow-up: lane-merge review findings (commit `T9.23: fix lane-merge review findings`)

- Blocking 1 (AVX2 refusal unreachable): `backends::accelerator_order` now goes through `accelerator_order_with(vulkan_build, native, vulkan_device)`, which asks whisper.cpp for its devices only when the processor runs the native builds (`cpu::runs_native_builds`). `summary_model::summary_gpu_here` goes through `summary_gpu_here_with(native, llama_gpu, order)`, which asks neither llama.cpp nor whisper.cpp without the baseline. Both seams are tested with fakes that panic if called. Every other caller was checked by grep: the only `gpu_devices` and `supports_gpu` callers are these two. Whisper and llama loads still refuse at `select_whisper` and `LlamaLoader`. The CUDA summaries trial and `probe_llama_gpu` run llama.cpp in a child process, so a crash there is a refusal, never the app. README is softened to what is proven ("not tried on such a processor").
- Advisory 2: a session that fails before the recording is saved, and a meeting-thread panic, are now `SummaryEvent::RecordingFailed { message }`. The screen shows "Recording not finished", headed "Meeting stopped", with only Open and no claim about what was saved. Notification: "The recording stopped". Tested in Rust (`session_failure`, the panic driver test) and the UI ("offers only Open when the recording itself did not finish").
- Advisory 3: the three `api_key_copy` strings had lost their line continuations and are fixed. New `plain_text_no_fixed_copy_holds_a_run_of_spaces` runs over every plain_text constant, the IPC, download and meeting copies, `runtime_text` of one error per variant, every API-key kind and `post_failed_copy`. It checks for no doubled space, no line break, no em dash, and a full stop at the end.
- Advisory 4: `problem.rs` now owns the step constants that `wasapi.rs` uses: the render endpoint, loopback activate and initialise (now named apart from the microphone's), process loopback, and render activate and initialise. On those output steps `E_ACCESSDENIED` is `Other`, never the microphone switch, and `E_NOTFOUND` means no output. New test `problem_access_denied_on_the_output_side_is_not_the_microphone_switch`. Note for the merge: this touches `wasapi.rs` (one `use` block and five `.ctx(..)` calls), which `lane/recording` also edits.
- Advisory 6: `MeetingDetailView`'s job summary switch has `PostFailed` and `RecordingFailed` arms and a `never` default.

### Accept re-run (follow-up)
- `cargo test -p earshot-desktop --bin earshot plain_text` -> `15 passed; 0 failed`
- `cargo test -p earshot-desktop --bin earshot meeting_events` -> `23 passed; 0 failed`
- `cargo test -p earshot-desktop --bin earshot` -> `386 passed; 0 failed; 7 ignored`
- `cargo test -p earshot-capture-wasapi` -> `163 passed; 0 failed; 21 ignored`
- `cargo test -p earshot-runtime --lib -- backends summary_gpu cpu download` -> `142 passed; 0 failed; 8 ignored` (both new seam tests seen by name)
- `cmd /c pnpm --filter @earshot/ui test`: 472 of 477 passed on a full run while other lanes were building. The 5 failures were all in `MeetingDetail.test.tsx` (title and line editing, word marks), none in code this follow-up touches. Re-run alone, that file failed 1 test ("keeps the word with Keep"), which then passed alone with `-t`. They read as timing under load: no bound was changed, and the coordinator's batch run decides.
- Clippy with `-D warnings` on the three crates (all targets), wasm32 clippy of earshot-capture-wasapi, and `cargo fmt --all -- --check`: all clean.

### TODO.md lines to add (review advisories 5 and 7, left as the coordinator asked)
- Advisories 5 and 7 of the T9.23 lane-merge review stay open. Their text was not in the message to this lane, so the coordinator writes their TODO.md lines from the review.
