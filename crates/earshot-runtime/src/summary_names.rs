//! A summary that uses a speaker's name the user has since changed (T12.82).
//!
//! A summary is written right after a meeting, before voices are named, so it says "Speaker 2
//! will..." about a voice the user then names. Linking a voice, renaming it here or renaming
//! its person, "Not <name>", "This is me" and "This isn't me" each change a speaker row's
//! name; such a change marks the meeting's current summary out of date
//! ([`crate::view::MeetingView::summary_out_of_date`]) only when that summary uses the name the
//! row had before the change: a whole word, case folded with [`crate::search::fold_case`], in
//! any text the meeting search reads in a summary. A summary that never used the old name
//! stays current, so naming voices does not put the note on almost every meeting. A change of
//! case alone is not a change of name.
//!
//! The mark is the device-local record `summary-names/<meeting>`
//! ([`crate::local::MeetingRecord::SummaryNames`]): `version u32 LE | JSON list` of the summary
//! row ids marked, so a new summary (a re-summarise) is current again by having another id, and
//! a summary an undo brings back keeps its mark. A naming edit records it in its own
//! transaction ([`name_changed_tx`]). Nothing new is synced: the name change itself travels
//! as the synced speaker row, and another computer applying that row judges it by the same rule
//! against the name its own copy of the row had ([`names_changed_tx`], run in the applied
//! page's transaction). A computer that never held the old name cannot tell, so it shows no
//! mark. The record holds row ids only, no text; one this version cannot read is no marks (a
//! lost mark only hides the note). Meeting deletion erases it with the meeting's other records.

use std::collections::BTreeSet;

use earshot_core::identity::EntityId;
use earshot_core::model::{MeetingRepo, current_summary};
use earshot_core::summary::SummaryV1;
use earshot_store::{Store, StoreError, StoreTx};

use crate::RuntimeError;
use crate::local::{LocalKey, MeetingRecord};
use crate::search::{fold_case, summary_texts};

/// The record's version, written before its JSON payload.
pub const RECORD_VERSION: u32 = 1;

/// The record's key for `meeting`.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::SummaryNames.key(&[], meeting, &[])
}

/// The record's bytes for `ids`.
///
/// # Errors
/// [`StoreError::Corrupt`] if the list does not serialise.
pub fn encode(ids: &BTreeSet<EntityId>) -> Result<Vec<u8>, StoreError> {
    let text: Vec<String> = ids.iter().map(ToString::to_string).collect();
    earshot_store::encode_versioned(RECORD_VERSION, &text)
        .map_err(|_| StoreError::Corrupt("summary names encoding".into()))
}

/// The ids of a stored record; one this version cannot read is no ids.
fn decode(bytes: &[u8]) -> BTreeSet<EntityId> {
    let Some((version, body)) = earshot_store::split_version(bytes) else {
        return BTreeSet::new();
    };
    if version != RECORD_VERSION {
        return BTreeSet::new();
    }
    serde_json::from_slice::<Vec<String>>(body)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Whether `summary` uses `name`: as a whole word (no letter or digit right before or after
/// it), case folded, in any text the meeting search reads in a summary. An empty name is used
/// nowhere.
#[must_use]
pub fn uses_name(summary: &SummaryV1, name: &str) -> bool {
    let needle: Vec<char> = fold_case(name.trim()).collect();
    if needle.is_empty() {
        return false;
    }
    summary_texts(summary).any(|text| {
        let hay: Vec<char> = fold_case(text).collect();
        hay.windows(needle.len()).enumerate().any(|(i, w)| {
            w == needle.as_slice()
                && (i == 0 || !hay[i - 1].is_alphanumeric())
                && hay
                    .get(i + needle.len())
                    .is_none_or(|c| !c.is_alphanumeric())
        })
    })
}

/// Records, inside the naming edit's transaction, that a speaker row of `meeting` went from
/// `old` to `new` (module docs): when the meeting's current summary uses `old`, its id joins
/// the record. Returns whether it was marked now.
///
/// # Errors
/// Store errors.
pub(crate) fn name_changed_tx(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    old: &str,
    new: &str,
) -> Result<bool, StoreError> {
    if fold_case(old.trim()).eq(fold_case(new.trim())) {
        return Ok(false);
    }
    let Some(current) = current_summary(tx.summaries(meeting)?) else {
        return Ok(false);
    };
    if !uses_name(&current.summary, old) {
        return Ok(false);
    }
    let key = record_key(meeting);
    let mut ids = tx
        .local_value(key.as_str())?
        .map(|b| decode(&b))
        .unwrap_or_default();
    if !ids.insert(current.id) {
        return Ok(false);
    }
    crate::local::set_tx(tx, &key, &encode(&ids)?)?;
    Ok(true)
}

/// A live speaker row's name before a pulled page was applied: (meeting, speaker, name).
pub(crate) type NameBefore = (EntityId, EntityId, String);

/// The names this device's live copies of `rows` have now, read before a pulled page is
/// applied ([`names_changed_tx`]).
///
/// # Errors
/// Store errors.
pub(crate) fn names_before(
    store: &Store,
    rows: impl IntoIterator<Item = (EntityId, EntityId)>,
) -> Result<Vec<NameBefore>, StoreError> {
    let mut out = Vec::new();
    for (meeting, speaker) in rows {
        if let Some(row) = store
            .speakers(meeting)?
            .into_iter()
            .find(|s| s.id == speaker && !s.meta.deleted)
        {
            out.push((meeting, speaker, row.display_name));
        }
    }
    Ok(out)
}

/// [`name_changed_tx`] for every row of `before` whose name a pulled page changed, run in that
/// page's transaction after its writes. Returns how many summaries were marked.
///
/// # Errors
/// Store errors.
pub(crate) fn names_changed_tx(
    tx: &mut StoreTx<'_>,
    before: &[NameBefore],
) -> Result<usize, StoreError> {
    let mut marked = 0;
    for (meeting, speaker, old) in before {
        let Some(now) = tx
            .speakers(*meeting)?
            .into_iter()
            .find(|s| s.id == *speaker && !s.meta.deleted)
        else {
            continue;
        };
        if name_changed_tx(tx, *meeting, old, &now.display_name)? {
            marked += 1;
        }
    }
    Ok(marked)
}

/// The summary rows of `meeting` marked as using a changed name. A record this version cannot
/// read counts as none.
///
/// # Errors
/// Store errors.
pub fn read(store: &Store, meeting: EntityId) -> Result<BTreeSet<EntityId>, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting).as_str())?
        .map(|b| decode(&b))
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::summary::{ActionItem, Decision, NoteBullet, NoteSection};

    fn summary(tldr: &str) -> SummaryV1 {
        SummaryV1 {
            title: "Planning".into(),
            tldr: tldr.into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        }
    }

    #[test]
    fn view_summary_names_match_whole_words_case_folded() {
        let s = summary("Speaker 2 will send the plan; ALICE agreed.");
        assert!(uses_name(&s, "Speaker 2"));
        assert!(uses_name(&s, "alice"), "case folded");
        assert!(uses_name(&s, "  Alice "), "the name is trimmed");
        assert!(!uses_name(&s, "Speaker 21"));
        assert!(
            !uses_name(&s, "Speak"),
            "a whole word, not the start of one"
        );
        assert!(!uses_name(&s, ""));
        // Inside another word does not count, before or after.
        let s = summary("Samantha and Speaker 21 met at the Alexway office.");
        assert!(!uses_name(&s, "Sam"));
        assert!(!uses_name(&s, "Speaker 2"));
        assert!(!uses_name(&s, "Alex"));
        assert!(!uses_name(&s, "Way"));
        // Punctuation is a word boundary.
        assert!(uses_name(&summary("(Sam) said yes."), "Sam"));
        assert!(uses_name(&summary("Sam's plan."), "Sam"));
    }

    #[test]
    fn view_summary_names_are_read_in_every_text_search_reads_and_not_topics() {
        let mut s = summary("Nothing here.");
        s.topics = vec!["Robin".into()];
        assert!(
            !uses_name(&s, "Robin"),
            "topics are not shown with the summary"
        );
        let mut d = s.clone();
        d.decisions = vec![Decision {
            text: "Robin decides".into(),
            source_segment_ids: vec![],
        }];
        assert!(uses_name(&d, "Robin"));
        let mut a = s.clone();
        a.action_items = vec![ActionItem {
            task: "Send it".into(),
            owner: "Robin".into(),
            due: None,
            source_segment_ids: vec![],
        }];
        assert!(uses_name(&a, "Robin"));
        let mut n = s.clone();
        n.enhanced_notes = vec![NoteSection {
            heading: "Plan".into(),
            bullets: vec![NoteBullet {
                text: "Robin owns the plan".into(),
                source_segment_ids: vec![],
            }],
        }];
        assert!(uses_name(&n, "Robin"));
        let mut q = s;
        q.open_questions = vec!["Will Robin come?".into()];
        assert!(uses_name(&q, "Robin"));
    }

    #[test]
    fn view_summary_names_record_round_trips_and_refuses_another_version() {
        let ids: BTreeSet<EntityId> =
            [EntityId::from_bytes([1; 16]), EntityId::from_bytes([2; 16])]
                .into_iter()
                .collect();
        let bytes = encode(&ids).unwrap();
        assert_eq!(decode(&bytes), ids);
        let mut other = bytes.clone();
        other[0] = 9;
        assert!(decode(&other).is_empty());
        assert!(decode(b"ab").is_empty());
        assert!(decode(&[1, 0, 0, 0, b'{']).is_empty());
    }

    #[test]
    fn view_summary_names_record_bytes_are_pinned() {
        let ids: BTreeSet<EntityId> = [EntityId::from_bytes([1; 16])].into_iter().collect();
        let mut want = 1u32.to_le_bytes().to_vec();
        want.extend_from_slice(br#"["01010101-0101-0101-0101-010101010101"]"#);
        assert_eq!(encode(&ids).unwrap(), want);
    }
}
