# Voiceprint names on the owner's own meetings (T12.71)

T12.67 found that its rule picks a voiceprint threshold of 0.56 (shipped: 0.462) on AMI dev meeting audio. A threshold move is the owner's decision (AGENTS 6.2), and AMI is close-talking audio of one corpus. This readout asks the question AMI cannot: on the owner's own calls, does 0.56 remove a wrong name without costing a right one? It is evaluation only and ships nothing. This rule was committed before the tool first ran on the owner's data.

## Data

A copy of the owner's `earshot.db`, made by the tool into `fixtures/private/real-meetings/<run>/` (git-ignored) and opened read-only (`earshot_store::ReadOnlyStore`); the original is read once as a file copy and never opened. No audio is read and no model is run: every score comes from what the final pass already stored.

- **Voices:** each entry of `speaker-identities/<meeting>` (the final pass's centroid per speaker row) of a live meeting whose speaker row is live and whose centroid is valid, of the model the store's voiceprints use. Ignored voices are left out.
- **Confirmed:** the row is linked to a person and the entry reads as a user edit (`SpeakerIdentity::edited`) or a saved voice (`enrolled`): the owner said who it is. The person (resolved through `merged_into`) is the voice's owner.
- **Unconfirmed:** linked by the pass and never touched. Reported only: the owner may simply not have checked.
- **No name:** no person. Reported only: it may be a person never saved.

**Leave one out.** Saving a voice adds that meeting's unit centroid to the person's voiceprint (`edit::remember_voices`), so a saved voice would partly score against itself. For an entry with `enrolled`, its unit centroid is taken out of its owner's voiceprint of that model before the voice is scored (`count` minus one). A voice whose owner has no voiceprint left is *unscorable* and counted, not judged. A voice saved and later re-transcribed (a new centroid, `enrolled` reset) cannot be taken out: that limit is stated in the report, not corrected.

## Settings

- **S (shipped):** threshold 0.462 (`CAMPPLUS_DEV_CLEAN_THRESHOLD`), margin 0.08 (`CAMPPLUS_MEETING_MARGIN`).
- **T (T12.67's pick):** threshold 0.56, margin 0.08.

Each voice is named exactly as the final pass names one voice (`post::voice_match`, the production function: the margin, the exemption for the user's own identity, and nobody but a Me row named as the user).

## Measures

For a confirmed, scorable voice under a setting: *correct* (named its owner), *wrong* (named anyone else), *miss* (named nobody). Also each such voice's own score (against its owner, left out) in bands: below 0.462, 0.462 to 0.56, 0.56 and above. For unconfirmed and no-name voices: how many each setting names, and whether it names the pass's person.

## Rule (fixed before the first run)

1. **Enough evidence:** at least 10 confirmed, scorable voices. Fewer: the readout is inconclusive and the recommendation is to keep 0.462.
2. **No lost names:** no confirmed voice that S names correctly may be named otherwise (wrongly or not at all) by T.
3. **A gain:** wrong(T) must be less than wrong(S) on confirmed voices (at least one wrong name removed).
4. **Recommend 0.56** only if 1, 2 and 3 all hold. Otherwise keep 0.462. Unconfirmed and no-name counts never decide; they are reported beside the result.
5. Raising it, if recommended and agreed, is its own task: a meeting threshold separate from the dev-clean calibration (which `voiceprint_calibration` pins), with `accept m3` re-run.

Correction (2026-11-17, before the first run on the owner's data): step 2 first also required misses(T) to equal misses(S). A wrong name that T removes becomes a miss, so that clause made step 3 impossible to meet whenever it held; the tool's own rule test found it. The clause was removed; the per-voice check in step 2 was already there and is unchanged.

## Privacy

The tool prints counts and bands only. Its per-voice report (scores, labels `m<k>` / `v<k>`, no ids, names or text) is written inside the run folder and never committed. Errors never quote an id, a name or a value.

## Result

**First run (2026-11-17, desktop, debug build of `vp_readout` at 7c0a2df): not enough voices; keep 0.462.**

- 8 voices across the meetings with a stored speaker record (6 meetings had none; 1 ignored voice left out).
- Confirmed: 2, of which 1 scorable. Its own score is 1.00: its centroid is its person's whole voiceprint, which is the stated limit (saved, then re-transcribed, so the saved centroid could not be taken out). It is not independent evidence. The other confirmed voice's person has no voiceprint once its own enrolment is left out.
- Both settings name every voice the same way: confirmed correct 1 / 1, wrong 0, miss 0 at both.
- No voice of any kind scores between 0.462 and 0.56 against its best person: the best scores are 0.29, 0.37 and then 0.71 to 0.86 (and 1.00). On this data the threshold choice changes no name.
- 3 no-name voices score 0.82 to 0.85 against a stored person and are named under both settings; 2 more (0.71, 0.86) best match the user's identity and stay unnamed, as the final pass does for a voice that is not a Me row.
- Verdict by the rule: step 1 fails (1 confirmed, scorable voice; 10 needed). Re-run after more call voices are named.
