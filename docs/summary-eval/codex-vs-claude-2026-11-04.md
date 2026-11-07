# Codex vs Claude vs the 4B: meeting summaries (T7.4gc, 2026-11-04)

Same test as `claude-vs-4b-2026-11-04.md`, with OpenAI Codex added through the production Codex path (`earshot_llm::codex`, codex-cli 0.155.1, signed in with a personal ChatGPT account, CLI default model; the answering model is not recorded by design).

## Setup

- Five AMI dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), reference transcripts (`--source reference`), the fixed real-item lists of `prompt-rounds-2026-11-04.md` (42 decisions, 18 actions).
- Conditions: Codex whole meeting (one request after hang-up), Codex chunked (600 s chunks during the meeting, then a reduce), plus the T7.4gb summaries of Claude (whole and chunked) and the 4B (chunked, CPU), reused unchanged.
- Runs: `cargo xtask sum-eval --model codex --source reference --split dev --meeting <5> [--chunk-seconds 600] --codex-cli <path>`; results in `target/sum-eval/t74gc-codex*/` (not committed: they hold the AMI text).
- Judging: all five conditions judged again in one new round, one Claude Code judge agent per meeting, all five summaries in one packet with labels A-E shuffled per meeting, the key kept outside the packet folder, the same rules file as T7.4gb. Unblinded after all five judges had written their verdicts.

## Results

| Condition | Decisions caught | Actions caught (owner right) | Made up | Real, not on the list | Duplicates | Decision items listed |
|---|---|---|---|---|---|---|
| Codex whole meeting | 39/42 | 18/18 (18) | 4 | 30 | 3 | 75 |
| Codex chunked | 34/42 | 18/18 (18) | 5 | 32 | 13 | 81 |
| Claude whole meeting | 38/42 | 17/18 (16) | 0 | 2 | 1 | 34 |
| Claude chunked | 35/42 | 18/18 (18) | 0 | 11 | 2 | 44 |
| 4B chunked | 28/42 | 12/18 (10) | 17 | 9 | 5 | 37 |

The re-judged Claude and 4B rows match the T7.4gb round within one item (Claude whole 38 vs 37 decisions, 16 vs 16 owners; 4B 28 vs 28, made up 17 vs 15), so the two rounds agree.

Per meeting (decisions caught):

| Meeting | Real | Codex | Codex chunked | Claude | Claude chunked | 4B |
|---|---|---|---|---|---|---|
| ES2003c | 8 | 7 | 7 | 7 | 7 | 6 |
| ES2011c | 4 | 3 | 1 | 1 | 1 | 1 |
| IS1008c | 4 | 4 | 4 | 4 | 4 | 4 |
| TS3004c | 17 | 16 | 14 | 17 | 15 | 11 |
| TS3006c | 9 | 9 | 8 | 9 | 8 | 6 |

## Time

| Meeting | Codex whole (after hang-up) | Codex chunked total | Codex chunked wait during meeting |
|---|---|---|---|
| ES2003c | 38.6 s | 123.8 s | 58.8 s |
| ES2011c | 30.3 s | 91.5 s | 49.8 s |
| IS1008c | 34.7 s | 91.1 s | 45.9 s |
| TS3004c | 37.5 s | 144.6 s | 49.5 s |
| TS3006c | 28.4 s | 142.1 s | 53.1 s |

Claude for comparison: 27-39 s after hang-up whole meeting, 40-55 s wait chunked. The 4B: 531-1392 s whole, 269-520 s with chunks during the meeting. No repairs in any Codex run; all ten summaries valid.

## What the judges saw

- Codex catches as many real items as Claude (39 vs 38 decisions, every action with the right owner) but lists about twice as many decisions (75 vs 34). Most extras are real but minor (30 "real, not on the list"), which makes the summary longer to read rather than wrong.
- Every Codex "made up" item is a proposal written as a decision (the RR on the top button, a long-press power button, "mahogany red", a bright green background, a backlight "you should test", a chip later replaced), never an invented topic or person. The 4B's 17 include wrong owners and work that happened before the meeting.
- Chunked Codex repeats itself (13 duplicates) and its reduce keeps superseded choices (the regular chip after the switch to the advanced chip); chunked Claude does not.
- ES2011c is hard for everyone (only Codex whole got 3 of 4).

## Judgement calls the judges flagged

- TS3004c D11 (small, yellow scroll wheel) counted as caught with one of the two features. Needing both costs Claude whole, Codex whole, Codex chunked and the 4B one decision each.
- ES2003c A3 owner: one summary (Claude whole) gave it to the UI Designer; marked wrong under the fixed list, though the transcript is ambiguous [267].
- ES2003c: items for work done during or before the meeting are marked made up.

## Reading

Codex is a usable alternative to Claude for summaries: the same catch rate and speed, with every action right. It is wordier and more willing to promote proposals to decisions; whole-meeting beats chunked for both providers on accuracy, chunked wins on wait. No change to defaults follows from this: the choice of provider stays the user's.
