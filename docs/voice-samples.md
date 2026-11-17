# Voice samples: taking one meeting's voice back out of a voiceprint (T12.77b)

Status: design v4, 2026-11-18, not built. v1 was blocked by a spec review (the stored sum kept as a synced, last-writer-wins, repaired value could bring a removed voice back); v2 made the samples the only record of new enrolments; v3 folded in the owner's decisions of 2026-11-18; the review of v3 was blocked on two points (the device-local `enrolled` flag, and a Replace that erased a sample when the new enrolment was refused). v4 applies the coordinator's decisions on those and every advisory (section 13). Section 14 is the split into sub-tasks, with sample sync and the vault format raise last.

## 1. The problem and the decision

`earshot_core::voiceprint::Voiceprint` (`crates/earshot-core/src/voiceprint/mod.rs`) keeps `sum: Vec<f32>`, `count` and `speech_ms`, and `Voiceprint::add` adds unit vectors into `sum` in f32. One enrolment cannot be subtracted exactly ((s + u) - u != s), and nothing records which print got it or how much speech it added.

The owner decided (2026-11-17): from the release that ships this (written "0.4.0" below; the copy names the real version) every enrolment is recorded, so "Not <name>" can take back exactly that meeting's contribution. Enrolments from before cannot be taken back.

The owner's answers of 2026-11-18, folded into the sections below:

1. Older builds and workers in one vault must update: the vault format is raised (section 7).
2. Saving a voice again replaces that meeting's sample for the voice (section 5.3).
3. Removal is keyed on the sample's own `(meeting_id, speaker_id)` only, so it works on any computer the sample has reached (section 6).
4. Samples are kept for the life of the print, with no compaction; removing the person removes every sample (section 8).
5. The copy for a voice saved before samples existed is fixed (section 10).
6. Only the user's own edits take a sample back (Not, relinking away, marking as me, merging speakers), on the computer where the edit is made; a final pass anywhere that drops or renumbers a speaker leaves its sample counting, and an orphaned sample keeps counting (section 6).
7. Linking a wrongly named voice to another saved person and pressing Save moves its sample: taken back from the first person and saved to the second, in one transaction (section 6.2).

Accepted by the coordinator (2026-11-18), consistent with voiceprint sync being opt-in: a "Forget voice" or a "Not" made on one computer never reaches a computer with voiceprint sync off. That computer keeps its own copy of the print and its samples until the user acts there.

## 2. Data

A new entity in `crates/earshot-core/src/voiceprint/mod.rs`, one row per enrolled embedding:

```rust
pub struct VoiceprintSample {
    id: EntityId, meta: SyncMeta,
    body: Option<SampleBody>,  // None exactly when meta.deleted (a tombstone)
}
struct SampleBody {
    voiceprint_id: EntityId,   // the print it adds to
    person_id: EntityId,       // that print's person_id when written
    model: EmbeddingModelId,
    vector: Vec<f32>,          // the unit vector exactly as `unit` computed it
    speech_ms: u64,            // the speech it adds; its count is 1
    meeting_id: Option<EntityId>, speaker_id: Option<EntityId>, // both None for "Your voice"
}
```

- **A tombstone keeps only its id and sync metadata.** Taking back or forgetting a sample writes `body: None`: the print, person, model, vector, speech, meeting and speaker are all cleared (coordinator, 2026-11-18). In the store the tombstone's columns are NULL and its vector an empty blob; `secure_delete` zeroes the freed bytes and the erasure's checkpoint removes them from the WAL. An on-disk test (second connection open) finds neither the vector bytes nor the meeting, speaker or person id bytes after a take back.
- **Fields private.** A sample is built only by `VoiceprintSample::new` (writers), `VoiceprintSample::erased` (tombstones) or `VoiceprintSample::from_stored` (store and sync decode), all through `check` (section 2.1); readers use accessors. `Debug` prints the id, meta, model and dimension, never the vector.
- **Sync class:** `SyncableOptInClass`, `ENTITY = "voiceprint_sample"`, declared as `Voiceprint` is. The trybuild snapshot `crates/earshot-core/tests/ui/never_sync_envelope.stderr` lists implementors and is regenerated in the same commit.
- **No foreign keys:** `SyncedRow::meeting()` (`crates/earshot-store/src/sync.rs`) returns `None` for a sample, so it is never held Busy or Parent and never dropped as the child of a deleted meeting.
- **Written once:** never changed while live; its only later write is the tombstone.

### 2.1 Validation (one checked constructor)

New constants beside the type:

- `MAX_EMBEDDING_DIM: usize = 4096` (CAM++ is `earshot_onnx::embed::CAMPPLUS_DIM`, 512). The record goes into the sync envelope as JSON numbers, which can print at f64 length (up to about 24 characters with sign, exponent and separator): 512 values are about 12 KB and 4096 values about 100 KB, inside `earshot_sync::MAX_PLAINTEXT` (192 KiB). A test seals a sample of exactly `MAX_EMBEDDING_DIM` values chosen to print at the longest length.
- `MAX_SAMPLE_SPEECH_MS: u64 = 86_400_000` (24 h; below `i64::MAX`, so it fits SQLite's `INTEGER`). Writers clamp to it.
- `SAMPLE_NORM_TOLERANCE: f32 = 1e-3`.

`check` accepts a live sample whose dimension is 1 to `MAX_EMBEDDING_DIM`, every value finite, Euclidean norm within `SAMPLE_NORM_TOLERANCE` of 1, `speech_ms` at most `MAX_SAMPLE_SPEECH_MS`, and `meeting_id` and `speaker_id` both set or both `None`. A tombstone is accepted whatever its incoming record holds and is stored as `body: None`. A live row that fails is an error in the store and a decode failure in sync (held as `Format`, section 7.2), never skipped silently. A vector survives `seal_row`, `open_row`, the store's write and read and back bit for bit (a test over values that need full f32 precision).

## 3. Invariants

### 3.1 Effective print = stored base + live samples

- 0.4.0 never adds to `voiceprint.sum`, `count` or `speech_ms` again; `Voiceprint::add` is removed. Those columns are by definition the untracked base: enrolments before 0.4.0. Older builds cannot add more once the vault format is raised (section 7), and a downgrade on one computer is not supported.
- A print created by 0.4.0 is a row with a zero `sum` of the model's dimension, `count` 0 and `speech_ms` 0, written once and tombstoned only when forgotten. A print tombstone, written here or pulled, is stored with its `sum` zeroed (core `forget_voiceprints` does it here; the store zeroes a pulled one before writing it).
- `voiceprint::effective(&Voiceprint, &[VoiceprintSample]) -> Effective { sum, count, speech_ms, skipped }` starts from the stored base and adds each live sample of that print in `(meta.created_at, id)` order: the vector in plain f32, `count` and `speech_ms` with `saturating_add`. It skips, deterministically and without panicking, samples of another model or dimension, non-finite ones and samples whose `person_id` differs from the print's (`skipped` counts them). Every computer with the same rows folds them in the same order and scores the same sum bit for bit.
- One `enroll` call writes its samples with increasing HLC stamps in input order, so `effective` of a 0.4.0 print equals today's `Voiceprint::add` result bit for bit, pinned by a test against literal sums captured from the commit before the change.
- A print is new (so `EnrolmentConfig::min_new_speech` applies) when its effective `count` is 0. A print with effective `count` 0 is not scored and is not "unscorable": it is left out of matching and never counted in `MatchReport::skipped_invalid`, so it never raises `PostWarning::SkippedVoiceprints` (`post::name_speakers`). Only a print with effective `count` above 0 and an invalid centroid counts as skipped.
- On overflow `count` and `speech_ms` saturate; an f32 sum that becomes infinite gives an invalid centroid and is skipped as above.

### 3.2 Only the fold reads the base

`Voiceprint`'s fields become private, with `Voiceprint::from_stored` for the store and sync decode and `base_sum()`, `base_count()`, `base_speech_ms()` for them and the fold. Serde keeps the field names, so the wire JSON of a voiceprint is unchanged: a test pins `serde_json::to_string` of a fixed `Voiceprint` byte for byte against a literal JSON string captured before the change. `Voiceprint::centroid` and `Voiceprint::score` become private to the module; scoring goes through `Effective::centroid()` and `Effective::score(&Embedding)`.

A scan test in earshot-runtime beside `local_state_is_written_only_through_the_registry` (`crates/earshot-runtime/src/local.rs`), `voiceprint_base_is_read_only_by_the_fold`, reads every `.rs` file under `crates/` and `apps/desktop/src-tauri/` (sources, tests and examples; `target` folders skipped) and fails if any outside `crates/earshot-core/src/voiceprint/`, `crates/earshot-store/src/repo.rs` and `crates/earshot-store/src/sync.rs` names `base_sum`, `base_count` or `base_speech_ms`.

Readers of the effective print, all through `voiceprint::live_prints(repo, model) -> Vec<(Voiceprint, Effective)>` or `live_prints_of(repo, person)`:

- `match_embedding_with_margin` (and through it `match_embedding`, `match_embedding_with_report`, `post::voice_match`);
- `post::name_speakers`, whose `InMemoryVoiceprintRepo` snapshot copies the model's samples as well as its prints;
- `enroll` and `enroll_me` ("is there a print", "is it new");
- `edit::user_has_voiceprint` (and through it `edit::recognised_me`, `can_unmark_me`, `unmark_me`): a live print with effective `count` above 0;
- `edit::list_persons_in` (`PersonInfo::has_voiceprint`, same rule);
- `my_voice::my_voice` (`crates/earshot-runtime/src/my_voice.rs`): `MyVoice::voiceprints` counts prints with effective `count` above 0 and `speech` sums their effective `speech_ms`;
- evaluation: `crates/earshot-onnx/tests/voiceprint_calibration.rs`, `crates/earshot-runtime/examples/diar_eval/main.rs`, `vp_meetings.rs` and `vp_readout.rs` (which calls `v.score` directly today and is rewritten to `Effective::score`). `accept m3`'s `vp.*` values and the ignored `vp_meetings_the_rule_still_picks_the_shipped_margin` must come out identical (section 14).

No IPC type carries a vector: the desktop sees counts and flags only (a test serialises every changed IPC result and finds no `vector` key).

### 3.3 The sample invariant (the user's own edits)

While its meeting is live, a sample with a meeting origin points at a live speaker row of that meeting that is linked into the sample's identity (`resolve_person(person_id)`). **Every user edit that ends that, on the computer where it is made, takes the sample back in the same transaction.**

This is enforced by construction: a speaker row's person changes, outside the final pass and sync apply, only through one primitive, `edit::set_row_person(tx, stamp, meeting, &mut row, person: Option<EntityId>) -> usize`, which writes the row and runs `take_back` with `keep` = the new person, resolved; a row an edit tombstones goes through `edit::retire_row`, which runs `take_back` with `keep = None`. A scan test, `edit_speaker_person_changes_only_through_set_row_person`, fails if any runtime source other than those two functions, `crates/earshot-runtime/src/post/` and the store's sync apply assigns a speaker row's `person_id` or tombstones a speaker row.

Not held to it, by decision: a final pass on any computer that tombstones or renumbers the row (the sample keeps counting), speaker rows changed on another computer and arriving by sync, a concurrent Save on one computer and "Not" on another (section 7.5), and meeting deletion (section 8).

## 4. Writers

`enroll(repo, person, embeddings: &[Embedding], speech, origin: SampleOrigin, config, stamp)` keeps taking a slice with one speech check on the total, so `accept m3`'s `vp.*` protocol and the calibration test keep their inputs. It writes a print row only if the person has no live print of the model, then one sample per embedding in input order. Speech is split so the samples sum to the total exactly: each gets `total / n` ms, the first also the remainder. `SampleOrigin` is `Meeting { meeting, speaker }` or `Own`. `enroll_me` takes the same `origin`. A new `voiceprint::admissible(repo, person, embeddings, speech, config, excluding: &[EntityId]) -> Result<(), VoiceprintError>` runs every check of `enroll` without writing, with the samples in `excluding` left out of the effective count; `enroll` calls it first.

| Writer | Origin | Repeat rule |
|---|---|---|
| `edit::remember_in` (`save_voice`, `remember_voices`) | `Meeting { meeting, speaker }` | replace (section 5.3) |
| Final pass `enroll_me` (`post/mod.rs`, only when `PostConfig::enroll_me`, off in every shipped build) | `Meeting { meeting, Me row id }` | skip if a live sample from that origin exists in the user's identity: a final pass never takes a sample back (decision 6), and the Me row id survives re-runs. Today each re-run adds again; this changes that. |
| `my_voice::enrol_my_voice` | `Own` | adds, as today; removed only by forgetting |

- Re-enrolling after "Forget voice" creates a new print.
- `merge_persons`, `merge_into_me` and the one-Me convergence rule in `converge` move nothing: samples follow their print's person through `resolve_person`.
- `VoiceprintRepo` gains `samples(model)`, `samples_of(print)`, `samples_of_person(person)`, `samples_from(meeting, speaker)` and `put_sample`, with one shared contract suite run against `InMemoryVoiceprintRepo` and the store (`crates/earshot-store/src/repo.rs`, the `impl VoiceprintRepo` macro); `ReadOnlyStore` refuses `put_sample`. Reads return tombstones; callers filter.

## 5. "Is this voice saved"

### 5.1 Live samples are the only answer

"This meeting's voice is saved for X" is exactly: a live sample with origin `(meeting, speaker)` whose resolved person is X. Nothing else answers it: samples sync, and nothing device-local can contradict a take back made on another computer.

### 5.2 `enrolled` is a frozen pre-samples marker

`SpeakerIdentity::enrolled` (device-local, `crates/earshot-store/src/identities.rs`) means only "this voice was saved on this computer before samples existed (untracked)". From 0.4.0 nothing writes `enrolled = true`. It is reset to `false` only where today's code already resets it (`link_in` to another person, `mark_me`, `unmark_me`, and the final pass writing a new entry), and never read to decide whether to save.

### 5.3 Save and re-save

`remember_in` for a voice linked to X, with `old` = the live samples from `(meeting, speaker)` whose resolved person is X:

1. `old` non-empty and one vector bit-equals `unit(entry.centroid)`: `Remembered::AlreadySaved` (renamed from `AlreadyEnrolled`), nothing written.
2. Otherwise, admissibility first: the model's voiceprint threshold (`Uncalibrated` otherwise) and `admissible(.., excluding: old)`. On any refusal (`NotEnoughSpeech`, `Uncalibrated`) or error, nothing is taken back: `old` stays and counts, the result says why, and an error rolls the whole `edit::write` transaction back.
3. Admissible: `old` is taken back and the new centroid enrolled in the same transaction. The result is `Remembered::Replaced { person, voiceprint }` when `old` was non-empty, else `Enrolled`. One sample per voice per meeting per person; never counted twice.

A voice saved before samples (the marker set, no sample) is saved again as a new sample on top of the untracked base: that meeting then counts twice in the print. Accepted: the base cannot be taken apart.

## 6. Removal: "this voice stops being X"

`voiceprint::take_back(repo, meeting, speaker, keep: Option<EntityId>, hlc, clock) -> usize` writes the tombstone (section 2) of every live sample with origin `(meeting, speaker)` whose resolved person is not `keep` (`None` takes back all). Keyed only on the sample's own meeting and speaker (decision 3): no identity record is needed.

### 6.1 The edits that take back (section 3.3)

| Edit (`crates/earshot-runtime/src/edit.rs`) | Through | `keep` | Result field |
|---|---|---|---|
| `not_this_person` | `set_row_person(.., None)` | `None` | `NotThisPerson { person, taken_back, saved_before_samples }` |
| `link_in` (`link_speaker`, `save_voice`) to another person | `set_row_person(.., Some(new))` | the new person, resolved | `taken_back` on `link_speaker`'s and `SavedVoice`'s result |
| `mark_me` | `set_row_person(.., Some(user))` | the user (a row already resolving to the user keeps its samples) | `MarkedMe { person, taken_back }` |
| `unmark_me` | `set_row_person(.., None)` | `None` (normally nothing: a Me row has no meeting sample unless `enroll_me` ran) | `taken_back` |
| `merge_speakers` (the merged-away row; "merge" in decision 6 is this, not `merge_persons`) | `retire_row` | `None` | `Merged::samples_taken_back` |

- `not_this_person` is accepted when the row is linked, not the user's (`is_users_row`), and has an identity entry or a live sample from its origin; `NoStoredVoice` only when it has neither. Without an entry the row is unlinked and given "Speaker N" but no ignore is recorded (there is no entry to hold it); a re-run on a computer with an entry reads the synced row as a user edit and keeps it unlinked. `can_not_this_person` follows the same rule (it reads `samples_from`).
- `saved_before_samples` is true exactly when `taken_back == 0`, no live sample from the origin exists in any identity, and this computer's entry has the frozen marker (5.2). Only then does the copy say the voice stays in the print (section 10).
- Each edit runs `take_back` inside its `edit::write` transaction, then `Store::flush_erasures()` after the commit when `taken_back > 0`; a `Busy` or failed checkpoint after the commit is reported as `checkpoint_pending`, never an error (as `merge_speakers` does today). Each has an on-disk erasure test with a second connection open.
- A print left with effective `count` 0 stays as an empty row: it holds no biometric data, a concurrent enrolment elsewhere still lands in it, and it reads as no voiceprint (3.1). It is never tombstoned for being empty.
- AGENTS.md gains `take_back` (through `set_row_person` / `retire_row`) as the one per-sample biometric erasure besides `forget_identity_biometrics` (the coordinator applies it).

### 6.2 Save on a wrongly named voice moves its sample (decision 7)

`save_voice_with` runs, in one `edit::write` transaction:

1. Admissibility of the voice for the new person Y (threshold and `admissible`, read only). An error returns before anything is written.
2. `link_in` to Y through `set_row_person`: the row's samples in X are taken back (the row is no longer X's, 3.3).
3. If step 1 accepted, enrol for Y; `SavedVoice` reports `moved_from: Some(X)` and `taken_back`.
4. If step 1 refused (`NotEnoughSpeech`, `Uncalibrated`, `NoStoredVoice`), the link to Y is kept as today and X's samples are still taken back, but nothing is saved for Y: `remembered` carries the refusal and the copy is the "Linked to" text plus the refusal's existing reason, never "Saved for {other}".

Tests: accepted move (X loses exactly its sample, Y gains one); each refusal (X's sample gone, Y unchanged, no "Saved for" copy); an error in the enrolment (whole transaction rolled back, X's sample still live).

## 7. Sync

### 7.1 Until the last sub-task: local only

Until T12.77b6 merges (section 14), no build sends or receives voice data in the new shape:

- `SyncKinds::syncs("voiceprint_sample")` answers `false` (from T12.77b2), so `outbox_except` / `outbox_len_except` leave samples out in SQL. The outbox triggers exist from migration 9, so every sample and sample tombstone waits in `sync_outbox`.
- `voiceprint` rows are held back on the push side too (a push-only list, `sync::HELD_UNTIL_SAMPLES`, read by `outbox_except` and `outbox_len_except`; apply is unchanged, so prints and tombstones from 0.3.x computers still arrive). Otherwise a main build would send a 0.4.0 print (zero base, count 0) that 0.3.x computers count as unscorable, and a forget made on a main build would reach them while its samples could not. This is a coordinator decision to confirm (section 13): it means a forget made on a main build reaches other computers only after the release.
- `ENVELOPE_FORMAT` stays 1. A runtime test with samples and a new print in the store runs a sync tick against `LocalRelay` and finds no sample or print pushed and the vault's format unchanged.

So main builds are safe to run against the real vault until T12.77b6 merges, and T12.77b6 merges only with the release that ships samples.

### 7.2 The last sub-task: entity, skip before hold, backlog

- `SyncedRow::VoiceprintSample` and `SyncedRow::decode` gain the entity; decode runs `VoiceprintSample::from_stored` (2.1): a live row that fails is held as `Format`, a tombstone is stored as `body: None`.
- `SyncKinds::syncs("voiceprint_sample")` returns `self.voiceprints`, and `HELD_UNTIL_SAMPLES` is removed. The backlog of samples, sample tombstones and prints written before the raise then pushes in the first ticks after it (a test: rows written with the arm off push after the change, oldest first, and apply on a second device to the same effective print).
- A row of a switched-off kind is skipped before it can be held (advisory A2): `Incoming` gains the envelope's `entity` name (read from the opened plaintext even when the record does not decode), and `apply_remote` checks `rules.kinds.syncs(entity)` before the `Format` hold. A worker test (voiceprint sync refused, `crates/earshot-worker/src/worker.rs` `refuse_voiceprints`) applies a page holding samples and prints, valid and malformed, and finds nothing stored and nothing in `sync_pending`.

### 7.3 The vault format is raised (decision 1)

- `earshot_sync::ENVELOPE_FORMAT` (`crates/earshot-sync/src/seal.rs`) goes from 1 to 2 in T12.77b6.
- The existing path: every push carries the format in header `Earshot-Sync-Format` (`earshot_sync::wire::FORMAT_HEADER`) and body (`PushRequest::format`, `sync/engine.rs`); the relay's `Db::push` (`crates/earshot-sync-server/src/db.rs`) raises `vault.format` to `max(format, 2)` in the transaction that stores the rows; `check_format` (`crates/earshot-sync-server/src/lib.rs`) answers a format-1 device's push with 409 `ErrorWord::UpdateNeeded`, which the client maps to `SyncProblem::UpdateNeeded` (`crates/earshot-runtime/src/sync/client.rs`; `stops()` is true) and the Sync page shows as "Another device uses a newer Earshot. Update Earshot on this computer to keep syncing." (`apps/desktop/src-tauri/src/sync_settings.rs`). A pull answer's `vault_format` above the build's is the same problem (`engine.rs`, `pull`).
- Pull today checks the format (`check_format`) before `Db::pull` reads rows, outside any transaction, so a push that raises the format between the two could hand a format-1 device a sample (advisory A3). T12.77b6 moves the check inside `Db::pull`: one read transaction reads `vault.format`, refuses with `update_needed` when the caller's format is below it, and only then reads rows. With the raise and the rows committed together on push, a format-1 device never opens or stores a sample. A relay test races a format-2 push against format-1 pulls.
- The vault rises at the first push from any 0.4.0 device, samples or not; one updated computer locks out every older one at once (the owner's decision).
- An `earshot-worker` on the older image stops with `update_needed`; under compose's restart policy it starts, is refused and stops again until its image is updated. T12.77b6 adds that note to docs/sync.md ("Running the worker").
- Rows of format 1 stay readable (`open_row` refuses only a format above the build's). No existing entity changes shape.
- The AGENTS.md bullets that keep `ENVELOPE_FORMAT` at 1 for a new entity gain the exception (the owner, 2026-11-18); the coordinator edits AGENTS.md, T12.77b6 the doc comment and `docs/sync-spec.md` sections 3 and 10.

### 7.4 Tombstones, repairs and what the window hears

- Tombstones are final in `apply_remote` for `voiceprint_sample` and `voiceprint`, as for meetings: a live row never overwrites a local tombstone, and an incoming stamp later than it gets the tombstone re-stamped and pushed.
- Repair in `converge` (dead print or dead person): a live sample whose print is tombstoned here, or whose `person_id` followed through `merged_into` to the end of its chain reaches a tombstoned person, is tombstoned; so is a live print whose person, followed the same way, is tombstoned. A print or person not yet known here leaves the row alone; the fold ignores samples of an unknown print.
- `Applied::biometrics_forgotten` counts every pulled sample or print tombstone that replaced a live local row and every repair erasure, so the sync engine (`engine.rs`, `if applied.biometrics_forgotten > 0`) runs `flush_erasures`.
- A written sample or print sets `Applied::people`, and a live sample adds its `meeting_id` to `Applied::meetings`, so `SyncEvent { meetings, people }` (T12.81) makes the People page and an open meeting reload (whether a voice reads as saved, and the "Not" tooltip). A sample tombstone names no meeting (its ids are cleared) and sets `people` only.

### 7.5 What sync does not repair (decided)

"Not" on computer B and Save of the same voice on computer A before either has heard of the other: A's new sample stays and counts (only the edit's own computer takes back, decision 6). The row ends as the later edit leaves it; if it ends unlinked, the user can link and Save or remove the person.

## 8. Deletion

- Every biometric path: core `forget_voiceprints` tombstones the identity's prints (sum zeroed) and every live sample whose `person_id` is in the identity or whose print is one of its prints (`samples_of_person`, `samples_of`), reached through `forget_identity_biometrics` (`crates/earshot-store/src/identities.rs`: `Store::forget_voiceprints`, `forget_voiceprints_of`, `remove_person`, a pulled person tombstone). It returns `Forgotten { voiceprints, samples }`, and every count that decides a flush or is shown (`edit::forget_person_voices`, `my_voice::forget_my_voice`, `biometrics_forgotten`) includes samples, so a person with only samples to erase still gets the checkpoint. `secure_delete` and the TRUNCATE checkpoint are unchanged.
- Kept for the life of the print, no compaction (decision 4): compaction would add samples into `voiceprint.sum`, which 3.1 forbids.
- Meeting deletion (`Store::delete_meeting`) leaves live samples untouched: they keep a `meeting_id` and `speaker_id` that point at nothing and keep counting (decisions 3 and 6); the one cost is that a UUIDv7 meeting id carries its creation time.

## 9. Migration

Migration 9 in `crates/earshot-store/src/schema.rs`: `CREATE TABLE voiceprint_sample (id BLOB PRIMARY KEY NOT NULL, {META}, voiceprint_id BLOB, person_id BLOB, model TEXT, vector BLOB NOT NULL, speech_ms INTEGER, meeting_id BLOB, speaker_id BLOB) STRICT` (every body column NULL and `vector` empty exactly for a tombstone, checked by a `CHECK` on `deleted`), indexes on `voiceprint_id`, `person_id`, `model` and `(meeting_id, speaker_id)`, outbox triggers in the V7/V8 style, no backfill. `SCHEMA_VERSION` 9 (`crates/earshot-store/src/db.rs`), an appended `APPLIED` hash, `SYNCED_TABLES` grows to 12; a new `crates/earshot-store/tests/data/schema_v8.sql` (none exists yet) with its pin test, and a v8-to-v9 test showing an old print's effective print equal to its stored base.

## 10. Copy

Result fields carry counts and flags; the words live in the UI (`packages/ui/src/detail/`), in a case table `fixtures/ui/voice_sample_copy_cases.json` read by the UI test and by a Rust test that pins `since` (the version in the copy) to be no later than the workspace version. `{name}` and `{other}` are filled with a function replacer (a name can hold `$&`).

| Case | Text |
|---|---|
| Not, `taken_back > 0` | "Unlinked. This meeting's voice is no longer part of {name}'s saved voice." |
| Not, `saved_before_samples` (agreed, decision 5) | "Unlinked. This voice was saved before Earshot {since}, so it stays part of {name}'s saved voice. To start their voice over, remove them on the People page." |
| Not, otherwise | "Unlinked." |
| Save that moved a sample (6.2 step 3) | "Saved for {other}. This meeting's voice is no longer part of {name}'s saved voice." |
| Link away or a refused move, `taken_back > 0` (6.2 step 4) | "Linked to {other}. This meeting's voice is no longer part of {name}'s saved voice." |
| This is me, `taken_back > 0` | "Marked as you. This meeting's voice is no longer part of {name}'s saved voice." |
| Merge, `samples_taken_back > 0` | "Merged. The merged voice is no longer part of {name}'s saved voice: save the voice again to add it." |

Only the decision-5 text is agreed; the rest are proposals for the owner. The "Not <name>" tooltip (`notThisPersonHint`, `SpeakersPanel.tsx`) says "Shows this voice as an unnamed speaker in this meeting, stops asking for its name and takes this meeting's voice out of {name}'s saved voice." when a live sample exists, today's text otherwise. The page-name guard (`page_names_tests.rs`, `pageNames.test.ts`) gets a source-pinning case for "People page".

## 11. Privacy

- Each sample is one more stored embedding (about 2 KB raw at 512 values, about 12 KB as sync JSON): one per saved voice, plus one per final pass for Me where `enroll_me` is on (no shipped build), plus one per "Your voice" sample, kept as long as its print.
- A live sample tied to a meeting id says which meeting a person spoke in, which a centroid alone does not; its tombstone says nothing (section 2).
- Samples sync only when voiceprint sync is on (off by default), sealed like every row; no build without the entity ever receives one (7.3); a computer with voiceprint sync off never hears of a forget or a "Not" made elsewhere (section 1).

## 12. Tests that pin each rule

- Core: `effective` bit-exact and independent of input order; equal to literal sums from the commit before; skips wrong model, wrong dimension, non-finite and wrong-person samples; saturation; an empty print neither scored nor counted in `skipped_invalid`; an enrolment never changes the stored base; effective count 0 needs `min_new_speech`; `admissible` with `excluding`; `take_back` restores the effective print exactly and respects `keep`; a tombstone has no body; `forget_voiceprints` erases samples whose print is missing and counts them; `check` at each boundary (dimension 0, 1, `MAX_EMBEDDING_DIM`, +1; norm at the tolerance; speech at the limit; one origin id set); `Debug` without the vector; `Voiceprint` wire JSON byte-identical to a literal; the shared `VoiceprintRepo` contract.
- Store: v8 fixture pin and migration test; trigger, synced-table and `CHECK` pins; on-disk erasure (vector and id bytes gone, second connection open) after Not, link away, mark me, merge, forget, remove person and a pulled sample tombstone; a pulled print tombstone stored with a zero sum; the vector bit-equal through seal, open, store and read.
- Sync before the raise (T12.77b2 to b5): no sample or print pushed, vault format unchanged. At the raise (T12.77b6): a format-1 client refused `update_needed` on push and on a pull racing a format-2 push; format-1 rows still open; the backlog pushes after the raise; Not racing a Save in both push orders and both clock orders (the taken-back sample never returns; the other computer's new sample stays, 7.5); sample and print tombstones final against later live copies; a late sample or print of a person tombstoned here, directly or through a `merged_into` chain, erased and flushed; each malformed pulled sample held as `Format`; a pulled tombstone with a body stored without one; a concurrent enrolment into an emptied print counts; a switched-off kind skipped, valid or not, before any hold (worker test); `Applied::people` and `meetings` for sample rows; `cargo xtask accept m11`.
- Runtime: each edit in 6.1 takes back with the right `keep` and reports its count; the `set_row_person` scan test; Save on a wrongly named voice in every branch of 6.2; re-save `AlreadySaved` by sample only, `Replaced`, and each refusal leaving the old sample live; `saved_before_samples` only with no live sample and the marker; the marker never written true; Not without an identity entry but with a sample; a re-run that carries the row still takes back, one that tombstones it leaves the sample counting; final-pass `enroll_me` skips a second sample; meeting deletion leaves samples untouched; `my_voice`, `user_has_voiceprint` and `list_persons_in` read effective counts; no vector in any IPC result; the base scan test over crates and the desktop.

## 13. Changes in v4

- B1: live samples are the only answer to "is this voice saved"; `enrolled` frozen as the pre-samples marker; `AlreadySaved` from samples only; the "saved before" copy only with no live sample and the marker (5, 6.1).
- Owner-call 1: a sample tombstone keeps only its id and sync metadata, tested on disk (2, 9).
- B2: admissibility before any take back, the old sample excluded; refusals leave the old sample; the move path's branches and copy (5.3, 6.2).
- Order: sample sync and the format raise are the last sub-task, merged only with the release; until then samples (and, to confirm, prints) wait in the outbox, tested (7.1, 14).
- A1: m3 accept, the vp_meetings margin test and m11 in the accept lists; the voiceprint wire JSON pinned (3.2, 14).
- A2: switched-off kinds skipped by entity name before any hold, worker test (7.2).
- A3: the pull format check inside `Db::pull`'s transaction; worker restart note for docs/sync.md (7.3).
- A4: forget counts include samples; pulled print tombstones zeroed; dead-person repair follows `merged_into` (7.4, 8).
- A5: `set_row_person` / `retire_row` and their scan test; sample rows reach `SyncEvent` (3.3, 7.4).
- A6: the listed tests; no vector in IPC or `Debug`; the base scan over crates and the desktop including tests and examples; the unreachable "held after update" case dropped; size estimate at f64 JSON length (2, 3.2, 12).
- Owner-call 3 stated (1, 11).
- To confirm: holding `voiceprint` rows on the push side until the raise (7.1).

## 14. Split

Each sub-task is under about 800 changed lines, merges alone with `cargo xtask ci` green and gets a reviewer (data model, migration, sync and biometric deletion: AGENTS 3.5). T12.77b1 to b5 keep every voice row on the computer that wrote it, so main builds are safe with the real vault; T12.77b6 turns sample sync on, raises the vault format and merges only with the release.

- [ ] T12.77b1 Voiceprint samples: the core model and the fold
  - deps: T12.77
  - do: (1) `VoiceprintSample` (`body: Option<SampleBody>`, private fields, `new` / `erased` / `from_stored` through `check`, `Debug` without the vector), `MAX_EMBEDDING_DIM`, `MAX_SAMPLE_SPEECH_MS`, `SAMPLE_NORM_TOLERANCE`, `SyncClass` / `SyncSerialize` (`voiceprint_sample`, opt-in) and the trybuild snapshot. (2) `effective`, `Effective::{centroid, score}`, `live_prints`, `live_prints_of`, `admissible`; `Voiceprint` fields private with `from_stored` and `base_*`, its wire JSON pinned to a literal; `centroid` and `score` private; empty prints left out of matching and of `skipped_invalid`. (3) Every reader in docs/voice-samples.md 3.2 moved to the effective print (folding no samples yet), the base scan test. No storage, no writer change.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-runtime voiceprint_base`; `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-onnx voiceprint_calibration`; `cargo xtask accept m3` (every `vp.*` and `dia.*` value identical to the committed `acceptance/results/m3.json`); `cargo test --release -p earshot-runtime --example vp_meetings vp_meetings_the_rule_still_picks_the_shipped_margin -- --ignored`; `cargo xtask purity`

- [ ] T12.77b2 Voiceprint samples: store table, repository and erasure, held from sync
  - deps: T12.77b1
  - do: (1) `VoiceprintRepo::{samples, samples_of, samples_of_person, samples_from, put_sample}` on `InMemoryVoiceprintRepo`, the store and `ReadOnlyStore` (refuses writes), one shared contract suite. (2) Migration 9 (table with the tombstone `CHECK`, indexes, outbox triggers), `SCHEMA_VERSION` 9, `APPLIED`, `SYNCED_TABLES`, `tests/data/schema_v8.sql` with its pin and a v8-to-v9 test. (3) `SyncKinds::syncs("voiceprint_sample")` answers false; `HELD_UNTIL_SAMPLES` holds `voiceprint` rows back from push (apply unchanged); a sync tick test pushes no sample or print and leaves the vault format unchanged. (4) `live_prints` folds stored samples; `name_speakers`' snapshot copies them. (5) `forget_voiceprints` tombstones samples by person and by print and returns `Forgotten { voiceprints, samples }`, counted by every caller; `voiceprint::take_back`. (6) On-disk erasure tests for forget, remove person and `take_back` (vector and id bytes gone).
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-store`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-runtime sync`; `cargo test -p earshot-runtime my_voice`

- [ ] T12.77b3 Voiceprint samples: every enrolment writes a sample
  - deps: T12.77b2
  - do: (1) `enroll` and `enroll_me` take `SampleOrigin`, run `admissible`, write a zero-base print when there is none and one sample per embedding (speech split exactly); `Voiceprint::add` removed; nothing writes `enrolled = true`. (2) Origins: `remember_in` (`Meeting`), `enrol_my_voice` (`Own`), final-pass `enroll_me` (`Meeting`, skip a repeat). (3) Evaluation callers (`diar_eval`, `vp_meetings`, `vp_readout`, `voiceprint_calibration`) give identical results; the literal-sum test against the commit before.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-store`; `cargo test -p earshot-onnx voiceprint_calibration`; `cargo xtask accept m3` (every `vp.*` and `dia.*` value identical to the committed `acceptance/results/m3.json`); `cargo test --release -p earshot-runtime --example vp_meetings vp_meetings_the_rule_still_picks_the_shipped_margin -- --ignored`

- [ ] T12.77b4 "Not <name>", relink, mark me and merge take the sample back
  - deps: T12.77b3
  - do: (1) `edit::set_row_person` and `edit::retire_row` (`take_back` inside), used by `not_this_person`, `link_in`, `mark_me`, `unmark_me` and `merge_speakers`, with the scan test; counts in their results, `flush_erasures` after the commit and `checkpoint_pending`. (2) `not_this_person` and `can_not_this_person` accept a row with a live sample and no entry; `saved_before_samples` from samples and the frozen marker. (3) `remember_in`: `AlreadySaved` from samples only, admissibility before take back, `Replaced`. (4) `save_voice` moves the sample by docs/voice-samples.md 6.2, every branch tested. (5) The runtime and on-disk tests of section 12; no vector in any result.
  - accept: `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-store`

- [ ] T12.77b5 Voice sample copy in the app
  - deps: T12.77b4
  - do: (1) IPC results carry the counts and flags (`not_this_person`, `link_speaker`, `save_voice`, `mark_me`, `merge_speakers`; `AlreadySaved`), bindings regenerated, a test that no IPC result serialises a vector. (2) `fixtures/ui/voice_sample_copy_cases.json` with the section 10 texts, read by the UI test and a Rust test pinning `since` to the workspace version; `notThisPersonHint` by whether a sample exists; the People page source-pinning case. (3) The Speakers panel in the Vite preview in light and dark.
  - accept: `cargo test -p earshot-desktop --bin earshot detail`; `cargo test -p earshot-desktop bindings`; `cmd /c pnpm --filter @earshot/ui test`; `cmd /c pnpm -r build`

- [ ] T12.77b6 Voiceprint samples: sync on and the vault format raise (merges only with the release)
  - deps: T12.77b5
  - do: (1) `SyncedRow::VoiceprintSample` and decode through `from_stored`; `Incoming` carries the entity name and `apply_remote` skips a switched-off kind before any hold; tombstones final for `voiceprint_sample` and `voiceprint`; pulled print tombstones zeroed; the dead-print and dead-person repair (following `merged_into`); `biometrics_forgotten` counts samples; `Applied::people` / `meetings` for sample rows. (2) `SyncKinds::syncs("voiceprint_sample")` follows the voiceprint switch; `HELD_UNTIL_SAMPLES` removed; the backlog test. (3) `ENVELOPE_FORMAT` 2 (doc comment, docs/sync-spec.md); the format check inside `Db::pull`'s transaction with the race test; the worker restart note in docs/sync.md. (4) The sync tests of docs/voice-samples.md section 12 and the worker page test; report the AGENTS.md wording for the coordinator.
  - accept: `cargo test -p earshot-sync`; `cargo test -p earshot-sync-server`; `cargo test -p earshot-store sync`; `cargo test -p earshot-runtime sync`; `cargo test -p earshot-worker`; `cargo xtask accept m11`
