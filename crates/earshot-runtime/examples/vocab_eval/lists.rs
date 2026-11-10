//! The word lists of docs/vocabulary.md ("Which words are listed"): common words from AMI
//! meetings outside the dev and test series, rare tokens of LibriSpeech dev-clean, and one
//! 100-word list per utterance (its rare tokens plus seeded distractors).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use earshot_core::metrics::normalize::normalize_english;
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

use crate::refs;

/// How many of AMI's most frequent tokens are common words.
pub const COMMON_WORDS: usize = 5_000;
/// A rare token appears in at most this many dev-clean chapters.
pub const MAX_CHAPTERS: usize = 2;
/// Words in every list (the Dictionary's limit, `MAX_VOCABULARY_WORDS`).
pub const LIST_LEN: usize = 100;
/// The seed of utterance `i`'s draws and shuffle is this plus `i`.
pub const LIST_SEED: u64 = 0x7121_5b00;
/// The AMI series of the scenario-only dev and test partitions, left out of the common words.
pub const HELD_SERIES: [&str; 10] = [
    "ES2003", "ES2011", "IS1008", "TS3004", "TS3006", "ES2004", "ES2014", "IS1009", "TS3003",
    "TS3007",
];

/// Normalised tokens of `text` (the WER normaliser, split on white space).
pub fn tokens(text: &str) -> Vec<String> {
    normalize_english(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The `n` most frequent tokens of `texts` (ties in text order).
pub fn most_frequent(texts: impl IntoIterator<Item = String>, n: usize) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for text in texts {
        for t in tokens(&text) {
            *counts.entry(t).or_default() += 1;
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(n).map(|(t, _)| t).collect()
}

/// Whether an AMI words file (`<meeting>.<speaker>.words.xml`) belongs to a held series.
pub fn held(file_name: &str) -> bool {
    HELD_SERIES.iter().any(|s| file_name.starts_with(s))
}

/// `xml` with `starttime="0"` given to every `<w>` that has no start time, so
/// `refs::parse_ami_words` reads its text (a few words of the training meetings have none;
/// the common words need no times).
pub fn with_start_times(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        out.push_str(&rest[..at]);
        let tag_end = rest[at..].find('>').map_or(rest.len(), |e| at + e);
        out.push_str("<w ");
        if !rest[at..tag_end].contains(" starttime=\"") {
            out.push_str("starttime=\"0\" ");
        }
        rest = &rest[at + 3..];
    }
    out.push_str(rest);
    out
}

/// The common words: [`COMMON_WORDS`] most frequent tokens of every AMI words file outside
/// [`HELD_SERIES`], one text per file (`refs::parse_ami_words`: no punctuation, no
/// unfinished fragments, `&#39;` decoded, spelled-letter underscores removed; a word without
/// a start time still counts).
pub fn ami_common(words_dir: &Path) -> Result<HashSet<String>, String> {
    let mut names: Vec<String> = std::fs::read_dir(words_dir)
        .map_err(|e| format!("{}: {e}", words_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".words.xml") && !held(n))
        .collect();
    names.sort();
    if names.len() < 100 {
        return Err(format!(
            "{}: only {} AMI words files; run `cargo xtask fetch-fixtures --set ami`",
            words_dir.display(),
            names.len()
        ));
    }
    let mut texts = Vec::with_capacity(names.len());
    for name in &names {
        let p = words_dir.join(name);
        let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let words = refs::parse_ami_words(&with_start_times(&refs::latin1(&bytes)))
            .map_err(|e| format!("{}: {e}", p.display()))?;
        texts.push(
            words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    Ok(most_frequent(texts, COMMON_WORDS))
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
    fn vocab_most_frequent_ranks_by_count_then_text() {
        let set = most_frequent(["b a a c".to_owned(), "c b".to_owned()], 2);
        // a 2, b 2, c 2: text order keeps a and b.
        assert_eq!(set, ["a", "b"].map(str::to_owned).into_iter().collect());
        let set = most_frequent(["z z z y".to_owned(), "x y".to_owned()], 2);
        assert_eq!(set, ["z", "y"].map(str::to_owned).into_iter().collect());
    }

    #[test]
    fn vocab_ami_words_without_a_start_time_still_count() {
        let xml = "<w nite:id=\"a\" starttime=\"1.5\" endtime=\"2\">Uh</w>\
                   <w nite:id=\"b\">T_V_</w><w nite:id=\"c\" punc=\"true\">.</w>\
                   <w nite:id=\"d\" trunc=\"true\">Defin</w><w nite:id=\"e\">don&#39;t</w>";
        let words = refs::parse_ami_words(&with_start_times(xml)).unwrap();
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Uh", "TV", "don't"]);
        assert_eq!(words[0].start, 1.5);
        assert!(refs::parse_ami_words(xml).is_err());
    }

    #[test]
    fn vocab_held_series_are_the_ten_dev_and_test_series() {
        assert!(held("ES2003a.A.words.xml"));
        assert!(held("TS3007d.D.words.xml"));
        assert!(!held("ES2002a.A.words.xml"));
        assert!(!held("EN2001a.B.words.xml"));
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
