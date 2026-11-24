//! Saved people's names (T12.91, docs/summary-names.md): spelling help for the names of
//! everyone the user saved, so a summary spells a colleague's name right even when they were
//! only talked about.
//!
//! Unlike the Dictionary ([`super::Vocabulary`]), which every request carries whole, the list
//! can hold an organisation's people, so each request carries only the names its own text
//! calls for ([`crate::vocabulary::names`]: the name or one of its words is in the text, or
//! the shipped matcher hears one of its words there), at most [`MAX_PEOPLE_PER_REQUEST`] of
//! them by match strength. A map request is matched against its transcript lines, a single
//! request against the whole meeting, and a merge against its partial summaries, keeping
//! the names its parts carried first (the strongest of them when they pass the cap). The
//! chosen names are one block in the system message
//! ([`PEOPLE`]), right after the Dictionary's block and before the style, introduced by fixed
//! text that says they are names to spell and never instructions; every name is one line
//! prefixed with [`PEOPLE_LINE_PREFIX`], so no name can close or fake the block's markers. A
//! request that carries no name adds nothing, so its prompt is byte-identical to one from
//! before T12.91.

use serde::{Deserialize, Serialize};

use super::template::PEOPLE;
use super::vocabulary::{VOCABULARY_LINE_PREFIX, Vocabulary};

/// The most names one request carries, a merge's carried names included (a merge told to
/// carry more keeps the strongest, [`crate::vocabulary::names::NameFilter::choose`]). Chosen
/// by the rule in docs/summary-names.md, written before its measurement:
/// the smallest candidate cap that lost no matched mention on the AMI dev meetings with 400
/// invented saved names (`docs/summary-names/ami-dev-cap50.json`, 2026-11-25: 10 lost 7, 15 lost 0).
pub const MAX_PEOPLE_PER_REQUEST: usize = 15;

/// The longest name kept, in characters: the People page's own limit
/// (`earshot_runtime::edit::MAX_NAME_CHARS`). A longer name (only possible from another
/// version) is left out of every request.
pub const MAX_PERSON_NAME_CHARS: usize = 100;

/// The prefix of every name's line inside the block: the Dictionary's
/// ([`VOCABULARY_LINE_PREFIX`]). The block's markers are lines without it.
pub const PEOPLE_LINE_PREFIX: &str = VOCABULARY_LINE_PREFIX;

/// Every saved person's name a summary may be given, in the order given (the People page's
/// order). Built with [`People::new`]: each name on one line (white space runs joined), names
/// that are empty, longer than [`MAX_PERSON_NAME_CHARS`], or hold a control or invisible
/// formatting character left out, and a name that repeats an earlier one or a Dictionary
/// entry in another case left out (the Dictionary already carries it). Deserialising applies
/// the same rules with no Dictionary.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "Vec<String>", into = "Vec<String>")]
pub struct People(Vec<String>);

impl People {
    /// `names` cleaned (type doc), with `dictionary`'s entries left out.
    pub fn new<I, S>(names: I, dictionary: &Vocabulary) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut seen: Vec<String> = dictionary
            .words()
            .iter()
            .map(|w| w.to_lowercase())
            .collect();
        let mut kept = Vec::new();
        for name in names {
            let Some(name) = usable(name.as_ref()) else {
                continue;
            };
            let key = name.to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            kept.push(name);
        }
        People(kept)
    }

    /// How many of `names` [`People::new`] leaves out for what they hold (a control or
    /// invisible formatting character, such as a zero-width joiner, or more than
    /// [`MAX_PERSON_NAME_CHARS`] characters), never for being empty or a repeat: what the
    /// Dictionary page says it left out (T12.91a). The Dictionary refuses the same
    /// characters, so these names are never given to a summary.
    #[must_use]
    pub fn left_out<I, S>(names: I) -> usize
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        names
            .into_iter()
            .filter(|n| {
                let n = n.as_ref();
                n.split_whitespace().next().is_some() && usable(n).is_none()
            })
            .count()
    }

    /// Whether there are no names (no request carries any).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The names, in order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.0
    }
}

impl From<Vec<String>> for People {
    fn from(names: Vec<String>) -> Self {
        People::new(names, &Vocabulary::default())
    }
}

impl From<People> for Vec<String> {
    fn from(p: People) -> Vec<String> {
        p.0
    }
}

/// `name` on one line (white space runs joined); `None` when it is empty or holds what no
/// kept name may: a control or invisible formatting character, or more than
/// [`MAX_PERSON_NAME_CHARS`] characters.
fn usable(name: &str) -> Option<String> {
    if name.chars().any(char::is_control) || super::format_chars::has_format_char(name) {
        return None;
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty() && name.chars().count() <= MAX_PERSON_NAME_CHARS).then_some(name)
}

/// One name's line in the block.
pub(super) fn people_line(name: &str) -> String {
    format!("{PEOPLE_LINE_PREFIX}{name}")
}

/// The block for a request carrying `chosen`: `None` when it carries none, else [`PEOPLE`]
/// with one [`PEOPLE_LINE_PREFIX`]ed line per name in its slot.
#[must_use]
pub(super) fn people_block(chosen: &[String]) -> Option<String> {
    (!chosen.is_empty()).then(|| {
        let lines: Vec<String> = chosen.iter().map(|n| people_line(n)).collect();
        PEOPLE.replace("{{names}}", &lines.join("\n"))
    })
}
