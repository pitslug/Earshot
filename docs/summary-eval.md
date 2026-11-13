# Summary-quality evaluation (T6.20)

Measured 2026-10-25 on the desktop (x86_64-pc-windows-msvc, RTX 5080, CUDA 13.4, cuDNN 9.26). The GPU was shared with other agents' ASR evaluations during some runs, so speed figures are indicative. Command: `cargo xtask sum-eval` (`xtask/src/sum_eval.rs`, which drives the `sum_eval` example of `crates/earshot-llm-local`). Reports go to `target/sum-eval/<out>/report.json` and `table.md`. No threshold from this page is in PLAN.md 4.2: the proposals below need the owner's agreement first.

## What runs

- **Meetings.** AMI scenario meetings with human abstractive summaries (`abssumm.xml`: abstract, decisions, actions, problems), pinned in `fixtures/manifest.toml`, set `ami-sum` (licence CC BY 4.0, `fixtures/LICENSES.md`). The partition is fixed by series, a to d of each:
  - dev: ES2003, ES2011, IS1008, TS3004, TS3006 (20 meetings). This split is used for every design and settings choice.
  - test: ES2004, ES2014, IS1009, TS3003, TS3007 (20 meetings, 705 minutes of audio). This split was run once with the chosen setting.
- **Sources.**
  - `reference`: turns built from the AMI words files (a gap of 1.0 s or more, or 30 s of speech, starts a new turn; speakers named by role, for example "Project Manager").
  - `earshot` (test split only): each test meeting is replayed through the release `earshot-cli replay`. The Project Manager's headset is the mic ("Me") and the other three headsets, summed, are the far end. Transcription uses `parakeet-tdt-0.6b-v2`, followed by the production live pass and cleanup; `--summary-input` writes exactly what Earshot would summarise. All 20 replays are cached in `target/sum-eval/test/earshot/`.
- **Summariser.** The production path:
  - the `earshot_core::prompt` planner (single request or map-reduce);
  - the production templates;
  - the llama.cpp backend of `earshot-llm-local` with the GBNF grammar from `summary_grammar`, greedy decoding, the repair loop and the loop guard;
  - validation through `earshot_core::summary::validate_reply` into `SummaryV1`.

  One process runs per candidate, and the backend is taken from the llama.cpp load log.
- **Remote ceiling.** Not run: `EARSHOT_LLM_URL` was not set. The report records "not run (EARSHOT_LLM_URL is not set)". No other endpoint is called; conversation content never left the device.

## Candidates (`models/manifest.toml`, set `llm-eval`)

Licences were checked on each model card. All are Q4_K_M GGUF files.

| Candidate | Licence | Source | Size (bytes) |
|---|---|---|---|
| Qwen3 4B | Apache-2.0 | Qwen official | 2,497,280,256 |
| Qwen3 8B | Apache-2.0 | Qwen official | 5,027,783,488 |
| Qwen3 14B | Apache-2.0 | Qwen official | 9,001,752,960 |
| Qwen3 4B Instruct 2507 | Apache-2.0 | unsloth | 2,497,281,120 |
| Qwen3.5 4B | Apache-2.0 | unsloth | 2,740,937,888 |
| Qwen3.5 9B | Apache-2.0 | unsloth | 5,680,522,464 |
| Phi-4-mini-instruct | MIT | unsloth | 2,491,874,272 |
| Granite 4.2 8B | Apache-2.0 | IBM official | 5,347,917,952 |
| Qwen2.5 1.5B Instruct (T6.19 baseline, set `llm`) | Apache-2.0 | Qwen official | 1,117,320,736 |

Gemma 4 is left out, pending the owner: its licence page links a prohibited-use policy.

**Where licences and pins come from (review note A7).**

- **Licences:** every licence in this table is read from Hugging Face model cards, on 2026-10-24. No licence file is fetched or kept in the repo.
  - Official GGUFs (Qwen3 4B, 8B and 14B from Qwen; Granite 4.2 8B from IBM; Qwen2.5 1.5B from Qwen): the licence is the one the GGUF repo's own card declares.
  - Community GGUFs from unsloth (Qwen3 4B Instruct 2507, Qwen3.5 4B and 9B, Phi-4-mini-instruct): the licence is the base model's, as stated on the base model's card (Qwen for Qwen3/Qwen3.5, Microsoft for Phi-4-mini). These are third-party conversions; the converter's repo card restates the same licence.
- **Pins:** each file is pinned to a Hugging Face commit of its GGUF repo, and its sha256 equals the file's LFS oid at that commit (HF API). The pins say which bytes were evaluated, not that a conversion is faithful to its base model.

The manifest comments in `models/manifest.toml` say the same. Nothing in the `llm-eval` set ships: a shipped model gets its own entry after T6.21.

Thinking is off for every candidate. The assistant prefix is taken from each GGUF chat template: Qwen3 and Qwen3.5 use `<think>\n\n</think>\n\n`, Granite 4.2 uses `<think></think>`.

The owner narrowed the final comparison to Phi-4-mini, Qwen3 4B and Qwen3 8B. The other candidates appear only in the earlier-format aside below.

## Scoring rules (fixed before measuring unless marked)

- **ROUGE.** `earshot_core::metrics::rouge` is a port of google-research `rouge-score` at commit 0b7b841b, without stemming. 18 cases are compared exactly against values computed once with the upstream package (`metrics/rouge_reference.rs`).
  - The prediction is the summary's `tldr` followed by every `enhanced_notes` bullet, one per line. The reference is the human abstract, one sentence per line.
  - ROUGE-Lsum F1 is the main number; ROUGE-L, ROUGE-1 and ROUGE-2 are reported beside it.
  - A meeting without a valid summary scores 0 and stays in the mean.
- **Decisions and actions caught, strict rule** (`earshot_core::metrics::items`, fixed before measuring).
  - Items are compared on content tokens: ROUGE tokens with the stop words removed.
  - A pair is eligible when its token F1 is at least 0.5.
  - Matching is one-to-one: the number of matches is maximised (Hungarian assignment, weight 1 + F1).
  - Summary decisions are compared with the human `decisions` sentences, and summary action-item `task`s with the human `actions` sentences. Owner and due date are not compared.
  - Counts are summed over meetings.
- **Reference-recall rule** (defined after seeing dev data; never tuned on test). Everything is the same, except that a pair is eligible when the summary item holds at least half of the reference item's content tokens.
  - Why it exists: the strict rule misses matches that are right in meaning, because model items are much longer than the annotations. On Phi dev, only 2 of 83 reference decisions had a best pair F1 of 0.5 or more; 27 had 0.2 or more.
  - The matcher was checked on a hand-made case and against an independent Python reimplementation, which gave identical counts.
- **Coverage rule** (defined after seeing data, from a hand check of ES2004c; never tuned on test; `items::covered_items`, reported as `decisions_cov` / `actions_cov`).
  - Why it exists: on ES2004c, one reference decision sentence bundles five decisions (LCD, rubber buttons, coloured rubber covers, kinetic battery, maybe speech recognition). Qwen3 8B listed them as five items, so any one-to-one rule scores 0 of 1.
  - The rule: a reference item counts as caught when the union of all the summary's items of that list holds at least half of its content tokens. The union takes, per token, the largest count in any one item. Tokens and stop words are the same as in the other rules.
  - Many-to-one, so only caught / reference is meaningful; there is no precision.
  - Every stored summary was re-scored with it (`cargo xtask sum-eval --rescore`, no new generation).
- **ROUGE-Lsum and set order (review note A3).** Upstream `_union_lcs` builds the union of LCS token positions as a Python `set`, then iterates it in set order; the port iterates positions in ascending order (`BTreeSet`). The order cannot change the score. Hits are counted per token against counts that only decrease: a token found in the union scores only while its counts in both texts are above zero. So the hits for each token are the smaller of its occurrences in the union and its remaining counts, whatever order the positions come in. The 18 pinned upstream cases agree exactly.
- **Speed.**
  - Wall seconds per meeting, excluding the model load.
  - Prompt and reply tokens per second, from llama.cpp's own counts.
  - Repairs, truncated replies and loop-guard stops.

## Production format changes made here (T6.19/T6.20)

- **Numbered citations.** Prompts number the transcript segments `[n]`, and the model cites numbers. `validate_reply` maps them back to segment ids, so the stored `SummaryV1` and `schemas/summary.v1.json` are unchanged. The reply schema is `schemas/summary-llm.v1.json`, generated and drift-checked by `cargo xtask schema`. At most 3 citations are allowed per item, each number at most once. The prompt carries a no-filler rule.
  - **Every provider path:** the local llama.cpp grammar, the OpenAI-compatible client (`response_format`) and the Anthropic Messages client (`output_config` and the forced tool's `input_schema`) all send the reply schema and each request's own reply budget as `max_tokens`. All of them validate the reply with `validate_reply`.
  - **Replay cases:** all 16 cite numbers, including the seven `anthropic_*` cases. `tests/replay.rs` checks that no request mentions a segment UUID and that every schema sent has integer citations. Accept m4 passes over the full case set.
  - **Merges:** a merge (reduce) reply may cite only the segments its partial summaries cite (`Citable::cited_by`), since it sees no transcript. A map reply may cite only its own part.
- **Content-sized reply budgets.** Each request's reply budget is proportional to what it summarises: half of the transcript tokens for a single or map request, all of the partial-summary tokens for a merge, at least 1024 tokens, and at most the configured ceiling. The planner sizes chunks so that the prompt, the reply budget and `REPAIR_MAX_TOKENS` always fit the context.
  - The first ratios (1/3, 1/2, at least 512) truncated legitimate merge replies on dev. Phi failed 7 of 20 dev meetings, every failure a merge step cut off at the budget. They were raised on dev only.
- **Loop guard.** Generation stops when the zlib compression ratio of the last 1200 bytes of a reply passes 4.0 (checked every 32 tokens). The result is a `Repetition` error, which is distinct from truncation.
- **Repeated items.** An item that appears 3 times (`REPEAT_LIMIT`) in one list is a `Repetition` error. A second copy is dropped (`drop_repeats`). Callers see the count: `CheckedReply::duplicates_dropped`, which both summarizer reports and `sum_eval` include.
  - What counts as the same item: action items need the same owner and task, so two people with one task stay two items. Note bullets are compared within one section only, so the same bullet under two headings stays. Sections with a repeated heading are merged. All comparisons ignore case, extra whitespace and trailing punctuation.
  - The first rule made any second copy an error. On dev it failed whole meetings over one repeated topic ("User Interface" and "User interface"), which three greedy repair rounds never removed (Phi on ES2011c and TS3004d, for example).
  - This change was made by the lane on dev data, and is flagged for the owner.
- **Compact JSON grammar: tried and dropped (negative result).** A grammar that forbids whitespace between tokens made greedy decoding fail: lists never closed, because the newline before `]` was not allowed. On dev, with the earlier format:
  - Qwen3 4B went from 16 of 20 valid to 0 of 20;
  - Phi-4-mini went from ROUGE-Lsum 0.311 to 0.153;
  - Qwen2.5 1.5B went from 9 of 20 valid to 7 of 20.

  `GrammarOptions::compact` stays available, off by default, with its test. The default grammar keeps llama.cpp's `SPACE_RULE`, which allows indentation.

**Which rules the tables were measured under.** The settings-rule tables (dev S16/S32) and the first numbered test and CPU runs were measured before four validation changes made in the lane-merge review. These were safety fixes, not tuning:

- action items compared on owner and task;
- bullets compared per section;
- merges limited to their partials' citations;
- a number cited twice in one list rejected.

Dev S16, test (both sources) and the Phi-4-mini CPU timing were then **re-run** with the current code (`indented-numbered-r4`), and those re-run tables are the ones the recommendation uses. The earlier numbered runs are kept beside them for comparison. All tables carry the coverage rule from the re-score.

**What the repo can and cannot show (review note A6).**

- **The settings rule** was committed in 3db37e9 (2026-10-25 12:54 +10:00), in the `xtask/src/sum_eval.rs` module docs. Git shows that text unchanged since then.
- **The two dev rule changes** came in later commits:
  - the reply ratios in 04db537;
  - dropping duplicates in 541fd62.

  Both were made after a first dev S16 run under the settings rule showed truncated merges and failures from duplicate items.
- **Not provable from the repo:**
  - that no dev result was seen before the settings rule was written;
  - when each run happened relative to each commit.

  Run results live under `target/` and are not committed. The commit order above is the only evidence.
- **The test split was first run in the superseded format** (segment UUIDs, a fixed 3072-token reply budget). Those results are the aside below, and the test runs in the tables came after.

**Checkpoint key of each table.** This is the `GRAMMAR` string of the `sum_eval` example. It is recorded as `grammar` in every result file and required by `covers()` in `xtask/src/sum_eval.rs` before a stored result is reused.

| Table | `grammar` key | Context / reply ceiling | Result directory |
|---|---|---|---|
| Compact-grammar negative result | `compact` | 16384 / fixed 3072 | `target/sum-eval/dev-b` |
| Aside, earlier format (dev) | `indented` | 16384 / fixed 3072 | `target/sum-eval/dev` |
| Aside, earlier format (test) | none (written before the key existed) | 16384 / fixed 3072 | `target/sum-eval/aside-test-uuid-3072` |
| Settings rule, dev S16 / S32 | `indented-numbered-r3` | 16384 / 8192 and 32768 / 16384 | `target/sum-eval/dev-s16`, `dev-s32` |
| First numbered test run, first CPU timing | `indented-numbered-r3` | 16384 / 8192 | `target/sum-eval/test`, `cpu` |
| Re-run after the review fixes (dev, test, CPU) | `indented-numbered-r4` | 16384 / 8192 | `target/sum-eval/rerun-*` |
| Claude ceiling (agents on exported prompts) | `indented-numbered-r4` (validation and scoring only; nothing generated locally) | 200000 / 16000 (`ANTHROPIC_DEFAULT_BUDGET`) | prompts `target/sum-eval/claude-export`, replies `claude-medium`, `claude-high`, reports in `rerun-test` |

## Settings rule (written before the runs it decides)

- **S16:** `--context 16384 --max-output 8192`.
- **S32:** `--context 32768 --max-output 16384`.

Both settings run on dev, reference source, for Phi-4-mini, Qwen3 4B and Qwen3 8B: 60 meeting runs each. A setting's score is the mean ROUGE-Lsum F1 over its 60 runs, with failures counting 0. S32 is chosen only if its score is at least 0.01 higher; otherwise S16 is chosen, since it uses half the memory.

| Setting | Phi-4-mini | Qwen3 4B | Qwen3 8B | Mean of 60 |
|---|---|---|---|---|
| S16 | 0.2456 (17/20) | 0.1153 (8/20) | 0.3197 (20/20) | 0.2269 |
| S32 | 0.2959 (20/20) | 0.0924 (6/20) | 0.3214 (19/20) | 0.2366 |

The difference is 0.0097, below 0.01, so **S16 is chosen**. It is close. Phi alone would gain from S32: at S16, 3 of its 20 dev meetings fail in a merge step, where the reply repeats an open question 3 or more times; at S32 most meetings fit one request. That is a reason to consider a larger context for Phi on machines with the memory, but the rule was applied as written.

## Dev results (reference source, S16)

These are re-run results after the review-driven safety fixes (`indented-numbered-r4`, `target/sum-eval/rerun-dev`). They are not tuning: no setting or rule was chosen from them.

Item counts are caught / reference items. Strict and reference recall are one-to-one. Coverage is many-to-one.

| Candidate | Valid | ROUGE-L | ROUGE-Lsum | Decisions: strict | Decisions: ref. recall* | Decisions: coverage** | Actions: strict | Actions: ref. recall* | Actions: coverage** | s/meeting | Reply tok/s | Repairs | Truncated | Loop stops | Duplicates dropped |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Phi-4-mini | 17/20 | 0.157 | 0.246 | 3/83 | 6/83 | 13/83 | 3/41 | 0/41 | 1/41 | 12.3 | 199 | 9 | 0 | 3 | 4 |
| Qwen3 4B | 9/20 | 0.078 | 0.128 | 0/83 | 2/83 | 3/83 | 2/41 | 2/41 | 4/41 | 25.4 | 163 | 25 | 3 | 28 | 20 |
| Qwen3 8B | 20/20 | 0.193 | 0.319 | 3/83 | 18/83 | 27/83 | 6/41 | 7/41 | 11/41 | 26.4 | 127 | 0 | 0 | 0 | 6 |

\* Reference-recall rule: defined after seeing dev data.
\** Coverage rule: defined after seeing data.

For comparison, the settings-rule S16 run made before the safety fixes (`r3`), re-scored:

| Candidate | Valid | ROUGE-Lsum | Decisions: strict / rr / cov | Actions: strict / rr / cov |
|---|---|---|---|---|
| Phi-4-mini | 17/20 | 0.246 | 3 / 6 / 13 of 83 | 3 / 0 / 1 of 41 |
| Qwen3 4B | 8/20 | 0.115 | 0 / 2 / 3 of 83 | 2 / 2 / 3 of 41 |
| Qwen3 8B | 20/20 | 0.320 | 3 / 18 / 27 of 83 | 6 / 7 / 11 of 41 |

The safety fixes moved the dev numbers by at most one meeting and 0.013 ROUGE-Lsum.

## Test results (re-run once after the safety fixes, S16, both sources)

These are re-run results after the review-driven safety fixes, not tuning (`target/sum-eval/rerun-test`). The Claude rows are an informational ceiling; see the next section.

| Candidate | Source | Valid | ROUGE-L | ROUGE-Lsum | Decisions: strict | Decisions: ref. recall* | Decisions: coverage** | Actions: strict | Actions: ref. recall* | Actions: coverage** |
|---|---|---|---|---|---|---|---|---|---|---|
| Phi-4-mini | reference | 18/20 | 0.155 | 0.264 | 2/94 | 6/94 | 11/94 | 1/30 | 0/30 | 0/30 |
| Phi-4-mini | earshot | 19/20 | 0.160 | 0.272 | 0/94 | 2/94 | 6/94 | 2/30 | 0/30 | 0/30 |
| Qwen3 4B | reference | 14/20 | 0.132 | 0.216 | 0/94 | 4/94 | 8/94 | 2/30 | 2/30 | 3/30 |
| Qwen3 4B | earshot | 13/20 | 0.111 | 0.181 | 0/94 | 6/94 | 7/94 | 2/30 | 0/30 | 0/30 |
| Qwen3 8B | reference | 20/20 | 0.182 | 0.313 | 3/94 | 17/94 | 25/94 | 7/30 | 4/30 | 8/30 |
| Qwen3 8B | earshot | 20/20 | 0.178 | 0.297 | 6/94 | 15/94 | 26/94 | 3/30 | 3/30 | 3/30 |
| Claude Opus 5.5, agent, medium effort (ceiling) | reference | 18/20 | 0.146 | 0.263 | 11/94 | 19/94 | 27/94 | 8/30 | 7/30 | 11/30 |
| Claude Opus 5.5, agent, medium effort (ceiling) | earshot | 20/20 | 0.162 | 0.286 | 12/94 | 18/94 | 30/94 | 7/30 | 11/30 | 12/30 |
| Claude Opus 5.5, agent, high effort (ceiling) | reference | 20/20 | 0.157 | 0.280 | 9/94 | 17/94 | 23/94 | 10/30 | 11/30 | 17/30 |
| Claude Opus 5.5, agent, high effort (ceiling) | earshot | 19/20 | 0.149 | 0.263 | 12/94 | 16/94 | 29/94 | 8/30 | 8/30 | 14/30 |

Run statistics of the local models on test:

| Candidate | Source | s/meeting | Reply tok/s | Prompt tok/s | Repairs | Truncated | Loop stops | Duplicates dropped |
|---|---|---|---|---|---|---|---|---|
| Phi-4-mini | reference | 9.6 | 195 | 13885 | 4 | 0 | 0 | 1 |
| Phi-4-mini | earshot | 9.5 | 190 | 13522 | 3 | 0 | 0 | 4 |
| Qwen3 4B | reference | 21.9 | 168 | 10938 | 15 | 4 | 16 | 67 |
| Qwen3 4B | earshot | 21.6 | 167 | 10880 | 19 | 4 | 16 | 24 |
| Qwen3 8B | reference | 25.6 | 121 | 7102 | 0 | 0 | 0 | 0 |
| Qwen3 8B | earshot | 24.7 | 125 | 7326 | 0 | 0 | 0 | 0 |

Wall time per hour of meeting on the RTX 5080 (test, summing every meeting): Phi-4-mini 16 s, Qwen3 4B 37 s, Qwen3 8B 42 to 43 s.

The first numbered test run, made before the safety fixes and re-scored, is kept for comparison:

| Candidate | Source | Valid | ROUGE-Lsum | Decisions: strict / rr / cov | Actions: strict / rr / cov |
|---|---|---|---|---|---|
| Phi-4-mini | reference | 19/20 | 0.281 | 2 / 6 / 11 | 1 / 0 / 0 |
| Phi-4-mini | earshot | 19/20 | 0.272 | 0 / 2 / 6 | 2 / 0 / 0 |
| Qwen3 4B | reference | 13/20 | 0.202 | 0 / 4 / 8 | 2 / 2 / 3 |
| Qwen3 4B | earshot | 12/20 | 0.166 | 0 / 5 / 6 | 2 / 0 / 0 |
| Qwen3 8B | reference | 20/20 | 0.315 | 3 / 17 / 25 | 7 / 4 / 8 |
| Qwen3 8B | earshot | 20/20 | 0.298 | 6 / 15 / 26 | 3 / 3 / 3 |

Observations:

- **One meeting lost to the duplicate-citation rule.** Phi-4-mini lost one test meeting to the new rejection of a number cited twice in one list: IS1009d, where the last reply of a map step had an action item citing segment 484 three times in one list, and two repair rounds did not fix it. With that meeting counted as a failure, the mean ROUGE-Lsum on the reference source drops from 0.281 to 0.264. The grammar cannot forbid repeated numbers within a list. Dropping a repeated number instead of rejecting the reply is a possible follow-up for the owner.
- **Earshot transcripts cost little.** Moving from reference to Earshot transcripts costs about 0.01 to 0.02 ROUGE-Lsum for the local models. This holds even though diarisation found 5 to 9 speakers for 4 people, and "Me" was never named because of headset crosstalk; both are T6 findings to follow up.
- **The item rules disagree in a useful way.** Strict counts are too low to separate the local models. Under the coverage rule, Qwen3 8B catches about 1 decision in 4 (25 to 27 of 94). Claude catches about 1 in 3 (23 to 30) and many more actions.
- **ROUGE does not track item recall here.** Qwen3 8B has the highest ROUGE-Lsum of every row, Claude included (0.313 against 0.263 to 0.292). Claude catches 2 to 4 times as many decisions under the strict rule (9 to 12 of 94, against 3 and 6). ROUGE-Lsum against the AMI abstract rewards the abstract's narrative wording, not the decisions and actions a user needs.
- **Qwen3 4B loops in the numbered format.** In the earlier format (segment UUIDs, a fixed 3072-token budget) it produced 16 of 20 valid dev summaries. In the numbered format, most of its replies repeat the same bullet with the same citation list until the loop guard or the repetition check stops them.

## Claude ceiling (informational): Claude Code agents on the exported prompts

**How it was produced.** No API key was used.

1. `cargo xtask sum-eval --split test --export-prompts target/sum-eval/claude-export` wrote the exact request the Anthropic provider path would send for every test meeting and source:
   - `<meeting>.<source>.system.txt` and `<meeting>.<source>.user.txt`, which are the `system` field and the one user turn `MessagesClient` sends;
   - `schema.json` (`SUMMARY_LLM_SCHEMA_JSON`);
   - `index.json`.

   The budget was `ANTHROPIC_DEFAULT_BUDGET` (200000-token context, 16000-token reply ceiling). All 40 requests were single requests.
2. Claude Opus 5.5 ran as Claude Code agents at medium and at high effort. Each agent read one exported prompt and wrote its JSON reply by hand into `<meeting>.<source>.reply.json`.
3. `--import-replies <dir> --label <name>` validated every reply with `summary::validate_reply`, using the same segment numbers and citable set as the local candidates. It scored them with ROUGE and all three item rules, exactly as the local candidates.

This is not an API call. There is no structured output, timing or token count.

**Parse failures are an artefact of hand-writing.** A missing or unparsable reply counts as a failed meeting, as for every candidate. No reply was repaired.

- Medium effort: IS1009c.reference and IS1009d.reference do not parse (a stray brace mid-object).
- High effort: IS1009c.earshot does not parse.
- Every reply that parsed also passed `validate_reply`.

The API's structured output would prevent these failures. So, beside the table above (failures counting 0), these are the scores over only the replies that parsed:

| Candidate | Source | Parsed and valid | ROUGE-L | ROUGE-Lsum | Decisions: strict / rr / cov | Actions: strict / rr / cov |
|---|---|---|---|---|---|---|
| Claude Opus 5.5, agent, medium | reference | 18/18 | 0.163 | 0.292 | 11 / 19 / 27 of 82 | 8 / 7 / 11 of 26 |
| Claude Opus 5.5, agent, medium | earshot | 20/20 | 0.162 | 0.286 | 12 / 18 / 30 of 94 | 7 / 11 / 12 of 30 |
| Claude Opus 5.5, agent, high | reference | 20/20 | 0.157 | 0.280 | 9 / 17 / 23 of 94 | 10 / 11 / 17 of 30 |
| Claude Opus 5.5, agent, high | earshot | 19/19 | 0.157 | 0.277 | 12 / 16 / 29 of 91 | 8 / 8 / 14 of 26 |

The reports are `target/sum-eval/rerun-test/claude-opus-5-5-{medium,high}-agent.report.json`.

## Aside: earlier format (segment UUIDs, fixed 3072-token reply, indented grammar)

These results were superseded by the format above and are kept for reference only; they were not used for any choice. Dev split, reference source, unless marked.

| Candidate | Valid | ROUGE-Lsum (failures 0) | Truncated replies |
|---|---|---|---|
| Phi-4-mini | 20/20 | 0.311 | 1 |
| Qwen3 8B | 17/20 | 0.289 | 11 |
| Qwen3 4B | 16/20 | 0.271 | 30 |
| Qwen3.5 9B | 8/20 | 0.130 | 43 |
| Qwen3.5 4B | 8/20 | 0.121 | 51 |
| Qwen2.5 1.5B | 9/20 | 0.118 | 47 |
| Granite 4.2 8B | 7/20 | 0.085 | 39 |
| Qwen3 4B Instruct 2507 | 1/20 | 0.019 | 66 |

Test split in the same format (partial, 3072-token reply):

| Candidate | Source | Valid | ROUGE-Lsum |
|---|---|---|---|
| Phi-4-mini | reference | 20/20 | 0.270 |
| Phi-4-mini | earshot | 20/20 | 0.257 |
| Qwen3 8B | reference | 16/20 | 0.253 |
| Qwen3.5 9B | reference | 7/20 | 0.123 |
| Qwen3 14B | reference | 4/20 | 0.066 (48 truncated replies: verbose replies cut at 3072) |
| Granite 4.2 8B | reference | 4/20 | 0.058 |
| Qwen2.5 1.5B | earshot | 11/20 | 0.128 |
| Qwen2.5 1.5B | reference | 6/20 | 0.085 |

In this format the fixed reply budget decided most failures. This is why the budget is now sized to the content.

Phi-4-mini scored higher in the earlier format than in the numbered one on dev (0.311 against 0.246 at S16 and 0.296 at S32), while Qwen3 8B scored lower (0.289 against 0.320). The numbered format is therefore not a uniform gain for every model; it is much cheaper in prompt tokens, and it is the production format now.

## CPU timings (4B class)

Setup:

- **Machine:** the desktop CPU (x86-64, AVX2 build), 8 llama.cpp threads.
- **Build:** `--backend cpu`, which builds the example **without** CUDA. Its `system_info` lists only the CPU, and `check_cpu_only` refuses a CPU result from a build that lists CUDA.
- **Command:** `cargo xtask sum-eval --backend cpu --split dev --source reference --model <ids> --limit 3`.
- **Meetings:** ES2003a, b and c, 92 minutes of audio in total, at S16.

| Model | Run | Valid | Prompt tok/s | Reply tok/s | Wall time | Per hour of meeting |
|---|---|---|---|---|---|---|
| Phi-4-mini | re-run after the safety fixes (`rerun-cpu`) | 3/3 | 131 | 13.5 | 513 s in total | 336 s (5.6 min) |
| Phi-4-mini | first run (`cpu`) | 3/3 | 136 | 14.6 | 67 s, 174 s, 242 s | 317 s (5.3 min) |
| Qwen3 4B | first run only | 1/3 | 100 | 9.9 | 84 s, 1371 s, 1569 s | 1981 s (33 min); the two failures each spent over 10,000 reply tokens in loops and repairs |

**First attempt was not a CPU timing.** The first CPU run used a CUDA build with no layers offloaded, and it is not reported here. llama.cpp's op offload still ran the large prompt batches on the GPU, which gave about 2,400 prompt tok/s against 136 on the CPU alone. The harness now builds without CUDA for `--backend cpu`.

A shipped CPU build with fewer threads, or a laptop CPU, will be slower than this desktop. For a one-hour meeting, Phi-4-mini needs roughly 5 to 10 minutes after the meeting ends.

## Recommendation for T6.21, per device class

The re-run after the safety fixes does not change the recommendation.

- **Desktop with an NVIDIA GPU (RTX 5080 class):** Qwen3 8B Q4_K_M at S16 (16k context, 8k reply ceiling).
  - Evidence: it is the only local candidate valid on every dev and test meeting with both sources. It has the best ROUGE of every row, and the best item recall of the local models under all three rules. It needs about 42 s of GPU time per hour of meeting.
  - Memory: about 5 GB of weights plus a 16k KV cache.
  - Against the Claude ceiling: Qwen3 8B catches about as many decisions under the coverage rule (25 to 26 of 94, against 23 to 30). It catches far fewer under the strict rule (3 to 6, against 9 to 12), and fewer actions.
- **Laptop or desktop without a usable GPU (x86-64 CPU):** Phi-4-mini-instruct Q4_K_M (MIT) at S16.
  - Evidence: 18 and 19 of 20 valid on test (reference and Earshot sources), ROUGE-Lsum 0.26 to 0.27. Its item recall is low under every rule (at most 11 of 94 decisions, 0 to 2 of 30 actions).
  - Not recommended: Qwen3 4B, which fails 6 to 7 of 20 test meetings in the production format.
  - CPU cost: about 5.5 minutes per hour of meeting on 8 desktop threads (131 prompt tok/s, 13.5 reply tok/s).
  - Qwen3 4B costs 6 times as much on the CPU, mostly in loops.
- **Surface Pro (Snapdragon X2 Elite, ARM64, 16 GB):** Phi-4-mini on the CPU, at S16.
  - llama.cpp has no QNN path here.
  - The ARM64 speed is unmeasured until GATE-03; the x86 CPU figures above are only an indication.
  - S32 helps Phi on dev, but doubles the KV cache, which matters on 16 GB.
- **Not recommended anywhere for summaries:** Qwen2.5 1.5B (the T6.19 spike model), Qwen3 4B Instruct 2507, Qwen3.5, Granite 4.2 and Qwen3 14B. All of them failed most dev meetings in the earlier format. Only Qwen3 4B was re-run in the production format, and it loops there.
- **If decisions and actions matter more than ROUGE:** the Claude ceiling suggests the opt-in Claude provider (T6.22) as the high-quality option.

## Proposed thresholds (for the owner; not in PLAN.md 4.2)

All proposals are measured on the AMI test split (re-run after the safety fixes) with the chosen model per device class and S16, as informational acceptance rows.

| Proposed id | Proposal | Measured (Qwen3 8B, CUDA) | Measured (Phi-4-mini, CUDA) |
|---|---|---|---|
| `sum.ami_valid` | >= 95% of meetings with a valid summary, each source (GPU model); see the note for the CPU model | 100% / 100% | 90% / 95% |
| `sum.ami_rougelsum_ref` | ROUGE-Lsum F1 >= 0.28 (GPU model) or >= 0.25 (CPU model), reference source | 0.313 | 0.264 |
| `sum.ami_rougelsum_earshot` | ROUGE-Lsum F1 >= 0.27 (GPU model) or >= 0.24 (CPU model), Earshot source | 0.297 | 0.272 |
| `sum.ami_decisions_cov` | Informational only: decisions caught, coverage rule (defined after seeing data) | 25/94 and 26/94 | 11/94 and 6/94 |
| `sum.ami_decisions_rr` | Informational only: decisions caught, reference-recall rule (defined after seeing dev data) | 17/94 and 15/94 | 6/94 and 2/94 |
| `sum.gpu_seconds_per_hour` | <= 90 s of summariser wall time per hour of meeting on the desktop GPU | 42 to 43 s | 16 s |

**Note on validity for the CPU model.** The re-run changes one proposal. Phi-4-mini is now 18/20 (90%) valid on the reference source: the new duplicate-citation rejection cost it IS1009d. So the earlier 95% proposal would fail for the CPU model. The choice is the owner's:

- propose 90% for the CPU model, a value read from the test split; or
- turn a number cited twice into a dropped duplicate instead of an error (the IS1009d reply would then validate), and re-measure.

Item-catch rates under the strict rule (at most 7 of 94 decisions for the local models) are too low and too noisy to gate on.

## Second round: newer models

### Decisions

- **Candidates:** Qwen 3.5 4B, Qwen 3.5 9B, Gemma 4 12B and Qwen 3.5 27B, plus NVIDIA Nemotron Nano 9B v2 (approved for evaluation only). Not included: gpt-oss, 30B-A3B models and 31B models.
- **Qwen 3.5 14B does not exist.** The official Qwen 3.5 family is 0.8B, 2B, 4B, 9B, 27B, 35B-A3B, 122B-A10B and 397B-A17B, and the only "14B" repos are community merges. The owner chose Qwen 3.5 27B instead. It runs on the desktop GPU only, with no CPU timing.
- **Gemma 4 12B uses Google's official QAT Q4_0 GGUF** (`google/gemma-4-12B-it-qat-q4_0-gguf`). Its quantisation type differs from the other candidates' Q4_K_M.
  - Licence: Apache-2.0 per the model card and Google's licence page. The site also links a general Gemma prohibited-use page, but the licence itself adds no terms.
- **Nemotron Nano 9B v2 (NVIDIA Open Model License)** is for evaluation only. Shipping it is decided only if it wins.
- **Format and settings are unchanged:** the production format and S16. This round compares models only, and citations stay model-written.

### Plan (written before the results, the owner's faster plan)

1. Models download while other jobs run. The GPU runs strictly one job at a time.
2. Every new model is screened on dev only: 20 meetings, reference source, S16. The comparison is with the existing dev numbers of Qwen3 8B and Phi-4-mini.
3. Test (reference source) runs only for the best two new models, by the existing dev rule: mean ROUGE-Lsum over the 20 dev meetings, failures counting 0.
4. The Earshot-transcript source runs only for the overall winner.
5. CPU timing (3 dev meetings, a build without CUDA) runs only for new CPU candidates that beat Phi-4-mini on dev. The CPU candidates are Qwen 3.5 4B and 9B, Nemotron 9B and Gemma 4 12B.
6. Queue order: CPU candidates first, Qwen 3.5 27B last.

### Round-two results

All runs use S16 and the production format (`indented-numbered-r4`), with the GPU running one job at a time. Item counts are caught / reference items under the strict, reference-recall* and coverage** rules (\* and \** defined after seeing data).

**Dev screening** (reference source, 20 meetings, `target/sum-eval/round2-dev`; existing rows from `rerun-dev`):

| Model | Valid | ROUGE-L | ROUGE-Lsum | Decisions s / rr / cov (of 83) | Actions s / rr / cov (of 41) | GPU s per hour of meeting |
|---|---|---|---|---|---|---|
| Qwen 3.5 4B (Q4_K_M) | 20/20 | 0.177 | 0.316 | 9 / 22 / 28 | 7 / 6 / 9 | 44 |
| Qwen 3.5 9B (Q4_K_M) | 20/20 | 0.188 | 0.319 | 7 / 25 / 33 | 6 / 2 / 5 | 64 |
| Qwen3 8B (existing) | 20/20 | 0.193 | 0.319 | 3 / 18 / 27 | 6 / 7 / 11 | 50 |
| Phi-4-mini (existing) | 17/20 | 0.157 | 0.246 | 3 / 6 / 13 | 3 / 0 / 1 | 23 |

The dev rule (mean ROUGE-Lsum, failures counting 0) selected Qwen 3.5 9B (0.319) and Qwen 3.5 4B (0.316) for test.

**Not run:**

- **Gemma 4 12B (QAT Q4_0) and Nemotron Nano 9B v2.** Both architectures load on the pinned llama.cpp. Neither chat template (Gemma 4's `<|turn>` markers, Nemotron's `<SPECIAL_10>`/`<SPECIAL_11>`) is one of llama.cpp's built-in template families, so `llama_chat_apply_template` refuses them. On one dev meeting, every Gemma request failed with "llama.cpp does not support this model's chat template". The owner decided to skip both for now, pending a meaning-based judge on a sample.
- **Aside: Qwen 3.5 27B (Q4_K_M, about 17 GB), not a candidate.** the owner: a 17 GB download for summaries is not a product option unless it were dramatically better. Its dev run had already finished:
  - 20 of 20 meetings, 20 valid;
  - ROUGE-L 0.172, ROUGE-Lsum 0.303;
  - decisions 7 / 21 / 33, actions 4 / 5 / 9;
  - 325 s per meeting on average (609 s per hour of meeting), at 16.7 reply tok/s and 164 prompt tok/s;
  - VRAM 15.8 of 16.3 GB in use.

  It is not better than the 9B models on any headline number.

**Test, reference source** (run once for the top two, `target/sum-eval/round2-test`; existing rows from `rerun-test`):

| Model | Valid | ROUGE-L | ROUGE-Lsum | Decisions s / rr / cov (of 94) | Actions s / rr / cov (of 30) | GPU s per hour of meeting |
|---|---|---|---|---|---|---|
| Qwen 3.5 9B | 20/20 | 0.175 | 0.319 | 6 / 14 / 26 | 11 / 8 / 14 | 50 |
| Qwen 3.5 4B | 19/20 | 0.154 | 0.282 | 5 / 12 / 25 | 7 / 7 / 9 | 33 |
| Qwen3 8B (existing) | 20/20 | 0.182 | 0.313 | 3 / 17 / 25 | 7 / 4 / 8 | 43 |
| Phi-4-mini (existing) | 18/20 | 0.155 | 0.264 | 2 / 6 / 11 | 1 / 0 / 0 | 16 |
| Claude Opus 5.5 agent, medium (ceiling) | 18/20 | 0.146 | 0.263 | 11 / 19 / 27 | 8 / 7 / 11 | - |
| Claude Opus 5.5 agent, high (ceiling) | 20/20 | 0.157 | 0.280 | 9 / 17 / 23 | 10 / 11 / 17 | - |

**Test, Earshot source, overall winner only** (Qwen 3.5 9B, `target/sum-eval/round2-test-earshot`):

| Model | Valid | ROUGE-L | ROUGE-Lsum | Decisions s / rr / cov (of 94) | Actions s / rr / cov (of 30) | GPU s per hour of meeting |
|---|---|---|---|---|---|---|
| Qwen 3.5 9B | 20/20 | 0.172 | 0.314 | 4 / 13 / 18 | 3 / 5 / 7 | 49 |
| Qwen3 8B (existing) | 20/20 | 0.178 | 0.297 | 6 / 15 / 26 | 3 / 3 / 3 | 42 |

**CPU timings** (no-CUDA build, 8 threads, ES2003a to c, 92 minutes of audio, `target/sum-eval/round2-cpu`):

| Model | Valid | Prompt tok/s | Reply tok/s | Wall time | Per hour of meeting |
|---|---|---|---|---|---|
| Qwen 3.5 4B | 3/3 | 129 | 14.4 | 102 s, 385 s, 539 s | 672 s (11.2 min) |
| Qwen 3.5 9B | 3/3 | 74 | 8.7 | 150 s, 689 s, 662 s | 983 s (16.4 min) |
| Phi-4-mini (existing) | 3/3 | 131 | 13.5 | 513 s in total | 336 s (5.6 min) |

The Qwen 3.5 timings were taken on a quiet machine, except for one short debug compile during Qwen 3.5 4B's ES2003b. An earlier attempt shared the CPU with the 27B GPU run; it is set aside in `target/sum-eval/aside-round2-cpu-contended` and not reported. Qwen 3.5 4B writes about twice as many reply tokens as Phi-4-mini on the same meetings (812 to 5847 per meeting). With the same reply speed, that is why it needs twice the time.

### Updated T6.21 recommendation (round two)

- **Desktop GPU:** Qwen 3.5 9B (Apache-2.0, 5.7 GB) replaces Qwen3 8B.
  - It is valid on every dev and test meeting with both sources. It has the best ROUGE-Lsum on test (0.319 reference, 0.314 Earshot) and the most actions caught on the reference source (strict 11 of 30).
  - It needs 50 s of GPU time per hour of meeting, against 43 s for Qwen3 8B.
  - Caveat: on the Earshot source it catches fewer decisions than Qwen3 8B under the looser rules (coverage 18 against 26 of 94). The meaning-based judge sample should settle that before the choice is final.
- **CPU-only x86-64:** Qwen 3.5 4B is better than Phi-4-mini on every quality number:
  - dev ROUGE-Lsum 0.316 against 0.246, test 0.282 against 0.264;
  - valid 20/20 and 19/20 against 17/20 and 18/20;
  - several times the decisions and actions caught.

  It costs about 11 minutes per hour of meeting on 8 desktop threads, against 5.6 for Phi-4-mini. If that is acceptable after a call, it is the CPU pick, with Phi-4-mini as the fast option. Qwen 3.5 9B is not a CPU candidate at 16 minutes per hour.
- **Surface (ARM64, 16 GB):** the same choice as CPU-only, Qwen 3.5 4B or Phi-4-mini. ARM64 speed is unmeasured until GATE-03.
- **Not candidates:** Qwen 3.5 27B (size and speed); Gemma 4 and Nemotron (their chat templates cannot be formatted by the pinned llama.cpp).

## Chunking experiment: summaries made during the meeting

### Rule (written before any chunked run)

- **Chunks.** `cargo xtask sum-eval --chunk-seconds 600` (`earshot_core::prompt::time_chunks`, also used by local summaries in the runtime). Window `k` is meeting time `[k x 600 s, (k + 1) x 600 s)` from the meeting start. A transcript segment belongs to the window that holds its start time, so chunks are cut only at segment boundaries (reference-source turns are at most 30 s long). The chunks are the non-empty windows in order; the last one is whatever is left at hang-up, however short. A meeting with one non-empty window is summarised by the ordinary unchunked request.
- **Requests.** One map request per chunk, built by `earshot_core::prompt::plan_chunks`, which builds exactly the map requests the planner builds for its own chunks (production templates, numbering, citable set, content-sized reply budget). Then the existing reduce (`reduce_groups`, `reduce_prompt`), planned for the model's whole context. Validation, repairs, the loop guard, the grammar and S16 (`--context 16384 --max-output 8192`) are unchanged. A chunk that does not fit the context fails the meeting (it is never split further).
- **Known deviation.** Each map prompt says "part i of n". A real in-meeting run would not know n until hang-up; the production prompt is kept unchanged for this experiment.
- **Format key.** `indented-numbered-r4-chunk600` (`format_key` in the example and in `xtask/src/sum_eval.rs`), so chunked and unchunked rows are never reused for each other.
- **Timing.** Every step's wall time is recorded (`report.steps`, repairs counted in their step). Per meeting: `map_s` (all map steps), `last_map_s`, `reduce_s` (all reduce steps; they all run after the last map) and `hangup_s` = `last_map_s` + `reduce_s`, the wait after the meeting ends when every earlier map step ran during the meeting. For an unchunked run `hangup_s` is the whole run.
- **Runs.** GPU (CUDA), all 20 dev meetings, reference source: Phi-4-mini, Qwen 3.5 4B and Qwen 3.5 9B, scored with ROUGE and all three item rules, beside their existing unchunked dev rows (Phi-4-mini from `rerun-dev`, Qwen 3.5 from `round2-dev`). CPU (no-CUDA build, 8 threads, quiet machine): the same three models on ES2003a to c, beside their existing unchunked CPU timings. Nothing here touches the test split.

### Results: GPU quality (dev, reference source, 20 meetings, `target/sum-eval/chunk-dev`)

The rule above produced 71 map steps over the 20 meetings (2 to 5 per meeting); no meeting had a single window. Item counts are caught / reference items under the strict, reference-recall* and coverage** rules (decisions of 83, actions of 41). The wait at hang-up for an unchunked run is the whole run.

| Model | Mode | Valid | ROUGE-L | ROUGE-Lsum | Decisions s / rr / cov | Actions s / rr / cov | At hang-up, s per meeting | Reduce alone, s per meeting | All GPU work, s per hour of meeting |
|---|---|---|---|---|---|---|---|---|---|
| Phi-4-mini | unchunked (`rerun-dev`) | 17/20 | 0.157 | 0.246 | 3 / 6 / 13 | 3 / 0 / 1 | 12.3 | - | 23 |
| Phi-4-mini | chunked | 19/20 | 0.160 | 0.262 | 1 / 6 / 12 | 2 / 1 / 1 | 6.1 | 3.5 | 25 |
| Qwen 3.5 4B | unchunked (`round2-dev`) | 20/20 | 0.177 | 0.316 | 9 / 22 / 28 | 7 / 6 / 9 | 23.5 | - | 44 |
| Qwen 3.5 4B | chunked | 20/20 | 0.164 | 0.285 | 7 / 16 / 31 | 8 / 6 / 11 | 18.0 | 13.0 | 61 |
| Qwen 3.5 9B | unchunked (`round2-dev`) | 20/20 | 0.188 | 0.319 | 7 / 25 / 33 | 6 / 2 / 5 | 33.8 | - | 64 |
| Qwen 3.5 9B | chunked | 20/20 | 0.174 | 0.297 | 11 / 24 / 35 | 7 / 5 / 13 | 24.2 | 17.6 | 86 |
| Claude Opus 5.5 agent, medium (ceiling, see below) | unchunked | 20/20 | 0.167 | 0.302 | 14 / 21 / 32 | 7 / 5 / 12 | - | - | - |

- **Quality.** Chunking lowers ROUGE-Lsum for both Qwen models (-0.031 for the 4B, -0.022 for the 9B) and raises it for Phi-4-mini (+0.016, two more valid meetings). Coverage rises a little for the Qwen models (4B: decisions 28 -> 31, actions 9 -> 11; 9B: decisions 33 -> 35, actions 5 -> 13). The strict and reference-recall counts move both ways. Small chunks do not bring the 4B up to the 9B.
- **Failures.** Phi-4-mini chunked failed one meeting, TS3004d: a map step repeated an open question and the repairs did not remove it. Unchunked it failed TS3004d and TS3006b (merge steps) and TS3006d (a map step).
- **Time.** The wait at hang-up falls by 25 to 50%. All GPU work rises 6 to 35% (more requests and a reduce on every meeting).

### Results: CPU timings (no-CUDA build, 8 threads, ES2003a to c, 92 minutes of audio, `target/sum-eval/chunk-cpu`)

No other job of this lane ran during the timing (CPU load 3% before the start); other agents' work on the host cannot be excluded. Every run was valid (3/3). Unchunked rows are from `rerun-cpu` (Phi-4-mini) and `round2-cpu` (Qwen 3.5). The unchunked runs of ES2003b and c were already map-reduce at S16 (2 maps and a reduce).

| Model | Mode | At hang-up per meeting (a, b, c) | At hang-up per hour of meeting | Reduce alone per hour | All CPU work per hour | One map step (10 min of meeting) |
|---|---|---|---|---|---|---|
| Phi-4-mini | unchunked | 70 s, 186 s, 257 s | 336 s (5.6 min) | - | 336 s | - |
| Phi-4-mini | chunked | 95 s, 152 s, 154 s | 263 s (4.4 min) | 160 s | 550 s | 42 to 82 s |
| Qwen 3.5 4B | unchunked | 102 s, 385 s, 539 s | 672 s (11.2 min) | - | 672 s | - |
| Qwen 3.5 4B | chunked | 210 s, 414 s, 273 s | 587 s (9.8 min) | 390 s | 1084 s | 80 to 134 s |
| Qwen 3.5 9B | unchunked | 150 s, 689 s, 662 s | 983 s (16.4 min) | - | 983 s | - |
| Qwen 3.5 9B | chunked | 201 s, 424 s, 416 s | 682 s (11.4 min) | 428 s | 1327 s | 98 to 173 s |

- **Reduce dominates the wait.** On the CPU the final reduce is 60 to 70% of the wait at hang-up. It is as long as the partial summaries it merges, and its reply is about as long, at 9 to 14 reply tok/s.
- **Short meetings get slower.** ES2003a (19 minutes, two windows) waits longer chunked than unchunked for every model: one single request becomes a map plus a reduce.
- **Map steps keep up.** Every map step took well under the 10 minutes of meeting it covers. During a real meeting it would share the CPU with live transcription, which was not running here.

## Meaning judge: packets and verdict format (T6.20a)

Five dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), seven summaries each: Claude Opus 5.5 (agent, medium), Phi-4-mini, Qwen 3.5 4B and Qwen 3.5 9B unchunked (the existing dev summaries) and the same three models chunked (the chunking experiment).

**Packets** (`sum_eval --judge-packets <dir>`, `judge.rs`): `<dir>/<meeting>/reference.json` holds the annotated decisions (`D1`, `D2`, ...) and action items (`A1`, ...), and `<dir>/<meeting>/summary.<model>.json` each summary with citations removed (decisions `S-D1`, ..., action items `S-A1`, ... with owner, task and due, plus the tldr and the notes). A meeting without a valid summary has `"valid": false` and empty lists.

**Verdict file** (one per meeting and model, written by the judge): `<dir>/<meeting>/verdict.<model>.json`:

```json
{
  "meeting": "ES2003c",
  "model": "<model, as in the summary packet's file name>",
  "decisions": [
    {"id": "D1", "verdict": "covered", "reason": "S-D2 states the same choice."}
  ],
  "action_items": [
    {"id": "A1", "verdict": "missing", "reason": "No summary item mentions the drawings."}
  ]
}
```

- Every reference item of `reference.json` gets exactly one entry in its list, and no other id appears.
- `verdict` is one of:
  - `covered`: the summary states the item's content in meaning (anywhere: a decision, an action item, a note bullet or the tldr; several summary items together may cover one reference item);
  - `contradicted`: the summary states something that conflicts with the item (for example the opposite choice, or a different owner or outcome);
  - `missing`: neither.
- `reason` is one non-empty line naming the summary item(s) used, or why nothing matches.
- Owner and due date count only when the reference item states them.

**Import** (`sum_eval --import-verdicts <dir> --annotations <ami annotations> --meetings dev=<list> --out <file>.json`): fails closed on a wrong meeting or model name, a missing, repeated or unknown id, a verdict outside the three, an empty or multi-line reason, a `reference.json` that differs from the annotations, or a summary packet without a verdict. It writes, per model, covered / contradicted / missing counts for decisions and for action items (summed over the meetings, and per meeting) to `<file>.json`, and the table to `<file>.md`.

**Blinding (added by the lane, for the owner to accept or drop).** With `--blind-key <file>`, `--judge-packets` names each meeting's summaries `S1` to `S7` instead of by model (`judge::blind_labels`: ranked by an FNV-1a hash of meeting and model, so the order differs between meetings), and writes the label-to-model key to `<file>`, which must be outside the packet directory. A blinded packet of an invalid summary keeps `"valid": false` but no failure text, which could name its source (`judge::blind`). `--import-verdicts` with the same `--blind-key` maps the labels back to models. `--judge-packets` also writes a fingerprint of the packet set (`judge::packet_fingerprint`: every `reference.json` and `summary.*.json` of the meetings) to `packet-set.json` in the packet directory and as `packet_set` in the key; `--import-verdicts` fails unless both agree with the packets there now, and fails on a verdict file without a summary packet. Rounds written before this have neither record and are imported with a printed note. Judges are then not shown which model wrote a summary (Claude or a local model, chunked or not). Summary text can still give hints, for example length and style.

### Claude dev ceiling (all 20 dev meetings, reference source, unchunked)

- **How it was made.** `cargo xtask sum-eval --split dev --source reference --export-prompts target/sum-eval/claude-export-dev` wrote 20 single requests (the same format as the test export). Claude Opus 5.5 ran as Claude Code agents at medium effort and wrote the replies into `target/sum-eval/claude-dev-medium`.
- **One difference from the test run.** This time the agents were allowed one JSON syntax check of their own files. All 20 replies parse.
- **Import.** `--import-replies --label claude-opus-5-5-medium-agent-dev` put the report in `target/sum-eval/claude-dev`. All 20 are valid. The scores are in the GPU quality table above.

### Judge packets as written (2026-10-26)

- **Where.** `target/sum-eval/judge-dev/<meeting>/` for ES2003c, ES2011c, IS1008c, TS3004c and TS3006c, blinded. The key is `target/sum-eval/judge-dev-key.json`.
- **Seven summaries per meeting.** All 35 packets are valid summaries:
  - `claude-opus-5-5-medium-agent`, from `claude-dev-medium` via `--judge-replies`;
  - `phi-4-mini` (`rerun-dev`), `qwen3.5-4b` and `qwen3.5-9b` (`round2-dev`), unchunked;
  - `phi-4-mini-chunked`, `qwen3.5-4b-chunked` and `qwen3.5-9b-chunked` (`chunk-dev`).
- **Command** (the release example run directly, with llama.cpp's `bin` directory on PATH):

  ```
  sum_eval.exe --annotations <fixtures>/ami/annotations --meetings dev=ES2003c,ES2011c,IS1008c,TS3004c,TS3006c --judge-packets target/sum-eval/judge-dev --blind-key target/sum-eval/judge-dev-key.json --summaries phi-4-mini=<rerun-dev>/phi-4-mini-instruct-q4_k_m-cuda.json --summaries qwen3.5-4b=<round2-dev>/qwen3.5-4b-q4_k_m-cuda.json --summaries qwen3.5-9b=<round2-dev>/qwen3.5-9b-q4_k_m-cuda.json --summaries phi-4-mini-chunked=<chunk-dev>/phi-4-mini-instruct-q4_k_m-cuda.json --summaries qwen3.5-4b-chunked=<chunk-dev>/qwen3.5-4b-q4_k_m-cuda.json --summaries qwen3.5-9b-chunked=<chunk-dev>/qwen3.5-9b-q4_k_m-cuda.json --judge-replies target/sum-eval/claude-dev-medium --label claude-opus-5-5-medium-agent
  ```

- **Import, once the verdicts are in:** the same `--annotations` and `--meetings`, plus `--import-verdicts target/sum-eval/judge-dev --blind-key target/sum-eval/judge-dev-key.json --out target/sum-eval/judge-dev-verdicts.json`.
- **Checked before handing over.** The import was tried end to end on a copy with synthetic verdicts: it counted per model through the key, and it refused a missing verdict file.

### Results: meaning judge, round one (2026-10-26, `target/sum-eval/judge-dev-verdicts.md`)

Five dev meetings (22 reference decisions, 11 reference action items), seven blinded summaries per meeting, one Claude Code agent (Opus 5.5) per meeting with the brief recorded in the coordinator's log.

| Model | Decisions covered | Contradicted | Missing | Actions covered | Contradicted | Missing |
|---|---|---|---|---|---|---|
| Claude Opus 5.5 (agents, medium) | 21 | 0 | 1 | 11 | 0 | 0 |
| Qwen 3.5 9B | 19 | 2 | 1 | 6 | 3 | 2 |
| Qwen 3.5 9B, chunked | 18 | 1 | 3 | 8 | 2 | 1 |
| Qwen 3.5 4B | 17 | 2 | 3 | 8 | 0 | 3 |
| Qwen 3.5 4B, chunked | 18 | 1 | 3 | 6 | 2 | 3 |
| Phi-4-mini | 8 | 2 | 12 | 6 | 2 | 3 |
| Phi-4-mini, chunked | 5 | 1 | 16 | 3 | 0 | 8 |

Hand check: four of the contradicted decisions are TS3006c D2 ("double-curved"). In the transcript the Project Manager says single curved instead, the Industrial Designer argues for double curved again, and it is not settled, so those summaries are faithful to the meeting and the reference is the doubtful side. The other verdicts checked (IS1008c, all seven summaries) hold.

### Gemma 4 12B (QAT Q4_0), 2026-10-26

**Prompt bug in the first measurements.** Every Gemma request below that names `chunk-dev-gemma`, `round2-dev-gemma`, `chunk-cpu-gemma` or judge round two ended with the empty thought channel twice: `chat_format::gemma4` already writes it, and the `sum_eval` candidate also appended it as `assistant_prefix` (found by the lane review; fixed in f5e8ad8, pinned by `sum_eval_gemma4_prefix_does_not_repeat_the_formatters_thought_channel`). Those numbers were measured on an out-of-template prompt and are kept only as labelled history. The corrected automatic scores follow; judge round two and the CPU timing were not repeated (Gemma 4 is not shipped).

- **Corrected automatic scores (dev, 20 meetings, reference source, `chunk-dev-gemma-fix`, `round2-dev-gemma-fix`).** Chunked: 20/20 valid, ROUGE-L 0.172, ROUGE-Lsum 0.296, decisions covered 32/83, actions covered 6/41, 4 repairs, 3 loop-guard stops, 34.4 s at hang-up per meeting on the GPU. Unchunked: 20/20 valid, ROUGE-L 0.187, ROUGE-Lsum 0.330, decisions covered 33/83, actions covered 3/41, 2 repairs, 2 loop-guard stops. Qwen 3.5 9B chunked for comparison: 20/20, ROUGE-L 0.174, decisions 35/83, actions 13/41, no loop-guard stops.

The rest of this section is the first, bugged measurement:

- **Chat format.** `chat_format::gemma4` reproduces the plain text path of the template embedded in the GGUF (system turn, `<|turn>user` / `<|turn>model` turns, an empty thought channel opening the reply); it is used only when llama.cpp refuses the template and every Gemma 4 marker is present. Expected strings in its tests are derived by hand from the template (no Jinja renderer on the host).
- **Automatic scores (dev, 20 meetings, reference source, `chunk-dev-gemma`, `round2-dev-gemma`).** Chunked: 20/20 valid, ROUGE-L 0.169, decisions covered 28/83, actions covered 3/41, 5 repairs, 4 loop-guard stops, 34.5 s at hang-up per meeting on the GPU. Unchunked: 18/20 (ES2011c and IS1008d invalid after 3 requests: repeated items), decisions covered 32/83, actions 3/41, 6 loop-guard stops. Gemma writes about half as many action items as Qwen 3.5 9B (53 against 108 over 20 meetings), and in different words, which the word-overlap rules miss.
- **Meaning judge, round two** (same five meetings and brief, four blinded summaries per meeting, two of them anchors from round one; `target/sum-eval/judge-dev-gemma-verdicts.md`):

| Model | Decisions covered | Contradicted | Missing | Actions covered | Contradicted | Missing |
|---|---|---|---|---|---|---|
| Claude Opus 5.5 (anchor) | 21 | 0 | 1 | 11 | 0 | 0 |
| Qwen 3.5 9B, chunked (anchor) | 19 | 1 | 2 | 8 | 2 | 1 |
| Gemma 4 12B, chunked | 19 | 0 | 3 | 9 | 1 | 1 |
| Gemma 4 12B | 13 | 2 | 7 | 3 | 2 | 6 |

  The anchors moved by at most one item between rounds (Qwen 3.5 9B chunked: 18 -> 19 decisions covered), so the two rounds can be read together. The unchunked row includes ES2011c, where the summary was invalid (every item missing).
- **CPU timing** (no-CUDA build, 8 threads, ES2003a to c, `chunk-cpu-gemma`): 3/3 valid; at hang-up 343 s, 1789 s and 588 s, 2720 s over 92 minutes of audio = 29.7 min per hour of meeting (Qwen 3.5 9B chunked: 11.4 min). ES2003b's reduce took 1563 s with 2 repairs and 2 loop-guard stops. Prompt processing 40 tok/s against 77 for Qwen 3.5 9B.
- **Reading (from the bugged measurement).** Chunked Gemma 4 matches chunked Qwen 3.5 9B under the judge (differences within one item), is a larger download (7.0 against 5.7 GB), loops and repairs more, and is 2.6x slower on the CPU. It is not a better default; the decision is the owner's.

### Decision and the test run of the default

- **Default:** Qwen 3.5 9B, chunked (600 s); Qwen 3.5 4B is the low-memory fallback; Gemma 4 is not shipped. No summary-quality threshold: the numbers in this document are informational, and quality is tuned through the prompt (T6.20b).
- **Test split, once, chunked** (reference source, 20 meetings, GPU, `target/sum-eval/chunk-test`):

| Model | Valid | ROUGE-L | ROUGE-Lsum | Decisions covered | Actions covered | At hang-up per meeting (GPU) |
|---|---|---|---|---|---|---|
| Qwen 3.5 9B | 20/20 | 0.166 | 0.300 | 21/94 | 10/30 | 22.3 s |
| Qwen 3.5 4B | 20/20 | 0.151 | 0.274 | 17/94 | 11/30 | 18.2 s |

## T6.20b prompt variants

Goal: better summaries from the prompt alone, on the shipped default only: Qwen 3.5 9B (`qwen3.5-9b-q4_k_m`), chunked 600 s, reference source, GPU. Everything below up to "Results" was written and committed before any variant was run.

### What a variant is

- `earshot_core::prompt::PromptVariant`: `current` (the production rules, `templates/system.md`) or one of four alternative rules files in `src/prompt/templates/variants/`. A variant replaces only the shared rules of the system message; the template text, map and reduce instructions, transcript numbering, schema, grammar, validation and repairs are the same. It travels on `SummaryInput::prompt_variant` (left out of the JSON when `current`, so stored inputs are unchanged); production never sets it.
- Selected for evaluation only: `cargo xtask sum-eval --prompt-variant <name>` (local runs; refused for exports, imports and re-scoring), passed to the `sum_eval` example as `--prompt-variant`. Each variant has its own format key, `indented-numbered-r4-chunk600-prompt-<name>`, built the same way in both places (`format_key` in `examples/sum_eval/main.rs` and `xtask::sum_eval`); the control keeps `indented-numbered-r4-chunk600`. xtask refuses to overwrite a result file made with another format key, so every variant runs into its own `--out`.
- Snapshots: `src/prompt/snapshots/variant_<name>_single_general.txt` (the general single request of the prompt tests); each differs from `single_general.txt` only in the rules lines and the token estimate.
- Not possible through the prompt: listing decisions and actions before the notes (the grammar fixes the key order: title, tldr, enhanced_notes, decisions, action_items, ...), and a null owner (`owner` is a required string; "Unassigned" stays the empty value).

### Variants and why (written before measuring)

Round one of the meaning judge (5 dev meetings, Qwen 3.5 9B chunked) found the weak spots: actions were covered 8 of 11 with 2 contradicted, both a wrong owner (ES2003c A2: the evaluation plan given to the Project Manager instead of the Marketing Expert; IS1008c A2: the display cost given to the Industrial Designer instead of the Marketing Expert), and TS3006c A2 (an evaluation task for the next meeting) was missed. Decisions were covered 18 of 22: TS3004c D3 and D5 missing (the battery and the LCD choice), ES2003c D1 missing (the summary merged the battery and chip choices into one decision without the battery type), and TS3006c D2 contradicted by a summary that kept a proposal that was not the final word. Current rule 5 says "`owner` is the speaker's name as it appears in the transcript", which reads as "the person who spoke the line".

1. **`owners`** (rule 5 rewritten). One action item per task that someone is to do after the meeting, including preparation for the next meeting; a task for two people is one item each, two tasks for one person two items. The owner is the person who will do the task, often addressed or named by someone else ("can you ...", a role named), never the speaker of the line by default; "Unassigned" when nobody is named. Why: both contradicted actions are an owner taken from who spoke, and the missed TS3006c A2 is a next-meeting task; bundled tasks hide a second owner.
2. **`decisions`** (a new rule 6). A decision is what the group settled (agreed or accepted, including a choice the person leading the meeting stated and nobody objected to); proposals, options argued for and questions still open are not decisions and go to the notes or `open_questions`; when a choice changed later, keep the final one; one choice per decision, each a short sentence naming what was chosen. Why: the missing decisions are choices folded into a longer decision or into the notes, and the contradicted one is a proposal recorded as settled. Expected risk: fewer decisions overall, if the model reads "settled" too strictly.
3. **`items`** (`owners` and `decisions` together). Why: the two rules address different lists and should add up; this tests whether they interfere (a longer system message on a 9B model).
4. **`lean`** (`items` plus a rewritten rule 4: at most four notes sections of at most five bullets, no decision or action repeated in the notes). Why: the grammar puts the notes before the decisions and action items, so long notes use reply budget and the model's attention before it reaches the lists the judge found weak; repeated items also feed the repetition and duplicate checks. Expected risk: lower ROUGE-L, which is computed on the tldr and the notes.

### Runs

- The current prompt (control) and each variant on all 20 dev meetings, reference source, `--chunk-seconds 600`, CUDA, S16 (`--context 16384 --max-output 8192`), one `--out` per prompt under `target/sum-eval/t620b-<name>`. The control is re-run with this build rather than reused, so every row comes from the same code (the stored `chunk-dev` row has the same format key and is listed beside it).
- Blinded judge packets for ES2003c, ES2011c, IS1008c, TS3004c and TS3006c with the four variants, the control and the Claude ceiling as an anchor (`--judge-replies <claude-dev-medium> --label claude-opus-5-5-medium-agent`), the key outside the packet directory. Judged with the T6.20 brief, one Claude Code agent per meeting.

### Selection rule (written before measuring)

A variant wins only if, against the current prompt in the same judge round:

1. it covers more items under the meaning judge on the 5 judged dev meetings, counting decisions covered plus actions covered (22 + 11 reference items);
2. it has no more contradicted items (decisions plus actions) than the current prompt;
3. its dev ROUGE-L (20 meetings) is no more than 0.01 below the current prompt's;
4. its valid count on the 20 dev meetings is not lower.

Among variants that pass, the one with the most items covered wins; then fewer contradicted; then higher dev ROUGE-L; a remaining tie, and any tie with the current prompt, goes to the current prompt. The winner (if any) runs the test split once (20 meetings, reference source, chunked, GPU) and becomes the default with a format key bump; otherwise the current prompt stays.

### Results: automatic scores (dev, 20 meetings, reference source, chunked 600 s, CUDA, 2026-10-26)

Out directories `target/sum-eval/t620b-<prompt>` of the main checkout (command per prompt: `cargo xtask sum-eval --model qwen3.5-9b-q4_k_m --split dev --source reference --chunk-seconds 600 --backend cuda --target-dir C:\eb-t620b --prompt-variant <prompt> --out target/sum-eval/t620b-<prompt>`). The control re-run gave the stored `chunk-dev` summaries byte for byte (20 of 20 identical), so greedy decoding on this build reproduces T6.20. Item counts are caught / reference items under the strict, reference-recall and coverage rules. "Written" counts the decisions and action items the summaries hold.

| Prompt | Format key | Valid | ROUGE-L | ROUGE-Lsum | Decisions s / rr / cov (of 83) | Actions s / rr / cov (of 41) | Decisions / actions written | Repairs | Loop-guard stops | At hang-up, s per meeting |
|---|---|---|---|---|---|---|---|---|---|---|
| current (stored `chunk-dev`) | `indented-numbered-r4-chunk600` | 20/20 | 0.174 | 0.297 | 11 / 24 / 35 | 7 / 5 / 13 | 111 / 108 | 0 | 0 | 24.2 |
| current | `indented-numbered-r4-chunk600` | 20/20 | 0.174 | 0.297 | 11 / 24 / 35 | 7 / 5 / 13 | 111 / 108 | 0 | 0 | 23.2 |
| owners | `indented-numbered-r4-chunk600-prompt-owners` | 20/20 | 0.180 | 0.322 | 11 / 29 / 43 | 5 / 7 / 16 | 118 / 175 | 1 | 1 | 22.3 |
| decisions | `indented-numbered-r4-chunk600-prompt-decisions` | 20/20 | 0.178 | 0.311 | 13 / 23 / 41 | 7 / 4 / 11 | 116 / 94 | 0 | 0 | 19.6 |
| items | `indented-numbered-r4-chunk600-prompt-items` | 20/20 | 0.188 | 0.317 | 16 / 24 / 37 | 6 / 8 / 18 | 109 / 160 | 0 | 0 | 20.3 |
| lean | `indented-numbered-r4-chunk600-prompt-lean` | 20/20 | 0.186 | 0.324 | 16 / 26 / 41 | 7 / 5 / 14 | 128 / 165 | 0 | 0 | 20.1 |

- Every variant keeps 20/20 valid and raises ROUGE-L (+0.004 to +0.014) and ROUGE-Lsum (+0.014 to +0.027), so rules 3 and 4 of the selection rule pass for all four; the decision is left to the meaning judge (rules 1 and 2).
- `owners` and the variants that include it write many more action items (160 to 175 against 108): the one-task-per-item rule splits bundled tasks. Coverage rises (actions 13 -> 14 to 18) while the strict rule moves both ways. `decisions` alone writes fewer action items (94) and covers fewer actions (11).
- `owners` had the only repair and loop-guard stop (one each, still valid).

### Judge packets, round three (2026-10-26)

- **Where.** `target/sum-eval/judge-dev-t620b/<meeting>/` of the main checkout for ES2003c, ES2011c, IS1008c, TS3004c and TS3006c, blinded; the key is `target/sum-eval/judge-dev-t620b-key.json` (outside the packet directory).
- **Six summaries per meeting** (30 packets, all valid): `qwen3.5-9b-chunked-current` (the control), `qwen3.5-9b-chunked-owners`, `-decisions`, `-items`, `-lean`, and the anchor `claude-opus-5-5-medium-agent` (from `claude-dev-medium` via `--judge-replies`). The control is the round-one `qwen3.5-9b-chunked` summary (identical bytes), so it is also an anchor between rounds.
- **Command** (the release example run directly, with llama.cpp's `bin` directory of the build on PATH):

  ```
  sum_eval.exe --annotations <fixtures>/ami/annotations --meetings dev=ES2003c,ES2011c,IS1008c,TS3004c,TS3006c --judge-packets target/sum-eval/judge-dev-t620b --blind-key target/sum-eval/judge-dev-t620b-key.json --summaries qwen3.5-9b-chunked-current=<t620b-current>/qwen3.5-9b-q4_k_m-cuda.json --summaries qwen3.5-9b-chunked-owners=<t620b-owners>/qwen3.5-9b-q4_k_m-cuda.json --summaries qwen3.5-9b-chunked-decisions=<t620b-decisions>/qwen3.5-9b-q4_k_m-cuda.json --summaries qwen3.5-9b-chunked-items=<t620b-items>/qwen3.5-9b-q4_k_m-cuda.json --summaries qwen3.5-9b-chunked-lean=<t620b-lean>/qwen3.5-9b-q4_k_m-cuda.json --judge-replies <claude-dev-medium> --label claude-opus-5-5-medium-agent
  ```

- **Import, once the verdicts are in:** the same `--annotations` and `--meetings`, plus `--import-verdicts target/sum-eval/judge-dev-t620b --blind-key target/sum-eval/judge-dev-t620b-key.json --out target/sum-eval/judge-dev-t620b-verdicts.json`.

### Results: meaning judge, round three (2026-10-26, `target/sum-eval/judge-dev-t620b-verdicts.md`)

Five dev meetings (22 reference decisions, 11 reference action items), six blinded summaries per meeting, one Claude Code agent per meeting with the T6.20 brief (run by the coordinator).

| Prompt | Decisions covered | Contradicted | Missing | Actions covered | Contradicted | Missing | Covered total (of 33) | Contradicted total |
|---|---|---|---|---|---|---|---|---|
| Claude Opus 5.5 (anchor) | 21 | 0 | 1 | 11 | 0 | 0 | 32 | 0 |
| current (T6.20 rules; anchor) | 19 | 1 | 2 | 8 | 2 | 1 | 27 | 3 |
| owners | 16 | 2 | 4 | 9 | 2 | 0 | 25 | 4 |
| decisions | 16 | 1 | 5 | 8 | 2 | 1 | 24 | 3 |
| items | 19 | 1 | 2 | 8 | 2 | 1 | 27 | 3 |
| lean | 19 | 1 | 2 | 9 | 2 | 0 | 28 | 3 |

- **Anchors.** Both anchors scored exactly as in round two (Claude 21 decisions and 11 actions covered; the current prompt, the same bytes as the round-one and round-two `qwen3.5-9b-chunked` summary, 19 and 8), so the three rounds can be read together.
- **Hand check (coordinator).** ES2003c A2 (product evaluation): the Project Manager says "you're gonna look at product evaluation" to another participant, so the reference owner (Marketing Expert) holds, and summaries naming the Project Manager make the speaker-as-owner error that `owners` targeted. TS3006c D2 ("double-curved") is again the transcript-ambiguous case of rounds one and two; it is counted as judged.

### Applying the selection rule

- Rule 1 (more covered than current, 27): only `lean` (28). `items` ties at 27 and the tie goes to the current prompt; `owners` (25) and `decisions` (24) cover fewer.
- Rule 2 (no more contradicted than current, 3): `lean` 3, passes.
- Rule 3 (dev ROUGE-L no more than 0.01 below current, 0.174): `lean` 0.186, passes.
- Rule 4 (valid count not lower, 20/20): `lean` 20/20, passes.
- **Winner: `lean`.** The margin is one item out of 33, which is within the noise of one judge round. The rule was fixed before measuring, so it stands. Beyond the rule: `lean` also writes more action items (165 against 108 over the 20 dev meetings) and, by its own rule, fewer notes, and its ROUGE-Lsum is higher (0.324 against 0.297).

### Test split, once, for the winner (2026-10-26, `target/sum-eval/t620b-lean-test`)

Reference source, 20 test meetings, chunked 600 s, GPU, run with `--prompt-variant lean` (format key `indented-numbered-r4-chunk600-prompt-lean`). Nothing was tuned on it. The current row is the T6.20 test run (`chunk-test`).

| Prompt | Valid | ROUGE-L | ROUGE-Lsum | Decisions s / rr / cov (of 94) | Actions s / rr / cov (of 30) | Decisions / actions written | Repairs | Loop-guard stops | At hang-up, s per meeting |
|---|---|---|---|---|---|---|---|---|---|
| current (T6.20 rules, `chunk-test`) | 20/20 | 0.166 | 0.300 | 6 / 10 / 21 | 10 / 6 / 10 | 95 / 82 | 0 | 0 | 22.3 |
| lean | 19/20 | 0.172 | 0.304 | 7 / 10 / 20 | 9 / 8 / 13 | 94 / 106 | 3 | 3 | 18.1 |

- **One invalid summary.** ES2014a failed in its reduce step after 3 requests: the merge repeated earlier action items, the loop guard stopped it three times and the two repairs did not remove the repeats ("repeats an earlier item of the same list"). On dev this would have failed rule 4; it happened only on test, where nothing is tuned. It is the first invalid Qwen 3.5 9B chunked summary in any split; the one-task-per-item rule (more, shorter action items for the merge to repeat) is the likely cause. It is reported here, not fixed.
- Elsewhere the test split moves as dev did, but less: ROUGE-L +0.006, actions covered 10 -> 13, decisions covered 21 -> 20, more action items written (106 against 82).

### Decision (coordinator, 2026-10-26, pending the owner)

- The rule picked `lean` on dev, by one item out of 33. Its single test run produced a failed summary (ES2014a), which outweighs a dev gain within noise, so **the default stays the T6.20 prompt** (`PromptVariant::Current`, format key `indented-numbered-r4*`) until the owner decides.
- The switch was prepared as commit a527e46 (branch deleted 2026-11-01 after the owner kept the current prompt; kept as tag `archive/t620b-lean`): `system.md` holds the `lean` rules and the T6.20 rules become the variant `t620`, the format key is bumped to `indented-numbered-r5`, snapshots are regenerated, and the two earshot-llm map-reduce test budgets grow by 404 tokens (the longer system message) with every assertion unchanged; `cargo xtask accept m4` passed on it.
- A follow-up would look at reduce-step repetition under `lean` (merges repeating action items) on dev data only; the test split is not run again for this format. Keeping the T6.20 prompt because of the invalid test summary is a judgement made after measuring: the selection rule fixed before measuring had no test-split veto.

## T6.20c two-call chunk summaries (evaluation only)

Question: do chunked summaries keep more action items when each chunk asks for them in a request of their own? The owner chose "report only": no selection rule and no default change. Everything up to "Results" was written before any two-call run.

### What a two-call run is

- **Per chunk, two requests** (`earshot_core::prompt::plan_chunks_two_call`): the `plan_chunks` map request of the chunk (same transcript lines, notes, numbering, citable segments and reply budget) with one more paragraph in its system message. Request A (`templates/twocall/map-actions.md`) extracts only the action items (owner, task, due when said, segment citations); request B (`templates/twocall/map-rest.md`) summarises everything else and leaves `action_items` empty. The chunk's partial summary is B's reply with A's action items.
- **Grammar** (`earshot_llm_local::grammar::summary_part_grammar`): both replies keep the summary v1 shape, so no new schema file; the lists a request leaves out are capped at zero items (A: notes, decisions, open questions and topics; B: action items). `title` and `tldr` stay required in A (a few words, one sentence). Validation and repairs are those of any step.
- **Exact repeats in code, paraphrases in the merge.** Before the merge, action items that repeat an earlier part's item (owner and task after normalising, the `drop_repeats` key) are dropped, first copy kept (`earshot_core::summary::drop_repeated_actions`). The merge (`reduce_prompt_two_call`) is the production reduce plus `templates/twocall/reduce-merge.md`: merge action items that are one commitment in different words, keep the union of their segment numbers (at most three), never drop a distinct commitment.
- **No KV-cache prefix reuse** between A and B (the owner: test without it first). Every request is processed from scratch.
- **Counts per meeting** (`report.actions`): `per_part` and `extracted` (action items A returned, after that reply's own repeats were dropped by validation), `exact_repeats` (removed in code), `to_merge` (what the merge received) and `final` (action items in the final summary). The merge reply holds at most 12 action items (`SummaryLimits::max_items`, the cap of every list).
- **Timing.** Both requests of a chunk are map steps (`map-actions`, `map-rest`); `last_map_s` is the last chunk's two requests, so `hangup_s` is the last chunk's two requests plus the merge.
- **Cap hits.** `report.actions.cap_hit` (added after round four) counts merge steps whose reply filled the action list to the cap (`SummaryLimits::max_items`, 12); a merge at the cap may have lost actions to the cap rather than by folding them.
- **Format key** `indented-numbered-r4-chunk600-twocall` (`-twocall` after the chunk suffix, before any `-prompt-<variant>`), built in `examples/sum_eval/main.rs` and `xtask::sum_eval::format_key`. Production (`earshot_runtime::summary`, `summarize`, `summarize_chunks`) is unchanged; its grammar is `SummaryPart::Whole`, byte for byte `summary_grammar`.
- **Command**: `cargo xtask sum-eval --model qwen3.5-9b-q4_k_m,qwen3.5-4b-q4_k_m --split dev --source reference --chunk-seconds 600 --two-call --meeting ES2003c,ES2011c,IS1008c,TS3004c,TS3006c --backend cuda --target-dir C:\eb-t620c --out target\sum-eval\t620c-twocall` (`--meeting` selects meetings within the splits; added for this trial).

### Runs

- Qwen 3.5 9B and Qwen 3.5 4B, CUDA, S16, reference source, the five judged dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), two-call chunked 600 s. Nothing touches the test split.
- One-call anchors from the same build and the same command without `--two-call` (`target\sum-eval\t620c-current`): the 4B's round-one chunked results (`chunk-dev`) are no longer on disk, and the 9B is re-run to check that this build reproduces round three's `t620b-current` summaries.
- GPU timings were taken while another lane (echo canceller: CPU-heavy tests, occasional CUDA `accept m2`) shared the desktop; they are indicative only.

### Judge packets, round four

- Five summaries per meeting, blinded: Claude Opus 5.5 (anchor), Qwen 3.5 9B and 4B current chunked, Qwen 3.5 9B and 4B two-call. The Claude replies (`claude-dev-medium`) are no longer on disk, so its packets are carried unchanged from round three (`target/sum-eval/judge-dev-t620b`, key `judge-dev-t620b-key.json` of the main checkout) with `sum_eval --prior-packets <model>=<dir> --prior-key <key>` (`judge::prior_packet`: fails closed unless the key names the model once for the meeting, the round's `reference.json` equals the annotations and the packet is the labelled one).

### Results: automatic scores (dev, 5 judged meetings, reference source, chunked 600 s, CUDA, 2026-10-31)

Out directories `target\sum-eval\t620c-current` and `t620c-twocall` of the lane worktree (`lane/t620c`). The 9B one-call anchor gave round three's `t620b-current` summaries byte for byte (5 of 5), so this build reproduces earlier rounds. Items are covered / reference under the coverage rule (decisions of 22, actions of 11). GPU seconds were measured while another lane shared the host.

| Model | Mode | Valid | ROUGE-L | ROUGE-Lsum | Decisions cov | Actions cov | Decisions / actions written | Actions extracted -> exact repeats -> final | Repairs | Loop-guard stops | GPU s per meeting (all) | At hang-up, s per meeting |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Qwen 3.5 4B | one call | 5/5 | 0.156 | 0.276 | 14 | 2 | 34 / 38 | - | 0 | 0 | 37.9 | 18.8 |
| Qwen 3.5 4B | two calls | 4/5 | 0.125 | 0.227 | 8 | 4 | 23 / 43 | 60 -> 0 -> 43 (4 valid meetings) | 3 | 4 | 50.2 | 26.6 |
| Qwen 3.5 9B | one call | 5/5 | 0.180 | 0.307 | 13 | 5 | 35 / 36 | - | 0 | 0 | 50.6 | 24.4 |
| Qwen 3.5 9B | two calls | 5/5 | 0.167 | 0.309 | 12 | 2 | 27 / 28 | 28 -> 0 -> 28 | 0 | 0 | 55.4 | 24.3 |

- **The 9B extracts fewer actions with two calls, not more.** Its extraction requests returned 28 action items over the five meetings against the 36 its one-call summaries hold, with 0 in 10 of the 19 chunks (mostly the early ones); the merge kept all 28.
- **The 4B extracts many more (60), and the merge drops a third.** Three of its four valid merges stop at 12 action items, the list cap of every reply (`SummaryLimits::max_items`): ES2003c 16 -> 12, ES2011c 13 -> 12, TS3004c 13 -> 12. Part of that loss is the cap, not the merge's judgement. TS3006c failed in the merge: repeated action items, three loop-guard stops and two repairs (the T6.20b `lean` test failure had the same shape).
- No exact repeats across chunks in any meeting: every repeat the models wrote was reworded.
- Two calls cost the 4B 32% more GPU time per meeting and the 9B 9%; the wait at hang-up is unchanged for the 9B and longer for the 4B (its failed merge).
- Meaning judge: blinded packets for round four are written (below); verdicts are the coordinator's.

### Judge packets, round four (2026-10-31)

- **Where.** `target\sum-eval\judge-dev-t620c\<meeting>\` of the lane worktree, blinded; key `target\sum-eval\judge-dev-t620c-key.json` (outside the packet directory). 25 packets, five per meeting: `claude-opus-5-5-medium-agent` (carried from round three, identical apart from the label), `qwen3.5-9b-chunked-current`, `qwen3.5-4b-chunked-current`, `qwen3.5-9b-chunked-twocall`, `qwen3.5-4b-chunked-twocall`. The 4B two-call TS3006c packet is `"valid": false` (judged all missing).
- **Command** (the release example, llama.cpp's `bin` directory of the build on PATH):

  ```
  sum_eval.exe --annotations <fixtures>\ami\annotations --meetings dev=ES2003c,ES2011c,IS1008c,TS3004c,TS3006c --judge-packets target\sum-eval\judge-dev-t620c --blind-key target\sum-eval\judge-dev-t620c-key.json --summaries qwen3.5-9b-chunked-current=<t620c-current>\qwen3.5-9b-q4_k_m-cuda.json --summaries qwen3.5-4b-chunked-current=<t620c-current>\qwen3.5-4b-q4_k_m-cuda.json --summaries qwen3.5-9b-chunked-twocall=<t620c-twocall>\qwen3.5-9b-q4_k_m-cuda.json --summaries qwen3.5-4b-chunked-twocall=<t620c-twocall>\qwen3.5-4b-q4_k_m-cuda.json --prior-packets claude-opus-5-5-medium-agent=<main>\target\sum-eval\judge-dev-t620b --prior-key <main>\target\sum-eval\judge-dev-t620b-key.json
  ```

- **Import, once the verdicts are in:** the same `--annotations` and `--meetings`, plus `--import-verdicts target\sum-eval\judge-dev-t620c --blind-key target\sum-eval\judge-dev-t620c-key.json --out target\sum-eval\judge-dev-t620c-verdicts.json`.

### Results: meaning judge, round four (2026-10-31, `target/sum-eval/judge-dev-t620c-verdicts.md`)

Five dev meetings (22 reference decisions, 11 reference action items), five blinded summaries per meeting, one Claude Code agent (Opus 5.5) per meeting. Brief: the verdict-file format and rules of "Meaning judge: packets and verdict format" above, plus: judge each summary independently and consistently, an invalid summary is all missing, read nothing outside the meeting's packet directory.

| model | decisions covered / contradicted / missing | actions covered / contradicted / missing |
|---|---|---|
| claude-opus-5-5-medium-agent | 21 / 0 / 1 | 11 / 0 / 0 |
| qwen3.5-9b-chunked-current | 18 / 2 / 2 | 7 / 2 / 2 |
| qwen3.5-9b-chunked-twocall | 19 / 3 / 0 | 4 / 4 / 3 |
| qwen3.5-4b-chunked-current | 18 / 2 / 2 | 5 / 2 / 4 |
| qwen3.5-4b-chunked-twocall | 16 / 1 / 5 | 5 / 3 / 3 |

- Two calls did not help: the 9B covered fewer actions (7 -> 4) with more contradicted owners (2 -> 4); judges named actions given to the wrong role. The extraction call no longer sees the rest of the discussion and guesses owners.
- Judge noise: the 9B current summaries are byte-identical to round three's (judged 27 covered there, 25 here), so differences of about 2 items are within noise.
- Claude scored as in round one (21 / 11): the gap to the local models is in the model, not in how the request is split.
- Decision: two-call is not used; the code stays as an evaluation option. Next: one larger model as a test (T6.20d); the owner expects real meetings to be easier than AMI's role-play annotations.

## T6.20d larger-model screen: Qwen3 30B-A3B Instruct 2507 (evaluation only)

Question: does one clearly larger model (a mixture of experts, 30B total and about 3B active parameters per token) summarise noticeably better than the shipped Qwen 3.5 9B? The owner approved it as a test; it is unlikely to ship. Report only: no selection rule and no default change. Everything up to "Results" was written before the run.

### Method

- **Model.** `qwen3-30b-a3b-instruct-2507-q4_k_m` in `models/manifest.toml`, set `llm-eval` only: Q4_K_M, 18,556,686,752 bytes, Unsloth's GGUF pinned to commit `eea7b2be`, sha256 the file's LFS oid. The Qwen team publishes no GGUF of the 2507 release. Licence Apache-2.0 from the base model's card (`Qwen/Qwen3-30B-A3B-Instruct-2507`, card metadata and its LICENSE file). The 2507 Instruct release is non-thinking: its ChatML template renders nothing after the assistant header, so the assistant prefix is empty (checked in the stored result header's `assistant_prefix`).
- **Backend.** The file does not fit the desktop's 16 GB GPU and a partial offload is refused by design, so the run is on the CPU: `--backend cpu`, which builds the example without CUDA (`check_cpu_only`), default thread count. The backend reported is the one in the result's `LoadReport`. The 9B anchor ran on CUDA, so seconds per meeting are not comparable between the two; they say only what a CPU run of this model costs on the desktop (other lanes may share the host).
- **Runs.** Dev split only, reference source, the five judged meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), chunked 600 s, the production prompt (one request per chunk, not two-call), S16.
- **Command**: `cargo xtask sum-eval --model qwen3-30b-a3b-instruct-2507-q4_k_m --split dev --source reference --chunk-seconds 600 --meeting ES2003c,ES2011c,IS1008c,TS3004c,TS3006c --backend cpu --target-dir C:\eb-t620d --out target\sum-eval\t620d-30b`.
- **Anchor.** Qwen 3.5 9B one-call chunked from T6.20c (`t620c-current`, the same five meetings and format key `indented-numbered-r4-chunk600`).
- **Judge packets, round five.** Three summaries per meeting, blinded: the 30B, plus `claude-opus-5-5-medium-agent` and `qwen3.5-9b-chunked-current` carried unchanged from round four (`judge-dev-t620c` of the `lane/t620c` worktree) with `--prior-packets` / `--prior-key`.

### Results: automatic scores (dev, 5 judged meetings, reference source, chunked 600 s, 2026-11-01)

Out directory `target\sum-eval\t620d-30b` of the lane worktree (`lane/t620d`). The load report gives backend `cpu` (model buffers `CPU_Mapped` and `CPU_REPACK` only), from a build without CUDA (`system_info` lists no CUDA, so no op offload to the GPU), 8 threads; the stored `assistant_prefix` is empty and the format key is `indented-numbered-r4-chunk600`. Items are covered / reference under the coverage rule (decisions of 22, actions of 11).

| Model | Backend | Valid | ROUGE-L | ROUGE-Lsum | Decisions cov | Actions cov | Decisions / actions written | Repairs | Loop-guard stops | s per meeting (backends differ, not a speed comparison) (all) | At hang-up, s per meeting (backends differ, not a speed comparison) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Qwen 3.5 9B (anchor, T6.20c `t620c-current`) | CUDA | 5/5 | 0.180 | 0.307 | 13 | 5 | 35 / 36 | 0 | 0 | 50.6 | 24.4 |
| Qwen3 30B-A3B Instruct 2507 | CPU | 5/5 | 0.175 | 0.285 | 13 | 3 | 27 / 38 | 0 | 0 | 539.6 | 189.3 |

- **No automatic gain over the 9B.** Same decisions covered (13 of 22), two fewer actions (3 against 5 of 11), ROUGE-L and ROUGE-Lsum slightly lower; it writes fewer decisions and about as many action items.
- **CPU cost.** 13.2 reply tokens/s and 88 prompt tokens/s; 539.6 s per meeting in all and 189.3 s at hang-up (the last chunk plus the merge), against 24.4 s for the 9B on the GPU. The backends differ, so this is not a model-for-model speed comparison; other lanes may have shared the host.
- Meaning judge: blinded packets for round five are written (below); verdicts are the coordinator's.

### Judge packets, round five (2026-11-01)

- **Where.** `target\sum-eval\judge-dev-t620d\<meeting>\` of the lane worktree, blinded; key `target\sum-eval\judge-dev-t620d-key.json` (outside the packet directory). 15 packets, three per meeting, all valid summaries: `qwen3-30b-a3b-instruct-2507-chunked` (new), and `claude-opus-5-5-medium-agent` and `qwen3.5-9b-chunked-current` carried from round four, identical apart from the label.
- **Command** (the release example of the CPU build, llama.cpp's `bin` directory of that build on PATH):

  ```
  sum_eval.exe --annotations <fixtures>\ami\annotations --meetings dev=ES2003c,ES2011c,IS1008c,TS3004c,TS3006c --judge-packets target\sum-eval\judge-dev-t620d --blind-key target\sum-eval\judge-dev-t620d-key.json --summaries qwen3-30b-a3b-instruct-2507-chunked=target\sum-eval\t620d-30b\qwen3-30b-a3b-instruct-2507-q4_k_m-cpu.json --prior-packets claude-opus-5-5-medium-agent=<lane-t620c>\target\sum-eval\judge-dev-t620c --prior-packets qwen3.5-9b-chunked-current=<lane-t620c>\target\sum-eval\judge-dev-t620c --prior-key <lane-t620c>\target\sum-eval\judge-dev-t620c-key.json
  ```

- **Import, once the verdicts are in:** the same `--annotations` and `--meetings`, plus `--import-verdicts target\sum-eval\judge-dev-t620d --blind-key target\sum-eval\judge-dev-t620d-key.json --out target\sum-eval\judge-dev-t620d-verdicts.json`.

### Results: meaning judge, round five (2026-11-01, `target/sum-eval/judge-dev-t620d-verdicts.md`)

Same five dev meetings and brief as round four, three blinded summaries per meeting (Claude and the 9B carried in with `--prior-packets`), one Claude Code agent per meeting.

| model | decisions covered / contradicted / missing | actions covered / contradicted / missing |
|---|---|---|
| claude-opus-5-5-medium-agent | 21 / 0 / 1 | 11 / 0 / 0 |
| qwen3.5-9b-chunked-current | 19 / 1 / 2 | 8 / 2 / 1 |
| qwen3-30b-a3b-instruct-2507-chunked | 17 / 1 / 4 | 7 / 1 / 3 |

- The 30B is within judge noise of the 9B (about 2 items) and not better, at about 10x the time per meeting on the CPU. Claude's ceiling is unchanged across rounds.
- Decision: the 9B stays the default; the 30B file was deleted after the screen (the manifest entry stays in `llm-eval` for reproducibility). The next signal is the owner's own meetings.
## Surface CPU timings (T8.3f, 2026-11-03)

Setup: the Surface (Snapdragon X2 Elite, 12 cores, ARM64, llama.cpp CPU build), 6 threads (`earshot_llm_local::default_threads()` on 12 cores, what production uses), the production prompt and chunking: `cargo xtask sum-eval --backend cpu --split dev --source reference --model qwen3.5-4b-q4_k_m,qwen3.5-9b-q4_k_m --limit 3 --chunk-seconds 600 --threads 6`. ES2003a to c, 92 minutes of audio (5,496 s). Quiet machine (nothing else ran). Raw table: `docs/summary-eval/surface-cpu-2026-11-03.md`.

| Model | Valid | Per meeting (a, b, c) | All summary work | Per hour of meeting | Wait at hang-up if maps ran during the meeting | Reply tok/s | Prompt tok/s | ROUGE-Lsum |
|---|---|---|---|---|---|---|---|---|
| Qwen 3.5 4B | 3/3 | 229 s, 739 s, 691 s | 1,660 s | 1,087 s (18.1 min) | 871 s (9.5 min per hour) | 13.8 | 93 | 0.231 |
| Qwen 3.5 9B | 3/3 | 316 s, 924 s, 898 s | 2,138 s | 1,401 s (23.3 min) | 959 s (10.5 min per hour) | 9.5 | 53 | 0.249 |

- The Surface's 6 threads run at about the desktop's 8 (desktop chunked, all work per hour: 4B 1,084 s, 9B 1,327 s).
- Production summarises after the meeting (`SummaryService::summarize` runs every map then the reduce), so the wait is the "all summary work" column: about 18 minutes per hour of meeting with the 4B and 23 with the 9B.
- The 4B generates 45% faster per token but writes longer replies, so it saves only 22% of the work and 9% of an at-hang-up wait. Quality on these three meetings is informational only; T6.20's test split has the 9B ahead (ROUGE-Lsum 0.319 against 0.282, 20/20 valid against 19/20).

## Qwen 3.5 2B against the 4B, actions by hand (T8.3l, 2026-11-03)

On the Surface CPU, three action-rich dev meetings, each model alone: the 2B is about 2.4 times faster (3.7-4.4 min from hang-up today, against 8.3-11.0 for the 4B) but failed one meeting of three (a repeated item through two repairs). By hand, the 4B caught 5 of 11 reference actions (plus 1 partly) and the 2B 3 of 11 (plus 1 partly); on the two meetings both completed they tie at 3 of 7. Every item and call: `docs/summary-eval/actions-review-2026-11-03.md`.

## T6.20e prompt rounds judged by hand (2026-11-04)

Five rounds on Qwen 3.5 4B over the five judged dev meetings, every action and decision judged against a fixed list of real items written from the transcripts. Round 2 (`PromptVariant::Settled`: action items are tasks for after the meeting with the person addressed as owner; decisions are every settled choice, the final one) won: 37 of 60 real items caught against 35, made up 10 against 24; on the 9B 40 against 37, made up 15 against 19. On the test series (automatic scores) the 9B failed one meeting from a repeated item and the action scores did not rise. The owner adopted `settled` as the production prompt the same day (T6.20f; the earlier rules are `PromptVariant::Previous`, format key `indented-numbered-r5`). Every item, verdict and the test table: `docs/summary-eval/prompt-rounds-2026-11-04.md`.

## T8.4d: a cut-off reply no longer loses the summary

Written before measuring, 2026-11-06.

**Cause.** With the settled prompt (T6.20f, format key `r5`) Qwen 3.5 9B on CUDA fails dev meeting ES2003b on every run: the map step of its short last chunk reaches its reply budget, three times, in the middle of a long action list. Runs of the earlier prompt (`r4`, `t620b-current`) were valid on all 20 dev meetings with no truncated reply. The budget is half the transcript, at least 1024 tokens, and greedy decoding repeats the same reply, so a repair at the same budget cannot help. The budget was meant never to decide how long a summary is (`prompt/builder.rs`).

**Change.** A reply cut off at its budget is asked for again, from the original messages, with twice the budget, up to what the context and `max_output_tokens` allow, within the same `MAX_REPAIRS` rounds. Only when the budget cannot grow does the existing repair ask for something shorter. A cut-off reply is still never parsed as complete, a looping reply still counts as repetition, and every reply that finished within its budget is unchanged.

**Rule.** On Qwen 3.5 9B on CUDA, chunked 600 s, reference source, the whole dev split:

1. ES2003b is valid, and every dev meeting is valid.
2. No cut-off reply is accepted: a meeting's `truncated` count may only come with a later complete reply.
3. On T8.4c's four other meetings the summaries are byte-identical to `t84c-cuda`, because they had no cut-off reply.

**First result, 2026-11-06.** ES2003b valid with one cut-off reply and a later complete one, and the four other meetings byte-identical, but TS3006d (19 of 20) failed: its merge step repeats one task per owner until the loop guard stops it, on all three requests. Leaving the looping text out of the repair changed nothing, so that was not kept.

**Second change.** Keep the settled prompt. A merge step with no valid reply joins the summaries it was merging (`summary::join_partials`): every part kept in order, exact repeats dropped, nothing invented, and the `tldr` opens with `JOINED_SUMMARY_NOTICE`. A map step with no valid reply still fails: a gap in the transcript is not something to join over. Rule 1 is judged with this in place, and the number of joined meetings is reported. A joined summary counts as valid; it is still a rougher one.

The prompt and the validation do not change, so the format key stays `r5`. No test split run.

**Result, 2026-11-06.** Passes. `target/sum-eval/t84d-cuda-2`: 20 of 20 dev meetings valid, ROUGE-Lsum 0.307, 874.9 s. One meeting joined: TS3006d, from 5 parts (24 decisions, 11 actions, ROUGE-Lsum 0.233). One cut-off reply, on ES2003b, followed by a complete one. Three looping replies, all TS3006d's merge. Every other summary is byte-identical to the run before the fallback, and T8.4c's four other meetings are byte-identical to `t84c-cuda`. The merge loop itself is not fixed: it is contained, on the settled prompt, and a joined summary is rougher than a merged one.

## T6.20a: summary quality judged by atomic facts

Written before measuring, 2026-11-06. Informational: summary quality has no threshold.

**Why.** AMI bundles several decisions into one annotated sentence, so one verdict per sentence undercounts a summary that lists them separately, and hides a contradiction of one part.

**Facts.** `sum_eval --split-facts fixtures/ami/atomic_facts.json` (`facts.rs`): Qwen 3.5 9B on CUDA splits each annotated decision and action sentence once, under a grammar. A fact holding under half of its content tokens from the sentence, facts that together hold under 70% of the sentence, or a reply that stops early keep the whole sentence as one fact and record why (`kept_whole`). The two shares are judgements. Dev is split first and the prompt can change only on dev; test is then split once with the settled prompt. The file is committed and spot-checked by hand; `--judge-packets` and `--import-verdicts` refuse it when its sentences differ from the annotations.

**Local judge.** `sum_eval --judge-facts <packets> --out <dir>`: Qwen 3.5 9B reads one fact and one blinded summary packet and answers covered, contradicted or missing with a one-line reason, under a grammar. An invalid summary is all missing. The verdict rules and file format are the ones in "Meaning judge" above, per fact (`D1.1`, `D1.2`, ...).

**Validation.** On the five judged dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), for the 9B and 4B summaries:

1. Two Claude Code agents label every fact on the blinded packets, separately. The brief is below with the results.
2. Their disagreements are settled by one more reading of the summary, and that is the gold set. **This is agent labelling standing in for hand labelling; the owner has not checked it.**
3. `sum_eval --agreement` compares the local judge with the gold set: percent agreement, Cohen's kappa, and the confusion table.

**Rule for trusting the judge, fixed now.** Agreement of at least 80% and kappa of at least 0.6 (the lower edge of "substantial" in Landis and Koch 1977). Contradictions are rare, so their precision and recall are always reported beside it. If the rule fails, the judge prompt may change once, on this dev sample, and the change is labelled; if it fails again, the tables are labelled "not validated".

**Tables.** Coverage rate and contradiction rate per model, on dev and test, counted in facts. A contradiction counts against a model more than a miss: net = coverage rate - 2 x contradiction rate (the weight is a judgement). Summaries come from stored runs, chunked 600 s, reference source, on the settled rules: no test summary is generated here and nothing is tuned on test.

### Results: facts (2026-11-06, `docs/summary-eval/t620a/`)

**Splitting.** 248 annotated sentences (124 dev, 124 test) gave 377 facts (176 dev, 201 test); 3 sentences were kept whole, each by the 70% rule, and each is a single statement, so nothing was lost. By hand: 40 random sentences read in full (seeds 620 and 621 of a scratch sampler), all faithful with no word added, plus ES2004c D1, the sentence that started this task, which splits into its five items with "possibly ... if it is still within the budget" kept on the fifth. The dev split needed no prompt change. **Spot-checked by the agent running this task, not by the owner.**

**Gold set.** 104 labels (52 facts, 2 summaries each). The two labellers agreed on 100 of 104 (96.2%, kappa 0.90). The 4 disagreements were settled by reading the summaries, once, before the local judge's verdicts were read, and no label changed after that: IS1008c S1 D2.3 and D2.7 missing (the notes only list them as proposed), TS3004c S2 D8.2 contradicted (the summary states both chips), TS3004c S2 A1.1 missing.

Brief to each labeller: the verdict rules of "Meaning judge" above, per fact, working only inside a folder of the blinded packets, with no transcript and no outside knowledge. Their prompts asked for a report of counts only.

**The local judge fails the rule. Its tables are not validated.**

| Local judge | Items | Agree | Kappa | Contradicted: judge called / gold / precision / recall |
|---|---|---|---|---|
| First prompt, gold set | 104 | 66.3% | 0.34 | 17 / 6 / 18% / 50% |
| Revised prompt (the one allowed change, seen this sample), gold set | 104 | 75.0% | 0.44 | 10 / 6 / 30% / 50% |
| Revised prompt, 5 other dev meetings, one agent's labels (added after the rule, not adjudicated) | 112 | 84.8% | 0.70 | 7 / 8 / 71% / 63% |

The rule was 80% and kappa 0.6 on the gold set, so it fails, and it stays failed. The third row passes those numbers, but it was added after the failure, has one labeller and is on other meetings. Its gap from row two is itself a warning that the agreement depends on the meetings. The revised prompt saw the gold set, so row two is if anything flattering.

Where it goes wrong: treating a difference as a contradiction (a missing detail or a neighbouring choice), and treating a topic as coverage. So a local contradiction rate here is **not** a measure of a model's real contradictions: at 30% precision on the gold set, most of them are false. Do not use these rates to choose between models.

**Tables, local judge, not validated.** Chunked 600 s, reference source, the settled rules, 20 meetings each. Dev: `dev-local.md`, 9B `t84d-cuda-2`, 4B generated here (20/20 valid). Test: `test-local.md`, stored `t620e-settled-test` (format key `r4-...-prompt-settled`, the same rules; the 9B has 19/20 valid, its invalid meeting all missing). Test was judged once, with the prompt already fixed.

| Split | Model | Facts | Coverage | Contradicted | Net (coverage - 2 x contradicted) | Decisions covered / contradicted / missing | Actions covered / contradicted / missing |
|---|---|---|---|---|---|---|---|
| Dev | Qwen 3.5 9B | 176 | 63.1% | 8.5% | 46.0% | 82 / 11 / 30 | 29 / 4 / 20 |
| Dev | Qwen 3.5 4B | 176 | 64.8% | 9.1% | 46.6% | 85 / 14 / 24 | 29 / 2 / 22 |
| Test | Qwen 3.5 9B | 201 | 60.7% | 12.4% | 35.8% | 101 / 20 / 38 | 21 / 5 / 16 |
| Test | Qwen 3.5 4B | 201 | 63.7% | 13.4% | 36.8% | 106 / 22 / 31 | 22 / 5 / 15 |

**Agent labels, dev only (the numbers to read).** Same summaries, facts judged by Claude Code agents:

| Sample | Model | Facts | Coverage | Contradicted | Net | Decisions covered / contradicted / missing | Actions covered / contradicted / missing |
|---|---|---|---|---|---|---|---|
| Gold set (`c` meetings) | Qwen 3.5 9B | 52 | 80.8% | 3.8% | 73.1% | 26 / 1 / 8 | 16 / 1 / 0 |
| Gold set (`c` meetings) | Qwen 3.5 4B | 52 | 67.3% | 7.7% | 51.9% | 25 / 2 / 8 | 10 / 2 / 5 |
| One agent (`b` meetings) | Qwen 3.5 9B | 56 | 66.1% | 7.1% | 51.8% | 34 / 2 / 8 | 3 / 2 / 7 |
| One agent (`b` meetings) | Qwen 3.5 4B | 56 | 66.1% | 7.1% | 51.8% | 33 / 3 / 8 | 4 / 1 / 7 |

These are small samples, with few contradictions (a difference of one or two items is noise). The 9B's lead on the gold set is in its actions on those meetings and does not appear on the other five. No choice follows from this: the 9B stays the default for the reasons already recorded.

**Reading.** The facts work: they turn one bundled sentence into something a judge can decide part by part. A local Qwen 3.5 9B judge is not good enough to replace agent judges for this purpose. Judging by agents on blinded fact packets works today through `--judge-packets --facts`. A better local judge is future work only if automatic judging is ever needed; summary quality has no threshold, so nothing waits on it.

**Reproduce.** `sum_eval` (with `--gpu --assistant-prefix` set as in `xtask::sum_eval::CANDIDATES`): `--split-facts fixtures/ami/atomic_facts.json`; `--judge-packets <dir> --facts fixtures/ami/atomic_facts.json --blind-key <key> --summaries ...`; `--judge-facts <dir> --out <dir2>`; `--agreement <gold> --against <dir2>`; `--import-verdicts <dir2> --facts ... --blind-key <key>`. Keys are `facts-dev-key.json` and `facts-test-key.json` in `docs/summary-eval/t620a/`; `gold/` and `heldout/` hold the packets and agent verdicts.

## T12.10: Qwen thinking on against off (evaluation only)

Written before measuring, 2026-11-09. Every evaluation so far ran Qwen 3.5 with thinking off (the empty `<think>` prefix). This measures thinking on, through an evaluation-only `sum_eval` option with its own format key; production is unchanged.

**Setup.** Desktop GPU (CUDA), idle host, dev split only: the ten dev meetings of T6.20a's agent labels (the five `c` meetings of the gold set and the five `b` meetings), reference source, chunked 600 s, the settled rules, Qwen 3.5 9B and 4B. Thinking is a bounded `<think>` section before the summary JSON, allowed by the grammar, with a cap of 2048 thinking tokens (fixed 2026-11-09 before measuring, a judgement), counted apart from the reply budget. Off summaries are generated again in the same run, so both sides come from the same build.

**Judging.** Atomic facts (`fixtures/ami/atomic_facts.json`), by Claude Code agents on blinded packets (`--judge-packets --facts`, `--import-verdicts --facts`), one agent per meeting, every packet of a meeting (both models, off and on) in one blinded round, with the brief recorded here.

**Rule, fixed now (judgements).** For each model, count facts not covered (missing plus contradicted) over the ten meetings (108 facts). Thinking on earns a follow-up task only if, for at least one model, that count falls by at least 5 (about 5%; T6.20a found a difference of one or two items to be noise), with contradictions up by at most 1 and no more invalid summaries; and the other model's count does not rise by more than 2. Time per meeting is reported beside it and decides nothing here. If the rule fails, nothing follows; the tables go here either way.

**Brief to each judge** (one Claude Code agent per meeting, 2026-11-09, working only inside that meeting's blinded packet folder, four summaries S1 to S4 per meeting): the verdict-file format and rules of "Meaning judge: packets and verdict format" above, per fact; a topic mentioned is not coverage and a missing detail is not a contradiction; an invalid summary covers nothing; judge each summary independently and consistently; reply with counts only.

### Results (2026-11-09, `docs/summary-eval/t1210/verdicts.md`): thinking on fails the rule

| Model | Facts not covered, off | On (cap 2048) | Contradicted off / on | Invalid off / on | At hang-up per meeting off / on |
|---|---|---|---|---|---|
| Qwen 3.5 9B | 25 of 108 | 22 | 2 / 2 | 0 / 0 | 20.7 / 52.6 s |
| Qwen 3.5 4B | 26 of 108 | 33 | 6 / 2 | 0 / 1 | 17.2 / 37.4 s |

- **9B:** 3 fewer facts not covered (decisions 17 -> 14 missing or contradicted, actions unchanged), against at least 5. A small gain inside T6.20a's noise.
- **4B:** 7 more, so the "other model rises by at most 2" clause fails too: one meeting's thinking summary was invalid (TS3006c, 6 facts all missing) and its decisions not covered rose from 13 to 23, while its action items improved (contradicted 3 -> 0, covered 16 -> 19).
- **Time:** thinking roughly doubles to triples the time at hang-up (both models on the RTX 5080).

**Decision:** no follow-up task; production stays thinking off. Generated off and on in the same build (format keys `...-chunk600` and `...-chunk600-think2048`, prefixes checked in each result header).

## T12.27: Granite 4.2 screened as a local writer (evaluation only)

Written and committed before any generation, 2026-11-12. Evaluation only: no test split, no default change. The local writer is paused (the owner's decision); this asks whether a newer small model is worth un-pausing it for.

**Models.** Set `llm-eval`, both official IBM GGUFs, Q4_K_M, pins re-checked against the Hugging Face API on 2026-11-12 (commit, size, LFS SHA-256):

| Candidate | Class (compared with) | Pin | Size (bytes) | Licence, where read |
|---|---|---|---|---|
| Granite 4.2 3B (`granite-4.2-3b-q4_k_m`) | Qwen 3.5 4B | `ibm-granite/granite-4.2-3b-GGUF` at `c40945d7` | 2,244,011,552 | Apache-2.0: card metadata of the GGUF repo at the pin and of `ibm-granite/granite-4.2-3b` (commit `4e9c275e`); no LICENSE file in either repo |
| Granite 4.2 8B (`granite-4.2-8b-q4_k_m`, already pinned in T6.20) | Qwen 3.5 9B | `ibm-granite/granite-4.2-8b-GGUF` at `93f3f6a8` | 5,347,917,952 | Apache-2.0: card metadata of the GGUF repo at the pin and of `ibm-granite/granite-4.2-8b` (commit `a7042144`), whose card links the Apache 2.0 text; no LICENSE file in either repo |

Note against the plan's premise: Granite 4.2 8B is not new here. It was a T6.20 round-one candidate and failed 13 of 20 dev meetings, but in the earlier format (fixed 3072-token reply, segment UUIDs; "Aside: earlier format" above), where the fixed budget decided most failures. It has never run in the production format. The 3B has never run here.

**Chat template, checked with the pinned llama.cpp before any generation.** The GGUF template (`tokenizer.chat_template`, read from each file's metadata; the 3B's and the 8B's are byte-identical, 9,227 bytes) is ChatML with a thinking switch; thinking is on by default. With a system and a user message and thinking off it renders `<|im_start|>system\n{system}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n<think></think>`. llama-cpp-sys-2 0.1.160's `llm_chat_detect_template` classifies any template holding `<|im_start|>` (without `<|im_sep|>` or `<end_of_utterance>`) as ChatML, which renders the same text up to `<|im_start|>assistant\n`; the candidate's assistant prefix `<think></think>` (`GRANITE_NO_THINK`) completes it, byte for byte. One known deviation, in repair rounds only: the template writes an earlier assistant turn as `<think></think>{reply}`, llama.cpp's ChatML as `{reply}`. First requests, map and merge, are exact. The stored result header's `assistant_prefix` is checked before reporting.

**Condition.** Thinking off (the screened condition, as the Qwen models ship). Dev split only, all 20 dev meetings, reference source, chunked 600 s, the production prompt and rules (format key `indented-numbered-r5-chunk600`), S16 (`--context 16384 --max-output 8192`), the production grammar, repairs, loop guard and join fallback, desktop GPU (CUDA). Qwen 3.5 4B and 9B are generated again in the same build and the same `--out`, so every row comes from one build:

```
cargo xtask sum-eval --model qwen3.5-4b-q4_k_m,qwen3.5-9b-q4_k_m,granite-4.2-3b-q4_k_m,granite-4.2-8b-q4_k_m --split dev --source reference --chunk-seconds 600 --backend cuda --target-dir C:\eb\granite --out target\sum-eval\t1227-dev
```

A thinking-on run is a separate condition, never the deciding one, and is not part of this screen (`--thinking` supports only the Qwen prefix today).

**Rule, fixed now (judgements).**

1. **First test, per Granite model, against the Qwen model of its class from the same run:** every dev meeting valid (20 of 20; a meeting saved by the join fallback counts as valid, as in T8.4d, and joined meetings are reported), and automatic coverage not below the Qwen model's: decisions covered plus actions covered under the coverage rule (`decisions_cov` + `actions_cov`, of 83 + 41 reference items) at least the Qwen model's total. Both lists are reported beside the total.
2. **Judged round, only for a model that passes 1.** The five judged dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), blinded packets (`--judge-packets --blind-key`): the passing Granite model(s) and Qwen 3.5 4B and 9B from `t1227-dev`, plus `claude-opus-5-5-medium-agent` and `qwen3.5-9b-chunked-current` carried from round five with `--prior-packets` / `--prior-key` (the main checkout's `target\sum-eval\judge-dev-t620d` and its key, read in place). One Claude Code agent per meeting gives the "Meaning judge" verdicts per annotated item. The same agent also reads each summary decision and action item against that meeting's fixed real-item list (`docs/summary-eval/prompt-rounds-2026-11-04.md`, with its definitions and borderline rules) and marks it real or made up; made-up items are reported beside caught items, per model.
3. **What the round can recommend.** A Granite model is worth a follow-up (Surface timing beyond tonight's, then the owner's call on a test split) only if, in the judged round, its decisions plus actions covered are at least the Qwen model of its class minus 2 (T6.20a: one or two items is noise) and its made-up items are at most that model's. Otherwise the recommendation is to keep the local writer paused on Qwen 3.5.

**Reported for every candidate**, beside quality: tokens read (prompt tokens) and written (generated tokens) per meeting, from llama.cpp's counts, and wall time per meeting and at hang-up. Other lanes build on this desktop tonight, so every wall time here is taken on a shared host and is not a timing figure; the Surface CPU timing (3B against 4B) is a separate run on an idle machine.

### Ornith 1.5 9B, by the same rule (optional, after Granite's runs)

Written before any generation, with the Granite rule. Only once Granite's runs have finished, desktop GPU only, never on the Surface.

- **Model.** `bartowski/Ornith-1.5-9B-GGUF` (a community imatrix quantisation of `ornith-ai/Ornith-1.5-9B`, Qwen 3.5 architecture) at commit `1fc60d68`, `Ornith-1.5-9B-Q4_K_M.gguf`, 5,910,782,848 bytes, LFS SHA-256 `886e98b1...b266` (re-checked 2026-11-12). Licence MIT from the card metadata of both repos (`ornith-ai/Ornith-1.5-9B` at commit `489cb979`); both cards' `license_link` points at a LICENSE file in the base repo that does not exist (HTTP 404 on 2026-11-12), so the MIT text itself was not read from the publisher. Attribution goes to Ornith AI (model) and bartowski (GGUF conversion).
- **Template.** Read from the GGUF before its manifest entry is committed; the assistant prefix is whatever that template renders with thinking off. If the pinned llama.cpp cannot load it or cannot format its template, that is reported and the screen stops. Checked 2026-11-12, before its generation: the GGUF template (7,939 bytes) is Qwen 3.5 9B's with two differences, thinking on by default and a tool-argument loop; with thinking off it renders `<|im_start|>assistant
<think>

</think>

`, so llama.cpp detects ChatML and the prefix is `QWEN_NO_THINK`, as for the Qwen models. Whether the pinned llama.cpp loads it is known only from its run.
- **Rule.** Rule 1 above against Qwen 3.5 9B from `t1227-dev` (its own `--out`, `target\sum-eval\t1227-ornith`, same build and settings); rule 2 and 3 likewise, with Qwen 3.5 9B as the class model.

### Results: Granite 4.2 fails the first test (2026-11-12, `target/sum-eval/t1227-dev`)

One run, one build, CUDA on the RTX 5080 (every load log: all layers offloaded to `CUDA0`), format key `indented-numbered-r5-chunk600`, assistant prefixes checked in each result header (`<think></think>` for Granite, `<think>\n\n</think>\n\n` for Qwen). Coverage is decisions covered of 83 plus actions covered of 41. Raw tables: `docs/summary-eval/t1227/dev-table.md` and `dev-tokens.md` (per meeting).

| Model | Class model | Valid | Decisions cov | Actions cov | Coverage total | Decisions / actions written | ROUGE-Lsum | Rule 1 |
|---|---|---|---|---|---|---|---|---|
| Qwen 3.5 4B | - | 20/20 | 35 | 10 | 45 | 114 / 70 | 0.304 | - |
| Granite 4.2 3B | Qwen 3.5 4B | 19/20 | 3 | 0 | 3 | 5 / 5 | 0.240 | fails (invalid meeting, coverage 3 < 45) |
| Qwen 3.5 9B | - | 20/20 (1 joined) | 43 | 6 | 49 | 140 / 91 | 0.307 | - |
| Granite 4.2 8B | Qwen 3.5 9B | 18/20 | 32 | 9 | 41 | 102 / 84 | 0.202 | fails (2 invalid meetings, coverage 41 < 49) |

Tokens and time per meeting, means over the 20 dev meetings. **Shared host:** other lanes were building on the desktop during the run, so these seconds are not timing figures; they only say how the models compare in the same conditions.

| Model | Tokens read | Tokens written | s per meeting (all) | s at hang-up | Repairs | Truncated | Looping |
|---|---|---|---|---|---|---|---|
| Qwen 3.5 4B | 22,202 | 5,224 | 36.8 | 19.2 | 1 | 0 | 0 |
| Granite 4.2 3B | 19,024 | 2,118 | 11.3 | 5.2 | 2 | 0 | 3 |
| Qwen 3.5 9B | 22,880 | 4,995 | 46.1 | 23.8 | 3 | 1 | 3 |
| Granite 4.2 8B | 24,682 | 8,105 | 88.5 | 49.8 | 12 | 3 | 0 |

- **Granite 4.2 3B** is fast because it writes little: 5 decisions and 5 action items over 20 meetings, against 114 and 70 for the 4B; almost everything goes into notes. TS3006a failed at a map step (a repeated item through all three requests).
- **Granite 4.2 8B** writes 60% more than the 9B and needs 4 times the repairs. TS3004c and TS3006d failed in planning: its map summaries were long enough that the merge needed about 16,400 tokens of a 16,384 context (the production window), so no request was sent. With a larger context they might have run; the rule fixes S16, as production does.
- No judged round (rule 2 needs rule 1). The fall-back to Granite 4.1 was not needed: llama.cpp loaded and formatted 4.2.

### Results: Ornith 1.5 9B fails the first test (2026-11-12, `target/sum-eval/t1227-ornith`)

Same build and settings as `t1227-dev`, run after it; CUDA (33/33 layers offloaded), prefix `<think>\n\n</think>\n\n` checked in the result header. The pinned llama.cpp loads and formats it. Compared with Qwen 3.5 9B from `t1227-dev`. Raw tables: `docs/summary-eval/t1227/ornith-table.md` and `ornith-tokens.md`.

| Model | Valid | Decisions cov | Actions cov | Coverage total | Decisions / actions written | ROUGE-Lsum | Tokens read | Tokens written | s per meeting (all) | s at hang-up | Rule 1 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Qwen 3.5 9B | 20/20 (1 joined) | 43 | 6 | 49 | 140 / 91 | 0.307 | 22,880 | 4,995 | 46.1 | 23.8 | - |
| Ornith 1.5 9B | 20/20 | 28 | 11 | 39 | 136 / 104 | 0.260 | 22,258 | 4,449 | 40.6 | 21.9 | fails (coverage 39 < 49) |

Means per meeting; seconds from a shared host (other lanes building), not timing figures. Ornith is valid on every meeting with one repair and no loop, and catches more actions (11 against 6), but 15 fewer decisions; no judged round.

### Recommendation

Keep the local writer paused on Qwen 3.5 (4B and 9B as shipped). None of the three candidates earned a judged round under the rule written before generation: Granite 4.2 3B barely writes decisions or actions, Granite 4.2 8B is longer, slower and overruns the production context on two merges, and Ornith 1.5 9B trades decisions for actions at the same speed. T12.25 and T12.26 can be unparked on the Qwen 4B, as their tasks assume. No test split was run and no default changed. The 3B's Surface timing against the 4B (if run) can only show a faster model that does not summarise decisions or actions, so it decides nothing.

## T12.17: shorter by default, plus a few templates

Written and committed before any run, 2026-11-13. The owner (2026-11-10, 2026-11-13): summaries are too long; decisions and actions are the priority, not minutiae; "optimise what we get before how we get it". Judged on the writer in use, Claude through Claude Code; the local Qwen 3.5 4B must only not get worse. No owner meeting is read or used: AMI dev for every choice, AMI test once at the end, reference source only (public AMI text is the only text sent to Claude).

**What can change.** The schema keeps `decisions` and `action_items` required and every decision and action item stays required by rules 5 and 6; nothing here removes either. Only the notes, the tldr and the wording of items can get shorter. Two kinds of change:

1. **The default rules** (`templates/system.md`), measured first as evaluation-only prompt variants (`PromptVariant`, T6.20b), each a complete rules file in `src/prompt/templates/variants/` that changes only rules 4, 7 and 8 of the current rules:
   - **`short`.** Rule 4: the notes hold only what a reader needs to understand the decisions, action items and open questions, plus anything important said that is in none of them: at most four sections of at most four bullets, each bullet one short sentence, following the structure of the user's notes when they have one; who said what, figures and options discussed but not chosen, and anything already listed as a decision or action item are left out. Rule 7: `tldr` one or two sentences. Rule 8 adds: every decision and every action item is listed, each in one short sentence; shortness never drops one.
   - **`brief`.** As `short`, tighter: at most three sections of at most three bullets, `tldr` one sentence, each decision and action item at most about fifteen words.
2. **Templates on the Summaries page**, chosen like any other choice (applies at once) and composed with the summary style (T7.4b): fixed text placed in the style block before the user's own style, so it can never remove the fixed rules (`prompt_style_cannot_remove_the_fixed_rules_*`). Texts fixed now:
   - **Standard:** nothing (the default rules alone).
   - **Brief:** "Keep it short: a one-sentence summary and no more than three note bullets in all. Still list every decision and action item, each in a few words."
   - **Decisions and actions:** "Focus on what was decided and who will do what. Keep the notes to the background a reader needs for those, and leave out the rest of the discussion."
   - **Detailed:** "Give fuller notes: cover each topic that was discussed, with the main points and the reasons given for each decision."

**Counts (committed tool).** `sum_eval`'s score gains a length block per meeting, summed in the aggregate: **words** (whitespace-separated words of the tldr, every section heading, every note bullet, every decision, every action item's task and every open question; the title, owners, due dates and topics are left out), note **sections** and **bullets**, decisions and action items written, and lists at the 12-item cap (any list, and decisions or action items alone). A meeting without a valid summary counts zero words and stays in the sums.

**Runs, every parameter fixed now.** Dev split, all 20 dev meetings, reference source, one `--out` per run under `target/sum-eval/t1217-<run>`, one build for every run of a round.

- Claude: `cargo xtask sum-eval --model claude-code --split dev --source reference --out target/sum-eval/t1217-claude-<variant>` (the production plan, unchunked, the CLI's default model and effort, `CLAUDE_CODE_BUDGET`), with `--prompt-variant <variant>` for a variant (evaluation-only, allowed for `claude-code` by this task, format key `...-claude-code-prompt-<variant>`). Claude is not deterministic, so the current rules run **twice** (`current-1`, `current-2`) and the control is the mean of the two.
- Qwen 3.5 4B: `cargo xtask sum-eval --model qwen3.5-4b-q4_k_m --split dev --source reference --chunk-seconds 600 --backend cuda --target-dir C:\eb\lane-sum-cu --out target/sum-eval/t1217-4b-<variant>` (the production chunking, S16 defaults, greedy, one run each; GPU because only quality is read here).

**Rule (judgements, fixed now).**

1. **Claude, each variant against the Claude control:** valid meetings not below the control's lower run; at least **25% fewer words** in total over the 20 meetings; coverage-rule items caught (`decisions_cov` + `actions_cov`, of 83 + 41) not more than 3 below the control mean, and neither list more than 2 below its control mean.
2. **Qwen 3.5 4B, each variant passing 1, against the 4B current run:** valid meetings not lower, coverage total not more than 3 below, and words not more than the current run's (it must not get longer).
3. **Winner:** among variants passing 1 and 2, the one with the fewest Claude words; if none passes, `system.md` stays as it is and only the templates ship. The winner becomes `system.md` (format key bump, `cargo xtask accept m4` re-run).
4. **The 12-item cap** (`SummaryLimits::max_items`, local grammar only; Claude has no cap). Lowering it can only drop decisions or actions, so it is never lowered. It is raised to 16 only if, in the 4B run of the shipped rules, a final `decisions` or `action_items` list holds 12 items in at least 2 of the 20 dev meetings; the raise is then measured by rule 2 again. Every run reports its lists at the cap.
5. **Templates, informational:** with the shipped rules, each template runs once on Claude over the five judged dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c) beside a Standard run on the same five; words and coverage are reported. A template whose coverage total there is more than 3 below Standard's is reworded once (labelled) or dropped. Nothing else is decided by these runs.
6. **Test split, once, at the end:** Claude and the 4B, current rules and the shipped rules, 20 test meetings, reference source, same settings. Reported to the owner; it cannot change the outcome of the dev rule. An invalid summary there is reported as a finding.

Meaning judgements (blinded packets, `--judge-packets --blind-key`) are not part of the rule; packets of the five judged dev meetings for the winner and the control can be written for the owner or agents to judge later.

### Results (2026-11-13, `docs/summary-eval/t1217/`): no variant passes, the default stays; the templates ship

Dev split, 20 meetings, reference source. Claude through Claude Code 2.1.292 at its default model and effort (`claude-code`, unchunked production plan); Qwen 3.5 4B on CUDA (RTX 5080), chunked 600 s, S16. One build per round; the four Claude runs ran side by side and the three 4B runs side by side, so the seconds are not timings. Raw tables (the tool's unmodified `table.md`, counts and scores only) are in `docs/summary-eval/t1217/`. Coverage is the coverage rule's decisions caught of 83 and actions caught of 41.

**Claude, rule 1** (control: the mean of `current-1` and `current-2`).

| Run | Valid | Words | Notes words | Sections / bullets | Decisions / actions written | Decisions cov | Actions cov | Coverage total | ROUGE-L |
|---|---|---|---|---|---|---|---|---|---|
| current-1 | 20/20 | 14,235 | 11,134 | 96 / 401 | 142 / 70 | 40 | 11 | 51 | 0.153 |
| current-2 | 20/20 | 14,298 | 11,164 | 99 / 397 | 136 / 69 | 42 | 16 | 58 | 0.156 |
| control (mean) | 20/20 | 14,266.5 | 11,149 | | | 41 | 13.5 | 54.5 | |
| short | 20/20 | 7,420 (-48.0%) | 4,278 | 61 / 192 | 153 / 75 | 42 | 9 | 51 | 0.192 |
| brief | 20/20 | 5,520 (-61.3%) | 2,846 | 55 / 151 | 141 / 74 | 44 | 8 | 52 | 0.185 |

- **`short` fails:** words pass (-48%), decisions pass (42 against a floor of 39), but actions 9 against a floor of 11.5 and the total 51 against a floor of 51.5.
- **`brief` fails:** words pass (-61%), decisions pass (44), the total passes (52 against 51.5), actions fail (8 against 11.5).
- **So no variant passes and `system.md` stays as it is** (rule 3); rule 2 is not reached. No format key bump, no `accept m4` re-run.
- **Reading, after the rule (not a decision).** Both variants write more action items than the control (74 and 75 against 69 and 70) and more decisions, while the token-based coverage of actions falls: shorter action wording carries fewer of the annotation's words. The two control runs themselves differ by 5 caught actions (11 and 16), more than the per-list tolerance of 2 the rule allows, so the action guard sits inside Claude's own run-to-run spread. Whether the shorter actions say the same in meaning is a question for the blinded meaning judge (packets of the five judged meetings), which this rule did not include. The owner may want that round before deciding on `short`.

**Qwen 3.5 4B, informational** (rule 2 applies only to a variant that passed rule 1).

| Run | Valid | Words | Notes words | Decisions / actions written | Decisions cov | Actions cov | Coverage total | Repairs |
|---|---|---|---|---|---|---|---|---|
| current | 20/20 | 11,778 | 8,387 | 114 / 70 | 35 | 10 | 45 | 1 |
| short | 20/20 | 6,601 (-44%) | 3,628 | 113 / 61 | 33 | 7 | 40 | 0 |
| brief | 20/20 | 5,944 (-50%) | 3,027 | 107 / 65 | 34 | 6 | 40 | 0 |

Both variants would also fail rule 2 on the 4B (coverage 5 below), and the 4B writes fewer action items under them, unlike Claude.

**The 12-item cap, rule 4.** In the 4B run of the shipped rules (`current`), a final decisions or action items list held 12 items in 2 of the 20 meetings, which triggers the raise to 16. Measured by rule 2 (`4b-cap16`, the same build with `SummaryLimits::max_items` 16; its "lists at the cap" column counts lists of 16):

| Run | Valid | Words | Decisions / actions written | Coverage total | Lists at the cap (decisions or actions) |
|---|---|---|---|---|---|
| current (cap 12) | 20/20 | 11,778 | 114 / 70 | 45 | 2 meetings at 12 |
| cap 16 | 20/20 | 12,117 (+2.9%) | 118 / 70 | 45 | 1 meeting at 16 |

Valid and coverage hold, but the words rise, so by rule 2 the raise fails and **the cap stays at 12** (the change was reverted). For context: Claude has no cap and wrote a decisions or action items list of 12 or more in 3 to 5 of the 20 dev meetings.

**Templates, rule 5** (the shipped rules; the five judged dev meetings; coverage of 22 decisions and 11 actions):

| Template | Valid | Words | Notes words | Bullets | Decisions / actions written | Decisions cov | Actions cov | Total |
|---|---|---|---|---|---|---|---|---|
| Standard | 5/5 | 3,894 | 3,020 | 116 | 36 / 16 | 11 | 1 | 12 |
| Brief | 5/5 | 1,137 (-71%) | 592 | 15 | 48 / 15 | 12 | 1 | 13 |
| Decisions and actions | 5/5 | 2,346 (-40%) | 1,514 | 48 | 36 / 16 | 12 | 1 | 13 |
| Detailed notes | 5/5 | 6,198 (+59%) | 5,119 | 154 | 43 / 17 | 13 | 1 | 14 |

No template is more than 3 below Standard, so all four ship unchanged. The templates do what their names say: Brief cuts the words by about 70% and Decisions and actions by about 40% with decision and action coverage within the rule's tolerance (five meetings, one run each, inside Claude's own run-to-run spread), which is the shorter summary the owner asked for, as a choice rather than the default.

**Test split, rule 6: not run.** The settled format is the current one (no variant passed, the cap is unchanged), so there is no new format to test once; the templates are style text and were checked on dev only, as rule 5 says.

**What shipped (T12.17b).** The template choice on the Summaries page ("Summary style": Template, then the user's own words), stored as `settings/summary-template` and composed with the style in one block (`earshot_core::prompt::StylePreset`, `style_block`). `system.md`, the format key and the 12-item cap are unchanged.

### T12.17 judged round: is a shorter default as good in meaning? (written before any packet, 2026-11-13)

Asked by the owner after the results above: the token rule said no because action coverage fell, but the variants write as many actions as the control, so the question is whether the shorter wording says the same thing. Written and committed before any packet was written or any summary was read.

- **Meetings.** The five judged dev meetings (ES2003c, ES2011c, IS1008c, TS3004c, TS3006c), reference source; the summaries already stored by the runs above, nothing generated again.
- **Summaries, seven per meeting, blinded** (`--judge-packets --blind-key`, key outside the packet directory): three runs of the shipped rules, `claude-current-1` and `claude-current-2` (those meetings of the 20-meeting runs) and `claude-template-standard`; the variants `claude-short` and `claude-brief`; the templates `claude-template-brief` and `claude-template-decisions`. Detailed notes is left out (longer, not the question). Packets in `target\sum-eval\judge-dev-t1217`, key `target\sum-eval\judge-dev-t1217-key.json`.
- **Judges.** One Claude Code agent per meeting (Opus 5.5) reads its meeting's numbered reference transcript (the exported user prompt, `target\sum-eval\t1217-judge-transcripts`), its packets and that meeting's fixed real-item list in `docs/summary-eval/prompt-rounds-2026-11-04.md`, and writes two things per summary: (a) the meaning-judge verdict against the AMI annotations (`verdict.<label>.json`, the format above, imported with `--import-verdicts`); (b) every summary decision and action item marked caught (with the real item's id), real not listed, made up or duplicate, under the "Judging rules" of that document, in `target\sum-eval\judge-dev-t1217-real\<meeting>\real.<label>.json`. The judge sees labels only. The coordinator checks every made-up mark against the transcript.
- **Measures per summary set, summed over the five meetings.** Meaning covered (decisions covered of 22 plus actions covered of 11) and contradicted; real items caught (of 42 decisions and 18 actions) and made up.
- **Control.** The mean of the three shipped-rules runs; their spread (largest minus smallest) is reported beside it.
- **Rule (judgements).** A variant (`short`, `brief`) is recommended as the new default only if all four hold: (1) meaning covered at least the control mean minus 2, and actions covered at least the control mean minus 1; (2) real items caught at least the control mean minus 2; (3) made up at most the control's largest; (4) contradicted at most the control's largest. If both pass, the one with fewer words over the 20 dev meetings is recommended. The owner decides; adopting one means `system.md` changes with a format key bump, `accept m4` and the test split once (rule 6 above). The templates are reported beside the variants and decide nothing.
- **Limits, stated now.** Claude judges Claude, and blinding hides the labels but not length or style; each summary set is one run; five meetings.

### Results: judged round (2026-11-13, `target/sum-eval/judge-dev-t1217-verdicts.md`): both shorter variants pass

Five Claude Code agents (Opus 5.5), one per meeting, judged the 35 blinded packets; `--import-verdicts` matched the packet set and key. The coordinator checked the one made-up mark against the transcript (it stands: a closing proposal the fixed list already names as not a decision). Words are those five meetings only.

| Summary set | Words (5 meetings) | AMI decisions covered (of 22) | AMI actions covered (of 11) | Meaning covered | Contradicted | Real decisions caught (of 42) | Real actions caught (of 18) | Real caught | Made up |
|---|---|---|---|---|---|---|---|---|---|
| current-1 | 3,954 | 18 | 11 | 29 | 0 | 36 | 17 | 53 | 0 |
| current-2 | 3,957 | 17 | 11 | 28 | 0 | 35 | 16 | 51 | 0 |
| template-standard | 3,894 | 17 | 11 | 28 | 0 | 34 | 17 | 51 | 0 |
| control (mean; spread) | 3,935 | | 11 | 28.3 (1) | 0 | | | 51.7 (2) | 0 |
| short | 1,937 (-51%) | 18 | 11 | 29 | 0 | 36 | 17 | 53 | 0 |
| brief | 1,504 (-62%) | 18 | 11 | 29 | 0 | 38 | 18 | 56 | 0 |
| template-brief | 1,137 (-71%) | 18 | 11 | 29 | 0 | 37 | 16 | 53 | 1 |
| template-decisions | 2,346 (-40%) | 18 | 11 | 29 | 0 | 35 | 17 | 52 | 0 |

- **`short` passes** all four parts of the rule (meaning 29 against a floor of 26.3, actions 11 against 10, real caught 53 against 49.7, nothing made up or contradicted).
- **`brief` passes** all four (meaning 29, actions 11, real caught 56, nothing made up or contradicted), and it has fewer words over the 20 dev meetings (5,520 against 7,420), so **the rule recommends `brief`**.
- **Why the token rule said no.** Every set covers all 11 AMI actions in meaning; the shorter action wording carries fewer of the annotation's words, which is what `actions_cov` counted.
- **Templates (decide nothing).** Brief and Decisions and actions keep meaning coverage; the Brief template made one item up (a closing proposal written as a decision).
- **Limits.** Near the ceiling: TS3006c was perfect for all seven and most differences are one or two items, so the round shows the shorter rules lose nothing measurable here, not that they are better. Claude judged Claude.
- **Not settled by this round.** `system.md` is shared by every writer, and on Qwen 3.5 4B `brief` cut the token coverage by 5 and wrote fewer action items (rule 2 above); the 4B was not in this judged round. Nothing has changed yet: adopting `brief` is the owner's call, and the steps are those of the rule (format key bump, `accept m4`, the test split once).

### T12.17 judged round on Qwen 3.5 4B (written before any packet, 2026-11-13)

Asked by the owner after the Claude round: does `brief` hold up on the local writer, and is it faster there? Written and committed before any 4B packet was written or any 4B summary was read.

- **Summaries, three per meeting, blinded:** `qwen3.5-4b-current`, `-short` and `-brief`, the five judged dev meetings of the stored runs `t1217-4b-current`, `-short`, `-brief` (CUDA, chunked 600 s, S16, greedy; nothing generated again). Greedy decoding gives one control run with no spread. Packets `target\sum-eval\judge-dev-t1217q`, key `target\sum-eval\judge-dev-t1217q-key.json`, real-item marks `target\sum-eval\judge-dev-t1217q-real`.
- **Judges and measures:** as in the Claude round above (one agent per meeting, the meaning-judge verdict and the real-item marks, the coordinator checks every made-up mark).
- **Rule (judgements).** A variant passes on the 4B if: (1) meaning covered at least the control minus 2 and actions covered at least the control minus 1; (2) real items caught at least the control minus 2; (3) made up at most the control's; (4) contradicted at most the control's. "Better coverage" is claimed only for a variant at least 3 above the control on meaning covered or real caught.
- **Time (informational, decides nothing here).** Prompt and generated tokens per meeting from llama.cpp's counts, over all 20 dev meetings and the five, and generated tokens of the last step (what a person waits for at hang-up). These are GPU runs: tokens stand in for Surface CPU time; a Surface timing is a separate run on an idle machine.
- **What a pass means.** With the Claude round, `brief` for every writer becomes a candidate for `system.md`; adopting it is still the owner's call (format key bump, `accept m4`, the test split once).

### Results: 4B judged round (2026-11-13, `target/sum-eval/judge-dev-t1217q-verdicts.md`): both variants fail on the 4B

The same five judges (one per meeting, kept from the Claude round so a summary item with the same meaning got the same verdict) judged the 15 blinded packets; `--import-verdicts` matched the packet set and key. Made up counts include items caught but with a false part. The coordinator checked the two recurring made-up claims against the transcript (TS3006c's case agreed double curved at 284-288; TS3004c's advanced chip at 672-675); the outcome below does not depend on the made-up counts, because both variants already fail parts 1 and 2.

| 4B run | Generated tokens (20 meetings) | AMI decisions covered / contradicted (of 22) | AMI actions covered / contradicted (of 11) | Meaning covered | Contradicted | Real decisions caught (of 42) | Real actions caught (of 18) | Real caught | Made up |
|---|---|---|---|---|---|---|---|---|---|
| current (control) | 104,477 | 17 / 1 | 6 / 2 | 23 | 3 | 25 | 11 | 36 | 12 |
| short | 69,670 (-33%) | 11 / 2 | 7 / 0 | 18 | 2 | 23 | 9 | 32 | 17 |
| brief | 65,637 (-37%) | 13 / 3 | 7 / 1 | 20 | 4 | 22 | 10 | 32 | 14 |

- **`brief` fails** parts 1 (meaning 20 against a floor of 21), 2 (real caught 32 against 34), 3 (made up 14 against 12) and 4 (contradicted 4 against 3). **`short` fails** parts 1, 2 and 3. Neither has better coverage.
- **So on the 4B the shorter rules lose decisions** (the judged decisions fall from 17 to 11 or 13 of 22), while actions hold. Combined with the Claude round: `brief` suits Claude and does not suit the local 4B.
- **Time.** The shorter rules generate about a third fewer tokens, so a Surface summary would be roughly a third quicker (an estimate from token counts, not a Surface timing); that comes with the coverage loss above.
- **For every writer, the 4B is much weaker than Claude** on these meetings: 36 real items caught and 12 made up against Claude's 51 to 53 and 0, with open or reversed points written as decisions (the case shape in TS3006c, the chip in TS3004c, the power source in ES2011c).

## T12.25a: judging on the owner's own meetings, privately (evaluation only)

The owner asked for summary changes to be judged on their own recorded meetings (AMI's scripted design meetings are hard to judge and unlike them). Their text may go to Claude Code agents as judges, as their summaries already do; nothing from them enters the repo.

**Prepare a run** (once per set of meetings; close Earshot first so the copy is consistent):

```
cargo xtask sum-eval --store-db <path to earshot.db> --store-run <name> [--store-count 6] [--store-min-minutes 15] [--store-exclude <id,...>]
```

- xtask refuses unless `fixtures/private/real-meetings/<name>/` is git-ignored (`git check-ignore`), then runs earshot-runtime's `store_inputs` example: the database (and its `-wal`) is copied there, and only the copy is opened, read-only (`earshot_store::ReadOnlyStore`: SQLite's read-only open and `query_only`, never `Store::open`, no migration; a store from another schema version is refused).
- **Pick rule (reads no text):** live meetings with a Final transcript and a duration of at least 15 minutes, minus `--store-exclude`, most recent start first, the first 6. They are labelled `r1` (the most recent) to `r6`.
- Written into the run folder only: `inputs/r<k>.json` (the `SummaryInput` production builds: `MeetingView::load` and `summary::evaluation_input`, the Final transcript, names as the app shows them, title and start, no style, template or Dictionary words), `transcripts/r<k>.txt` (the transcript as a model reads it, numbered, for the real-item list writers and the judges), `picked.json` (labels, meeting ids, starts, durations). It prints counts and the folder only.

**Summarise** with any candidate, today's plan:

```
cargo xtask sum-eval --source store --store-run <name> --model <candidate> [--chunk-seconds 600] [--prompt-variant <v>] [--out <folder inside the run>]
```

Results, checkpoints, `report.json` and `table.md` go to `<run>/results` (or an `--out` inside the run folder; anything else is refused by xtask and again by the example). A store run is scored by length counts only (`score::store_score`: words, sections, decisions and actions written); there is no annotation. Claude Code and Codex may summarise them (`--model claude-code`, `--model codex`); a remote endpoint never does, whatever `EARSHOT_LLM_URL` says.

**Real items and judging.** One Claude Code agent per meeting writes the real decisions and actions from `transcripts/r<k>.txt` under the definitions and borderline rules of `docs/summary-eval/prompt-rounds-2026-11-04.md`, into the run folder, before any summary of that meeting is read. Blinded packets come from `sum_eval --store-inputs <run> --meetings real=r1,... --judge-packets <run>/packets --blind-key <run>/key.json --summaries <name>=<run>/results/<file>.json ...` (all paths inside the private tree, checked): each meeting folder holds `transcript.txt` in place of `reference.json` and the blinded `summary.S<n>.json` packets. The AMI meaning judge (`--import-verdicts`, `--facts`) and carried AMI packets are refused for these meetings; the real-item marks are counted by the coordinator.

**What may leave the run folder:** counts only (meetings, picked, valid, decisions and actions written, tokens, seconds), under the labels `r<k>`. No meeting id, title, name or text in any committed file, log line or test.
