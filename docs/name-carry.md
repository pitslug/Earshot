# A forgotten voice keeps its name over a re-transcription (T14.54)

Status: revised after spec review 1 (2026-11-27), not yet re-reviewed. Nothing here is built. The owner's decisions are in section 7. Sections 1 to 6 are the design and the measurement rule; section 5 is fixed before any measurement and is not changed after the first run (results go in section 8 only).

## 1. The question

The owner's rule (2026-11-27, PLAN.md T14.54, docs/sync-spec.md 7A.2 P5 and 7A.12 point 7): forgetting a person's voice never takes them out of an existing transcript; it only stops them being recognised in future.

Today a forget keeps every speaker row's name, person and ignore flag (P5's invariant), but it drops that identity's entries from every `speaker-identities/<meeting>` record (`earshot_store::identities::forget_identity`, through `forget_identity_biometrics`), and a re-transcription carries a speaker row only through its entry (`earshot_runtime::identities::match_previous`). So re-transcribing one of that person's meetings afterwards tombstones the row, and their lines become "Speaker N".

This task carries such a row by line timing instead of voice. It stores and enrols nothing.

### 1.1 Where the rule runs (the real trigger scope)

`speaker-identities/<meeting>` is device-local (`local_state`, never synced). The rule keys on an entry being absent, so it runs for every row this computer's record has no usable entry for, not only after a forget. A re-transcription runs only where the meeting's audio is (audio rows and files never sync), but on that computer the rows without entries are:

- rows of a person whose voice was forgotten (the motivating case, on any computer: the forget drops entries everywhere it runs);
- rows this computer's final pass did not write: a speaker made by a user edit, here or on another computer of the vault (a new "Speaker N" from `edit::move_line`, `edit::new_speaker_role`), then named;
- rows whose entry another computer's edit changed in meaning only there: a mark made on another computer (`mark_me` writes `user_edited` into that computer's entry only), a link or rename made elsewhere (here the entry exists and reads as edited by its stamp, so voice carrying applies, as today);
- meetings whose last pass ran before T6.25 (no record), records this build cannot read, entries of another embedding model;
- a row the one-off sweep left without an entry and that was later revived (7A.12a).

The rule is judged and tested for all of these (section 6, tests 13 and 14), not only for a forget.

## 2. What the code does today

All in the final pass's write transaction, `crates/earshot-runtime/src/post/mod.rs` step 4 (about lines 1724 to 2215):

- `replaced` are the earlier live Final segment rows on the processed tracks whose text the user did not edit; `edited` are those with `text_edited_at` (a typed correction, and since T12.90 every moved or split line). Edited lines are kept with their speaker; new words under their span are dropped before labelling (`drop_words_under_edits`).
- `ours(row)` is an earlier live speaker row this run replaces (on a full re-run, every one not kept for an unprocessed track).
- `previous` is the record's entries. Voice candidates are the entries whose row is ours, not Me (except a voice marked as the user in the room, T9.12: `in_room_kinds`, the entry's `user_edited`, and this run named no Me), and whose centroid is of this run's embedding model. Another model's entries raise `PostWarning::SpeakerNamesNotCarried`; so does an unreadable record.
- `name_speakers` names new voices by voiceprint (`naming.person`, `naming.name`) before any carrying.
- `match_previous` pairs new voices (worded, not Me) with candidates one to one on the same track, cosine at or above the same-voice threshold, edited entries first.
- A continued row whose entry reads as edited keeps its name, person and synced `ignored` flag; only the role follows this run's policy (Me stays Me); a person removed here is unlinked as P3 writes it (7A.3); its new entry gets this run's centroid. A continued row that does not read as edited is renamed by this run (its voiceprint naming or a new "Speaker N").
- Every other ours row that is not continued, not the reused Me row and not kept for its edited lines is tombstoned.

So a row with no usable entry can never be continued, and its name goes.

## 3. The rule

### 3.1 Which rows

A row is **eligible to be carried by lines** only if every one of these holds:

1. It is an earlier live speaker row that this run replaces (`ours`).
2. It is **not a voice candidate**: no entry in a readable record names it, or the record is unreadable, or its entry's centroid is of another embedding model. A row that is a voice candidate is decided by voice alone, as today, whether or not its voice matches (section 3.5). The in-room Me row is the one exception (section 3.6).
3. It is **named**: it links to a person (`person_id` is set), or it is ignored by the one rule (`counts_as_ignored(row, false)`: there is no usable entry, so no legacy flag), or its name is not of the automatic form (`speaker_number(&display_name)` is `None`). A plain unlinked "Speaker N" is not carried: the run numbers its voices afresh, as today.
4. It is not the Me row, except in the room (section 3.6).

Without an entry the pass's own naming (a voiceprint match from an earlier run) cannot be told from the user's link, so both count (the owner's decision 4).

### 3.2 Groups of old rows

Eligible rows are pooled before agreement, so one person on two rows that the re-run merges keeps the name:

- rows linked to the same person (`person_id`, resolved through `merged_into` with `resolve_person`) form one group;
- unlinked rows with the same non-automatic name (exact text) form one group;
- every other eligible row (an ignored "Speaker N") is a group of its own;
- groups are per track.

A carried group keeps the id of its row with the most old speech on that track (ties: the lower id). Its other rows are tombstoned like any replaced row, and their lines go to the voice with the rest. The in-room Me row is always a group of its own.

### 3.3 Speech and agreement

All times are `MeetingTime` milliseconds, intervals half-open `[start, end)`. Word times on an earlier row may have been pulled from another computer, so they are read defensively:

- a word is clamped to its line, `[max(w.start, line.start), min(w.end, line.end))`;
- an interval whose end is not after its start is empty (an inverted, zero-length or wholly clamped-away word adds nothing);
- durations are computed with saturating arithmetic on `u64`, never an unsigned subtraction that can wrap;
- a line with no non-empty word interval counts as its own `[start, end)`, or nothing if that is empty too.

Then:

- **The old speech** of group G on track t, `O(G, t)`, is the union of the speech of G's `replaced` lines on t. Edited lines (typed, moved or split) are not in it: they stay where the user put them, with the speaker the user gave them.
- **The new speech** of a new voice k, `N(k)`, is the union of the speech of every labelled part this run gives k, on k's track (after `drop_words_under_edits`, so nothing under an edited line counts). Parts with no speaker (unassigned, T12.65) belong to no voice.
- **Overlap** `ov(G, k) = |O(G, t) ∩ N(k)|`, with t the voice's track.
- **Agreement** `a(G, k) = min(ov / |O(G, t)|, ov / |N(k)|)`, and 0 when either is empty. Both shares must be large: most of the group's old speech went to k, and most of k's speech was the group's. A name never spreads onto speech that was mostly someone else's (the owner's decision 3).

### 3.4 Matching

`a(G, k) >= LINE_CARRY_MIN_SHARE` is a possible pair. Pairs are taken greedily by agreement, highest first, ties by overlap (longer first), then new voice index, then group index; each group and each voice in at most one pair. Above 0.5 a group can agree that strongly with only one voice and a voice with only one group, except where neighbouring word times overlap by a few milliseconds; the greedy order settles those deterministically.

The matcher is pure, in `earshot_runtime::identities` beside `match_previous`:

```rust
pub fn match_by_lines(
    old: &[LineGroup<'_>],            // track, old speech intervals
    new: &[Option<LineVoice<'_>>],    // `None`: not matchable (section 3.5)
    min_share: f32,
) -> Vec<Option<usize>>
```

with `line_agreement` (the measure of 3.3, including the clamping) public for the evaluation tool. It takes no embedding: its signature is the proof that the path reads no voice.

### 3.5 Order in step 4

1. **Voice naming**: `name_speakers` as today.
2. **Voice carrying**: `match_previous` exactly as today, over the voice candidates.
3. **Line carrying**: `match_by_lines` over the eligible groups and the new voices that are worded, not continued in step 2, and **not named by this run** (not Me, and no voiceprint naming: `naming.person` is `None`). A voice this run recognises keeps that naming even where an earlier name P would have been carried by lines (the owner's decision 5): timing only fills in where nothing recognises the voice. P's row is then tombstoned like any uncarried row.
4. Groups matched in step 3 are not tombstoned (their kept row); every other row behaves as today.

**`SpeakerNamesNotCarried`** keeps its copy and its reasons (another model; an unreadable record), and fires only when at least one row that step 3 could have carried was carried by neither step:

- another model: an eligible named row whose entry is of another model, not carried;
- an unreadable record: any eligible named ours row of that meeting, not carried (an unreadable record names no row, so every named ours row counts).

When step 3 carries every such row, no warning fires.

### 3.6 Me

- **In a call** the Me row is never carried by lines (the owner's decision 1: rooms only). "Me" still comes only from `two_track`'s single mic voice, the user's voiceprint, or a mark in the room.
- **In the room**, the Me row is carried by lines when this run named no Me and the row is not a voice candidate. That covers the user's forgotten voice (`my_voice::forget_my_voice` drops a mark's entry, and a voiceprint-named Me row never had one) and a mark made on another computer (the entry here does not hold `user_edited`, so today's voice exception does not take it). It is matched only against room voices not named by this run (step 3). It stays the Me row, linked to the user; its diarisation speaker stays a room voice, so nothing is enrolled (`enroll_me` follows the diarisation's Me, which is none); no `voiceprint-me/<meeting>` record is written, so it reads as a mark and "This isn't me" is allowed. `MeNotFoundAgain` then does not fire, because a Me row is present.
- This applies whether or not the user has a live voiceprint: a voiceprint that recognises the user names a Me (step 1) and takes the Me row as today; one that does not recognise them no longer takes them out of a room meeting whose lines still agree.
- A non-Me row linked to a person that resolves to the user (`edit::is_users_row`) is carried like any linked row; its role follows the policy and never becomes Me.

### 3.7 What a carried row gets

It goes through the same branch as a continued edited row:

- id, name, person and synced `ignored` flag kept; the role follows this run's policy for the voice's track (Me stays Me);
- a person that does not resolve here is unlinked as P3 writes it ("Speaker N", no link, not ignored; 7A.3);
- the row is written only when its role or the P3 rewrite changes it (`meta.touch`), so an unchanged row is not re-stamped;
- any carried row named "Speaker N" (an ignored voice, or a linked row so named) raises `next_unknown` to its number, so the number is not given to a new voice;
- its lines are the new Final segments of the voice it continues.

It gets **no identity entry** (section 3.9). Save on it answers `Remembered::NoStoredVoice` and keeps the link (the owner's decision 2).

### 3.8 Cases

- **A voice split differently.** The re-run splits G's old speech into k1 (70%) and k2 (30%): carried onto k1 if 0.7 reaches the minimum; k2 is a new voice.
- **Voices merged differently.** One person on two rows, merged by the re-run: pooled (3.2), so carried. Two different people merged: precision is low, nothing is carried and the name is lost rather than spread (decision 3).
- **Two groups wanting one voice.** The greedy order gives it to the higher agreement; the other group is tombstoned.
- **Labels swapped.** Lines, not labels, decide.
- **A voice recognised as someone else.** This run names it Q by voiceprint: Q wins (decision 5), P's row goes.
- **A row whose lines were all moved or split by the user.** Moved and split lines are edited lines and stay with the speaker they were moved to. A row left with no lines has nothing to carry and is tombstoned on a full re-run, as today. A row with only edited lines is kept for them (T12.16) and not carried onto a voice.
- **A row with lines on two tracks** (a cross-track merge the user made, which a re-transcription undoes): agreement is per track, and the group is carried onto at most one voice.
- **A partial re-run.** Only rows the run replaces take part (`ours`).
- **A person removed here.** Unlinked as P3 writes it, with no entry to drop.
- **The next re-run.** The carried row still has no entry, so it is carried by lines again.

### 3.9 Nothing biometric

This path reads no embedding and writes none: no identity entry for a row carried by lines (it would hold a centroid of a voice the user asked to forget, linked through the row to that person), no voiceprint, no voice sample, no enrolment, no new device-local record, field or key. The new voices this run does not carry by lines get their entries exactly as today.

### 3.10 Sync

A carried row is an ordinary speaker row write by the final pass: unchanged when its role is unchanged (nothing pushed), touched otherwise. No synced field, entity, envelope format (`ENVELOPE_FORMAT` stays) or relay rule changes. Other computers are affected only through rows, as by any re-transcription: the computer that re-transcribes carries rows that other computers made or named (section 1.1), and its kept or tombstoned speaker rows and new Final segments then sync as usual (`pass_run`, docs/sync-spec.md section 7 rule 4, unchanged). Each computer's own identity record is untouched by another's carry. `summary-names` records nothing, because no name changes.

## 4. Where the code goes

- `earshot_runtime::identities`: `line_agreement`, `match_by_lines`, `LineGroup`, `LineVoice`, the grouping helper, and the constant `LINE_CARRY_MIN_SHARE`, whose doc cites this file and section 8.
- `earshot_runtime::post`, step 4: eligibility (3.1, 3.6), grouping, the third step, carried rows through the edited branch with no entry pushed, the narrowed warning. The module docs (step 4, "Carrying speakers across runs") gain a paragraph.
- Nothing in earshot-store, earshot-core, the desktop or the UI changes.
- AGENTS.md gains one bullet when built (by the coordinator).

## 5. The minimum share: the rule, written before measuring

Written before any measurement of this task and before the tool. Data, comparisons, measures, grid and rule are not changed after the first run.

### 5.1 The data

Dev data only, the inputs of T12.85 (docs/far-speakers/training-cap.md, "The data"): no AMI test-series meeting (ES2004, ES2014, IS1009, TS3003, TS3007), no LibriSpeech test-clean, no acceptance input, no private recording.

- **A.** The fifteen AMI dev meetings of fixture set `ami-dev` (ES2003, ES2011, IS1008, TS3004, TS3006, meetings a, b and c), each the whole `Mix-Headset` recording as one in-room track, with T12.9's word-time reference and UEM (`cluster_sweep_parts/ami.rs`).
- **L.** The 16 dev-clean long scenes of `cluster_sweep` (T12.9, every speaker in exactly one scene), with their turn reference, each as one in-room track.

Two-track calls are not measured: the rule works per track, the far-end track of a call is diarised and labelled by the same code as a room track, and the mic track's voices in a call are mostly Me (never carried by lines there). A dev set of multi-voice far-end tracks with a reference does not exist in the repo; the per-track measurement stands in for it, and tests 3 to 7 run on two-track meetings too.

### 5.2 The pipeline and comparisons

Each item is run through the production final pass (`PostProcessor`, CPU) on a scratch store under `target/name-carry/` (swept at start and end), once as the **old run** under a variant setting, then:

1. the old run's Final lines are snapshotted (by row, with word times) before anything else, because the re-run tombstones them;
2. every speaker row of the old run that is not Me is linked through the production edit (`edit::link_speaker`) to an invented person ("Person 1", "Person 2", ...), rows with the same owner (5.3) sharing one person, so over-split speakers exercise grouping (3.2);
3. each of those persons' voices is forgotten through the production forget (`Store::forget_voiceprints`), which drops the entries;
4. the item is run again through the production final pass as the **re-run**, with `PostConfig::default()` and the default transcription model (the shipped build).

Comparisons (old run's variant against the shipped defaults), each counted once, in that one direction:

- **T-.** Clustering threshold 0.60 (`PostConfig::clustering_thresholds`).
- **T+.** Clustering threshold 0.76.
- **R.** Short-turn regrouping off, through a named no-op `Regroup` built for the tool (evaluation only, pinned out of shipped code by a scan test).
- **M.** Transcription model parakeet-tdt-110m (model set `ci`) against parakeet-tdt-0.6b-v2.

That is 4 comparisons on 31 items, 124 in all. A comparison whose two runs give identical voices and lines (expected for R on the AMI meetings, where T12.85 found regrouping changes nothing) is not evidence: it is reported by count and left out of every measure. The number of distinct, non-identical comparisons is reported first.

Model outputs (transcription, segmentation, embeddings) are recorded by input hash and replayed, as `cluster_sweep` does; an unrecorded output fails the run. The recording transcription wrapper forwards `Transcriber::may_skip_speech` (and every other trait method) to the wrapped model. `cluster_sweep`'s scene builder moves into `cluster_sweep_parts` so both tools include it by path.

The re-run's carry decisions do not change which voices it finds or which lines each gets, only which row ids they reuse. So the tool reads the snapshot of the old lines and the re-run's Final lines grouped by row (one row per worded voice), groups the old rows as 3.2 does, computes every `line_agreement` with the production function and replays the production `match_by_lines` at each grid value, excluding voices the re-run named by voiceprint (there are none: the persons' voices were forgotten, and AMI and scene voices have no other voiceprint). Consistency check: at the committed constant, the replayed carries must equal the rows the re-run actually kept, on every comparison, or the run fails.

### 5.3 Truth

Each word's speech (3.3, clamped) is given to the reference speakers by time overlap (AMI: the word-time reference as turns, as `cluster_sweep_parts/ami.rs` builds them; L: the scene's turns). The **owner** of an old group or a new voice is the reference speaker owning the most of its speech; a tie has no owner. A carry G -> k is **right** when both have an owner and it is the same, else **wrong**. A group has a **counterpart** when some new voice has its owner.

### 5.4 The measures

For each grid value v, pooled over the non-identical comparisons, and per comparison kind:

- W(v): wrong carries;
- C(v): right carries;
- K(v) = C(v) / (groups with a counterpart).

Also reported, selecting nothing: the agreement of every right and every wrong pair (counts in 0.05 bins); seconds of speech under a wrong carry; and the **crosstalk share** of every wrong pair and of every pair within 0.10 of the chosen value, right or wrong: the share of its overlap during which the reference has two or more speakers active.

Reported grid: 0.30, 0.35, ..., 0.95. Selectable grid: 0.55, 0.60, ..., 0.95. Values below 0.55 are informational: they exist to show where wrong carries begin.

### 5.5 The rule

1. **A non-zero baseline is required.** If W(v) = 0 at every value of the reported grid, the data never showed a wrong carry, so it cannot place the constant: stop and ask the owner. (AGENTS.md: a criterion that needs a count to fall first requires a non-zero baseline.)
2. v0 is the smallest selectable value v such that W(w) = 0 for v and every larger grid value.
3. If no v0 exists, or v0 > 0.85 (no room for the margin), stop and ask the owner; nothing is carried by lines until they decide.
4. `LINE_CARRY_MIN_SHARE` = v0 + 0.10 (the margin from the point where wrong names stop; a judgement, two grid steps, chosen before measuring).
5. If pooled K at that value is below 0.5, stop and ask the owner before building: the rule would keep fewer than half the names it could. This decides whether to build, never the value.

### 5.6 The tool and its report

A new evaluation-only example, `crates/earshot-runtime/examples/name_carry.rs`, including `cluster_sweep_parts/ami.rs` and the moved scene builder by path. Counts and times only: no word text is printed or written, and the invented person names are the only names it writes. Its report holds the commit (`-dirty` when dirty), a UTC start time, the host load, and every shipped constant it read (the clustering threshold, the regroup, `LINE_CARRY_MIN_SHARE`). It is run from a clean tree after the code commit.

Reproduction (fixtures and models first):

```powershell
cargo xtask fetch-fixtures --set ami-dev
```

```powershell
cargo xtask fetch-fixtures --set librispeech
```

```powershell
cargo xtask fetch-models --set ci
```

```powershell
cargo run --release -p earshot-runtime --example name_carry -- . --models <dir> --out docs/name-carry/dev.json
```

`docs/name-carry/dev.json` is committed unmodified. Order of work inside the task: the code with the constant above 1.0 (carries nothing), committed on the lane; the tool run from that clean commit (the sweep replays the matcher at every grid value, so it does not depend on the constant); the constant set by the rule; the tool run once more, so its consistency check runs at the chosen value; the tests of section 6 run against that constant. No commit on `main` holds a value the report does not support.

## 6. Tests

Names carry the module word so the task's accept filters select them. Every bound derives from `LINE_CARRY_MIN_SHARE`, never a literal; scenes are built with agreements just above and just below it.

Pure (`earshot-runtime`, `identities`):

1. `post_name_carry_line_agreement_*`: union of word intervals, untimed words, the line-span fallback, other tracks ignored, min of the two shares, empty sides give 0; pulled-time validation: an inverted word, a word outside its line, a line with `end < start`, and times near `u64::MAX` give no panic and no wrap.
2. `post_name_carry_match_by_lines_*`: one to one, greedy order and ties, nothing below the minimum, a `None` voice never taken; grouping by person (through `merged_into`), by name, and alone.

Final pass (`earshot-runtime`, `post`, fake diarizer and transcriber as the existing post tests, through FLAC):

3. **Forget, re-run, name kept**: a row linked to P, P's voice forgotten through `Store::forget_voiceprints`, re-run with the same voices: row id, name and person kept. Control: lines moved so the agreement is just below the minimum: the row tombstoned, a new "Speaker N".
4. **Voices moved**: two people merged (no carry, no name on the merged voice); a 70/30 split (carried onto the larger part only); swapped labels (the name follows the lines); one person on two rows merged by the re-run (carried, one row kept, the other tombstoned). Each with an unmoved positive control.
5. **Two groups, one voice**: the higher agreement wins, the other is tombstoned.
6. **Edited, moved and split lines**: a row whose lines were all moved away is not carried; a row with some edited and some plain lines is carried by the plain ones; new words under edited lines count for nothing.
7. **Voice first, recognition first**: a voice candidate whose voice does not match is not carried by lines even when its lines agree fully (today's behaviour pinned); a voice this run names Q by a fresh voiceprint keeps Q over a line carry of P, and P's row is tombstoned. Control: the same scene with no voiceprint for Q carries P.
8. **Me**: in the room, the user's voice forgotten and a re-run naming no Me: the Me row kept as Me, no enrolment, no `voiceprint-me` record, `unmark_me` allowed, no `MeNotFoundAgain`. Controls: a run that names Me by voiceprint takes the Me row as today; a call naming no Me does not carry the Me row (as today).
9. **Ignored and plain**: an ignored row with no entry is carried with its flag, unlinked; a plain "Speaker N" with no entry is not carried; a carried "Speaker 7" raises `next_unknown` so no new voice is called "Speaker 7".
10. **A person removed here**: a carried row linked to a tombstoned person is written as P3 writes it. Control: the same row linked to a live person keeps its link.
11. **Nothing biometric**: after a re-run that carried a row by lines, the identity record holds no entry for that row, and the voiceprint and voice sample tables are byte for byte as before. Positive control in the same meeting: a row carried by voice does get an entry.
12. **No re-stamp**: a carried row whose role is unchanged keeps its `updated_at` and queues nothing for sync. Control: a carried row whose role changes (a two-track policy change) is re-stamped and queued.
13. **Rows made elsewhere** (1.1): a speaker made by `move_line` here and named, and one pulled from another computer through `LocalRelay` and named there, both with no entry here: carried by lines on re-transcription.
14. **A mark made on another computer**: the in-room Me row marked on computer B, pulled to A, whose entry for it lacks `user_edited`; A re-transcribes and names no Me: the Me row kept by lines.
15. **The warning**: another model's entries or an unreadable record with every named row carried by lines: no `SpeakerNamesNotCarried`; with one named row uncarried: the warning, its copy unchanged.

Store (`earshot-store`, `identities`): the existing forget tests stay; one more pins that `forget_voiceprints` changes no speaker row (name, `person_id`, `ignored`) in a meeting whose entries it drops, with the dropped entry as the positive control.

Desktop (`earshot-desktop --bin earshot`, `detail`): forget a voice, re-transcribe through the job, and the meeting detail shows the same name on the same lines; control: the identity record intact gives the same detail by voice.

A scan test pins that the tool's no-op `Regroup` is used by no shipped code.

## 7. The owner's decisions (2026-11-27)

1. **The user's own Me row in the room is carried by lines; calls are not** (3.6).
2. **Save on a voice carried by lines answers "no stored voice"**, accepted: no identity entry is stored for it (3.7, 3.9).
3. **A merged voice drops the name** rather than spreading it onto someone else's speech (3.3, 3.8).
4. **Rows the pass named by voiceprint and now without an entry are carried too** (3.1).
5. **A voiceprint naming in this run wins over a line carry**: when this run names a voice by voiceprint (person Q), that naming wins over a timing carry of an earlier name P; timing only fills in where nothing recognises the voice (3.5 step 3).

## 8. Result

Empty until the measurement of section 5 runs.

## 9. Revisions

- **2026-11-27, first draft** (T14.54). The design and the measurement rule, before any spec review or measurement.
- **2026-11-27, after spec review 1** (blocked on B1, eight advisories) and the owner's decisions (section 7).
  - B1: the reported grid now runs from 0.30, below the selectable one, and the rule stops when no wrong carry appears anywhere in it (non-zero baseline, 5.5 step 1).
  - A1 and decision 5: step 3 excludes every voice this run named, by voiceprint as well as Me.
  - A2: condition S dropped; each comparison counted once, in one direction (variant against shipped defaults); identical-run comparisons excluded and counted; 124 comparisons before exclusions; 5.5 step 5 judged on the pooled comparisons.
  - A3: old rows pooled by person, then by name, before agreement (3.2); the tool links rows of one owner to a shared person.
  - A4: the real trigger scope stated (1.1); 3.6 widened to a mark made on another computer and no longer conditioned on the user's voiceprint; 3.10 corrected; tests 13 and 14.
  - A5: pulled word times clamped and validated (3.3); test 1.
  - A6: the tool's build steps (scene builder moved, transcription recorded with `may_skip_speech` forwarded, a named no-op `Regroup`, the old lines snapshotted before the re-run).
  - A7: `next_unknown` for any carried "Speaker N"; the narrowed warning defined for an unreadable record; controls for tests 10 and 12; test 7's fresh voiceprint naming; two-track calls not measured, with the reason (5.1).
  - A8: crosstalk share reported for wrong and near-threshold pairs.
  - The reviewer's lessons, verbatim: "`speaker-identities/<meeting>` is device-local, so any final-pass rule keyed on an entry being absent runs on every computer that did not run the earlier pass. Judge and test it for that case, not only the case that motivated it." and "A sweep whose conditions are replayed both ways round counts each unordered comparison once. A condition that replays identical runs is not evidence."
