# Overnight report: lane/recovery (T9.18, T9.19)

Branched from lane/recording at the T9.17 commit (202d688). Not for merging tonight: both tasks wait for the owner's hand checks below. Neither task is marked done anywhere.

## Commits

- 64c1f93 T9.18: Recover a recording that was interrupted
- a360b8b T9.19: Finish the recording when Windows shuts down or the session thread panics

## T9.18 Recover a recording that was interrupted

Result: code and every automated accept pass. Reviewer (data model, deletion rules): no blockers, 6 advisory; 1, 2 (doc), 3, 5 and 6 fixed before the commit, 4 left (below).

What changed:
- `earshot_store::recover_part`: decodes each frame after the 42-byte placeholder header with claxon through a byte-counting reader (sync code, header CRC-8, subframes, frame CRC-16, frame number, block size), cuts after the last whole frame, writes STREAMINFO itself (MD5 zero; byte-equal to the writer's header apart from the MD5, pinned by a test), syncs, `rename_noreplace`. No whole frame: the `.part` is left byte for byte. Another process holding the `.part` open (exclusive open refused, Windows) is not touched. `describe_flac` reads a row-less finished file's header.
- `earshot_store::recover_track_file` (no store access) and `Store::adopt_track_audio` (the `finalize_track_audio` rules plus "meeting not deleted", never a second row).
- `sweep_orphan_audio` and `DataDir::cleanup_stale_parts` skip a folder named after a live meeting; deleted and unknown meetings' audio still goes after the grace period. Stale comments in `audio.rs` fixed (grace period, retention clock).
- Flushing (do item 2): the writer already writes each 4096-sample frame (256 ms) to the file in one unbuffered write, so a kill loses at most the frame being filled. No `BufWriter` exists and no fsync was added. Documented in `flac.rs`; nothing measured.
- `earshot_runtime::recover::recover_interrupted`: every live meeting with Track rows, no duration and no lease in this process; holds a `Processing` lease meanwhile; file work with the store free; duration from the shortest track; notice record `recovered-recording/<meeting>` (new `MeetingRecord::RecoveredRecording`, swept by deletion); retention clock not started. `take_notices` returns each once.
- Desktop: `recovery::spawn` runs recovery then the retention loop on one thread (replaces `spawn_retention`), emits `ShellEvent::RecordingsRecovered`; command `recovered_recordings`. UI: AppShell asks at mount and on that event and shows "Earshot closed during this recording. N minutes were saved." once, with Open meeting and Dismiss. `recordingsGone` covers Interrupted, with "No usable audio from this recording" on Re-transcribe.

Accept (all run in this session, CARGO_TARGET_DIR=C:\eb\recovery):
- `cargo test -p earshot-store flac` -> `test result: ok. 15 passed; 0 failed` (audio binary)
- `cargo test -p earshot-store audio` -> `ok. 10 passed` (audio binary), plus 1 + 1 + 1 in other binaries, 0 failed
- `cargo test -p earshot-runtime recover` -> `ok. 9 passed; 0 failed`
- `cargo test -p earshot-desktop --bin earshot` -> `ok. 385 passed; 0 failed; 7 ignored` (before the T9.19 tests; 389 after)
- `cmd /c pnpm --filter @earshot/ui test` -> `Test Files 36 passed (36)`, `Tests 475 passed (475)`
- also `cargo test -p earshot-runtime --lib delete` (28 passed) and `--lib local` (46 passed), clippy -D warnings on store, runtime, desktop, `cargo fmt --all -- --check`: clean.
- The byte-cut test runs 2310 cuts (about 25 s in debug; each recovery syncs).

Not done:
- Screenshots of the new bar in light and dark on the Vite preview were not taken (other lanes were building). The mock takes `recovered` in `createMockIpc` data, but no URL switch was added.
- Reviewer advisory 4: `DataDir::cleanup_stale_parts` still has no production caller; it now follows the live-meeting rule. Remove it or call it, owner's choice.
- Reviewer advisory 1b: `recover_then` does not report `RecoveryReport::failed` anywhere visible; a failure repeats quietly at each start.

### By hand (owner): kill during a recording

1. Install a build of this branch on the desktop (or run `cargo xtask build --app` and start `earshot.exe` from the build folder).
2. Start a meeting (Call or In the room) and talk for at least two minutes.
3. Open Task Manager, find Earshot, End task (not Quit from the tray).
4. Start Earshot again.
5. Look for: within a few seconds a bar "Recording recovered: Earshot closed during this recording. 2 minutes were saved." (the number about what you recorded, rounded down). Open meeting goes to it.
6. The meeting's list line shows its length and "Draft transcript", not "Interrupted". Re-transcribe is enabled; press it and wait: a final transcript of what you said up to the kill.
7. Quit and start Earshot again: the bar does not come back.
8. Optional: in `%LOCALAPPDATA%\Earshot\audio\<meeting id>\`, `mic.flac` (and `loopback.flac` for a call) exist and no `.part` is left.

## T9.19 Finish the recording when Windows shuts down or the session thread panics

Result: code and every automated accept pass. New unsafe FFI (`shutdown_win.rs`), so reviewed: no blockers, 7 advisory; 2, 3 and 4 fixed, 5 to 7 left (below).

Finding: Tauri's `RunEvent::Exit` is delivered at a session end. tao 0.37.2 handles `WM_ENDSESSION` (wParam TRUE) on its hidden top-level event window by emitting `LoopDestroyed` (tauri-runtime-wry: `RunEvent::Exit`) synchronously on the main thread and then calling `std::process::exit(0)` (`tao-0.37.2/src/platform_impl/windows/event_loop.rs` around line 2340). So no window subclass was needed. Read from source only: the real restart is the owner's check.

What changed:
- `main.rs` uses `.build(..).run(..)`; on `RunEvent::Exit`, `shell::save_before_exit` stops a recording with `MeetingDriver::stop_and_wait_for(EXIT_SAVE_LIMIT)` (4 s, a judgement under Windows' about-5-second wait) while `shutdown_win::BlockReason` sets "Earshot is saving a recording." on the main window. The final pass and summary are cut; the meeting stays Unprocessed, as tray Quit does. Nothing happens when nothing records.
- `wait_until` (meeting.rs) is the bounded wait, tested clock-free.
- Session thread: `run` is under `catch_unwind`; `Session::after_panic` stops the source, finishes every writer as far as it got (the T9.17 path) with an `EndReason::Failed` end, sends `RecordingSaved` unless already sent (`recording_saved` flag), then `Ended(Failed)`; `wait` still returns `RuntimeError::Poisoned`, so the app shows its existing "The recording is saved, but the live transcript did not finish" message. A track whose finish itself panicked gets a `RuntimeWarning::Recording` saying it is saved at the next start. `FlacTrackWriter::frames_taken` added.

Accept:
- `cargo test -p earshot-runtime --lib live_recording` -> `ok. 13 passed; 0 failed`
- `cargo test -p earshot-desktop --bin earshot meeting` -> `ok. 108 passed; 0 failed; 1 ignored`
- whole desktop bin: 389 passed; clippy -D warnings (runtime, desktop, store) and fmt clean.

Not done:
- Reviewer advisory 5: only one panic position is tested (inside `pump`). A panic after `RecordingSaved` (in `finish_workers`) is guarded by `recording_saved` but untested.
- Advisories 6 and 7 (nits): the copy-pin test and `#[cfg(windows)]` on `mod shutdown_win` left as they are.
- `EXIT_SAVE_LIMIT` bounds only the wait: a capture thread stuck in `stop` (T9.20's case) makes it expire, and recovery takes over at the next start. After T9.20 lands, the rebase should check this path.

### By hand (owner): restart Windows during a recording, desktop and Surface

Do each on the desktop and on the Surface.
1. Install this branch's build. Save and close other work.
2. Start a meeting and talk for at least one minute.
3. Start > Power > Restart, without stopping the meeting.
4. Look for: Windows restarts, possibly after a moment on its "apps are preventing restart" screen showing Earshot with "Earshot is saving a recording." (it may not appear if the save takes under about 5 s). Do not press "Restart anyway" for a few seconds.
5. After signing in, start Earshot (or let it start with Windows).
6. Look for: the meeting is in the list with its length and "Draft transcript" (saved by the shutdown path), or the "Recording recovered" bar (the shutdown path ran out of time and recovery saved it). Either way, Re-transcribe gives a transcript up to the restart.
7. Repeat once with Sign out instead of Restart.
8. Tell the coordinator which of the two you saw in step 6; the first means the shutdown path worked.

## For AGENTS.md (proposed bullets)

- Interrupted recordings (T9.18) are recovered only by `earshot_runtime::recover::recover_interrupted` (the app: `recovery::spawn`, before retention's first run) through `earshot_store::recover_track_file` (frames after byte 42 decoded and checked whole, torn tail cut, STREAMINFO with MD5 zero, `rename_noreplace`) and `Store::adopt_track_audio`. A `.part` or row-less `.flac` in a live (not tombstoned) meeting's folder is never swept by `sweep_orphan_audio` or `cleanup_stale_parts`, whatever its age. The notice `recovered-recording/<meeting>` is read once by `take_notices`; recovery never starts the retention clock.
- The desktop app's `unsafe` files include `shutdown_win.rs` (T9.19: `ShutdownBlockReasonCreate`/`Destroy` on the main window, fixed copy). Update the Credential Manager bullet's list.
- A Windows restart, shut down or sign-out reaches the app only as tao's `WM_ENDSESSION` -> `LoopDestroyed` -> `RunEvent::Exit` on the main thread, then tao's own `process::exit(0)`: anything that must happen before the process ends there goes in `shell::save_before_exit`, bounded by `EXIT_SAVE_LIMIT`, and must not wait on the main thread's event loop.
- A session-thread panic ends in `Session::after_panic`: `RecordingSaved` (guarded by `recording_saved`) and `Ended(Failed)` once each, and `LiveSession::wait` still returns `Poisoned`.

## TODO.md (proposed lines)

- `crates/earshot-store/src/layout.rs:121` `cleanup_stale_parts` has no production caller: call it or remove it.
- `apps/desktop/src-tauri/src/recovery.rs:41` `recover_then` drops `RecoveryReport::failed`: make a repeating recovery failure visible (a count on the Storage page).
- `crates/earshot-runtime/src/live/tests.rs` test a session-thread panic after `RecordingSaved` (the `recording_saved` guard).

## Left for the owner

- The two hand checks above.
- After T9.20 to T9.22 land on main: rebase this branch (live/mod.rs and flac.rs overlap with lane/recording).
