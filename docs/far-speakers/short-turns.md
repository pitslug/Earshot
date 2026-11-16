# A far-end voice that speaks only in short turns (T12.66)

Written and committed before any code or measurement of this task. The data, candidates, grid, measures and rule below are not changed after the first run; results go in the Result section only. Evaluation only: nothing here changes what the app does.

## The question

T12.65 found that a talker who speaks only in short turns gets no speaker of their own: each short turn makes a small cluster, and step 5 of the clustering (`earshot_core::diar::cluster`) merges every small cluster into the most similar large one, however unlike it is. Leaving such clusters unassigned (a cosine floor) cost more right speech than it saved on dev data. The private finding was that the talker's short-turn clusters were more like each other than like any speaker found, though too far apart to merge at the clustering threshold (0.68). The candidate here: take the small clusters that are unlike every large cluster, group them among themselves at a looser distance, and make a new speaker from a group only when it holds enough speech.

Known before writing this: the T12.9 and T12.65 results (`ami-dev-calibration.md`, `unassigned-floor.md` and their JSON). No small cluster's grouping has been measured on any data.

## The data

Dev data only: no AMI test-series meeting, no LibriSpeech test-clean, no private recording.

- **A.** The five AMI dev mixes of fixture set `ami-dev` (ES2003a, ES2011a, IS1008a, TS3004a, TS3006a), with T12.9's word-time reference and UEM.
- **L.** The 16 dev-clean long scenes of `cluster_sweep` (T12.9), unchanged. No speaker there speaks only in short turns.
- **S.** 10 new dev-clean short-turn scenes, built by `cluster_sweep` from a fixed seed (`SHORT_SEED` = 16_600 plus the scene index):
  - 4 speakers each, 300 s long. Scene `i` (0 to 9): the short-turn talker is dev-clean speaker position `i`, the three long-turn speakers are positions `10 + 3i`, `11 + 3i` and `12 + 3i` (positions in sorted folder order, as T12.9). Every one of the 40 speakers is in exactly one S scene.
  - Long-turn speakers take turns of 8 to 25 s in a random order, never the same speaker twice in a row. After each long turn, with probability 1/3, the short-turn talker speaks one turn of 1.0 to 3.5 s before the next long turn. Gaps between turns are 100 to 800 ms; no overlap.
  - The talker's speech in a scene is reported (expected about 25 to 40 s).
  - **No audio is played twice.** S scenes read each speaker's utterances from the last one backwards; L scenes read them from the first one forwards. The tool fails unless, for every speaker, no utterance file is used by both its L scene and its S scene, and no turn within a scene replays a sample.

## The pipeline

The production final pass on the CPU: `load_diarisation(models, Accelerator::Cpu)`, `PostConfig::default().diarization`, the committed clustering threshold 0.68 and unassigned floor -1. Model outputs are recorded by input hash and replayed for every setting, as in T12.9. The **baseline** is today's pipeline; the replayed baseline must equal the direct run exactly on every item.

## The candidates

A candidate never changes the baseline's clusters, centroids or the speaker any embedding is stitched to, except to move an embedding to a **new speaker**. So it can only take speech away from the speakers today's pipeline finds and give it to a new one; it never moves speech between two of today's speakers.

1. **Candidate groups.** At step 5, a small cluster whose highest cosine to every large cluster's centroid is below `f` is a candidate group (exactly T12.65's test). At `f` the baseline's assignment of these groups is kept as the fallback.
2. **Grouping.** The candidate groups are clustered among themselves by centroid linkage (the same Euclidean distance between the means of unit embeddings as step 2), each group starting as one node, merging while the closest pair's distance is at most `d`. Lowest index on ties.
3. **Enough speech.** A grouped cluster becomes a new speaker only when its speech estimate is at least `m` seconds. Speech estimate: the sum over its member embeddings of each embedding's clean frames times the segmenter's frame step, divided by the number of windows that cover one moment (window length over window step, 10 with the shipped settings). A grouped cluster below `m` falls back: each of its groups stays where the baseline put it.
4. **Weighting.** Two variants of step 2: **plain** (every member embedding counts once in a mean) and **weighted** (each member embedding counts by its clean frames, so a longer stretch of speech pulls the mean further).
5. **Stitching.** The baseline's stitching is computed first, unchanged. Then each window local whose embedding is a member of a new speaker is given to that new speaker instead of its baseline speaker; two locals of one window given to the same new speaker keep only the more similar one there (the other keeps its baseline speaker). An embedding outside the clustering's training subset (more than 1000 embeddings) is given to a new speaker only when its cosine to that new speaker's centroid is higher than to its baseline speaker's centroid. Reconstruction and turns are then the baseline's code over the baseline's speakers plus the new ones.

The candidate is a setting of `earshot_core::diar` that is off by default; turning it on anywhere but this tool needs its own task.

## The grid

- `f` (cosine): 0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 1.0 (1.0 takes every small cluster).
- `d` (distance): 0.68, 0.80, 0.90, 1.00, 1.10.
- `m` (seconds): 4, 8, 16.
- weighting: plain, weighted.

240 settings, each over all 31 items.

## The measures

Per item and setting, inside the item's UEM, with DER by `DerConfig::nist(AMI_COLLAR_PER_SIDE_S)`. The **owner** of a found speaker is the true speaker whose reference speech it overlaps the longest (ties to the lower label), as in T12.65.

- **Talker found** (S scenes): some found speaker is owned by the scene's short-turn talker. Counted at the setting and at the baseline.
- **New speakers:** found speakers the candidate made (not one of the baseline's).
- **False new speaker:** a new speaker not owned by an S scene's short-turn talker. On L scenes and AMI dev meetings every new speaker is false.
- **False scenes:** L scenes with at least one new speaker.
- **Moved speech:** seconds that left a baseline speaker for a new one, split into right (the frame's reference holds the new speaker's owner) and wrong.
- DER per item; pooled DER over A, over L and over S.

## The rule

A setting **passes** when all of these hold:

1. **Finds the talker:** talkers found on S at the setting is at least 5 of 10, and more than at the baseline.
2. **Few false speakers on long scenes:** at most 1 of the 16 L scenes is a false scene.
3. **Few false speakers among short scenes:** at most 2 false new speakers summed over the 10 S scenes.
4. **No AMI dev meeting worse:** every AMI dev meeting's DER at the setting is at most 0.005 above its baseline DER.
5. **Short scenes not worse:** pooled S DER at the setting is at most the baseline's.

If several settings pass, the rule picks the one with the most talkers found, then the fewest false new speakers (L, S and A together), then the lowest pooled A DER, then the highest `f`, the lowest `d`, the highest `m`, plain before weighted.

If a setting passes, the result is a production task for the owner to agree (AGENTS 6.2), which must re-run `cargo xtask accept m3` and `accept m6` and add the per-model setting with its source. If no setting passes, the result is recorded, the setting stays off, and the question is parked.

## How far this can be trusted

- S scenes are synthetic: read speech, one speaker per turn, no overlap, no echo. A pass says the grouping can find a short-turn talker who is like themselves; a real call has noisier short turns.
- Ten talkers: a pass at 5 of 10 is a weak signal, which is why rules 2 to 5 guard the cost.
- 10 ms frames; owners per setting.
- Close-talking AMI mixes only.

## Result

Not run yet.
