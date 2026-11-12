# Overnight report: lane/recording (2026-11-12)

Tasks: T9.17, T9.20, T9.21, T9.22. `cargo xtask ci` was not run (the coordinator runs it on main). Every task ran `cargo fmt --all -- --check` and `cargo clippy -p <crates> --all-targets -- -D warnings` with `CLIPPY_CONF_DIR` at the worktree root: both pass. No timing was taken.

## T9.17 Keep the audio when a recording write fails: done (202d688)

- The FLAC writer writes each frame whole and remembers the length and stream info of the frames on disk. After the first failed write it takes no more samples; `finish` cuts the torn tail, writes a header for the whole frames (MD5 zero, "unknown") and renames; if even that fails the `.part` stays for T9.18. `abort` is used only by `start_live` before any audio. Test seam: `FlacTrackWriter::inject_fault(WriteFault::{DiskFullAfter, BrokenAfter})` (doc-hidden), reached from runtime tests through a `#[cfg(test)]` registry keyed by data folder (`live::write_faults`).
- The session finishes a failed track's file at once, sends `RuntimeWarning::RecordingStopped { track, at, disk_full, error }` once (also in `problems`), keeps the other track, and when every track has stopped sends `EveryRecordingStopped` and stops the meeting itself. `StoreError::is_disk_full` names a full disk.
- Desktop: banners for both; `shell::always_shown_toast` notifies on `EveryRecordingStopped` whether or not the window is watched (fixed copy), and the end's own notification is not repeated. CLI lines added.
- The final pass takes tracks of different lengths: `post_live_recording_cut_short_track_goes_through_the_final_pass`.
- Accept: `cargo test -p earshot-store flac` -> 10 passed; `cargo test -p earshot-runtime --lib live_recording` -> 11 passed (plus the post test, 1 passed).
- `lane/recovery` was created at 202d688 (not checked out, not pushed by this lane).

## T9.20 Stop and Quit never wait for ever on the capture thread: done (fcf3ecb), reviewed

- `ChannelSource::stop` runs its stopper on its own thread and waits at most `STOP_WAIT` (7 s, a judgement above the 5 s activation timeout; `with_stop_wait` for tests). A thread still running is given up (never joined): the next pumps deliver what it sent, a `Note { EndpointWarning::StoppedAnswering }` on the mic and one substitute `End` (`Stopped`, or `Failed` after a reported capture failure). A given-up thread that exits without its end takes the vanished-thread path (no note). Runtime: `RuntimeWarning::DeviceStoppedAnswering`; desktop banner `device-stopped-answering`.
- `MeetingDriver::stop_and_wait` returns after `STOP_CEILING` (15 s, a judgement) and now returns whether nothing records; rule `keep_waiting` tested clock-free.
- Reviewer: CLEAR, 3 advisories. Fixed: Failed reason after a failure or a vanished thread; STOP_CEILING doc. Declined: dropping the `cfg(windows)` guards (earshot-capture-wasapi is a Windows-only dependency of both crates, so the guards are needed).
- Accept: `cargo test -p earshot-capture-wasapi` -> 164 passed; `cargo test -p earshot-runtime --lib live` -> 187 passed; `cargo test -p earshot-desktop --bin earshot meeting` -> 107 passed.

## T9.21 Re-open the stream on any stream error: not started (stop and report)

PLAN says "Tests through the existing fake stream seam with an injected error on the third drain". No such seam exists: `Track` holds a real `CaptureStream` (COM) and `run_capture` has no fake path. Building one is a refactor of the unsafe capture loop. Proposed task before it: "T9.21a A fake stream seam for the capture loop" (a trait over `CaptureStream`'s drain/stop/start/event used by `Track` and `run_capture`, with a scripted fake). Nothing committed; no `wip/T9.21` branch (no work to keep).

## T9.22 A stuck meeting never blocks the ones after it: part done as T9.22a (c57860f), reviewed

Too large for one commit with care; split proposed:
- **T9.22a (done):** (1) one cancel token from `RecordingSaved` on: `LiveSession::cancel_catch_up`, wired from the driver's finishing token; (3) `MeetingModels::resolve_unless` and `cuda::wait_for_onnx_unless` poll the token; (2, live catch-up only) `worker::StallWatch` + `LiveWorker::finish_watched`: signs of life are the decoded frontier, the decode's `CancelToken::polls` (new: every `is_cancelled` counts) and worker messages; `LiveConfig::catch_up_stall` 120 s, a judgement. A stalled catch-up is given up (thread left), `RuntimeWarning::LiveCatchUpStalled`, and the desktop leaves the meeting unprocessed (`CATCH_UP_STALLED_MESSAGE`) so the next meeting's turn comes.
- **T9.22b (proposed, deps T9.22a):** no-progress watchdogs for a final-pass step and a local summary load (reuse `StallWatch` over the phase's `CancelToken::polls` plus `PostStage`/the llama heartbeat; a model load does not poll, so loads need their own bound or heartbeat), and for a live model load hung on the session's hold thread (`end_hold` joins it before the catch-up). Accept as T9.22's.
- Reviewer: CLEAR, 6 advisories. Fixed: wider negative-control bound (1 s, 100 ms polls), bounded joins in `tests/meeting_queue_cancel.rs`, the hold-thread gap named in the meeting.rs docs, `LiveTranscriptFinished` doc. Left: `meeting_queue::wait_for_the_library_choice` could call `wait_for_onnx_unless` (nit, for TODO.md).
- Accept: `cargo test -p earshot-runtime --lib live` -> 191 passed; `cargo test -p earshot-runtime meeting_queue` -> 20 + 1 + 1 passed; `cargo test -p earshot-desktop --bin earshot meeting` -> 109 passed.

## TODO.md (for the coordinator)

- Remove line 52 (`meeting_models.resolve()` ... no cancel): fixed by T9.22a.
- Rewrite line 16 to: "`host::after_meeting`: a wedged final-pass model call or local summary load holds the meeting with no limit (T9.22b)."
- Rewrite line 51 to: "a hung final pass or summary of an older meeting leaves a newer one `Waiting` until Quit (T9.22b); `wait_turn` itself has a cancel."
- Add: "crates/earshot-runtime/src/meeting_queue.rs (`wait_for_the_library_choice`): sleep-polls the CUDA choice; call `cuda::wait_for_onnx_unless` instead."

## Proposed AGENTS.md bullets

- Replace "`ChannelSource::stop`/drop blocks until the capture thread has sent its End and exited" with: `ChannelSource::stop`/drop waits at most `STOP_WAIT` (7 s, a judgement above the 5 s activation timeout) for the thread; past it the thread is given up (never joined), the next pumps deliver what it sent, a `Note { StoppedAnswering }` on the mic and one substitute `End` (Failed after a reported failure; a given-up thread that exits is a vanished thread). `MeetingDriver::stop_and_wait` returns after `STOP_CEILING` (15 s) and says whether the recording is saved.
- A failed FLAC write never deletes audio (T9.17): the writer counts only frames written whole, `finish` after a failure keeps them (MD5 zero), `abort` only for a start with no audio. Tests inject failures with `FlacTrackWriter::inject_fault`, from runtime tests through `live::write_faults`; never a real full disk.
- A live catch-up stalls only on no sign of life (`worker::StallWatch`: frontier, the decode's `CancelToken::polls`, messages), never elapsed time; every `is_cancelled` counts as a poll, so every shipped decode keeps polling its token inside long calls and only the worker polls a worker's token. A stalled or cancelled worker is left, never joined, and its stretch record is marked `stopped`.

## Left

- T9.21 (needs a fake stream seam first), T9.22b. PLAN.md status: T9.17 and T9.20 `[x]`; T9.22 stays `[ ]` with T9.22a `[x]` and T9.22b added if the coordinator takes the split; T9.21a added before T9.21.

## Lane-merge review follow-up (T9.17: fix lane-merge review findings)

1. A write failure before any whole frame: the writer is aborted (no AudioFile row, no `.part`), the warning still sent (`at` 0). Test `live_recording_that_failed_before_any_frame_keeps_no_empty_file`: both tracks fail on frame 0, no rows, the meeting stops itself, `load_post_input` finds no track.
2. The session no longer holds the store across the file work: `earshot_store::finalize_track_audio_outside_lock` reaches the store through a `WithStore` callback only for the track check and the row; `Store::finalize_track_audio` is that function with its own store. Test `audio_finalised_outside_the_lock_reaches_the_store_only_for_the_check_and_the_row` (the check before the rename, the row after; a gone track: one look, no row, `.part` kept).
3. `live_worker_token_is_polled_only_by_the_worker`: a source scan that `.cancel_token()` is taken only in `live/worker.rs`, once.
4. `LiveConfig::catch_up_stall` and `StallWatch` docs state the rule: no single call between two polls may take the bound (Whisper polls in its graph compute, Parakeet once per piece and chunk); a backend that breaks it polls more often or raises the bound.
5. `shell.rs`: `call_end_stopped` renamed `end_already_told` (and `ended_toast`'s parameter `already_told`).

Accept: `cargo test -p earshot-store flac` -> 10 passed; `cargo test -p earshot-store` -> all pass (audio 22); `cargo test -p earshot-runtime --lib live_recording` -> 13 passed; `cargo test -p earshot-runtime --lib live` -> 193 passed; `cargo test -p earshot-runtime meeting_queue` -> 20 + 1 + 1 passed; `cargo test -p earshot-capture-wasapi` -> 164 passed; `cargo test -p earshot-desktop --bin earshot meeting` -> 109 passed; `shell` -> 23 passed. fmt and clippy `-D warnings` clean on core, capture-wasapi, store, runtime, cli, desktop.

AGENTS.md: the bullet "An `AudioFile` row is written only by `Store::finalize_track_audio`" should read "only by `Store::finalize_track_audio` or `finalize_track_audio_outside_lock` (the same rules, the store reached only for the check and the row)".