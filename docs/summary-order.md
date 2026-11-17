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
