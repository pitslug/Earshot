# Voice samples: taking one meeting's voice back out of a voiceprint (T12.77b)

Status: design v3, 2026-11-18, not built. v1 was blocked by a spec review (the stored sum kept as a synced, last-writer-wins, repaired value could bring a removed voice back); v2 made the samples the only record of new enrolments and passed a second review with items to fix; v3 folds in the owner's decisions of 2026-11-18 and those fixes (section 13 lists the changes). Section 14 proposes the split into sub-tasks. Needs a review of v3 before code (data model and sync: AGENTS 3.5).

## 1. The problem and the decision

`earshot_core::voiceprint::Voiceprint` (`crates/earshot-core/src/voiceprint/mod.rs`) keeps `sum: Vec<f32>`, `count` and `speech_ms`, and `Voiceprint::add` adds unit vectors into `sum` in f32. One enrolment cannot be subtracted exactly ((s + u) - u != s), and nothing records which print got it or how much speech it added.

The owner decided (2026-11-17): from the release that ships this (written "0.4.0" below; the copy names the real version) every enrolment is recorded, so "Not <name>" can take back exactly that meeting's contribution. Enrolments from before cannot be taken back.

The owner's answers of 2026-11-18, folded into the sections below:

1. Older builds and workers in one vault must update: the vault format is raised (section 7).
2. Saving a voice again replaces that meeting's sample for the voice (section 5.3).
3. Removal is keyed on the sample's own `(meeting_id, speaker_id)` only, so it works on any computer (section 6).
4. Samples are kept for the life of the print, with no compaction; removing the person removes every sample (section 8).
5. The copy for a voice saved before samples existed is fixed (section 10).
6. Only the user's own edits take a sample back (Not, relinking away, marking as me, merging speakers), on the computer where the edit is made; a final pass anywhere that drops or renumbers a speaker leaves its sample counting, and an orphaned sample keeps counting (section 6).
7. Linking a wrongly named voice to another saved person and pressing Save moves its sample: taken back from the first person and saved to the second, in one transaction (section 6.2).

## 2. Data

A new entity in `crates/earshot-core/src/voiceprint/mod.rs`, one row per enrolled embedding:

```rust
pub struct VoiceprintSample {
    id: EntityId, meta: SyncMeta,
    voiceprint_id: EntityId,   // the print it adds to
    person_id: EntityId,       // that print's person_id when written
    model: EmbeddingModelId,
    vector: Vec<f32>,          // the unit vector exactly as `unit` computed it
    speech_ms: u64,            // the speech it adds; its count is 1
    meeting_id: Option<EntityId>, speaker_id: Option<EntityId>, // both None for "Your voice"
}
```

- **Fields private.** A sample is built only by `VoiceprintSample::new` (writers) or `VoiceprintSample::from_stored` (store and sync decode), both running `check` (section 2.1); readers use accessors.
- **Sync class:** `SyncableOptInClass`, `ENTITY = "voiceprint_sample"`, declared as `Voiceprint` is. The trybuild snapshot `crates/earshot-core/tests/ui/never_sync_envelope.stderr` lists implementors and is regenerated in the same commit.
- **No foreign keys:** `SyncedRow::meeting()` (`crates/earshot-store/src/sync.rs`) returns `None` for a sample, so it is never held Busy or Parent and never dropped as the child of a deleted meeting.
- **Written once:** never changed while live; its only later write is zero, then tombstone.

### 2.1 Validation (one checked constructor)

New constants beside the type:

- `MAX_EMBEDDING_DIM: usize = 4096` (CAM++ is `earshot_onnx::embed::CAMPPLUS_DIM`, 512). A 4096-value sample as sync JSON is under 70 KB, well inside `earshot_sync::MAX_PLAINTEXT` (192 KiB); a test pins that a sample of exactly `MAX_EMBEDDING_DIM` values with the longest-printing f32 seals.
- `MAX_SAMPLE_SPEECH_MS: u64 = 86_400_000` (24 h; below `i64::MAX`, so it fits SQLite's `INTEGER`). Writers clamp to it (a voice in one meeting cannot pass it; the clamp only keeps a bad input from being refused).
- `SAMPLE_NORM_TOLERANCE: f32 = 1e-3`.

`check` accepts a live sample whose dimension is 1 to `MAX_EMBEDDING_DIM`, every value finite, Euclidean norm within `SAMPLE_NORM_TOLERANCE` of 1, `speech_ms` at most `MAX_SAMPLE_SPEECH_MS`, and `meeting_id` and `speaker_id` both set or both `None`. A tombstone is accepted whatever it holds and is stored with its vector zeroed. A live row that fails is a decode failure in sync (held as `Format`, section 7) and an error in the store, never skipped silently.

## 3. Invariants

### 3.1 Effective print = stored base + live samples

- 0.4.0 never adds to `voiceprint.sum`, `count` or `speech_ms` again; `Voiceprint::add` is removed. Those columns are by definition the untracked base: enrolments before 0.4.0. Older builds cannot add more, because they are locked out of the vault (section 7) and a downgrade on one computer is not supported.
- A print created by 0.4.0 is a row with a zero `sum` of the model's dimension, `count` 0 and `speech_ms` 0, written once and tombstoned only when forgotten.
- `voiceprint::effective(&Voiceprint, &[VoiceprintSample]) -> Effective { sum, count, speech_ms, skipped }` starts from the stored base and adds each live sample of that print in `(meta.created_at, id)` order: the vector in plain f32, `count` and `speech_ms` with `saturating_add`. It skips, deterministically and without panicking, samples of another model or dimension, non-finite ones, and samples whose `person_id` differs from the print's (`skipped` counts them). Every computer with the same rows folds them in the same order, so every computer scores the same sum bit for bit.
- An enrolment in 0.4.0 of one call's embeddings writes their samples with increasing HLC stamps in input order, so `effective` of a 0.4.0 print equals today's `Voiceprint::add` result bit for bit (pinned by a test against literal sums captured from the commit before the change, AGENTS "default equals the old behaviour").
- A print is new (so `EnrolmentConfig::min_new_speech` applies) when its effective `count` is 0. A print with effective `count` 0 (all samples taken back, or samples still held in `sync_pending` after an update) is not scored and is not "unscorable": it is left out of matching and never counted in `MatchReport::skipped_invalid`, so it never raises `PostWarning::SkippedVoiceprints` (`crates/earshot-runtime/src/post/mod.rs`, `name_speakers`). Only a print with effective `count` above 0 and an invalid centroid (zero or non-finite sum) counts as skipped.
- On overflow `count` and `speech_ms` saturate; an f32 sum that becomes infinite gives an invalid centroid and is skipped as above.

### 3.2 Only the fold reads the base

`Voiceprint`'s fields become private, with `Voiceprint::from_stored` for the store and sync decode and `base_sum()`, `base_count()`, `base_speech_ms()` for them and the fold. `Voiceprint::centroid` and `Voiceprint::score` become private to the module; scoring goes through `Effective::centroid()` and `Effective::score(&Embedding)`. A scan test in earshot-runtime beside `local_state_is_written_only_through_the_registry` (`crates/earshot-runtime/src/local.rs`), `voiceprint_base_is_read_only_by_the_fold`, fails if any workspace source outside `crates/earshot-core/src/voiceprint/`, `crates/earshot-store/src/repo.rs` and `crates/earshot-store/src/sync.rs` names `base_sum`, `base_count` or `base_speech_ms`.

Readers of the effective print, all moved to `voiceprint::live_prints(repo, model) -> Vec<(Voiceprint, Effective)>` or `live_prints_of(repo, person)`:

- `match_embedding_with_margin` (and through it `match_embedding`, `match_embedding_with_report`, `post::voice_match`);
- `post::name_speakers`, whose `InMemoryVoiceprintRepo` snapshot copies the model's samples as well as its prints;
- `enroll` and `enroll_me` ("is there a print", "is it new");
- `edit::user_has_voiceprint` (and through it `edit::recognised_me`, `can_unmark_me`, `unmark_me`): a live print with effective `count` above 0;
- `edit::list_persons_in` (`PersonInfo::has_voiceprint`, same rule);
- `my_voice::my_voice` (`crates/earshot-runtime/src/my_voice.rs`): `MyVoice::voiceprints` counts prints with effective `count` above 0 and `speech` sums their effective `speech_ms`;
- evaluation: `crates/earshot-onnx/tests/voiceprint_calibration.rs`, `crates/earshot-runtime/examples/diar_eval/main.rs`, `vp_meetings.rs` and `vp_readout.rs` (which calls `v.score` directly today and is rewritten to `Effective::score`).

### 3.3 The sample invariant (the user's own edits)

While its meeting is live, a sample with a meeting origin points at a live speaker row of that meeting that is linked into the sample's identity (`resolve_person(sample.person_id)`). **Every user edit that ends that, on the computer where it is made, takes the sample back in the same transaction.** The edits are listed in section 6.1.

The invariant deliberately covers only the user's own edits (decision 6). Not held to it, by decision: a final pass on any computer that tombstones or renumbers the row (the sample keeps counting, findable only while the row id lives on), speaker rows changed on another computer and arriving by sync, a concurrent Save on one computer and "Not" on another (section 7.4), and meeting deletion (section 8).

## 4. Writers

`enroll(repo, person, embeddings: &[Embedding], speech, origin: SampleOrigin, config, stamp)` keeps taking a slice with one speech check on the total, so `accept m3`'s `vp.*` protocol and the calibration test keep their inputs. It writes a print row only if the person has no live print of the model, then one sample per embedding in input order. Speech is split so the samples sum to the total exactly: each gets `total / n` ms, the first also the remainder. `SampleOrigin` is `Meeting { meeting, speaker }` or `Own`. `enroll_me` takes the same `origin`.

| Writer | Origin | Repeat rule |
|---|---|---|
| `edit::remember_in` (`save_voice`, `remember_voices`) | `Meeting { meeting, speaker }` | replace (section 5.3) |
| Final pass `enroll_me` (`post/mod.rs`, only when `PostConfig::enroll_me`, off in every shipped build) | `Meeting { meeting, Me row id }` | skip if a live sample from that origin exists in the user's identity: a final pass never takes a sample back (decision 6), and the Me row id survives re-runs, so a re-run adds nothing twice. Today each re-run adds again; this changes that. |
| `my_voice::enrol_my_voice` | `Own` | adds, as today; removed only by forgetting |

- Re-enrolling after "Forget voice" creates a new print.
- `merge_persons`, `merge_into_me` and the one-Me convergence rule in `converge` move nothing: samples follow their print's person through `resolve_person`.
- The evaluation examples enrol through the same `enroll`, so their sums are unchanged.
- `SpeakerIdentity::samples` from v2 is dropped: removal needs no list (decision 3).
- `VoiceprintRepo` gains `samples(model)`, `samples_of(print)`, `samples_of_person(person)`, `samples_from(meeting, speaker)` and `put_sample`, with one shared contract suite run against `InMemoryVoiceprintRepo` and the store (`crates/earshot-store/src/repo.rs`, the `impl VoiceprintRepo` macro); `ReadOnlyStore` refuses `put_sample`. Reads return tombstones; callers filter.

## 5. "Saved from this meeting"

### 5.1 The answer comes from live samples

"This meeting's voice is saved for X" is: a live sample with origin `(meeting, speaker)` whose resolved person is X's identity. It is decided from samples, which sync, never from the device-local `SpeakerIdentity::enrolled` alone.

### 5.2 Pre-0.4.0 entries

An identity entry with `enrolled == true` and no live sample from that origin in any identity was saved before samples existed (or saved here and taken back elsewhere, which unlinks the row and so resets `enrolled` at the next link, `link_in`). `remember_in` still answers `Remembered::AlreadyEnrolled` for it, as today. 0.4.0 keeps writing `enrolled = true` on an enrolment so this rule reads the same on the computer that saved.

### 5.3 Re-save replaces

`remember_in` for a voice linked to X:

1. A live sample from `(meeting, speaker)` in X's identity whose vector bit-equals `unit(entry.centroid)`: `AlreadyEnrolled`, nothing written.
2. A live sample from that origin in X's identity with another vector (a re-run gave a new centroid, which resets `enrolled`): every such sample is taken back (zeroed, tombstoned) and the new centroid enrolled, in the same transaction; the result is `Remembered::Replaced { person, voiceprint }` (a new variant). One sample per voice per meeting per identity; never counted twice.
3. No such sample: the pre-0.4.0 rule (5.2), else enrol.

A re-run of a meeting saved before 0.4.0 resets `enrolled`, so a later Save enrols a sample on top of the untracked base: that meeting then counts twice in the print. Accepted: the base cannot be taken apart.

## 6. Removal: "this voice stops being X"

`voiceprint::take_back(repo, meeting, speaker, keep: Option<EntityId>, hlc, clock) -> usize` zeroes and tombstones every live sample with origin `(meeting, speaker)` whose resolved person is not `keep` (the resolved person the row stays linked to; `None` takes back all). Keyed only on the sample's own meeting and speaker (decision 3): no identity record is needed, so it works on any computer where the sample has arrived.

### 6.1 The edits that take back (section 3.3)

| Edit (`crates/earshot-runtime/src/edit.rs`) | `keep` | Result field |
|---|---|---|
| `not_this_person` | `None` | `NotThisPerson { person, taken_back, saved_before_samples }` |
| `link_in` to a person outside the sample's identity (`link_speaker`, and `save_voice` before it enrols) | the new person, resolved | `taken_back` on `link_speaker`'s and `SavedVoice`'s result |
| `mark_me` | the user (a row already resolving to the user keeps its samples) | `MarkedMe { person, taken_back }` |
| `unmark_me` | `None` (normally nothing: a Me row has no meeting sample unless `enroll_me` ran) | `taken_back` |
| `merge_speakers` (the merged-away row; "merge" in decision 6 is this, not `merge_persons`) | `None` for the merged-away speaker | `Merged::samples_taken_back` |

- `not_this_person` is now accepted when the row is linked, not the user's (`is_users_row`), and has an identity entry **or** a live sample from its origin; `NoStoredVoice` only when it has neither. Without an entry the row is unlinked and given "Speaker N" but no ignore is recorded (there is no entry to hold it); a re-run on a computer with an entry reads the synced row as a user edit and keeps it unlinked. `can_not_this_person` follows the same rule (it reads `samples_from`).
- `saved_before_samples` is true when `taken_back == 0` and the entry says `enrolled` (section 5.2): the copy then says the voice stays in the print (section 10).
- Each edit runs `take_back` inside its `edit::write` transaction, then `Store::flush_erasures()` after the commit when `taken_back > 0`; a `Busy` or failed checkpoint after the commit is reported as `checkpoint_pending`, never an error (as `merge_speakers` does today). Each has an on-disk erasure test with a second connection open.
- A print left with effective `count` 0 stays as an empty row: it holds no biometric data (a zero base), and a concurrent enrolment elsewhere still lands in it. It is never tombstoned for being empty, and it reads as no voiceprint (section 3.1).
- AGENTS.md gains `take_back` as the one per-sample biometric erasure besides `forget_identity_biometrics` (the coordinator applies it).

### 6.2 Save on a wrongly named voice moves its sample (decision 7)

`save_voice_with` already runs `link_in` then `remember_in` in one `edit::write` transaction. With section 6.1, `link_in` to person Y takes back the row's samples in X's identity, then `remember_in` enrols the voice for Y. One transaction: either both happen or neither. `SavedVoice` reports `taken_back` and the person it was taken from, for the copy.

## 7. Sync

### 7.1 The vault format is raised (decision 1)

- `earshot_sync::ENVELOPE_FORMAT` (`crates/earshot-sync/src/seal.rs`) goes from 1 to 2 in the release that ships samples. That is the release that starts sending them, because samples are written only from that release (section 14, T12.77b3 before T12.77b4).
- The path that exists: every push carries the format in header `Earshot-Sync-Format` (`earshot_sync::wire::FORMAT_HEADER`) and body (`PushRequest::format`, `sync/engine.rs`); the relay's `Db::push` (`crates/earshot-sync-server/src/db.rs`) raises `vault.format` to `max(format, 2)` in the same transaction that stores the rows; from then `check_format` (`crates/earshot-sync-server/src/lib.rs`) answers every push and pull from a format-1 device with 409 `ErrorWord::UpdateNeeded`, which the client maps to `SyncProblem::UpdateNeeded` (`crates/earshot-runtime/src/sync/client.rs`; `stops()` is true, so the loop stops) and the Sync page shows as "Another device uses a newer Earshot. Update Earshot on this computer to keep syncing." (`apps/desktop/src-tauri/src/sync_settings.rs`). A pull answer's `vault_format` above the build's is the same problem (`engine.rs`, `pull`). An `earshot-worker` image on the older version stops the same way (its run stops on a stopping problem with a fixed health word).
- Because the format rises in the transaction that first stores a format-2 push, no older build ever receives a sample, with or without voiceprint sync (this closes v2's N1: older builds held undecodable rows before checking the switches). An updated build decodes the entity and skips it while voiceprint sync is off (`Applied::skipped`), so nothing is held.
- The vault rises at the first push from any 0.4.0 device, whether or not it has a sample to send. That is the owner's decision; it also means one updated computer locks out every older one at once.
- Rows of format 1 stay readable (`open_row` refuses only a format above the build's). No existing entity changes shape.
- The AGENTS.md bullets "ENVELOPE_FORMAT stays 1 when a new entity is added" and "Raise it only when an existing entity's serialised shape changes", the `ENVELOPE_FORMAT` doc comment, and `docs/sync-spec.md` sections 3 and 10, gain the exception: a new entity may raise it when older builds must not hold its rows (the owner, 2026-11-18). The coordinator edits AGENTS.md; the sub-task edits the doc comment and the spec.

### 7.2 The entity

- `SyncedRow::VoiceprintSample` and `SyncedRow::decode` gain the entity; decode runs `VoiceprintSample::from_stored` (section 2.1): a live row that fails is held as `Format`, never skipped, and a tombstone is accepted and stored zeroed.
- `SyncKinds::syncs("voiceprint_sample")` returns `self.voiceprints`; `outbox_except` / `outbox_len_except` leave samples out in SQL when it is off; the worker's `refuse_voiceprints` (`crates/earshot-worker/src/worker.rs`) covers samples because it refuses the switch.
- Store migration 9 adds the table's outbox triggers in the V7/V8 style (section 9).

### 7.3 Tombstones and repairs

- Tombstones are final in `apply_remote` for `voiceprint_sample` and `voiceprint`, as for meetings: a live row never overwrites a local tombstone, and an incoming stamp later than it gets the tombstone re-stamped and pushed.
- Repair in `converge` (dead print or dead person): a live sample whose print or `person_id` is tombstoned here, or a live print whose person is tombstoned here, is zeroed and tombstoned (rows arriving late for someone removed here). A print or person not yet known here leaves the row alone; the fold ignores samples of an unknown print.
- `Applied::biometrics_forgotten` also counts every pulled sample or print tombstone that replaced a live local row and every repair erasure, so the sync engine (`engine.rs`, `if applied.biometrics_forgotten > 0`) runs `flush_erasures`.

### 7.4 What sync does not repair (decided)

- "Not" on computer B and Save of the same voice on computer A before either has heard of the other: A's new sample stays and counts (only the edit's own computer takes back, decision 6). The row ends as the later edit leaves it; if it ends unlinked, the user can link and Save or remove the person.
- A computer that has just updated and still holds samples in `sync_pending` reads a smaller print until `retry_held` applies them; nothing is counted twice.

## 8. Deletion

- Every biometric path: core `forget_voiceprints` zeroes and tombstones the identity's prints and every live sample whose `person_id` is in the identity or whose print is one of its prints (`samples_of_person`, `samples_of`), reached through `forget_identity_biometrics` (`crates/earshot-store/src/identities.rs`: `Store::forget_voiceprints`, `forget_voiceprints_of`, `remove_person`, a pulled person tombstone). `secure_delete` and the TRUNCATE checkpoint are unchanged.
- Kept for the life of the print, no compaction (decision 4): compaction would add samples into `voiceprint.sum`, which section 3.1 forbids.
- Meeting deletion (`Store::delete_meeting`) leaves samples untouched: they keep a `meeting_id` and `speaker_id` that point at nothing and keep counting (decisions 3 and 6). The ids are opaque; the one cost is that a UUIDv7 meeting id carries its creation time.

## 9. Migration

Migration 9 in `crates/earshot-store/src/schema.rs`: `CREATE TABLE voiceprint_sample (id BLOB PRIMARY KEY NOT NULL, {META}, voiceprint_id BLOB NOT NULL, person_id BLOB NOT NULL, model TEXT NOT NULL, vector BLOB NOT NULL, speech_ms INTEGER NOT NULL, meeting_id BLOB, speaker_id BLOB) STRICT`, indexes on `voiceprint_id`, `person_id`, `model` and `(meeting_id, speaker_id)`, outbox triggers in the V7/V8 style, no backfill. `SCHEMA_VERSION` 9 (`crates/earshot-store/src/db.rs`), an appended `APPLIED` hash, `SYNCED_TABLES` grows to 12; a new `crates/earshot-store/tests/data/schema_v8.sql` (none exists yet) with its pin test, and a v8-to-v9 test showing an old print's effective print equal to its stored base.

## 10. Copy

Result fields carry counts and flags; the words live in the UI (`packages/ui/src/detail/`), in a case table `fixtures/ui/voice_sample_copy_cases.json` read by the UI test and by a Rust test that pins `since` (the version in the copy) to be no later than the workspace version. `{name}` and `{other}` are filled with a function replacer (a name can hold `$&`).

| Case | Text |
|---|---|
| Not, `taken_back > 0` | "Unlinked. This meeting's voice is no longer part of {name}'s saved voice." |
| Not, `saved_before_samples` (agreed, decision 5) | "Unlinked. This voice was saved before Earshot {since}, so it stays part of {name}'s saved voice. To start their voice over, remove them on the People page." |
| Not, otherwise | "Unlinked." |
| Save that moved a sample | "Saved for {other}. This meeting's voice is no longer part of {name}'s saved voice." |
| Link away, `taken_back > 0` | "Linked to {other}. This meeting's voice is no longer part of {name}'s saved voice." |
| This is me, `taken_back > 0` | "Marked as you. This meeting's voice is no longer part of {name}'s saved voice." |
| Merge, `samples_taken_back > 0` | "Merged. The merged voice is no longer part of {name}'s saved voice: save the voice again to add it." |

Only the decision-5 text is agreed; the rest are proposals for the owner. The "Not <name>" tooltip (`notThisPersonHint`, `SpeakersPanel.tsx`) loses "may hold it" for a voice saved from 0.4.0: "Shows this voice as an unnamed speaker in this meeting, stops asking for its name and takes this meeting's voice out of {name}'s saved voice." when a live sample exists, today's text otherwise. The People page's page-name guard (`page_names_tests.rs`, `pageNames.test.ts`) gets a source-pinning case for "People page".

## 11. Privacy

- Each sample is one more stored embedding (about 2 KB raw at 512 values, about 8 KB as sync JSON): one per saved voice, plus one per final pass for Me where `enroll_me` is on (no shipped build), plus one per "Your voice" sample, kept as long as its print.
- A sample tied to a meeting id says which meeting a person spoke in, which a centroid alone does not.
- Samples sync only when voiceprint sync is on (off by default), sealed like every row; no build without the entity ever receives one (section 7.1).

## 12. Tests that pin each rule

- Core: `effective` bit-exact and independent of input order; equal to literal sums from the commit before (one-call slices); skips wrong model, wrong dimension, non-finite and wrong-person samples deterministically; saturation; an empty print is neither scored nor counted in `skipped_invalid`; an enrolment never changes the stored base; effective count 0 needs `min_new_speech`; `take_back` restores the effective print exactly and respects `keep`; `forget_voiceprints` erases samples whose print is missing; `VoiceprintSample::check` at each boundary (dimension 0, 1, `MAX_EMBEDDING_DIM`, +1; norm at the tolerance; speech at the limit; one origin id set); the shared `VoiceprintRepo` contract.
- Store: v8 fixture pin and migration test; trigger and synced-table pins; on-disk erasure after Not, link away, mark me, merge, forget and a pulled sample tombstone (second connection open); the opt-in switch on push and apply.
- Sync: the format raise (a format-1 client after a format-2 push gets `update_needed` on push and pull; a format-2 client reads format-1 rows); Not racing a Save in both push orders and both clock orders (the taken-back sample never returns; the other computer's new sample stays, section 7.4); sample and print tombstones final against later live copies; a late sample or print of a person tombstoned here erased and flushed; a just-updated device with held samples gives base plus samples, never double; each malformed pulled sample held as `Format`; a tombstone with non-zero values stored zeroed; a concurrent enrolment into an emptied print counts; samples skipped, not held, with voiceprint sync off.
- Runtime: each edit in 6.1 takes back with the right `keep` and reports its count; Save on a wrongly named voice moves the sample in one transaction (a forced failure in `remember_in` leaves the first sample in place); re-save replaces, same centroid is `AlreadyEnrolled`; the pre-0.4.0 entry answers `AlreadyEnrolled` and Not reports `saved_before_samples`; Not without an identity entry but with a sample works; a re-run that carries the row still takes back, one that tombstones it leaves the sample counting; the final pass `enroll_me` skips a second sample; meeting deletion leaves samples untouched; `my_voice`, `user_has_voiceprint` and `list_persons_in` read effective counts; the base scan test.

## 13. Changes in v3

- Section 11 decisions folded into the body; the vault format raise names `ENVELOPE_FORMAT`, `FORMAT_HEADER`, `Db::push`, `check_format`, `SyncProblem::UpdateNeeded` and the Sync page copy (7.1).
- Removal keyed on `(meeting_id, speaker_id)` only; `SpeakerIdentity::samples` dropped; `not_this_person` works without an identity entry (6, 6.1).
- `enroll` keeps a slice with one speech check; speech split exactly; final-pass `enroll_me` skips a repeat (4).
- The invariant stated once (3.3) with its edit list (6.1), including `unmark_me`, and its decided exceptions (3.3, 7.4).
- Readers completed (`my_voice::my_voice`, `list_persons_in`, the snapshot in `name_speakers`, the eval examples); `centroid` and `score` private, fields private, base scan test (3.2).
- Empty or held prints never raise `PostWarning::SkippedVoiceprints` (3.1).
- Validation constants with values, one checked constructor, any tombstone accepted and zeroed, wrong-person samples skipped, saturation on overflow (2.1, 3.1).
- Erasure counts, copy, flush and on-disk tests for every edit; pre-0.4.0 `enrolled` keeps `AlreadyEnrolled`; re-save replaces (5, 6, 10).
- Save on a wrongly named voice moves the sample in one transaction (6.2).
- Compaction dropped (8).

## 14. Proposed split

Each sub-task is under about 800 changed lines, merges alone with `cargo xtask ci` green, and gets a reviewer (data model, migration, sync and biometric deletion: AGENTS 3.5). Samples are written only from T12.77b4, after the format raise in T12.77b3, so main never has a state where a build sends samples to a vault older builds can still read.

- [ ] T12.77b1 Voiceprint samples: the core model and the fold
  - deps: T12.77
  - do: (1) `VoiceprintSample` (private fields, `new` / `from_stored` through `check`), `MAX_EMBEDDING_DIM`, `MAX_SAMPLE_SPEECH_MS`, `SAMPLE_NORM_TOLERANCE`, `SyncClass` / `SyncSerialize` (`voiceprint_sample`, opt-in) and the trybuild snapshot. (2) `effective`, `Effective::{centroid, score}`, `live_prints`, `live_prints_of`; `Voiceprint` fields private with `from_stored` and `base_*` accessors; `centroid` and `score` private; empty prints left out of matching and of `skipped_invalid`. (3) Every reader in docs/voice-samples.md 3.2 moved to the effective print (folding no samples yet: behaviour unchanged except the empty-print rule), the base scan test. No storage, no writer change.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-runtime voiceprint_base`; `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-onnx voiceprint_calibration`; `cargo xtask purity`

- [ ] T12.77b2 Voiceprint samples: store table, repository and erasure
  - deps: T12.77b1
  - do: (1) `VoiceprintRepo::{samples, samples_of, samples_of_person, samples_from, put_sample}` on `InMemoryVoiceprintRepo`, the store and `ReadOnlyStore` (refuses writes), with one shared contract suite. (2) Migration 9 (table, indexes, outbox triggers), `SCHEMA_VERSION` 9, `APPLIED`, `SYNCED_TABLES`, `tests/data/schema_v8.sql` with its pin and a v8-to-v9 test. (3) `live_prints` folds stored samples; `name_speakers`' snapshot copies them. (4) `forget_voiceprints` erases samples by person and by print; `voiceprint::take_back` (core, with `keep`). (5) On-disk erasure tests for forget and `take_back`. No writer creates samples yet.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-store`; `cargo test -p earshot-runtime post`

- [ ] T12.77b3 Voiceprint samples: sync and the vault format raise
  - deps: T12.77b2
  - do: (1) `SyncedRow::VoiceprintSample`, decode through `from_stored` (live failures held as `Format`, tombstones zeroed), `SyncKinds::syncs("voiceprint_sample")`, outbox exclusion. (2) Tombstones final for `voiceprint_sample` and `voiceprint` in `apply_remote`; the dead-print and dead-person repair in `converge`; `biometrics_forgotten` counts. (3) `ENVELOPE_FORMAT` 2 (doc comment and docs/sync-spec.md updated), with a test that a format-1 client is refused `update_needed` on push and pull after a format-2 push and that format-1 rows still open. (4) The sync tests of docs/voice-samples.md section 12; report the AGENTS.md wording for the coordinator.
  - accept: `cargo test -p earshot-sync`; `cargo test -p earshot-sync-server`; `cargo test -p earshot-store sync`; `cargo test -p earshot-runtime sync`; `cargo test -p earshot-worker`

- [ ] T12.77b4 Voiceprint samples: every enrolment writes a sample
  - deps: T12.77b3
  - do: (1) `enroll` and `enroll_me` take `SampleOrigin`, write a zero-base print when there is none and one sample per embedding (speech split exactly); `Voiceprint::add` removed. (2) Origins: `remember_in` (`Meeting`), `enrol_my_voice` (`Own`), final-pass `enroll_me` (`Meeting`, skip a repeat). (3) Evaluation callers (`diar_eval`, `vp_meetings`, `vp_readout`, `voiceprint_calibration`) unchanged in result; the literal-sum test against the commit before.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-store`; `cargo test -p earshot-onnx voiceprint_calibration`

- [ ] T12.77b5 "Not <name>", relink, mark me and merge take the sample back
  - deps: T12.77b4
  - do: (1) `take_back` in `not_this_person`, `link_in` (and so `link_speaker` and `save_voice`), `mark_me`, `unmark_me` and `merge_speakers`, with the `keep` of section 6.1, counts in their results, `flush_erasures` after the commit and `checkpoint_pending`. (2) `not_this_person` and `can_not_this_person` accept a row with a live sample and no entry; `saved_before_samples`. (3) `remember_in`: `AlreadyEnrolled` by sample, `Replaced`, the pre-0.4.0 rule. (4) Save on a wrongly named voice moves the sample (section 6.2). (5) Runtime and on-disk tests of docs/voice-samples.md section 12.
  - accept: `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-store`

- [ ] T12.77b6 Voice sample copy in the app
  - deps: T12.77b5
  - do: (1) IPC results carry the counts and flags (`not_this_person`, `link_speaker`, `save_voice`, `mark_me`, `merge_speakers`), bindings regenerated. (2) `fixtures/ui/voice_sample_copy_cases.json` with the section 10 texts, read by the UI test and a Rust test pinning `since` to the workspace version; `notThisPersonHint` by whether a sample exists; the People page source-pinning case. (3) The Speakers panel in the Vite preview in light and dark.
  - accept: `cargo test -p earshot-desktop --bin earshot detail`; `cargo test -p earshot-desktop bindings`; `cmd /c pnpm --filter @earshot/ui test`; `cmd /c pnpm -r build`
