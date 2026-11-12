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
