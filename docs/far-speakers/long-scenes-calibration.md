# Long scenes in the clustering calibration (T6.9b)

The protocol and rule are the module docs of `crates/earshot-onnx/tests/diar_calibration.rs`. Curve: `long-scenes-calibration.txt`.

**Status: the rule picks 0.83, and the owner held the constant at 0.68 (2026-11-07).** The rule's data has no real meeting, and the move costs real meetings far more than it gains on these scenes. `diar_calibration_long_scenes_rule_still_picks_the_value_that_was_declined` (ignored, long) pins both values, so a change that alters the rule's pick fails and asks for a new decision. The rule and its measurements are unchanged. The branch that held the move is deleted; its tip was d289f46.

## How far to trust this

- **The scenes are not independent.** Each speaker's one stream plays in its `one`, `even` and `dominant` scenes, against AGENTS' "scenes never reuse a speaker's audio". No audio repeats within a scene. The 40 scenes are about 20 independent voices, so `COUNT_MARGIN` (written as 2 of 40 scenes) and the summed DER overstate the evidence for 0.83. Found in review. The protocol was not re-cut or re-measured, because this rule selects nothing while the constant is held.
- **The order cannot be checked from git.** That the rule came before the measurements rests on the lane's word: both commits came after the runs and share one timestamp, and the hashes the lane reported were not committed as objects, apart from the final blob. What can be checked is that the rule reproduces the result from the raw curve: applied to `long-scenes-calibration.txt`, it admits 0.68-0.72 and 0.74-0.85 and picks 0.83, with a DER win of 0.0052 and a count win of 0.075.

## What the rule measured

40 scenes, 14400 s, dev-clean speakers 21-40, CAM++ `campplus-en-voxceleb@357a834f702b`.

| Threshold | T3.2b DER | Long-scene DER | Mean count error | one / even / dominant count error |
|---|---|---|---|---|
| 0.68 | 0.1782 | 0.1301 | 0.075 | 0.050 / 0.200 / 0.000 |
| 0.83 (winner) | 0.1812 | 0.1249 | 0.000 | 0.000 / 0.000 / 0.000 |

Every step of the rule holds for 0.83: it is inside the T3.2b valley, its DER win is 0.0052 against a margin of 0.002, and `accept m3` passes.

## Why it is held

The rule's data has no real meeting and no far-field audio, and two runs outside it show what that costs. Neither took part in the rule.

**AMI dev mixes, close-talking** (the T6.9a report, `word-changes.json` and `word-changes-at-083.json`, 4 true speakers each):

| Threshold | Speakers found: ES2003a, ES2011a, IS1008a, TS3004a, TS3006a | Words given the wrong speaker |
|---|---|---|
| 0.68 | 5, 3, 4, 4, 4 | 13.16% |
| 0.83 | 5, 1, 4, 4, 1 | 32.53% |

Two of the five real meetings collapse into one speaker.

**`accept m3` on this host** (both pass):

| Metric | Limit | 0.68 | 0.83 |
|---|---|---|---|
| `dia.der_ami_sdm` | 35% | 16.19% | 26.63% |
| of which ES2004a far-field | | 17.88% | 36.43% |
| `dia.der_ami_ihm` | 25% | 11.55% | 11.55% |
| `dia.count_error_large` | 1 | 0.933 | 0.867 |
| `dia.count_error` | 1 | 0 | 0 |

The acceptance numbers are reported, not tuned on. The decision rests on the dev meetings above.

The two sources pull opposite ways: clean LibriSpeech scenes favour a higher threshold, and real meetings already under-count at 0.68 (ES2011a found 3 of 4). A further rule should have AMI dev meetings in its data, written before measuring.

Timings under the night's load are not figures.
