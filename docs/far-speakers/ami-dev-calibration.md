# Clustering threshold with AMI dev meetings in the data (T12.9)

Written and committed before any measurement of this task. The data, measures and rule below are not changed after the first run; results go in the Result section only. Measurement only: `CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD` (0.68) and every test, threshold and fixture stay as they are whatever the rule picks. A different pick is a recommendation for the owner.

Known before writing this: the committed T3.2b short-scene curve S(t) and the T6.9b long-scene curve (`long-scenes-calibration.txt`), and the speakers found on the five AMI dev meetings at 0.68 (5, 3, 4, 4, 4) and 0.83 (5, 1, 4, 4, 1) from `long-scenes-calibration.md`. Nothing else about these meetings or the scenes below has been measured.

Tool: `cargo run --release -p earshot-runtime --example cluster_sweep -- <repo-root> --models <dir> --out docs/far-speakers/ami-dev-calibration.json`, written after this file. Counts and times only; no word text is printed or written.

## The data

No acceptance or test data takes part: no AMI test-series meeting (ES2004, ES2014, IS1009, TS3003, TS3007), no LibriSpeech test-clean, no private recording.

**A. AMI dev meetings.** The first meeting of each AMI dev series, ES2003a, ES2011a, IS1008a, TS3004a and TS3006a: the whole `Mix-Headset` recording from fixture set `ami-dev` (16 kHz mono, quantised as the FLAC writer stores audio), diarised from time 0.

- Reference: the AMI manual annotations v1.6.2 words files `<meeting>.<letter>.words.xml` (letters A to E, whichever exist). A speaker's speech is its `<w>` elements that are not `punc="true"` and have a start and end time with end > start (truncated fragments included: they are speech). One speaker's intervals with a gap under 0.5 s between them are joined. The true count is the number of speakers with any interval.
- Scored region (UEM): from the earliest reference start to the latest reference end of the meeting.
- DER: `DerConfig::nist(AMI_COLLAR_PER_SIDE_S)` (0.25 s per side) with that UEM, overlap scored.

This is not the AMI-diarization-setup `only_words` reference (it has no dev RTTMs pinned here), so these DERs are not comparable with `accept m3`'s. Only differences between thresholds matter.

**B. Long scenes on disjoint audio.** LibriSpeech dev-clean, the 40 speaker folders of `librispeech/dev-clean/LibriSpeech/dev-clean` in path order. **Every speaker plays in exactly one scene**, so no audio is reused within or across scenes. A speaker's stream is its utterances in path order (chapter folders, then files), joined with 200 ms of silence; a stream is only read forward. 16 scenes, each 360 s long unless a stream runs out first: a scene ends at the turn whose speaker has less than that turn's minimum length left (the length reached is reported).

| Kind | Speakers (path positions) | Scenes | Turns | True count |
|---|---|---|---|---|
| one | 0, 1, 2, 3 | 4 | one speaker, 360 s of its stream | 1 |
| even | (4,5), (6,7), (8,9) | 3 | alternating, each 5-30 s | 2 |
| dominant | (10,11), (12,13), (14,15) | 3 | alternating, first speaker 40-90 s, second 4-12 s | 2 |
| four | 16-19, 20-23, 24-27, 28-31, 32-35, 36-39 | 6 | 2-20 s each; the first four turns go to the group in order, each later turn to one of the other three speakers, drawn | 4 |

- Draws (turn lengths in ms, the next speaker, the change) come from a 64-bit LCG (`far_speakers`'s `next_turn` constants) seeded `12_900 + scene index` (scenes numbered in the table's order, 0-15), so they do not depend on the audio.
- A change in `even` and `dominant` is 200 ms of silence. A change in `four` is a silence of 0.1-0.8 s, or with one draw in five an overlap of 0.2-1.0 s capped at half the previous turn.
- The mix is scaled down only if it would clip. Reference turns are the turn spans (pauses inside a turn count as speech). DER: `DerConfig::nist(AMI_COLLAR_PER_SIDE_S)` over the whole scene.
- The tool fails before measuring if a speaker folder is in two scenes, or if two turns of one speaker play overlapping samples of its stream.

The 40 speakers were seen by T6.9, T6.9a and T6.9b as dev data; these scenes are new.

## The pipeline

The production final pass on the CPU: `load_diarisation(models, Accelerator::Cpu)` and `PostConfig::default().diarization`. Only `clustering.threshold` varies. Each input is diarised once with the models; the segmenter's and embedder's outputs are kept by input hash and replayed for every other threshold (they do not depend on it). A replay that needs an output not recorded fails the run, and the replayed diarisation at 0.68 must equal the direct one exactly.

Grid: 0.55, 0.56, ..., 1.00.

## The measures

Per meeting or scene at threshold t:

- DER components: missed, false alarm, confusion, total. Pooled per set by summing components.
- Found speakers k (speakers of the diarisation with a turn inside the scored region) and count error |k - true|.
- **Home** of a true speaker: the found speaker whose turns overlap that speaker's reference speech the longest inside the scored region (no collar; ties to the lower index). A true speaker overlapped by no found speaker is **lost**.
- **Merges**: true speakers that share a home with another true speaker, beyond the first (true speakers with a home minus distinct homes). A collapse of 4 speakers into 1 is 3 merges.
- **Extras** (over-splits): found speakers that are no true speaker's home, whatever their amount of speech; their speech is reported beside the count.

So k - true = extras - merges - lost.

Per set at t: A(t) the pooled AMI dev DER, L(t) the pooled scene DER, M(t) and X(t) the merges and extras of each set, and C(t) the summed count error over all 5 meetings and 16 scenes. S(t) is the committed T3.2b short-scene DER (`long-scenes-calibration.txt`, not re-measured).

## The selection rule

1. **Admissible**: t with S(t) <= min S + 0.011 over the committed curve (the T3.2b valley height, as T6.9b's rule), and, for every meeting and every scene, merges at t no more than at 0.68. A threshold that merges one more real or synthetic speaker than the held value is out.
2. **Winner** among the admissible: lowest A rounded to 0.001, then lowest C, then lowest L rounded to 0.001, then nearest 0.68, then the lower threshold.
3. **Move** to the winner only if all of these hold, otherwise the rule picks 0.68:
   - either A(0.68) - A(winner) >= 0.005, or A(winner) <= A(0.68) and C(0.68) - C(winner) >= 2. The count branch can only fire if C(0.68) >= 2: a count cannot fall from zero, and that is stated rather than fixed after the fact.
   - L(winner) <= L(0.68) + 0.002.
   - No AMI dev meeting's own DER at the winner is more than 0.02 above its DER at 0.68.
4. The pick is a recommendation. The constant changes only with the owner's agreement, and then `accept m3` and `accept m6` must pass; this task runs neither with another threshold.

Also reported, selecting nothing: the threshold that minimises A(t) over the whole grid without step 1, so the owner can see what the short-scene valley costs on real meetings.

## How far this can be trusted

- Five AMI dev meetings with four speakers each, all close-talking mixes: 20 real voices. No far-field dev audio is pinned (`accept m3`'s SDM loss at 0.83 came from a far-field test meeting), so a merge that appears only on far-field audio is invisible here.
- The AMI reference is built here from the words files, not the published diarisation setup.
- Timings are not measured; the host may be shared.

## Result

Everything above was committed unchanged before the tool was written (commit 48f6844) and before the run. Raw report: `ami-dev-calibration.json` (every grid point, every meeting and scene). One run on the CPU, CAM++ `campplus-en-voxceleb@357a834f702b`; the replay at 0.68 equalled the direct run on all 21 inputs. The tool was reformatted by `cargo fmt` after the run, with no change to what it computes. Every scene reached 360 s. Speakers found at 0.68 (5, 3, 4, 4, 4) and 0.83 (5, 1, 4, 4, 1) match the T6.9b report.

**The rule picks 0.72.** Admissible: 0.68-0.72 (0.73 and up add a merge on a real meeting or a scene, or leave the short-scene valley; below 0.68 leaves the valley). Winner 0.72 by A rounded (0.099, tied with 0.70 and 0.71), then the lowest C. The move holds on the count branch: A(0.72) <= A(0.68) and C falls from 4 to 1; scenes improve; no meeting gets worse. The AMI DER win alone (0.0022) is under the 0.005 margin.

| t | S(t) | A(t) AMI dev | L(t) scenes | C(t) | AMI merges / extras | scene merges / extras | found per meeting |
|---|---|---|---|---|---|---|---|
| 0.68 (held) | 0.1782 | 0.1014 | 0.1456 | 4 | 1 / 1 | 0 / 2 | 5, 3, 4, 4, 4 |
| 0.70 | 0.1792 | 0.0993 | 0.1417 | 2 | 1 / 0 | 0 / 1 | 4, 3, 4, 4, 4 |
| 0.72 (pick) | 0.1851 | 0.0992 | 0.1370 | 1 | 1 / 0 | 0 / 0 | 4, 3, 4, 4, 4 |
| 0.73 | 0.1947 | 0.1189 | 0.1370 | 2 | 2 / 0 | 0 / 0 | 4, 3, 4, 3, 4 |
| 0.80 | 0.1812 | 0.1609 | 0.1370 | 3 | 3 / 2 | 0 / 0 | 5, 2, 4, 4, 4 |
| 0.83 (T6.9b) | 0.1812 | 0.3071 | 0.1420 | 8 | 7 / 2 | 1 / 0 | 5, 1, 4, 4, 1 |

Per AMI dev meeting (DER; 4 true speakers each):

| Meeting | 0.68 | 0.70 | 0.72 | 0.73 | 0.83 |
|---|---|---|---|---|---|
| ES2003a | 0.0802 (1 extra, 17.7 s) | 0.0690 | 0.0690 | 0.0690 | 0.0690 (1 extra, 3.0 s) |
| ES2011a | 0.1076 (1 merge) | 0.1076 (1 merge) | 0.1076 (1 merge) | 0.1079 (1 merge) | 0.5242 (3 merges) |
| IS1008a | 0.0487 | 0.0487 | 0.0487 | 0.0487 | 0.0487 |
| TS3004a | 0.1147 | 0.1141 | 0.1139 | 0.2088 (1 merge) | 0.2096 (1 merge, 1 extra) |
| TS3006a | 0.1477 | 0.1477 | 0.1476 | 0.1477 | 0.6186 (3 merges) |

Scenes by kind (pooled DER, summed count error):

| Kind | 0.68 | 0.70 | 0.72 |
|---|---|---|---|
| one (4) | 0.1642, 0 | 0.1642, 0 | 0.1642, 0 |
| even (3) | 0.1244, 0 | 0.1244, 0 | 0.1244, 0 |
| dominant (3) | 0.1342, 1 (dominant-7: 1 extra, 21.9 s) | 0.1137, 0 | 0.1137, 0 |
| four (6) | 0.1492, 1 (four-13: 1 extra, 27.0 s) | 0.1492, 1 | 0.1362, 0 |

Also reported, selecting nothing: the lowest A over the whole grid is at 0.70 (0.0993; 0.71 and 0.72 round the same). Going lower never fixes ES2011a's merge (3 of 4 found at every threshold from 0.55 to 0.79), so that merge is not a threshold problem.

What this does and does not say:

- 0.72 sits one grid step below a cliff: at 0.73 TS3004a merges two speakers and its DER nearly doubles. 0.70 gets most of the gain (all AMI and dominant extras gone, A 0.0993) three steps from that cliff; its only remaining extra is one four-speaker scene. The rule has no margin-to-cliff term; that is a gap in it, noted rather than fixed after the fact.
- The gain is small and rests on three events: C falls from 4 to 1 by removing three over-splits (ES2003a, dominant-7, four-13), each a short extra speaker of 18-27 s. The fourth count error is ES2011a's merge, which stays.
- 0.72 is inside the T3.2b valley only by 0.0041 (S 0.1851 against a limit of 0.1892).
- Close-talking mixes only: a far-field merge (the `accept m3` SDM loss at 0.83) cannot be seen here.

## If the owner agrees to a move

The constant would change in `crates/earshot-core/src/diar/cluster.rs`, and these would then need the owner's decision, because each pins 0.68 today:

- `diar_calibration_campplus_dev_clean_cluster_threshold_is_reproducible` (the T3.2b rule picks 0.68 and the test requires the constant to equal it) and `diar_calibration_long_scenes_rule_still_picks_the_value_that_was_declined` (requires the constant to be the held 0.68). Both would fail by design; the decision would be to record this rule as the one that sets the constant and change those pins in the same commit.
- `cargo xtask accept m3` (it fails if the thresholds used differ from the core calibrations, so it follows the constant; re-run and compare `dia.der_ami_sdm`, `dia.der_ami_ihm`, `dia.count_error`, `dia.count_error_large` with the 0.68 values in `long-scenes-calibration.md`), then `cargo xtask accept m6`, and `word_changes` re-run for the AMI dev WDER.

None of these was run with another threshold in this task.
