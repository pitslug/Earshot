# Every moment in the speaker grouping, with and without regrouping (T12.76)

Written and committed before any measurement of this task. The data, candidates, measures and rule below are not changed after the first run; results go in the Result section only. Measurement only: nothing shipped changes, whatever the rule picks.

## The question

The clustering (`earshot_core::diar::cluster`) builds its groups from at most 1000 embeddings (`ClusteringConfig::max_train_embeddings`, pyannote's `max_num_embeddings`); every other embedding is attached to the nearest final group afterwards. A long meeting has more: 1239 to 2592 on the AMI dev "a" meetings (T12.66). A voice that speaks little then has few embeddings in the sample, forms only a small cluster, and step 5 merges it into the most similar large cluster with no lower bound. T12.66's regrouping (step 6, parked as T12.74) turns such small clusters into new speakers, but its speech estimate counts only the training sample, so on a long meeting it makes a new speaker less readily.

The owner's rule (T12.75, 2026-11-18): two people merged into one voice is the costly error, because the user cannot split a voice; an extra voice costs one merge in the Speakers panel. So the cost below counts a merge as several extra voices.

Known before writing this: T12.9, T12.65 and T12.66's results and JSON (`ami-dev-calibration.md`, `unassigned-floor.md`, `short-turns.md`), T12.67's voiceprint trials on the AMI dev "b" and "c" meetings (40 found speakers in those ten meetings, no per-meeting merge counts), and a counts-only diagnosis of the owner's call that raised T12.75 (private, never data here): there, using every embedding gave one stored talker and one unstored talker voices of their own, and adding T12.66's regrouping separated most of the four far-end voices. Nothing about the candidates below has been measured on the data below.

## The data

Dev data only: no AMI test-series meeting (ES2004, ES2014, IS1009, TS3003, TS3007), no LibriSpeech test-clean, no private recording.

- **A.** All fifteen AMI dev meetings of fixture set `ami-dev`: ES2003, ES2011, IS1008, TS3004 and TS3006, meetings a, b and c, each the whole `Mix-Headset` recording, with T12.9's word-time reference and UEM (`cluster_sweep_parts/ami.rs`).
- **L.** The 16 dev-clean long scenes of `cluster_sweep` (T12.9), unchanged.
- **S.** The 10 dev-clean short-turn scenes of T12.66, unchanged.

L and S scenes hold fewer than 1000 embeddings each (T12.66: 357 to 619), so the training cap changes nothing there; they guard against regrouping harm. The cap acts only on A.

## The pipeline

The production final pass on the CPU (`load_diarisation(models, Accelerator::Cpu)`, `PostConfig::default().diarization`, clustering threshold 0.68, unassigned floor -1). Model outputs are recorded by input hash in a direct baseline run and replayed for every candidate; the replayed baseline must equal the direct run exactly on every item, or the tool fails.

## The candidates

- **B**, the baseline: today's pipeline (training cap 1000, no regrouping).
- **C**: the training cap raised so every embedding of the item takes part in the hierarchy (`max_train_embeddings` = usize::MAX); nothing else changes.
- **R**: T12.66's pick, regrouping at f 0.60, d 0.80, m 8 s, plain, cap 1000. Its new speakers are speakers of the diarisation like any other.
- **CR**: C and R together.

## The measures

Per item and candidate, inside the item's UEM, with T12.9's definitions (`cluster_sweep::speakers_of`): found speakers, **merges** (true speakers sharing a home with another, beyond the first), **lost** (true speakers overlapped by no found speaker), **extras** (found speakers that are no true speaker's home) and their speech, DER by `DerConfig::nist(AMI_COLLAR_PER_SIDE_S)`. On S scenes, **talker found**: some found speaker is owned by the scene's short-turn talker (T12.66's measure). Per item, the number of embeddings.

**Cost** of an item: 4 x (merges + lost) + extras. A merge or a lost speaker counts as four extra voices: an extra voice costs the user one merge, a merged voice costs a wrong name on every line of the hidden person and cannot be undone in the app. The weight 4 is a judgement, fixed here.

## The rule

A candidate **passes** when all of these hold against B:

1. **Cheaper:** its cost summed over every item (A, L and S) is lower than B's.
2. **No new merge on a real meeting:** on every AMI dev meeting, merges + lost are no more than B's on that meeting.
3. **Extra voices bounded:** every AMI dev meeting's DER is at most 0.02 above its DER at B (T12.9's per-meeting slack).
4. **No harm on the scenes:** S talkers found at least B's; on every L and S scene, merges + lost no more than B's.

If several pass, the rule picks the lowest summed cost, then the lowest summed merges + lost over A, then the lowest pooled A DER, then R before C before CR (fewest changes first; R is T12.66's pick).

If a candidate passes, the result is a production task for the owner to agree (AGENTS 6.2): for R or CR it is T12.74 (unparked, with the cap change in it for CR), for C a task of its own; each re-runs `cargo xtask accept m3` and `accept m6`. If none passes, nothing changes and the result goes to the owner.

Also reported, selecting nothing: each item's embeddings, and the found speakers per AMI meeting at every candidate.

## How far this can be trusted

- Fifteen AMI dev meetings, four participants each, close-talking mixes: about 60 real voices, the same 20 people across a series. One merge more or less decides a lot.
- Few AMI participants speak only in short turns, so A tests whether the candidates add merges or extras on ordinary meetings more than whether they find a quiet talker. S finds talkers, but its scenes are under the cap, so C's effect on a quiet talker in a long meeting is seen only on A.
- The hierarchy over every embedding costs time about the square of the count (about 6.7 times the work at 2592 embeddings as at 1000); this measures no timing.

## The owner's meetings (informational, never selecting)

Added at the owner's request (2026-11-18) after the dev run had started and before any of its results or any private run were seen. Nothing here is an input to the rule above.

- **Data.** A copy of the owner's `earshot.db` made into `fixtures/private/real-meetings/<run>/` (git-ignored) by the `store_parts/copy.rs` rule and opened read-only; recordings are read in place and never written. Every live meeting whose recording is still on disk: the reference (far-end) track of a call, the mic track of a meeting in the room. Tracks are labelled `m1`, `m2`, ... in start order; no id, title or name is printed or written.
- **Pipeline.** As above: the production final-pass diarisation on the CPU, model outputs recorded once and replayed, the replayed baseline equal to the direct run, the four candidates.
- **Measures**, per track and candidate: embeddings, speakers found, and how the baseline's speakers are split or joined (for each candidate speaker, the baseline speakers it takes speech from). For the call that raised T12.75 only, its far-end lines in four groups inferred from the owner's corrections and a per-line voice check (a private file in the run folder; inferred, not verified): per candidate, how each group's lines spread over the found speakers. Each candidate speaker's turn times are written inside the run folder only, so the owner can listen to them later.
- **Use.** None selects. If the rule picks a candidate, these tracks are shown to the owner before any production task, and a split or merge the owner rejects on them is reported with it.

## Result

(Filled after the run.)
