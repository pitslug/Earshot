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

On a 10 ms grid inside each item's scored region (UEM), compare floor `f` with floor -1 frame by frame. A frame is **moved** when, at `f`, it lies in unassigned speech and at -1 it lay in some found speaker's turn. Homes are T12.9's (the found speaker overlapping a true speaker's reference speech the longest, at floor -1). A moved frame is:

- a **loss** when one of the speakers active at -1 in that frame is the home of a true speaker active in that frame (right speech taken away);
- a **gain** when the reference has speech in that frame and no speaker active at -1 is the home of a true speaker active there (wrong speech taken away);
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
- 10 ms frames; homes are taken at floor -1 and not recomputed per floor.
- Close-talking mixes only, as in T12.9.

## Result

(Filled in after the run.)
