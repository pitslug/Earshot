# Overnight report: the Surface, 2026-11-12/13

The Surface (Snapdragon X2 Elite, 12 cores, ARM64, 16 GB), on mains throughout. Measurements ran one at a time; no build, test or other measurement of this session ran beside a timing. Other user apps stayed open (a music player, browsers, a filtering proxy), so every live run records the machine load it saw (`host.machine_load_percent`, medians 4-34%). No shipped code changed on any branch.

## Pushing: read this first

- **`lane/stretches` is on origin, pushed without the names check.** This checkout had no `core.hooksPath`, so the pre-push hook did not run on that push. Afterwards `cargo xtask public-guard` (without the list) passed ("0 list entries, names were not checked"), and `--require-list` refuses because `%USERPROFILE%\.earshot-private\names.txt` is not on the Surface. Its content is my own prose, AMI meeting ids, and counts and times only; still, it has not been checked against the names list. **Please run `cargo xtask public-guard --require-list` on that branch on the desktop.**
- I then set `git config core.hooksPath .githooks` here. Every later push was refused by the hook for the missing list, as the brief says. Those lanes are git bundles under `target\overnight\` on the Surface (each verified with `git bundle verify`), not on origin:
  - `lane-granite-surface.bundle` (base `origin/lane/granite`, head `lane/granite-surface`)
  - `lane-ui-profile.bundle` (base `origin/overnight/2026-11-12`, head `lane/ui-profile`)
  - `lane-nemotron-surface.bundle` (base `origin/lane/nemotron`, head `lane/nemotron-surface`)
  - `lane-npu-previews-surface.bundle` (base `origin/lane/npu-previews`, head `lane/npu-previews-surface`, this report included)
  - To take one on the desktop: `git fetch <path-to-bundle> lane/<name>:lane/<name>`, then check it with `public-guard --require-list` before pushing.
- `acceptance/results/m2.json` was already modified on this checkout's `main` before the session; it was left untouched and unstaged on every branch.

## 1. T12.24 Shorter speech stretches (`lane/stretches`, on origin)

Commits: `docs: T12.24 rule for shorter Parakeet stretches, before measuring` (the rule, committed before any run), `T12.24: evaluation switch for max_piece and the stretch_eval harness`, `T12.24: measure shorter Parakeet stretches on the Surface`.

- Code (examples only): `live_lag --max-piece <s>`, `m2_prod --max-piece <s>` (never passed by `accept m2`; tests pin that `None` is production's config and that only `max_piece` changes), and a new example `stretch_eval` (AMI dev mixes in pieces; WER, cuts, cut-window WER, doubled words, decode rtf; counts only). Unit tests: `cargo test --release -p earshot-runtime --features qnn --example stretch_eval --example live_lag --example m2_prod` -> stretch_eval 5 passed, live_lag 12 passed, m2_prod 4 passed. `cargo clippy --release -p earshot-runtime --features qnn --example stretch_eval --example live_lag --example m2_prod -- -D warnings` (`CLIPPY_CONF_DIR` at the repo root) clean; `cargo fmt --all -- --check` clean. Fixtures: `cargo xtask fetch-fixtures --set ami-dev` (5 artefacts, 185.5 MB, verified).
- Result (full tables in docs/live-previews.md, reports in `docs/live-previews/t1224/`):

| `max_piece` | bundled ref lag median / p95 | continuous ref p95 | AMI dev WER CPU / NPU | streams WER | doubled per 10 min CPU / NPU | NPU rtf |
|---|---|---|---|---|---|---|
| 30 | 14.1 / 37.0 s | 49.7 s | 17.70 / 17.86% | 1.624% | 0.21 / 0.31 | 0.0150 |
| 20 | 13.8 / 33.7 s | 37.1 s | 18.09 / 18.01% | 1.666% | 0.31 / 0.52 | 0.0172 |
| 15 | 14.1 / 22.4 s | 25.2 s | 18.12 / 18.22% | 1.688% | 0.21 / 0.21 | 0.0202 |
| 10 | 11.7 / 18.0 s | 18.1 s | 18.98 / 19.17% | 1.837% | 0.72 / 0.62 | 0.0247 |

  Live and final words differing: 0 at every value (D20 holds). **No value passes the rule**: 20 and 15 s pass points 1-5 but miss point 6 (the bundled median does not fall by 2 s); 10 s passes point 6 but fails points 2 and 3 (AMI +1.28 / +1.31 points, streams +0.21 points). Parakeet keeps 30 s. Beside the verdict: with natural pauses the median is set by the pauses; 15 s shortens the tail a lot (p95 37.0 -> 22.4 s on bundled, 49.7 -> 25.2 s on continuous) at +0.4 points of AMI dev WER. Moving to 15 s would need the owner to change point 6 to a tail criterion, labelled as decided after the measurement.
- Commands: in docs/live-previews.md under the result. All runs finished on the Surface; nothing is left for the desktop.

## 2. T12.27 Surface timing (`lane/granite-surface`, bundle)

`cargo xtask fetch-models --set llm-eval --model granite-4.2-3b-q4_k_m` (2,244 MB, verified against the pin), then:

```powershell
cargo xtask sum-eval --backend cpu --split dev --source reference --model qwen3.5-4b-q4_k_m,granite-4.2-3b-q4_k_m --meeting ES2003a --chunk-seconds 600 --threads 6 --out target\overnight\granite-surface
```

| | Qwen 3.5 4B | Granite 4.2 3B |
|---|---|---|
| tokens read / written | 9,099 / 2,272 | 8,652 / 1,698 |
| prompt / reply rate | 106 / 16.9 tok/s | 98 / 15.3 tok/s |
| whole meeting | 220.3 s | 199.8 s |
| at hang-up | 151.3 s | 127.5 s |

Both valid, no repairs. Granite is about 10% slower per token on this CPU and finished sooner only because it wrote 25% fewer tokens. Timing only, one meeting, no quality claim. Doc: `docs/summary-eval/granite-surface-cpu-2026-11-13.md` (+ the sum-eval table). The result files with summary text are not committed.

## 3. T9.24 before-profile (`lane/ui-profile`, bundle)

Driven from the session in the built-in browser. A preview-only page `apps/desktop/profile/` (never built: Vite builds `index.html` only and tsconfig does not include it) wraps `AppShell` in React's `<Profiler>`, with a 300-meeting mock store, a 3-hour finished meeting and a hook that lands 3 hours of live transcript at once. React development build: compare scenarios with each other. Full table and caveats: `docs/ui-profile/t924-before.md`. A first pass taken while the pane was hidden is discarded; the numbers are from the pane in view.

| Scenario | Commits/s | Median commit | React per second |
|---|---|---|---|
| Idle, list of 300 | 1.1 | 108 ms | 10% |
| Idle, 3 h transcript open (1,800 rows) | 1.1 | 284 ms | 30% |
| Typing a search with the transcript open | 1.6 | 248 ms | 34% |
| Live call, just started | 10.7 | 54 ms | 63% |
| Live call, 3 h of transcript | 7.2 (dropping frames) | 98 ms | 75% |
| Settings, mock download running | 2.2 | 7 ms | 1.5% |

Reading: an idle once-a-second whole-app commit (consistent with the 1 s clock) and the 10 Hz `Levels` events re-rendering far beyond the meter are the biggest costs; the long transcript makes every commit about 280 ms. Download progress cannot be judged on the mock (it emits at its own step rate). No fixes made.

## 4. T12.23 Surface run (`lane/nemotron-surface`, bundle)

From `lane/nemotron` at `dec19c6` (after the review fixes). `cargo xtask fetch-models --set stream-eval` (4 artefacts, 661.9 MB, verified). `cargo test --release -p earshot-runtime --features qnn --example live_lag` -> 13 passed. 16 runs: off, window on (`--force-previews`, the load T12.12c allows), and streaming `on` and `everyone` at 2, 3 and 4 threads, each on `bundled` and `continuous --seconds 120`, all with `--backend qnn`. Reports in `docs/live-previews/t1223/`; result section in docs/live-previews.md.

- Tracks on QNN, streaming sessions on the CPU, stored words identical to off, main transcript unaffected (point 1-3 hold).
- Thread count changed only the streaming model's own rtf (0.18 / 0.15 / 0.13 at 2 / 3 / 4); every count, lag and hash was identical.
- **Neither setting passes.** `on` fails point 4 (bundled far-end p95 36.0 s against window's 13.1 s), point 5 (17.6% of far-end words changed on bundled against 10% and window's 5.4%) and point 6 (process CPU 88-91 s against window's 54 s; peak private 3.36 against 2.71 GiB). `everyone` fails point 5; its memory is within T12.12c's 1.5 GiB over off (+1.39 GiB). The streaming median is under 1 s against the window's 2.4 s. The NPU conversion step is not indicated (points 4 and 5 fail, not only cost).
- The desktop CPU run is the coordinator's.

## 5. T12.12c check (`lane/npu-previews-surface`, bundle)

From `lane/npu-previews` at `9d18e84`. `cargo test --release -p earshot-runtime --features qnn --example live_lag` -> 12 passed. Without `--force-previews`:

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews on --out target\overnight\t1212c\qnn4-on-bundled.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio continuous --seconds 120 --previews on --out target\overnight\t1212c\qnn4-on-continuous.json
```

```powershell
cargo run --release -p earshot-runtime --features qnn --example live_lag -- . --backend qnn --audio bundled --previews everyone --out target\overnight\t1212c\qnn4-everyone-bundled.json
```

- `on`, both audios: `preview_sessions[0]` `loaded.kind` Qnn, `refusal_bypassed` false, `preview_refusal.app_first` null. Far-end on screen median / p95 2.3 / 7.8 s (bundled) and 1.8 / 3.5 s (continuous).
- `everyone`: the far end loads on QNN; the mic preview is refused with "not for your own voice on the NPU yet: some words shown there vanished again, and why is not understood".
- Pass. Reports `docs/live-previews/qnn4-*.json` and a paragraph in docs/live-previews.md's T12.12c section. Left for the owner: the real call with words within about 3 s.

## For AGENTS.md

- On a fresh clone the pre-push hook is off until `git config core.hooksPath .githooks`; the Surface checkout had it off, so one push went out unchecked. A session should check `git config core.hooksPath` before its first push.
- `stretch_eval` (earshot-runtime example, T12.24, evaluation only) decodes the AMI dev mixes with `transcribe_in_pieces` per `max_piece`; its WER is against every participant's words in time order, so it compares values with each other and is not `asr.wer_ami_ihm`. `live_lag` and `m2_prod` take an evaluation-only `--max-piece`, never passed by `accept m2`.
- `apps/desktop/profile/` (T9.24) is a preview-only profiling page, outside the build and tsconfig; numbers from it are React development-build commit times and are taken only with the Browser pane in view.

## Not done or left

- The desktop's own CPU timing for T12.23 and every merge are the coordinator's.
- The names-list check of everything above (no list on the Surface).
- By hand for the owner: T12.12c's real call; the ARM64 installer check from the owner's own PowerShell.
