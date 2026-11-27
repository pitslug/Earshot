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

Writers and locking: the record has three writers (`after_meeting`, a summary job, the hand-back in `Offload::step`), and every write is a read-modify-write in one store transaction under the store lock, re-reading the record inside it.

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

Status: design (T14.66, PLAN.md section 8, with the owner's additions of 2026-11-27 in 11.6 to 11.8); not built, not yet spec-reviewed.

### 11.1 What went wrong

A re-summarise ran [local model, Claude Code, a summary worker]. The local step and Claude failed and the run moved on, but:

- The run sent no warning mid-run. The job then ended `JobEvent::Offloaded`, and `JobView` keeps only a job's last event.
- The meeting list reads `MeetingState` from this process's leases only. A run `Waiting` on another computer holds none, so the row showed no working dot.
- Nothing could stop the worker. A withdrawal only stops it storing (`offload::still_claimed` in its write transaction). The worker's token is cancelled only when the worker stops (`Worker::cancel`). The desktop helper passes `CancelToken::new()` to `write_for_request`, so nothing ever cancels it.

### 11.2 Stop (the asker)

**Where.** The waiting banner on the meeting page (`MeetingDetail.waiting_for`, built by `offload::waiting_banner` from `offload::waiting_shown` and `run_view`) gets one action, "Stop". It can show only on the asker: `waiting_shown` reads only requests this device asked.

**Command.** `stop_waiting_summary(meeting)` in `apps/desktop/src-tauri/src/offload.rs` (async, through `ipc::blocking`) calls a new `summary_order::run::stop_waiting(services, meeting) -> StopAnswer`. That is one `edit::write` transaction under the store lock, re-reading everything inside it:

1. Find this device's open request for the meeting (`offload::open_request`: asked by me, not deleted, requested or claimed, `authored`).
2. If there is one, write it `Withdrawn` (the asker is its only allowed writer), then by the run record:
   - **The record waits on it** (`waiting_on == Some(request)`, `state: waiting`): end the run in the same transaction with `put_own_tx`. Push the new outcome `OutcomeWord::Stopped` for step `cursor - 1`, clear `waiting_on`, set `state: ended`. This is 4.1's "the run stops" ending, written in its own transaction as `hand_back`'s named branch is. No later step runs, whatever the run's origin.
   - **No record** (a legacy request): also write a fresh ended record (`origin: named`, steps `[computer]`, outcome `stopped`). Without it, the next offload step would read the withdrawal as `HandBack::Legacy` and write the summary here.
   - **Unreadable, or naming another request or none**: leave the record. `hand_back` already settles those with no job.
   Answer `Stopped`.
3. **No open request, but the helper has already written** (a `Done` request the record waits on, or `summarised_since`): write nothing to the request, end the run with that computer as writer as `end_after_done` does, and answer `AlreadyWritten`.
4. **Otherwise** (withdrawn by the 90 s or stale rule and a continuation job already runs, or the run already ended): write nothing and answer `NotWaiting`. A running continuation is stopped by the job's existing Cancel (`JobRunner::cancel`).

After a commit that withdrew, the command calls `SyncHost::nudge`, so the withdrawal goes out at the next poll rather than the next regular tick.

**Afterwards.** The asker's offload step sees a withdrawn request its record no longer waits on, so `hand_back` answers `Settle`: no job, no continuation, also after a restart.

**Races.**
- **The helper's `Done` is applied first.** Case 3: the summary shows, nothing is withdrawn.
- **Stop commits first, but the helper had already stored.** The helper stored before it pulled the withdrawal. Its summary row arrives and is kept (rule 7.2, and sync-spec 10.1 "a done answer stays last writer wins"). The stop note (11.7) shows only while no summary created after the request exists (`summarised_since`), so it never says "stopped" over a summary.
- **A withdrawal written by a device that is not the asker.** `authored` ignores it in every reader and in the helper's cancel check (11.3). Stop only ever writes this device's own requests.
- **Sync off on the asker.** Stop still withdraws and ends the run here. The withdrawal waits in the outbox, and the helper writes on until it finishes or the asker's presence goes stale. Its summary, arriving when sync resumes, is kept as above.
- **Re-summarise pressed at the same moment.** `start` withdraws open requests and replaces the record in one transaction. Both transactions re-read, so the second one to commit sees the first's state (`NotWaiting`, or nothing open to withdraw).

**What the user sees.**
- The command's result reloads the detail and the waiting banner goes.
- The run note says the summary was stopped (`stopped_note`).
- A summary stored before the run stays shown, and Undo is unchanged.
- The list row loses its dot at its next read.

### 11.3 The helper stops writing

**One token per handed request.** `Offload` gains `writing: Mutex<BTreeMap<EntityId, CancelToken>>`. `as_helper` creates a token when it hands a request. `OffloadHost::write_for(request, cancel: CancelToken)` (a trait change) passes it to `write_for_request`:
- **Desktop.** `HostOffload::write_for` passes the token instead of `CancelToken::new()`.
- **Worker.** `WorkerHost` keeps the tokens it was given (`Mutex<Vec<CancelToken>>` plus a stopping flag). `Worker::cancel` sets the flag and cancels them all, and a token registered after the flag is cancelled at once. The worker's single `cancel` field goes.

**The check.** In `Offload::step_with`, the scan also wants any request with `target_device == me`, `state == Withdrawn`, `authored` and its id in `writing`.
- It collects those ids under the store lock.
- After the lock is dropped, and before the per-request loop, it cancels and removes each token.
- It also drops entries whose request reads `Done`, `Declined` or deleted.
- It runs in every step, `step_sync_stopped` included. No lock is held across a cancel.

The T14.50 skip rule is unaffected. While a write runs, its request is claimed and open, so the scan is never idle. A pulled withdrawal is a row the tick applied, which forces the next scan.

**A cancelled helper writes nothing.**
- `write_for_request` returns `HelperOutcome::Cancelled` (`Err(_) if cancel.is_cancelled()`, or `wait_turn` refusing).
- It writes no decline: sync-spec 10.1 lets the target write only `Claimed`, `Done` and `Declined`, and `move_request(Claimed -> Declined)` would find `Withdrawn` anyway.
- Its write transaction stores nothing, because `still_claimed` fails.

`HelperOutcome::Cancelled`'s doc is corrected to cover both causes: the worker stopping (the request stays claimed) and a withdrawal (nothing more to do).

**The bound**, from the asker's commit:

| Part | Bound |
|---|---|
| Asker pushes | after `nudge`: the next poll, `SyncTiming::poll` (2 s), plus one push |
| Helper hears it | its next tick, at most `TICK` (30 s) after its last, plus that tick's pull and the step |
| Endpoint writer (the worker always: its own model is an HTTP endpoint too; the desktop's endpoint or Anthropic) | in-flight request abandoned within `CANCEL_POLL` (20 ms, earshot-llm `client.rs`) |
| Claude Code, Codex (desktop helper) | the CLI's job killed within `earshot_proc::POLL` (10 ms) |
| Local llama on the CPU (desktop helper) | the next generated token (`generate_cancellable`). A load in progress is not interruptible and runs to its end (at most `cpu_load_limit`, 600 s); the run then stops before its first request |
| GPU summary child | killed at the parent's next watch poll (`summary_child::watch_lines`) |
| Waiting for a queue turn | at once (`wait_turn(cancel)`) |

So on a healthy relay, writing stops within about 35 s plus the writer's own latency. A helper whose relay keeps failing backs off up to `MAX_BACKOFF` (15 min) and hears nothing until it reaches the relay, but it still never stores (`still_claimed`).

### 11.4 The meeting list shows a summary written elsewhere

**Source.** A new `offload::meetings_waiting_shown(store, me) -> BTreeSet<EntityId>`. It uses the same rule as `waiting_shown`, so the list and the page agree.
- It is read once in `ipc::meeting_list`, inside `Services::live_meetings`' single lock, with the first meeting, as the folders are.
- It costs one `summary_requests()` read (open, asked by me, `authored`, not deleted), then one `summarised_since` per meeting in that set (usually none or one). There is no loop over every meeting.

**Display.**
- A new field `MeetingListItem.summary_elsewhere: bool` (bindings regenerated).
- `rowDot` counts it as `working`.
- `workWord` gives `list_elsewhere` when no local work is heard.
- `MeetingState` is unchanged.

**Refresh.** The list is already re-read on the meeting's `Summary` event, a job's terminal event and `SyncEvent.meetings` (`StepReport.meetings` covers withdrawals and hand-backs). A successful Stop re-reads it too.

### 11.5 Failed writers said as they happen

**Runtime.**
- A new `summary_order::run::run_summary_order_reporting(.., on_step: &mut dyn FnMut(StepMoved))`. `run_summary_order` becomes it with a no-op, so existing callers and tests stay.
- `StepMoved { step: OrderStep, word: OutcomeWord, next: OrderStep }` is called in `run_steps` right after each `run.push(i, word)` for a step tried and left, and only when `run.next_step(i + 1)` names a step. It only observes: outcomes do not change.
- It is not called for steps dropped for a kept-local meeting (pushed before the run starts), nor when the last step fails (the end banner follows at once). `not now` and a computer that was not asked are reported like any other move.
- It is a parameter, not a `RunPlan` field, because `after_meeting` forwards it into its own `progress: &mut dyn FnMut(HostStep)`, which a shared plan built before it cannot borrow.

**A meeting's own summary.**
- `host::after_meeting` passes `&mut |m| progress(HostStep::SummaryStepMoved(m))` (a new variant).
- The driver's watch counts it as the summary phase (unwatched, so T9.22's timings do not change).
- The driver maps it through `Inner::send` to `MeetingEvent::Warning { key: STEPS_BANNER_KEY, kind: Backend, message: summary_order::step_moved_text(&m, &cx) }`. That is the run's existing `summary-steps` key: each move replaces the last, and the end-of-run banner (`later_writer` or `no_summary`) replaces the moves.
- It is sent after `Ended`, so `afterMeetingBanners` shows it, and the existing Warning fold arm in `SentRecord` covers it.

**A re-summarise.** `jobs::summarise_run_with` passes `&mut |m| warn(step_moved_text(&m, &cx))`, so each move is a `JobEvent::Warning` mid-run. A job that ends `Offloaded` replaces its warnings, so the waiting banner also lists the steps already left (`earlier`, 11.7), read from the run record. That account survives the job's end and a window reload.

**CLI.** `earshot-cli` prints `<step>: <word>` as moves arrive, as well as its end-of-run lines.

### 11.6 No stale notes while a summary is written (owner addition 5)

`MeetingDetailView` computes `summaryInProgress = writingSummary || detail.waiting_for !== null`:
- `writingSummary` already exists: a re-summarise job on the meeting, not terminal.
- `waiting_for` is the waiting banner's own source, `offload::waiting_shown`, which also turns null once a summary created after the request exists.

While `summaryInProgress` holds, the `summary_stale` bar is not rendered. That bar holds both the earlier-name and earlier-transcript texts, and its Re-summarise button. It returns once the run ends: the job's terminal event, or a detail read with `waiting_for` null. No Rust change.

### 11.7 The header says a new summary is being written (owner addition 6), and run notes

**Header line.** `detail.ts::headerLine(d, writing)` takes `writing: { on: string | null } | null`.
- With a current summary, its part reads "Summary by <writer> · <in_progress>". Without one, only "<in_progress>".
- `in_progress_here` is used for a job here, and `in_progress_on` while the run waits on a computer.
- `{device}` comes from a new `MeetingDetail.waiting_device: Option<String>`: the name, or `unnamed_inline`. It is built in `detail.rs` beside `waiting_for`, from the same request (`StepContext::device`).

**Banners and notes.**
- `waiting_banner` appends `earlier` when the waiting run's record holds moved steps, reusing `step_outcome` and `joiner`.
- `run_note` gets an arm for an ended run holding `stopped`: `stopped_note`, plus `earlier` when other steps moved first.

### 11.8 The header keeps a readable title (owner addition 7)

**Choice.** Collapse the buttons to their icons whenever their labels would leave the title less than a readable width. Rejected: giving the buttons their own row always.

**Why.**
- T14.55's measured switch without back and forth (`useLabelsFit`) is already built and tested. The fault is one measurement: `measureRow` compares the buttons' need with the whole header (`row.parentElement.clientWidth`) and ignores the title, which keeps only its 240 px flex basis.
- A row of their own costs a line of height on every meeting at every width, and pushes the banners down.

**The change.**
- `measureRow` reports `available = header width - TITLE_MIN_PX - column gap`.
- `TITLE_MIN_PX` is 360, a judgement (about 30 characters of `Title2`). It also becomes `headerText`'s `flexBasis`, so the measure and the layout agree.
- The buttons that collapse are T14.55's set: Speakers, Export to Obsidian and Copy summary. Re-transcribe and Re-summarise have no icon in that set and keep their labels.
- If even the icons leave the title under 360 px (a very narrow window), the header's existing `flexWrap` puts the buttons on the next line.

### 11.9 Copy (drafts for the owner)

New keys in `fixtures/ui/summary_order_cases.json`. The UI and its mock import them, the desktop's tests pin them, and `_comment` gains a line for each.

| Key | Draft |
|---|---|
| `words.stopped` | stopped |
| `step_moved` | {writer}: {word}. Trying {next} next. |
| `earlier` | Tried first: {steps}. |
| `stopped_note` | You stopped the summary on {device}. Re-summarise to write it again. |
| `stop_button` | Stop |
| `stop_label` | Stop the summary on {device} (accessible name) |
| `stop_already_written` | {device} had already written the summary. |
| `stop_not_waiting` | This summary is no longer waiting for another computer. |
| `stop_failed` | The summary could not be stopped. Try again. |
| `in_progress_here` | New summary being written |
| `in_progress_on` | New summary being written on {device} |
| `list_elsewhere` | Summarising on another computer... |

`{device}` is filled with a function replacer, because a name can hold `$&`.

### 11.10 Privacy

- **Records.** The run record gains one fixed word (`stopped`). The request gains no field, and `offload-times` is unchanged.
- **Events and fields.** `HostStep::SummaryStepMoved`, the warnings, `StopAnswer`, `summary_elsewhere` and `waiting_device` hold fixed words, step kinds and a computer's name the page already shows.
- **Logs.** No log line is added.
- **Wire.** Nothing new is sent. Stop writes the existing `Withdrawn` state, which only the asker may write. A cancelled helper sends nothing and writes no decline.
- **Older builds.** An older build reading a record holding `stopped` finds an unknown word. It reads the record as `Unreadable` ("The summary was interrupted"), failing closed. `RUN_VERSION` stays 1, because the record is device-local and a downgrade is its only other reader.

### 11.11 Contradictions found in the code

1. **The desktop helper cannot be cancelled.** `HostOffload::write_for` passes `CancelToken::new()`, so its write is never cancelled, not even on quit. Sync-spec 10.1's "a cancel leaves it claimed" holds only for the worker.
2. **`HelperOutcome::Cancelled`'s doc is wrong after a withdrawal.** It says the request "stays claimed and is handed again at the next start". After a withdrawal the request is withdrawn, and nothing hands it again.
3. **4.1's list of record writers is incomplete.** It names three writers of the run record. `end_after_done`, `step_sync_stopped`'s hand-back and now `stop_waiting` also write it. Each is a read-modify-write in one transaction, so the locking rule holds.
4. **A stopped run has no account.** `steps_text` returns `None` for a run that ended with steps untried. 11.7 adds a note for `stopped` only.
5. **`measureRow` ignores the title** (11.8).
6. **The list could never show 10.1's waiting state.** Its working dot reads only this process's leases.

### 11.12 Tests (each with its control)

1. **Stop on a waiting order run.** `stop_waiting` withdraws the request and ends the run with `stopped` in one transaction; an injected fault between them leaves neither. Control: a record waiting on another request is untouched (`NotWaiting`).
2. **No continuation after a stop.** The asker's offload step settles the stopped request with no `continue_run`, also with a fresh `Offload` (a restart). Control: the same request withdrawn by the 90 s rule continues the run.
3. **Stop on a legacy request.** Stop with no record writes an ended record, and the step does not write here. Control: a legacy withdrawal without Stop writes here (`HandBack::Legacy`).
4. **`Done` already applied.** A `Done` applied before Stop gives `AlreadyWritten`: nothing withdrawn, and the run ends with that writer. Control: the same with no `Done` gives `Stopped`.
5. **A summary arriving after the stop.** It is kept, and the stop note is hidden. Control: with no summary, the note shows.
6. **The helper's cancel.** A step after a pulled, asker-authored withdrawal cancels the helper's token, and `write_for_request` returns `Cancelled` with nothing stored and no decline written. Controls: a withdrawal row last written by a third device cancels nothing; a request still claimed is not cancelled.
7. **No lock across a cancel.** A fake host's token hook takes `services.store()` with a bounded wait.
8. **Worker stop.** `Worker::cancel` cancels every registered token, and one registered after it is cancelled at once. Control: a withdrawal cancels only its own request's token.
9. **The bound.** With short `SyncTiming` and an endpoint mock that never answers, the helper's request is abandoned within one tick plus a `CANCEL_POLL` margin of the withdrawal reaching it. Control: without the withdrawal, it is still in flight after that time.
10. **The list's source.** `meetings_waiting_shown` lists a meeting with an open, authored request asked here, and `meeting_list` sets `summary_elsewhere` from it. Controls: a request asked by another device, a closed request, and one with a summary created after it are left out.
11. **Reporting moves.** `run_summary_order_reporting` calls `on_step` once per tried-and-left step that has a next step, in order, and the outcome equals `run_summary_order`'s over the same scripted writers. Control: no call for kept-local dropped steps or a failing last step.
12. **The meeting driver.** A two-step order whose first step fails sends one `summary-steps` warning with `step_moved` text before the result. Control: a first step that writes sends none.
13. **A job.** The same arrives as a `JobEvent::Warning`, and after `Offloaded` the waiting banner holds `earlier`. Control: a run asked at step 1 has no `earlier`.
14. **The stale bar (UI).** It is hidden while a re-summarise job runs or `waiting_for` is set, and back after the terminal event or a detail read without `waiting_for`. Control: with neither, it shows.
15. **The header line (UI).** `headerLine` with `writing` gives both drafts. Control: `null` gives today's line byte for byte.
16. **Stop (UI).** Stop calls `stopWaitingSummary`, and the page and list re-read. Controls: each answer's copy (`AlreadyWritten`, `NotWaiting`, a failure), and no Stop while `waiting_for` is null.
17. **Header width (UI, `headerButtons.test.tsx`).** Labels whose need fits the whole header but not the header less `TITLE_MIN_PX` show icons. Control: a header wide enough for both keeps labels. Plus a Browser-pane check of the Vite preview with an invented long title ("Quarterly planning review for the northern warehouse fit-out and supplier onboarding") at 1024 px and narrower, in light and dark.
18. **Copy.** The new keys in `summary_order_cases.json` are read by both suites (the desktop's copy tests and the UI mock's tests), with no em dash and no run of spaces.
