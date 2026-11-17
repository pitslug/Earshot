# Voice samples: taking one meeting's voice back out of a voiceprint (T12.77b)

Status: draft design v2, 2026-11-18, not built. v1 was blocked by a spec review (the stored sum kept as a synced, last-writer-wins, repaired value could bring a removed voice back); v2 makes the samples the only record of new enrolments. Waits on a second spec review and the owner's answers to section 11.

## 1. The problem and the decision

`earshot_core::voiceprint::Voiceprint` keeps `sum: Vec<f32>`, `count` and `speech_ms`. One enrolment cannot be subtracted exactly, and nothing records which print got it or how much speech it added.

The owner decided (2026-11-17): from 0.4.0 every enrolment is recorded, so "Not <name>" can take back exactly that meeting's contribution. Enrolments from before 0.4.0 cannot be taken back.

## 2. Data

A new entity in `crates/earshot-core/src/voiceprint/mod.rs`, one per enrolled embedding:

```rust
pub struct VoiceprintSample {
    id: EntityId, meta: SyncMeta,
    voiceprint_id: EntityId,   // the print it adds to
    person_id: EntityId,       // the print's person when written
    model: EmbeddingModelId,
    vector: Vec<f32>,          // the unit vector exactly as computed by `unit`
    speech_ms: u64,            // the speech it adds; its count is 1
    meeting_id: Option<EntityId>, speaker_id: Option<EntityId>, // None for "Your voice"
}
```

- **Sync class:** `SyncableOptInClass`, `ENTITY = "voiceprint_sample"`, declared as `Voiceprint` is.
- **No foreign keys:** `SyncedRow::meeting()` returns `None` for a sample, so it is never held Busy or Parent and never dropped as the child of a deleted meeting.
- **Written once:** never touched while live; its only later write is zero then tombstone.
- **No envelope bump:** the voiceprint row's shape is unchanged and a new entity does not bump `ENVELOPE_FORMAT`; older builds hold the new rows as `Format`.

## 3. Invariant: effective print = stored base + live samples

- 0.4.0 never adds to `voiceprint.sum`, `count` or `speech_ms` again. Those columns are by definition the untracked base: enrolments before 0.4.0, and enrolments by older builds since.
- A print created in 0.4.0 is a row with zero `sum`, `count` 0 and `speech_ms` 0, written once and tombstoned only when forgotten.
- The effective print is computed at read time by `voiceprint::effective(&Voiceprint, &[VoiceprintSample]) -> Effective { sum, count, speech_ms, skipped }`: start from the stored `sum`, add each live sample's vector in plain `f32` in `(meta.created_at, id)` order, `count` and `speech_ms` with `checked_add`.
- The fold skips, deterministically and without panicking, samples of another model or dimension and samples with non-finite values; on integer overflow the whole print reads as unscorable. Skips are counted in `MatchReport::skipped_invalid`.
- Readers of the effective print: `match_embedding_with_margin`, the `name_speakers` snapshot in `post/mod.rs` (which copies samples too), the "has a print" checks of `enroll` / `enroll_me`, `edit::user_has_voiceprint` / `recognised_me`, `list_persons`, and the evaluation examples. `Voiceprint::add` is removed.
- A print is "new" (so `min_new_speech` applies) when its effective count is 0.

No repairs of the aggregate, no write-order dependence. A device that has just updated and still holds samples in `sync_pending` reads a smaller print until `retry_held` applies them; nothing is counted twice.

## 4. Writers

`enroll(repo, person, embedding: &Embedding, speech, origin: SampleOrigin, ..)` takes one embedding, writes a print row only if there is none, then writes one sample. `enroll_me` gains the same `origin`.

| Writer | Origin |
|---|---|
| `edit::remember_in` (`save_voice`, `remember_voices`) | `(meeting, speaker)`; the new sample id is added to the identity entry's new `samples: Vec<EntityId>` (serde default) |
| Final pass `enroll_me` (`post/mod.rs`) | `(meeting, Me row id)` |
| `my_voice::enrol_my_voice` | `(None, None)` |

- Re-enrol after forget creates a new print.
- `merge_persons`, `merge_into_me` and the one-Me convergence rule move nothing; samples follow their print's person through `resolve_person`.
- `merge_speakers` appends the merged-away entry's `samples` to the kept entry.
- The evaluation examples (`diar_eval`, `vp_meetings`, `vp_readout`) enrol one embedding at a time in today's order, so their sums are unchanged.
- `VoiceprintRepo` gains `samples(model)`, `samples_of(print)`, `samples_of_person(person)` and `put_sample`, with one shared contract suite run against `InMemoryVoiceprintRepo` and the store; `ReadOnlyStore` refuses writes.

## 5. Removal: "this voice stops being X"

`voiceprint::take_back(repo, meeting, entry_samples, speaker, identity, stamp) -> usize` zeroes and tombstones the live samples whose id is in the entry's `samples` list or whose `(meeting_id, speaker_id)` equals `(meeting, speaker)`, whose `meeting_id` is `meeting`, and whose `person_id` is in `identity_of(resolve_person(X))`.

Every write that moves a voice away from X calls it in the same `edit::write` transaction: `not_this_person`, `link_in` to a person outside X's identity, and `mark_me`. The caller then runs `Store::flush_erasures()`; a `Busy` after it means the change is committed. AGENTS.md gains this as the one per-sample biometric erasure besides `forget_identity_biometrics`.

- A print left with effective count 0 stays as an empty row: it holds no biometric data, and a concurrent enrolment elsewhere still lands in it. It is never tombstoned for being empty.
- Re-runs: `match_previous` carries a user-edited entry, `samples` included, to the new row. If it does not (another model, no match), the samples keep the old `speaker_id`, cannot be found from the new row, and "Not" reports nothing to take back; their contribution stays, as a pre-0.4.0 one does.
- The result is `TakenBack(n)`: `n > 0` means this meeting's voice was removed; `0` means nothing recorded can be taken back. "Saved from this meeting" is decided from live samples, never from `enrolled`; `remember_in` answers `AlreadyEnrolled` when a live sample from this voice exists for that identity, and `enrolled` stays only so old records read.

## 6. Sync

- `SyncKinds::syncs("voiceprint_sample")` returns the voiceprint switch; `outbox_except` / `outbox_len_except` leave samples out in SQL when it is off; the worker's voiceprint refusal covers samples; `SyncedRow::VoiceprintSample` and `decode` gain the entity.
- Validation at decode: a live sample has a dimension of 1 to `MAX_EMBEDDING_DIM`, all values finite, a norm within 1e-3 of 1, and `speech_ms` at most `MAX_SAMPLE_SPEECH_MS`; a tombstone is all zeros. A row that fails is a decode failure, held as `Format`, never skipped.
- Tombstones are final in `apply_remote` for `voiceprint_sample` and `voiceprint`, as for meetings: a live row never overwrites a local tombstone, and a later incoming stamp gets the tombstone re-stamped and pushed.
- Repair in `converge`, dead print or dead person: a live sample or print whose print or `person_id` is tombstoned here is zeroed and tombstoned (rows arriving late for someone removed here). A print or person not yet known here leaves the row alone.
- Receiver-side erasure: `Applied::biometrics_forgotten` also counts every pulled sample or print tombstone that replaced a live local row and every repair erasure, so the client runs `flush_erasures` whenever it is above zero.

## 7. Deletion

- Every biometric path: core `forget_voiceprints` zeroes and tombstones the identity's prints and every live sample whose `person_id` is in the identity or whose print is one of its prints, reached through `forget_identity_biometrics` (`Store::forget_voiceprints`, `forget_voiceprints_of`, `remove_person`, a pulled person tombstone). `secure_delete` and the TRUNCATE checkpoint are unchanged.
- Meeting deletion leaves samples untouched: a live sample is never re-stamped, so it keeps `meeting_id` and `speaker_id` that point at nothing. Acceptable because voiceprints outlive meetings by the owner's rule and the ids are opaque; the one cost is that a UUIDv7 meeting id carries its creation time.

## 8. Migration

Migration 9 in `crates/earshot-store/src/schema.rs`: `CREATE TABLE voiceprint_sample (id BLOB PRIMARY KEY NOT NULL, {META}, voiceprint_id BLOB NOT NULL, person_id BLOB NOT NULL, model TEXT NOT NULL, vector BLOB NOT NULL, speech_ms INTEGER NOT NULL, meeting_id BLOB, speaker_id BLOB) STRICT`, indexes on `voiceprint_id`, `person_id`, `model` and `(meeting_id, speaker_id)`, outbox triggers in the V7/V8 style, no backfill. `SCHEMA_VERSION` 9, an appended `APPLIED` hash, `SYNCED_TABLES` grows; a new `tests/data/schema_v8.sql` with its pin test and a v8-to-v9 test showing an old print's effective print unchanged; the trybuild `*.stderr` lists of implementing types updated in the same commit.

## 9. Privacy

- Each sample is one more stored embedding (about 1 KB raw, a few KB as sync JSON): one per saved voice, plus one per final pass for Me when `enroll_me` is on, kept as long as its print.
- A sample tied to a meeting id says which meeting a person spoke in, which a centroid alone does not.
- Samples sync only when voiceprint sync is on (off by default), sealed like every row.

## 10. Tests that pin each rule

- Core: `effective` bit-exact and independent of input order; the fold skips wrong model, wrong dimension and non-finite values deterministically, overflow makes the print unscorable, nothing panics; an enrolment never changes `voiceprint.sum`; effective count 0 needs `min_new_speech`; `take_back` restores the effective print exactly; `forget_voiceprints` erases samples whose print is missing; the shared `VoiceprintRepo` contract.
- Store: v8 fixture pin and migration test; trigger and synced-table pins; on-disk erasure after "Not", after forget and after a pulled sample tombstone (second connection open); the opt-in switch on push and apply.
- Sync: "Not" racing an enrolment in both push orders and both clock orders (the removed voice never comes back, the enrolment stays); a sample tombstone final against its live copy; a print tombstone final against a later live print; a late sample or print of a person tombstoned here erased and flushed; a just-updated device with held samples gives base plus samples, never double; each malformed pulled sample held as `Format`; a concurrent enrolment into an emptied print counts; an older build holds the entity as `Format`.
- Runtime: `save_voice` then `not_this_person`, relink away or `mark_me` takes the sample back; the pre-0.4.0 case returns `TakenBack(0)`; a re-run that carries the entry still takes back, one that does not reports nothing; `merge_speakers` merges sample lists; each enrol path records its origin; meeting deletion leaves samples untouched.

## 11. Still the owner's to decide

1. **Older builds in one vault.** They match on the base only, so a print created in 0.4.0 is unscorable there. Require every computer on 0.4.0 before voiceprints sync, or accept it?
2. **Re-saving a voice after a re-run.** It reads `AlreadyEnrolled` while the old sample lives. Replace the old sample instead?
3. **"Not" with no identity record.** On a computer without the meeting's identity record (a synced meeting; centroids are device-local) "Not" stays refused. Take back by `(meeting, speaker)` there?
4. **Retention.** Keep samples for the life of the print, or compact old ones into the base (they could no longer be taken back)?
5. **Copy.** Wording for "nothing saved from this meeting can be taken back".

## 12. Second spec review (2026-11-18): what is left before code

v2 closes the first review's convergence failures (no removed voice comes back, nothing counted twice, removal independent of clock order). Still open:

- **Blocking, the owner's decision (N1).** An older build holds an undecodable row before it checks the opt-in switches, so every computer still on 0.3.x and every `earshot-worker` not yet updated would keep voice samples in `sync_pending` beside the vault key, even with voiceprint sync off; the worker exists to keep voice data off the server. Either raise the vault format when samples start flowing (older builds get `UpdateNeeded`, the existing path) or accept that residue explicitly.
- **To fix in the design before code:**
  - Keep `enroll` taking a slice with one speech check on the total (one sample per embedding, in order), so the calibration test and `accept m3`'s `vp.*` protocol stay unchanged; `vp_readout` needs rewriting.
  - State the invariant instead of listing call sites: while its meeting is live, a meeting-origin sample points at a live speaker row linked into its identity, and any write that ends that takes the sample back in the same transaction. That covers `merge_speakers`, `link_in` away and `mark_me` (except a row already resolving to the user); whether the final pass and speaker rows relinked on another computer are held to it is the owner's call (cost: real enrolments erased by a re-transcribe elsewhere).
  - Key removal one way only: on the sample's `(meeting_id, speaker_id)` (works on any computer, keeps a link after deletion) or on the device-local list (no residue, one computer only). The owner's call.
  - Readers missed: `my_voice::my_voice` (speech total), `Voiceprint::centroid` and `score` (make private, pin with a scan test); an empty or held print must not raise `PostWarning::SkippedVoiceprints`.
  - Validation: writers build samples through the same checked constructor; give `MAX_EMBEDDING_DIM` (JSON under `MAX_PLAINTEXT`) and `MAX_SAMPLE_SPEECH_MS` (at most `i64::MAX`, clamped at write) values; accept any tombstone and store zeros; skip a sample whose `person_id` differs from its print's; saturate rather than disable on overflow.
  - Relink and `mark_me` erasures report their count, have copy, flush and an on-disk test each; a pre-0.4.0 entry with `enrolled == true` still answers `AlreadyEnrolled`.
  - Drop compaction (section 11 question 4): it would add to `voiceprint.sum`, which section 3 forbids.
- **Added to the owner's questions:** samples keeping a deleted meeting's id on every synced computer (section 7 decided it; it is a privacy call), and orphaned samples (keep counting, or erase when their speaker row goes).
