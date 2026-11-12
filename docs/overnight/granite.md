# Overnight report, lane/granite (T12.27, evaluation only)

## Result

T12.27 done as far as the brief goes: Granite 4.2 3B, Granite 4.2 8B and (optional) Ornith 1.5 9B screened on the dev split; all three fail the rule's first test, so no judged round ran. Recommendation: keep the local writer paused on Qwen 3.5. No test split, no default change. Granite 4.2 loaded and formatted in the pinned llama.cpp, so the 4.1 fallback was not used.

## Commits

- `e5b7016` T12.27: pin Granite 4.2 3B in llm-eval and as a sum_eval candidate (pushed at once for the Surface)
- `ad7f19a` plan: T12.27 rule for the Granite 4.2 and Ornith 1.5 9B screens, before any generation (docs only; the `plan:` prefix should have been `docs:`)
- `b2620a0` T12.27: pin Ornith 1.5 9B in llm-eval and as a sum_eval candidate
- `cac2e5e` T12.27: Granite 4.2 3B and 8B fail the screen's first test
- `fa9787d` T12.27: Ornith 1.5 9B fails the screen's first test; recommendation

## Accept commands

- `cargo test -p xtask models` -> `test result: ok. 14 passed; 0 failed` (lib) and `1 passed` (bin)
- `cargo test -p xtask sum_eval` -> `test result: ok. 34 passed; 0 failed` (lib) and `2 passed` (bin)
- `cargo clippy -p xtask --all-targets -- -D warnings` (CLIPPY_CONF_DIR at the worktree root) -> clean; `cargo fmt --all -- --check` -> clean
- `cargo xtask sum-eval --model qwen3.5-4b-q4_k_m,qwen3.5-9b-q4_k_m,granite-4.2-3b-q4_k_m,granite-4.2-8b-q4_k_m --split dev --source reference --chunk-seconds 600 --backend cuda --target-dir C:\eb\granite --out target\sum-eval\t1227-dev` -> exit 0
- `cargo xtask sum-eval --model ornith-1.5-9b-q4_k_m ... --out target\sum-eval\t1227-ornith` -> exit 0

## Pins (re-verified against the HF API, 2026-11-12)

All three match the brief: commit, size and LFS SHA-256. Licences: Granite Apache-2.0 from the card metadata of the GGUF repos and base repos (no LICENSE file in any of them); Ornith MIT from both cards' metadata, but the LICENSE file both cards link returns 404, so the MIT text was not read from the publisher (recorded in the manifest comment and docs/summary-eval.md).

## Numbers (dev, 20 meetings, coverage = decisions covered of 83 + actions covered of 41)

| Model | Valid | Coverage | Tokens read / written per meeting | s per meeting (shared host) |
|---|---|---|---|---|
| Qwen 3.5 4B | 20/20 | 45 | 22,202 / 5,224 | 36.8 |
| Granite 4.2 3B | 19/20 | 3 | 19,024 / 2,118 | 11.3 |
| Qwen 3.5 9B | 20/20 | 49 | 22,880 / 4,995 | 46.1 |
| Granite 4.2 8B | 18/20 | 41 | 24,682 / 8,105 | 88.5 |
| Ornith 1.5 9B | 20/20 | 39 | 22,258 / 4,449 | 40.6 |

All wall times were taken while other lanes built on the desktop: not timing figures. Full tables in docs/summary-eval.md and docs/summary-eval/t1227/.

## Findings for the coordinator

- PLAN T12.27's premise "has not been screened here" was half wrong: Granite 4.2 8B was already a T6.20 candidate (pinned identically) and failed 13 of 20 in the earlier format. Recorded in the rule; it had never run in the production format.
- Granite 4.2 8B's two failures are planning failures: its map summaries are long enough that a merge needs just over the 16,384-token production context.
- `sum_eval --thinking` supports only the Qwen prefix, so a labelled Granite thinking-on run (PLAN's optional separate condition) was not run.
- The Surface's 3B timing (Surface queue item 2) can only time a model that writes almost no decisions or actions; it decides nothing now.

## For PLAN.md (coordinator)

- T12.27 -> `[x]` with the result line: Granite 4.2 3B and 8B and Ornith 1.5 9B fail rule 1 (dev, `t1227-dev`, `t1227-ornith`); local writer stays paused on Qwen 3.5.
- T12.25 and T12.26: their park condition ("until T12.27 has judged Granite") is met; whether to start them is the owner's call.

## Proposed AGENTS.md bullet

- A GGUF's chat template can be read without loading the model (`tokenizer.chat_template` in the GGUF metadata header) and checked against llama-cpp-sys-2's `llm_chat_detect_template` in the cargo registry; a template holding `<|im_start|>` is ChatML there, so only the assistant prefix (what the template renders after `<|im_start|>assistant\n` with thinking off) needs pinning in `sum_eval::CANDIDATES`.
- While a `cargo xtask sum-eval` runs, a commit that touches xtask sources cannot be pushed: the pre-push hook's `cargo xtask public-guard` must rebuild the running `xtask.exe` ("Access is denied"). Push after the run.

## TODO.md

Nothing to add or remove.

## Left

Nothing in the lane's scope. Model files stay in the models cache (Granite 3B and 8B, Ornith 9B: about 13.5 GB); delete them if the owner does not want them kept (the manifest entries stay for reproducibility, as with the 30B).
