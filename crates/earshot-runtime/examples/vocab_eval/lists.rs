//! The word lists of docs/vocabulary.md ("Which words are listed"): rare tokens of
//! LibriSpeech dev-clean and one 100-word list per utterance (its rare tokens plus seeded
//! distractors). The common words (AMI meetings outside the dev and test series) are made by
//! `cargo xtask common-words` and embedded in core (`earshot_core::vocabulary::suggest`).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use earshot_core::metrics::normalize::normalize_english;
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

/// A rare token appears in at most this many dev-clean chapters.
pub const MAX_CHAPTERS: usize = 2;
/// Words in every list (the Dictionary's limit, `MAX_VOCABULARY_WORDS`).
pub const LIST_LEN: usize = 100;
/// The seed of utterance `i`'s draws and shuffle is this plus `i`.
pub const LIST_SEED: u64 = 0x7121_5b00;
/// Normalised tokens of `text` (the WER normaliser, split on white space).
pub fn tokens(text: &str) -> Vec<String> {
    normalize_english(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// One LibriSpeech utterance's id and reference, in `(speaker, chapter, utterance)` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utterance {
    pub id: String,
    pub reference: String,
}

impl Utterance {
    /// `(speaker, chapter, utterance)` of the id.
    pub fn key(&self) -> (u64, u64, u64) {
        let mut p = self
            .id
            .split('-')
            .map(|x| x.parse::<u64>().unwrap_or(u64::MAX));
        (
            p.next().unwrap_or(u64::MAX),
            p.next().unwrap_or(u64::MAX),
            p.next().unwrap_or(u64::MAX),
        )
    }

    /// `speaker-chapter`.
    pub fn chapter(&self) -> &str {
        self.id.rsplit_once('-').map_or(&self.id, |(c, _)| c)
    }
}

/// Whether `token` may be rare: only a-z and apostrophes.
pub fn word_like(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_lowercase() || c == '\'')
}

/// Rare tokens of a set of utterances: word-like, not common, in at most [`MAX_CHAPTERS`]
/// chapters.
pub fn rare_tokens(utterances: &[Utterance], common: &HashSet<String>) -> BTreeSet<String> {
    let mut chapters: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for u in utterances {
        for t in tokens(&u.reference) {
            chapters.entry(t).or_default().insert(u.chapter());
        }
    }
    chapters
        .into_iter()
        .filter(|(t, c)| word_like(t) && !common.contains(t) && c.len() <= MAX_CHAPTERS)
        .map(|(t, _)| t)
        .collect()
}

/// One utterance's list: the listed tokens (normalised, lower case) in list order, and how
/// many of them the utterance says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub tokens: Vec<String>,
    pub spoken: usize,
}

impl List {
    /// The written forms (title case), in list order.
    pub fn forms(&self) -> Vec<String> {
        self.tokens.iter().map(|t| title_case(t)).collect()
    }
}

/// `word` with its first letter upper case.
pub fn title_case(word: &str) -> String {
    let mut c = word.chars();
    c.next().map_or_else(String::new, |f| {
        f.to_uppercase()
            .chain(c.flat_map(char::to_lowercase))
            .collect()
    })
}

fn below(rng: &mut ChaCha8Rng, n: usize) -> usize {
    usize::try_from(rng.next_u64() % (n as u64)).unwrap_or(0)
}

/// The list of utterance `index` (docs/vocabulary.md step 4).
pub fn list_for(index: usize, reference: &str, rare: &BTreeSet<String>, pool: &[String]) -> List {
    let said: Vec<String> = tokens(reference);
    let said_set: HashSet<&str> = said.iter().map(String::as_str).collect();
    let mut chosen: Vec<String> = Vec::new();
    for t in &said {
        if rare.contains(t) && !chosen.contains(t) && chosen.len() < LIST_LEN {
            chosen.push(t.clone());
        }
    }
    fill(LIST_SEED + index as u64, chosen, &said_set, pool)
}

/// A list from its spoken tokens `chosen` (distinct, at most [`LIST_LEN`]): distractors drawn
/// without repeats from `pool` minus `said` until it holds [`LIST_LEN`] words, then shuffled,
/// with ChaCha8 seeded `seed` (docs/vocabulary.md step 4).
pub fn fill(seed: u64, mut chosen: Vec<String>, said_set: &HashSet<&str>, pool: &[String]) -> List {
    let spoken = chosen.len();
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut taken: HashSet<String> = chosen.iter().cloned().collect();
    let available = pool
        .iter()
        .filter(|t| !said_set.contains(t.as_str()))
        .count();
    while chosen.len() < LIST_LEN && taken.len() < spoken + available {
        let t = &pool[below(&mut rng, pool.len())];
        if said_set.contains(t.as_str()) || taken.contains(t) {
            continue;
        }
        taken.insert(t.clone());
        chosen.push(t.clone());
    }
    for i in (1..chosen.len()).rev() {
        let j = below(&mut rng, i + 1);
        chosen.swap(i, j);
    }
    List {
        tokens: chosen,
        spoken,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utt(id: &str, text: &str) -> Utterance {
        Utterance {
            id: id.into(),
            reference: text.into(),
        }
    }

    #[test]
    fn vocab_rare_tokens_leave_out_common_words_numbers_and_words_of_many_chapters() {
        let us = [
            utt("1-10-0", "THE BOZZLE SAW 3 LADIES"),
            utt("1-11-0", "THE LADIES LEFT"),
            utt("2-20-0", "LADIES AGAIN AND BOZZLE"),
            utt("2-20-1", "QUIXOTE"),
        ];
        let common: HashSet<String> = ["the", "and", "saw", "left", "again"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        let rare = rare_tokens(&us, &common);
        // ladies is in three chapters; 3 is not word-like.
        assert_eq!(
            rare,
            ["bozzle", "quixote"]
                .map(str::to_owned)
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn vocab_lists_hold_the_spoken_rare_words_and_unspoken_distractors() {
        let pool: Vec<String> = (0u8..250)
            .map(|i| {
                format!(
                    "q{}{}",
                    char::from(b'a' + i / 26),
                    char::from(b'a' + i % 26)
                )
            })
            .collect();
        let mut rare: BTreeSet<String> = pool.iter().cloned().collect();
        rare.insert("bozzle".into());
        let list = list_for(7, "OLD BOZZLE SAID QAF", &rare, &pool);
        assert_eq!(list.tokens.len(), LIST_LEN);
        assert_eq!(list.spoken, 2);
        assert!(list.tokens.contains(&"bozzle".to_owned()));
        assert!(list.tokens.contains(&"qaf".to_owned()));
        let distinct: HashSet<&String> = list.tokens.iter().collect();
        assert_eq!(distinct.len(), LIST_LEN);
        // Seeded: the same index gives the same list, another index another.
        assert_eq!(list, list_for(7, "OLD BOZZLE SAID QAF", &rare, &pool));
        assert_ne!(list, list_for(8, "OLD BOZZLE SAID QAF", &rare, &pool));
        // A small pool ends the draws instead of looping.
        let small: Vec<String> = pool[..5].to_vec();
        let short = list_for(0, "QAB", &rare, &small);
        assert_eq!(short.tokens.len(), 5);
        assert_eq!(short.spoken, 1);
    }

    #[test]
    fn vocab_forms_are_title_case_and_ids_sort_by_number() {
        assert_eq!(title_case("bozzle's"), "Bozzle's");
        assert_eq!(title_case(""), "");
        let a = utt("84-121123-0002", "");
        let b = utt("1272-128104-0000", "");
        assert!(a.key() < b.key());
        assert_eq!(a.chapter(), "84-121123");
    }
}
