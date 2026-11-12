# Overnight report: lane/nemotron (T12.23, evaluation only)

Desktop, 2026-11-12/13, six lanes building beside it. No timing was taken: the machine was shared the whole time. T12.23 is not finished: the measurements (desktop CPU, then the Surface) and the recommendation are left, so it stays `[ ]`.

## Commits

1. `T12.23: pin the Nemotron streaming 0.6B int8 export for evaluation` (manifest set `stream-eval`, xtask manifest tests)
2. `T12.23: Nemotron streaming decoder checked against sherpa-onnx` (`earshot_onnx::nemotron`, example `nemotron_probe`)
3. `T12.23: evaluation-only streaming preview seam and Nemotron loader` (`LivePreviews::stream`, `backends::load_nemotron_preview`, scan test)
4. `T12.23: rule for streaming previews, committed before measuring` (docs/live-previews.md)
5. `T12.23: live_lag --stream nemotron, the evaluation loader in a replay`
6. This report.

`lane/nemotron` was pushed after commit 5 (public-guard ok), so the Surface can run the rule from it.

## Step 1: the package

- It exists as researched: sherpa-onnx's `sherpa-onnx-nemotron-speech-streaming-en-0.6b-560ms-int8-2026-04-25`, as a GitHub release tarball (`k2-fsa/sherpa-onnx`, tag `asr-models`) and as a Hugging Face repo `csukuangfj2/...` that sherpa-onnx's own workflow (`.github/workflows/export-nemotron-speech-streaming-en-0.6b.yaml` at v2.0.1) publishes to. The tarball is `.tar.bz2`, which xtask cannot extract, so the four files are pinned one by one from huggingface.co (already in `hosts`; redirects to `*.hf.co`, already in `redirect_hosts`). No new host.
- Pinned to commit `52056fdc070914a48dcd68b31b44d6a6f5b85902`: `encoder.int8.onnx` 652,916,849 bytes, `decoder.int8.onnx` 7,257,753, `joiner.int8.onnx` 1,735,862 (SHA-256 = Hugging Face LFS oids), `tokens.txt` 8,952 (SHA-256 computed, git blob sha1 as publisher checksum). Licence `LicenseRef-NVIDIA-Open-Model-License`, allowed by `models_are_cache_only_with_licence_and_permissive_terms` only when an artefact's sets are exactly `["stream-eval"]`; `models_manifest_has_the_planned_models_and_sets` pins the set list and that its artefacts join no other set.
- `cargo test -p xtask models` -> `test result: ok. 14 passed`; `cargo xtask fetch-models --set stream-eval` -> 4 artefacts, 661.9 MB, `downloaded, verified`.

## Step 2: the decoder against the reference

`earshot_onnx::nemotron` ports sherpa-onnx v2.0.1's streaming NeMo transducer (kaldi-native-fbank v1.22.4 features with sherpa-onnx's NeMo options; 65-frame chunks moving 56; three encoder caches carried over; greedy RNNT, 10 symbols per frame, prediction network state carried over). The reference is sherpa-onnx's own Python package 2.0.1 (`sherpa-onnx` + `sherpa-onnx-core`, hash-pinned wheels, `--require-hashes --no-deps --only-binary :all:`, throwaway venv under `target/nemotron-ref/`), run by `examples/nemotron_probe/reference.py` with sherpa-onnx's own protocol (whole clip, 0.66 s zeros, input finished, every ready chunk) on the 20 smoke clips. What was compared, per clip: the result text, every token and every token's time.

- **Text and tokens: identical on all 20 clips** (306 tokens), decoding the whole clip and decoding it pushed in uneven pieces (decode after every push). The pushed decode equals the whole one exactly, times included.
- **Token times: 31 of 306 differ, by up to 3 encoder frames (240 ms).** Investigated before going on:
  - The reference itself moves them with its ONNX Runtime: the same package with its `onnxruntime.dll` (1.28.2) replaced by Earshot's pinned 1.31.0 changes 24 of 286 token times by up to 3 frames, and a comma on one clip (`reference_ort131.jsonl`, committed).
  - The port's front end matches kaldi-native-fbank 1.22.4 (Python wheel, hash-pinned) to float rounding: largest difference 2.6e-4 in a log-mel value, mean about 2e-6, same frame counts on every clip.
  - Given kaldi-native-fbank's own features (`knf_features.py`, then `nemotron_probe --decode-features`), the decoder gives sherpa-onnx-on-1.31.0's tokens **and every time exactly** on all 20 clips. So encoder chunking, caches and greedy search are exact; the FFT's rounding moves a few near-tie emission frames, as the ORT version does in the reference.
- So the probe requires identical text and tokens and reports times. `cargo test -p earshot-onnx --example nemotron_probe -- --include-ignored` -> `test result: ok. 3 passed` (`nemotron_probe: 0 clips differ; 31 of 306 token times moved, by up to 3 frames` from `cargo run`). `cargo test -p earshot-onnx --lib nemotron` -> `3 passed` (front end independent of push sizes; frame counts; mel bins).

## Step 3: scan test and seam

- `earshot_runtime::live::StreamingPreview` + `LivePreviews::stream` (None in the app and in `LivePreviews::new`): with it set, a track's preview thread feeds every packet once (quantised as stored) and shows finished words; start/stop, settling, events and report are the existing path. A word is shown once a whole chunk (7 encoder frames) has been decoded after its last token, so a shown word never changes.
- `backends::load_nemotron_preview(models, threads)`: CPU, background pool threads, placement checked, files checked with the fetch command in the error. The only `StreamingPreview` implementation in shipped crates is private in `backends.rs` and built only by it.
- `backends_nemotron_preview_load_is_used_by_no_shipped_code` scans runtime, CLI and desktop sources for `load_nemotron_preview`, `nemotron::`, `earshot_onnx::nemotron` and `NEMOTRON_STREAM` -> pass.
- New runtime tests (fake streaming model): `live_preview_stream_shows_words_hears_each_sample_once_and_stores_the_same` (stored words equal to previews off, window loader never called, every pushed sample counted once, no restart) and `live_preview_stream_words_leave_when_settled_and_never_reach_the_store` -> both pass, three runs in a row.
- `cargo test -p earshot-runtime --lib -- live_preview backends_ catalogue`: 122 passed, 2 failed under the 6-lane load; both passed alone. One was the existing `live_preview_own_voice_in_a_call_shows_both_tracks_and_reports_each` (`words_left_early` assertion), which this lane does not touch (the window path only gained an early `if let` for the stream); the other was an earlier draft of my own stream test, since rewritten (it asserted a load-dependent property). Worth watching in the batch ci.

## Step 4: the rule

Committed in docs/live-previews.md ("Provisional words from a streaming model") before any measurement: six points (where it ran; stored words identical; main transcript rtf <= 1.5x off and catch-up <= off + 2 s, no drops; on-screen median <= 3 s and <= window + 0.5 s, p95 <= window's; changed <= 10% and <= 2x window's; CPU time and peak memory <= window's, or T12.12c's limits over off where window previews are refused), on both audios, for `on` and `everyone`. Desktop at 2 threads; Surface at 2, 3 and 4.

## Step 5: live_lag

`--stream nemotron [--stream-threads n]` (needs `--previews on|everyone`, refuses `--memory`, `--force-previews`, `--card-memory-gib`, `--contention`). `cargo test -p earshot-runtime --example live_lag` -> `13 passed`. One 30 s functional run on the loaded host confirmed it loads on `CPUExecutionProvider` (3 sessions), shows words and reports; its numbers are not a measurement and are not kept.

Clippy (`CLIPPY_CONF_DIR` at the worktree root, `-p earshot-runtime -p earshot-onnx --all-targets -D warnings`) and `cargo fmt --all -- --check`: clean.

## Desktop timing commands for the coordinator (idle machine only)

From the worktree root (or main after the merge), with `CARGO_TARGET_DIR` as you build. Models: `ci` and `stream-eval` (already fetched on this desktop). Ten runs, about 23 minutes of real time; nothing else running:

```powershell
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews off --out target\live-lag\t1223-off-bundled.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews on --out target\live-lag\t1223-window-on-bundled.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews on --stream nemotron --stream-threads 2 --out target\live-lag\t1223-stream-on-bundled.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews everyone --out target\live-lag\t1223-window-everyone-bundled.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio bundled --previews everyone --stream nemotron --stream-threads 2 --out target\live-lag\t1223-stream-everyone-bundled.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio continuous --seconds 120 --previews off --out target\live-lag\t1223-off-continuous.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio continuous --seconds 120 --previews on --out target\live-lag\t1223-window-on-continuous.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio continuous --seconds 120 --previews on --stream nemotron --stream-threads 2 --out target\live-lag\t1223-stream-on-continuous.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio continuous --seconds 120 --previews everyone --out target\live-lag\t1223-window-everyone-continuous.json
cargo run --release -p earshot-runtime --example live_lag -- . --audio continuous --seconds 120 --previews everyone --stream nemotron --stream-threads 2 --out target\live-lag\t1223-stream-everyone-continuous.json
```

Judge with the rule's "What to read" list; copy the reports to `docs/live-previews/t1223/` and add a Result section under the rule (never change the rule).

## For PLAN.md, AGENTS.md and TODO.md

- PLAN T12.23: leave `[ ]`; note steps 1-2 done, the rule committed, measurement and recommendation open (desktop timing above, Surface run from `lane/nemotron`).
- AGENTS.md bullet proposed: "Streaming previews (T12.23) are evaluation only: `earshot_onnx::nemotron` (a port of sherpa-onnx v2.0.1's streaming NeMo transducer, checked by `nemotron_probe` against sherpa-onnx's Python package: identical tokens; times move by up to 3 frames with ONNX Runtime's version in the reference itself, exact on kaldi-native-fbank's features), `LivePreviews::stream` (None in the app), `backends::load_nemotron_preview` and `live_lag --stream`; no shipped code names them (`backends_nemotron_preview_load_is_used_by_no_shipped_code`). Manifest set `stream-eval` is the only place the NVIDIA Open Model License is allowed; its artefacts join no other set. Reference text for a model comes only from its publisher's own tool (hash-pinned wheels in a throwaway venv under `target/`), never from this crate."
- TODO.md: none added or removed.

## Not done

- Every timing: the desktop runs above (idle machine) and the Surface runs (`lane/nemotron-surface`).
- The result and recommendation in docs/live-previews.md.
- No reviewer was spawned (T12.23 is not on the review list); the seam in `live/preview.rs` is shipped code with `stream: None`, so the lane-merge reviewer should read `stream_until_stopped` and the two new tests.
