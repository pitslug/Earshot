# Voiceprint matches on meeting audio (T12.67)

Written and committed before any measurement of this task, before the tool that measures it and before the test meetings were downloaded. The data, measures, grids and rule below are not changed after the first run; results go in the Result section only.

## The question

The final pass names each voice of a track on its own (`post::name_speakers`, `voiceprint::match_embedding_with_report` per speaker): the stored person with the highest cosine score wins if that score reaches `voiceprint::CAMPPLUS_DEV_CLEAN_THRESHOLD` (0.462). That threshold was calibrated on clean read speech only (dev-clean, 10 s enrolment, 2.5 s clips), never on meeting audio or on a voiceprint enrolled from one meeting's diarised centroid, which is how the app enrols (`edit::save_voice`). Nothing stops one person being given to two voices of a track. This measures how often the shipped rule names the wrong person on real meeting audio, and judges three candidates.

Known before writing this: the finding in PLAN.md T12.67 (one private call, read by the owner's session: not data here and not re-read), the committed `vp.*` values of `accept m3` (`vp.top1` 100%, `vp.false_accept` 2.5%), and the five AMI dev "a" meetings' diarisations at 0.68 from T12.9. Nothing about voiceprint scores on AMI audio has been measured.

## The data

Dev data only: no AMI test-series meeting (ES2004, ES2014, IS1009, TS3003, TS3007), no LibriSpeech test-clean in selection, no private recording.

- **Enrolment meetings**: the first meeting of each AMI dev series, ES2003a, ES2011a, IS1008a, TS3004a, TS3006a: the whole `Mix-Headset` recording of fixture set `ami-dev`.
- **Test meetings**: the second and third meetings of the same series, ES2003b, ES2003c, ES2011b, ES2011c, IS1008b, IS1008c, TS3004b, TS3004c, TS3006b, TS3006c: their `Mix-Headset` recordings, added to set `ami-dev` in `fixtures/manifest.toml` (same host and licence as the "a" meetings, CC BY 4.0, size and SHA-256 pinned) and fetched only through `cargo xtask fetch-fixtures`.
- **Who is who**: a speaker's identity across meetings is the `global_name` of `corpusResources/meetings.xml` for that meeting's `nxt_agent` letter (AMI manual annotations v1.6.2). Reference speech per letter is T12.9's (`ami-dev-calibration.md`: the words files, non-punctuation words with end > start, one speaker's intervals with gaps under 0.5 s joined), scored region from the first to the last reference word.

## The pipeline

The production final pass on the CPU: `load_diarisation(models, Accelerator::Cpu)`, `PostConfig::default().diarization` with the committed clustering threshold and unassigned floor, each meeting diarised from time 0 at 16 kHz as the FLAC writer stores it.

- **Owner** of a found speaker: the true speaker whose reference speech it overlaps the longest inside the scored region (no collar; ties to the lower name). A found speaker that overlaps no reference speech has no owner.
- **Home** of a true speaker: the found speaker overlapping its reference speech the longest.

**Enrolment**, as the app enrols one meeting's voices: in each enrolment meeting, every true speaker whose home is owned by that same true speaker becomes one person with one voiceprint, the home's centroid. Other true speakers are not enrolled (counted). Enrolment is done through `voiceprint::enroll` into an in-memory repository, so scores are `Voiceprint::score` exactly as the final pass reads them.

**Trials**: every found speaker of every test meeting, with its centroid. Two conditions:

- **Known**: every enrolled person is stored. A trial is *correct* when it is named its owner; *wrong* when it is named anyone else, or anyone at all while its owner is not enrolled or it has no owner; a *miss* when it is not named while its owner is enrolled; a *correct reject* otherwise.
- **Unknown**: the persons of the test meeting's own series are removed, so nobody in the meeting is stored. Any name is a *false accept*.

Rates over all trials of a condition: FA = false accepts / trials (unknown), W = wrong / trials (known), M = misses / trials whose owner is enrolled (known). **Two-voices events**: in the known condition, the (test meeting, person) pairs named for two or more found speakers, split into *split events* (all those found speakers have the same owner: one voice split in two) and *mixed events* (different owners).

## The candidates

Each judged alone against the shipped rule (threshold 0.462, best person wins, each voice on its own).

- **A. A threshold calibrated on this data**: thresholds 0.40, 0.42, ..., 0.80, plus 0.462 itself.
- **B. A margin**: at threshold 0.462, a voice is named only if its best person's score exceeds the runner-up person's by at least m (no runner-up: no margin needed). m = 0.00, 0.02, ..., 0.20.
- **C. Two voices, one person**: at threshold 0.462 with no margin, when two or more voices of one track (one meeting here) are named the same person, the highest-scoring keeps the name and each other loses it, unless its centroid's cosine to the keeper's centroid is at least the threshold (a split of one voice keeps both names).

## The rule

The **false-accept requirement**: FA <= 0.05 and W <= 0.05 (the bound of `vp.false_accept`, applied to meeting audio).
The **miss cost**: M at the candidate is at most M(shipped) + 0.10.

1. If the shipped rule meets the false-accept requirement, A and B pick nothing (0.462, m = 0). Otherwise:
   - A picks the lowest grid threshold above 0.462 that meets the requirement within the miss cost.
   - B picks the smallest margin that meets the requirement within the miss cost.
   - If none does, that candidate picks nothing.
2. C passes if, against the shipped rule: mixed events fall, the correct names it removes are no more than the wrong names it removes (each lost right name pays for at least one removed wrong name), and it stays within the miss cost. A data set with no mixed event cannot show that C helps, and C then does not pass.
3. Every pick must also leave `vp.top1` and `vp.false_accept` passing with their thresholds unchanged (`cargo xtask accept m3`, which runs the test-clean protocol; that is a pass check on the pick, never a selection input).
4. What ships: C if it passes; B's margin if it picks one (if both, the combination is measured and must meet the requirement within the miss cost, else only C ships). A threshold pick is never shipped by this task: it is a recommendation for the owner (AGENTS 6.2), reported with its numbers, `CAMPPLUS_DEV_CLEAN_THRESHOLD` unchanged. If nothing passes, nothing ships and the result goes to the owner.

## How far this can be trusted

- 20 AMI dev speakers, close-talking mixes, five enrolment meetings and ten test meetings: about forty known trials and forty unknown ones. A rate of 0.05 is two trials. Small numbers decide here.
- Enrolment from one diarised centroid per speaker, as the app does; the app can also add centroids from later meetings, which this does not.
- The unknown condition's impostors are other AMI dev participants recorded in the same rooms and setup, which may score higher than strangers would.

## Result

Everything above was committed (440b59d) before the test meetings were fetched or the tool written. Raw report: `voiceprint-meetings.json` (the tool's output). One run on the CPU, CAM++ `campplus-en-voxceleb@357a834f702b`, clustering threshold 0.68, unassigned floor -1. 19 of the 20 dev speakers were enrolled (one had no found speaker in its first meeting that was both its home and owned by it, or too little speech for a voiceprint); 40 trials (every found speaker of the ten test meetings). The tool's naming at the shipped threshold equalled `match_embedding` on every trial.

**The shipped rule fails the false-accept requirement on meeting audio.** With nobody of the meeting stored, a voice was still named someone 8 times in 40 (FA 0.20, against 0.025 on dev-clean clips); with everyone stored, 2 of 40 voices got another person's name (W 0.05) and every voice whose person was stored got its own name (M 0 of 38). The known condition also has 2 mixed events (two voices of one meeting given one person with different owners) and no split.

| rule | FA (unknown) | W (known) | M (known) | correct | mixed events |
|---|---|---|---|---|---|
| shipped, 0.462 | 0.200 (8/40) | 0.050 (2/40) | 0 (0/38) | 38 | 2 |
| A 0.50 | 0.150 | 0.050 | 0 | 38 | 2 |
| A 0.52 | 0.075 | 0.050 | 0 | 38 | 2 |
| **A 0.56 (pick)** | 0.025 (1/40) | 0.050 (2/40) | 0 | 38 | 2 |
| A 0.58 | 0 | 0.025 | 0 | 38 | 1 |
| A 0.60-0.80 | 0 | 0 | 0 | 38 | 0 |
| B m 0.02 | 0.125 | 0.050 | 0 | 38 | 2 |
| B m 0.04 | 0.075 | 0.050 | 0 | 38 | 2 |
| B m 0.06 | 0.075 | 0.025 | 0 | 38 | 1 |
| **B m 0.08 (pick)** | 0.025 (1/40) | 0.025 (1/40) | 0 | 38 | 1 |
| B m 0.12-0.20 | 0 | 0 | 0 | 38 | 0 |
| C | 0.175 | 0.050 | 0 | 38 | 2 |

- **A picks 0.56**, the lowest grid threshold above 0.462 meeting the requirement within the miss cost. A threshold move is the owner's decision (AGENTS 6.2): it is reported here, not shipped; `CAMPPLUS_DEV_CLEAN_THRESHOLD` stays 0.462.
- **B picks a margin of 0.08** and ships (rule step 4): `voiceprint::CAMPPLUS_MEETING_MARGIN`, applied by `match_embedding_with_margin` in the final pass's naming and in `accept m3`'s `vp.*` protocol. Rule step 3 (`vp.top1`, `vp.false_accept` unchanged thresholds) is below.
- **C does not pass**: in the known condition it removes no name (the two voices of each mixed event score at least 0.462 against each other, so C reads them as one voice split in two), so mixed events do not fall. In the unknown condition it removes one false accept (0.175).
- Every candidate costs no miss here: all 38 known voices score far above any of these settings against their own person. The miss cost never binds on this data.

The margin only acts when two or more persons are stored: with one stored person there is no runner-up, and the threshold alone decides, as before. It never costs the user their own name: when the best match is the user's own identity at the threshold, the final pass keeps it however close the runner-up (review fix; `post_the_margin_never_costs_the_user_their_own_name` pins both ways).

How far this goes: forty voices, so one trial is 2.5 points; the margin's win over the shipped rule is 7 false accepts fewer in 40 in the unknown condition and 1 fewer wrong name in the known one. Close-talking mixes of one corpus; impostors are other AMI dev participants. No user is enrolled here, so the exemption for the user's own identity is pinned by tests only.

**Rule step 3 holds** with the margin in place (`cargo xtask accept m3`): `vp.top1` 100% (unchanged) and `vp.false_accept` 1.67% (2.5% before; 2 of 120 unknown clips accepted instead of 3), thresholds unchanged; the DER and count measurements are unchanged.

Reproduction: `vp_meetings` fails if the margin it picks differs from `CAMPPLUS_MEETING_MARGIN`, and the ignored test `vp_meetings_the_rule_still_picks_the_shipped_margin` runs the whole measurement (`cargo test --release -p earshot-runtime --example vp_meetings -- --ignored`). Both were added after this run, changing nothing the tool computes; a re-run gave the same report byte for byte.