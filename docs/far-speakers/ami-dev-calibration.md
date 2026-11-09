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

(Filled in after the run.)
