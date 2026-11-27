//! A summary that uses a speaker's name the user has since changed (T12.82).
//!
//! A summary is written right after a meeting, before voices are named, so it says "Speaker 2
//! will..." about a voice the user then names. Linking a voice, renaming it here or renaming
//! its person (the user's own name too, `crate::my_voice::set_my_name`), "Not <name>", "This
//! is me", "This isn't me" and removing a person each change a
//! speaker row's name. Such a change records the old name against the meeting's current
//! summary only when that summary uses it: a whole word, case folded with
//! [`crate::search::fold_case`], in any text the meeting search reads in a summary
//! ([`uses_name`]). The user's default name ([`DEFAULT_ME_NAME`], "Me") is never recorded: as a
//! word it is mostly the pronoun. A change of case alone is not a change of name.
//!
//! The summary is out of date ([`crate::view::MeetingView::summary_out_of_date`]) while its text
//! still uses a recorded earlier name that no live speaker row of the meeting has now
//! ([`out_of_date_by_names`], judged at every read): a name put back reads current again, and
//! a new summary (a re-summarise) has no recorded names. A summary an undo brings back keeps
//! its names.
//!
//! The record is the device-local `summary-names/<meeting>`
//! ([`crate::local::MeetingRecord::SummaryNames`]): `version u32 LE | JSON object` from summary
//! row id to the earlier names it used. It holds names (content); meeting deletion erases it
//! with the meeting's other records. One this version cannot read is no names (a lost name only
//! hides the note). Nothing new is synced: the name change travels as the synced speaker row,
//! and another computer applying that row judges it by the same rule against the name its own
//! copy had and against the summary that was current before the page ([`names_before`],
//! [`names_changed_tx`]), so a summary written after the rename and pulled in the same page is
//! never marked. A computer that never held the old name cannot tell, so it shows no mark.

use std::collections::{BTreeMap, BTreeSet};

use earshot_core::identity::EntityId;
use earshot_core::model::{MeetingRepo, Speaker, Summary, current_summary};
use earshot_core::summary::SummaryV1;
use earshot_core::voiceprint::DEFAULT_ME_NAME;
use earshot_store::{GuardedRead, GuardedTx, Store, StoreError, StoreTx};

use crate::RuntimeError;
use crate::local::{LocalKey, MeetingRecord};
use crate::search::{fold_case, summary_texts};

/// The record's version, written before its JSON payload.
pub const RECORD_VERSION: u32 = 1;

/// Summary row id to the earlier names it used.
pub type EarlierNames = BTreeMap<EntityId, Vec<String>>;

/// The record's key for `meeting`.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::SummaryNames.key(&[], meeting, &[])
}

/// The record's bytes for `names`.
///
/// # Errors
/// [`StoreError::Corrupt`] if the record does not serialise.
pub fn encode(names: &EarlierNames) -> Result<Vec<u8>, StoreError> {
    let text: BTreeMap<String, &Vec<String>> =
        names.iter().map(|(id, n)| (id.to_string(), n)).collect();
    earshot_store::encode_versioned(RECORD_VERSION, &text)
        .map_err(|_| StoreError::Corrupt("summary names encoding".into()))
}

/// A stored record; one this version cannot read is no names.
fn decode(bytes: &[u8]) -> EarlierNames {
    let Some((version, body)) = earshot_store::split_version(bytes) else {
        return EarlierNames::new();
    };
    if version != RECORD_VERSION {
        return EarlierNames::new();
    }
    serde_json::from_slice::<BTreeMap<String, Vec<String>>>(body)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, n)| Some((id.parse().ok()?, n)))
        .collect()
}

/// Whether two names are one name: trimmed, case folded.
fn same_name(a: &str, b: &str) -> bool {
    fold_case(a.trim()).eq(fold_case(b.trim()))
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

/// Whether `summary` is out of date by names: its text uses one of `earlier` (its recorded
/// earlier names) that no live row of `speakers` has as its name now.
#[must_use]
pub fn out_of_date_by_names(summary: &SummaryV1, earlier: &[String], speakers: &[Speaker]) -> bool {
    earlier.iter().any(|name| {
        uses_name(summary, name)
            && !speakers
                .iter()
                .any(|s| !s.meta.deleted && same_name(&s.display_name, name))
    })
}

/// Records `old` against `summary` when a speaker row of `meeting` went from `old` to `new`
/// and the summary uses `old` (module docs). Returns whether a name was recorded. A guarded
/// root (T14.57): it reads and writes only the record.
pub(crate) fn record_tx(
    g: &mut GuardedTx<'_, '_>,
    meeting: EntityId,
    summary: &Summary,
    old: &str,
    new: &str,
) -> Result<bool, StoreError> {
    if same_name(old, new) || same_name(old, DEFAULT_ME_NAME) || !uses_name(&summary.summary, old) {
        return Ok(false);
    }
    let key = record_key(meeting);
    let mut names = g
        .local_value(key.as_str())?
        .map(|b| decode(&b))
        .unwrap_or_default();
    let list = names.entry(summary.id).or_default();
    if list.iter().any(|n| same_name(n, old)) {
        return Ok(false);
    }
    list.push(old.trim().to_owned());
    crate::local::set_guarded(g, &key, &encode(&names)?)?;
    Ok(true)
}

/// Records, inside a naming edit's transaction, that a speaker row of `meeting` went from
/// `old` to `new`, against the meeting's current summary (module docs). Returns whether a name
/// was recorded. A user's naming edit is not a guarded path (docs/sync-spec.md 16.5, "left
/// out of scope"): a summary that does not read refuses the edit.
///
/// # Errors
/// Store errors.
pub(crate) fn name_changed_tx(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    old: &str,
    new: &str,
) -> Result<bool, StoreError> {
    let Some(current) = current_summary(tx.summaries(meeting)?) else {
        return Ok(false);
    };
    record_tx(&mut tx.guarded(), meeting, &current, old, new)
}

/// One meeting's current summary and the names of some of its live speaker rows, read before
/// a change made outside a naming edit (a pulled sync page, removing a person).
#[derive(Debug, Clone)]
pub(crate) struct NamesBefore {
    meeting: EntityId,
    summary: EntityId,
    names: Vec<(EntityId, String)>,
}

/// The names the live rows `rows` (meeting to speaker ids) have now, with each meeting's
/// current summary: one read of each per meeting. A meeting without a summary is left out; so
/// is one whose current summary does not read, and a speaker row that does not read loses only
/// its own mark (docs/sync-spec.md 16.5 item 14).
///
/// # Errors
/// Store errors.
pub(crate) fn names_before(
    g: &GuardedRead<'_>,
    rows: &BTreeMap<EntityId, BTreeSet<EntityId>>,
) -> Result<Vec<NamesBefore>, StoreError> {
    let mut out = Vec::new();
    for (meeting, speakers) in rows {
        let Some(Ok(summary)) = g.current_summary_or_damaged(*meeting)? else {
            continue;
        };
        let names: Vec<(EntityId, String)> = g
            .speakers_or_damaged(*meeting)?
            .0
            .into_iter()
            .filter(|s| !s.meta.deleted && speakers.contains(&s.id))
            .map(|s| (s.id, s.display_name))
            .collect();
        if !names.is_empty() {
            out.push(NamesBefore {
                meeting: *meeting,
                summary: summary.id,
                names,
            });
        }
    }
    Ok(out)
}

/// The live speaker rows here, by meeting, that a pulled page's removal rules can rename outside
/// the page's own speaker rows (docs/sync-spec.md 7A.9, "Names"): rows linked to a person that
/// is tombstoned, or whose `merged_into` chain ends at a tombstone that is not the user's (P4),
/// counting both the copy here and the page's copy of each person (P1's second case and P2 turn
/// a live person here into a tombstone; P3 renames the rows linked to one). On a page applied
/// with every repair (`every`) every such person counts; otherwise only one whose chain reaches
/// a person the page holds a row for, so a person merely held by the page (live, not chaining to
/// a tombstone) adds nothing. Picked by guarded reads (`person_links`, `speaker_links`); a row
/// whose id, meeting, link or `deleted` does not read is left out, and [`names_before`] skips a
/// damaged row of the meetings it gives (16.5 items 12 and 14).
///
/// # Errors
/// Store errors.
pub(crate) fn rows_removal_may_rename(
    g: &GuardedRead<'_>,
    page: &[earshot_store::Incoming],
    every: bool,
) -> Result<BTreeMap<EntityId, BTreeSet<EntityId>>, StoreError> {
    use earshot_store::{Col, Deleted, SyncedRow};
    let pulled: BTreeMap<EntityId, &earshot_core::voiceprint::Person> = page
        .iter()
        .filter_map(|inc| match &inc.row {
            Some(SyncedRow::Person(p)) => Some((p.id, p)),
            _ => None,
        })
        .collect();
    let mut out: BTreeMap<EntityId, BTreeSet<EntityId>> = BTreeMap::new();
    if pulled.is_empty() && !every {
        return Ok(out);
    }
    let here: BTreeMap<EntityId, earshot_store::PersonLink> = g
        .person_links()?
        .into_iter()
        .filter_map(|l| match l.id {
            Col::Read(id) => Some((id, l)),
            Col::Unreadable => None,
        })
        .collect();
    // Whether `start`'s chain ends at a removal (with the page's copies when `overlay`), and
    // whether it passed a person the page holds a row for. A loop, a person known nowhere or a
    // step that does not read ends it as no removal.
    let walk = |start: EntityId, overlay: bool| -> (bool, bool) {
        let mut seen = BTreeSet::new();
        let mut cur = start;
        let mut touched = false;
        loop {
            if !seen.insert(cur) {
                return (false, touched);
            }
            let local = here.get(&cur);
            let theirs = if overlay { pulled.get(&cur) } else { None };
            touched |= theirs.is_some();
            if local.is_none() && theirs.is_none() {
                return (false, touched);
            }
            // P4: the copy here decides `is_me` when it reads, else the pulled copy.
            let is_me = match local.map(|l| l.is_me) {
                Some(Col::Read(v)) => Some(v),
                _ => theirs.map(|p| p.is_me),
            };
            let tomb = local.is_some_and(|l| l.deleted == Deleted::Yes)
                || theirs.is_some_and(|p| p.meta.deleted);
            if tomb {
                return (is_me == Some(false), touched);
            }
            let next = match (theirs, local) {
                (Some(p), _) => p.merged_into,
                (None, Some(l)) if l.deleted == Deleted::No => match l.merged_into {
                    Col::Read(n) => n,
                    Col::Unreadable => return (false, touched),
                },
                _ => return (false, touched),
            };
            match next {
                Some(n) => cur = n,
                None => return (false, touched),
            }
        }
    };
    let persons: BTreeSet<EntityId> = here.keys().chain(pulled.keys()).copied().collect();
    let removed: BTreeSet<EntityId> = persons
        .into_iter()
        .filter(|p| {
            let (ends, touched) = walk(*p, true);
            if every {
                ends || walk(*p, false).0
            } else {
                ends && touched
            }
        })
        .collect();
    if removed.is_empty() {
        return Ok(out);
    }
    for s in g.speaker_links(None)? {
        if let (Col::Read(id), Col::Read(meeting), Col::Read(Some(person)), Deleted::No) =
            (s.id, s.meeting_id, s.person_id, s.deleted)
            && removed.contains(&person)
        {
            out.entry(meeting).or_default().insert(id);
        }
    }
    Ok(out)
}

/// Records every name of `before` that has changed since, against the summary that was current
/// then and only while it still is (a summary the change brought in is new and records
/// nothing). Run in the change's transaction after its writes. Returns how many names were
/// recorded. A current summary that does not read skips that meeting's marks, a speaker row
/// that does not read its own (docs/sync-spec.md 16.5 item 12).
///
/// # Errors
/// Store errors.
pub(crate) fn names_changed_tx(
    g: &mut GuardedTx<'_, '_>,
    before: &[NamesBefore],
) -> Result<usize, StoreError> {
    let mut recorded = 0;
    for b in before {
        let Some(Ok(current)) = g.current_summary_or_damaged(b.meeting)? else {
            continue;
        };
        if current.id != b.summary {
            continue;
        }
        let now = g.speakers_or_damaged(b.meeting)?.0;
        for (speaker, old) in &b.names {
            let Some(row) = now.iter().find(|s| s.id == *speaker && !s.meta.deleted) else {
                continue;
            };
            if record_tx(g, b.meeting, &current, old, &row.display_name)? {
                recorded += 1;
            }
        }
    }
    Ok(recorded)
}

/// The earlier names recorded for `meeting`'s summaries. A record this version cannot read
/// counts as none.
///
/// # Errors
/// Store errors.
pub fn read(store: &Store, meeting: EntityId) -> Result<EarlierNames, RuntimeError> {
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
        let names: EarlierNames = [
            (EntityId::from_bytes([1; 16]), vec!["Speaker 2".into()]),
            (
                EntityId::from_bytes([2; 16]),
                vec!["Alice".into(), "Rowan".into()],
            ),
        ]
        .into_iter()
        .collect();
        let bytes = encode(&names).unwrap();
        assert_eq!(decode(&bytes), names);
        let mut other = bytes.clone();
        other[0] = 9;
        assert!(decode(&other).is_empty());
        assert!(decode(b"ab").is_empty());
        assert!(decode(&[1, 0, 0, 0, b'[']).is_empty());
    }

    #[test]
    fn view_summary_names_record_bytes_are_pinned() {
        let names: EarlierNames = [(EntityId::from_bytes([1; 16]), vec!["Speaker 2".into()])]
            .into_iter()
            .collect();
        let mut want = 1u32.to_le_bytes().to_vec();
        want.extend_from_slice(br#"{"01010101-0101-0101-0101-010101010101":["Speaker 2"]}"#);
        assert_eq!(encode(&names).unwrap(), want);
    }

    #[test]
    fn view_summary_names_out_of_date_only_while_no_speaker_has_the_name_now() {
        let s = summary("Speaker 2 will book the venue.");
        let earlier = vec!["Speaker 2".to_owned()];
        let row = |name: &str| {
            let clock = earshot_core::identity::ManualClock::new(1);
            let mut hlc = earshot_core::identity::Hlc::new(
                earshot_core::identity::DeviceId::from_bytes([3; 16]),
            );
            let mut rng = <rand_chacha::ChaCha8Rng as rand_core::SeedableRng>::seed_from_u64(1);
            let mut stamp = earshot_core::model::Stamp {
                clock: &clock,
                hlc: &mut hlc,
                rng: &mut rng,
            };
            Speaker::new(
                EntityId::from_bytes([4; 16]),
                earshot_core::diar::SpeakerRole::Remote,
                name,
                None,
                &mut stamp,
            )
        };
        assert!(out_of_date_by_names(&s, &earlier, &[row("Rowan")]));
        assert!(
            !out_of_date_by_names(&s, &earlier, &[row("speaker 2")]),
            "the name is back"
        );
        let mut gone = row("Speaker 2");
        gone.meta.deleted = true;
        assert!(
            out_of_date_by_names(&s, &earlier, &[gone]),
            "a tombstone has no name now"
        );
        assert!(!out_of_date_by_names(
            &summary("The venue was booked."),
            &earlier,
            &[]
        ));
    }
}
