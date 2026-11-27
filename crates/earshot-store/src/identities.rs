//! The speaker identity record (T6.25), device-locally (never synced, no migration): per
//! meeting, the embedding centroid of every speaker row the final pass wrote (except "Me"),
//! with the name and person the pass gave the row, so a later run of the cleanup can carry
//! the user's names over (`earshot_runtime::identities`, `earshot_runtime::post` step 4).
//! A voice the user marked as themselves in the room (`earshot_runtime::edit::mark_me`, T9.12)
//! keeps the entry it had, as an edit: the only "Me" row with one. Forgetting the user's voice
//! drops it like any entry of that identity, and the next pass then loses the mark.
//!
//! The stored value (local_state `speaker-identities/<meeting>`, [`record_key`]) is
//! `version (u32 LE) | JSON` ([`IdentityRecord`]); a record of another version or a damaged
//! payload is [`StoredIdentities::Unreadable`], never an error, and is never parsed before
//! its version is checked.
//!
//! **A user edit** is told apart from the pass's own naming by the row's stamp: a row whose
//! `meta.updated_at` differs from the one the pass left ([`SpeakerIdentity::given_stamp`])
//! was written after the pass (by the user, or by a Person rename rewriting the row, which
//! is the same thing), even when its name is back to the automatic one. A name the pass
//! carried as an edit stays one ([`SpeakerIdentity::user_edited`]). Version 1 records have
//! neither field and fall back to comparison: a row whose `display_name` or `person_id`
//! differs from what the pass gave it ([`SpeakerIdentity::given_name`],
//! [`SpeakerIdentity::given_person`]).
//!
//! Any later write to a speaker row changes its `updated_at` and so reads as a user edit:
//! a Person rename rewriting the row, and a future sync applying a remote write to it. The
//! consequence is a name that is kept, never one that is lost. A future sync must preserve
//! the recorded stamp (apply a remote write only when it changes the row) or re-stamp this
//! record in the same transaction.
//!
//! **The record is biometric data** (speaker embeddings, directly comparable with
//! voiceprints, PLAN.md 3.9). It stays on this device, is never synced, and lives exactly as
//! long as the meeting's final transcript: the final pass rewrites it in its write
//! transaction, dropping the entries of speakers it tombstoned. It is deletable per person
//! like voiceprints: [`crate::Store::forget_voiceprints`] drops, in the same transaction,
//! every entry of that identity ([`forget_identity`]). Deleting the meeting erases it: the
//! key is the runtime registry's `MeetingRecord::SpeakerIdentities` (`earshot_runtime`
//! `local.rs`), whose prefix is pinned to [`RECORD_PREFIX`] by a runtime test.

use std::str::FromStr;

use earshot_core::capture::TrackId;
use earshot_core::diar::Embedding;
use earshot_core::identity::{Clock, EntityId, Hlc, HlcTimestamp};
use serde::{Deserialize, Serialize};

use crate::{GuardedTx, StoreError};

/// The format of the stored record (version 2 adds [`SpeakerIdentity::given_stamp`] and
/// [`SpeakerIdentity::user_edited`]; [`SpeakerIdentity::enrolled`] and
/// [`SpeakerIdentity::ignored`] joined version 2 with defaults older readers ignore).
pub const IDENTITY_RECORD_VERSION: u32 = 2;

/// Versions [`decode`] reads: version 1 records (no stamp, no edit flag) read with those
/// fields at their defaults, so they fall back to comparing names and persons. A record of
/// any other version is not used.
pub const READABLE_VERSIONS: [u32; 2] = [1, IDENTITY_RECORD_VERSION];

/// The local_state key prefix of every record.
pub const RECORD_PREFIX: &str = "speaker-identities/";

/// One speaker row of an earlier final pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerIdentity {
    /// The `Speaker` row.
    pub speaker: EntityId,
    /// The track the speaker was found on.
    pub track: TrackId,
    /// The speaker's cluster centroid in that pass.
    pub centroid: Embedding,
    /// The display name the pass gave the row.
    pub given_name: String,
    /// The person the pass linked the row to.
    pub given_person: Option<EntityId>,
    /// The row's `meta.updated_at` as the pass left it (`None` in a version 1 record, and
    /// for an entry carried as [`SpeakerIdentity::user_edited`]).
    #[serde(default)]
    pub given_stamp: Option<HlcTimestamp>,
    /// The row was already a user edit when the pass wrote this entry (a carried name the
    /// pass kept): it stays an edit whatever the pass did to the row's role.
    #[serde(default)]
    pub user_edited: bool,
    /// This centroid was added to the linked person's voiceprint (a voice the user saved,
    /// `earshot_runtime::edit::save_voice`), so it is not added again. Since voice samples
    /// (T12.77b) nothing sets it: it is the frozen marker of a save made before samples
    /// existed, carried by a final pass to the same voice of the same person and cleared by
    /// linking the row to another person. Added within version 2 with a `false` default (also for
    /// version 1 records): earlier version-2 readers ignore the unknown field (no
    /// `deny_unknown_fields`), so the version stays 2 and records written by either build stay
    /// readable, with names carried, by both. A bump would only make earlier builds drop the
    /// record and the user's names with it.
    #[serde(default)]
    pub enrolled: bool,
    /// A legacy ignore (T12.19 to 0.4.2): the user chose to ignore this voice when the choice
    /// was kept here, device-locally. Since T12.93 the choice lives on the synced speaker row
    /// (`earshot_core::model::Speaker::ignored`). The one writer of `true` is a final pass that
    /// runs before this device's one-off copy, carrying a legacy flag into the new entry of the
    /// voice it continues (so the copy still finds it); it is read only until the copy has run
    /// (`earshot_runtime::ignore_copy`), and never after. Added within version 2 with a
    /// `false` default, as [`SpeakerIdentity::enrolled`] was.
    #[serde(default)]
    pub ignored: bool,
}

/// The one rule for an ignored voice (T12.19, T12.93; `earshot_runtime::edit::is_ignored`
/// passes its legacy set through it): only an unnamed voice that is not the user counts, with
/// the row's synced flag or, until the one-off copy ran, a legacy entry flag
/// (`legacy_entry_flag`). Applied to every read, because a synced row is a trust boundary.
#[must_use]
pub fn counts_as_ignored(row: &earshot_core::model::Speaker, legacy_entry_flag: bool) -> bool {
    row.role != earshot_core::diar::SpeakerRole::Me
        && row.person_id.is_none()
        && (row.ignored || legacy_entry_flag)
}

impl SpeakerIdentity {
    /// Whether `row` was changed after the pass that wrote this entry (module docs): a
    /// carried edit, an ignored row by the one rule ([`counts_as_ignored`]: the row's flag, or
    /// this entry's legacy flag [`SpeakerIdentity::ignored`] while `legacy_ignores` holds, that
    /// is until the one-off copy ran), a row written since the pass ([`SpeakerIdentity::given_stamp`]; so a rename
    /// back to the automatic name is still an edit), or, for entries without a stamp, a name
    /// or person different from what the pass gave it.
    #[must_use]
    pub fn edited(&self, row: &earshot_core::model::Speaker, legacy_ignores: bool) -> bool {
        self.user_edited
            || counts_as_ignored(row, legacy_ignores && self.ignored)
            || self.given_stamp.is_some_and(|s| row.meta.updated_at != s)
            || row.display_name != self.given_name
            || row.person_id != self.given_person
    }
}

/// A meeting's speaker identities (module docs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IdentityRecord {
    /// One entry per speaker row ("Me" excluded, but for a voice the user marked: module docs).
    pub speakers: Vec<SpeakerIdentity>,
}

/// A stored record as read back: usable, or why not.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredIdentities {
    /// The record.
    Record(IdentityRecord),
    /// A record of another version or that does not parse.
    Unreadable(String),
}

/// The local_state key of `meeting`'s record.
#[must_use]
pub fn record_key(meeting: EntityId) -> String {
    format!("{RECORD_PREFIX}{meeting}")
}

/// The stored form of `record` (module docs).
#[must_use]
pub fn encode(record: &IdentityRecord) -> Vec<u8> {
    // Plain data (ids, names, finite floats): serialising cannot fail; if it did, the version
    // alone is stored, which reads as unreadable.
    crate::encode_versioned(IDENTITY_RECORD_VERSION, record)
        .unwrap_or_else(|_| IDENTITY_RECORD_VERSION.to_le_bytes().to_vec())
}

/// Reads [`encode`]: the record, or why it cannot be used.
#[must_use]
pub fn decode(bytes: &[u8]) -> StoredIdentities {
    let Some((version, payload)) = crate::split_version(bytes) else {
        return StoredIdentities::Unreadable("the speaker identity record is truncated".into());
    };
    if !READABLE_VERSIONS.contains(&version) {
        return StoredIdentities::Unreadable(format!(
            "speaker identity record version {version}, this build reads {READABLE_VERSIONS:?}"
        ));
    }
    match serde_json::from_slice(payload) {
        Ok(record) => StoredIdentities::Record(record),
        Err(e) => StoredIdentities::Unreadable(format!("the speaker identity record: {e}")),
    }
}

/// Who started an erasure ([`forget_identity_biometrics`]): an identity entry that cannot be
/// placed is kept when it could be the user's only for a pulled person tombstone
/// (docs/sync-spec.md 16.5 item 20, 16.12 b); an erasure the user started drops it whatever it
/// might be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Erasure {
    /// Remove, Forget voice, Forget my voice: started by this device's user.
    User,
    /// A pulled person tombstone's side effect.
    Pulled,
}

/// Forgets every piece of biometric data of the identity `person` inside `g`
/// (docs/sync-spec.md 16.5 item 11): its speaker centroids in every meeting's record
/// ([`forget_identity`]), then its prints (tombstoned with the vector zeroed by rowid, a
/// vector that does not read included, 16.4) and its live samples (erased by id, or by rowid
/// when the id does not read), with no core call. The identity is [`crate::identity_in`]:
/// membership by `id` and `merged_into` only. The one path for
/// [`crate::Store::forget_voiceprints`], [`crate::Store::remove_person`] and a pulled person
/// tombstone. The caller checkpoints after the commit.
///
/// # Errors
/// Store errors other than a row that does not read.
pub fn forget_identity_biometrics<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    person: EntityId,
    hlc: &mut Hlc,
    clock: &C,
    who: Erasure,
) -> Result<earshot_core::voiceprint::Forgotten, StoreError> {
    let identity = crate::identity_in(g, person)?;
    forget_identity_voices(g, &identity, hlc, clock, who)
}

/// [`forget_identity_biometrics`] for an identity the caller has placed.
pub(crate) fn forget_identity_voices<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    identity: &crate::Identity,
    hlc: &mut Hlc,
    clock: &C,
    who: Erasure,
) -> Result<earshot_core::voiceprint::Forgotten, StoreError> {
    use crate::{Col, Deleted};
    let mut out = earshot_core::voiceprint::Forgotten {
        skipped: identity.skipped,
        ..Default::default()
    };
    let (entries, kept) = forget_identity(g, &identity.members, who)?;
    out.entries = entries;
    out.skipped += kept;
    let rows = g.voice_rows_of(&identity.members)?;
    // Prints in identity order, each member's by id (core `forget_voiceprints`' order).
    for p in rows.prints.iter().filter(|p| p.deleted != Deleted::Yes) {
        g.erase_print_by_rowid(hlc, clock, p.rowid)?;
        out.voiceprints += 1;
        if !p.sum_reads {
            out.zeroed += 1;
        }
    }
    // Samples in id order, then those whose id does not read.
    for s in &rows.samples {
        let erased = match s.id {
            Col::Read(id) => g.erase_sample_by_id(hlc, clock, id)?,
            Col::Unreadable => g.erase_sample_by_rowid(hlc, clock, s.rowid)?,
        };
        if erased {
            out.samples += 1;
        }
    }
    Ok(out)
}

/// Drops from every meeting's record the entries of the identity `persons` (a person and
/// everyone merged into it, [`crate::identity_in`]): entries the pass linked to one of them,
/// or whose speaker row links to one of them now. An entry that cannot be placed (its row's
/// `person_id` does not read, or it matches no readable row while the meeting holds a row
/// whose `id` does not read) is dropped too, except that for a pulled person tombstone
/// ([`Erasure::Pulled`]) one that could be the user's is kept (docs/sync-spec.md 16.5 item 20,
/// 16.12 b). A record that cannot be read is deleted whole (it may hold such entries and
/// carries nothing anyway), counting as one; a record left empty is deleted. Returns how many
/// entries were dropped and how many were kept because they could be the user's. Only the
/// device-local entry goes; no row is written. The caller checkpoints after the commit
/// (`secure_delete` zeroes the replaced pages).
///
/// # Errors
/// Store errors.
pub fn forget_identity(
    g: &mut GuardedTx<'_, '_>,
    persons: &[EntityId],
    who: Erasure,
) -> Result<(usize, usize), StoreError> {
    use crate::{Col, SpeakerLink};
    let keys = g.identity_record_keys()?;
    // Persons whose `is_me` reads `true` or does not read: a pulled tombstone keeps an entry
    // that could be theirs.
    let maybe_user: Vec<EntityId> = if who == Erasure::Pulled {
        g.person_links()?
            .into_iter()
            .filter(|l| l.is_me != Col::Read(false))
            .filter_map(|l| l.id.read())
            .collect()
    } else {
        Vec::new()
    };
    let mut dropped = 0;
    let mut kept = 0;
    for key in keys {
        let Some(bytes) = g.local_value(&key)? else {
            continue;
        };
        let mut record = match decode(&bytes) {
            StoredIdentities::Record(r) => r,
            StoredIdentities::Unreadable(_) => {
                g.delete_local_value(&key)?;
                dropped += 1;
                continue;
            }
        };
        let rows: Vec<SpeakerLink> = match EntityId::from_str(&key[RECORD_PREFIX.len()..]) {
            Ok(meeting) => g.speaker_links(Some(meeting))?,
            Err(_) => Vec::new(),
        };
        for r in &rows {
            if !r.id.reads() || !r.person_id.reads() {
                g.count_damaged("speaker", &[r.rowid]);
            }
        }
        let user_row = |r: &SpeakerLink| {
            matches!(
                r.role,
                Col::Read(earshot_core::diar::SpeakerRole::Me) | Col::Unreadable
            ) || match r.person_id {
                Col::Read(Some(p)) => maybe_user.contains(&p),
                Col::Read(None) => false,
                Col::Unreadable => true,
            }
        };
        let unplaced_rows: Vec<&SpeakerLink> = rows.iter().filter(|r| !r.id.reads()).collect();
        // `Some(true)` drop, `Some(false)` keep, `None` keep because it could be the user's.
        let decide = |i: &SpeakerIdentity| -> Option<bool> {
            if i.given_person.is_some_and(|p| persons.contains(&p)) {
                return Some(true);
            }
            let could_be_user = |row_says: bool| {
                who == Erasure::Pulled
                    && (row_says || i.given_person.is_some_and(|p| maybe_user.contains(&p)))
            };
            match rows.iter().find(|r| r.id == Col::Read(i.speaker)) {
                Some(r) => match r.person_id {
                    Col::Read(Some(p)) => Some(persons.contains(&p)),
                    Col::Read(None) => Some(false),
                    Col::Unreadable => {
                        if could_be_user(user_row(r)) {
                            None
                        } else {
                            Some(true)
                        }
                    }
                },
                None if unplaced_rows.is_empty() => Some(false),
                None => {
                    if could_be_user(unplaced_rows.iter().any(|r| user_row(r))) {
                        None
                    } else {
                        Some(true)
                    }
                }
            }
        };
        let before = record.speakers.len();
        let mut keep = Vec::with_capacity(before);
        for i in record.speakers {
            match decide(&i) {
                Some(true) => {}
                Some(false) => keep.push(i),
                None => {
                    kept += 1;
                    keep.push(i);
                }
            }
        }
        record.speakers = keep;
        let n = before - record.speakers.len();
        if n == 0 {
            continue;
        }
        dropped += n;
        if record.speakers.is_empty() {
            g.delete_local_value(&key)?;
        } else {
            g.set_local_value(&key, &encode(&record))?;
        }
    }
    Ok((dropped, kept))
}
