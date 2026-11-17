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
  - Long-turn speakers take turns of 8 to 20 s in a random order, never the same speaker twice in a row. After each long turn, with probability 2/3, the short-turn talker speaks one turn of 1.0 to 3.5 s before the next long turn. Gaps between turns are 100 to 800 ms; no overlap.
  - The talker's speech in a scene is reported, and the tool fails if any scene holds less than 16 s (the top of the `m` grid).
  - **No audio is played twice.** S scenes read each speaker's utterances from the last one backwards; L scenes read them from the first one forwards. The tool fails unless, for every speaker, no utterance file is used by both its L scene and its S scene, and no turn within a scene replays a sample.

## The pipeline

The production final pass on the CPU: `load_diarisation(models, Accelerator::Cpu)`, `PostConfig::default().diarization`, the committed clustering threshold 0.68 and unassigned floor -1. Model outputs are recorded by input hash and replayed for every setting, as in T12.9. The **baseline** is today's pipeline; the replayed baseline must equal the direct run exactly on every item.

## The candidates

A candidate never changes the baseline's clusters, centroids or the speaker any embedding is stitched to, except to move an embedding to a **new speaker**. So it can only take speech away from the speakers today's pipeline finds and give it to a new one; it never moves speech between two of today's speakers.

1. **Candidate groups.** At step 5, a small cluster whose highest cosine to every large cluster's centroid is below `f` is a candidate group (exactly T12.65's test). At `f` the baseline's assignment of these groups is kept as the fallback.
2. **Grouping.** The candidate groups are clustered among themselves by centroid linkage (the same Euclidean distance between the means of unit embeddings as step 2), each group starting as one node, merging while the closest pair's distance is at most `d`. Lowest index on ties.
3. **Enough speech.** A grouped cluster becomes a new speaker only when its speech estimate is at least `m` seconds. Speech estimate: the sum over its member embeddings of each embedding's clean frames times the segmenter's frame step, divided by the number of windows that cover one moment (window length over window step, 10 with the shipped settings). A grouped cluster below `m` falls back: each of its groups stays where the baseline put it.
4. **Weighting.** Two variants of step 2: **plain** (every member embedding counts once in a mean) and **weighted** (each member embedding counts by its clean frames, so a longer stretch of speech pulls the mean further).
5. **Stitching.** The baseline's stitching is computed first, unchanged. Then each window local whose embedding is a member of a new speaker is given to that new speaker instead of its baseline speaker; two locals of one window given to the same new speaker keep only the more similar one there (the other keeps its baseline speaker). An embedding outside the clustering's training subset (more than 1000 embeddings) is given to a new speaker only when its cosine to that new speaker's centroid is higher than to its baseline speaker's centroid. A local left out of clustering by the `min_active_ratio` filter (off in the shipped settings) is given to a new speaker by the same similarity test against the speaker it was stitched to. Reconstruction keeps the baseline's speakers on each frame as the baseline would have them (computed from the baseline's stitching), except that a baseline speaker on in a frame is replaced there by a new speaker when more of its activity in that frame went to that new speaker than stayed with it; so a baseline speaker never gains a frame and only ever loses one to a new speaker. Turns then come from the baseline's code.

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
- **Kept speakers gaining:** seconds where a baseline speaker is on that the baseline did not have on. Must be 0 on every item and setting; the tool fails otherwise.
- Per item: the number of embeddings and whether it exceeds the clustering's 1000 training embeddings.
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
- The speech estimate sums training members only. On an item with more than 1000 embeddings (likely every AMI meeting, unlikely a 300 s scene) only a sample of 1000 trains, so a group's speech is under-counted by about 1000 over the embedding count: at one `m` a new speaker is harder to make on AMI than on a scene, which makes rule 4 easier to pass and a picked `m` not directly transferable to long real meetings.

Correction (2026-11-17, before the first run): the first commit of this file had long turns of 8 to 25 s and a talker turn after a long turn with probability 1/3, and expected 25 to 40 s of talker speech per scene. The implementer found those parameters give 3 to 21 s (about 13 s on average), so `m` = 16 could pass rule 1 in at most 4 scenes and one scene could not reach `m` = 4. Long turns became 8 to 20 s, the probability 2/3, and a scene below 16 s of talker speech now fails the tool. The scene builder's draws do not depend on the audio, so the new talker speech per scene is known without a run: 18.8 to 33.0 s. No measurement had been made.

Second correction (2026-11-17, before any result was seen): a review of the tool found that reconstruction, which picks each frame's speakers by activity, could let one of today's speakers win a frame from another when locals moved to a new speaker, breaking the promise in "The candidates" at the frame level while the tool did not measure it. A first run had started; it was stopped before writing any result, and its progress log was deleted unread. Step 5 now pins each frame's baseline speakers (above), the tool checks per frame that no baseline speaker gains a frame, and reports embedding counts. The `min_active_ratio` sentence and the training-subset caveat were added at the same time.

## Result

**First run (2026-11-17, desktop, CPU, release build of `cluster_sweep --short-turns`): the rule picks f 0.60, d 0.80, m 8 s, plain.** Raw output: `short-turns.json` beside this file, the tool's unmodified report.

- The replayed baseline equalled the direct run on every item, and no baseline speaker gained a frame at any setting (kept speakers gaining 0.00 s throughout); the tool fails on either.
- Talker speech per short scene: 18.8 to 33.0 s, as predicted. Embeddings: 1239 to 2592 per AMI dev meeting (all five above the 1000 trained), 357 to 619 per scene (none above).
- **Baseline:** talkers found 6 of 10. Pooled DER: A 0.1014, L 0.1456, S 0.1748.
- **Pick (f 0.60, d 0.80, m 8, plain):** talkers found 9 of 10; 0 new speakers on any L scene or AMI meeting and 0 false new speakers on S (3 new speakers, all owned by their scene's talker). Moved speech 45.5 s: 43.8 s right, 1.7 s wrong. Pooled DER: A 0.1014 (every meeting unchanged), L 0.1456 (unchanged), S 0.1615. The three scenes that gained the talker went from 0.2322, 0.1921 and 0.1788 to 0.1715, 0.1554 and 0.1421; the other seven are unchanged. One talker (short-7) is still not found.
- 72 of the 240 settings pass, at f 0.2 to 0.6 and d 0.80 to 1.10; 38 of them find 9 talkers, none finds 10. The weighted variant ties the plain one at the pick (same counts, plain wins the tie-break). Settings with f 1.0 fail rule 4 (new speakers on AMI meetings); at d 0.68 almost nothing is regrouped.

What this does not show:

- **The AMI guard was not exercised at the pick.** It made no new speaker on any AMI dev meeting, so rule 4 held trivially. Part of the reason may be the under-counted speech estimate on meetings above 1000 embeddings (see above): on a long real meeting the same `m` makes a new speaker less readily than on a 300 s scene.
- **The baseline already finds 6 of the 10 synthetic talkers,** so these scenes are easier than the private call that raised the question. The gain is 3 talkers on clean read speech.
- Ten talkers and one seed per scene: a weak signal (see "How far this can be trusted").

Per the rule, the next step is a production task for the owner to agree: the per-model setting with its source, a re-run of `cargo xtask accept m3` and `accept m6`, and, before it ships, a look at the private recording that raised the question.

## The owner's calls (T12.73)

Written and committed before the first run on the owner's data. Evaluation only: nothing shipped changes.

**Data.** A copy of the owner's `earshot.db`, made into `fixtures/private/real-meetings/<run>/` (git-ignored) by the `store_parts/copy.rs` rule and opened read-only; the recordings are read in place as files and never written. The call that raised the question is named only in a private settings file outside the repo (`%USERPROFILE%\.earshot-private\t1273.toml`, key `call_title`); the tool refuses unless exactly one live meeting has that title. The other tracks are the reference (far-end) track of every other live meeting whose reference recording is still on disk. Track labels are `c` for the call and `t1`, `t2`, ... for the others, in start order; no id, title or name is printed.

**Pipeline.** As in "The pipeline" above: the production final-pass diarisation on the CPU of each stored reference track, model outputs recorded once and replayed, the replayed baseline equal to the direct run. Two settings only: the baseline and T12.66's pick (f 0.60, d 0.80, m 8 s, plain). Nothing is chosen on this data.

**Measures**, per track:
- speakers found at the baseline and at the pick;
- new speakers, each with its speech in seconds;
- speech moved from each baseline speaker to a new one;
- kept speakers gaining, which must be 0 (the tool fails otherwise).

Each new speaker's turns (start and end times only) go into a file inside the run folder, so the owner can listen to them; nothing else of a turn is written.

**Rule.**
1. **The call:** at least one new speaker appears on track `c`, and the owner, listening to its turns (all of them, or the first 10 when there are more), says most of its speech is the talker who spoke only in short turns.
2. **The other tracks:** no new speaker appears, or the owner, listening the same way, says each new speaker is a real voice today's clustering missed.
3. Both hold: T12.74 may start once the owner agrees. Otherwise T12.74 does not start, the result is recorded here and the question is parked.

**Limits.** One call is the case that raised the question, so a pass says the pick fixes the case seen, not that it generalises. The owner's judgement by ear is the only truth on these tracks. The other tracks are only those whose recordings retention has kept.

### Result (T12.73)

Not run yet.
