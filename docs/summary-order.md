# Summary writers in order (T12.63)

Status: T12.63a (runtime) and T12.63b (app) built. Tasks T12.63a (runtime) and T12.63b (app), PLAN.md section 8. Spec review, 2026-11-16: round 1 four blocking findings, round 2 one (the hand-back protocol), round 3 one (an unreadable run record, adopted as the reviewer wrote it), all addressed below.

## 1. Why

Today two sections of the Summaries page decide who writes a summary:

- "Who writes the summary": one writer on this computer (the local model, the user's endpoint, Claude through Claude Code, OpenAI through Codex, or no summary).
- "Another computer" (T11.3db): optionally one of the user's other computers, tried first, with this computer's writer running when it is not available.

That is already an order of two steps, split over two sections and fixed at two. With a summary worker (T11.6) in the vault the owner found it clunky and asked for one ordered list, for example "my own server, then Claude, then this computer". Re-summarise offers only "Here" or "On <computer>"; the owner asked to choose any writer there, so a summary they do not like can be written again by another.

## 2. What changes for the user

- One section, "Who writes the summary", holds "Write summaries" or "No summary", then one list of every writer this computer can use (T12.68, which replaced T12.63b's step 1 choice and "If it can't" selects). Each row has a tick box; the ticked writers come first, in the order Earshot asks them, each with its rank ("Step n" to a screen reader) and up and down buttons (the first cannot go up, the last ticked cannot go down). At most three are ticked: a fourth tick is refused with fixed copy and stores nothing. No drag and drop.
- The writers are:
  - **On this computer**: the local model. Listed greyed with "Not downloaded" while no summary model is installed; it cannot be ticked then (a ticked one stays and can be unticked).
  - **A computer of yours**: a paired computer or worker whose presence offers summaries, shown "On <name> (<writer kind>)" as today.
  - **Your endpoint**: the one endpoint the page already configures, labelled by its provider: "Anthropic API" or "Model server (<host>)". There is still one endpoint configuration. Without an address and model it is listed greyed with "Not set up" and cannot be ticked; its form shows below the list, and saving it ticks it last when there is room (with three ticked, the save is refused with the same fixed copy). A set-up endpoint that is not ticked keeps its form below the list, and saving it changes only its fields.
  - **Claude (through Claude Code)** or **OpenAI (through Codex)**, with their existing model and effort fields. They stay tickable whatever their sign-in: the page asks the CLI only once its card shows (it is ticked), and a run that finds it not signed in moves to the next writer (4.4).
- "No summary" is a choice of its own instead of a step: it clears the order and greys the list (no writer's form shows under it). "Write summaries" from no summary starts with this computer; unticking the last writer is no summary.
- A writer appears at most once.
- Each row says where the transcript goes, in fixed copy: "Stays on this computer", "Goes to <name>, which writes it with its <writer kind>", "Goes to <host>", "Goes to Anthropic", "Goes to Anthropic through Claude Code", "Goes to OpenAI through Codex"; the Claude Code and Codex rows also carry their privacy line before they are ticked (PLAN 3.9). Under the list, one line says what a meeting kept on this computer does with the order (4.2).
- The "Another computer" section is removed.

## 3. Stored setting

- Device-local `settings/summary-order` (`DeviceRecord::SummaryOrder`), framed with `encode_versioned`: version 1, JSON `{"steps": [...]}` or `{"none": true}`. A step is `{"writer": "local" | "endpoint" | "claude-code" | "codex"}` or `{"computer": "<device id>"}`. At most 3 steps, no writer or computer twice, at least one step unless `none`. Validation also requires the endpoint fields (`summary_settings::validate`) whenever an endpoint step is present, wherever it sits.
- Written only by `summary_order::set_summary_order`. The order and the mirrored old keys (below) are written in one transaction, together with the writer settings when the page changes both. `set_summary_settings` and `offload::set_chosen_helper` stop writing `settings/summary-writer` and `settings/summary-offload-device` themselves: they go through `set_summary_order` or are removed, so the old keys have exactly one writer.
- Read only through `summary_order::summary_order(store)` -> (`SummaryOrder`, notes).
- **Old keys, both directions.** On every write, the old keys get a mirror an older build reads safely:
  - `summary-writer` = the order's first writer step, or `none` when the order has no writer step;
  - `summary-offload-device` = the computer only when it is step 1, else empty.
  On read: with no stored order, or when the stored old keys differ from the mirror of the stored order (a downgraded build changed them), the old keys win and are read as `[computer]` (if set) then `[writer]`; a writer `none` with a computer set reads as `[computer]` alone, and `none` with no computer as `none`. So a choice made on an older build is never overridden by a stale order.
- An unreadable or refused stored order reads as the old keys, with a note (fixed copy, never the stored text).
- Per-writer settings (endpoint fields and key, Claude Code model and effort, Codex model) are unchanged and apply wherever their writer sits.

## 4. Running the order

### 4.1 The meeting's run record

Each summary attempt is one **order run**, stored device-locally as `summary-run/<meeting>` (`MeetingRecord::SummaryRun`, `version u32 LE | JSON`, fixed words only, swept by meeting deletion):

- `origin`: `order` (an automatic summary or "Use my order") or `named` (a re-summarise naming one writer);
- `steps`: the steps of the run, copied from the order when the run starts;
- `cursor`: the next step to try;
- `outcomes`: one `{step, word}` per step tried (words in 4.4);
- `waiting_on`: the request id while a computer step waits;
- `state`: `running`, `waiting`, `continuing` or `ended`.

Writers and locking: the record's writers are `after_meeting` and a summary job (including `run_steps`' per-move write, T14.66), the hand-back and `end_after_done` in `Offload::step` (also `step_sync_stopped`), and `stop_waiting` (T14.66; its legacy record only while none exists), and every write is a read-modify-write in one store transaction under the store lock, re-reading the record inside it.

- **Starting a run** replaces the record, and in the same transaction withdraws any open request this device asked for the meeting, so a new run never attaches to an older run's request.
- **Asking a computer**: `ask_for` sets `waiting_on` and `state: waiting` inside the transaction that writes the request, so no open request exists without a record naming it.
- **Ending**: `state: ended` is written in the summary's own write transaction (`store_summary`), or when the run stops or every step has failed. An ended record stays as the last run's account (banner, CLI) until the next run. A record not `ended` after a restart is an interrupted run: the banner says "The summary was interrupted" and nothing is retried by itself, except the hand-back case below.

The run starts **when the summary is due** (`host::after_meeting` after the final pass; a re-summarise job at its start), never at meeting start. The order, the API key and the Claude Code and Codex checks are read then, so a writer removed from the order during a meeting never receives that meeting. The meeting-start checks stay for the start banner only (4.5).

### 4.2 Which steps a meeting may use

When the run starts, its steps are filtered by the meeting's privacy:

- A meeting kept on this computer (T12.13), in a run with `origin: order` and a non-empty order (never `none`, which stays no summary), drops every endpoint, Claude Code and Codex step, and **always ends with an implicit local step** when the order holds no local step. A named run is not filtered: section 5 decides it. This keeps T12.13's rule: today a kept-local meeting whose writer is remote gets the local model instead, and that stays true. A computer step stays: the other computer writes it with its local model or declines `kept_local_no_model` (T11.3da, unchanged).
- With nothing left (no local model installed and no computer step), the run ends with the existing kept-local result (`SkipReason::KeptLocalNoModel`).

### 4.3 Trying a step, and when the next one runs

The run's steps are tried in order inside the meeting's existing queue turn and lease (`after_meeting` or the job's `run_summary`). Each step builds its own service (the per-step equivalent of today's single `PreparedSummary`). The next step runs only when the tried step returned one of these, a closed list:

| Step | Moves on for |
|---|---|
| Computer | not asked (sync off, relay unreachable, offline, not offering, recording, no writer, no final transcript, request not written); withdrawn (no claim in 90 s, helper stale 5 min); declined (any reason) |
| Local model | `SummaryPlan::NotInstalled` or `KeptLocalNoModel`; every `LocalSummaryError` variant from load or generation (the builder lists them in the table's test, including `GpuStalled`, `LoadStalled`, `GpuChildDied`, `CudaFilesChanged`, each after its own CPU retry); an invalid reply after the repair rounds; `LlmError::Prompt` (the transcript does not fit this model) |
| Endpoint | `SummaryPlan::NoApiKey`; `LlmError::Http`, transport errors, `LlmError::Refused`, `LlmError::FormatRejected`, `LlmError::Invalid` after the repair rounds, `LlmError::Prompt` |
| Claude Code, Codex | `ClaudeCodeUnavailable` / `CodexUnavailable` at the run; `ClaudeCodeError` / `CodexError` from the run (`sign_in_busy`, `access_denied`, `tool_ran`, `unknown_event`, timeout and the rest); `LlmError::Invalid` after the repair rounds, `LlmError::Prompt` |

Anything else **stops the run** with no next step, and the run's failure is that error as today: cancellation, `RuntimeError::Conflict` (transcript changed or meeting deleted), `MeetingBusy`, store errors, `NotRecorded` (a handover record that could not be stored), stalls of the after-meeting watch (T9.22), and any error not in the table. A builder adds a case to the table only with a test.

A **computer step** suspends the run: the request is written, `waiting_on` is set, and the turn ends as today (`SummaryResult::Offloaded`, "Waiting for <computer>"; a job ends with a new terminal `JobEvent::Offloaded` instead of hanging). When the computer stores the summary, the run ends. When it withdraws or declines, the asker's side of `Offload::step` (`as_asker`) reads the run record and decides itself; `OffloadHost::write_here` becomes `continue_run(meeting, request)` and answers `Started`, `Refused` (the meeting is busy: handed again next step, as today) or `Settled`:

- **The record names this request in `waiting_on`, `origin: named`**: the run ends (`state: ended`, the computer's outcome recorded), nothing is written here, and the meeting says "<computer> could not write the summary: <word>" (fixed copy). Settled. A named choice has no next step (section 5).
- **Names it, `origin: order`**: a re-summarise job continues the **same run** from `cursor` (the steps copied at the start, never the current order). The job's first transaction records the computer's outcome, clears `waiting_on` and sets `state: continuing`. A restart during `continuing` finds the record not ended and no request named, so it is an interrupted run (banner, no retry), and the transcript is never sent to a later step twice.
- **No record for the meeting at all** (a request asked before this version): a legacy run. It continues as today, writing here with the order's first writer step under the meeting's privacy, as an ordinary re-summarise reads it, and records an ended run afterwards.
- **A record that cannot be read** (damaged, or a version this build does not know): fails closed. The request is settled with no job, nothing is written or sent, and the meeting shows the "The summary was interrupted" banner. Never the legacy branch.
- **A record that names another request, or none**: the request is settled at once with no job and no message. Settled requests are not read again in the process, and this replaces the 7-day re-hand for them.

**When the computer stores the summary**, it arrives through `apply_remote`, not this device's `store_summary`. The asker's step therefore also reads a `Done` request that a `waiting` record names, and writes `state: ended` with that computer as the writer (one transaction, re-reading the record). Readers also treat `waiting` plus a summary created after the request as ended, so a restart before that step never shows "interrupted" over a good summary.

A later computer step may ask another computer: the earlier request is closed by then, so "one open request per meeting" holds.

### 4.4 What the user is told

- Outcome words, fixed: `offline`, `recording`, `not offering`, `no writer`, `no reply`, `declined`, `not installed`, `no API key`, `not signed in`, `failed`, `not now`, `skipped (kept on this computer)`.
- A summary written by step 1 adds nothing to today's display.
- A summary written by a later step shows banner `summary-steps` (after `Ended`, and as the job's terminal warning for a job): "<writer> wrote the summary. <step> <word>; <step> <word>."
- Every step failed: no summary, and the same banner says "No summary. <step> <word>; ...".
- For a **one-step order**, the banner text is today's banner for that cause (`summary-key`, `summary-model`, `summary-offload`, `summary-kept-local`, the Claude Code and Codex banners), so a one-step user sees no change. These keys remain for that case only.
- `HostOutcome.steps` carries the run's outcomes for the CLI (one line per step). `HostOutcome.network` and `handovers` are the union over every step tried, so the audit lists every endpoint any step reached.

### 4.5 Start checks, the queue, live summaries and the benchmark

- **Start banner.** At meeting start the checks run for each step of the filtered order (none for a kept-local meeting's dropped steps) and never block recording. A step known to fail is reported once: "<step> can't write summaries now (<word>); <next step> will." It is still tried at summary time.
- **Queue hold.** The hold is recorded once per ticket at meeting start (`meeting_queue`). `SummaryHold::of` treats the summary as Local when any step of the filtered order at that moment is the local model, so a newer meeting's live decoding never runs beside a llama load reached by a fallback (T12.39's `may_run_beside`, `BESIDE_PAIRS` empty). When the run starts, a local step that the recorded hold did not cover (the order changed during the meeting) is skipped with the outcome word `not now` only while a newer meeting's ticket is open, rather than run beside it; with no newer meeting it runs. A re-summarise job is outside the queue and takes no hold, so this rule does not apply to it.
- **Live summaries** run when the filtered order's first step is the local model, or when its first step is a computer and the next is the local model (today: a local writer plus a chosen helper runs them, and that stays).
- **Benchmark** runs the local step alone when the order's first writer step that runs on this computer (computer steps skipped) is the local model, so `[computer, local]` benchmarks the local model as before; otherwise it skips with `BenchSkip::NotOnThisComputer` (review of T12.63a: a chosen helper must not stop the Settings benchmark).

### 4.6 A computer answering a request

A computer or worker writing another computer's request runs **exactly one writer: the kind its presence advertised** (`WriterKind::of_order`, the first writer step of its own filtered order). It runs no computer step and no next step. Its failure declines the request (`failed`, unchanged), and the asker's run moves on. This keeps the asker's label true: "On Desk (local model)" never sends the transcript to Desk's Claude step or to a third computer.

## 5. Re-summarise

- The header's Re-summarise is a split button. Primary action: **Use my order** (a run with `origin: order`). Menu: **Use my order**, then every writer that can run here now (the local model if installed; the endpoint if configured, with a key when it needs one; Claude Code and Codex when installed; each computer that offers summaries and is online), labelled as in section 2.
- A writer chosen by name starts a run with `origin: named` and one step. Its failure fails the job with today's fixed copy (`api_key`, `claude_code`, `summary_kept_local`, and the rest). A computer chosen by name is refused when it would not be asked (today's rule), and a hand-back ends the run (4.3); this replaces today's write-here on hand-back.
- On a kept-local meeting, endpoint, Claude Code and Codex items are disabled with "This meeting is kept on this computer". The existing kept-local "Allow" action (one way, `keep_local::allow_writer`) enables them.
- Undo stays one step back. Not changed here.

## 6. First run

The wizard's Summaries step is the Summaries page's list of writers (T12.79): ticks, moves and No summary save at once, as on the page, and what is typed in a writer's form saves with Next. One difference: "On this computer" can be ticked before a summary model is downloaded, because the Download step that follows fetches the model whenever this computer is anywhere in the order.

## 7. Privacy

This replaces one rule. Today: "never a fallback to a local model or another provider". After: **a summary goes only to the steps the user put in the order, in that order, with one exception: a kept-local meeting may end with the local model (4.2), which keeps T12.13 and never sends content anywhere.** Each consequence below is pinned by a test:

- A writer not in the order is never tried, whatever fails (apart from the kept-local local step).
- A kept-local meeting never reaches an endpoint, Claude Code or Codex, from the order, from a computer step, or from a named re-summarise before "Allow".
- A computer answering a request uses only the writer kind its presence advertised (4.6).
- A named re-summarise has no next step, including after a computer's hand-back.
- A run continues from the steps copied at its start, never from a later order.
- No new network destination. Copy, banners and records hold fixed words, writer kinds, the endpoint's host and computer names only.

Review: T12.63a changes which provider receives meeting content, so it gets the privacy review AGENTS.md 3.5 gives network code, before merging.

## 8. Tests (T12.63a unless marked)

- Order read and write: validation, at most 3, duplicates refused, endpoint fields required; the mirror; old keys win after a downgrade edit; unreadable order reads the old keys with a note; one transaction (a fault between the order and the mirror leaves neither).
- The move-on table: each listed result moves on; cancel, conflict, busy, store error and an unlisted error stop.
- Kept-local: the remote steps dropped; the implicit local step; `KeptLocalNoModel` when nothing is left; no probe for dropped steps.
- The decision at summary time: a writer removed from the order during a meeting is not used.
- Computer steps: suspension, continuation from the copied steps, a named hand-back ending the run, two computer steps in one run; the request and `waiting_on` in one transaction (a fault between them leaves neither); a new run withdrawing the open request; a restart during `continuing` (interrupted, nothing re-sent); a request with no record (legacy, written here); a request the record does not name (settled, no job, not handed again); `continue_run`'s three answers.
- Kept-local filter: not applied to a named run or to `none`.
- The hold: a local step added mid-meeting is skipped `not now`; live summaries with [computer, local].
- An interrupted run's banner after a restart.
- An unreadable record on a hand-back: settled, nothing written or sent, never the legacy branch (a named run's damaged record is the case).
- A summary stored by the other computer ends a `waiting` record through the asker's step; `waiting` plus a newer summary reads as ended before that step runs.
- The helper's single writer: a helper whose own order is [local, Claude Code] with a failing local model declines and never calls Claude Code.
- `SummaryHold` Local when any step is local; live summaries and the benchmark rules.
- The audit union over steps.
- (T12.63b) Copy table both suites; the page's order editing and its save rule; the re-summarise menu, including disabled items on a kept-local meeting; the banners; the first-run step.

## 9. Not in scope

- More than one endpoint configuration.
- An order per meeting (a meeting's one-off choice is re-summarise).
- Retrying the same step.
- Undo beyond one step.

## 10. Copy

All copy here (step names, destinations, outcome words, banners, re-summarise items, the disabled reason, the named hand-back) lives in `fixtures/ui/summary_order_cases.json`, read by the desktop's tests and the UI mock's tests. `fixtures/ui/offload_copy_cases.json` keeps the helper and waiting copy.

## 11. T14.66: stopping a summary written elsewhere, and saying failed writers as they happen

**Status.** Built on `lane/offload-stop` (as built, after the lane review; the sections below describe the code).
- **Source.** T14.66 in PLAN.md section 8, plus the owner's additions of 2026-11-27 (11.6 to 11.8, 11.13, 11.14).
- **Review.** Spec review round 1 (2026-11-27) found 2 blocking and 6 advisory findings. All are addressed in this revision, which is not yet reviewed again.

### 11.1 What went wrong

A re-summarise ran the order [local model, Claude Code, a summary worker]. The local step and Claude failed and the run moved on, but:

- **Nothing said so.** The run sent nothing mid-run. The job then ended `JobEvent::Offloaded`, and `JobView` keeps only a job's last event.
- **The list showed no work.** It reads `MeetingState` from this process's leases only, and a run `Waiting` on another computer holds none.
- **Nothing could stop the worker.**
  - A withdrawal only stops a helper from storing: `offload::still_claimed` is checked in its write transaction.
  - The worker's token is cancelled only when the worker stops (`Worker::cancel`).
  - The desktop helper passes `CancelToken::new()` to `write_for_request`, so nothing ever cancels it.

### 11.2 Stop (the asker)

**Where.** The waiting banner gets one action, "Stop".
- The banner is `MeetingDetail.waiting_for`, built by `offload::waiting_banner` from `offload::waiting_shown` and `run_view`.
- Only the asker shows it: `waiting_shown` reads only requests this device asked.

**Command.** `stop_waiting_summary(meeting)`, in `apps/desktop/src-tauri/src/offload.rs`, is async through `ipc::blocking`.
- It calls a new `summary_order::run::stop_waiting(services, meeting) -> StopAnswer`.
- That is one `edit::write` transaction under the store lock, re-reading everything inside it.

The transaction does this, in order:

1. **Find the request.** Find this device's newest request for the meeting that is either:
   - open: `offload::open_request` (asked by me, not deleted, requested or claimed, `authored`); or
   - named by a record still `waiting` on it, whatever its state (the case 4 below).
2. **Already written.** If `summarised_since(request)` holds (a summary row can arrive before the request's `Done`), or the request is `Done`:
   - an open request is withdrawn all the same (a summary written by a third device must not leave the helper writing); the asker's step then settles it, because a summary exists since it was asked, so it is never handed back; a `Done` request is left as it is;
   - end a record waiting on it with that computer as writer, as `end_after_done` does;
   - answer `AlreadyWritten { device }`: `Some(computer)` only when that computer's `Done` is here (it wrote it); `None` when only a summary row was found, which another device may have written, and the copy then names no device (`stop.already_summarised`).
3. **Withdraw.** If the request is open, write it `Withdrawn`: the asker is its only allowed writer.
4. **End the run**, by the run record:
   - **The record waits on this request** (`waiting_on == Some(request)`, `state: waiting`). This also covers a request already withdrawn by the 90 s or stale rule whose continuation has not begun. End it in the same transaction with `put_own_tx`:
     - push the new outcome `OutcomeWord::Stopped` for step `cursor - 1`;
     - set the new `SummaryRun::stopped: Option<EntityId>` (`serde(default)`) to the request;
     - clear `waiting_on` and set `state: ended`.

     This is 4.1's "the run stops" ending. No later step runs, whatever the run's origin. If the hand-back had already been handed to the host, the continuation's `begin_continue` now finds the record not waiting. It settles with nothing run (`JobEvent::Settled`, dropped by the UI).
   - **No record** (a legacy request). Write a fresh ended record: `origin: named`, steps `[computer]`, outcome `stopped`, `stopped` set. It is written with `put_tx` only when the record is still absent inside the transaction. Without it, the next offload step would read the withdrawal as `HandBack::Legacy` and write the summary here.
   - **Unreadable, or naming another request or none.** Leave the record: `hand_back` settles those with no job.
5. **Answer.** `Stopped` if step 3 or step 4 wrote. Otherwise `NotWaiting`, for example when a continuation job already runs: that job is stopped by its existing Cancel (`JobRunner::cancel`).

**Getting the withdrawal out.** After a commit that withdrew, the command calls a new `SyncHost::tick_now`.
- It sets a new signal `urgent`, which `thread.rs` honours even while ticks are failing. Plain `nudge` is ignored while `failures > 0`, so the withdrawal could otherwise wait the asker's whole back-off, up to `MAX_BACKOFF` (15 min).
- `urgent` makes one attempt and leaves the back-off as it was.
- With sync off there is no thread, and the withdrawal waits in the outbox.

**Afterwards.** The asker's offload step sees a withdrawn request its record no longer waits on. `hand_back` answers `Settle`: no job and no continuation, also after a restart.

**Races.**

- **The helper's summary or `Done` arrives first.** Step 2: the summary shows and nothing is withdrawn.
- **Stop commits first, but the helper had already stored.** The helper stored before it pulled the withdrawal. Its summary row arrives and is kept (rule 7.2, and sync-spec 10.1 "a done answer stays last writer wins").
  - The stop note (11.7) is shown only while no summary was created after `SummaryRun::stopped`'s request. The reader looks that request up by id.
  - So the note never says "stopped" over a summary that came anyway.
- **A withdrawal written by a device that is not the asker.**
  - `authored` ignores it in every reader and in the helper's cancel check (11.3).
  - Stop only ever writes this device's own requests.
- **Sync off on the asker.** Stop still withdraws and ends the run here.
  - The withdrawal waits in the outbox, and the helper writes on until it finishes. No rule ends a helper's write because of the asker's presence.
  - Its summary, arriving when sync resumes, is kept as above.
- **Re-summarise pressed at the same moment.** `start` withdraws open requests and replaces the record in one transaction. Both transactions re-read.
  - Stop commits first: `start` finds nothing open and replaces the ended record.
  - `start` commits first: Stop finds no open request and no record waiting on one, and answers `NotWaiting`.

**What the user sees.**
- The command's result re-reads the detail and the list, and the waiting banner goes.
- The run note says `stopped_note`.
- A summary stored before the run stays shown. Undo is unchanged.
- The copy promises only that the computer was asked to stop. A worker older than this change, such as the 0.9.1 image, never cancels: it writes on, and its summary is kept as above (11.3).

### 11.3 The helper stops writing

**One registry, owned by the host.** The tokens live in the `OffloadHost`, not in `Offload`.
- `Offload` belongs to one sync thread (`Arc::new(Offload::new())` per thread in `thread.rs`), and `SyncHost::start_locked` replaces the thread on set up, join, sync off then on, and Sync now. A token held there would be orphaned while its write ran on.
- The desktop's `HostOffload` is made once and set with `SyncHost::set_offload_host`, so it outlives every thread.
- The worker's host becomes one shared `Arc<WorkerHost>`, made in `Worker::open_with`. `Worker::host` returns a clone of it, and `write_for` clones the `Arc` instead of rebuilding `Self`.

**The shape.** A new `offload::Writing` holds `Mutex<WritingState { tokens: BTreeMap<EntityId, CancelToken>, stopping: bool }>`. Each host embeds one.

`OffloadHost` gains two required methods (no default bodies, so every host says what it writes):
- `writing(&self) -> Vec<EntityId>`: the ids being written now;
- `cancel_writing(&self, request: EntityId)`.

`write_for(request)` keeps its signature. Under the registry lock it does one of three things:
- **Already being written** (a second hand, for example by a new sync thread): do nothing.
- **Stopping** (worker only): do nothing. The request stays claimed and is handed again at the next start.
- **Otherwise:** insert a fresh token, then spawn the write with it. A guard (`WritingEntry`) moved into the thread removes the entry when the write ends, panics or fails to spawn.

`Writing::cancel(id)` and the worker's stop (`Writing::cancel_all`, which sets `stopping`) take the same lock.
- `Worker::cancel` calls `cancel_all`.
- The worker's own `cancel` field goes.

**The check in `Offload::step_with`.**
- **Before** taking the store lock, it reads `host.writing()`.
- **During the scan**, it also wants any request with `target_device == me`, `state == Withdrawn` and `authored`, whose id is in that set.
- **After** the lock is dropped and before the per-request loop, it calls `host.cancel_writing(id)` for each.

This runs in every step, `step_sync_stopped` included. No store lock is held across a cancel.

The T14.50 skip rule: a scan never goes idle while `host.writing()` is not empty (lane review), so a withdrawal pulled by a later tick is always read, whatever the request looked like at this scan. A pulled withdrawal is also a row the tick applied, which forces the next scan.

**After a restart or a thread replacement.**
- **A new `Offload` finds the request claimed and hands it again.** `write_for` sees an existing entry and does nothing. The running write keeps its one token, and the next withdrawal cancels it.
- **After an app restart, there is no entry.**
  - If the withdrawal is pulled before the first step, the request is never handed: `as_helper` handles open requests only.
  - If the first tick fails, the step still runs (it runs whatever the tick's outcome), and the request is handed and registered. The next tick that pulls the withdrawal cancels it.

**A cancelled helper writes nothing.**
- `write_for_request` returns `HelperOutcome::Cancelled`: either `Err(_) if cancel.is_cancelled()`, or `wait_turn` refusing.
- It writes no decline. Sync-spec 10.1 lets the target write only `Claimed`, `Done` and `Declined`, and `move_request(Claimed -> Declined)` would find `Withdrawn` anyway.
- Its write transaction stores nothing, because `still_claimed` fails.
- `HelperOutcome::Cancelled`'s doc is corrected to cover both causes: the worker stopping (the request stays claimed) and a withdrawal (nothing more to do).

**The bound**, from the asker's commit:

| Part | Bound |
|---|---|
| Asker pushes | `tick_now`: at once, also while failing (one attempt) |
| Helper hears it | its next tick, at most `TICK` (30 s) after its last, plus that tick's pull and the step. A helper whose own ticks fail backs off up to `MAX_BACKOFF` (15 min) and hears nothing until it reaches the relay; it still never stores (`still_claimed`) |
| Endpoint writer (the worker always: its own model is an HTTP endpoint too; the desktop's endpoint or Anthropic) | in-flight request abandoned within `CANCEL_POLL` (20 ms, earshot-llm `client.rs`) |
| Claude Code, Codex (desktop helper) | the CLI's job killed within `earshot_proc::POLL` (10 ms) |
| Local llama on the CPU (desktop helper) | the next generated token (`generate_cancellable`). A model load in progress cannot be interrupted, so the run stops only after it, before its first request (see below) |
| GPU summary child | killed at the parent's next watch poll (`summary_child::watch_lines`) |
| Waiting for a queue turn | at once (`wait_turn(cancel)`) |

So on a healthy relay, writing stops within about 30 s, plus one tick and the writer's own delay.

**What an uninterruptible load costs the helper.** A local load is bounded by `cpu_load_limit` (600 s). Until it returns, the helper keeps:
- its meeting queue turn, so its own later meetings' summaries wait behind it;
- its work pass, which refuses the model check, benchmark and voice sample;
- the meeting's `Processing` lease.

Recording a meeting is never blocked.

**Old workers.** A worker image from before this change (0.9.1 and earlier) has no registry and never cancels: it writes on and stops only when its container stops. The worker must be updated for Stop to reach it. The Stop copy (11.9) says "asked to stop" for that reason.

### 11.4 The meeting list shows a summary written elsewhere

**Source.** A new `offload::meetings_waiting_shown(read: &GuardedRead, me) -> (BTreeSet<EntityId>, usize)`. It applies `waiting_shown`'s rule over guarded readers, so one damaged row never fails the list.
- **Requests.** Read with a new `GuardedRead::summary_requests_or_damaged()`, built like `summaries_or_damaged`. It keeps open requests that I asked, `authored` and not deleted, and counts the damaged rows.
- **Summaries.** Then one `summaries_or_damaged(meeting)` per meeting in that set, which is usually none or one, to apply `summarised_since`.
- **When it is read.** Once in `ipc::meeting_list`, inside `Services::live_meetings`' single lock, with the first meeting, as the folders are. There is no loop over every meeting.
- **Damage.** Damaged rows count as "no meeting waits" for them. Their number is reported against `damage::Scope::Total` in its own `damage::report` call, never added to `MeetingsAndFolders`: that figure counts only meeting and folder rows, so request and summary rows would make every list read ask for a scan (round 2, A1).
- **Errors.** A store error from these reads also gives an empty set: the list's own reads surface a real store failure. It never fails the list.

**Display.**
- A new field, `MeetingListItem.summary_elsewhere: bool` (bindings regenerated).
- `rowDot` counts it as `working`.
- `workWord` gives `list_elsewhere` only when no local work is heard: heard work wins.
- `MeetingState` is unchanged.

**Refresh.** The list is already re-read on:
- the meeting's `Summary` event;
- a job's terminal event;
- `SyncEvent.meetings`, which comes from `StepReport.meetings` and covers withdrawals and hand-backs.

A successful Stop re-reads it too.

### 11.5 Failed writers said as they happen

**The record is the one account.** Each move is written to the run record as it happens, and every surface reads the record. There is no keyed warning banner, so nothing can go stale, double up with the waiting banner (T12.81: one banner), or be dropped for a meeting that is not the shown one.

**Runtime.**
- In `run_steps`, after each `run.push(i, word)` for a step that was tried and left with a later step still to try, the run is written through `put_own(services, meeting, &run)`.
  - That is one transaction, with the record still `running` (or `continuing`).
  - A store error there stops the run as a store error does today.
- Then a new callback reports the move with ids only: `run_summary_order_reporting(.., on_change: &mut dyn FnMut())`.
  - `run_summary_order` is the same function with a no-op callback, so existing callers and tests stay.
  - It is a parameter rather than a `RunPlan` field, because `after_meeting` forwards it into its own `progress: &mut dyn FnMut(HostStep)`, which a shared plan built before it cannot borrow.
- Outcomes are unchanged: the callback only observes.

**Readers.**
- **Unfinished, while work runs.** `run_note(view, working, cx)` gives the new `moving` note while the record is `Unfinished` and the host knows work runs on the meeting. It is built from the record:
  - "So far: {steps}." from the moved steps;
  - plus "Now trying {next}." from the step at `cursor`, if any.
- **Unfinished, with nothing running.** Still "interrupted", as today.
- **Waiting.** The waiting banner (11.7, `earlier`) lists the moved steps. `summary_run` is already hidden while `waiting_for` is set.
- **Ended.** The `later_writer`, `no_summary` or `stopped_note` account replaces it.

**Triggers for a re-read.**
- **A meeting's own summary.** `host::after_meeting` passes `&mut || progress(HostStep::SummaryRunChanged)`, a new variant.
  - The driver's watch counts it as the summary phase (unwatched), so T9.22's timings do not change.
  - The driver maps it to a new `MeetingEvent::SummaryRunChanged { meeting }` with ids only. It joins the events the driver passes for an older meeting (`Ended`, `PostProgress`, `Summary`), so an older meeting's page still hears it.
  - `meeting_events::SentRecord` gets no fold arm for it: it sets no screen state (its catch-all arm ignores it).
  - `AppShell` handles it as `PostProgress Stored` is handled: `setDetailRefresh(countedFor(.., meeting))`.
- **A re-summarise.** `jobs::summarise_run_with` passes a callback that sends a new non-terminal `JobEvent::SummaryRunChanged { meeting }`.
  - `jobNotice` draws it as the running job's bar, as before it.
  - The page re-reads its detail when it arrives.
  - After the job ends `Offloaded`, the waiting banner still lists the steps, read from the record, so the account survives the job's end and a window reload.
- **CLI.** `earshot-cli` prints a fixed line at each change ("summary: a writer could not write it; trying the next"); its end-of-run lines say which writer and why.

### 11.6 No stale notes while a summary is written (owner addition 5)

`MeetingDetailView` computes `summaryInProgress = writingSummary || detail.waiting_for !== null`:
- `writingSummary` already exists: a re-summarise job on the meeting that is not terminal.
- `waiting_for` comes from the waiting banner's own source, `offload::waiting_shown`. It also turns null once a summary created after the request exists.

While `summaryInProgress` holds, the `summary_stale` bar is not rendered. That one bar holds both the earlier-name and earlier-transcript texts and their Re-summarise button. It returns when the run ends: the job's terminal event, or a detail read with `waiting_for` null. This needs no Rust change.

### 11.7 The header, the waiting banner and the run notes (owner addition 6)

**The header line.** `detail.ts::headerLine(d, writing)` takes `writing: { on: string | null } | null`.
- With a current summary, its part reads "Summary by <writer> · <in_progress>". Without one, it reads only "<in_progress>".
- `in_progress_here` is for a job here, and `in_progress_on` is for a run waiting on a computer.
- `{device}` comes from a new `MeetingDetail.waiting_device: Option<String>`: the computer's name, or `unnamed_inline`. It is built in `detail.rs` beside `waiting_for`, from the same request, through `StepContext::device`.

**Banners and notes.**
- **Waiting banner.** `waiting_banner` appends `earlier` when the waiting record holds moved steps. It reuses `step_outcome` and `joiner`.
- **`run_note`.** It gains two arms, placed before `named_hand_back`'s arm. Otherwise a named computer that was stopped would read as a hand-back.
  - **Ended, `stopped` set:** `stopped_note`, plus `earlier` when other steps moved first. Shown only while no summary was created after the `stopped` request (looked up by id). After that it gives `None`, and the summary speaks for itself.
  - **Unfinished, while working:** `moving` (11.5).

### 11.8 The header's buttons sit on their own row, as icons (the owner, 2026-11-28)

This replaces the width rule designed here earlier (owner addition 7), by the owner's decision.

**The layout.** The header is a column, at every width:
1. the title, which takes the full width and wraps;
2. the caption line;
3. one row of buttons (`role="group"`, "Meeting actions");
4. the Summary and Transcript tabs.

**The buttons.** All five buttons show only their icons at every width: Speakers (with its count badge), Export to Obsidian, Copy summary, Re-transcribe and Re-summarise (a split button that keeps its menu).
- Each keeps its accessible name and a tooltip with its label.
- The greyed Re-transcribe keeps its "Recording not available" (or finishing) description.
- Re-transcribe and Re-summarise get icons of their own (`RetranscribeIcon`, `ResummariseIcon`).

**Removed.** T14.55's width measuring (`headerFit.ts`: `measureRow`, `useLabelsFit`, the label and icon switching) and its tests. `headerButtons.test.tsx` now pins three things:
- the order: title, caption, button row, tabs;
- every button an icon with its name and tooltip;
- a long invented title kept out of the button row.

### 11.9 Copy (approved by the owner, 2026-11-28)

**Fixture.** The new keys go in `fixtures/ui/summary_order_cases.json`.
- The UI and its mock import them, the desktop's tests pin them, and `_comment` names each.
- `{device}` is filled with a function replacer, because a name can hold `$&`.
- Sentences that name a Settings page are pinned by `page_names_tests.rs` (a source-pinning test that each named page and section exists).

| Key | Draft |
|---|---|
| `words.stopped` | stopped |
| `earlier` | Tried first: {steps}. |
| `moving` | So far: {steps}. Now trying {next}. |
| `stopped_note` | You stopped this summary and asked {device} to stop writing it. Re-summarise to write it again. |
| `stop_button` | Stop |
| `stop_label` | Stop the summary on {device} (accessible name) |
| `stop.already_written` | {device} had already written the summary. |
| `stop.already_summarised` | A summary of this meeting had already arrived. |
| `stop_not_waiting` | This summary is no longer waiting for another computer. |
| `finishing_tip` | {label}: available when the summary is done. (a greyed header button's tooltip while the meeting finishes, led by its own label) |
| `stop_failed` | The summary could not be stopped. Try again. |
| `in_progress_here` | New summary being written |
| `in_progress_on` | New summary being written on {device} |
| `list_elsewhere` | Summarising on another computer... |

**Local failure kinds (11.13).** The kind words are `words.too_long` ("too long"), `words.reply_unusable` ("reply could not be used"), `words.card_stopped` ("graphics card process stopped"), `words.stalled` ("stopped making progress"), `words.load_failed` ("could not load"), `words.load_stuck` ("an earlier load is stuck"), `words.wrong_backend` ("loaded on the wrong device") and `words.cuda_changed` ("CUDA files changed"). The sentences go in one `local_failures` object:

| Kind | Draft |
|---|---|
| `too_long` | The meeting is too long for the summary model on this computer. Choose another writer in Settings, Summaries. |
| `reply_unusable` | The summary model on this computer wrote a reply Earshot could not use, even after asking it to correct it. Try again or choose another model. |
| `card_stopped` | The graphics card's summary process stopped before it finished. Try again: the next summary tries the card again. |
| `stalled` | The summary model on this computer stopped making progress. Restart Earshot and try again. |
| `load_failed` | The summary model on this computer could not be loaded. Try again, or choose another model in Settings, Summaries. |
| `load_stuck` | An earlier summary model load is stuck. Restart Earshot and try again. |
| `wrong_backend` | The summary model did not load where Earshot asked it to. Try again. |
| `cuda_changed` | The CUDA files changed after Earshot started. Restart Earshot so it can check them. |
| `other` | today's `LOCAL_SUMMARY_FAILED`, unchanged |

### 11.10 Privacy

- **Records.** The run record gains fixed words only (`stopped` and the kinds of 11.13) and a request id (`stopped`), which is device-local.
- **The child protocol.** `ChildLine::Failed` gains only a fixed kind word.
- **Events and fields.** `HostStep::SummaryRunChanged`, `MeetingEvent::SummaryRunChanged` and `JobEvent::SummaryRunChanged` hold a meeting id. `StopAnswer`, `summary_elsewhere` and `waiting_device` hold fixed words and a computer's name the page already shows.
- **Logs.** No log line is added.
- **Wire.** Nothing new is sent.
  - Stop writes the existing `Withdrawn` state, which only the asker may write.
  - `tick_now` adds one sync attempt, not a destination.
  - A cancelled helper sends nothing and writes no decline.
- **Older builds.** An older build reading a record holding a new word or the `stopped` field reads it as `Unreadable`, because `deny_unknown_fields` applies and the word is unknown. It shows "The summary was interrupted", which fails closed. `RUN_VERSION` stays 1, because the record is device-local and a downgrade is its only other reader.

### 11.11 Contradictions found in the code

1. **The desktop helper can never be cancelled.** `HostOffload::write_for` passes `CancelToken::new()`, so its write is never cancelled, not even on quit. Sync-spec 10.1's "a cancel leaves it claimed" holds only for the worker.
2. **`HelperOutcome::Cancelled`'s doc is wrong after a withdrawal.** It says the request "stays claimed and is handed again at the next start". After a withdrawal the request is withdrawn, and nothing hands it again.
3. **4.1's list of record writers was incomplete.** It names three writers of the run record. `end_after_done`, `step_sync_stopped`'s hand-back, and now `stop_waiting` and `run_steps`' per-move write also write it. 4.1 is updated in this revision. Each writer is a read-modify-write in one transaction:
   - the per-move write and `stop_waiting`'s ended record through `put_own_tx`;
   - the legacy Stop record only while the record is absent.
4. **A stopped run has no account.** `steps_text` returns `None` for a run that ended with untried steps. 11.7 adds a note for `stopped` only.
5. **`measureRow` ignores the title** (11.8).
6. **The list could never show 10.1's waiting state.** Its working dot reads only this process's leases.
7. **A GPU run's failure loses its kind.** It crosses from the child as text only (`ChildLine::Failed { message }` -> `LocalSummaryError::ChildRun(String)`) (11.13).
8. **A stuck load cannot be told apart.** A stuck earlier load and a load on the wrong device both arrive as `earshot_llm_local::LlmError::Backend(text)`, so no rule can tell them apart without reading text (11.13).
9. **`autoHide.ts` names an exclusion no code applies** (11.14).
10. **Plain `nudge` is ignored while ticks are failing** (`thread.rs`, `soon && failures == 0`), so a host's "tick soon" waits for the back-off (11.2).

### 11.12 Tests (each with its control)

**Stop**

1. **Stop on a waiting order run.** `stop_waiting` withdraws the request and ends the run with `stopped` and the request id, in one transaction. An injected fault between the two writes leaves neither. Control: a record waiting on another request is untouched (`NotWaiting`).
2. **No continuation after a stop.** After a stop, the asker's offload step settles the request with no `continue_run`, and does the same with a fresh `Offload` (a restart). Control: the same request withdrawn by the 90 s rule continues the run.
3. **Stop on a legacy request.** With no record, Stop writes an ended record, the step does not write here, and the detail's run note equals `stopped_note` with the computer's name filled in. Control: a legacy withdrawal without Stop writes here (`HandBack::Legacy`).
4. **The summary arrived first.** A summary row applied before Stop, with the request still open, gives `AlreadyWritten`. A `Done` applied before Stop gives the same. In both, nothing is withdrawn and the run ends with that writer. Control: neither gives `Stopped`.
5. **A summary after the stop.** A summary arriving after Stop is kept, and the stop note gives `None`. Control: with no summary, the note shows.
6. **Withdrawn but not yet continued (case 4).** A request withdrawn by the 90 s rule, with the record still waiting: Stop ends the run with `stopped`, and a continuation handed before it settles with nothing run. Control: without Stop, the continuation runs the next step.
7. **Re-summarise in both orders.** Stop then `start`: `start` replaces the ended record and finds nothing open. `start` then Stop: Stop answers `NotWaiting` and the new run is untouched.
8. **Record format.**
   - A record holding an unknown outcome word decodes `Unreadable`.
   - A stopped record's bytes are pinned by `stopped_run_bytes_are_pinned`, written as literal JSON.
   - Control: today's record without `stopped` still decodes.
9. **The urgent tick.** While ticks are failing, `tick_now` causes one attempt before the back-off ends. Control: `nudge` does not.

**The helper's cancel**

10. **A pulled withdrawal cancels the write.** After a pulled, asker-authored withdrawal, the next step calls `cancel_writing`. `write_for_request` returns `Cancelled`, with nothing stored and no decline written. Controls: a withdrawal row last written by a third device cancels nothing; a request still claimed is not cancelled.
11. **No store lock across a cancel.** A fake host's `cancel_writing` takes `services.store()` with a bounded wait (the trait method is the seam).
12. **A thread replaced mid-write.** A write starts and `SyncHost` replaces its thread. The new thread's `Offload` hands the request again: `write_for` does nothing and no second token exists. The withdrawal, heard by the new thread, cancels the running write. Control: a different request handed by the new thread starts its own write.
13. **The owner's restart cases.** A fresh `Offload` starts with the request claimed here.
    - (a) The withdrawal is pulled before the first step: the request is never handed and nothing is written.
    - (b) The first tick fails: the request is handed and registered, and the next tick, which pulls the withdrawal, cancels it.
    - Control: with no withdrawal, (b)'s write runs to `Done`.
14. **Worker stop.** `cancel_all` cancels every token and refuses new writes; the entries go when their threads end. Control: a withdrawal cancels only its own request's token, under the same lock.
15. **The bound, by rendezvous.**
    - The endpoint mock signals when the summary request arrives, then the withdrawal is applied and the step run.
    - The test waits, under a 60 s guard, for `write_for_request` to return `Cancelled` and for the mock to see the connection close.
    - Control: without the withdrawal, the write has not returned when the test releases the mock, and it then ends `Done`.

**The list**

16. **The list's source.** `meetings_waiting_shown` equals `waiting_shown` meeting by meeting over a varied store: requested, claimed, withdrawn, done, asked by another device, authored by another device, and with a summary after. Control: an empty store gives an empty set.
17. **Damage never fails the list.** A damaged request row and a damaged summary row each leave the list readable. Their meetings are not marked, and the damaged count reaches `damage::report`. Control: the same store undamaged marks the meeting.
18. **The row (UI).** `rowDot` gives `working` and `secondaryLine` gives `list_elsewhere` for `summary_elsewhere`. Control: heard local work's word wins.

**Moves said as they happen**

19. **Each move is in the record.** Each tried-and-left step with a later step is in the record before the next step is built, and the callback fires once per move. The outcome equals `run_summary_order`'s over the same scripted writers. Control: no call for kept-local dropped steps or for a failing last step.
20. **A meeting's own summary.** The driver sends `SummaryRunChanged` for the meeting, also when it is an older meeting than the one shown, and the page's re-read shows `moving`. Control: a first step that writes sends none.
21. **A job.** It sends `JobEvent::SummaryRunChanged`. After `Offloaded`, the waiting banner holds `earlier`. Control: a run asked at step 1 has no `earlier`.

**The page (UI)**

22. **No stale bar while a summary is written.** The stale bar is hidden while a re-summarise job runs or `waiting_for` is set. It comes back after the terminal event, or after a detail read without `waiting_for`. Control: with neither, it shows.
23. **The header line.** `headerLine` with `writing` gives both drafts. Control: `null` gives today's line byte for byte.
24. **The Stop button.** Stop calls `stopWaitingSummary`, and the page and the list re-read. Controls: each answer's copy (`AlreadyWritten`, `NotWaiting`, a failure); no Stop while `waiting_for` is null.
25. **The title rule** (`headerButtons.test.tsx`):
    - (a) A long title, with labels that fit the header but not the header less `TITLE_MIN_PX`, gives icons.
    - (b) A short title whose own width leaves room keeps labels.
    - (c) In the wrapped band (even icons do not fit beside the reserve, labels fit the full width), the wrapped row is labelled.
    - Control: a wide header keeps labels.
    - Plus a Browser-pane check of the Vite preview with an invented long title ("Quarterly planning review for the northern warehouse fit-out and supplier onboarding"), at 1024 px and narrower, in light and dark.

**Copy and failure kinds**

26. **Copy in both suites.** The new keys are read by both suites, with no em dash and no run of spaces. `page_names_tests.rs` pins every sentence that names a Settings page.
27. **Every local error has a kind.** Every `LocalSummaryError` variant is built, including every `SummaryFailure` in `Run`, every `LlmError` in `Load`, and `LoadRetried` from two kinds. `kind()` is checked against a written-out table.
    - Each error is built twice with different words, and the copy and the word must not change, as `plain_text_every_runtime_error_maps_to_fixed_copy` does.
    - Control: `Run(Cancelled)` gives `Cancelled`, which keeps `SUMMARY_CANCELLED`.
28. **The child's kind word.** A fake child failing with `Invalid` gives the parent `ChildRun { kind: Some(ReplyUnusable) }` and the `reply_unusable` copy. The parent reads a child kind of `cancelled` as `Other`, and the child's own kind set never holds it. Control: a `Failed` line with no kind gives `Other`.
29. **Kind words in the order.** `failure_word(Local, ..)` gives each kind's word, and `failed` for `Other`. Control: endpoint and agent-CLI failures still give `failed`.

**Confirmations (11.14)**

30. Per 11.14 step 3, after the repro.

### 11.13 Local summary failures say which kind (owner addition 8)

Today `plain_text::runtime_text` maps every `LocalSummaryError` except a cancel to `LOCAL_SUMMARY_FAILED`, and `run::local_word` maps every one to `failed`.

**A typed stuck load.** In earshot-llm-local, `log::load_window`'s stuck-lock error becomes a new `LlmError::LoadStuck` instead of `LlmError::Backend(STUCK_MESSAGE)`. Its Display keeps today's words for the CLI.

**The classifier.** A new `summary::LocalFailureKind`: `TooLong`, `ReplyUnusable`, `CardStopped`, `Stalled`, `LoadFailed`, `LoadStuck`, `WrongBackend`, `CudaChanged`, `Other`, `Cancelled`. `LocalSummaryError::kind()` matches every variant, nested ones included, with no wildcard:

| Error | Kind |
|---|---|
| `Run(TooLong)`, `Run(Plan)` (no plan fits the context), `Run(Model(ContextTooSmall))`, `Load(ContextTooSmall)` | TooLong |
| `Run(Invalid)` (still invalid after the repair rounds) | ReplyUnusable |
| `GpuChildDied`, `GpuChildStalled` | CardStopped |
| `GpuStalled`, `LoadStalled` | Stalled |
| `Load(LlmError::Load)` | LoadFailed |
| `Load(LlmError::LoadStuck)` | LoadStuck |
| `Load(LlmError::Backend)` | WrongBackend |
| `Load(Input)`, `Load(Decode)`, `Load(Utf8)`, `Run(Model(Load, Input, Decode, Utf8, Backend, LoadStuck))`, `Run(Grammar)`, `Chunks` | Other |
| `Load(LlmError::Cancelled)`, `Run(Model(Cancelled))`, `Run(Cancelled)` | Cancelled |
| `LoadRetried { cpu, .. }` | `cpu.kind()`: the last attempt's kind |
| `CudaFilesChanged` | CudaChanged |
| `ChildRun { kind, .. }` (the run failed in the GPU child) | `kind`, else Other |
| `ChildLoad { kind, .. }` (the GPU child refused the load; the summary may move to the CPU) | `kind`, else Other; never WrongBackend by itself |

**The GPU child.** Today a GPU summary's run failure reaches the parent only as text, which can never be shown. It is the likely path of the owner's 79-minute failure. The changes:
- `summary_child::ChildLine::Failed` gains `kind: Option<String>` (`serde(default)`). The child fills it with its error's kind word.
- Nothing in the child cancels (`summary_child.rs`: the parent kills it), so its kind set excludes `cancelled`. The parent's parse maps an unknown word, or `cancelled`, to `None`, which reads as Other.
- `ChildEnd::RunFailed` carries the parsed kind, and `LocalSummaryError::ChildRun` becomes `ChildRun { message: String, kind: Option<LocalFailureKind> }`.

**Copy.** `runtime_text`'s arm becomes `RuntimeError::LocalSummary(l) => local_failure_copy(l.kind())`.
- That is an exhaustive match: `SUMMARY_CANCELLED` for every Cancelled kind, today's `LOCAL_SUMMARY_FAILED` for Other, and 11.9's `local_failures` sentences for the rest. It never shows the error's text.
- Cancelled, as before: only `Run(Cancelled)` is reachable (llama.cpp's own cancel becomes it, and a load is not interruptible), and it kept `SUMMARY_CANCELLED` and stopped the run before. `local_word` stops the run for every Cancelled kind.
- "Try again or choose another model" is kept only where it helps: the reply, and the load (there as "choose another model").

**The order's words.**
- `run::local_word` returns one new `OutcomeWord` per kind other than Other and Cancelled.
- Other keeps `Failed`. Cancelled keeps `None`, which stops the run.
- These are the words the record stores, so `moving`, `earlier` and the end banner say the kind. For example: "So far: this computer: too long. Now trying Claude Code."
- `OutcomeWord::ALL` and the fixture's `words` table grow with them.
- Endpoint and agent-CLI failures keep `failed`.

### 11.14 Confirmations on the Summary tab hide by themselves (owner addition 9)

**What the owner saw.** "New summary ready." and "Exported to <path>" (success intent, only Dismiss) stayed on the Summary tab.

**What the code does today.** `autoHide.ts`'s header comment lists "the Summary tab's banners" among those that never hide. No code applies that:
- The page notice (`noticeHold`, "Exported to") and the job bar (`jobHold`, `STORED_TEXT`) both use `hidesByItself`, whatever the tab.
- Only `SummaryBanners` gets no hold: the stale bar, the run note and kept-local.
- `autoHide.test.tsx` already hides "Exported to" on the default tab, which is `summary`.

So the comment is wrong, but it is not what kept the banners. Narrowing it alone fixes nothing the owner saw, and a test of it would pass before the change.

**As built.** The repro was tried in the Vite preview, on the Summary tab with the window watched: "New summary ready." hid after `BANNER_HIDE_MS`, so the bug did not reproduce. The mock always reads the window as watched, so the remaining suspect is the real app's `useWindowWatched` (`shell::window_watched`), which the owner's machine must check. Built here: the comment as the owner states the rule, and a test that "New summary ready." hides on the Summary tab. No behaviour change.

**Build steps.**
1. **Reproduce first**, in the real app or the Vite preview, on the Summary tab, and find why the countdown never ran. Two causes the code allows:
   - **A hold that never lets go.** `useAutoHide` takes its hold from `el.matches(":hover")` or from focus inside the bar when `shown` changes. A pinned bar that appears under a resting pointer holds until the pointer leaves it.
   - **`watched` false while the owner looks.** `useWindowWatched` comes from `shell::window_watched`. A wrong reading stops every countdown, on every tab.

   The fix goes where the repro points, and a test that fails before it comes first.
2. **Make the rule explicit.** Rewrite the comment to the owner's rule:
   - **Hide:** a Summary-tab banner that is success or info, offers only Dismiss and is not progress hides after `BANNER_HIDE_MS`.
   - **Stay:** its notes (out of date, stale names, run note, kept-local, waiting for another computer), every warning and error, and every banner with an action.
3. **Tests.** On the Summary tab, "New summary ready." and "Exported to" hide after the delay under the repro's condition. Controls: each kept note stays (the stale bar, the run note, kept-local, waiting), and the existing tests are kept.

### 11.15 Spec review round 2 (CLEAR, advisories folded)

These rules join, and where they differ override, the sections above.

1. **11.4: damage figure and scan roots.**
   - The list's damaged request and summary rows go to `Scope::Total`.
   - A test checks that a second list read after the scan asks for none.
   - `meetings_waiting_shown` joins `ROOTS` in `sync_damaged_scan.rs`, the reader list of sync-spec 16.3, and the table of 17.3.
2. **11.2: the `stopped` field and the summarised-since case.**
   - `SummaryRun::stopped` is `skip_serializing_if = "Option::is_none"`, so a record that is not stopped serialises byte for byte as today's (test 8 pins it).
   - In step 2, an open request with `summarised_since` is also withdrawn, in the same transaction. A summary written by a third device must not leave the helper writing with no Stop. The answer is still `AlreadyWritten`.
3. **11.5: the per-move write.**
   - "Now trying" names `next_step(cursor)`, the first step at or after the cursor with no outcome, and the write carries the advanced cursor.
   - `put_own` returning `Ok(false)` means the record was replaced, and stops the run as `ask_for_run`'s `Conflict` does.
   - A store error on this write, which only records the account, is ignored, as handover-record failures are: it never costs the summary.
   - Test 19 covers all three, with a kept-local run in its control.
4. **11.8: the title's flex basis and the measuring key.** Superseded by the owner's decision in 11.8 (no measuring); kept as the record of round 2.
   - The title's `flexBasis` is the computed reserve, set as an inline style and floored at 240 px.
   - The measuring key includes the title text and whether the title editor shows.
   - Test 25 gains a short title in the icon band.
5. **11.3: the `Writing` registry.**
   - `writing()` and `cancel_writing()` are required trait methods, with no default bodies.
   - `Writing`'s lock is a leaf: a failed spawn's `decline` runs after the lock is released.
   - Hosts hold an `Arc<Writing>`.
   - A test pins the order: no store lock is taken under `Writing`'s lock.
6. **11.5: AppShell and `JobEvent::SummaryRunChanged`.** AppShell never stores the event as a job's `last` and never bumps `seq`; it only triggers the detail refresh. A test checks that a warning on the job bar survives a move.
7. **11.2: `tick_now` after a failed attempt.** It leaves `failures` and the time of the next regular tick unchanged. Test 9 checks it.
8. **11.13: two kind fixes.**
   - `ChildEnd::LoadRefused` maps to `Other` unless the child sent a kind word, never to WrongBackend.
   - `Run(Model(ContextTooSmall))` maps to TooLong.

## 12. T14.72: one online service

Status: built (2026-11-28). Task T14.72, PLAN.md section 8. Where this section differs from sections 2, 3, 4.3, 4.4, 5, 6 and 7, it overrides them.

### 12.1 What went wrong

- The Re-summarise menu (`resummarise_view`, `apps/desktop/src-tauri/src/summary_order.rs`) offers Claude Code and Codex whenever their CLI is installed, whether or not the user ever chose them. The owner was offered OpenAI (through Codex), which they never set up.
- The Summaries page lists the endpoint, Claude Code and Codex as three separate writers, so an order can hold two online services. The owner found that clunky.

### 12.2 The decisions (the owner, 2026-11-28)

- The writers are This computer, each of the user's other computers (unchanged), and one **Online service** row.
- The row has a picker with four services: Claude (through Claude Code), OpenAI (through Codex), Anthropic API, and Your own server. Only the picked service has a setup section.
- The picked service is the only online writer anywhere (the order, Re-summarise, the first-run wizard) until the user picks another.
- There is no fallback from one online service to another (accepted).
- The Online service row keeps its tick box and its up and down buttons, because other people may want it first. Ticking it adds it at the end of the order, as for every writer (section 2). Stored orders keep their positions.

### 12.3 Terms

- **Online writer**: the `WriterStep` values `Endpoint`, `ClaudeCode` and `Codex`. `Local` and computer steps are not online writers.
- **Service**: what the picker names. Anthropic API and Your own server are both the one endpoint configuration, told apart by its provider (`Anthropic` or `OpenAiCompatible`). There is still exactly one endpoint configuration (section 9).

### 12.4 Stored setting

- New device-local key `settings/online-service` (`DeviceRecord::OnlineService`), plain text, one of `claude-code`, `codex`, `endpoint`. Which endpoint service it is comes from the endpoint's stored provider, never from this key, so the key and the provider can never disagree about it.
- **One writer for the order, the key and the provider.** `write_order` (behind `set_summary_order`, `set_summary_order_keeping_writer` and the new `set_online_service`) reads the pick itself under its `&mut Store`, and takes an `Option<Service>` only for a new pick. In its one transaction it writes the order (unless `keep_order`), the mirror, the key, and, for a new pick, the endpoint provider with the 12.7 clear. The key it stores is the order's online writer when the order holds one, else the new pick, else the pick it read. Nothing else writes the key or the provider (12.7a).
- **The pick, as read** (`summary_order::online_service(store)` -> `Option<Service>`, plus notes):
  1. If the order, as read (section 3, old keys included), holds an online writer, the pick is that writer: for `Endpoint`, Anthropic API or Your own server by the stored provider.
  2. Otherwise the stored key, if it reads.
  3. Otherwise none: the row shows "Choose a service" and cannot be ticked.
- On this build the key and the order's online writer are always written together, so they never differ. Rule 1 exists only to repair what an older build wrote (12.10): an older build can put another online writer in the order, or change the old keys so they win.
- **An untick never changes the pick.** Unticking the online row stores the order without it and the key as the pick read before the write. So an order [Local, Claude Code] with no key (every upgrade) keeps Claude Code picked after the untick, and an order an older build gave Codex keeps Codex.
- An unknown word in the key reads as no key, with a note in fixed copy that never shows the stored text (the `local::UnknownSetting` pattern).

### 12.5 An order holds at most one online writer

- **Reading.** `summary_order` keeps the first online writer of a stored order and drops any later ones, with a note in fixed copy. This runs right after the order decodes, before the read filter (runtime `summary_order.rs`, the `readable` filter), so [Claude Code, Endpoint] with an endpoint that is not usable reads as [Claude Code] instead of being unreadable. Reading never writes. The next write of the order stores it in this form. The old-key mirror (section 3) is unchanged, because it is computed from the order.
- **Writing.** `set_summary_order` refuses an order that holds two online writers, or one whose online writer is not the pick as read, with `IpcError::invalid` and fixed copy. The page never sends such an order, because it offers only the pick, so the refusal guards the boundary and is not a user path. `set_online_service` passes the new pick, so its own in-place replacement (12.6) is checked against the new pick, not refused by this guard.
- **The CLI.** `earshot-cli` builds its one-step order from its command line (`CliWriters`). It never reads the pick and is unchanged.
- **Helpers.** A computer answering a request still runs only the writer kind its presence advertised, its own order's first writer step (4.6). An order holding one online writer leaves that rule as it is. `helper_writer` builds through `SettingsWriters`, so 12.9a's check applies to it too. If the helper's pick changed between its order read and the build, it fails closed: `Built::Skip`, the request is declined `no_writer`, and nothing is written.

### 12.6 Changing the pick

`set_online_service(store, service)` is a desktop command that is `async` plus `ipc::blocking` (it reaches the store). It ships with a mock and regenerated bindings in the same commit. It goes through `write_order`: the order, the pick and the settings are read under the caller's `&mut Store` (the services lock) before its one transaction, which is safe because every writer of these keys holds that lock. It always writes the order as read, with `keep_order` and `keep_writer` false, as a tick on the page does. So when the stored order is unreadable, or the old keys win (section 3), the order as read is stored in their place, and the pick cannot be overruled by rule 1 afterwards. The command returns the settings view and the order view, and the page's hook replaces both in its cache before it releases busy (T12.63b's rule).

- **The order holds no online writer.** It stores the key, and for an endpoint service the provider (12.7). The order is stored unchanged.
- **The order holds the old pick, and the new service can be a step.** Claude Code and Codex always can be: they are tickable whatever their sign-in (section 2). The old step is replaced by the new one at the same position, and the key and mirror are written in the same transaction.
- **The order holds the old pick, and the new service cannot be a step yet** (an endpoint service whose fields do not validate after 12.7). The step is removed from the order (unticked), the key is stored, and the page opens the new service's setup section. Saving that form ticks it last when there is room (section 2's rule). Its old position is lost: see question Q1 in 12.12.
- **Picking the service already picked** stores nothing when the stored order already reads as itself; otherwise (unreadable, old keys that win, a second online writer, or nothing stored) it writes the order as read.

The picker applies at once, like every choice on the page, with one exception: the endpoint's fields still need their Save. The Summaries settings fixture's save-rule test gains the picker as a choice.

### 12.7 Switching between Anthropic API and Your own server

Today the endpoint form has a Provider select. That select goes, because the picker is the provider.

- Picking Anthropic API stores provider `Anthropic`. Picking Your own server stores `OpenAiCompatible`.
- **One rule for the clear:** whenever a pick's provider differs from the stored provider, the stored address and model are cleared in the same transaction, whatever the previous pick was. So Anthropic API, then Claude Code, then Your own server clears them at the third step. The endpoint then reads "Not set up" and leaves the order (12.6). This is so a transcript never goes to one service's address under the other's protocol. Without the clear, Your own server's protocol would send the transcript to Anthropic's address with no key, and Anthropic's would send it to a server's address. The Anthropic API key stays in Credential Manager, bound to its address (T14.31).
- Picking Anthropic API over an empty address fills Anthropic's address (`settings.anthropic_url`) into the form as a draft, as the Provider select does today. Nothing is stored until the form's Save, and the fill alone is not an edit (T12.79's rule). When the key was saved for that address, it reads as bound again once the address is saved.
- The cost: switching away from Your own server loses its address and model, and switching away from Anthropic API loses its model name. See question Q2 in 12.12.
- Picking Claude Code or Codex changes no stored endpoint field, the provider included. The clear happens only when an endpoint service is picked whose provider differs from the one stored.

### 12.7a The provider has one writer

Today `set_summary_settings` (runtime `summary_settings.rs`) stores the whole cached settings value from the page (`stored_values`: provider, address, model and the rest; `useSummaries.ts` sends the cached value with every save), and it changes the order too: `order.with_writer(s.writer)` puts the value's writer in place of the order's first writer step. A save from a cache older than a pick switch would put the old provider, address and model back, undo 12.7's clear, and could put an unpicked online writer into the order.

- The guard sits in `write_order`, which both `set_summary_settings` and the endpoint form's Save (`set_summary_order` with settings, `useSummaries.ts` sending `{...now.value, ...change}`) reach. `SummaryProvider` leaves `stored_values`. `write_order` stores the provider only for a pick change (12.4). It refuses settings whose provider differs from the stored one, with `IpcError::invalid` and fixed copy (`page.settings_changed`), and stores nothing on that refusal.
- `set_summary_settings` validates with the mirror of the order as read, which is what `write_order` already substitutes, not with the caller's writer, so a stale writer in a cache never refuses an unrelated save.
- `write_order`'s refusals are their own `SummaryOrderError` variants: `StaleProvider`, `NotThePick` and `TwoOnline`. `set_summary_settings`'s error map passes them through with their fixed copy (`page.settings_changed`, `page.not_the_service`), never as an internal store error.
- `set_summary_settings` stops changing the order: `with_writer` goes from that path, and it stores the order as read with the pick as read. So the writer word comes only from the order's mirror, and only the order list and the picker change who writes.
- The hook replaces its cached settings with what each command returns before it releases busy, so the refusal is reached only by a stale caller, not by the page itself.

### 12.8 The Summaries page and the first-run wizard

- **Rows** (`OrderSection`): This computer; each other computer that offers summaries (unchanged); one Online service row.
  - The row's name is the picked service's step label ("Claude (through Claude Code)", "Anthropic API", "Model server (<host>)", "Your own server", "OpenAI (through Codex)"), with the picker on the row.
  - With no pick it reads "Online service" with "Choose a service", greyed and not tickable.
  - The rank, tick and move rules are those of any writer.
- **Disclosure.** The picked service's destination line and, for Claude Code and Codex, its privacy line (`CLAUDE_CODE_PRIVACY`, `CODEX_PRIVACY`; PLAN 3.9) show on the row as soon as it is picked, before it can be ticked. The endpoint's remote-host warning keeps its rule.
- **Setup sections.** Only the picked service's section shows (T12.96's `SetupSection`: folded by default, opened as today, and opened by 12.6's untick case). The others are not rendered.
- **Sign-in check.** The Claude Code or Codex sign-in check runs when that service's card shows, as today; that is now when it is picked. It sends no meeting content.
- **Picking alone sends nothing.** Content reaches the service only when it is ticked in the order and a summary runs, or when the user chooses it by name in Re-summarise.
- **The first-run wizard** reuses `OrderSection` (T12.79), so it gets the same row and picker. The picker saves at once, as ticks do. The Download step still counts only This computer.
- **Rust's offered list.** The order view's `offered` list holds This computer and the computers as `StepView`s, as today. The online row is its own field, `online: OnlineRowView { service: Option<Service>, step: Option<StepChoice>, reason: Option<String> }`, because `StepView.step` is a `StepChoice` and has no "choose" shape. It never holds an unpicked online writer. The row's React key is the constant `online`, not the step key, so changing the pick keeps the picker's focus (T12.68's focus rule).
- **The UI mock** mirrors the pick read, the normalising and the 12.7 clear from one shared case table, `fixtures/ui/online_service_cases.json`, read by both suites.

### 12.9 Re-summarise

The menu (`resummarise_view`, section 5) offers:

- Use my order;
- the local model, if installed;
- **the picked service, whether or not it is ticked in the order**, when it can run by today's rule: Claude Code or Codex installed, or the endpoint set up (an Anthropic endpoint also needs a key bound to its address);
- each computer that offers summaries and is online (unchanged).

No other online writer is offered. No sign-in probe runs when the menu opens, as today (the menu is read on every open, and `auth status` can take 5 s). A named run that finds the CLI signed out fails with today's fixed copy. On a kept-local meeting the online item is disabled until "Allow" (section 5).

**The menu is not the check.** A menu read before the pick changed, or any stale caller, can still send `resummarise_with` an online writer that is no longer the pick. So:

- `resummarise_with` refuses an online writer that is not the pick as read, with `IpcError::invalid` and `page.not_the_service`, before a job starts.
- A pick changed between the command and the job's step is caught by 12.9a's check, which every named run also passes. A named run whose step reads `not chosen` fails with `page.not_the_service`, and nothing is handed over. (Round 2 dropped a check in the named start's transaction: `start` runs on a `StoreTx`, while the pick's readers take a `&Store`.)

### 12.9a Runs try only the pick

Runs copy their steps when they start and continue from that copy (4.1, 4.3), so a copy can hold an online writer that is not the pick: a run started on 0.9.2 that holds [Desk, Claude Code, Codex] and waits on Desk across the update, or a run waiting while the user changes the pick. This overrides 4.3 and section 7's "a run continues from the steps copied at its start" for online steps only:

- **Where.** The check sits in `SettingsWriters::writer` (runtime `host.rs`), the app's builder of a writer step. It runs under the same `services.store()` guard that reads the settings the service is built from, so the pick checked and the destination built come from one snapshot. It is one short lock, with nothing nested and no transaction. It is never in `run_steps`, which `earshot-cli`'s `CliWriters` shares: the CLI never reads the pick (12.5), and `AskOnly` is untouched.
- **What.** An online step that is not the pick is skipped with a new `SkipReason::NotChosen`, mapped by `skip_word` to the outcome word `not chosen`, and the run moves on. This applies to every app run, fresh or continued, `origin: order` or `named`.
- So an app run tries at most one online writer, the pick at the moment it gets there, and never falls back from one online service to another.
- `not chosen` gets arms in `steps_text` and in the one-step banner (4.4). A run record holding it is a word an older build does not know, so that build reads the record as unreadable and fails closed (4.3). The build pins this with a test.
- Computer and local steps are unchanged.

### 12.10 Older builds

- An older build ignores `settings/online-service` and still reads `settings/summary-order`. An order this build writes holds at most one online writer, which any older build reads.
- An older build can tick a second online writer. On this build the order then reads with the first kept (12.5), and the pick follows it (12.4 rule 1).
- An older build can change the provider select. Rule 1 still holds, because the endpoint's service comes from the provider. If that leaves an address of the wrong kind, the run fails as it does today (no key bound, or the server refuses). Nothing is sent that an older build would not have sent.

### 12.11 Copy

New copy goes in `fixtures/ui/summary_order_cases.json`, read by both suites (approved by the owner 2026-11-28, with the release note naming the services as the picker does):

| Key | Text |
|---|---|
| `page.online_service` | "Online service" |
| `page.choose_service` | "Choose a service" |
| `page.service_label` | "Service" (the picker's label) |
| `page.second_online_dropped` | "Your order of summary writers held more than one online service, so only the first is used." |
| `page.unknown_service` | "The stored online service is not one this version knows, so none is chosen." |
| `page.not_the_service` | "Only the online service you chose can write summaries." |
| `page.settings_changed` | "The summary settings changed. Check them and try again." |
| `words.not_chosen` | "not chosen" (an outcome word, 4.4) |

Service names reuse `steps.*`; destinations reuse `destinations.*`.

**Release note** (CHANGELOG): "Summaries: the Summaries page now has one Online service row, where you choose Claude (through Claude Code), OpenAI (through Codex), the Anthropic API or your own server. Only that service is used, in your order and in Re-summarise. If your order listed more than one online service, the first is kept."

### 12.12 Questions for the owner (both decided 2026-11-28: the recommended answer)

- **Q1. Lost position.** Switching the pick, while it is ticked, to a service that needs setting up unticks it, and saving the setup ticks it last. Recommended: accept this (simple, and rare). Alternative: remember its position until the setup is saved.
- **Q2. One endpoint configuration.** Switching between Anthropic API and Your own server clears the address and model (12.7). Recommended: accept this, because it keeps section 9's one configuration. Alternative: store an address and model per endpoint service (two more device-local keys), so switching back loses nothing.

### 12.13 Tests (each with its control)

1. **Pick read.** With an online step in the order, the pick is that step; for the endpoint, by provider. With none, the stored key. With neither, none. An unknown word gives the note. Control: old keys that win (section 3) and name an online writer different from the stored key give that writer (rule 1, "old keys included").
2. **Normalising.** A stored order [Claude Code, Local, Codex] reads as [Claude Code, Local], with the note, and storage is unchanged until the next write. The next write stores the normal form. [Claude Code, Endpoint] with an endpoint that is not usable reads as [Claude Code], not as unreadable. Control: an order with one online writer reads unchanged, with no note.
3. **Write refusals.** `set_summary_order` refuses two online writers, and an online writer that is not the pick. Control: the pick at any position is accepted, and so is `set_online_service`'s in-place replacement.
4. **Pick change in place.** [Local, Claude Code] with Codex picked becomes [Local, Codex], with key and mirror in one transaction (a fault between them leaves neither). Control: an untick when the new endpoint service is not set up, with the order otherwise unchanged. With an unreadable stored order, or old keys that win, the pick change stores the order as read and the new pick reads back.
5. **Endpoint switch.** Anthropic API to Your own server clears address and model and sets provider `OpenAiCompatible`. Anthropic API (set up), then Claude Code, then Your own server clears them at the third step and keeps the endpoint out of the order. A fault after the provider is written leaves the provider, address and model all as they were. Control: Claude Code to Codex leaves the endpoint fields as stored. The address prefill on an empty Anthropic switch is a draft, not stored and not an edit.
6. **Untick keeps the pick.** Order [Local, Claude Code] with no key: untick Claude Code, and the pick reads Claude Code. Key `claude-code` with an order an older build gave Codex: untick Codex, and the pick reads Codex. Control: picking another service changes it.
7. **Stale settings save.** Switch the pick from Anthropic API to Your own server, then save from a cache that still says Anthropic: the live-summaries switch, and separately the endpoint form (`[.., Endpoint]` with Anthropic's provider, address and model). Both are refused with `page.settings_changed`, nothing is stored, and the order and pick are unchanged. Control: the same saves after the hook replaced its cache are stored. `set_summary_settings` never changes the order (no `with_writer`), and a stale writer word in the cache does not refuse an unrelated save.
8. **Re-summarise.** With Codex installed and not picked (Claude Code picked, installed), the menu has no Codex item and has Claude Code, ticked or not. Controls: with Codex picked and installed, Codex is offered; a picked Anthropic endpoint without a bound key is not offered; a picked OpenAI-compatible endpoint with no key is offered. Kept-local disables the online item.
9. **Named start.** `resummarise_with` with Codex when Claude Code is picked is refused before a job starts. A pick changed between the command and the job's step fails the job with the same copy and no handover. Control: the picked service is accepted and runs.
10. **Runs.** A continued run whose copied steps are [Desk, Claude Code, Codex], with Claude Code picked and Desk declining, tries Claude Code. If Claude Code fails, the run skips Codex with `not chosen` and never hands over to it. With Codex picked, it skips Claude Code and tries Codex. Control: a run whose copied online step is the pick tries it. CLI: `earshot-cli` with `--llm-url` and a stored pick of Claude Code still uses its endpoint (`CliWriters` ignores the pick).
10a. **Callers of `set_summary_settings`.** Each moves to the new paths with no assertion weakened:
    - the app's replay (`apps/desktop/src-tauri/src/headless.rs`, which sets its endpoint writer only through it today) uses `set_online_service` and then `set_summary_order([Endpoint], Some(..))`, so `accept m7` still reaches the mock;
    - the tests that set a writer through it get one named setup helper: `cuda_settings_tests.rs`, `models_tests.rs`, both `keep_local_tests.rs`, `offload_tests.rs`, `benchmark_tests.rs`, `summary_order_tests.rs`, and the Anthropic cases on a fresh store in `summary_settings_tests.rs`;
    - the UI mock drops its copy of `with_writer` (`mockSummaryOrder.ts`), and the provider refusal joins `fixtures/ui/summary_settings_cases.json`.
10b. **Helpers.** A helper whose pick changed after its order read declines the request `no_writer` and writes nothing. Control: a helper whose order's online writer is the pick writes it.
11. **The page.** One online row with the constant key; only the picked setup section rendered; the picker applies at once; the privacy line shows before ticking; "Choose a service" greyed with no pick; move buttons on the online row. The save-rule test lists the picker and drops the Provider exception, because the Provider select is gone. Wizard: same row; the Download step unchanged.
12. **Privacy.** A run with Codex installed but not picked never builds a Codex step, from the order, a continuation or Re-summarise. Picking alone makes no request and hands over nothing (the fakes record no CLI run and no handover).
13. **Older builds.** An order with two online writers and provider changes, written as a 0.9.2 build writes them, read by this build per 12.10.
14. **Copy and the mock.** Every new key and `fixtures/ui/online_service_cases.json` read by both suites.

### 12.14 Not in scope

- More than one online service in an order (owner decision).
- A per-service endpoint configuration (Q2, unless the owner chooses it).
- Changes to how a helper computer writes (4.6).

### 12.15 Spec review

- **Round 1 (2026-11-28): blocked on four findings, all addressed above.**
  1. A named re-summarise was checked only in the menu: now also in `resummarise_with` and at the run's step (12.9, 12.9a; round 2 dropped a start-transaction check).
  2. An untick changed the pick: every order write stores the key with it (12.4).
  3. A continued run could fall back to an unpicked online writer from its copied steps: online steps are checked against the pick when reached (12.9a).
  4. `set_summary_settings` could restore a stale provider and change the order: the provider has one writer, and that path no longer changes the order (12.7a).
- **Round 2 (2026-11-28): blocked on three findings, all addressed above.**
  1. The endpoint form's Save could still write a stale provider: the guard moved into `write_order` (12.7a).
  2. The clear ran only on a direct switch between the two endpoint services: it now runs whenever the picked provider differs from the stored one (12.7).
  3. The run check had no place, and `run_steps` is shared with the CLI: it sits in `SettingsWriters::writer` (12.9a).

- **Round 3 (2026-11-28): clear, with five advisories folded in.** Helpers pass the check and fail closed (12.5, test 10b). Settings validate with the order's mirror (12.7a). Round 1's item 1 is corrected. The refusals are named error variants (12.7a). `set_online_service` is a command (12.6).
- **Round 2's advisories** are folded in too: the named start check dropped; `write_order` reads the pick itself; the callers of `set_summary_settings` and the mock (test 10a); `not chosen` in the banners and older builds.
- **Round 1 advisories, folded in:** the Anthropic fill is a draft (12.7); the online row's view type and constant key (12.8); normalising before the read filter (12.5); the write used when the order is unreadable or the old keys win (12.6); the extra tests and controls (12.13); the mock's shared table; the release note (12.11).
