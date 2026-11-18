# Naming voices: leave a voice unnamed before guessing, keep voices apart before merging (T12.84)

## Why

The owner's rule (2026-11-18, after reviewing a call counts-only): a wrong name and two people merged into one voice are the costly errors, because the user can merge two voices in the Speakers panel (which also strengthens the voiceprint) but cannot split a voice or a line. So when unsure, a voice stays "Speaker N", and when unsure whether two voices are one, they stay apart. Suggestions and per-line naming were considered and declined the same day.

That call showed both errors: a far-end voice named after a stored person it was not, its whole-voice score just over the 0.462 threshold; and a voice holding two people, one of them stored and clearly matched on their own lines, left unnamed because the mixed centroid fell inside the margin. The call is private and selects nothing here.

## 1. Naming threshold: 0.56 (shipped)

T12.67 (`voiceprint-meetings.md`) measured the naming rule on AMI dev meeting audio. Its candidate A picked threshold 0.56: the lowest grid threshold above 0.462 with false accepts at most 5% in both conditions. It was held for the owner (AGENTS 6.2); the owner adopted it on 2026-11-18.

What changes, and what does not:

- **Naming a voice after a stored person** (the final pass's step 4, `post::voice_match`) needs `voiceprint::naming_threshold`: CAM++ 0.56 (`CAMPPLUS_MEETING_THRESHOLD`, `CALIBRATED_NAMING_THRESHOLDS`), plus the 0.08 margin over the runner-up (T12.67) as before. This includes naming the user's own voice by voiceprint in the room.
- **Same-voice checks** keep the dev-clean 0.462 (`calibrated_threshold`, `CAMPPLUS_DEV_CLEAN_THRESHOLD`): carrying a meeting's own speakers across re-runs (`identities::match_previous`), the echo speaker check and `drop_echo_clusters` rule 2, and the presence checks of the voice sample and "remember this voice". These compare a voice with itself or with the far end, not with a stored person, and changing them would change echo cleaning (measured by `accept m2`) for no naming gain.
- `accept m3`'s `vp.*` protocol matches at the naming threshold, as production names; `vp.top1` and `vp.false_accept` keep their thresholds.
- `vp_meetings` keeps judging T12.67's rule at 0.462 (its report reproduces T12.67's) and now also fails if candidate A's pick differs from the naming threshold.

The rule from the task (fixed before measuring): at the new threshold, false accepts on T12.67's AMI dev protocol do not rise, and `vp.top1` / `vp.false_accept` still pass.

### Result

One run of `vp_meetings` on the CPU (the ARM64 Surface; T12.67 ran on x64), CAM++ `campplus-en-voxceleb@357a834f702b`. Raw report: `voiceprint-naming.json` (the tool's output). Every T12.67 line reproduced exactly (19 persons enrolled, 40 trials, A picks 0.56, B picks 0.08, C does not pass), and the run passed both reproduction checks.

| naming rule | FA (unknown) | W (known) | M (known) | mixed events |
|---|---|---|---|---|
| before T12.84: 0.462 + margin 0.08 | 0.025 (1/40) | 0.025 (1/40) | 0 (0/38) | 1 |
| **since T12.84: 0.56 + margin 0.08** | **0 (0/40)** | 0.025 (1/40) | 0 (0/38) | 1 |

False accepts do not rise (they fall by one), no stored voice is missed: the first part of the rule holds.

**`accept m3` passes at 0.56** (ARM64, CPU): `vp.top1` 100% (unchanged), `vp.false_accept` 0% (1.67% at 0.462 with the margin: 0 of 120 unknown clips accepted), thresholds unchanged; DER and count measurements pass as before. Both parts of the rule hold, so 0.56 ships.

## 2. Clustering threshold: no lower value keeps people apart (nothing ships)

The task asked whether a clustering threshold below 0.68 keeps two people out of one voice at an acceptable cost in extra voices, with a merge counting several times an extra voice.

T12.9's committed sweep (`far-speakers/ami-dev-calibration.json`, the five AMI dev "a" meetings and 16 dev-clean long scenes, every threshold from 0.55 to 1.00 over recorded model outputs) already answers it, so nothing was run again. This reading was made after that data was measured; no rule here was fixed before it.

| threshold | AMI merges | AMI extras | scene merges | scene extras | AMI DER |
|---|---|---|---|---|---|
| 0.55 | 1 | 3 | 0 | 6 | 0.1080 |
| 0.60 | 1 | 2 | 0 | 3 | 0.1037 |
| 0.65 | 1 | 2 | 0 | 2 | 0.1036 |
| 0.68 (shipped) | 1 | 1 | 0 | 2 | 0.1014 |
| 0.70 | 1 | 0 | 0 | 1 | 0.0993 |
| 0.72 | 1 | 0 | 0 | 0 | 0.0992 |
| 0.73 | 2 | 0 | 0 | 0 | 0.1189 |

- **Lowering the threshold removes no merge.** The one AMI merge (ES2011a) is there at every threshold from 0.55 to 0.79, and the scenes have none; below 0.68 only extra voices and DER rise. So "merges x factor + extras" falls below 0.68 for no factor: a lower threshold cannot pass, and 0.68 stays.
- Over the whole grid the same score is lowest at 0.72 (T12.9's pick, held by the owner), one grid step below the cliff where a second merge appears (0.73). Moving up toward a merge cliff is the opposite of the owner's rule, so it is not recommended here either.
- The merge in the owner's call is therefore not a clustering-threshold effect. Likely places (not measured): a voice whose short turns never form a cluster of their own and are merged into the most similar large cluster (T12.65 measured an unassigned floor; none passed its rule), or two voices whose embeddings on call audio sit closer than the threshold. Finding which would need the call's own diarisation, which can explain but never select a setting.
