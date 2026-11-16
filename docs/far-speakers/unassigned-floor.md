# Leaving a voice unassigned: the cosine floor (T12.65)

Written and committed before any measurement of this task, and before the tool that measures it. The data, measures, grid, bounds and rule below are not changed after the first run; results go in the Result section only.

## The question

At clustering step 5 (`earshot_core::diar::cluster`, pyannote 3.1's small-cluster reassignment) every small cluster is merged into the large cluster whose centroid is most cosine-similar, with no lower bound. A talker who speaks only in short turns forms only small clusters, so their speech is given to speakers they are not like. The candidate change: a small cluster whose best cosine to every large cluster's centroid is below a floor `f` is left **unassigned** instead of merged. Its speech then has no speaker: no speaker row, no centroid, no voiceprint match, no "Speaker N"; the transcript shows its words as an unknown speaker. A floor of -1 is today's behaviour bit for bit (every cosine is at least -1, so nothing is ever left out).

A wrong name misleads a reader about who said what; no name does not. But unassigning a small cluster that belonged to the right speaker takes a correct name away, so the floor must not cost much right speech.

Known before writing this: the finding in PLAN.md T12.65 (one private call, read by the owner's session: not data here and not re-read), the committed T12.9 curves and per-meeting results (`ami-dev-calibration.md`, `ami-dev-calibration.json`) at floor -1. Nothing about the cosines of small clusters on the data below has been measured.

## The data

Dev data only, the T12.9 inputs exactly (`ami-dev-calibration.md`, "The data"): no AMI test-series meeting, no LibriSpeech test-clean, no private recording.

- **A.** The five AMI dev mixes of fixture set `ami-dev` (ES2003a, ES2011a, IS1008a, TS3004a, TS3006a), with T12.9's word-time reference and UEM.
- **B.** The 16 dev-clean long scenes of `cluster_sweep` (every speaker in exactly one scene), with T12.9's turn reference.

## The pipeline

The production final pass on the CPU: `load_diarisation(models, Accelerator::Cpu)` and `PostConfig::default().diarization`, with the committed clustering threshold (0.68). Only the floor varies. Model outputs are recorded by input hash and replayed for every floor, as in T12.9; the replayed diarisation at floor -1 must equal the direct run exactly (it is today's pipeline).

Grid (floors, cosine): -1 (today), then 0.00, 0.05, 0.10, ..., 0.60.

## The measures

On a 10 ms grid inside each item's scored region (UEM), compare floor `f` with floor -1 frame by frame. A frame is **moved** when, at `f`, it lies in unassigned speech and at -1 it lay in some found speaker's turn. The **owner** of a found speaker at floor -1 is the true speaker whose reference speech it overlaps the longest inside the UEM (ties to the lower label): the person that found speaker's name stands for. A moved frame is:

- a **loss** when one of the speakers active at -1 in that frame is owned by a true speaker active in that frame (right speech taken away);
- a **gain** when the reference has speech in that frame and no speaker active at -1 is owned by a true speaker active there (wrong speech taken away);
- **non-speech** when the reference has no speech in that frame (a false alarm taken away; reported, selects nothing).

Also per floor: the unassigned speech in seconds, the number of small clusters left unassigned, and DER with `DerConfig::nist(AMI_COLLAR_PER_SIDE_S)` over the UEM where **unassigned speech is scored as missed** (unassigned turns are not in the hypothesis). Moving wrong speech to missed leaves DER unchanged; moving right speech raises it.

Pooled per floor over all 21 items: G(f) gain seconds, Lo(f) loss seconds, R the total reference speech (the DER denominator summed over items, the same at every floor).

## The rule

A floor `f > -1` on the grid **passes** when all of these hold:

1. G(f) > 0: it takes some wrong speech away (a floor that changes nothing is not evidence for itself).
2. Lo(f) <= 0.25 x G(f): each second of right speech lost buys at least four seconds of wrong speech removed.
3. Lo(f) <= 0.005 x R: at most half a percent of all reference speech loses its right speaker.
4. Every AMI dev meeting's own DER at `f` is at most 0.005 above its DER at -1.

The floor shipped for CAM++ (`campplus-en-voxceleb@357a834f702b`) is the **highest** passing floor on the grid. If no floor passes, nothing ships: the CAM++ floor stays -1 (today's behaviour, pinned by a test), the tool and this measurement stay, and the result goes to the owner.

If a floor ships, `cargo xtask accept m3` and `cargo xtask accept m6` are re-run with it and their values reported; no threshold in `acceptance/thresholds.toml` changes, and if either fails the floor is held at -1 and the result goes to the owner.

## How far this can be trusted

- The dev data was not chosen for short-turn talkers: the five AMI dev meetings have four speakers each with long turns, and the scenes give every speaker long turns. If no small cluster there falls below a floor, rule 1 fails at every floor and nothing ships; that says the data cannot show a benefit, not that there is none (T12.66 adds short-turn scenes).
- 10 ms frames; owners are taken at floor -1 and not recomputed per floor.
- Correction before the first run: the first commit of this file judged a frame by T12.9's homes (each true speaker's main found speaker). The tool's own unit test showed that this counts a short-turn talker's speech under a long-turn speaker as right whenever that speaker is the talker's home, the very case this task is about. Owners (each found speaker's main true speaker) replace homes. No run had been made.
- Close-talking mixes only, as in T12.9.

## Result

Everything above was committed before the tool was written (3f3a915), with the switch from homes to owners committed before any run (02b327a). Raw report: `unassigned-floor.json` (every floor, every meeting and scene, as the tool wrote it). One run on the CPU, CAM++ `campplus-en-voxceleb@357a834f702b`, clustering threshold 0.68; the replay at floor -1 equalled the direct run on all 21 inputs. R = 8990.1 s of reference speech.

**No floor passes; the rule picks -1, and nothing ships.** `CAMPPLUS_UNASSIGNED_FLOOR` stays -1 (today's clustering bit for bit, pinned by `diar_cluster_unassigned_floor_is_validated_and_calibrated_per_model`). At every floor the right speech taken away is several times the wrong speech taken away, so rule 2 (loss <= 0.25 x gain) fails everywhere; from 0.20 up rule 3 fails too, and rule 4 at 0.20 and from 0.30.

| floor | A(f) AMI dev | L(f) scenes | gain s | loss s | non-speech s | unassigned s | rules 1-4 |
|---|---|---|---|---|---|---|---|
| -1 (today) | 0.1014 | 0.1456 | 0.00 | 0.00 | 0.00 | 0.00 | (reference) |
| 0.00 | 0.1010 | 0.1456 | 1.89 | 7.41 | 0.28 | 9.58 | yes, no, yes, yes |
| 0.05 | 0.1006 | 0.1456 | 1.74 | 11.67 | 0.36 | 13.77 | yes, no, yes, yes |
| 0.10 | 0.1013 | 0.1456 | 3.10 | 28.63 | 0.75 | 32.48 | yes, no, yes, yes |
| 0.15 | 0.1017 | 0.1456 | 5.41 | 39.47 | 1.41 | 46.29 | yes, no, yes, yes |
| 0.20 | 0.1040 | 0.1460 | 8.46 | 52.26 | 1.75 | 62.47 | yes, no, no, no |
| 0.30 | 0.1058 | 0.1460 | 14.58 | 81.82 | 5.60 | 102.00 | yes, no, no, no |
| 0.40 | 0.1113 | 0.1460 | 22.21 | 134.60 | 10.37 | 167.18 | yes, no, no, no |
| 0.50 | 0.1183 | 0.1458 | 31.90 | 202.90 | 15.92 | 250.72 | yes, no, no, no |
| 0.60 | 0.1370 | 0.1474 | 40.32 | 346.14 | 25.66 | 412.12 | yes, no, no, no |

(0.25, 0.35, 0.45 and 0.55 follow the same pattern; every row is in the JSON.)

What this does and does not say:

- Almost all the effect is on the AMI dev meetings: the scenes leave only 0.11 s unassigned at 0.00 and their DER barely moves. On AMI the small clusters that fall below a low floor are mostly fragments of a real speaker's own speech (loss), not another talker (gain): at 0.00, TS3004a gains 1.7 s and loses 3.4 s, TS3006a gains 0.2 s and loses 3.8 s.
- The pooled AMI DER dips by up to 0.0008 at 0.00-0.05 (a few seconds of confusion become missed speech, and a few small clusters no longer pull a centroid), but by the rule that is not the question: the name taken away is right more often than wrong.
- The dev data has no short-turn talker of the kind the private finding was about (as this doc expected under "How far this can be trusted"), so this does not show the floor is useless there; it shows that on ordinary meetings a cosine floor alone costs more right names than it saves wrong ones. T12.66 builds short-turn scenes.
- The tool reports unassigned seconds rather than the number of small clusters left out (`Diarization` keeps spans, not clusters); a deviation from "The measures", noted rather than changed after the run.

The code stays behind the default: `ClusteringConfig::unassigned_floor`, `Diarization::unassigned`, the post path's `PostWarning::SpeakerUnknown`, the unknown-speaker line in the app and summaries and exports. With the shipped floor of -1 none of it changes a transcript. For the owner: a floor on this data is not worth shipping; a rule that also needs the small cluster's members to be like each other (T12.66) is the next thing to measure.