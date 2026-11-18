# Voice samples: taking one meeting's voice back out of a voiceprint (T12.77b)

Status: design v6, 2026-11-18, not built; the v5 review was clear and v6 applies its advisories. v1 was blocked by a spec review (the stored sum kept as a synced, last-writer-wins, repaired value could bring a removed voice back); v2 made the samples the only record of new enrolments; v3 folded in the owner's decisions of 2026-11-18; v4 fixed the review of v3 (the device-local `enrolled` flag; a Replace that erased a sample when the new enrolment was refused). The review of v4 was blocked on two points: its push-side hold made print tombstones non-final, and between two sub-tasks a repeat Save or a relink could leave a wrong sample. v5 applies the coordinator's decisions on those and the advisories (section 13). Section 14 is the split, with sample sync and the vault format raise last.

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

`Voiceprint`'s fields become private, with `Voiceprint::from_stored` for the store and sync decode and `base_sum()`, `base_count()`, `base_speech_ms()` for them and the fold. Serde keeps the field names, so the wire JSON of a voiceprint is unchanged: a test pins `serde_json::to_string` of a fixed `Voiceprint` byte for byte against a literal JSON string captured before the change. `Voiceprint::centroid` and `Voiceprint::score` become private to the module; scoring goes through `Effective::centroid()` and `Effective::score(&Embedding)`, and the one base check outside the fold is `Effective::has_base()` (non-zero base count), so `edit.rs` and the other runtime readers never name the base.

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

This is enforced by construction: a speaker row's person changes, outside the final pass and sync apply, only through one primitive, `edit::set_row_person(tx, stamp, meeting, &mut row, person: Option<EntityId>) -> usize`, which writes the row and runs `take_back` with `keep` = the new person, resolved; a row an edit tombstones goes through `edit::retire_row`, which runs `take_back` with `keep = None`.

A scan test, `edit_speaker_person_changes_only_through_set_row_person` (earshot-runtime, beside the base scan), walks the non-test sources of every workspace member except xtask and the test-support packages, by the module walk `local_state_is_written_only_through_the_registry` uses (modules under `#[cfg(test)]` left out). It reports, with file and enclosing function, every:

- assignment matching `\bperson_id\s*=[^=]`;
- struct literal `Speaker\s*\{` whose braces contain `person_id\s*:` (including `Speaker { person_id: .. }` with a struct update);
- call `mark_deleted(`, or assignment matching `\bmeta\.deleted\s*=\s*true`, in a function that also calls `put_speaker(`.

The scan runs on source with string literals, raw strings and comments blanked first, so SQL text such as `UPDATE speaker SET person_id = ?` or `deleted = 1` inside a query never counts as a hit; fixtures pin both directions (a hit in code, none in a string or a comment).

Each hit fails unless its enclosing function is on the test's written-out list, each entry with its reason: `edit::set_row_person` and `edit::retire_row`; every function under `crates/earshot-runtime/src/post/` (the final pass, decision 6); the store's sync apply (`apply_remote`, `converge`); `Store::remove_person` (`crates/earshot-store/src/delete.rs`: it sends the person's rows back to "Speaker N" in the transaction that forgets every sample of the identity, so nothing is left to take back); core `delete_meeting` (a deleted meeting's rows; its samples stay by decision, section 8); and the constructors that set `person_id` on another type (`Voiceprint::from_stored`, `VoiceprintSample::new` and `from_stored`, `Person::new`, the store's row readers). A new hit fails until it is reviewed and listed.

Not held to it, by decision: a final pass on any computer that tombstones or renumbers the row (the sample keeps counting), speaker rows changed on another computer and arriving by sync, a concurrent Save on one computer and "Not" on another (section 7.5), and meeting deletion (section 8).

## 4. Writers

`enroll(repo, person, embeddings: &[Embedding], speech, origin: SampleOrigin, config, stamp)` keeps taking a slice with one speech check on the total, so `accept m3`'s `vp.*` protocol and the calibration test keep their inputs. (It is first added in T12.77b3 as `voiceprint::enroll_sample` beside today's `enroll`, used only by tests; a core test pins that `enroll` and `enroll_sample` on the same inputs give bit-equal effective prints. The evaluation callers (`diar_eval`, `vp_meetings`, `voiceprint_calibration`) stay on `enroll` until T12.77b6, where `enroll_sample` replaces `enroll` for every caller at once, section 14.) It writes a print row only if the person has no live print of the model, then one sample per embedding in input order. When the person has two live prints of the model (two computers each created one before hearing of the other), it adds to the one with the earliest `(meta.created_at, id)`, so every computer picks the same print; a test pins it. Speech is split so the samples sum to the total exactly: each gets `total / n` ms, the first also the remainder. `SampleOrigin` is `Meeting { meeting, speaker }` or `Own`. `enroll_me` takes the same `origin`. A new `voiceprint::admissible(repo, person, embeddings, speech, config, excluding: &[EntityId]) -> Result<(), VoiceprintError>` runs every check of `enroll` without writing, with the samples in `excluding` left out of the effective count; `enroll` calls it first.

| Writer | Origin | Repeat rule |
|---|---|---|
| `edit::remember_in` (`save_voice`, `remember_voices`) | `Meeting { meeting, speaker }` | replace (section 5.3) |
| Final pass `enroll_me` (`post/mod.rs`, only when `PostConfig::enroll_me`, off in every shipped build) | `Meeting { meeting, Me row id }` | skip if a live sample from that origin exists in the user's identity: a final pass never takes a sample back (decision 6), and the Me row id survives re-runs. Today each re-run adds again; this changes that. |
| `my_voice::enrol_my_voice` | `Own` | adds, as today; removed only by forgetting |

- Re-enrolling after "Forget voice" creates a new print.
- `vp_readout.rs` (its replacement lands in T12.77b3; it reads stored rows and calls no writer) today treats `entry.enrolled` as "confirmed" and, for such a voice, scores against the print without its enrolment (`without_enrolment`, an approximation of the subtraction). Its replacement: a voice is confirmed when a live sample from its `(meeting, speaker)` exists in the owner's identity, or the frozen marker (5.2) is set, or the row was edited (as today); with a live sample the leave-one-out is exact (`effective` of the print with that sample excluded); with the marker alone it keeps `without_enrolment`, labelled approximate in its report.
- `merge_persons`, `merge_into_me` and the one-Me convergence rule in `converge` move nothing: samples follow their print's person through `resolve_person`.
- `VoiceprintRepo` gains `samples(model)`, `samples_of(print)`, `samples_of_person(person)`, `samples_from(meeting, speaker)` and `put_sample`, with one shared contract suite run against `InMemoryVoiceprintRepo` and the store (`crates/earshot-store/src/repo.rs`, the `impl VoiceprintRepo` macro); `ReadOnlyStore` refuses `put_sample`. Reads return tombstones; callers filter.

## 5. "Is this voice saved"

### 5.1 Live samples are the only answer

"This meeting's voice is saved for X" is exactly: a live sample with origin `(meeting, speaker)` whose resolved person is X. Nothing else answers it: samples sync, and nothing device-local can contradict a take back made on another computer.

### 5.2 `enrolled` is a frozen pre-samples marker

`SpeakerIdentity::enrolled` (device-local, `crates/earshot-store/src/identities.rs`) means only "this voice was saved on this computer before samples existed (untracked)". From 0.4.0 nothing writes `enrolled = true`. It is reset to `false` only where today's code already resets it (`link_in` to another person, `mark_me`, `unmark_me`, and the final pass writing a new entry). It is read only through `voiceprint::saved_before_samples` (the marker, no live sample, and `Effective::has_base()`), the one predicate behind the pre-samples guard of 5.3 step 1 and the `saved_before_samples` flag of 6.1. It never makes a voice with a live sample read as unsaved, or one without as saved, on its own. One edge is accepted (2026-11-18): "Not" on a pre-0.4.0 saved voice keeps the marker, a later relink to the same person clears it (`link_in`), and a Save after that counts that pre-0.4.0 meeting twice (its base and a new sample); recording the marker's person could not help existing markers, and keeping the marker on any relink would answer `AlreadySaved` for a different person.

### 5.3 Save and re-save

`remember_in` for a voice linked to X, with `old` = the live samples from `(meeting, speaker)` whose resolved person is X:

1. `old` non-empty and one vector bit-equals `unit(entry.centroid)`: `Remembered::AlreadySaved` (renamed from `AlreadyEnrolled`), nothing written. Also `AlreadySaved`, with nothing written, when `voiceprint::saved_before_samples` holds: this computer's entry has the frozen marker (5.2), no live sample from the origin exists, and X's live print of the model has a base, read through the core accessor `Effective::has_base()` (non-zero base count), so `edit.rs` never reads the base itself. A print forgotten and created again has a zero base, so the same voice then saves anew. Tests for both.
2. Otherwise, admissibility first: the model's voiceprint threshold (`Uncalibrated` otherwise) and `admissible(.., excluding: old)`. On any refusal (`NotEnoughSpeech`, `Uncalibrated`) or error, nothing is taken back: `old` stays and counts, the result says why, and an error rolls the whole `edit::write` transaction back.
3. Admissible: `old` is taken back and the new centroid enrolled in the same transaction. The result is `Remembered::Replaced { person, voiceprint }` when `old` was non-empty, else `Enrolled`. One sample per voice per meeting per person; never counted twice.

- A second Save of the same voice with the same centroid writes nothing (step 1); a test saves twice and finds one live sample.
- Save with the row's own person (`PersonChoice::Existing` of the person it is already linked to) changes no link (`link_in` writes nothing, `set_row_person` is not called) and goes straight to steps 1 to 3, with `excluding` = that person's samples from this origin.
- Save with `PersonChoice::New` creates the person inside the same `edit::write` transaction and judges admissibility against them there (no print yet, so `min_new_speech` applies to the voice's speech). A refusal keeps the new person and the link, as `save_voice` does today, and saves nothing; an error rolls back the person too.
- A voice saved before samples whose print has since lost its base (forgotten and created again) saves as a new sample. While the base is there, step 1 answers `AlreadySaved`, so a pre-samples save is not counted twice.

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
- `saved_before_samples` uses the predicate of 5.3 step 1, `voiceprint::saved_before_samples(repo, entry, person, model)`, one function for both: this computer's entry has the frozen marker (5.2), no live sample from the origin exists in any identity, and the person's live print of that model has a base (`Effective::has_base()`). Only then does the copy say the voice stays in the print (section 10). A test covers the Forget-elsewhere case: the print was forgotten on another computer and a new one created (zero base), so "Not" shows no "saved before" copy although the marker is still set here.
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

### 7.1 Until the last sub-task: the push hold

The hold exists from T12.77b4 with the production switch off (tests drive it on), is switched on in T12.77b6, the sub-task that first stops adding into `voiceprint.sum`, and is removed in T12.77b8. Before T12.77b6 enrolments still add into `sum` (today's rule), no sample exists outside tests, and pushes are unchanged.

- **A runtime push-only value, no store list.** `sync::engine` gains `PushHold { Off, Samples }`, passed as a parameter to `push` and to the waiting count (one production constant chooses it: `Off` in T12.77b4 and b5, `Samples` from T12.77b6; T12.77b8 removes the type). With `Samples`, `push` asks `outbox_except` with the device's kinds with `voiceprints` turned off, which leaves out in SQL every `voiceprint` and `voiceprint_sample` row; then, when the device's voiceprint switch is on, it pushes the outbox's **voiceprint tombstones** through a new read, `Store::outbox_tombstones(table, limit)` (only `deleted = 1` rows of that table, in the outbox order). Live prints and every sample row (live or tombstone) wait in `sync_outbox`. No existing store function, list or assertion changes (ground rule 4).
- **The tombstone read runs even when the ordinary page is empty.** A tick in which a print tombstone is the only sendable row pushes it (a tick test pins exactly that case).
- **Why tombstones go.** A print tombstone is byte-identical in shape to what 0.3.x writes, so a Forget made on a main build reaches 0.3.x computers at once and is final (7.4). A live 0.4.0 print (zero base, count 0) would read as unscorable on 0.3.x, so it waits.
- **Accepted residue:** a live print in today's shape (a 0.3.x-era enrolment made on this computer) still unsent when the hold build first runs also waits until the raise; nothing is lost, it is late.
- **One waiting count.** "Waiting to send" is one shared SQL count, `Store::outbox_len_held(meetings, kinds, hold)` (the ordinary rows `outbox_except` would read under the hold's kinds plus the outbox's voiceprint tombstones), used by both `push`'s paging and the Sync page's status, so the page never shows held rows as stuck and never differs from what a push sends (a test compares the count with what a tick pushes).
- `ENVELOPE_FORMAT` stays 1. A runtime test with samples, a new print and a print tombstone in the store runs a sync tick against `LocalRelay` with the hold on: the tombstone is pushed, no live print and no sample row is, and the vault's format is unchanged.

So main builds are safe to run against the real vault until T12.77b8 merges, and T12.77b8 merges only with the release that ships samples. Rows held before the raise stay in the outbox and push after it (7.2).

### 7.2 The last sub-task: entity, skip before hold, backlog

- `SyncedRow::decode` maps the entity (the `SyncedRow::VoiceprintSample` variant and its store arms exist from T12.77b2; until now a pulled sample row is held as `Format`); decode runs `VoiceprintSample::from_stored` (2.1): a live row that fails is held as `Format`, a tombstone is stored as `body: None`.
- `PushHold` is removed. The backlog of samples, sample tombstones and prints written before the raise then pushes in the first ticks after it (a test: rows written with the arm off push after the change, oldest first, and apply on a second device to the same effective print).
- A row of a switched-off kind is skipped before it can be held (advisory A2): `Incoming` gains the envelope's `entity` name (read from the opened plaintext even when the record does not decode), and `apply_remote` checks `rules.kinds.syncs(entity)` before the `Format` hold. A worker test (voiceprint sync refused, `crates/earshot-worker/src/worker.rs` `refuse_voiceprints`) applies a page holding samples and prints, valid and malformed, and finds nothing stored and nothing in `sync_pending`.

### 7.3 The vault format is raised (decision 1)

- `earshot_sync::ENVELOPE_FORMAT` (`crates/earshot-sync/src/seal.rs`) goes from 1 to 2 in T12.77b8.
- The existing path: every push carries the format in header `Earshot-Sync-Format` (`earshot_sync::wire::FORMAT_HEADER`) and body (`PushRequest::format`, `sync/engine.rs`); the relay's `Db::push` (`crates/earshot-sync-server/src/db.rs`) raises `vault.format` to `max(format, 2)` in the transaction that stores the rows; `check_format` (`crates/earshot-sync-server/src/lib.rs`) answers a format-1 device's push with 409 `ErrorWord::UpdateNeeded`, which the client maps to `SyncProblem::UpdateNeeded` (`crates/earshot-runtime/src/sync/client.rs`; `stops()` is true) and the Sync page shows as "Another device uses a newer Earshot. Update Earshot on this computer to keep syncing." (`apps/desktop/src-tauri/src/sync_settings.rs`). A pull answer's `vault_format` above the build's is the same problem (`engine.rs`, `pull`).
- Pull today checks the format (`check_format`) before `Db::pull` reads rows, outside any transaction, so a push that raises the format between the two could hand a format-1 device a sample (advisory A3). T12.77b8 moves the check inside `Db::pull`: one read transaction reads `vault.format`, refuses with `update_needed` when the caller's format is below it, and only then reads rows. With the raise and the rows committed together on push, a format-1 device never opens or stores a sample. A relay test races a format-2 push against format-1 pulls.
- The vault rises at the first push from any 0.4.0 device, samples or not; one updated computer locks out every older one at once (the owner's decision).
- An `earshot-worker` on the older image stops with `update_needed`; under compose's restart policy it starts, is refused and stops again until its image is updated. T12.77b8 adds that note to docs/sync.md ("Running the worker").
- Rows of format 1 stay readable (`open_row` refuses only a format above the build's). No existing entity changes shape.
- The AGENTS.md bullets that keep `ENVELOPE_FORMAT` at 1 for a new entity gain the exception (the owner, 2026-11-18); the coordinator edits AGENTS.md, T12.77b8 the doc comment and `docs/sync-spec.md` sections 3 and 10.

### 7.4 Tombstones, repairs and what the window hears

Pieces with only local effects come in the erasure sub-task (T12.77b3), before any production sample exists, because prints sync today:

- **Print tombstones are final** in `apply_remote`, as meeting tombstones are: a live `voiceprint` row never overwrites a local print tombstone, and one with a later stamp gets the tombstone re-stamped (it goes out again and wins at the relay). A pulled print tombstone is stored with its `sum` zeroed.
- **Dead-print sample repair** in `converge`, in the same transaction: a live sample whose print is tombstoned here is tombstoned (section 2).
- `Applied::biometrics_forgotten` counts every pulled print tombstone that replaced a live local row and every repair erasure, so the sync engine (`engine.rs`, `if applied.biometrics_forgotten > 0`) runs `flush_erasures`.
- Tests for the two sequences the v4 review traced: (1) a Forget made here, then a 0.3.x computer that still had the print writes it live with a later stamp: the live copy arrives, the tombstone stays and is re-stamped, the next push is accepted and the 0.3.x computer applies the tombstone; (2) a print tombstone pulled while this computer holds samples of that print: the samples are erased in the same apply transaction, counted, and flushed. Also: a tombstone pushed while the relay holds a later live copy is answered superseded, the pull brings the live copy, finality re-stamps the tombstone and the next push wins.

Pieces that need the entity on the wire come in T12.77b8:

- Sample tombstones are final in `apply_remote` the same way.
- Dead-person repair in `converge`: a live sample whose `person_id`, followed through `merged_into` to the end of its chain, reaches a tombstoned person is tombstoned; so is a live print whose person, followed the same way, is tombstoned. A print or person not yet known here leaves the row alone; the fold ignores samples of an unknown print. (Before T12.77b8 a pulled person tombstone already erases its samples through `forget_identity_biometrics`; only samples arriving later need this.)
- `biometrics_forgotten` also counts pulled sample tombstones that replaced a live row.
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

Result fields carry counts and flags; the words live in the UI (`packages/ui/src/detail/`), in a case table `fixtures/ui/voice_sample_copy_cases.json` read by the UI test and by a Rust test that pins `since` (the version in the copy, 0.4.0) to be at least the workspace version, so it can never name a release already past; T12.77b8, which merges only with the release bump, pins it equal to the workspace version. `{name}` and `{other}` are filled with a function replacer (a name can hold `$&`).

| Case | Text |
|---|---|
| Not, `taken_back > 0` | "Unlinked. This meeting's voice is no longer part of {name}'s saved voice." |
| Not, `saved_before_samples` (agreed, decision 5) | "Unlinked. This voice was saved before Earshot {since}, so it stays part of {name}'s saved voice. To start their voice over, remove them on the People page." |
| Not, otherwise | "Unlinked." |
| Save that moved a sample (6.2 step 3) | "Saved for {other}. This meeting's voice is no longer part of {name}'s saved voice." |
| Link away or a refused move, `taken_back > 0` (6.2 step 4) | "Linked to {other}. This meeting's voice is no longer part of {name}'s saved voice." |
| This is me, `taken_back > 0` | "Marked as you. This meeting's voice is no longer part of {name}'s saved voice." |
| Merge, `samples_taken_back > 0` | "Merged. The merged voice is no longer part of {name}'s saved voice: save the voice again to add it." |

The owner agreed this table and the tooltip below as written (2026-11-18, T12.77b7). The "Not <name>" tooltip (`notThisPersonHint`, `SpeakersPanel.tsx`) says "Shows this voice as an unnamed speaker in this meeting, stops asking for its name and takes this meeting's voice out of {name}'s saved voice." when a live sample exists, today's text otherwise. The page-name guard (`page_names_tests.rs`, `pageNames.test.ts`) gets a source-pinning case for "People page".

## 11. Privacy

- Each sample is one more stored embedding (about 2 KB raw at 512 values, about 12 KB as sync JSON): one per saved voice, plus one per final pass for Me where `enroll_me` is on (no shipped build), plus one per "Your voice" sample, kept as long as its print.
- A live sample tied to a meeting id says which meeting a person spoke in, which a centroid alone does not; its tombstone says nothing (section 2).
- Samples sync only when voiceprint sync is on (off by default), sealed like every row; no build without the entity ever receives one (7.3); a computer with voiceprint sync off never hears of a forget or a "Not" made elsewhere (section 1).

## 12. Tests that pin each rule

- Core: `effective` bit-exact and independent of input order; equal to literal sums from the commit before; skips wrong model, wrong dimension, non-finite and wrong-person samples; saturation; an empty print neither scored nor counted in `skipped_invalid`; an enrolment never changes the stored base; effective count 0 needs `min_new_speech`; `admissible` with `excluding`; `take_back` restores the effective print exactly and respects `keep`; a tombstone has no body; `forget_voiceprints` erases samples whose print is missing and counts them; `check` at each boundary (dimension 0, 1, `MAX_EMBEDDING_DIM`, +1; norm at the tolerance; speech at the limit; one origin id set); `Debug` without the vector; `Voiceprint` wire JSON byte-identical to a literal; the shared `VoiceprintRepo` contract.
- Store: v8 fixture pin and migration test; trigger, synced-table and `CHECK` pins; on-disk erasure (vector and id bytes gone, second connection open) after Not, link away, mark me, merge, forget, remove person and a pulled sample tombstone; a pulled print tombstone stored with a zero sum; the vector bit-equal through seal, open, store and read.
- Sync before samples (T12.77b3): print tombstone final against a later live copy (re-stamped, re-sent, applied by the other side); a held tombstone superseded at the relay by a later live copy wins on the next push; a pulled print tombstone erases this computer's samples of it in the same transaction, counted and flushed. Under the push hold (driven on in T12.77b4 tests, on in production from T12.77b6): voiceprint tombstones pushed, no live print or sample row pushed, "waiting to send" without held rows, vault format unchanged. At the raise (T12.77b8): a format-1 client refused `update_needed` on push and on a pull racing a format-2 push; format-1 rows still open; the backlog pushes after the raise; Not racing a Save in both push orders and both clock orders (the taken-back sample never returns; the other computer's new sample stays, 7.5); sample and print tombstones final against later live copies; a late sample or print of a person tombstoned here, directly or through a `merged_into` chain, erased and flushed; each malformed pulled sample held as `Format`; a pulled tombstone with a body stored without one; a concurrent enrolment into an emptied print counts; a switched-off kind skipped, valid or not, before any hold (worker test); `Applied::people` and `meetings` for sample rows; `cargo xtask accept m11`.
- Added in v6: `Effective::has_base()`; `enroll` and `enroll_sample` bit-equal on the same inputs; `saved_before_samples` false after a Forget elsewhere and a new zero-base print; a tick whose only sendable row is a print tombstone pushes it; the shared waiting count equals what a tick sends; `outbox_except` and `outbox_len_except` agree with sample rows present; the scan's fixtures (hits in code, none in strings or comments, `meta.deleted = true` on a speaker caught).
- Runtime: each edit in 6.1 takes back with the right `keep` and reports its count; the `set_row_person` scan test; Save on a wrongly named voice in every branch of 6.2; re-save `AlreadySaved` by sample, `Replaced`, and each refusal leaving the old sample live; a repeat Save writing no second sample; the pre-samples guard (marker and non-zero base: `AlreadySaved`; zero base: saves anew); Save with the row's own person; Save with a new person refused and accepted; `enroll` choosing the earliest of two live prints; `saved_before_samples` only with no live sample and the marker; the marker never written true; Not without an identity entry but with a sample; a re-run that carries the row still takes back, one that tombstones it leaves the sample counting; final-pass `enroll_me` skips a second sample; meeting deletion leaves samples untouched; `my_voice`, `user_has_voiceprint` and `list_persons_in` read effective counts; no vector in any IPC result; the base scan test over crates and the desktop.

## 13. Changes in v6

Earlier rounds (summary): v4 made live samples the answer to "is this voice saved", froze `enrolled`, cleared sample tombstones to id and metadata, put admissibility before any take back and moved sample sync and the format raise last; v5 replaced the store-side push list with a runtime hold that lets print tombstones go, moved print-tombstone finality and dead-print repair before any sample exists, and put the writer change with the repeat guard and every take back.

v6 (the v5 review was clear; its advisories):

1. `saved_before_samples` uses the same predicate as 5.3 step 1 (`voiceprint::saved_before_samples`: marker, no live sample, a live print with a base); the Forget-elsewhere case is tested (5.2, 5.3, 6.1).
2. The old b4 is split: the push-hold mechanism with the production switch off (b4); the take backs, inert (b5); the writer switch with the hold on (b6); copy (b7); sync on and the format raise (b8) (14).
3. The evaluation callers stay on `enroll` until b6; b3 pins `enroll` and `enroll_sample` bit-equal; only `vp_readout`'s "confirmed" replacement stays in b3 (4).
4. The tombstone read runs on an otherwise empty page; one shared SQL waiting count for push and the status; a live today-shape print unsent when the hold first runs waits until the raise, accepted (7.1).
5. b2 adds the store-side arms (`synced_by_id`, `put_synced`) and the `SyncedRow::VoiceprintSample` variant, so the store is total over `SYNCED_TABLES`, with the wire decode still last; a test that `outbox_except` and `outbox_len_except` agree (14).
6. `Effective::has_base()` is the one base check outside the fold; the scan blanks strings and comments before matching and also catches `meta.deleted = true` on a speaker (3.3, 5.3).

## 14. Split

Each sub-task is under about 800 changed lines, merges alone with `cargo xtask ci` green and gets a reviewer (data model, migration, sync and biometric deletion: AGENTS 3.5). The likely biggest is T12.77b6 (the writer switch: every production and evaluation caller of `enroll`, the whole of 5.3 and 6.2 and their tests); T12.77b5 is next. Nothing stops adding into `voiceprint.sum` before T12.77b6, which arrives after the take backs (b5) and the hold (b4) already exist and switches the hold on, so no merged state can write a second sample, leave a relinked voice's sample in place, or push a live print or sample. T12.77b1 to b7 keep every live print and sample on the computer that wrote it, so main builds are safe with the real vault; T12.77b8 turns sample sync on, raises the vault format and merges only with the release.

- [ ] T12.77b1 Voiceprint samples: the core model and the fold
  - deps: T12.77
  - do: (1) `VoiceprintSample` (`body: Option<SampleBody>`, private fields, `new` / `erased` / `from_stored` through `check`, `Debug` without the vector), `MAX_EMBEDDING_DIM`, `MAX_SAMPLE_SPEECH_MS`, `SAMPLE_NORM_TOLERANCE`, `SyncClass` / `SyncSerialize` (`voiceprint_sample`, opt-in) and the trybuild snapshot. (2) `effective`, `Effective::{centroid, score, has_base}`, `live_prints`, `live_prints_of`, `admissible`; `Voiceprint` fields private with `from_stored` and `base_*`, its wire JSON pinned to a literal; `centroid` and `score` private; empty prints left out of matching and of `skipped_invalid`. (3) Every reader in docs/voice-samples.md 3.2 moved to the effective print (folding no samples yet), the base scan test over `crates/` and `apps/desktop/src-tauri/`. No storage, no writer change.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-store`; `cargo test -p earshot-runtime voiceprint_base`; `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-onnx voiceprint_calibration`; `cargo xtask accept m3` (every `vp.*` and `dia.*` value identical to the committed `acceptance/results/m3.json`); `cargo test --release -p earshot-runtime --example vp_meetings vp_meetings_the_rule_still_picks_the_shipped_margin -- --ignored`; `cargo xtask purity`

- [ ] T12.77b2 Voiceprint samples: store table, migration and repository
  - deps: T12.77b1
  - do: (1) `VoiceprintRepo::{samples, samples_of, samples_of_person, samples_from, put_sample}` on `InMemoryVoiceprintRepo`, the store and `ReadOnlyStore` (refuses writes), one shared contract suite. (2) Migration 9 (table with the tombstone `CHECK`, indexes, outbox triggers), `SCHEMA_VERSION` 9, `APPLIED`, `SYNCED_TABLES`, `tests/data/schema_v8.sql` with its pin and a v8-to-v9 test. (3) The store is total over `SYNCED_TABLES`: the `SyncedRow::VoiceprintSample` variant and its `synced_by_id` and `put_synced` arms, and the new arm `SyncKinds::syncs("voiceprint_sample") => self.voiceprints`; `SyncedRow::decode` does not map the entity yet (wire decode is T12.77b8), so a pulled sample row is still held as `Format`; a test that `outbox_except` and `outbox_len_except` agree with sample rows present. (4) `live_prints` folds stored samples; `name_speakers`' snapshot copies them. (5) The vector bit-equal through the store's write and read. No writer creates samples.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-store`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime sync`

- [ ] T12.77b3 Voiceprint samples: erasure, print tombstones on apply, and the core writer
  - deps: T12.77b2
  - do: (1) `voiceprint::take_back`; `forget_voiceprints` tombstones samples by person and by print and returns `Forgotten { voiceprints, samples }`, counted by `forget_person_voices`, `forget_my_voice` and every flush decision. (2) Print tombstones final in `apply_remote` (re-stamped when a later live copy arrives), pulled print tombstones stored zeroed, the dead-print sample repair in `converge`, `biometrics_forgotten` counts and the flush. (3) `voiceprint::enroll_sample` (origin, `admissible` first, zero-base print when there is none, earliest of two live prints, speech split exactly) and `voiceprint::saved_before_samples`, used only by tests; a core test that `enroll` and `enroll_sample` on the same inputs give bit-equal effective prints; the literal-sum test. (4) `vp_readout`'s "confirmed" replacement (docs/voice-samples.md 4). (5) On-disk erasure tests (vector and id bytes gone) for forget, remove person, `take_back` and a pulled print tombstone; the two traced sequences and the superseded-at-the-relay test (7.4).
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-store`; `cargo test -p earshot-runtime sync`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime edit`; `cargo test --release -p earshot-runtime --example vp_readout`

- [ ] T12.77b4 The voiceprint push hold, off in production
  - deps: T12.77b3
  - do: (1) `sync::engine::PushHold { Off, Samples }` as a parameter of `push` and of the waiting count, chosen by one production constant set to `Off`. (2) `Store::outbox_tombstones(table, limit)` and the shared waiting count `Store::outbox_len_held(meetings, kinds, hold)`, used by push and by the Sync page's status. (3) Tests drive the hold on: a tick with samples, a live print and a print tombstone pushes only the tombstone and leaves the vault format unchanged; a tick whose only sendable row is a print tombstone pushes it; the waiting count equals what a tick sends; with `Off` every push is as today.
  - accept: `cargo test -p earshot-runtime sync`; `cargo test -p earshot-store sync`; `cargo test -p earshot-desktop --bin earshot sync`

- [ ] T12.77b5 Take backs on relink, Not, mark me and merge (inert until samples are written)
  - deps: T12.77b4
  - do: (1) `edit::set_row_person` and `edit::retire_row` (`take_back` inside), used by `not_this_person`, `link_in`, `mark_me`, `unmark_me` and `merge_speakers`; counts in their results (`taken_back`, `Merged::samples_taken_back`, `MarkedMe`, `NotThisPerson`), `flush_erasures` after the commit and `checkpoint_pending`. (2) The scan test `edit_speaker_person_changes_only_through_set_row_person` with its blanking pass, patterns and exception list (docs/voice-samples.md 3.3). (3) `not_this_person` and `can_not_this_person` accept a row with a live sample and no entry; `saved_before_samples` through `voiceprint::saved_before_samples`, the Forget-elsewhere test. (4) The desktop maps the new result types with today's copy. Tests make samples with `enroll_sample`; production writes none yet.
  - accept: `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-store`; `cargo test -p earshot-desktop --bin earshot detail`

- [ ] T12.77b6 Every enrolment writes a sample, and the push hold goes on
  - deps: T12.77b5
  - do: (1) `enroll_sample` replaces `enroll` for every caller (production, `diar_eval`, `vp_meetings`, `voiceprint_calibration`, tests); `enroll_me` takes `SampleOrigin`; `Voiceprint::add` removed; origins for `remember_in`, `enrol_my_voice` and final-pass `enroll_me` (skip a repeat); nothing writes `enrolled = true`. (2) `remember_in` by docs/voice-samples.md 5.3 as a whole: `AlreadySaved` from live samples and the pre-samples guard, admissibility before any take back, `Replaced`, own person, new person, the repeat-Save test. (3) `save_voice` moves the sample by 6.2, every branch tested. (4) The production `PushHold` constant set to `Samples`. (5) The desktop maps the changed `Remembered` (`AlreadySaved`, `Replaced`) with today's copy.
  - accept: `cargo test -p earshot-core voiceprint`; `cargo test -p earshot-runtime edit`; `cargo test -p earshot-runtime post`; `cargo test -p earshot-runtime my_voice`; `cargo test -p earshot-runtime sync`; `cargo test -p earshot-store`; `cargo test -p earshot-desktop --bin earshot detail`; `cargo test -p earshot-onnx voiceprint_calibration`; `cargo xtask accept m3` (every `vp.*` and `dia.*` value identical to the committed `acceptance/results/m3.json`); `cargo test --release -p earshot-runtime --example vp_meetings vp_meetings_the_rule_still_picks_the_shipped_margin -- --ignored`

- [ ] T12.77b7 Voice sample copy in the app
  - deps: T12.77b6
  - do: (1) The owner reviews the docs/voice-samples.md section 10 copy first. (2) IPC results carry the counts and flags (`not_this_person`, `link_speaker`, `save_voice`, `mark_me`, `merge_speakers`; `AlreadySaved`, `Replaced`), bindings regenerated, a test that no IPC result serialises a vector. (3) `fixtures/ui/voice_sample_copy_cases.json` with the agreed texts, read by the UI test and a Rust test pinning `since` to the workspace version; `notThisPersonHint` by whether a sample exists; the People page source-pinning case. (4) The Speakers panel in the Vite preview in light and dark.
  - accept: `cargo test -p earshot-desktop --bin earshot detail`; `cargo test -p earshot-desktop bindings`; `cmd /c pnpm --filter @earshot/ui test`; `cmd /c pnpm -r build`

- [ ] T12.77b8 Voiceprint samples: sync on and the vault format raise (merges only with the release)
  - deps: T12.77b7
  - do: (1) `SyncedRow::decode` maps `voiceprint_sample` through `from_stored`; `Incoming` carries the entity name and `apply_remote` skips a switched-off kind before any hold; sample tombstones final; the dead-person repair (following `merged_into`); `biometrics_forgotten` counts pulled sample tombstones; `Applied::people` / `meetings` for sample rows. (2) `PushHold` removed; the backlog test (rows held before push after, oldest first, and give the same effective print on a second device). (3) `ENVELOPE_FORMAT` 2 (doc comment, docs/sync-spec.md); the format check inside `Db::pull`'s transaction with the race test; the worker restart note in docs/sync.md. (4) The sync tests of docs/voice-samples.md section 12 and the worker page test; report the AGENTS.md wording for the coordinator.
  - accept: `cargo test -p earshot-sync`; `cargo test -p earshot-sync-server`; `cargo test -p earshot-store sync`; `cargo test -p earshot-runtime sync`; `cargo test -p earshot-worker`; `cargo xtask accept m11`

## 15. Learned (for AGENTS.md)

- Where a review found a hold or exception that could be undone by an older build, the fix kept tombstones flowing: a push hold for a new data shape holds live rows only and lets tombstones of existing tables go, because a tombstone has the same shape in every build and must stay final.
- A writer change and the guards that keep it correct (repeat guard, take back on every edit that ends what the written row means) merge in the same sub-task or after the guards, never before; split elsewhere to stay under the size limit.
- A device-local flag never answers a question that synced rows can contradict; when a device-local marker must stay, it is frozen to what it meant before, read through one predicate together with the synced state, and that predicate has a test for the change made on another computer.
