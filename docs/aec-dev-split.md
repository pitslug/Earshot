# AEC tuning: dev / gating split (proposal)

Status: adopted by the owner as T1.6b, in this form (which replaces the
"held-out (gate)" row below): the held-out set is the gating scenarios at
`seed + 10_000 .. seed + 10_004` with the speaker pair 260 (far) / 237 (near) pinned in
`fixtures/librispeech/aec_heldout.toml`; `accept m1` keeps gating on the dev scenarios
exactly as before, reports every held-out scenario as an informational check (worst over
the five seeds, per metric, next to the dev value), and fails only when a held-out value
is worse than `acceptance/aec_heldout_baseline.json`. That baseline is written only by
`cargo xtask aec-heldout-baseline`, and every regeneration is logged in LOG.md. The
dev-wide set was implemented by T1.6c in another form: every gating scenario at `seed + 2000`
with eight voice pairs of LibriSpeech dev-clean speakers (`fixtures/librispeech/aec_dev.toml`,
`aec_eval --devwide`), reported by `accept m1` as an informational check; it is dev data.

Original proposal (M1 review follow-up, 2026-10-24) follows. Nothing here changes a
threshold, a gating scenario or a tuned constant; adopting it changes what `accept m1`
gates on and needs the owner's agreement (AGENTS.md 6.2).

## The problem

Every tuned echo-canceller constant (double-talk ratios and hangover, `mono_coherence_veto`,
`STEREO_ERLE_SCALE`, postfilter overdrive and `reverb_decay`, the mismatch bypass, the
delay estimator's coherence bar) was chosen by measuring the same simulated calls that
`accept m1` gates on:

- the scenarios in `acceptance/aec_scenarios.toml` at their fixed seeds (101-113);
- the `[robustness]` delay x drift grid over the listed scenarios, at those same seeds;
- both rendered from the same 20 committed smoke clips (far end: speaker 61, near end:
  speaker 121; `fixtures/smoke`), with only utterance order, gaps and noise depending on
  the seed.

So the gate measures fit, not generalisation. The only held-out evidence today is:

- the informational second-seed sweep of `accept m1` (each gating scenario at
  `seed + 1000`, T6.6a). It has been printed in every m1 run since, so it is no longer
  unseen. At 7df358d it shows `stereo-double-talk-ser-0@seed+1000` at 9.87 dB and
  `double-talk-ser-0@seed+1000` at 10.01 dB against the 10 dB bar, while the gating
  seeds give 11.57 dB (headline) and 11.97 dB;
- the GATE-10 real recording (informational ERLE, and the far-end-only ERLE test in
  `xtask/src/echo_nl.rs`), which has also been used to choose
  constants (T1.5f), so it is dev data too.

## Proposal

Three sets, fixed before any further tuning:

| Set | Content | Use |
|---|---|---|
| dev | the current scenarios at their current seeds, the robustness grid at those seeds, `seed + 1000`, and the GATE-10 recordings | tuning, diagnosis and every number quoted while choosing a constant |
| dev-wide | the current scenarios at `seed + 2000` and `seed + 3000` | sanity check of a candidate change before it is kept (catches a change that only helps the dev seeds) |
| held-out (gate) | the current scenarios and robustness grid at `seed + 10_000 .. seed + 10_004` (five seeds), and with a different speaker pair (see below) | `accept m1` gates on these; nobody prints or reads per-scenario held-out values while tuning |

Rules that go with it:

1. A constant is chosen on dev only, and the rule that picks it is written down before
   measuring (as the diarisation calibration already does, AGENTS.md "Calibration
   protocols").
2. The held-out set is only run by `accept m1` (and by the validator); its report states
   the per-metric worst case over the five seeds, the same `worst` fold as today.
3. Thresholds stay as in `acceptance/thresholds.toml`; the gate is "every held-out seed
   passes", like the robustness grid today.
4. A held-out failure is fixed by a change justified on dev, never by tuning against the
   failing held-out seed; if that is not possible it is reported to the owner (AGENTS.md 5).

Different talkers: all sets above share speakers 61 and 121, so the split guards against
overfitting gaps and overlaps but not voices. The smoke set has only those two speakers.
A stronger held-out set would draw far and near talkers from the LibriSpeech test-clean
subset that `cargo xtask fetch-fixtures` already provides (any two speakers other than 61
and 121), which makes `accept m1` depend on fetched fixtures; it fails with the fetch
instruction when they are missing (AGENTS.md invariant on fetchable fixtures).

## What changes if adopted

- `crates/earshot-core/examples/aec_eval.rs`: a held-out pass (seed offsets as above,
  optionally the second speaker pair) whose worst case becomes the headline, with the
  current fixed-seed values reported as dev.
- `xtask/src/accept/m1.rs`: gate on the held-out headline and grid; keep the dev values
  and the second-seed sweep as informational checks.
- `acceptance/aec_scenarios.toml`: a `[split]` block naming the offsets, so tests and the
  runner read the same numbers.
- Expected first result: the held-out double-talk SI-SDR at SER 0 is likely below the
  gating-seed value (the second-seed sweep is already within 0.2 dB of the bar), so
  adopting the split may turn `accept m1` red until the double-talk work (T1.5f
  follow-ups) improves on dev.
